//! Streams and dead letters of one job's bus (`/api/jobs/{job}/streams`, `/api/jobs/{job}/dead-letters`). Every
//! key is read through the request's scoped bus, so another scope's streams are unreachable by construction.
use axum::{
    Json, Router,
    extract::State,
    routing::{delete, get, post},
};
use carebus::Bus;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    audit,
    error::{ApiError, ApiPath, ApiQuery, ApiResult},
    page::{Page, Paged},
    redis_admin as ra,
    state::AppState,
    tenancy::{JobAdmin, JobCtx, JobMember},
};

const DEAD: &str = "bus:dead";

/// Relative paths; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/streams", get(list))
        .route("/streams/{key}", get(one))
        .route("/streams/{key}/entries", get(entries))
        .route("/streams/{key}/groups/{group}/claim", post(claim))
        .route("/dead-letters", get(dead_list))
        .route("/dead-letters/{entry_id}/replay", post(dead_replay))
        .route("/dead-letters/{entry_id}", delete(dead_delete))
}

#[derive(Deserialize)]
struct KeyPath {
    key: String,
}

#[derive(Deserialize)]
struct GroupPath {
    key: String,
    group: String,
}

#[derive(Deserialize)]
struct EntryPath {
    entry_id: String,
}

/// One audit row in the job's scope.
async fn audited(
    s: &AppState,
    ctx: &JobCtx,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    details: Value,
) -> ApiResult<()> {
    let mut tx = ctx.begin(&s.db).await?;
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        action,
        resource_type,
        resource_id,
        true,
        details,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn checked(bus: &Bus, key: &str) -> ApiResult<()> {
    if ra::owns(bus, key).await? {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("stream {key}")))
    }
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
) -> ApiResult<Json<Paged<ra::StreamInfo>>> {
    let bus = s.bus.scoped(&ctx.scope());
    let mut items = Vec::new();
    for k in ra::list_streams(&bus).await? {
        items.push(ra::stream_info(&bus, &k).await?);
    }
    Ok(Json(Paged { items, next: None }))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(p): ApiPath<KeyPath>,
) -> ApiResult<Json<ra::StreamInfo>> {
    let bus = s.bus.scoped(&ctx.scope());
    checked(&bus, &p.key).await?;
    Ok(Json(ra::stream_info(&bus, &p.key).await?))
}

#[derive(Deserialize)]
struct EntriesQuery {
    count: Option<i64>,
    limit: Option<i64>,
    before: Option<String>,
}

/// `before` is a stream id (exclusive); anything else is 400 `invalid_cursor`.
fn stream_cursor(page: &Page) -> ApiResult<Option<&str>> {
    match page.before.as_deref() {
        Some(b) if !ra::valid_stream_id(b) => Err(ApiError::BadRequest(
            "invalid_cursor",
            "The before cursor is not valid.".into(),
        )),
        b => Ok(b),
    }
}

async fn entries(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(p): ApiPath<KeyPath>,
    ApiQuery(q): ApiQuery<EntriesQuery>,
) -> ApiResult<Json<Paged<ra::Entry>>> {
    let bus = s.bus.scoped(&ctx.scope());
    checked(&bus, &p.key).await?;
    let page = Page::new(q.limit.or(q.count), q.before);
    let before = stream_cursor(&page)?;
    let (items, next) = ra::entries_page(&bus, &p.key, page.limit() as usize, before).await?;
    Ok(Json(Paged { items, next }))
}

async fn claim(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(GroupPath { key, group }): ApiPath<GroupPath>,
) -> ApiResult<Json<Value>> {
    let bus = s.bus.scoped(&ctx.scope());
    checked(&bus, &key).await?;
    let n = match ra::claim(&bus, &key, &group, 30_000).await {
        Ok(n) => n,
        Err(e) if e.code() == Some("NOGROUP") => {
            return Err(ApiError::NotFound(format!("group {group} on {key}")));
        }
        Err(e) => return Err(e.into()),
    };
    audited(
        &s,
        &ctx,
        "claim",
        "stream",
        &format!("{key}#{group}"),
        json!({"claimed": n}),
    )
    .await?;
    Ok(Json(json!({"claimed": n})))
}

#[derive(Deserialize)]
struct DeadQuery {
    limit: Option<i64>,
    before: Option<String>,
}

async fn dead_list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<DeadQuery>,
) -> ApiResult<Json<Paged<ra::Entry>>> {
    let bus = s.bus.scoped(&ctx.scope());
    let page = Page::new(q.limit, q.before);
    let before = stream_cursor(&page)?;
    // A Redis failure is 503, never an empty list: unavailable is not zero.
    let (items, next) = ra::entries_page(&bus, DEAD, page.limit() as usize, before)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "dead letters unreadable");
            ApiError::Upstream("redis")
        })?;
    Ok(Json(Paged { items, next }))
}

async fn find_dead(bus: &Bus, id: &str) -> ApiResult<ra::Entry> {
    let not_found = || ApiError::NotFound(format!("dead letter {id}"));
    if !ra::valid_stream_id(id) {
        return Err(not_found());
    }
    ra::entry_by_id(bus, DEAD, id).await?.ok_or_else(not_found)
}

async fn dead_replay(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(EntryPath { entry_id: id }): ApiPath<EntryPath>,
) -> ApiResult<Json<Value>> {
    let bus = s.bus.scoped(&ctx.scope());
    let dl = find_dead(&bus, &id).await?;
    // bus.rs dead_letter stores the FULL prefixed key in `stream` and the envelope under `original`.
    let bad = |m: String| ApiError::BadRequest("bad_dead_letter", m);
    let origin = dl.envelope.payload["stream"]
        .as_str()
        .ok_or_else(|| bad("dead letter has no origin stream".into()))?
        .to_string();
    // Only a stream of THIS scope: the scoped prefix is part of the check.
    if !origin.starts_with(&bus.streams.key("bus:")) || origin == bus.streams.dead() {
        return Err(bad(format!(
            "origin {} is not a replayable care-bus stream",
            ra::strip(&bus, &origin)
        )));
    }
    let inner: carebus::Envelope = serde_json::from_value(dl.envelope.payload["original"].clone())
        .map_err(|_| {
            bad(
                "dead letter has no original envelope (it could not be parsed when it failed)"
                    .into(),
            )
        })?;
    // Delete FIRST: XDEL is the claim. Of two concurrent replays only the one that removes the entry
    // publishes; the other sees 0 and gets 404.
    if ra::delete(&bus, DEAD, &id).await? != 1 {
        return Err(ApiError::NotFound(format!("dead letter {id}")));
    }
    // The scoped publish re-stamps the envelope with this scope.
    let new_id = match bus.publish(&origin, &inner).await {
        Ok(id) => id,
        Err(e) => {
            // Put the dead letter back (new id) so a failed publish does not lose it.
            tracing::error!(error = %format!("{e:#}"), dead_letter = %id, "replay publish failed; restoring dead letter");
            if let Err(re) = bus.publish(&bus.streams.dead(), &dl.envelope).await {
                tracing::error!(error = %format!("{re:#}"), dead_letter = %id, "could not restore dead letter");
            }
            return Err(ApiError::Upstream("redis"));
        }
    };
    let stream = ra::strip(&bus, &origin).to_string();
    audited(
        &s,
        &ctx,
        "replay",
        "dead_letter",
        &id,
        json!({"stream": stream, "new_entry_id": new_id}),
    )
    .await?;
    Ok(Json(json!({"stream": stream, "entry_id": new_id})))
}

async fn dead_delete(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(EntryPath { entry_id: id }): ApiPath<EntryPath>,
) -> ApiResult<Json<Value>> {
    let bus = s.bus.scoped(&ctx.scope());
    if !ra::valid_stream_id(&id) || ra::delete(&bus, DEAD, &id).await? != 1 {
        return Err(ApiError::NotFound(format!("dead letter {id}")));
    }
    audited(&s, &ctx, "discard", "dead_letter", &id, json!({})).await?;
    Ok(Json(json!({"deleted": id})))
}
