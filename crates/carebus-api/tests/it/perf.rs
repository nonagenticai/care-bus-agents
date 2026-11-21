//! Latency benchmarks for citation verification. Every citation re-parses the cited step's 40 000-character result,
//! so these benches measure the current cost of the verify loop per source kind. Ignored by default; CI never runs
//! them:
//!
//! ```text
//! cargo test -p carebus-api --test it -- --ignored perf --nocapture
//! ```

use std::time::Instant;

use carebus_api::research::SourceKind;
use carebus_api::research::tools::{Resolution, Resolver};
use carebus_api::research::verify::{
    CitationIn, LITERATURE_SEARCH, MIN_QUOTE_CHARS, MIN_QUOTE_WORDS, ToolStepView, VerifyStatus,
    normalise, verify_citation,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// Every step's stored result sits at [`carebus_api::research::TOOL_TEXT_CAP`].
const STEP_TEXT_CHARS: usize = 40_000;
/// Each bench times this many citations of its own kind: ten steps of that kind, twenty citations each.
const CITATIONS_PER_KIND: usize = 200;
const CITATIONS_PER_STEP: usize = 20;
const STEPS_PER_KIND: usize = 10;

/// Realistic prose; every quote a web fixture uses is one of these sentences verbatim. ASCII only, so a quote is
/// also a literal substring of the serialised JSON.
const SENTENCES: [&str; 24] = [
    "The tenancy agreement may be terminated only in writing and with a notice period of three months.",
    "A landlord who seeks possession must first serve a valid notice that states the ground relied upon.",
    "The tenant is entitled to a written statement of the reasons for any increase in the monthly rent.",
    "Where the dwelling is unfit for habitation, the tenant may withhold rent only after a formal complaint.",
    "The parties agreed that repairs requested by the tenant would be completed within fourteen days.",
    "Notice served by electronic mail is effective only when the agreement expressly permits that method.",
    "The court may postpone the eviction where the tenant shows that suitable alternative housing is unavailable.",
    "Rent arrears alone do not justify termination unless the landlord has given a written warning.",
    "The landlord must return the deposit within one month of the end of the tenancy.",
    "Any deduction from the deposit must be itemised and supported by receipts or an inspection report.",
    "The tenant has the right to receive copies of all documents signed in connection with the lease.",
    "A clause that waives the tenant's statutory rights is void unless the law expressly allows it.",
    "The property was inspected jointly at the start and at the end of the tenancy, and a protocol was signed.",
    "The landlord is responsible for maintaining the building in a condition suitable for its intended use.",
    "The tenant must notify the landlord of any defect that could cause further damage to the property.",
    "Termination for breach of contract requires a prior written demand and a reasonable cure period.",
    "The notice period runs from the first day of the month following delivery of the written notice.",
    "Disputes concerning the amount of the service charge are heard by the district court.",
    "The agreement may be renewed for a further fixed term if neither party objects in writing.",
    "The landlord may enter the dwelling only with the tenant's consent or in an emergency.",
    "Subletting requires the landlord's prior written consent, which may not be unreasonably withheld.",
    "The tenant is obliged to pay the utility charges shown on the meters installed in the dwelling.",
    "A termination notice that omits the statutory warning is ineffective and must be served again.",
    "The parties confirmed that the inventory attached to the agreement formed an integral part of it.",
];

const FILLER: &str = "The parties recorded the agreed terms in writing before the hearing ";

struct Paper {
    id: Uuid,
    doi: String,
    pmid: String,
    title: String,
    quote: String,
    abstract_text: String,
}

struct Fixture {
    steps: Vec<ToolStepView>,
    web: Vec<CitationIn>,
    literature: Vec<CitationIn>,
}

/// Fillers of exactly `n` ASCII characters.
fn filler(n: usize) -> String {
    FILLER.repeat(n / FILLER.len() + 1)[..n].to_string()
}

/// Exactly `chars` characters of prose: whole sentences, then filler.
fn prose(seed: usize, chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    let mut i = seed;
    while out.len() + 1 + SENTENCES[i % SENTENCES.len()].len() <= chars {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(SENTENCES[i % SENTENCES.len()]);
        i += 5;
    }
    out.push_str(&filler(chars - out.len()));
    out
}

fn paper(step: usize, idx: usize) -> Paper {
    let quote = format!(
        "Study {step}-{idx} of residential tenancy files found that written notice was honoured in most proceedings \
         and that only a minority of disputes reached the appellate stage."
    );
    Paper {
        id: Uuid::from_u128(((step as u128) << 32) | (idx as u128 + 1)),
        doi: format!("10.5555/tenancy.{step}.{idx}"),
        pmid: (30_000_000 + step * 100 + idx).to_string(),
        title: format!(
            "Written notice and termination grounds in residential tenancies: cohort {step}-{idx}"
        ),
        abstract_text: format!("{quote} {}", prose(step * 20 + idx, 1_400)),
        quote,
    }
}

fn records(papers: &[Paper]) -> Vec<Value> {
    papers
        .iter()
        .map(|p| {
            json!({
                "id": p.id.to_string(),
                "doi": p.doi,
                "pmid": p.pmid,
                "title": p.title,
                "abstract": p.abstract_text,
            })
        })
        .collect()
}

fn fixture() -> Fixture {
    let mut steps = Vec::with_capacity(2 * STEPS_PER_KIND);
    let mut web = Vec::with_capacity(CITATIONS_PER_KIND);
    let mut literature = Vec::with_capacity(CITATIONS_PER_KIND);
    for s in 0..STEPS_PER_KIND {
        let ordinal = s as i32;
        let url = format!("https://example.test/tenancy/{s}");
        let title = format!("Tenancy guidance note {s}");
        let body = prose(s * 13 + 1, STEP_TEXT_CHARS - 1_000);
        let partial = json!({"url": url, "title": title, "text": body}).to_string();
        let body = format!("{body}{}", filler(STEP_TEXT_CHARS - partial.len()));
        let text = json!({"url": url, "title": title, "text": body}).to_string();
        steps.push(ToolStepView {
            ordinal,
            name: "web_scrape".into(),
            arguments: json!({"url": url}),
            text,
            truncated: false,
        });
        for j in 0..CITATIONS_PER_STEP {
            web.push(CitationIn {
                source_kind: SourceKind::Web,
                locator: url.clone(),
                title: Some(title.clone()),
                quote: SENTENCES[(s * 7 + j * 11) % SENTENCES.len()].to_string(),
                step_ordinal: Some(ordinal),
            });
        }
    }
    for s in 0..STEPS_PER_KIND {
        let ordinal = (STEPS_PER_KIND + s) as i32;
        let mut papers: Vec<Paper> = (0..CITATIONS_PER_STEP).map(|j| paper(s, j)).collect();
        let partial = json!({"items": records(&papers)}).to_string();
        let pad = STEP_TEXT_CHARS - partial.len();
        papers
            .last_mut()
            .unwrap()
            .abstract_text
            .push_str(&filler(pad));
        let text = json!({"items": records(&papers)}).to_string();
        steps.push(ToolStepView {
            ordinal,
            name: LITERATURE_SEARCH.into(),
            arguments: json!({"query": format!("written notice tenancy {s}")}),
            text,
            truncated: false,
        });
        for (j, p) in papers.iter().enumerate() {
            literature.push(CitationIn {
                source_kind: SourceKind::Literature,
                locator: if j % 2 == 0 {
                    p.id.to_string()
                } else {
                    format!("doi:{}", p.doi)
                },
                title: Some(p.title.clone()),
                quote: p.quote.clone(),
                step_ordinal: Some(ordinal),
            });
        }
    }
    Fixture {
        steps,
        web,
        literature,
    }
}

/// Web and literature paths never reach the register; literature citations in the fixture are resolved locally.
struct MissingResolver;

impl Resolver for MissingResolver {
    async fn resolve(&self, _: Uuid, _: SourceKind, _: &str) -> Resolution {
        Resolution::Missing("not in the register".into())
    }
}

fn report(kind: &str, n: usize, elapsed: std::time::Duration) {
    let ms = elapsed.as_secs_f64() * 1_000.0;
    let per_citation = elapsed.as_secs_f64() * 1_000_000.0 / n as f64;
    println!("perf verify {kind}: {n} citations in {ms:.1}ms = {per_citation:.1} µs/citation");
}

/// Cost of verifying 200 citations whose quotes live in `web_scrape` results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: cargo test -p carebus-api --test it -- --ignored perf --nocapture"]
async fn perf_verify_web() {
    let fixture = fixture();
    let resolver = MissingResolver;
    let started = Instant::now();
    for citation in &fixture.web {
        let verdict = verify_citation(Uuid::nil(), citation, &fixture.steps, &resolver).await;
        std::hint::black_box(verdict.status);
    }
    report("web", fixture.web.len(), started.elapsed());
}

/// Cost of verifying 200 citations whose quotes live in `research_literature_search` results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: cargo test -p carebus-api --test it -- --ignored perf --nocapture"]
async fn perf_verify_literature() {
    let fixture = fixture();
    let resolver = MissingResolver;
    let started = Instant::now();
    for citation in &fixture.literature {
        let verdict = verify_citation(Uuid::nil(), citation, &fixture.steps, &resolver).await;
        std::hint::black_box(verdict.status);
    }
    report("literature", fixture.literature.len(), started.elapsed());
}

/// Stops both benches from silently measuring an early return: the fixtures are at the cap, every quote is long
/// enough to be considered, and four samples verify.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bench_fixtures_are_well_formed() {
    let fixture = fixture();
    let resolver = MissingResolver;
    assert_eq!(fixture.steps.len(), 2 * STEPS_PER_KIND);
    assert_eq!(fixture.web.len(), CITATIONS_PER_KIND);
    assert_eq!(fixture.literature.len(), CITATIONS_PER_KIND);
    for step in &fixture.steps {
        let len = step.text.chars().count();
        assert_eq!(
            len,
            STEP_TEXT_CHARS,
            "step {ordinal} is not at the cap",
            ordinal = step.ordinal
        );
    }
    for citation in fixture.web.iter().chain(&fixture.literature) {
        let quote = normalise(&citation.quote);
        assert!(
            quote.chars().count() >= MIN_QUOTE_CHARS
                && quote.split_whitespace().count() >= MIN_QUOTE_WORDS,
            "quote is below the minimum: {}",
            citation.quote
        );
    }
    let samples = [
        &fixture.web[0],
        &fixture.web[fixture.web.len() / 2],
        &fixture.literature[0],
        &fixture.literature[fixture.literature.len() / 2],
    ];
    for citation in samples {
        let verdict = verify_citation(Uuid::nil(), citation, &fixture.steps, &resolver).await;
        assert_eq!(
            verdict.status,
            VerifyStatus::Verified,
            "{:?} citation did not verify: {:?}",
            citation.source_kind,
            verdict.detail
        );
    }
}
