//! What a worker calls. `ToolClient` is the seam: `FakeTools` serves demo
//! data in-process; an MCP-backed client (rmcp / rig-rmcp) replaces it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub message: String,
    pub retryable: bool,
}

impl ToolError {
    pub fn fatal(m: impl Into<String>) -> Self {
        Self {
            message: m.into(),
            retryable: false,
        }
    }
}

pub trait ToolClient: Send + Sync + 'static {
    /// `idempotency_key` is stable across retries of one step. A tool with
    /// side effects must return the first result for a key it has seen.
    fn call(
        &self,
        tool: &str,
        method: &str,
        params: &Value,
        idempotency_key: &str,
    ) -> impl std::future::Future<Output = Result<Value, ToolError>> + Send;
}

/// Required params per `tool.method`. Stand-in for the worker's RAG lookup
/// of the tool-request-format.
pub fn requirements(tool: &str, method: &str) -> Option<&'static [&'static str]> {
    Some(match (tool, method) {
        ("crm", "verify_customer") => &["customer_id"],
        ("order", "get_order" | "get_payments") => &["order_id"],
        ("order", "create_rma") => &["order_id", "reason"],
        ("order", "create_warranty_claim") => &["order_id", "defect"],
        ("order", "issue_refund") => &["payment_id", "amount"],
        ("delivery", "track_shipment") => &["shipment_id"],
        ("business-rules", "check_return_window" | "check_warranty") => &["order_id", "category"],
        ("business-rules", "check_payment_issue") => &["order_id", "error_type"],
        ("procedures", "get_procedure") => &["name"],
        ("faq", "ask") => &["question"],
        _ => return None,
    })
}

pub const TODAY: &str = "2026-10-15";

pub fn customer() -> Value {
    json!({"customer_id": "C-10482", "name": "Anna de Vries", "tier": "Gold", "email": "a.devries@example.nl"})
}

pub fn orders() -> Vec<Value> {
    vec![
        json!({"order_id": "ORD-58790", "placed": "2026-10-11", "item": "Philips Airfryer XXL HD9285", "category": "kitchen", "amount": 229.0, "status": "in_transit", "shipment_id": "SHP-88123", "delivered_on": null}),
        json!({"order_id": "ORD-58213", "placed": "2026-10-02", "item": "Sony WH-1000XM6 headphones", "category": "audio", "amount": 379.0, "status": "delivered", "shipment_id": "SHP-87410", "delivered_on": "2026-10-05"}),
        json!({"order_id": "ORD-57102", "placed": "2026-06-14", "item": "Bosch PSR 18 LI-2 cordless drill", "category": "tools", "amount": 149.0, "status": "delivered", "shipment_id": "SHP-80022", "delivered_on": "2026-06-18"}),
    ]
}

pub fn payments() -> Vec<Value> {
    vec![
        json!({"payment_id": "PAY-3398", "order_id": "ORD-58790", "method": "Visa •• 4417", "amount": 229.0, "status": "captured", "at_epoch_s": 1_791_746_531}),
        json!({"payment_id": "PAY-3399", "order_id": "ORD-58790", "method": "Visa •• 4417", "amount": 229.0, "status": "captured", "at_epoch_s": 1_791_746_569}),
        json!({"payment_id": "PAY-3120", "order_id": "ORD-58213", "method": "iDEAL", "amount": 379.0, "status": "captured", "at_epoch_s": 1_791_000_940}),
    ]
}

fn days_since(date: &str) -> i64 {
    // Days from `date` to TODAY, both YYYY-MM-DD.
    fn ord(d: &str) -> i64 {
        let p: Vec<i64> = d.split('-').filter_map(|x| x.parse().ok()).collect();
        let (y, m, dd) = (p[0], p[1], p[2]);
        let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
        365 * y + y / 4 - y / 100 + y / 400 + (153 * (m - 3) + 2) / 5 + dd
    }
    ord(TODAY) - ord(date)
}

/// In-process demo tools. `fail_once` makes the next call to `tool.method`
/// time out, to exercise retries.
#[derive(Clone, Default)]
pub struct FakeTools {
    fail_once: Arc<Mutex<HashSet<String>>>,
    side_effects: Arc<Mutex<HashMap<String, Value>>>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

impl FakeTools {
    pub fn fail_next(&self, tool_method: &str) {
        self.fail_once.lock().unwrap().insert(tool_method.into());
    }

    fn once(&self, key: &str, make: impl FnOnce(usize) -> Value) -> Value {
        let mut m = self.side_effects.lock().unwrap();
        let n = m.len();
        m.entry(key.into()).or_insert_with(|| make(n)).clone()
    }

    fn exec(&self, tool: &str, method: &str, p: &Value, key: &str) -> Result<Value, ToolError> {
        let s = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let order = |id: &str| orders().into_iter().find(|o| o["order_id"] == id);
        Ok(match (tool, method) {
            ("crm", "verify_customer") => {
                if s("customer_id") != "C-10482" {
                    return Err(ToolError::fatal("customer not found"));
                }
                json!({"verified": true, "customer": customer()})
            }
            ("order", "get_order") => order(&s("order_id")).ok_or_else(|| ToolError::fatal(format!("order {} not found", s("order_id"))))?,
            ("order", "get_payments") => json!({"payments": payments().into_iter().filter(|x| x["order_id"] == s("order_id").as_str()).collect::<Vec<_>>()}),
            ("order", "create_rma") => self.once(key, |n| json!({"rma_id": format!("RMA-{}", 41000 + n), "order_id": s("order_id"), "carrier": "PostNL"})),
            ("order", "create_warranty_claim") => self.once(key, |n| json!({"claim_id": format!("WC-{}", 7700 + n), "order_id": s("order_id"), "next_step": "Courier pickup Friday 09:00-13:00."})),
            ("order", "issue_refund") => self.once(key, |n| json!({"refund_id": format!("REF-{}", 90200 + n), "payment_id": s("payment_id"), "amount": p["amount"], "eta": "3-5 business days"})),
            ("delivery", "track_shipment") => match s("shipment_id").as_str() {
                "SHP-88123" => json!({"carrier": "PostNL", "status": "in_transit", "last_event": "Sorted at Nieuwegein, 2026-10-14 06:12", "eta": "2026-10-16, 14:00-17:00"}),
                "SHP-87410" => json!({"carrier": "PostNL", "status": "delivered", "delivered_at": "2026-10-05 13:42", "signed_by": "A. de Vries"}),
                "SHP-80022" => json!({"carrier": "DHL", "status": "delivered", "delivered_at": "2026-06-18 11:05"}),
                other => return Err(ToolError::fatal(format!("unknown shipment {other}"))),
            },
            ("business-rules", "check_return_window") => match p.get("delivered_on").and_then(Value::as_str) {
                None => json!({"eligible": false, "rule": "RET-01", "reason": "Not delivered yet; refuse the parcel at the door for an automatic refund."}),
                Some(d) if days_since(d) > 30 => json!({"eligible": false, "rule": "RET-01", "days_since_delivery": days_since(d), "reason": "The 30-day return period has ended. Faulty items are still covered by the 2-year warranty."}),
                Some(d) => json!({"eligible": true, "rule": "RET-01", "days_since_delivery": days_since(d)}),
            },
            ("business-rules", "check_warranty") => match p.get("delivered_on").and_then(Value::as_str) {
                Some(d) if days_since(d) <= 730 => json!({"eligible": true, "rule": "WAR-01"}),
                _ => json!({"eligible": false, "rule": "WAR-01", "reason": "Not covered by the 2-year warranty."}),
            },
            ("business-rules", "check_payment_issue") => {
                let pays: Vec<Value> = payments().into_iter().filter(|x| x["order_id"] == s("order_id").as_str()).collect();
                let dup = pays.windows(2).find(|w| w[0]["amount"] == w[1]["amount"] && (w[1]["at_epoch_s"].as_i64().unwrap_or(0) - w[0]["at_epoch_s"].as_i64().unwrap_or(0)).abs() <= 300);
                match (s("error_type").as_str(), dup) {
                    ("duplicate_charge", Some(w)) => json!({"eligible": true, "rule": "PAY-04", "refund_payment_id": w[1]["payment_id"], "refund_amount": w[1]["amount"]}),
                    _ => json!({"eligible": false, "rule": "PAY-04", "reason": "No duplicate or declined payment found on this order."}),
                }
            }
            ("procedures", "get_procedure") => json!({"name": s("name"), "steps": ["verify", "act", "notify customer"]}),
            // Same shape as the real faq_ask (strata_rag `/api/ask` behind MCP):
            // `{answer, citations, sources, timings_ms}`; no grounding = no citations.
            ("faq", "ask") => {
                // The real faq_ask confirms a store filter it applied (`scope.leaf_ids`, executor::unconfirmed_scope).
                if let Some(ids) = p.get("leaf_ids") {
                    return Ok(json!({"answer": "From the stores you may search. [1]", "citations": ["kb-scoped"],
                                     "sources": [{"chunk_id": "kb-scoped", "document_id": "kb-scoped", "store_path": ["faq"]}],
                                     "timings_ms": {"total": 1}, "scope": {"leaf_ids": ids}}));
                }
                let q = s("question").to_lowercase();
                let hit = [
                    (["refund", "money back"], "Refunds reach your original payment method within 5 business days after we receive the return. [1]", "kb-refunds"),
                    (["return policy", "return period"], "Unused items can be returned within 30 days of delivery. [1]", "kb-returns"),
                    (["warranty", "guarantee"], "Every product has a 2-year warranty from delivery. [1]", "kb-warranty"),
                ]
                .into_iter()
                .find(|(kw, _, _)| kw.iter().any(|k| q.contains(k)));
                match hit {
                    Some((_, answer, chunk)) => json!({"answer": answer, "citations": [chunk], "sources": [{"chunk_id": chunk, "document_id": chunk, "store_path": ["faq"]}], "timings_ms": {"total": 1}}),
                    None => json!({"answer": "I do not know; the sources do not cover this.", "citations": [], "sources": [], "timings_ms": {"total": 1}}),
                }
            }
            _ => return Err(ToolError::fatal(format!("unknown tool {tool}.{method}"))),
        })
    }
}

impl ToolClient for FakeTools {
    async fn call(
        &self,
        tool: &str,
        method: &str,
        params: &Value,
        key: &str,
    ) -> Result<Value, ToolError> {
        let tm = format!("{tool}.{method}");
        self.calls.lock().unwrap().push(tm.clone());
        if self.fail_once.lock().unwrap().remove(&tm) {
            return Err(ToolError {
                message: "timeout after 3000 ms".into(),
                retryable: true,
            });
        }
        self.exec(tool, method, params, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_since_crosses_months() {
        assert_eq!(days_since("2026-10-05"), 10);
        assert_eq!(days_since("2026-06-18"), 119);
    }

    #[tokio::test]
    async fn side_effect_tools_are_idempotent_per_key() {
        let t = FakeTools::default();
        let p = json!({"order_id": "ORD-58213", "reason": "x"});
        let a = t
            .call("order", "create_rma", &p, "CASE-1:2:rma")
            .await
            .unwrap();
        let b = t
            .call("order", "create_rma", &p, "CASE-1:2:rma")
            .await
            .unwrap();
        let c = t
            .call("order", "create_rma", &p, "CASE-2:2:rma")
            .await
            .unwrap();
        assert_eq!(a, b);
        assert_ne!(a["rma_id"], c["rma_id"]);
    }
}
