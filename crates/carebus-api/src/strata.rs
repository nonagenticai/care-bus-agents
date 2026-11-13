//! strata_rag calls shared by the source routes, the run lifecycle and the knowledge write proxies. Every call
//! acts for one tenant (`X-Tenant-Id`), the request's, with the bearer crate::strata_tokens gives for it (the static
//! pinned token, or a licence token minted for that tenant), as app `care-bus` (`X-App-Id`) and with an
//! `X-Actor-Id` (the person, else `SERVICE_ACTOR_ID`).
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::strata_tokens::StrataApp;
use crate::upstream::{self, Extra, Payload, Reply};

pub const NAME: &str = "strata_rag";

/// One strata_rag request acting for `tenant`. Unconfigured is 503; a tenant the static token is not pinned to
/// is 409 `strata_not_provisioned_for_tenant` (a minted token: a tenant without its strata-rag licence is 409
/// `strata_not_licensed_for_tenant`), and nothing is sent.
pub(crate) async fn call(
    s: &AppState,
    tenant: Uuid,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> ApiResult<Reply> {
    let base = s
        .cfg
        .strata_rag_url
        .as_deref()
        .ok_or(ApiError::Upstream(NAME))?;
    let cred = s.strata_tokens.cred(&s.cfg, StrataApp::Rag, tenant).await?;
    upstream::call(&s.http, NAME, Some(base), cred.auth(), method, path, body).await
}

/// The person a strata_rag write is recorded against (`X-Actor-Id`): the caller's subject when it is a uuid (a
/// Keycloak user or service account). Any other subject names no one, and the call goes with care-bus' service
/// account (`SERVICE_ACTOR_ID`, crate::strata_tokens) or, without one, no header: never a made-up id.
pub fn actor(caller: &Caller) -> Option<Uuid> {
    carebus::identity::human_actor(&caller.sub)
}

/// [`call`] with any payload, acting for `actor`, with an optional timeout of its own.
pub(crate) async fn call_as(
    s: &AppState,
    tenant: Uuid,
    actor: Option<Uuid>,
    method: Method,
    path: &str,
    payload: Payload,
    timeout: Option<std::time::Duration>,
) -> ApiResult<Reply> {
    let base = s
        .cfg
        .strata_rag_url
        .as_deref()
        .ok_or(ApiError::Upstream(NAME))?;
    let cred = s.strata_tokens.cred(&s.cfg, StrataApp::Rag, tenant).await?;
    upstream::call_extra(
        &s.http,
        NAME,
        Some(base),
        cred.auth(),
        method,
        path,
        payload,
        Extra { actor, timeout },
    )
    .await
}

/// strata_rag's eval 4xx as our error, keeping its eval codes so the UI can act on them:
/// - 409 by its code: `eval_running` (one eval per tenant at a time) and every other strata_rag 409 code
///   (`conflict`, `question_exists`, `eval_not_ready`, ...) as itself; an unknown or missing code is `conflict`
/// - 422 `too_many_questions` / `invalid_input` as themselves
/// - `not_found` is our 404 naming `what`; a 404/405 without the envelope (an older strata_rag) is 503
/// - anything else is `upstream_rejected`
///
/// The message is strata_rag's own (it names the missing leaves / documents / bad CSV lines), cut at 2000 chars.
pub fn eval_refusal(status: StatusCode, body: &Value, what: &str) -> ApiError {
    let msg: String = upstream::message(NAME, status, body)
        .chars()
        .take(2000)
        .collect();
    match (status, body["error"]["code"].as_str()) {
        (StatusCode::NOT_FOUND, Some("not_found")) => ApiError::NotFound(what.to_string()),
        (StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED, _) => ApiError::Upstream(NAME),
        (StatusCode::CONFLICT, code) => ApiError::Conflict(conflict_code(code), msg),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("too_many_questions")) => {
            ApiError::Unprocessable("too_many_questions", msg)
        }
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_input")) => {
            ApiError::Unprocessable("invalid_input", msg)
        }
        _ => ApiError::Rejected(status, msg),
    }
}

/// strata_rag's 409 codes, kept as they are (our error codes are `&'static str`). A code not listed here is
/// strata_rag's generic `conflict`: still a 409 with its own message, never a made-up meaning.
const CONFLICT_CODES: [&str; 6] = [
    "eval_running",
    "question_exists",
    "eval_not_ready",
    "embed_model_mismatch",
    "provider_not_configured",
    "conflict",
];

fn conflict_code(code: Option<&str>) -> &'static str {
    code.and_then(|c| CONFLICT_CODES.iter().find(|k| **k == c).copied())
        .unwrap_or("conflict")
}

/// 409 `strata_not_provisioned_for_tenant` when strata_rag is configured and its static token is not pinned to
/// `tenant` (minted tokens: the mint's refusal, and a minted token is cached for the call that follows). Writes
/// check it first, so the refusal is neither sent nor audited as a write attempt. Unconfigured passes: the call
/// itself is then the 503.
pub async fn provisioned_for(s: &AppState, tenant: Uuid) -> ApiResult<()> {
    if s.cfg.strata_rag_url.is_some() {
        s.strata_tokens.cred(&s.cfg, StrataApp::Rag, tenant).await?;
    }
    Ok(())
}

/// strata_rag's 4xx as our error:
/// - `not_found` is our 404 naming `what`
/// - a 404/405 without its envelope (route missing) is 503
/// - 409 is `store_conflict`
/// - anything else is `upstream_rejected` with strata_rag's own message
pub fn refusal(status: StatusCode, body: &Value, what: &str) -> ApiError {
    upstream::refusal(NAME, status, body, what, "store_conflict")
}

/// `Some("leaf" | "router")`, or `None` when strata_rag does not know the store.
pub async fn store_kind(s: &AppState, tenant: Uuid, id: Uuid) -> ApiResult<Option<String>> {
    let Reply { status, body } =
        call(s, tenant, Method::GET, &format!("/api/stores/{id}"), None).await?;
    if status.is_success() {
        return body["kind"]
            .as_str()
            .map(|k| Some(k.to_string()))
            .ok_or(ApiError::Upstream(NAME));
    }
    match refusal(status, &body, "store") {
        ApiError::NotFound(_) => Ok(None),
        e => Err(e),
    }
}

pub enum Gone {
    Deleted,
    /// strata_rag no longer has it: the outcome the caller wanted.
    Missing,
}

pub async fn delete_document(s: &AppState, tenant: Uuid, id: &str) -> ApiResult<Gone> {
    let id = upstream::segment(id, false)?;
    let Reply { status, body } = call(
        s,
        tenant,
        Method::DELETE,
        &format!("/api/documents/{id}"),
        None,
    )
    .await?;
    if status.is_success() {
        return Ok(Gone::Deleted);
    }
    match refusal(status, &body, "document") {
        ApiError::NotFound(_) => Ok(Gone::Missing),
        e => Err(e),
    }
}

/// `POST /api/stores`; strata_rag's 201 body (a `StoreView`).
pub async fn create_store(s: &AppState, tenant: Uuid, body: Value) -> ApiResult<Value> {
    let Reply { status, body } = call(s, tenant, Method::POST, "/api/stores", Some(body)).await?;
    if status.is_success() {
        return match body["id"].as_str() {
            Some(_) => Ok(body),
            None => Err(ApiError::Upstream(NAME)),
        };
    }
    Err(refusal(status, &body, "parent store"))
}

pub async fn delete_store(s: &AppState, tenant: Uuid, id: Uuid, recursive: bool) -> ApiResult<()> {
    let path = format!("/api/stores/{id}?recursive={recursive}");
    let Reply { status, body } = call(s, tenant, Method::DELETE, &path, None).await?;
    if status.is_success() {
        return Ok(());
    }
    Err(refusal(status, &body, &format!("store {id}")))
}

/// `id` and every store below it, from strata_rag's tree (`children` lists). Just `id` when strata_rag lists
/// no such store.
pub async fn descendants(s: &AppState, tenant: Uuid, id: Uuid) -> ApiResult<Vec<Uuid>> {
    let Reply { status, body } = call(s, tenant, Method::GET, "/api/stores", None).await?;
    if !status.is_success() {
        return Err(refusal(status, &body, "stores"));
    }
    let stores = body["stores"].as_array().ok_or(ApiError::Upstream(NAME))?;
    let children = |sid: &str| -> Vec<String> {
        stores
            .iter()
            .find(|st| st["id"] == sid)
            .and_then(|st| st["children"].as_array())
            .map(|c| {
                c.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut out = vec![id];
    let mut queue = vec![id.to_string()];
    while let Some(sid) = queue.pop() {
        for c in children(&sid) {
            if let Ok(u) = Uuid::parse_str(&c)
                && !out.contains(&u)
            {
                out.push(u);
                queue.push(c);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env(code: &str) -> Value {
        json!({"error": {"code": code, "message": format!("strata says {code}")}})
    }

    #[test]
    fn eval_refusals_keep_strata_codes() {
        let cases = [
            (
                StatusCode::CONFLICT,
                "question_exists",
                409,
                "question_exists",
            ),
            (
                StatusCode::CONFLICT,
                "eval_not_ready",
                409,
                "eval_not_ready",
            ),
            (StatusCode::CONFLICT, "eval_running", 409, "eval_running"),
            // a generic conflict is not "an eval is running"
            (StatusCode::CONFLICT, "conflict", 409, "conflict"),
            (
                StatusCode::CONFLICT,
                "embed_model_mismatch",
                409,
                "embed_model_mismatch",
            ),
            (StatusCode::CONFLICT, "leaf_full", 409, "conflict"),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "too_many_questions",
                422,
                "too_many_questions",
            ),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_input",
                422,
                "invalid_input",
            ),
            (StatusCode::NOT_FOUND, "not_found", 404, "not_found"),
            (
                StatusCode::BAD_REQUEST,
                "invalid_tenant",
                400,
                "upstream_rejected",
            ),
        ];
        for (st, code, want_status, want_code) in cases {
            let e = eval_refusal(st, &env(code), "eval question");
            let (s, c) = e.status_and_code();
            assert_eq!((s.as_u16(), c), (want_status, want_code), "{code}");
            if want_code != "not_found" {
                assert_eq!(e.to_string(), format!("strata says {code}"));
            }
        }
        // an older strata_rag without the route: unavailable, never "not found"
        let e = eval_refusal(StatusCode::NOT_FOUND, &Value::Null, "eval run");
        assert_eq!(e.code(), "upstream_unavailable");
    }

    #[test]
    fn only_a_uuid_subject_is_an_actor() {
        let caller = |sub: &str| Caller {
            sub: sub.into(),
            actor: sub.into(),
            kind: crate::auth::CallerKind::Service,
        };
        let id = "7f1c9a2e-0000-4000-8000-00000000ac70";
        assert_eq!(actor(&caller(id)), Some(Uuid::parse_str(id).unwrap()));
        assert_eq!(actor(&caller("u-admin")), None);
        assert_eq!(actor(&caller("00000000-0000-0000-0000-000000000000")), None);
    }
}
