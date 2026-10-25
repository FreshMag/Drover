//! Drover: a lightweight REST API that evaluates rules against JSON records.

pub mod admin;
pub mod api;
pub mod app;
pub mod bootstrap;
pub mod cache;
pub mod engine;
pub mod error;
pub mod executions;
pub mod lang;
pub mod metrics;
pub mod model;
pub mod settings;
pub mod store;
pub mod testing;

pub use error::{Error, Result};
