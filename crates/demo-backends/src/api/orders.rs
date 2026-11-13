//! Order, payment and shipment rows and their JSON shapes.
use axum::Json;
use axum::extract::State;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::extract::{PathIn, QueryIn};
use crate::money::euros;
use crate::{AppState, DemoError, Result};

const ORDER_COLS: &str = "order_id, customer_id, placed, item, category, amount_cents, status, shipment_id, delivered_on";

#[derive(sqlx::FromRow)]
pub struct OrderRow {
    pub order_id: String,
    pub customer_id: String,
    pub placed: NaiveDate,
    pub item: String,
    pub category: String,
    pub amount_cents: i64,
    pub status: String,
    pub shipment_id: Option<String>,
    pub delivered_on: Option<NaiveDate>,
}

#[derive(sqlx::FromRow)]
pub struct PaymentRow {
    pub payment_id: String,
    pub order_id: String,
    pub method: String,
    pub amount_cents: i64,
    pub status: String,
    pub captured_at: DateTime<Utc>,
}

pub fn order_json(o: &OrderRow) -> Value {
    json!({"order_id": o.order_id, "customer_id": o.customer_id, "placed": o.placed, "item": o.item, "category": o.category,
           "amount": euros(o.amount_cents), "status": o.status, "shipment_id": o.shipment_id, "delivered_on": o.delivered_on})
}

pub fn payment_json(p: &PaymentRow) -> Value {
    json!({"payment_id": p.payment_id, "order_id": p.order_id, "method": p.method, "amount": euros(p.amount_cents),
           "status": p.status, "at": p.captured_at, "at_epoch_s": p.captured_at.timestamp()})
}

pub async fn load_order(pool: &PgPool, id: &str) -> Result<OrderRow> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders WHERE order_id = $1"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DemoError::NotFound(format!("order {id}")))
}

pub async fn load_payments(pool: &PgPool, order_id: &str) -> Result<Vec<PaymentRow>> {
    Ok(sqlx::query_as(
        "SELECT payment_id, order_id, method, amount_cents, status, captured_at FROM payments WHERE order_id = $1 ORDER BY captured_at, payment_id",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await?)
}

#[derive(Deserialize)]
pub struct ByCustomer {
    customer_id: String,
}

pub async fn list(
    State(s): State<AppState>,
    QueryIn(p): QueryIn<ByCustomer>,
) -> Result<Json<Value>> {
    let rows: Vec<OrderRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ORDER_COLS} FROM orders WHERE customer_id = $1 ORDER BY placed DESC, order_id DESC"
    )))
    .bind(&p.customer_id)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(
        json!({"orders": rows.iter().map(order_json).collect::<Vec<_>>()}),
    ))
}

pub async fn get_order(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
) -> Result<Json<Value>> {
    s.faults.hit("order_get_order", s.cfg.fault_sleep).await?;
    let o = load_order(&s.pool, &id).await?;
    let lines: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('line_no', line_no, 'sku', sku, 'name', name, 'category', category, 'qty', qty, 'unit_price', unit_price_cents / 100.0) FROM order_lines WHERE order_id = $1 ORDER BY line_no",
    )
    .bind(&id)
    .fetch_all(&s.pool)
    .await?;
    let mut v = order_json(&o);
    v["lines"] = json!(lines);
    Ok(Json(v))
}

pub async fn payments(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
) -> Result<Json<Value>> {
    s.faults
        .hit("order_get_payments", s.cfg.fault_sleep)
        .await?;
    load_order(&s.pool, &id).await?;
    let p = load_payments(&s.pool, &id).await?;
    Ok(Json(
        json!({"order_id": id, "payments": p.iter().map(payment_json).collect::<Vec<_>>()}),
    ))
}

pub async fn shipment(
    State(s): State<AppState>,
    PathIn(id): PathIn<String>,
) -> Result<Json<Value>> {
    s.faults
        .hit("delivery_track_shipment", s.cfg.fault_sleep)
        .await?;
    let head: Option<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(x) FROM (SELECT shipment_id, order_id, carrier, status, eta, to_char(delivered_at AT TIME ZONE 'Europe/Amsterdam', 'YYYY-MM-DD HH24:MI') AS delivered_at, signed_by FROM shipments WHERE shipment_id = $1) x",
    )
    .bind(&id)
    .fetch_optional(&s.pool)
    .await?;
    let mut v = head.ok_or_else(|| DemoError::NotFound(format!("shipment {id}")))?;
    let events: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('seq', seq, 'at', to_char(at AT TIME ZONE 'Europe/Amsterdam', 'YYYY-MM-DD HH24:MI'), 'location', location, 'description', description) FROM shipment_events WHERE shipment_id = $1 ORDER BY seq",
    )
    .bind(&id)
    .fetch_all(&s.pool)
    .await?;
    v["last_event"] = events
        .last()
        .map(|e| {
            json!(format!(
                "{} at {}, {}",
                e["description"].as_str().unwrap_or(""),
                e["location"].as_str().unwrap_or(""),
                e["at"].as_str().unwrap_or("")
            ))
        })
        .unwrap_or(Value::Null);
    v["events"] = json!(events);
    Ok(Json(v))
}
