//! Admin routes: fault injection, reset, demo scenarios.
use axum::Json;
use axum::extract::State;
use chrono::Duration;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::extract::{JsonIn, PathIn};
use crate::faults::FaultMode;
use crate::{AppState, DemoError, Result, seed};

#[derive(Deserialize)]
pub struct FaultIn {
    tool: String,
    mode: FaultMode,
    #[serde(default = "one")]
    count: u32,
}

fn one() -> u32 {
    1
}

pub async fn put_fault(
    State(s): State<AppState>,
    JsonIn(b): JsonIn<FaultIn>,
) -> Result<Json<Value>> {
    s.faults.set(&b.tool, b.mode, b.count)?;
    Ok(Json(s.faults.list()))
}

pub async fn get_faults(State(s): State<AppState>) -> Json<Value> {
    Json(s.faults.list())
}

#[derive(Deserialize, Default)]
pub struct ResetIn {
    seed: Option<u64>,
}

pub async fn reset(
    State(s): State<AppState>,
    body: Option<JsonIn<ResetIn>>,
) -> Result<Json<Value>> {
    let seed_value = body.and_then(|b| b.0.seed).unwrap_or(s.cfg.seed);
    let r = seed::reset(&s.pool, seed_value, s.cfg.today).await?;
    s.faults.clear();
    Ok(Json(
        serde_json::to_value(r).expect("SeedReport serializes"),
    ))
}

const SCENARIOS: [(&str, &str); 4] = [
    (
        "duplicate-charge",
        "Restore Anna's duplicate capture on ORD-58790 (PAY-3399, 38 s after PAY-3398) and remove its refunds.",
    ),
    (
        "late-parcel",
        "The airfryer parcel SHP-88123 is delayed at the sorting centre; its ETA has passed.",
    ),
    (
        "old-order-return",
        "The headphones ORD-58213 were delivered 45 days ago, so a return is refused by RET-01.",
    ),
    (
        "carrier-timeout",
        "The next carrier tracking call times out, to show a retry.",
    ),
];

pub async fn list_scenarios() -> Json<Value> {
    Json(
        json!({"scenarios": SCENARIOS.iter().map(|(n, d)| json!({"name": n, "description": d})).collect::<Vec<_>>()}),
    )
}

pub async fn apply_scenario(
    State(s): State<AppState>,
    PathIn(name): PathIn<String>,
) -> Result<Json<Value>> {
    let mut tx = s.pool.begin().await?;
    let changes: Vec<&str> = match name.as_str() {
        "duplicate-charge" => {
            sqlx::query("DELETE FROM refunds WHERE payment_id IN ('PAY-3398','PAY-3399')")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE payments SET status='captured', amount_cents=22900, captured_at = CASE payment_id WHEN 'PAY-3398' THEN timestamptz '2026-10-11T19:02:11Z' ELSE timestamptz '2026-10-11T19:02:49Z' END WHERE payment_id IN ('PAY-3398','PAY-3399')")
                .execute(&mut *tx)
                .await?;
            vec![
                "PAY-3398 and PAY-3399 captured 38 s apart",
                "refunds on both removed",
            ]
        }
        "late-parcel" => {
            let at = s
                .cfg
                .today
                .and_hms_opt(7, 30, 0)
                .expect("07:30 is a valid time")
                .and_utc();
            sqlx::query(
                "UPDATE shipments SET status='delayed', eta=$1 WHERE shipment_id='SHP-88123'",
            )
            .bind(format!("{}, 14:00–17:00", s.cfg.today - Duration::days(1)))
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE orders SET status='delayed' WHERE order_id='ORD-58790'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO shipment_events VALUES ('SHP-88123', 3, $1, 'Sorting centre Nieuwegein', 'Delayed: sorting backlog') ON CONFLICT (shipment_id, seq) DO UPDATE SET at = $1, description = EXCLUDED.description")
                .bind(at)
                .execute(&mut *tx)
                .await?;
            vec![
                "SHP-88123 delayed, ETA in the past",
                "ORD-58790 status delayed",
            ]
        }
        "old-order-return" => {
            let d = s.cfg.today - Duration::days(45);
            sqlx::query("UPDATE orders SET delivered_on=$1 WHERE order_id='ORD-58213'")
                .bind(d)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE shipments SET delivered_at = ($1::date + time '13:42') AT TIME ZONE 'Europe/Amsterdam' WHERE shipment_id='SHP-87410'")
                .bind(d)
                .execute(&mut *tx)
                .await?;
            vec![
                "ORD-58213 delivered_on moved 45 days back",
                "SHP-87410 delivered_at moved to match",
            ]
        }
        "carrier-timeout" => {
            s.faults
                .set("delivery_track_shipment", FaultMode::Timeout, 1)?;
            vec!["next delivery_track_shipment call times out"]
        }
        other => return Err(DemoError::NotFound(format!("scenario {other}"))),
    };
    tx.commit().await?;
    Ok(Json(json!({"applied": name, "changes": changes})))
}
