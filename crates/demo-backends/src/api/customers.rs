//! Customer lookup, search and the 90-day start-data bundle.
use axum::Json;
use axum::extract::State;
use chrono::Duration;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::api::orders::{OrderRow, PaymentRow, order_json, payment_json};
use crate::extract::{PathIn, QueryIn};
use crate::{AppState, DemoError, Result};

const CUSTOMER_COLS: &str = "customer_id, name, email, phone, tier, language, city, customer_since";

pub async fn load_customer(pool: &PgPool, id: &str) -> Result<Value> {
    let row: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(c) FROM (SELECT {CUSTOMER_COLS} FROM customers WHERE customer_id = $1) c"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.ok_or_else(|| DemoError::NotFound(format!("customer {id}")))
}

pub async fn get_customer(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
) -> Result<Json<Value>> {
    s.faults
        .hit("crm_verify_customer", s.cfg.fault_sleep)
        .await?;
    Ok(Json(load_customer(&s.pool, &id).await?))
}

#[derive(Deserialize)]
pub struct Search {
    q: Option<String>,
}

pub async fn search(State(s): State<AppState>, QueryIn(p): QueryIn<Search>) -> Result<Json<Value>> {
    let q = format!("%{}%", p.q.unwrap_or_default().replace(['%', '_'], ""));
    let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(c) FROM (SELECT {CUSTOMER_COLS} FROM customers WHERE name ILIKE $1 OR email ILIKE $1 OR customer_id ILIKE $1 ORDER BY customer_id LIMIT 50) c"
    )))
    .bind(q)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(json!({"customers": rows})))
}

pub async fn start_data(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
) -> Result<Json<Value>> {
    s.faults
        .hit("crm_get_start_data", s.cfg.fault_sleep)
        .await?;
    let customer = load_customer(&s.pool, &id).await?;
    let since = s.cfg.today - Duration::days(90);
    let orders: Vec<OrderRow> = sqlx::query_as(
        "SELECT order_id, customer_id, placed, item, category, amount_cents, status, shipment_id, delivered_on FROM orders WHERE customer_id = $1 AND placed BETWEEN $2 AND $3 ORDER BY placed DESC, order_id DESC",
    )
    .bind(&id)
    .bind(since)
    .bind(s.cfg.today)
    .fetch_all(&s.pool)
    .await?;
    let ids: Vec<String> = orders.iter().map(|o| o.order_id.clone()).collect();
    let deliveries: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('shipment_id', shipment_id, 'order_id', order_id, 'status', status) FROM shipments WHERE order_id = ANY($1) ORDER BY shipment_id DESC",
    )
    .bind(&ids)
    .fetch_all(&s.pool)
    .await?;
    let pays: Vec<PaymentRow> = sqlx::query_as(
        "SELECT payment_id, order_id, method, amount_cents, status, captured_at FROM payments WHERE order_id = ANY($1) ORDER BY captured_at",
    )
    .bind(&ids)
    .fetch_all(&s.pool)
    .await?;
    let comms: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('at', at, 'channel', channel, 'subject', subject) FROM communications WHERE customer_id = $1 AND at >= $2 ORDER BY at DESC, id DESC",
    )
    .bind(&id)
    .bind(since)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(json!({
        "customer": customer,
        "orders": orders.iter().map(order_json).collect::<Vec<_>>(),
        "deliveries": deliveries,
        "payments": pays.iter().map(payment_json).collect::<Vec<_>>(),
        "communications": comms,
    })))
}
