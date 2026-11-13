//! In-memory fault injection for demos. Process-local by design: one demo-backends replica.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{DemoError, Result};

/// The MCP tools demo-backends serves (overview list minus faq_ask and procedures_get_procedure).
pub const TOOL_NAMES: [&str; 11] = [
    "crm_verify_customer",
    "crm_get_start_data",
    "order_get_order",
    "order_get_payments",
    "order_create_rma",
    "order_create_warranty_claim",
    "order_issue_refund",
    "delivery_track_shipment",
    "rules_check_return_window",
    "rules_check_warranty",
    "rules_check_payment_issue",
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaultMode {
    None,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct Fault {
    mode: FaultMode,
    remaining: u32,
}

#[derive(Clone, Default)]
pub struct FaultState(Arc<Mutex<BTreeMap<String, Fault>>>);

impl FaultState {
    pub fn set(&self, tool: &str, mode: FaultMode, count: u32) -> Result<()> {
        if !TOOL_NAMES.contains(&tool) {
            return Err(DemoError::Invalid(format!(
                "unknown tool {tool}; valid: {}",
                TOOL_NAMES.join(", ")
            )));
        }
        let mut m = self.0.lock().unwrap();
        if mode == FaultMode::None || count == 0 {
            m.remove(tool);
        } else {
            m.insert(
                tool.to_string(),
                Fault {
                    mode,
                    remaining: count,
                },
            );
        }
        Ok(())
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    pub fn list(&self) -> Value {
        json!({"faults": *self.0.lock().unwrap()})
    }

    /// Uses up one count. The lock guard is dropped before any `.await`.
    pub async fn hit(&self, tool: &str, sleep: Duration) -> Result<()> {
        let mode = {
            let mut m = self.0.lock().unwrap();
            let Some(f) = m.get_mut(tool) else {
                return Ok(());
            };
            f.remaining -= 1;
            let mode = f.mode;
            if f.remaining == 0 {
                m.remove(tool);
            }
            mode
        };
        match mode {
            FaultMode::Timeout => {
                tokio::time::sleep(sleep).await;
                Ok(())
            }
            FaultMode::Error => Err(DemoError::FaultInjected(tool.to_string())),
            FaultMode::None => Ok(()),
        }
    }
}
