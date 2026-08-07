//! The [`SessionStore`] adapter trait.

use async_trait::async_trait;
use thiserror::Error;

use super::types::{
    SessionKey, SessionListSubkeysKey, SessionStoreEntry, SessionStoreListEntry,
    SessionSummaryEntry,
};

/// Errors surfaced by [`SessionStore`] operations.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// The adapter does not implement this optional operation.
    ///
    /// Upstream Python models `SessionStore` as a `Protocol` whose optional
    /// methods default to raising `NotImplementedError`; call sites duck-type
    /// probe for a real override before invoking. Rust traits have no
    /// runtime introspection for "was this default method overridden", so
    /// this sentinel plays the same role: default trait-method bodies
    /// return it, and callers match on it exactly where Python call sites
    /// checked for the method's absence.
    #[error("SessionStore adapter does not implement this operation")]
    NotImplemented,
    /// The adapter failed to complete the operation.
    #[error("session store error: {0}")]
    Backend(#[from] anyhow::Error),
}

/// Adapter for mirroring session transcripts to external storage (S3,
/// Postgres, Redis, etc.), used for resume-from-store and cross-process
/// session listing.
///
/// The subprocess still writes to local disk (set `CLAUDE_CONFIG_DIR=/tmp`
/// for an ephemeral local copy); the adapter receives a secondary copy.
///
/// The SDK never deletes from your store unless a caller explicitly invokes
/// [`SessionStore::delete`]. Retention is the adapter's responsibility —
/// implement TTL, object-storage lifecycle policies, or scheduled cleanup
/// according to your compliance requirements (e.g. ZDR/HIPAA retention
/// windows). Local-disk transcripts under `CLAUDE_CONFIG_DIR` are swept by
/// the existing `cleanupPeriodDays` setting independently of this adapter.
///
/// Only [`append`](SessionStore::append) and [`load`](SessionStore::load)
/// are required. The remaining methods are optional: implementations may
/// leave them at their default, which returns
/// [`SessionStoreError::NotImplemented`]. Each optional method has a
/// matching `supports_*` capability flag (default `false`) that
/// implementations overriding the method must also flip to `true` — Rust
/// has no way to detect a trait-method override at compile or run time, so
/// callers that need to know a capability *before* they have arguments to
/// call the method with (e.g. pre-flight config validation) check the flag
/// instead of calling-and-matching-the-sentinel.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Mirror a batch of transcript entries.
    ///
    /// Called AFTER the subprocess's local write succeeds — durability is
    /// already guaranteed locally.
    ///
    /// Batches arrive at ~100ms cadence during active turns. Within a single
    /// process, persist entries in append-call order; across concurrent
    /// processes, order is by storage commit time, not call time.
    ///
    /// Most entries carry a stable [`SessionStoreEntry::uuid`] that adapters
    /// should treat as an idempotency key (upsert / ignore-duplicate).
    /// Entries without a `uuid` (e.g. titles, tags, mode markers) should be
    /// appended without dedup. Retry/backoff/drop behavior for failed
    /// batches is the caller's responsibility, not this trait's.
    async fn append(
        &self,
        key: &SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> Result<(), SessionStoreError>;

    /// Load a full session for resume.
    ///
    /// Called once, in the SDK parent, before subprocess spawn. The result
    /// is materialized to a temporary JSONL file; the subprocess resumes
    /// from that file using its existing resume code.
    ///
    /// Return `None` for a key that was never written; adapters that cannot
    /// distinguish "never written" from "emptied" (e.g. Redis `LRANGE`) may
    /// return `None` for both. Returned entries must be deep-equal to what
    /// was appended — byte-equal serialization is NOT required (e.g.
    /// Postgres `JSONB` may reorder object keys); the SDK never hashes or
    /// byte-compares entries.
    async fn load(&self, key: &SessionKey) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError>;

    /// List sessions for a `project_key`. Returns IDs + modification times.
    ///
    /// `mtime` is Unix epoch milliseconds; adapters without a native
    /// modification time (e.g. Redis) must maintain their own index. Result
    /// order is unspecified — callers sort by `mtime` descending.
    ///
    /// Optional — see [`Self::supports_list_sessions`].
    async fn list_sessions(
        &self,
        _project_key: &str,
    ) -> Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        Err(SessionStoreError::NotImplemented)
    }

    /// Cheap, synchronous capability probe for [`Self::list_sessions`].
    /// Implementations that override `list_sessions` must also override
    /// this to return `true`.
    fn supports_list_sessions(&self) -> bool {
        false
    }

    /// Return incrementally-maintained summaries for all sessions in one
    /// call.
    ///
    /// Stores should maintain these via `fold_session_summary` inside
    /// [`Self::append`]. Skip the fold for keys with a `subpath` — subagent
    /// transcripts must not contribute to the main session's summary.
    ///
    /// Like [`Self::list_sessions`], results are scoped to a single
    /// `project_key` and exclude `subpath` entries.
    ///
    /// Optional — see [`Self::supports_list_session_summaries`].
    ///
    /// Stores that maintain summaries inside `append()` MUST serialize
    /// sidecar writes if `append()` calls can race for the same session —
    /// e.g., wrap the read-fold-write in a transaction/CAS, or hold a
    /// per-session lock.
    async fn list_session_summaries(
        &self,
        _project_key: &str,
    ) -> Result<Vec<SessionSummaryEntry>, SessionStoreError> {
        Err(SessionStoreError::NotImplemented)
    }

    /// Cheap, synchronous capability probe for
    /// [`Self::list_session_summaries`].
    fn supports_list_session_summaries(&self) -> bool {
        false
    }

    /// Delete a session.
    ///
    /// Deleting a main-transcript key (no `subpath`) must cascade to all
    /// subkeys under that session so subagent transcripts aren't orphaned.
    /// A targeted delete with an explicit `subpath` removes only that one
    /// entry.
    ///
    /// Optional — if unimplemented, deletion is a no-op (appropriate for
    /// WORM/append-only backends like object storage). See
    /// [`Self::supports_delete`].
    async fn delete(&self, _key: &SessionKey) -> Result<(), SessionStoreError> {
        Err(SessionStoreError::NotImplemented)
    }

    /// Cheap, synchronous capability probe for [`Self::delete`].
    fn supports_delete(&self) -> bool {
        false
    }

    /// List all subpath keys under a session (e.g. subagent transcripts).
    ///
    /// Used during resume to discover and materialize all subagent data.
    ///
    /// Optional — if unimplemented, resume only materializes the main
    /// transcript. See [`Self::supports_list_subkeys`].
    async fn list_subkeys(&self, _key: &SessionListSubkeysKey) -> Result<Vec<String>, SessionStoreError> {
        Err(SessionStoreError::NotImplemented)
    }

    /// Cheap, synchronous capability probe for [`Self::list_subkeys`].
    fn supports_list_subkeys(&self) -> bool {
        false
    }
}
