//! Allow-listed record browser/editor. Table, key and column names only ever come from SPECS,
//! never from the request; request values are bound parameters merged by jsonb_populate_record.
use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::extract::{JsonIn, PathIn, QueryIn};
use crate::{AppState, DemoError, Result};

pub struct Spec {
    pub table: &'static str,
    pub pk: &'static str,
    pub editable: &'static [&'static str],
    pub search: &'static [&'static str],
}

pub const SPECS: &[Spec] = &[
    Spec {
        table: "customers",
        pk: "customer_id",
        editable: &["name", "email", "phone", "tier", "city"],
        search: &["name", "email"],
    },
    Spec {
        table: "orders",
        pk: "order_id",
        editable: &["status", "delivered_on", "shipment_id"],
        search: &["customer_id", "item"],
    },
    Spec {
        table: "shipments",
        pk: "shipment_id",
        editable: &["status", "eta", "delivered_at", "signed_by", "carrier"],
        search: &["order_id", "carrier"],
    },
    Spec {
        table: "payments",
        pk: "payment_id",
        editable: &["status", "method"],
        search: &["order_id", "method"],
    },
    Spec {
        table: "communications",
        pk: "id",
        editable: &["subject", "channel"],
        search: &["customer_id", "subject"],
    },
    Spec {
        table: "refunds",
        pk: "refund_id",
        editable: &[],
        search: &["payment_id"],
    },
    Spec {
        table: "rmas",
        pk: "rma_id",
        editable: &[],
        search: &["order_id"],
    },
    Spec {
        table: "warranty_claims",
        pk: "claim_id",
        editable: &[],
        search: &["order_id"],
    },
];

fn spec(t: &str) -> Result<&'static Spec> {
    SPECS
        .iter()
        .find(|s| s.table == t)
        .ok_or_else(|| DemoError::NotFound(format!("table {t}")))
}

#[derive(Deserialize)]
pub struct ListQ {
    q: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

pub async fn list(
    State(s): State<AppState>,
    PathIn(table): PathIn<String>,
    QueryIn(p): QueryIn<ListQ>,
) -> Result<Json<Value>> {
    let sp = spec(&table)?;
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let offset = p.offset.unwrap_or(0).max(0);
    let q = format!("%{}%", p.q.unwrap_or_default().replace(['%', '_'], ""));
    let mut cond = vec![format!("t.{}::text ILIKE $1", sp.pk)];
    cond.extend(sp.search.iter().map(|c| format!("t.{c}::text ILIKE $1")));
    let wh = cond.join(" OR ");
    let rows: Vec<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(t) FROM {} t WHERE {wh} ORDER BY t.{} LIMIT $2 OFFSET $3",
        sp.table, sp.pk
    )))
    .bind(&q)
    .bind(limit)
    .bind(offset)
    .fetch_all(&s.pool)
    .await?;
    let total: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {} t WHERE {wh}",
        sp.table
    )))
    .bind(&q)
    .fetch_one(&s.pool)
    .await?;
    let columns: Vec<String> = sqlx::query_scalar("SELECT column_name::text FROM information_schema.columns WHERE table_schema='public' AND table_name=$1 ORDER BY ordinal_position")
        .bind(sp.table)
        .fetch_all(&s.pool)
        .await?;
    Ok(Json(json!({
        "table": sp.table, "primary_key": sp.pk, "editable": sp.editable,
        "columns": columns, "rows": rows, "total": total
    })))
}

pub async fn get_one(
    State(s): State<AppState>,
    PathIn((table, id)): PathIn<(String, String)>,
) -> Result<Json<Value>> {
    let sp = spec(&table)?;
    let row: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT to_jsonb(t) FROM {} t WHERE t.{}::text = $1",
        sp.table, sp.pk
    )))
    .bind(&id)
    .fetch_optional(&s.pool)
    .await?;
    Ok(Json(row.ok_or_else(|| {
        DemoError::NotFound(format!("{table} {id}"))
    })?))
}

pub async fn update(
    State(s): State<AppState>,
    PathIn((table, id)): PathIn<(String, String)>,
    JsonIn(patch): JsonIn<Map<String, Value>>,
) -> Result<Json<Value>> {
    let sp = spec(&table)?;
    if sp.editable.is_empty() {
        return Err(DemoError::ReadOnly(format!(
            "{table} is read-only in the demo editor"
        )));
    }
    if patch.is_empty() {
        return Err(DemoError::Invalid("nothing to update".into()));
    }
    let cols: Vec<&'static str> = patch
        .keys()
        .map(|k| {
            sp.editable.iter().copied().find(|c| c == k).ok_or_else(|| {
                DemoError::Invalid(format!(
                    "{k} cannot be edited on {table}; editable: {}",
                    sp.editable.join(", ")
                ))
            })
        })
        .collect::<Result<_>>()?;
    let set = cols
        .iter()
        .map(|c| format!("{c} = r.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE {t} x SET {set} FROM jsonb_populate_record((SELECT y FROM {t} y WHERE y.{pk}::text = $2), $1::jsonb) r WHERE x.{pk}::text = $2 RETURNING to_jsonb(x)",
        t = sp.table,
        pk = sp.pk
    );
    let row: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(Value::Object(patch))
        .bind(&id)
        .fetch_optional(&s.pool)
        .await
        .map_err(DemoError::from_db)?;
    Ok(Json(row.ok_or_else(|| {
        DemoError::NotFound(format!("{table} {id}"))
    })?))
}
