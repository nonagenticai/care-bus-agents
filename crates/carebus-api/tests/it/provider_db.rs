//! `carebus::provider_db`: the interaction daemon's own path to a tenant's provider. It must see only
//! that tenant's rows (RLS), return `None` for an unbound or soft-deleted provider, open both stored
//! secret forms, and fail loudly — never fall back — when a bound secret cannot be opened.

use carebus::provider_db;
use carebus::provider_secret::{Secrets, seal, seal_token};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{app_pool, env_with, insert_tenant, pool};

/// A test keyring (the `v2:` form); tests that need one opt in exactly as production does.
fn keyring() -> carebus_api::provider_keyring::Keyring {
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode((7u8..39).collect::<Vec<u8>>());
    carebus_api::config::parse_provider_keyring(
        Some(format!(r#"{{"k1":"{key}"}}"#)),
        Some("k1".to_string()),
    )
    .unwrap()
    .unwrap()
}

/// An environment whose provider keyring is configured (the default test config keeps it off).
async fn env_keyed(test: &str) -> crate::common::TestEnv {
    env_with(test, |c| c.provider_keyring = Some(keyring())).await
}

/// Inserts a provider for `tenant` through the owner pool (RLS does not apply) and returns its id.
async fn provider(
    db: &PgPool,
    tenant: Uuid,
    base_url: &str,
    sealed: Option<&str>,
    legacy: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO llm_providers
             (tenant_id, id, name, kind, base_url, secret_sealed, secret_fpr, secret_updated_at,
              secret_enc, updated_by)
         VALUES ($1, $2, $3, 'openai', $4, $5,
                 CASE WHEN $5 IS NULL THEN NULL ELSE 'fpr00000' END,
                 CASE WHEN $5 IS NULL THEN NULL ELSE now() END, $6, 'test')",
    )
    .bind(tenant)
    .bind(id)
    .bind(format!("p-{id}"))
    .bind(base_url)
    .bind(sealed)
    .bind(legacy)
    .execute(db)
    .await
    .unwrap();
    id
}

async fn bind(db: &PgPool, tenant: Uuid, id: Uuid, use_name: &str) {
    sqlx::query(
        "INSERT INTO llm_provider_uses (tenant_id, \"use\", provider_id, model, updated_by)
         VALUES ($1, $2, $3, 'their-model', 'test')",
    )
    .bind(tenant)
    .bind(use_name)
    .bind(id)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn resolve_reads_only_the_tenants_own_binding() {
    let e = env_keyed("provider-db-rls").await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(pool(&e), &a.to_string()).await;
    insert_tenant(pool(&e), &b.to_string()).await;
    let ka = keyring();
    let secrets = Secrets {
        source_key: None,
        keyring: Some(&ka),
    };
    let pa = provider(pool(&e), a, "https://a.example.test/v1", None, None).await;
    let pb = provider(pool(&e), b, "https://b.example.test/v1", None, None).await;
    let sealed = seal(secrets, a, pa, "a-token").unwrap();
    sqlx::query("UPDATE llm_providers SET secret_sealed = $2, secret_fpr = $3, secret_updated_at = now() WHERE id = $1")
        .bind(pa)
        .bind(&sealed.sealed)
        .bind(&sealed.fingerprint)
        .execute(pool(&e))
        .await
        .unwrap();
    bind(pool(&e), a, pa, "qualify").await;
    bind(pool(&e), b, pb, "qualify").await;

    let own = provider_db::resolve(app_pool(&e), a, "qualify", secrets)
        .await
        .unwrap()
        .expect("tenant A's binding");
    assert_eq!(own.id, pa);
    assert_eq!(own.base_url, "https://a.example.test/v1");
    assert_eq!(own.token, "a-token");
    assert_eq!(own.model.as_deref(), Some("their-model"));

    // B's rows are invisible to A's transaction: A can never reach another tenant's provider.
    let other = provider_db::resolve(app_pool(&e), b, "qualify", secrets)
        .await
        .unwrap()
        .expect("tenant B's binding");
    assert_eq!(other.id, pb);
    assert_eq!(other.token, "");
}

#[tokio::test]
async fn a_soft_deleted_provider_and_an_unbound_use_both_resolve_to_none() {
    let e = env_with("provider-db-none", |_| {}).await;
    let t = Uuid::new_v4();
    insert_tenant(pool(&e), &t.to_string()).await;
    let p = provider(pool(&e), t, "https://a.example.test/v1", None, None).await;
    bind(pool(&e), t, p, "qualify").await;
    let secrets = Secrets {
        source_key: None,
        keyring: None,
    };
    assert!(
        provider_db::resolve(app_pool(&e), t, "qualify", secrets)
            .await
            .unwrap()
            .is_some()
    );
    sqlx::query("UPDATE llm_providers SET deleted_at = now() WHERE id = $1")
        .bind(p)
        .execute(pool(&e))
        .await
        .unwrap();
    assert!(
        provider_db::resolve(app_pool(&e), t, "qualify", secrets)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        provider_db::resolve(app_pool(&e), t, "gates", secrets)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn both_stored_forms_open_and_an_unopenable_secret_is_an_error() {
    let e = env_keyed("provider-db-forms").await;
    let t = Uuid::new_v4();
    insert_tenant(pool(&e), &t.to_string()).await;
    let k = keyring();
    let secrets = e.state.cfg.secrets();
    // v2 (the keyring form).
    let p = provider(pool(&e), t, "https://v2.example.test/v1", None, None).await;
    let sealed = seal(secrets, t, p, "v2-token").unwrap();
    sqlx::query("UPDATE llm_providers SET secret_sealed = $2, secret_fpr = $3, secret_updated_at = now() WHERE id = $1")
        .bind(p)
        .bind(&sealed.sealed)
        .bind(&sealed.fingerprint)
        .execute(pool(&e))
        .await
        .unwrap();
    bind(pool(&e), t, p, "qualify").await;
    assert_eq!(
        provider_db::resolve(app_pool(&e), t, "qualify", secrets)
            .await
            .unwrap()
            .unwrap()
            .token,
        "v2-token"
    );
    // The legacy envelope (the three production rows that predate 0031).
    let src = crate::common::source_key();
    let legacy = seal_token(&src, p, "legacy-token");
    sqlx::query("UPDATE llm_providers SET secret_sealed = NULL, secret_fpr = NULL, secret_updated_at = NULL, secret_enc = $2 WHERE id = $1")
        .bind(p)
        .bind(&legacy)
        .execute(pool(&e))
        .await
        .unwrap();
    assert_eq!(
        provider_db::resolve(app_pool(&e), t, "qualify", e.state.cfg.secrets())
            .await
            .unwrap()
            .unwrap()
            .token,
        "legacy-token"
    );
    // A secret sealed for another tenant does not open: the caller fails, it never falls back.
    let wrong = seal(
        Secrets {
            source_key: None,
            keyring: Some(&k),
        },
        Uuid::new_v4(),
        p,
        "not-yours",
    )
    .unwrap();
    sqlx::query("UPDATE llm_providers SET secret_enc = NULL, secret_sealed = $2, secret_fpr = $3, secret_updated_at = now() WHERE id = $1")
        .bind(p)
        .bind(&wrong.sealed)
        .bind(&wrong.fingerprint)
        .execute(pool(&e))
        .await
        .unwrap();
    assert!(
        provider_db::resolve(app_pool(&e), t, "qualify", e.state.cfg.secrets())
            .await
            .is_err()
    );
}
