//! Latency benchmarks. They run against the compose Redis through a proxy that adds the measured
//! `mf1` -> `worker3` link cost (10 ms each way, 19.7 ms measured) so the cross-site cost of every
//! Redis round trip is visible. Ignored by default; CI never runs them:
//!
//! ```text
//! cargo test -p carebus --test it -- --ignored perf --nocapture
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use carebus::bus::{Bus, ConsumeOptions, Envelope};
use carebus::interaction::{Interaction, RuleQualifier, Turn};
use carebus::spaces::SpaceCatalog;

use crate::common::{DelayProxy, delay_proxy, scope_a};

/// One-way delay. `mf1` -> `worker3` measured 19.7 ms round trip on 2026-11-21; compose Redis on
/// this box is 0.15 ms.
const ONE_WAY: Duration = Duration::from_millis(10);

/// A scoped bus whose every connection goes through the delay proxy.
async fn proxied(test: &str) -> (DelayProxy, Bus) {
    let upstream = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6391".into());
    let proxy = delay_proxy(&upstream, ONE_WAY).await;
    let prefix = format!("perf:{test}:{}:", uuid::Uuid::new_v4().simple());
    let bus = Bus::connect(&proxy.url, &prefix)
        .await
        .expect("compose Redis must be reachable: docker compose up -d")
        .scoped(&scope_a());
    (proxy, bus)
}

fn envelope(n: usize) -> Envelope {
    Envelope::new(
        "order-support.return.request.qualified",
        "perf",
        &format!("case-{n}"),
        serde_json::json!({"n": n}),
    )
    .expect("envelope")
}

/// Cost of one publish: today `XADD` + console `PUBLISH`, each a full round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: cargo test -- --ignored perf"]
async fn perf_publish_round_trips() {
    let (proxy, bus) = proxied("publish").await;
    let stream = bus.streams.customer("order-support");
    const N: usize = 100;
    let before = proxy.commands.load(Ordering::Relaxed);
    let started = Instant::now();
    for n in 0..N {
        bus.publish(&stream, &envelope(n)).await.expect("publish");
    }
    let elapsed = started.elapsed();
    let commands = proxy.commands.load(Ordering::Relaxed) - before;
    println!(
        "perf publish: {N} publishes in {elapsed:?} = {:.2} ms/publish, {:.2} commands/publish",
        elapsed.as_secs_f64() * 1000.0 / N as f64,
        commands as f64 / N as f64
    );
}

/// Cost of consuming a batch: today every message pays five sequential admin round trips.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: cargo test -- --ignored perf"]
async fn perf_consume_throughput() {
    let (proxy, bus) = proxied("consume").await;
    let stream = bus.streams.customer("order-support");
    const N: usize = 64;
    for n in 0..N {
        bus.publish(&stream, &envelope(n)).await.expect("publish");
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let counted = seen.clone();
    let opts = ConsumeOptions {
        batch: 16,
        block: Duration::from_millis(100),
        ..ConsumeOptions::default()
    };
    let before = proxy.commands.load(Ordering::Relaxed);
    let started = Instant::now();
    let task = tokio::spawn({
        let bus = bus.clone();
        let stream = stream.clone();
        async move {
            bus.consume_all(
                vec![stream],
                "perf-consumers",
                "perf-1",
                opts,
                move |_env| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), anyhow::Error>(())
                    }
                },
            )
            .await
        }
    });
    while seen.load(Ordering::SeqCst) < N {
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "drained {} of {N}",
            seen.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    task.abort();
    let elapsed = started.elapsed();
    let commands = proxy.commands.load(Ordering::Relaxed) - before;
    println!(
        "perf consume: {N} messages in {elapsed:?} = {:.2} ms/message, {:.2} commands/message",
        elapsed.as_secs_f64() * 1000.0 / N as f64,
        commands as f64 / N as f64
    );
}

/// Cost of a whole delivery-status case: qualify, publish, orchestrator, workers, executor, reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: cargo test -- --ignored perf"]
async fn perf_case_latency() {
    let (proxy, bus) = proxied("case").await;
    let harness = crate::flow::harness_at(
        bus.clone(),
        Duration::from_secs(3),
        SpaceCatalog::bootstrap(),
    )
    .await;
    let agent = Arc::new(Interaction::new(harness.bus.clone(), RuleQualifier));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let replies = agent.clone();
    let replies_task = tokio::spawn(async move {
        replies
            .run_replies("perf-interaction", move |case, text| {
                let _ = tx.send((case, text));
            })
            .await
    });
    const K: usize = 5;
    let before = proxy.commands.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..K {
        let mut draft = None;
        let turn = agent
            .handle(&mut draft, "Where is my airfryer?")
            .await
            .unwrap();
        assert!(
            matches!(turn, Turn::Published(_)),
            "the rule qualifier publishes a delivery-status case in one turn"
        );
    }
    for _ in 0..K {
        tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("every case replies within 120 s")
            .expect("the reply channel stays open");
    }
    let elapsed = started.elapsed();
    replies_task.abort();
    let commands = proxy.commands.load(Ordering::Relaxed) - before;
    println!(
        "perf case: {K} delivery-status cases in {elapsed:?} = {:.0} ms/case, {:.0} commands/case",
        elapsed.as_secs_f64() * 1000.0 / K as f64,
        commands as f64 / K as f64
    );
}
