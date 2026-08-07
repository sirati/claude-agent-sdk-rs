//! Pluggable session-store subsystem.
//!
//! [`SessionStore`] is an adapter interface for mirroring session
//! transcripts to external storage (S3, Postgres, Redis, etc.), used for
//! resume-from-store and cross-process session listing. This module defines
//! the shared wire types, the trait itself, an in-memory reference
//! implementation ([`InMemorySessionStore`]), and pre-flight option
//! validation.
//!
//! Ported from upstream's `_internal/session_store.py`,
//! `_internal/session_store_validation.py`, and the `SessionStore`-related
//! section of `types.py`. See [`conformance`] (test-only) for the shared
//! behavioral contract suite adapter authors can run their own
//! implementation against.
//!
//! Session mutations (rename/tag/delete/fork, both local-disk and
//! `SessionStore`-backed) and store import are ported from upstream's
//! `_internal/session_mutations.py` and `_internal/session_import.py`; see
//! [`mutations`], [`fork`], [`mutations_store`], and [`import_to_store`].
//!
//! Resume-from-store materialization (writing a store-backed session out to
//! a temp `CLAUDE_CONFIG_DIR` the CLI subprocess can resume from, including
//! macOS-Keychain credential copying) is ported from upstream's
//! `_internal/session_resume.py`; see [`resume`], [`resume_credentials`],
//! and [`resume_subkeys`].
//!
//! Top-level listing/query APIs (`list_sessions`, `get_session_info`,
//! `get_session_messages`, `list_subagents`, `get_subagent_messages`, and
//! their `SessionStore`-backed `*_from_store` counterparts) are ported from
//! the second half of upstream's `_internal/sessions.py`; see [`listing`],
//! [`listing_worktrees`], [`session_info`], [`messages`], [`subagents`],
//! [`message_paging`], [`listing_store`], [`listing_store_fast_path`], and
//! [`subagents_store`].

mod fork;
mod fork_transform;
mod import_to_store;
mod in_memory;
mod info;
mod iso_time;
mod json_extract;
mod lite_info;
mod listing;
mod listing_store;
mod listing_store_fast_path;
mod listing_worktrees;
mod local;
mod local_session_file;
mod message_paging;
mod messages;
mod mutations;
mod mutations_store;
mod project_dir;
mod resume;
mod resume_credentials;
mod resume_io;
mod resume_subkeys;
mod session_info;
mod store;
mod subagents;
mod subagents_store;
mod summary;
mod transcript;
mod transcript_mirror;
mod types;
mod unicode_sanitize;
mod validation;

#[cfg(test)]
pub mod conformance;

pub use fork::{fork_session, ForkSessionResult};
pub use import_to_store::import_session_to_store;
pub use in_memory::InMemorySessionStore;
pub use info::{SDKSessionInfo, SessionMessage, SessionMessageType};
pub use listing::list_sessions;
pub use listing_store::{get_session_info_from_store, get_session_messages_from_store, list_sessions_from_store};
pub use messages::get_session_messages;
pub use mutations::{delete_session, rename_session, tag_session};
pub use mutations_store::{
    delete_session_via_store, fork_session_via_store, rename_session_via_store, tag_session_via_store,
};
pub use project_dir::project_key_for_directory;
pub use resume::{apply_materialized_options, build_mirror_batcher, materialize_resume_session, MaterializedResume};
pub use session_info::get_session_info;
pub use store::{SessionStore, SessionStoreError};
pub use subagents::{get_subagent_messages, list_subagents};
pub use subagents_store::{get_subagent_messages_from_store, list_subagents_from_store};
pub use summary::{fold_session_summary, summary_entry_to_sdk_info};
pub use transcript_mirror::{
    MAX_PENDING_BYTES, MAX_PENDING_ENTRIES, MirrorErrorCallback, TranscriptMirrorBatcher,
};
pub use types::{
    SessionKey, SessionListSubkeysKey, SessionStoreEntry, SessionStoreFlushMode,
    SessionStoreListEntry, SessionSummaryEntry,
};
pub use validation::validate_session_store_options;
