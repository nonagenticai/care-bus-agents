//! The seam between workers' tool calls and the MCP server.
//! `tool.method` (worker side) maps to an MCP tool name (`{prefix}_{method}`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::domain::{Plan, TOOLS};
use crate::tools::{FakeTools, ToolClient, ToolError};

/// Tools that change backend state; they receive `idempotency_key`.
pub const SIDE_EFFECT_TOOLS: [&str; 3] = [
    "order_create_rma",
    "order_create_warranty_claim",
    "order_issue_refund",
];

/// Arguments of `faq_ask` that name stores. Only the worker sets one (`leaf_ids`, from carebus-api's faq scope);
/// whatever a plan step put there is dropped first.
pub const FAQ_STORE_ARGS: [&str; 3] = ["leaf_ids", "root_id", "root_ids"];
/// A `faq_ask` sent with `leaf_ids` whose answer does not confirm them (`scope.leaf_ids`): an MCP server that does
/// not know the filter searched every store, walled ones included, so the answer is dropped.
pub const FAQ_UNSCOPED: &str =
    "faq_unscoped: the MCP server did not confirm the store filter; the answer was dropped";

/// `Some(error)` when a call sent with `leaf_ids` got a result that does not echo them as `scope.leaf_ids`.
pub fn unconfirmed_scope(arguments: &Value, result: &Value) -> Option<&'static str> {
    let sent = arguments.get("leaf_ids")?;
    (result.pointer("/scope/leaf_ids") != Some(sent)).then_some(FAQ_UNSCOPED)
}

pub trait McpClient: Send + Sync + 'static {
    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> impl Future<Output = Result<Value, ToolError>> + Send;
}

fn prefix(tool: &str) -> &str {
    if tool == "business-rules" {
        "rules"
    } else {
        tool
    }
}

pub fn mcp_tool_name(tool: &str, method: &str) -> String {
    format!("{}_{method}", prefix(tool))
}

pub fn split_mcp_name(name: &str) -> Option<(&'static str, String)> {
    TOOLS.into_iter().find_map(|t| {
        name.strip_prefix(prefix(t))
            .and_then(|rest| rest.strip_prefix('_'))
            .map(|method| (t, method.to_string()))
    })
}

/// Whether a call of `tool.method` changes backend state: the step's own `side_effect` flag when the procedure set
/// one, else membership in `SIDE_EFFECT_TOOLS`.
pub fn is_side_effect(tool: &str, method: &str, side_effect: Option<bool>) -> bool {
    side_effect.unwrap_or_else(|| SIDE_EFFECT_TOOLS.contains(&mcp_tool_name(tool, method).as_str()))
}

/// Whether phase `phase` of `plan` has a step whose MCP tool is `mcp_tool` (a step row's flag has a plan step to
/// apply to).
pub fn plan_has_tool(plan: &Plan, phase: usize, mcp_tool: &str) -> bool {
    plan.phases.get(phase).is_some_and(|ph| {
        ph.steps
            .iter()
            .any(|st| mcp_tool_name(&st.worker, &st.action) == mcp_tool)
    })
}

pub fn mcp_arguments(
    tool: &str,
    method: &str,
    params: &Value,
    idempotency_key: &str,
    side_effect: Option<bool>,
) -> Value {
    let mut args = match params {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    if is_side_effect(tool, method, side_effect) {
        args.insert(
            "idempotency_key".into(),
            Value::String(idempotency_key.into()),
        );
    }
    Value::Object(args)
}

/// In-process MCP over `FakeTools`, for tests and local runs without a server.
#[derive(Clone)]
pub struct FakeMcp {
    pub tools: FakeTools,
    delays: Arc<Mutex<HashMap<String, Duration>>>,
    slow: Arc<Mutex<HashMap<String, Duration>>>,
    failing: Arc<Mutex<HashMap<String, String>>>,
    dropped: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl FakeMcp {
    pub fn new(tools: FakeTools) -> Self {
        Self {
            tools,
            delays: Arc::default(),
            slow: Arc::default(),
            failing: Arc::default(),
            dropped: Arc::default(),
        }
    }

    /// Every call to `mcp_tool` sleeps `d` first (an MCP server that stopped answering).
    pub fn delay_all(&self, mcp_tool: &str, d: Duration) {
        self.slow.lock().unwrap().insert(mcp_tool.into(), d);
    }

    /// Every call to `mcp_tool` loses argument `arg` before the tool sees it (a server that does not know it).
    pub fn drop_arg(&self, mcp_tool: &str, arg: &str) {
        self.dropped
            .lock()
            .unwrap()
            .entry(mcp_tool.into())
            .or_default()
            .push(arg.into());
    }

    /// Every call to `mcp_tool` fails permanently with `message`.
    pub fn fail_all(&self, mcp_tool: &str, message: &str) {
        self.failing
            .lock()
            .unwrap()
            .insert(mcp_tool.into(), message.into());
    }

    /// The next call to `mcp_tool` sleeps `d` first (to trip the executor's timeout).
    pub fn delay_next(&self, mcp_tool: &str, d: Duration) {
        self.delays.lock().unwrap().insert(mcp_tool.into(), d);
    }
}

impl McpClient for FakeMcp {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, ToolError> {
        let delay = self.delays.lock().unwrap().remove(name);
        let delay = delay.or_else(|| self.slow.lock().unwrap().get(name).copied());
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let failure = self.failing.lock().unwrap().get(name).cloned();
        if let Some(m) = failure {
            return Err(ToolError::fatal(m));
        }
        let (tool, method) = split_mcp_name(name)
            .ok_or_else(|| ToolError::fatal(format!("unknown MCP tool {name}")))?;
        let mut arguments = arguments;
        let gone = self.dropped.lock().unwrap().get(name).cloned();
        if let (Some(gone), Value::Object(m)) = (gone, &mut arguments) {
            for a in gone {
                m.remove(&a);
            }
        }
        let key = arguments
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.tools.call(tool, &method, &arguments, &key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{FakeTools, requirements};
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn names_follow_the_mcp_contract_and_round_trip() {
        assert_eq!(
            mcp_tool_name("business-rules", "check_return_window"),
            "rules_check_return_window"
        );
        assert_eq!(mcp_tool_name("faq", "ask"), "faq_ask");
        for (tool, method) in [
            ("crm", "verify_customer"),
            ("order", "get_order"),
            ("order", "get_payments"),
            ("order", "create_rma"),
            ("order", "create_warranty_claim"),
            ("order", "issue_refund"),
            ("delivery", "track_shipment"),
            ("business-rules", "check_return_window"),
            ("business-rules", "check_warranty"),
            ("business-rules", "check_payment_issue"),
            ("procedures", "get_procedure"),
            ("faq", "ask"),
        ] {
            assert!(
                requirements(tool, method).is_some(),
                "{tool}.{method} is a known action"
            );
            let name = mcp_tool_name(tool, method);
            assert_eq!(
                split_mcp_name(&name),
                Some((tool, method.to_string())),
                "{name}"
            );
        }
        assert_eq!(split_mcp_name("unknown_x"), None);
    }

    #[test]
    fn only_side_effect_tools_get_the_idempotency_key() {
        let p = json!({"order_id": "ORD-1", "reason": "x"});
        assert_eq!(
            mcp_arguments("order", "create_rma", &p, "K", None)["idempotency_key"],
            "K"
        );
        assert!(
            mcp_arguments("order", "get_order", &p, "K", None)
                .get("idempotency_key")
                .is_none()
        );
    }

    #[test]
    fn a_step_flag_overrides_the_side_effect_list() {
        let p = json!({"order_id": "ORD-1"});
        // Some(true) on a tool the list does not name: side-effecting.
        assert_eq!(
            mcp_arguments("order", "get_order", &p, "K", Some(true))["idempotency_key"],
            "K"
        );
        // Some(false) on a listed tool: not side-effecting.
        assert!(
            mcp_arguments("order", "create_rma", &p, "K", Some(false))
                .get("idempotency_key")
                .is_none()
        );
        // None: the list decides, both ways.
        assert!(is_side_effect("order", "issue_refund", None));
        assert!(!is_side_effect("delivery", "track_shipment", None));
        // the caller's params are kept either way
        assert_eq!(
            mcp_arguments("order", "get_order", &p, "K", Some(true))["order_id"],
            "ORD-1"
        );
    }

    #[test]
    fn plan_has_tool_matches_phase_and_mcp_name() {
        let plan = crate::domain::static_plan("return").expect("bootstrap return plan");
        let (phase, name) = plan
            .phases
            .iter()
            .enumerate()
            .find_map(|(i, ph)| {
                ph.steps
                    .first()
                    .map(|st| (i, mcp_tool_name(&st.worker, &st.action)))
            })
            .unwrap();
        assert!(plan_has_tool(&plan, phase, &name));
        assert!(!plan_has_tool(&plan, phase, "order_nope"));
        assert!(!plan_has_tool(&plan, plan.phases.len(), &name));
    }

    #[test]
    fn work_request_side_effect_is_optional_on_the_wire() {
        let w = json!({
            "work_id": "C:0:order:1", "idempotency_key": "C:0:order", "case_id": "C", "phase": 0,
            "as": "order", "worker": "order", "action": "get_order", "args": {}, "attempt": 1,
            "request_type": "return"
        });
        let old: crate::domain::WorkRequest = serde_json::from_value(w.clone()).unwrap();
        assert_eq!(old.side_effect, None);
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("side_effect")
                .is_none()
        );
        let mut flagged = w;
        flagged["side_effect"] = json!(true);
        let new: crate::domain::WorkRequest = serde_json::from_value(flagged).unwrap();
        assert_eq!(new.side_effect, Some(true));
        assert_eq!(serde_json::to_value(&new).unwrap()["side_effect"], true);
    }

    #[tokio::test]
    async fn fake_mcp_serves_fake_tools_and_keeps_side_effects_idempotent() {
        let mcp = FakeMcp::new(FakeTools::default());
        let args = mcp_arguments(
            "order",
            "create_rma",
            &json!({"order_id": "ORD-58213", "reason": "x"}),
            "CASE-1:2:rma",
            None,
        );
        let a = mcp
            .call_tool("order_create_rma", args.clone())
            .await
            .unwrap();
        let b = mcp.call_tool("order_create_rma", args).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(mcp.tools.calls.lock().unwrap().len(), 2);
        let err = mcp.call_tool("nope_x", json!({})).await.unwrap_err();
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn delay_next_slows_one_call() {
        let mcp = FakeMcp::new(FakeTools::default());
        mcp.delay_next("order_get_order", Duration::from_millis(300));
        let t = Instant::now();
        mcp.call_tool("order_get_order", json!({"order_id": "ORD-58790"}))
            .await
            .unwrap();
        assert!(t.elapsed() >= Duration::from_millis(300));
        let t = Instant::now();
        mcp.call_tool("order_get_order", json!({"order_id": "ORD-58790"}))
            .await
            .unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(300),
            "only the next call is slowed"
        );
    }
}
