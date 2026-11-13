use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

#[derive(Debug, Default, Deserialize)]
pub struct Page {
    pub limit: Option<i64>,
    pub before: Option<String>,
}
impl Page {
    /// Built from the `limit`/`before` fields of a route's own query struct (no serde flatten).
    pub fn new(limit: Option<i64>, before: Option<String>) -> Self {
        Self { limit, before }
    }

    /// `before` as an integer cursor (a row id); unparseable is 400 `invalid_cursor`.
    pub fn before_i64(&self) -> Result<Option<i64>, ApiError> {
        self.before
            .as_deref()
            .map(|b| b.parse::<i64>().map_err(|_| invalid_cursor()))
            .transpose()
    }

    /// `before` as a timestamp cursor (see `ts_cursor`); unparseable is 400 `invalid_cursor`.
    pub fn before_ts(&self) -> Result<Option<DateTime<Utc>>, ApiError> {
        self.before
            .as_deref()
            .map(|b| {
                DateTime::parse_from_rfc3339(b)
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|_| invalid_cursor())
            })
            .transpose()
    }

    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, 500)
    }
}

fn invalid_cursor() -> ApiError {
    ApiError::BadRequest("invalid_cursor", "The before cursor is not valid.".into())
}

/// Timestamp cursor: microsecond precision (matches Postgres), `Z` suffix.
pub fn ts_cursor(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Micros, true)
}

#[derive(Debug, Serialize)]
pub struct Paged<T: Serialize> {
    pub items: Vec<T>,
    pub next: Option<String>,
}
impl<T: Serialize> Paged<T> {
    /// `key` returns the sort key of a row; a full page means there may be more.
    pub fn new(items: Vec<T>, limit: i64, key: impl Fn(&T) -> String) -> Self {
        let next = (items.len() as i64 == limit)
            .then(|| items.last().map(&key))
            .flatten();
        Self { items, next }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_defaults_and_clamps() {
        assert_eq!(Page::default().limit(), 50);
        assert_eq!(
            Page {
                limit: Some(0),
                before: None
            }
            .limit(),
            1
        );
        assert_eq!(
            Page {
                limit: Some(9999),
                before: None
            }
            .limit(),
            500
        );
    }

    #[test]
    fn cursors_parse_or_400() {
        let p = Page::new(None, Some("12".into()));
        assert_eq!(p.before_i64().unwrap(), Some(12));
        assert!(Page::new(None, Some("x".into())).before_i64().is_err());
        assert_eq!(Page::default().before_i64().unwrap(), None);
        let t = "2026-10-15T10:00:00.123456Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        assert_eq!(ts_cursor(t), "2026-10-15T10:00:00.123456Z");
        let back = Page::new(None, Some(ts_cursor(t))).before_ts().unwrap();
        assert_eq!(back, Some(t));
        assert!(Page::new(None, Some("nope".into())).before_ts().is_err());
    }

    #[test]
    fn next_cursor_only_on_a_full_page() {
        let full = Paged::new(vec![1, 2], 2, |n| n.to_string());
        assert_eq!(full.next.as_deref(), Some("2"));
        let short = Paged::new(vec![1], 2, |n| n.to_string());
        assert_eq!(short.next, None);
    }
}
