//! Application assembly and startup sequence.
//!
//! [`start`] performs, in order:
//!
//! 1. load and validate the application configuration (`system` entries);
//! 2. (data sources are opened by the caller, see [`Stores::open`]);
//! 3. load rules and groups, compile them and build the cache;
//! 4. apply the invalid-rule policy (`fail_on_invalid_rules`);
//! 5. start the worker pool (`worker_threads`) and the execution manager,
//!    and mark executions interrupted by a previous run as failed;
//! 6. load and run the startup test suite per `startup_tests`;
//! 7. start background tasks (cache polling, execution retention).
//!
//! Any failure aborts startup: Drover never serves requests with partially
//! initialised state. Until `start` returns, readiness reports `starting`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::admin::AdminService;
use crate::cache::RuleCache;
use crate::engine::pool::WorkerPool;
use crate::error::Error;
use crate::executions::ExecutionManager;
use crate::lang::LanguageRegistry;
use crate::metrics::{Gauges, Metrics};
use crate::settings::{Settings, StartupTestPolicy};
use crate::store::Stores;
use crate::testing::{TestRunner, TestSelection, TestStatus};

#[derive(Debug, Clone, Serialize)]
pub struct StartupTestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    /// Ids of tests that did not pass.
    pub not_passed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartupReport {
    pub started_at: DateTime<Utc>,
    pub duration_ms: f64,
    pub tests: Option<StartupTestSummary>,
    pub recovered_executions: u64,
    pub warnings: Vec<String>,
}

pub struct Runtime {
    pub stores: Stores,
    pub languages: Arc<LanguageRegistry>,
    pub cache: Arc<RuleCache>,
    pub pool: Arc<WorkerPool>,
    pub executions: Arc<ExecutionManager>,
    pub tests: Arc<TestRunner>,
    pub admin: AdminService,
    pub metrics: Arc<Metrics>,
    pub startup: StartupReport,
    background: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    /// Do not start polling and retention tasks (one-shot commands, tests).
    pub no_background: bool,
    /// Overrides the `startup_tests` setting.
    pub startup_tests: Option<StartupTestPolicy>,
}

pub async fn start(stores: Stores, options: StartOptions) -> Result<Arc<Runtime>, Error> {
    let started = Instant::now();
    let started_at = Utc::now();

    // 1. Application configuration.
    let entries = stores.config.list().await.map_err(Error::data_source("config"))?;
    let (settings, mut warnings) = Settings::from_entries(&entries)
        .map_err(|e| Error::ConfigValidation(format!("invalid application configuration: {e}")))?;
    for w in &warnings {
        tracing::warn!("{w}");
    }

    // 3. Rules, groups and the cache.
    let languages = Arc::new(LanguageRegistry::builtin());
    let cache = Arc::new(RuleCache::load(stores.clone(), languages.clone()).await?);
    let snapshot = cache.snapshot();
    tracing::info!(
        rules = snapshot.rules.len(),
        groups = snapshot.groups.len(),
        rule_config = snapshot.rule_config.len(),
        "rules loaded"
    );
    for diagnostic in &snapshot.diagnostics {
        tracing::warn!("{diagnostic}");
        warnings.push(diagnostic.clone());
    }

    // 4. Invalid rules.
    let invalid: Vec<String> = snapshot
        .invalid_rules()
        .map(|r| format!("{}: {}", r.def.id, r.compile_error().unwrap_or_default()))
        .collect();
    if !invalid.is_empty() && settings.fail_on_invalid_rules {
        return Err(Error::ConfigValidation(format!(
            "{} rule(s) failed to compile (set fail_on_invalid_rules to false to start anyway): {}",
            invalid.len(),
            invalid.join("; ")
        )));
    }

    // 5. Execution.
    let metrics = Arc::new(Metrics::default());
    let pool = WorkerPool::new(settings.worker_threads);
    let executions = ExecutionManager::new(
        cache.clone(),
        pool.clone(),
        stores.executions.clone(),
        settings.max_queued_checks,
        metrics.clone(),
    );
    let recovered_executions = executions.recover_interrupted().await?;
    if recovered_executions > 0 {
        tracing::warn!(
            count = recovered_executions,
            "marked interrupted executions as failed"
        );
    }
    let tests = Arc::new(TestRunner::new(
        cache.clone(),
        pool.clone(),
        stores.tests.clone(),
        metrics.clone(),
    ));

    // 6. Startup tests.
    let policy = options.startup_tests.unwrap_or(settings.startup_tests);
    let test_summary = match policy {
        StartupTestPolicy::Off => None,
        policy => {
            let report = tests.run(&TestSelection::default()).await?;
            let not_passed: Vec<String> = report
                .results
                .iter()
                .filter(|r| r.status != TestStatus::Passed)
                .map(|r| r.id.clone())
                .collect();
            for r in report.results.iter().filter(|r| r.status != TestStatus::Passed) {
                tracing::warn!(test = %r.id, status = ?r.status, "startup test did not pass: {}", r.diagnostics.join("; "));
            }
            tracing::info!(
                total = report.total,
                passed = report.passed,
                "startup tests finished"
            );
            if policy == StartupTestPolicy::Fail && !not_passed.is_empty() {
                pool.shutdown();
                return Err(Error::ConfigValidation(format!(
                    "{} startup test(s) did not pass (startup_tests = fail): {}",
                    not_passed.len(),
                    not_passed.join(", ")
                )));
            }
            if !not_passed.is_empty() {
                warnings.push(format!("startup tests did not pass: {}", not_passed.join(", ")));
            }
            Some(StartupTestSummary {
                total: report.total,
                passed: report.passed,
                failed: report.failed,
                errors: report.errors,
                not_passed,
            })
        }
    };

    let admin = AdminService::new(
        stores.clone(),
        cache.clone(),
        languages.clone(),
        pool.clone(),
        tests.clone(),
    );
    let runtime = Arc::new(Runtime {
        stores,
        languages,
        cache,
        pool,
        executions,
        tests,
        admin,
        metrics,
        startup: StartupReport {
            started_at,
            duration_ms: started.elapsed().as_secs_f64() * 1e3,
            tests: test_summary,
            recovered_executions,
            warnings,
        },
        background: Mutex::new(Vec::new()),
    });

    // 7. Background tasks.
    if !options.no_background {
        let tasks = vec![
            tokio::spawn(poll_refresh(runtime.cache.clone())),
            tokio::spawn(purge_executions(runtime.executions.clone())),
        ];
        *runtime.background.lock().unwrap() = tasks;
    }
    Ok(runtime)
}

async fn poll_refresh(cache: Arc<RuleCache>) {
    loop {
        let interval = cache.snapshot().settings.refresh_interval;
        match interval {
            // Polling disabled: re-check the setting periodically.
            None => tokio::time::sleep(Duration::from_secs(5)).await,
            Some(interval) => {
                tokio::time::sleep(interval).await;
                if let Err(e) = cache.refresh(false).await {
                    tracing::warn!("polling refresh failed, keeping current rules: {e}");
                }
            }
        }
    }
}

async fn purge_executions(executions: Arc<ExecutionManager>) {
    loop {
        tokio::time::sleep(Duration::from_secs(600)).await;
        match executions.purge_expired().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "purged expired executions"),
            Err(e) => tracing::warn!("purging executions failed: {e}"),
        }
    }
}

impl Runtime {
    pub fn gauges(&self) -> Gauges {
        let snapshot = self.cache.snapshot();
        let pool = self.pool.stats();
        let (admitted, capacity) = self.executions.load();
        let status = self.cache.status();
        Gauges {
            rules_version: snapshot.version,
            rules: snapshot.rules.len(),
            invalid_rules: snapshot.invalid_rules().count(),
            groups: snapshot.groups.len(),
            pool_threads: pool.threads,
            pool_busy: pool.busy,
            pool_queued: pool.queued,
            checks_admitted: admitted,
            checks_capacity: capacity,
            refresh_failures: status.failures,
            refresh_changes: status.changes,
        }
    }

    /// Stops background tasks, waits for in-flight async checks up to
    /// `grace` and stops the worker pool.
    pub async fn shutdown(&self, grace: Duration) {
        for task in self.background.lock().unwrap().drain(..) {
            task.abort();
        }
        let remaining = self.executions.drain(grace).await;
        if remaining > 0 {
            // Their workers are abandoned with the process; the executions
            // are repaired by the next startup.
            tracing::warn!(
                remaining,
                "shutting down with async checks still running; they will be marked interrupted at next startup"
            );
            return;
        }
        let pool = self.pool.clone();
        let _ = tokio::task::spawn_blocking(move || pool.shutdown()).await;
    }
}
