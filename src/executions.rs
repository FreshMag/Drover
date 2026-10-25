//! Execution manager: admits, runs and persists checks.
//!
//! # Admission
//!
//! At most `max_queued_checks` checks (sync and async, pending or running)
//! are admitted at once; further checks are rejected immediately with
//! `execution_scheduling_failed` (HTTP 503) rather than queued without bound.
//! Admitted checks share the fixed worker pool.
//!
//! # Persistence and state transitions
//!
//! * **Async**: the execution is inserted as `pending` before the request
//!   returns (if that insert fails, the request fails), updated to `running`
//!   when evaluation starts and to `completed`/`failed` when it ends. Updates
//!   are retried; a background update that ultimately fails is logged and
//!   counted, and the execution is repaired on the next startup.
//! * **Sync**: nobody can observe the execution before the response carries
//!   its id, so only the terminal state is written, once. A sync check whose
//!   execution cannot be persisted fails with `execution_persistence_failed`.
//!   If the client disconnects, the check is cancelled and not persisted.
//! * **Restart**: executions left `pending`/`running` by a previous process
//!   are marked `failed` (`interrupted`) at startup. Request records are not
//!   persisted, so interrupted checks cannot be resumed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::Value as Json;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::cache::RuleCache;
use crate::engine::pool::WorkerPool;
use crate::engine::run::{CheckJob, CheckOutput, RunError, run};
use crate::error::Error;
use crate::metrics::Metrics;
use crate::model::{CheckMode, Execution, ExecutionError, ExecutionStatus, ReportLevel};
use crate::store::ExecutionRepository;

pub struct CheckRequest {
    pub records: Vec<Json>,
    pub group: Option<String>,
    pub mode: CheckMode,
    pub report: ReportLevel,
}

pub enum Submission {
    /// A synchronous check that completed.
    Completed(Execution),
    /// An asynchronous check that was accepted (status `pending`).
    Accepted(Execution),
}

pub struct ExecutionManager {
    cache: Arc<RuleCache>,
    pool: Arc<WorkerPool>,
    repo: Arc<dyn ExecutionRepository>,
    admission: Arc<Semaphore>,
    capacity: usize,
    metrics: Arc<Metrics>,
    background: AtomicUsize,
    idle: Notify,
}

const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];

impl ExecutionManager {
    pub fn new(
        cache: Arc<RuleCache>,
        pool: Arc<WorkerPool>,
        repo: Arc<dyn ExecutionRepository>,
        capacity: usize,
        metrics: Arc<Metrics>,
    ) -> Arc<Self> {
        Arc::new(ExecutionManager {
            cache,
            pool,
            repo,
            admission: Arc::new(Semaphore::new(capacity)),
            capacity,
            metrics,
            background: AtomicUsize::new(0),
            idle: Notify::new(),
        })
    }

    /// Number of admitted checks and the admission limit.
    pub fn load(&self) -> (usize, usize) {
        (self.capacity - self.admission.available_permits(), self.capacity)
    }

    pub async fn submit(self: &Arc<Self>, request: CheckRequest) -> Result<Submission, Error> {
        let snapshot = self.cache.snapshot();
        let settings = snapshot.settings.clone();
        if request.records.len() > settings.max_records_per_check {
            return Err(Error::InvalidRequest(format!(
                "a check accepts at most {} records (max_records_per_check), got {}",
                settings.max_records_per_check,
                request.records.len()
            )));
        }
        let selected = snapshot.select(request.group.as_deref())?;
        let Ok(permit) = self.admission.clone().try_acquire_owned() else {
            self.metrics.check_rejected();
            return Err(Error::Scheduling(format!(
                "too many checks in progress (max_queued_checks = {}); retry later",
                self.capacity
            )));
        };

        let now = Utc::now();
        let mut execution = Execution {
            execution_id: Uuid::now_v7(),
            status: ExecutionStatus::Pending,
            mode: request.mode,
            outcome: None,
            selection: selected.selection,
            report: request.report,
            record_count: request.records.len(),
            created_at: now,
            started_at: None,
            finished_at: None,
            duration_ms: None,
            summary: None,
            results: None,
            diagnostics: selected.diagnostics,
            error: None,
        };
        let job = CheckJob {
            snapshot,
            plan: selected.plan,
            records: request.records,
            report: request.report,
            deadline: settings.check_timeout.map(|t| Instant::now() + t),
        };

        match request.mode {
            CheckMode::Sync => {
                let started = Instant::now();
                execution.status = ExecutionStatus::Running;
                execution.started_at = Some(now);
                let result = run(&self.pool, job).await;
                drop(permit);
                self.finish(&mut execution, result, started);
                if let Err(e) = self.repo.insert(&execution).await {
                    self.metrics.persistence_failed();
                    tracing::error!(execution_id = %execution.execution_id, "persisting execution failed: {e}");
                    return Err(Error::ExecutionPersistence {
                        execution_id: execution.execution_id.to_string(),
                        message: e.to_string(),
                    });
                }
                match &execution.error {
                    None => Ok(Submission::Completed(execution)),
                    Some(err) => Err(Error::ExecutionFailed {
                        execution_id: execution.execution_id.to_string(),
                        code: error_code(&err.code),
                        message: err.message.clone(),
                    }),
                }
            }
            CheckMode::Async => {
                if let Err(e) = self.repo.insert(&execution).await {
                    self.metrics.persistence_failed();
                    return Err(Error::ExecutionPersistence {
                        execution_id: execution.execution_id.to_string(),
                        message: e.to_string(),
                    });
                }
                let accepted = execution.clone();
                self.background.fetch_add(1, Ordering::SeqCst);
                let this = self.clone();
                tokio::spawn(async move {
                    this.run_async(execution, job, permit).await;
                    if this.background.fetch_sub(1, Ordering::SeqCst) == 1 {
                        this.idle.notify_waiters();
                    }
                });
                Ok(Submission::Accepted(accepted))
            }
        }
    }

    async fn run_async(&self, mut execution: Execution, job: CheckJob, _permit: OwnedSemaphorePermit) {
        let started = Instant::now();
        execution.status = ExecutionStatus::Running;
        execution.started_at = Some(Utc::now());
        self.persist_update(&execution).await;
        let result = run(&self.pool, job).await;
        self.finish(&mut execution, result, started);
        self.persist_update(&execution).await;
    }

    fn finish(&self, execution: &mut Execution, result: Result<CheckOutput, RunError>, started: Instant) {
        let elapsed = started.elapsed();
        execution.finished_at = Some(Utc::now());
        execution.duration_ms = Some((elapsed.as_secs_f64() * 1e6).round() / 1e3);
        match result {
            Ok(output) => {
                execution.status = ExecutionStatus::Completed;
                execution.outcome = Some(output.summary.outcome());
                execution.results = (execution.report != ReportLevel::Summary).then_some(output.results);
                execution.summary = Some(output.summary);
            }
            Err(e) => {
                let code = match e {
                    RunError::Timeout => "execution_timeout",
                    RunError::PoolClosed => "execution_scheduling_failed",
                    RunError::Panicked => "internal_error",
                };
                tracing::warn!(execution_id = %execution.execution_id, "check failed: {e}");
                execution.status = ExecutionStatus::Failed;
                execution.error = Some(ExecutionError {
                    code: code.to_string(),
                    message: e.to_string(),
                });
            }
        }
        self.metrics.check_finished(
            execution.mode,
            execution.status,
            execution.summary.as_ref(),
            elapsed.as_secs_f64(),
        );
    }

    async fn persist_update(&self, execution: &Execution) -> bool {
        for delay in RETRY_DELAYS.iter().copied().map(Some).chain([None]) {
            match self.repo.update(execution).await {
                Ok(()) => return true,
                Err(e) => {
                    self.metrics.persistence_failed();
                    tracing::warn!(
                        execution_id = %execution.execution_id,
                        status = execution.status.as_str(),
                        "updating execution failed: {e}"
                    );
                    match delay {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => break,
                    }
                }
            }
        }
        tracing::error!(
            execution_id = %execution.execution_id,
            "giving up persisting execution state '{}'", execution.status.as_str()
        );
        false
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<Execution>, Error> {
        self.repo.get(id).await.map_err(Error::data_source("executions"))
    }

    /// Marks executions interrupted by a previous shutdown as failed.
    pub async fn recover_interrupted(&self) -> Result<u64, Error> {
        let error = ExecutionError {
            code: "interrupted".into(),
            message: "the check was interrupted by a Drover restart before it completed".into(),
        };
        self.repo
            .fail_unfinished(&error, Utc::now())
            .await
            .map_err(Error::data_source("executions"))
    }

    /// Deletes finished executions older than `execution_retention_secs`.
    pub async fn purge_expired(&self) -> Result<u64, Error> {
        let Some(retention) = self.cache.snapshot().settings.execution_retention else {
            return Ok(0);
        };
        let cutoff = Utc::now() - chrono::Duration::from_std(retention).unwrap_or(chrono::Duration::MAX);
        self.repo
            .purge_finished_before(cutoff)
            .await
            .map_err(Error::data_source("executions"))
    }

    /// Waits until background (async) checks have finished, up to `timeout`.
    /// Returns the number still running.
    pub async fn drain(&self, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.idle.notified();
            let running = self.background.load(Ordering::SeqCst);
            if running == 0 {
                return 0;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.background.load(Ordering::SeqCst);
            }
        }
    }
}

fn error_code(code: &str) -> &'static str {
    match code {
        "execution_timeout" => "execution_timeout",
        "execution_scheduling_failed" => "execution_scheduling_failed",
        _ => "internal_error",
    }
}
