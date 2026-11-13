use std::net::SocketAddr;
use std::time::Duration;

use chrono::NaiveDate;

use crate::error::{DemoError, Result};

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    /// Every accepted caller token: `DEMO_BACKENDS_TOKEN` plus the comma list `DEMO_BACKENDS_TOKENS`,
    /// so each calling service carries its own secret.
    pub tokens: Vec<String>,
    pub today: NaiveDate,
    pub seed: u64,
    pub fault_sleep: Duration,
}

/// Never prints the bearer token; the database URL keeps its shape but loses the password.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("database_url", &redact_url(&self.database_url))
            .field("tokens", &"<redacted>")
            .field("today", &self.today)
            .field("seed", &self.seed)
            .field("fault_sleep", &self.fault_sleep)
            .finish()
    }
}

/// `scheme://user:pw@host/db` → `scheme://user:***@host/db`. A URL that does not parse this way
/// is redacted whole.
fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<redacted>".into();
    };
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => match userinfo.split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:***@{host}{tail}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

impl Config {
    pub fn from_env() -> Result<Config> {
        Config::from_map(|k| std::env::var(k).ok())
    }

    /// Pure: reads every setting through `get`, so tests never touch the process env
    /// (env mutation is `unsafe` in Rust 2024 and races with tests that read `TEST_PG_REGISTRY`).
    pub fn from_map(get: impl Fn(&str) -> Option<String>) -> Result<Config> {
        let var = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let bind = var("DEMO_BACKENDS_BIND").unwrap_or_else(|| "0.0.0.0:8096".into());
        let bind: SocketAddr = bind
            .parse()
            .map_err(|e| DemoError::Config(format!("DEMO_BACKENDS_BIND {bind}: {e}")))?;
        if bind.port() == 8000 {
            return Err(DemoError::Config("port 8000 is reserved; use 8096".into()));
        }
        let database_url = var("DATABASE_URL")
            .ok_or_else(|| DemoError::Config("DATABASE_URL is not set".into()))?;
        let mut tokens: Vec<String> = var("DEMO_BACKENDS_TOKEN")
            .map(|t| t.trim().to_string())
            .into_iter()
            .collect();
        for t in var("DEMO_BACKENDS_TOKENS").unwrap_or_default().split(',') {
            let t = t.trim();
            if !t.is_empty() && !tokens.iter().any(|x| x == t) {
                tokens.push(t.to_string());
            }
        }
        if tokens.is_empty() {
            return Err(DemoError::Config(
                "neither DEMO_BACKENDS_TOKEN nor DEMO_BACKENDS_TOKENS is set; refusing to serve without auth"
                    .into(),
            ));
        }
        let today = var("DEMO_TODAY").unwrap_or_else(|| "2026-10-15".into());
        let today = NaiveDate::parse_from_str(&today, "%Y-%m-%d")
            .map_err(|e| DemoError::Config(format!("DEMO_TODAY {today}: {e}")))?;
        let seed = var("DEMO_SEED")
            .map(|s| s.parse())
            .transpose()
            .map_err(|e| DemoError::Config(format!("DEMO_SEED: {e}")))?
            .unwrap_or(20261015);
        let ms: u64 = var("DEMO_FAULT_TIMEOUT_MS")
            .map(|s| s.parse())
            .transpose()
            .map_err(|e| DemoError::Config(format!("DEMO_FAULT_TIMEOUT_MS: {e}")))?
            .unwrap_or(3500);
        Ok(Config {
            bind,
            database_url,
            tokens,
            today,
            seed,
            fault_sleep: Duration::from_millis(ms),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> crate::Result<Config> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_map(move |k| m.get(k).cloned())
    }

    #[test]
    fn debug_redacts_token_and_password() {
        let c = cfg(&[
            (
                "DATABASE_URL",
                "postgres://svc:hunter2pw@db.example.com:5432/carebus?sslmode=require",
            ),
            ("DEMO_BACKENDS_TOKEN", "tok-very-secret"),
        ])
        .unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("tok-very-secret"), "{dbg}");
        assert!(!dbg.contains("hunter2pw"), "{dbg}");
        assert!(dbg.contains("svc:***@db.example.com:5432/carebus"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert_eq!(super::redact_url("not a url pw"), "<redacted>");
    }

    #[test]
    fn refuses_missing_token_and_port_8000_and_defaults_to_8096() {
        let db = ("DATABASE_URL", "postgres://x@127.0.0.1:1/x");
        assert!(
            cfg(&[db])
                .unwrap_err()
                .to_string()
                .contains("DEMO_BACKENDS_TOKEN")
        );
        assert!(
            cfg(&[
                db,
                ("DEMO_BACKENDS_TOKEN", "t"),
                ("DEMO_BACKENDS_BIND", "0.0.0.0:8000")
            ])
            .unwrap_err()
            .to_string()
            .contains("8000")
        );
        let ok = cfg(&[db, ("DEMO_BACKENDS_TOKEN", "t")]).unwrap();
        assert_eq!(ok.bind.port(), 8096);
        assert_eq!(ok.today.to_string(), "2026-10-15");
        assert_eq!(ok.fault_sleep.as_millis(), 3500);
        assert!(
            cfg(&[db, ("DEMO_BACKENDS_TOKEN", "  ")]).is_err(),
            "a blank token counts as missing"
        );
    }

    #[test]
    fn token_list_adds_per_caller_tokens() {
        let db = ("DATABASE_URL", "postgres://x@127.0.0.1:1/x");
        let only_list = cfg(&[db, ("DEMO_BACKENDS_TOKENS", " a , ,b ")]).unwrap();
        assert_eq!(only_list.tokens, vec!["a", "b"]);
        let both = cfg(&[
            db,
            ("DEMO_BACKENDS_TOKEN", "t"),
            ("DEMO_BACKENDS_TOKENS", "t,mcp"),
        ])
        .unwrap();
        assert_eq!(
            both.tokens,
            vec!["t", "mcp"],
            "deduplicated, single token first"
        );
        assert!(cfg(&[db, ("DEMO_BACKENDS_TOKENS", " , ")]).is_err());
    }
}
