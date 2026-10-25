//! End-to-end tests of the main API through the HTTP router.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

use common::*;
use drover::bootstrap::AuthConfig;
use drover::model::{ConfigScope, Execution, ExecutionError};
use drover::store::{ExecutionRepository, StoreError};

#[tokio::test]
async fn sync_check_returns_and_persists_results() {
    let h = harness(sample_stores()).await;
    let r = h
        .post(
            "/api/v1/check",
            json!({"records": [
                {"age": 30, "email": "a@example.com"},
                {"age": 12, "email": "nope"}
            ]}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let b = &r.body;
    assert_eq!(b["status"], "completed");
    assert_eq!(b["mode"], "sync");
    assert_eq!(b["outcome"], "invalid");
    assert_eq!(b["summary"]["records"], 2);
    assert_eq!(b["summary"]["valid"], 1);
    assert_eq!(b["summary"]["invalid"], 1);
    assert_eq!(b["selection"]["rule_count"], 2);
    assert_eq!(b["results"][0]["outcome"], "valid");
    assert_eq!(
        b["results"][1]["rules"][0],
        json!({"rule": "adult", "status": "fail", "message": "must be an adult"})
    );
    assert_eq!(b["results"][1]["rules"][1]["message"], "invalid email");
    let id = b["execution_id"].as_str().unwrap();
    assert_eq!(b["links"]["self"], format!("/api/v1/executions/{id}"));

    // The execution was persisted and can be retrieved later.
    let stored = h.get(&format!("/api/v1/executions/{id}")).await;
    assert_eq!(stored.status, StatusCode::OK);
    assert_eq!(stored.body["results"], b["results"]);
    assert_eq!(stored.body["status"], "completed");
}

#[tokio::test]
async fn bare_array_body_with_query_parameters() {
    let h = harness(sample_stores()).await;
    let r = h
        .raw(
            Method::POST,
            "/api/v1/check?group=kyc&report=failures",
            Some(r#"[{"age": 30, "email": "x"}, {"age": 3}]"#.into()),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["selection"]["group"], "kyc");
    assert_eq!(r.body["selection"]["rule_count"], 1);
    // The e-mail rule is not in the group, and passing outcomes are omitted.
    assert_eq!(r.body["results"][0]["rules"], json!([]));
    assert_eq!(r.body["results"][1]["rules"][0]["rule"], "adult");

    let r = h
        .raw(
            Method::POST,
            "/api/v1/check?group=kyc",
            Some(r#"{"records": [], "group": "kyc"}"#.into()),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["error"]["code"], "invalid_request");
}

#[tokio::test]
async fn summary_report_omits_results() {
    let h = harness(sample_stores()).await;
    let r = h
        .post(
            "/api/v1/check",
            json!({"records": [{"age": 30, "email": "a@b"}], "report": "summary"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.get("results").is_none());
    assert_eq!(r.body["summary"]["valid"], 1);
    assert_eq!(r.body["outcome"], "valid");
}

#[tokio::test]
async fn unknown_group_strict_mode_rejects() {
    let h = harness(sample_stores()).await;
    let r = h
        .post(
            "/api/v1/check",
            json!({"records": [{"age": 1}], "group": "missing"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["error"]["code"], "unknown_rule_group");
    assert_eq!(r.body["error"]["details"]["group"], "missing");
}

#[tokio::test]
async fn unknown_group_fallback_mode_runs_all_rules() {
    let mut stores = sample_stores();
    stores.config = Arc::new(drover::store::memory::MemoryRepository::with_items([
        entry("min_age", json!(18), ConfigScope::Rule),
        entry("unknown_group_mode", json!("fallback"), ConfigScope::System),
    ]));
    let h = harness(stores).await;
    let r = h
        .post(
            "/api/v1/check",
            json!({"records": [{"age": 30, "email": "a@b"}], "group": "missing"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["selection"]["fallback"], true);
    assert_eq!(r.body["selection"]["requested_group"], "missing");
    assert_eq!(r.body["selection"]["rule_count"], 2);
    assert!(r.body["diagnostics"][0].as_str().unwrap().contains("fallback"));
}

#[tokio::test]
async fn async_check_is_accepted_then_polled() {
    let h = harness(sample_stores()).await;
    let r = h
        .post(
            "/api/v1/check",
            json!({"records": [{"age": 30, "email": "a@b"}, {"age": 2, "email": "a@b"}], "mode": "async"}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    assert_eq!(r.body["status"], "pending");
    assert!(r.body.get("results").is_none());
    let location = r.headers["location"].to_str().unwrap().to_string();
    assert_eq!(location, r.body["links"]["self"]);
    assert!(location.ends_with(r.body["execution_id"].as_str().unwrap()));

    let done = h.wait_terminal(&location).await;
    assert_eq!(done.body["status"], "completed");
    assert_eq!(done.body["mode"], "async");
    assert_eq!(done.body["outcome"], "invalid");
    assert_eq!(done.body["summary"]["invalid"], 1);
    assert_eq!(done.body["results"].as_array().unwrap().len(), 2);
    assert!(done.body["started_at"].is_string() && done.body["finished_at"].is_string());
    assert!(done.headers.get("retry-after").is_none());
}

#[tokio::test]
async fn validation_failures_are_not_execution_failures() {
    let h = harness(sample_stores_with(
        vec![
            rule("errors", "cel", "record.missing > 1"),
            rule("fails", "cel", "false"),
        ],
        vec![group("only-errors", &["errors"])],
        vec![],
    ))
    .await;
    let r = h.post("/api/v1/check", json!({"records": [{}]})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.body["status"], "completed",
        "a check with failing rules still completes"
    );
    assert_eq!(r.body["outcome"], "invalid");

    let r = h
        .post("/api/v1/check", json!({"records": [{}], "group": "only-errors"}))
        .await;
    assert_eq!(r.body["status"], "completed");
    assert_eq!(r.body["outcome"], "error");
    let rule = &r.body["results"][0]["rules"][0];
    assert_eq!(rule["status"], "error");
    assert_eq!(rule["error"]["kind"], "runtime");
    assert_eq!(r.body["summary"]["rule_errors"], 1);
}

#[tokio::test]
async fn invalid_input_is_rejected() {
    let h = harness_with(sample_stores(), admin_auth(), 512).await;
    let r = h
        .raw(
            Method::POST,
            "/api/v1/check",
            Some("{\"records\": [".into()),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["error"]["code"], "invalid_json");

    let r = h
        .raw(Method::POST, "/api/v1/check", Some("\"hello\"".into()), None)
        .await;
    assert_eq!(r.body["error"]["code"], "invalid_json");

    let r = h.post("/api/v1/check", json!({"records": [{"a": 1}, 7]})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body["error"]["message"].as_str().unwrap().contains("record 1"));

    let r = h
        .post("/api/v1/check", json!({"records": [], "grup": "kyc"}))
        .await;
    assert_eq!(r.body["error"]["code"], "invalid_json");

    let r = h
        .post("/api/v1/check", json!({"records": [], "mode": "later"}))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let r = h
        .post("/api/v1/check", json!({"records": [{"pad": "x".repeat(1000)}]}))
        .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.body["error"]["code"], "payload_too_large");

    let r = h.get("/api/v1/executions/not-a-uuid").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = h.get(&format!("/api/v1/executions/{}", Uuid::now_v7())).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["error"]["code"], "not_found");
}

#[tokio::test]
async fn record_limit_is_enforced() {
    let mut stores = sample_stores();
    stores.config = Arc::new(drover::store::memory::MemoryRepository::with_items([
        entry("min_age", json!(18), ConfigScope::Rule),
        entry("max_records_per_check", json!(2), ConfigScope::System),
    ]));
    let h = harness(stores).await;
    let r = h.post("/api/v1/check", json!({"records": [{}, {}, {}]})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_records_per_check")
    );
}

#[tokio::test]
async fn empty_check_completes() {
    let h = harness(sample_stores()).await;
    let r = h.post("/api/v1/check", json!({"records": []})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["outcome"], "valid");
    assert_eq!(r.body["summary"]["records"], 0);
}

#[tokio::test]
async fn admission_limit_rejects_excess_checks() {
    let slow = "def check(record, config):\n    n = 0\n    for i in range(200000):\n        n += i\n    return True\n";
    let h = harness(sample_stores_with(
        vec![rule("slow", "starlark", slow)],
        vec![],
        vec![
            entry("max_queued_checks", json!(1), ConfigScope::System),
            entry("starlark_max_ticks", json!(100_000_000), ConfigScope::System),
            entry("rule_timeout_ms", json!(0), ConfigScope::System),
        ],
    ))
    .await;
    let records: Vec<_> = (0..20).map(|_| json!({})).collect();
    let first = h
        .post("/api/v1/check", json!({"records": records, "mode": "async"}))
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    let second = h.post("/api/v1/check", json!({"records": [{}]})).await;
    assert_eq!(second.status, StatusCode::SERVICE_UNAVAILABLE, "{}", second.text);
    assert_eq!(second.body["error"]["code"], "execution_scheduling_failed");
    assert_eq!(second.headers["retry-after"], "1");
    let location = first.headers["location"].to_str().unwrap().to_string();
    h.wait_terminal(&location).await;
    // Capacity is released once the first check finishes.
    let third = h.post("/api/v1/check", json!({"records": [{}]})).await;
    assert_eq!(third.status, StatusCode::OK);
}

#[tokio::test]
async fn check_timeout_fails_the_execution() {
    let slow = "def check(record, config):\n    n = 0\n    for i in range(100000):\n        n += i\n    return True\n";
    let h = harness(sample_stores_with(
        vec![rule("slow", "starlark", slow)],
        vec![],
        vec![entry("check_timeout_ms", json!(50), ConfigScope::System)],
    ))
    .await;
    let records: Vec<_> = (0..5000).map(|_| json!({})).collect();
    let r = h.post("/api/v1/check", json!({"records": records.clone()})).await;
    assert_eq!(r.status, StatusCode::GATEWAY_TIMEOUT, "{}", r.text);
    assert_eq!(r.body["error"]["code"], "execution_timeout");
    let id = r.body["error"]["details"]["execution_id"].as_str().unwrap();
    let stored = h.get(&format!("/api/v1/executions/{id}")).await;
    assert_eq!(stored.body["status"], "failed");
    assert_eq!(stored.body["error"]["code"], "execution_timeout");

    let r = h
        .post("/api/v1/check", json!({"records": records, "mode": "async"}))
        .await;
    let done = h.wait_terminal(r.headers["location"].to_str().unwrap()).await;
    assert_eq!(done.body["status"], "failed");
    assert!(done.body.get("outcome").is_none());
}

struct FailingExecutions;

#[async_trait]
impl ExecutionRepository for FailingExecutions {
    async fn insert(&self, _: &Execution) -> Result<(), StoreError> {
        Err(StoreError::Database(
            "connection refused (secret-host:5432)".into(),
        ))
    }
    async fn update(&self, _: &Execution) -> Result<(), StoreError> {
        Err(StoreError::Database("down".into()))
    }
    async fn get(&self, _: Uuid) -> Result<Option<Execution>, StoreError> {
        Err(StoreError::Database("down".into()))
    }
    async fn fail_unfinished(&self, _: &ExecutionError, _: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(0)
    }
    async fn purge_finished_before(&self, _: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(0)
    }
    fn describe(&self) -> String {
        "failing".into()
    }
}

#[tokio::test]
async fn persistence_failures_are_reported_without_internal_details() {
    let mut stores = sample_stores();
    stores.executions = Arc::new(FailingExecutions);
    let h = harness(stores).await;
    for mode in ["sync", "async"] {
        let r = h
            .post("/api/v1/check", json!({"records": [{"age": 30}], "mode": mode}))
            .await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{mode}: {}", r.text);
        assert_eq!(r.body["error"]["code"], "execution_persistence_failed");
        assert!(
            !r.text.contains("secret-host"),
            "internal details leaked: {}",
            r.text
        );
    }
    let r = h.get(&format!("/api/v1/executions/{}", Uuid::now_v7())).await;
    assert_eq!(r.body["error"]["code"], "data_source_error");
    assert_eq!(
        r.body["error"]["message"],
        "data source 'executions' is unavailable"
    );
}

#[tokio::test]
async fn api_tokens_protect_the_main_api() {
    let auth = AuthConfig {
        api_tokens: vec!["client-a".into(), "client-b".into()],
        ..admin_auth()
    };
    let h = harness_with(sample_stores(), auth, 1 << 20).await;
    let body = Some(json!({"records": [{"age": 30}]}));
    let r = h.call(Method::POST, "/api/v1/check", body.clone(), None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.headers["www-authenticate"], "Bearer");
    let r = h
        .call(Method::POST, "/api/v1/check", body.clone(), Some("wrong"))
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = h
        .call(Method::POST, "/api/v1/check", body, Some("client-b"))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h.call(Method::GET, "/metrics", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // Health endpoints stay open for probes.
    assert_eq!(h.get("/health/ready").await.status, StatusCode::OK);
}

#[tokio::test]
async fn readiness_reflects_startup_phase() {
    let state = drover::api::AppState::new(admin_auth(), 1024);
    let router = drover::api::router(state.clone());
    let h = Harness {
        router,
        runtime: harness(sample_stores()).await.runtime,
        state: state.clone(),
    };
    let r = h.get("/health/ready").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.body["status"], "starting");
    assert_eq!(h.get("/health/live").await.status, StatusCode::OK);
    let r = h.post("/api/v1/check", json!({"records": []})).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.body["error"]["code"], "not_ready");

    state.set_ready(h.runtime.clone());
    let r = h.get("/health/ready").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["rules"], 2);
    state.set_phase(drover::api::Phase::Stopping);
    assert_eq!(h.get("/health/ready").await.body["status"], "stopping");
}

#[tokio::test]
async fn metrics_are_exposed() {
    let h = harness(sample_stores()).await;
    h.post(
        "/api/v1/check",
        json!({"records": [{"age": 30, "email": "a@b"}, {"age": 1}]}),
    )
    .await;
    let r = h.get("/metrics").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert!(
        r.text
            .contains("drover_checks_total{mode=\"sync\",status=\"completed\"} 1"),
        "{}",
        r.text
    );
    assert!(r.text.contains("drover_records_total 2"));
    assert!(
        r.text
            .contains("drover_rule_evaluations_total{status=\"fail\"} 2")
    );
    assert!(
        r.text
            .contains("drover_check_duration_seconds_count{mode=\"sync\"} 1")
    );
    assert!(r.text.contains("drover_rules_loaded 2"));
}

#[tokio::test]
async fn api_test_run_reports_summary() {
    let mut stores = sample_stores();
    stores.tests = Arc::new(drover::store::memory::MemoryRepository::with_items([
        test_case(
            "adult-ok",
            json!({"age": 40, "email": "a@b"}),
            drover::model::TestScope::All,
            drover::model::Outcome::Valid,
            &[],
        ),
        test_case(
            "minor-rejected",
            json!({"age": 4}),
            drover::model::TestScope::Rule { rule: "adult".into() },
            drover::model::Outcome::Invalid,
            &["adult"],
        ),
        test_case(
            "wrong",
            json!({"age": 4}),
            drover::model::TestScope::Group { group: "kyc".into() },
            drover::model::Outcome::Valid,
            &[],
        ),
    ]));
    let h = harness(stores).await;
    let r = h.raw(Method::POST, "/api/v1/tests/run", None, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["total"], 3);
    assert_eq!(r.body["passed"], 2);
    assert_eq!(r.body["failed"], 1);
    let wrong = r.body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "wrong")
        .unwrap();
    assert_eq!(wrong["status"], "failed");
    assert_eq!(wrong["actual"]["failed_rules"], json!(["adult"]));
    assert!(
        wrong["diagnostics"][0]
            .as_str()
            .unwrap()
            .contains("expected the record to be valid")
    );

    let r = h
        .post("/api/v1/tests/run", json!({"ids": ["minor-rejected"]}))
        .await;
    assert_eq!(r.body["total"], 1);
    assert_eq!(r.body["passed"], 1);
    let r = h.post("/api/v1/tests/run", json!({"ids": ["nope"]})).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_routes_use_the_error_format() {
    let h = harness(sample_stores()).await;
    let r = h.get("/nope").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body["error"]["code"], "not_found");
}
