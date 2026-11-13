//! Read/admin helpers over the bus streams. Only `{prefix}bus:*` stream keys are reachable.
//! Keys in and out of this module are UNPREFIXED (`bus:tool:delivery`); the prefix is added here.
//! Callers pass the request's SCOPED bus (`st.bus.scoped(&ctx.scope())`), so `{prefix}` is the environment prefix
//! plus `s:{tenant}:{job}:` and another scope's keys can be neither listed nor addressed. A root bus here would
//! see only keys outside every scope, i.e. nothing a route should serve.
use carebus::{Bus, Envelope};
use redis::Value as R;
use redis::streams::{StreamAutoClaimReply, StreamRangeReply};
use serde::Serialize;
use serde_json::{Map, Value, json};

#[derive(Serialize)]
pub struct GroupInfo {
    pub name: String,
    pub consumers: u64,
    pub pending: u64,
    pub lag: Option<u64>,
    pub last_delivered_id: String,
}

#[derive(Serialize)]
pub struct StreamInfo {
    pub key: String,
    pub bus: String,
    pub area: Option<String>,
    pub length: u64,
    pub last_entry_id: Option<String>,
    pub groups: Vec<GroupInfo>,
}

/// `stream` is the unprefixed key (`bus:dead` for dead letters).
#[derive(Serialize)]
pub struct Entry {
    pub stream: String,
    pub entry_id: String,
    pub envelope: Envelope,
}

/// RESP2 flat arrays of (k, v, k, v) become objects; bulk strings become strings.
pub fn value_to_json(v: R) -> Value {
    match v {
        R::Nil => Value::Null,
        R::Int(i) => json!(i),
        R::BulkString(b) => Value::String(String::from_utf8_lossy(&b).into_owned()),
        R::SimpleString(s) => Value::String(s),
        R::Okay => Value::String("OK".into()),
        R::Array(items) | R::Set(items) => {
            let pairable = !items.is_empty()
                && items.len() % 2 == 0
                && items
                    .iter()
                    .step_by(2)
                    .all(|k| matches!(k, R::BulkString(_) | R::SimpleString(_)));
            if pairable {
                let mut m = Map::new();
                let mut it = items.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    if let Value::String(k) = value_to_json(k) {
                        m.insert(k, value_to_json(v));
                    }
                }
                Value::Object(m)
            } else {
                Value::Array(items.into_iter().map(value_to_json).collect())
            }
        }
        R::Map(pairs) => Value::Object(
            pairs
                .into_iter()
                .filter_map(|(k, v)| match value_to_json(k) {
                    Value::String(k) => Some((k, value_to_json(v))),
                    _ => None,
                })
                .collect(),
        ),
        R::Double(d) => json!(d),
        R::Boolean(b) => json!(b),
        other => Value::String(format!("{other:?}")),
    }
}

/// A stream key without the bus prefix, as every route reports it.
pub fn strip<'a>(bus: &Bus, key: &'a str) -> &'a str {
    key.strip_prefix(&bus.streams.key("")).unwrap_or(key)
}

/// `<ms>` or `<ms>-<seq>`, digits only: the only ids XRANGE/XDEL are ever given.
pub fn valid_stream_id(id: &str) -> bool {
    let mut parts = id.splitn(2, '-');
    let digits = |s: &str| !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit());
    parts.next().is_some_and(digits) && parts.next().is_none_or(digits)
}

/// Only `{prefix}bus:*` keys that exist as streams are served.
pub async fn owns(bus: &Bus, unprefixed: &str) -> redis::RedisResult<bool> {
    if !unprefixed.starts_with("bus:") {
        return Ok(false);
    }
    let t: String = redis::cmd("TYPE")
        .arg(bus.streams.key(unprefixed))
        .query_async(&mut bus.conn())
        .await?;
    Ok(t == "stream")
}

pub async fn list_streams(bus: &Bus) -> redis::RedisResult<Vec<String>> {
    let mut c = bus.conn();
    let pattern = bus.streams.key("bus:*");
    let (mut cursor, mut out) = (0u64, Vec::new());
    loop {
        let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("TYPE")
            .arg("stream")
            .arg("COUNT")
            .arg(500)
            .query_async(&mut c)
            .await?;
        out.extend(keys.into_iter().map(|k| strip(bus, &k).to_string()));
        if next == 0 {
            break;
        }
        cursor = next;
    }
    out.sort();
    out.dedup(); // SCAN may return a key more than once
    Ok(out)
}

fn bus_and_area(key: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["bus", "customer", area] => ("customer".into(), Some((*area).into())),
        ["bus", "worker", area, ..] => ("worker".into(), Some((*area).into())),
        ["bus", "tool", ..] => ("tool".into(), None),
        ["bus", "dead"] => ("dead".into(), None),
        _ => ("other".into(), None),
    }
}

pub async fn stream_info(bus: &Bus, key: &str) -> redis::RedisResult<StreamInfo> {
    let mut c = bus.conn();
    let full = bus.streams.key(key);
    let s = value_to_json(
        redis::cmd("XINFO")
            .arg("STREAM")
            .arg(&full)
            .query_async::<R>(&mut c)
            .await?,
    );
    let g = value_to_json(
        redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(&full)
            .query_async::<R>(&mut c)
            .await?,
    );
    let groups = g
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|g| GroupInfo {
            name: g["name"].as_str().unwrap_or_default().into(),
            consumers: g["consumers"].as_u64().unwrap_or(0),
            pending: g["pending"].as_u64().unwrap_or(0),
            lag: g["lag"].as_u64(),
            last_delivered_id: g["last-delivered-id"].as_str().unwrap_or_default().into(),
        })
        .collect();
    let (b, area) = bus_and_area(key);
    Ok(StreamInfo {
        key: key.into(),
        bus: b,
        area,
        length: s["length"].as_u64().unwrap_or(0),
        last_entry_id: s["last-generated-id"].as_str().map(String::from),
        groups,
    })
}

fn to_entries(key: &str, reply: StreamRangeReply) -> Vec<Entry> {
    reply
        .ids
        .into_iter()
        .filter_map(|id| {
            let raw: String = id.get("envelope")?;
            match serde_json::from_str(&raw) {
                Ok(envelope) => Some(Entry {
                    stream: key.into(),
                    entry_id: id.id,
                    envelope,
                }),
                Err(e) => {
                    tracing::warn!(stream = key, entry = %id.id, error = %e, "entry without a parseable envelope skipped");
                    None
                }
            }
        })
        .collect()
}

/// Newest first. `before` is an exclusive stream id. Returns the entries and the id of the last RAW
/// row read (entries with an unparseable envelope are skipped but still advance the cursor).
pub async fn entries_page(
    bus: &Bus,
    key: &str,
    count: usize,
    before: Option<&str>,
) -> redis::RedisResult<(Vec<Entry>, Option<String>)> {
    let end = before.map_or_else(|| "+".to_string(), |b| format!("({b}"));
    let reply: StreamRangeReply = redis::cmd("XREVRANGE")
        .arg(bus.streams.key(key))
        .arg(end)
        .arg("-")
        .arg("COUNT")
        .arg(count)
        .query_async(&mut bus.conn())
        .await?;
    let raw_len = reply.ids.len();
    let last_raw = reply.ids.last().map(|i| i.id.clone());
    let cursor = (raw_len == count).then_some(last_raw).flatten();
    Ok((to_entries(key, reply), cursor))
}

/// One entry by id (`XRANGE key id id`); `None` when absent or its envelope cannot be parsed.
pub async fn entry_by_id(bus: &Bus, key: &str, id: &str) -> redis::RedisResult<Option<Entry>> {
    let reply: StreamRangeReply = redis::cmd("XRANGE")
        .arg(bus.streams.key(key))
        .arg(id)
        .arg(id)
        .query_async(&mut bus.conn())
        .await?;
    Ok(to_entries(key, reply).into_iter().next())
}

/// Moves long-pending entries to consumer `carebus-api-admin`. Err with code NOGROUP when the group is missing.
pub async fn claim(
    bus: &Bus,
    key: &str,
    group: &str,
    min_idle_ms: u64,
) -> redis::RedisResult<usize> {
    let r: StreamAutoClaimReply = redis::cmd("XAUTOCLAIM")
        .arg(bus.streams.key(key))
        .arg(group)
        .arg("carebus-api-admin")
        .arg(min_idle_ms)
        .arg("0-0")
        .arg("COUNT")
        .arg(100)
        .query_async(&mut bus.conn())
        .await?;
    Ok(r.claimed.len())
}

/// XDEL; the count of entries actually removed (the replay/discard gate).
pub async fn delete(bus: &Bus, key: &str, id: &str) -> redis::RedisResult<u64> {
    redis::cmd("XDEL")
        .arg(bus.streams.key(key))
        .arg(id)
        .query_async(&mut bus.conn())
        .await
}
