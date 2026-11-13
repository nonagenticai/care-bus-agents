//! `GET /api/jobs/{job}/events`: live SSE of one job's bus (Pub/Sub `console` mirror), recipient-ack and chat
//! events. Both channels are the job's scoped channels, so another scope's frames never reach the connection.
//!
//! `ack` frames are lossy like everything on Pub/Sub; a client that reconnects rebuilds ack state from
//! `GET /api/cases/{id}` (each trace entry's `acked` is recomputed from XINFO GROUPS + XPENDING).
//!
//! The Pub/Sub connection lives inside the response stream, not in a spawned task: when the client goes
//! away axum drops the body, which drops the generator, the message stream and the connection.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{ApiQuery, ApiResult},
    state::AppState,
    tenancy::JobMember,
};

/// Relative path; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/events", get(events))
}

#[derive(Deserialize)]
struct Filter {
    conversation_id: Option<String>,
}

async fn events(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(f): ApiQuery<Filter>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let bus = s.bus.scoped(&ctx.scope());
    // Redis down here is a 503 envelope (From<RedisError>), never an empty stream.
    let mut pubsub = bus.client().get_async_pubsub().await?;
    let console = bus.streams.console();
    let chat = carebus::chat::chat_channel(&bus);
    pubsub.subscribe(&console).await?;
    pubsub.subscribe(&chat).await?;
    let conv = f.conversation_id;
    let prefix = bus.streams.key("");
    let stream = async_stream::stream! {
        let mut msgs = pubsub.into_on_message();
        // case_id -> conversation, cached per connection. Only a FOUND link is cached: the first event of
        // every new case (request.qualified) is published before link_case runs, so caching "no owner"
        // would hide that case for the rest of the connection.
        let mut owner: HashMap<String, String> = HashMap::new();
        let mut chat_seq: u64 = 0;
        while let Some(m) = msgs.next().await {
            let channel = m.get_channel_name().to_string();
            let Ok(raw) = m.get_payload::<String>() else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&raw) else { continue };
            if channel == chat {
                if conv.as_deref().is_none_or(|c| v["conversation_id"] == c) {
                    // Per-connection counter: two chat events in one millisecond must not share an id.
                    chat_seq += 1;
                    let id = format!("chat-{}-{chat_seq}", v["at_ms"].as_u64().unwrap_or(0));
                    yield Ok(Event::default().event("chat").id(id).data(raw));
                }
            } else {
                // The console channel carries two frames: a publish (`{stream, entry_id, envelope, consumer_group,
                // acked}`) and the recipient group's XACK (`{kind: "ack", stream, entry_id, consumer_group,
                // correlation_id}`). Both belong to the case in their correlation id.
                let is_ack = v["kind"] == "ack";
                if let Some(c) = conv.as_deref() {
                    let cid = if is_ack { &v["correlation_id"] } else { &v["envelope"]["correlation_id"] };
                    let case = cid.as_str().unwrap_or_default().to_string();
                    if !owner.contains_key(&case) {
                        let got: Option<String> = match redis::cmd("GET")
                            .arg(carebus::chat::case_conv_key(&bus, &case))
                            .query_async(&mut bus.conn())
                            .await
                        {
                            Ok(g) => g,
                            Err(e) => {
                                tracing::warn!(error = %e, case = %case, "sse owner lookup failed; frame skipped");
                                continue;
                            }
                        };
                        if let Some(conv_id) = got {
                            owner.insert(case.clone(), conv_id);
                        }
                    }
                    if owner.get(&case).map(String::as_str) != Some(c) {
                        continue;
                    }
                }
                let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
                // One BusEvent shape across REST and SSE: the stream key is UNPREFIXED.
                let full = v["stream"].as_str().unwrap_or_default();
                let stream = full.strip_prefix(prefix.as_str()).unwrap_or(full);
                if is_ack {
                    // No SSE id: an ack is not a stream entry, and must not move a Last-Event-ID.
                    let data = json!({
                        "stream": stream,
                        "entry_id": entry_id,
                        "consumer_group": v["consumer_group"],
                        "correlation_id": v["correlation_id"],
                    });
                    yield Ok(Event::default().event("ack").data(data.to_string()));
                    continue;
                }
                let data = json!({
                    "stream": stream,
                    "entry_id": entry_id,
                    "envelope": v["envelope"],
                    "consumer_group": v["consumer_group"],
                    "acked": v["acked"],
                });
                yield Ok(Event::default().event("bus").id(entry_id).data(data.to_string()));
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}
