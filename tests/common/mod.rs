#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value as Json, json};
use tower::ServiceExt;

use drover::api::{self, AppState};
use drover::app::{self, Runtime, StartOptions};
use drover::bootstrap::AuthConfig;
use drover::model::{ConfigEntry, ConfigScope, Expectation, Outcome, Rule, RuleGroup, TestCase, TestScope};
use drover::settings::StartupTestPolicy;
use drover::store::Stores;
use drover::store::memory::MemoryRepository;

pub const ADMIN: &str = "admin-token-0123456789";

pub fn rule(id: &str, language: &str, source: &str) -> Rule {
    Rule {
        id: id.into(),
        language: language.into(),
        source: source.into(),
        description: String::new(),
        message: None,
        enabled: true,
        metadata: Default::default(),
    }
}

pub fn group(name: &str, rules: &[&str]) -> RuleGroup {
    RuleGroup {
        name: name.into(),
        description: String::new(),
        rules: rules.iter().map(|s| s.to_string()).collect(),
    }
}

pub fn entry(key: &str, value: Json, scope: ConfigScope) -> ConfigEntry {
    ConfigEntry {
        key: key.into(),
        value,
        scope,
        description: String::new(),
    }
}

pub fn test_case(id: &str, record: Json, scope: TestScope, outcome: Outcome, rules: &[&str]) -> TestCase {
    TestCase {
        id: id.into(),
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

/// Rules used by most API tests: a CEL age check reading `rule` config, a
/// Starlark e-mail check and a group containing the age check.
pub fn sample_stores() -> Stores {
    let mut adult = rule("adult", "cel", "record.age >= config.min_age");
    adult.message = Some("must be an adult".into());
    sample_stores_with(
        vec![
            adult,
            rule(
                "email",
                "starlark",
                "def check(record, config):\n    if '@' not in record.get('email', ''):\n        return {'pass': False, 'message': 'invalid email'}\n    return True\n",
            ),
        ],
        vec![group("kyc", &["adult"])],
        vec![entry("min_age", json!(18), ConfigScope::Rule)],
    )
}

pub fn sample_stores_with(rules: Vec<Rule>, groups: Vec<RuleGroup>, config: Vec<ConfigEntry>) -> Stores {
    let mut stores = Stores::memory();
    stores.rules = Arc::new(MemoryRepository::with_items(rules));
    stores.groups = Arc::new(MemoryRepository::with_items(groups));
    stores.config = Arc::new(MemoryRepository::with_items(config));
    stores
}

pub struct Harness {
    pub router: Router,
    pub runtime: Arc<Runtime>,
    pub state: AppState,
}

pub fn admin_auth() -> AuthConfig {
    AuthConfig {
        admin_token: Some(ADMIN.into()),
        api_tokens: vec![],
        allow_unauthenticated_admin: false,
    }
}

pub async fn harness(stores: Stores) -> Harness {
    harness_with(stores, admin_auth(), 1024 * 1024).await
}

pub async fn harness_with(stores: Stores, auth: AuthConfig, max_body: usize) -> Harness {
    let runtime = app::start(
        stores,
        StartOptions {
            no_background: true,
            startup_tests: Some(StartupTestPolicy::Off),
        },
    )
    .await
    .expect("runtime starts");
    let state = AppState::new(auth, max_body);
    state.set_ready(runtime.clone());
    Harness {
        router: api::router(state.clone()),
        runtime,
        state,
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Json,
    pub text: String,
}

impl Harness {
    pub async fn call(&self, method: Method, path: &str, body: Option<Json>, token: Option<&str>) -> Reply {
        let body = body.map(|b| b.to_string());
        self.raw(method, path, body, token).await
    }

    pub async fn raw(&self, method: Method, path: &str, body: Option<String>, token: Option<&str>) -> Reply {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let body = serde_json::from_slice(&bytes).unwrap_or(Json::Null);
        Reply {
            status,
            headers,
            body,
            text,
        }
    }

    pub async fn post(&self, path: &str, body: Json) -> Reply {
        self.call(Method::POST, path, Some(body), None).await
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.call(Method::GET, path, None, None).await
    }

    pub async fn admin(&self, method: Method, path: &str, body: Option<Json>) -> Reply {
        self.call(method, path, body, Some(ADMIN)).await
    }

    /// Polls an execution until it reaches a terminal state.
    pub async fn wait_terminal(&self, location: &str) -> Reply {
        for _ in 0..3000 {
            let r = self.get(location).await;
            let status = r.body["status"].as_str().unwrap_or_default().to_string();
            if status == "completed" || status == "failed" {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("execution {location} did not finish");
    }
}
