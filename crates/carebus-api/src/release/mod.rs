//! Releases, read-only: the report of a job's current release and its stored conflicts, plus the canonical JSON
//! hash that base rows and setting overrides are keyed by. The base itself is seeded by migrations.

pub mod conflicts;
pub mod hash;
pub mod report;

pub use hash::{HashError, bundle_hash, canonical_json, row_hash};
