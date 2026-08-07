//! Unit tests for [`super::TranscriptMirrorBatcher`].

use super::*;
use crate::session::SessionStoreError;
use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex as AsyncMutex;

fn entry(uuid: &str) -> SessionStoreEntry {
    SessionStoreEntry::new("user", json!({"uuid": uuid}).as_object().cloned().unwrap())
}

/// Errors reported via `on_error` during a test: `(session key, message)`.
type ReportedErrors = AsyncMutex<Vec<(Option<SessionKey>, String)>>;

struct RecordingStore {
    appends: AsyncMutex<Vec<(SessionKey, Vec<SessionStoreEntry>)>>,
    fail_times: AtomicUsize,
}

impl RecordingStore {
    fn new(fail_times: usize) -> Self {
        Self { appends: AsyncMutex::new(Vec::new()), fail_times: AtomicUsize::new(fail_times) }
    }
}

#[async_trait]
impl SessionStore for RecordingStore {
    async fn append(
        &self,
        key: &SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> Result<(), SessionStoreError> {
        if self.fail_times.load(Ordering::SeqCst) > 0 {
            self.fail_times.fetch_sub(1, Ordering::SeqCst);
            return Err(SessionStoreError::Backend(anyhow::anyhow!("boom")));
        }
        self.appends.lock().await.push((key.clone(), entries));
        Ok(())
    }

    async fn load(
        &self,
        _key: &SessionKey,
    ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        Ok(None)
    }
}

fn no_op_on_error() -> MirrorErrorCallback {
    Arc::new(|_key, _err| Box::pin(async {}))
}

#[test]
fn file_path_to_session_key_main_transcript() {
    let key = file_path_to_session_key("/root/proj-a/sess-1.jsonl", "/root").unwrap();
    assert_eq!(key.project_key, "proj-a");
    assert_eq!(key.session_id, "sess-1");
    assert!(key.subpath.is_none());
}

#[test]
fn file_path_to_session_key_subagent_transcript() {
    let key = file_path_to_session_key("/root/proj-a/sess-1/subagents/agent-1.jsonl", "/root").unwrap();
    assert_eq!(key.project_key, "proj-a");
    assert_eq!(key.session_id, "sess-1");
    assert_eq!(key.subpath.as_deref(), Some("subagents/agent-1"));
}

#[test]
fn file_path_to_session_key_rejects_paths_outside_projects_dir() {
    assert!(file_path_to_session_key("/elsewhere/proj/sess.jsonl", "/root").is_none());
}

#[tokio::test]
async fn flush_delivers_pending_entries() {
    let store = Arc::new(RecordingStore::new(0));
    let batcher =
        TranscriptMirrorBatcher::new(store.clone(), "/root", no_op_on_error(), SessionStoreFlushMode::Batched);

    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("a")]);
    batcher.flush().await;

    let appends = store.appends.lock().await;
    assert_eq!(appends.len(), 1);
    assert_eq!(appends[0].1.len(), 1);
}

#[tokio::test]
async fn coalesces_multiple_frames_for_same_file_into_one_append() {
    let store = Arc::new(RecordingStore::new(0));
    let batcher =
        TranscriptMirrorBatcher::new(store.clone(), "/root", no_op_on_error(), SessionStoreFlushMode::Batched);

    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("a")]);
    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("b")]);
    batcher.flush().await;

    let appends = store.appends.lock().await;
    assert_eq!(appends.len(), 1);
    assert_eq!(appends[0].1.len(), 2);
}

#[tokio::test]
async fn eager_mode_flushes_without_explicit_flush_call() {
    let store = Arc::new(RecordingStore::new(0));
    let batcher =
        TranscriptMirrorBatcher::new(store.clone(), "/root", no_op_on_error(), SessionStoreFlushMode::Eager);

    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("a")]);
    // Give the worker a moment to process the eager background drain.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let appends = store.appends.lock().await;
    assert_eq!(appends.len(), 1);
}

#[tokio::test]
async fn retries_then_succeeds() {
    let store = Arc::new(RecordingStore::new(2)); // fail twice, succeed on 3rd
    let batcher =
        TranscriptMirrorBatcher::new(store.clone(), "/root", no_op_on_error(), SessionStoreFlushMode::Batched);

    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("a")]);
    batcher.flush().await;

    let appends = store.appends.lock().await;
    assert_eq!(appends.len(), 1);
}

#[tokio::test]
async fn exhausted_retries_report_on_error() {
    let store = Arc::new(RecordingStore::new(10)); // always fails
    let reported: Arc<ReportedErrors> = Arc::new(AsyncMutex::new(Vec::new()));
    let reported_clone = reported.clone();
    let on_error: MirrorErrorCallback = Arc::new(move |key, err| {
        let reported = reported_clone.clone();
        Box::pin(async move {
            reported.lock().await.push((key, err));
        })
    });
    let batcher = TranscriptMirrorBatcher::new(store.clone(), "/root", on_error, SessionStoreFlushMode::Batched);

    batcher.enqueue("/root/proj/sess.jsonl", vec![entry("a")]);
    batcher.flush().await;

    assert!(store.appends.lock().await.is_empty());
    // `on_error` is now dispatched via a detached `tokio::spawn` rather than
    // awaited inline (see `drain()`'s doc comment), so `flush()` returning
    // no longer guarantees the callback has run yet -- poll briefly instead
    // of asserting immediately.
    let reported = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if reported.lock().await.len() == 1 {
                return reported.lock().await.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("on_error should have been reported within 1s");
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].0.as_ref().unwrap().session_id, "sess");
}

// Regression test: a slow `on_error` callback must not block the worker
// from starting a *later*, unrelated drain's `store.append` attempts.
// Upstream Python's `_drain()` explicitly reports errors only after
// releasing its append-ordering lock for exactly this reason -- see
// `drain()`'s doc comment in the parent module.
#[tokio::test]
async fn slow_on_error_does_not_block_a_later_independent_append() {
    use std::sync::atomic::AtomicBool;
    let store = Arc::new(RecordingStore::new(10)); // always fails
    let on_error_started = Arc::new(AtomicBool::new(false));
    let on_error_started_clone = on_error_started.clone();
    let on_error: MirrorErrorCallback = Arc::new(move |_key, _err| {
        let flag = on_error_started_clone.clone();
        Box::pin(async move {
            flag.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(500)).await;
        })
    });
    let batcher = TranscriptMirrorBatcher::with_thresholds(
        store.clone(),
        "/root",
        on_error,
        0, // eager: any enqueue triggers a background drain
        0,
        Duration::from_secs(5),
    );

    // First frame: exhausts all 3 retry attempts, then fires the slow
    // on_error above in the background (eager threshold = 0).
    batcher.enqueue("/root/proj/sess-a.jsonl", vec![entry("a")]);

    // Give the worker time to exhaust retries (200ms + 800ms backoff
    // between the 3 attempts) and start on_error.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(on_error_started.load(Ordering::SeqCst), "on_error should have started by now");

    // Second, unrelated frame for a different session, enqueued while the
    // first frame's on_error (500ms sleep) is still pending.
    batcher.enqueue("/root/proj/sess-b.jsonl", vec![entry("b")]);
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The first drain consumed 3 of the 10 shared fail_times. If the
    // second frame's append attempts had started too, more than 3 would be
    // consumed by now; if the worker were still blocked awaiting the first
    // on_error, it would still be exactly 3.
    let calls_so_far = 10 - store.fail_times.load(Ordering::SeqCst);
    assert!(
        calls_so_far > 3,
        "expected the second frame's append attempts to have started while \
         the first frame's on_error (500ms sleep) was still pending, but \
         only {calls_so_far} store.append call(s) were made -- the worker \
         appears blocked on the slow on_error callback"
    );
}

#[tokio::test]
async fn unresolvable_file_path_is_dropped_without_appending() {
    let store = Arc::new(RecordingStore::new(0));
    let batcher =
        TranscriptMirrorBatcher::new(store.clone(), "/root", no_op_on_error(), SessionStoreFlushMode::Batched);

    batcher.enqueue("/elsewhere/proj/sess.jsonl", vec![entry("a")]);
    batcher.flush().await;

    assert!(store.appends.lock().await.is_empty());
}
