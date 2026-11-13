//! Pure business rules. Reasons/alternatives are the customer-facing copy from the console design.
use chrono::{Duration, NaiveDate};
use serde_json::{Value, json};

use crate::money::euros;

pub fn return_window(delivered_on: Option<NaiveDate>, today: NaiveDate) -> Value {
    let Some(d) = delivered_on else {
        return json!({"eligible": false, "rule": "RET-01", "reason": "Your order hasn't been delivered yet, so a return can't be registered.",
            "alternative": "If you no longer want it, you can refuse the parcel at the door. It comes back to us and you're refunded automatically."});
    };
    let days = (today - d).num_days();
    let deadline = d + Duration::days(30);
    if days > 30 {
        return json!({"eligible": false, "rule": "RET-01", "days_since_delivery": days, "deadline": deadline,
            "reason": format!("Unfortunately the 30-day return period ended on {deadline}, so this can't be returned."),
            "alternative": "If it's faulty, it's still covered by the 2-year warranty. Say \"it stopped working\" and I'll start a warranty claim."});
    }
    json!({"eligible": true, "rule": "RET-01", "reason": null, "alternative": null, "days_since_delivery": days, "deadline": deadline})
}

pub fn warranty(delivered_on: Option<NaiveDate>, today: NaiveDate) -> Value {
    let Some(d) = delivered_on else {
        return json!({"eligible": false, "rule": "WAR-01", "reason": "This order hasn't been delivered yet, so a warranty claim isn't possible.", "alternative": null});
    };
    let until = d + Duration::days(730);
    if today > until {
        return json!({"eligible": false, "rule": "WAR-01", "warranty_until": until, "reason": "The 2-year warranty has expired.", "alternative": null});
    }
    json!({"eligible": true, "rule": "WAR-01", "reason": null, "alternative": null, "warranty_until": until})
}

/// payments: (payment_id, amount_cents, captured epoch seconds), any order.
pub fn payment_issue(payments: &[(String, i64, i64)], error_type: &str) -> Value {
    if error_type == "duplicate_charge" {
        let mut p = payments.to_vec();
        p.sort_by_key(|x| x.2);
        if let Some(w) = p
            .windows(2)
            .find(|w| w[0].1 == w[1].1 && (w[1].2 - w[0].2) <= 300)
        {
            return json!({"eligible": true, "rule": "PAY-04", "reason": null, "alternative": null, "refund_payment_id": w[1].0, "refund_amount": euros(w[1].1), "gap_seconds": w[1].2 - w[0].2});
        }
        return json!({"eligible": false, "rule": "PAY-04", "reason": "I checked the payments on this order and found only one charge.",
            "alternative": "If your bank statement shows two, it may be a pending authorisation that drops off within 7 days."});
    }
    json!({"eligible": false, "rule": "PAY-02", "reason": "No declined payment is recorded on this order; it was paid successfully.", "alternative": null})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }
    const TODAY: (i32, u32, u32) = (2026, 10, 15);

    #[test]
    fn ret01() {
        let t = d(TODAY.0, TODAY.1, TODAY.2);
        let v = return_window(Some(d(2026, 10, 5)), t);
        assert_eq!(v["eligible"], true);
        assert_eq!(v["deadline"], "2026-11-04");
        assert_eq!(v["days_since_delivery"], 10);
        let v = return_window(Some(d(2026, 6, 18)), t);
        assert_eq!(v["eligible"], false);
        assert!(
            v["reason"]
                .as_str()
                .unwrap()
                .contains("ended on 2026-07-18")
        );
        assert_eq!(return_window(None, t)["eligible"], false);
        // boundary: exactly 30 days is still inside the window
        assert_eq!(return_window(Some(d(2026, 9, 15)), t)["eligible"], true);
        assert_eq!(return_window(Some(d(2026, 9, 14)), t)["eligible"], false);
    }

    #[test]
    fn every_response_carries_reason_and_alternative_keys() {
        let t = d(TODAY.0, TODAY.1, TODAY.2);
        let p = vec![("A".to_string(), 100, 0), ("B".to_string(), 100, 10)];
        let all = [
            return_window(Some(d(2026, 10, 5)), t),
            return_window(Some(d(2026, 6, 18)), t),
            return_window(None, t),
            warranty(Some(d(2026, 6, 18)), t),
            warranty(Some(d(2024, 10, 14)), t),
            warranty(None, t),
            payment_issue(&p, "duplicate_charge"),
            payment_issue(&p[..1], "duplicate_charge"),
            payment_issue(&p, "payment_declined"),
        ];
        for v in all {
            let o = v.as_object().unwrap();
            assert!(o.contains_key("reason"), "missing reason: {v}");
            assert!(o.contains_key("alternative"), "missing alternative: {v}");
        }
        assert!(return_window(Some(d(2026, 10, 5)), t)["alternative"].is_null());
    }

    #[test]
    fn war01() {
        let t = d(TODAY.0, TODAY.1, TODAY.2);
        let v = warranty(Some(d(2026, 6, 18)), t);
        assert_eq!(v["eligible"], true);
        assert_eq!(v["warranty_until"], "2028-06-17");
        assert_eq!(warranty(Some(d(2024, 10, 14)), t)["eligible"], false);
        assert_eq!(warranty(None, t)["eligible"], false);
    }

    #[test]
    fn pay04() {
        let p = vec![
            ("PAY-3398".to_string(), 22900, 1_791_752_531),
            ("PAY-3399".to_string(), 22900, 1_791_752_569),
        ];
        let v = payment_issue(&p, "duplicate_charge");
        assert_eq!(v["eligible"], true);
        assert_eq!(v["refund_payment_id"], "PAY-3399");
        assert_eq!(v["refund_amount"], 229.0);
        assert_eq!(v["gap_seconds"], 38);
        let far = vec![("A".to_string(), 100, 0), ("B".to_string(), 100, 301)];
        assert_eq!(payment_issue(&far, "duplicate_charge")["eligible"], false);
        assert_eq!(payment_issue(&p, "payment_declined")["eligible"], false);
    }
}
