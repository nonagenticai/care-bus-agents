//! `GET /api/jobs/{job}/overview`: shared dependency health, plus the job's case counters and attention items (its
//! bus, procedures and model routes).
use std::time::Duration;

use axum::{Json, Router, extract::State, routing::get};
use carebus::Bus;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::ApiError;
use crate::redis_admin as ra;
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember};

const DEAD: &str = "bus:dead";
const LAG_WARN: u64 = 20;

pub fn routes() -> Router<AppState> {
    // Relative path; nested under `/api/jobs/{job}` by `routes::all`.
    Router::new().route("/overview", get(overview))
}

/// Every dependency call in the overview is bounded so one hung upstream cannot hold the page for the pool's timeout.
const STEP: Duration = Duration::from_secs(2);

/// Runs `f` for at most `STEP`; `Err("timed out")` on expiry, `Err("unreachable")` on any other failure.
async fn bounded<T, E: std::fmt::Display>(
    what: &str,
    f: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, &'static str> {
    match tokio::time::timeout(STEP, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "overview: {what} unavailable");
            Err("unreachable")
        }
        Err(_) => {
            tracing::warn!("overview: {what} timed out");
            Err("timed out")
        }
    }
}

/// One day of case counters; `None` when the database cannot answer (never reported as zeros).
async fn day(s: &AppState, ctx: &JobCtx, offset_days: i32) -> Option<Value> {
    bounded("case counters", async {
        let mut tx = ctx.begin(&s.db).await?;
        let v = sqlx::query_scalar::<_, Value>(
            "SELECT jsonb_build_object(
                'cases', count(*),
                'completed', count(*) FILTER (WHERE outcome = 'completed'),
                'not_eligible', count(*) FILTER (WHERE outcome = 'not_eligible'),
                'failed', count(*) FILTER (WHERE outcome = 'failed'),
                'open', count(*) FILTER (WHERE outcome IS NULL),
                'median_ms', percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms))
             FROM case_history WHERE started_at::date = current_date - $1",
        )
        .bind(offset_days)
        .fetch_one(&mut *tx)
        .await?;
        Ok::<Value, ApiError>(v)
    })
    .await
    .ok()
}

fn entry(name: &str, status: &str, detail: Option<&str>) -> Value {
    json!({"name": name, "status": status, "detail": detail})
}

fn dep(name: &str, r: Result<(), &'static str>) -> Value {
    match r {
        Ok(()) => entry(name, "ok", None),
        Err(why) => entry(name, "down", Some(why)),
    }
}

/// `base` is `None` when the dependency is not configured. The probed URL is never echoed (it may carry a path or userinfo);
/// `detail` carries only the failure reason.
async fn probe(s: &AppState, name: &str, base: Option<String>, path: &str) -> Value {
    let Some(b) = base else {
        return entry(name, "unconfigured", None);
    };
    let url = format!("{}{path}", b.trim_end_matches('/'));
    match s.probe.get(url).timeout(STEP).send().await {
        Ok(r) if r.status().is_success() => entry(name, "ok", None),
        Ok(r) => entry(name, "down", Some(&format!("HTTP {}", r.status().as_u16()))),
        Err(e) => entry(
            name,
            "down",
            Some(if e.is_timeout() {
                "timed out"
            } else {
                "unreachable"
            }),
        ),
    }
}

/// nonagenticai_mcp_server's `/api/health`, probed with the same credentials the tenant's tool calls carry (its
/// care-bus licence and strata-rag token, crate::mcp_credentials), so a licence the MCP server would refuse shows up
/// here too. No MCP client (no minter), or credentials that cannot be minted, is `down` with the reason and nothing is
/// sent; a 401 renews the credentials once, then stays `down`.
async fn probe_mcp(s: &AppState, tenant: Uuid) -> Value {
    const NAME: &str = "mcp";
    let Some(base) = s.cfg.mcp_url.as_deref().map(mcp_health_base) else {
        return entry(NAME, "unconfigured", None);
    };
    let Some(mcp) = s.mcp.as_ref() else {
        return entry(NAME, "down", Some("no MCP credentials"));
    };
    let url = format!("{base}/api/health");
    let mut creds = match tokio::time::timeout(STEP, mcp.credentials(tenant)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            tracing::warn!(%tenant, error = %format!("{e:#}"), "overview: mcp credentials unavailable");
            return entry(NAME, "down", Some("credentials unavailable"));
        }
        Err(_) => return entry(NAME, "down", Some("timed out")),
    };
    for attempt in 0..2 {
        let mut req = s.probe.get(&url).timeout(STEP);
        for (name, value) in creds.headers() {
            req = req.header(name, value);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => return entry(NAME, "ok", None),
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => {
                if attempt == 1 {
                    return entry(NAME, "down", Some("HTTP 401"));
                }
                // Refused: renew the tenant's credentials once and retry.
                let renewed =
                    tokio::time::timeout(STEP, mcp.renew_credentials(tenant, &creds)).await;
                creds = match renewed {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        tracing::warn!(%tenant, error = %format!("{e:#}"), "overview: mcp credentials unavailable");
                        return entry(NAME, "down", Some("credentials unavailable"));
                    }
                    Err(_) => return entry(NAME, "down", Some("timed out")),
                };
            }
            Ok(r) => return entry(NAME, "down", Some(&format!("HTTP {}", r.status().as_u16()))),
            Err(e) => {
                return entry(
                    NAME,
                    "down",
                    Some(if e.is_timeout() {
                        "timed out"
                    } else {
                        "unreachable"
                    }),
                );
            }
        }
    }
    unreachable!("the loop returns on its second attempt")
}

fn mcp_health_base(mcp_url: &str) -> String {
    let t = mcp_url.trim_end_matches('/');
    t.strip_suffix("/mcp").unwrap_or(t).to_string()
}

fn attn(severity: &str, title: String, page: &str) -> Value {
    json!({"severity": severity, "title": title, "page": page})
}

async fn bus_attention(bus: &Bus, out: &mut Vec<Value>) {
    let dead = bounded("dead-letter count", async {
        if ra::owns(bus, DEAD).await? {
            Ok(ra::stream_info(bus, DEAD).await?.length)
        } else {
            Ok::<u64, redis::RedisError>(0)
        }
    })
    .await;
    match dead {
        Ok(0) => {}
        Ok(n) => out.push(attn(
            "err",
            format!("{n} messages in the dead-letter stream"),
            "buses",
        )),
        Err(_) => out.push(attn(
            "err",
            "Dead letters could not be counted".into(),
            "buses",
        )),
    }
    let lag = bounded("consumer lag", async {
        let mut infos = Vec::new();
        for k in ra::list_streams(bus).await? {
            let i = ra::stream_info(bus, &k).await?;
            infos.push((k, i));
        }
        Ok::<_, redis::RedisError>(infos)
    })
    .await;
    match lag {
        Ok(infos) => {
            for (k, i) in infos {
                for g in i.groups.iter().filter(|g| g.lag.unwrap_or(0) > LAG_WARN) {
                    out.push(attn(
                        "warn",
                        format!(
                            "{} is {} messages behind on {k}",
                            g.name,
                            g.lag.unwrap_or(0)
                        ),
                        "buses",
                    ));
                }
            }
        }
        Err(_) => out.push(attn(
            "warn",
            "Consumer lag could not be read".into(),
            "buses",
        )),
    }
}

async fn db_attention(s: &AppState, ctx: &JobCtx, out: &mut Vec<Value>) {
    match bounded("canary procedures", async {
        let mut tx = ctx.begin(&s.db).await?;
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT name || '@' || version FROM eff_procedures WHERE version_status = 'CANARY' ORDER BY name, version_sort DESC",
        )
        .fetch_all(&mut *tx)
        .await?;
        Ok::<Vec<String>, ApiError>(rows)
    })
    .await
    {
        Ok(rows) => {
            for p in rows {
                out.push(attn(
                    "warn",
                    format!("{p} is CANARY and needs review"),
                    "procedures",
                ));
            }
        }
        Err(_) => out.push(attn(
            "warn",
            "Procedure versions could not be read".into(),
            "procedures",
        )),
    }
}

async fn overview(JobMember(ctx): JobMember, State(s): State<AppState>) -> Json<Value> {
    let (redis_r, pg_r, strata, demo, mcp) = tokio::join!(
        bounded("redis", async {
            redis::cmd("PING")
                .query_async::<String>(&mut s.bus.conn())
                .await
        }),
        bounded(
            "postgres",
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&s.db)
        ),
        probe(&s, "strata_rag", s.cfg.strata_rag_url.clone(), "/health"),
        probe(
            &s,
            "demo-backends",
            s.cfg.demo_backends_url.clone(),
            "/health"
        ),
        probe_mcp(&s, ctx.t.tenant),
    );
    let (redis_ok, pg_ok) = (redis_r.is_ok(), pg_r.is_ok());
    let health = vec![
        dep("redis", redis_r.map(|_| ())),
        dep("postgres", pg_r.map(|_| ())),
        strata,
        demo,
        mcp,
    ];

    let mut attention = Vec::new();
    for h in &health {
        if h["status"] == "down" {
            attention.push(attn(
                "err",
                format!("{} is down", h["name"].as_str().unwrap_or("a dependency")),
                "overview",
            ));
        }
    }
    if redis_ok {
        bus_attention(&s.bus.scoped(&ctx.scope()), &mut attention).await;
    }
    let (today, week_ago) = if pg_ok {
        db_attention(&s, &ctx, &mut attention).await;
        (day(&s, &ctx, 0).await, day(&s, &ctx, 7).await)
    } else {
        (None, None)
    };

    Json(json!({
        "today": today,
        "week_ago": week_ago,
        "health": health,
        "attention": attention,
    }))
}

#[cfg(test)]
mod tests {
    use super::mcp_health_base;

    #[test]
    fn mcp_base_drops_trailing_mcp() {
        assert_eq!(mcp_health_base("http://h:9/mcp"), "http://h:9");
        assert_eq!(mcp_health_base("http://h:9/mcp/"), "http://h:9");
        assert_eq!(mcp_health_base("http://h:9"), "http://h:9");
    }
}
