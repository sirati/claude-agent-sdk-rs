//! Materializes subagent transcripts (`SessionStore::list_subkeys`) during
//! resume, with a path-traversal guard on subpaths returned by the store.
//!
//! Security-sensitive: ported from upstream `_internal/session_resume.py`'s
//! `_materialize_subkeys` and `_is_safe_subpath`. `subpath` values come
//! from an external store — an adversarial or merely buggy adapter is a
//! real threat model — and are used as filesystem path components below.
//! Read [`is_safe_subpath`]'s doc comment before changing it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::errors::Result;
use crate::session::{SessionKey, SessionListSubkeysKey, SessionStore};

use super::resume::with_timeout;
use super::resume_io::write_secure_file;
use super::resume_io::write_jsonl;

/// Load and write all subagent transcripts/metadata under `session_id`.
pub(super) async fn materialize_subkeys(
    store: &dyn SessionStore,
    project_dir: &Path,
    project_key: &str,
    session_id: &str,
    timeout: Duration,
) -> Result<()> {
    let session_dir = project_dir.join(session_id);
    let list_key = SessionListSubkeysKey {
        project_key: project_key.to_string(),
        session_id: session_id.to_string(),
    };
    let subkeys = with_timeout(
        store.list_subkeys(&list_key),
        timeout,
        &format!("SessionStore.list_subkeys() for session {session_id}"),
    )
    .await?;

    for subpath in subkeys {
        if !is_safe_subpath(&subpath, &session_dir).await {
            tracing::warn!(
                subpath = %subpath,
                "[SessionStore] skipping unsafe subpath from list_subkeys"
            );
            continue;
        }

        let sub_key = SessionKey::with_subpath(project_key, session_id, subpath.clone());
        let sub_entries = with_timeout(
            store.load(&sub_key),
            timeout,
            &format!("SessionStore.load() for session {session_id} subpath {subpath}"),
        )
        .await?;
        let Some(sub_entries) = sub_entries else { continue };
        if sub_entries.is_empty() {
            continue;
        }

        // Partition: agent_metadata entries describe the .meta.json
        // sidecar; everything else is a transcript line.
        let mut metadata = Vec::new();
        let mut transcript = Vec::new();
        for e in sub_entries {
            if e.type_ == "agent_metadata" {
                metadata.push(e);
            } else {
                transcript.push(e);
            }
        }

        let target = session_dir.join(&subpath);
        let file_name = target.file_name().and_then(|f| f.to_str()).unwrap_or_default().to_string();
        let sub_file = target.with_file_name(format!("{file_name}.jsonl"));

        if !transcript.is_empty() {
            write_jsonl(&sub_file, &transcript).await?;
        }

        // Last metadata entry wins. `SessionStoreEntry::extra` already
        // excludes the synthetic `type` discriminant (it's captured
        // separately by `SessionStoreEntry::type_`), so no explicit
        // stripping is needed here — it falls out of the struct shape.
        if let Some(last) = metadata.pop() {
            let meta_file = sub_file.with_file_name(format!("{file_name}.meta.json"));
            let content = serde_json::to_vec(&last.extra)
                .map_err(|e| crate::errors::ClaudeError::Other(e.into()))?;
            write_secure_file(&meta_file, &content).await?;
        }
    }
    Ok(())
}

/// Reject subpaths that are empty, absolute, contain `.`/`..` components,
/// carry a Windows drive prefix, embed a NUL byte, or resolve outside
/// `session_dir`.
///
/// `subpath` values come from an external [`SessionStore`] adapter and are
/// joined onto `session_dir` below to become filesystem paths — this is a
/// path-traversal guard, not a formatting nicety. Empty string is rejected
/// explicitly: `"" + ".jsonl"` -> `".jsonl"`, a hidden dotfile that would
/// pass a naive prefix check.
async fn is_safe_subpath(subpath: &str, session_dir: &Path) -> bool {
    if subpath.is_empty() {
        return false;
    }
    if Path::new(subpath).is_absolute() || subpath.starts_with('/') || subpath.starts_with('\\') {
        return false;
    }
    // Drive-prefixed ("C:foo") subpaths are never legitimate store keys.
    // Checked regardless of host OS (matches upstream's `ntpath.splitdrive`
    // use) so a Windows consumer is protected even if the store was
    // populated elsewhere; on POSIX this also rejects `C:foo`, which is
    // fine since the only subpaths we ever emit are `subagents/...`.
    if has_drive_prefix(subpath) {
        return false;
    }
    if subpath.split(['/', '\\']).any(|part| part == "." || part == "..") {
        return false;
    }
    if subpath.contains('\0') {
        return false;
    }

    // Resolve the .jsonl target using the same expression the writer above
    // uses, so the validated path can't drift from the written one, and
    // confirm it stays under session_dir. Both resolutions can fail (e.g.
    // broken symlink chains); treat any failure as unsafe so the subpath is
    // skipped with a warning rather than aborting the whole resume.
    let target = session_dir.join(subpath);
    let Some(file_name) = target.file_name().and_then(|f| f.to_str()) else {
        return false;
    };
    let sub_file = target.with_file_name(format!("{file_name}.jsonl"));

    let (Ok(resolved_sub_file), Ok(resolved_session_dir)) =
        (resolve_non_strict(&sub_file).await, resolve_non_strict(session_dir).await)
    else {
        return false;
    };
    resolved_sub_file.starts_with(&resolved_session_dir)
}

fn has_drive_prefix(subpath: &str) -> bool {
    // Matches `ntpath.splitdrive`'s actual check: `normp[1:2] == ':'`. It
    // does NOT require the first character to be a letter — `"1:foo"` and
    // `"@:foo"` are drive-prefixed too, in Python and here. Indexed by
    // `char`, not byte, so multi-byte UTF-8 at position 0 (e.g. `"é:foo"`)
    // lines up with Python's code-point indexing instead of misreading a
    // continuation byte.
    subpath.chars().nth(1) == Some(':')
}

/// `Path.resolve(strict=False)`-equivalent: canonicalize the longest
/// existing ancestor (following symlinks) and lexically append whatever
/// tail doesn't exist yet.
///
/// `tokio::fs::canonicalize` (like `std::fs::canonicalize`) requires the
/// full path to exist, but subkey targets are checked here *before*
/// they're written — at that point `session_dir` itself, let alone the
/// `.jsonl` target, often doesn't exist yet. Python's `pathlib.Path.resolve`
/// has no such requirement, so this walks up to the nearest existing
/// ancestor, canonicalizes that, and reattaches the (already
/// `.`/`..`-free, by the caller's checks) tail unresolved.
async fn resolve_non_strict(path: &Path) -> std::io::Result<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match tokio::fs::canonicalize(&current).await {
            Ok(mut base) => {
                for part in tail.iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let name = current.file_name().map(|f| f.to_os_string());
                let parent = current.parent().map(|p| p.to_path_buf());
                match (name, parent) {
                    (Some(name), Some(parent)) if parent != current => {
                        tail.push(name);
                        current = parent;
                    }
                    _ => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn safe_subpath_under_nonexistent_session_dir_is_accepted() {
        let base = tempfile::tempdir().unwrap();
        // `session_dir` itself does not exist yet, but its parent does —
        // this is the normal materialize-time state.
        let session_dir = base.path().join("session-id");
        assert!(is_safe_subpath("subagents/agent-ok", &session_dir).await);
    }

    #[tokio::test]
    async fn rejects_known_traversal_shapes() {
        let base = tempfile::tempdir().unwrap();
        let session_dir = base.path().join("session-id");
        for bad in [
            "",
            ".",
            "./",
            "a/.",
            "subagents/.",
            "/etc/passwd",
            "../escape",
            "a/../b",
            "C:escape",
            "C:\\abs",
            "1:foo",
            "@:foo",
            "subagents/agent\0x",
        ] {
            assert!(
                !is_safe_subpath(bad, &session_dir).await,
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn has_drive_prefix_matches_ntpath_splitdrive_semantics() {
        // Matches Python: normp[1:2] == ':' regardless of what's at index 0.
        assert!(has_drive_prefix("C:foo"));
        assert!(has_drive_prefix("1:foo"));
        assert!(has_drive_prefix("@:foo"));
        // Multi-byte UTF-8 at position 0: must be char-indexed, not
        // byte-indexed, or the second byte of 'é' would be misread.
        assert!(has_drive_prefix("é:foo"));
        assert!(!has_drive_prefix("foo"));
        assert!(!has_drive_prefix("a"));
        assert!(!has_drive_prefix(""));
    }

    #[tokio::test]
    async fn rejects_subpath_escaping_via_existing_symlink() {
        #[cfg(unix)]
        {
            let base = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let session_dir = base.path().join("session-id");
            tokio::fs::create_dir_all(&session_dir).await.unwrap();
            let link = session_dir.join("escape");
            tokio::fs::symlink(outside.path(), &link).await.unwrap();

            // "escape/file" resolves (via the symlink) outside session_dir.
            assert!(!is_safe_subpath("escape/file", &session_dir).await);
        }
    }
}
