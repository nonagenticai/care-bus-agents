//! demo-backends: fake CRM, order system, carrier and payment provider for care-bus demos.
pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod extract;
pub mod faults;
pub mod money;
pub mod rules;
pub mod seed;

use std::sync::Arc;

pub use config::Config;
pub use error::{DemoError, Result};

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub cfg: Arc<Config>,
    pub faults: faults::FaultState,
}
