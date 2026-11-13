//! Early-access signup settings (agent `SignupConfig` + `IdentityConfig`, names kept; app-prefixed ones carry
//! `CAREBUS_`). Fail loud: a set-but-malformed value stops the boot with its NAME (never its value); with
//! `CAREBUS_SIGNUP_ENABLED=true` every part the approve needs is required.
//!
//! - `CAREBUS_SIGNUP_ENABLED` (default false): the signup routes answer 503 `signup_not_configured` while off (read
//!   per call from [`crate::signup::Signup::enabled`]).
//! - `SIGNUP_INTAKE_AZP` / `SIGNUP_INTAKE_ROLE` (default `care-bus-signup-intake`), `SIGNUP_DECIDER_AZP` (default
//!   `chat-bridge-signup-decider-care-bus`), `SIGNUP_DECIDER_ROLE` (default `care-bus-signup-decide`),
//!   `SIGNUP_INTAKE_ISSUER` / `SIGNUP_DECIDER_ISSUER` (crate::config::issuers_from).
//! - `SIGNUP_QUEUE_MAX_PENDING` (500), `SIGNUP_TICK_SECONDS` (60), `SIGNUP_APP_KEY` (`care-bus`: the `signup_app`
//!   attribute the email theme branches on), `SIGNUP_TELEGRAM_KEY` (`carebus`: the `ea1:<key>` callback prefix and the
//!   chat-bridge `SIGNUP_DECISION_TARGETS` key; chat-bridge accepts `[a-z0-9]{1,16}`, so `care-bus` cannot be it).
//! - `TELEGRAM_SIGNUP_BOT_TOKEN` (the nonagenticaibot token, SEND ONLY), `TELEGRAM_SIGNUP_CHAT_ID`,
//!   `TELEGRAM_SIGNUP_THREAD_ID`, `TELEGRAM_SIGNUP_API_BASE`. Not `TELEGRAM_BOT_TOKEN`: that is the
//!   @care_bus_agent_bot channel (crate::telegram), a different bot with its own webhook.
//! - `CAREBUS_PUBLIC_URL` (e.g. `https://cust.nonagentic.ai`): the email redirect and the Telegram deep link.
//!   `SIGNUP_FRONTEND_CLIENT_ID` (default `care-bus`), `INVITE_EMAIL_LIFESPAN_SECONDS` (14 days).
//! - `KEYCLOAK_ADMIN_TOKEN_URL`, `KEYCLOAK_ADMIN_CLIENT_ID` (default `care-bus-admin`), `KEYCLOAK_ADMIN_CLIENT_SECRET`,
//!   `KEYCLOAK_ADMIN_API_URL` (`.../admin/realms/care-bus`): the user-realm admin SA that creates the login.
//! - Licences issuing (the existing `care-bus-licenses-admin` SA of gate 2): `LICENSE_ADMIN_BASE_URL`,
//!   `LICENSE_SA_TOKEN_URL`, `LICENSE_SA_CLIENT_ID`, `LICENSE_SA_CLIENT_SECRET`, `LICENSE_APP_KEY` (`care-bus`), plus
//!   `LICENSE_PLAN_CODE` (`standard`), `LICENSE_SUBJECT_REALM` (`care-bus`), `LICENSE_BUNDLE_KEY` (`care-bus`),
//!   `LICENSE_BUNDLE_PLANS` (CSV, `standard`).
//! - `SIGNUP_JOB_KEY` (`customer_care_agent`): the job every workspace is founded with; `SIGNUP_BASE_TENANT_ID`
//!   (default the bootstrap tenant `0195f48b-d000-7000-8000-000000000099`): the tenant whose job of that key holds the
//!   base release a new workspace's job is put on (the `base_release` step).
//! - `CAREBUS_TENANT_BUNDLE_RECONCILE` (default false): the signup tick registers every ACTIVE tenant in licences and
//!   ensures its licence set (bundle) once, recorded in `tenant_bundle_reconcile` (migration 0022), retried with
//!   backoff. Needs the licences settings above. Runs whether or not `CAREBUS_SIGNUP_ENABLED` is on.
//! - `CAREBUS_LOGIN_LICENCE_ENABLED` (default false): licence on login. Separate from the signup switch on purpose:
//!   while care-bus logins are still in the portal realm, a member licence stamped `subject_realm=care-bus` would be
//!   wrong, so it is turned on with the realm cutover.
use anyhow::{Context, Result};

use crate::config::split_list;

#[derive(Clone)]
pub struct ClientCredentials {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
}
impl std::fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[redacted]")
            .finish()
    }
}

/// `KEYCLOAK_ADMIN_*`: the user realm's admin service account (`care-bus-admin`).
#[derive(Clone, Debug)]
pub struct KeycloakAdminConfig {
    pub creds: ClientCredentials,
    /// `.../admin/realms/care-bus`, no trailing slash.
    pub api_url: String,
}

/// What the approve and the login licence issue with.
#[derive(Clone, Debug)]
pub struct LicencesIssueConfig {
    pub creds: ClientCredentials,
    /// `LICENSE_ADMIN_BASE_URL`, no trailing slash.
    pub url: String,
    /// `LICENSE_OWNER_KEY` (default `care-bus`): care-bus's owning app in licences (the api_client of its SA). A tenant
    /// another owner holds is never ensured by care-bus.
    pub owner_key: String,
    pub app_key: String,
    pub plan_code: String,
    pub subject_realm: String,
    pub bundle_key: String,
    pub bundle_plans: Vec<String>,
}

#[derive(Clone)]
pub struct SignupTelegram {
    pub bot_token: String,
    pub chat_id: String,
    pub thread_id: i64,
    pub api_base: String,
}
impl std::fmt::Debug for SignupTelegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignupTelegram")
            .field("bot_token", &"[redacted]")
            .field("chat_id", &self.chat_id)
            .field("thread_id", &self.thread_id)
            .field("api_base", &self.api_base)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct SignupConfig {
    pub enabled: bool,
    pub intake_azp: Vec<String>,
    pub intake_role: String,
    pub decider_azp: Vec<String>,
    pub decider_role: String,
    pub intake_issuer: String,
    pub decider_issuer: String,
    pub max_pending: i64,
    pub tick_seconds: u64,
    pub app_key: String,
    pub telegram_key: String,
    pub telegram: Option<SignupTelegram>,
    pub telegram_missing: Vec<&'static str>,
    /// `CAREBUS_PUBLIC_URL`, no trailing slash.
    pub public_url: Option<String>,
    pub frontend_client_id: String,
    pub invite_email_lifespan_seconds: u64,
    pub keycloak_admin: Option<KeycloakAdminConfig>,
    pub licences: Option<LicencesIssueConfig>,
    pub login_licence: bool,
    /// `CAREBUS_TENANT_BUNDLE_RECONCILE`: ensure every active tenant's licence set once (the tick).
    pub bundle_reconcile: bool,
    /// The job every approved workspace is founded with (`SIGNUP_JOB_KEY`, default `customer_care_agent`).
    pub job_key: String,
    /// `SIGNUP_BASE_TENANT_ID`: whose `job_key` job's current release a new workspace starts on.
    pub base_tenant: uuid::Uuid,
}

/// `SIGNUP_BASE_TENANT_ID` when unset: the bootstrap tenant (migration 0006), the one job born with the base.
pub const DEFAULT_BASE_TENANT: uuid::Uuid = uuid::uuid!("0195f48b-d000-7000-8000-000000000099");

impl Default for SignupConfig {
    /// Off; the defaults of every name.
    fn default() -> Self {
        Self {
            enabled: false,
            intake_azp: vec![DEFAULT_INTAKE.into()],
            intake_role: DEFAULT_INTAKE.into(),
            decider_azp: vec![DEFAULT_DECIDER.into()],
            decider_role: DEFAULT_DECIDE_ROLE.into(),
            intake_issuer: String::new(),
            decider_issuer: String::new(),
            max_pending: 500,
            tick_seconds: 60,
            app_key: "care-bus".into(),
            telegram_key: "carebus".into(),
            telegram: None,
            telegram_missing: vec![],
            public_url: None,
            frontend_client_id: "care-bus".into(),
            invite_email_lifespan_seconds: 14 * 24 * 3600,
            keycloak_admin: None,
            licences: None,
            login_licence: false,
            bundle_reconcile: false,
            job_key: "customer_care_agent".into(),
            base_tenant: DEFAULT_BASE_TENANT,
        }
    }
}

pub const DEFAULT_INTAKE: &str = "care-bus-signup-intake";
pub const DEFAULT_DECIDER: &str = "chat-bridge-signup-decider-care-bus";
pub const DEFAULT_DECIDE_ROLE: &str = "care-bus-signup-decide";

fn flag(trimmed: &impl Fn(&str) -> Option<String>, k: &str) -> Result<bool> {
    match trimmed(k).as_deref() {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(_) => anyhow::bail!("{k} must be true or false"),
    }
}

fn number<T: std::str::FromStr>(
    trimmed: &impl Fn(&str) -> Option<String>,
    k: &str,
    default: T,
) -> Result<T> {
    match trimmed(k) {
        Some(v) => v
            .parse::<T>()
            .map_err(|_| anyhow::anyhow!("{k} is not a valid number")),
        None => Ok(default),
    }
}

fn http_url(k: &str, v: String) -> Result<String> {
    let v = v.trim_end_matches('/').to_string();
    if !(v.starts_with("https://") || v.starts_with("http://")) {
        anyhow::bail!("{k} must be an http(s) URL");
    }
    Ok(v)
}

/// `^[a-z0-9]{1,16}$`: what chat-bridge's `ea1` callback parser accepts as the app key.
pub fn valid_telegram_key(k: &str) -> bool {
    (1..=16).contains(&k.len())
        && k.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

impl SignupConfig {
    /// `trimmed` answers a set, non-blank value trimmed, else `None`.
    pub fn from_lookup(
        trimmed: &impl Fn(&str) -> Option<String>,
        intake_issuer: &str,
        decider_issuer: &str,
    ) -> Result<Self> {
        let or = |k: &str, d: &str| trimmed(k).unwrap_or_else(|| d.to_string());
        let list = |k: &str, d: &str| {
            let v = split_list(&or(k, d));
            if v.is_empty() {
                anyhow::bail!("{k} names no client");
            }
            Ok(v)
        };
        let enabled = flag(trimmed, "CAREBUS_SIGNUP_ENABLED")?;
        let login_licence = flag(trimmed, "CAREBUS_LOGIN_LICENCE_ENABLED")?;
        let bundle_reconcile = flag(trimmed, "CAREBUS_TENANT_BUNDLE_RECONCILE")?;
        let max_pending: i64 = number(trimmed, "SIGNUP_QUEUE_MAX_PENDING", 500)?;
        if max_pending < 1 {
            anyhow::bail!("SIGNUP_QUEUE_MAX_PENDING must be a whole number of at least 1");
        }
        let tick_seconds: u64 = number(trimmed, "SIGNUP_TICK_SECONDS", 60)?;
        if tick_seconds < 1 {
            anyhow::bail!("SIGNUP_TICK_SECONDS must be at least 1");
        }
        let telegram_key = or("SIGNUP_TELEGRAM_KEY", "carebus");
        if !valid_telegram_key(&telegram_key) {
            anyhow::bail!(
                "SIGNUP_TELEGRAM_KEY must be 1 to 16 of a-z and 0-9 (chat-bridge's ea1 key)"
            );
        }
        let names = [
            "TELEGRAM_SIGNUP_BOT_TOKEN",
            "TELEGRAM_SIGNUP_CHAT_ID",
            "TELEGRAM_SIGNUP_THREAD_ID",
        ];
        let telegram_missing: Vec<&'static str> =
            names.into_iter().filter(|k| trimmed(k).is_none()).collect();
        let telegram = match (
            trimmed("TELEGRAM_SIGNUP_BOT_TOKEN"),
            trimmed("TELEGRAM_SIGNUP_CHAT_ID"),
            trimmed("TELEGRAM_SIGNUP_THREAD_ID"),
        ) {
            (Some(bot_token), Some(chat_id), Some(thread_id)) => {
                let thread_id: i64 = thread_id.parse().map_err(|_| {
                    anyhow::anyhow!("TELEGRAM_SIGNUP_THREAD_ID is not a message_thread_id")
                })?;
                // Topic 167 is reserved for external-probe alarms (guide C1).
                if thread_id == 167 {
                    anyhow::bail!(
                        "TELEGRAM_SIGNUP_THREAD_ID must not be 167 (reserved for probe alarms)"
                    );
                }
                Some(SignupTelegram {
                    bot_token,
                    chat_id,
                    thread_id,
                    api_base: or("TELEGRAM_SIGNUP_API_BASE", "https://api.telegram.org")
                        .trim_end_matches('/')
                        .to_string(),
                })
            }
            _ => None,
        };
        let public_url = trimmed("CAREBUS_PUBLIC_URL")
            .map(|v| http_url("CAREBUS_PUBLIC_URL", v))
            .transpose()?;
        let keycloak_admin = match (
            trimmed("KEYCLOAK_ADMIN_TOKEN_URL"),
            trimmed("KEYCLOAK_ADMIN_CLIENT_SECRET"),
            trimmed("KEYCLOAK_ADMIN_API_URL"),
        ) {
            (Some(token_url), Some(client_secret), Some(api)) => Some(KeycloakAdminConfig {
                creds: ClientCredentials {
                    token_url,
                    client_id: or("KEYCLOAK_ADMIN_CLIENT_ID", "care-bus-admin"),
                    client_secret,
                },
                api_url: http_url("KEYCLOAK_ADMIN_API_URL", api)?,
            }),
            (None, None, None) => None,
            _ => anyhow::bail!(
                "KEYCLOAK_ADMIN_TOKEN_URL, KEYCLOAK_ADMIN_CLIENT_SECRET and KEYCLOAK_ADMIN_API_URL are set together or not at all"
            ),
        };
        let licences = match (
            trimmed("LICENSE_ADMIN_BASE_URL"),
            trimmed("LICENSE_SA_TOKEN_URL"),
            trimmed("LICENSE_SA_CLIENT_ID"),
            trimmed("LICENSE_SA_CLIENT_SECRET"),
        ) {
            (Some(url), Some(token_url), Some(client_id), Some(client_secret)) => {
                let bundle_plans = split_list(&or("LICENSE_BUNDLE_PLANS", "standard"));
                if bundle_plans.is_empty() {
                    anyhow::bail!("LICENSE_BUNDLE_PLANS names no plan");
                }
                Some(LicencesIssueConfig {
                    creds: ClientCredentials {
                        token_url,
                        client_id,
                        client_secret,
                    },
                    url: http_url("LICENSE_ADMIN_BASE_URL", url)?,
                    owner_key: or("LICENSE_OWNER_KEY", "care-bus"),
                    app_key: or("LICENSE_APP_KEY", crate::licence::DEFAULT_APP_KEY),
                    plan_code: or("LICENSE_PLAN_CODE", "standard"),
                    subject_realm: or("LICENSE_SUBJECT_REALM", "care-bus"),
                    bundle_key: or("LICENSE_BUNDLE_KEY", "care-bus"),
                    bundle_plans,
                })
            }
            _ => None,
        };
        let c = Self {
            enabled,
            intake_azp: list("SIGNUP_INTAKE_AZP", DEFAULT_INTAKE)?,
            intake_role: or("SIGNUP_INTAKE_ROLE", DEFAULT_INTAKE),
            decider_azp: list("SIGNUP_DECIDER_AZP", DEFAULT_DECIDER)?,
            decider_role: or("SIGNUP_DECIDER_ROLE", DEFAULT_DECIDE_ROLE),
            intake_issuer: intake_issuer.to_string(),
            decider_issuer: decider_issuer.to_string(),
            max_pending,
            tick_seconds,
            app_key: or("SIGNUP_APP_KEY", "care-bus"),
            telegram_key,
            telegram,
            telegram_missing,
            public_url,
            frontend_client_id: or("SIGNUP_FRONTEND_CLIENT_ID", "care-bus"),
            invite_email_lifespan_seconds: number(
                trimmed,
                "INVITE_EMAIL_LIFESPAN_SECONDS",
                14 * 24 * 3600,
            )?,
            keycloak_admin,
            licences,
            login_licence,
            bundle_reconcile,
            job_key: or("SIGNUP_JOB_KEY", "customer_care_agent"),
            base_tenant: match trimmed("SIGNUP_BASE_TENANT_ID") {
                None => DEFAULT_BASE_TENANT,
                Some(v) => uuid::Uuid::parse_str(&v)
                    .ok()
                    .filter(|u| !u.is_nil() && v.len() == 36)
                    .context("SIGNUP_BASE_TENANT_ID must be a bare, non-nil tenant uuid")?,
            },
        };
        let mut missing: Vec<&str> = Vec::new();
        if c.enabled {
            if c.public_url.is_none() {
                missing.push("CAREBUS_PUBLIC_URL");
            }
            if c.keycloak_admin.is_none() {
                missing.push(
                    "KEYCLOAK_ADMIN_TOKEN_URL/KEYCLOAK_ADMIN_CLIENT_SECRET/KEYCLOAK_ADMIN_API_URL",
                );
            }
        }
        if (c.enabled || c.login_licence || c.bundle_reconcile) && c.licences.is_none() {
            missing.push("LICENSE_ADMIN_BASE_URL/LICENSE_SA_TOKEN_URL/LICENSE_SA_CLIENT_ID/LICENSE_SA_CLIENT_SECRET");
        }
        if !missing.is_empty() {
            return Err(anyhow::anyhow!(
                "CAREBUS_SIGNUP_ENABLED / CAREBUS_LOGIN_LICENCE_ENABLED / CAREBUS_TENANT_BUNDLE_RECONCILE need {}",
                missing.join(", ")
            ))
            .context("early-access signup configuration");
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "https://kc.test/realms/care-bus";

    fn on(k: &str) -> Option<String> {
        match k {
            "CAREBUS_SIGNUP_ENABLED" => Some("true"),
            "CAREBUS_PUBLIC_URL" => Some("https://cust.nonagentic.ai/"),
            "KEYCLOAK_ADMIN_TOKEN_URL" => {
                Some("http://kc/realms/care-bus/protocol/openid-connect/token")
            }
            "KEYCLOAK_ADMIN_CLIENT_SECRET" => Some("s3cret"),
            "KEYCLOAK_ADMIN_API_URL" => Some("http://kc/admin/realms/care-bus"),
            "LICENSE_ADMIN_BASE_URL" => Some("http://licenses"),
            "LICENSE_SA_TOKEN_URL" => {
                Some("http://kc/realms/secure-apps/protocol/openid-connect/token")
            }
            "LICENSE_SA_CLIENT_ID" => Some("care-bus-licenses-admin"),
            "LICENSE_SA_CLIENT_SECRET" => Some("s3cret"),
            _ => None,
        }
        .map(String::from)
    }

    #[test]
    fn defaults_are_the_care_bus_contract() {
        let c = SignupConfig::from_lookup(&on, P, P).unwrap();
        assert!(c.enabled && !c.login_licence);
        assert_eq!(c.intake_azp, vec!["care-bus-signup-intake"]);
        assert_eq!(c.intake_role, "care-bus-signup-intake");
        assert_eq!(c.decider_azp, vec!["chat-bridge-signup-decider-care-bus"]);
        assert_eq!(c.decider_role, "care-bus-signup-decide");
        assert_eq!(c.telegram_key, "carebus");
        assert_eq!(c.app_key, "care-bus");
        assert_eq!(c.frontend_client_id, "care-bus");
        assert_eq!(c.public_url.as_deref(), Some("https://cust.nonagentic.ai"));
        let l = c.licences.unwrap();
        assert_eq!(
            (
                l.app_key.as_str(),
                l.subject_realm.as_str(),
                l.bundle_key.as_str(),
                l.plan_code.as_str()
            ),
            ("care-bus", "care-bus", "care-bus", "standard")
        );
        assert_eq!(c.keycloak_admin.unwrap().creds.client_id, "care-bus-admin");
        assert!(c.telegram.is_none());
        assert_eq!(c.telegram_missing.len(), 3);
        assert!(!format!("{:?}", SignupConfig::from_lookup(&on, P, P).unwrap()).contains("s3cret"));
    }

    #[test]
    fn enabled_without_its_parts_fails_by_name() {
        let only = |k: &str| (k == "CAREBUS_SIGNUP_ENABLED").then(|| "true".to_string());
        let e = format!("{:#}", SignupConfig::from_lookup(&only, P, P).unwrap_err());
        assert!(
            e.contains("CAREBUS_PUBLIC_URL") && e.contains("KEYCLOAK_ADMIN_API_URL"),
            "{e}"
        );
        // off: nothing is required
        assert!(
            !SignupConfig::from_lookup(&|_: &str| None, P, P)
                .unwrap()
                .enabled
        );
        let bad = |k: &str| (k == "SIGNUP_TELEGRAM_KEY").then(|| "care-bus".to_string());
        assert!(
            SignupConfig::from_lookup(&bad, P, P).is_err(),
            "chat-bridge refuses a dash"
        );
        let topic = |k: &str| match k {
            "TELEGRAM_SIGNUP_BOT_TOKEN" => Some("1:x".to_string()),
            "TELEGRAM_SIGNUP_CHAT_ID" => Some("-1003934173351".to_string()),
            "TELEGRAM_SIGNUP_THREAD_ID" => Some("167".to_string()),
            _ => None,
        };
        assert!(SignupConfig::from_lookup(&topic, P, P).is_err());
    }
}
