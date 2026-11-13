//! The scope registry and the per-scope supervisor: scopes start and stop with the registry, a failed scope
//! task is restarted.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carebus::{Scope, scopes, supervisor};

/// Sets its flag when dropped: the scope task was aborted.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
    }
}

#[tokio::test]
async fn registry_lists_replaces_and_skips_garbage() {
    let root = crate::common::root("registry").await;
    let (a, b) = (crate::common::scope_a(), crate::common::scope_b());
    assert!(scopes::list(&root).await.unwrap().is_empty());
    scopes::add(&root, &a).await.unwrap();
    scopes::add(&root, &a).await.unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(root.root_key(scopes::SCOPES_KEY))
        .arg("not-a-scope")
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert_eq!(scopes::list(&root).await.unwrap(), vec![a.clone()]);
    scopes::replace_all(&root, &[b.clone(), b.clone()])
        .await
        .unwrap();
    assert_eq!(scopes::list(&root).await.unwrap(), vec![b.clone()]);
    scopes::remove(&root, &b).await.unwrap();
    assert!(scopes::list(&root).await.unwrap().is_empty());
    scopes::add(&root, &a).await.unwrap();
    scopes::replace_all(&root, &[]).await.unwrap();
    assert!(scopes::list(&root).await.unwrap().is_empty());
    // the registry lives at the environment root, not under any scope
    let scoped_members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(root.scoped(&a).streams.key(scopes::SCOPES_KEY))
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert!(scoped_members.is_empty());
}

#[tokio::test]
async fn scopes_start_and_stop() {
    let root = crate::common::root("sup").await;
    let started = Arc::new(Mutex::new(Vec::<Scope>::new()));
    let a_dropped = Arc::new(AtomicBool::new(false));
    let (st, ad) = (started.clone(), a_dropped.clone());
    let sup = tokio::spawn(supervisor::run_per_scope(
        root.clone(),
        "test",
        move |bus, s| {
            let (st, ad) = (st.clone(), ad.clone());
            async move {
                assert_eq!(bus.scope(), Some(&s), "the task gets its scope's bus");
                let _guard = (s == crate::common::scope_a()).then(|| DropFlag(ad));
                st.lock().unwrap().push(s);
                std::future::pending::<()>().await;
                Ok(())
            }
        },
    ));
    // the supervisor subscribes before it reads the registry; give it a moment, the 30 s resync is the backstop
    tokio::time::sleep(Duration::from_millis(200)).await;
    scopes::add(&root, &crate::common::scope_a()).await.unwrap();
    crate::common::eventually(|| started.lock().unwrap().contains(&crate::common::scope_a())).await;
    scopes::add(&root, &crate::common::scope_b()).await.unwrap();
    crate::common::eventually(|| started.lock().unwrap().len() == 2).await;
    assert!(!a_dropped.load(SeqCst));
    scopes::remove(&root, &crate::common::scope_a())
        .await
        .unwrap();
    crate::common::eventually(|| a_dropped.load(SeqCst)).await;
    // b keeps running and nothing was started twice
    assert_eq!(started.lock().unwrap().len(), 2);
    assert!(!sup.is_finished());
    sup.abort();
}

#[tokio::test]
async fn restarts_a_failed_scope_task() {
    let root = crate::common::root("sup-restart").await;
    scopes::add(&root, &crate::common::scope_a()).await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    let sup = tokio::spawn(supervisor::run_per_scope(
        root.clone(),
        "test",
        move |_bus, _s| {
            let c = c.clone();
            async move {
                if c.fetch_add(1, SeqCst) == 0 {
                    anyhow::bail!("first run fails");
                }
                std::future::pending::<()>().await;
                Ok(())
            }
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(7);
    while calls.load(SeqCst) < 2 {
        assert!(
            Instant::now() < deadline,
            "restarted {} times",
            calls.load(SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    sup.abort();
}
