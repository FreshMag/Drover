//! Process metrics, exposed in Prometheus text format at `GET /metrics`.
//!
//! Counters are plain atomics updated once per check (never per rule
//! evaluation); per-rule statistics live in the rule cache and are exposed by
//! the administration API.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::model::{CheckMode, ExecutionStatus, Summary};

const BUCKETS: [f64; 10] = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0];

#[derive(Default)]
struct Histogram {
    buckets: [AtomicU64; BUCKETS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl Histogram {
    fn observe(&self, seconds: f64) {
        for (i, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros
            .fetch_add((seconds * 1e6) as u64, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        for (i, bound) in BUCKETS.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{{labels}le=\"{bound}\"}} {}",
                self.buckets[i].load(Ordering::Relaxed)
            );
        }
        let count = self.count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{{labels}le=\"+Inf\"}} {count}");
        let _ = writeln!(
            out,
            "{name}_sum{{{}}} {}",
            labels.trim_end_matches(','),
            self.sum_micros.load(Ordering::Relaxed) as f64 / 1e6
        );
        let _ = writeln!(out, "{name}_count{{{}}} {count}", labels.trim_end_matches(','));
    }
}

#[derive(Default)]
pub struct Metrics {
    /// [mode][completed, failed]
    checks: [[AtomicU64; 2]; 2],
    checks_rejected: AtomicU64,
    records: AtomicU64,
    /// [pass, fail, error]
    rule_evaluations: [AtomicU64; 3],
    durations: [Histogram; 2],
    persistence_failures: AtomicU64,
    tests_run: AtomicU64,
    tests_failed: AtomicU64,
}

fn mode_index(mode: CheckMode) -> usize {
    match mode {
        CheckMode::Sync => 0,
        CheckMode::Async => 1,
    }
}

/// Values sampled at render time from other components.
pub struct Gauges {
    pub rules_version: u64,
    pub rules: usize,
    pub invalid_rules: usize,
    pub groups: usize,
    pub pool_threads: usize,
    pub pool_busy: usize,
    pub pool_queued: usize,
    pub checks_admitted: usize,
    pub checks_capacity: usize,
    pub refresh_failures: u64,
    pub refresh_changes: u64,
}

impl Metrics {
    pub fn check_finished(
        &self,
        mode: CheckMode,
        status: ExecutionStatus,
        summary: Option<&Summary>,
        seconds: f64,
    ) {
        let status = usize::from(status == ExecutionStatus::Failed);
        self.checks[mode_index(mode)][status].fetch_add(1, Ordering::Relaxed);
        self.durations[mode_index(mode)].observe(seconds);
        if let Some(s) = summary {
            self.records.fetch_add(s.records as u64, Ordering::Relaxed);
            let pass = s.rule_evaluations - s.rule_failures - s.rule_errors;
            self.rule_evaluations[0].fetch_add(pass, Ordering::Relaxed);
            self.rule_evaluations[1].fetch_add(s.rule_failures, Ordering::Relaxed);
            self.rule_evaluations[2].fetch_add(s.rule_errors, Ordering::Relaxed);
        }
    }

    pub fn check_rejected(&self) {
        self.checks_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn persistence_failed(&self) {
        self.persistence_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn tests_finished(&self, run: u64, failed: u64) {
        self.tests_run.fetch_add(run, Ordering::Relaxed);
        self.tests_failed.fetch_add(failed, Ordering::Relaxed);
    }

    pub fn render(&self, g: &Gauges) -> String {
        let mut out = String::with_capacity(4096);
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);

        out.push_str("# HELP drover_checks_total Finished checks by mode and execution status.\n# TYPE drover_checks_total counter\n");
        for (m, mode) in ["sync", "async"].iter().enumerate() {
            for (s, status) in ["completed", "failed"].iter().enumerate() {
                let _ = writeln!(
                    out,
                    "drover_checks_total{{mode=\"{mode}\",status=\"{status}\"}} {}",
                    load(&self.checks[m][s])
                );
            }
        }
        let counters = [
            (
                "drover_checks_rejected_total",
                "Checks rejected because the admission limit was reached.",
                load(&self.checks_rejected),
            ),
            (
                "drover_records_total",
                "Records evaluated by completed checks.",
                load(&self.records),
            ),
            (
                "drover_execution_persistence_failures_total",
                "Failed writes to the execution data source.",
                load(&self.persistence_failures),
            ),
            (
                "drover_tests_run_total",
                "Tests executed (API, admin and startup).",
                load(&self.tests_run),
            ),
            (
                "drover_tests_failed_total",
                "Tests that did not pass.",
                load(&self.tests_failed),
            ),
            (
                "drover_cache_refresh_changes_total",
                "Cache refreshes that installed a new snapshot.",
                g.refresh_changes,
            ),
            (
                "drover_cache_refresh_failures_total",
                "Cache refreshes that failed.",
                g.refresh_failures,
            ),
        ];
        for (name, help, value) in counters {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}");
        }
        out.push_str("# HELP drover_rule_evaluations_total Rule evaluations by status.\n# TYPE drover_rule_evaluations_total counter\n");
        for (i, status) in ["pass", "fail", "error"].iter().enumerate() {
            let _ = writeln!(
                out,
                "drover_rule_evaluations_total{{status=\"{status}\"}} {}",
                load(&self.rule_evaluations[i])
            );
        }
        out.push_str("# HELP drover_check_duration_seconds Check execution time (queueing + evaluation).\n# TYPE drover_check_duration_seconds histogram\n");
        for (m, mode) in ["sync", "async"].iter().enumerate() {
            self.durations[m].render(
                &mut out,
                "drover_check_duration_seconds",
                &format!("mode=\"{mode}\","),
            );
        }
        let gauges = [
            (
                "drover_rules_version",
                "Version of the active rule snapshot.",
                g.rules_version as f64,
            ),
            (
                "drover_rules_loaded",
                "Rules in the active snapshot.",
                g.rules as f64,
            ),
            (
                "drover_rules_invalid",
                "Rules that failed to compile.",
                g.invalid_rules as f64,
            ),
            (
                "drover_rule_groups_loaded",
                "Rule groups in the active snapshot.",
                g.groups as f64,
            ),
            (
                "drover_worker_threads",
                "Size of the evaluation thread pool.",
                g.pool_threads as f64,
            ),
            (
                "drover_worker_busy",
                "Worker threads currently evaluating.",
                g.pool_busy as f64,
            ),
            (
                "drover_worker_queue_depth",
                "Tasks waiting for a worker thread.",
                g.pool_queued as f64,
            ),
            (
                "drover_checks_in_flight",
                "Admitted checks (pending or running).",
                g.checks_admitted as f64,
            ),
            (
                "drover_checks_capacity",
                "Maximum admitted checks (max_queued_checks).",
                g.checks_capacity as f64,
            ),
        ];
        for (name, help, value) in gauges {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}");
        }
        out
    }
}
