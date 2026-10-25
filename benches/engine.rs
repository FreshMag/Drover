//! Rule evaluation benchmarks (`cargo bench`).
//!
//! A dependency-free harness: each scenario evaluates a batch of records
//! several times and reports the best throughput. Scenarios cover CEL,
//! Starlark and mixed rule sets, single-threaded (per-record cost) and on the
//! worker pool (parallel throughput).

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;
use std::time::{Duration, Instant};

use drover::engine::pool::WorkerPool;
use drover::engine::run::{CheckJob, run};
use drover::engine::{Definitions, LocalStats, Snapshot};
use drover::lang::{LanguageRegistry, Limits};
use drover::model::{ConfigEntry, ConfigScope, ReportLevel, Rule};
use serde_json::{Value as Json, json};

fn rule(id: String, language: &str, source: &str) -> Rule {
    Rule {
        id,
        language: language.into(),
        source: source.into(),
        description: String::new(),
        message: None,
        enabled: true,
        metadata: Default::default(),
    }
}

const CEL_RULES: &[&str] = &[
    "record.age >= config.min_age",
    "record.country in config.countries",
    r#"record.email.contains("@") && record.email.size() < 254"#,
    "record.items.all(i, i.qty > 0 && i.price >= 0.0)",
    r#"has(record.name) ? {"pass": size(record.name) > 1} : {"pass": false, "message": "name missing"}"#,
];

const STARLARK_RULES: &[&str] = &[
    "def check(record, config):\n    return record['age'] >= config['min_age']\n",
    "def check(record, config):\n    return record['country'] in config['countries']\n",
    "def check(record, config):\n    e = record.get('email', '')\n    return '@' in e and len(e) < 254\n",
    "def check(record, config):\n    for i in record['items']:\n        if i['qty'] <= 0 or i['price'] < 0:\n            return {'pass': False, 'message': 'bad line ' + i['sku']}\n    return True\n",
    "def check(record, config):\n    n = record.get('name')\n    if n == None:\n        return {'pass': False, 'message': 'name missing'}\n    return len(n) > 1\n",
];

fn snapshot(cel: usize, starlark: usize) -> Arc<Snapshot> {
    snapshot_from(cel, CEL_RULES, starlark, STARLARK_RULES)
}

fn snapshot_from(cel: usize, cel_rules: &[&str], starlark: usize, starlark_rules: &[&str]) -> Arc<Snapshot> {
    let mut rules = Vec::new();
    for i in 0..cel {
        rules.push(rule(format!("cel-{i:03}"), "cel", cel_rules[i % cel_rules.len()]));
    }
    for i in 0..starlark {
        rules.push(rule(
            format!("star-{i:03}"),
            "starlark",
            starlark_rules[i % starlark_rules.len()],
        ));
    }
    let config = vec![
        ConfigEntry {
            key: "min_age".into(),
            value: json!(18),
            scope: ConfigScope::Rule,
            description: String::new(),
        },
        ConfigEntry {
            key: "countries".into(),
            value: json!(["IT", "FR", "DE", "ES", "NL"]),
            scope: ConfigScope::Rule,
            description: String::new(),
        },
    ];
    Arc::new(
        Snapshot::build(
            1,
            Definitions {
                rules,
                groups: vec![],
                config,
            },
            Arc::new(LanguageRegistry::builtin()),
            None,
        )
        .expect("snapshot builds"),
    )
}

fn records(n: usize) -> Vec<Json> {
    (0..n)
        .map(|i| {
            let country = ["IT", "FR", "US", "DE"][i % 4];
            json!({
                "id": i,
                "name": format!("customer-{i}"),
                "age": 10 + (i % 60),
                "country": country,
                "email": format!("user{i}@example.com"),
                "items": [
                    {"sku": "A-1", "qty": 1 + (i % 3), "price": 9.5},
                    {"sku": "B-2", "qty": (i % 5), "price": 20.0},
                ],
            })
        })
        .collect()
}

fn report(name: &str, records: usize, rules: usize, best: Duration) {
    let secs = best.as_secs_f64();
    println!(
        "{name:<44} {:>10.0} records/s {:>9.2} µs/record {:>8.0} ns/rule-eval",
        records as f64 / secs,
        secs * 1e6 / records as f64,
        secs * 1e9 / (records * rules) as f64,
    );
}

fn single_thread(name: &str, cel: usize, starlark: usize, n: usize) {
    single_thread_on(name, snapshot(cel, starlark), n);
}

/// Measures direct evaluation on the calling thread.
fn single_thread_on(name: &str, s: Arc<Snapshot>, n: usize) {
    let (cel, starlark) = (s.rules.len(), 0);
    let batch = records(n);
    let limits = Limits::from_settings(&s.settings, None);
    let mut stats = LocalStats::new(s.rules.len());
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let started = Instant::now();
        for (i, r) in batch.iter().enumerate() {
            std::hint::black_box(s.evaluate_record(&s.all, r, i, &limits, ReportLevel::Failures, &mut stats));
        }
        best = best.min(started.elapsed());
    }
    report(name, n, cel + starlark, best);
}

fn parallel(
    rt: &tokio::runtime::Runtime,
    pool: &WorkerPool,
    name: &str,
    cel: usize,
    starlark: usize,
    n: usize,
) {
    let s = snapshot(cel, starlark);
    let batch = records(n);
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let job = CheckJob {
            snapshot: s.clone(),
            plan: s.all.clone(),
            records: batch.clone(),
            report: ReportLevel::Failures,
            deadline: None,
        };
        let started = Instant::now();
        let out = rt.block_on(run(pool, job)).expect("run succeeds");
        best = best.min(started.elapsed());
        std::hint::black_box(out);
    }
    report(
        &format!("{name} ({} threads)", pool.threads()),
        n,
        cel + starlark,
        best,
    );
}

fn main() {
    // `cargo test --benches` runs benches with `--bench`-less args; keep it quick.
    let quick = !std::env::args().any(|a| a == "--bench");
    let n = if quick { 200 } else { 10_000 };
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    println!("records per batch: {n}, cpus: {threads}\n");

    // Trivial rules isolate the per-record cost (record conversion, setup).
    single_thread_on(
        "single thread, 5 trivial CEL rules",
        snapshot_from(5, &["true"], 0, &[]),
        n,
    );
    single_thread_on(
        "single thread, 5 trivial Starlark rules",
        snapshot_from(0, &[], 5, &["def check(record, config):\n    return True\n"]),
        n,
    );
    single_thread("single thread, 5 CEL rules", 5, 0, n);
    single_thread("single thread, 5 Starlark rules", 0, 5, n);
    single_thread("single thread, 25 CEL + 25 Starlark rules", 25, 25, n / 4);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let pool = WorkerPool::new(threads);
    parallel(&rt, &pool, "pool, 5 CEL rules", 5, 0, n);
    parallel(&rt, &pool, "pool, 5 Starlark rules", 0, 5, n);
    parallel(&rt, &pool, "pool, 25 CEL + 25 Starlark rules", 25, 25, n);

    println!();
    for t in [1, 2, 4, threads] {
        let pool = WorkerPool::new(t);
        parallel(&rt, &pool, "scaling, 5 CEL rules", 5, 0, n);
        parallel(&rt, &pool, "scaling, 5 Starlark rules", 0, 5, n);
    }

    let started = Instant::now();
    let s = snapshot(250, 250);
    println!(
        "\ncompile 250 CEL + 250 Starlark rules: {:.1} ms ({} rules)",
        started.elapsed().as_secs_f64() * 1e3,
        s.rules.len()
    );
}
