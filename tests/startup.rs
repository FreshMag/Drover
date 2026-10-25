//! Startup sequence and failure policies.

mod common;

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use common::*;
use drover::app::{self, StartOptions};
use drover::model::{
    CheckMode, ConfigScope, Execution, ExecutionStatus, Outcome, ReportLevel, Selection, TestScope,
};
use drover::settings::StartupTestPolicy;
use drover::store::memory::{MemoryExecutions, MemoryRepository};
use drover::store::{DataSourceConfig, DataSourcesConfig, ExecutionRepository, Stores};

fn options(policy: Option<StartupTestPolicy>) -> StartOptions {
    StartOptions {
        no_background: true,
        startup_tests: policy,
    }
}

#[tokio::test]
async fn invalid_configuration_prevents_startup() {
    let stores = sample_stores_with(
        vec![],
        vec![],
        vec![entry("worker_threads", json!(0), ConfigScope::System)],
    );
    let err = app::start(stores, options(None))
        .await
        .err()
        .expect("startup must fail");
    assert_eq!(err.code(), "configuration_invalid");
    assert!(err.to_string().contains("worker_threads"), "{err}");
}

#[tokio::test]
async fn invalid_rules_follow_the_configured_policy() {
    let rules = vec![
        rule("ok", "cel", "true"),
        rule("broken", "starlark", "def check(:"),
    ];
    let err = app::start(sample_stores_with(rules.clone(), vec![], vec![]), options(None))
        .await
        .err()
        .expect("invalid rules abort startup by default");
    assert!(err.to_string().contains("broken"), "{err}");

    let stores = sample_stores_with(
        rules,
        vec![],
        vec![entry("fail_on_invalid_rules", json!(false), ConfigScope::System)],
    );
    let h = harness(stores).await;
    assert!(h.runtime.startup.warnings.iter().any(|w| w.contains("broken")));
    let r = h.post("/api/v1/check", json!({"records": [{}]})).await;
    assert_eq!(r.body["outcome"], "error");
    let broken = &r.body["results"][0]["rules"][0];
    assert_eq!(broken["rule"], "broken");
    assert_eq!(broken["error"]["kind"], "compile");
    let ready = h.get("/health/ready").await;
    assert_eq!(ready.body["invalid_rules"], 1);
}

fn stores_with_tests(passing: bool) -> Stores {
    let mut stores = sample_stores();
    let outcome = if passing { Outcome::Invalid } else { Outcome::Valid };
    stores.tests = Arc::new(MemoryRepository::with_items([
        test_case(
            "minor",
            json!({"age": 3}),
            TestScope::Rule { rule: "adult".into() },
            outcome,
            &[],
        ),
        test_case(
            "adult",
            json!({"age": 33, "email": "a@b"}),
            TestScope::All,
            Outcome::Valid,
            &[],
        ),
    ]));
    stores
}

#[tokio::test]
async fn startup_tests_policy() {
    let rt = app::start(stores_with_tests(true), options(Some(StartupTestPolicy::Fail)))
        .await
        .unwrap();
    let summary = rt.startup.tests.as_ref().unwrap();
    assert_eq!((summary.total, summary.passed), (2, 2));

    let err = app::start(stores_with_tests(false), options(Some(StartupTestPolicy::Fail)))
        .await
        .err()
        .expect("a failing startup test aborts startup with the fail policy");
    assert!(err.to_string().contains("minor"), "{err}");

    let rt = app::start(stores_with_tests(false), options(Some(StartupTestPolicy::Warn)))
        .await
        .unwrap();
    let summary = rt.startup.tests.as_ref().unwrap();
    assert_eq!(summary.not_passed, vec!["minor".to_string()]);
    assert!(rt.startup.warnings.iter().any(|w| w.contains("minor")));

    let rt = app::start(stores_with_tests(false), options(Some(StartupTestPolicy::Off)))
        .await
        .unwrap();
    assert!(rt.startup.tests.is_none());

    // The policy is read from configuration when not overridden.
    let mut stores = stores_with_tests(false);
    stores.config = Arc::new(MemoryRepository::with_items([
        entry("min_age", json!(18), ConfigScope::Rule),
        entry("startup_tests", json!("fail"), ConfigScope::System),
    ]));
    assert!(app::start(stores, options(None)).await.is_err());
}

fn execution(status: ExecutionStatus) -> Execution {
    Execution {
        execution_id: Uuid::now_v7(),
        status,
        mode: CheckMode::Async,
        outcome: None,
        selection: Selection {
            requested_group: None,
            group: None,
            fallback: false,
            rule_count: 1,
            rules_version: 1,
        },
        report: ReportLevel::All,
        record_count: 1,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        duration_ms: None,
        summary: None,
        results: None,
        diagnostics: vec![],
        error: None,
    }
}

#[tokio::test]
async fn interrupted_executions_are_failed_at_startup() {
    let executions = Arc::new(MemoryExecutions::default());
    let pending = execution(ExecutionStatus::Pending);
    let running = execution(ExecutionStatus::Running);
    let mut done = execution(ExecutionStatus::Completed);
    done.outcome = Some(Outcome::Valid);
    for e in [&pending, &running, &done] {
        executions.insert(e).await.unwrap();
    }
    let mut stores = sample_stores();
    stores.executions = executions.clone();
    let rt = app::start(stores, options(Some(StartupTestPolicy::Off)))
        .await
        .unwrap();
    assert_eq!(rt.startup.recovered_executions, 2);
    for e in [&pending, &running] {
        let stored = executions.get(e.execution_id).await.unwrap().unwrap();
        assert_eq!(stored.status, ExecutionStatus::Failed);
        assert_eq!(stored.error.unwrap().code, "interrupted");
    }
    assert_eq!(
        executions.get(done.execution_id).await.unwrap().unwrap().status,
        ExecutionStatus::Completed
    );
}

#[tokio::test]
async fn unreadable_data_sources_prevent_startup() {
    let dir = tempfile::tempdir().unwrap();
    let rules = dir.path().join("rules.yaml");
    std::fs::write(
        &rules,
        "rules:\n  - id: a\n    language: cel\n    sorce: 'true'\n",
    )
    .unwrap();
    let config = DataSourcesConfig {
        rules: DataSourceConfig::Yaml {
            path: rules,
            section: None,
        },
        ..DataSourcesConfig::memory()
    };
    let stores = Stores::open(&config).await.unwrap();
    let err = app::start(stores, options(None))
        .await
        .err()
        .expect("malformed rules abort startup");
    assert_eq!(err.code(), "data_source_error");
    assert!(err.to_string().contains("sorce"), "{err}");
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn unreachable_database_prevents_startup() {
    let config = DataSourcesConfig {
        rules: DataSourceConfig::Postgres {
            url: "postgres://drover:drover@127.0.0.1:1/drover".into(),
            table: None,
            max_connections: 1,
            connect_timeout_secs: 1,
            auto_migrate: true,
        },
        ..DataSourcesConfig::memory()
    };
    let err = Stores::open(&config).await.err().expect("connection must fail");
    assert!(err.to_string().contains("cannot connect to PostgreSQL"), "{err}");
}

fn write_project(dir: &std::path::Path, test_expectation: &str) -> std::path::PathBuf {
    std::fs::write(
        dir.join("definitions.yaml"),
        format!(
            "rules:\n  - id: adult\n    language: cel\n    source: record.age >= 18\ntests:\n  - id: minor\n    record: {{age: 3}}\n    expect: {{outcome: {test_expectation}}}\n"
        ),
    )
    .unwrap();
    let config = dir.join("drover.yaml");
    std::fs::write(
        &config,
        "datasources:\n  rules: {type: yaml, path: definitions.yaml}\n  groups: {type: yaml, path: definitions.yaml}\n  tests: {type: yaml, path: definitions.yaml}\n  config: {type: memory}\n  executions: {type: memory}\n",
    )
    .unwrap();
    config
}

#[test]
fn cli_check_config_and_test_commands() {
    let bin = env!("CARGO_BIN_EXE_drover");
    let dir = tempfile::tempdir().unwrap();
    let config = write_project(dir.path(), "invalid");

    let out = Command::new(bin)
        .current_dir(dir.path())
        .args(["check-config", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("configuration OK"));

    let out = Command::new(bin)
        .current_dir(dir.path())
        .args(["test", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("PASS  minor"), "{stdout}");

    let config = write_project(dir.path(), "valid");
    let out = Command::new(bin)
        .current_dir(dir.path())
        .args(["test", "--json", "-c"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["failed"], 1);

    let out = Command::new(bin)
        .args(["-c", "/nonexistent/drover.yaml"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read configuration file"));
}
