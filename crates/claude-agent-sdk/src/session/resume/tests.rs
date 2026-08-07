//! Tests for [`super::materialize_resume_session`] and friends, ported from
//! upstream `tests/test_session_resume.py`'s `TestNoMaterialization`,
//! `TestHappyPath`, `TestSubkeyMaterialization`, and `TestTimeoutsAndErrors`
//! classes (client-integration tests in that file are out of scope here —
//! they exercise `ClaudeSDKClient`, owned by a parallel port). See the
//! module-level doc comment on `resume.rs` for the one upstream test
//! (cancellation-cleans-temp-dir) that has no faithful Rust equivalent and
//! was intentionally not ported.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::session::{
    InMemorySessionStore, SessionListSubkeysKey, SessionStoreError, SessionStoreListEntry,
};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const SESSION_ID_2: &str = "660e8400-e29b-41d4-a716-446655440000";

/// `materialize_resume_session` always creates its temp dir directly under
/// the shared OS temp dir (production code has no injection point for a
/// custom base, matching upstream's un-mockable `tempfile.mkdtemp` — its
/// own test suite deals with this by monkeypatching the function itself,
/// which Rust has no equivalent for). Tests that assert "no `claude-resume-*`
/// dir was leaked" by scanning that shared directory would otherwise race
/// against every *other* test in this file that creates one concurrently
/// (cargo test runs `#[tokio::test]` fns on separate OS threads by
/// default). Every test that creates a `claude-resume-*` dir — and both
/// tests that scan for leaked ones — hold this lock for their duration so
/// the scan only ever sees this file's own, fully-settled state.
static TEMP_DIR_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn temp_dir_guard() -> tokio::sync::MutexGuard<'static, ()> {
    TEMP_DIR_LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

fn entry(type_: &str, uuid: &str) -> SessionStoreEntry {
    SessionStoreEntry::new(type_, json!({"uuid": uuid}).as_object().unwrap().clone())
}

/// A `cwd` plus an isolated `CLAUDE_CONFIG_DIR` (so `copy_auth_files`
/// never touches the real `$HOME`/Keychain) and a matching `project_key`.
struct Fixture {
    _cwd_dir: tempfile::TempDir,
    _config_dir: tempfile::TempDir,
    cwd: std::path::PathBuf,
    project_key: String,
    env: std::collections::HashMap<String, String>,
}

fn fixture() -> Fixture {
    let cwd_dir = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path().to_path_buf();
    let cwd_str = cwd.to_str().expect("tempdir path is valid UTF-8");
    let project_key = project_key_for_directory(Some(cwd_str));
    let mut env = std::collections::HashMap::new();
    env.insert("CLAUDE_CONFIG_DIR".to_string(), config_dir.path().display().to_string());
    Fixture { _cwd_dir: cwd_dir, _config_dir: config_dir, cwd, project_key, env }
}

fn options(f: &Fixture, store: Arc<dyn SessionStore>) -> ClaudeAgentOptions {
    ClaudeAgentOptions::builder()
        .cwd(f.cwd.clone())
        .env(f.env.clone())
        .session_store(store)
        .build()
}

// ---------------------------------------------------------------------
// No-materialization cases
// ---------------------------------------------------------------------

#[tokio::test]
async fn no_store_returns_none() {
    let f = fixture();
    let opts = ClaudeAgentOptions::builder()
        .cwd(f.cwd.clone())
        .resume(SESSION_ID)
        .build();
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

#[tokio::test]
async fn no_resume_or_continue_returns_none() {
    let f = fixture();
    let store = Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>;
    let opts = options(&f, store);
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

#[tokio::test]
async fn non_uuid_session_id_returns_none() {
    let f = fixture();
    let store = Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>;
    let opts = ClaudeAgentOptions {
        resume: Some("not-a-uuid".to_string()),
        ..options(&f, store)
    };
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

#[tokio::test]
async fn load_returns_none_yields_none() {
    let f = fixture();
    let store = Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>;
    let opts = ClaudeAgentOptions { resume: Some(SESSION_ID.to_string()), ..options(&f, store) };
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

#[tokio::test]
async fn continue_with_empty_list_sessions_returns_none() {
    let f = fixture();
    let store = Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>;
    let opts =
        ClaudeAgentOptions { continue_conversation: true, ..options(&f, store) };
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

// ---------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------

#[tokio::test]
async fn resume_writes_jsonl_and_cleanup_removes_dir() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    let entries = vec![entry("user", "u1"), entry("assistant", "a1")];
    let key = SessionKey::new(f.project_key.clone(), SESSION_ID);
    store.append(&key, entries.clone()).await.unwrap();

    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(store))
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();
    assert_eq!(m.resume_session_id, SESSION_ID);
    assert!(m.config_dir.is_dir());

    let jsonl = m.config_dir.join("projects").join(&f.project_key).join(format!("{SESSION_ID}.jsonl"));
    let text = tokio::fs::read_to_string(&jsonl).await.unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let parsed: Vec<SessionStoreEntry> =
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(parsed, entries);

    m.cleanup().await;
    assert!(!m.config_dir.exists());
}

#[tokio::test]
async fn continue_picks_most_recently_appended_session() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "old")])
        .await
        .unwrap();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID_2), vec![entry("user", "new")])
        .await
        .unwrap();

    let opts =
        ClaudeAgentOptions { continue_conversation: true, ..options(&f, Arc::new(store)) };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();
    assert_eq!(m.resume_session_id, SESSION_ID_2);
    m.cleanup().await;
}

#[tokio::test]
async fn continue_skips_sidechain_sessions() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "main")])
        .await
        .unwrap();
    let sidechain_id = "770e8400-e29b-41d4-a716-446655440000";
    let mut sidechain_entry = entry("user", "sc");
    sidechain_entry.extra.insert("isSidechain".to_string(), serde_json::Value::Bool(true));
    // Appended after the main session, so it naturally has a newer mtime —
    // exercising the "sidechain has the highest mtime" case this guard
    // exists for.
    store
        .append(&SessionKey::new(f.project_key.clone(), sidechain_id), vec![sidechain_entry])
        .await
        .unwrap();

    let opts =
        ClaudeAgentOptions { continue_conversation: true, ..options(&f, Arc::new(store)) };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();
    assert_eq!(m.resume_session_id, SESSION_ID);
    m.cleanup().await;
}

#[tokio::test]
async fn continue_returns_none_when_only_sidechains() {
    let f = fixture();
    let store = InMemorySessionStore::new();
    let mut sidechain_entry = entry("user", "sc");
    sidechain_entry.extra.insert("isSidechain".to_string(), serde_json::Value::Bool(true));
    store
        .append(
            &SessionKey::new(f.project_key.clone(), "880e8400-e29b-41d4-a716-446655440000"),
            vec![sidechain_entry],
        )
        .await
        .unwrap();

    let opts =
        ClaudeAgentOptions { continue_conversation: true, ..options(&f, Arc::new(store)) };
    assert!(materialize_resume_session(&opts).await.unwrap().is_none());
}

// ---------------------------------------------------------------------
// Subkey materialization
// ---------------------------------------------------------------------

#[tokio::test]
async fn subagent_jsonl_and_meta_json_are_written() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "u1")])
        .await
        .unwrap();
    let mut meta = entry("agent_metadata", "unused");
    meta.extra.insert("agentType".to_string(), json!("general"));
    meta.extra.remove("uuid");
    store
        .append(
            &SessionKey::with_subpath(f.project_key.clone(), SESSION_ID, "subagents/agent-abc"),
            vec![entry("user", "su1"), entry("assistant", "sa1"), meta],
        )
        .await
        .unwrap();

    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(store))
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();

    let session_dir = m.config_dir.join("projects").join(&f.project_key).join(SESSION_ID);
    let jsonl = session_dir.join("subagents").join("agent-abc.jsonl");
    let meta_file = session_dir.join("subagents").join("agent-abc.meta.json");

    let lines: Vec<SessionStoreEntry> = tokio::fs::read_to_string(&jsonl)
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines, vec![entry("user", "su1"), entry("assistant", "sa1")]);

    let meta_json: serde_json::Value =
        serde_json::from_str(&tokio::fs::read_to_string(&meta_file).await.unwrap()).unwrap();
    assert_eq!(meta_json, json!({"agentType": "general"}));

    m.cleanup().await;
}

struct EvilStore;

#[async_trait]
impl SessionStore for EvilStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        Ok(())
    }

    async fn load(
        &self,
        key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        match key.subpath.as_deref() {
            Some("subagents/agent-ok") => Ok(Some(vec![entry("user", "ok")])),
            None => Ok(Some(vec![entry("user", "main")])),
            Some(other) => panic!("loaded unsafe subpath {other:?}"),
        }
    }

    async fn list_subkeys(
        &self,
        _key: &SessionListSubkeysKey,
    ) -> std::result::Result<Vec<String>, SessionStoreError> {
        Ok(vec![
            "".into(),
            ".".into(),
            "./".into(),
            "a/.".into(),
            "subagents/.".into(),
            "/etc/passwd".into(),
            "../escape".into(),
            "a/../b".into(),
            "C:escape".into(),
            "C:\\abs".into(),
            "subagents/agent\0x".into(),
            "subagents/agent-ok".into(),
        ])
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn traversal_guards_reject_unsafe_subpaths_but_keep_safe_one() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(EvilStore))
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();

    let session_dir = m.config_dir.join("projects").join(&f.project_key).join(SESSION_ID);
    assert!(session_dir.join("subagents").join("agent-ok.jsonl").is_file());

    // Main transcript wasn't clobbered by any unsafe subpath resolving to it
    // (regression guard for subpath "." previously landing on
    // project_dir/{sid}.jsonl).
    let main_jsonl =
        m.config_dir.join("projects").join(&f.project_key).join(format!("{SESSION_ID}.jsonl"));
    let main: Vec<SessionStoreEntry> = tokio::fs::read_to_string(&main_jsonl)
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(main, vec![entry("user", "main")]);

    m.cleanup().await;
}

struct MinimalStore;

#[async_trait]
impl SessionStore for MinimalStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        Ok(())
    }

    async fn load(
        &self,
        _key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        Ok(Some(vec![entry("user", "u1")]))
    }
}

#[tokio::test]
async fn store_without_list_subkeys_skips_subagents() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(MinimalStore))
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();
    assert!(
        m.config_dir.join("projects").join(&f.project_key).join(format!("{SESSION_ID}.jsonl")).is_file()
    );
    assert!(!m.config_dir.join("projects").join(&f.project_key).join(SESSION_ID).exists());
    m.cleanup().await;
}

// ---------------------------------------------------------------------
// Timeouts and error wrapping
// ---------------------------------------------------------------------

struct SlowLoadStore;

#[async_trait]
impl SessionStore for SlowLoadStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        Ok(())
    }

    async fn load(
        &self,
        _key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(None)
    }
}

#[tokio::test]
async fn load_timeout_raises() {
    let f = fixture();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        load_timeout_ms: 50,
        ..options(&f, Arc::new(SlowLoadStore))
    };
    let err = materialize_resume_session(&opts).await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
}

struct HungListStore;

#[async_trait]
impl SessionStore for HungListStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        Ok(())
    }

    async fn load(
        &self,
        _key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        Ok(None)
    }

    async fn list_sessions(
        &self,
        _project_key: &str,
    ) -> std::result::Result<Vec<SessionStoreListEntry>, SessionStoreError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(vec![])
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn list_sessions_timeout_on_continue_path() {
    let f = fixture();
    let opts = ClaudeAgentOptions {
        continue_conversation: true,
        load_timeout_ms: 50,
        ..options(&f, Arc::new(HungListStore))
    };
    let err = materialize_resume_session(&opts).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("list_sessions()") && msg.contains("timed out"), "{msg}");
}

struct HungSubkeysStore {
    inner: InMemorySessionStore,
}

#[async_trait]
impl SessionStore for HungSubkeysStore {
    async fn append(
        &self,
        key: &SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        self.inner.load(key).await
    }

    async fn list_subkeys(
        &self,
        _key: &SessionListSubkeysKey,
    ) -> std::result::Result<Vec<String>, SessionStoreError> {
        tokio::time::sleep(Duration::from_secs(3600)).await;
        Ok(vec![])
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn list_subkeys_timeout_raises_and_cleans_temp_dir() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = HungSubkeysStore { inner: InMemorySessionStore::new() };
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "u1")])
        .await
        .unwrap();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        load_timeout_ms: 50,
        ..options(&f, Arc::new(store))
    };
    let err = materialize_resume_session(&opts).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("list_subkeys()") && msg.contains("timed out"), "{msg}");
    // The load() that ran before mkdtemp succeeded, so a temp dir was
    // created; the failure inside materialize_into() must have removed it.
    // We don't have the path directly (materialize_resume_session doesn't
    // leak it on error), so instead assert indirectly: a fresh happy-path
    // call still works (no leftover state interferes) and, more directly,
    // that no `claude-resume-*` dir from *this* test is left in the OS temp
    // dir root.
    let mut leaked = false;
    if let Ok(mut rd) = tokio::fs::read_dir(std::env::temp_dir()).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_name().to_string_lossy().starts_with("claude-resume-") {
                leaked = true;
            }
        }
    }
    assert!(!leaked, "materialize_resume_session leaked a claude-resume-* temp dir on error");
}

struct LoadErrStore;

#[async_trait]
impl SessionStore for LoadErrStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        Ok(())
    }

    async fn load(
        &self,
        _key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        Err(SessionStoreError::Backend(anyhow::anyhow!("boom")))
    }
}

#[tokio::test]
async fn load_exception_is_wrapped_with_context() {
    let f = fixture();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(LoadErrStore))
    };
    let err = materialize_resume_session(&opts).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("boom") && msg.contains("failed"), "{msg}");
}

struct FailingSubkeysStore {
    inner: InMemorySessionStore,
}

#[async_trait]
impl SessionStore for FailingSubkeysStore {
    async fn append(
        &self,
        key: &SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> std::result::Result<(), SessionStoreError> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: &SessionKey,
    ) -> std::result::Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        self.inner.load(key).await
    }

    async fn list_subkeys(
        &self,
        _key: &SessionListSubkeysKey,
    ) -> std::result::Result<Vec<String>, SessionStoreError> {
        Err(SessionStoreError::Backend(anyhow::anyhow!("subkeys boom")))
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn failure_after_mkdir_cleans_temp_dir() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = FailingSubkeysStore { inner: InMemorySessionStore::new() };
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "u1")])
        .await
        .unwrap();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(store))
    };
    assert!(materialize_resume_session(&opts).await.is_err());

    let mut leaked = false;
    if let Ok(mut rd) = tokio::fs::read_dir(std::env::temp_dir()).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_name().to_string_lossy().starts_with("claude-resume-") {
                leaked = true;
            }
        }
    }
    assert!(!leaked, "materialize_resume_session leaked a claude-resume-* temp dir on error");
}

// ---------------------------------------------------------------------
// apply_materialized_options / build_mirror_batcher
// ---------------------------------------------------------------------

#[tokio::test]
async fn apply_materialized_options_repoints_env_resume_and_continue() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "u1")])
        .await
        .unwrap();
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, Arc::new(store))
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();

    let applied = apply_materialized_options(&opts, &m);
    assert_eq!(
        applied.env.get("CLAUDE_CONFIG_DIR"),
        Some(&m.config_dir.display().to_string())
    );
    assert_eq!(applied.resume.as_deref(), Some(SESSION_ID));
    assert!(!applied.continue_conversation);

    m.cleanup().await;
}

#[tokio::test]
async fn build_mirror_batcher_uses_materialized_projects_dir() {
    let _temp_dir_guard = temp_dir_guard().await;
    let f = fixture();
    let store = InMemorySessionStore::new();
    store
        .append(&SessionKey::new(f.project_key.clone(), SESSION_ID), vec![entry("user", "u1")])
        .await
        .unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(store);
    let opts = ClaudeAgentOptions {
        resume: Some(SESSION_ID.to_string()),
        ..options(&f, store.clone())
    };
    let m = materialize_resume_session(&opts).await.unwrap().unwrap();

    let on_error: crate::session::MirrorErrorCallback = Arc::new(|_, _| Box::pin(async {}));
    let batcher = build_mirror_batcher(
        store.clone(),
        Some(&m),
        &f.env,
        on_error,
        crate::session::SessionStoreFlushMode::Batched,
    );

    // Enqueue a frame whose path is under the materialized projects dir and
    // confirm it resolves and appends (proves projects_dir was wired to the
    // materialized temp dir, not the CLAUDE_CONFIG_DIR fallback).
    let file_path = m
        .config_dir
        .join("projects")
        .join(&f.project_key)
        .join(format!("{SESSION_ID_2}.jsonl"));
    batcher.enqueue(file_path.display().to_string(), vec![entry("user", "mirrored")]);
    batcher.flush().await;

    let key = SessionKey::new(f.project_key.clone(), SESSION_ID_2);
    let mirrored = store.load(&key).await.unwrap().unwrap();
    assert_eq!(mirrored, vec![entry("user", "mirrored")]);

    m.cleanup().await;
}
