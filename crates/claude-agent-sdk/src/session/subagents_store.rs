//! `SessionStore`-backed subagent listing/reading: `list_subagents_from_store()`,
//! `get_subagent_messages_from_store()`.
//!
//! Ported from `list_subagents_from_store` and
//! `get_subagent_messages_from_store` in upstream `_internal/sessions.py`.

use std::collections::HashSet;

use serde_json::Value;

use crate::errors::{ClaudeError, Result};

use super::info::SessionMessage;
use super::listing_store::store_err;
use super::local::validate_uuid;
use super::message_paging::entries_to_subagent_messages;
use super::project_dir::project_key_for_directory;
use super::store::SessionStore;
use super::transcript::filter_transcript_entries;
use super::types::{SessionKey, SessionListSubkeysKey, SessionStoreEntry};

/// Lists subagent IDs for a session from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::list_subagents`]. Empty list
/// if `session_id` is invalid or the session has no subagents.
///
/// # Errors
/// Returns [`ClaudeError::InvalidConfig`] if `session_store` does not
/// implement [`SessionStore::list_subkeys`].
pub async fn list_subagents_from_store(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&str>,
) -> Result<Vec<String>> {
    if validate_uuid(session_id).is_none() {
        return Ok(Vec::new());
    }
    if !store.supports_list_subkeys() {
        return Err(ClaudeError::InvalidConfig(
            "session_store does not implement list_subkeys() -- cannot list subagents. \
             Provide a store with a list_subkeys() method."
                .to_string(),
        ));
    }
    let project_key = project_key_for_directory(directory);
    let subkeys = store
        .list_subkeys(&SessionListSubkeysKey { project_key, session_id: session_id.to_string() })
        .await
        .map_err(store_err)?;

    let mut seen: HashSet<String> = HashSet::new();
    let mut ids = Vec::new();
    for subpath in subkeys {
        if !subpath.starts_with("subagents/") {
            continue;
        }
        let last = subpath.rsplit('/').next().unwrap_or(&subpath);
        if let Some(agent_id) = last.strip_prefix("agent-") {
            if seen.insert(agent_id.to_string()) {
                ids.push(agent_id.to_string());
            }
        }
    }
    Ok(ids)
}

/// Reads a subagent's conversation messages from a [`SessionStore`].
///
/// Async, store-backed counterpart to [`super::get_subagent_messages`].
/// Subagents may live at `subagents/agent-<id>` or nested under
/// `subagents/workflows/<runId>/agent-<id>`. Scans subkeys when the store
/// implements [`SessionStore::list_subkeys`]; otherwise tries the direct
/// (unnested) path. Empty list if the session/subagent is not found.
pub async fn get_subagent_messages_from_store(
    store: &dyn SessionStore,
    session_id: &str,
    agent_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if validate_uuid(session_id).is_none() || agent_id.is_empty() {
        return Ok(Vec::new());
    }
    let project_key = project_key_for_directory(directory);

    let mut subpath = format!("subagents/agent-{agent_id}");
    if store.supports_list_subkeys() {
        let subkeys = store
            .list_subkeys(&SessionListSubkeysKey { project_key: project_key.clone(), session_id: session_id.to_string() })
            .await
            .map_err(store_err)?;
        let target = format!("agent-{agent_id}");
        let Some(matched) =
            subkeys.into_iter().find(|sk| sk.starts_with("subagents/") && sk.rsplit('/').next() == Some(target.as_str()))
        else {
            return Ok(Vec::new());
        };
        subpath = matched;
    }

    let key = SessionKey::with_subpath(project_key, session_id, subpath);
    let entries = store.load(&key).await.map_err(store_err)?;
    let Some(entries) = entries else { return Ok(Vec::new()) };
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Drop synthetic agent_metadata entries injected by the mirror hook —
    // they describe the .meta.json sidecar, not transcript lines.
    let transcript: Vec<&SessionStoreEntry> = entries.iter().filter(|e| e.type_ != "agent_metadata").collect();
    if transcript.is_empty() {
        return Ok(Vec::new());
    }

    let values: Vec<Value> = transcript.iter().map(|e| e.to_value()).collect();
    Ok(entries_to_subagent_messages(&filter_transcript_entries(&values), limit, offset))
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
    async fn list_subagents_from_store_rejects_invalid_uuid() {
        let store = InMemorySessionStore::new();
        assert!(list_subagents_from_store(&store, "not-a-uuid", None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_subagents_from_store_errors_without_list_subkeys() {
        struct NoSubkeys;
        #[async_trait::async_trait]
        impl SessionStore for NoSubkeys {
            async fn append(
                &self,
                _key: &SessionKey,
                _entries: Vec<SessionStoreEntry>,
            ) -> std::result::Result<(), super::super::store::SessionStoreError> {
                Ok(())
            }
            async fn load(
                &self,
                _key: &SessionKey,
            ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, super::super::store::SessionStoreError> {
                Ok(None)
            }
        }
        let err = list_subagents_from_store(&NoSubkeys, SESSION_ID, None).await.unwrap_err();
        assert!(err.to_string().contains("list_subkeys"));
    }

    #[tokio::test]
    async fn list_subagents_and_get_messages_round_trip() {
        let store = InMemorySessionStore::new();
        let directory = Some("/tmp/subagents-store-proj");
        let project_key = project_key_for_directory(directory);
        let key = SessionKey::with_subpath(project_key, SESSION_ID, "subagents/agent-abc");
        store.append(&key, vec![user_entry("u1", None), user_entry("u2", Some("u1"))]).await.unwrap();

        let ids = list_subagents_from_store(&store, SESSION_ID, directory).await.unwrap();
        assert_eq!(ids, vec!["abc".to_string()]);

        let messages = get_subagent_messages_from_store(&store, SESSION_ID, "abc", directory, None, 0).await.unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].uuid, "u1");
        assert_eq!(messages[1].uuid, "u2");
    }

    #[tokio::test]
    async fn get_subagent_messages_from_store_missing_agent_returns_empty() {
        let store = InMemorySessionStore::new();
        let messages = get_subagent_messages_from_store(&store, SESSION_ID, "missing", None, None, 0).await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn get_subagent_messages_from_store_rejects_empty_agent_id() {
        let store = InMemorySessionStore::new();
        assert!(get_subagent_messages_from_store(&store, SESSION_ID, "", None, None, 0).await.unwrap().is_empty());
    }
}
