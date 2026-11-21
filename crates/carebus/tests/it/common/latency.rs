//! A TCP proxy that forwards to Redis with a fixed one-way delay, so a test can measure what a
//! cross-site round trip costs. Measured 2026-11-21: `mf1` -> `worker3` (where `carebus-redis`
//! runs) is 19.7 ms; a same-datacenter pair is ~1 ms; this box to compose Redis is 0.15 ms.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A running proxy. Dropping it stops accepting connections and aborts the accept loop.
pub struct DelayProxy {
    /// `redis://127.0.0.1:<port>` to hand to `Bus::connect`.
    pub url: String,
    /// Client-to-server chunks seen by the proxy: one per command or pipeline issued. It counts
    /// coalesced writes as one, which is exactly what a round trip is.
    pub commands: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for DelayProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Forward `upstream` (`redis://host:port`) through a loopback port, adding `one_way` to each
/// direction of every connection, so one command round trip costs `2 * one_way`.
pub async fn delay_proxy(upstream: &str, one_way: Duration) -> DelayProxy {
    let addr = upstream
        .trim_start_matches("redis://")
        .trim_end_matches('/')
        .to_string();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the delay proxy");
    let local = listener.local_addr().expect("proxy address");
    let commands = Arc::new(AtomicUsize::new(0));
    let counted = commands.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                break;
            };
            let Ok(server) = TcpStream::connect(&addr).await else {
                continue;
            };
            let counted = counted.clone();
            tokio::spawn(async move {
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                let _ = tokio::join!(
                    pump(&mut cr, &mut sw, one_way, Some(&counted)),
                    pump(&mut sr, &mut cw, one_way, None),
                );
            });
        }
    });
    DelayProxy {
        url: format!("redis://{local}"),
        commands,
        task,
    }
}

/// Copy bytes one way, sleeping `one_way` before every write. The buffer is large enough that a
/// RESP command or a small reply is one chunk: one command costs one sleep per direction.
async fn pump<R, W>(
    r: &mut R,
    w: &mut W,
    one_way: Duration,
    counted: Option<&AtomicUsize>,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if let Some(counted) = counted {
            counted.fetch_add(1, Ordering::Relaxed);
        }
        tokio::time::sleep(one_way).await;
        w.write_all(&buf[..n]).await?;
        w.flush().await?;
    }
}
