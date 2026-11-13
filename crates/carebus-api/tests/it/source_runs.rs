use crate::common::{Part, fakes};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};

const SECRET: &str = "k-run-SECRETMARK";

async fn create(e: &crate::common::TestEnv, body: Value) -> String {
    let admin = crate::common::admin();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/sources"), Some(&admin), body),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b["id"].as_str().unwrap().to_string()
}

fn website(name: &str) -> Value {
    json!({"name": name, "type": "website", "target_store_id": fakes::LEAF,
           "config": {"urls": ["https://help.example.nl/returns", "https://help.example.nl/warranty"],
                      "options": {"captions": false}}})
}

fn api() -> Value {
    json!({"name": "Customer API", "type": "api", "target_store_id": fakes::OTHER_LEAF,
           "config": {"url": "https://api.example.nl/v2/customers", "auth": {"type": "bearer"}, "options": {"language": "nl"}},
           "mapping": [{"source_field": "customer_id", "entity": "Customer", "attribute": "customer_id"}],
           "credentials": {"secret": SECRET}})
}

async fn resync(e: &crate::common::TestEnv, id: &str) -> (StatusCode, Value) {
    let admin = crate::common::admin();
    crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/sources/{id}/resync")),
            Some(&admin),
            json!({}),
        ),
    )
    .await
}

async fn run(e: &crate::common::TestEnv, id: &str, run: &str) -> Value {
    let viewer = crate::common::user();
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs/{run}")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

fn delete(uri: &str, token: &str) -> Request<Body> {
    Request::delete(uri)
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn website_resync_tracks_jobs_and_replaces_only_documents_the_new_run_lacks() {
    let (e, fake) = fakes::sources_env("runs-web").await;
    let id = create(&e, website("Help centre")).await;
    let (s, r1) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r1}");
    assert_eq!(r1["status"], "running");
    let jobs1 = ids(&r1["ingest_job_ids"]);
    assert_eq!(jobs1.len(), 2, "one job per URL");
    let sent = fake.lock().unwrap().job_requests[0].clone();
    assert_eq!(
        sent,
        json!({"store_id": fakes::LEAF, "source_id": id,
        "urls": ["https://help.example.nl/returns", "https://help.example.nl/warranty"], "options": {"captions": false}})
    );

    let r1_id = r1["id"].as_str().unwrap().to_string();
    assert_eq!(
        run(&e, &id, &r1_id).await["status"],
        "running",
        "jobs still queued"
    );
    let (d1, d2, d3, d4) = (
        fakes::doc_id(1),
        fakes::doc_id(2),
        fakes::doc_id(3),
        fakes::doc_id(4),
    );
    fakes::complete(&fake, &jobs1[0], &[d1.clone(), d2.clone()]);
    fakes::complete(&fake, &jobs1[1], &[d2.clone(), d3.clone()]);
    let done1 = run(&e, &id, &r1_id).await;
    assert_eq!(done1["status"], "completed");
    assert_eq!(
        ids(&done1["document_ids"]),
        [d1.clone(), d2.clone(), d3.clone()],
        "deduplicated, in order"
    );
    assert_eq!(
        done1["replace_report"],
        json!({"deleted": 0, "already_gone": 0, "kept_shared": 0, "failed": []})
    );
    assert!(done1["finished_at"].is_string());

    let (_, r2) = resync(&e, &id).await;
    let jobs2 = ids(&r2["ingest_job_ids"]);
    // d3 is unchanged content: strata_rag hands back the SAME id; it must survive the replace
    fakes::complete(&fake, &jobs2[0], std::slice::from_ref(&d3));
    fakes::complete(&fake, &jobs2[1], std::slice::from_ref(&d4));
    let r2_id = r2["id"].as_str().unwrap().to_string();
    let done2 = run(&e, &id, &r2_id).await;
    assert_eq!(done2["status"], "completed");
    assert_eq!(done2["replaced_run_id"], r1_id.as_str());
    assert_eq!(
        done2["replace_report"],
        json!({"deleted": 2, "already_gone": 0, "kept_shared": 1, "failed": []})
    );
    let mut deleted = fake.lock().unwrap().deleted_docs.clone();
    deleted.sort();
    assert_eq!(
        deleted,
        [d1, d2],
        "only documents the new run no longer has"
    );

    let viewer = crate::common::user();
    let (_, src) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/sources/{id}")), Some(&viewer)),
    )
    .await;
    assert_eq!(src["last_run"]["id"], r2_id.as_str());
    assert_eq!(src["last_run"]["document_count"], 2);
    let (_, page) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs?limit=1")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(page["items"][0]["id"], r2_id.as_str());
    let next = page["next"].as_str().unwrap().to_string();
    let (_, older) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!(
                "/sources/{id}/runs?limit=1&before={}",
                next.replace('+', "%2B")
            )),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(older["items"][0]["id"], r1_id.as_str());
    let replaces: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action_type='replace' AND actor_type='system'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(replaces, 2);
}

#[tokio::test]
async fn resync_while_running_is_409_and_sends_nothing() {
    let (e, fake) = fakes::sources_env("runs-busy").await;
    let id = create(&e, website("Help centre")).await;
    let (_, r1) = resync(&e, &id).await;
    let (s, b) = resync(&e, &id).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("run_in_progress"))
    );
    assert_eq!(
        fake.lock().unwrap().job_requests.len(),
        1,
        "the second click sent nothing"
    );
    for j in ids(&r1["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[]);
    }
    let (s, _) = resync(&e, &id).await;
    assert_eq!(
        s,
        StatusCode::ACCEPTED,
        "a finished-but-unread run is refreshed first, then the new one starts"
    );
}

#[tokio::test]
async fn two_readers_seeing_completion_at_once_replace_once() {
    let (e, fake) = fakes::sources_env("runs-race").await;
    let id = create(&e, website("Help centre")).await;
    let (_, r1) = resync(&e, &id).await;
    for j in ids(&r1["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[fakes::doc_id(1)]);
    }
    run(&e, &id, r1["id"].as_str().unwrap()).await;
    let (_, r2) = resync(&e, &id).await;
    for j in ids(&r2["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[fakes::doc_id(2)]);
    }
    let r2_id = r2["id"].as_str().unwrap().to_string();
    // Hold every job lookup so all four readers fetch the final jobs BEFORE any of them finishes the run:
    // the `running -> final` update is then the only thing that picks one winner.
    fake.lock().unwrap().job_delay_ms = 150;
    let (a, b, c, d) = tokio::join!(
        run(&e, &id, &r2_id),
        run(&e, &id, &r2_id),
        run(&e, &id, &r2_id),
        run(&e, &id, &r2_id)
    );
    for x in [&a, &b, &c, &d] {
        assert_eq!(x["status"], "completed");
    }
    assert_eq!(fake.lock().unwrap().deleted_docs, [fakes::doc_id(1)]);
    // The fake answers 404 to a repeated delete, so the document list alone cannot see a double replace;
    // the audit rows can: one `replace` per completed run, never two for the same run.
    let replaces: Vec<Value> = sqlx::query_scalar(
        "SELECT details->'run_id' FROM audit_events WHERE action_type='replace' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        replaces,
        [r1["id"].clone(), json!(r2_id)],
        "the run that two readers completed replaced once"
    );
}

#[tokio::test]
async fn api_resync_sends_the_extended_contract_and_its_job_never_shows_the_secret() {
    let (e, fake) = fakes::sources_env("runs-api").await;
    let id = create(&e, api()).await;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(
        (s, r["status"].clone()),
        (StatusCode::ACCEPTED, json!("running")),
        "{r}"
    );

    let sent = fake.lock().unwrap().job_requests[0].clone();
    assert_eq!(sent["store_id"], fakes::OTHER_LEAF);
    assert_eq!(sent["source_id"], id.as_str());
    assert_eq!(sent["source"]["kind"], "api");
    assert_eq!(
        sent["source"]["credentials"],
        json!({"secret": SECRET}),
        "the credential IS sent to strata_ingest"
    );
    assert_eq!(
        sent["source"]["config"],
        json!({"url": "https://api.example.nl/v2/customers", "auth": {"type": "bearer"}})
    );
    assert_eq!(sent["options"], json!({"language": "nl"}));
    assert_eq!(
        sent["mapping"],
        json!([{"source_field": "customer_id", "entity": "Customer", "attribute": "customer_id"}])
    );

    let viewer = crate::common::user();
    let job = ids(&r["ingest_job_ids"])[0].clone();
    let (_, j) = crate::common::send(
        &e.app,
        crate::common::get(&format!("/api/ingest/jobs/{job}"), Some(&viewer)),
    )
    .await;
    assert_eq!(j["meta"]["source_id"], id.as_str());
    assert!(!j.to_string().contains("SECRETMARK"), "{j}");
}

#[tokio::test]
async fn a_source_fetch_failure_is_a_202_from_ingest_and_a_failed_run_with_its_code() {
    let (e, fake) = fakes::sources_env("runs-api-fail").await;
    {
        let mut g = fake.lock().unwrap();
        g.echo_secret_in_errors = true;
        g.source_failure = Some(("source_auth".into(), "the source answered HTTP 401".into()));
    }
    let id = create(&e, api()).await;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::OK, "final at once: {r}");
    assert_eq!(r["status"], "failed", "NOT unsupported: {r}");
    let job = ids(&r["ingest_job_ids"])[0].clone();
    let err = r["error"].as_str().unwrap();
    assert!(
        err.contains(&format!(
            "{job} failed: source_auth: the source answered HTTP 401"
        )),
        "{err}"
    );
    assert!(
        err.contains("[redacted]") && !err.contains("SECRETMARK"),
        "secret scrubbed: {err}"
    );
    assert!(r["finished_at"].is_string());

    let viewer = crate::common::user();
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/sources"), Some(&viewer)),
    )
    .await;
    assert_eq!(list["items"][0]["last_run"]["status"], "failed");
    assert!(!list.to_string().contains("SECRETMARK"));
    let (_, j) = crate::common::send(
        &e.app,
        crate::common::get(&format!("/api/ingest/jobs/{job}"), Some(&viewer)),
    )
    .await;
    assert!(!j.to_string().contains("SECRETMARK"), "{j}");
    assert!(!crate::common::audit_text(&e).await.contains("SECRETMARK"));
}

#[tokio::test]
async fn a_base64_or_percent_encoded_echo_of_the_secret_is_scrubbed_too() {
    let (e, fake) = fakes::sources_env("runs-api-encoded").await;
    {
        let mut g = fake.lock().unwrap();
        g.echo_secret_in_errors = true;
        g.echo_secret_encoded = true;
        g.source_failure = Some(("source_auth".into(), "the source answered HTTP 401".into()));
    }
    const ODD: &str = "s3cr/t+VAL=ue?&";
    let mut body = api();
    body["credentials"]["secret"] = json!(ODD);
    let id = create(&e, body).await;
    let (_, r) = resync(&e, &id).await;
    let forms = [ODD, "czNjci90K1ZBTD11ZT8m", "s3cr%2Ft%2BVAL%3Due%3F%26"];
    let err = r["error"].as_str().unwrap();
    assert!(
        err.contains("Basic [redacted], url ?key=[redacted]"),
        "{err}"
    );
    let viewer = crate::common::user();
    let job = ids(&r["ingest_job_ids"])[0].clone();
    let (_, j) = crate::common::send(
        &e.app,
        crate::common::get(&format!("/api/ingest/jobs/{job}"), Some(&viewer)),
    )
    .await;
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get("/api/ingest/jobs", Some(&viewer)),
    )
    .await;
    for f in forms {
        assert!(!err.contains(f), "run error holds {f}: {err}");
        assert!(!j.to_string().contains(f), "job view holds {f}: {j}");
        assert!(!list.to_string().contains(f), "job list holds {f}");
    }
}

#[tokio::test]
async fn only_an_unsupported_source_kind_is_unsupported_a_malformed_request_is_failed() {
    let (e, fake) = fakes::sources_env("runs-api-refused").await;
    fake.lock().unwrap().echo_secret_in_errors = true;
    let id = create(&e, api()).await;

    fake.lock().unwrap().source_bad_request = true;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(
        (s, r["status"].clone()),
        (StatusCode::OK, json!("failed")),
        "{r}"
    );
    let err = r["error"].as_str().unwrap();
    assert!(
        err.starts_with("strata_ingest refused the request (HTTP 400): invalid body"),
        "{err}"
    );
    assert!(
        err.contains("[redacted]") && !err.contains("SECRETMARK"),
        "{err}"
    );
    fake.lock().unwrap().source_bad_request = false;

    // a strata_ingest that does not know the kind (422 `unsupported_source_kind`) is the only `unsupported`
    fake.lock().unwrap().unsupported_kinds = true;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(
        (s, r["status"].clone()),
        (StatusCode::OK, json!("unsupported")),
        "{r}"
    );
    let err = r["error"].as_str().unwrap();
    assert!(
        err.starts_with(
            "strata_ingest refused the request (HTTP 422): source kind `api` is not supported"
        ),
        "{err}"
    );
}

#[tokio::test]
async fn file_resync_forwards_the_uploads_as_multipart() {
    let (e, fake) = fakes::sources_env("runs-file").await;
    let id = create(
        &e,
        json!({"name": "Manuals", "type": "file", "target_store_id": fakes::LEAF,
                                "config": {"options": {"ocr": "auto"}}}),
    )
    .await;
    let admin = crate::common::admin();
    let uri = crate::common::j(&format!("/sources/{id}/resync"));
    let parts = [
        Part {
            name: "file",
            file_name: Some("airfryer.pdf"),
            content_type: Some("application/pdf"),
            body: b"%PDF-1.7 airfryer",
        },
        Part {
            name: "file",
            file_name: Some("drill.txt"),
            content_type: Some("text/plain"),
            body: b"drill manual",
        },
    ];
    let (s, r) =
        crate::common::send(&e.app, crate::common::multipart(&uri, Some(&admin), &parts)).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    assert_eq!(ids(&r["ingest_job_ids"]).len(), 2);
    let sent = fake.lock().unwrap().job_requests[0]["multipart"].clone();
    assert_eq!(sent["store_id"], fakes::LEAF);
    assert_eq!(
        sent["source_id"],
        id.as_str(),
        "source_id goes before the files"
    );
    assert_eq!(sent["options"], json!({"ocr": "auto"}));
    assert_eq!(
        sent["files"][0],
        json!({"name": "airfryer.pdf", "content_type": "application/pdf", "size": 17, "text": "%PDF-1.7 airfryer"})
    );
    assert_eq!(sent["files"][1]["name"], "drill.txt");

    for j in ids(&r["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[]);
    }
    run(&e, &id, r["id"].as_str().unwrap()).await;
    let (s, b) =
        crate::common::send(&e.app, crate::common::multipart(&uri, Some(&admin), &[])).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("no_file"))
    );
    let other = [Part {
        name: "notes",
        file_name: None,
        content_type: None,
        body: b"x",
    }];
    let (s, _) =
        crate::common::send(&e.app, crate::common::multipart(&uri, Some(&admin), &other)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = resync(&e, &id).await; // JSON, not multipart
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM source_runs")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(runs, 1, "a refused upload leaves no run behind");
}

#[tokio::test]
async fn ingest_unavailable_fails_the_run_with_503_and_stale_reads_stay_running() {
    let (e, fake) = fakes::sources_env("runs-down").await;
    let id = create(&e, website("Help centre")).await;
    fake.lock().unwrap().ingest_down = true;
    let (s, b) = resync(&e, &id).await;
    assert_eq!(
        (s, b["error"]["message"].clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!("strata_ingest is unavailable")
        )
    );
    let viewer = crate::common::user();
    let (_, runs) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(runs["items"][0]["status"], "failed");
    assert_eq!(
        runs["items"][0]["error"],
        "strata_ingest did not confirm the request; check Ingest jobs before re-syncing."
    );

    fake.lock().unwrap().ingest_down = false;
    let (_, r) = resync(&e, &id).await;
    fake.lock().unwrap().ingest_down = true;
    let stale = run(&e, &id, r["id"].as_str().unwrap()).await;
    assert_eq!(
        (stale["status"].clone(), stale["stale"].clone()),
        (json!("running"), json!(true)),
        "unknown, not failed"
    );
    let (_, runs) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(runs["items"][0]["stale"], true);
}

#[tokio::test]
async fn a_failed_job_fails_the_run_and_replaces_nothing() {
    let (e, fake) = fakes::sources_env("runs-fail").await;
    let id = create(&e, website("Help centre")).await;
    let (_, r1) = resync(&e, &id).await;
    for j in ids(&r1["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[fakes::doc_id(1)]);
    }
    run(&e, &id, r1["id"].as_str().unwrap()).await;
    let (_, r2) = resync(&e, &id).await;
    let jobs = ids(&r2["ingest_job_ids"]);
    fakes::complete(&fake, &jobs[0], &[fakes::doc_id(2)]);
    fakes::fail(&fake, &jobs[1], "HTTP 404 from help.example.nl");
    let done = run(&e, &id, r2["id"].as_str().unwrap()).await;
    assert_eq!(done["status"], "failed");
    assert_eq!(
        done["error"],
        format!(
            "1 of 2 ingest jobs did not complete: {} failed: fetch_failed: HTTP 404 from help.example.nl",
            jobs[1]
        )
    );
    assert_eq!(
        ids(&done["document_ids"]),
        [fakes::doc_id(2)],
        "partial documents are still recorded"
    );
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "no replace on a failed run"
    );
}

#[tokio::test]
async fn an_empty_source_fails_as_no_records_and_keeps_the_earlier_runs_documents() {
    let (e, fake) = fakes::sources_env("runs-no-records").await;
    let id = create(&e, api()).await;
    let first = complete_run(&e, &fake, &id, &[fakes::doc_id(1), fakes::doc_id(2)]).await;
    fake.lock().unwrap().source_failure = Some((
        "no_records".into(),
        "the source holds no records; nothing was ingested (and nothing replaced)".into(),
    ));
    let (s, r) = resync(&e, &id).await;
    assert_eq!(
        (s, r["status"].clone()),
        (StatusCode::OK, json!("failed")),
        "{r}"
    );
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("no_records: the source holds no records"),
        "{r}"
    );
    assert_eq!(
        r["replace_report"],
        Value::Null,
        "no replace on a failed run"
    );
    // a later read of the failed run does not replace either
    run(&e, &id, r["id"].as_str().unwrap()).await;
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "the earlier run's documents stay in strata_rag"
    );
    let removed: bool =
        sqlx::query_scalar("SELECT documents_removed FROM source_runs WHERE id = $1::uuid")
            .bind(&first)
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert!(!removed, "the earlier run still owns its documents");
}

#[tokio::test]
async fn an_orphaned_run_without_jobs_fails_after_a_minute_and_unblocks_the_source() {
    let (e, _) = fakes::sources_env("runs-orphan").await;
    let id = create(&e, website("Help centre")).await;
    sqlx::query("INSERT INTO source_runs (id, source_id, status, started_by, started_at) VALUES ($1, $2::uuid, 'running', 'a', now() - interval '2 minutes')")
        .bind(uuid::Uuid::new_v4())
        .bind(&id)
        .execute(crate::common::pool(&e))
        .await
        .unwrap();
    let (s, r) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let viewer = crate::common::user();
    let (_, runs) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{id}/runs")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(runs["items"][1]["status"], "failed");
    assert!(
        runs["items"][1]["error"]
            .as_str()
            .unwrap()
            .contains("No ingest job was recorded")
    );
}

#[tokio::test]
async fn a_failed_deletion_is_reported_and_retried_by_the_next_completion() {
    let (e, fake) = fakes::sources_env("runs-delfail").await;
    let id = create(&e, website("Help centre")).await;
    let complete_run = |docs: Vec<String>| {
        let (e, fake, id) = (&e, fake.clone(), id.clone());
        async move {
            let (_, r) = resync(e, &id).await;
            for j in ids(&r["ingest_job_ids"]) {
                fakes::complete(&fake, &j, &docs);
            }
            run(e, &id, r["id"].as_str().unwrap()).await
        }
    };
    complete_run(vec![fakes::doc_id(1), fakes::doc_id(2)]).await;
    fake.lock().unwrap().failing_docs.insert(fakes::doc_id(1));
    let r2 = complete_run(vec![fakes::doc_id(3)]).await;
    assert_eq!(
        r2["replace_report"],
        json!({"deleted": 1, "already_gone": 0, "kept_shared": 0, "failed": [fakes::doc_id(1)]})
    );
    fake.lock().unwrap().failing_docs.clear();
    let r3 = complete_run(vec![fakes::doc_id(4)]).await;
    assert_eq!(r3["replace_report"]["failed"], json!([]));
    let mut deleted = fake.lock().unwrap().deleted_docs.clone();
    deleted.sort();
    assert_eq!(
        deleted,
        [fakes::doc_id(1), fakes::doc_id(2), fakes::doc_id(3)],
        "d1 retried, d3 replaced"
    );
}

#[tokio::test]
async fn delete_keeps_strata_documents_unless_purge_and_never_mid_run() {
    let (e, fake) = fakes::sources_env("runs-delete").await;
    let admin = crate::common::admin();
    let viewer = crate::common::user();
    let keep = create(&e, website("Keep")).await;
    let (_, r) = resync(&e, &keep).await;
    for j in ids(&r["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[fakes::doc_id(1), fakes::doc_id(2)]);
    }
    let (s, _) = crate::common::send(
        &e.app,
        delete(&crate::common::j(&format!("/sources/{keep}")), &viewer),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = crate::common::send(
        &e.app,
        delete(&crate::common::j(&format!("/sources/{keep}")), &admin),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"id": keep, "purged": false, "documents_deleted": 0, "documents_kept": 2})
    );
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "config only by default"
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE action_type='delete' AND resource_type='source'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(
        details["kept_document_ids"],
        json!([fakes::doc_id(1), fakes::doc_id(2)]),
        "the kept ids stay traceable"
    );

    let purge = create(&e, website("Purge")).await;
    let (_, r) = resync(&e, &purge).await;
    let jobs = ids(&r["ingest_job_ids"]);
    let uri = crate::common::j(&format!("/sources/{purge}?purge=true"));
    let (s, b) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("run_in_progress"))
    );
    for j in &jobs {
        fakes::complete(&fake, j, &[fakes::doc_id(7)]);
    }
    fake.lock().unwrap().rag_down = true;
    let (s, _) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(
                &crate::common::j(&format!("/sources/{purge}")),
                Some(&viewer)
            )
        )
        .await
        .0,
        StatusCode::OK,
        "kept"
    );
    fake.lock().unwrap().rag_down = false;
    let (s, b) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"id": purge, "purged": true, "documents_deleted": 1, "documents_kept": 0})
    );
    assert_eq!(fake.lock().unwrap().deleted_docs, [fakes::doc_id(7)]);
    assert_eq!(
        crate::common::send(
            &e.app,
            crate::common::get(
                &crate::common::j(&format!("/sources/{purge}")),
                Some(&viewer)
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn disabled_sources_and_viewers_cannot_resync() {
    let (e, _) = fakes::sources_env("runs-guard").await;
    let mut body = website("Off");
    body["enabled"] = json!(false);
    let id = create(&e, body).await;
    let (s, b) = resync(&e, &id).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("source_disabled"))
    );
    let on = create(&e, website("On")).await;
    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &crate::common::j(&format!("/sources/{on}/resync")),
            Some(&viewer),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{on}/runs")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "viewers read run history");
    let ghost = uuid::Uuid::new_v4();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j(&format!("/sources/{on}/runs/{ghost}")),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn failure_audit_rows_hold_our_code_and_status_never_upstream_text() {
    let (e, fake) = fakes::sources_env("runs-audit").await;
    let admin = crate::common::admin();
    let id = create(&e, website("Help centre")).await;
    fake.lock().unwrap().ingest_down = true;
    let (s, _) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let d: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE action_type='resync' AND status='failure'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(d, json!({"status": 503, "code": "upstream_unavailable"}));
    fake.lock().unwrap().ingest_down = false;

    // a purge that reaches strata_rag and fails leaves a failure row; the source is kept
    let (_, r) = resync(&e, &id).await;
    for j in ids(&r["ingest_job_ids"]) {
        fakes::complete(&fake, &j, &[fakes::doc_id(5)]);
    }
    run(&e, &id, r["id"].as_str().unwrap()).await;
    let uri = crate::common::j(&format!("/sources/{id}?purge=true"));
    fake.lock().unwrap().rag_down = true;
    let (s, _) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    fake.lock().unwrap().rag_down = false;
    fake.lock().unwrap().failing_docs.insert(fakes::doc_id(5));
    // the fake's 500 for one document is an upstream failure to us: 503, source kept
    let (s, _) = crate::common::send(&e.app, delete(&uri, &admin)).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let rows: Vec<Value> = sqlx::query_scalar("SELECT details FROM audit_events WHERE action_type='delete' AND status='failure' ORDER BY id")
        .fetch_all(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            json!({"purge": true, "status": 503, "code": "upstream_unavailable"}),
            json!({"purge": true, "status": 503, "code": "upstream_unavailable"})
        ]
    );
    assert!(
        !crate::common::audit_text(&e).await.contains("disk full"),
        "no upstream text in audit"
    );
}

/// Completes one run of `id` with exactly `docs` (every job reports the same ids).
async fn complete_run(
    e: &crate::common::TestEnv,
    fake: &fakes::Shared,
    id: &str,
    docs: &[String],
) -> String {
    let (s, r) = resync(e, id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    for j in ids(&r["ingest_job_ids"]) {
        fakes::complete(fake, &j, docs);
    }
    let rid = r["id"].as_str().unwrap().to_string();
    assert_eq!(run(e, id, &rid).await["status"], "completed");
    rid
}

#[tokio::test]
async fn replace_never_deletes_a_document_another_source_still_owns() {
    let (e, fake) = fakes::sources_env("runs-shared-replace").await;
    let a = create(&e, website("A")).await;
    let b = create(&e, website("B")).await;
    let (d1, d2, d3) = (fakes::doc_id(1), fakes::doc_id(2), fakes::doc_id(3));
    // same leaf store, same content: strata_rag hands both sources the SAME id d1
    complete_run(&e, &fake, &b, std::slice::from_ref(&d1)).await;
    complete_run(&e, &fake, &a, &[d1.clone(), d2.clone()]).await;
    let a2 = complete_run(&e, &fake, &a, std::slice::from_ref(&d3)).await;
    let done = run(&e, &a, &a2).await;
    assert_eq!(
        done["replace_report"],
        json!({"deleted": 1, "already_gone": 0, "kept_shared": 1, "failed": []})
    );
    assert_eq!(
        fake.lock().unwrap().deleted_docs,
        [d2],
        "d1 belongs to B as well"
    );
}

#[tokio::test]
async fn purge_never_deletes_a_document_another_source_still_owns() {
    let (e, fake) = fakes::sources_env("runs-shared-purge").await;
    let admin = crate::common::admin();
    let a = create(&e, website("A")).await;
    let b = create(&e, website("B")).await;
    let (d1, d2) = (fakes::doc_id(1), fakes::doc_id(2));
    complete_run(&e, &fake, &b, std::slice::from_ref(&d1)).await;
    complete_run(&e, &fake, &a, &[d1.clone(), d2.clone()]).await;
    let (s, body) = crate::common::send(
        &e.app,
        delete(
            &crate::common::j(&format!("/sources/{a}?purge=true")),
            &admin,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({"id": a, "purged": true, "documents_deleted": 1, "documents_kept": 1})
    );
    assert_eq!(fake.lock().unwrap().deleted_docs, [d2]);
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE action_type='delete' AND resource_type='source'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(details["kept_document_ids"], json!([d1]));
    assert_eq!(details["kept_document_ids_truncated"], false);
}

#[tokio::test]
async fn the_audit_row_says_when_the_kept_ids_were_cut_off() {
    let (e, fake) = fakes::sources_env("runs-kept-cap").await;
    let admin = crate::common::admin();
    let a = create(&e, website("Big")).await;
    let docs: Vec<String> = (1..=501).map(fakes::doc_id).collect();
    complete_run(&e, &fake, &a, &docs).await;
    let (s, body) = crate::common::send(
        &e.app,
        delete(&crate::common::j(&format!("/sources/{a}")), &admin),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(
        body["documents_kept"], 501,
        "the response keeps the full count"
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE action_type='delete' AND resource_type='source'",
    )
    .fetch_one(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(details["kept_document_ids"].as_array().unwrap().len(), 500);
    assert_eq!(details["kept_document_ids_truncated"], true);
    assert_eq!(details["documents_kept"], 501);
}

fn crawl(name: &str) -> Value {
    json!({"name": name, "type": "website", "target_store_id": fakes::LEAF,
           "config": {"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml",
                      "sitemap": {"include": ["/help/"]}}})
}

/// Starts a crawl/sitemap run whose parent finds one page per entry of `pages` and completes each page with those
/// documents (`None`: the page fails); returns the run id.
async fn crawl_run(
    e: &crate::common::TestEnv,
    fake: &fakes::Shared,
    id: &str,
    pages: &[Option<Vec<String>>],
) -> String {
    let (s, r) = resync(e, id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let parents = ids(&r["ingest_job_ids"]);
    assert_eq!(parents.len(), 1, "one parent job: {r}");
    let rid = r["id"].as_str().unwrap().to_string();
    let jobs = fakes::add_pages(fake, &parents[0], pages.len());
    for (j, docs) in jobs.iter().zip(pages) {
        match docs {
            Some(d) => fakes::complete(fake, j, d),
            None => fakes::fail(fake, j, "HTTP 404 from help.example.nl"),
        }
    }
    fakes::settle_parent(fake, &parents[0]);
    rid
}

#[tokio::test]
async fn a_sitemap_run_with_every_page_done_completes_and_replaces_with_their_documents() {
    let (e, fake) = fakes::sources_env("runs-sitemap-done").await;
    let id = create(&e, crawl("Help sitemap")).await;
    let (d1, d2, d3) = (fakes::doc_id(1), fakes::doc_id(2), fakes::doc_id(3));
    let r1 = crawl_run(
        &e,
        &fake,
        &id,
        &[Some(vec![d1.clone()]), Some(vec![d2.clone()])],
    )
    .await;
    let done1 = run(&e, &id, &r1).await;
    assert_eq!(done1["status"], "completed", "{done1}");
    assert_eq!(ids(&done1["document_ids"]), [d1.clone(), d2.clone()]);
    assert_eq!(done1["children"]["total"], 2);

    let r2 = crawl_run(&e, &fake, &id, &[Some(vec![d2.clone(), d3.clone()])]).await;
    let done2 = run(&e, &id, &r2).await;
    assert_eq!(done2["status"], "completed", "{done2}");
    assert_eq!(ids(&done2["document_ids"]), [d2, d3]);
    assert_eq!(done2["replaced_run_id"], r1.as_str());
    assert_eq!(
        fake.lock().unwrap().deleted_docs,
        [d1],
        "only the page the new run lacks"
    );
}

#[tokio::test]
async fn a_sitemap_run_that_discovers_no_page_fails_and_keeps_the_earlier_runs_documents() {
    let (e, fake) = fakes::sources_env("runs-sitemap-empty").await;
    let id = create(&e, crawl("Help sitemap")).await;
    let first = crawl_run(
        &e,
        &fake,
        &id,
        &[Some(vec![fakes::doc_id(1)]), Some(vec![fakes::doc_id(2)])],
    )
    .await;
    assert_eq!(run(&e, &id, &first).await["status"], "completed");

    // an empty urlset, or include/exclude filtering out every URL: the parent completes with 0 children
    let empty = crawl_run(&e, &fake, &id, &[]).await;
    let done = run(&e, &id, &empty).await;
    assert_eq!(done["status"], "failed", "{done}");
    assert!(
        done["error"]
            .as_str()
            .unwrap()
            .starts_with("no pages discovered:"),
        "{done}"
    );
    assert_eq!(
        done["replace_report"],
        Value::Null,
        "no replace on a failed run"
    );
    assert_eq!(done["replaced_run_id"], Value::Null);
    // a later read of the failed run does not replace either
    run(&e, &id, &empty).await;
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "the earlier run's documents stay in strata_rag"
    );
    let removed: bool =
        sqlx::query_scalar("SELECT documents_removed FROM source_runs WHERE id = $1::uuid")
            .bind(&first)
            .fetch_one(crate::common::pool(&e))
            .await
            .unwrap();
    assert!(!removed, "the earlier run still owns its documents");
}

#[tokio::test]
async fn one_failed_page_fails_a_sitemap_run_and_replaces_nothing() {
    let (e, fake) = fakes::sources_env("runs-sitemap-page-fail").await;
    let id = create(&e, crawl("Help sitemap")).await;
    let first = crawl_run(&e, &fake, &id, &[Some(vec![fakes::doc_id(1)])]).await;
    assert_eq!(run(&e, &id, &first).await["status"], "completed");

    let r2 = crawl_run(&e, &fake, &id, &[Some(vec![fakes::doc_id(2)]), None]).await;
    let done = run(&e, &id, &r2).await;
    assert_eq!(done["status"], "failed", "{done}");
    let err = done["error"].as_str().unwrap();
    assert!(
        err.starts_with("1 of 3 ingest jobs did not complete:")
            && err.contains("failed: fetch_failed: HTTP 404 from help.example.nl"),
        "{err}"
    );
    assert_eq!(ids(&done["document_ids"]), [fakes::doc_id(2)]);
    assert!(
        fake.lock().unwrap().deleted_docs.is_empty(),
        "no replace on a failed run"
    );
}

/// A job a running source run still follows is not deleted (409 `owned_by_active_run` naming the run), so the run's
/// outcome is not decided by a job vanishing under it. Once the run has finished, deleting its job is allowed and the
/// recorded outcome stays as it was.
#[tokio::test]
async fn deleting_a_job_of_an_unfinished_run_is_refused_until_the_run_finishes() {
    let (e, fake) = fakes::sources_env("runs-job-delete").await;
    let admin = crate::common::admin();
    let id = create(&e, website("Help centre")).await;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let run_id = r["id"].as_str().unwrap().to_string();
    let jobs = ids(&r["ingest_job_ids"]);
    let d1 = fakes::doc_id(1);
    fakes::complete(&fake, &jobs[0], std::slice::from_ref(&d1));

    // jobs[0] has ended, but its run still waits for jobs[1]
    let one = format!("/api/ingest/jobs/{}", jobs[0]);
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone(), b["error"]["run_id"].clone()),
        (
            StatusCode::CONFLICT,
            json!("owned_by_active_run"),
            json!(run_id)
        ),
        "{b}"
    );
    assert!(fake.lock().unwrap().jobs.contains_key(&jobs[0]));
    assert!(fake.lock().unwrap().deleted_jobs.is_empty());
    assert_eq!(run(&e, &id, &run_id).await["status"], "running");

    let d2 = fakes::doc_id(2);
    fakes::complete(&fake, &jobs[1], std::slice::from_ref(&d2));
    let (s, b) = crate::common::send(&e.app, delete(&one, &admin)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(fake.lock().unwrap().deleted_jobs, [jobs[0].clone()]);
    // the run finished (during the delete's refresh) with both jobs' documents, and keeps that outcome
    let done = run(&e, &id, &run_id).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(ids(&done["document_ids"]), [d1, d2]);

    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT status, details FROM audit_events WHERE resource_type = 'ingest_job' AND action_type = 'delete' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(
        (audit[0].0.as_str(), audit[0].1["error_code"].clone()),
        ("failure", json!("owned_by_active_run"))
    );
    assert_eq!(audit[1].0, "success");
}

/// A crawl/sitemap PAGE job is not in its run's `ingest_job_ids` (only the parent is): `active_owner` finds the run
/// through the parent's `child_job_ids`, so it is refused as `owned_by_active_run` while the run runs. Once the run has
/// finished, strata_ingest's own 409 `child_of_parent_job` comes through as itself with `parent_job_id` (not collapsed
/// to `ingest_conflict`), and deleting the parent removes the page jobs with it.
#[tokio::test]
async fn a_page_job_is_owned_through_its_parent_and_then_deletes_only_with_it() {
    let (e, fake) = fakes::sources_env("runs-page-job-delete").await;
    let admin = crate::common::admin();
    let id = create(&e, crawl("Help sitemap")).await;
    let (s, r) = resync(&e, &id).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let run_id = r["id"].as_str().unwrap().to_string();
    let parents = ids(&r["ingest_job_ids"]);
    assert_eq!(parents.len(), 1, "{r}");
    let pages = fakes::add_pages(&fake, &parents[0], 2);
    fakes::complete(&fake, &pages[0], &[fakes::doc_id(1)]);
    assert!(
        !ids(&run(&e, &id, &run_id).await["ingest_job_ids"]).contains(&pages[0]),
        "the run lists only the parent: the page job is found through it"
    );

    // pages[1] still queued: the run is running, and pages[0] (ended) is its job through the parent
    let page = format!("/api/ingest/jobs/{}", pages[0]);
    let (s, b) = crate::common::send(&e.app, delete(&page, &admin)).await;
    assert_eq!(
        (s, b["error"]["code"].clone(), b["error"]["run_id"].clone()),
        (
            StatusCode::CONFLICT,
            json!("owned_by_active_run"),
            json!(run_id)
        ),
        "{b}"
    );
    assert!(fake.lock().unwrap().jobs.contains_key(&pages[0]));

    // the run finishes; strata_ingest refuses the page job alone and names its parent
    fakes::complete(&fake, &pages[1], &[fakes::doc_id(2)]);
    fakes::settle_parent(&fake, &parents[0]);
    let (s, b) = crate::common::send(&e.app, delete(&page, &admin)).await;
    assert_eq!(
        (
            s,
            b["error"]["code"].clone(),
            b["error"]["parent_job_id"].clone()
        ),
        (
            StatusCode::CONFLICT,
            json!("child_of_parent_job"),
            json!(parents[0])
        ),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&parents[0]),
        "{b}"
    );
    assert!(fake.lock().unwrap().jobs.contains_key(&pages[0]));
    assert_eq!(run(&e, &id, &run_id).await["status"], "completed");

    let (s, b) = crate::common::send(
        &e.app,
        delete(&format!("/api/ingest/jobs/{}", parents[0]), &admin),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["deleted_jobs"], 3, "the parent and its two page jobs");
    let g = fake.lock().unwrap();
    assert!(!g.jobs.contains_key(&pages[0]) && !g.jobs.contains_key(&pages[1]));
}
