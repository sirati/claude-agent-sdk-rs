//! Shared secure-file-write helpers for resume materialization.
//!
//! Every file this module writes under a materialized `CLAUDE_CONFIG_DIR`
//! either carries credentials (`.credentials.json`) or is otherwise meant
//! to be private to the invoking user, so writes go through one place that
//! restricts permissions to owner read/write.

use std::path::Path;

use crate::errors::{ClaudeError, Result};
use crate::session::SessionStoreEntry;

/// Write `contents` to `path`, creating parent directories as needed, then
/// best-effort restrict permissions to `0o600` (owner read/write only) on
/// Unix. A no-op on other platforms, matching upstream's
/// `with suppress(OSError): path.chmod(0o600)` (which is similarly
/// best-effort — chmod failures never fail materialization).
pub(super) async fn write_secure_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(ClaudeError::Io)?;
    }
    tokio::fs::write(path, contents).await.map_err(ClaudeError::Io)?;
    restrict_permissions(path).await;
    Ok(())
}

#[cfg(unix)]
async fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}

#[cfg(not(unix))]
async fn restrict_permissions(_path: &Path) {}

/// Stream-write `entries` as JSONL (one compact JSON object per line) to
/// `path`, mode `0o600`. Mirrors upstream's `_write_jsonl`.
///
/// Byte-for-byte parity with Python's `json.dumps` key ordering is not
/// attempted (and not required — see [`SessionStoreEntry`]'s docs): this
/// writes whatever key order `serde_json` produces for each entry's `extra`
/// map. Round-tripping through `serde_json` is the only invariant callers
/// may rely on.
pub(super) async fn write_jsonl(path: &Path, entries: &[SessionStoreEntry]) -> Result<()> {
    let mut buf = String::new();
    for entry in entries {
        let line = serde_json::to_string(entry).map_err(|e| ClaudeError::Other(e.into()))?;
        buf.push_str(&line);
        buf.push('\n');
    }
    write_secure_file(path, buf.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(type_: &str, extra: serde_json::Value) -> SessionStoreEntry {
        SessionStoreEntry::new(type_, extra.as_object().unwrap().clone())
    }

    #[tokio::test]
    async fn write_jsonl_round_trips_and_is_one_line_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("session.jsonl");
        let entries = vec![
            entry("user", json!({"uuid": "u1", "message": {"content": "hi \"q\"\n"}})),
            entry("assistant", json!({"uuid": "a1"})),
        ];

        write_jsonl(&path, &entries).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.last(), Some(&""), "trailing newline");
        let body = &lines[..lines.len() - 1];
        assert_eq!(body.len(), entries.len());
        let parsed: Vec<SessionStoreEntry> = body
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed, entries);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_secure_file_restricts_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_secure_file(&path, b"top secret").await.unwrap();
        let mode = tokio::fs::metadata(&path).await.unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
