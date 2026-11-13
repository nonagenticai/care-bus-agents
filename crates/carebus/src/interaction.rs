//! Interaction agent: talks to the customer, holds a draft until every
//! required field is known, and only then publishes to `bus:customer`.

use std::sync::Arc;

use anyhow::{Context, Result};
use redis::AsyncCommands;
use rig_agent::AgentBuilder;
use rig_agent::extractor::{Extractor, ExtractorBuilder};
use rig_core::http_client::{self, DynHttpClient, HttpMiddleware};
use rig_core::providers::openai::OpenAIConfig;
use rig_core::wasm_compat::WasmBoxedFuture;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bus::{Bus, ConsumeOptions, Envelope};
use crate::domain::{
    Outcome, QualifiedRequest, REQUEST_QUALIFIED, REQUEST_RESOLVED, RequestType, Resolution, Slots,
};
use crate::gates::{GateClient, RequestDecision, RequestGate, Verdict, response_state};
use crate::spaces::{ContextSpace, SpaceCatalog};
use crate::tools;

pub const GROUP: &str = "interaction";

/// What the qualifier reads out of one customer message.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Extraction {
    /// One of the `request_types` listed in the prompt, or null if the message does not say.
    pub request_type: Option<String>,
    /// The fields the message gives, each under its name from the request type's `fields` in the prompt
    /// (e.g. `order_id`, `reason`). Flattened so the tool schema has no nested `$ref` object: vLLM's Qwen
    /// tool parser returns an untyped nested object as a JSON string.
    #[serde(flatten)]
    pub slots: Slots,
}

/// The bootstrap job's request types: the keyword rules, questions and reply templates below know these. Any
/// other request type of the job (a context space added in a release) gets the generic behaviour.
pub const BUILT_IN: [&str; 5] = [
    "return",
    "warranty_claim",
    "delivery_status",
    "payment_error",
    "faq",
];

/// What the model is told a field means; the field's own name for fields the bootstrap job does not have.
pub fn field_hint(field: &str) -> &str {
    match field {
        "order_id" => "Order number like ORD-58213.",
        "reason" => "Why the customer returns the item.",
        "defect" => "What is wrong with the item.",
        "error_type" => "duplicate_charge or payment_declined.",
        "question" => "The customer's general question.",
        other => other,
    }
}

/// The request types and their fields as the qualifier's prompt lists them.
fn request_types_prompt(spaces: &SpaceCatalog) -> Value {
    spaces
        .spaces()
        .iter()
        .map(|s| {
            let fields: serde_json::Map<String, Value> = s
                .required
                .iter()
                .map(|f| (f.clone(), Value::String(field_hint(f).to_string())))
                .collect();
            serde_json::json!({"request_type": s.request_type, "fields": fields})
        })
        .collect()
}

/// conversation-start-data: most recent records only (last 90 days).
#[derive(Clone, Debug, Serialize)]
pub struct StartData {
    pub customer: Value,
    pub orders: Vec<Value>,
    pub payments: Vec<Value>,
}

pub fn conversation_start_data() -> StartData {
    let orders: Vec<Value> = tools::orders()
        .into_iter()
        .filter(|o| o["placed"].as_str().is_some_and(|p| p >= "2026-07-17"))
        .collect();
    let ids: Vec<&str> = orders
        .iter()
        .filter_map(|o| o["order_id"].as_str())
        .collect();
    let payments = tools::payments()
        .into_iter()
        .filter(|p| p["order_id"].as_str().is_some_and(|id| ids.contains(&id)))
        .collect();
    StartData {
        customer: tools::customer(),
        orders,
        payments,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub request_type: RequestType,
    pub slots: Slots,
    pub utterance: String,
    pub awaiting: Option<String>,
}

pub trait Qualifier: Send + Sync + 'static {
    /// `spaces` are the scope's context spaces: the request types (and their fields) the message can qualify as.
    fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> impl std::future::Future<Output = Result<Extraction>> + Send;
}

/// Keyword rules. Deterministic, for tests and running without a model. The bootstrap request types have their
/// keyword rules; any other request type of the job is recognised when the message names it (`gift card` for
/// `gift_card`), and its fields are filled one answer per question.
pub struct RuleQualifier;

impl Qualifier for RuleQualifier {
    async fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> Result<Extraction> {
        let t = text.to_lowercase();
        let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));
        let named = spaces
            .spaces()
            .iter()
            .map(|s| s.request_type.as_str())
            .filter(|rt| !BUILT_IN.contains(rt))
            .find(|rt| t.contains(&rt.replace('_', " ")));
        let request_type = named.or_else(|| {
            if has(&["charged", "payment", "declined", "card"]) {
                Some("payment_error")
            } else if has(&["how long", "how do", "policy", "what is"]) && !has(&["my "]) {
                Some("faq")
            } else if has(&["warranty", "stopped working", "broken", "defect", "faulty"]) {
                Some("warranty_claim")
            } else if has(&["return", "send it back"]) {
                Some("return")
            } else if has(&["where is", "track", "deliver", "parcel", "arrive"]) {
                Some("delivery_status")
            } else {
                None
            }
        });
        let mut slots = Slots::default();
        if let Some(id) = find_order_id(&t, start) {
            slots.set("order_id", id);
        }
        let awaiting = draft.and_then(|d| d.awaiting.as_deref());
        let rt = request_type.or(draft.map(|d| d.request_type.as_str()));
        match rt {
            Some("return")
                if awaiting == Some("reason")
                    || has(&["because", "too ", "doesn't fit", "don't like"]) =>
            {
                slots.set("reason", text)
            }
            Some("warranty_claim")
                if awaiting == Some("defect") || has(&["stopped working", "broken", "faulty"]) =>
            {
                slots.set("defect", text)
            }
            Some("payment_error") if has(&["twice", "double", "duplicate"]) => {
                slots.set("error_type", "duplicate_charge")
            }
            Some("payment_error") if has(&["declined", "failed"]) => {
                slots.set("error_type", "payment_declined")
            }
            Some("faq") => slots.set("question", text),
            // Any other request type: the message answers the field it was asked for.
            Some(other) if !BUILT_IN.contains(&other) => {
                if let Some(field) = awaiting.filter(|f| slots.get(f).is_none()) {
                    slots.set(field, text);
                }
            }
            _ => {}
        }
        // Infer from conversation-start-data when the customer did not say.
        if slots.get("order_id").is_none() && rt == Some("delivery_status") {
            let open: Vec<&Value> = start
                .orders
                .iter()
                .filter(|o| o["status"] == "in_transit")
                .collect();
            if let [only] = open.as_slice()
                && let Some(id) = only["order_id"].as_str()
            {
                slots.set("order_id", id);
            }
        }
        if slots.get("order_id").is_none() && slots.get("error_type") == Some("duplicate_charge") {
            let dup = start
                .payments
                .windows(2)
                .find(|w| w[0]["order_id"] == w[1]["order_id"] && w[0]["amount"] == w[1]["amount"]);
            if let Some(id) = dup.and_then(|w| w[0]["order_id"].as_str()) {
                slots.set("order_id", id);
            }
        }
        Ok(Extraction {
            request_type: request_type.map(String::from),
            slots,
        })
    }
}

fn find_order_id(t: &str, start: &StartData) -> Option<String> {
    if let Some(i) = t.find("ord-") {
        let digits: String = t[i + 4..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.len() == 5 {
            return Some(format!("ORD-{digits}"));
        }
    }
    start.orders.iter().find_map(|o| {
        let item = o["item"].as_str()?.to_lowercase();
        let hit = item
            .split_whitespace()
            .filter(|w| w.len() > 4)
            .any(|w| t.contains(w.trim_end_matches('s')));
        hit.then(|| o["order_id"].as_str().map(String::from))
            .flatten()
    })
}

const PREAMBLE: &str = "You are the intake step of a customer-care desk. From the customer's latest \
message, the draft request so far and the customer's recent records, extract the request type (one of \
`request_types`) and any of its `fields` the message provides, each under its field name. Use order ids \
from the recent records when the customer names the item. Never invent a value: leave a field out when \
the message and records do not give it.";

/// Default OpenAI-compatible endpoint: the vLLM gateway serving `local-llm`.
pub const DEFAULT_LLM_BASE_URL: &str = "https://api.nonagentic.ai/vllm/v1";
/// Default model id on that endpoint (Qwen3.8-27B, a thinking model).
pub const DEFAULT_MODEL: &str = "local-llm";
pub const LLM_TIMEOUT_SECS: u64 = 60;
/// Extractor retries after the first attempt.
pub const LLM_RETRIES: u64 = 2;
/// Worst-case time one `Qualifier::extract` call can take.
pub const QUALIFIER_BUDGET_SECS: u64 = LLM_TIMEOUT_SECS * (LLM_RETRIES + 1);
pub const LLM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(LLM_TIMEOUT_SECS);
/// Reasoning tokens count against this budget, so it is generous.
const MAX_TOKENS: u64 = 4096;

/// What one model-backed extraction spent, all retries included (W3 plan 15: the API writes it to its usage ledger).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlmUsage {
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Completion requests of the accepted attempt.
    pub calls: u64,
    pub latency_ms: u64,
}

tokio::task_local! {
    static LLM_USAGE: std::cell::RefCell<Vec<LlmUsage>>;
}

/// Hand `u` to the enclosing [`metered`] call; a no-op outside one.
pub fn record_llm_usage(u: LlmUsage) {
    let _ = LLM_USAGE.try_with(|c| c.borrow_mut().push(u));
}

/// Run `fut` and return what every model call inside it recorded, so a caller can meter usage without the
/// `Qualifier` trait carrying it.
pub async fn metered<F: std::future::Future>(fut: F) -> (F::Output, Vec<LlmUsage>) {
    LLM_USAGE
        .scope(std::cell::RefCell::new(Vec::new()), async move {
            let out = fut.await;
            (out, LLM_USAGE.with(|c| c.take()))
        })
        .await
}

/// Provider recorded for the qualifier's endpoint (the vLLM gateway), as billing's `provider_pricing` keys it.
pub const LLM_PROVIDER: &str = "vllm";

/// The end tenant a platform-model call is made for, so inference-router bills that tenant rather than care-bus'
/// own licence. Sent only to the platform's model, never to a tenant's own provider. The vLLM gateway ignores it.
pub const EFFECTIVE_TENANT_HEADER: &str = "x-effective-tenant-id";

tokio::task_local! {
    static EFFECTIVE_TENANT: uuid::Uuid;
}

/// Run `fut` with `tenant` as the end tenant of every platform-model call inside it ([`EFFECTIVE_TENANT_HEADER`]).
pub async fn with_effective_tenant<F: std::future::Future>(
    tenant: uuid::Uuid,
    fut: F,
) -> F::Output {
    EFFECTIVE_TENANT.scope(tenant, fut).await
}

/// The enclosing [`with_effective_tenant`] tenant, `None` outside one.
pub fn effective_tenant() -> Option<uuid::Uuid> {
    EFFECTIVE_TENANT.try_with(|t| *t).ok()
}

/// Sets [`EFFECTIVE_TENANT_HEADER`] and the caller identity inference-router requires (`X-App-Id`, `X-Project-Id`,
/// `X-Actor-Id`: [`crate::identity::router_headers`]) from the enclosing [`with_effective_tenant`] on every request
/// of the platform qualifier's rig client; outside one the request goes without any of them. No person is behind a
/// qualifier call (a conversation's customers are not Keycloak users), so the actor is `service_actor`
/// ([`crate::identity::SERVICE_ACTOR_ENV`]), and with none set no `X-Actor-Id` is sent.
#[derive(Clone, Copy, Debug, Default)]
pub struct EffectiveTenantHeader {
    pub service_actor: Option<uuid::Uuid>,
}

impl HttpMiddleware for EffectiveTenantHeader {
    fn before_request_headers<'a>(
        &'a self,
        _method: &'a http_client::Method,
        _uri: &'a http_client::Uri,
        headers: &'a mut http_client::HeaderMap,
    ) -> WasmBoxedFuture<'a, http_client::Result<()>> {
        if let Some(tenant) = effective_tenant() {
            for (name, value) in crate::identity::router_headers(tenant, self.service_actor) {
                if let Ok(v) = http_client::HeaderValue::try_from(value) {
                    headers.insert(name, v);
                }
            }
        }
        Box::pin(async { Ok(()) })
    }
}

/// Model-backed qualifier on rig's typed extractor, over an OpenAI-compatible
/// Chat Completions endpoint (vLLM).
pub struct RigQualifier {
    extractor: Extractor<Extraction>,
    model_id: String,
}

impl RigQualifier {
    /// Reads `VLLM_LICENSE_TOKEN` (required), `CAREBUS_LLM_BASE_URL`, `CAREBUS_MODEL` and `SERVICE_ACTOR_ID`
    /// (optional; malformed is an error).
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("VLLM_LICENSE_TOKEN").context("VLLM_LICENSE_TOKEN not set")?;
        let base_url =
            std::env::var("CAREBUS_LLM_BASE_URL").unwrap_or_else(|_| DEFAULT_LLM_BASE_URL.into());
        let model_id = std::env::var("CAREBUS_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
        let service_actor = crate::identity::service_actor_from_env()?;
        Self::with_service_actor(token, base_url, model_id, service_actor)
    }

    /// The platform model at `base_url` (its calls carry [`EFFECTIVE_TENANT_HEADER`], `X-App-Id` and
    /// `X-Project-Id` inside [`with_effective_tenant`]), without an `X-Actor-Id`.
    pub fn new(token: String, base_url: String, model_id: String) -> Result<Self> {
        Self::with_service_actor(token, base_url, model_id, None)
    }

    /// [`RigQualifier::new`] whose calls inside [`with_effective_tenant`] also carry `X-Actor-Id` = `service_actor`.
    pub fn with_service_actor(
        token: String,
        base_url: String,
        model_id: String,
        service_actor: Option<uuid::Uuid>,
    ) -> Result<Self> {
        let http = rig_reqwest::reqwest::Client::builder()
            .timeout(LLM_TIMEOUT)
            .build()
            .context("building the HTTP client")?;
        // The platform model: its calls carry the end tenant and care-bus' identity (EffectiveTenantHeader).
        let http = DynHttpClient::new(rig_reqwest::ReqwestClient::from(http))
            .with_middleware(EffectiveTenantHeader { service_actor });
        Ok(Self::build(token, base_url, model_id, http))
    }

    /// Over a caller-built HTTP client (carebus-api: a tenant's provider, no redirects, its host checked and
    /// pinned). The caller sets the timeout ([`LLM_TIMEOUT`]). A tenant's own provider: no
    /// [`EFFECTIVE_TENANT_HEADER`] and none of care-bus' identity headers.
    pub fn with_client(
        token: String,
        base_url: String,
        model_id: String,
        http: rig_reqwest::reqwest::Client,
    ) -> Self {
        Self::build(
            token,
            base_url,
            model_id,
            DynHttpClient::new(rig_reqwest::ReqwestClient::from(http)),
        )
    }

    fn build(token: String, base_url: String, model_id: String, http: DynHttpClient) -> Self {
        let model = OpenAIConfig::new(token)
            .with_base_url(base_url)
            .connect(http)
            .chat(model_id.clone());
        let extractor = ExtractorBuilder::from_agent_builder(
            AgentBuilder::new(model)
                .preamble(PREAMBLE)
                .max_tokens(MAX_TOKENS),
        )
        .retries(LLM_RETRIES as usize)
        .build();
        Self {
            extractor,
            model_id,
        }
    }
}

impl Qualifier for RigQualifier {
    async fn extract(
        &self,
        text: &str,
        draft: Option<&Draft>,
        start: &StartData,
        spaces: &SpaceCatalog,
    ) -> Result<Extraction> {
        let prompt = serde_json::json!({
            "message": text,
            "draft": draft,
            "recent_records": start,
            "request_types": request_types_prompt(spaces),
        })
        .to_string();
        let started = std::time::Instant::now();
        let run = self
            .extractor
            .extract(prompt)
            .await
            .map_err(|e| anyhow::anyhow!("extraction failed: {e}"))?;
        record_llm_usage(LlmUsage {
            provider: LLM_PROVIDER.into(),
            model: self.model_id.clone(),
            input_tokens: run.usage.input_tokens.unwrap_or(0),
            output_tokens: run.usage.output_tokens.unwrap_or(0),
            calls: run.completion_calls.len() as u64,
            latency_ms: started.elapsed().as_millis() as u64,
        });
        // The model fills unknown fields with "" or null; `Slots` drops both, a blank is not a value.
        Ok(run.output)
    }
}

#[derive(Debug, PartialEq)]
pub enum Turn {
    /// Not qualified yet: ask the customer this.
    Ask(String),
    /// Published to the customer bus.
    Published(String),
    /// No request type recognised.
    Unclear(String),
    /// Qualified, but a request gate stopped it (handoff/block, spec 2026-10-18 §3): not published; the customer
    /// reads this text.
    Gated(String),
}

pub struct Interaction<Q: Qualifier> {
    bus: Bus,
    qualifier: Q,
    pub start: StartData,
    /// The scope's context spaces: the request types a message can qualify as, their fields, and the customer
    /// streams (one per area) requests are published to and replies read from.
    spaces: SpaceCatalog,
    /// The request point (carebus-api's turn); `None`: every qualified request is published.
    request_gate: Option<Arc<dyn RequestGate>>,
    /// The response point through carebus-api (the daemon); `None`: replies are sent as composed.
    gates: Option<GateClient>,
}

impl<Q: Qualifier> Interaction<Q> {
    pub fn new(bus: Bus, qualifier: Q) -> Self {
        Self {
            bus,
            qualifier,
            start: conversation_start_data(),
            spaces: SpaceCatalog::bootstrap(),
            request_gate: None,
            gates: None,
        }
    }

    /// Run `gate` on every inbound turn before it is answered (and before a completed request is published).
    pub fn with_request_gate(mut self, gate: Arc<dyn RequestGate>) -> Self {
        self.request_gate = Some(gate);
        self
    }

    /// Gate every composed reply (point `response`) through carebus-api; `None` leaves replies ungated.
    pub fn with_gates(mut self, gates: Option<GateClient>) -> Self {
        self.gates = gates;
        self
    }

    /// Qualify against these context spaces (the scope's) instead of the bootstrap job's.
    pub fn with_spaces(mut self, spaces: SpaceCatalog) -> Self {
        self.spaces = spaces;
        self
    }

    pub fn spaces(&self) -> &SpaceCatalog {
        &self.spaces
    }

    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    pub async fn handle(&self, draft: &mut Option<Draft>, text: &str) -> Result<Turn> {
        let ex = self
            .qualifier
            .extract(text, draft.as_ref(), &self.start, &self.spaces)
            .await?;
        // Only a request type of this job's context spaces counts; anything else is no request type.
        let named: Option<RequestType> = ex
            .request_type
            .as_deref()
            .and_then(|rt| self.spaces.by_request_type(rt))
            .and_then(|s| RequestType::new(s.request_type.as_str()).ok());
        let keep =
            matches!(&*draft, Some(d) if named.as_ref().is_none_or(|rt| *rt == d.request_type));
        // Where this turn stands: `answer` when it is not ready (unclear, or a field is missing), else the context
        // space of the request it completes. The request gate sees every turn before any answer goes out.
        let mut answer: Option<Turn> = None;
        let mut ready: Option<&ContextSpace> = None;
        if !keep {
            match named {
                None => answer = Some(Turn::Unclear(self.unclear())),
                Some(rt) => {
                    *draft = Some(Draft {
                        request_type: rt,
                        slots: Slots::default(),
                        utterance: text.into(),
                        awaiting: None,
                    })
                }
            }
        }
        if answer.is_none() {
            let d = draft.as_mut().context("draft exists")?;
            d.slots.merge(ex.slots);
            match self.spaces.space_of(&d.request_type) {
                None => {
                    // A stored draft whose context space the job no longer has (a release removed it): start over.
                    *draft = None;
                    answer = Some(Turn::Unclear(self.unclear()));
                }
                Some(space) => {
                    if let Some(first) = d.slots.missing(&space.required).first() {
                        d.awaiting = Some((*first).to_string());
                        answer = Some(Turn::Ask(self.question(first)));
                    } else {
                        ready = Some(space);
                    }
                }
            }
        }
        let mut req: Option<QualifiedRequest> = match (ready, draft.as_ref()) {
            (Some(space), Some(d)) => Some(QualifiedRequest {
                case_id: format!("CASE-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
                customer_id: self.start.customer["customer_id"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                request_type: d.request_type.clone(),
                area: space.area.clone(),
                context_space: space.key.clone(),
                slots: d.slots.clone(),
                utterance: d.utterance.clone(),
                gates: None,
            }),
            _ => None,
        };
        if let Some(gate) = &self.request_gate {
            match gate.check(text, draft.as_ref(), req.as_ref()).await {
                RequestDecision::Pass => {}
                // Attached only to a request this turn publishes.
                RequestDecision::Annotate(answers) => {
                    if let Some(r) = req.as_mut() {
                        r.gates = Some(answers);
                    }
                }
                RequestDecision::Stop(message) => {
                    // Handed off or blocked: this request is done, the next message starts a new one.
                    *draft = None;
                    return Ok(Turn::Gated(message));
                }
            }
        }
        let (Some(space), Some(req)) = (ready, req) else {
            return answer.context("a turn that publishes nothing has an answer");
        };
        self.publish_qualified(space, &req).await?;
        *draft = None;
        Ok(Turn::Published(req.case_id))
    }

    async fn publish_qualified(&self, space: &ContextSpace, req: &QualifiedRequest) -> Result<()> {
        let env = Envelope::new(
            &space.topic(REQUEST_QUALIFIED),
            "interaction",
            &req.case_id,
            req,
        )?;
        self.bus
            .publish(&self.bus.streams.customer(&space.area), &env)
            .await?;
        Ok(())
    }

    /// What the customer reads when no request type of this job was recognised.
    fn unclear(&self) -> String {
        let names: Vec<String> = self
            .spaces
            .spaces()
            .iter()
            .map(|s| s.request_type.replace('_', " "))
            .collect();
        let bootstrap = names.len() == BUILT_IN.len()
            && BUILT_IN
                .iter()
                .all(|b| self.spaces.by_request_type(b).is_some());
        if names.is_empty() {
            "I can't take requests here yet. A colleague will reply by email.".into()
        } else if bootstrap {
            "I can help with returns, warranty claims, deliveries, payments and general questions. What do you need?".into()
        } else {
            format!("I can help with: {}. What do you need?", names.join(", "))
        }
    }

    fn question(&self, slot: &str) -> String {
        match slot {
            "order_id" => {
                let items: Vec<String> = self
                    .start
                    .orders
                    .iter()
                    .map(|o| {
                        format!(
                            "{} ({})",
                            o["item"].as_str().unwrap_or("?"),
                            o["order_id"].as_str().unwrap_or("?")
                        )
                    })
                    .collect();
                format!(
                    "Which order is this about? I can see {}. For older orders, please share the order number.",
                    items.join(" and ")
                )
            }
            "reason" => "What's the reason you'd like to return it?".into(),
            "defect" => "What exactly is wrong with it?".into(),
            "error_type" => "Were you charged twice, or was a payment declined?".into(),
            "question" => "What's your question?".into(),
            other => format!("Could you tell me the {}?", other.replace('_', " ")),
        }
    }

    /// Consume resolutions and hand each composed reply to `reply` once per case.
    pub async fn run_replies<F>(self: Arc<Self>, consumer: &str, reply: F) -> Result<()>
    where
        F: Fn(String, String) + Send + Sync + 'static,
    {
        let reply = Arc::new(reply);
        let me = self.clone();
        let streams: Vec<String> = self
            .spaces
            .areas()
            .iter()
            .map(|a| self.bus.streams.customer(a))
            .collect();
        self.bus
            .consume_all(
                streams,
                GROUP,
                consumer,
                ConsumeOptions::default(),
                move |env| {
                    let (me, reply) = (me.clone(), reply.clone());
                    async move {
                        if env.event() != Some(REQUEST_RESOLVED) {
                            return Ok(());
                        }
                        let res: Resolution = env.payload_as()?;
                        // Gated before the reply is claimed, so a redelivery reuses the stored verdict.
                        let text = me.gated_reply(&res).await?;
                        let first = me
                            .bus
                            .conn()
                            .set_nx::<_, _, bool>(
                                me.bus.streams.key(&format!("replied:{}", res.case_id)),
                                1,
                            )
                            .await?;
                        if first {
                            reply(res.case_id.clone(), text);
                        }
                        Ok(())
                    }
                },
            )
            .await
    }
}

impl<Q: Qualifier> Interaction<Q> {
    /// The composed reply after the response gate: replaced by the gate's handoff text when it fired or failed
    /// closed. The first verdict carebus-api returns per case is stored (`gate:{case}`, 24 h) and every redelivery
    /// reuses it. A transport error to carebus-api counts as failed closed for this delivery only (whether the
    /// point has fail-closed questions is unknown then, and the response gate is a safety gate); it is not stored.
    async fn gated_reply(&self, res: &Resolution) -> Result<String> {
        let reply = compose_reply(res);
        let (Some(gates), Some(scope)) = (&self.gates, self.bus.scope()) else {
            return Ok(reply);
        };
        let key = self.bus.streams.key(&format!("gate:{}", res.case_id));
        let mut c = self.bus.conn();
        let stored: Option<String> = c.get(&key).await?;
        let verdict = match stored.and_then(|s| serde_json::from_str::<Verdict>(&s).ok()) {
            Some(v) => v,
            None => {
                let state = response_state(&reply, res);
                let v = match gates
                    .evaluate(scope, "response", &state, Some(&res.case_id), None)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        // Not stored: a transport error is not carebus-api's verdict, so a redelivery asks again.
                        // This delivery still fails closed.
                        tracing::warn!(case = %res.case_id, error = %format!("{e:#}"), "response gate unreachable; failing closed");
                        return Ok(Verdict::failed_closed("carebus-api unreachable").text());
                    }
                };
                // First verdict wins: a concurrent delivery that stored one already decides for both.
                let prev: Option<String> = redis::cmd("SET")
                    .arg(&key)
                    .arg(serde_json::to_string(&v)?)
                    .arg("NX")
                    .arg("GET")
                    .arg("EX")
                    .arg(86_400)
                    .query_async(&mut c)
                    .await?;
                prev.and_then(|p| serde_json::from_str(&p).ok())
                    .unwrap_or(v)
            }
        };
        Ok(if verdict.stops() {
            verdict.text()
        } else {
            reply
        })
    }
}

/// What the customer reads when the faq tool produced no grounded answer.
pub const FAQ_FALLBACK: &str = "I couldn't find an answer. A colleague will reply by email.";

/// Template replies. A rig agent can phrase these instead; the facts stay the same.
pub fn compose_reply(r: &Resolution) -> String {
    let f = &r.facts;
    let s = |obj: &str, k: &str| {
        f.get(obj)
            .and_then(|o| o.get(k))
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default()
    };
    match r.outcome {
        Outcome::Failed => format!(
            "I couldn't finish this automatically ({}). A colleague will follow up by email today.",
            r.reason.as_deref().unwrap_or("unknown error")
        ),
        Outcome::NotEligible => r
            .reason
            .clone()
            .unwrap_or_else(|| "This request isn't eligible.".into()),
        Outcome::Completed => match r.request_type.as_str() {
            "delivery_status" if s("shipment", "status") == "delivered" => format!(
                "Your {} was delivered on {}.",
                s("order", "item"),
                s("shipment", "delivered_at")
            ),
            "delivery_status" => format!(
                "Your {} is with {}. Last update: {}. Expected: {}.",
                s("order", "item"),
                s("shipment", "carrier"),
                s("shipment", "last_event"),
                s("shipment", "eta")
            ),
            "return" => format!(
                "Return approved: {} for your {}. A prepaid {} label is on its way by email.",
                s("rma", "rma_id"),
                s("order", "item"),
                s("rma", "carrier")
            ),
            "warranty_claim" => format!(
                "Warranty claim {} registered for your {}. {}",
                s("claim", "claim_id"),
                s("order", "item"),
                s("claim", "next_step")
            ),
            "payment_error" => format!(
                "{} was a duplicate charge. Refund {} of €{} is issued and arrives within {}.",
                s("rules", "refund_payment_id"),
                s("refund", "refund_id"),
                s("refund", "amount"),
                s("refund", "eta")
            ),
            // faq_ask returns `{answer, citations, sources, timings_ms}`. Only an answer
            // that cites at least one source is grounded; anything else escalates.
            "faq" => f
                .get("faq")
                .filter(|x| x["citations"].as_array().is_some_and(|c| !c.is_empty()))
                .and_then(|x| x["answer"].as_str())
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .unwrap_or(FAQ_FALLBACK)
                .into(),
            // A request type of a context space added in a release: no template, a plain confirmation.
            other => format!(
                "Your {} request is done. A confirmation is on its way by email.",
                other.replace('_', " ")
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metered_collects_what_the_calls_inside_it_record() {
        // outside a `metered` call recording is a silent no-op
        record_llm_usage(LlmUsage::default());
        let u = LlmUsage {
            provider: LLM_PROVIDER.into(),
            model: "local-llm".into(),
            input_tokens: 812,
            output_tokens: 64,
            calls: 1,
            latency_ms: 5,
        };
        let recorded = u.clone();
        let (out, got) = metered(async move {
            record_llm_usage(recorded);
            7
        })
        .await;
        assert_eq!(out, 7);
        assert_eq!(got, vec![u]);
        let (_, none) = metered(async {}).await;
        assert!(none.is_empty());
    }

    async fn effective_tenant_headers_as(
        service_actor: Option<uuid::Uuid>,
    ) -> http_client::HeaderMap {
        let mut h = http_client::HeaderMap::new();
        let uri: http_client::Uri = "http://vllm.example:8001/v1/chat/completions"
            .parse()
            .unwrap();
        EffectiveTenantHeader { service_actor }
            .before_request_headers(&http_client::Method::POST, &uri, &mut h)
            .await
            .unwrap();
        h
    }

    async fn effective_tenant_headers() -> http_client::HeaderMap {
        effective_tenant_headers_as(None).await
    }

    /// Inside the scope the middleware adds the router's caller identity next to the tenant; outside it, none.
    #[tokio::test]
    async fn the_platform_client_names_care_bus_the_project_and_the_service_actor_inside_its_scope()
    {
        let actor = uuid::Uuid::new_v4();
        let outside = effective_tenant_headers_as(Some(actor)).await;
        assert!(
            outside.is_empty(),
            "no identity outside a tenant scope: {outside:?}"
        );
        let tenant = uuid::Uuid::new_v4();
        let h = with_effective_tenant(tenant, effective_tenant_headers_as(Some(actor))).await;
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        assert_eq!(get("X-App-Id").as_deref(), Some(crate::identity::APP_ID));
        assert_eq!(
            get("X-Project-Id"),
            Some(crate::identity::tenant_level_project_id(tenant).to_string())
        );
        assert_eq!(get("X-Actor-Id"), Some(actor.to_string()));
        // without a configured service actor: no X-Actor-Id, never a placeholder
        let h = with_effective_tenant(tenant, effective_tenant_headers()).await;
        assert!(h.get("X-Actor-Id").is_none());
        assert!(h.get("X-App-Id").is_some());
    }

    #[tokio::test]
    async fn the_platform_client_sends_the_effective_tenant_only_inside_its_scope() {
        // outside `with_effective_tenant`: no header
        assert!(effective_tenant().is_none());
        assert!(
            effective_tenant_headers()
                .await
                .get(EFFECTIVE_TENANT_HEADER)
                .is_none()
        );
        let tenant = uuid::Uuid::new_v4();
        let h = with_effective_tenant(tenant, effective_tenant_headers()).await;
        assert_eq!(
            h.get("X-Effective-Tenant-Id").and_then(|v| v.to_str().ok()),
            Some(tenant.to_string().as_str())
        );
        // the scope ends with its future
        assert!(effective_tenant().is_none());
    }

    /// The `request.resolved` payload of CASE-42b46a3b, captured from the FAQ bus
    /// (2026-10-16): faq_ask's real `{answer, citations, sources, timings_ms}`.
    const LIVE_FAQ_RESOLVED: &str = r#"{"case_id":"CASE-42b46a3b","request_type":"faq","outcome":"completed","reason":null,"facts":{"faq":{"answer":"The email address of Leanne Graham is Sincere@april.biz [1].","citations":["01a1456d-6310-75e4-abd3-fff43ecec074"],"sources":[{"chunk_id":"01a1456d-6310-75e4-abd3-fff43ecec074","document_id":"01a1456d-62dc-72fd-8c0b-c4a0f28f99e4","store_path":["e2e-2026-10-16"]}],"timings_ms":{"embed":162,"route":0,"search":169,"rerank":367,"total":792,"generate":1701}}}}"#;

    #[test]
    fn faq_reply_is_the_cited_answer_from_the_live_faq_ask_contract() {
        let r: Resolution = serde_json::from_str(LIVE_FAQ_RESOLVED).unwrap();
        assert_eq!(
            compose_reply(&r),
            "The email address of Leanne Graham is Sincere@april.biz [1]."
        );
    }

    #[test]
    fn faq_reply_escalates_when_the_answer_cites_no_source() {
        let mut r: Resolution = serde_json::from_str(LIVE_FAQ_RESOLVED).unwrap();
        r.facts["faq"] = serde_json::json!({
            "answer": "I do not know; the sources do not say.",
            "citations": [], "sources": [], "timings_ms": {}
        });
        assert_eq!(
            compose_reply(&r),
            FAQ_FALLBACK,
            "an uncited answer is not grounded"
        );
        r.facts["faq"] = serde_json::json!({"answer": "   ", "citations": ["x"]});
        assert_eq!(
            compose_reply(&r),
            FAQ_FALLBACK,
            "a blank answer is no answer"
        );
        r.facts.remove("faq");
        assert_eq!(compose_reply(&r), FAQ_FALLBACK);
    }

    #[tokio::test]
    async fn rules_infer_order_from_start_data_and_skip_old_orders() {
        let start = conversation_start_data();
        assert_eq!(
            start.orders.len(),
            2,
            "the June drill order is outside conversation-start-data"
        );
        let q = RuleQualifier;
        let cat = SpaceCatalog::bootstrap();
        let ex = q
            .extract("I think I was charged twice", None, &start, &cat)
            .await
            .unwrap();
        assert_eq!(ex.request_type.as_deref(), Some("payment_error"));
        assert_eq!(ex.slots.get("order_id"), Some("ORD-58790"));
        let ex = q
            .extract("My drill stopped working", None, &start, &cat)
            .await
            .unwrap();
        assert_eq!(ex.slots.get("order_id"), None);
        let ex = q
            .extract("I want to return my headphones", None, &start, &cat)
            .await
            .unwrap();
        assert_eq!(
            (
                ex.request_type.as_deref(),
                ex.slots.get("order_id"),
                ex.slots.get("reason")
            ),
            (Some("return"), Some("ORD-58213"), None)
        );
    }

    fn with_gift_cards() -> SpaceCatalog {
        let mut all = SpaceCatalog::bootstrap().spaces().to_vec();
        all.push(ContextSpace {
            key: "retail:vouchers:gift-cards".into(),
            area: "vouchers".into(),
            request_type: "gift_card".into(),
            required: vec!["card_number".into()],
            procedure: "return".into(),
        });
        SpaceCatalog::new(all)
    }

    #[tokio::test]
    async fn rules_recognise_a_request_type_the_job_added_and_fill_its_fields_per_question() {
        let start = conversation_start_data();
        let cat = with_gift_cards();
        let q = RuleQualifier;
        // "card" alone is a payment keyword; naming the job's request type wins
        let ex = q
            .extract("My gift card does not work", None, &start, &cat)
            .await
            .unwrap();
        assert_eq!(ex.request_type.as_deref(), Some("gift_card"));
        let draft = Draft {
            request_type: RequestType::new("gift_card").unwrap(),
            slots: Slots::default(),
            utterance: "My gift card does not work".into(),
            awaiting: Some("card_number".into()),
        };
        let ex = q
            .extract("GC-1234-5678", Some(&draft), &start, &cat)
            .await
            .unwrap();
        assert_eq!(ex.request_type, None);
        assert_eq!(ex.slots.get("card_number"), Some("GC-1234-5678"));
        // without that space the same message is a payment error, as today
        let ex = q
            .extract(
                "My gift card does not work",
                None,
                &start,
                &SpaceCatalog::bootstrap(),
            )
            .await
            .unwrap();
        assert_eq!(ex.request_type.as_deref(), Some("payment_error"));
    }

    #[test]
    fn the_prompt_lists_the_jobs_request_types_with_their_fields() {
        let p = request_types_prompt(&with_gift_cards());
        let items = p.as_array().unwrap();
        assert_eq!(items.len(), 6);
        assert!(items.contains(&serde_json::json!({
            "request_type": "gift_card", "fields": {"card_number": "card_number"}
        })));
        assert!(items.contains(&serde_json::json!({
            "request_type": "return",
            "fields": {"order_id": "Order number like ORD-58213.", "reason": "Why the customer returns the item."}
        })));
    }

    #[test]
    fn extraction_reads_any_field_and_drops_blanks() {
        let ex: Extraction = serde_json::from_value(serde_json::json!({
            "request_type": "gift_card", "card_number": "GC-1", "order_id": "", "reason": null
        }))
        .unwrap();
        assert_eq!(ex.request_type.as_deref(), Some("gift_card"));
        assert_eq!(ex.slots, Slots::of(&[("card_number", "GC-1")]));
    }

    #[test]
    fn a_request_type_without_a_template_gets_a_plain_confirmation() {
        let r = Resolution {
            case_id: "CASE-1".into(),
            request_type: RequestType::new("gift_card").unwrap(),
            outcome: Outcome::Completed,
            reason: None,
            facts: serde_json::Map::new(),
        };
        assert_eq!(
            compose_reply(&r),
            "Your gift card request is done. A confirmation is on its way by email."
        );
    }
}
