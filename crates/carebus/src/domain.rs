//! Message payloads and the procedure-source plans.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::spaces::ContextSpace;

pub const REQUEST_QUALIFIED: &str = "request.qualified";
pub const REQUEST_RESOLVED: &str = "request.resolved";
pub const WORK_REQUESTED: &str = "work.requested";
pub const WORK_SUCCEEDED: &str = "work.succeeded";
pub const WORK_FAILED: &str = "work.failed";
pub const TOOL_CALL: &str = "tool.call";
pub const TOOL_RESPONSE: &str = "tool.response";

/// The bootstrap job's departments (`spaces::SpaceCatalog::bootstrap`). Each area gets its own streams, so a backlog
/// in one cannot delay another. Agents consume the areas of their scope's context spaces (`spaces::run_with_catalog`).
pub const AREAS: [&str; 4] = ["order-support", "logistics", "payments", "faq"];
/// Worker kinds. One worker group and one `bus:tool:{tool}` stream per entry.
pub const TOOLS: [&str; 6] = [
    "crm",
    "order",
    "delivery",
    "faq",
    "business-rules",
    "procedures",
];
/// Every event a consumer can match on, independent of the context-space prefix.
pub const EVENTS: [&str; 7] = [
    REQUEST_QUALIFIED,
    REQUEST_RESOLVED,
    WORK_REQUESTED,
    WORK_SUCCEEDED,
    WORK_FAILED,
    TOOL_CALL,
    TOOL_RESPONSE,
];

/// `order-support.returns.request.qualified` and bare `request.qualified` both give `request.qualified`.
pub fn event_of(topic: &str) -> Option<&'static str> {
    EVENTS
        .into_iter()
        .find(|e| topic == *e || topic.strip_suffix(e).is_some_and(|p| p.ends_with('.')))
}

/// `order-support.returns.tool.call` with `tool.response` gives `order-support.returns.tool.response`: the same
/// context-space prefix, another event. A topic with no known event gives the bare `event`.
pub fn with_event(topic: &str, event: &str) -> String {
    match event_of(topic).and_then(|e| topic.strip_suffix(e)) {
        Some(prefix) => format!("{prefix}{event}"),
        None => event.to_string(),
    }
}

/// A request type: the `request_type` of one of the job's context spaces (`spaces::SpaceCatalog`), e.g. `return`.
/// Configuration, not code: a release can add one. Serialized as the bare string; a value that is not
/// `^[a-z][a-z0-9_]{0,62}$` does not deserialize, so a malformed payload is dead-lettered, never routed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct RequestType(String);

impl RequestType {
    pub fn new(s: impl Into<String>) -> anyhow::Result<Self> {
        let s = s.into();
        anyhow::ensure!(
            Self::valid(&s),
            "request type {s:?} is not ^[a-z][a-z0-9_]{{0,62}}$"
        );
        Ok(Self(s))
    }

    /// `^[a-z][a-z0-9_]{0,62}$`, as the release bundle validates it.
    pub fn valid(s: &str) -> bool {
        let mut chars = s.chars();
        s.len() <= 63
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bootstrap job's request types (`SpaceCatalog::bootstrap`).
    pub fn r#return() -> Self {
        Self("return".into())
    }
    pub fn warranty_claim() -> Self {
        Self("warranty_claim".into())
    }
    pub fn delivery_status() -> Self {
        Self("delivery_status".into())
    }
    pub fn payment_error() -> Self {
        Self("payment_error".into())
    }
    pub fn faq() -> Self {
        Self("faq".into())
    }
}

impl<'de> Deserialize<'de> for RequestType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RequestType {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for RequestType {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for RequestType {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// The fields a request carries, by name (the `required` of its context space, e.g. `order_id`, `reason`). A blank
/// value is no value: it is dropped when read and never stored. Serialized as a flat object of strings with the
/// bootstrap job's five fields always present (`null` when empty), so today's payloads keep their exact shape;
/// `null` members are accepted and dropped when read.
#[derive(Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(transparent)]
pub struct Slots(BTreeMap<String, String>);

/// The fields every payload carried before field names came from the context spaces.
const LEGACY_SLOTS: [&str; 5] = ["order_id", "reason", "defect", "error_type", "question"];

impl Serialize for Slots {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let extra: Vec<(&String, &String)> = self
            .0
            .iter()
            .filter(|(k, _)| !LEGACY_SLOTS.contains(&k.as_str()))
            .collect();
        let mut m = s.serialize_map(Some(LEGACY_SLOTS.len() + extra.len()))?;
        for k in LEGACY_SLOTS {
            m.serialize_entry(k, &self.0.get(k))?;
        }
        for (k, v) in extra {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl<'de> Deserialize<'de> for Slots {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = BTreeMap::<String, Value>::deserialize(d)?;
        let mut out = Slots::default();
        for (k, v) in raw {
            match v {
                Value::String(s) => out.set(k, s),
                Value::Number(n) => out.set(k, n.to_string()),
                Value::Bool(b) => out.set(k, b.to_string()),
                _ => {}
            }
        }
        Ok(out)
    }
}

impl Slots {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// Set `name`; a blank value removes it.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let (name, value) = (name.into(), value.into());
        if value.trim().is_empty() {
            self.0.remove(&name);
        } else {
            self.0.insert(name, value);
        }
    }

    /// `set` for each pair; `Slots::of(&[("order_id", "ORD-1")])`.
    pub fn of(pairs: &[(&str, &str)]) -> Self {
        let mut s = Self::default();
        for (k, v) in pairs {
            s.set(*k, *v);
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The `required` fields that have no value, in `required` order.
    pub fn missing<'a>(&self, required: &'a [String]) -> Vec<&'a str> {
        required
            .iter()
            .map(String::as_str)
            .filter(|k| self.get(k).is_none())
            .collect()
    }

    /// Fill only the fields that are still empty.
    pub fn merge(&mut self, other: Slots) {
        for (k, v) in other.0 {
            if !self.0.contains_key(&k) {
                self.set(k, v);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualifiedRequest {
    pub case_id: String,
    pub customer_id: String,
    pub request_type: RequestType,
    /// Department, e.g. `order-support`.
    pub area: String,
    /// Ontology key, e.g. `retail:order-support:returns`.
    pub context_space: String,
    pub slots: Slots,
    pub utterance: String,
    /// Request-gate answers of an `annotate` verdict (spec 2026-10-18 §3); absent when no gate annotated it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gates: Option<Value>,
}

impl QualifiedRequest {
    /// Topic for `event` in the request's context space, e.g. `order-support.returns.work.requested`: the area and
    /// the key's last segment, as `ContextSpace::topic`. Routing needs no catalog lookup: the interaction agent
    /// qualified the request against the scope's context spaces and stamped both.
    pub fn topic(&self, event: &str) -> String {
        let space = self.context_space.rsplit(':').next().unwrap_or_default();
        format!("{}.{space}.{event}", self.area)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub worker: String,
    pub action: String,
    /// Name the result is stored under; later steps reference it as `$name.field`.
    #[serde(rename = "as")]
    pub as_name: String,
    pub args: Map<String, Value>,
    /// Whether the step's tool call changes backend state (it then gets `idempotency_key`). From the procedure's
    /// step row (release bundle / `POST /api/procedures`); `None`: `mcp::SIDE_EFFECT_TOOLS` decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Phase {
    pub steps: Vec<Step>,
    /// Result whose `eligible: false` stops the case as not eligible.
    pub gate: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub phases: Vec<Phase>,
    /// `name@version` of the procedure version this plan came from, set by the source that chose it
    /// (`HttpProcedures`: the PUBLISHED version). `None` for built-in plans. Stored with the plan in the
    /// case hash, so every replica stamps the same version on the case's envelopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkRequest {
    /// `{case}:{phase}:{as}:{attempt}`. Deterministic, so a re-dispatch is a duplicate the worker can drop.
    pub work_id: String,
    /// `{case}:{phase}:{as}`. Same across retries, so a side-effecting tool runs once.
    pub idempotency_key: String,
    pub case_id: String,
    pub phase: usize,
    #[serde(rename = "as")]
    pub as_name: String,
    pub worker: String,
    pub action: String,
    pub args: Value,
    pub attempt: u32,
    /// Routes the work and its outcome to the case's department streams and topics.
    pub request_type: RequestType,
    /// The plan step's `side_effect` (`Step::side_effect`); `None`: `mcp::SIDE_EFFECT_TOOLS` decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkOutcome {
    pub work_id: String,
    pub case_id: String,
    pub phase: usize,
    #[serde(rename = "as")]
    pub as_name: String,
    pub worker: String,
    pub action: String,
    pub attempt: u32,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub retryable: bool,
    /// Routes the work and its outcome to the case's department streams and topics.
    pub request_type: RequestType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    NotEligible,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resolution {
    pub case_id: String,
    pub request_type: RequestType,
    pub outcome: Outcome,
    pub reason: Option<String>,
    pub facts: Map<String, Value>,
}

/// Payload of `tool.call` on `bus:tool:{tool}`. `call_id` is the work id, so a
/// re-dispatched attempt is a different call while a redelivery is the same one.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub mcp_tool: String,
    pub arguments: Value,
    pub timeout_ms: u64,
    pub work: WorkRequest,
}

/// Payload of `tool.response` on `bus:tool:{tool}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResponse {
    pub call_id: String,
    /// The MCP tool that ran (`ToolCall::mcp_tool`); the audit trail's resource.
    pub mcp_tool: String,
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub retryable: bool,
    pub latency_ms: u64,
}

fn step(worker: &str, action: &str, as_name: &str, args: Value) -> Step {
    let Value::Object(args) = args else {
        unreachable!("plan args are object literals")
    };
    Step {
        worker: worker.into(),
        action: action.into(),
        as_name: as_name.into(),
        args,
        side_effect: None,
    }
}

fn phase(steps: Vec<Step>, gate: Option<&str>) -> Phase {
    Phase {
        steps,
        gate: gate.map(Into::into),
    }
}

/// Where the orchestrator gets the plan for a context space: its PUBLISHED procedure.
pub trait ProcedureSource: Send + Sync + 'static {
    fn plan(
        &self,
        space: &ContextSpace,
    ) -> impl std::future::Future<Output = anyhow::Result<Plan>> + Send;
}

/// The built-in plans of the bootstrap job's procedures, by the space's `procedure` (tests, local runs without
/// carebus-api). Any space naming one of them runs it, whatever its request type.
pub struct StaticProcedures;

impl ProcedureSource for StaticProcedures {
    async fn plan(&self, space: &ContextSpace) -> anyhow::Result<Plan> {
        static_plan(&space.procedure).ok_or_else(|| {
            anyhow::anyhow!(
                "no built-in procedure {:?} for {}",
                space.procedure,
                space.key
            )
        })
    }
}

/// The bootstrap job's procedures (`0002_seed.sql`), by name: `return`, `warranty_claim`, `delivery_status`,
/// `duplicate_charge_refund`, `faq_answer`. `None` for any other name.
pub fn static_plan(procedure: &str) -> Option<Plan> {
    let verify = || {
        step(
            "crm",
            "verify_customer",
            "customer",
            json!({"customer_id": "$request.customer_id"}),
        )
    };
    let get_order = || {
        step(
            "order",
            "get_order",
            "order",
            json!({"order_id": "$request.order_id"}),
        )
    };
    let phases = match procedure {
        "return" => vec![
            phase(vec![verify(), get_order()], None),
            phase(
                vec![
                    step(
                        "business-rules",
                        "check_return_window",
                        "rules",
                        json!({"order_id": "$order.order_id", "category": "$order.category", "delivered_on": "$order.delivered_on"}),
                    ),
                    step(
                        "procedures",
                        "get_procedure",
                        "procedure",
                        json!({"name": "return"}),
                    ),
                ],
                Some("rules"),
            ),
            phase(
                vec![step(
                    "order",
                    "create_rma",
                    "rma",
                    json!({"order_id": "$request.order_id", "reason": "$request.reason"}),
                )],
                None,
            ),
        ],
        "warranty_claim" => vec![
            phase(vec![verify(), get_order()], None),
            phase(
                vec![
                    step(
                        "business-rules",
                        "check_warranty",
                        "rules",
                        json!({"order_id": "$order.order_id", "category": "$order.category", "delivered_on": "$order.delivered_on"}),
                    ),
                    step(
                        "procedures",
                        "get_procedure",
                        "procedure",
                        json!({"name": "warranty_claim"}),
                    ),
                ],
                Some("rules"),
            ),
            phase(
                vec![step(
                    "order",
                    "create_warranty_claim",
                    "claim",
                    json!({"order_id": "$request.order_id", "defect": "$request.defect"}),
                )],
                None,
            ),
        ],
        "delivery_status" => vec![
            phase(vec![get_order()], None),
            phase(
                vec![step(
                    "delivery",
                    "track_shipment",
                    "shipment",
                    json!({"shipment_id": "$order.shipment_id"}),
                )],
                None,
            ),
        ],
        "duplicate_charge_refund" => vec![
            phase(
                vec![
                    verify(),
                    step(
                        "order",
                        "get_payments",
                        "payments",
                        json!({"order_id": "$request.order_id"}),
                    ),
                ],
                None,
            ),
            phase(
                vec![
                    step(
                        "business-rules",
                        "check_payment_issue",
                        "rules",
                        json!({"order_id": "$request.order_id", "error_type": "$request.error_type"}),
                    ),
                    step(
                        "procedures",
                        "get_procedure",
                        "procedure",
                        json!({"name": "duplicate_charge_refund"}),
                    ),
                ],
                Some("rules"),
            ),
            phase(
                vec![step(
                    "order",
                    "issue_refund",
                    "refund",
                    json!({"payment_id": "$rules.refund_payment_id", "amount": "$rules.refund_amount"}),
                )],
                None,
            ),
        ],
        "faq_answer" => vec![phase(
            vec![step(
                "faq",
                "ask",
                "faq",
                json!({"question": "$request.question"}),
            )],
            None,
        )],
        _ => return None,
    };
    Some(Plan {
        phases,
        procedure: None,
    })
}

/// Replace `"$name.path"` strings with values from the case context.
pub fn resolve_args(args: &Map<String, Value>, ctx: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (k, v) in args {
        let resolved = match v.as_str().and_then(|s| s.strip_prefix('$')) {
            Some(path) => lookup(ctx, path),
            None => v.clone(),
        };
        out.insert(k.clone(), resolved);
    }
    Value::Object(out)
}

fn lookup(ctx: &Map<String, Value>, path: &str) -> Value {
    let mut parts = path.split('.');
    let Some(first) = parts.next() else {
        return Value::Null;
    };
    let mut cur = match ctx.get(first) {
        Some(v) => v,
        None => return Value::Null,
    };
    for p in parts {
        cur = match cur.get(p) {
            Some(v) => v,
            None => return Value::Null,
        };
    }
    cur.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nested_paths_and_leaves_literals() {
        let mut ctx = Map::new();
        ctx.insert(
            "order".into(),
            json!({"shipment_id": "SHP-1", "nested": {"x": 3}}),
        );
        let Value::Object(args) = json!({"a": "$order.shipment_id", "b": "$order.nested.x", "c": "literal", "d": "$missing.y"})
        else {
            unreachable!()
        };
        let out = resolve_args(&args, &ctx);
        assert_eq!(
            out,
            json!({"a": "SHP-1", "b": 3, "c": "literal", "d": null})
        );
    }

    #[test]
    fn missing_lists_only_empty_required_slots() {
        let s = Slots::of(&[("order_id", "ORD-1"), ("reason", "  ")]);
        let req = |r: &[&str]| r.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(s.missing(&req(&["order_id", "reason"])), vec!["reason"]);
        assert!(s.missing(&req(&["order_id"])).is_empty());
    }

    #[test]
    fn slots_drop_nulls_and_blanks_and_merge_only_empty_fields() {
        let mut s: Slots = serde_json::from_value(
            json!({"order_id": "ORD-1", "reason": null, "defect": " ", "n": 3}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({"order_id": "ORD-1", "reason": null, "defect": null, "error_type": null, "question": null, "n": "3"})
        );
        s.merge(Slots::of(&[("order_id", "ORD-2"), ("reason", "too big")]));
        assert_eq!(s.get("order_id"), Some("ORD-1"));
        assert_eq!(s.get("reason"), Some("too big"));
    }

    #[test]
    fn request_types_are_validated_strings_that_serialize_bare() {
        for (rt, name) in [
            (RequestType::r#return(), "return"),
            (RequestType::warranty_claim(), "warranty_claim"),
            (RequestType::delivery_status(), "delivery_status"),
            (RequestType::payment_error(), "payment_error"),
            (RequestType::faq(), "faq"),
        ] {
            assert_eq!(serde_json::to_value(&rt).unwrap(), json!(name));
            assert_eq!(
                serde_json::from_value::<RequestType>(json!(name)).unwrap(),
                rt
            );
        }
        assert_eq!(RequestType::new("gift_card_2").unwrap(), "gift_card_2");
        let long = "x".repeat(64);
        for bad in ["", "Return", "2fa", "gift-card", "a b", long.as_str()] {
            assert!(RequestType::new(bad).is_err(), "{bad:?}");
            assert!(serde_json::from_value::<RequestType>(json!(bad)).is_err());
        }
        assert!(RequestType::new("x".repeat(63)).is_ok());
    }

    #[test]
    fn qualified_request_topics_keep_the_context_space_prefix() {
        let req = QualifiedRequest {
            case_id: "C".into(),
            customer_id: "CUST".into(),
            request_type: RequestType::r#return(),
            area: "order-support".into(),
            context_space: "retail:order-support:returns".into(),
            slots: Slots::default(),
            utterance: String::new(),
            gates: None,
        };
        assert_eq!(
            req.topic(REQUEST_QUALIFIED),
            "order-support.returns.request.qualified"
        );
        assert_eq!(
            with_event("order-support.returns.tool.call", TOOL_RESPONSE),
            "order-support.returns.tool.response"
        );
        assert_eq!(with_event("tool.call", TOOL_RESPONSE), TOOL_RESPONSE);
        assert_eq!(with_event("dead.letter", TOOL_RESPONSE), TOOL_RESPONSE);
    }

    #[test]
    fn every_bootstrap_procedure_has_a_built_in_plan() {
        for p in [
            "return",
            "warranty_claim",
            "delivery_status",
            "duplicate_charge_refund",
            "faq_answer",
        ] {
            assert!(
                static_plan(p).is_some_and(|plan| !plan.phases.is_empty()),
                "{p}"
            );
        }
        assert!(static_plan("gift_card").is_none());
    }

    #[test]
    fn event_of_accepts_prefixed_and_bare_topics() {
        assert_eq!(
            event_of("order-support.returns.request.qualified"),
            Some(REQUEST_QUALIFIED)
        );
        assert_eq!(event_of("request.qualified"), Some(REQUEST_QUALIFIED));
        assert_eq!(
            event_of("logistics.delivery-status.tool.response"),
            Some(TOOL_RESPONSE)
        );
        assert_eq!(
            event_of("payments.payment-errors.work.failed"),
            Some(WORK_FAILED)
        );
        assert_eq!(
            event_of("order-support.returns.xtool.call"),
            None,
            "suffix must start at a dot"
        );
        assert_eq!(event_of("dead.letter"), None);
    }
}
