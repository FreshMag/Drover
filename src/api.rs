//! HTTP layer: routing, authentication, request parsing and error responses.
//!
//! Errors are returned as `{"error": {"code", "message", "details"?}}`.
//! Responses of the main API omit data source and internal details; the
//! (authenticated) administration API includes them so operators can act.

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json as AxumJson, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use crate::admin::{DraftRequest, RuleInput};
use crate::app::Runtime;
use crate::bootstrap::AuthConfig;
use crate::error::Error;
use crate::executions::{CheckRequest, Submission};
use crate::model::{CheckMode, ConfigEntry, Execution, ReportLevel, RuleGroup, TestCase};
use crate::testing::TestSelection;

// --- state -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Ready,
    Failed,
    Stopping,
}

struct Inner {
    runtime: OnceLock<Arc<Runtime>>,
    phase: RwLock<Phase>,
    auth: AuthConfig,
    max_body: usize,
}

/// Shared HTTP state. The server starts before initialisation completes so
/// that liveness can be probed; every API route answers `not_ready` until
/// [`AppState::set_ready`].
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl AppState {
    pub fn new(auth: AuthConfig, max_body: usize) -> Self {
        AppState(Arc::new(Inner {
            runtime: OnceLock::new(),
            phase: RwLock::new(Phase::Starting),
            auth,
            max_body,
        }))
    }

    pub fn set_ready(&self, runtime: Arc<Runtime>) {
        let _ = self.0.runtime.set(runtime);
        *self.0.phase.write().unwrap() = Phase::Ready;
    }

    pub fn set_phase(&self, phase: Phase) {
        *self.0.phase.write().unwrap() = phase;
    }

    pub fn phase(&self) -> Phase {
        self.0.phase.read().unwrap().clone()
    }

    pub fn runtime(&self) -> Result<&Arc<Runtime>, Error> {
        match (self.phase(), self.0.runtime.get()) {
            (Phase::Ready, Some(rt)) => Ok(rt),
            _ => Err(Error::NotReady),
        }
    }
}

// --- errors --------------------------------------------------------------------

/// Error of the main API (internal details redacted).
pub struct ApiError(pub Error);
/// Error of the administration API (full detail).
pub struct AdminError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

impl From<Error> for AdminError {
    fn from(e: Error) -> Self {
        AdminError(e)
    }
}

fn error_response(error: &Error, message: String) -> Response {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if status.is_server_error() {
        tracing::warn!(code = error.code(), "{error}");
    }
    let mut body = json!({ "error": { "code": error.code(), "message": message } });
    if let Some(details) = error.details() {
        body["error"]["details"] = details;
    }
    let mut response = (status, AxumJson(body)).into_response();
    let headers = response.headers_mut();
    match error {
        Error::Unauthorized => {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        Error::Scheduling(_) | Error::NotReady => {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        _ => {}
    }
    response
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error_response(&self.0, self.0.public_message())
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        error_response(&self.0, self.0.to_string())
    }
}

type ApiResult = Result<Response, ApiError>;
type AdminResult = Result<Response, AdminError>;

fn ok<T: Serialize>(value: T) -> Response {
    AxumJson(value).into_response()
}

fn created<T: Serialize>(value: T) -> Response {
    (StatusCode::CREATED, AxumJson(value)).into_response()
}

// --- request bodies ----------------------------------------------------------------

/// The raw request body, read up to `server.max_body_bytes`.
pub struct RawBody(pub Bytes);

impl FromRequest<AppState> for RawBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let limit = state.0.max_body;
        match axum::body::to_bytes(req.into_body(), limit).await {
            Ok(bytes) => Ok(RawBody(bytes)),
            Err(e) => {
                let too_large =
                    std::error::Error::source(&e).is_some_and(|s| s.is::<http_body_util::LengthLimitError>());
                Err(ApiError(if too_large {
                    Error::PayloadTooLarge(limit)
                } else {
                    Error::InvalidRequest(format!("cannot read the request body: {e}"))
                }))
            }
        }
    }
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|e| Error::InvalidJson(format!("invalid JSON body: {e}")))
}

/// A JSON request body with structured errors.
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned> FromRequest<AppState> for JsonBody<T> {
    type Rejection = AdminError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let RawBody(bytes) = RawBody::from_request(req, state)
            .await
            .map_err(|e| AdminError(e.0))?;
        Ok(JsonBody(parse_json(&bytes)?))
    }
}

fn query<T>(q: Result<Query<T>, QueryRejection>) -> Result<T, Error> {
    q.map(|Query(v)| v)
        .map_err(|e| Error::InvalidRequest(format!("invalid query string: {}", e.body_text())))
}

// --- authentication ------------------------------------------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

async fn api_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let tokens = &state.0.auth.api_tokens;
    let authorized = tokens.is_empty()
        || bearer(req.headers()).is_some_and(|t| {
            tokens
                .iter()
                .any(|k| constant_time_eq(k.as_bytes(), t.as_bytes()))
        });
    if authorized {
        next.run(req).await
    } else {
        ApiError(Error::Unauthorized).into_response()
    }
}

async fn admin_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let auth = &state.0.auth;
    let authorized = match &auth.admin_token {
        Some(token) => {
            bearer(req.headers()).is_some_and(|t| constant_time_eq(token.as_bytes(), t.as_bytes()))
        }
        None if auth.allow_unauthenticated_admin => true,
        None => {
            return AdminError(Error::Forbidden(
                "the administration API is disabled: configure auth.admin_token".into(),
            ))
            .into_response();
        }
    };
    if authorized {
        next.run(req).await
    } else {
        AdminError(Error::Unauthorized).into_response()
    }
}

async fn observe(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let mut response = next.run(req).await;
    let status = response.status();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1e3;
    if status.is_server_error() {
        tracing::warn!(%method, path, status = status.as_u16(), elapsed_ms, "request failed");
    } else {
        tracing::debug!(%method, path, status = status.as_u16(), elapsed_ms, "request");
    }
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

// --- router --------------------------------------------------------------------------

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/v1/check", post(check))
        .route("/api/v1/executions/{id}", get(get_execution))
        .route("/api/v1/tests/run", post(api_run_tests))
        .route("/metrics", get(metrics))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_auth));

    let admin = Router::new()
        .route("/admin/api/status", get(admin_status))
        .route("/admin/api/languages", get(languages))
        .route("/admin/api/settings", get(setting_specs))
        .route("/admin/api/refresh", post(refresh))
        .route("/admin/api/rules", get(list_rules).post(create_rule))
        .route("/admin/api/rules/evaluate", post(evaluate_draft))
        .route(
            "/admin/api/rules/{id}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/admin/api/groups", get(list_groups).post(create_group))
        .route(
            "/admin/api/groups/{name}",
            get(get_group).put(update_group).delete(delete_group),
        )
        .route(
            "/admin/api/groups/{name}/rules/{rule}",
            put(add_member).delete(remove_member),
        )
        .route("/admin/api/tests", get(list_tests).post(create_test))
        .route("/admin/api/tests/run", post(admin_run_tests))
        .route(
            "/admin/api/tests/{id}",
            get(get_test).put(update_test).delete(delete_test),
        )
        .route("/admin/api/tests/{id}/run", post(run_one_test))
        .route("/admin/api/config", get(list_config).post(create_config))
        .route(
            "/admin/api/config/{key}",
            get(get_config).put(update_config).delete(delete_config),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), admin_auth));

    let public = Router::new()
        .route("/", get(|| async { Redirect::temporary("/admin/") }))
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(ui_index))
        .route("/admin/app.js", get(ui_js))
        .route("/admin/app.css", get(ui_css))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready));

    Router::new()
        .merge(api)
        .merge(admin)
        .merge(public)
        .fallback(|| async {
            ApiError(Error::NotFound {
                kind: "route",
                id: "requested path".into(),
            })
        })
        .layer(middleware::from_fn(observe))
        .with_state(state)
}

// --- main API ----------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckBody {
    records: Vec<Json>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    mode: Option<CheckMode>,
    #[serde(default)]
    report: Option<ReportLevel>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckQuery {
    group: Option<String>,
    mode: Option<CheckMode>,
    report: Option<ReportLevel>,
}

fn merge<T>(name: &str, query: Option<T>, body: Option<T>) -> Result<Option<T>, Error> {
    match (query, body) {
        (Some(_), Some(_)) => Err(Error::InvalidRequest(format!(
            "'{name}' is given both in the query string and in the body"
        ))),
        (q, b) => Ok(q.or(b)),
    }
}

#[derive(Serialize)]
struct Links {
    #[serde(rename = "self")]
    this: String,
}

#[derive(Serialize)]
struct ExecutionResponse<'a> {
    #[serde(flatten)]
    execution: &'a Execution,
    links: Links,
}

fn execution_location(id: Uuid) -> String {
    format!("/api/v1/executions/{id}")
}

fn execution_response(status: StatusCode, execution: &Execution) -> Response {
    let location = execution_location(execution.execution_id);
    let body = ExecutionResponse {
        execution,
        links: Links {
            this: location.clone(),
        },
    };
    let mut response = (status, AxumJson(body)).into_response();
    let headers = response.headers_mut();
    if status == StatusCode::ACCEPTED {
        if let Ok(value) = HeaderValue::from_str(&location) {
            headers.insert(header::LOCATION, value);
        }
    }
    if !execution.status.is_terminal() {
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    response
}

async fn check(
    State(state): State<AppState>,
    q: Result<Query<CheckQuery>, QueryRejection>,
    RawBody(bytes): RawBody,
) -> ApiResult {
    let runtime = state.runtime()?;
    let q = query(q)?;
    let body: CheckBody = match bytes.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'[') => CheckBody {
            records: parse_json(&bytes)?,
            ..Default::default()
        },
        Some(b'{') => parse_json(&bytes)?,
        _ => {
            return Err(Error::InvalidJson(
                "the body must be a JSON array of records or an object with a 'records' array".into(),
            )
            .into());
        }
    };
    if let Some(i) = body.records.iter().position(|r| !r.is_object()) {
        return Err(Error::InvalidJson(format!("record {i} is not a JSON object")).into());
    }
    let group = merge("group", q.group, body.group)?;
    if group.as_deref().is_some_and(|g| g.trim().is_empty()) {
        return Err(Error::InvalidRequest("'group' must not be empty".into()).into());
    }
    let request = CheckRequest {
        records: body.records,
        group,
        mode: merge("mode", q.mode, body.mode)?.unwrap_or_default(),
        report: merge("report", q.report, body.report)?.unwrap_or_default(),
    };
    Ok(match runtime.executions.submit(request).await? {
        Submission::Completed(execution) => execution_response(StatusCode::OK, &execution),
        Submission::Accepted(execution) => execution_response(StatusCode::ACCEPTED, &execution),
    })
}

async fn get_execution(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let runtime = state.runtime()?;
    let id = Uuid::parse_str(&id)
        .map_err(|_| Error::InvalidRequest(format!("'{id}' is not a valid execution id")))?;
    let execution = runtime.executions.get(id).await?.ok_or_else(|| Error::NotFound {
        kind: "execution",
        id: id.to_string(),
    })?;
    Ok(execution_response(StatusCode::OK, &execution))
}

fn selection(bytes: &[u8]) -> Result<TestSelection, Error> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        Ok(TestSelection::default())
    } else {
        parse_json(bytes)
    }
}

async fn api_run_tests(State(state): State<AppState>, RawBody(bytes): RawBody) -> ApiResult {
    let runtime = state.runtime()?;
    Ok(ok(runtime.tests.run(&selection(&bytes)?).await?))
}

async fn metrics(State(state): State<AppState>) -> ApiResult {
    let runtime = state.runtime()?;
    let body = runtime.metrics.render(&runtime.gauges());
    Ok((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response())
}

// --- health ------------------------------------------------------------------------------

async fn live() -> Response {
    ok(json!({ "status": "live" }))
}

async fn ready(State(state): State<AppState>) -> Response {
    let unavailable = |status: &str| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            AxumJson(json!({ "status": status })),
        )
            .into_response()
    };
    match state.phase() {
        Phase::Starting => unavailable("starting"),
        Phase::Failed => unavailable("failed"),
        Phase::Stopping => unavailable("stopping"),
        Phase::Ready => match state.runtime() {
            Ok(rt) => {
                let snapshot = rt.cache.snapshot();
                ok(json!({
                    "status": "ready",
                    "rules_version": snapshot.version,
                    "rules": snapshot.rules.len(),
                    "invalid_rules": snapshot.invalid_rules().count(),
                    "groups": snapshot.groups.len(),
                }))
            }
            Err(_) => unavailable("starting"),
        },
    }
}

// --- administration ---------------------------------------------------------------------

fn admin_runtime(state: &AppState) -> Result<&Arc<Runtime>, AdminError> {
    state.runtime().map_err(AdminError)
}

async fn admin_status(State(state): State<AppState>) -> AdminResult {
    let rt = admin_runtime(&state)?;
    let snapshot = rt.cache.snapshot();
    let (admitted, capacity) = rt.executions.load();
    Ok(ok(json!({
        "rules_version": snapshot.version,
        "loaded_at": snapshot.loaded_at,
        "rules": snapshot.rules.len(),
        "enabled_rules": snapshot.all.len(),
        "groups": snapshot.groups.len(),
        "rule_config_keys": snapshot.rule_config.keys().collect::<Vec<_>>(),
        "invalid_rules": snapshot.invalid_rules().map(|r| json!({
            "id": r.def.id,
            "error": r.compile_error(),
        })).collect::<Vec<_>>(),
        "diagnostics": snapshot.diagnostics,
        "settings": *snapshot.settings,
        "refresh": rt.cache.status(),
        "pool": rt.pool.stats(),
        "checks": { "admitted": admitted, "capacity": capacity },
        "data_sources": rt.stores.describe(),
        "languages": rt.languages.ids(),
        "startup": rt.startup,
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

async fn languages(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.languages.infos()))
}

async fn setting_specs() -> Response {
    ok(crate::settings::specs())
}

async fn refresh(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.refresh().await?))
}

async fn list_rules(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.list_rules().await?))
}

async fn get_rule(State(state): State<AppState>, Path(id): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.get_rule(&id).await?))
}

async fn create_rule(State(state): State<AppState>, JsonBody(input): JsonBody<RuleInput>) -> AdminResult {
    Ok(created(admin_runtime(&state)?.admin.create_rule(input).await?))
}

async fn update_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(input): JsonBody<RuleInput>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.update_rule(&id, input).await?))
}

async fn delete_rule(State(state): State<AppState>, Path(id): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.delete_rule(&id).await?))
}

async fn evaluate_draft(
    State(state): State<AppState>,
    JsonBody(draft): JsonBody<DraftRequest>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.evaluate_draft(draft).await?))
}

async fn list_groups(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.list_groups().await?))
}

async fn get_group(State(state): State<AppState>, Path(name): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.get_group(&name).await?))
}

async fn create_group(State(state): State<AppState>, JsonBody(group): JsonBody<RuleGroup>) -> AdminResult {
    Ok(created(admin_runtime(&state)?.admin.create_group(group).await?))
}

async fn update_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
    JsonBody(group): JsonBody<RuleGroup>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?
        .admin
        .update_group(&name, group)
        .await?))
}

async fn delete_group(State(state): State<AppState>, Path(name): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.delete_group(&name).await?))
}

async fn add_member(
    State(state): State<AppState>,
    Path((name, rule)): Path<(String, String)>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.add_member(&name, &rule).await?))
}

async fn remove_member(
    State(state): State<AppState>,
    Path((name, rule)): Path<(String, String)>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?
        .admin
        .remove_member(&name, &rule)
        .await?))
}

async fn list_tests(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.list_tests().await?))
}

async fn get_test(State(state): State<AppState>, Path(id): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.get_test(&id).await?))
}

async fn create_test(State(state): State<AppState>, JsonBody(test): JsonBody<TestCase>) -> AdminResult {
    Ok(created(admin_runtime(&state)?.admin.create_test(test).await?))
}

async fn update_test(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(test): JsonBody<TestCase>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.update_test(&id, test).await?))
}

async fn delete_test(State(state): State<AppState>, Path(id): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.delete_test(&id).await?))
}

async fn admin_run_tests(State(state): State<AppState>, RawBody(bytes): RawBody) -> AdminResult {
    let selection = selection(&bytes).map_err(AdminError)?;
    Ok(ok(admin_runtime(&state)?.admin.run_tests(&selection).await?))
}

async fn run_one_test(State(state): State<AppState>, Path(id): Path<String>) -> AdminResult {
    let selection = TestSelection {
        ids: vec![id],
        tags: Vec::new(),
    };
    Ok(ok(admin_runtime(&state)?.admin.run_tests(&selection).await?))
}

async fn list_config(State(state): State<AppState>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.list_config().await?))
}

async fn get_config(State(state): State<AppState>, Path(key): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.get_config(&key).await?))
}

async fn create_config(State(state): State<AppState>, JsonBody(entry): JsonBody<ConfigEntry>) -> AdminResult {
    Ok(created(admin_runtime(&state)?.admin.create_config(entry).await?))
}

async fn update_config(
    State(state): State<AppState>,
    Path(key): Path<String>,
    JsonBody(entry): JsonBody<ConfigEntry>,
) -> AdminResult {
    Ok(ok(admin_runtime(&state)?
        .admin
        .update_config(&key, entry)
        .await?))
}

async fn delete_config(State(state): State<AppState>, Path(key): Path<String>) -> AdminResult {
    Ok(ok(admin_runtime(&state)?.admin.delete_config(&key).await?))
}

// --- administration UI ------------------------------------------------------------------------

const UI_HTML: &str = include_str!("../ui/index.html");
const UI_JS: &str = include_str!("../ui/app.js");
const UI_CSS: &str = include_str!("../ui/app.css");

fn static_asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

async fn ui_index() -> Response {
    static_asset("text/html; charset=utf-8", UI_HTML)
}

async fn ui_js() -> Response {
    static_asset("text/javascript; charset=utf-8", UI_JS)
}

async fn ui_css() -> Response {
    static_asset("text/css; charset=utf-8", UI_CSS)
}

#[cfg(test)]
mod tests {
    #[test]
    fn constant_time_comparison() {
        assert!(super::constant_time_eq(b"secret", b"secret"));
        assert!(!super::constant_time_eq(b"secret", b"secreT"));
        assert!(!super::constant_time_eq(b"secret", b"secrets"));
    }
}
