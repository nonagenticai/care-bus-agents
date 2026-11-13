//! Procedures: versioned plans with PDDL steps, and the reviewed DRAFT -> CANARY -> PUBLISHED lifecycle.
//!
//! Reads serve the effective versions (`eff_procedures`): the base versions of the current release plus the
//! tenant's own. A version is an immutable record, so a tenant never overrides a base version: it adds a version
//! (an extension) and runs it through the lifecycle; transitions and approvals of a base version are 409
//! `base_row_read_only`. A base PUBLISHED version is `shadowed` when a tenant PUBLISHED version exists for its name
//! or context space, and only non-shadowed PUBLISHED versions are served by name and to the orchestrator.
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::{
    audit,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    page::Paged,
    refine::{self, Layer, ObjectKind, RowKind},
    secrets,
    state::AppState,
    tenancy::{JobAdmin, ProcedureReader},
};

/// `{name}` under the nested `/api/jobs/{job}` (serde ignores the `job` key).
#[derive(Deserialize)]
struct NamePath {
    name: String,
}

#[derive(Deserialize)]
struct VersionPath {
    name: String,
    version: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/procedures", get(list).post(create))
        .route("/procedures/{name}", get(published))
        .route("/procedures/{name}/versions/{version}", get(one))
        .route(
            "/procedures/{name}/versions/{version}/status",
            post(transition),
        )
        .route(
            "/procedures/{name}/versions/{version}/approve",
            post(approve),
        )
}

const STATUSES: [&str; 5] = ["DRAFT", "CANARY", "PUBLISHED", "ARCHIVED", "DISCARDED"];

#[derive(Deserialize)]
struct ListQ {
    status: Option<String>,
    context_space: Option<String>,
    layer: Option<Layer>,
}

async fn list(
    ProcedureReader(ctx): ProcedureReader,
    State(s): State<AppState>,
    ApiQuery(q): ApiQuery<ListQ>,
) -> ApiResult<Json<Paged<Value>>> {
    if let Some(st) = &q.status
        && !STATUSES.contains(&st.as_str())
    {
        return Err(ApiError::BadRequest(
            "invalid_status",
            format!("status must be one of {}", STATUSES.join(", ")),
        ));
    }
    let layer = q.layer.unwrap_or_default();
    let mut tx = ctx.begin(&s.db).await?;
    let items: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        // `shadowed`: a base PUBLISHED version a tenant PUBLISHED version replaces. A status=PUBLISHED list (the
        // orchestrator's lookup by context space) serves only the versions that run, i.e. not shadowed.
        //
        // `steps` counts the steps of THIS version's plan. procedure_steps (the PDDL annotation rows) is optional per
        // version -- the seeded return@1.4.0-beta canary has none -- so counting those rows reported 0 for a 5-step plan.
        //
        // `runs_7d` / `success_rate` come from case_history rows the recorder attributed to THIS version (the
        // `name@version` the orchestrator stamps on `request.resolved`), started in the last 7 days:
        //   * a run is a RESOLVED case of the version: open cases have no outcome yet (and no attribution), and
        //     rows recorded before attribution existed (procedure columns NULL) count for no version;
        //   * success = the procedure reached a decision: outcome `completed` OR `not_eligible`. A not-eligible
        //     case is the eligibility gate answering "no" -- the procedure working as designed, which is why the
        //     cases page shows it as a warning, not an error -- so it is a run but not a failure;
        //   * failure = outcome `failed` (a tool failed or the case deadline passed);
        //   * success_rate = (completed + not_eligible) / runs_7d, null when runs_7d = 0 (no data is not 0 %).
        "SELECT jsonb_build_object('name',p.name,'version',p.version,'version_status',p.version_status::text,'context_space',p.context_space,
                'domain',p.domain,'origin',p.origin,'overridden',p.overridden,'shadowed',p.shadowed,'hidden',p.hidden,
                'steps',(SELECT count(*) FROM jsonb_array_elements(COALESCE(p.plan->'phases','[]'::jsonb)) ph,
                                              jsonb_array_elements(COALESCE(ph->'steps','[]'::jsonb)) st),
                'reviewers',p.reviewers,'updated_at',p.updated_at,'plan',p.plan,
                'runs_7d',r.runs,'success_rate',r.ok::float8 / NULLIF(r.runs,0),
                'side_effect_steps',COALESCE((SELECT jsonb_agg(jsonb_build_object('phase',st.phase,'mcp_tool',st.mcp_tool,
                                                                                  'side_effect',st.side_effect)
                                                               ORDER BY st.step_number)
                                                FROM procedure_steps st
                                               WHERE st.procedure_id = p.id AND st.origin = p.origin
                                                 AND st.side_effect IS NOT NULL), '[]'::jsonb))
         FROM {src} p
         CROSS JOIN LATERAL (
             SELECT count(*) AS runs, count(*) FILTER (WHERE c.outcome IN ('completed','not_eligible')) AS ok
               FROM case_history c
              WHERE c.procedure_name = p.name AND c.procedure_version = p.version
                AND c.started_at >= now() - interval '7 days' AND c.outcome IS NOT NULL) r
         WHERE ($1::text IS NULL OR p.version_status::text = $1) AND ($2::text IS NULL OR p.context_space = $2)
           AND NOT ($1::text IS NOT DISTINCT FROM 'PUBLISHED' AND p.shadowed)
         ORDER BY p.name, p.version_sort DESC, p.id DESC",
        src = refine::layer_source(ObjectKind::Procedure, layer),
    )))
    .bind(q.status)
    .bind(q.context_space)
    .fetch_all(&mut *tx)
    .await?;
    let items = items.into_iter().map(with_step_side_effects).collect();
    Ok(Json(Paged { items, next: None }))
}

/// Moves a list item's `side_effect_steps` (its step rows that set `side_effect`) onto its `plan`: the plan step in
/// the row's phase whose MCP tool is the row's `mcp_tool` gets `side_effect`, the k-th such row the k-th such step.
/// The orchestrator plans from this `plan` (`HttpProcedures`), so a flag set in a release bundle or on
/// `POST /api/procedures` reaches the work request. A plan step without a flagged row is left as stored (no key),
/// so the list keeps its shape for every procedure that sets no flag.
fn with_step_side_effects(mut item: Value) -> Value {
    let rows = item
        .as_object_mut()
        .and_then(|o| o.remove("side_effect_steps"))
        .and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        })
        .unwrap_or_default();
    if let Some(plan) = item.get_mut("plan") {
        overlay_side_effects(plan, &rows);
    }
    item
}

fn overlay_side_effects(plan: &mut Value, rows: &[Value]) {
    let Some(phases) = plan.get_mut("phases").and_then(Value::as_array_mut) else {
        return;
    };
    let mut used: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for row in rows {
        let (Some(phase), Some(tool), Some(flag)) = (
            row.get("phase")
                .and_then(Value::as_u64)
                .and_then(|p| usize::try_from(p).ok()),
            row.get("mcp_tool").and_then(Value::as_str),
            row.get("side_effect").and_then(Value::as_bool),
        ) else {
            continue;
        };
        let Some(steps) = phases
            .get_mut(phase)
            .and_then(|ph| ph.get_mut("steps"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let tool_of = |st: &Value| {
            carebus::mcp::mcp_tool_name(
                st.get("worker").and_then(Value::as_str).unwrap_or_default(),
                st.get("action").and_then(Value::as_str).unwrap_or_default(),
            )
        };
        let hit =
            (0..steps.len()).find(|&j| !used.contains(&(phase, j)) && tool_of(&steps[j]) == tool);
        if let Some(j) = hit
            && let Some(st) = steps[j].as_object_mut()
        {
            used.insert((phase, j));
            st.insert("side_effect".into(), Value::Bool(flag));
        }
    }
}

#[derive(Deserialize)]
struct StatusQ {
    status: Option<String>,
}

/// Current PUBLISHED version by name (plan 03 `procedures_get_procedure`).
async fn published(
    ProcedureReader(ctx): ProcedureReader,
    State(s): State<AppState>,
    ApiPath(NamePath { name }): ApiPath<NamePath>,
    ApiQuery(q): ApiQuery<StatusQ>,
) -> ApiResult<Json<Value>> {
    if q.status.as_deref().is_some_and(|st| st != "PUBLISHED") {
        return Err(ApiError::BadRequest(
            "unsupported_status",
            "only status=PUBLISHED can be looked up by name; use /versions/{version} for others"
                .into(),
        ));
    }
    let mut tx = ctx.begin(&s.db).await?;
    let version: String = sqlx::query_scalar(
        "SELECT version FROM eff_procedures WHERE name=$1 AND version_status='PUBLISHED' AND NOT shadowed
          ORDER BY (origin = 'tenant') DESC LIMIT 1",
    )
    .bind(&name)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("published procedure {name}")))?;
    Ok(Json(load(&mut tx, &name, &version).await?))
}

/// One version's detail. `history` is this version's audit trail (the `procedure` rows `create`, `approve` and
/// `transition` write, resource `name@version`), oldest first: who did what and when, including the archive a later
/// publish or roll-back caused (`replaced_by`). Refused requests write nothing. Versions seeded by migration have no
/// rows, so their history is empty.
/// The scope and overlay bookkeeping columns are dropped from the row; `origin`, `overridden` and `shadowed` stay.
async fn load(conn: &mut PgConnection, name: &str, version: &str) -> ApiResult<Value> {
    sqlx::query_scalar::<_, Value>(
        "SELECT ((to_jsonb(p) - 'version_status' - 'version_sort' - 'tenant_id' - 'job_id' - 'release_id' - 'content_hash'
                  - 'base_hash' - 'hidden' - 'fk_origin') || jsonb_build_object('version_status', p.version_status::text,
                'steps', COALESCE((SELECT jsonb_agg(to_jsonb(st) - 'procedure_id' - 'tenant_id' - 'job_id' - 'origin' - 'release_id'
                                                    - 'content_hash' - 'base_hash' - 'hidden' - 'side_effect'
                                                    || CASE WHEN st.side_effect IS NULL THEN '{}'::jsonb
                                                            ELSE jsonb_build_object('side_effect', st.side_effect) END
                                                    ORDER BY st.step_number)
                                   FROM procedure_steps st WHERE st.procedure_id = p.id), '[]'::jsonb),
                'history', COALESCE((SELECT jsonb_agg(jsonb_build_object('at', a.timestamp, 'actor', a.actor_id,
                                       'action', a.action_type, 'ok', a.status = 'success',
                                       'from', a.details->>'from', 'to', a.details->>'to',
                                       'reason', a.details->>'reason', 'replaced_by', a.details->>'replaced_by')
                                     ORDER BY a.id)
                                   FROM audit_events a
                                   WHERE a.resource_type = 'procedure' AND a.resource_id = p.name || '@' || p.version), '[]'::jsonb)))
         FROM eff_procedures p WHERE p.name=$1 AND p.version=$2",
    )
    .bind(name)
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("procedure {name}@{version}")))
}

async fn one(
    ProcedureReader(ctx): ProcedureReader,
    State(s): State<AppState>,
    ApiPath(VersionPath { name, version }): ApiPath<VersionPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = ctx.begin(&s.db).await?;
    Ok(Json(load(&mut tx, &name, &version).await?))
}

#[derive(Deserialize)]
struct StepInput {
    step_number: i32,
    phase: i32,
    name: String,
    action_type: String,
    worker: String,
    mcp_tool: String,
    performed_by: String,
    #[serde(default)]
    preconditions: Vec<String>,
    #[serde(default)]
    postconditions: Vec<String>,
    #[serde(default)]
    parameters: Option<Map<String, Value>>,
    #[serde(default)]
    gate: bool,
    /// Absent: the agents' `carebus::mcp::SIDE_EFFECT_TOOLS` decides; set, it decides for this step.
    #[serde(default)]
    side_effect: Option<bool>,
}

#[derive(Deserialize)]
struct NewProcedure {
    name: String,
    version: String,
    context_space: String,
    description: Option<String>,
    pddl_domain: Option<String>,
    plan: Value,
    steps: Vec<StepInput>,
}

/// `^\d{1,9}\.\d{1,9}\.\d{1,9}(-[A-Za-z0-9.]+)?$`. Up to 9 digits so the generated `version_sort` fits an int4.
fn valid_version(v: &str) -> bool {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| (1..=9).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(|p| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        })
}

/// The stored plan must deserialize as a `Plan` and be runnable by the bus.
fn check_plan(plan: &Value) -> ApiResult<carebus::domain::Plan> {
    let parsed = serde_json::from_value::<carebus::domain::Plan>(plan.clone()).map_err(|_| {
        ApiError::BadRequest(
            "invalid_plan",
            "plan does not match the Plan schema ({phases:[{steps:[{worker,action,as,args}],gate}]}).".into(),
        )
    })?;
    carebus::procedures::validate_plan(&parsed)
        .map_err(|e| ApiError::BadRequest("invalid_plan", format!("plan is not runnable: {e}")))?;
    Ok(parsed)
}

/// The PDDL step rows describe the plan, one row per plan step: ordered by `step_number`, row i sits in the same
/// phase and names the same worker as the i-th plan step (phases in order, steps in order). Without this a version
/// could be stored with a runnable plan and no (or different) step rows, and its detail would show no steps.
fn check_steps_mirror_plan(plan: &carebus::domain::Plan, steps: &[StepInput]) -> ApiResult<()> {
    let expected: Vec<(i32, &str)> = plan
        .phases
        .iter()
        .enumerate()
        .flat_map(|(i, ph)| {
            let phase = i32::try_from(i).unwrap_or(i32::MAX);
            ph.steps.iter().map(move |st| (phase, st.worker.as_str()))
        })
        .collect();
    let mut ordered: Vec<&StepInput> = steps.iter().collect();
    ordered.sort_by_key(|st| st.step_number);
    let got: Vec<(i32, &str)> = ordered
        .iter()
        .map(|st| (st.phase, st.worker.as_str()))
        .collect();
    if got != expected {
        return Err(ApiError::BadRequest(
            "steps_do_not_match_plan",
            format!(
                "steps must describe the plan one-to-one: the plan has {} step(s); ordered by step_number, each step needs the phase and worker of the matching plan step.",
                expected.len()
            ),
        ));
    }
    Ok(())
}

/// A step that sets `side_effect` must call the tool of a plan step in its phase, or the flag would apply to nothing
/// (the procedure list matches flags to plan steps by phase and MCP tool).
fn check_side_effect_steps(plan: &carebus::domain::Plan, steps: &[StepInput]) -> ApiResult<()> {
    for st in steps.iter().filter(|st| st.side_effect.is_some()) {
        let ok = usize::try_from(st.phase)
            .is_ok_and(|ph| carebus::mcp::plan_has_tool(plan, ph, &st.mcp_tool));
        if !ok {
            return Err(ApiError::BadRequest(
                "invalid_steps",
                format!(
                    "step {} sets side_effect but phase {} of the plan has no step calling {}.",
                    st.step_number, st.phase, st.mcp_tool
                ),
            ));
        }
    }
    Ok(())
}

async fn create(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    // Untyped first: a secret anywhere (description, plan args, step parameters) is refused before
    // parsing, and a type error gets a fixed message instead of serde's, which echoes the value.
    secrets::reject_secret_fields(&raw, &[])?;
    let p: NewProcedure = secrets::parse_body(raw, "procedure")?;
    if p.name.trim().is_empty() || p.context_space.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "invalid_body",
            "name and context_space are required.".into(),
        ));
    }
    if !valid_version(&p.version) {
        return Err(ApiError::BadRequest(
            "invalid_version",
            "version must look like 1.4.0 or 1.4.0-beta (numbers up to 9 digits).".into(),
        ));
    }
    let plan = check_plan(&p.plan)?;
    let mut numbers: Vec<i32> = p.steps.iter().map(|st| st.step_number).collect();
    numbers.sort_unstable();
    numbers.dedup();
    if numbers.len() != p.steps.len() {
        return Err(ApiError::BadRequest(
            "invalid_steps",
            "step_number must be unique within a procedure.".into(),
        ));
    }
    check_steps_mirror_plan(&plan, &p.steps)?;
    check_side_effect_steps(&plan, &p.steps)?;
    let tools: Vec<String> = p.steps.iter().map(|s| s.mcp_tool.clone()).collect();
    let success: Vec<String> = p
        .steps
        .iter()
        .max_by_key(|s| s.step_number)
        .map(|s| s.postconditions.clone())
        .unwrap_or_default();
    let mut tx = ctx.begin(&s.db).await?;
    refine::lock_job_shared(&mut tx, ctx.job_id).await?;
    // a tenant version references an effective context space (the composite FK is enforced for base rows only)
    let space_ok: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM eff_context_spaces WHERE key = $1)")
            .bind(&p.context_space)
            .fetch_one(&mut *tx)
            .await?;
    if !space_ok {
        return Err(ApiError::BadRequest(
            "unknown_context_space",
            format!("context space {} does not exist", p.context_space),
        ));
    }
    if target_origin(&mut tx, &p.name, &p.version).await?.is_some() {
        return Err(ApiError::Conflict(
            "version_exists",
            format!("{}@{} already exists", p.name, p.version),
        ));
    }
    let inserted: Result<i64, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO procedures (name, version, context_space, description, pddl_domain, plan, tool_requirements, success_criteria)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id",
    )
    .bind(&p.name)
    .bind(&p.version)
    .bind(&p.context_space)
    .bind(&p.description)
    .bind(&p.pddl_domain)
    .bind(&p.plan)
    .bind(&tools)
    .bind(&success)
    .fetch_one(&mut *tx)
    .await;
    let id = match inserted {
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
            return Err(ApiError::Conflict(
                "version_exists",
                format!("{}@{} already exists", p.name, p.version),
            ));
        }
        Err(sqlx::Error::Database(d)) if d.is_foreign_key_violation() => {
            return Err(ApiError::BadRequest(
                "unknown_context_space",
                format!("context space {} does not exist", p.context_space),
            ));
        }
        r => r?,
    };
    for st in &p.steps {
        sqlx::query(
            "INSERT INTO procedure_steps (procedure_id, step_number, phase, name, action_type, worker, mcp_tool, performed_by, preconditions, postconditions, parameters, gate, side_effect)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .bind(id)
        .bind(st.step_number)
        .bind(st.phase)
        .bind(&st.name)
        .bind(&st.action_type)
        .bind(&st.worker)
        .bind(&st.mcp_tool)
        .bind(&st.performed_by)
        .bind(&st.preconditions)
        .bind(&st.postconditions)
        .bind(Value::Object(st.parameters.clone().unwrap_or_default()))
        .bind(st.gate)
        .bind(st.side_effect)
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "create",
        "procedure",
        &format!("{}@{}", p.name, p.version),
        true,
        json!({"context_space": p.context_space, "steps": p.steps.len(), "refinement": RowKind::Extension.as_str()}),
    )
    .await?;
    let row = load(&mut tx, &p.name, &p.version).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// The origin of `name@version` among the tenant's versions and the current base (tenant first; tombstones skipped).
async fn target_origin(
    tx: &mut PgConnection,
    name: &str,
    version: &str,
) -> ApiResult<Option<String>> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT p.origin FROM procedures p
          WHERE p.name = $1 AND p.version = $2 AND NOT p.hidden AND (p.origin = 'tenant' OR p.release_id = {})
          ORDER BY (p.origin = 'tenant') DESC LIMIT 1",
        refine::current_release("p")
    )))
    .bind(name)
    .bind(version)
    .fetch_optional(&mut *tx)
    .await?)
}

/// A lifecycle write needs a tenant version: 404 when the version does not exist, 409 `base_row_read_only` for a
/// base version. Takes the job share lock first (a release must not swap the base under the check).
async fn tenant_target(
    tx: &mut PgConnection,
    job_id: uuid::Uuid,
    name: &str,
    version: &str,
) -> ApiResult<()> {
    refine::lock_job_shared(tx, job_id).await?;
    match target_origin(tx, name, version).await?.as_deref() {
        Some("tenant") => Ok(()),
        Some(_) => Err(refine::base_row_read_only()),
        None => Err(ApiError::NotFound(format!("procedure {name}@{version}"))),
    }
}

#[derive(Deserialize)]
struct To {
    to: String,
}

/// ARCHIVED -> PUBLISHED is the roll-back: an archived version was PUBLISHED before, and re-publishing it takes
/// the same path as any publish (runnable plan, two distinct reviewers, the current PUBLISHED of the name is
/// archived in the same transaction, one PUBLISHED per context space). Reviewers are added by `approve`, never by a
/// transition: a publish with fewer than two distinct reviewers is refused and changes nothing.
fn allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("DRAFT", "CANARY")
            | ("DRAFT", "DISCARDED")
            | ("CANARY", "PUBLISHED")
            | ("CANARY", "DISCARDED")
            | ("PUBLISHED", "ARCHIVED")
            | ("ARCHIVED", "PUBLISHED")
    )
}

async fn transition(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(VersionPath { name, version }): ApiPath<VersionPath>,
    ApiJson(t): ApiJson<To>,
) -> ApiResult<Json<Value>> {
    if !STATUSES.contains(&t.to.as_str()) {
        return Err(ApiError::BadRequest(
            "invalid_status",
            format!("to must be one of {}", STATUSES.join(", ")),
        ));
    }
    let resource = format!("{name}@{version}");
    let mut tx = ctx.begin(&s.db).await?;
    tenant_target(&mut tx, ctx.job_id, &name, &version).await?;
    // Lock every tenant version of this name in id order, so two publishes of one name serialize
    // (no deadlock, and the one-PUBLISHED unique index is never the thing that reports it).
    let rows: Vec<(i64, String, String, Vec<String>, Value)> = sqlx::query_as(
        "SELECT id, version, version_status::text, reviewers, plan FROM procedures
          WHERE name=$1 AND origin='tenant' AND NOT hidden ORDER BY id FOR UPDATE",
    )
    .bind(&name)
    .fetch_all(&mut *tx)
    .await?;
    let (id, _, from, mut reviewers, plan) = rows
        .into_iter()
        .find(|r| r.1 == version)
        .ok_or_else(|| ApiError::NotFound(format!("procedure {resource}")))?;
    if !allowed(&from, &t.to) {
        return Err(ApiError::Conflict(
            "invalid_transition",
            format!("{from} cannot become {}", t.to),
        ));
    }
    if matches!(t.to.as_str(), "CANARY" | "PUBLISHED") {
        check_plan(&plan)?;
    }
    let mut archived: Vec<String> = Vec::new();
    reviewers.sort();
    reviewers.dedup();
    if t.to == "PUBLISHED" {
        if reviewers.len() < 2 {
            // Refused like every other invalid transition: nothing is written (the transaction rolls back on drop).
            return Err(ApiError::Conflict(
                "needs_two_reviewers",
                format!(
                    "{} reviewer(s) so far; publishing needs two distinct reviewers",
                    reviewers.len()
                ),
            ));
        }
        archived = sqlx::query_scalar(
            "UPDATE procedures SET version_status='ARCHIVED', updated_at=now()
              WHERE name=$1 AND origin='tenant' AND version_status='PUBLISHED' RETURNING version",
        )
        .bind(&name)
        .fetch_all(&mut *tx)
        .await?;
    }
    let moved = sqlx::query(
        "UPDATE procedures SET version_status=$2::version_status, updated_at=now() WHERE id=$1",
    )
    .bind(id)
    .bind(&t.to)
    .execute(&mut *tx)
    .await;
    match moved {
        // Another procedure name already serves this context space (R26): HttpProcedures reads one plan.
        Err(sqlx::Error::Database(d))
            if d.is_unique_violation()
                && d.constraint() == Some("procedures_one_published_per_space") =>
        {
            return Err(ApiError::Conflict(
                "space_already_published",
                "another procedure is already PUBLISHED in this context space; archive it first"
                    .into(),
            ));
        }
        r => r?,
    };
    // The version a publish displaced gets its own row, so its history says who archived it and why.
    for old in &archived {
        audit::record(
            &mut tx,
            &ctx.t.caller.actor,
            ctx.t.caller.actor_type(),
            "transition",
            "procedure",
            &format!("{name}@{old}"),
            true,
            json!({"from": "PUBLISHED", "to": "ARCHIVED", "replaced_by": version}),
        )
        .await?;
    }
    audit::record(
        &mut tx,
        &ctx.t.caller.actor,
        ctx.t.caller.actor_type(),
        "transition",
        "procedure",
        &resource,
        true,
        json!({"from": from, "to": t.to}),
    )
    .await?;
    let row = load(&mut tx, &name, &version).await?;
    tx.commit().await?;
    Ok(Json(row))
}

/// Approve a version under review (CANARY): the caller joins `reviewers` (kept sorted and distinct). Approving is
/// separate from publishing -- a publish needs two distinct reviewers and is its own `status` call. Returns the
/// updated detail. Only CANARY can be approved (`not_in_review`), and once per reviewer (`already_approved`).
async fn approve(
    JobAdmin(ctx): JobAdmin,
    State(s): State<AppState>,
    ApiPath(VersionPath { name, version }): ApiPath<VersionPath>,
) -> ApiResult<Json<Value>> {
    let resource = format!("{name}@{version}");
    let actor = ctx.t.caller.actor.clone();
    let mut tx = ctx.begin(&s.db).await?;
    tenant_target(&mut tx, ctx.job_id, &name, &version).await?;
    // Same per-name lock as `transition`, so an approval and a publish of one name serialize.
    let rows: Vec<(i64, String, String, Vec<String>)> = sqlx::query_as(
        "SELECT id, version, version_status::text, reviewers FROM procedures
          WHERE name=$1 AND origin='tenant' AND NOT hidden ORDER BY id FOR UPDATE",
    )
    .bind(&name)
    .fetch_all(&mut *tx)
    .await?;
    let (id, _, status, mut reviewers) = rows
        .into_iter()
        .find(|r| r.1 == version)
        .ok_or_else(|| ApiError::NotFound(format!("procedure {resource}")))?;
    if status != "CANARY" {
        return Err(ApiError::Conflict(
            "not_in_review",
            format!("only a CANARY version can be approved; {resource} is {status}"),
        ));
    }
    if reviewers.contains(&actor) {
        return Err(ApiError::Conflict(
            "already_approved",
            format!("{actor} has already approved {resource}"),
        ));
    }
    reviewers.push(actor.clone());
    reviewers.sort();
    reviewers.dedup();
    sqlx::query("UPDATE procedures SET reviewers=$2, updated_at=now() WHERE id=$1")
        .bind(id)
        .bind(&reviewers)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &actor,
        ctx.t.caller.actor_type(),
        "approve",
        "procedure",
        &resource,
        true,
        json!({"reviewers": reviewers}),
    )
    .await?;
    let row = load(&mut tx, &name, &version).await?;
    tx.commit().await?;
    Ok(Json(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Value {
        json!({"phases": [
            {"gate": null, "steps": [
                {"worker": "crm", "action": "verify_customer", "as": "customer", "args": {}},
                {"worker": "order", "action": "get_order", "as": "order", "args": {}}
            ]},
            {"gate": null, "steps": [
                {"worker": "order", "action": "create_rma", "as": "rma1", "args": {}},
                {"worker": "order", "action": "create_rma", "as": "rma2", "args": {}}
            ]}
        ]})
    }

    #[test]
    fn flagged_rows_land_on_their_plan_steps() {
        let item = json!({"plan": plan(), "side_effect_steps": [
            {"phase": 0, "mcp_tool": "order_get_order", "side_effect": true},
            {"phase": 1, "mcp_tool": "order_create_rma", "side_effect": false},
            {"phase": 1, "mcp_tool": "order_create_rma", "side_effect": true},
            // names no plan step: ignored
            {"phase": 0, "mcp_tool": "order_issue_refund", "side_effect": true},
            {"phase": 7, "mcp_tool": "order_get_order", "side_effect": true}
        ]});
        let out = with_step_side_effects(item);
        assert!(out.get("side_effect_steps").is_none());
        let ph = &out["plan"]["phases"];
        assert!(ph[0]["steps"][0].get("side_effect").is_none());
        assert_eq!(ph[0]["steps"][1]["side_effect"], true);
        assert_eq!(ph[1]["steps"][0]["side_effect"], false);
        assert_eq!(ph[1]["steps"][1]["side_effect"], true);
        let p: carebus::domain::Plan = serde_json::from_value(out["plan"].clone()).unwrap();
        assert_eq!(p.phases[0].steps[0].side_effect, None);
        assert_eq!(p.phases[0].steps[1].side_effect, Some(true));
        assert_eq!(p.phases[1].steps[0].side_effect, Some(false));
    }

    #[test]
    fn no_flagged_rows_leave_the_plan_as_stored() {
        let out = with_step_side_effects(json!({"plan": plan(), "side_effect_steps": []}));
        assert_eq!(out, json!({"plan": plan()}));
    }
}
