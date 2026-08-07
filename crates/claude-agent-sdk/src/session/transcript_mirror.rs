//! Batching layer between `transcript_mirror` stdout frames and a
//! [`SessionStore`].
//!
//! The CLI subprocess emits `{"type": "transcript_mirror", "filePath": ...,
//! "entries": [...]}` frames interleaved with normal SDK messages. The query
//! read loop peels these off and hands them to
//! [`TranscriptMirrorBatcher::enqueue`], which accumulates them and flushes
//! to [`SessionStore::append`] either when a `result` message arrives
//! (explicit [`TranscriptMirrorBatcher::flush`]) or when the pending buffer
//! exceeds size thresholds (eager background flush). This keeps adapter
//! latency off the hot path during model streaming.
//!
//! Ported from upstream's `_internal/transcript_mirror_batcher.py`.
//!
//! DEDUP CANDIDATE: [`file_path_to_session_key`] duplicates upstream's
//! `_internal/session_store.py::file_path_to_session_key`. Sibling,
//! concurrently-landing porting efforts under this same `session` module
//! (resume materialization, filesystem session listing) each derive
//! equivalent path-resolution primitives; this copy was kept private and
//! self-contained rather than reaching into an in-flight sibling module to
//! avoid a build-order hazard while both ports were landing at once. Once
//! one of those slices exposes an equivalent crate-visible helper, this
//! copy should be deleted in favor of it.
//!
//! # Concurrency model
//!
//! Upstream buffers `_pending` directly on the batcher (synchronous
//! `enqueue`) and uses an `anyio.Lock` to serialize concurrent drains while
//! `enqueue` keeps accumulating into a fresh buffer. This crate avoids
//! `Mutex`-guarded shared state for async coordination (see the workspace
//! conventions) and instead runs a single background actor task that owns
//! the pending buffer and the store: [`TranscriptMirrorBatcher::enqueue`]
//! and [`TranscriptMirrorBatcher::flush`] just send a command over an
//! unbounded channel. Because one task processes commands strictly in
//! order, drains are automatically serialized without an explicit lock, and
//! `enqueue` never blocks the caller (the query read loop) even while a
//! slow adapter call is in flight — matching the upstream guarantee.
//!
//! Upstream also takes care to call `on_error` only after releasing its
//! lock, so a slow `on_error` callback cannot delay a concurrently-running
//! drain's `store.append` calls. This single-actor design has no separate
//! lock to release, so the same property is preserved by dispatching
//! `on_error` through a detached `tokio::spawn` instead of awaiting it
//! inline in `drain()` — otherwise it would block the actor from
//! processing every later `Enqueue`/`Flush` command, reintroducing the
//! head-of-line blocking upstream's lock design avoids.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot};

use super::store::SessionStore;
use super::types::{SessionKey, SessionStoreEntry, SessionStoreFlushMode};

/// Eager-flush entry-count threshold for [`SessionStoreFlushMode::Batched`].
pub const MAX_PENDING_ENTRIES: usize = 500;
/// Eager-flush byte-size threshold for [`SessionStoreFlushMode::Batched`].
pub const MAX_PENDING_BYTES: usize = 1 << 20; // 1 MiB
/// Per-`append` call timeout.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounded retry for transient adapter failures.
const MIRROR_APPEND_MAX_ATTEMPTS: usize = 3;
/// Backoff between attempts. Length must be `MIRROR_APPEND_MAX_ATTEMPTS - 1`.
const MIRROR_APPEND_BACKOFF: [Duration; 2] = [Duration::from_millis(200), Duration::from_millis(800)];

/// Callback invoked when a batch permanently fails to mirror, after
/// exhausting retries. The batch is dropped (at-most-once delivery) —
/// callers use this as their only failure signal, typically to surface a
/// `MirrorErrorMessage` back to the consumer.
pub type MirrorErrorCallback =
    Arc<dyn Fn(Option<SessionKey>, String) -> BoxFuture<'static, ()> + Send + Sync>;

struct PendingFrame {
    file_path: String,
    entries: Vec<SessionStoreEntry>,
    bytes: usize,
}

enum Command {
    Enqueue(PendingFrame),
    Flush(oneshot::Sender<()>),
}

/// Accumulates `transcript_mirror` frames and flushes them to a
/// [`SessionStore`].
///
/// Cloning is cheap (an `mpsc::UnboundedSender` clone) — clone freely to
/// share a batcher between the query read loop and any other task that
/// needs to enqueue or flush.
#[derive(Clone)]
pub struct TranscriptMirrorBatcher {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl TranscriptMirrorBatcher {
    /// Construct a batcher for `flush_mode`.
    ///
    /// `Eager` zeroes the pending thresholds so every enqueued frame
    /// schedules a background flush; `Batched` keeps the upstream defaults
    /// (flush on `result` or 500-entry / 1 MiB overflow).
    pub fn new(
        store: Arc<dyn SessionStore>,
        projects_dir: impl Into<String>,
        on_error: MirrorErrorCallback,
        flush_mode: SessionStoreFlushMode,
    ) -> Self {
        let (max_pending_entries, max_pending_bytes) = match flush_mode {
            SessionStoreFlushMode::Eager => (0, 0),
            SessionStoreFlushMode::Batched => (MAX_PENDING_ENTRIES, MAX_PENDING_BYTES),
        };
        Self::with_thresholds(
            store,
            projects_dir,
            on_error,
            max_pending_entries,
            max_pending_bytes,
            SEND_TIMEOUT,
        )
    }

    /// Constructor with explicit thresholds/timeout, for tests and callers
    /// that need to deviate from the upstream defaults.
    pub fn with_thresholds(
        store: Arc<dyn SessionStore>,
        projects_dir: impl Into<String>,
        on_error: MirrorErrorCallback,
        max_pending_entries: usize,
        max_pending_bytes: usize,
        send_timeout: Duration,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let projects_dir = projects_dir.into();
        tokio::spawn(run_worker(
            cmd_rx,
            store,
            projects_dir,
            on_error,
            max_pending_entries,
            max_pending_bytes,
            send_timeout,
        ));
        Self { cmd_tx }
    }

    /// Buffer a frame. Fire-and-forget: the worker schedules its own
    /// background flush once pending thresholds are exceeded, so this never
    /// blocks the caller (the query read loop) even while a prior flush is
    /// in flight.
    pub fn enqueue(&self, file_path: impl Into<String>, entries: Vec<SessionStoreEntry>) {
        let bytes = serde_json::to_vec(&entries).map(|buf| buf.len()).unwrap_or(0);
        let frame = PendingFrame { file_path: file_path.into(), entries, bytes };
        // The worker task only goes away once every clone of this batcher is
        // dropped, so a send failure here means nobody can observe the
        // dropped frame anyway.
        let _ = self.cmd_tx.send(Command::Enqueue(frame));
    }

    /// Flush all pending entries, serialized after any in-flight background
    /// flush (the worker processes commands strictly in send order).
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Command::Flush(tx)).is_err() {
            return;
        }
        let _ = rx.await;
    }

    /// Final flush before teardown. Never panics and never propagates
    /// adapter errors — failures already went through `on_error`.
    ///
    /// Because the actual drain runs on the separately-spawned worker task
    /// rather than inline in the caller's future, dropping the caller's
    /// `close()` future (e.g. the surrounding query task getting cancelled)
    /// does not stop the worker from completing the flush — this gives the
    /// same "the final batch still reaches the store" guarantee upstream
    /// gets from `anyio.CancelScope(shield=True)`, without needing an
    /// explicit shield here.
    pub async fn close(&self) {
        self.flush().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    store: Arc<dyn SessionStore>,
    projects_dir: String,
    on_error: MirrorErrorCallback,
    max_pending_entries: usize,
    max_pending_bytes: usize,
    send_timeout: Duration,
) {
    let mut pending: Vec<PendingFrame> = Vec::new();
    let mut pending_entries = 0usize;
    let mut pending_bytes = 0usize;

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Enqueue(frame) => {
                pending_entries += frame.entries.len();
                pending_bytes += frame.bytes;
                pending.push(frame);
                if pending_entries > max_pending_entries || pending_bytes > max_pending_bytes {
                    drain(
                        &mut pending,
                        &mut pending_entries,
                        &mut pending_bytes,
                        &store,
                        &projects_dir,
                        &on_error,
                        send_timeout,
                    )
                    .await;
                }
            }
            Command::Flush(done) => {
                drain(
                    &mut pending,
                    &mut pending_entries,
                    &mut pending_bytes,
                    &store,
                    &projects_dir,
                    &on_error,
                    send_timeout,
                )
                .await;
                let _ = done.send(());
            }
        }
    }
}

/// Detach the pending buffer and send it to the store, one `append` per
/// unique `file_path`. Never panics; adapter failures go through
/// `on_error` after retries are exhausted.
#[allow(clippy::too_many_arguments)]
async fn drain(
    pending: &mut Vec<PendingFrame>,
    pending_entries: &mut usize,
    pending_bytes: &mut usize,
    store: &Arc<dyn SessionStore>,
    projects_dir: &str,
    on_error: &MirrorErrorCallback,
    send_timeout: Duration,
) {
    if pending.is_empty() {
        return;
    }
    let items = std::mem::take(pending);
    *pending_entries = 0;
    *pending_bytes = 0;

    // Coalesce by file_path, preserving first-seen order, so each unique
    // file gets one append per flush instead of one per enqueued frame.
    let mut order: Vec<String> = Vec::new();
    let mut by_path: HashMap<String, Vec<SessionStoreEntry>> = HashMap::new();
    for item in items {
        by_path
            .entry(item.file_path.clone())
            .or_insert_with(|| {
                order.push(item.file_path.clone());
                Vec::new()
            })
            .extend(item.entries);
    }

    for file_path in order {
        let Some(entries) = by_path.remove(&file_path) else { continue };
        if entries.is_empty() {
            // Avoid creating phantom keys in adapters that touch storage on
            // append([]) — nothing to write.
            continue;
        }
        let Some(key) = file_path_to_session_key(&file_path, projects_dir) else {
            tracing::warn!(
                file_path = %file_path,
                projects_dir = %projects_dir,
                "[SessionStore] dropping mirror frame: filePath is not under projects_dir \
                 -- subprocess CLAUDE_CONFIG_DIR likely differs from parent (custom env / container?)"
            );
            continue;
        };

        let mut last_err: Option<String> = None;
        let mut succeeded = false;
        for attempt in 0..MIRROR_APPEND_MAX_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(MIRROR_APPEND_BACKOFF[attempt - 1]).await;
            }
            match tokio::time::timeout(send_timeout, store.append(&key, entries.clone())).await {
                Ok(Ok(())) => {
                    succeeded = true;
                    break;
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        attempt = attempt + 1,
                        max_attempts = MIRROR_APPEND_MAX_ATTEMPTS,
                        file_path = %file_path,
                        error = %e,
                        "[TranscriptMirrorBatcher] append attempt failed"
                    );
                    last_err = Some(e.to_string());
                }
                Err(_elapsed) => {
                    // Don't retry on timeout: cancellation is best-effort
                    // for adapters wrapping non-cancellable I/O, so the
                    // in-flight call may still land — a retry would launch
                    // a concurrent duplicate.
                    tracing::debug!(
                        seconds = send_timeout.as_secs_f64(),
                        file_path = %file_path,
                        "[TranscriptMirrorBatcher] append timed out -- not retrying"
                    );
                    last_err = Some(format!("append timed out after {:.1}s", send_timeout.as_secs_f64()));
                    break;
                }
            }
        }

        if !succeeded {
            let error_text = last_err.unwrap_or_else(|| "unknown error".to_string());
            tracing::error!(
                file_path = %file_path,
                error = %error_text,
                "[TranscriptMirrorBatcher] flush failed"
            );
            // Spawned rather than awaited inline: upstream's `_drain()`
            // reports errors only AFTER releasing its append-ordering lock,
            // specifically so a slow `on_error` callback cannot delay a
            // concurrently-triggered drain's `store.append` calls. This
            // actor processes one command at a time, so awaiting `on_error`
            // here directly would block every later `Enqueue`/`Flush`
            // command (including unrelated files' appends) behind it,
            // reintroducing the same head-of-line blocking upstream avoids.
            tokio::spawn(on_error(Some(key), error_text));
        }
    }
}

/// Derive a [`SessionKey`] from an absolute transcript file path.
///
/// Main transcripts: `<projects_dir>/<project_key>/<session_id>.jsonl`
/// Subagent transcripts:
/// `<projects_dir>/<project_key>/<session_id>/subagents/agent-<id>.jsonl`
///
/// Returns `None` if `file_path` is not under `projects_dir` or has an
/// unrecognized shape. See the module-level "DEDUP CANDIDATE" note.
fn file_path_to_session_key(file_path: &str, projects_dir: &str) -> Option<SessionKey> {
    let rel = Path::new(file_path).strip_prefix(projects_dir).ok()?;
    let parts: Vec<&str> = rel.iter().map(|c| c.to_str().unwrap_or("")).collect();
    if parts.is_empty() || parts[0] == ".." {
        return None;
    }
    if parts.len() < 2 {
        return None;
    }

    let project_key = parts[0].to_string();
    let second = parts[1];

    // Main transcript: <project_key>/<session_id>.jsonl
    if parts.len() == 2 {
        let session_id = second.strip_suffix(".jsonl")?;
        return Some(SessionKey::new(project_key, session_id));
    }

    // Subagent transcript: <project_key>/<session_id>/subagents/.../agent-<id>.jsonl
    if parts.len() >= 4 {
        let mut subpath_parts: Vec<String> = parts[2..].iter().map(|s| s.to_string()).collect();
        if let Some(last) = subpath_parts.last_mut()
            && let Some(stripped) = last.strip_suffix(".jsonl")
        {
            *last = stripped.to_string();
        }
        // Subpaths are always /-joined regardless of path separator so keys
        // are portable across platforms.
        let subpath = subpath_parts.join("/");
        return Some(SessionKey::with_subpath(project_key, second, subpath));
    }

    None
}

#[cfg(test)]
mod tests;
