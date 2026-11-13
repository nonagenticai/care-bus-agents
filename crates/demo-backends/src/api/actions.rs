//! Side effects. Idempotency: UNIQUE(idempotency_key) + ON CONFLICT DO NOTHING; a repeat returns the stored row with 200.
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::Duration;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::orders::load_order;
use crate::extract::{JsonIn, PathIn};
use crate::money::{cents, euros};
use crate::{AppState, DemoError, Result};

type Created = (StatusCode, Json<Value>);

fn key_ok(k: &str) -> Result<()> {
    if k.trim().is_empty() || k.len() > 200 {
        return Err(DemoError::Invalid(
            "idempotency_key must be 1-200 characters".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct RmaIn {
    reason: String,
    idempotency_key: String,
}

pub async fn create_rma(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
    JsonIn(b): JsonIn<RmaIn>,
) -> Result<Created> {
    s.faults.hit("order_create_rma", s.cfg.fault_sleep).await?;
    key_ok(&b.idempotency_key)?;
    let o = load_order(&s.pool, &id).await?;
    let delivered = o.delivered_on.ok_or_else(|| DemoError::Conflict {
        code: "not_delivered",
        message: format!("order {id} has not been delivered yet"),
    })?;
    let inserted: Option<Value> = sqlx::query_scalar(
        "INSERT INTO rmas (rma_id, order_id, reason, deadline, idempotency_key) VALUES ('RMA-' || nextval('rma_seq'), $1, $2, $3, $4)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING to_jsonb(rmas.*)")
        .bind(&id).bind(&b.reason).bind(delivered + Duration::days(30)).bind(&b.idempotency_key)
        .fetch_optional(&s.pool).await.map_err(DemoError::from_db)?;
    let (status, row) = match inserted {
        Some(r) => (StatusCode::CREATED, r),
        None => (
            StatusCode::OK,
            existing(&s, "rmas", &b.idempotency_key, &id).await?,
        ),
    };
    Ok((
        status,
        Json(
            json!({"rma_id": row["rma_id"], "order_id": row["order_id"], "reason": row["reason"], "carrier": row["carrier"],
        "deadline": row["deadline"], "label": "emailed", "created_at": row["created_at"]}),
        ),
    ))
}

#[derive(Deserialize)]
pub struct ClaimIn {
    defect: String,
    idempotency_key: String,
}

pub async fn create_claim(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
    JsonIn(b): JsonIn<ClaimIn>,
) -> Result<Created> {
    s.faults
        .hit("order_create_warranty_claim", s.cfg.fault_sleep)
        .await?;
    key_ok(&b.idempotency_key)?;
    load_order(&s.pool, &id).await?;
    let pickup = s.cfg.today + Duration::days(2);
    let next = format!(
        "A service partner courier will pick it up on {} between 09:00 and 13:00. Repair or replacement takes up to 14 days.",
        pickup.format("%A %-d %B")
    );
    let inserted: Option<Value> = sqlx::query_scalar(
        "INSERT INTO warranty_claims (claim_id, order_id, defect, next_step, idempotency_key) VALUES ('WC-' || nextval('claim_seq'), $1, $2, $3, $4)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING to_jsonb(warranty_claims.*)")
        .bind(&id).bind(&b.defect).bind(next).bind(&b.idempotency_key)
        .fetch_optional(&s.pool).await.map_err(DemoError::from_db)?;
    let (status, row) = match inserted {
        Some(r) => (StatusCode::CREATED, r),
        None => (
            StatusCode::OK,
            existing(&s, "warranty_claims", &b.idempotency_key, &id).await?,
        ),
    };
    Ok((
        status,
        Json(
            json!({"claim_id": row["claim_id"], "order_id": row["order_id"], "defect": row["defect"], "next_step": row["next_step"], "created_at": row["created_at"]}),
        ),
    ))
}

#[derive(Deserialize)]
pub struct RefundIn {
    amount: f64,
    idempotency_key: String,
}

pub async fn create_refund(
    State(s): State<AppState>,
    PathIn(pid): PathIn<String>,
    JsonIn(b): JsonIn<RefundIn>,
) -> Result<Created> {
    s.faults
        .hit("order_issue_refund", s.cfg.fault_sleep)
        .await?;
    key_ok(&b.idempotency_key)?;
    let amount = cents(b.amount)?;
    if amount == 0 {
        return Err(DemoError::Invalid("amount must be greater than 0".into()));
    }
    let mut tx = s.pool.begin().await?;
    // Repeat of a key → return it before any capacity check.
    if let Some(r) = sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(r) FROM refunds r WHERE idempotency_key = $1",
    )
    .bind(&b.idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        if r["payment_id"] != pid {
            return Err(DemoError::Conflict {
                code: "idempotency_conflict",
                message: "idempotency_key was used for another payment".into(),
            });
        }
        let method: String =
            sqlx::query_scalar("SELECT method FROM payments WHERE payment_id = $1")
                .bind(&pid)
                .fetch_one(&mut *tx)
                .await?;
        return Ok((StatusCode::OK, Json(refund_json(&r, &method))));
    }
    // Lock the payment row: concurrent refunds on one payment serialise here.
    let pay: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT amount_cents, method, status FROM payments WHERE payment_id = $1 FOR UPDATE",
    )
    .bind(&pid)
    .fetch_optional(&mut *tx)
    .await?;
    let (captured, method, status) =
        pay.ok_or_else(|| DemoError::NotFound(format!("payment {pid}")))?;
    // Re-check the key under the lock. Under READ COMMITTED this SELECT sees a racer's committed
    // row: a same-key racer that won the lock first is returned as a repeat (200), never a 23505.
    if let Some(r) = sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(r) FROM refunds r WHERE idempotency_key = $1",
    )
    .bind(&b.idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        if r["payment_id"] != pid {
            return Err(DemoError::Conflict {
                code: "idempotency_conflict",
                message: "idempotency_key was used for another payment".into(),
            });
        }
        return Ok((StatusCode::OK, Json(refund_json(&r, &method))));
    }
    if status == "declined" {
        return Err(DemoError::Conflict {
            code: "refund_exceeds_capture",
            message: format!(
                "payment {pid} was declined: nothing was captured, so nothing can be refunded"
            ),
        });
    }
    let refunded: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(amount_cents), 0)::bigint FROM refunds WHERE payment_id = $1",
    )
    .bind(&pid)
    .fetch_one(&mut *tx)
    .await?;
    let total = refunded.checked_add(amount);
    if total.is_none_or(|t| t > captured) {
        return Err(DemoError::Conflict {
            code: "refund_exceeds_capture",
            message: format!(
                "refund of {:.2} exceeds the {:.2} still refundable on {pid}",
                euros(amount),
                euros(captured - refunded)
            ),
        });
    }
    let r: Value = sqlx::query_scalar("INSERT INTO refunds (refund_id, payment_id, amount_cents, idempotency_key) VALUES ('REF-' || nextval('refund_seq'), $1, $2, $3) RETURNING to_jsonb(refunds.*)")
        .bind(&pid).bind(amount).bind(&b.idempotency_key).fetch_one(&mut *tx).await.map_err(DemoError::from_db)?;
    if total == Some(captured) {
        sqlx::query("UPDATE payments SET status = 'refunded' WHERE payment_id = $1")
            .bind(&pid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(refund_json(&r, &method))))
}

fn refund_json(r: &Value, method: &str) -> Value {
    json!({"refund_id": r["refund_id"], "payment_id": r["payment_id"], "amount": euros(r["amount_cents"].as_i64().unwrap_or(0)),
           "method": method, "eta": "3–5 business days", "created_at": r["created_at"]})
}

/// The row a repeated key already created; 409 if it belongs to a different order.
async fn existing(s: &AppState, table: &'static str, key: &str, order_id: &str) -> Result<Value> {
    let row: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(t) FROM {table} t WHERE idempotency_key = $1"
    )))
    .bind(key)
    .fetch_one(&s.pool)
    .await?;
    if row["order_id"] != order_id {
        return Err(DemoError::Conflict {
            code: "idempotency_conflict",
            message: "idempotency_key was used for another order".into(),
        });
    }
    Ok(row)
}
