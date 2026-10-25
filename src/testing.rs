//! Test execution.
//!
//! Tests reuse the engine's [`Snapshot::evaluate_record`], the same code path
//! as checks, and run on the worker pool. A test's *status* (`passed`,
//! `failed`, `error`) is distinct from the *outcome* of the evaluation it
//! performs: a test expecting `invalid` passes when the record is invalid.
//! `error` means the test itself could not be evaluated as written (unknown
//! group or rule, expectation naming a rule outside the scope).
//!
//! Test runs are synchronous. The latest result of each test is kept in
//! memory (not persisted) for the administration interface.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cache::RuleCache;
use crate::engine::pool::WorkerPool;
use crate::engine::{LocalStats, Plan, Snapshot};
use crate::error::Error;
use crate::lang::Limits;
use crate::metrics::Metrics;
use crate::model::{Expectation, Outcome, ReportLevel, RuleOutcome, RuleStatus, TestCase, TestScope};
use crate::store::Repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestStatus {
    Passed,
    Failed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActualOutcome {
    pub outcome: Outcome,
    pub failed_rules: Vec<String>,
    pub errored_rules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub id: String,
    pub status: TestStatus,
    pub scope: TestScope,
    pub expected: Expectation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<ActualOutcome>,
    /// Why the test did not pass (empty when it passed).
    pub diagnostics: Vec<String>,
    /// Every rule outcome of the evaluation.
    pub rules: Vec<RuleOutcome>,
    pub duration_ms: f64,
    pub rules_version: u64,
    pub ran_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub duration_ms: f64,
    pub rules_version: u64,
    pub results: Vec<TestResult>,
}

impl TestRunReport {
    pub fn all_passed(&self) -> bool {
        self.passed == self.total
    }
}

/// Which tests to run: tests matching any listed id or any listed tag.
/// An empty selection runs every test.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSelection {
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Resolves the rules a test scope evaluates.
pub fn scope_plan(snapshot: &Snapshot, scope: &TestScope) -> Result<Arc<Plan>, String> {
    match scope {
        TestScope::All => Ok(snapshot.all.clone()),
        TestScope::Group { group } => snapshot
            .groups
            .get(group)
            .map(|g| g.plan.clone())
            .ok_or_else(|| format!("rule group '{group}' does not exist")),
        TestScope::Rule { rule } => snapshot
            .plan_for_rule(rule)
            .map(Arc::new)
            .ok_or_else(|| format!("rule '{rule}' does not exist")),
    }
}

/// Evaluates one test against a snapshot.
pub fn evaluate_test(snapshot: &Snapshot, test: &TestCase, limits: &Limits) -> TestResult {
    let started = Instant::now();
    let mut result = TestResult {
        id: test.id.clone(),
        status: TestStatus::Error,
        scope: test.scope.clone(),
        expected: test.expect.clone(),
        actual: None,
        diagnostics: Vec::new(),
        rules: Vec::new(),
        duration_ms: 0.0,
        rules_version: snapshot.version,
        ran_at: Utc::now(),
    };
    let plan = match scope_plan(snapshot, &test.scope) {
        Ok(plan) => plan,
        Err(e) => {
            result.diagnostics.push(e);
            return result;
        }
    };
    let in_scope: BTreeSet<&str> = plan.rules.iter().map(|&i| &*snapshot.rules[i].id).collect();
    let outside: Vec<&str> = test
        .expect
        .rules
        .iter()
        .map(String::as_str)
        .filter(|r| !in_scope.contains(r))
        .collect();
    if !outside.is_empty() {
        result.diagnostics.push(format!(
            "expected rules are not in the test scope: {}",
            outside.join(", ")
        ));
        return result;
    }

    // Statistics are not flushed: tests do not count as production evaluations.
    let mut stats = LocalStats::new(snapshot.rules.len());
    let record = snapshot.evaluate_record(&plan, &test.record, 0, limits, ReportLevel::All, &mut stats);
    let with_status = |status: RuleStatus| -> Vec<String> {
        record
            .rules
            .iter()
            .filter(|o| o.status == status)
            .map(|o| o.rule.to_string())
            .collect()
    };
    let actual = ActualOutcome {
        outcome: record.outcome,
        failed_rules: with_status(RuleStatus::Fail),
        errored_rules: with_status(RuleStatus::Error),
    };
    result.diagnostics = check_expectation(&test.expect, &actual, &record.rules);
    result.status = if result.diagnostics.is_empty() {
        TestStatus::Passed
    } else {
        TestStatus::Failed
    };
    result.actual = Some(actual);
    result.rules = record.rules;
    result.duration_ms = started.elapsed().as_secs_f64() * 1e3;
    result
}

fn describe(outcome: &RuleOutcome) -> String {
    match (&outcome.status, &outcome.error, &outcome.message) {
        (RuleStatus::Error, Some(e), _) => format!("errored ({:?}: {})", e.kind, e.message),
        (RuleStatus::Fail, _, Some(m)) => format!("failed: {m}"),
        (status, _, _) => format!("{status:?}").to_lowercase(),
    }
}

fn check_expectation(expect: &Expectation, actual: &ActualOutcome, rules: &[RuleOutcome]) -> Vec<String> {
    let mut diagnostics = Vec::new();
    let find = |id: &str| rules.iter().find(|o| &*o.rule == id);
    if actual.outcome != expect.outcome {
        let mut detail = Vec::new();
        if !actual.failed_rules.is_empty() {
            detail.push(format!("failed: {}", actual.failed_rules.join(", ")));
        }
        if !actual.errored_rules.is_empty() {
            detail.push(format!("errored: {}", actual.errored_rules.join(", ")));
        }
        diagnostics.push(format!(
            "expected the record to be {}, but it was {}{}",
            expect.outcome.as_str(),
            actual.outcome.as_str(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", detail.join("; "))
            }
        ));
    }
    let (wanted_status, observed) = match expect.outcome {
        Outcome::Valid => return diagnostics,
        Outcome::Invalid => (RuleStatus::Fail, &actual.failed_rules),
        Outcome::Error => (RuleStatus::Error, &actual.errored_rules),
    };
    let verb = if wanted_status == RuleStatus::Fail {
        "fail"
    } else {
        "error"
    };
    for id in &expect.rules {
        if let Some(o) = find(id).filter(|o| o.status != wanted_status) {
            diagnostics.push(format!("expected rule '{id}' to {verb}, but it {}", describe(o)));
        }
    }
    if expect.exclusive {
        let unexpected: Vec<&str> = observed
            .iter()
            .map(String::as_str)
            .filter(|r| !expect.rules.iter().any(|e| e == r))
            .collect();
        if !unexpected.is_empty() {
            diagnostics.push(format!(
                "expected only [{}] to {verb}, but these did too: {}",
                expect.rules.join(", "),
                unexpected.join(", ")
            ));
        }
    }
    diagnostics
}

pub struct TestRunner {
    cache: Arc<RuleCache>,
    pool: Arc<WorkerPool>,
    repo: Arc<dyn Repository<TestCase>>,
    metrics: Arc<Metrics>,
    latest: RwLock<HashMap<String, TestResult>>,
}

impl TestRunner {
    pub fn new(
        cache: Arc<RuleCache>,
        pool: Arc<WorkerPool>,
        repo: Arc<dyn Repository<TestCase>>,
        metrics: Arc<Metrics>,
    ) -> Self {
        TestRunner {
            cache,
            pool,
            repo,
            metrics,
            latest: RwLock::new(HashMap::new()),
        }
    }

    /// Loads tests from the test data source and runs the selected ones.
    pub async fn run(&self, selection: &TestSelection) -> Result<TestRunReport, Error> {
        let tests = self.repo.list().await.map_err(Error::data_source("tests"))?;
        let unknown: Vec<&str> = selection
            .ids
            .iter()
            .map(String::as_str)
            .filter(|id| !tests.iter().any(|t| t.id == *id))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "unknown test ids: {}",
                unknown.join(", ")
            )));
        }
        let everything = selection.ids.is_empty() && selection.tags.is_empty();
        let selected: Vec<TestCase> = tests
            .into_iter()
            .filter(|t| {
                everything
                    || selection.ids.contains(&t.id)
                    || t.tags.iter().any(|tag| selection.tags.contains(tag))
            })
            .collect();
        self.run_tests(selected).await
    }

    /// Runs the given tests on the worker pool against the current snapshot.
    pub async fn run_tests(&self, tests: Vec<TestCase>) -> Result<TestRunReport, Error> {
        let snapshot = self.cache.snapshot();
        let started = Instant::now();
        let version = snapshot.version;
        let results = self
            .pool
            .spawn(move || {
                let limits = Limits::from_settings(&snapshot.settings, None);
                tests
                    .iter()
                    .map(|t| evaluate_test(&snapshot, t, &limits))
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| Error::Scheduling(format!("test run could not be executed: {e}")))?;
        let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count();
        let report = TestRunReport {
            total: results.len(),
            passed: count(TestStatus::Passed),
            failed: count(TestStatus::Failed),
            errors: count(TestStatus::Error),
            duration_ms: started.elapsed().as_secs_f64() * 1e3,
            rules_version: version,
            results,
        };
        self.metrics
            .tests_finished(report.total as u64, (report.total - report.passed) as u64);
        let mut latest = self.latest.write().unwrap();
        for r in &report.results {
            latest.insert(r.id.clone(), r.clone());
        }
        Ok(report)
    }

    pub fn latest(&self, id: &str) -> Option<TestResult> {
        self.latest.read().unwrap().get(id).cloned()
    }

    pub fn forget(&self, id: &str) {
        self.latest.write().unwrap().remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Definitions;
    use crate::engine::tests::rule;
    use crate::lang::LanguageRegistry;
    use crate::model::RuleGroup;
    use serde_json::json;

    fn snapshot() -> Snapshot {
        Snapshot::build(
            1,
            Definitions {
                rules: vec![
                    rule("adult", "cel", "record.age >= 18"),
                    rule(
                        "named",
                        "starlark",
                        "def check(record, config):\n    return len(record.get('name', '')) > 0\n",
                    ),
                    rule("strict", "cel", "record.score > 1"),
                ],
                groups: vec![RuleGroup {
                    name: "kyc".into(),
                    description: String::new(),
                    rules: vec!["adult".into()],
                }],
                config: vec![],
            },
            Arc::new(LanguageRegistry::builtin()),
            None,
        )
        .unwrap()
    }

    fn test(record: serde_json::Value, scope: TestScope, outcome: Outcome, rules: &[&str]) -> TestCase {
        TestCase {
            id: "t".into(),
            description: String::new(),
            tags: vec![],
            record,
            scope,
            expect: Expectation {
                outcome,
                rules: rules.iter().map(|s| s.to_string()).collect(),
                exclusive: false,
            },
        }
    }

    fn status(t: &TestCase) -> (TestStatus, Vec<String>) {
        let r = evaluate_test(&snapshot(), t, &Limits::default());
        (r.status, r.diagnostics)
    }

    #[test]
    fn success_expectation() {
        let t = test(
            json!({"age": 30, "name": "a", "score": 5}),
            TestScope::All,
            Outcome::Valid,
            &[],
        );
        assert_eq!(status(&t).0, TestStatus::Passed);
        let t = test(
            json!({"age": 3, "name": "a", "score": 5}),
            TestScope::All,
            Outcome::Valid,
            &[],
        );
        let (s, d) = status(&t);
        assert_eq!(s, TestStatus::Failed);
        assert!(d[0].contains("failed: adult"), "{d:?}");
    }

    #[test]
    fn general_failure_expectation() {
        let t = test(
            json!({"age": 3, "name": "a", "score": 5}),
            TestScope::All,
            Outcome::Invalid,
            &[],
        );
        assert_eq!(
            status(&t).0,
            TestStatus::Passed,
            "a test expecting rejection passes when rejected"
        );
    }

    #[test]
    fn rule_specific_failure_expectation() {
        let rec = json!({"age": 3, "name": "", "score": 5});
        let t = test(rec.clone(), TestScope::All, Outcome::Invalid, &["adult"]);
        assert_eq!(status(&t).0, TestStatus::Passed);
        let t = test(rec.clone(), TestScope::All, Outcome::Invalid, &["strict"]);
        let (s, d) = status(&t);
        assert_eq!(s, TestStatus::Failed);
        assert!(d[0].contains("expected rule 'strict' to fail"), "{d:?}");

        let mut t = test(rec, TestScope::All, Outcome::Invalid, &["adult"]);
        t.expect.exclusive = true;
        let (s, d) = status(&t);
        assert_eq!(s, TestStatus::Failed);
        assert!(d[0].contains("named"), "{d:?}");
    }

    #[test]
    fn group_failure_expectation() {
        let scope = TestScope::Group { group: "kyc".into() };
        // Only the group's rules are evaluated: `named` would fail but is out of scope.
        let t = test(json!({"age": 3}), scope.clone(), Outcome::Invalid, &[]);
        assert_eq!(status(&t).0, TestStatus::Passed);
        let t = test(json!({"age": 30}), scope, Outcome::Valid, &[]);
        assert_eq!(status(&t).0, TestStatus::Passed);
    }

    #[test]
    fn error_expectation_and_test_errors() {
        let t = test(
            json!({"age": 30}),
            TestScope::Rule {
                rule: "strict".into(),
            },
            Outcome::Error,
            &["strict"],
        );
        assert_eq!(status(&t).0, TestStatus::Passed);
        let t = test(
            json!({}),
            TestScope::Group { group: "nope".into() },
            Outcome::Valid,
            &[],
        );
        assert_eq!(status(&t).0, TestStatus::Error);
        let t = test(
            json!({}),
            TestScope::Rule { rule: "adult".into() },
            Outcome::Invalid,
            &["named"],
        );
        let (s, d) = status(&t);
        assert_eq!(s, TestStatus::Error);
        assert!(d[0].contains("not in the test scope"));
    }
}
