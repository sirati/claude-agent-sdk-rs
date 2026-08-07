//! Reusable conformance test suite for [`super::store::SessionStore`]
//! adapters.
//!
//! Call [`run_session_store_conformance`] from an async test to assert the
//! same 14 behavioral contracts upstream's `session_store_conformance.py`
//! enforces. Contracts for optional methods (`list_sessions`,
//! `list_session_summaries`, `delete`, `list_subkeys`) are skipped when
//! named in `skip_optional`, or automatically when the store's `supports_*`
//! flag reports `false` (see [`super::store::SessionStore`]'s docs on why
//! Rust needs an explicit flag where Python could duck-type probe).
//!
//! # Example
//! ```ignore
//! use std::sync::Arc;
//! use claude_agent_sdk::session::conformance::run_session_store_conformance;
//! use claude_agent_sdk::session::SessionStore;
//!
//! #[tokio::test]
//! async fn my_store_conformance() {
//!     run_session_store_conformance(
//!         || async { Arc::new(MyStore::new()) as Arc<dyn SessionStore> },
//!         &[],
//!     )
//!     .await;
//! }
//! ```

mod optional;
mod required;

use std::future::Future;
use std::sync::Arc;

use super::store::SessionStore;
use super::types::{SessionKey, SessionStoreEntry};

/// Names one of [`SessionStore`]'s optional methods, for use with
/// `skip_optional` in [`run_session_store_conformance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalContract {
    /// [`SessionStore::list_sessions`].
    ListSessions,
    /// [`SessionStore::list_session_summaries`].
    ListSessionSummaries,
    /// [`SessionStore::delete`].
    Delete,
    /// [`SessionStore::list_subkeys`].
    ListSubkeys,
}

/// Assert the [`SessionStore`] behavioral contracts against fresh stores
/// built by `make_store` (invoked once per contract, for isolation).
///
/// Contracts for optional methods are skipped when named in
/// `skip_optional`, or automatically when the store's corresponding
/// `supports_*` flag returns `false`.
pub async fn run_session_store_conformance<F, Fut>(make_store: F, skip_optional: &[OptionalContract])
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let probe = make_store().await;
    let has_list_sessions =
        probe.supports_list_sessions() && !skip_optional.contains(&OptionalContract::ListSessions);
    let has_list_summaries = probe.supports_list_session_summaries()
        && !skip_optional.contains(&OptionalContract::ListSessionSummaries);
    let has_delete = probe.supports_delete() && !skip_optional.contains(&OptionalContract::Delete);
    let has_list_subkeys = probe.supports_list_subkeys()
        && !skip_optional.contains(&OptionalContract::ListSubkeys);
    drop(probe);

    required::run(&make_store, has_list_sessions).await;
    optional::run(
        &make_store,
        has_list_sessions,
        has_list_summaries,
        has_delete,
        has_list_subkeys,
    )
    .await;
}

/// Build a main-transcript [`SessionKey`] for test fixtures.
pub(super) fn key(project_key: &str, session_id: &str) -> SessionKey {
    SessionKey::new(project_key, session_id)
}

/// Build a [`SessionStoreEntry`] satisfying the `type` requirement from a
/// JSON object literal. Adapters must treat entries as opaque pass-through
/// blobs; the value of `type` is irrelevant to the contracts under test.
pub(super) fn entry(fields: serde_json::Value) -> SessionStoreEntry {
    let mut obj = match fields {
        serde_json::Value::Object(m) => m,
        other => panic!("test entry fields must be a JSON object, got {other:?}"),
    };
    let type_ = obj
        .remove("type")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "x".to_string());
    SessionStoreEntry::new(type_, obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::InMemorySessionStore;

    #[tokio::test]
    async fn in_memory_store_passes_full_conformance_suite() {
        run_session_store_conformance(
            || async { Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore> },
            &[],
        )
        .await;
    }

    #[tokio::test]
    async fn minimal_store_implementing_only_required_methods_passes() {
        use super::super::store::SessionStoreError;
        use async_trait::async_trait;
        use std::collections::HashMap;
        use tokio::sync::Mutex;

        #[derive(Default)]
        struct MinimalStore {
            data: Mutex<HashMap<String, Vec<SessionStoreEntry>>>,
        }

        fn slot(key: &SessionKey) -> String {
            format!(
                "{}/{}/{}",
                key.project_key,
                key.session_id,
                key.subpath.clone().unwrap_or_default()
            )
        }

        #[async_trait]
        impl SessionStore for MinimalStore {
            async fn append(
                &self,
                key: &SessionKey,
                entries: Vec<SessionStoreEntry>,
            ) -> Result<(), SessionStoreError> {
                self.data.lock().await.entry(slot(key)).or_default().extend(entries);
                Ok(())
            }

            async fn load(
                &self,
                key: &SessionKey,
            ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
                Ok(self.data.lock().await.get(&slot(key)).cloned())
            }
        }

        // No skip_optional needed — MinimalStore's supports_* flags all
        // default to false, so optional contracts auto-skip.
        run_session_store_conformance(
            || async { Arc::new(MinimalStore::default()) as Arc<dyn SessionStore> },
            &[],
        )
        .await;
    }
}
