//! One integration-test binary per crate: a single link, and libtest runs every test in parallel.
mod actions;
mod admin;
mod common;
mod faults;
mod health_auth;
mod reads;
mod records;
mod rules_api;
mod seed;
