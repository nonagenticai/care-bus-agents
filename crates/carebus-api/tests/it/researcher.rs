//! The research runner end to end against Postgres (testcontainers, migrations 0001..0020, the RLS runtime role),
//! with a scripted model and fake tools: checkpointed steps, claims/citations, deterministic verification, the final
//! memo; resume after a crash; cancel (also while verifying); budget exhaustion; the claim-attempt cap; metering of
//! every model call (0023); the lease heartbeat during a slow model call; the tenant's `external_llm_allowed` gate.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use carebus_api::research::SourceKind;
use carebus_api::research::llm::{
    ChatReply, ChatRequest, DefaultLlm, Llm, LlmEndpoint, LlmError, Usage,
};
use carebus_api::research::prompts::{LEGAL_DISCLAIMER, LEGAL_REPORT_TITLE, UNVERIFIED_HEADING};
use carebus_api::research::runner::{Outcome, Runner, RunnerConfig};
use carebus_api::research::store::{self, RunScope};
use carebus_api::research::tools::{
    Resolution, Resolver, ToolCtx, ToolOutcome, ToolSet, function_spec,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::watch;
use uuid::Uuid;

const ELI_TEXT: &str = r#"{"eli":"DU/1964/59","in_force":true,"text":"Art. 133. § 1. Ojciec i matka obowiązani są do świadczeń alimentacyjnych względem dziecka."}"#;
const WEB_TEXT: &str = "Strona kancelarii. Termin wynosi 14 dni od doręczenia.";

/// (message count, tool_choice, names of the tools offered) per model call.
type Seen = Vec<(usize, Option<Value>, Vec<String>)>;

/// A scripted model: replies in order; records (message count, tool_choice, tools offered) of every call.
#[derive(Clone, Default)]
struct Script {
    replies: Arc<Mutex<VecDeque<Value>>>,
    seen: Arc<Mutex<Seen>>,
    /// The `X-Actor-Id` each model call's endpoint carries.
    actors: Arc<Mutex<Vec<Option<Uuid>>>>,
}

impl Script {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into())),
            seen: Arc::default(),
            actors: Arc::default(),
        }
    }
    fn seen(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }
}

impl Llm for Script {
    async fn chat(&self, ep: &LlmEndpoint, req: &ChatRequest) -> Result<ChatReply, LlmError> {
        self.actors.lock().unwrap().push(ep.actor);
        self.seen.lock().unwrap().push((
            req.messages.len(),
            req.tool_choice.clone(),
            req.tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap_or("").to_string())
                .collect(),
        ));
        let next = self.replies.lock().unwrap().pop_front();
        match next {
            Some(message) => Ok(ChatReply {
                message,
                finish_reason: Some("tool_calls".into()),
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    total_tokens: 110,
                },
            }),
            None => Err(LlmError::Permanent("script exhausted".into())),
        }
    }
}

/// Two tools and a register that knows ELI acts (in force) but no SAOS judgment.
#[derive(Default)]
struct Fakes {
    calls: Mutex<Vec<String>>,
    /// strata_rag's store tree for the wall re-check (`None`: no strata for the tenant).
    tree: Mutex<Option<Result<Value, String>>>,
    /// The tenant each register lookup was made as (its MCP credentials).
    resolved_as: Mutex<Vec<Uuid>>,
    /// The actor each tool call was made as (strata_rag's `X-Actor-Id`).
    tool_actors: Mutex<Vec<Option<Uuid>>>,
}

impl ToolSet for Fakes {
    async fn store_tree(&self, _tenant: Uuid) -> Result<Option<Value>, String> {
        self.tree.lock().unwrap().clone().transpose()
    }
    async fn specs(&self, _ctx: &ToolCtx<'_>) -> Vec<Value> {
        vec![
            function_spec("legal_eli_act_text", "act text", json!({"type": "object"})),
            function_spec("web_scrape", "scrape", json!({"type": "object"})),
        ]
    }
    async fn call(&self, ctx: &ToolCtx<'_>, name: &str, _args: &Value) -> ToolOutcome {
        self.calls.lock().unwrap().push(name.to_string());
        self.tool_actors.lock().unwrap().push(ctx.actor);
        match name {
            "legal_eli_act_text" => ToolOutcome::ok(ELI_TEXT),
            "web_scrape" => ToolOutcome::ok(WEB_TEXT),
            _ => ToolOutcome::error("unknown"),
        }
    }
}

impl Resolver for Fakes {
    async fn resolve(&self, tenant: Uuid, kind: SourceKind, _locator: &str) -> Resolution {
        self.resolved_as.lock().unwrap().push(tenant);
        match kind {
            SourceKind::Eli => Resolution::Found {
                in_force: Some(true),
            },
            _ => Resolution::Missing("not found".into()),
        }
    }
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]})
}

fn turn_eli() -> Value {
    call(
        "c1",
        "legal_eli_act_text",
        json!({"eli": "DU/1964/59", "article": "133"}),
    )
}

fn turn_web() -> Value {
    call("c2", "web_scrape", json!({"url": "https://example.pl/t"}))
}

fn turn_report() -> Value {
    call(
        "c3",
        "submit_report",
        json!({
            "summary_markdown": "Rodzice mają obowiązek alimentacyjny.",
            "claims": [
                {"text": "Rodzice są zobowiązani do alimentacji dziecka.", "citations": [
                    {"source_kind": "eli", "locator": "DU/1964/59 art. 133", "title": "KRO",
                     "quote": "Ojciec i matka obowiązani są do świadczeń alimentacyjnych", "step_ordinal": 1}]},
                {"text": "Termin wynosi 14 dni.", "citations": [
                    {"source_kind": "web", "locator": "https://example.pl/t",
                     "quote": "Termin wynosi 14 dni", "step_ordinal": 3}]},
                {"text": "Sąd tak orzekł.", "citations": [
                    {"source_kind": "saos", "locator": "999", "quote": "Ojciec i matka obowiązani są",
                     "step_ordinal": 1}]},
                {"text": "Teza zmyślona.", "citations": [
                    {"source_kind": "eli", "locator": "DU/1964/59", "quote": "zmyślony cytat, którego nie ma w ustawie",
                     "step_ordinal": 1}]}
            ]
        }),
    )
}

struct Db {
    app: PgPool,
    owner: PgPool,
    tenant: Uuid,
    job: Uuid,
}

async fn db(test: &str) -> Db {
    let cfg = crate::common::config_with(test, |_| {}).await;
    let owner = PgPool::connect(&cfg.database_url).await.unwrap();
    sqlx::migrate!("./migrations").run(&owner).await.unwrap();
    let app = PgPool::connect(&cfg.database_app_url).await.unwrap();
    let tenant: Uuid = crate::common::BOOT_TENANT.parse().unwrap();
    let job: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(crate::common::BOOT_JOB)
        .fetch_one(&owner)
        .await
        .unwrap();
    Db {
        app,
        owner,
        tenant,
        job,
    }
}

async fn insert_run(d: &Db, budget: Value) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO research_runs (tenant_id, job_id, id, profile, question, budget, created_by)
         VALUES ($1, $2, $3, 'legal', 'Kto ma obowiązek alimentacyjny wobec dziecka?', $4, 'test')",
    )
    .bind(d.tenant)
    .bind(d.job)
    .bind(id)
    .bind(budget)
    .execute(&d.owner)
    .await
    .unwrap();
    id
}

fn config(owner: &str) -> RunnerConfig {
    let mut cfg = RunnerConfig::new(
        owner.into(),
        DefaultLlm {
            base_url: "http://model.invalid/v1".into(),
            model: "local-llm".into(),
            token: None,
        },
    );
    cfg.lease_s = 60;
    cfg.llm_timeout = Duration::from_secs(5);
    cfg.llm_backoff = Duration::from_millis(10);
    cfg
}

fn runner(d: &Db, owner: &str, script: Script, tools: Arc<Fakes>) -> Runner<Script, Fakes, Fakes> {
    Runner {
        db: d.app.clone(),
        cfg: config(owner),
        llm: script,
        tools: tools.clone(),
        resolver: tools,
    }
}

async fn expire_lease(d: &Db, run: Uuid) {
    sqlx::query("UPDATE research_runs SET lease_until = now() - interval '1 second' WHERE id = $1")
        .bind(run)
        .execute(&d.owner)
        .await
        .unwrap();
}

async fn run_row(d: &Db, run: Uuid) -> (String, i32, i64, Option<String>, Option<String>, bool) {
    sqlx::query_as(
        "SELECT status, steps_used, tokens_used, report, error, coalesce(lease_until <= now(), true)
           FROM research_runs WHERE id = $1",
    )
    .bind(run)
    .fetch_one(&d.owner)
    .await
    .unwrap()
}

async fn step_kinds(d: &Db, run: Uuid) -> Vec<(i32, String, String)> {
    sqlx::query_as(
        "SELECT ordinal, kind, status FROM research_steps WHERE run_id = $1 ORDER BY ordinal",
    )
    .bind(run)
    .fetch_all(&d.owner)
    .await
    .unwrap()
}

fn kinds(steps: &[(i32, String, String)]) -> Vec<(i32, &str)> {
    steps.iter().map(|(o, k, _)| (*o, k.as_str())).collect()
}

async fn assert_verified_report(d: &Db, run: Uuid) {
    let (status, steps_used, tokens, report, error, released) = run_row(d, run).await;
    assert_eq!(status, "done", "error: {error:?}");
    assert_eq!(steps_used, 3, "three model turns");
    assert_eq!(tokens, 330);
    assert!(released, "the lease ends with the run");
    let steps = step_kinds(d, run).await;
    assert_eq!(
        kinds(&steps),
        [
            (0, "model"),
            (1, "tool"),
            (2, "model"),
            (3, "tool"),
            (4, "model"),
            (5, "tool")
        ]
    );
    let cits: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.source_kind, c.quote, c.verify_status FROM research_claims cl
           CROSS JOIN LATERAL unnest(cl.citation_ids) AS u(cid)
           JOIN research_citations c ON c.id = u.cid
          WHERE cl.run_id = $1 ORDER BY cl.ordinal",
    )
    .bind(run)
    .fetch_all(&d.owner)
    .await
    .unwrap();
    let st: Vec<&str> = cits.iter().map(|c| c.2.as_str()).collect();
    // the saos quote is in step 1, but step 1 is the ELI act text, not that judgment: provenance fails
    assert_eq!(
        st,
        ["verified", "verified", "quote_mismatch", "quote_mismatch"]
    );
    let claims: Vec<String> =
        sqlx::query_scalar("SELECT status FROM research_claims WHERE run_id = $1 ORDER BY ordinal")
            .bind(run)
            .fetch_all(&d.owner)
            .await
            .unwrap();
    assert_eq!(
        claims,
        ["supported", "supported", "unsupported", "unsupported"]
    );
    let report = report.expect("report");
    let (top, unverified) = report
        .split_once(UNVERIFIED_HEADING)
        .expect("unverified section");
    assert!(top.starts_with(LEGAL_REPORT_TITLE), "{report}");
    // the model's free-text summary is not in the report; it stays in the submit_report step only
    assert!(
        !report.contains("Rodzice mają obowiązek alimentacyjny."),
        "{report}"
    );
    let submitted: Value = sqlx::query_scalar(
        "SELECT request FROM research_steps WHERE run_id = $1 AND kind = 'tool' AND request->>'name' = 'submit_report'",
    )
    .bind(run)
    .fetch_one(&d.owner)
    .await
    .unwrap();
    assert_eq!(
        submitted["arguments"]["summary_markdown"],
        "Rodzice mają obowiązek alimentacyjny."
    );
    assert!(
        top.contains("Rodzice są zobowiązani do alimentacji dziecka. [1]"),
        "{report}"
    );
    assert!(top.contains("eli `DU/1964/59 art. 133` — KRO"), "{report}");
    assert!(!top.contains("Teza zmyślona"), "{report}");
    assert!(
        unverified.contains(
            "Teza zmyślona. — reason: eli `DU/1964/59`: quote_mismatch (quote_not_found)"
        ),
        "{report}"
    );
    assert!(
        unverified
            .contains("Sąd tak orzekł. — reason: saos `999`: quote_mismatch (wrong_step_kind)"),
        "{report}"
    );
    assert!(report.contains(LEGAL_DISCLAIMER));
}

#[tokio::test]
async fn a_run_checkpoints_every_step_and_verifies_its_citations() {
    let d = db("research_e2e").await;
    let run = insert_run(&d, json!({})).await;
    let script = Script::new(vec![turn_eli(), turn_web(), turn_report()]);
    let tools = Arc::new(Fakes::default());
    let r = runner(&d, "w-e2e", script.clone(), tools.clone());
    let s = r.claim_once().await.unwrap().expect("claimable");
    assert_eq!(
        s,
        RunScope {
            tenant: d.tenant,
            job: d.job,
            run
        }
    );
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    assert_verified_report(&d, run).await;
    // each turn saw the conversation so far: system + user, then +2 per tool round
    let seen: Vec<usize> = script.seen().iter().map(|x| x.0).collect();
    assert_eq!(seen, [2, 4, 6]);
    assert_eq!(
        *tools.calls.lock().unwrap(),
        ["legal_eli_act_text", "web_scrape"]
    );
    // the register was asked as the run's tenant, never another identity
    let resolved_as = tools.resolved_as.lock().unwrap().clone();
    assert!(!resolved_as.is_empty());
    assert!(
        resolved_as.iter().all(|t| *t == d.tenant),
        "{resolved_as:?}"
    );
    // the stored tool step keeps the result verbatim
    let resp: Value =
        sqlx::query_scalar("SELECT response FROM research_steps WHERE run_id = $1 AND ordinal = 1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(
        resp,
        json!({"ok": true, "text": ELI_TEXT, "truncated": false})
    );
    // nothing left to claim
    assert!(r.claim_once().await.unwrap().is_none());
}

#[tokio::test]
async fn a_crashed_run_resumes_from_its_last_step() {
    let d = db("research_resume").await;
    let run = insert_run(&d, json!({})).await;
    let tools1 = Arc::new(Fakes::default());
    let mut r1 = runner(&d, "w-1", Script::new(vec![turn_eli()]), tools1.clone());
    r1.cfg.crash_after_steps = Some(2);
    let s = r1.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r1.process(s, &rx).await.unwrap(), Outcome::Crashed);
    assert_eq!(
        kinds(&step_kinds(&d, run).await),
        [(0, "model"), (1, "tool")]
    );

    let script2 = Script::new(vec![turn_web(), turn_report()]);
    let tools2 = Arc::new(Fakes::default());
    let r2 = runner(&d, "w-2", script2.clone(), tools2.clone());
    assert!(
        r2.claim_once().await.unwrap().is_none(),
        "a live lease is not re-claimed"
    );
    expire_lease(&d, run).await;
    let s2 = r2.claim_once().await.unwrap().expect("expired lease");
    assert_eq!(s2.run, run);
    // the crashed runner cannot write any more
    assert_eq!(r1.process(s, &rx).await.unwrap(), Outcome::LeaseLost);
    assert_eq!(r2.process(s2, &rx).await.unwrap(), Outcome::Done);
    assert_verified_report(&d, run).await;
    // steps 0-1 were not repeated: the first resumed turn already saw them
    assert_eq!(
        script2.seen().iter().map(|x| x.0).collect::<Vec<_>>(),
        [4, 6]
    );
    assert_eq!(*tools2.calls.lock().unwrap(), ["web_scrape"]);
    let owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM research_runs WHERE id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(owner.as_deref(), Some("w-2"));
}

#[tokio::test]
async fn a_cancel_request_stops_the_run_before_the_next_step() {
    let d = db("research_cancel").await;
    let run = insert_run(&d, json!({})).await;
    let mut r1 = runner(
        &d,
        "w-1",
        Script::new(vec![turn_eli()]),
        Arc::new(Fakes::default()),
    );
    r1.cfg.crash_after_steps = Some(1);
    let s = r1.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r1.process(s, &rx).await.unwrap(), Outcome::Crashed);
    sqlx::query("UPDATE research_runs SET cancel_requested = true WHERE id = $1")
        .bind(run)
        .execute(&d.owner)
        .await
        .unwrap();
    expire_lease(&d, run).await;
    let script = Script::new(vec![turn_web(), turn_report()]);
    let r2 = runner(&d, "w-2", script.clone(), Arc::new(Fakes::default()));
    let s2 = r2.claim_once().await.unwrap().unwrap();
    assert_eq!(r2.process(s2, &rx).await.unwrap(), Outcome::Cancelled);
    assert!(script.seen().is_empty(), "no model call after the cancel");
    let (status, ..) = run_row(&d, run).await;
    assert_eq!(status, "cancelled");
    assert_eq!(
        step_kinds(&d, run).await.len(),
        1,
        "the pending tool call was not run"
    );
    assert!(r2.claim_once().await.unwrap().is_none());
}

/// Resolves as [`Fakes`], but first requests a cancel of the run: a cancel that arrives while verifying.
struct CancelOnResolve {
    owner: PgPool,
    run: Uuid,
}

impl Resolver for CancelOnResolve {
    async fn resolve(&self, tenant: Uuid, kind: SourceKind, locator: &str) -> Resolution {
        sqlx::query("UPDATE research_runs SET cancel_requested = true WHERE id = $1")
            .bind(self.run)
            .execute(&self.owner)
            .await
            .unwrap();
        let fakes = Fakes::default();
        fakes.resolve(tenant, kind, locator).await
    }
}

#[tokio::test]
async fn a_cancel_while_verifying_ends_the_run_cancelled_without_a_report() {
    let d = db("research_cancel_verify").await;
    let run = insert_run(&d, json!({})).await;
    let r = Runner {
        db: d.app.clone(),
        cfg: config("w-cv"),
        llm: Script::new(vec![turn_eli(), turn_web(), turn_report()]),
        tools: Arc::new(Fakes::default()),
        resolver: Arc::new(CancelOnResolve {
            owner: d.owner.clone(),
            run,
        }),
    };
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Cancelled);
    let (status, _, _, report, error, released) = run_row(&d, run).await;
    assert_eq!(status, "cancelled", "error: {error:?}");
    assert!(report.is_none() && released);
    let finished: bool =
        sqlx::query_scalar("SELECT finished_at IS NOT NULL FROM research_runs WHERE id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert!(finished);
    assert!(r.claim_once().await.unwrap().is_none());
}

#[tokio::test]
async fn a_run_claimed_more_than_the_maximum_fails() {
    let d = db("research_attempts").await;
    let run = insert_run(&d, json!({})).await;
    // three earlier claims, each of which crashed its runner
    sqlx::query("UPDATE research_runs SET attempts = 3 WHERE id = $1")
        .bind(run)
        .execute(&d.owner)
        .await
        .unwrap();
    let script = Script::new(vec![turn_eli()]);
    let r = runner(&d, "w-a", script.clone(), Arc::new(Fakes::default()));
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(
        r.process(s, &rx).await.unwrap(),
        Outcome::Failed("too_many_attempts".into())
    );
    assert!(script.seen().is_empty(), "no model call");
    let (status, _, _, report, error, released) = run_row(&d, run).await;
    assert_eq!(
        (status.as_str(), error.as_deref()),
        ("failed", Some("too_many_attempts"))
    );
    assert!(report.is_none() && released);
    let (attempts, finished): (i32, bool) =
        sqlx::query_as("SELECT attempts, finished_at IS NOT NULL FROM research_runs WHERE id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(attempts, 4);
    assert!(finished);
    assert!(r.claim_once().await.unwrap().is_none());
}

#[tokio::test]
async fn an_exhausted_budget_gets_two_write_turns_then_fails_as_insufficient_evidence() {
    let d = db("research_budget").await;
    let run = insert_run(&d, json!({"max_steps": 2})).await;
    // two research turns, then both write turns answer in plain text
    let script = Script::new(vec![
        turn_eli(),
        turn_web(),
        json!({"role": "assistant", "content": "Nie mam więcej czasu."}),
        json!({"role": "assistant", "content": "Nadal nie."}),
    ]);
    let r = runner(&d, "w-b", script.clone(), Arc::new(Fakes::default()));
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    let want = "budget_exhausted: insufficient_evidence: no report with cited claims was accepted in 2 write \
                turn(s) after 2 tool call(s)";
    assert_eq!(
        r.process(s, &rx).await.unwrap(),
        Outcome::Failed(want.into())
    );
    let seen = script.seen();
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[0].1, None);
    assert_eq!(
        seen[0].2,
        ["legal_eli_act_text", "web_scrape", "submit_report"]
    );
    for write in &seen[2..] {
        assert_eq!(
            write.1,
            Some(json!({"type": "function", "function": {"name": "submit_report"}}))
        );
        // a write turn offers nothing but submit_report
        assert_eq!(write.2, ["submit_report"]);
    }
    let (status, steps_used, _, report, error, released) = run_row(&d, run).await;
    assert_eq!((status.as_str(), error.as_deref()), ("failed", Some(want)));
    assert_eq!(steps_used, 4);
    assert!(report.is_none() && released);
}

/// Run 06010c5b (2026-10-24): the forced turn ignored `tool_choice` and asked for `web_scrape`, which was run, and
/// the run failed with nothing. Now that call is refused unexecuted and the second write turn delivers the report.
#[tokio::test]
async fn a_write_turn_runs_no_research_tool_and_the_next_one_may_deliver() {
    let d = db("research_budget_write").await;
    let run = insert_run(&d, json!({"max_steps": 2})).await;
    let wander = call(
        "c9",
        "web_scrape",
        json!({"url": "https://example.pl/inne"}),
    );
    let script = Script::new(vec![turn_eli(), turn_web(), wander, turn_report()]);
    let tools = Arc::new(Fakes::default());
    let r = runner(&d, "w-bw", script.clone(), tools.clone());
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    assert_eq!(
        *tools.calls.lock().unwrap(),
        ["legal_eli_act_text", "web_scrape"],
        "the write turn's web_scrape was not executed"
    );
    let steps = step_kinds(&d, run).await;
    assert_eq!(steps[5], (5, "tool".into(), "denied".into()));
    let text: String = sqlx::query_scalar(
        "SELECT response->>'text' FROM research_steps WHERE run_id = $1 AND ordinal = 5",
    )
    .bind(run)
    .fetch_one(&d.owner)
    .await
    .unwrap();
    assert!(
        text.contains("only `submit_report` may be called now"),
        "{text}"
    );
    // the report cites steps 1 and 3, verified as in the normal run
    let claims: Vec<String> =
        sqlx::query_scalar("SELECT status FROM research_claims WHERE run_id = $1 ORDER BY ordinal")
            .bind(run)
            .fetch_all(&d.owner)
            .await
            .unwrap();
    assert_eq!(
        claims,
        ["supported", "supported", "unsupported", "unsupported"]
    );
    let (status, steps_used, ..) = run_row(&d, run).await;
    assert_eq!((status.as_str(), steps_used), ("done", 4));
}

/// The same call twice is answered from the first step, not executed again; an unknown tool is refused with the
/// tools that exist.
#[tokio::test]
async fn a_repeated_call_is_not_executed_and_an_unknown_tool_names_the_real_ones() {
    let d = db("research_repeat").await;
    let run = insert_run(&d, json!({})).await;
    let report = call(
        "c5",
        "submit_report",
        json!({"claims": [{"text": "Termin wynosi 14 dni.", "citations": [
            {"source_kind": "web", "locator": "https://example.pl/t",
             "quote": "Termin wynosi 14 dni od doręczenia", "step_ordinal": 1}]}]}),
    );
    let script = Script::new(vec![
        turn_web(),
        // the same page again: key order and a trailing slash do not make it a new call
        call("c3", "web_scrape", json!({"url": "https://example.pl/t/"})),
        call("c4", "web_search", json!({"query": "termin"})),
        report,
    ]);
    let tools = Arc::new(Fakes::default());
    let r = runner(&d, "w-rep", script.clone(), tools.clone());
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    assert_eq!(
        *tools.calls.lock().unwrap(),
        ["web_scrape"],
        "executed once"
    );
    let rows: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT ordinal, status, response->>'text' FROM research_steps
          WHERE run_id = $1 AND kind = 'tool' ORDER BY ordinal",
    )
    .bind(run)
    .fetch_all(&d.owner)
    .await
    .unwrap();
    assert_eq!(rows[1].0, 3);
    assert_eq!(rows[1].1, "denied");
    assert!(rows[1].2.contains("[step 1]"), "{}", rows[1].2);
    assert_eq!(rows[2].1, "denied");
    for t in [
        "`web_search` is not an available tool",
        "`legal_eli_act_text`",
        "`web_scrape`",
        "`submit_report`",
    ] {
        assert!(rows[2].2.contains(t), "{}", rows[2].2);
    }
    let claims: Vec<String> =
        sqlx::query_scalar("SELECT status FROM research_claims WHERE run_id = $1 ORDER BY ordinal")
            .bind(run)
            .fetch_all(&d.owner)
            .await
            .unwrap();
    assert_eq!(claims, ["supported"]);
}

#[tokio::test]
async fn the_forced_turn_may_still_deliver_the_report() {
    let d = db("research_budget_ok").await;
    let run = insert_run(&d, json!({"max_steps": 2})).await;
    let script = Script::new(vec![turn_eli(), turn_web(), turn_report()]);
    let r = runner(&d, "w-b2", script.clone(), Arc::new(Fakes::default()));
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    assert_verified_report(&d, run).await;
}

#[tokio::test]
async fn shutdown_releases_the_lease_for_another_runner() {
    let d = db("research_shutdown").await;
    let run = insert_run(&d, json!({})).await;
    let r = runner(&d, "w-s", Script::new(vec![]), Arc::new(Fakes::default()));
    let s = r.claim_once().await.unwrap().unwrap();
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Shutdown);
    let (status, .., released) = run_row(&d, run).await;
    assert_eq!(status, "running");
    assert!(released);
    // a graceful shutdown gives the claim back: it does not count as an attempt
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM research_runs WHERE id = $1")
        .bind(run)
        .fetch_one(&d.owner)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
    let other = runner(&d, "w-t", Script::new(vec![]), Arc::new(Fakes::default()));
    assert_eq!(other.claim_once().await.unwrap().map(|x| x.run), Some(run));
}

#[tokio::test]
async fn run_rows_are_invisible_to_another_tenant() {
    let d = db("research_rls").await;
    let run = insert_run(&d, json!({})).await;
    let other: Uuid = crate::common::OTHER_TENANT.parse().unwrap();
    let s = RunScope {
        tenant: other,
        job: d.job,
        run,
    };
    assert!(store::load_run(&d.app, s).await.unwrap().is_none());
    let mine = RunScope {
        tenant: d.tenant,
        job: d.job,
        run,
    };
    assert!(store::load_run(&d.app, mine).await.unwrap().is_some());
}

#[tokio::test]
async fn every_model_call_is_metered_under_the_run() {
    let d = db("research_metering").await;
    let run = insert_run(&d, json!({})).await;
    let script = Script::new(vec![turn_eli(), turn_web(), turn_report()]);
    let r = runner(&d, "w-m", script, Arc::new(Fakes::default()));
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    type Row = (
        Uuid,
        Uuid,
        String,
        Option<Uuid>,
        Option<String>,
        String,
        String,
        i64,
        i64,
        i32,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT tenant_id, job_id, source, research_run_id, conversation_id, provider, model, input_tokens,
                output_tokens, calls
           FROM usage_ledger ORDER BY occurred_at, id",
    )
    .fetch_all(&d.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3, "one row per model call: {rows:?}");
    for row in &rows {
        assert_eq!(
            row,
            &(
                d.tenant,
                d.job,
                "research".to_string(),
                Some(run),
                None,
                "vllm".to_string(),
                "local-llm".to_string(),
                100,
                10,
                1
            )
        );
    }
    // emit is off: no billing outbox row
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM metering_outbox")
        .fetch_one(&d.owner)
        .await
        .unwrap();
    assert_eq!(outbox, 0);
    // the rows are the tenant's: another tenant's scope sees none of them
    let other: Uuid = crate::common::OTHER_TENANT.parse().unwrap();
    for (tenant, n) in [(d.tenant, 3i64), (other, 0)] {
        let mut tx = carebus_api::tenancy::begin_scoped(&d.app, tenant, None)
            .await
            .unwrap();
        let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_ledger")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, n);
    }
}

/// The actor of every model and tool call: the run's creator when it is a uuid subject, else care-bus' service
/// account (`SERVICE_ACTOR_ID`) for the model; tools get only the creator (their client adds the service actor).
#[tokio::test]
async fn a_runs_model_and_tool_calls_carry_its_creator_else_the_service_actor() {
    let d = db("research_actor").await;
    let service = Uuid::new_v4();
    let creator = Uuid::new_v4();
    for (created_by, model_actor, tool_actor) in [
        (creator.to_string(), Some(creator), Some(creator)),
        ("test".to_string(), Some(service), None),
    ] {
        let run = insert_run(&d, json!({})).await;
        sqlx::query("UPDATE research_runs SET created_by = $1 WHERE id = $2")
            .bind(&created_by)
            .bind(run)
            .execute(&d.owner)
            .await
            .unwrap();
        let script = Script::new(vec![turn_eli(), turn_web(), turn_report()]);
        let tools = Arc::new(Fakes::default());
        let mut r = runner(&d, "w-actor", script.clone(), tools.clone());
        r.cfg.service_actor = Some(service);
        let s = r.claim_once().await.unwrap().unwrap();
        let (_tx, rx) = watch::channel(false);
        assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
        let models = script.actors.lock().unwrap().clone();
        assert_eq!(models.len(), 3, "{created_by}");
        assert!(
            models.iter().all(|a| *a == model_actor),
            "{created_by}: {models:?}"
        );
        let used = tools.tool_actors.lock().unwrap().clone();
        assert_eq!(used.len(), 2, "{created_by}");
        assert!(
            used.iter().all(|a| *a == tool_actor),
            "{created_by}: {used:?}"
        );
    }
}

#[tokio::test]
async fn a_research_ledger_row_must_name_its_run() {
    let d = db("research_metering_check").await;
    let insert = |source: &'static str, run: Option<Uuid>| {
        let owner = d.owner.clone();
        let (tenant, job) = (d.tenant, d.job);
        async move {
            sqlx::query(
                "INSERT INTO usage_ledger (tenant_id, job_id, event_key, source, research_run_id, provider, model,
                                           input_tokens, output_tokens, calls, latency_ms)
                 VALUES ($1, $2, $3, $4, $5, 'vllm', 'local-llm', 1, 1, 1, 1)",
            )
            .bind(tenant)
            .bind(job)
            .bind(Uuid::new_v4())
            .bind(source)
            .bind(run)
            .execute(&owner)
            .await
        }
    };
    assert!(insert("research", Some(Uuid::new_v4())).await.is_ok());
    assert!(insert("chat_qualifier", None).await.is_ok());
    for (source, run) in [
        ("research", None),
        ("chat_qualifier", Some(Uuid::new_v4())),
        ("something_else", None),
    ] {
        let e = insert(source, run).await.unwrap_err().to_string();
        assert!(e.contains("usage_ledger_"), "{source}: {e}");
    }
}

/// `lease_until` (epoch seconds) before and after each slow model call.
type Leases = Arc<Mutex<Vec<(f64, f64)>>>;

/// Before delegating to the script, runs `sql` (bound to the run id) on the owner pool, then sleeps `delay`.
/// Records `lease_until` (epoch seconds) before and after the sleep.
struct Slow {
    inner: Script,
    owner: PgPool,
    run: Uuid,
    sql: Option<&'static str>,
    delay: Duration,
    leases: Leases,
}

async fn lease_until(owner: &PgPool, run: Uuid) -> f64 {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM lease_until)::float8 FROM research_runs WHERE id = $1",
    )
    .bind(run)
    .fetch_one(owner)
    .await
    .unwrap()
}

impl Llm for Slow {
    async fn chat(&self, ep: &LlmEndpoint, req: &ChatRequest) -> Result<ChatReply, LlmError> {
        if let Some(sql) = self.sql {
            sqlx::query(sql)
                .bind(self.run)
                .execute(&self.owner)
                .await
                .unwrap();
        }
        let before = lease_until(&self.owner, self.run).await;
        tokio::time::sleep(self.delay).await;
        let after = lease_until(&self.owner, self.run).await;
        self.leases.lock().unwrap().push((before, after));
        self.inner.chat(ep, req).await
    }
}

fn slow_runner(
    d: &Db,
    owner: &str,
    run: Uuid,
    sql: Option<&'static str>,
    delay: Duration,
    replies: Vec<Value>,
) -> (Runner<Slow, Fakes, Fakes>, Leases) {
    let leases: Leases = Arc::default();
    let mut cfg = config(owner);
    cfg.lease_s = 10;
    cfg.heartbeat = Some(Duration::from_millis(100));
    let tools = Arc::new(Fakes::default());
    (
        Runner {
            db: d.app.clone(),
            cfg,
            llm: Slow {
                inner: Script::new(replies),
                owner: d.owner.clone(),
                run,
                sql,
                delay,
                leases: leases.clone(),
            },
            tools: tools.clone(),
            resolver: tools,
        },
        leases,
    )
}

#[tokio::test]
async fn the_lease_is_renewed_while_a_model_call_is_in_flight() {
    let d = db("research_heartbeat").await;
    let run = insert_run(&d, json!({})).await;
    let (r, leases) = slow_runner(
        &d,
        "w-hb",
        run,
        None,
        Duration::from_millis(1200),
        vec![turn_eli(), turn_web(), turn_report()],
    );
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    let leases = leases.lock().unwrap().clone();
    assert_eq!(leases.len(), 3);
    for (before, after) in leases {
        // renewed (now() + 10 s) during the call, not only before and after it
        assert!(
            after - before >= 0.8,
            "lease not renewed mid-call: {before} -> {after}"
        );
    }
}

#[tokio::test]
async fn a_lease_lost_during_a_model_call_abandons_it() {
    let d = db("research_heartbeat_lost").await;
    let run = insert_run(&d, json!({})).await;
    let (r, _) = slow_runner(
        &d,
        "w-hl",
        run,
        Some("UPDATE research_runs SET lease_owner = 'thief' WHERE id = $1"),
        Duration::from_secs(30),
        vec![turn_eli()],
    );
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    let started = std::time::Instant::now();
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::LeaseLost);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call was not abandoned"
    );
    assert!(step_kinds(&d, run).await.is_empty(), "nothing written");
    let owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM research_runs WHERE id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(owner.as_deref(), Some("thief"));
}

#[tokio::test]
async fn a_cancel_during_a_model_call_ends_the_run_cancelled() {
    let d = db("research_heartbeat_cancel").await;
    let run = insert_run(&d, json!({})).await;
    let (r, _) = slow_runner(
        &d,
        "w-hc",
        run,
        Some("UPDATE research_runs SET cancel_requested = true WHERE id = $1"),
        Duration::from_secs(30),
        vec![turn_eli()],
    );
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    let started = std::time::Instant::now();
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(10));
    let (status, .., released) = run_row(&d, run).await;
    assert_eq!(status, "cancelled");
    assert!(released);
}

/// Binds the tenant's `research` use to an external (non-platform) openai provider on a loopback IP literal.
async fn bind_external_research_provider(d: &Db) {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO llm_providers (tenant_id, id, name, kind, base_url, updated_by)
         VALUES ($1, $2, 'Cloud', 'openai', 'http://127.0.0.1:9/v1', 'test')",
    )
    .bind(d.tenant)
    .bind(id)
    .execute(&d.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO llm_provider_uses (tenant_id, \"use\", provider_id, model, updated_by)
         VALUES ($1, 'research', $2, 'gpt-x', 'test')",
    )
    .bind(d.tenant)
    .bind(id)
    .execute(&d.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn an_external_model_needs_the_tenants_external_llm_allowed() {
    let d = db("research_external_llm").await;
    bind_external_research_provider(&d).await;

    // default (no settings row): refused before any model call, never a silent fallback
    let run = insert_run(&d, json!({})).await;
    let script = Script::new(vec![turn_eli(), turn_web(), turn_report()]);
    let mut r = runner(&d, "w-x", script.clone(), Arc::new(Fakes::default()));
    r.cfg.allow_loopback = true;
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    let Outcome::Failed(err) = r.process(s, &rx).await.unwrap() else {
        panic!("the run must fail");
    };
    assert!(err.starts_with("external_llm_not_allowed"), "{err}");
    assert!(script.seen().is_empty(), "no model call");
    let (status, _, _, report, error, released) = run_row(&d, run).await;
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some(err.as_str()));
    assert!(report.is_none() && released);

    // explicitly off: the same
    sqlx::query(
        "INSERT INTO tenant_llm_settings (tenant_id, external_llm_allowed, updated_by) VALUES ($1, false, 'test')",
    )
    .bind(d.tenant)
    .execute(&d.owner)
    .await
    .unwrap();
    insert_run(&d, json!({})).await;
    let s = r.claim_once().await.unwrap().unwrap();
    assert!(matches!(
        r.process(s, &rx).await.unwrap(),
        Outcome::Failed(e) if e.starts_with("external_llm_not_allowed")
    ));

    // allowed: the run uses the tenant's provider
    sqlx::query("UPDATE tenant_llm_settings SET external_llm_allowed = true WHERE tenant_id = $1")
        .bind(d.tenant)
        .execute(&d.owner)
        .await
        .unwrap();
    let run = insert_run(&d, json!({})).await;
    let s = r.claim_once().await.unwrap().unwrap();
    assert_eq!(s.run, run);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    let provider: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT provider FROM usage_ledger WHERE research_run_id = $1")
            .bind(run)
            .fetch_all(&d.owner)
            .await
            .unwrap();
    assert_eq!(provider.len(), 1);
    assert!(provider[0].starts_with("provider:"), "{provider:?}");
}

#[tokio::test]
async fn tenant_llm_settings_are_isolated_per_tenant() {
    let d = db("research_external_llm_rls").await;
    let other = crate::common::insert_tenant(&d.owner, crate::common::OTHER_TENANT).await;
    sqlx::query(
        "INSERT INTO tenant_llm_settings (tenant_id, external_llm_allowed, updated_by) VALUES ($1, true, 'test')",
    )
    .bind(d.tenant)
    .execute(&d.owner)
    .await
    .unwrap();
    assert!(store::external_llm_allowed(&d.app, d.tenant).await.unwrap());
    assert!(!store::external_llm_allowed(&d.app, other).await.unwrap());
    // the runtime role cannot write another tenant's row
    let mut tx = carebus_api::tenancy::begin_scoped(&d.app, other, None)
        .await
        .unwrap();
    let e = sqlx::query(
        "INSERT INTO tenant_llm_settings (tenant_id, external_llm_allowed, updated_by) VALUES ($1, true, 'x')",
    )
    .bind(d.tenant)
    .execute(&mut *tx)
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("row-level security"), "{e}");
}

#[tokio::test]
async fn turning_external_llm_allowed_off_stops_a_running_run_at_its_next_model_call() {
    let d = db("research_external_llm_off").await;
    bind_external_research_provider(&d).await;
    sqlx::query(
        "INSERT INTO tenant_llm_settings (tenant_id, external_llm_allowed, updated_by) VALUES ($1, true, 'test')",
    )
    .bind(d.tenant)
    .execute(&d.owner)
    .await
    .unwrap();
    let run = insert_run(&d, json!({})).await;
    // the first model call turns the flag off (an admin, mid-run); the endpoint stays cached for the run
    let (mut r, calls) = slow_runner(
        &d,
        "w-xo",
        run,
        Some(
            "UPDATE tenant_llm_settings SET external_llm_allowed = false
              WHERE tenant_id = (SELECT tenant_id FROM research_runs WHERE id = $1)",
        ),
        Duration::ZERO,
        vec![turn_eli(), turn_web(), turn_report()],
    );
    r.cfg.allow_loopback = true;
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    let Outcome::Failed(err) = r.process(s, &rx).await.unwrap() else {
        panic!("the run must fail");
    };
    assert!(err.starts_with("external_llm_not_allowed"), "{err}");
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "no model call after the flag went off"
    );
    // the one call that happened is metered; its tool step was written; nothing after
    let metered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_ledger WHERE research_run_id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(metered, 1);
    assert_eq!(
        kinds(&step_kinds(&d, run).await),
        [(0, "model"), (1, "tool")]
    );
    let (status, _, _, report, error, released) = run_row(&d, run).await;
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some(err.as_str()));
    assert!(report.is_none() && released);
}

const W_ROUTER: &str = "7f1c9a2e-0000-4000-8000-0000000000b0";
const W_LEAF: &str = "7f1c9a2e-0000-4000-8000-0000000000b1";
const W_OPEN: &str = "7f1c9a2e-0000-4000-8000-0000000000b2";

/// A queued run of `creator`, filed under `matter` (or none), naming `stores`.
async fn insert_store_run(d: &Db, matter: Option<Uuid>, stores: &[&str], creator: &str) -> Uuid {
    let id = Uuid::new_v4();
    let stores: Vec<String> = stores.iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "INSERT INTO research_runs (tenant_id, job_id, id, profile, question, budget, created_by, matter_id, store_ids)
         VALUES ($1, $2, $3, 'legal', 'Co wynika z akt?', '{}', $4, $5, $6)",
    )
    .bind(d.tenant)
    .bind(d.job)
    .bind(id)
    .bind(creator)
    .bind(matter)
    .bind(&stores)
    .execute(&d.owner)
    .await
    .unwrap();
    id
}

/// Claims the one queued run and processes it.
async fn claim_and_process(r: &Runner<Script, Fakes, Fakes>) -> anyhow::Result<Outcome> {
    let s = r.claim_once().await.unwrap().expect("a claimable run");
    let (_tx, rx) = watch::channel(false);
    r.process(s, &rx).await
}

/// A store walled (migration 0026) after a run was filed stops the run at its next claim, before any model call:
/// a store of another matter, a store above one (through the strata tree), or its own matter's store once the run's
/// creator is no longer a member. A member's run under the matter goes on; an unreadable tree never lets a run go on.
#[tokio::test]
async fn a_store_walled_after_the_run_was_filed_stops_it_at_the_next_claim() {
    let d = db("research_walls").await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, display_name, updated_by)
         VALUES ('u-lead', $1, 'tenant_user', 'A. Lead', 'test')",
    )
    .bind(d.tenant)
    .execute(&d.owner)
    .await
    .unwrap();
    let matter = Uuid::new_v4();
    for q in [
        "INSERT INTO legal_matters (tenant_id, job_id, id, name, created_by) VALUES ($1, $2, $3, 'Sprawa', 'u-lead')",
        "INSERT INTO legal_matter_members (tenant_id, job_id, matter_id, user_sub, role, added_by)
         VALUES ($1, $2, $3, 'u-lead', 'lead', 'u-lead')",
    ] {
        sqlx::query(q)
            .bind(d.tenant)
            .bind(d.job)
            .bind(matter)
            .execute(&d.owner)
            .await
            .unwrap();
    }
    let tools = Arc::new(Fakes::default());
    let script = Script::new(vec![]);
    let r = runner(&d, "w-walls", script.clone(), tools.clone());

    // filed before the wall: a run without a matter on the store, one on the router above it
    let direct = insert_store_run(&d, None, &[W_LEAF], "u-other").await;
    sqlx::query(
        "INSERT INTO legal_matter_stores (tenant_id, job_id, matter_id, store_id, added_by)
         VALUES ($1, $2, $3, $4, 'u-lead')",
    )
    .bind(d.tenant)
    .bind(d.job)
    .bind(matter)
    .bind(W_LEAF)
    .execute(&d.owner)
    .await
    .unwrap();
    let walled = |o: Outcome| matches!(o, Outcome::Failed(ref e) if e.starts_with("store_walled"));
    let o = claim_and_process(&r).await.unwrap();
    assert!(walled(o.clone()), "{o:?}");
    let (status, _, _, _, error, released) = run_row(&d, direct).await;
    assert_eq!(status, "failed");
    assert!(error.unwrap().starts_with("store_walled"));
    assert!(released);

    *tools.tree.lock().unwrap() = Some(Ok(json!({"stores": [
        {"id": W_ROUTER, "parent_id": null, "kind": "router", "children": [W_LEAF, W_OPEN]},
        {"id": W_LEAF, "parent_id": W_ROUTER, "kind": "leaf", "children": []},
        {"id": W_OPEN, "parent_id": W_ROUTER, "kind": "leaf", "children": []},
    ]})));
    insert_store_run(&d, None, &[W_ROUTER.to_uppercase().as_str()], "u-other").await;
    let o = claim_and_process(&r).await.unwrap();
    assert!(walled(o.clone()), "{o:?}");

    // its own matter, but the creator is no longer a member
    insert_store_run(&d, Some(matter), &[W_LEAF], "u-gone").await;
    let o = claim_and_process(&r).await.unwrap();
    assert!(walled(o.clone()), "{o:?}");
    assert!(script.seen().is_empty(), "no model call for a walled run");

    // an unreadable tree (a run above a wall could reach it) is an error: the run is not continued
    *tools.tree.lock().unwrap() = Some(Err("strata_rag down".into()));
    let blocked = insert_store_run(&d, None, &[W_OPEN], "u-other").await;
    assert!(claim_and_process(&r).await.is_err());
    assert!(script.seen().is_empty());
    sqlx::query("UPDATE research_runs SET status = 'cancelled', lease_until = NULL WHERE id = $1")
        .bind(blocked)
        .execute(&d.owner)
        .await
        .unwrap();

    // a member's run under the matter, and a run on an open store, go on to the model
    *tools.tree.lock().unwrap() = None;
    insert_store_run(&d, Some(matter), &[W_LEAF], "u-lead").await;
    let o = claim_and_process(&r).await.unwrap();
    assert!(!walled(o.clone()), "{o:?}");
    assert_eq!(script.seen().len(), 1, "the member's run reached the model");
}

// --- MCP tasks (C6): a tool call that starts a long-running task parks the run until the task ends -----------------

/// One long-running tool, `fr_project_start`: every call records a real task row under the calling run (as
/// `LiveTools::start_task` does). With `finish_at_once` the task has already ended when the call returns.
struct TaskTools {
    db: PgPool,
    calls: Mutex<u32>,
    finish_at_once: bool,
}

fn finished_detail() -> Value {
    json!({"taskId": "t-1", "status": "completed", "result": {"isError": false,
        "content": [{"type": "text", "text": WEB_TEXT}]}})
}

async fn end_task(db: &PgPool, tenant: Uuid, job: Uuid, task_id: &str) {
    let mut tx = carebus_api::tenancy::begin_scoped(db, tenant, Some(job))
        .await
        .unwrap();
    let snap = carebus::mcp_tasks::TaskSnapshot::from_detail(finished_detail());
    carebus_api::mcp_tasks::apply_snapshot(&mut tx, task_id, &snap)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

impl ToolSet for TaskTools {
    async fn specs(&self, _ctx: &ToolCtx<'_>) -> Vec<Value> {
        vec![function_spec(
            "fr_project_start",
            "start a project",
            json!({"type": "object"}),
        )]
    }
    async fn call(&self, ctx: &ToolCtx<'_>, _name: &str, _args: &Value) -> ToolOutcome {
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let run = ctx.run.expect("a run");
        let task_id = format!("t-{n}");
        let origin = carebus_api::mcp_tasks::Origin {
            tenant: run.tenant,
            job: run.job,
            run: Some(run.run),
            case: None,
            created_by: ctx.created_by,
        };
        let h = carebus::mcp_tasks::TaskHandle {
            task_id: task_id.clone(),
            status: "working".into(),
            status_message: None,
            poll_interval_ms: Some(2000),
            ttl_ms: None,
        };
        carebus_api::mcp_tasks::record_started(&self.db, &origin, "fr_project_start", &h)
            .await
            .unwrap();
        if self.finish_at_once {
            end_task(&self.db, run.tenant, run.job, &task_id).await;
        }
        let mut o = ToolOutcome::ok(format!("waiting for MCP task {task_id}"));
        o.task_id = Some(task_id);
        o
    }
}

impl Resolver for TaskTools {
    async fn resolve(&self, _tenant: Uuid, _kind: SourceKind, _locator: &str) -> Resolution {
        Resolution::Missing("not found".into())
    }
}

fn turn_task_report() -> Value {
    call(
        "c9",
        "submit_report",
        json!({
            "summary_markdown": "Projekt wystartował.",
            "claims": [{"text": "Termin wynosi 14 dni.", "citations": [
                {"source_kind": "web", "locator": "https://example.pl/t",
                 "quote": "Termin wynosi 14 dni", "step_ordinal": 1}]}]
        }),
    )
}

fn task_runner(
    d: &Db,
    owner: &str,
    script: Script,
    tools: Arc<TaskTools>,
) -> Runner<Script, TaskTools, TaskTools> {
    Runner {
        db: d.app.clone(),
        cfg: config(owner),
        llm: script,
        tools: tools.clone(),
        resolver: tools,
    }
}

#[tokio::test]
async fn a_run_whose_tool_call_starts_a_task_waits_and_resumes_with_its_result() {
    let d = db("research_task_wait").await;
    let run = insert_run(&d, json!({})).await;
    let tools = Arc::new(TaskTools {
        db: d.app.clone(),
        calls: Mutex::new(0),
        finish_at_once: false,
    });
    let script = Script::new(vec![
        call("c1", "fr_project_start", json!({})),
        turn_task_report(),
    ]);
    let r = task_runner(&d, "w-1", script.clone(), tools.clone());
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Waiting);

    // Parked: waiting, no lease, the claim not counted, the step marked waiting; nobody can claim it.
    let (status, ..) = run_row(&d, run).await;
    assert_eq!(status, "waiting");
    let (owner, attempts): (Option<String>, i32) =
        sqlx::query_as("SELECT lease_owner, attempts FROM research_runs WHERE id = $1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!((owner, attempts), (None, 0));
    assert_eq!(
        kinds(&step_kinds(&d, run).await),
        [(0, "model"), (1, "tool")]
    );
    let resp: Value =
        sqlx::query_scalar("SELECT response FROM research_steps WHERE run_id = $1 AND ordinal = 1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(resp["waiting"], true);
    assert_eq!(resp["task_id"], "t-1");
    assert_eq!(resp["ok"], true);
    assert!(r.claim_once().await.unwrap().is_none());

    // The task ends: its result is the step's, the run is queued again.
    end_task(&d.app, d.tenant, d.job, "t-1").await;
    let (status, ..) = run_row(&d, run).await;
    assert_eq!(status, "queued");
    let (st, resp): (String, Value) = sqlx::query_as(
        "SELECT status, response FROM research_steps WHERE run_id = $1 AND ordinal = 1",
    )
    .bind(run)
    .fetch_one(&d.owner)
    .await
    .unwrap();
    assert_eq!(st, "ok");
    assert_eq!(resp["text"], WEB_TEXT);
    assert!(resp.get("waiting").is_none(), "{resp}");

    // Resumed: the model sees system, question, its call and the tool result, and the tool is NOT called again.
    let s2 = r.claim_once().await.unwrap().expect("requeued run");
    assert_eq!(r.process(s2, &rx).await.unwrap(), Outcome::Done);
    assert_eq!(*tools.calls.lock().unwrap(), 1, "one task, never a second");
    assert_eq!(
        script.seen().iter().map(|x| x.0).collect::<Vec<_>>(),
        [2, 4]
    );
    let (status, _, _, report, ..) = run_row(&d, run).await;
    assert_eq!(status, "done");
    assert!(report.unwrap().contains("Termin wynosi 14 dni"));
}

/// The task ended before the run could park (the poller won the race): the result is the step's at once and the
/// run goes on in the same claim, never left `waiting` with nobody to requeue it.
#[tokio::test]
async fn a_task_that_ended_before_the_run_parked_does_not_leave_it_waiting() {
    let d = db("research_task_race").await;
    let run = insert_run(&d, json!({})).await;
    let tools = Arc::new(TaskTools {
        db: d.app.clone(),
        calls: Mutex::new(0),
        finish_at_once: true,
    });
    let script = Script::new(vec![
        call("c1", "fr_project_start", json!({})),
        turn_task_report(),
    ]);
    let r = task_runner(&d, "w-1", script.clone(), tools.clone());
    let s = r.claim_once().await.unwrap().unwrap();
    let (_tx, rx) = watch::channel(false);
    assert_eq!(r.process(s, &rx).await.unwrap(), Outcome::Done);
    assert_eq!(*tools.calls.lock().unwrap(), 1);
    let resp: Value =
        sqlx::query_scalar("SELECT response FROM research_steps WHERE run_id = $1 AND ordinal = 1")
            .bind(run)
            .fetch_one(&d.owner)
            .await
            .unwrap();
    assert_eq!(resp["text"], WEB_TEXT);
    assert!(resp.get("waiting").is_none(), "{resp}");
}
