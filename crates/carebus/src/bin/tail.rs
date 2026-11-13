//! Live console of one scope: `tail <tenant>/<job> [stream]` prints every publish and recipient ack from that
//! scope's Pub/Sub mirror, optionally only for one stream (unprefixed, e.g. `bus:customer:faq`). Lossy by design.
use anyhow::Context;
use carebus::Scope;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let scope = Scope::parse(&args.next().context("usage: tail <tenant>/<job> [stream]")?)?;
    let bus = carebus::bus_from_env().await?.scoped(&scope);
    let prefix = bus.streams.prefix().to_string();
    let only = args.next().map(|s| bus.streams.key(&s));
    let mut pubsub = bus.client().get_async_pubsub().await?;
    pubsub.subscribe(bus.streams.console()).await?;
    let mut stream = pubsub.on_message();
    use futures_util::StreamExt;
    while let Some(msg) = stream.next().await {
        let v: serde_json::Value = serde_json::from_str(&msg.get_payload::<String>()?)?;
        let full = v["stream"].as_str().unwrap_or("");
        if only.as_deref().is_some_and(|o| o != full) {
            continue;
        }
        let name = full.strip_prefix(prefix.as_str()).unwrap_or(full);
        if v["kind"] == "ack" {
            println!(
                "{:<24} ack {:<12} {:<22} by {}",
                name,
                v["entry_id"].as_str().unwrap_or(""),
                v["correlation_id"].as_str().unwrap_or(""),
                v["consumer_group"].as_str().unwrap_or("")
            );
            continue;
        }
        let e = &v["envelope"];
        println!(
            "{:<24} {:<16} {:<22} {} -> {}",
            name,
            e["topic"].as_str().unwrap_or(""),
            e["correlation_id"].as_str().unwrap_or(""),
            e["from"].as_str().unwrap_or(""),
            e["payload"]
        );
    }
    Ok(())
}
