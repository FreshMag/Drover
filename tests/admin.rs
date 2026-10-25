//! Administration API: persistence, validation and runtime refresh.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use serde_json::{Value as Json, json};

use common::*;
use drover::bootstrap::AuthConfig;
use drover::model::Rule;
use drover::store::memory::MemoryRepository;
use drover::store::{DataSourceConfig, DataSourcesConfig, Repository, StoreError, Stores};

const STARLARK_NAME: &str = "def check(record, config):\n    return len(record.get('name', '')) > 0\n";

#[tokio::test]
async fn admin_api_requires_the_admin_token() {
    let h = harness(sample_stores()).await;
    assert_eq!(h.get("/admin/api/rules").await.status, StatusCode::UNAUTHORIZED);
    let r = h.call(Method::GET, "/admin/api/rules", None, Some("nope")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.admin(Method::GET, "/admin/api/rules", None).await.status,
        StatusCode::OK
    );

    let disabled = harness_with(sample_stores(), AuthConfig::default(), 1 << 20).await;
    let r = disabled.admin(Method::GET, "/admin/api/rules", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("admin_token")
    );

    let open = harness_with(
        sample_stores(),
        AuthConfig {
            allow_unauthenticated_admin: true,
            ..Default::default()
        },
        1 << 20,
    )
    .await;
    assert_eq!(open.get("/admin/api/rules").await.status, StatusCode::OK);
}

#[tokio::test]
async fn invalid_rules_are_rejected_before_persisting() {
    let h = harness(sample_stores()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "bad", "language": "cel", "source": "record.age >="})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["error"]["code"], "rule_compilation_failed");
    assert_eq!(r.body["error"]["details"]["rule"], "bad");
    assert_eq!(
        h.admin(Method::GET, "/admin/api/rules/bad", None).await.status,
        StatusCode::NOT_FOUND
    );

    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "x", "language": "lua", "source": "x"})),
        )
        .await;
    assert_eq!(r.body["error"]["code"], "invalid_definition");
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "bad id", "language": "cel", "source": "true"})),
        )
        .await;
    assert_eq!(r.body["error"]["code"], "invalid_definition");
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "x", "language": "cel", "source": "true", "groups": ["ghost"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.body["error"]["message"].as_str().unwrap().contains("ghost"));
    assert_eq!(
        h.admin(Method::GET, "/admin/api/rules/x", None).await.status,
        StatusCode::NOT_FOUND
    );
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "adult", "language": "cel", "source": "true"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn rule_changes_apply_to_checks_immediately() {
    let h = harness(sample_stores()).await;
    let check = |group: &'static str| {
        let h = &h;
        async move {
            h.post(
                "/api/v1/check",
                json!({"records": [{"age": 30, "name": ""}], "group": group}),
            )
            .await
        }
    };
    assert_eq!(check("kyc").await.body["selection"]["rule_count"], 1);

    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "named", "language": "starlark", "source": STARLARK_NAME, "groups": ["kyc"], "message": "name required"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["data"]["state"], "active");
    assert_eq!(r.body["data"]["groups"], json!(["kyc"]));
    let version = r.body["rules_version"].as_u64().unwrap();
    assert!(version > 1);

    let c = check("kyc").await;
    assert_eq!(c.body["selection"]["rule_count"], 2);
    assert_eq!(c.body["selection"]["rules_version"], version);
    assert_eq!(
        c.body["results"][0]["rules"][1],
        json!({"rule": "named", "status": "fail", "message": "name required"})
    );

    // Update: change behaviour and disable.
    let r = h
        .admin(
            Method::PUT,
            "/admin/api/rules/named",
            Some(json!({"language": "cel", "source": "true", "enabled": false})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["data"]["state"], "disabled");
    assert_eq!(
        r.body["data"]["groups"],
        json!(["kyc"]),
        "memberships are kept when `groups` is omitted"
    );
    assert_eq!(check("kyc").await.body["selection"]["rule_count"], 1);

    let r = h
        .admin(
            Method::PUT,
            "/admin/api/rules/named",
            Some(json!({"id": "other", "language": "cel", "source": "true"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Delete removes the rule from its groups.
    let r = h.admin(Method::DELETE, "/admin/api/rules/named", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let g = h.admin(Method::GET, "/admin/api/groups/kyc", None).await;
    assert_eq!(g.body["rules"], json!(["adult"]));
    assert_eq!(
        h.admin(Method::DELETE, "/admin/api/rules/named", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn group_management() {
    let h = harness(sample_stores()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/groups",
            Some(json!({"name": "contact", "rules": ["email", "ghost"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let r = h
        .admin(
            Method::POST,
            "/admin/api/groups",
            Some(json!({"name": "contact", "description": "Contact data", "rules": ["email", "email"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["data"]["rules"], json!(["email"]));
    let c = h
        .post(
            "/api/v1/check",
            json!({"records": [{"email": "x"}], "group": "contact"}),
        )
        .await;
    assert_eq!(c.body["outcome"], "invalid");
    assert_eq!(c.body["selection"]["rule_count"], 1);

    let r = h
        .admin(Method::PUT, "/admin/api/groups/contact/rules/adult", None)
        .await;
    assert_eq!(r.body["data"]["rules"], json!(["email", "adult"]));
    let r = h
        .admin(Method::DELETE, "/admin/api/groups/contact/rules/email", None)
        .await;
    assert_eq!(r.body["data"]["rules"], json!(["adult"]));
    let r = h
        .admin(Method::DELETE, "/admin/api/groups/contact/rules/email", None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let list = h.admin(Method::GET, "/admin/api/groups", None).await;
    assert_eq!(list.body.as_array().unwrap().len(), 2);
    let rules = h.admin(Method::GET, "/admin/api/rules/adult", None).await;
    assert_eq!(rules.body["groups"], json!(["contact", "kyc"]));

    assert_eq!(
        h.admin(Method::DELETE, "/admin/api/groups/contact", None)
            .await
            .status,
        StatusCode::OK
    );
    let c = h
        .post("/api/v1/check", json!({"records": [{}], "group": "contact"}))
        .await;
    assert_eq!(c.body["error"]["code"], "unknown_rule_group");
}

#[tokio::test]
async fn configuration_management() {
    let h = harness(sample_stores()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/config",
            Some(json!({"key": "worker_threads", "value": "lots", "scope": "system"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["error"]["code"], "configuration_invalid");
    let r = h
        .admin(
            Method::POST,
            "/admin/api/config",
            Some(json!({"key": "made_up", "value": 1, "scope": "system"})),
        )
        .await;
    assert_eq!(r.body["error"]["code"], "configuration_invalid");
    let r = h
        .admin(
            Method::POST,
            "/admin/api/config",
            Some(json!({"key": "x", "value": 1, "scope": "global"})),
        )
        .await;
    assert_eq!(r.body["error"]["code"], "invalid_json");

    // Rule-scoped values reach rules immediately.
    let c = h
        .post("/api/v1/check", json!({"records": [{"age": 19}], "group": "kyc"}))
        .await;
    assert_eq!(c.body["outcome"], "valid");
    let r = h
        .admin(
            Method::PUT,
            "/admin/api/config/min_age",
            Some(json!({"key": "min_age", "value": 21, "scope": "rule"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["data"]["exposed_to_rules"], true);
    let c = h
        .post("/api/v1/check", json!({"records": [{"age": 19}], "group": "kyc"}))
        .await;
    assert_eq!(c.body["outcome"], "invalid");

    // Hot system settings apply on refresh; restart-only settings say so.
    let r = h
        .admin(
            Method::POST,
            "/admin/api/config",
            Some(json!({"key": "unknown_group_mode", "value": "fallback", "scope": "system"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.body["data"]["exposed_to_rules"], false);
    assert!(r.body.get("restart_required").is_none());
    let c = h
        .post("/api/v1/check", json!({"records": [{}], "group": "nope"}))
        .await;
    assert_eq!(c.body["selection"]["fallback"], true);

    let r = h
        .admin(
            Method::POST,
            "/admin/api/config",
            Some(json!({"key": "worker_threads", "value": 2, "scope": "system"})),
        )
        .await;
    assert_eq!(r.body["restart_required"], true);
    assert_eq!(r.body["data"]["apply"], "restart");

    let list = h.admin(Method::GET, "/admin/api/config", None).await;
    assert_eq!(list.body.as_array().unwrap().len(), 3);
    assert_eq!(
        h.admin(Method::DELETE, "/admin/api/config/unknown_group_mode", None)
            .await
            .status,
        StatusCode::OK
    );
    let c = h
        .post("/api/v1/check", json!({"records": [{}], "group": "nope"}))
        .await;
    assert_eq!(c.body["error"]["code"], "unknown_rule_group");
}

#[tokio::test]
async fn test_management_and_execution() {
    let h = harness(sample_stores()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/tests",
            Some(json!({"id": "t1", "record": {"age": 3}, "scope": {"type": "rule", "rule": "ghost"}, "expect": {"outcome": "invalid"}})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = h
        .admin(
            Method::POST,
            "/admin/api/tests",
            Some(json!({"id": "t1", "record": {"age": 3}, "scope": {"type": "group", "group": "kyc"}, "expect": {"outcome": "invalid", "rules": ["email"]}})),
        )
        .await;
    assert!(r.body["error"]["message"].as_str().unwrap().contains("email"));
    let r = h
        .admin(
            Method::POST,
            "/admin/api/tests",
            Some(
                json!({"id": "t1", "record": {"age": 3}, "expect": {"outcome": "valid", "rules": ["adult"]}}),
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    for (id, record, outcome) in [
        ("minor", json!({"age": 3}), "invalid"),
        ("adult", json!({"age": 30}), "valid"),
    ] {
        let r = h
            .admin(
                Method::POST,
                "/admin/api/tests",
                Some(json!({"id": id, "tags": ["kyc"], "record": record, "scope": {"type": "rule", "rule": "adult"}, "expect": {"outcome": outcome}})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    }
    let r = h.admin(Method::POST, "/admin/api/tests/minor/run", None).await;
    assert_eq!(r.body["passed"], 1);
    let list = h.admin(Method::GET, "/admin/api/tests", None).await;
    let tests = list.body.as_array().unwrap();
    assert_eq!(tests.len(), 2);
    let minor = tests.iter().find(|t| t["id"] == "minor").unwrap();
    assert_eq!(minor["latest"]["status"], "passed");
    assert!(
        tests
            .iter()
            .find(|t| t["id"] == "adult")
            .unwrap()
            .get("latest")
            .is_none()
    );

    let r = h
        .admin(
            Method::POST,
            "/admin/api/tests/run",
            Some(json!({"tags": ["kyc"]})),
        )
        .await;
    assert_eq!(r.body["total"], 2);
    assert_eq!(r.body["passed"], 2);

    // Changing the rule makes its regression test fail.
    h.admin(
        Method::PUT,
        "/admin/api/rules/adult",
        Some(json!({"language": "cel", "source": "true"})),
    )
    .await;
    let r = h.admin(Method::POST, "/admin/api/tests/run", None).await;
    assert_eq!(r.body["failed"], 1);
    let failed = r.body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["status"] == "failed")
        .unwrap();
    assert_eq!(failed["id"], "minor");
    assert_eq!(failed["actual"]["outcome"], "valid");

    let r = h
        .admin(
            Method::PUT,
            "/admin/api/tests/minor",
            Some(json!({"id": "minor", "record": {"age": 3}, "scope": {"type": "rule", "rule": "adult"}, "expect": {"outcome": "valid"}})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(
        h.admin(Method::GET, "/admin/api/tests/minor", None)
            .await
            .body
            .get("latest")
            .is_none()
    );
    assert_eq!(
        h.admin(Method::DELETE, "/admin/api/tests/minor", None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        h.admin(Method::GET, "/admin/api/tests/minor", None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_a_rule_warns_about_tests() {
    let mut stores = sample_stores();
    stores.tests = Arc::new(MemoryRepository::with_items([test_case(
        "t",
        json!({}),
        drover::model::TestScope::Rule { rule: "email".into() },
        drover::model::Outcome::Invalid,
        &[],
    )]));
    let h = harness(stores).await;
    let r = h.admin(Method::DELETE, "/admin/api/rules/email", None).await;
    assert!(r.body["warnings"][0].as_str().unwrap().contains("t"));
    let run = h.admin(Method::POST, "/admin/api/tests/run", None).await;
    assert_eq!(run.body["errors"], 1);
}

#[tokio::test]
async fn draft_evaluation_does_not_persist() {
    let h = harness(sample_stores()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules/evaluate",
            Some(json!({"language": "cel", "source": "record.age >= config.min_age", "records": [{"age": 20}, {"age": 2}, {}]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["compiled"], true);
    let statuses: Vec<&Json> = r.body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| &x["status"])
        .collect();
    assert_eq!(statuses, vec!["pass", "fail", "error"]);

    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules/evaluate",
            Some(json!({"language": "starlark", "source": "def check(:", "records": [{}]})),
        )
        .await;
    assert_eq!(r.body["compiled"], false);
    assert!(r.body["compile_error"].is_string());
    assert_eq!(
        h.admin(Method::GET, "/admin/api/rules", None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

/// A rules repository whose `list` can be made to fail.
struct Flaky {
    inner: MemoryRepository<Rule>,
    fail_list: AtomicBool,
}

#[async_trait]
impl Repository<Rule> for Flaky {
    async fn list(&self) -> Result<Vec<Rule>, StoreError> {
        if self.fail_list.load(Ordering::SeqCst) {
            return Err(StoreError::Database("read replica unavailable".into()));
        }
        self.inner.list().await
    }
    async fn get(&self, key: &str) -> Result<Option<Rule>, StoreError> {
        self.inner.get(key).await
    }
    async fn create(&self, item: &Rule) -> Result<(), StoreError> {
        self.inner.create(item).await
    }
    async fn update(&self, item: &Rule) -> Result<(), StoreError> {
        self.inner.update(item).await
    }
    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.inner.delete(key).await
    }
    async fn fingerprint(&self) -> Result<Option<String>, StoreError> {
        Ok(None)
    }
    fn describe(&self) -> String {
        "flaky".into()
    }
}

#[tokio::test]
async fn persisted_but_unapplied_changes_report_apply_failed() {
    let flaky = Arc::new(Flaky {
        inner: MemoryRepository::with_items([rule("a", "cel", "true")]),
        fail_list: AtomicBool::new(false),
    });
    let mut stores = Stores::memory();
    stores.rules = flaky.clone();
    let h = harness(stores).await;
    flaky.fail_list.store(true, Ordering::SeqCst);
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "b", "language": "cel", "source": "false"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.text);
    assert_eq!(r.body["error"]["code"], "apply_failed");
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("read replica unavailable")
    );
    // The rule was saved but the running instance still uses the old rules.
    assert!(flaky.inner.get("b").await.unwrap().is_some());
    assert_eq!(h.runtime.cache.snapshot().rules.len(), 1);
    let status = h.admin(Method::GET, "/admin/api/status", None).await;
    assert!(
        status.body["refresh"]["last_error"]
            .as_str()
            .unwrap()
            .contains("replica")
    );

    flaky.fail_list.store(false, Ordering::SeqCst);
    let r = h.admin(Method::POST, "/admin/api/refresh", None).await;
    assert_eq!(r.body["changed"], true);
    assert_eq!(h.runtime.cache.snapshot().rules.len(), 2);
}

#[tokio::test]
async fn admin_changes_persist_to_files_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let defs = dir.path().join("definitions.yaml");
    std::fs::write(
        &defs,
        "# Drover definitions\nrules:\n  - id: adult\n    language: cel\n    source: record.age >= 18\ngroups: []\n",
    )
    .unwrap();
    let config = DataSourcesConfig {
        rules: DataSourceConfig::Yaml {
            path: defs.clone(),
            section: None,
        },
        groups: DataSourceConfig::Yaml {
            path: defs.clone(),
            section: None,
        },
        tests: DataSourceConfig::Json {
            path: dir.path().join("tests.json"),
            section: None,
        },
        config: DataSourceConfig::Yaml {
            path: dir.path().join("config.yaml"),
            section: None,
        },
        executions: DataSourceConfig::Json {
            path: dir.path().join("executions"),
            section: None,
        },
    };
    let h = harness(Stores::open(&config).await.unwrap()).await;
    let r = h
        .admin(
            Method::POST,
            "/admin/api/groups",
            Some(json!({"name": "kyc", "rules": ["adult"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let r = h
        .admin(
            Method::POST,
            "/admin/api/rules",
            Some(json!({"id": "named", "language": "starlark", "source": STARLARK_NAME, "groups": ["kyc"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    h.admin(
        Method::POST,
        "/admin/api/config",
        Some(json!({"key": "min_age", "value": 21, "scope": "rule"})),
    )
    .await;
    h.admin(
        Method::POST,
        "/admin/api/tests",
        Some(json!({"id": "t", "record": {"age": 1}, "expect": {"outcome": "invalid", "rules": ["adult"]}})),
    )
    .await;
    let check = h
        .post(
            "/api/v1/check",
            json!({"records": [{"age": 30, "name": "x"}], "group": "kyc"}),
        )
        .await;
    let execution_id = check.body["execution_id"].as_str().unwrap().to_string();

    let text = std::fs::read_to_string(&defs).unwrap();
    assert!(text.contains("id: named") && text.contains("- named"), "{text}");
    // Fields keep their declaration order (id first) for readability.
    assert!(text.find("id: named").unwrap() < text.find("language: starlark").unwrap());

    // A fresh instance on the same files sees everything.
    let h2 = harness(Stores::open(&config).await.unwrap()).await;
    let s = h2.runtime.cache.snapshot();
    assert_eq!(s.rules.len(), 2);
    assert_eq!(s.groups["kyc"].group.rules, vec!["adult", "named"]);
    assert_eq!(s.rule_config["min_age"], 21);
    assert_eq!(h2.post("/api/v1/tests/run", json!({})).await.body["passed"], 1);
    let stored = h2.get(&format!("/api/v1/executions/{execution_id}")).await;
    assert_eq!(stored.body["outcome"], "valid");
}

#[tokio::test]
async fn status_languages_and_ui() {
    let h = harness(sample_stores()).await;
    let s = h.admin(Method::GET, "/admin/api/status", None).await;
    assert_eq!(s.body["rules"], 2);
    assert_eq!(s.body["languages"], json!(["cel", "starlark"]));
    assert_eq!(s.body["data_sources"]["rules"], "memory (not persistent)");
    let l = h.admin(Method::GET, "/admin/api/languages", None).await;
    assert_eq!(l.body[1]["id"], "starlark");
    assert!(l.body[1]["template"].as_str().unwrap().contains("def check"));
    let specs = h.admin(Method::GET, "/admin/api/settings", None).await;
    assert!(
        specs
            .body
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["key"] == "unknown_group_mode")
    );

    let ui = h.get("/admin/").await;
    assert_eq!(ui.status, StatusCode::OK);
    assert!(
        ui.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("default-src 'self'")
    );
    assert_eq!(h.get("/admin/app.js").await.status, StatusCode::OK);
}
