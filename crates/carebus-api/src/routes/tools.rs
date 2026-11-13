//! `GET /api/jobs/{job}/tools`: the MCP tool catalogue as the nonagenticai MCP server offers it to the caller's tenant
//! (listed with that tenant's own care-bus licence) plus the job's 24 h call stats from the audit rows the recorder
//! writes in that job's scope.
use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Value, json};

use crate::error::{ApiError, ApiResult};
use crate::page::Paged;
use crate::state::AppState;
use crate::tenancy::JobMember;

/// Relative path; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/tools", get(list))
}

type Stat = (String, i64, Option<f64>, Option<f64>);

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
) -> ApiResult<Json<Paged<Value>>> {
    let mcp = s.mcp.as_ref().ok_or(ApiError::Upstream("mcp"))?;
    let tools = mcp.list_tools(ctx.t.tenant).await.map_err(|e| {
        tracing::warn!(error = %format!("{e:#}"), "mcp tools/list failed");
        ApiError::Upstream("mcp")
    })?;
    // Opened after the MCP call: no transaction is held across an upstream request.
    let mut tx = ctx.begin(&s.db).await?;
    let stats: Vec<Stat> = sqlx::query_as(
        "SELECT resource_id, count(*),
                avg((status='failure')::int)::float8,
                percentile_cont(0.95) WITHIN GROUP (ORDER BY (details->>'latency_ms')::float8)
         FROM audit_events
         WHERE action_type='execute' AND resource_type='tool' AND timestamp > now() - interval '24 hours'
         GROUP BY resource_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    drop(tx);
    let items = tools
        .into_iter()
        .map(|t| {
            let st = stats.iter().find(|r| r.0 == t.name);
            json!({
                "name": t.name, "description": t.description, "input_schema": t.input_schema,
                "calls_24h": st.map_or(0, |r| r.1),
                "error_rate": st.and_then(|r| r.2),
                "p95_ms": st.and_then(|r| r.3),
            })
        })
        .collect();
    Ok(Json(Paged { items, next: None }))
}
