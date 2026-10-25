//! Domain model shared by the API, the rule engine and the data sources.
//!
//! Everything in this module is plain data: definitions that are persisted in
//! data sources (rules, groups, tests, configuration entries, executions) and
//! the result types produced by the engine.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

pub type JsonMap = serde_json::Map<String, Json>;

fn default_true() -> bool {
    true
}

/// A rule definition as stored in the rules data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    /// Identifier of the rule language (`cel`, `starlark`, ...).
    pub language: String,
    pub source: String,
    #[serde(default)]
    pub description: String,
    /// Failure message used when the rule fails without providing its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Disabled rules are not selected by checks (they can still be tested).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Free-form metadata (owner, severity, links...). Not interpreted by Drover.
    #[serde(default, skip_serializing_if = "JsonMap::is_empty")]
    pub metadata: JsonMap,
}

/// A named collection of rules. Memberships are owned by the group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleGroup {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Identifiers of the member rules.
    #[serde(default)]
    pub rules: Vec<String>,
}

/// A test: one JSON record, the rules to evaluate it against and the expected outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCase {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Tags select tests into suites (`POST /api/v1/tests/run {"tags": [...]}`).
    #[serde(default)]
    pub tags: Vec<String>,
    pub record: Json,
    #[serde(default)]
    pub scope: TestScope,
    pub expect: Expectation,
}

/// Which rules a test evaluates its record against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestScope {
    /// All enabled rules.
    #[default]
    All,
    /// The enabled rules of a group. Unknown groups are always an error for tests.
    Group { group: String },
    /// A single rule, evaluated even when it is disabled.
    Rule { rule: String },
}

/// Expected result of a test evaluation.
///
/// * `outcome: valid` — the record must pass every rule in scope.
/// * `outcome: invalid` — at least one rule must fail; when `rules` is set,
///   each listed rule must fail (and, with `exclusive`, no other rule may fail).
/// * `outcome: error` — at least one rule must error (no rule may fail); `rules`
///   and `exclusive` apply to the erroring rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclusive: bool,
}

/// Scope of a configuration entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigScope {
    /// Exposed to rule implementations as the `config` variable.
    Rule,
    /// Internal Drover setting; never visible to rules.
    System,
}

impl ConfigScope {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigScope::Rule => "rule",
            ConfigScope::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "rule" => Some(ConfigScope::Rule),
            "system" => Some(ConfigScope::System),
            _ => None,
        }
    }
}

/// A key-value configuration entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigEntry {
    pub key: String,
    pub value: Json,
    pub scope: ConfigScope,
    #[serde(default)]
    pub description: String,
}

// ---------------------------------------------------------------------------
// Evaluation results
// ---------------------------------------------------------------------------

/// Validation outcome of a record or of a whole check.
///
/// For a record: `invalid` if any rule failed, otherwise `error` if any rule
/// errored, otherwise `valid`. A check aggregates its records the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Valid,
    Invalid,
    Error,
}

impl Outcome {
    pub fn from_flags(any_fail: bool, any_error: bool) -> Self {
        if any_fail {
            Outcome::Invalid
        } else if any_error {
            Outcome::Error
        } else {
            Outcome::Valid
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Valid => "valid",
            Outcome::Invalid => "invalid",
            Outcome::Error => "error",
        }
    }
}

/// Result of one rule on one record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleStatus {
    /// The record satisfies the rule.
    Pass,
    /// The record does not satisfy the rule (an ordinary validation result).
    Fail,
    /// The rule could not be evaluated (compile error, runtime error, limit...).
    Error,
}

/// Classification of rule evaluation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleErrorKind {
    /// The rule failed to compile or prepare (it is reported on every evaluation).
    Compile,
    /// The rule raised an error while evaluating the record.
    Runtime,
    /// The rule exceeded its wall-clock time limit.
    Timeout,
    /// The rule exceeded a resource limit (Starlark ticks or heap size).
    ResourceLimit,
    /// The rule returned a value Drover cannot interpret.
    InvalidResult,
    /// The rule language runtime failed unexpectedly.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleErrorInfo {
    pub kind: RuleErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleOutcome {
    pub rule: Arc<str>,
    pub status: RuleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RuleErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordResult {
    /// Position of the record in the request array.
    pub index: usize,
    pub outcome: Outcome,
    /// Rule outcomes, filtered according to the requested report level.
    pub rules: Vec<RuleOutcome>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub records: usize,
    pub valid: usize,
    pub invalid: usize,
    pub errored: usize,
    pub rule_evaluations: u64,
    pub rule_failures: u64,
    pub rule_errors: u64,
}

impl Summary {
    pub fn outcome(&self) -> Outcome {
        Outcome::from_flags(self.invalid > 0, self.errored > 0)
    }

    pub fn merge(&mut self, other: &Summary) {
        self.records += other.records;
        self.valid += other.valid;
        self.invalid += other.invalid;
        self.errored += other.errored;
        self.rule_evaluations += other.rule_evaluations;
        self.rule_failures += other.rule_failures;
        self.rule_errors += other.rule_errors;
    }
}

/// How much per-rule detail a check reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportLevel {
    /// Every rule outcome of every record.
    #[default]
    All,
    /// Only failed and errored rule outcomes (records are still all listed).
    Failures,
    /// Only the summary; `results` is omitted.
    Summary,
}

impl ReportLevel {
    pub fn keeps(self, status: RuleStatus) -> bool {
        match self {
            ReportLevel::All => true,
            ReportLevel::Failures => status != RuleStatus::Pass,
            ReportLevel::Summary => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Executions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckMode {
    #[default]
    Sync,
    Async,
}

/// Lifecycle state of a check execution (independent of its validation outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl ExecutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, ExecutionStatus::Completed | ExecutionStatus::Failed)
    }
}

/// How the rules of a check were selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// Group named in the request, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_group: Option<String>,
    /// Group whose rules were executed; `None` means all enabled rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// True when the requested group did not exist and fallback mode applied.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// Number of rules executed per record.
    pub rule_count: usize,
    /// Version of the rule cache snapshot used by the check.
    pub rules_version: u64,
}

/// Execution-level error (the check could not complete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionError {
    pub code: String,
    pub message: String,
}

/// A persisted check execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub execution_id: Uuid,
    pub status: ExecutionStatus,
    pub mode: CheckMode,
    /// Validation outcome; present once the execution is `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    pub selection: Selection,
    pub report: ReportLevel,
    pub record_count: usize,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<RecordResult>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ExecutionError>,
}

// ---------------------------------------------------------------------------
// Identifier validation
// ---------------------------------------------------------------------------

/// Validates identifiers of rules, groups, tests and configuration keys:
/// 1–128 characters, starting with an ASCII letter or digit, then letters,
/// digits, `_`, `-`, `.` or `:`.
pub fn validate_identifier(kind: &str, value: &str) -> Result<(), String> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid {kind} '{value}': use 1-128 characters [A-Za-z0-9_.:-], starting with a letter or digit"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert!(validate_identifier("rule", "age.min-18").is_ok());
        assert!(validate_identifier("rule", "").is_err());
        assert!(validate_identifier("rule", "-x").is_err());
        assert!(validate_identifier("rule", "a b").is_err());
        assert!(validate_identifier("rule", &"a".repeat(129)).is_err());
    }

    #[test]
    fn test_case_yaml_shape() {
        let t: TestCase = serde_yaml_ng::from_str(
            "id: t1\nrecord: {age: 3}\nscope: {type: rule, rule: adult}\nexpect: {outcome: invalid, rules: [adult]}\n",
        )
        .unwrap();
        assert_eq!(t.scope, TestScope::Rule { rule: "adult".into() });
        assert_eq!(t.expect.outcome, Outcome::Invalid);
        let t: TestCase =
            serde_json::from_str(r#"{"id":"t2","record":{},"expect":{"outcome":"valid"}}"#).unwrap();
        assert_eq!(t.scope, TestScope::All);
    }

    #[test]
    fn outcome_priority() {
        assert_eq!(Outcome::from_flags(true, true), Outcome::Invalid);
        assert_eq!(Outcome::from_flags(false, true), Outcome::Error);
        assert_eq!(Outcome::from_flags(false, false), Outcome::Valid);
    }
}
