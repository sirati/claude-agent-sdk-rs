//! Contracts 1-6: required `append`/`load` behavior + project isolation.

use std::future::Future;
use std::sync::Arc;

use serde_json::json;

use super::super::store::SessionStore;
use super::{entry, key};

pub(super) async fn run<F, Fut>(make_store: &F, has_list_sessions: bool)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn SessionStore>>,
{
    let k = key("proj", "sess");

    // 1. append then load returns same entries in same order.
    let store = make_store().await;
    store
        .append(
            &k,
            vec![entry(json!({"uuid": "b", "n": 1})), entry(json!({"uuid": "a", "n": 2}))],
        )
        .await
        .unwrap();
    // Deep-equal is the contract; byte-equal serialization is intentionally
    // NOT checked (Postgres JSONB may reorder keys — never byte-compared).
    assert_eq!(
        store.load(&k).await.unwrap(),
        Some(vec![entry(json!({"uuid": "b", "n": 1})), entry(json!({"uuid": "a", "n": 2}))])
    );

    // 2. load unknown key returns None.
    let store = make_store().await;
    assert_eq!(store.load(&key("proj", "nope")).await.unwrap(), None);
    store.append(&k, vec![entry(json!({"uuid": "x", "n": 1}))]).await.unwrap();
    let mut unknown_subpath = k.clone();
    unknown_subpath.subpath = Some("nope".to_string());
    assert_eq!(store.load(&unknown_subpath).await.unwrap(), None);

    // 3. multiple append calls preserve call order.
    let store = make_store().await;
    store.append(&k, vec![entry(json!({"uuid": "z", "n": 1}))]).await.unwrap();
    store
        .append(&k, vec![entry(json!({"uuid": "a", "n": 2})), entry(json!({"uuid": "m", "n": 3}))])
        .await
        .unwrap();
    store.append(&k, vec![entry(json!({"uuid": "b", "n": 4}))]).await.unwrap();
    assert_eq!(
        store.load(&k).await.unwrap(),
        Some(vec![
            entry(json!({"uuid": "z", "n": 1})),
            entry(json!({"uuid": "a", "n": 2})),
            entry(json!({"uuid": "m", "n": 3})),
            entry(json!({"uuid": "b", "n": 4})),
        ])
    );

    // 4. append([]) is a no-op.
    let store = make_store().await;
    store.append(&k, vec![entry(json!({"uuid": "a", "n": 1}))]).await.unwrap();
    store.append(&k, vec![]).await.unwrap();
    assert_eq!(store.load(&k).await.unwrap(), Some(vec![entry(json!({"uuid": "a", "n": 1}))]));

    // 5. subpath keys are stored independently of main.
    let store = make_store().await;
    let mut sub = k.clone();
    sub.subpath = Some("subagents/agent-1".to_string());
    store.append(&k, vec![entry(json!({"uuid": "m", "n": 1}))]).await.unwrap();
    store.append(&sub, vec![entry(json!({"uuid": "s", "n": 1}))]).await.unwrap();
    assert_eq!(store.load(&k).await.unwrap(), Some(vec![entry(json!({"uuid": "m", "n": 1}))]));
    assert_eq!(store.load(&sub).await.unwrap(), Some(vec![entry(json!({"uuid": "s", "n": 1}))]));

    // 6. project_key isolation.
    let store = make_store().await;
    let a = key("A", "s1");
    let b = key("B", "s1");
    store.append(&a, vec![entry(json!({"from": "A"}))]).await.unwrap();
    store.append(&b, vec![entry(json!({"from": "B"}))]).await.unwrap();
    assert_eq!(store.load(&a).await.unwrap(), Some(vec![entry(json!({"from": "A"}))]));
    assert_eq!(store.load(&b).await.unwrap(), Some(vec![entry(json!({"from": "B"}))]));
    if has_list_sessions {
        assert_eq!(store.list_sessions("A").await.unwrap().len(), 1);
        assert_eq!(store.list_sessions("B").await.unwrap().len(), 1);
    }
}
