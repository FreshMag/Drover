//! Parallel execution of a check on the worker pool.
//!
//! The records of a check are split into chunks. Up to `threads` tickets for
//! the check are queued on the pool; each ticket claims chunks through an
//! atomic cursor until none are left, so a large batch spreads over the pool
//! while a small one costs a single task. The last chunk to finish assembles
//! the results in record order.
//!
//! The check deadline is checked between records (and passed to languages
//! that can interrupt an evaluation). Dropping the returned future cancels
//! the check: workers stop claiming chunks.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::Value as Json;
use tokio::sync::oneshot;

use super::pool::WorkerPool;
use super::{LocalStats, Plan, Snapshot};
use crate::lang::{Limits, guard};
use crate::model::{RecordResult, ReportLevel, Summary};

pub struct CheckJob {
    pub snapshot: Arc<Snapshot>,
    pub plan: Arc<Plan>,
    pub records: Vec<Json>,
    pub report: ReportLevel,
    pub deadline: Option<Instant>,
}

#[derive(Debug)]
pub struct CheckOutput {
    /// Empty when the report level is `summary`.
    pub results: Vec<RecordResult>,
    pub summary: Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("the check exceeded its time limit")]
    Timeout,
    #[error("the worker pool is shut down")]
    PoolClosed,
    #[error("a worker failed while evaluating the check")]
    Panicked,
}

type ChunkOutput = (Vec<RecordResult>, Summary);

struct Shared {
    snapshot: Arc<Snapshot>,
    plan: Arc<Plan>,
    report: ReportLevel,
    deadline: Option<Instant>,
    /// Records stay in one vector, freed together when the check ends:
    /// freeing them from the workers was measured to be slower (allocators
    /// contend on frees of memory allocated by another thread).
    records: Vec<Json>,
    limits: Limits,
    chunk_size: usize,
    chunks: usize,
    next: AtomicUsize,
    remaining: AtomicUsize,
    cancelled: AtomicBool,
    parts: Mutex<Vec<Option<ChunkOutput>>>,
    done: Mutex<Option<oneshot::Sender<Result<CheckOutput, RunError>>>>,
}

impl Shared {
    fn finish(&self, result: Result<CheckOutput, RunError>) {
        if let Some(tx) = self.done.lock().unwrap().take() {
            let _ = tx.send(result);
        }
    }

    fn fail(&self, error: RunError) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.finish(Err(error));
    }

    fn work(&self) {
        let mut stats = LocalStats::new(self.snapshot.rules.len());
        let keep = self.report != ReportLevel::Summary;
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                return;
            }
            let chunk = self.next.fetch_add(1, Ordering::Relaxed);
            if chunk >= self.chunks {
                return;
            }
            let start = chunk * self.chunk_size;
            let end = (start + self.chunk_size).min(self.records.len());
            let mut results = Vec::with_capacity(if keep { end - start } else { 0 });
            for (offset, record) in self.records[start..end].iter().enumerate() {
                if self.cancelled.load(Ordering::Relaxed) {
                    return;
                }
                if self.deadline.is_some_and(|d| Instant::now() >= d) {
                    stats.flush(&self.snapshot);
                    self.fail(RunError::Timeout);
                    return;
                }
                let result = self.snapshot.evaluate_record(
                    &self.plan,
                    record,
                    start + offset,
                    &self.limits,
                    self.report,
                    &mut stats,
                );
                stats.count_record(result.outcome);
                if keep {
                    results.push(result);
                }
            }
            stats.flush(&self.snapshot);
            let summary = std::mem::take(&mut stats.summary);
            self.parts.lock().unwrap()[chunk] = Some((results, summary));
            if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                self.assemble();
            }
        }
    }

    fn assemble(&self) {
        let parts = std::mem::take(&mut *self.parts.lock().unwrap());
        let mut summary = Summary::default();
        let mut results = Vec::with_capacity(if self.report == ReportLevel::Summary {
            0
        } else {
            self.records.len()
        });
        for (chunk_results, chunk_summary) in parts.into_iter().flatten() {
            summary.merge(&chunk_summary);
            results.extend(chunk_results);
        }
        self.finish(Ok(CheckOutput { results, summary }));
    }
}

struct CancelOnDrop(Arc<Shared>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Maximum records per chunk: small enough to balance load, large enough to
/// amortise scheduling.
const MAX_CHUNK: usize = 256;

pub async fn run(pool: &WorkerPool, job: CheckJob) -> Result<CheckOutput, RunError> {
    let total = job.records.len();
    if total == 0 {
        return Ok(CheckOutput {
            results: Vec::new(),
            summary: Summary::default(),
        });
    }
    let threads = pool.threads();
    let chunk_size = total.div_ceil(threads * 4).clamp(1, MAX_CHUNK);
    let chunks = total.div_ceil(chunk_size);
    let (tx, rx) = oneshot::channel();
    let limits = Limits::from_settings(&job.snapshot.settings, job.deadline);
    let deadline = job.deadline;
    let shared = Arc::new(Shared {
        snapshot: job.snapshot,
        plan: job.plan,
        report: job.report,
        deadline,
        records: job.records,
        limits,
        chunk_size,
        chunks,
        next: AtomicUsize::new(0),
        remaining: AtomicUsize::new(chunks),
        cancelled: AtomicBool::new(false),
        parts: Mutex::new((0..chunks).map(|_| None).collect()),
        done: Mutex::new(Some(tx)),
    });
    let _cancel = CancelOnDrop(shared.clone());
    for _ in 0..threads.min(chunks) {
        let ticket = shared.clone();
        pool.execute(move || {
            if guard(|| ticket.work()).is_none() {
                ticket.fail(RunError::Panicked);
            }
        })
        .map_err(|_| RunError::PoolClosed)?;
    }
    let outcome = match deadline {
        // Safety net for checks that wait in the queue past their deadline.
        Some(deadline) => match tokio::time::timeout_at(deadline.into(), rx).await {
            Ok(received) => received,
            Err(_) => return Err(RunError::Timeout),
        },
        None => rx.await,
    };
    outcome.unwrap_or(Err(RunError::Panicked))
}
