//! Data source conformance: every backend must behave identically.
//!
//! PostgreSQL tests run when `DROVER_TEST_POSTGRES_URL` is set, e.g.
//! `postgres://drover:drover@127.0.0.1:55432/drover`; each run uses fresh,
//! uniquely named tables.

mod common;

use std::sync::Arc;

use chrono::{Duration, Utc};
use serde_json::json;
use uuid::Uuid;

use common::*;
use drover::model::{
    CheckMode, ConfigEntry, ConfigScope, Execution, ExecutionError, ExecutionStatus, Outcome, ReportLevel,
    Rule, RuleGroup, Selection, TestCase, TestScope,
};
use drover::store::{
    DataSourceConfig, DataSourcesConfig, Definition, ExecutionRepository, Repository, StoreError, Stores,
};

async fn conformance<T: Definition + std::fmt::Debug>(repo: &dyn Repository<T>, a: T, b: T, a2: T) {
    let name = repo.describe();
    assert!(repo.list().await.unwrap().is_empty(), "{name}: starts empty");
    repo.create(&b).await.unwrap();
    repo.create(&a).await.unwrap();
    assert!(
        matches!(repo.create(&a).await, Err(StoreError::Conflict { .. })),
        "{name}: duplicate create is a conflict"
    );
    let listed = repo.list().await.unwrap();
    assert_eq!(
        listed,
        vec![a.clone(), b.clone()],
        "{name}: list is sorted by key"
    );
    assert_eq!(repo.get(a.key()).await.unwrap(), Some(a.clone()));
    assert_eq!(repo.get("missing").await.unwrap(), None);

    let before = repo.fingerprint().await.unwrap();
    repo.update(&a2).await.unwrap();
    assert_eq!(
        repo.get(a.key()).await.unwrap(),
        Some(a2.clone()),
        "{name}: update replaces"
    );
    if let (Some(before), Some(after)) = (before, repo.fingerprint().await.unwrap()) {
        assert_ne!(before, after, "{name}: fingerprint changes on write");
    }
    repo.delete(b.key()).await.unwrap();
    assert!(
        matches!(repo.update(&b).await, Err(StoreError::NotFound { .. })),
        "{name}: update of missing"
    );
    assert!(
        matches!(repo.delete(b.key()).await, Err(StoreError::NotFound { .. })),
        "{name}: delete of missing"
    );
    assert_eq!(repo.list().await.unwrap(), vec![a2], "{name}: delete removes");
}

fn rules() -> (Rule, Rule, Rule) {
    let mut a = rule("age.adult", "cel", "record.age >= 18");
    a.metadata.insert("owner".into(), json!("kyc-team"));
    a.message = Some("too young".into());
    let b = rule(
        "email",
        "starlark",
        "def check(record, config):\n    return True\n",
    );
    let mut a2 = a.clone();
    a2.source = "record.age >= 21".into();
    a2.enabled = false;
    a2.message = None;
    a2.description = "Legal age".into();
    (a, b, a2)
}

fn groups() -> (RuleGroup, RuleGroup, RuleGroup) {
    let a = group("a-kyc", &["z-rule", "age.adult"]);
    let b = group("contact", &["email"]);
    let mut a2 = a.clone();
    a2.rules = vec!["email".into(), "age.adult".into(), "new".into()];
    a2.description = "KYC".into();
    (a, b, a2)
}

fn tests() -> (TestCase, TestCase, TestCase) {
    let mut a = test_case(
        "a",
        json!({"age": 3, "nested": {"list": [1, 2.5, null, "x"]}}),
        TestScope::Rule { rule: "age".into() },
        Outcome::Invalid,
        &["age"],
    );
    a.tags = vec!["kyc".into(), "smoke".into()];
    let b = test_case("b", json!({}), TestScope::All, Outcome::Valid, &[]);
    let mut a2 = a.clone();
    a2.scope = TestScope::Group { group: "kyc".into() };
    a2.expect.exclusive = true;
    a2.tags.clear();
    (a, b, a2)
}

fn config() -> (ConfigEntry, ConfigEntry, ConfigEntry) {
    let a = entry("countries", json!(["IT", "FR"]), ConfigScope::Rule);
    let b = entry("worker_threads", json!(4), ConfigScope::System);
    let mut a2 = a.clone();
    a2.value = json!({"eu": ["IT"]});
    a2.scope = ConfigScope::System;
    a2.description = "changed".into();
    (a, b, a2)
}

fn execution(status: ExecutionStatus, finished_ago: Option<Duration>) -> Execution {
    Execution {
        execution_id: Uuid::now_v7(),
        status,
        mode: CheckMode::Async,
        outcome: None,
        selection: Selection {
            requested_group: Some("g".into()),
            group: Some("g".into()),
            fallback: false,
            rule_count: 2,
            rules_version: 3,
        },
        report: ReportLevel::Failures,
        record_count: 1,
        created_at: Utc::now(),
        started_at: None,
        finished_at: finished_ago.map(|d| Utc::now() - d),
        duration_ms: None,
        summary: None,
        results: None,
        diagnostics: vec!["note".into()],
        error: None,
    }
}

/// `age_file` back-dates the stored file of an execution (file backends skip
/// files modified after the purge cutoff without parsing them).
async fn execution_conformance(repo: &dyn ExecutionRepository, age_file: Option<&dyn Fn(Uuid)>) {
    let name = repo.describe();
    let mut e = execution(ExecutionStatus::Pending, None);
    repo.insert(&e).await.unwrap();
    assert!(
        matches!(repo.insert(&e).await, Err(StoreError::Conflict { .. })),
        "{name}: no overwrite on insert"
    );
    assert_eq!(repo.get(e.execution_id).await.unwrap().as_ref(), Some(&e));

    e.status = ExecutionStatus::Completed;
    e.outcome = Some(Outcome::Invalid);
    e.finished_at = Some(Utc::now());
    e.results = Some(vec![drover::model::RecordResult {
        index: 0,
        outcome: Outcome::Invalid,
        rules: vec![drover::model::RuleOutcome {
            rule: Arc::from("r"),
            status: drover::model::RuleStatus::Fail,
            message: Some(Arc::from("m")),
            error: None,
        }],
    }]);
    repo.update(&e).await.unwrap();
    let stored = repo.get(e.execution_id).await.unwrap().unwrap();
    assert_eq!(stored.status, ExecutionStatus::Completed);
    assert_eq!(stored.results, e.results);
    assert!(repo.get(Uuid::now_v7()).await.unwrap().is_none());
    let missing = execution(ExecutionStatus::Running, None);
    assert!(matches!(
        repo.update(&missing).await,
        Err(StoreError::NotFound { .. })
    ));

    let running = execution(ExecutionStatus::Running, None);
    repo.insert(&running).await.unwrap();
    let err = ExecutionError {
        code: "interrupted".into(),
        message: "restart".into(),
    };
    assert_eq!(repo.fail_unfinished(&err, Utc::now()).await.unwrap(), 1, "{name}");
    let failed = repo.get(running.execution_id).await.unwrap().unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(failed.error, Some(err));
    assert!(failed.finished_at.is_some());
    assert_eq!(failed.diagnostics, vec!["note".to_string()]);

    let old = execution(ExecutionStatus::Completed, Some(Duration::days(30)));
    repo.insert(&old).await.unwrap();
    let unfinished = execution(ExecutionStatus::Pending, None);
    repo.insert(&unfinished).await.unwrap();
    if let Some(age) = age_file {
        age(old.execution_id);
        age(unfinished.execution_id);
    }
    assert_eq!(
        repo.purge_finished_before(Utc::now() - Duration::days(1))
            .await
            .unwrap(),
        1,
        "{name}: purge"
    );
    assert!(repo.get(old.execution_id).await.unwrap().is_none());
    assert!(repo.get(unfinished.execution_id).await.unwrap().is_some());
    assert!(repo.get(e.execution_id).await.unwrap().is_some());
}

async fn full_conformance(stores: Stores) {
    let (a, b, a2) = rules();
    conformance(&*stores.rules, a, b, a2).await;
    let (a, b, a2) = groups();
    conformance(&*stores.groups, a, b, a2).await;
    let (a, b, a2) = tests();
    conformance(&*stores.tests, a, b, a2).await;
    let (a, b, a2) = config();
    conformance(&*stores.config, a, b, a2).await;
}

#[tokio::test]
async fn memory_backend() {
    full_conformance(Stores::memory()).await;
    execution_conformance(&*Stores::memory().executions, None).await;
}

fn age_execution_file<'a>(dir: &'a std::path::Path, extension: &'static str) -> impl Fn(Uuid) + 'a {
    move |id| {
        let file = std::fs::File::options()
            .write(true)
            .open(dir.join("executions").join(format!("{id}.{extension}")))
            .unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        file.set_modified(old).unwrap();
    }
}

fn file_config(dir: &std::path::Path, yaml: bool, shared: bool) -> DataSourcesConfig {
    let ds = |name: &str| {
        let path = dir.join(if shared {
            "all".to_string()
        } else {
            name.to_string()
        });
        let path = path.with_extension(if yaml { "yaml" } else { "json" });
        if yaml {
            DataSourceConfig::Yaml { path, section: None }
        } else {
            DataSourceConfig::Json { path, section: None }
        }
    };
    DataSourcesConfig {
        rules: ds("rules"),
        groups: ds("groups"),
        tests: ds("tests"),
        config: ds("config"),
        executions: if yaml {
            DataSourceConfig::Yaml {
                path: dir.join("executions"),
                section: None,
            }
        } else {
            DataSourceConfig::Json {
                path: dir.join("executions"),
                section: None,
            }
        },
    }
}

#[tokio::test]
async fn json_backend() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&file_config(dir.path(), false, false))
        .await
        .unwrap();
    full_conformance(stores.clone()).await;
    execution_conformance(&*stores.executions, Some(&age_execution_file(dir.path(), "json"))).await;
}

#[tokio::test]
async fn yaml_backend() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&file_config(dir.path(), true, false)).await.unwrap();
    full_conformance(stores.clone()).await;
    execution_conformance(&*stores.executions, Some(&age_execution_file(dir.path(), "yaml"))).await;
}

#[tokio::test]
async fn shared_yaml_file_keeps_every_section() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&file_config(dir.path(), true, true)).await.unwrap();
    // Concurrent writes from different categories to the same file.
    let (r, g, t, c) = (rules().0, groups().0, tests().0, config().0);
    let (s1, s2, s3, s4) = (stores.clone(), stores.clone(), stores.clone(), stores.clone());
    let tasks = [
        tokio::spawn(async move { s1.rules.create(&r).await }),
        tokio::spawn(async move { s2.groups.create(&g).await }),
        tokio::spawn(async move { s3.tests.create(&t).await }),
        tokio::spawn(async move { s4.config.create(&c).await }),
    ];
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(stores.rules.list().await.unwrap().len(), 1);
    assert_eq!(stores.groups.list().await.unwrap().len(), 1);
    assert_eq!(stores.tests.list().await.unwrap().len(), 1);
    assert_eq!(stores.config.list().await.unwrap().len(), 1);
    let text = std::fs::read_to_string(dir.path().join("all.yaml")).unwrap();
    for section in ["rules:", "groups:", "tests:", "config:"] {
        assert!(text.contains(section), "{section} missing from\n{text}");
    }
}

#[tokio::test]
async fn malformed_files_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules.json");
    let config = DataSourcesConfig {
        rules: DataSourceConfig::Json {
            path: path.clone(),
            section: None,
        },
        ..DataSourcesConfig::memory()
    };
    let stores = Stores::open(&config).await.unwrap();
    std::fs::write(&path, "{\"rules\": [").unwrap();
    assert!(matches!(
        stores.rules.list().await,
        Err(StoreError::Format { .. })
    ));
    std::fs::write(&path, r#"{"rules": {"id": "x"}}"#).unwrap();
    assert!(matches!(
        stores.rules.list().await,
        Err(StoreError::Format { .. })
    ));
    std::fs::write(
        &path,
        r#"{"rules": [{"id": "x", "language": "cel", "source": "true"}, {"id": "x", "language": "cel", "source": "false"}]}"#,
    )
    .unwrap();
    let err = stores.rules.list().await.unwrap_err();
    assert!(err.to_string().contains("duplicate rule 'x'"), "{err}");
    // A failed write leaves the file untouched.
    let rule = rules().0;
    assert!(stores.rules.create(&rule).await.is_err());
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"false\""));
    // A custom section name.
    let config = DataSourcesConfig {
        rules: DataSourceConfig::Json {
            path: path.clone(),
            section: Some("validation_rules".into()),
        },
        ..DataSourcesConfig::memory()
    };
    let stores = Stores::open(&config).await.unwrap();
    stores.rules.create(&rule).await.unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("validation_rules") && text.contains("\"rules\""),
        "{text}"
    );
}

#[tokio::test]
async fn executions_reject_sections() {
    let config = DataSourcesConfig {
        executions: DataSourceConfig::Json {
            path: "x".into(),
            section: Some("e".into()),
        },
        ..DataSourcesConfig::memory()
    };
    assert!(Stores::open(&config).await.is_err());
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;

    fn pg(url: &str, table: String) -> DataSourceConfig {
        DataSourceConfig::Postgres {
            url: url.into(),
            table: Some(table),
            max_connections: 4,
            connect_timeout_secs: 5,
            auto_migrate: true,
        }
    }

    fn url() -> Option<String> {
        let url = std::env::var("DROVER_TEST_POSTGRES_URL").ok();
        if url.is_none() {
            eprintln!("DROVER_TEST_POSTGRES_URL not set; skipping PostgreSQL tests");
        }
        url
    }

    fn config(url: &str) -> DataSourcesConfig {
        let prefix = format!("t{}", Uuid::new_v4().simple());
        DataSourcesConfig {
            rules: pg(url, format!("{prefix}_rules")),
            groups: pg(url, format!("{prefix}_groups")),
            tests: pg(url, format!("{prefix}_tests")),
            config: pg(url, format!("{prefix}_config")),
            executions: pg(url, format!("{prefix}_executions")),
        }
    }

    #[tokio::test]
    async fn postgres_backend() {
        let Some(url) = url() else { return };
        let stores = Stores::open(&config(&url)).await.unwrap();
        full_conformance(stores.clone()).await;
        execution_conformance(&*stores.executions, None).await;
    }

    #[tokio::test]
    async fn postgres_sees_external_sql_changes() {
        let Some(url) = url() else { return };
        let cfg = config(&url);
        let stores = Stores::open(&cfg).await.unwrap();
        let h = harness(stores).await;
        let DataSourceConfig::Postgres {
            table: Some(rules), ..
        } = &cfg.rules
        else {
            unreachable!()
        };
        let DataSourceConfig::Postgres {
            table: Some(groups), ..
        } = &cfg.groups
        else {
            unreachable!()
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {rules} (id, language, source) VALUES ('sql-rule', 'cel', 'record.ok == true')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {groups} (name) VALUES ('g')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {groups}_members (group_name, rule_id) VALUES ('g', 'sql-rule')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        // Polling picks the change up (PostgreSQL has no cheap fingerprint).
        assert!(h.runtime.cache.refresh(false).await.unwrap().changed);
        let r = h
            .post("/api/v1/check", json!({"records": [{"ok": false}], "group": "g"}))
            .await;
        assert_eq!(r.body["outcome"], "invalid", "{}", r.text);
        // Deleting a group cascades to its memberships.
        h.admin(axum::http::Method::DELETE, "/admin/api/groups/g", None)
            .await;
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {groups}_members"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
}
