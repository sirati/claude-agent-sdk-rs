//! Orchestrates resume-from-store materialization: loads a session from
//! `options.session_store`, writes it to a temp directory laid out like
//! `~/.claude/`, and returns the path so the caller can point the
//! subprocess at it via `CLAUDE_CONFIG_DIR`.
//!
//! Ported from upstream `_internal/session_resume.py`. The CLI subprocess
//! only knows how to resume from a local file; `options.resume` (or
//! `options.continue_conversation`) paired with `options.session_store`
//! means the session JSONL almost certainly does not exist on local disk —
//! it lives in the external store. This module bridges the gap.
//!
//! Known gap vs. upstream: Python's cancellation is a catchable
//! `BaseException`, so `materialize_resume_session` can run cleanup code
//! *after* observing a cancellation and still propagate it. Tokio task
//! cancellation (the future being dropped) offers no such hook — dropped
//! code simply stops running at the next await point. There is no faithful
//! Rust equivalent of upstream's "cancelled mid-`list_subkeys()`, temp dir
//! still cleaned up" test; ordinary `Err` propagation (this module handles)
//! is fully ported, cancellation-during-setup leaking `tmp_base` is not.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use unicode_normalization::UnicodeNormalization;

use crate::errors::{ClaudeError, Result};
use crate::session::{
    SessionKey, SessionStore, SessionStoreEntry, SessionStoreError, SessionStoreFlushMode,
};
use crate::types::config::ClaudeAgentOptions;

use super::local::validate_uuid;
use super::project_dir::{get_projects_dir, project_key_for_directory};
use super::resume_credentials::copy_auth_files;
use super::resume_io::write_jsonl;
use super::resume_subkeys::materialize_subkeys;
use super::transcript_mirror::{MirrorErrorCallback, TranscriptMirrorBatcher};

/// Result of [`materialize_resume_session`].
#[derive(Debug)]
pub struct MaterializedResume {
    /// Temporary directory laid out like `~/.claude/` — point the
    /// subprocess at it via `CLAUDE_CONFIG_DIR`.
    pub config_dir: PathBuf,
    /// Session ID to pass as `--resume`. When the input was
    /// `continue_conversation`, this is the most-recent session resolved
    /// via `SessionStore::list_sessions`.
    pub resume_session_id: String,
}

impl MaterializedResume {
    /// Best-effort removal of [`Self::config_dir`]. Call after the
    /// subprocess exits. Idempotent — safe to call more than once.
    pub async fn cleanup(&self) {
        rmtree_with_retry(&self.config_dir).await;
    }
}

/// Return a copy of `options` repointed at a materialized temp config dir:
/// sets `CLAUDE_CONFIG_DIR` in `env`, `resume` to the materialized session
/// id, and clears `continue_conversation` (already resolved to a concrete
/// session id during materialization).
pub fn apply_materialized_options(
    options: &ClaudeAgentOptions,
    materialized: &MaterializedResume,
) -> ClaudeAgentOptions {
    let mut env = options.env.clone();
    env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        materialized.config_dir.display().to_string(),
    );
    ClaudeAgentOptions {
        env,
        resume: Some(materialized.resume_session_id.clone()),
        continue_conversation: false,
        ..options.clone()
    }
}

/// Construct the [`TranscriptMirrorBatcher`] for a session.
///
/// Resolves `projects_dir` to the materialized temp dir when present (so
/// `file_path` -> key resolution matches what the subprocess writes),
/// otherwise to the standard projects directory under the effective
/// `CLAUDE_CONFIG_DIR`.
///
/// `SessionStoreFlushMode::Eager` zeroes the batcher's pending thresholds;
/// `Batched` keeps the defaults (500 entries / 1 MiB) — see
/// [`TranscriptMirrorBatcher`] for how thresholds are surfaced to callers.
pub fn build_mirror_batcher(
    store: Arc<dyn SessionStore>,
    materialized: Option<&MaterializedResume>,
    env: &HashMap<String, String>,
    on_error: MirrorErrorCallback,
    flush_mode: SessionStoreFlushMode,
) -> TranscriptMirrorBatcher {
    let projects_dir = match materialized {
        Some(m) => m.config_dir.join("projects").display().to_string(),
        None => projects_dir_for_env(env).display().to_string(),
    };
    TranscriptMirrorBatcher::new(store, projects_dir, on_error, flush_mode)
}

/// `_get_projects_dir(env_override)`-equivalent: consults `env_override`
/// (options.env) before falling back to
/// [`super::project_dir::get_projects_dir`]'s process-env/`$HOME` lookup —
/// so store-backed callers that pass `CLAUDE_CONFIG_DIR` via `options.env`
/// (rather than the process environment) resolve the same directory the
/// subprocess will write to.
fn projects_dir_for_env(env_override: &HashMap<String, String>) -> PathBuf {
    if let Some(dir) = env_override.get("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        let nfc_dir: String = dir.nfc().collect();
        return PathBuf::from(nfc_dir).join("projects");
    }
    get_projects_dir()
}

/// Load a session from `options.session_store` and write it to a temp dir.
///
/// Returns `Ok(None)` when no materialization is needed (no store, no
/// resume/continue, store has no entries, or the resolved session ID is
/// not a valid UUID) — caller falls through to the normal (no-store)
/// resume/spawn path. For `continue_conversation` this means a fresh
/// session; for an explicit `resume` value the CLI receives it unchanged.
///
/// Returns `Err` if a store call fails or times out.
pub async fn materialize_resume_session(
    options: &ClaudeAgentOptions,
) -> Result<Option<MaterializedResume>> {
    let Some(store) = options.session_store.as_ref() else {
        return Ok(None);
    };
    if options.resume.is_none() && !options.continue_conversation {
        return Ok(None);
    }

    let timeout = Duration::from_millis(options.load_timeout_ms);
    let cwd_str = options.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let project_key = project_key_for_directory(cwd_str.as_deref());

    // Resolve the session ID — explicit resume wins; otherwise pick the
    // most-recently-modified non-sidechain session from the store. Empty
    // list_sessions() -> fresh session (matches CLI --continue with no
    // history).
    let resolved = if let Some(resume_id) = &options.resume {
        // session_id is used as a path component below; reject anything
        // that isn't a UUID to prevent traversal and match every other
        // resume path.
        if validate_uuid(resume_id).is_none() {
            return Ok(None);
        }
        load_candidate(store.as_ref(), &project_key, resume_id, timeout).await?
    } else {
        resolve_continue_candidate(store.as_ref(), &project_key, timeout).await?
    };
    let Some((session_id, entries)) = resolved else {
        return Ok(None);
    };

    let tmp_base = tempfile::Builder::new()
        .prefix("claude-resume-")
        .tempdir()
        .map_err(ClaudeError::Io)?
        .keep();

    if let Err(e) = materialize_into(
        store.as_ref(),
        &tmp_base,
        &project_key,
        &session_id,
        &entries,
        options,
        timeout,
    )
    .await
    {
        // Any failure after mkdtemp leaves tmp_base (which may already
        // contain a .credentials.json copy) on disk with no path for the
        // caller to clean it up. Remove it before returning.
        rmtree_with_retry(&tmp_base).await;
        return Err(e);
    }

    Ok(Some(MaterializedResume { config_dir: tmp_base, resume_session_id: session_id }))
}

async fn materialize_into(
    store: &dyn SessionStore,
    tmp_base: &Path,
    project_key: &str,
    session_id: &str,
    entries: &[SessionStoreEntry],
    options: &ClaudeAgentOptions,
    timeout: Duration,
) -> Result<()> {
    let project_dir = tmp_base.join("projects").join(project_key);
    tokio::fs::create_dir_all(&project_dir).await.map_err(ClaudeError::Io)?;
    write_jsonl(&project_dir.join(format!("{session_id}.jsonl")), entries).await?;

    // The subprocess will run with CLAUDE_CONFIG_DIR=tmp_base. Copy auth
    // config from the caller's effective config locations so it can
    // authenticate. Missing files are fine (API-key auth, etc.).
    copy_auth_files(tmp_base, &options.env).await;

    // Materialize subagent transcripts if the store can enumerate them.
    if store.supports_list_subkeys() {
        materialize_subkeys(store, &project_dir, project_key, session_id, timeout).await?;
    }
    Ok(())
}

async fn load_candidate(
    store: &dyn SessionStore,
    project_key: &str,
    session_id: &str,
    timeout: Duration,
) -> Result<Option<(String, Vec<SessionStoreEntry>)>> {
    let key = SessionKey::new(project_key, session_id);
    let entries = with_timeout(
        store.load(&key),
        timeout,
        &format!("SessionStore.load() for session {session_id}"),
    )
    .await?;
    Ok(match entries {
        Some(e) if !e.is_empty() => Some((session_id.to_string(), e)),
        _ => None,
    })
}

/// Pick the most-recently-modified non-sidechain session.
///
/// Sidechain transcripts are mirrored as ordinary top-level keys and often
/// have the highest mtime (their append lands after the main session's in
/// the same flush). Walk newest -> oldest, loading each candidate (the load
/// is needed anyway) and skipping sidechains so `continue_conversation`
/// resumes the user's conversation, not a subagent's.
async fn resolve_continue_candidate(
    store: &dyn SessionStore,
    project_key: &str,
    timeout: Duration,
) -> Result<Option<(String, Vec<SessionStoreEntry>)>> {
    let mut sessions =
        with_timeout(store.list_sessions(project_key), timeout, "SessionStore.list_sessions()")
            .await?;
    if sessions.is_empty() {
        return Ok(None);
    }
    // Stable sort descending by mtime: ties keep their original
    // list_sessions() order (`slice::sort_by_key` is a documented-stable
    // sort), matching Python's `sorted(..., reverse=True)`.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));

    for cand in sessions {
        if validate_uuid(&cand.session_id).is_none() {
            continue;
        }
        let Some(loaded) = load_candidate(store, project_key, &cand.session_id, timeout).await?
        else {
            continue;
        };
        let is_sidechain =
            loaded.1[0].extra.get("isSidechain").and_then(|v| v.as_bool()) == Some(true);
        if is_sidechain {
            continue;
        }
        return Ok(Some(loaded));
    }
    Ok(None)
}

/// Await `fut` with a timeout, re-raising as [`ClaudeError`] with context.
pub(super) async fn with_timeout<T>(
    fut: impl Future<Output = std::result::Result<T, SessionStoreError>>,
    timeout: Duration,
    what: &str,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(ClaudeError::Other(anyhow::anyhow!(
            "{what} failed during resume materialization: {e}"
        ))),
        Err(_) => Err(ClaudeError::Other(anyhow::anyhow!(
            "{what} timed out after {}ms during resume materialization",
            timeout.as_millis()
        ))),
    }
}

/// Best-effort recursive removal of `path`, with a few retries before
/// falling back to an ignore-errors partial sweep. Never fails.
///
/// Simplification vs. upstream: upstream retries on a specific
/// (Windows-centric) set of transient-lock errnos (AV/indexer briefly
/// holding a handle on a freshly-written file); this retries on any error
/// and always falls back to a manual best-effort walk that removes
/// whatever it can, rather than matching specific errno values.
async fn rmtree_with_retry(path: &Path) {
    const RETRIES: u32 = 4;
    const DELAY: Duration = Duration::from_millis(100);

    if tokio::fs::symlink_metadata(path).await.is_err() {
        return;
    }
    for _ in 0..RETRIES {
        if tokio::fs::remove_dir_all(path).await.is_ok() {
            return;
        }
        tokio::time::sleep(DELAY).await;
    }
    best_effort_remove_all(path).await;
}

/// Remove as much of the tree under (and including) `path` as possible,
/// ignoring individual failures. Matches `shutil.rmtree(ignore_errors=True)`.
fn best_effort_remove_all(path: &Path) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        if let Ok(mut entries) = tokio::fs::read_dir(path).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let child = entry.path();
                match entry.file_type().await {
                    Ok(ft) if ft.is_dir() => best_effort_remove_all(&child).await,
                    _ => {
                        let _ = tokio::fs::remove_file(&child).await;
                    }
                }
            }
        }
        let _ = tokio::fs::remove_dir(path).await;
    })
}

#[cfg(test)]
mod tests;
