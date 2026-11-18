//! A consumer whose connection dies must reconnect and keep reading. Before the reconnect loop, the first
//! transport error returned `Err` from `consume`, `consume_all`'s `try_join_all` tore down the pod's other
//! streams, and the supervisor restarted the whole scope 5 s later.
use std::time::Duration;

use carebus::bus::{Bus, Envelope};
use serde_json::json;

/// The test Redis (the same source `common` uses).
fn url() -> String {
    std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6391".into())
}

/// The host and port of the test Redis (`redis://[user:pw@]host:port[/db]`), for the proxy below.
fn upstream_addr(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// A TCP proxy in front of the test Redis, bound before it is spawned (so the caller can connect at once) and
/// killed by `shutdown`, which closes every connection it carries: a real transport failure for the consumer
/// without touching the server or the other tests.
async fn start_proxy(
    addr: std::net::SocketAddr,
    upstream: String,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                accepted = listener.accept() => {
                    let Ok((mut inbound, _)) = accepted else { break };
                    let upstream = upstream.clone();
                    let mut sd = shutdown.clone();
                    tokio::spawn(async move {
                        if let Ok(mut out) = tokio::net::TcpStream::connect(&upstream).await {
                            tokio::select! {
                                _ = sd.changed() => {}
                                _ = tokio::io::copy_bidirectional(&mut inbound, &mut out) => {}
                            }
                        }
                    });
                }
            }
        }
    })
}

#[tokio::test]
async fn a_consumer_reconnects_after_a_transport_failure_and_keeps_reading() {
    let url = url();
    let upstream = upstream_addr(&url);
    // A loopback address nothing listens on yet: the proxy takes it, and takes it again after the failure.
    let addr = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let first_proxy = start_proxy(addr, upstream.clone(), stopped).await;

    let prefix = format!("test:reconnect:{}:", uuid::Uuid::new_v4().simple());
    let proxied_url = url.replacen(&upstream, &addr.to_string(), 1);
    let proxied = Bus::connect(&proxied_url, &prefix)
        .await
        .expect("the proxy answers");
    let direct = Bus::connect(&url, &prefix)
        .await
        .expect("the test Redis answers");
    let scope = crate::common::scope_a();
    let stream = proxied.streams.customer("faq");
    let group = "reconnect-test";

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let (b, s) = (proxied.scoped(&scope), stream.clone());
    let consumer = tokio::spawn(async move {
        b.consume(&s, group, "c1", crate::common::fast(), move |_env| {
            let tx = tx.clone();
            async move {
                tx.send(()).await.ok();
                Ok(())
            }
        })
        .await
    });

    let entry = |case: &str| {
        Envelope::new("faq.request.qualified", "interaction", case, json!({})).unwrap()
    };
    let first_id = direct
        .scoped(&scope)
        .publish(&stream, &entry("CASE-1"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("the first entry is read through the proxy")
        .expect("the handler ran");
    // handled and acked before the failure, so the entry below is the only one this consumer can see
    assert!(direct.is_acked(&stream, group, &first_id).await.unwrap());

    // Break the transport under the consumer only: the publisher keeps the direct connection.
    stop.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), first_proxy).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !consumer.is_finished(),
        "consume returned on a transport failure instead of reconnecting"
    );

    // The same address comes back: the next entry has to be read through it.
    let (stop_again, stopped_again) = tokio::sync::watch::channel(false);
    let second_proxy = start_proxy(addr, upstream, stopped_again).await;
    direct
        .scoped(&scope)
        .publish(&stream, &entry("CASE-2"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("the consumer reconnected and read the next entry")
        .expect("the handler ran");

    consumer.abort();
    stop_again.send(true).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), second_proxy).await;
}
