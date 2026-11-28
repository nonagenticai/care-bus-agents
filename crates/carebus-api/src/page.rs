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

/// Tokens of a free-text `q`: whitespace-separated, lowercased, `%`/`_` stripped (so a search never
/// turns into a wildcard), at most 8 tokens of 64 chars each. Every token must occur in the row.
pub fn search_tokens(q: Option<&str>) -> Vec<String> {
    q.unwrap_or("")
        .split_whitespace()
        .map(|t| t.replace(['%', '_'], "").to_lowercase())
        .filter(|t| !t.is_empty())
        .take(8)
        .map(|t| t.chars().take(64).collect())
        .collect()
}

/// The allowlisted SQL expression for a `sort` key; `None` when the caller asked for no order.
/// Unknown keys are 400 `invalid_sort` — the expression is never caller text.
pub fn sort_expr<'a>(
    sort: Option<&str>,
    allowed: &'a [(&'a str, &'a str)],
) -> Result<Option<&'a str>, ApiError> {
    match sort {
        None => Ok(None),
        Some(key) => allowed
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, expr)| Some(*expr))
            .ok_or_else(|| {
                let keys = allowed
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ");
                ApiError::BadRequest("invalid_sort", format!("sort must be one of {keys}."))
            }),
    }
}

/// `dir` as descending: absent is ascending; anything but `asc`/`desc` is 400.
pub fn descending(dir: Option<&str>) -> Result<bool, ApiError> {
    match dir {
        None | Some("asc") => Ok(false),
        Some("desc") => Ok(true),
        Some(_) => Err(ApiError::BadRequest(
            "invalid_dir",
            "dir must be asc or desc.".into(),
        )),
    }
}

/// Offset of a sorted page from its `before` cursor (`o:<n>`); `None` before the first page.
/// Sorted pages use offset paging: a keyset tuple for an arbitrary column (and its type) would
/// need one predicate per column, while these are admin lists with little concurrent churn.
pub fn sort_offset(before: Option<&str>) -> Result<i64, ApiError> {
    match before {
        None => Ok(0),
        Some(b) => b
            .strip_prefix("o:")
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| (0..=1_000_000).contains(n))
            .ok_or_else(invalid_cursor),
    }
}

/// `next` for a sorted page: the offset past this page when the page was full.
pub fn offset_next(offset: i64, limit: i64, full: bool) -> Option<String> {
    full.then(|| format!("o:{}", offset + limit))
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
