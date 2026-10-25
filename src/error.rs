//! Application error taxonomy.
//!
//! Every error that can reach a client maps to a stable machine-readable
//! `code`, an HTTP status and a human-readable message. Rule evaluation
//! errors are *not* represented here: they are part of check results
//! (`RuleStatus::Error`), because a check whose rules error still completes.

use serde_json::Value as Json;

use crate::store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Malformed or semantically invalid API request parameters.
    #[error("{0}")]
    InvalidRequest(String),
    /// The request body is not valid JSON or does not have the expected shape.
    #[error("{0}")]
    InvalidJson(String),
    #[error("request body exceeds the limit of {0} bytes")]
    PayloadTooLarge(usize),
    /// A check requested a rule group that does not exist (strict mode).
    #[error("rule group '{0}' does not exist")]
    UnknownRuleGroup(String),
    /// A rule, group or test definition is invalid (shape, references...).
    #[error("{message}")]
    InvalidDefinition { message: String, details: Option<Json> },
    /// A rule failed to compile or prepare.
    #[error("rule '{rule}' failed to compile: {message}")]
    RuleCompilation { rule: String, message: String },
    #[error("{kind} '{id}' was not found")]
    NotFound { kind: &'static str, id: String },
    #[error("{kind} '{id}' already exists")]
    Conflict { kind: &'static str, id: String },
    /// A data source could not be read or written.
    #[error("data source '{category}' failed: {source}")]
    DataSource {
        category: &'static str,
        #[source]
        source: StoreError,
    },
    /// A configuration entry or the bootstrap configuration is invalid.
    #[error("{0}")]
    ConfigValidation(String),
    /// A check could not be scheduled (admission queue full, pool stopped).
    #[error("{0}")]
    Scheduling(String),
    /// The execution record could not be persisted.
    #[error("execution {execution_id} could not be persisted: {message}")]
    ExecutionPersistence { execution_id: String, message: String },
    /// The check could not complete (timeout, worker failure).
    #[error("{message}")]
    ExecutionFailed {
        execution_id: String,
        code: &'static str,
        message: String,
    },
    /// A change was persisted but could not be applied to the running instance.
    #[error("change was saved but not applied to the running instance: {0}")]
    ApplyFailed(String),
    #[error("Drover is not ready to serve requests")]
    NotReady,
    #[error("missing or invalid credentials")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidRequest(_) => "invalid_request",
            Error::InvalidJson(_) => "invalid_json",
            Error::PayloadTooLarge(_) => "payload_too_large",
            Error::UnknownRuleGroup(_) => "unknown_rule_group",
            Error::InvalidDefinition { .. } => "invalid_definition",
            Error::RuleCompilation { .. } => "rule_compilation_failed",
            Error::NotFound { .. } => "not_found",
            Error::Conflict { .. } => "conflict",
            Error::DataSource { .. } => "data_source_error",
            Error::ConfigValidation(_) => "configuration_invalid",
            Error::Scheduling(_) => "execution_scheduling_failed",
            Error::ExecutionPersistence { .. } => "execution_persistence_failed",
            Error::ExecutionFailed { code, .. } => code,
            Error::ApplyFailed(_) => "apply_failed",
            Error::NotReady => "not_ready",
            Error::Unauthorized => "unauthorized",
            Error::Forbidden(_) => "forbidden",
            Error::Internal(_) => "internal_error",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Error::InvalidRequest(_) | Error::InvalidJson(_) => 400,
            Error::Unauthorized => 401,
            Error::Forbidden(_) => 403,
            Error::NotFound { .. } => 404,
            Error::Conflict { .. } => 409,
            Error::PayloadTooLarge(_) => 413,
            Error::UnknownRuleGroup(_)
            | Error::InvalidDefinition { .. }
            | Error::RuleCompilation { .. }
            | Error::ConfigValidation(_) => 422,
            Error::DataSource { .. }
            | Error::Scheduling(_)
            | Error::ExecutionPersistence { .. }
            | Error::NotReady => 503,
            Error::ExecutionFailed { code, .. } if *code == "execution_timeout" => 504,
            Error::ExecutionFailed { code, .. } if *code == "execution_scheduling_failed" => 503,
            Error::ExecutionFailed { .. } | Error::ApplyFailed(_) | Error::Internal(_) => 500,
        }
    }

    /// Structured details for the error response, if any.
    pub fn details(&self) -> Option<Json> {
        match self {
            Error::InvalidDefinition { details, .. } => details.clone(),
            Error::RuleCompilation { rule, message } => {
                Some(serde_json::json!({ "rule": rule, "compile_error": message }))
            }
            Error::UnknownRuleGroup(group) => Some(serde_json::json!({ "group": group })),
            Error::NotFound { kind, id } | Error::Conflict { kind, id } => {
                Some(serde_json::json!({ "kind": kind, "id": id }))
            }
            Error::ExecutionPersistence { execution_id, .. }
            | Error::ExecutionFailed { execution_id, .. } => {
                Some(serde_json::json!({ "execution_id": execution_id }))
            }
            Error::DataSource { category, .. } => Some(serde_json::json!({ "data_source": category })),
            _ => None,
        }
    }

    /// Message safe to return to untrusted API clients: data source and
    /// internal failures are reported without storage-specific detail.
    pub fn public_message(&self) -> String {
        match self {
            Error::DataSource { category, .. } => {
                format!("data source '{category}' is unavailable")
            }
            Error::ExecutionPersistence { execution_id, .. } => {
                format!("execution {execution_id} could not be persisted")
            }
            Error::Internal(_) => "internal error".to_string(),
            other => other.to_string(),
        }
    }

    pub fn invalid_definition(message: impl Into<String>) -> Self {
        Error::InvalidDefinition {
            message: message.into(),
            details: None,
        }
    }

    pub fn data_source(category: &'static str) -> impl FnOnce(StoreError) -> Error {
        move |source| match source {
            StoreError::NotFound { kind, id } => Error::NotFound { kind, id },
            StoreError::Conflict { kind, id } => Error::Conflict { kind, id },
            source => Error::DataSource { category, source },
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
