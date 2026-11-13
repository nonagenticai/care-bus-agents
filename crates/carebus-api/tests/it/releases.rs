//! Releases are read-only (spec 2026-10-18 §1: nothing pushes a release, the base is seeded by migrations):
//! `GET /api/jobs/{job}/releases/current` reads the bootstrap release back and `GET …/release-conflicts` lists the
//! stored conflicts. Fixtures go through the owner pool (bootstrap tenant/job session GUCs, RLS bypassed); the API
//! serves as `carebus_app`.

use crate::common::send;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

/// An override: a tenant copy of a base context space with another label, `base_hash` = the base content_hash.
async fn override_space(owner: &PgPool, key: &str, label: &str) {
    sqlx::query(
        "INSERT INTO context_spaces (key, area, label, request_type, required, entities, procedure, base_hash)
         SELECT key, area, $2, request_type, required, entities, procedure, content_hash
           FROM context_spaces WHERE key = $1 AND origin = 'base'",
    )
    .bind(key)
    .bind(label)
    .execute(owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn current_release_reads_back() {
    let e = crate::common::env("rel-current").await;
    let (s, b) = send(
        &e.app,
        crate::common::get(
            &crate::common::j("/releases/current"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["release_id"], "bootstrap-0009");
    assert_eq!(b["hash"], "sha256:bootstrap-0009");
    assert_eq!(b["applied"], true);
    assert_eq!(b["previous_release_id"], Value::Null);
    assert_eq!(b["counts"]["context_spaces"], 5);
    assert_eq!(b["conflicts"], json!([]));
    // Only `current` is served; an unprovisioned person is not a reader.
    let (s, _) = send(
        &e.app,
        crate::common::get(
            &crate::common::j("/releases/bootstrap-0009"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(
        &e.app,
        crate::common::get(
            &crate::common::j("/releases/current"),
            Some(&crate::common::token_for("u-nobody", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_push_route_is_gone() {
    let e = crate::common::env("rel-no-push").await;
    let (s, b) = send(
        &e.app,
        crate::common::json(
            "PUT",
            &crate::common::j("/releases/2026-10-20.1"),
            Some(&crate::common::admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED, "{b}");
}

#[tokio::test]
async fn new_job_without_release_is_404() {
    let e = crate::common::env("rel-current-404").await;
    let req = crate::common::json(
        "PUT",
        &format!("/api/tenants/{}/jobs/second", crate::common::BOOT_TENANT),
        Some(&crate::common::super_admin()),
        json!({"label": "Second"}),
    );
    let (s, b) = send(&e.app, req).await;
    assert!(s.is_success(), "{s} {b}");
    let (s, b) = send(
        &e.app,
        crate::common::get(
            "/api/jobs/second/releases/current",
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["message"], "release not found");
}

#[tokio::test]
async fn conflicts_list_tracks_resolution() {
    let e = crate::common::env("rel-conflict-list").await;
    let owner = crate::common::pool(&e);
    override_space(owner, "retail:faq:general", "FAQ (ours)").await;
    // A conflict as an earlier release apply stored it (none is detected any more: nothing pushes).
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO release_conflicts (release_id, kind, object, key, tenant_row, base_hash_before, base_hash_after)
         VALUES ('bootstrap-0009', 'base_changed', 'context_space', '{\"key\": \"retail:faq:general\"}', 'override',
                 'sha256:before', 'sha256:after')
         RETURNING id",
    )
    .fetch_one(owner)
    .await
    .unwrap();

    let list = |status: &'static str| {
        let app = e.app.clone();
        async move {
            let (s, b) = send(
                &app,
                crate::common::get(
                    &crate::common::j(&format!("/release-conflicts?status={status}")),
                    Some(&crate::common::user()),
                ),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
            assert_eq!(b["next"], Value::Null);
            b["items"].as_array().unwrap().clone()
        }
    };
    let open = list("open").await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["id"], id);
    assert_eq!(open[0]["release_id"], "bootstrap-0009");
    assert_eq!(open[0]["resolution"], Value::Null);
    // the current release reports its open conflicts too
    let (_, cur) = send(
        &e.app,
        crate::common::get(
            &crate::common::j("/releases/current"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(cur["conflicts"][0]["id"], id, "{cur}");

    // the admin accepts the base under the override: the conflict row records the resolution
    let (s, b) = send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/refinements/rebase"),
            Some(&crate::common::admin()),
            json!({"object": "context_space", "key": {"key": "retail:faq:general"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(list("open").await.len(), 0);
    let all = list("all").await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["resolution"], "rebased");
    assert_eq!(all[0]["resolved_by"], "u-admin");
    assert!(all[0]["resolved_at"].is_string());

    let (s, b) = send(
        &e.app,
        crate::common::get(
            &crate::common::j("/release-conflicts?status=nope"),
            Some(&crate::common::user()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "invalid_query");
}
