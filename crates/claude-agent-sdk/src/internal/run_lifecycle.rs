//! In-flight delegated-task tracking for [`super::query_full::QueryFull`]'s
//! stdin-closing gate.
//!
//! Ported from upstream `_internal/query.py`'s `DEFERRING_TASK_TYPES`,
//! `TERMINAL_TASK_STATUSES`, and `Query._track_task_lifecycle` (see
//! upstream issue #1088): closing stdin as soon as a `result` message
//! arrives can cut off a query that spawned a delegated background-agent
//! task that is still running, because that task's completion later needs
//! the control channel (hooks / SDK-MCP round trips) for its follow-up
//! turn.
//!
//! This crate's `QueryFull` never auto-closes stdin per turn the way
//! upstream's `Query.stream_input` does — callers close it explicitly
//! (`QueryFull::close` / `wait_for_result_and_end_input`). The tracking
//! here still matters for that explicit path: an app that calls
//! `close()`/`disconnect()` right after observing a `result` message must
//! not cut off a delegated agent task still running in the background.

use std::collections::HashSet;

use tokio::sync::oneshot;

/// Task types whose completion runs a follow-up turn, and which therefore
/// may still need the control channel after the turn's `result` frame.
///
/// This mirrors the set the CLI itself holds a result back for, which is
/// narrower than its notion of "delegated agent work". The types left out
/// are left out on purpose:
///   - background shells and monitors run indefinitely by design, so
///     deferring the close on one withholds it forever rather than
///     briefly;
///   - teammates are long-lived too — their status stays running for their
///     whole lifetime, so they never settle the ledger;
///   - remote agents can be long-running monitors the CLI likewise refuses
///     to wait on.
///
/// Anything added here must be a type that reliably reaches a terminal
/// status, or it will hang the query.
pub(crate) const DEFERRING_TASK_TYPES: &[&str] = &["local_agent", "local_workflow"];

/// Task statuses that mean the task has finished and should be cleared from
/// in-flight tracking. Spans both lifecycle vocabularies: `task_notification`
/// reports `stopped` (the CLI's mapped form of a killed task) while
/// `task_updated` reports the raw `killed`.
///
/// Part of the crate's public API (re-exported at the crate root) so
/// consumers handling [`crate::TaskUpdatedMessage`]/[`crate::TaskNotificationMessage`]
/// can check a status against the same set the SDK itself uses, matching
/// upstream's public `TERMINAL_TASK_STATUSES` export.
pub const TERMINAL_TASK_STATUSES: &[&str] = &["completed", "failed", "stopped", "killed"];

/// Update `inflight` from a `system` message's task-lifecycle subtype.
///
/// `task_started` marks a task in flight (only for [`DEFERRING_TASK_TYPES`]);
/// `task_notification` or a `task_updated` patch with a terminal status
/// clears it. Terminal completion can arrive as either frame (not every
/// terminal task emits a notification), so both are handled.
///
/// This is a mitigation, not a complete answer to #1088: an empty set means
/// "nothing we know of is running", which is not the same as "the run is
/// over" — a task that settles *before* the turn's result frame leaves the
/// set empty at that result, so stdin closes even though the completion may
/// still wake a follow-up turn. What this does fix is the common ordering,
/// where the task outlives the turn that spawned it.
pub(crate) fn track_task_lifecycle(
    inflight: &mut HashSet<String>,
    subtype: &str,
    task_id: Option<&str>,
    task_type: Option<&str>,
    patch_status: Option<&str>,
) {
    let Some(task_id) = task_id else { return };
    match subtype {
        "task_started" => {
            if task_type.is_some_and(|t| DEFERRING_TASK_TYPES.contains(&t)) {
                inflight.insert(task_id.to_string());
            }
        }
        "task_notification" => {
            inflight.remove(task_id);
        }
        "task_updated" => {
            if patch_status.is_some_and(|s| TERMINAL_TASK_STATUSES.contains(&s)) {
                inflight.remove(task_id);
            }
        }
        _ => {}
    }
}

/// One-shot "the run has ended" latch.
///
/// Mirrors upstream's `anyio.Event` semantics for `_first_result_event`:
/// [`RunEndedSignal::fire`] is idempotent (only the first call has any
/// effect) and [`RunEndedSignal::wait`] returns immediately if `fire` has
/// already happened, even if it happened before `wait` was called — a plain
/// `tokio::sync::Notify` would lose a `fire` that races ahead of `wait`.
/// Built on a `oneshot` channel instead: sending buffers the value until
/// received, so there is no missed-wakeup window to reason about.
pub(crate) struct RunEndedSignal {
    tx: tokio::sync::Mutex<Option<oneshot::Sender<()>>>,
    rx: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

impl RunEndedSignal {
    pub(crate) fn new() -> Self {
        let (tx, rx) = oneshot::channel();
        Self { tx: tokio::sync::Mutex::new(Some(tx)), rx: tokio::sync::Mutex::new(Some(rx)) }
    }

    /// Fire the signal. Safe to call more than once (e.g. once from a
    /// run-ending `result` and once from the read loop's early-exit path);
    /// only the first call has any effect.
    pub(crate) async fn fire(&self) {
        if let Some(tx) = self.tx.lock().await.take() {
            let _ = tx.send(());
        }
    }

    /// Wait for the signal. Returns immediately if already fired (including
    /// before this call started), and also returns immediately on every
    /// call after the first (the receiver is consumed on first wait, same
    /// as upstream's `Event.wait()` being safe to call repeatedly once set).
    pub(crate) async fn wait(&self) {
        let rx = self.rx.lock().await.take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_started_tracks_only_deferring_types() {
        let mut inflight = HashSet::new();
        track_task_lifecycle(&mut inflight, "task_started", Some("t1"), Some("local_agent"), None);
        assert!(inflight.contains("t1"));

        track_task_lifecycle(&mut inflight, "task_started", Some("t2"), Some("background_shell"), None);
        assert!(!inflight.contains("t2"));
    }

    #[test]
    fn task_notification_clears_regardless_of_status() {
        let mut inflight: HashSet<String> = ["t1".to_string()].into_iter().collect();
        track_task_lifecycle(&mut inflight, "task_notification", Some("t1"), None, None);
        assert!(inflight.is_empty());
    }

    #[test]
    fn task_updated_clears_only_on_terminal_status() {
        let mut inflight: HashSet<String> = ["t1".to_string()].into_iter().collect();
        track_task_lifecycle(&mut inflight, "task_updated", Some("t1"), None, Some("running"));
        assert!(inflight.contains("t1"));

        track_task_lifecycle(&mut inflight, "task_updated", Some("t1"), None, Some("killed"));
        assert!(inflight.is_empty());
    }

    #[test]
    fn missing_task_id_is_a_no_op() {
        let mut inflight = HashSet::new();
        track_task_lifecycle(&mut inflight, "task_started", None, Some("local_agent"), None);
        assert!(inflight.is_empty());
    }

    #[tokio::test]
    async fn run_ended_signal_wait_returns_immediately_if_already_fired() {
        let signal = RunEndedSignal::new();
        signal.fire().await;
        // Must not hang.
        tokio::time::timeout(std::time::Duration::from_millis(200), signal.wait())
            .await
            .expect("wait() should return immediately once fired");
    }

    #[tokio::test]
    async fn run_ended_signal_wait_unblocks_on_later_fire() {
        let signal = std::sync::Arc::new(RunEndedSignal::new());
        let waiter = {
            let signal = signal.clone();
            tokio::spawn(async move { signal.wait().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        signal.fire().await;
        tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("wait() should unblock after fire()")
            .unwrap();
    }

    #[tokio::test]
    async fn run_ended_signal_fire_is_idempotent() {
        let signal = RunEndedSignal::new();
        signal.fire().await;
        signal.fire().await; // must not panic
    }
}
