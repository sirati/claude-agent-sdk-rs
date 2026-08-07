//! `list_session_summaries()` fast path for [`super::listing_store::list_sessions_from_store`].
//!
//! Ported from the `_store_implements(session_store, "list_session_summaries")`
//! branch of upstream's `list_sessions_from_store` in `_internal/sessions.py`.
//! Split out from [`super::listing_store`] because the gap-fill /
//! staleness-check logic is a distinct, sizable concern from the plain
//! per-session `load()` path there.
//!
//! ## Staleness check
//!
//! A summary sidecar is "fresh" only if `summary.mtime >= list_sessions()`'s
//! reported `mtime` for that session (see [`super::types::SessionSummaryEntry`]'s
//! doc comment for why the two must share a clock). Fresh summaries are used
//! as-is — zero `load()` calls. Sessions with a missing or stale sidecar are
//! routed through [`super::listing_store::derive_infos_via_load`] (the same
//! gap-fill path as `list_sessions()` not reporting a sidecar at all), and
//! that re-fold only runs on the *paginated* page, not the whole listing —
//! so a store with 500 sessions lacking sidecars and `limit=10` issues at
//! most 10 `load()` calls, not 500.

use std::collections::{HashMap, HashSet};

use crate::errors::Result;

use super::info::SDKSessionInfo;
use super::listing_store::{derive_infos_via_load, store_err};
use super::store::SessionStore;
use super::summary::summary_entry_to_sdk_info;
use super::types::{SessionStoreListEntry, SessionSummaryEntry};

/// A session's position in the sorted/paginated result set, with its
/// summary-derived info if already resolved (`None` routes through
/// gap-fill).
struct Slot {
    mtime: i64,
    session_id: String,
    info: Option<SDKSessionInfo>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn list_sessions_via_summaries(
    store: &dyn SessionStore,
    summaries: Vec<SessionSummaryEntry>,
    has_list_sessions: bool,
    project_key: &str,
    project_path: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SDKSessionInfo>> {
    // Gap-fill requires list_sessions(): if the store implements
    // list_session_summaries but not list_sessions, sessions without a
    // sidecar cannot be discovered and are absent from the result.
    let (listing, known_mtimes): (Vec<SessionStoreListEntry>, HashMap<String, i64>) = if has_list_sessions {
        let listing = store.list_sessions(project_key).await.map_err(store_err)?;
        let known = listing.iter().map(|e| (e.session_id.clone(), e.mtime)).collect();
        (listing, known)
    } else {
        (Vec::new(), HashMap::new())
    };

    // Build a unified slot list. Fresh summaries (mtime >= the session's
    // current mtime from list_sessions) get their info up front; sessions
    // present in list_sessions() but missing OR with a stale sidecar get a
    // placeholder slot routed through gap-fill so the fold is recomputed
    // from source entries. Summary-backed sidechain/empty sessions are
    // dropped here (free — already determined) so they don't consume
    // offset/limit positions.
    let mut slots: Vec<Slot> = Vec::new();
    let mut fresh_summary_ids: HashSet<String> = HashSet::new();

    for s in summaries {
        let session_id = s.session_id.clone();
        if has_list_sessions {
            match known_mtimes.get(&session_id) {
                // Summary for a session list_sessions() no longer reports —
                // drop it.
                None => continue,
                // Stale sidecar — let gap-fill re-fold from source.
                Some(&known) if s.mtime < known => continue,
                _ => {}
            }
        }
        if let Some(info) = summary_entry_to_sdk_info(&s, Some(project_path)) {
            slots.push(Slot { mtime: s.mtime, session_id: session_id.clone(), info: Some(info) });
        }
        fresh_summary_ids.insert(session_id);
    }
    if has_list_sessions {
        slots.extend(
            listing
                .iter()
                .filter(|e| !fresh_summary_ids.contains(&e.session_id))
                .map(|e| Slot { mtime: e.mtime, session_id: e.session_id.clone(), info: None }),
        );
    }

    // Paginate BEFORE per-session load so gap-fill load() count is bounded
    // by page size, not total missing.
    slots.sort_by_key(|sl| std::cmp::Reverse(sl.mtime));
    let mut page = if offset > 0 {
        let start = offset.min(slots.len());
        slots.split_off(start)
    } else {
        slots
    };
    if let Some(l) = limit {
        if l > 0 {
            page.truncate(l);
        }
    }

    let to_fill: Vec<SessionStoreListEntry> = page
        .iter()
        .filter(|sl| sl.info.is_none())
        .map(|sl| SessionStoreListEntry { session_id: sl.session_id.clone(), mtime: sl.mtime })
        .collect();
    if !to_fill.is_empty() {
        let filled = derive_infos_via_load(store, &to_fill, directory, project_path).await;
        let by_session_id: HashMap<String, SDKSessionInfo> =
            filled.into_iter().map(|info| (info.session_id.clone(), info)).collect();
        for slot in &mut page {
            if slot.info.is_none() {
                slot.info = by_session_id.get(&slot.session_id).cloned();
            }
        }
    }

    // Gap-fill placeholders that resolved to None (sidechain / no
    // extractable summary after load) are dropped here, AFTER pagination —
    // that case alone can short-page.
    Ok(page.into_iter().filter_map(|sl| sl.info).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::InMemorySessionStore;
    use crate::session::{SessionKey, SessionStoreEntry};
    use serde_json::Value;

    fn user_entry(uuid: &str, content: &str) -> SessionStoreEntry {
        let mut fields = serde_json::Map::new();
        fields.insert("uuid".to_string(), Value::String(uuid.to_string()));
        fields.insert("message".to_string(), serde_json::json!({"content": content}));
        SessionStoreEntry::new("user", fields)
    }

    #[tokio::test]
    async fn fast_path_uses_fresh_summary_without_load() {
        let store = InMemorySessionStore::new();
        let directory = Some("/tmp/fast-path-fresh");
        let project_key = crate::session::project_key_for_directory(directory);
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let key = SessionKey::new(project_key.clone(), session_id);
        store.append(&key, vec![user_entry("u1", "fresh summary test")]).await.unwrap();

        let results = crate::session::listing_store::list_sessions_from_store(&store, directory, None, 0)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].first_prompt.as_deref(), Some("fresh summary test"));
    }

    #[tokio::test]
    async fn fast_path_drops_sessions_missing_from_list_sessions() {
        // A summary with no corresponding list_sessions() entry (simulated
        // via a store whose list_sessions always returns empty, even though
        // it DOES implement list_sessions) must be dropped, not surfaced —
        // this only kicks in when has_list_sessions is true; a store that
        // doesn't implement list_sessions at all trusts summaries as-is
        // (see the doc comment: gap-fill/orphan-check both require it).
        struct SummariesOnlyListSessionsEmpty(InMemorySessionStore);
        #[async_trait::async_trait]
        impl SessionStore for SummariesOnlyListSessionsEmpty {
            async fn append(
                &self,
                key: &SessionKey,
                entries: Vec<SessionStoreEntry>,
            ) -> std::result::Result<(), super::super::store::SessionStoreError> {
                self.0.append(key, entries).await
            }
            async fn load(
                &self,
                key: &SessionKey,
            ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, super::super::store::SessionStoreError> {
                self.0.load(key).await
            }
            async fn list_sessions(
                &self,
                _project_key: &str,
            ) -> std::result::Result<Vec<SessionStoreListEntry>, super::super::store::SessionStoreError> {
                Ok(Vec::new())
            }
            fn supports_list_sessions(&self) -> bool {
                true
            }
            async fn list_session_summaries(
                &self,
                project_key: &str,
            ) -> std::result::Result<Vec<SessionSummaryEntry>, super::super::store::SessionStoreError> {
                self.0.list_session_summaries(project_key).await
            }
            fn supports_list_session_summaries(&self) -> bool {
                true
            }
        }

        let inner = InMemorySessionStore::new();
        let directory = Some("/tmp/fast-path-orphan");
        let project_key = crate::session::project_key_for_directory(directory);
        let session_id = "550e8400-e29b-41d4-a716-446655440000";
        let key = SessionKey::new(project_key.clone(), session_id);
        inner.append(&key, vec![user_entry("u1", "orphaned")]).await.unwrap();

        let store = SummariesOnlyListSessionsEmpty(inner);
        let results = crate::session::listing_store::list_sessions_from_store(&store, directory, None, 0)
            .await
            .unwrap();
        assert!(results.is_empty());
    }
}
