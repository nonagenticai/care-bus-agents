use crate::common::fakes;
use axum::http::StatusCode;
use serde_json::{Value, json};

#[tokio::test]
async fn jobs_list_and_detail_pass_strata_ingest_through_as_paged_views() {
    let (e, fake) = fakes::sources_env("ingest-list").await;
    let a = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/a"}),
    );
    let b = fakes::add_job(
        &fake,
        fakes::OTHER_LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/b"}),
    );
    fakes::complete(&fake, &a, &[fakes::doc_id(1)]);
    let viewer = crate::common::user();

    let (s, page) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(page["next"], Value::Null);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let first = &page["items"][1];
    assert_eq!(first["id"], a.as_str());
    assert!(first.get("tenant").is_none(), "tenant is not passed on");
    assert_eq!(first["steps"][1]["cost_micros"], 1840);
    assert_eq!(first["steps"][1]["provider"], "vllm");
    assert!(first["steps"][0].get("output").is_none());

    let uri = format!(
        "/api/ingest/jobs?store_id={}&status=queued&limit=9999",
        fakes::OTHER_LEAF
    );
    let (_, filtered) = crate::common::send(&e.app, crate::common::get(&uri, Some(&viewer))).await;
    assert_eq!(filtered["items"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["items"][0]["id"], b.as_str());
    let seen = fake.lock().unwrap().list_queries.last().cloned().unwrap();
    assert_eq!(
        seen,
        format!("limit=200&store_id={}&status=queued", fakes::OTHER_LEAF),
        "limit capped at 200"
    );

    for bad in [
        "/api/ingest/jobs?before=x",
        "/api/ingest/jobs?status=paused",
        "/api/ingest/jobs?store_id=nope",
    ] {
        let (s, b) = crate::common::send(&e.app, crate::common::get(bad, Some(&viewer))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
    }
    let (s, one) = crate::common::send(
        &e.app,
        crate::common::get(&format!("/api/ingest/jobs/{a}"), Some(&viewer)),
    )
    .await;
    assert_eq!(
        (s, one["document_ids"].clone()),
        (StatusCode::OK, json!([fakes::doc_id(1)]))
    );
    let ghost = format!("/api/ingest/jobs/{}", uuid::Uuid::new_v4());
    let (s, b) = crate::common::send(&e.app, crate::common::get(&ghost, Some(&viewer))).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );
}

#[tokio::test]
async fn retry_and_cancel_are_admin_audited_and_map_409() {
    let (e, fake) = fakes::sources_env("ingest-actions").await;
    let job = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/a"}),
    );
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let retry = format!("/api/ingest/jobs/{job}/retry");
    let cancel = format!("/api/ingest/jobs/{job}/cancel");

    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &retry, Some(&viewer), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &retry, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict"))
    );
    assert_eq!(b["error"]["message"], "only a failed job can be retried");
    fakes::fail(&fake, &job, "timeout fetching https://help.example.nl/a");
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &retry, Some(&admin), json!({})),
    )
    .await;
    assert_eq!((s, b["status"].clone()), (StatusCode::OK, json!("queued")));
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &cancel, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("cancelled"))
    );

    let details: Vec<String> = sqlx::query_scalar(
        "SELECT details::text FROM audit_events WHERE resource_type='ingest_job' AND status='failure'",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(details.len(), 1);
    assert!(
        !details[0].contains("only a failed job"),
        "upstream text in audit: {}",
        details[0]
    );
    assert!(details[0].contains("ingest_conflict"), "{}", details[0]);

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT action_type, status FROM audit_events WHERE resource_type='ingest_job' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("retry".into(), "failure".into()),
            ("retry".into(), "success".into()),
            ("cancel".into(), "success".into())
        ]
    );
}

#[tokio::test]
async fn an_echoed_source_never_leaks_credentials_through_the_proxy() {
    let (e, fake) = fakes::sources_env("ingest-redact").await;
    let job = fakes::add_job(
        &fake,
        fakes::OTHER_LEAF,
        json!({
        "kind": "api", "config": {"url": "https://api.example.nl/c", "auth": {"type": "bearer"}},
        "credentials": {"secret": "SECRETMARK-1"},
        "nested": [{"token": "SECRETMARK-2", "Credentials": {"x": "SECRETMARK-3"}}]}),
    );
    let viewer = crate::common::user();
    for uri in [format!("/api/ingest/jobs/{job}"), "/api/ingest/jobs".into()] {
        let (s, b) = crate::common::send(&e.app, crate::common::get(&uri, Some(&viewer))).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!b.to_string().contains("SECRETMARK"), "{b}");
    }
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::get(&format!("/api/ingest/jobs/{job}"), Some(&viewer)),
    )
    .await;
    assert_eq!(b["source"]["nested"][0]["token"], "[redacted]");
    assert_eq!(
        b["source"]["config"]["url"], "https://api.example.nl/c",
        "non-secret config survives"
    );
}

#[tokio::test]
async fn ingest_down_unset_or_refusing_our_token_is_503_never_empty() {
    let (e, fake) = fakes::sources_env("ingest-down").await;
    let viewer = crate::common::user();
    let admin = crate::common::admin();
    fake.lock().unwrap().ingest_down = true;
    let id = uuid::Uuid::new_v4();
    for req in [
        crate::common::get("/api/ingest/jobs", Some(&viewer)),
        crate::common::get(&format!("/api/ingest/jobs/{id}"), Some(&viewer)),
        crate::common::json(
            "POST",
            &format!("/api/ingest/jobs/{id}/retry"),
            Some(&admin),
            json!({}),
        ),
    ] {
        let (s, b) = crate::common::send(&e.app, req).await;
        assert_eq!(
            (s, b["error"]["message"].clone()),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!("strata_ingest is unavailable")
            )
        );
    }
    let (e2, _) = fakes::sources_env_with("ingest-unset", |c| c.strata_ingest_url = None).await;
    assert_eq!(
        crate::common::send(
            &e2.app,
            crate::common::get("/api/ingest/jobs", Some(&viewer))
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let (e3, _) = fakes::sources_env_with("ingest-badtoken", |c| {
        c.strata_ingest_token = Some("wrong".into())
    })
    .await;
    assert_eq!(
        crate::common::send(
            &e3.app,
            crate::common::get("/api/ingest/jobs", Some(&viewer))
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn the_jobs_list_passes_the_before_cursor_and_next_through() {
    let (e, fake) = fakes::sources_env("ingest-cursor").await;
    let jobs: Vec<String> = (0..3)
        .map(|n| {
            fakes::add_job(
                &fake,
                fakes::LEAF,
                json!({"kind": "url", "url": format!("https://help.example.nl/{n}")}),
            )
        })
        .collect();
    let viewer = crate::common::user();
    let (s, p1) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs?limit=2", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p1}");
    let got = |p: &Value| -> Vec<String> {
        p["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(got(&p1), [jobs[2].clone(), jobs[1].clone()], "newest first");
    assert_eq!(p1["next"], jobs[1].as_str(), "strata_ingest's `next`");

    let uri = format!(
        "/api/ingest/jobs?limit=2&store_id={}&before={}",
        fakes::LEAF,
        jobs[1]
    );
    let (s, p2) = crate::common::send(&e.app, crate::common::get(&uri, Some(&viewer))).await;
    assert_eq!(s, StatusCode::OK, "{p2}");
    assert_eq!(got(&p2), [jobs[0].clone()]);
    assert_eq!(p2["next"], Value::Null, "last page");
    assert_eq!(
        fake.lock().unwrap().list_queries.last().unwrap(),
        &format!("limit=2&store_id={}&before={}", fakes::LEAF, jobs[1]),
        "the cursor is forwarded as `before`"
    );

    // not a job id → 400 from us; a job strata_ingest does not know → its 400
    for bad in [
        "/api/ingest/jobs?before=x".to_string(),
        format!("/api/ingest/jobs?before={}", uuid::Uuid::new_v4()),
    ] {
        let (s, b) = crate::common::send(&e.app, crate::common::get(&bad, Some(&viewer))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {b}");
    }
}

#[tokio::test]
async fn retrying_a_failed_source_job_resubmits_it_as_a_new_resync() {
    let (e, fake) = fakes::sources_env("ingest-resubmit").await;
    let admin = crate::common::admin();
    let (s, src) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j("/sources"),
            Some(&admin),
            json!({"name": "Customer API", "type": "api", "target_store_id": fakes::OTHER_LEAF,
                   "config": {"url": "https://api.example.nl/v2/customers", "auth": {"type": "bearer"}},
                   "mapping": [{"source_field": "customer_id", "entity": "Customer", "attribute": "customer_id"}],
                   "credentials": {"secret": "k-retry-SECRETMARK"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{src}");
    let id = src["id"].as_str().unwrap().to_string();
    fake.lock().unwrap().source_failure = Some((
        "source_unavailable".into(),
        "the source answered HTTP 503".into(),
    ));
    let (_, r) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/sources/{id}/resync")),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(r["status"], "failed", "{r}");
    let failed = r["ingest_job_ids"][0].as_str().unwrap().to_string();

    fake.lock().unwrap().source_failure = None;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/ingest/jobs/{failed}/retry"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_ne!(
        b["id"],
        failed.as_str(),
        "a NEW job, not strata_ingest's retry"
    );
    assert_eq!(b["status"], "queued");
    assert_eq!(b["meta"]["source_id"], id.as_str());
    assert!(!b.to_string().contains("SECRETMARK"), "{b}");
    {
        let g = fake.lock().unwrap();
        assert!(
            g.retried.is_empty(),
            "strata_ingest's retry (409) is never called"
        );
        assert_eq!(
            g.job_requests.len(),
            2,
            "re-submitted with the stored credential"
        );
        assert_eq!(
            g.job_requests[1]["source"]["credentials"]["secret"],
            "k-retry-SECRETMARK"
        );
        assert_eq!(g.job_requests[1]["source_id"], id.as_str());
    }
    let viewer = crate::common::user();
    let (_, runs) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(
        runs["items"][0]["status"], "running",
        "the retry is a new run: {runs}"
    );
    assert_eq!(runs["items"][0]["ingest_job_ids"][0], b["id"]);
    let d: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type='ingest_job' AND action_type='retry'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(d["resubmitted_as"], b["id"], "{d}");

    // the source is gone: nothing can re-submit it (strata_ingest kept no credentials) → 409
    for q in ["DELETE FROM source_runs", "DELETE FROM sources"] {
        sqlx::query(q)
            .execute(crate::common::pool(&e))
            .await
            .unwrap();
    }
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/ingest/jobs/{failed}/retry"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict")),
        "{b}"
    );
    assert!(fake.lock().unwrap().retried.is_empty());
}

fn delete(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::delete(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

/// strata_ingest PR #11: cancel is `POST …/cancel` (a running job is `cancelling` until its step ends), `DELETE` removes
/// an ENDED job only and leaves its documents in strata_rag.
#[tokio::test]
async fn cancel_is_cooperative_and_delete_removes_only_an_ended_job() {
    let (e, fake) = fakes::sources_env("ingest-cancel-delete").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let job = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/a"}),
    );
    fakes::claim(&fake, &job);
    let cancel = format!("/api/ingest/jobs/{job}/cancel");
    let one = format!("/api/ingest/jobs/{job}");

    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &cancel, Some(&viewer), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(&e.app, delete(&one, &viewer)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // running → cancelling: still in flight, so it cannot be deleted yet
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &cancel, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("cancelling")),
        "{b}"
    );
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict")),
        "{b}"
    );
    // cancelling is a filterable state, and a repeated cancel is idempotent
    let (s, page) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs?status=cancelling", Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["id"], job.as_str());
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &cancel, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("cancelling"))
    );

    fakes::settle_cancel(&fake, &job);
    // a blob storage_service cannot delete: unavailable, and PARTLY deleted (blobs removed before the refusal are
    // gone), so the job record stays, `deleting`, and can never run again
    fake.lock().unwrap().blob_delete_fails = true;
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("upstream_unavailable")
        )
    );
    assert!(fake.lock().unwrap().jobs.contains_key(&job));
    assert!(fake.lock().unwrap().blobs_lost.contains(&job));
    // the job stays `deleting`: shown and filterable, never cancelled; DELETE again finishes it
    let (s, b) = crate::common::send(&e.app, crate::common::get(&one, Some(&viewer))).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("deleting"))
    );
    let (s, page) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs?status=deleting", Some(&viewer)),
    )
    .await;
    assert_eq!(
        (s, page["items"][0]["id"].clone()),
        (StatusCode::OK, json!(job))
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &cancel, Some(&admin), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict"))
    );
    fake.lock().unwrap().blob_delete_fails = false;
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["status"], "cancelled",
        "the status it had before the first DELETE"
    );
    assert_eq!(b["documents"], "retained");
    assert_eq!(
        fake.lock().unwrap().deleted_jobs,
        std::slice::from_ref(&job)
    );
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );

    // a completed job: cancel is a 409, delete keeps its documents in strata_rag and says so
    let done = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/b"}),
    );
    fakes::complete(&fake, &done, &[fakes::doc_id(7)]);
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/ingest/jobs/{done}/cancel"),
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict"))
    );
    let (s, b) =
        crate::common::send(&e.app, delete(&format!("/api/ingest/jobs/{done}"), &admin)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["document_ids"], json!([fakes::doc_id(7)]));
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "documents are never deleted in strata_rag"
    );

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT action_type, status FROM audit_events WHERE resource_type='ingest_job' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    let rows: Vec<(&str, &str)> = rows.iter().map(|(a, s)| (a.as_str(), s.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("cancel", "success"),
            ("delete", "failure"),
            ("cancel", "success"),
            ("delete", "failure"),
            ("cancel", "failure"),
            ("delete", "success"),
            ("delete", "failure"),
            ("cancel", "failure"),
            ("delete", "success"),
        ]
    );
    let retained: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type='ingest_job' AND action_type='delete' AND status='success' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(retained["documents_retained"], 1);
}

/// strata_ingest PR #11 `db::jobs::cancel`: a running job between steps is `cancelled` at once (only a step in flight
/// makes it `cancelling`); a crawl parent that has ended still cancels its open page jobs, and only then is 409.
#[tokio::test]
async fn cancel_between_steps_is_immediate_and_an_ended_crawl_parent_cancels_its_pages() {
    let (e, fake) = fakes::sources_env("ingest-cancel-idle").await;
    let admin = crate::common::admin();
    let cancel = |id: &str| {
        crate::common::json(
            "POST",
            &format!("/api/ingest/jobs/{id}/cancel"),
            Some(&admin),
            json!({}),
        )
    };
    let job = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/b"}),
    );
    fakes::between_steps(&fake, &job);
    let (s, b) = crate::common::send(&e.app, cancel(&job)).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("cancelled")),
        "{b}"
    );

    let parent = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "crawl", "url": "https://help.example.nl/"}),
    );
    let pages = fakes::add_pages(&fake, &parent, 3);
    fakes::complete(&fake, &pages[0], &[]);
    fakes::claim(&fake, &pages[1]);
    fakes::settle_parent(&fake, &parent);
    let (s, b) = crate::common::send(&e.app, cancel(&parent)).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("completed")),
        "the parent keeps its status: {b}"
    );
    let status = |id: &str| fake.lock().unwrap().jobs[id]["status"].clone();
    assert_eq!(
        [status(&pages[0]), status(&pages[1]), status(&pages[2])],
        [json!("completed"), json!("cancelling"), json!("cancelled")]
    );
    // nothing left open: an ended parent is now the usual 409
    let (s, b) = crate::common::send(&e.app, cancel(&parent)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("ingest_conflict")),
        "{b}"
    );
}

/// strata_ingest may finish a delete in the background: 202 `{job_id, status: "deleting"}`, passed on as 202 with the
/// same shape; the job reads `deleting` until its sweeper removes it (then 404).
#[tokio::test]
async fn a_delete_strata_ingest_finishes_in_the_background_is_202_deleting_then_gone() {
    let (e, fake) = fakes::sources_env("ingest-delete-async").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let job = fakes::add_job(
        &fake,
        fakes::LEAF,
        json!({"kind": "url", "url": "https://help.example.nl/c"}),
    );
    fakes::complete(&fake, &job, &[fakes::doc_id(9)]);
    fake.lock().unwrap().delete_async = true;
    let one = format!("/api/ingest/jobs/{job}");

    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b, json!({"job_id": job, "status": "deleting"}));
    let (s, b) = crate::common::send(&e.app, crate::common::get(&one, Some(&viewer))).await;
    assert_eq!(
        (s, b["status"].clone()),
        (StatusCode::OK, json!("deleting"))
    );

    fakes::sweep_deletes(&fake);
    let (s, b) = crate::common::send(&e.app, crate::common::get(&one, Some(&viewer))).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found")),
        "{b}"
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type = 'ingest_job' AND action_type = 'delete'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(details, json!({"status": "deleting", "accepted": true}));
}
