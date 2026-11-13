//! `GET /api/jobs/{job}/usage?from=<rfc3339>&to=<rfc3339>`: one job's model token usage from `usage_ledger`, for the
//! owner's visibility (never billing: license-authz is the billing meter). Any tenant member may read; RLS scopes
//! every query to the job. The window is `[from, to)`, by default the last 30 days.
use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

const DEFAULT_WINDOW_DAYS: i64 = 30;

pub fn routes() -> Router<AppState> {
    Router::new().route("/usage", get(usage))
}

#[derive(Deserialize)]
struct Window {
    from: Option<String>,
    to: Option<String>,
}

fn instant(v: Option<&str>, code: &'static str, name: &str) -> ApiResult<Option<DateTime<Utc>>> {
    match v.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|_| {
                ApiError::BadRequest(
                    code,
                    format!("{name} must be an RFC 3339 timestamp, e.g. 2026-10-01T00:00:00Z"),
                )
            }),
    }
}

async fn usage(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(w): ApiQuery<Window>,
) -> ApiResult<Json<Value>> {
    let from = instant(w.from.as_deref(), "invalid_from", "from")?;
    let to = instant(w.to.as_deref(), "invalid_to", "to")?.unwrap_or_else(Utc::now);
    let from = from.unwrap_or(to - Duration::days(DEFAULT_WINDOW_DAYS));
    if from > to {
        return Err(ApiError::BadRequest(
            "invalid_window",
            "from must not be after to".into(),
        ));
    }
    let mut tx = ctx.begin(&s.db).await?;
    let total: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('calls', COALESCE(sum(calls), 0)::bigint,
                'input_tokens', COALESCE(sum(input_tokens), 0)::bigint,
                'output_tokens', COALESCE(sum(output_tokens), 0)::bigint)
         FROM usage_ledger
         WHERE occurred_at >= $1 AND occurred_at < $2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(&mut *tx)
    .await?;
    let by_day: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('day', day, 'model', model, 'provider', provider, 'calls', calls,
                'input_tokens', input_tokens, 'output_tokens', output_tokens)
         FROM (SELECT to_char(occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS day, model, provider,
                      sum(calls)::bigint AS calls, sum(input_tokens)::bigint AS input_tokens,
                      sum(output_tokens)::bigint AS output_tokens
                 FROM usage_ledger
                WHERE occurred_at >= $1 AND occurred_at < $2
                GROUP BY 1, 2, 3) d
         ORDER BY day, model, provider",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let ts = |t: DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::Millis, true);
    Ok(Json(
        json!({"from": ts(from), "to": ts(to), "total": total, "by_day": by_day}),
    ))
}
