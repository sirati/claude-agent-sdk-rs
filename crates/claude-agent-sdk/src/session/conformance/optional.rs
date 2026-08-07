//! Contracts 7-14: optional `list_sessions` / `list_session_summaries` /
//! `delete` / `list_subkeys` behavior.

use std::future::Future;
use std::sync::Arc;

use serde_json::json;

use super::super::store::SessionStore;
use super::super::types::SessionListSubkeysKey;
use super::{entry, key};

const EPOCH_MS_FLOOR: i64 = 1_000_000_000_000; // rules out epoch-seconds (~2001 in ms)

pub(super) async fn run<F, Fut>(
    make_store: &F,
    has_list_sessions: bool,
    has_list_summaries: bool,
    has_delete: bool,
    has_list_subkeys: bool,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    if has_list_sessions {
        list_sessions_contracts(make_store).await;
    }
    if has_list_summaries {
        list_session_summaries_contract(make_store, has_list_sessions, has_delete).await;
    }
    if has_delete {
        delete_contracts(make_store, has_list_subkeys, has_list_sessions).await;
    }
    if has_list_subkeys {
        list_subkeys_contracts(make_store).await;
    }
}

// 7 + 8: list_sessions returns session_ids for a project, with epoch-ms
// mtimes, and excludes subagent subpaths.
async fn list_sessions_contracts<F, Fut>(make_store: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let store = make_store().await;
    store.append(&key("proj", "a"), vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&key("proj", "b"), vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&key("other", "c"), vec![entry(json!({"n": 1}))]).await.unwrap();
    let mut sessions = store.list_sessions("proj").await.unwrap();
    sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    assert_eq!(
        sessions.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert!(sessions.iter().all(|s| s.mtime > EPOCH_MS_FLOOR));
    assert_eq!(store.list_sessions("never-appended-project").await.unwrap(), vec![]);

    let store = make_store().await;
    let main = key("proj", "main");
    let mut sub = main.clone();
    sub.subpath = Some("subagents/agent-1".to_string());
    store.append(&main, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub, vec![entry(json!({"n": 1}))]).await.unwrap();
    let sessions = store.list_sessions("proj").await.unwrap();
    assert_eq!(sessions.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(), vec!["main"]);
}

// Structural half of upstream's contract #14. The semantic half — folding
// customTitle/etc. via the real `fold_session_summary` and asserting exact
// derived fields — is NOT ported here: `fold_session_summary` itself is
// owned by a parallel port of `_internal/session_summary.py` that hasn't
// landed yet (see `InMemorySessionStore`'s placeholder fold). What IS
// asserted below — project scoping, subagent exclusion, mtime clock
// alignment with `list_sessions`, delete cascade — does not depend on the
// real fold and holds for any adapter's summary sidecar.
async fn list_session_summaries_contract<F, Fut>(
    make_store: &F,
    has_list_sessions: bool,
    has_delete: bool,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let store = make_store().await;
    let k = key("proj", "summ-sess");
    store
        .append(
            &k,
            vec![
                entry(json!({"timestamp": "2024-01-01T00:00:00.000Z", "customTitle": "first"})),
                entry(json!({"timestamp": "2024-01-01T00:00:01.000Z"})),
            ],
        )
        .await
        .unwrap();
    store
        .append(&k, vec![entry(json!({"timestamp": "2024-01-01T00:00:02.000Z", "customTitle": "second"}))])
        .await
        .unwrap();
    store
        .append(&key("other", "elsewhere"), vec![entry(json!({"timestamp": "2024-01-01T00:00:00.000Z"}))])
        .await
        .unwrap();

    let summaries = store.list_session_summaries("proj").await.unwrap();
    assert_eq!(summaries.len(), 1);
    let summ = &summaries[0];
    assert_eq!(summ.session_id, "summ-sess");
    assert!(summ.mtime > EPOCH_MS_FLOOR);
    // Clock alignment: sidecar mtime is storage write time, and must share a
    // clock with list_sessions().mtime for the same session — adapters that
    // derive it from entry ISO timestamps instead would report a strictly
    // older value and make every sidecar look stale to a fast-path
    // freshness check built on top of these two methods.
    if has_list_sessions {
        let listed = store.list_sessions("proj").await.unwrap();
        let ls_mtime = listed.iter().find(|s| s.session_id == "summ-sess").unwrap().mtime;
        assert!(summ.mtime >= ls_mtime);
    }
    assert!(summ.data.is_object());

    // Subagent appends must NOT affect the main session's summary.
    let mut sub = k.clone();
    sub.subpath = Some("subagents/agent-1".to_string());
    store
        .append(&sub, vec![entry(json!({"timestamp": "2024-01-01T00:00:09.000Z", "customTitle": "subagent"}))])
        .await
        .unwrap();
    let after_sub = store.list_session_summaries("proj").await.unwrap();
    assert_eq!(after_sub.iter().find(|s| s.session_id == "summ-sess").unwrap().data, summ.data);

    assert_eq!(store.list_session_summaries("never-appended-project").await.unwrap(), vec![]);

    if has_delete {
        store.delete(&k).await.unwrap();
        assert_eq!(store.list_session_summaries("proj").await.unwrap(), vec![]);
    }
}

// 9-11: delete semantics.
async fn delete_contracts<F, Fut>(make_store: &F, has_list_subkeys: bool, has_list_sessions: bool)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let k = key("proj", "sess");

    // 9. delete main then load returns None; deleting a never-written key is
    // a no-op.
    let store = make_store().await;
    store.delete(&key("proj", "never-written")).await.unwrap();
    store.append(&k, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.delete(&k).await.unwrap();
    assert_eq!(store.load(&k).await.unwrap(), None);

    // 10. delete main cascades to subkeys, but not to other sessions/projects.
    let store = make_store().await;
    let mut sub1 = k.clone();
    sub1.subpath = Some("subagents/agent-1".to_string());
    let mut sub2 = k.clone();
    sub2.subpath = Some("subagents/agent-2".to_string());
    let other = key("proj", "sess2");
    let other_proj = key("other-proj", &k.session_id);
    store.append(&k, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub1, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub2, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&other, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&other_proj, vec![entry(json!({"n": 1}))]).await.unwrap();

    store.delete(&k).await.unwrap();

    assert_eq!(store.load(&k).await.unwrap(), None);
    assert_eq!(store.load(&sub1).await.unwrap(), None);
    assert_eq!(store.load(&sub2).await.unwrap(), None);
    assert_eq!(store.load(&other).await.unwrap().map(|v| v.len()), Some(1));
    assert_eq!(store.load(&other_proj).await.unwrap().map(|v| v.len()), Some(1));
    if has_list_subkeys {
        assert_eq!(store.list_subkeys(&SessionListSubkeysKey::from(&k)).await.unwrap(), Vec::<String>::new());
    }
    if has_list_sessions {
        let listed = store.list_sessions(&k.project_key).await.unwrap();
        assert!(!listed.iter().any(|s| s.session_id == k.session_id));
    }

    // 11. delete with an explicit subpath removes only that one subkey.
    let store = make_store().await;
    store.append(&k, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub1, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub2, vec![entry(json!({"n": 1}))]).await.unwrap();

    store.delete(&sub1).await.unwrap();

    assert_eq!(store.load(&sub1).await.unwrap(), None);
    assert_eq!(store.load(&sub2).await.unwrap().map(|v| v.len()), Some(1));
    assert_eq!(store.load(&k).await.unwrap().map(|v| v.len()), Some(1));
    if has_list_subkeys {
        assert_eq!(store.list_subkeys(&SessionListSubkeysKey::from(&k)).await.unwrap(), vec!["subagents/agent-2".to_string()]);
    }
}

// 12-13: list_subkeys semantics.
async fn list_subkeys_contracts<F, Fut>(make_store: &F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let k = key("proj", "sess");

    let store = make_store().await;
    let mut sub1 = k.clone();
    sub1.subpath = Some("subagents/agent-1".to_string());
    let mut sub2 = k.clone();
    sub2.subpath = Some("subagents/agent-2".to_string());
    let mut other_session_sub = key("proj", "other-sess");
    other_session_sub.subpath = Some("subagents/agent-x".to_string());
    store.append(&k, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub1, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&sub2, vec![entry(json!({"n": 1}))]).await.unwrap();
    store.append(&other_session_sub, vec![entry(json!({"n": 1}))]).await.unwrap();

    let mut subkeys = store.list_subkeys(&SessionListSubkeysKey::from(&k)).await.unwrap();
    subkeys.sort();
    assert_eq!(subkeys, vec!["subagents/agent-1".to_string(), "subagents/agent-2".to_string()]);

    let store = make_store().await;
    store.append(&k, vec![entry(json!({"n": 1}))]).await.unwrap();
    assert_eq!(store.list_subkeys(&SessionListSubkeysKey::from(&k)).await.unwrap(), Vec::<String>::new());
    assert_eq!(
        store
            .list_subkeys(&SessionListSubkeysKey::from(&key("proj", "never-appended")))
            .await
            .unwrap(),
        Vec::<String>::new()
    );
}
