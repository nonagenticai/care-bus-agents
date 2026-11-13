use crate::common::fakes;
use axum::http::StatusCode;
use serde_json::{Value, json};

const SECRET: &str = "k-123-SECRETMARK-value";

fn api_source(url: &str) -> Value {
    json!({"name": "Customer API", "type": "api",
           "config": {"url": url, "auth": {"type": "header", "header_name": "X-Api-Key"}, "options": {"language": "nl"}},
           "mapping": [{"source_field": "customer_id", "entity": "Customer", "attribute": "customer_id"},
                       {"source_field": "email", "entity": "Customer", "attribute": "email"}],
           "target_store_id": fakes::OTHER_LEAF, "schedule": "0 3 * * *",
           "credentials": {"secret": SECRET}})
}

fn website_source() -> Value {
    json!({"name": "Help centre", "type": "website",
           "config": {"urls": ["https://help.example.nl/returns", "https://help.example.nl/warranty"]},
           "target_store_id": fakes::LEAF})
}

async fn post(e: &crate::common::TestEnv, body: Value) -> (StatusCode, Value) {
    let admin = crate::common::admin();
    crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/sources"), Some(&admin), body),
    )
    .await
}

async fn stored(e: &crate::common::TestEnv, id: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    sqlx::query_as("SELECT credentials_enc, credentials_nonce FROM sources WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(crate::common::pool(e))
        .await
        .unwrap()
}

fn decrypt(id: &str, enc: &[u8], nonce: &[u8]) -> String {
    let id = uuid::Uuid::parse_str(id).unwrap();
    String::from_utf8(
        crate::common::source_key()
            .open(id.as_bytes(), nonce, enc)
            .unwrap(),
    )
    .unwrap()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test]
async fn credentials_are_write_only_and_encrypted_at_rest() {
    let (e, _) = fakes::sources_env("src-create").await;
    let viewer = crate::common::user();
    let (s, created) = post(&e, api_source("https://api.example.nl/v2/customers")).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["has_credentials"], true);
    assert_eq!(created["type"], "api");
    assert_eq!(created["last_run"], Value::Null);
    assert_eq!(created["mapping"].as_array().unwrap().len(), 2);
    assert_eq!(created["updated_by"], "u-admin");
    assert!(created.get("credentials").is_none());

    let (_, one) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j(&format!("/sources/{id}")), Some(&viewer)),
    )
    .await;
    let (_, list) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/sources"), Some(&viewer)),
    )
    .await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["next"], Value::Null);
    for body in [&created, &one, &list] {
        assert!(
            !body.to_string().contains("SECRETMARK"),
            "secret leaked: {body}"
        );
    }
    assert!(!crate::common::audit_text(&e).await.contains("SECRETMARK"));

    let (enc, nonce) = stored(&e, &id).await;
    let (enc, nonce) = (enc.unwrap(), nonce.unwrap());
    assert_eq!(nonce.len(), 12);
    assert!(!contains(&enc, b"SECRETMARK"), "stored as ciphertext");
    assert_eq!(
        decrypt(&id, &enc, &nonce),
        format!(r#"{{"secret":"{SECRET}"}}"#)
    );
    let other = uuid::Uuid::new_v4();
    assert!(
        crate::common::source_key()
            .open(other.as_bytes(), &nonce, &enc)
            .is_err(),
        "bound to its own source id"
    );
}

#[tokio::test]
async fn only_the_top_level_credentials_key_is_exempt_from_the_secret_rejector() {
    let (e, _) = fakes::sources_env("src-exempt").await;
    let mut in_config = api_source("https://api.example.nl/v2/customers");
    in_config["config"]["auth"]["token"] = json!(SECRET);
    let mut nested = website_source();
    nested["config"]["credentials"] = json!({"secret": SECRET});
    let mut in_url = api_source("https://api.example.nl/v2/customers?api_key=abc123");
    in_url["name"] = json!("Keyed URL");
    let mut long_query =
        api_source("https://api.example.nl/c?x=Zx81Qm0aPv7LhK2cYt5NwR9eB3uJd6Fg0SxT4oHq");
    long_query["name"] = json!("Opaque query");
    for body in [in_config, nested, in_url, long_query] {
        let (s, b) = post(&e, body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "secret_value_rejected");
        let m = b["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("credentials field") && !m.contains("secret_ref"),
            "{m}"
        );
        assert!(!b.to_string().contains("SECRETMARK"));
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 0, "rejected writes store nothing");
}

#[tokio::test]
async fn put_keeps_replaces_and_clears_credentials() {
    let (e, _) = fakes::sources_env("src-put").await;
    let admin = crate::common::admin();
    let (_, created) = post(&e, api_source("https://api.example.nl/v2/customers")).await;
    let id = created["id"].as_str().unwrap().to_string();
    let uri = crate::common::j(&format!("/sources/{id}"));
    let (enc0, _) = stored(&e, &id).await;

    let mut keep = api_source("https://api.example.nl/v3/customers");
    keep.as_object_mut().unwrap().remove("credentials");
    let (s, b) =
        crate::common::send(&e.app, crate::common::json("PUT", &uri, Some(&admin), keep)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["has_credentials"], true);
    assert_eq!(b["config"]["url"], "https://api.example.nl/v3/customers");
    assert_eq!(
        stored(&e, &id).await.0,
        enc0,
        "absent = keep, byte for byte"
    );

    let mut replace = api_source("https://api.example.nl/v3/customers");
    replace["credentials"] = json!({"secret": "new-SECRETMARK-2"});
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::json("PUT", &uri, Some(&admin), replace),
    )
    .await;
    assert_eq!(b["has_credentials"], true);
    let (enc, nonce) = stored(&e, &id).await;
    assert_eq!(
        decrypt(&id, enc.as_deref().unwrap(), nonce.as_deref().unwrap()),
        r#"{"secret":"new-SECRETMARK-2"}"#
    );

    let mut clear = api_source("https://api.example.nl/v3/customers");
    clear["credentials"] = Value::Null;
    let (_, b) = crate::common::send(
        &e.app,
        crate::common::json("PUT", &uri, Some(&admin), clear),
    )
    .await;
    assert_eq!(b["has_credentials"], false);
    assert_eq!(stored(&e, &id).await, (None, None));

    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_events WHERE resource_type='source' AND action_type='update' ORDER BY id",
    )
    .fetch_all(crate::common::pool(&e))
    .await
    .unwrap();
    let words: Vec<&str> = details
        .iter()
        .map(|d| d["credentials"].as_str().unwrap())
        .collect();
    assert_eq!(words, ["unchanged", "replaced", "cleared"]);
    assert!(!crate::common::audit_text(&e).await.contains("SECRETMARK"));

    // switching to a type that cannot hold credentials while they are still stored is refused
    let mut to_web = website_source();
    to_web["name"] = json!("Customer API");
    crate::common::send(
        &e.app,
        crate::common::json("PUT", &uri, Some(&admin), {
            let mut b = api_source("https://api.example.nl/v3/customers");
            b["credentials"] = json!({"secret": "again"});
            b
        }),
    )
    .await;
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("PUT", &uri, Some(&admin), to_web),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(b["error"]["code"], "credentials_not_supported");
}

#[tokio::test]
async fn validation_refuses_bad_sources_with_a_named_code() {
    let (e, _) = fakes::sources_env("src-validate").await;
    let api = || api_source("https://api.example.nl/v2/customers");
    let web = website_source;
    let cases: Vec<(Value, &str)> = vec![
        (
            {
                let mut b = web();
                b["mapping"] = api()["mapping"].clone();
                b
            },
            "mapping_not_supported",
        ),
        (
            {
                let mut b = web();
                b["credentials"] = json!({"secret": "x"});
                b
            },
            "credentials_not_supported",
        ),
        (
            {
                let mut b = api();
                b["config"]["auth"] = json!({"type": "none"});
                b
            },
            "credentials_not_supported",
        ),
        (
            {
                let mut b = api();
                b["mapping"][0]["attribute"] = json!("nonexistent");
                b
            },
            "unknown_attribute",
        ),
        (
            {
                let mut b = api();
                b["mapping"][1]["source_field"] = json!("customer_id");
                b
            },
            "invalid_mapping",
        ),
        (
            {
                let mut b = api();
                b["schedule"] = json!("every day");
                b
            },
            "invalid_schedule",
        ),
        (
            {
                let mut b = api();
                b["target_store_id"] = json!(uuid::Uuid::new_v4());
                b
            },
            "unknown_store",
        ),
        (
            {
                let mut b = api();
                b["target_store_id"] = json!(fakes::ROUTER);
                b
            },
            "store_not_leaf",
        ),
        (
            {
                let mut b = api();
                b["config"]["url"] = json!("http://10.0.0.1/x");
                b
            },
            "invalid_url",
        ),
        (
            {
                let mut b = api();
                b["config"]["extra"] = json!(1);
                b
            },
            "invalid_config",
        ),
        (
            {
                let mut b = api();
                b["config"]["auth"] = json!({"type": "header", "header_name": "Host"});
                b
            },
            "invalid_config",
        ),
        (
            {
                let mut b = api();
                b["config"]["options"] = json!({"ocr": "auto", "speed": 9});
                b
            },
            "invalid_config",
        ),
        (
            {
                let mut b = api();
                b["type"] = json!("datatable");
                b
            },
            "invalid_config",
        ),
        (
            {
                let mut b = web();
                b["config"]["urls"] = json!([]);
                b
            },
            "invalid_config",
        ),
        (
            {
                let mut b = web();
                b["type"] = json!("ftp");
                b
            },
            "invalid_body",
        ),
        (
            {
                let mut b = web();
                b["name"] = json!("  ");
                b
            },
            "invalid_name",
        ),
        (
            {
                let mut b = api();
                b["credentials"] = json!({"secret": ""});
                b
            },
            "invalid_credentials",
        ),
        (
            {
                let mut b = api();
                b["credentials"] = json!({"secret": "a\r\nX-Evil: 1"});
                b
            },
            "invalid_credentials",
        ),
        (
            {
                let mut b = api();
                b["credentials"] = json!({"password": "x"});
                b
            },
            "invalid_credentials",
        ),
        (json!([1]), "invalid_body"),
    ];
    for (body, code) in cases {
        let (s, b) = post(&e, body.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{code}: {b}");
        assert_eq!(b["error"]["code"], code, "{body}");
    }
    assert_eq!(post(&e, api()).await.0, StatusCode::CREATED);
    let mut dt = api();
    dt["name"] = json!("Customers sheet");
    dt["type"] = json!("datatable");
    dt["config"]["format"] = json!("csv");
    let mut bad_flag = dt.clone();
    bad_flag["name"] = json!("Headerless flag as text");
    bad_flag["config"]["header_row"] = json!("no");
    let (s, b) = post(&e, bad_flag).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_config")),
        "header_row must be a boolean: {b}"
    );
    let mut headerless = dt.clone();
    headerless["name"] = json!("Headerless sheet");
    headerless["config"]["header_row"] = json!(false);
    let (s, b) = post(&e, headerless).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["config"]["header_row"], json!(false), "{b}");
    assert_eq!(post(&e, dt).await.0, StatusCode::CREATED);
    let (s, b) = post(&e, api()).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("source_exists"))
    );

    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::json("POST", &crate::common::j("/sources"), Some(&viewer), web()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::with_tenant(
            crate::common::json(
                "POST",
                &crate::common::j("/sources"),
                Some("svc-test-token"),
                web(),
            ),
            crate::common::BOOT_TENANT,
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a service token never passes Admin"
    );
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/sources/not-a-uuid"), Some(&viewer)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let ghost = crate::common::j(&format!("/sources/{}", uuid::Uuid::new_v4()));
    assert_eq!(
        crate::common::send(&e.app, crate::common::get(&ghost, Some(&viewer)))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn list_pages_by_name() {
    let (e, _) = fakes::sources_env("src-page").await;
    for n in ["c", "a", "b"] {
        let mut b = website_source();
        b["name"] = json!(n);
        assert_eq!(post(&e, b).await.0, StatusCode::CREATED);
    }
    let viewer = crate::common::user();
    let (_, p1) = crate::common::send(
        &e.app,
        crate::common::get(&crate::common::j("/sources?limit=2"), Some(&viewer)),
    )
    .await;
    let names: Vec<&str> = p1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!((names, p1["next"].clone()), (vec!["a", "b"], json!("b")));
    let (_, p2) = crate::common::send(
        &e.app,
        crate::common::get(
            &crate::common::j("/sources?limit=2&before=b"),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(p2["items"][0]["name"], "c");
    assert_eq!(p2["next"], Value::Null);
}

#[tokio::test]
async fn without_a_source_key_credentials_fail_closed() {
    let (e, _) = fakes::sources_env_with("src-nokey", |c| c.source_key = None).await;
    let (s, b) = post(&e, api_source("https://api.example.nl/v2/customers")).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["code"], "not_configured");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing is stored without encryption");
    assert_eq!(
        post(&e, website_source()).await.0,
        StatusCode::CREATED,
        "no credentials, no key needed"
    );
}

#[tokio::test]
async fn strata_rag_unavailable_on_save_is_503_never_a_guess() {
    let (e, fake) = fakes::sources_env("src-ragdown").await;
    fake.lock().unwrap().rag_down = true;
    let (s, b) = post(&e, website_source()).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["message"], "strata_rag is unavailable");
    let (e2, _) = fakes::sources_env_with("src-ragunset", |c| c.strata_rag_url = None).await;
    assert_eq!(
        post(&e2, website_source()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
