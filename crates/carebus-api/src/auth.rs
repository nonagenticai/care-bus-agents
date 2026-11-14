//! Keycloak JWT verification, the resolved caller, and read-only service tokens.
//!
//! Keycloak only authenticates. The one role care-bus reads is a client role of the `care-bus` client
//! (`resource_access[<audience>].roles`): `super_admin`; `realm_access` is never read. A person's
//! tenant and tenant role come from care-bus's own `users` table, through the `carebus_auth_user` definer function.
use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// The JWKS is cached this long.
const JWKS_TTL: Duration = Duration::from_secs(600);
/// An unknown `kid` re-fetches the JWKS (key rotation) at most this often.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);

/// Client role: acts in any tenant (with `X-Active-Tenant-Id`) as its admin, and may provision.
pub const SUPER_ADMIN: &str = "super_admin";

#[derive(Clone, Debug)]
pub struct Claims {
    pub sub: String,
    pub preferred_username: Option<String>,
    /// `resource_access[<our audience>].roles` only.
    pub client_roles: Vec<String>,
    /// A static service token (`CAREBUS_API_SERVICE_TOKENS`), never a person. Read-only everywhere.
    pub service: bool,
}
impl Claims {
    pub fn actor(&self) -> String {
        self.preferred_username
            .clone()
            .unwrap_or_else(|| self.sub.clone())
    }
    pub fn has(&self, role: &str) -> bool {
        self.client_roles.iter().any(|r| r == role)
    }
}

/// A service-account token after signature, issuer and expiry checks; the signup door's guard decides the rest
/// (crate::signup::require_intake / require_decider).
#[derive(Clone, Debug)]
pub struct ServiceClaims {
    /// The VERIFIED issuer (one of `KEYCLOAK_TRUSTED_ISSUERS`): each door pins the one it accepts.
    pub iss: String,
    pub sub: String,
    pub azp: Option<String>,
    pub aud: Vec<String>,
    /// REALM roles only (`realm_access.roles`): the door roles are realm roles; a client role of the same name never
    /// opens a door.
    pub roles: Vec<String>,
    /// Every client's roles. Read ONLY to refuse an operator role anywhere in the token.
    pub client_roles: Vec<String>,
}
impl ServiceClaims {
    pub fn has(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
    pub fn has_anywhere(&self, role: &str) -> bool {
        self.has(role) || self.client_roles.iter().any(|r| r == role)
    }
}

/// `aud` is a string or an array of strings.
#[derive(Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}
impl Aud {
    fn into_vec(self) -> Vec<String> {
        match self {
            Aud::One(s) => vec![s],
            Aud::Many(v) => v,
        }
    }
}

#[derive(Deserialize)]
struct RawClaims {
    sub: String,
    preferred_username: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    aud: Option<Aud>,
    #[serde(default)]
    resource_access: HashMap<String, ClientAccess>,
    #[serde(default)]
    realm_access: ClientAccess,
}
#[derive(Deserialize, Default)]
struct ClientAccess {
    #[serde(default)]
    roles: Vec<String>,
}

/// A person's role in their tenant (`users.tenant_role`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantRole {
    TenantAdmin,
    TenantUser,
}
impl TenantRole {
    pub fn as_str(self) -> &'static str {
        match self {
            TenantRole::TenantAdmin => "tenant_admin",
            TenantRole::TenantUser => "tenant_user",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tenant_admin" => Some(TenantRole::TenantAdmin),
            "tenant_user" => Some(TenantRole::TenantUser),
            _ => None,
        }
    }
}

/// A provisioned person's home tenant.
#[derive(Clone, Debug)]
pub struct Home {
    pub tenant: Uuid,
    pub role: TenantRole,
    pub suspended: bool,
}

#[derive(Clone, Debug)]
pub enum CallerKind {
    /// A Keycloak user. `home` is `None` when care-bus has no `users` row for the subject.
    Person {
        home: Option<Home>,
        super_admin: bool,
    },
    /// A static service token: read-only machine caller (procedures only), names its tenant with the header.
    Service,
}

/// Who is calling, before any tenant is chosen (crate::tenancy picks the tenant).
#[derive(Clone, Debug)]
pub struct Caller {
    pub sub: String,
    /// `preferred_username`, else `sub`: what the audit trail records.
    pub actor: String,
    pub kind: CallerKind,
}
impl Caller {
    pub fn is_super_admin(&self) -> bool {
        matches!(
            self.kind,
            CallerKind::Person {
                super_admin: true,
                ..
            }
        )
    }
    pub fn home(&self) -> Option<&Home> {
        match &self.kind {
            CallerKind::Person { home, .. } => home.as_ref(),
            _ => None,
        }
    }
    /// `audit_events.actor_type` for this caller.
    pub fn actor_type(&self) -> &'static str {
        match self.kind {
            CallerKind::Person { .. } => "human",
            CallerKind::Service => "system",
        }
    }
}

/// Parsed keys by `kid`, plus when we last fetched successfully and when we last tried at all.
#[derive(Default)]
struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    /// Last successful fetch; the cache is fresh for `JWKS_TTL` after it.
    fetched: Option<Instant>,
    /// Last fetch attempt, successful or not. The 30 s floor applies to this, so a Keycloak outage is
    /// hit once per 30 s in total instead of once per request.
    last_attempt: Option<Instant>,
    last_failed: bool,
}

enum Keys {
    Jwks {
        url: String,
        http: reqwest::Client,
        cache: Mutex<JwksCache>,
    },
    Static(DecodingKey),
}

/// One trusted issuer: its exact `iss`, whether its realm is service-only, and its own key source.
struct Trusted {
    issuer: String,
    service_only: bool,
    keys: Keys,
}

/// Realm per app (agent A1): a token's UNVERIFIED `iss` selects one entry of `KEYCLOAK_TRUSTED_ISSUERS`; an `iss`
/// that is not listed is 401 before any key lookup. Each issuer has its own JWKS cache and the token is validated with
/// `iss` pinned to exactly that issuer, so one realm's key never vouches for another realm's `iss`. A realm in
/// `KEYCLOAK_SERVICE_ONLY_REALMS` (secure-apps) verifies only as a service account (the signup doors).
pub struct Verifier {
    audience: String,
    issuers: Vec<Trusted>,
}

/// The `iss` of a token, read WITHOUT verifying it: only to pick which trusted issuer validates it.
fn unverified_iss(token: &str) -> Option<String> {
    use base64::Engine;
    #[derive(Deserialize)]
    struct Iss {
        iss: Option<String>,
    }
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<Iss>(&bytes).ok()?.iss
}

fn jwks_keys(issuer: &str, http: reqwest::Client) -> Keys {
    Keys::Jwks {
        url: format!(
            "{}/protocol/openid-connect/certs",
            issuer.trim_end_matches('/')
        ),
        http,
        cache: Mutex::new(JwksCache::default()),
    }
}

impl Verifier {
    /// One issuer (`<issuer>/protocol/openid-connect/certs`), not service-only.
    pub fn jwks(issuer: &str, audience: &str, http: reqwest::Client) -> Self {
        Self::trusted(
            audience,
            &[],
            vec![(issuer.to_string(), jwks_keys(issuer, http))],
        )
    }

    /// `KEYCLOAK_TRUSTED_ISSUERS`, each against its own JWKS.
    pub fn jwks_many(
        trusted: &[String],
        service_only_realms: &[String],
        audience: &str,
        http: reqwest::Client,
    ) -> Self {
        let entries = trusted
            .iter()
            .map(|iss| (iss.clone(), jwks_keys(iss, http.clone())))
            .collect();
        Self::trusted(audience, service_only_realms, entries)
    }

    /// HS256 with a fixed secret. Reachable only through `AppState::build_for_tests`.
    pub fn test_key(issuer: &str, audience: &str, secret: &[u8]) -> Self {
        Self::test_key_many(&[issuer.to_string()], &[], audience, secret)
    }

    /// [`Verifier::test_key`] for several issuers (one shared HS256 secret).
    pub fn test_key_many(
        trusted: &[String],
        service_only_realms: &[String],
        audience: &str,
        secret: &[u8],
    ) -> Self {
        let entries = trusted
            .iter()
            // jsonwebtoken 12 makes `from_secret` fallible: an empty HMAC secret is now a
            // hard error (InvalidKeyFormat). This helper is test-only and the secret is a
            // literal, so panicking with the reason is the honest failure mode.
            .map(|iss| {
                (
                    iss.clone(),
                    Keys::Static(
                        DecodingKey::from_secret(secret)
                            .expect("test HMAC secret must not be empty"),
                    ),
                )
            })
            .collect();
        Self::trusted(audience, service_only_realms, entries)
    }

    fn trusted(
        audience: &str,
        service_only_realms: &[String],
        entries: Vec<(String, Keys)>,
    ) -> Self {
        let issuers = entries
            .into_iter()
            .map(|(issuer, keys)| Trusted {
                service_only: crate::config::realm_of_issuer(&issuer)
                    .is_some_and(|r| service_only_realms.iter().any(|s| s == r)),
                issuer: issuer.trim_end_matches('/').to_string(),
                keys,
            })
            .collect();
        Self {
            audience: audience.into(),
            issuers,
        }
    }

    /// The claims, validated against EXACTLY the trusted issuer its `iss` names, and that issuer.
    async fn decode_raw(
        &self,
        token: &str,
        check_audience: bool,
        allow_service_only: bool,
    ) -> Result<(RawClaims, &str), ApiError> {
        let header = decode_header(token).map_err(|_| ApiError::Unauthorized)?;
        let iss = unverified_iss(token).ok_or(ApiError::Unauthorized)?;
        let t = self
            .issuers
            .iter()
            .find(|t| t.issuer == iss)
            .ok_or(ApiError::Unauthorized)?;
        if t.service_only && !allow_service_only {
            return Err(ApiError::Unauthorized);
        }
        let (key, alg) = match &t.keys {
            Keys::Static(k) => (k.clone(), Algorithm::HS256),
            Keys::Jwks { url, http, cache } => {
                let kid = header.kid.as_deref().ok_or(ApiError::Unauthorized)?;
                (Self::jwk(url, http, cache, kid).await?, Algorithm::RS256)
            }
        };
        let mut v = Validation::new(alg);
        // jsonwebtoken checks `iss`/`aud` only when present: make them (and `exp`, `sub`) mandatory, so a token
        // minted for another client without an `aud` cannot pass, and one without `exp` never verifies.
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.set_issuer(&[&t.issuer]);
        if check_audience {
            v.set_audience(&[&self.audience]);
        } else {
            v.validate_aud = false;
        }
        v.leeway = 5;
        let raw = decode::<RawClaims>(token, &key, &v)
            .map_err(|_| ApiError::Unauthorized)?
            .claims;
        if raw.sub.trim().is_empty() {
            return Err(ApiError::Unauthorized);
        }
        Ok((raw, t.issuer.as_str()))
    }

    /// A person's token: `aud` must contain our audience; a service-only realm is 401.
    pub async fn verify(&self, token: &str) -> Result<Claims, ApiError> {
        let (mut raw, _) = self.decode_raw(token, true, false).await?;
        let client_roles = raw
            .resource_access
            .remove(&self.audience)
            .map(|a| a.roles)
            .unwrap_or_default();
        Ok(Claims {
            sub: raw.sub,
            preferred_username: raw.preferred_username,
            client_roles,
            service: false,
        })
    }

    /// A service-account token: signature, a trusted issuer (service-only realms included), expiry and a present
    /// `aud`; the door's guard checks the rest, the issuer pin included.
    pub async fn verify_service(&self, token: &str) -> Result<ServiceClaims, ApiError> {
        let (raw, iss) = self.decode_raw(token, false, true).await?;
        let iss = iss.to_string();
        let aud = raw.aud.map(Aud::into_vec).unwrap_or_default();
        if aud.is_empty() {
            return Err(ApiError::Unauthorized);
        }
        let client_roles = raw
            .resource_access
            .into_values()
            .flat_map(|a| a.roles)
            .collect();
        Ok(ServiceClaims {
            iss,
            sub: raw.sub,
            azp: raw.azp.filter(|a| !a.is_empty()),
            aud,
            roles: raw.realm_access.roles,
            client_roles,
        })
    }

    /// JWKS cached 10 minutes. A stale cache or an unknown `kid` triggers a fetch (key rotation), but
    /// never more than one attempt per 30 s (failures count), so neither garbage `kid`s nor a Keycloak
    /// outage can turn this service into a hammer. If a fetch fails, keys already held keep verifying
    /// (stale beats down); 503 only when no usable key exists.
    async fn jwk(
        url: &str,
        http: &reqwest::Client,
        cache: &Mutex<JwksCache>,
        kid: &str,
    ) -> Result<DecodingKey, ApiError> {
        // The lock is held across the fetch on purpose: concurrent callers wait for one attempt and
        // then reuse its outcome (the floor below stops them from fetching again).
        let mut c = cache.lock().await;
        let fresh = c.fetched.is_some_and(|t| t.elapsed() < JWKS_TTL);
        let known = c.keys.contains_key(kid);
        if !(fresh && known)
            && c.last_attempt
                .is_none_or(|t| t.elapsed() >= JWKS_MIN_REFRESH)
        {
            c.last_attempt = Some(Instant::now());
            match fetch(url, http).await {
                Ok(keys) => {
                    c.keys = keys;
                    c.fetched = Some(Instant::now());
                    c.last_failed = false;
                }
                Err(_) => c.last_failed = true,
            }
        }
        if let Some(k) = c.keys.get(kid) {
            return Ok(k.clone());
        }
        // Unknown kid and the last fetch failed: we cannot tell "rotated" from "forged", so say 503.
        if c.last_failed {
            return Err(ApiError::Upstream("keycloak"));
        }
        Err(ApiError::Unauthorized)
    }
}

async fn fetch(
    url: &str,
    http: &reqwest::Client,
) -> Result<HashMap<String, DecodingKey>, ApiError> {
    let resp = http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|_| ApiError::Upstream("keycloak"))?;
    let set: JwkSet = resp
        .json()
        .await
        .map_err(|_| ApiError::Upstream("keycloak"))?;
    Ok(set
        .keys
        .iter()
        .filter_map(|j| {
            let kid = j.common.key_id.clone()?;
            Some((kid, DecodingKey::from_jwk(j).ok()?))
        })
        .collect())
}

/// Constant-time membership test, so a timing probe cannot learn a service token byte by byte.
/// Every configured token is compared (no early exit); a length mismatch is not secret.
fn is_service_token(tokens: &[String], presented: &str) -> bool {
    let mut hit = 0u8;
    for t in tokens {
        hit |= t.as_bytes().ct_eq(presented.as_bytes()).unwrap_u8();
    }
    hit == 1
}

async fn claims(parts: &Parts, st: &AppState) -> Result<Claims, ApiError> {
    let h = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;
    let token = h.strip_prefix("Bearer ").ok_or(ApiError::Unauthorized)?;
    // Service callers (plan 03 tools, plan 01 HttpProcedures): static bearer, read-only.
    if is_service_token(&st.cfg.service_tokens, token) {
        return Ok(Claims {
            sub: "service".into(),
            preferred_username: Some("service".into()),
            client_roles: vec![],
            service: true,
        });
    }
    st.auth.verify(token).await
}

/// Token → caller. A person's home tenant is read through `carebus_auth_user` (SECURITY DEFINER), the one read of
/// `users` that needs no tenant GUC.
async fn resolve(parts: &Parts, st: &AppState) -> Result<Caller, ApiError> {
    let c = claims(parts, st).await?;
    let actor = c.actor();
    if c.service {
        return Ok(Caller {
            sub: c.sub,
            actor,
            kind: CallerKind::Service,
        });
    }
    let row: Option<(Uuid, String, String, bool)> = sqlx::query_as(
        "SELECT tenant_id, tenant_role, tenant_status, member_licence FROM carebus_auth_user($1)",
    )
    .bind(&c.sub)
    .fetch_optional(&st.db)
    .await?;
    let super_admin = c.has(SUPER_ADMIN);
    // Licence on login (care-bus 17): a login of an active tenant whose member licence is not recorded yet gets one
    // ensured in the background (crate::signup::Signup::ensure_login_licence); the request never waits for it.
    if let Some((tenant, _, status, false)) = &row
        && status == "active"
    {
        st.signup.ensure_login_licence(st, &c.sub, *tenant);
    }
    let home = match row {
        None => None,
        Some((tenant, role, status, _)) => Some(Home {
            tenant,
            role: TenantRole::parse(&role).ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!("users.tenant_role has an unknown value"))
            })?,
            suspended: status != "active",
        }),
    };
    Ok(Caller {
        sub: c.sub,
        actor,
        kind: CallerKind::Person { home, super_admin },
    })
}

/// Any verified caller. Resolved once per request (cached in the request extensions), so a handler that takes a
/// tenant guard as well does not look the subject up twice.
impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        if let Some(c) = parts.extensions.get::<Caller>() {
            return Ok(c.clone());
        }
        let c = resolve(parts, st).await?;
        parts.extensions.insert(c.clone());
        Ok(c)
    }
}

/// Any token from a trusted issuer, read as a service account (the signup door's guard decides, issuer pin included).
impl FromRequestParts<AppState> for ServiceClaims {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .ok_or(ApiError::Unauthorized)?;
        st.auth.verify_service(token).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn service_token_match_is_exact() {
        let t = vec!["abc".to_string(), "def".to_string()];
        assert!(is_service_token(&t, "abc"));
        assert!(is_service_token(&t, "def"));
        assert!(!is_service_token(&t, "ab"));
        assert!(!is_service_token(&t, "abcd"));
        assert!(!is_service_token(&t, ""));
        assert!(!is_service_token(&[], ""));
    }

    use std::sync::LazyLock;

    /// Test keypair generated once per test binary (RSA keygen is slow); nothing secret is committed.
    struct TestKey {
        pem: String,
        /// base64url (no padding) modulus, as a JWKS `n`.
        n: String,
    }

    fn b64url(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let v = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..=c.len() {
                out.push(T[((v >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }

    fn gen_key() -> TestKey {
        use rsa::Generate;
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::traits::PublicKeyParts;
        let k = rsa::PrivateKey::generate();
        TestKey {
            pem: k
                .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
                .unwrap()
                .to_string(),
            n: b64url(&k.n_bytes()),
        }
    }

    static KEY_A_GEN: LazyLock<TestKey> = LazyLock::new(gen_key);
    static KEY_B_GEN: LazyLock<TestKey> = LazyLock::new(gen_key);
    static KEY_A: LazyLock<&'static str> = LazyLock::new(|| KEY_A_GEN.pem.as_str());
    static KEY_B: LazyLock<&'static str> = LazyLock::new(|| KEY_B_GEN.pem.as_str());

    struct Stub {
        issuer: String,
        hits: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
    }

    /// A JWKS endpoint serving `jwks`; counts hits; answers 500 while `fail` is set.
    async fn stub_jwks(jwks: serde_json::Value) -> Stub {
        let hits = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let (h, f) = (hits.clone(), fail.clone());
        let app = axum::Router::new().route(
            "/realms/portal/protocol/openid-connect/certs",
            axum::routing::get(move || {
                let (h, f, jwks) = (h.clone(), f.clone(), jwks.clone());
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    if f.load(Ordering::SeqCst) {
                        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
                            .into_response();
                    }
                    axum::Json(jwks).into_response()
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}/realms/portal", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Stub { issuer, hits, fail }
    }

    fn empty() -> serde_json::Value {
        serde_json::json!({"keys": []})
    }
    fn jwks_a() -> serde_json::Value {
        serde_json::json!({"keys": [{"kty":"RSA","kid":"k1","use":"sig","alg":"RS256","n":KEY_A_GEN.n.as_str(),"e":"AQAB"}]})
    }

    fn rs256(pem: &str, kid: &str, iss: &str, aud: &str) -> String {
        let mut h = jsonwebtoken::Header::new(Algorithm::RS256);
        h.kid = Some(kid.into());
        let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
        jsonwebtoken::encode(
            &h,
            &serde_json::json!({"sub":"u-9","preferred_username":"j.devries","exp":exp,"iss":iss,"aud":aud,
                "realm_access":{"roles":["care-admin"]},
                "resource_access":{aud:{"roles":["super_admin"]},"other-client":{"roles":["other-role"]}}}),
            &jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    /// A token naming `v`'s (first) issuer, so it reaches that issuer's key lookup.
    fn token_with_kid(v: &Verifier, kid: &str) -> String {
        // The key lookup fails before the signature matters, so any signing key will do.
        let mut h = jsonwebtoken::Header::new(Algorithm::HS256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(
            &h,
            &serde_json::json!({"sub": "x", "iss": v.issuers[0].issuer}),
            &jsonwebtoken::EncodingKey::from_secret(b"k").expect("literal test secret"),
        )
        .unwrap()
    }

    /// Pretend the last fetch (and attempt) happened `secs` ago.
    async fn age(v: &Verifier, secs: u64) {
        if let Keys::Jwks { cache, .. } = &v.issuers[0].keys {
            let mut c = cache.lock().await;
            let t = Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .unwrap();
            c.fetched = Some(t);
            c.last_attempt = Some(t);
        }
    }

    #[tokio::test]
    async fn rs256_token_signed_by_the_jwks_key_verifies() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(&s.issuer, "aud", reqwest::Client::new());
        let c = v
            .verify(&rs256(*KEY_A, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        assert_eq!(c.sub, "u-9");
        assert_eq!(c.actor(), "j.devries");
        // only our audience's client roles; realm roles and other clients' roles are never read
        assert_eq!(c.client_roles, vec!["super_admin".to_string()]);
        // the parsed key is cached: a second token does not re-fetch
        v.verify(&rs256(*KEY_A, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // wrong audience still refused on the production path
        let r = v.verify(&rs256(*KEY_A, "k1", &s.issuer, "other")).await;
        assert!(matches!(r, Err(ApiError::Unauthorized)));
    }

    #[tokio::test]
    async fn rs256_unknown_kid_or_foreign_signature_is_401() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(&s.issuer, "aud", reqwest::Client::new());
        let r = v.verify(&rs256(*KEY_A, "nope", &s.issuer, "aud")).await;
        assert!(matches!(r, Err(ApiError::Unauthorized)));
        // right kid, signed by a different keypair
        let r = v.verify(&rs256(*KEY_B, "k1", &s.issuer, "aud")).await;
        assert!(matches!(r, Err(ApiError::Unauthorized)));
        // control: the real key is accepted by the same verifier
        assert!(
            v.verify(&rs256(*KEY_A, "k1", &s.issuer, "aud"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unknown_kid_refreshes_jwks_at_most_once_per_30s() {
        let s = stub_jwks(empty()).await;
        let v = Verifier::jwks(&s.issuer, "aud", reqwest::Client::new());
        for i in 0..5 {
            let r = v.verify(&token_with_kid(&v, &format!("rotated-{i}"))).await;
            assert!(matches!(r, Err(ApiError::Unauthorized)));
        }
        // 1 initial fetch; the unknown kids right after must not re-fetch
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        // 30 s later an unknown kid is allowed to refresh once
        age(&v, 31).await;
        let _ = v.verify(&token_with_kid(&v, "rotated-late")).await;
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
        let _ = v.verify(&token_with_kid(&v, "rotated-late-2")).await;
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn keycloak_outage_is_hit_once_not_once_per_request() {
        let s = stub_jwks(empty()).await;
        let v = Arc::new(Verifier::jwks(&s.issuer, "aud", reqwest::Client::new()));
        v.verify(&token_with_kid(&v, "warm")).await.unwrap_err();
        assert_eq!(s.hits.load(Ordering::SeqCst), 1);
        s.fail.store(true, Ordering::SeqCst);
        age(&v, 31).await;
        let tasks: Vec<_> = (0..5)
            .map(|i| {
                let v = v.clone();
                tokio::spawn(async move { v.verify(&token_with_kid(&v, &format!("n{i}"))).await })
            })
            .collect();
        for t in tasks {
            // the cache holds no such kid and the last attempt failed: 503, not 401
            assert!(matches!(
                t.await.unwrap(),
                Err(ApiError::Upstream("keycloak"))
            ));
        }
        // 1 (warm) + exactly 1 failed attempt shared by all 5 waiters
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_keys_keep_verifying_through_an_outage() {
        let s = stub_jwks(jwks_a()).await;
        let v = Verifier::jwks(&s.issuer, "aud", reqwest::Client::new());
        v.verify(&rs256(*KEY_A, "k1", &s.issuer, "aud"))
            .await
            .unwrap();
        s.fail.store(true, Ordering::SeqCst);
        age(&v, 601).await; // past the TTL
        for _ in 0..3 {
            let c = v
                .verify(&rs256(*KEY_A, "k1", &s.issuer, "aud"))
                .await
                .unwrap();
            assert_eq!(c.sub, "u-9");
        }
        // 1 warm-up + 1 failed refresh; the next two requests reuse the failure
        assert_eq!(s.hits.load(Ordering::SeqCst), 2);
    }

    /// Realm per app: each trusted issuer verifies with its own key and its own `iss`; an unlisted `iss` is 401; a
    /// service-only realm verifies only as a service account.
    #[tokio::test]
    async fn issuers_pin_their_own_iss_and_service_only_realms_are_doors_only() {
        let cb = "https://kc.test/realms/care-bus".to_string();
        let sa = "https://kc.test/realms/secure-apps".to_string();
        let v = Verifier::test_key_many(
            &[cb.clone(), sa.clone()],
            &["secure-apps".into()],
            "care-bus",
            b"k",
        );
        let tok = |iss: &str, aud: &str| {
            let exp = (chrono::Utc::now() + chrono::Duration::minutes(5)).timestamp();
            jsonwebtoken::encode(
                &jsonwebtoken::Header::default(),
                &serde_json::json!({"sub": "s", "iss": iss, "aud": aud, "exp": exp, "azp": "c",
                                    "realm_access": {"roles": ["r"]}}),
                &jsonwebtoken::EncodingKey::from_secret(b"k").expect("literal test secret"),
            )
            .unwrap()
        };
        assert!(v.verify(&tok(&cb, "care-bus")).await.is_ok());
        assert!(matches!(
            v.verify(&tok(&sa, "care-bus")).await,
            Err(ApiError::Unauthorized)
        ));
        let s = v.verify_service(&tok(&sa, "c")).await.unwrap();
        assert_eq!(
            (s.iss.as_str(), s.azp.as_deref(), s.has("r")),
            (sa.as_str(), Some("c"), true)
        );
        assert!(matches!(
            v.verify_service(&tok("https://kc.test/realms/other", "c"))
                .await,
            Err(ApiError::Unauthorized)
        ));
        // no exp: never verifies
        let no_exp = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &serde_json::json!({"sub": "s", "iss": sa, "aud": "c"}),
            &jsonwebtoken::EncodingKey::from_secret(b"k").expect("literal test secret"),
        )
        .unwrap();
        assert!(v.verify_service(&no_exp).await.is_err());
    }

    #[tokio::test]
    async fn keycloak_down_is_503_not_401() {
        let v = Verifier::jwks(
            "http://127.0.0.1:1/realms/portal",
            "aud",
            reqwest::Client::new(),
        );
        let r = v.verify(&token_with_kid(&v, "k")).await;
        assert!(matches!(r, Err(ApiError::Upstream("keycloak"))));
    }
}
