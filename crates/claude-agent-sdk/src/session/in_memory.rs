//! In-memory reference implementation of [`SessionStore`].

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::store::{SessionStore, SessionStoreError};
use super::summary::fold_session_summary;
use super::types::{
    SessionKey, SessionListSubkeysKey, SessionStoreEntry, SessionStoreListEntry,
    SessionSummaryEntry,
};

fn key_to_string(key: &SessionKey) -> String {
    match &key.subpath {
        Some(subpath) if !subpath.is_empty() => {
            format!("{}/{}/{}", key.project_key, key.session_id, subpath)
        }
        _ => format!("{}/{}", key.project_key, key.session_id),
    }
}

#[derive(Default)]
struct Inner {
    store: HashMap<String, Vec<SessionStoreEntry>>,
    mtimes: HashMap<String, i64>,
    summaries: HashMap<(String, String), SessionSummaryEntry>,
    last_mtime: i64,
}

impl Inner {
    /// Storage write time for this adapter, in Unix epoch ms.
    ///
    /// Guaranteed strictly monotonically increasing across calls within the
    /// process so back-to-back appends always produce distinct mtimes (real
    /// storage backends — file mtime on modern filesystems, S3
    /// `LastModified`, Postgres `updated_at` — get this property for free
    /// from their commit ordering).
    fn next_mtime(&mut self) -> i64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let stamped = if now_ms <= self.last_mtime {
            self.last_mtime + 1
        } else {
            now_ms
        };
        self.last_mtime = stamped;
        stamped
    }
}

/// In-memory [`SessionStore`] implementation for testing and development.
///
/// Stores entries in a map keyed by a composite `project_key/session_id`
/// string (with an optional `/subpath` suffix). Not suitable for
/// production — data is lost when the process exits.
#[derive(Default)]
pub struct InMemorySessionStore {
    inner: Mutex<Inner>,
}

impl InMemorySessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Test helper — get all entries for a key (empty vec if absent).
    pub async fn get_entries(&self, key: &SessionKey) -> Vec<SessionStoreEntry> {
        let inner = self.inner.lock().await;
        inner.store.get(&key_to_string(key)).cloned().unwrap_or_default()
    }

    /// Test helper — number of stored sessions (main transcripts only).
    pub async fn size(&self) -> usize {
        let inner = self.inner.lock().await;
        inner
            .store
            .keys()
            .filter(|k| match k.find('/') {
                Some(first_slash) => !k[first_slash + 1..].contains('/'),
                None => false,
            })
            .count()
    }

    /// Test helper — clear all stored data.
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        inner.store.clear();
        inner.mtimes.clear();
        inner.summaries.clear();
        inner.last_mtime = 0;
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn append(
        &self,
        key: &SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> Result<(), SessionStoreError> {
        let mut inner = self.inner.lock().await;
        let k = key_to_string(key);
        inner.store.entry(k.clone()).or_default().extend(entries.iter().cloned());
        let now_ms = inner.next_mtime();

        // Maintain the per-session summary sidecar incrementally so
        // list_session_summaries() never re-reads. Subagent subpaths don't
        // contribute to the main session's summary.
        if key.subpath.is_none() {
            let sk = (key.project_key.clone(), key.session_id.clone());
            let mut folded = fold_session_summary(inner.summaries.get(&sk), key, &entries);
            // Stamp the sidecar with this adapter's storage write time — the
            // SAME clock list_sessions() exposes below. SessionSummaryEntry
            // mtime is contractually storage write time (not entry time), so
            // the fast-path staleness check callers build on top works.
            folded.mtime = now_ms;
            inner.summaries.insert(sk, folded);
        }
        inner.mtimes.insert(k, now_ms);
        Ok(())
    }

    async fn load(&self, key: &SessionKey) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        let inner = self.inner.lock().await;
        Ok(inner.store.get(&key_to_string(key)).cloned())
    }

    async fn list_sessions(
        &self,
        project_key: &str,
    ) -> Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        let inner = self.inner.lock().await;
        let prefix = format!("{project_key}/");
        let mut results = Vec::new();
        for k in inner.store.keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                // Only include main transcripts (no subpath, so no second '/')
                if !rest.contains('/') {
                    results.push(SessionStoreListEntry {
                        session_id: rest.to_string(),
                        mtime: inner.mtimes.get(k).copied().unwrap_or(0),
                    });
                }
            }
        }
        Ok(results)
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    async fn list_session_summaries(
        &self,
        project_key: &str,
    ) -> Result<Vec<SessionSummaryEntry>, SessionStoreError> {
        let inner = self.inner.lock().await;
        Ok(inner
            .summaries
            .iter()
            .filter(|((pk, _), _)| pk == project_key)
            .map(|(_, summary)| summary.clone())
            .collect())
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }

    async fn delete(&self, key: &SessionKey) -> Result<(), SessionStoreError> {
        let mut inner = self.inner.lock().await;
        let k = key_to_string(key);
        inner.store.remove(&k);
        inner.mtimes.remove(&k);

        // Deleting the main transcript cascades to its subkeys (subagent
        // transcripts, metadata) so they aren't orphaned. A targeted delete
        // with an explicit subpath removes only that one entry.
        if key.subpath.is_none() {
            inner
                .summaries
                .remove(&(key.project_key.clone(), key.session_id.clone()));
            let prefix = format!("{}/{}/", key.project_key, key.session_id);
            let doomed: Vec<String> = inner
                .store
                .keys()
                .filter(|sk| sk.starts_with(&prefix))
                .cloned()
                .collect();
            for sk in doomed {
                inner.store.remove(&sk);
                inner.mtimes.remove(&sk);
            }
        }
        Ok(())
    }

    fn supports_delete(&self) -> bool {
        true
    }

    async fn list_subkeys(&self, key: &SessionListSubkeysKey) -> Result<Vec<String>, SessionStoreError> {
        let inner = self.inner.lock().await;
        let prefix = format!("{}/{}/", key.project_key, key.session_id);
        Ok(inner
            .store
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}
