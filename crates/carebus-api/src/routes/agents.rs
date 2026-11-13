//! Agents: the fixed catalogue of bus participants, with live numbers aggregated from the consumer groups that
//! Redis reports on every stream of the job's scope (agents are shared processes, but each serves every scope with
//! its own consumer group per scoped stream).
use axum::{Json, Router, extract::State, routing::get};
use carebus::domain::TOOLS;
use carebus::{executor, interaction, orchestrator};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, ApiPath, ApiResult},
    page::Paged,
    redis_admin as ra,
    state::AppState,
    tenancy::{JobCtx, JobMember},
};

const LAG_WARN: u64 = 20;
const PENDING_WARN: u64 = 10;

#[derive(Serialize, Clone)]
pub struct AgentView {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub group: String,
    pub tool: Option<String>,
    pub consumers: u64,
    pub pending: u64,
    pub lag: u64,
    pub streams: Vec<String>,
    pub status: &'static str,
    pub model_routes: Vec<String>,
}

/// LLM uses each agent's work calls (`llm_provider_uses`); only uses the tenant has bound to a provider are
/// reported. The interaction front's turn runs in carebus-api: `qualify` and the request `gates`.
fn catalogue() -> Vec<(AgentView, &'static [&'static str])> {
    let mk = |id: String,
              name: String,
              kind: &'static str,
              group: String,
              tool: Option<String>,
              uses: &'static [&'static str]| {
        (
            AgentView {
                id,
                name,
                kind,
                group,
                tool,
                consumers: 0,
                pending: 0,
                lag: 0,
                streams: vec![],
                status: "idle",
                model_routes: vec![],
            },
            uses,
        )
    };
    let mut v = vec![
        mk(
            "interaction".into(),
            "Interaction agent".into(),
            "front",
            interaction::GROUP.into(),
            None,
            &["qualify", "gates"],
        ),
        mk(
            "orchestrator".into(),
            "Orchestrator agent".into(),
            "coordination",
            orchestrator::GROUP.into(),
            None,
            &[],
        ),
    ];
    v.extend(TOOLS.iter().map(|t| {
        mk(
            format!("worker-{t}"),
            format!("{t} worker"),
            "worker",
            format!("worker-{t}"),
            Some((*t).to_string()),
            &[],
        )
    }));
    v.push(mk(
        "tool-executor".into(),
        "Tool executor (MCP bridge)".into(),
        "bridge",
        executor::GROUP.into(),
        None,
        &[],
    ));
    v
}

fn status(a: &AgentView) -> &'static str {
    if a.consumers == 0 {
        "idle"
    } else if a.lag > LAG_WARN || a.pending > PENDING_WARN {
        "warn"
    } else {
        "ok"
    }
}

async fn collect(s: &AppState, ctx: &JobCtx) -> ApiResult<Vec<AgentView>> {
    let configured: Vec<String> = {
        let mut tx = ctx.begin(&s.db).await?;
        sqlx::query_scalar("SELECT \"use\" FROM llm_provider_uses WHERE provider_id IS NOT NULL")
            .fetch_all(&mut *tx)
            .await?
    };
    let bus = s.bus.scoped(&ctx.scope());
    let mut agents = catalogue();
    for key in ra::list_streams(&bus).await? {
        let info = ra::stream_info(&bus, &key).await?;
        for g in info.groups {
            if let Some((a, _)) = agents.iter_mut().find(|(a, _)| a.group == g.name) {
                // One process joins the same group on several streams: max, not sum.
                a.consumers = a.consumers.max(g.consumers);
                a.pending += g.pending;
                a.lag = a.lag.max(g.lag.unwrap_or(0));
                a.streams.push(key.clone());
            }
        }
    }
    Ok(agents
        .into_iter()
        .map(|(mut a, uses)| {
            a.status = status(&a);
            a.model_routes = uses
                .iter()
                .filter(|u| configured.iter().any(|c| c == *u))
                .map(|u| (*u).to_string())
                .collect();
            a
        })
        .collect())
}

/// Relative paths; nested under `/api/jobs/{job}` by `routes::all`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list))
        .route("/agents/{id}", get(one))
}

#[derive(Deserialize)]
struct IdPath {
    id: String,
}

async fn list(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
) -> ApiResult<Json<Paged<AgentView>>> {
    Ok(Json(Paged {
        items: collect(&s, &ctx).await?,
        next: None,
    }))
}

async fn one(
    JobMember(ctx): JobMember,
    State(s): State<AppState>,
    ApiPath(IdPath { id }): ApiPath<IdPath>,
) -> ApiResult<Json<AgentView>> {
    collect(&s, &ctx)
        .await?
        .into_iter()
        .find(|a| a.id == id)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("agent {id}")))
}
