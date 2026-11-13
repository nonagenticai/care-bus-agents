use crate::common::{Part, fakes};
use axum::http::StatusCode;
use serde_json::json;

const XLSX_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// A two-sheet workbook ("Intro", "Customers"): shared strings, an inline string, a number and a
/// boolean, built with the zip writer (no binary fixture).
fn workbook() -> Vec<u8> {
    use std::io::Write;
    const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let sheet = |data: &str| {
        format!(r#"<?xml version="1.0"?><worksheet {NS}><sheetData>{data}</sheetData></worksheet>"#)
    };
    let parts = [
        (
            "xl/workbook.xml",
            format!(
                r#"<?xml version="1.0"?><workbook {NS}><sheets><sheet name="Intro" sheetId="1" r:id="rId1"/><sheet name="Customers" sheetId="2" r:id="rId2"/></sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#.to_string(),
        ),
        (
            "xl/sharedStrings.xml",
            format!(r#"<?xml version="1.0"?><sst {NS}><si><t>customer_id</t></si><si><t>name</t></si><si><t>Anna de Vries</t></si></sst>"#),
        ),
        (
            "xl/worksheets/sheet1.xml",
            sheet(r#"<row r="1"><c r="A1" t="inlineStr"><is><t>read me</t></is></c></row>"#),
        ),
        (
            "xl/worksheets/sheet2.xml",
            sheet(concat!(
                r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="inlineStr"><is><t>active</t></is></c></row>"#,
                r#"<row r="2"><c r="A2"><v>10482</v></c><c r="B2" t="s"><v>2</v></c><c r="C2" t="b"><v>1</v></c></row>"#,
                r#"<row r="3"><c r="A3"><v>10483</v></c><c r="C3" t="b"><v>0</v></c></row>"#,
            )),
        ),
    ];
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in parts {
        w.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn file<'a>(name: &'a str, body: &'a [u8]) -> Part<'a> {
    Part {
        name: "file",
        file_name: Some(name),
        content_type: Some("text/csv"),
        body,
    }
}

#[tokio::test]
async fn csv_preview_returns_columns_samples_and_count_and_stores_nothing() {
    let (e, _) = fakes::sources_env("src-preview").await;
    let admin = crate::common::admin();
    let csv = b"customer_id;name;email\nC-10482;Anna de Vries;anna@example.nl\nC-10483;Bram Jansen;bram@example.nl\n";
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::multipart(
            &crate::common::j("/sources/preview-columns"),
            Some(&admin),
            &[file("customers.csv", csv)],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["format"], "csv");
    assert_eq!(b["columns"], json!(["customer_id", "name", "email"]));
    assert_eq!(b["row_count"], 2);
    assert_eq!(
        b["sample_rows"][0],
        json!(["C-10482", "Anna de Vries", "anna@example.nl"])
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 0);
    let audit = crate::common::audit_text(&e).await;
    assert!(
        audit.contains("\"preview\"") && !audit.contains("Anna"),
        "audited without data: {audit}"
    );
}

#[tokio::test]
async fn xlsx_preview_reads_the_named_sheet_or_the_first() {
    let (e, _) = fakes::sources_env("src-preview-xlsx").await;
    let admin = crate::common::admin();
    let uri = &crate::common::j("/sources/preview-columns");
    let wb: &'static [u8] = Box::leak(workbook().into_boxed_slice());
    let xlsx = || Part {
        name: "file",
        file_name: Some("customers.xlsx"),
        content_type: Some(XLSX_TYPE),
        body: wb,
    };
    let sheet = |name: &'static str| Part {
        name: "sheet",
        file_name: None,
        content_type: None,
        body: name.as_bytes(),
    };

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::multipart(uri, Some(&admin), &[xlsx(), sheet("Customers")]),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["format"], "xlsx");
    assert_eq!(b["columns"], json!(["customer_id", "name", "active"]));
    assert_eq!(b["row_count"], 2);
    assert_eq!(
        b["sample_rows"],
        json!([["10482", "Anna de Vries", "TRUE"], ["10483", "", "FALSE"]])
    );

    // No sheet field (or a blank one): the first sheet.
    for parts in [vec![xlsx()], vec![sheet(" "), xlsx()]] {
        let (s, b) =
            crate::common::send(&e.app, crate::common::multipart(uri, Some(&admin), &parts)).await;
        assert_eq!(
            (s, b["columns"].clone()),
            (StatusCode::OK, json!(["read me"]))
        );
    }

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::multipart(uri, Some(&admin), &[xlsx(), sheet("Orders")]),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("sheet_not_found"))
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("\"Orders\""),
        "{b}"
    );

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
        .fetch_one(crate::common::pool(&e))
        .await
        .unwrap();
    assert_eq!(n, 0);
    let audit = crate::common::audit_text(&e).await;
    assert!(
        audit.contains("\"xlsx\"") && !audit.contains("Anna"),
        "audited without data: {audit}"
    );
}

#[tokio::test]
async fn preview_refuses_wrong_uploads_with_the_right_status() {
    let (e, _) = fakes::sources_env("src-preview-bad").await;
    let admin = crate::common::admin();
    let uri = &crate::common::j("/sources/preview-columns");
    let send = |parts: Vec<Part<'static>>| {
        let app = e.app.clone();
        let admin = admin.clone();
        async move {
            crate::common::send(&app, crate::common::multipart(uri, Some(&admin), &parts)).await
        }
    };
    // An .xlsx that is not a zip is a 400 like an unreadable CSV, by name or by content type.
    let (s, b) = send(vec![file("sheet.xlsx", b"PK\x03\x04")]).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("unreadable_table"))
    );
    let xlsx_by_type = Part {
        name: "file",
        file_name: Some("data.csv"),
        content_type: Some(XLSX_TYPE),
        body: b"a,b\n1,2\n",
    };
    let (s, b) = send(vec![xlsx_by_type]).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("unreadable_table")),
        "the spreadsheet content type wins over the .csv name: {b}"
    );
    let (s, _) = send(vec![file("report.pdf", b"%PDF")]).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (s, b) = send(vec![Part {
        name: "sheet",
        file_name: None,
        content_type: None,
        body: b"Customers",
    }])
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("no_file"))
    );
    let (s, b) = send(vec![file("empty.csv", b"\n")]).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("unreadable_table"))
    );
    // 4,096 duplicate headers (an 8 KB line): refused fast, not deduplicated in cubic time
    let wide: &'static [u8] = Box::leak(
        format!("{}\n1\n", vec!["a"; 4_096].join(","))
            .into_bytes()
            .into_boxed_slice(),
    );
    let t = std::time::Instant::now();
    let (s, b) = send(vec![file("wide.csv", wide)]).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("too_many_columns")),
        "{b}"
    );
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t.elapsed()
    );
    let big: &'static [u8] = Box::leak(vec![b'a'; 11 * 1024 * 1024].into_boxed_slice());
    let (s, b) = send(vec![file("big.csv", big)]).await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::PAYLOAD_TOO_LARGE, json!("payload_too_large"))
    );
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json("POST", uri, Some(&admin), json!({"file": "x"})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body"))
    );
    let viewer = crate::common::user();
    let (s, _) = crate::common::send(
        &e.app,
        crate::common::multipart(uri, Some(&viewer), &[file("a.csv", b"a\n1\n")]),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

fn field<'a>(name: &'a str, value: &'a str) -> Part<'a> {
    Part {
        name,
        file_name: None,
        content_type: None,
        body: value.as_bytes(),
    }
}

/// The defect: a CSV with no header row lost its first row to the header. `header_row=false` numbers the
/// columns and keeps row 1 as data; `true` (or no field) is the old reading.
#[tokio::test]
async fn header_row_false_keeps_the_first_row_as_data() {
    let (e, _) = fakes::sources_env("src-preview-nohdr").await;
    let admin = crate::common::admin();
    let uri = &crate::common::j("/sources/preview-columns");
    let csv: &[u8] =
        b"C-10482;Anna de Vries;anna@example.nl\nC-10483;Bram Jansen;bram@example.nl\n";
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::multipart(
            uri,
            Some(&admin),
            &[field("header_row", "false"), file("customers.csv", csv)],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["columns"], json!(["column_1", "column_2", "column_3"]));
    assert_eq!(b["row_count"], 2);
    assert_eq!(
        b["sample_rows"][0],
        json!(["C-10482", "Anna de Vries", "anna@example.nl"])
    );

    for parts in [
        vec![file("customers.csv", csv), field("header_row", "true")],
        vec![file("customers.csv", csv)],
    ] {
        let (s, b) =
            crate::common::send(&e.app, crate::common::multipart(uri, Some(&admin), &parts)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["columns"][0], "C-10482");
        assert_eq!(b["row_count"], 1);
    }

    let (s, b) = crate::common::send(
        &e.app,
        crate::common::multipart(
            uri,
            Some(&admin),
            &[file("customers.csv", csv), field("header_row", "maybe")],
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body")),
        "{b}"
    );
}
