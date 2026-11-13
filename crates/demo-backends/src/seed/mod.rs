//! Reset: wipe every table, restart id sequences, load the fixed customer, then generated data.
pub mod fixed;
pub mod generator;

use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;

use crate::Result;

#[derive(Debug, Serialize, PartialEq)]
pub struct SeedReport {
    pub customers: i64,
    pub orders: i64,
    pub shipments: i64,
    pub payments: i64,
}

pub async fn reset(pool: &PgPool, seed: u64, today: NaiveDate) -> Result<SeedReport> {
    let mut tx = pool.begin().await?;
    sqlx::query("TRUNCATE customers, orders, order_lines, shipments, shipment_events, payments, refunds, rmas, warranty_claims, communications RESTART IDENTITY CASCADE")
        .execute(&mut *tx).await?;
    for s in [
        "ALTER SEQUENCE rma_seq RESTART WITH 41001",
        "ALTER SEQUENCE claim_seq RESTART WITH 7701",
        "ALTER SEQUENCE refund_seq RESTART WITH 90201",
    ] {
        sqlx::query(s).execute(&mut *tx).await?;
    }
    fixed::insert(&mut tx).await?;
    generator::insert(&mut tx, seed, today).await?;
    let count = |t: &'static str| format!("SELECT count(*) FROM {t}");
    let mut n = [0i64; 4];
    for (i, t) in ["customers", "orders", "shipments", "payments"]
        .into_iter()
        .enumerate()
    {
        n[i] = sqlx::query_scalar(sqlx::AssertSqlSafe(count(t)))
            .fetch_one(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(SeedReport {
        customers: n[0],
        orders: n[1],
        shipments: n[2],
        payments: n[3],
    })
}
