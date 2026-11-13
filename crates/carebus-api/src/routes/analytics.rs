//! `GET /api/jobs/{job}/analytics?days=7|28`: one job's cases and tool latencies (RLS scopes every query).
use axum::{Json, Router, extract::State, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

pub fn routes() -> Router<AppState> {
    Router::new().route("/analytics", get(analytics))
}

#[derive(Deserialize)]
struct Days {
    days: Option<i32>,
}

async fn analytics(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiQuery(d): ApiQuery<Days>,
) -> ApiResult<Json<Value>> {
    let days = d.days.unwrap_or(7);
    if days != 7 && days != 28 {
        return Err(ApiError::BadRequest(
            "invalid_days",
            "days must be 7 or 28".into(),
        ));
    }
    let mut tx = ctx.begin(&s.db).await?;
    let per_day: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('day', to_char(g.d, 'YYYY-MM-DD'),
                'completed', count(c.*) FILTER (WHERE c.outcome='completed'),
                'not_eligible', count(c.*) FILTER (WHERE c.outcome='not_eligible'),
                'failed', count(c.*) FILTER (WHERE c.outcome='failed'),
                'open', count(c.*) FILTER (WHERE c.outcome IS NULL AND c.case_id IS NOT NULL))
         FROM generate_series(current_date - ($1 - 1), current_date, interval '1 day') AS g(d)
         LEFT JOIN case_history c ON c.started_at::date = g.d::date
         GROUP BY g.d ORDER BY g.d",
    )
    .bind(days)
    .fetch_all(&mut *tx)
    .await?;
    let by_type: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('request_type', request_type, 'cases', count(*),
                'completed', count(*) FILTER (WHERE outcome='completed'),
                'not_eligible', count(*) FILTER (WHERE outcome='not_eligible'),
                'failed', count(*) FILTER (WHERE outcome='failed'),
                'retries', COALESCE(sum(retries),0),
                'p50_ms', percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms))
         FROM case_history
         WHERE started_at > now() - make_interval(days => $1) AND outcome IS NOT NULL
         GROUP BY request_type ORDER BY count(*) DESC, request_type",
    )
    .bind(days)
    .fetch_all(&mut *tx)
    .await?;
    let latency: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('tool', resource_id,
                'p50_ms', percentile_cont(0.5) WITHIN GROUP (ORDER BY (details->>'latency_ms')::float8),
                'p95_ms', percentile_cont(0.95) WITHIN GROUP (ORDER BY (details->>'latency_ms')::float8))
         FROM audit_events
         WHERE action_type='execute' AND resource_type='tool' AND timestamp > now() - make_interval(days => $1)
         GROUP BY resource_id ORDER BY resource_id",
    )
    .bind(days)
    .fetch_all(&mut *tx)
    .await?;
    let previous: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('cases', count(*),
                'completed', count(*) FILTER (WHERE outcome='completed'),
                'not_eligible', count(*) FILTER (WHERE outcome='not_eligible'),
                'failed', count(*) FILTER (WHERE outcome='failed'))
         FROM case_history
         WHERE started_at BETWEEN now() - make_interval(days => $1 * 2) AND now() - make_interval(days => $1)",
    )
    .bind(days)
    .fetch_one(&mut *tx)
    .await?;
    Ok(Json(
        json!({"days": per_day, "by_type": by_type, "latency": latency, "previous": previous}),
    ))
}
