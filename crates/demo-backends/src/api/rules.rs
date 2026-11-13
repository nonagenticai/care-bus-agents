//! Business-rule endpoints (RET-01, WAR-01, PAY-04). Pure logic lives in `crate::rules`.
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::Value;

use crate::api::orders::{load_order, load_payments};
use crate::extract::QueryIn;
use crate::{AppState, DemoError, Result, rules};

#[derive(Deserialize)]
pub struct ByOrder {
    order_id: String,
    error_type: Option<String>,
}

pub async fn return_window(
    State(s): State<AppState>,
    QueryIn(q): QueryIn<ByOrder>,
) -> Result<Json<Value>> {
    s.faults
        .hit("rules_check_return_window", s.cfg.fault_sleep)
        .await?;
    let o = load_order(&s.pool, &q.order_id).await?;
    Ok(Json(rules::return_window(o.delivered_on, s.cfg.today)))
}

pub async fn warranty(
    State(s): State<AppState>,
    QueryIn(q): QueryIn<ByOrder>,
) -> Result<Json<Value>> {
    s.faults
        .hit("rules_check_warranty", s.cfg.fault_sleep)
        .await?;
    let o = load_order(&s.pool, &q.order_id).await?;
    Ok(Json(rules::warranty(o.delivered_on, s.cfg.today)))
}

pub async fn payment_issue(
    State(s): State<AppState>,
    QueryIn(q): QueryIn<ByOrder>,
) -> Result<Json<Value>> {
    s.faults
        .hit("rules_check_payment_issue", s.cfg.fault_sleep)
        .await?;
    let et = q.error_type.as_deref().unwrap_or("");
    if !matches!(et, "duplicate_charge" | "payment_declined") {
        return Err(DemoError::Invalid(
            "error_type must be duplicate_charge or payment_declined".into(),
        ));
    }
    load_order(&s.pool, &q.order_id).await?;
    let p: Vec<(String, i64, i64)> = load_payments(&s.pool, &q.order_id)
        .await?
        .into_iter()
        .filter(|p| p.status == "captured")
        .map(|p| (p.payment_id, p.amount_cents, p.captured_at.timestamp()))
        .collect();
    Ok(Json(rules::payment_issue(&p, et)))
}
