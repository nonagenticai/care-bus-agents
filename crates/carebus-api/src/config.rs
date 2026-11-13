use anyhow::{Context, Result};

use uuid::Uuid;

use crate::crypto::SourceKey;
use crate::error::{ApiError, ApiResult};
use crate::upstream::Auth;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    /// `DATABASE_URL`: the database owner. Used only to run migrations at startup, then closed.
    pub database_url: String,
    /// `DATABASE_APP_URL`: the runtime role (`carebus_app`: NOSUPERUSER, NOBYPASSRLS, owns no table), so row-level
    /// security applies to every query the API serves. Startup refuses a role that bypasses RLS.
    pub database_app_url: String,
    pub redis_url: String,
    pub prefix: String,
    pub keycloak_issuer: String,
    pub keycloak_audience: String,
    /// `KEYCLOAK_TRUSTED_ISSUERS` (realm per app, agent A1): every issuer whose tokens verify, each against its own
    /// JWKS with `iss` pinned to exactly that value. Default `[KEYCLOAK_ISSUER]`; always contains it.
    pub keycloak_trusted_issuers: Vec<String>,
    /// `KEYCLOAK_SERVICE_ONLY_REALMS` (default `secure-apps`): realms whose tokens verify only on the
    /// service-account doors (the signup intake and decider), never as a person.
    pub keycloak_service_only_realms: Vec<String>,
    /// Early-access signup and licence on login (`CAREBUS_SIGNUP_*`, `SIGNUP_*`, crate::signup::config).
    pub signup: crate::signup::config::SignupConfig,
    pub strata_rag_url: Option<String>,
    pub strata_rag_token: Option<String>,
    /// strata_ingest (`STRATA_INGEST_URL`, `STRATA_INGEST_TOKEN`): source re-sync and the ingest jobs page.
    pub strata_ingest_url: Option<String>,
    pub strata_ingest_token: Option<String>,
    /// `STRATA_TOKENS_TENANT_ID`: the tenant the static strata tokens are pinned to. A strata call for any other
    /// tenant is refused (409 `strata_not_provisioned_for_tenant`) instead of being sent with a token that acts for
    /// this one. Required (startup fails) once either strata URL is set.
    pub strata_tenant: Option<Uuid>,
    /// `CAREBUS_STRATA_BUNDLE_TOKENS=true` (crate::strata_tokens): a short-lived licence token is minted per tenant
    /// for every strata call instead of the static tokens above. `None` = the static, pinned tokens (the default).
    pub strata_bundle: Option<crate::strata_tokens::BundleTokenConfig>,
    pub demo_backends_url: Option<String>,
    pub demo_backends_token: Option<String>,
    /// nonagenticai MCP server endpoint (`MCP_URL`, ends in `/mcp`): the tool list and the overview health probe, called
    /// as each tenant with its own minted licences (crate::mcp_credentials).
    pub mcp_url: Option<String>,
    /// `MCP_INTERNAL_URL`: the MCP server's origin for `/mcp/internal/...` (bindings/ensure, crate::mcp_binding);
    /// unset = the origin of `MCP_URL`.
    pub mcp_internal_url: Option<String>,
    pub service_tokens: Vec<String>,
    /// Allowed browser origins (`CAREBUS_API_CORS_ORIGINS`); empty = no CORS layer.
    pub cors_origins: Vec<String>,
    /// `CAREBUS_SOURCE_KEY`: AES-256-GCM key for source credentials. `None` = credentials can be neither stored
    /// nor used (503 `not_configured`); a malformed value stops startup.
    pub source_key: Option<SourceKey>,
    /// Test-only: the api test-connection may reach 127.0.0.0/8 and ::1 (the in-process stub). No environment
    /// variable can set it; `from_lookup` always writes `false`.
    pub source_fetch_allow_loopback: bool,
    /// Licence gate 2 (`LICENSE_*`, crate::licence). Off by default.
    pub licence: crate::licence::LicenceConfig,
    /// Usage ledger + metering outbox (`METERING_*`, crate::metering). Off by default.
    pub metering: crate::metering::MeteringConfig,
    /// Telegram channel (`TELEGRAM_*`, crate::telegram). `None` = off (the default).
    pub telegram: Option<crate::telegram::TelegramConfig>,
    /// `SERVICE_ACTOR_ID` (carebus::identity): care-bus' Keycloak service-account sub, sent as `X-Actor-Id` on a
    /// strata_rag call no person is behind. `None` = no `X-Actor-Id` then; a malformed value stops startup.
    pub service_actor: Option<Uuid>,
}

/// Comma-separated list; surrounding whitespace trimmed, blanks dropped.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

/// `None` when unset; an error naming the variable (never its value) when it is not a 32-byte base64 key.
pub fn parse_source_key(raw: Option<String>) -> Result<Option<SourceKey>> {
    raw.map(|r| SourceKey::from_base64(&r).map_err(|e| anyhow::anyhow!("{e}")))
        .transpose()
}

/// `STRATA_TOKENS_TENANT_ID`: a bare, non-nil uuid (what strata_rag's `X-Tenant-Id` accepts), or unset.
pub fn parse_strata_tenant(raw: Option<String>) -> Result<Option<Uuid>> {
    let Some(raw) = raw else { return Ok(None) };
    let raw = raw.trim();
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(Some(u)),
        _ => anyhow::bail!("STRATA_TOKENS_TENANT_ID must be a bare, non-nil tenant uuid"),
    }
}

impl Config {
    /// The static strata tokens act for `strata_tenant` only; any other tenant is a 409, never a call.
    fn strata_auth<'a>(&self, token: Option<&'a str>, tenant: Uuid) -> ApiResult<Auth<'a>> {
        if self.strata_tenant != Some(tenant) {
            return Err(ApiError::Conflict(
                "strata_not_provisioned_for_tenant",
                "Knowledge and ingest are not provisioned for this tenant yet.".into(),
            ));
        }
        Ok(Auth {
            token,
            tenant: Some(tenant),
            ..Auth::default()
        })
    }

    /// strata_rag's bearer plus the tenant the call acts for.
    pub fn strata_rag_auth(&self, tenant: Uuid) -> ApiResult<Auth<'_>> {
        self.strata_auth(self.strata_rag_token.as_deref(), tenant)
    }

    /// strata_ingest's bearer plus the tenant the call acts for.
    pub fn strata_ingest_auth(&self, tenant: Uuid) -> ApiResult<Auth<'_>> {
        self.strata_auth(self.strata_ingest_token.as_deref(), tenant)
    }

    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// `from_env` over any lookup, so tests never touch the process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let req = |k: &str| get(k).with_context(|| format!("{k} is not set"));
        let opt = |k: &str| get(k).filter(|v| !v.is_empty());
        let cors_origins = split_list(&get("CAREBUS_API_CORS_ORIGINS").unwrap_or_default());
        for o in &cors_origins {
            o.parse::<axum::http::HeaderValue>()
                .with_context(|| format!("CAREBUS_API_CORS_ORIGINS has an invalid origin: {o}"))?;
        }
        let strata_rag_url = opt("STRATA_RAG_URL");
        let strata_ingest_url = opt("STRATA_INGEST_URL");
        let strata_tenant = parse_strata_tenant(opt("STRATA_TOKENS_TENANT_ID"))?;
        let strata_bundle = crate::strata_tokens::BundleTokenConfig::from_lookup(&get)?;
        if strata_tenant.is_none()
            && strata_bundle.is_none()
            && (strata_rag_url.is_some() || strata_ingest_url.is_some())
        {
            anyhow::bail!(
                "STRATA_TOKENS_TENANT_ID is not set: STRATA_RAG_URL / STRATA_INGEST_URL are, and the static strata \
                 tokens must name the tenant they are pinned to"
            );
        }
        let keycloak_issuer = req("KEYCLOAK_ISSUER")?.trim_end_matches('/').to_string();
        let trimmed = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let issuers = issuers_from(&trimmed, &keycloak_issuer)?;
        let signup = crate::signup::config::SignupConfig::from_lookup(
            &trimmed,
            &issuers.intake,
            &issuers.decider,
        )?;
        Ok(Self {
            bind: req("CAREBUS_API_BIND")?,
            database_url: req("DATABASE_URL")?,
            database_app_url: req("DATABASE_APP_URL")?,
            redis_url: req("REDIS_URL")?,
            prefix: get("CAREBUS_PREFIX").unwrap_or_default(),
            keycloak_issuer,
            keycloak_audience: req("KEYCLOAK_AUDIENCE")?,
            keycloak_trusted_issuers: issuers.trusted,
            keycloak_service_only_realms: issuers.service_only_realms,
            signup,
            strata_rag_url,
            strata_rag_token: opt("STRATA_RAG_TOKEN"),
            strata_ingest_url,
            strata_ingest_token: opt("STRATA_INGEST_TOKEN"),
            strata_tenant,
            strata_bundle,
            demo_backends_url: opt("DEMO_BACKENDS_URL"),
            demo_backends_token: opt("DEMO_BACKENDS_TOKEN"),
            mcp_url: opt("MCP_URL"),
            mcp_internal_url: opt("MCP_INTERNAL_URL"),
            service_tokens: split_list(&get("CAREBUS_API_SERVICE_TOKENS").unwrap_or_default()),
            cors_origins,
            source_key: parse_source_key(opt("CAREBUS_SOURCE_KEY"))?,
            source_fetch_allow_loopback: false,
            licence: crate::licence::LicenceConfig::from_lookup(&get)?,
            metering: crate::metering::MeteringConfig::from_lookup(&get)?,
            telegram: crate::telegram::TelegramConfig::from_lookup(&get)?,
            service_actor: carebus::identity::parse_service_actor(
                get(carebus::identity::SERVICE_ACTOR_ENV).as_deref(),
            )?,
        })
    }
}

/// `KEYCLOAK_SERVICE_ONLY_REALMS` when unset.
pub const DEFAULT_SERVICE_ONLY_REALMS: &str = "secure-apps";

/// The trusted issuers and the per-door issuer pins (realm per app, agent A1).
#[derive(Debug)]
pub struct Issuers {
    pub trusted: Vec<String>,
    pub service_only_realms: Vec<String>,
    pub intake: String,
    pub decider: String,
}

/// The realm named by a Keycloak issuer URL (`https://host/realms/<realm>` -> `<realm>`).
pub fn realm_of_issuer(issuer: &str) -> Option<&str> {
    let (_, realm) = issuer.trim_end_matches('/').rsplit_once("/realms/")?;
    (!realm.is_empty() && !realm.contains('/')).then_some(realm)
}

/// `KEYCLOAK_TRUSTED_ISSUERS`, `KEYCLOAK_SERVICE_ONLY_REALMS`, `SIGNUP_INTAKE_ISSUER`, `SIGNUP_DECIDER_ISSUER` (agent
/// `issuers_from`). Every default reproduces the single-issuer behaviour. Refused at boot: an empty or non-http(s)
/// list, a list without `KEYCLOAK_ISSUER`, `KEYCLOAK_ISSUER` in a service-only realm, and a door pin outside the list.
pub fn issuers_from(trimmed: &impl Fn(&str) -> Option<String>, default: &str) -> Result<Issuers> {
    let mut trusted: Vec<String> = Vec::new();
    match trimmed("KEYCLOAK_TRUSTED_ISSUERS") {
        None => trusted.push(default.to_string()),
        Some(raw) => {
            for i in split_list(&raw) {
                let i = i.trim_end_matches('/').to_string();
                let ok = reqwest::Url::parse(&i).is_ok_and(|p| {
                    matches!(p.scheme(), "http" | "https") && p.host_str().is_some()
                });
                if !ok {
                    anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS must list http(s) issuer URLs");
                }
                if !trusted.contains(&i) {
                    trusted.push(i);
                }
            }
        }
    }
    if trusted.is_empty() {
        anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS names no issuer (no token could ever verify)");
    }
    if !trusted.iter().any(|t| t == default) {
        anyhow::bail!("KEYCLOAK_TRUSTED_ISSUERS must contain KEYCLOAK_ISSUER");
    }
    let service_only_realms = split_list(
        &trimmed("KEYCLOAK_SERVICE_ONLY_REALMS")
            .unwrap_or_else(|| DEFAULT_SERVICE_ONLY_REALMS.into()),
    );
    if realm_of_issuer(default).is_some_and(|r| service_only_realms.iter().any(|s| s == r)) {
        anyhow::bail!(
            "KEYCLOAK_ISSUER is in a KEYCLOAK_SERVICE_ONLY_REALMS realm: no person could ever sign in"
        );
    }
    let pin = |k: &str| -> Result<String> {
        let v = trimmed(k)
            .map(|v| v.trim_end_matches('/').to_string())
            .unwrap_or_else(|| default.to_string());
        if !trusted.contains(&v) {
            anyhow::bail!("{k} is not in KEYCLOAK_TRUSTED_ISSUERS (that door could never verify)");
        }
        Ok(v)
    };
    let intake = pin("SIGNUP_INTAKE_ISSUER")?;
    let decider = pin("SIGNUP_DECIDER_ISSUER")?;
    Ok(Issuers {
        trusted,
        service_only_realms,
        intake,
        decider,
    })
}

#[cfg(test)]
mod tests {
    use super::split_list;

    #[test]
    fn issuers_default_to_the_single_issuer_and_refuse_bad_pins() {
        const P: &str = "https://kc.test/realms/care-bus";
        const S: &str = "https://kc.test/realms/secure-apps";
        let none = |_: &str| None::<String>;
        let i = super::issuers_from(&none, P).unwrap();
        assert_eq!(i.trusted, vec![P.to_string()]);
        assert_eq!(i.service_only_realms, vec!["secure-apps".to_string()]);
        assert_eq!((i.intake.as_str(), i.decider.as_str()), (P, P));
        let both = |k: &str| match k {
            "KEYCLOAK_TRUSTED_ISSUERS" => Some(format!("{P}, {S}/")),
            "SIGNUP_INTAKE_ISSUER" | "SIGNUP_DECIDER_ISSUER" => Some(S.to_string()),
            _ => None,
        };
        let i = super::issuers_from(&both, P).unwrap();
        assert_eq!(i.trusted, vec![P.to_string(), S.to_string()]);
        assert_eq!(i.intake, S);
        let outside = |k: &str| (k == "SIGNUP_DECIDER_ISSUER").then(|| S.to_string());
        let e = super::issuers_from(&outside, P).unwrap_err().to_string();
        assert!(e.contains("SIGNUP_DECIDER_ISSUER"), "{e}");
        let missing = |k: &str| (k == "KEYCLOAK_TRUSTED_ISSUERS").then(|| S.to_string());
        assert!(super::issuers_from(&missing, P).is_err());
        assert!(
            super::issuers_from(&none, S).is_err(),
            "a person issuer in secure-apps"
        );
        assert_eq!(super::realm_of_issuer(P), Some("care-bus"));
    }

    #[test]
    fn split_list_trims_and_drops_blanks() {
        assert_eq!(split_list(" a, ,b ,,c"), vec!["a", "b", "c"]);
        assert!(split_list("").is_empty());
        assert!(split_list(" , ").is_empty());
    }

    use super::Config;

    /// Built at runtime (bytes 0..32), so no key literal is ever committed.
    fn key() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode((0u8..32).collect::<Vec<u8>>())
    }

    fn base(k: &str) -> Option<String> {
        match k {
            "CAREBUS_API_BIND" => Some("127.0.0.1:8095"),
            "DATABASE_URL" => Some("postgres://u@h/carebus"),
            "DATABASE_APP_URL" => Some("postgres://carebus_app@h/carebus"),
            "REDIS_URL" => Some("redis://127.0.0.1:6391"),
            "KEYCLOAK_ISSUER" => Some("https://issuer.test/realms/portal"),
            "KEYCLOAK_AUDIENCE" => Some("care-bus"),
            _ => None,
        }
        .map(String::from)
    }

    fn with(extra: &'static str, value: String) -> impl Fn(&str) -> Option<String> {
        move |k: &str| {
            if k == extra {
                Some(value.clone())
            } else {
                base(k)
            }
        }
    }

    #[test]
    fn source_key_is_optional_but_never_malformed() {
        let c = Config::from_lookup(base).unwrap();
        assert!(
            c.source_key.is_none(),
            "unset = credentials disabled (fail closed per request)"
        );
        let c = Config::from_lookup(with("CAREBUS_SOURCE_KEY", key())).unwrap();
        assert!(c.source_key.is_some());
        assert!(
            !format!("{c:?}").contains(&key()),
            "Debug never prints the key"
        );
        let short = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(b"short")
        };
        let err = Config::from_lookup(with("CAREBUS_SOURCE_KEY", short.clone()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("CAREBUS_SOURCE_KEY") && err.contains("32 bytes"),
            "{err}"
        );
        assert!(!err.contains(&short), "the value is never echoed: {err}");
    }

    const TENANT: &str = "7f1c9a2e-0000-4000-8000-00000000fe11";

    #[test]
    fn strata_url_without_token_tenant_fails() {
        for url in ["STRATA_RAG_URL", "STRATA_INGEST_URL"] {
            let err = Config::from_lookup(|k| {
                if k == url {
                    Some("http://127.0.0.1:1".into())
                } else {
                    base(k)
                }
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains("STRATA_TOKENS_TENANT_ID"), "{url}: {err}");
        }
        // no strata at all: no tenant needed
        assert!(Config::from_lookup(base).unwrap().strata_tenant.is_none());
        // bundle tokens on: every tenant gets its own token, so no pinned tenant is needed either
        let c = Config::from_lookup(|k| match k {
            "STRATA_RAG_URL" => Some("http://127.0.0.1:1".into()),
            "CAREBUS_STRATA_BUNDLE_TOKENS" => Some("true".into()),
            "LICENSE_ADMIN_BASE_URL" => Some("http://lic.test".into()),
            "LICENSE_SA_TOKEN_URL" => Some("http://kc.test/token".into()),
            "LICENSE_BUNDLE_MINTER_CLIENT_SECRET" => Some("minter-secret".into()),
            _ => base(k),
        })
        .unwrap();
        assert!(c.strata_tenant.is_none());
        let b = c.strata_bundle.expect("bundle tokens on");
        assert_eq!(b.creds.client_id, "care-bus-bundle-minter");
        assert_eq!(b.creds.token_url, "http://kc.test/token");
    }

    #[test]
    fn the_app_database_url_is_required() {
        let err = Config::from_lookup(|k| {
            if k == "DATABASE_APP_URL" {
                None
            } else {
                base(k)
            }
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("DATABASE_APP_URL"), "{err}");
    }

    #[test]
    fn strata_auth_acts_only_for_the_token_tenant() {
        let c = Config::from_lookup(|k| match k {
            "STRATA_RAG_URL" => Some("http://127.0.0.1:8092".into()),
            "STRATA_TOKENS_TENANT_ID" => Some(TENANT.into()),
            _ => base(k),
        })
        .unwrap();
        let t: uuid::Uuid = TENANT.parse().unwrap();
        assert_eq!(c.strata_tenant, Some(t));
        assert_eq!(c.strata_rag_auth(t).unwrap().tenant, Some(t));
        assert_eq!(c.strata_ingest_auth(t).unwrap().tenant, Some(t));
        let other: uuid::Uuid = "7f1c9a2e-0000-4000-8000-00000000fe12".parse().unwrap();
        for r in [c.strata_rag_auth(other), c.strata_ingest_auth(other)] {
            match r {
                Err(crate::error::ApiError::Conflict(code, _)) => {
                    assert_eq!(code, "strata_not_provisioned_for_tenant")
                }
                _ => panic!("another tenant must be refused"),
            }
        }
        for bad in [
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
            "7f1c9a2e00004000800000000000fe11",
            "Bearer 7f1c9a2e-0000-4000-8000-00000000fe11",
        ] {
            let err = Config::from_lookup(with("STRATA_TOKENS_TENANT_ID", bad.to_string()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("STRATA_TOKENS_TENANT_ID"), "{bad}: {err}");
        }
    }

    #[test]
    fn service_actor_is_optional_but_never_malformed() {
        assert_eq!(Config::from_lookup(base).unwrap().service_actor, None);
        let sa = "7f1c9a2e-0000-4000-8000-000000005e5a";
        let c = Config::from_lookup(with("SERVICE_ACTOR_ID", sa.to_string())).unwrap();
        assert_eq!(c.service_actor, Some(uuid::Uuid::parse_str(sa).unwrap()));
        for bad in ["svc-care-bus", "00000000-0000-0000-0000-000000000000"] {
            let err = Config::from_lookup(with("SERVICE_ACTOR_ID", bad.to_string()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("SERVICE_ACTOR_ID"), "{bad}: {err}");
        }
    }

    #[test]
    fn ingest_env_is_read_and_loopback_can_never_be_enabled_from_env() {
        let c = Config::from_lookup(|k| match k {
            "STRATA_INGEST_URL" => Some("http://127.0.0.1:9135".into()),
            "STRATA_INGEST_TOKEN" => Some("t".into()),
            "CAREBUS_SOURCE_FETCH_ALLOW_LOOPBACK" => Some("true".into()),
            "STRATA_TOKENS_TENANT_ID" => Some(TENANT.into()),
            _ => base(k),
        })
        .unwrap();
        assert_eq!(
            c.strata_ingest_url.as_deref(),
            Some("http://127.0.0.1:9135")
        );
        assert_eq!(c.strata_ingest_token.as_deref(), Some("t"));
        assert!(!c.source_fetch_allow_loopback);
    }
}
