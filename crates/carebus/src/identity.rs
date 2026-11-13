//! Who care-bus is to the platform services it calls on a tenant's behalf: inference-router (the platform model)
//! and strata_rag. inference-router requires `X-App-Id` + `X-Project-Id` (and `X-Actor-Id`) next to
//! `X-Effective-Tenant-Id` before it arms enforce mode (then a missing one is HTTP 400), and bills app and actor
//! from them. A tenant's own model provider gets none of these.
use anyhow::{Result, bail};
use uuid::Uuid;

/// care-bus' key in the licences registry, sent as `X-App-Id`.
pub const APP_ID: &str = "care-bus";

/// Lowercase: a `&'static str` header name must be (rig's and reqwest's `HeaderName::from_static` panics on any
/// uppercase letter).
pub const APP_ID_HEADER: &str = "x-app-id";
pub const PROJECT_ID_HEADER: &str = "x-project-id";
pub const ACTOR_ID_HEADER: &str = "x-actor-id";

/// care-bus' own Keycloak service-account `sub`, the actor of a call no person is behind.
pub const SERVICE_ACTOR_ENV: &str = "SERVICE_ACTOR_ID";

/// The tenant-level project id of `tenant` for a project-less app: the 32 hex digits of the tenant uuid reversed,
/// read as a uuid (as strata_rag's `metering::tenant_level_project_id`). Its own inverse.
pub fn tenant_level_project_id(tenant: Uuid) -> Uuid {
    let b = tenant.as_u128().swap_bytes();
    let lo = 0x0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_u128;
    Uuid::from_u128(((b & lo) << 4) | ((b >> 4) & lo))
}

/// The headers of a platform-model call made for `tenant`: `X-Effective-Tenant-Id`, `X-App-Id`, `X-Project-Id`
/// and, when `actor` is a non-nil uuid, `X-Actor-Id`. Names lowercase, values as sent.
pub fn router_headers(tenant: Uuid, actor: Option<Uuid>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        (
            crate::interaction::EFFECTIVE_TENANT_HEADER,
            tenant.to_string(),
        ),
        (APP_ID_HEADER, APP_ID.to_string()),
        (
            PROJECT_ID_HEADER,
            tenant_level_project_id(tenant).to_string(),
        ),
    ];
    if let Some(a) = actor.filter(|a| !a.is_nil()) {
        h.push((ACTOR_ID_HEADER, a.to_string()));
    }
    h
}

/// A person's subject as an actor: a non-nil uuid (a Keycloak user or service account), anything else none.
pub fn human_actor(sub: &str) -> Option<Uuid> {
    Uuid::parse_str(sub.trim()).ok().filter(|u| !u.is_nil())
}

/// The actor of a call: the person behind it, else care-bus' service account, else none. Never a nil uuid.
pub fn pick_actor(human: Option<Uuid>, service: Option<Uuid>) -> Option<Uuid> {
    human
        .filter(|u| !u.is_nil())
        .or(service.filter(|u| !u.is_nil()))
}

/// [`SERVICE_ACTOR_ENV`]: unset or blank is `None`; anything but a hyphenated, non-nil uuid is an error naming the
/// variable, so a typo stops startup instead of sending a wrong or no actor.
pub fn parse_service_actor(raw: Option<&str>) -> Result<Option<Uuid>> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    match Uuid::parse_str(raw) {
        Ok(u) if !u.is_nil() && raw.len() == 36 => Ok(Some(u)),
        _ => bail!("{SERVICE_ACTOR_ENV} must be a hyphenated, non-nil uuid (a Keycloak sub)"),
    }
}

/// [`parse_service_actor`] of the process environment.
pub fn service_actor_from_env() -> Result<Option<Uuid>> {
    parse_service_actor(std::env::var(SERVICE_ACTOR_ENV).ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    #[test]
    fn the_project_id_is_the_tenant_hex_reversed() {
        // the fixed vector strata_rag's metering tests use
        assert_eq!(
            tenant_level_project_id(u("0195f48b-d000-7000-8000-000000000100")),
            u("00100000-0000-0008-0007-000db84f5910")
        );
        for _ in 0..64 {
            let t = Uuid::new_v4();
            assert_eq!(tenant_level_project_id(tenant_level_project_id(t)), t);
            let hex: String = t.simple().to_string().chars().rev().collect();
            assert_eq!(tenant_level_project_id(t).simple().to_string(), hex);
        }
    }

    #[test]
    fn the_router_gets_exactly_the_four_headers() {
        let tenant = u("0195f48b-d000-7000-8000-000000000100");
        let actor = u("7f1c9a2e-0000-4000-8000-00000000ac70");
        assert_eq!(
            router_headers(tenant, Some(actor)),
            vec![
                (
                    "x-effective-tenant-id",
                    "0195f48b-d000-7000-8000-000000000100".to_string()
                ),
                ("x-app-id", "care-bus".to_string()),
                (
                    "x-project-id",
                    "00100000-0000-0008-0007-000db84f5910".to_string()
                ),
                ("x-actor-id", actor.to_string()),
            ]
        );
        // no actor (or a nil one): the actor header is left out, never sent empty or nil
        for none in [None, Some(Uuid::nil())] {
            let h = router_headers(tenant, none);
            assert_eq!(h.len(), 3, "{h:?}");
            assert!(h.iter().all(|(k, _)| *k != ACTOR_ID_HEADER));
        }
        for (k, _) in router_headers(tenant, Some(actor)) {
            assert_eq!(
                k,
                k.to_ascii_lowercase(),
                "static header names are lowercase"
            );
        }
    }

    #[test]
    fn the_actor_is_the_person_then_the_service_account_then_none() {
        let human = Some(u("7f1c9a2e-0000-4000-8000-00000000ac70"));
        let service = Some(u("7f1c9a2e-0000-4000-8000-000000005e5a"));
        assert_eq!(pick_actor(human, service), human);
        assert_eq!(pick_actor(None, service), service);
        assert_eq!(pick_actor(Some(Uuid::nil()), service), service);
        assert_eq!(pick_actor(None, None), None);
        assert_eq!(pick_actor(Some(Uuid::nil()), Some(Uuid::nil())), None);
        assert_eq!(human_actor("7f1c9a2e-0000-4000-8000-00000000ac70"), human);
        for not_a_person in ["svc-test", "", "00000000-0000-0000-0000-000000000000"] {
            assert_eq!(human_actor(not_a_person), None, "{not_a_person:?}");
        }
    }

    #[test]
    fn a_malformed_service_actor_is_refused() {
        assert_eq!(parse_service_actor(None).unwrap(), None);
        assert_eq!(parse_service_actor(Some("  ")).unwrap(), None);
        assert_eq!(
            parse_service_actor(Some(" 7f1c9a2e-0000-4000-8000-00000000ac70 ")).unwrap(),
            Some(u("7f1c9a2e-0000-4000-8000-00000000ac70"))
        );
        for bad in [
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
            "7f1c9a2e00004000800000000000ac70",
            "{7f1c9a2e-0000-4000-8000-00000000ac70}",
        ] {
            let e = parse_service_actor(Some(bad)).unwrap_err().to_string();
            assert!(e.contains(SERVICE_ACTOR_ENV), "{e}");
            assert!(!e.contains(bad), "the value is not echoed: {e}");
        }
    }
}
