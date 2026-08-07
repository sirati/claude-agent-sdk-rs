//! `SessionStore`-backed session mutations.
//!
//! Ported from `rename_session_via_store` / `tag_session_via_store` /
//! `delete_session_via_store` / `fork_session_via_store` in upstream
//! `_internal/session_mutations.py` — the async, store-backed counterparts
//! of [`super::mutations`] and [`super::fork`].

use serde_json::Value;
use uuid::Uuid;

use crate::errors::{ClaudeError, Result};

use super::fork::ForkSessionResult;
use super::fork_transform::{build_fork_lines, derive_title_from_entries, partition_transcript_entries, Entry};
use super::iso_time::iso_now;
use super::local::validate_uuid;
use super::project_dir::project_key_for_directory;
use super::store::{SessionStore, SessionStoreError};
use super::types::{SessionKey, SessionStoreEntry};
use super::unicode_sanitize::sanitize_unicode;

fn store_err(e: SessionStoreError) -> ClaudeError {
    ClaudeError::Other(anyhow::Error::new(e))
}

/// Builds a top-level mutation entry (`custom-title`/`tag`), stamping a
/// fresh `uuid` + `timestamp` the way upstream's `_iso_now()` +
/// `uuid.uuid4()` calls do inline.
fn build_entry(type_: &str, mut fields: serde_json::Map<String, Value>) -> SessionStoreEntry {
    fields.insert("uuid".to_string(), Value::String(Uuid::new_v4().to_string()));
    fields.insert("timestamp".to_string(), Value::String(iso_now()));
    SessionStoreEntry::new(type_, fields)
}

fn session_store_entry_to_map(entry: SessionStoreEntry) -> Entry {
    let mut map = entry.extra;
    map.insert("type".to_string(), Value::String(entry.type_));
    map
}

fn map_to_session_store_entry(mut map: Entry) -> SessionStoreEntry {
    let type_ = map.remove("type").and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    SessionStoreEntry::new(type_, map)
}

/// Rename a session by appending a `custom-title` entry to a
/// [`SessionStore`]. Async, store-backed counterpart to
/// [`super::mutations::rename_session`].
pub async fn rename_session_via_store(
    store: &dyn SessionStore,
    session_id: &str,
    title: &str,
    directory: Option<&str>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeError::InvalidInput("title must be non-empty".to_string()));
    }
    let key = SessionKey::new(project_key_for_directory(directory), session_id);

    let mut fields = serde_json::Map::new();
    fields.insert("customTitle".to_string(), Value::String(stripped.to_string()));
    fields.insert("sessionId".to_string(), Value::String(session_id.to_string()));
    let entry = build_entry("custom-title", fields);

    store.append(&key, vec![entry]).await.map_err(store_err)
}

/// Tag a session by appending a `tag` entry to a [`SessionStore`]. Pass
/// `None` to clear the tag. Async, store-backed counterpart to
/// [`super::mutations::tag_session`].
pub async fn tag_session_via_store(
    store: &dyn SessionStore,
    session_id: &str,
    tag: Option<&str>,
    directory: Option<&str>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    let sanitized_tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag);
            let sanitized = sanitized.trim();
            if sanitized.is_empty() {
                return Err(ClaudeError::InvalidInput("tag must be non-empty (use None to clear)".to_string()));
            }
            sanitized.to_string()
        }
        None => String::new(),
    };
    let key = SessionKey::new(project_key_for_directory(directory), session_id);

    let mut fields = serde_json::Map::new();
    fields.insert("tag".to_string(), Value::String(sanitized_tag));
    fields.insert("sessionId".to_string(), Value::String(session_id.to_string()));
    let entry = build_entry("tag", fields);

    store.append(&key, vec![entry]).await.map_err(store_err)
}

/// Delete a session from a [`SessionStore`]. Async, store-backed
/// counterpart to [`super::mutations::delete_session`].
///
/// If the store does not implement [`SessionStore::delete`] (see
/// [`SessionStore::supports_delete`]), deletion is a no-op — appropriate
/// for WORM/append-only backends, matching the [`SessionStore`] contract.
pub async fn delete_session_via_store(store: &dyn SessionStore, session_id: &str, directory: Option<&str>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    if !store.supports_delete() {
        return Ok(());
    }
    let key = SessionKey::new(project_key_for_directory(directory), session_id);
    store.delete(&key).await.map_err(store_err)
}

/// Fork a session into a new branch with fresh UUIDs via a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::fork::fork_session`]. Runs
/// the fork transform directly over the objects returned by
/// [`SessionStore::load`] — no JSONL round-trip. A storage-layer copy (e.g.
/// S3 `CopyObject`) is NOT sufficient: the transform remaps every UUID,
/// rewrites `sessionId` on each entry, and stamps `forkedFrom`, so the data
/// must pass through this process once.
///
/// # Errors
/// [`ClaudeError::InvalidInput`] if `session_id` or `up_to_message_id` is
/// not a valid UUID, or if the session has no messages to fork.
/// [`ClaudeError::NotFound`] if the source session is not found in the
/// store.
pub async fn fork_session_via_store(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeError::InvalidInput(format!("Invalid session_id: {session_id}")));
    }
    if let Some(up_to) = up_to_message_id {
        if validate_uuid(up_to).is_none() {
            return Err(ClaudeError::InvalidInput(format!("Invalid up_to_message_id: {up_to}")));
        }
    }
    let project_key = project_key_for_directory(directory);
    let src_key = SessionKey::new(project_key.clone(), session_id);
    let loaded = store.load(&src_key).await.map_err(store_err)?;
    let loaded = match loaded {
        Some(entries) if !entries.is_empty() => entries,
        _ => return Err(ClaudeError::NotFound(format!("Session {session_id} not found"))),
    };

    // Partition into transcript entries (with uuid) and content-replacement
    // records, mirroring parse_fork_transcript for the already-parsed path.
    let raw: Vec<Entry> = loaded.into_iter().map(session_store_entry_to_map).collect();
    let (transcript, content_replacements) = partition_transcript_entries(raw.clone(), session_id);

    let (forked_session_id, entries) = build_fork_lines(
        transcript,
        content_replacements,
        session_id,
        up_to_message_id,
        title,
        || derive_title_from_entries(&raw),
    )?;

    let dst_key = SessionKey::new(project_key, forked_session_id.clone());
    let store_entries: Vec<SessionStoreEntry> = entries.into_iter().map(map_to_session_store_entry).collect();
    store.append(&dst_key, store_entries).await.map_err(store_err)?;

    Ok(ForkSessionResult { session_id: forked_session_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::InMemorySessionStore;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn user_entry(uuid: &str, parent: Option<&str>) -> SessionStoreEntry {
        let mut fields = serde_json::Map::new();
        fields.insert("uuid".to_string(), Value::String(uuid.to_string()));
        if let Some(p) = parent {
            fields.insert("parentUuid".to_string(), Value::String(p.to_string()));
        }
        SessionStoreEntry::new("user", fields)
    }

    #[tokio::test]
    async fn rename_session_via_store_appends_custom_title() {
        let store = InMemorySessionStore::new();
        rename_session_via_store(&store, SESSION_ID, "New Title", Some("/tmp/proj")).await.unwrap();

        let key = SessionKey::new(project_key_for_directory(Some("/tmp/proj")), SESSION_ID);
        let entries = store.get_entries(&key).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].type_, "custom-title");
        assert_eq!(entries[0].extra["customTitle"], "New Title");
        assert!(entries[0].uuid().is_some());
        assert!(entries[0].timestamp().is_some());
    }

    #[tokio::test]
    async fn rename_session_via_store_rejects_blank_title() {
        let store = InMemorySessionStore::new();
        let err = rename_session_via_store(&store, SESSION_ID, "  ", None).await.unwrap_err();
        assert!(err.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn tag_session_via_store_sanitizes_tag() {
        let store = InMemorySessionStore::new();
        tag_session_via_store(&store, SESSION_ID, Some("exp\u{200b}eriment"), Some("/tmp/proj")).await.unwrap();

        let key = SessionKey::new(project_key_for_directory(Some("/tmp/proj")), SESSION_ID);
        let entries = store.get_entries(&key).await;
        assert_eq!(entries[0].extra["tag"], "experiment");
    }

    #[tokio::test]
    async fn delete_session_via_store_noop_when_unsupported() {
        struct NoDelete;
        #[async_trait::async_trait]
        impl SessionStore for NoDelete {
            async fn append(
                &self,
                _key: &SessionKey,
                _entries: Vec<SessionStoreEntry>,
            ) -> std::result::Result<(), SessionStoreError> {
                Ok(())
            }
            async fn load(
                &self,
                _key: &SessionKey,
            ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
                Ok(None)
            }
        }
        let store = NoDelete;
        // Should not error even though `delete` is unimplemented.
        delete_session_via_store(&store, SESSION_ID, None).await.unwrap();
    }

    #[tokio::test]
    async fn delete_session_via_store_deletes_when_supported() {
        let store = InMemorySessionStore::new();
        let key = SessionKey::new(project_key_for_directory(None), SESSION_ID);
        store.append(&key, vec![user_entry("u1", None)]).await.unwrap();

        delete_session_via_store(&store, SESSION_ID, None).await.unwrap();

        assert!(store.get_entries(&key).await.is_empty());
    }

    #[tokio::test]
    async fn fork_session_via_store_remaps_uuids() {
        let store = InMemorySessionStore::new();
        let directory = Some("/tmp/fork-proj");
        let project_key = project_key_for_directory(directory);
        let src_key = SessionKey::new(project_key.clone(), SESSION_ID);
        store.append(&src_key, vec![user_entry("u1", None), user_entry("u2", Some("u1"))]).await.unwrap();

        let result = fork_session_via_store(&store, SESSION_ID, directory, None, Some("Fork Title")).await.unwrap();
        assert_ne!(result.session_id, SESSION_ID);

        let dst_key = SessionKey::new(project_key, result.session_id.clone());
        let entries = store.get_entries(&dst_key).await;
        // 2 messages + 1 custom-title.
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.extra.get("sessionId").and_then(|v| v.as_str()) != Some(SESSION_ID)));
    }

    #[tokio::test]
    async fn fork_session_via_store_not_found() {
        let store = InMemorySessionStore::new();
        let err = fork_session_via_store(&store, SESSION_ID, None, None, None).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn fork_session_via_store_rejects_invalid_up_to_message_id() {
        let store = InMemorySessionStore::new();
        let err =
            fork_session_via_store(&store, SESSION_ID, None, Some("not-a-uuid"), None).await.unwrap_err();
        assert!(err.to_string().contains("Invalid up_to_message_id"));
    }
}
