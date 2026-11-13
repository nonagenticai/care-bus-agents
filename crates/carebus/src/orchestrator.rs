//! Orchestrator agent: subscribes to every area's (from the scope's context spaces) `bus:customer:{area}` and `bus:worker:{area}:results`,
//! turns a qualified request into phased work, and publishes the resolution.
//!
//! It keeps no state in memory. Any replica can handle any message, and a
//! restart resumes from the case hash.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};

use crate::bus::{Bus, ConsumeOptions, Envelope};
use crate::case::{CaseStore, DEFAULT_CASE_DEADLINE};
use crate::domain::{
    Outcome, Plan, ProcedureSource, QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED,
    Resolution, Step, WORK_FAILED, WORK_REQUESTED, WORK_SUCCEEDED, WorkOutcome, WorkRequest,
    resolve_args,
};
use crate::executor::is_timeout_error;
use crate::spaces::{ContextSpace, SpaceCatalog};

pub const GROUP: &str = "orchestrator";
const NAME: &str = "orchestrator";

pub struct Orchestrator<A: ProcedureSource> {
    bus: Bus,
    cases: CaseStore,
    sources: A,
    pub max_attempts: u32,
    pub consume: ConsumeOptions,
    /// An unresolved case older than this is failed by the sweeper.
    case_deadline: Duration,
    /// The scope's context spaces: one customer and one results stream per area; a request's procedure is its
    /// space's.
    spaces: SpaceCatalog,
}

const SWEEP_EVERY: Duration = Duration::from_secs(5);
const SWEEP_BATCH: usize = 50;

impl<A: ProcedureSource> Orchestrator<A> {
    pub fn new(bus: Bus, sources: A) -> Self {
        Self {
            cases: CaseStore::new(bus.clone()),
            bus,
            sources,
            max_attempts: 2,
            consume: ConsumeOptions::default(),
            case_deadline: DEFAULT_CASE_DEADLINE,
            spaces: SpaceCatalog::bootstrap(),
        }
    }

    /// Serve these context spaces (the scope's) instead of the bootstrap job's: their areas' streams.
    pub fn with_spaces(mut self, spaces: SpaceCatalog) -> Self {
        self.spaces = spaces;
        self
    }

    /// The context space a request was qualified into. The request carries its key, area and type, so a space the
    /// catalog does not list yet (added since this task last read it) still gets planned by its key.
    fn space_for(&self, req: &QualifiedRequest) -> ContextSpace {
        self.spaces
            .by_key(&req.context_space)
            .cloned()
            .unwrap_or_else(|| ContextSpace {
                key: req.context_space.clone(),
                area: req.area.clone(),
                request_type: req.request_type.to_string(),
                required: Vec::new(),
                procedure: String::new(),
            })
    }

    /// Fail unresolved cases older than `d` (`CASE_DEADLINE_MS`). Set before `run`.
    pub fn with_case_deadline(mut self, d: Duration) -> Self {
        self.case_deadline = d;
        self.cases = self.cases.clone().with_deadline(d);
        self
    }

    pub fn case_deadline(&self) -> Duration {
        self.case_deadline
    }

    pub fn cases(&self) -> &CaseStore {
        &self.cases
    }

    /// Consume every department's customer and results streams until one fails.
    pub async fn run(self: Arc<Self>, consumer: &str) -> Result<()> {
        let s = &self.bus.streams;
        let areas = self.spaces.areas();
        let customer: Vec<String> = areas.iter().map(|a| s.customer(a)).collect();
        let results: Vec<String> = areas.iter().map(|a| s.worker_results(a)).collect();
        let (a, b, c) = (self.clone(), self.clone(), self.clone());
        tokio::try_join!(
            async move {
                c.sweep_loop().await;
                Ok(())
            },
            self.bus.consume_all(
                customer,
                GROUP,
                consumer,
                self.consume.clone(),
                move |env| {
                    let a = a.clone();
                    async move { a.on_customer(env).await }
                }
            ),
            self.bus
                .consume_all(results, GROUP, consumer, self.consume.clone(), move |env| {
                    let b = b.clone();
                    async move { b.on_result(env).await }
                }),
        )?;
        Ok(())
    }

    /// Sweep overdue cases every few seconds; a failed sweep is logged and retried, never ends `run`.
    async fn sweep_loop(&self) {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match self.sweep_once().await {
                Ok(0) => {}
                Ok(n) => tracing::warn!(failed = n, "deadline sweep failed overdue cases"),
                Err(e) => tracing::error!(error = %format!("{e:#}"), "deadline sweep failed"),
            }
        }
    }

    /// Fail every case past its deadline; returns how many this replica failed.
    /// At most one replica wins each case (`ZREM` claim), and `finish` publishes once.
    pub async fn sweep_once(&self) -> Result<usize> {
        let now = self.cases.now_ms().await?;
        let mut failed = 0;
        for case_id in self.cases.overdue(now, SWEEP_BATCH).await? {
            if !self.cases.claim_overdue(&case_id, now).await? {
                continue;
            }
            // A failed sweep is retried when the claim's lease runs out.
            match self.sweep_case(&case_id).await {
                Ok(true) => failed += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::error!(%case_id, error = %format!("{e:#}"), "sweeping an overdue case failed");
                }
            }
        }
        Ok(failed)
    }

    async fn sweep_case(&self, case_id: &str) -> Result<bool> {
        if self.cases.is_resolved(case_id).await? {
            self.cases.drop_deadline(case_id).await?;
            return Ok(false);
        }
        let Ok(req) = self.cases.request(case_id).await else {
            tracing::warn!(%case_id, "overdue case has no request (hash expired); dropped from the deadline set");
            self.cases.drop_deadline(case_id).await?;
            return Ok(false);
        };
        // Earliest phase first, then work id, so the named worker is deterministic.
        let mut open: Vec<&Value> = Vec::new();
        let work = self.cases.work(case_id).await?;
        open.extend(work.iter().filter(|w| w["status"] == "outstanding"));
        open.sort_by_key(|w| {
            let id = w["work_id"].as_str().unwrap_or_default();
            // work_id = {case}:{phase}:{as}:{attempt}
            let phase = id
                .rsplit(':')
                .nth(2)
                .and_then(|p| p.parse::<usize>().ok())
                .unwrap_or(usize::MAX);
            (phase, id.to_string())
        });
        let outstanding: Vec<String> = open
            .iter()
            .filter_map(|w| w["worker"].as_str().map(String::from))
            .collect();
        let mut names = outstanding.clone();
        names.sort();
        names.dedup();
        let waited = if self.case_deadline.subsec_millis() == 0 {
            format!("{} s", self.case_deadline.as_secs())
        } else {
            format!("{} ms", self.case_deadline.as_millis())
        };
        let detail = format!(
            "deadline exceeded after {waited}; outstanding: {}",
            names.join(", ")
        );
        tracing::warn!(%case_id, error = %detail, "case deadline exceeded");
        self.cases.set_error(case_id, &detail).await?;
        let reason = match outstanding.first() {
            Some(w) => format!("{w} did not answer in time"),
            None => "the request did not finish in time".to_string(),
        };
        self.finish(&req, Outcome::Failed, Some(reason)).await?;
        Ok(true)
    }

    /// customer-bus carries both directions; only `request.qualified` is ours.
    pub async fn on_customer(&self, env: Envelope) -> Result<()> {
        match env.event() {
            Some(REQUEST_QUALIFIED) => self.on_qualified(env).await,
            _ => Ok(()),
        }
    }

    pub async fn on_qualified(&self, env: Envelope) -> Result<()> {
        let req: QualifiedRequest = env.payload_as()?;
        self.cases.create(&req).await?;
        if self.cases.is_resolved(&req.case_id).await? {
            return Ok(());
        }
        let plan = match self.cases.plan(&req.case_id).await? {
            Some(p) => p,
            None => match self.sources.plan(&self.space_for(&req)).await {
                Ok(p) => self.cases.set_plan(&req.case_id, &p).await?,
                Err(e) => {
                    // The raw chain (URLs, hosts) is for operators: logs and the case hash.
                    let detail = format!("no procedure for {}: {e:#}", req.context_space);
                    tracing::warn!(case_id = %req.case_id, error = %detail, "procedure lookup failed");
                    self.cases.set_error(&req.case_id, &detail).await?;
                    let reason = format!(
                        "the procedure registry is unavailable for {}",
                        req.context_space
                    );
                    return self.finish(&req, Outcome::Failed, Some(reason)).await;
                }
            },
        };
        let phase = self.cases.phase(&req.case_id).await?;
        self.start_phase(&req, &plan, phase).await
    }

    pub async fn on_result(&self, env: Envelope) -> Result<()> {
        if !matches!(env.event(), Some(WORK_SUCCEEDED | WORK_FAILED)) {
            return Ok(());
        }
        let o: WorkOutcome = env.payload_as()?;
        if self.cases.is_resolved(&o.case_id).await? {
            return Ok(());
        }
        let req = self.cases.request(&o.case_id).await?;
        let plan = self
            .cases
            .plan(&o.case_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("case {} has no plan", o.case_id))?;
        let status = if o.ok { "succeeded" } else { "failed" };
        self.cases
            .set_work_status(&o.case_id, &o.work_id, &json!({"work_id": o.work_id, "worker": o.worker, "action": o.action, "attempt": o.attempt, "status": status, "error": o.error}))
            .await?;

        if !o.ok {
            if o.retryable && o.attempt < self.max_attempts {
                let step = find_step(&plan, o.phase, &o.as_name)?;
                let ctx = self.cases.context(&req).await?;
                return self
                    .dispatch(
                        &req,
                        o.phase,
                        step,
                        resolve_args(&step.args, &ctx),
                        o.attempt + 1,
                        plan.procedure.as_deref(),
                    )
                    .await;
            }
            // The raw error is already on the case (work status); the customer gets a category.
            let error = o.error.as_deref().unwrap_or("failed");
            tracing::warn!(case_id = %o.case_id, work_id = %o.work_id, %error, "work failed, case fails");
            return self
                .finish(
                    &req,
                    Outcome::Failed,
                    Some(tool_failure_reason(&o.worker, error)),
                )
                .await;
        }

        self.cases
            .record_result(
                &o.case_id,
                o.phase,
                &o.as_name,
                o.result.as_ref().unwrap_or(&Value::Null),
            )
            .await?;
        self.maybe_advance(&req, &plan, o.phase).await
    }

    /// Safe to call repeatedly: dispatch ids are deterministic and steps with
    /// a recorded result are skipped.
    async fn start_phase(&self, req: &QualifiedRequest, plan: &Plan, idx: usize) -> Result<()> {
        let Some(phase) = plan.phases.get(idx) else {
            return self.finish(req, Outcome::Completed, None).await;
        };
        self.cases
            .init_pending(&req.case_id, idx, phase.steps.len())
            .await?;
        let ctx = self.cases.context(req).await?;
        for step in &phase.steps {
            if !self.cases.has_result(&req.case_id, &step.as_name).await? {
                self.dispatch(
                    req,
                    idx,
                    step,
                    resolve_args(&step.args, &ctx),
                    1,
                    plan.procedure.as_deref(),
                )
                .await?;
            }
        }
        // A redelivery after every result landed has nothing to dispatch.
        self.maybe_advance(req, plan, idx).await
    }

    async fn maybe_advance(&self, req: &QualifiedRequest, plan: &Plan, idx: usize) -> Result<()> {
        if self.cases.remaining(&req.case_id, idx).await? > 0 {
            return Ok(());
        }
        if let Some(gate) = plan.phases[idx].gate.as_deref() {
            let results = self.cases.results(&req.case_id).await?;
            if results.get(gate).and_then(|g| g.get("eligible")) == Some(&Value::Bool(false)) {
                let reason = results[gate]
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(String::from);
                return self.finish(req, Outcome::NotEligible, reason).await;
            }
        }
        self.cases.advance_phase(&req.case_id, idx, idx + 1).await?;
        if self.cases.phase(&req.case_id).await? == idx + 1 {
            return Box::pin(self.start_phase(req, plan, idx + 1)).await;
        }
        Ok(())
    }

    async fn dispatch(
        &self,
        req: &QualifiedRequest,
        phase: usize,
        step: &Step,
        args: Value,
        attempt: u32,
        procedure: Option<&str>,
    ) -> Result<()> {
        let key = format!("{}:{phase}:{}", req.case_id, step.as_name);
        let work = WorkRequest {
            work_id: format!("{key}:{attempt}"),
            idempotency_key: key,
            case_id: req.case_id.clone(),
            phase,
            as_name: step.as_name.clone(),
            worker: step.worker.clone(),
            action: step.action.clone(),
            args,
            attempt,
            request_type: req.request_type.clone(),
            side_effect: step.side_effect,
        };
        self.cases
            .set_work_status(&req.case_id, &work.work_id, &json!({"work_id": work.work_id, "worker": work.worker, "action": work.action, "attempt": attempt, "status": "outstanding"}))
            .await?;
        let env = Envelope::new(&req.topic(WORK_REQUESTED), NAME, &req.case_id, &work)?
            .with_procedure(procedure);
        self.bus
            .publish(&self.bus.streams.worker(&req.area, &step.worker), &env)
            .await?;
        Ok(())
    }

    async fn finish(
        &self,
        req: &QualifiedRequest,
        outcome: Outcome,
        reason: Option<String>,
    ) -> Result<()> {
        if self.cases.is_resolved(&req.case_id).await? {
            return Ok(());
        }
        let proposed = Resolution {
            case_id: req.case_id.clone(),
            request_type: req.request_type.clone(),
            outcome,
            reason,
            facts: self.cases.results(&req.case_id).await?,
        };
        // First writer defines THE resolution; every caller publishes the stored one, so a sweep and a
        // final result racing on one case cannot publish two different outcomes.
        let res = self.cases.set_resolution(&req.case_id, &proposed).await?;
        // The version the case ran, from the plan stored on the case (first plan wins, so every replica and a
        // republish stamp the same one). None when no plan was ever chosen (registry down) or it was built in.
        let procedure = self
            .cases
            .plan(&req.case_id)
            .await?
            .and_then(|p| p.procedure);
        // Publish before marking: a crash in between republishes, and the
        // interaction agent drops the duplicate by case id.
        let env = Envelope::new(&req.topic(REQUEST_RESOLVED), NAME, &req.case_id, &res)?
            .with_procedure(procedure.as_deref());
        self.bus
            .publish(&self.bus.streams.customer(&req.area), &env)
            .await?;
        self.cases
            .mark_resolved(&req.case_id, &serde_json::to_string(&res.outcome)?)
            .await
    }
}

/// Customer-safe reason for a failed tool step: never the raw error, which can carry URLs and hosts.
pub fn tool_failure_reason(worker: &str, error: &str) -> String {
    if is_timeout_error(error) {
        format!("{worker} did not answer in time")
    } else {
        format!("{worker} is unavailable")
    }
}

fn find_step<'a>(plan: &'a Plan, phase: usize, as_name: &str) -> Result<&'a Step> {
    plan.phases
        .get(phase)
        .and_then(|p| p.steps.iter().find(|s| s.as_name == as_name))
        .ok_or_else(|| anyhow::anyhow!("no step {as_name} in phase {phase}"))
}
