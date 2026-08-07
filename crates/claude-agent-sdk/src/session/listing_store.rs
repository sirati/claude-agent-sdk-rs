//! `SessionStore`-backed session listing: `list_sessions_from_store()`,
//! `get_session_info_from_store()`, `get_session_messages_from_store()`.
//!
//! Ported from `_load_store_entries_as_jsonl`, `_derive_infos_via_load`,
//! `list_sessions_from_store`, `get_session_info_from_store`, and
//! `get_session_messages_from_store` in upstream `_internal/sessions.py`.
//! The `list_session_summaries()` fast path (with its gap-fill / staleness
//! check) lives in [`super::listing_store_fast_path`] — large enough on its
//! own to be a separate concern from the plain per-session `load()` path
//! here.

use futures::stream::{self, StreamExt};
use serde_json::Value;

use crate::errors::{ClaudeError, Result};

use super::info::{SDKSessionInfo, SessionMessage};
use super::lite_info::{entries_to_jsonl, jsonl_to_lite, mtime_from_jsonl_tail, parse_session_info_from_lite};
use super::listing::apply_sort_limit_offset;
use super::listing_store_fast_path::list_sessions_via_summaries;
use super::local::validate_uuid;
use super::message_paging::entries_to_session_messages;
use super::project_dir::{canonicalize_path, project_key_for_directory, sanitize_path};
use super::store::{SessionStore, SessionStoreError};
use super::transcript::filter_transcript_entries;
use super::types::{SessionKey, SessionStoreEntry, SessionStoreListEntry};

/// Upper bound on concurrent `store.load()` calls issued by
/// [`derive_infos_via_load`]. Keeps large project listings from exhausting
/// adapter connection pools or tripping backend rate limits.
pub(crate) const STORE_LIST_LOAD_CONCURRENCY: usize = 16;

pub(crate) fn store_err(e: SessionStoreError) -> ClaudeError {
    ClaudeError::Other(anyhow::Error::new(e))
}

/// Loads entries from a [`SessionStore`] and serializes them to a JSONL
/// string. Returns `None` if the session has no entries.
pub(crate) async fn load_store_entries_as_jsonl(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Option<String>> {
    let project_key = project_key_for_directory(directory);
    let key = SessionKey::new(project_key, session_id);
    let entries = store.load(&key).await.map_err(store_err)?;
    let Some(entries) = entries else { return Ok(None) };
    if entries.is_empty() {
        return Ok(None);
    }
    let values: Vec<Value> = entries.iter().map(SessionStoreEntry::to_value).collect();
    Ok(Some(entries_to_jsonl(&values)))
}

/// Derives [`SDKSessionInfo`] for each `listing` entry via per-session
/// `store.load()` + lite-parse.
///
/// Loads run concurrently with a fixed bound so large listings don't
/// exhaust adapter connection pools or hit backend rate limits; adapter
/// errors degrade that row to an empty summary instead of failing the whole
/// list. Sidechain and no-summary sessions are dropped.
pub(crate) async fn derive_infos_via_load(
    store: &dyn SessionStore,
    listing: &[SessionStoreListEntry],
    directory: Option<&str>,
    project_path: &str,
) -> Vec<SDKSessionInfo> {
    let results: Vec<Option<SDKSessionInfo>> = stream::iter(listing.iter().map(|entry| {
        let session_id = entry.session_id.clone();
        let mtime = entry.mtime;
        async move {
            match load_store_entries_as_jsonl(store, &session_id, directory).await {
                Ok(Some(jsonl)) => {
                    let lite = jsonl_to_lite(&jsonl, mtime);
                    parse_session_info_from_lite(&session_id, &lite, Some(project_path)).map(|mut info| {
                        info.last_modified = mtime;
                        info
                    })
                }
                Ok(None) => None,
                Err(_) => Some(SDKSessionInfo {
                    session_id,
                    summary: String::new(),
                    last_modified: mtime,
                    file_size: None,
                    custom_title: None,
                    first_prompt: None,
                    git_branch: None,
                    cwd: None,
                    tag: None,
                    created_at: None,
                }),
            }
        }
    }))
    .buffer_unordered(STORE_LIST_LOAD_CONCURRENCY)
    .collect()
    .await;

    results.into_iter().flatten().collect()
}

/// List sessions from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::list_sessions`]. Loads each
/// session's entries to derive a real summary via the same lite-parse used
/// by the filesystem path, so disk and store paths produce identical
/// results for the same transcript content.
///
/// `include_worktrees` is a filesystem concept and is not honored on the
/// store path — the store operates on a single `project_key`.
///
/// # Errors
/// Returns [`ClaudeError::InvalidConfig`] if `session_store` implements
/// neither [`SessionStore::list_session_summaries`] nor
/// [`SessionStore::list_sessions`].
pub async fn list_sessions_from_store(
    store: &dyn SessionStore,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SDKSessionInfo>> {
    let project_path = canonicalize_path(directory.unwrap_or("."));
    let project_key = sanitize_path(&project_path);
    let has_list_sessions = store.supports_list_sessions();

    // Fast path: if the store maintains incremental summaries, fetch them
    // in one call instead of N per-session load()s.
    if store.supports_list_session_summaries() {
        match store.list_session_summaries(&project_key).await {
            Ok(summaries) => {
                return list_sessions_via_summaries(
                    store,
                    summaries,
                    has_list_sessions,
                    &project_key,
                    &project_path,
                    directory,
                    limit,
                    offset,
                )
                .await;
            }
            Err(SessionStoreError::NotImplemented) => {}
            Err(e) => return Err(store_err(e)),
        }
    }

    if !has_list_sessions {
        return Err(ClaudeError::InvalidConfig(
            "session_store implements neither list_session_summaries() nor list_sessions() \
             -- cannot list sessions. Provide a store with at least one of those methods."
                .to_string(),
        ));
    }

    // Copy — store.list_sessions() may return a reference to internal state.
    let listing = store.list_sessions(&project_key).await.map_err(store_err)?;
    let results = derive_infos_via_load(store, &listing, directory, &project_path).await;
    Ok(apply_sort_limit_offset(results, limit, offset))
}

/// Reads metadata for a single session from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::get_session_info`]. Returns
/// `None` if the session is not found, `session_id` is not a valid UUID,
/// the session is a sidechain session, or it has no extractable summary.
pub async fn get_session_info_from_store(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Option<SDKSessionInfo>> {
    if validate_uuid(session_id).is_none() {
        return Ok(None);
    }
    let Some(jsonl) = load_store_entries_as_jsonl(store, session_id, directory).await? else {
        return Ok(None);
    };
    let lite = jsonl_to_lite(&jsonl, mtime_from_jsonl_tail(&jsonl));
    let project_path = canonicalize_path(directory.unwrap_or("."));
    Ok(parse_session_info_from_lite(session_id, &lite, Some(&project_path)))
}

/// Reads a session's conversation messages from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::get_session_messages`]. Feeds
/// `session_store.load()` results directly into the chain builder — no
/// JSONL round-trip. Empty list if the session is not found or
/// `session_id` is invalid.
pub async fn get_session_messages_from_store(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if validate_uuid(session_id).is_none() {
        return Ok(Vec::new());
    }
    let project_key = project_key_for_directory(directory);
    let key = SessionKey::new(project_key, session_id);
    let entries = store.load(&key).await.map_err(store_err)?;
    let Some(entries) = entries else { return Ok(Vec::new()) };
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let values: Vec<Value> = entries.iter().map(SessionStoreEntry::to_value).collect();
    Ok(entries_to_session_messages(&filter_transcript_entries(&values), limit, offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::InMemorySessionStore;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn user_entry(uuid: &str, parent: Option<&str>) -> SessionStoreEntry {
        let mut fields = serde_json::Map::new();
        fields.insert("uuid".to_string(), Value::String(uuid.to_string()));
        fields.insert("message".to_string(), serde_json::json!({"content": "hi"}));
        if let Some(p) = parent {
            fields.insert("parentUuid".to_string(), Value::String(p.to_string()));
        }
        SessionStoreEntry::new("user", fields)
    }

    #[tokio::test]
    async fn get_session_info_from_store_rejects_invalid_uuid() {
        let store = InMemorySessionStore::new();
        assert!(get_session_info_from_store(&store, "not-a-uuid", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_session_info_from_store_missing_session_returns_none() {
        let store = InMemorySessionStore::new();
        assert!(get_session_info_from_store(&store, SESSION_ID, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_session_info_from_store_parses_summary() {
        let store = InMemorySessionStore::new();
        let key = SessionKey::new(project_key_for_directory(None), SESSION_ID);
        store.append(&key, vec![user_entry("u1", None)]).await.unwrap();

        let info = get_session_info_from_store(&store, SESSION_ID, None).await.unwrap().unwrap();
        assert_eq!(info.session_id, SESSION_ID);
        assert_eq!(info.first_prompt.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn get_session_messages_from_store_builds_chain() {
        let store = InMemorySessionStore::new();
        let key = SessionKey::new(project_key_for_directory(None), SESSION_ID);
        store.append(&key, vec![user_entry("u1", None), user_entry("u2", Some("u1"))]).await.unwrap();

        let messages = get_session_messages_from_store(&store, SESSION_ID, None, None, 0).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "u1");
        assert_eq!(messages[1].uuid, "u2");
    }

    #[tokio::test]
    async fn get_session_messages_from_store_missing_returns_empty() {
        let store = InMemorySessionStore::new();
        assert!(get_session_messages_from_store(&store, SESSION_ID, None, None, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_from_store_slow_path_derives_summaries() {
        struct ListOnly(InMemorySessionStore);
        #[async_trait::async_trait]
        impl SessionStore for ListOnly {
            async fn append(
                &self,
                key: &SessionKey,
                entries: Vec<SessionStoreEntry>,
            ) -> std::result::Result<(), SessionStoreError> {
                self.0.append(key, entries).await
            }
            async fn load(
                &self,
                key: &SessionKey,
            ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
                self.0.load(key).await
            }
            async fn list_sessions(
                &self,
                project_key: &str,
            ) -> std::result::Result<Vec<SessionStoreListEntry>, SessionStoreError> {
                self.0.list_sessions(project_key).await
            }
            fn supports_list_sessions(&self) -> bool {
                true
            }
        }

        let store = ListOnly(InMemorySessionStore::new());
        let directory = Some("/tmp/list-sessions-slow-path");
        let project_key = project_key_for_directory(directory);
        let key = SessionKey::new(project_key, SESSION_ID);
        store.append(&key, vec![user_entry("u1", None)]).await.unwrap();

        let results = list_sessions_from_store(&store, directory, None, 0).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, SESSION_ID);
    }

    #[tokio::test]
    async fn list_sessions_from_store_errors_without_either_capability() {
        struct Bare;
        #[async_trait::async_trait]
        impl SessionStore for Bare {
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
        let err = list_sessions_from_store(&Bare, None, None, 0).await.unwrap_err();
        assert!(err.to_string().contains("list_session_summaries"));
    }
}
