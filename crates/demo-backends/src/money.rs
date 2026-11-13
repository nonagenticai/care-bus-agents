use crate::error::{DemoError, Result};

pub fn euros(cents: i64) -> f64 {
    cents as f64 / 100.0
}

pub fn cents(euros: f64) -> Result<i64> {
    if !euros.is_finite() || !(0.0..=1_000_000_000.0).contains(&euros) {
        return Err(DemoError::Invalid(format!(
            "amount must be a non-negative number of at most 1000000000, got {euros}"
        )));
    }
    Ok((euros * 100.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_absurd_amounts() {
        assert!(cents(1e17).is_err());
        assert!(cents(1_000_000_000.01).is_err());
        assert_eq!(cents(1_000_000_000.0).unwrap(), 100_000_000_000);
        assert!(cents(f64::INFINITY).is_err());
    }

    #[test]
    fn round_trip() {
        assert_eq!(cents(229.0).unwrap(), 22900);
        assert_eq!(cents(0.1 + 0.2).unwrap(), 30);
        assert_eq!(euros(22900), 229.0);
        assert!(cents(-1.0).is_err());
        assert!(cents(f64::NAN).is_err());
    }
}
