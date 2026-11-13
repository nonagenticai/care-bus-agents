//! Early-access signup (care-bus 17; early-access v2 guide contracts C1-C6). Ported from the `agent` app
//! (`crates/agent-api/src/signup`), itself after strata_configurator and commonpractice v2: the service-account
//! doors, requester-text cleaning, the default workspace name, the Telegram notice (crate::signup::telegram) and the
//! journaled, claim-guarded, tick-retried approval (crate::signup::provisioning). Routes: crate::routes::signups.
//!
//! Every route is mounted; while `CAREBUS_SIGNUP_ENABLED` is off (read per call, [`Signup::enabled`]) or a part the
//! approve needs is missing, each answers 503 `signup_not_configured`.
//!
//! Licence on login ([`Signup::ensure_login_licence`]): a care-bus login with a `users` row but no recorded member
//! licence gets one ensured once (register tenant, ensure the tenant licence set, the member licence), recorded in
//! `member_licences` inside its tenant context, so it is one licences round per login, not per request.
pub mod config;
pub mod keycloak_admin;
pub mod licences;
pub mod provisioning;
pub mod removal;
pub mod store;
pub mod telegram;
pub mod upstream;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::auth::{SUPER_ADMIN, ServiceClaims};
use crate::error::ApiError;
use crate::signup::config::SignupConfig;
use crate::signup::keycloak_admin::KeycloakAdmin;
use crate::signup::licences::LicencesAdmin;
use crate::state::AppState;

/// The languages a signup can carry (contract C6; the `signup_approvals.locale` CHECK).
pub const SIGNUP_LOCALES: [&str; 8] = ["ar", "de", "en", "es", "fr", "he", "nl", "pl"];
pub const DEFAULT_LOCALE: &str = "en";

/// A caller-supplied language tag -> one of [`SIGNUP_LOCALES`], or `None` (lenient: `de-DE` is German, an unknown
/// language is "not told", never a refused signup).
pub fn normalise_locale(v: Option<&str>) -> Option<&'static str> {
    let v = v?.trim().to_ascii_lowercase();
    let primary = v.split(['-', '_']).next().unwrap_or_default();
    SIGNUP_LOCALES.iter().copied().find(|l| *l == primary)
}

fn forbidden(code: &'static str, why: String) -> ApiError {
    ApiError::Denied(code, why)
}

/// The service-account door shared by the intake and the decider: the token must come from `issuer` (the ONE issuer
/// the door accepts its client from), carry no `super_admin` anywhere, have an allowed `azp`, an `aud` containing that
/// `azp`, and hold the narrow `role` as a REALM role. 403 `code` otherwise.
fn require_service(
    c: &ServiceClaims,
    issuer: &str,
    allowed: &[String],
    role: &str,
    code: &'static str,
    what: &str,
) -> Result<String, ApiError> {
    if c.iss != issuer {
        return Err(forbidden(
            code,
            format!("the token was not issued by the realm the {what} accepts its client from"),
        ));
    }
    if c.has_anywhere(SUPER_ADMIN) {
        return Err(forbidden(
            code,
            format!(
                "the {what} accepts only its dedicated service-account token, never an operator session"
            ),
        ));
    }
    let Some(azp) = c.azp.as_deref().filter(|a| allowed.iter().any(|x| x == a)) else {
        return Err(forbidden(
            code,
            format!("this client is not allowed to use the {what}"),
        ));
    };
    if !c.aud.iter().any(|a| a == azp) {
        return Err(forbidden(
            code,
            format!("the token is not addressed to the {what}"),
        ));
    }
    if role.is_empty() || !c.has(role) {
        return Err(forbidden(
            code,
            format!("role '{role}' is required for the {what}"),
        ));
    }
    Ok(azp.to_string())
}

/// The intake door (`care-bus-signup-intake`, secure-apps). Returns its `azp`.
pub fn require_intake(cfg: &SignupConfig, c: &ServiceClaims) -> Result<String, ApiError> {
    require_service(
        c,
        &cfg.intake_issuer,
        &cfg.intake_azp,
        &cfg.intake_role,
        "signup_intake_forbidden",
        "signup intake",
    )
}

/// The Telegram decision door (C3, `chat-bridge-signup-decider-care-bus` with `care-bus-signup-decide`).
pub fn require_decider(cfg: &SignupConfig, c: &ServiceClaims) -> Result<String, ApiError> {
    require_service(
        c,
        &cfg.decider_issuer,
        &cfg.decider_azp,
        &cfg.decider_role,
        "signup_decider_forbidden",
        "signup decision door",
    )
}

/// Unicode format characters (general category Cf): bidi embeddings, overrides and isolates, LRM/RLM,
/// zero-width characters, the BOM, the soft hyphen and the tag characters. Invisible by design, so
/// requester-typed text drops them.
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR.
fn is_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// One printable line (commonpractice `singleLine`): control characters and line/paragraph separators
/// become spaces, format characters go, whitespace collapses to one space, and the ends are trimmed.
pub fn single_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    for c in v.chars() {
        if is_format(c) {
            continue;
        }
        if c.is_control() || is_separator(c) || c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

/// Printable text keeping its line breaks (commonpractice `multiLine`): CRLF, CR and the line/paragraph
/// separators become `\n`, a tab a space, every other control and format character goes, and the ends
/// are trimmed.
pub fn multi_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if is_separator(c) => out.push('\n'),
            c if c.is_control() || is_format(c) => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

fn invalid(msg: String) -> ApiError {
    ApiError::BadRequest("invalid_body", msg)
}

fn too_long(field: &str, max: usize) -> ApiError {
    invalid(format!("{field} must be at most {max} characters"))
}

/// A requester-typed one-line field: [`single_line`], then 1 to `max` characters (the raw input is
/// bounded at four times that first). Empty after stripping is refused.
pub fn single_line_field(field: &str, v: &str, max: usize) -> Result<String, ApiError> {
    if v.chars().count() > max * 4 {
        return Err(too_long(field, max));
    }
    let t = single_line(v);
    if t.is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if t.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(t)
}

/// A requester-typed multi-line field: [`multi_line`], at most `max` characters; empty after stripping is
/// `None` (as if not given).
pub fn multi_line_field(field: &str, v: &str, max: usize) -> Result<Option<String>, ApiError> {
    if v.chars().count() > max * 4 {
        return Err(too_long(field, max));
    }
    let t = multi_line(v);
    if t.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(Some(t).filter(|t| !t.is_empty()))
}

const MAX_EMAIL: usize = 254;

/// `v` as an email address: trimmed, no control characters or whitespace, at most 254 characters, one `@`
/// with a non-empty local part and a dotted domain. Keycloak checks the rest.
pub fn email_address(field: &str, v: &str) -> Result<String, ApiError> {
    let e = v.trim();
    let ok = !e.is_empty()
        && e.chars().count() <= MAX_EMAIL
        && !e.chars().any(|c| c.is_control() || c.is_whitespace())
        && e.matches('@').count() == 1
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if !ok {
        return Err(invalid(format!("{field} must be an email address")));
    }
    Ok(e.to_string())
}

/// A workspace name (approval `tenantName`): trimmed, no control characters, 2 to 120 characters.
pub fn tenant_name(v: &str) -> Result<String, ApiError> {
    let t = v.trim();
    if t.chars().any(char::is_control) {
        return Err(invalid(
            "tenantName must not contain control characters".into(),
        ));
    }
    let n = t.chars().count();
    if !(2..=120).contains(&n) {
        return Err(invalid("tenantName must be 2 to 120 characters".into()));
    }
    Ok(t.to_string())
}

/// Public mailbox providers, which never name a customer (commonpractice `PUBLIC_EMAIL_DOMAINS`).
pub const PUBLIC_EMAIL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "hotmail.co.uk",
    "live.com",
    "msn.com",
    "yahoo.com",
    "yahoo.co.uk",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "gmx.com",
    "gmx.de",
    "gmx.net",
    "web.de",
    "mail.com",
    "zoho.com",
    "yandex.com",
    "yandex.ru",
    "hey.com",
    "fastmail.com",
    "tutanota.com",
    "tuta.io",
    "qq.com",
    "163.com",
    "ziggo.nl",
    "kpnmail.nl",
    "hetnet.nl",
    "planet.nl",
    "telenet.be",
    "orange.fr",
    "free.fr",
    "laposte.net",
    "t-online.de",
];

/// Second-level labels under a country TLD (`acme.co.uk`).
const SECOND_LEVEL_LABELS: [&str; 9] =
    ["co", "com", "org", "net", "ac", "gov", "edu", "ltd", "plc"];

/// `mail.acme.co.uk` → `acme` (commonpractice `organisationLabel`, no public-suffix list).
pub fn organisation_label(domain: &str) -> String {
    let lower = domain.trim().to_lowercase();
    let mut labels: Vec<&str> = lower.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 1 {
        return labels.first().map(|l| l.to_string()).unwrap_or_default();
    }
    labels.pop();
    if labels.len() > 1
        && labels
            .last()
            .is_some_and(|l| SECOND_LEVEL_LABELS.contains(l))
    {
        labels.pop();
    }
    labels.last().map(|l| l.to_string()).unwrap_or_default()
}

/// A usable workspace name: trimmed, at most 120 characters, at least 2, with a letter or digit.
fn usable(v: Option<&str>) -> Option<String> {
    let t: String = v?.trim().chars().take(120).collect();
    let t = t.trim();
    (t.chars().count() >= 2 && t.chars().any(|c| c.is_ascii_alphanumeric())).then(|| t.to_string())
}

/// The workspace name an approval uses when none is given (commonpractice `defaultTenantNameFrom`): the
/// company; else the organisation of the email domain, unless it is a public mailbox provider; else the
/// requester's name; else the email's local part.
pub fn default_tenant_name(company: Option<&str>, email: &str, name: Option<&str>) -> String {
    if let Some(c) = usable(company) {
        return c;
    }
    let at = email.rfind('@');
    if let Some(i) = at {
        let domain = email[i + 1..].trim().to_lowercase();
        if !domain.is_empty() && !PUBLIC_EMAIL_DOMAINS.contains(&domain.as_str()) {
            let label = organisation_label(&domain);
            if let Some(l) = usable(Some(label.as_str())) {
                return l;
            }
        }
    }
    if let Some(n) = usable(name) {
        return n;
    }
    usable(Some(&email[..at.unwrap_or(0)])).unwrap_or_else(|| "workspace".into())
}

/// A failed login-licence attempt is not retried before this (per login, per process).
const LOGIN_LICENCE_BACKOFF: Duration = Duration::from_secs(300);

/// The signup service: its settings, the per-call switch, the user-realm admin and the licences issuer.
pub struct Signup {
    pub cfg: SignupConfig,
    pub http: reqwest::Client,
    enabled: AtomicBool,
    pub kc: Option<KeycloakAdmin>,
    pub licences: Option<Arc<LicencesAdmin>>,
    /// Logins whose licence is being ensured or failed recently: `sub` -> not before.
    login_inflight: Mutex<HashMap<String, Instant>>,
}

impl Signup {
    pub fn new(cfg: SignupConfig, http: reqwest::Client) -> Self {
        let kc = cfg
            .keycloak_admin
            .as_ref()
            .map(|k| KeycloakAdmin::new(k, http.clone()));
        let licences = cfg
            .licences
            .as_ref()
            .map(|l| Arc::new(LicencesAdmin::new(l, http.clone())));
        Self {
            enabled: AtomicBool::new(cfg.enabled),
            cfg,
            http,
            kc,
            licences,
            login_inflight: Mutex::new(HashMap::new()),
        }
    }

    /// The per-call switch (`CAREBUS_SIGNUP_ENABLED`) and every part the approve needs.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
            && self.kc.is_some()
            && self.licences.is_some()
            && self.cfg.public_url.is_some()
    }

    /// Test seam: flip the switch.
    #[doc(hidden)]
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn log_status(&self) {
        tracing::info!(
            enabled = self.enabled(),
            telegram = self.cfg.telegram.is_some(),
            telegram_missing = ?self.cfg.telegram_missing,
            login_licence = self.cfg.login_licence,
            "early_access_signup"
        );
    }

    /// The public URL (set whenever [`Signup::enabled`]).
    pub fn public_url(&self) -> &str {
        self.cfg.public_url.as_deref().unwrap_or_default()
    }

    /// Licence on login: when on and this login (`sub`, home `tenant`) has no recorded member licence, ensure one in
    /// the background, at most one attempt in flight per login and a failed one not retried for 5 minutes. Never
    /// delays or fails the request.
    pub fn ensure_login_licence(&self, st: &AppState, sub: &str, tenant: Uuid) {
        if !self.cfg.login_licence {
            return;
        }
        let Some(lic) = self.licences.clone() else {
            return;
        };
        {
            let mut g = self
                .login_inflight
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            g.retain(|_, until| *until > now);
            if g.contains_key(sub) {
                return;
            }
            g.insert(sub.to_string(), now + LOGIN_LICENCE_BACKOFF);
        }
        let st = st.clone();
        let sub = sub.to_string();
        tokio::spawn(async move {
            let res = provisioning::ensure_member_licence(&st, &lic, tenant, &sub).await;
            let ok = res.is_ok();
            match res {
                Ok(id) => tracing::info!(%tenant, license_id = %id, "login_member_licence_ensured"),
                Err(e) => tracing::warn!(%tenant, error = %e, "login_member_licence_failed"),
            }
            if ok {
                let mut g = st
                    .signup
                    .login_inflight
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                g.remove(&sub);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CB: &str = "https://kc.test/realms/care-bus";
    const SECURE_APPS: &str = "https://kc.test/realms/secure-apps";

    fn claims(iss: &str, azp: Option<&str>, aud: &[&str], roles: &[&str]) -> ServiceClaims {
        ServiceClaims {
            iss: iss.into(),
            sub: "sa".into(),
            azp: azp.map(String::from),
            aud: aud.iter().map(|s| s.to_string()).collect(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            client_roles: vec![],
        }
    }

    #[test]
    fn service_doors_check_issuer_azp_aud_role_and_refuse_super_admin() {
        let allowed = vec!["care-bus-signup-intake".to_string()];
        let role = "care-bus-signup-intake";
        let c = "care-bus-signup-intake";
        let ok = claims(SECURE_APPS, Some(c), &[c], &[role]);
        assert_eq!(
            require_service(&ok, SECURE_APPS, &allowed, role, "x", "door").unwrap(),
            c
        );
        for bad in [
            claims(CB, Some(c), &[c], &[role]),
            claims(SECURE_APPS, Some(c), &[c], &[role, "super_admin"]),
            claims(
                SECURE_APPS,
                Some("cp-signup-intake"),
                &["cp-signup-intake"],
                &[role],
            ),
            claims(SECURE_APPS, None, &[c], &[role]),
            claims(SECURE_APPS, Some(c), &["care-bus"], &[role]),
            claims(SECURE_APPS, Some(c), &[c], &["other"]),
            ServiceClaims {
                client_roles: vec![role.to_string()],
                ..claims(SECURE_APPS, Some(c), &[c], &[])
            },
            ServiceClaims {
                client_roles: vec!["super_admin".to_string()],
                ..ok.clone()
            },
        ] {
            let e = require_service(
                &bad,
                SECURE_APPS,
                &allowed,
                role,
                "signup_intake_forbidden",
                "door",
            )
            .unwrap_err();
            assert_eq!(e.code(), "signup_intake_forbidden");
            assert_eq!(e.status_and_code().0, axum::http::StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn locales_are_lenient_and_closed() {
        assert_eq!(normalise_locale(Some("de-DE")), Some("de"));
        assert_eq!(normalise_locale(Some(" NL ")), Some("nl"));
        assert_eq!(normalise_locale(Some("pt_BR")), None);
        assert_eq!(normalise_locale(Some("")), None);
        assert_eq!(normalise_locale(None), None);
    }

    #[test]
    fn single_line_strips_controls_and_format_characters() {
        assert_eq!(single_line("  Ann\nEvil\r\n Corp  "), "Ann Evil Corp");
        assert_eq!(single_line("A\u{202E}nn\u{200B}"), "Ann");
        assert_eq!(single_line("a\u{2028}b\u{00A0}\u{3000}c\td"), "a b c d");
        assert!(single_line_field("name", "\u{200B} \n", 160).is_err());
        assert!(single_line_field("name", &"x".repeat(161), 160).is_err());
    }

    #[test]
    fn multi_line_keeps_line_breaks_only() {
        assert_eq!(
            multi_line(" Hi\r\nthere\rand\u{2029}more\tok\u{0007}\u{202E} "),
            "Hi\nthere\nand\nmore ok"
        );
        assert_eq!(
            multi_line_field("message", " \u{200B} ", 2000).unwrap(),
            None
        );
        assert!(multi_line_field("message", &"x".repeat(2001), 2000).is_err());
    }

    #[test]
    fn emails_and_names_validate() {
        assert_eq!(email_address("email", " a@b.co ").unwrap(), "a@b.co");
        for bad in [
            "", "a", "a@b", "@b.co", "a@@b.co", "a b@c.co", "a@.co", "a@co.",
        ] {
            assert!(email_address("email", bad).is_err(), "{bad:?}");
        }
        assert!(tenant_name("x").is_err());
        assert!(tenant_name("a\u{7}b").is_err());
        assert_eq!(tenant_name(" ab ").unwrap(), "ab");
    }

    #[test]
    fn default_names_follow_commonpractice() {
        assert_eq!(
            default_tenant_name(Some("Globex"), "ann@globex.com", Some("Ann")),
            "Globex"
        );
        assert_eq!(
            default_tenant_name(None, "ann@mail.acme.co.uk", Some("Ann")),
            "acme"
        );
        assert_eq!(
            default_tenant_name(Some(" "), "ann.early@gmail.com", Some("Ann Early")),
            "Ann Early"
        );
        assert_eq!(
            default_tenant_name(None, "ann.early@gmail.com", Some("A")),
            "ann.early"
        );
        assert_eq!(default_tenant_name(None, "a@gmail.com", None), "workspace");
    }
}
