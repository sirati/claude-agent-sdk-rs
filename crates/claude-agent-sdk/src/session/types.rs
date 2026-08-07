//! Plain data types for the session-store subsystem.
//!
//! Ported from the `SessionKey`/`SessionStoreEntry`/... section of upstream
//! `types.py`. These are the wire/storage shapes adapters exchange with the
//! SDK; the adapter contract itself lives in [`super::store`].

use serde::{Deserialize, Serialize};

/// Identifies a session transcript or subagent transcript in a store.
///
/// Main transcripts have no `subpath`; subagent transcripts include a
/// `subpath` like `"subagents/agent-{id}"` that mirrors the on-disk
/// directory structure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    /// Caller-defined scope. Default: sanitized cwd. Multi-tenant
    /// deployments should set this to a tenant ID or project name. Paths
    /// longer than 200 characters are truncated and suffixed with a
    /// portable djb2 hash so the same path yields the same key across
    /// runtimes.
    pub project_key: String,
    /// Unique session identifier.
    pub session_id: String,
    /// Omit for the main transcript; set for subagent files. Empty string
    /// is invalid — use `None` for the main transcript. Opaque to the
    /// adapter — just use it as a storage key suffix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

impl SessionKey {
    /// Build a main-transcript key (no `subpath`).
    pub fn new(project_key: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            project_key: project_key.into(),
            session_id: session_id.into(),
            subpath: None,
        }
    }

    /// Build a subagent-transcript key with the given `subpath`.
    pub fn with_subpath(
        project_key: impl Into<String>,
        session_id: impl Into<String>,
        subpath: impl Into<String>,
    ) -> Self {
        Self {
            project_key: project_key.into(),
            session_id: session_id.into(),
            subpath: Some(subpath.into()),
        }
    }
}

/// One JSONL transcript line as observed by a [`super::store::SessionStore`]
/// adapter.
///
/// The concrete shape is the CLI's on-disk transcript format (a large
/// discriminated union). That union is internal, so this is a minimal
/// structural supertype — adapters should treat entries as pass-through
/// blobs. Round-tripping `serde_json::to_value`/`from_value` is the only
/// required invariant, so unknown fields are preserved verbatim in `extra`
/// rather than being modeled as a fixed struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStoreEntry {
    /// Discriminant for the entry's shape (e.g. `"user"`, `"assistant"`,
    /// `"summary"`). Always present.
    #[serde(rename = "type")]
    pub type_: String,
    /// Every other field on the entry, passed through opaquely. Includes
    /// `uuid` and `timestamp` when present — use [`Self::uuid`] /
    /// [`Self::timestamp`] rather than reaching into this map directly.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl SessionStoreEntry {
    /// Build an entry from its `type` discriminant plus opaque fields.
    pub fn new(type_: impl Into<String>, extra: serde_json::Map<String, serde_json::Value>) -> Self {
        Self {
            type_: type_.into(),
            extra,
        }
    }

    /// The entry's `uuid`, if present. Most entries carry a stable `uuid`
    /// that adapters should treat as an idempotency key (upsert /
    /// ignore-duplicate) in `append`. Entries without one (e.g. titles,
    /// tags, mode markers) should be appended without dedup.
    pub fn uuid(&self) -> Option<&str> {
        self.extra.get("uuid").and_then(|v| v.as_str())
    }

    /// The entry's ISO-8601 `timestamp`, if present.
    pub fn timestamp(&self) -> Option<&str> {
        self.extra.get("timestamp").and_then(|v| v.as_str())
    }

    /// Reconstruct the entry as a plain JSON object with `type` merged back
    /// into `extra`.
    ///
    /// Used by the store-backed listing/read path (`session/listing_store.rs`,
    /// `session/subagents_store.rs`) to feed store-loaded entries into the
    /// `serde_json::Value`-based transcript parser
    /// ([`super::transcript::filter_transcript_entries`]) shared with the
    /// local-disk path.
    pub(crate) fn to_value(&self) -> serde_json::Value {
        let mut obj = self.extra.clone();
        obj.insert("type".to_string(), serde_json::Value::String(self.type_.clone()));
        serde_json::Value::Object(obj)
    }
}

/// Entry returned by [`super::store::SessionStore::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStoreListEntry {
    /// Session identifier.
    pub session_id: String,
    /// Last-modified time in Unix epoch milliseconds. Adapters without a
    /// native modification time (e.g. Redis) must maintain their own index.
    pub mtime: i64,
}

/// Incrementally-maintained session summary.
///
/// Stores obtain this from `fold_session_summary` inside
/// [`super::store::SessionStore::append`] and persist it verbatim; they
/// return the full set from
/// [`super::store::SessionStore::list_session_summaries`]. The `data` field
/// is opaque SDK-owned state — stores MUST NOT interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummaryEntry {
    /// Session identifier.
    pub session_id: String,
    /// Storage write time of the sidecar, in Unix epoch milliseconds. Must
    /// use the same clock source as the `mtime` returned by
    /// [`super::store::SessionStore::list_sessions`] for this session —
    /// typically file mtime, S3 `LastModified`, Postgres `updated_at`, or
    /// whatever native timestamp the adapter surfaces. Do NOT derive this
    /// from entry ISO timestamps: adapters that write in batches with any
    /// persist latency (every real backend) would report storage times
    /// strictly later than the last entry's timestamp, making every sidecar
    /// appear stale and defeating the fast-path staleness check callers
    /// build on top of `list_sessions`/`list_session_summaries`.
    /// `fold_session_summary` preserves whatever `mtime` the caller passes
    /// in via `prev` and does not set it itself; stamp it after persisting.
    pub mtime: i64,
    /// Opaque SDK-owned summary state. Persist verbatim; do not interpret.
    pub data: serde_json::Value,
}

/// Key argument to [`super::store::SessionStore::list_subkeys`] (no
/// `subpath`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionListSubkeysKey {
    /// Caller-defined scope, see [`SessionKey::project_key`].
    pub project_key: String,
    /// Session identifier.
    pub session_id: String,
}

impl From<&SessionKey> for SessionListSubkeysKey {
    /// Drops `subpath` — callers typically have a main-transcript
    /// [`SessionKey`] in hand and want its subkeys.
    fn from(key: &SessionKey) -> Self {
        Self {
            project_key: key.project_key.clone(),
            session_id: key.session_id.clone(),
        }
    }
}

/// Controls when transcript-mirror entries are flushed to a
/// [`super::store::SessionStore`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStoreFlushMode {
    /// Buffer entries and flush once per turn (on the `result` message) or
    /// when the pending buffer exceeds 500 entries / 1 MiB. Keeps adapter
    /// latency off the streaming hot path. Default.
    #[default]
    Batched,
    /// Trigger a background flush after every `transcript_mirror` frame so
    /// `SessionStore::append()` sees entries in near real time. Appends are
    /// still serialized in enqueue order; a slow adapter will not stall the
    /// read loop but will see frames coalesced while it is busy.
    Eager,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_serializes_without_subpath_field() {
        let key = SessionKey::new("proj", "sess");
        let json = serde_json::to_value(&key).unwrap();
        assert_eq!(json["project_key"], "proj");
        assert_eq!(json["session_id"], "sess");
        assert!(json.get("subpath").is_none());
    }

    #[test]
    fn session_store_entry_round_trips_unknown_fields() {
        let raw = serde_json::json!({
            "type": "user",
            "uuid": "abc",
            "timestamp": "2024-01-01T00:00:00.000Z",
            "someUnknownField": {"nested": true},
        });
        let entry: SessionStoreEntry = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(entry.type_, "user");
        assert_eq!(entry.uuid(), Some("abc"));
        assert_eq!(entry.timestamp(), Some("2024-01-01T00:00:00.000Z"));

        let round_tripped = serde_json::to_value(&entry).unwrap();
        assert_eq!(round_tripped, raw);
    }

    #[test]
    fn session_store_flush_mode_serde() {
        assert_eq!(
            serde_json::to_string(&SessionStoreFlushMode::Batched).unwrap(),
            "\"batched\""
        );
        assert_eq!(
            serde_json::to_string(&SessionStoreFlushMode::Eager).unwrap(),
            "\"eager\""
        );
        assert_eq!(SessionStoreFlushMode::default(), SessionStoreFlushMode::Batched);
    }
}
