//! Replay a local on-disk session transcript into a [`SessionStore`].
//!
//! Ported from `import_session_to_store` in upstream
//! `_internal/session_import.py`. This is the inverse of resume
//! materialization (a sibling slice): where that reads a store and writes a
//! temp `~/.claude` tree, [`import_session_to_store`] reads the local
//! `~/.claude/projects/<dir>/<sessionId>.jsonl` (plus subagent transcripts)
//! and replays each line into `store.append()`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::errors::{ClaudeError, JsonDecodeError, Result};

use super::local::validate_uuid;
use super::local_session_file::find_session_file;
use super::store::{SessionStore, SessionStoreError};
use super::transcript_mirror::{MAX_PENDING_BYTES, MAX_PENDING_ENTRIES};
use super::types::{SessionKey, SessionStoreEntry};

fn store_err(e: SessionStoreError) -> ClaudeError {
    ClaudeError::Other(anyhow::Error::new(e))
}

/// Replay a local session transcript into a [`SessionStore`].
///
/// Streams the on-disk JSONL line-by-line and calls `store.append(key,
/// batch)` every `batch_size` entries (or 1 MiB of line bytes, whichever
/// comes first). Useful for migrating existing local sessions to a remote
/// store, or for catching a store up after a live-mirror gap. Adapters
/// should treat each entry's `uuid` as an idempotency key so re-import is
/// duplicate-safe.
///
/// The destination `project_key` is the name of the on-disk project
/// directory the session file was found in, so an imported session is
/// indistinguishable from a live-mirrored one and resumable from the
/// original `cwd`.
///
/// `batch_size` of `0` falls back to the 500-entry default.
///
/// # Errors
/// [`ClaudeError::InvalidInput`] if `session_id` is not a valid UUID.
/// [`ClaudeError::NotFound`] if the session JSONL cannot be found on disk.
pub async fn import_session_to_store(
    session_id: &str,
    store: &dyn SessionStore,
    directory: Option<&str>,
    include_subagents: bool,
    batch_size: usize,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }

    let Some(resolved) = find_session_file(session_id, directory).await else {
        return Err(ClaudeError::NotFound(format!("Session {session_id} not found")));
    };

    // Key under the on-disk project directory name — matches what a live
    // mirror would have produced even when the resolver's search found the
    // file via the directory=None or worktree-fallback path.
    let project_key = resolved
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let batch_size = if batch_size == 0 { MAX_PENDING_ENTRIES } else { batch_size };

    let main_key = SessionKey::new(project_key.clone(), session_id);
    append_jsonl_file_in_batches(&resolved, &main_key, store, batch_size).await?;

    if !include_subagents {
        return Ok(());
    }

    // Subagent transcripts live at <projectDir>/<sessionId>/subagents/**.
    let session_dir = resolved.parent().map(|p| p.join(session_id)).unwrap_or_else(|| PathBuf::from(session_id));
    let subagents_dir = session_dir.join("subagents");
    let mut files = Vec::new();
    collect_jsonl_files(&subagents_dir, &mut files).await;

    for file_path in files {
        // subpath is the path relative to session_dir, '/'-joined, sans
        // .jsonl — e.g. subagents/agent-abc or
        // subagents/workflows/run-1/agent-def.
        let rel = file_path.strip_prefix(&session_dir).unwrap_or(&file_path);
        let mut rel_parts: Vec<String> =
            rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        if let Some(last) = rel_parts.last_mut() {
            if let Some(stripped) = last.strip_suffix(".jsonl") {
                *last = stripped.to_string();
            }
        }
        let sub_key = SessionKey::with_subpath(project_key.clone(), session_id, rel_parts.join("/"));
        append_jsonl_file_in_batches(&file_path, &sub_key, store, batch_size).await?;

        // The on-disk .jsonl does NOT contain agent_metadata entries — those
        // are only sent to live mirrors and persisted in the .meta.json
        // sidecar. Import the sidecar so resume materialization can
        // recreate it and resumed subagents keep their agentType/worktreePath.
        import_subagent_meta_sidecar(&file_path, &sub_key, store).await?;
    }

    Ok(())
}

async fn import_subagent_meta_sidecar(file_path: &Path, sub_key: &SessionKey, store: &dyn SessionStore) -> Result<()> {
    let meta_path = file_path.with_extension("meta.json");
    let content = match tokio::fs::read_to_string(&meta_path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let meta: Value = serde_json::from_str(&content)
        .map_err(|e| ClaudeError::JsonDecode(JsonDecodeError::new(e.to_string(), content.clone())))?;

    // Mirrors upstream's `{"type": "agent_metadata"}` seeded then
    // `.update(meta)` — if the sidecar itself carries a "type" field, it
    // overwrites the "agent_metadata" default (matches upstream verbatim).
    let mut fields = serde_json::Map::new();
    fields.insert("type".to_string(), Value::String("agent_metadata".to_string()));
    if let Value::Object(meta_fields) = meta {
        for (k, v) in meta_fields {
            fields.insert(k, v);
        }
    }
    let type_ = fields.remove("type").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "agent_metadata".to_string());
    let entry = SessionStoreEntry::new(type_, fields);

    store.append(sub_key, vec![entry]).await.map_err(store_err)
}

/// Stream-reads a JSONL file line-by-line, parsing each line and flushing
/// to `store.append()` in batches of `batch_size` entries (or
/// `MAX_PENDING_BYTES` of line text, whichever comes first). Skips blank
/// lines.
async fn append_jsonl_file_in_batches(
    file_path: &Path,
    key: &SessionKey,
    store: &dyn SessionStore,
    batch_size: usize,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let file = tokio::fs::File::open(file_path).await?;
    let mut lines = BufReader::new(file).lines();

    let mut batch: Vec<SessionStoreEntry> = Vec::new();
    let mut nbytes: usize = 0;
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let entry: SessionStoreEntry = serde_json::from_str(&line)
            .map_err(|e| ClaudeError::JsonDecode(JsonDecodeError::new(e.to_string(), line.clone())))?;
        nbytes += line.len();
        batch.push(entry);
        if batch.len() >= batch_size || nbytes >= MAX_PENDING_BYTES {
            store.append(key, std::mem::take(&mut batch)).await.map_err(store_err)?;
            nbytes = 0;
        }
    }
    if !batch.is_empty() {
        store.append(key, batch).await.map_err(store_err)?;
    }
    Ok(())
}

/// Recursively collects all `*.jsonl` file paths under `base_dir`, sorted
/// per directory so import order is deterministic. Yields nothing if
/// `base_dir` does not exist.
fn collect_jsonl_files<'a>(
    base_dir: &'a Path,
    out: &'a mut Vec<PathBuf>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let Ok(mut entries) = tokio::fs::read_dir(base_dir).await else {
            return;
        };
        let mut dirents: Vec<(PathBuf, bool)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            dirents.push((entry.path(), is_dir));
        }
        dirents.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));
        for (path, is_dir) in dirents {
            if is_dir {
                collect_jsonl_files(&path, out).await;
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::local_session_file::env_lock;
    use crate::session::InMemorySessionStore;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    async fn setup_session(
        config_dir: &std::path::Path,
        project_dir: &std::path::Path,
        session_id: &str,
        content: &str,
    ) -> std::path::PathBuf {
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir) };
        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.to_str().unwrap()));
        let sessions_dir = config_dir.join("projects").join(project_key);
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        let file = sessions_dir.join(format!("{session_id}.jsonl"));
        tokio::fs::write(&file, content).await.unwrap();
        file
    }

    #[tokio::test]
    async fn import_session_to_store_replays_main_transcript() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let transcript = "{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"assistant\",\"uuid\":\"u2\"}\n";
        setup_session(config_dir.path(), project_dir.path(), SESSION_ID, transcript).await;

        let store = InMemorySessionStore::new();
        import_session_to_store(SESSION_ID, &store, Some(project_dir.path().to_str().unwrap()), false, 0)
            .await
            .unwrap();

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let key = SessionKey::new(project_key, SESSION_ID);
        let entries = store.get_entries(&key).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].uuid(), Some("u1"));
        assert_eq!(entries[1].uuid(), Some("u2"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn import_session_to_store_batches_by_batch_size() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();
        let transcript = "{\"type\":\"user\",\"uuid\":\"u1\"}\n{\"type\":\"user\",\"uuid\":\"u2\"}\n{\"type\":\"user\",\"uuid\":\"u3\"}\n";
        setup_session(config_dir.path(), project_dir.path(), SESSION_ID, transcript).await;

        let store = InMemorySessionStore::new();
        import_session_to_store(SESSION_ID, &store, Some(project_dir.path().to_str().unwrap()), false, 2)
            .await
            .unwrap();

        let project_key = super::super::project_dir::project_key_for_directory(Some(project_dir.path().to_str().unwrap()));
        let key = SessionKey::new(project_key, SESSION_ID);
        // All entries still land regardless of batch boundaries.
        assert_eq!(store.get_entries(&key).await.len(), 3);
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn import_session_to_store_not_found_errors() {
        let _guard = env_lock().lock().await;
        let config_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", config_dir.path()) };
        tokio::fs::create_dir_all(config_dir.path().join("projects")).await.unwrap();

        let store = InMemorySessionStore::new();
        let err = import_session_to_store(SESSION_ID, &store, None, false, 0).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
    }

    #[tokio::test]
    async fn import_session_to_store_rejects_invalid_uuid() {
        let store = InMemorySessionStore::new();
        let err = import_session_to_store("bad-id", &store, None, false, 0).await.unwrap_err();
        assert!(err.to_string().contains("Invalid session_id"));
    }

    #[tokio::test]
    async fn collect_jsonl_files_recurses_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("nested")).await.unwrap();
        tokio::fs::write(dir.path().join("b.jsonl"), "").await.unwrap();
        tokio::fs::write(dir.path().join("nested/a.jsonl"), "").await.unwrap();
        tokio::fs::write(dir.path().join("ignore.txt"), "").await.unwrap();

        let mut files = Vec::new();
        collect_jsonl_files(dir.path(), &mut files).await;

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.ends_with("b.jsonl")));
        assert!(files.iter().any(|f| f.ends_with("nested/a.jsonl")));
    }

    #[tokio::test]
    async fn collect_jsonl_files_missing_dir_yields_empty() {
        let mut files = Vec::new();
        collect_jsonl_files(std::path::Path::new("/definitely/does/not/exist"), &mut files).await;
        assert!(files.is_empty());
    }
}
