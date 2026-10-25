use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::pool::WorkerPool;
use super::run::{CheckJob, RunError, run};
use super::*;
use crate::lang::LanguageRegistry;
use crate::model::{ConfigEntry, ConfigScope, Rule, RuleGroup};

pub(crate) fn rule(id: &str, language: &str, source: &str) -> Rule {
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

fn config(key: &str, value: Json, scope: ConfigScope) -> ConfigEntry {
    ConfigEntry {
        key: key.into(),
        value,
        scope,
        description: String::new(),
    }
}

fn group(name: &str, rules: &[&str]) -> RuleGroup {
    RuleGroup {
        name: name.into(),
        description: String::new(),
        rules: rules.iter().map(|s| s.to_string()).collect(),
    }
}

fn sample() -> Snapshot {
    let mut adult = rule("adult", "cel", "record.age >= config.min_age");
    adult.message = Some("must be an adult".into());
    Snapshot::build(
        1,
        Definitions {
            rules: vec![
                adult,
                rule(
                    "email",
                    "starlark",
                    "def check(record, config):\n    if '@' not in record.get('email', ''):\n        return {'pass': False, 'message': 'invalid email'}\n    return True\n",
                ),
                rule("country", "cel", "record.country in config.countries"),
            ],
            groups: vec![group("kyc", &["adult", "country"]), group("contact", &["email", "ghost"])],
            config: vec![
                config("min_age", json!(18), ConfigScope::Rule),
                config("countries", json!(["IT", "FR"]), ConfigScope::Rule),
            ],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap()
}

fn eval_all(s: &Snapshot, plan: &Plan, records: &[Json], report: ReportLevel) -> Vec<RecordResult> {
    let mut stats = LocalStats::new(s.rules.len());
    let limits = Limits::from_settings(&s.settings, None);
    records
        .iter()
        .enumerate()
        .map(|(i, r)| s.evaluate_record(plan, r, i, &limits, report, &mut stats))
        .collect()
}

fn statuses(r: &RecordResult) -> Vec<(String, RuleStatus)> {
    r.rules.iter().map(|o| (o.rule.to_string(), o.status)).collect()
}

#[test]
fn all_rules_across_languages_and_records() {
    let s = sample();
    let sel = s.select(None).unwrap();
    assert_eq!(sel.selection.rule_count, 3);
    let results = eval_all(
        &s,
        &sel.plan,
        &[
            json!({"age": 30, "email": "a@b.c", "country": "IT"}),
            json!({"age": 12, "email": "nope", "country": "IT"}),
        ],
        ReportLevel::All,
    );
    assert_eq!(results[0].outcome, Outcome::Valid);
    assert_eq!(results[1].outcome, Outcome::Invalid);
    // Report order is rule id order regardless of language.
    assert_eq!(
        statuses(&results[1]),
        vec![
            ("adult".into(), RuleStatus::Fail),
            ("country".into(), RuleStatus::Pass),
            ("email".into(), RuleStatus::Fail)
        ]
    );
    assert_eq!(results[1].rules[0].message.as_deref(), Some("must be an adult"));
    assert_eq!(results[1].rules[2].message.as_deref(), Some("invalid email"));
}

#[test]
fn group_selection_and_dangling_members() {
    let s = sample();
    let sel = s.select(Some("kyc")).unwrap();
    assert_eq!(sel.selection.group.as_deref(), Some("kyc"));
    let r = eval_all(
        &s,
        &sel.plan,
        &[json!({"age": 30, "country": "DE"})],
        ReportLevel::All,
    );
    assert_eq!(
        statuses(&r[0]),
        vec![
            ("adult".into(), RuleStatus::Pass),
            ("country".into(), RuleStatus::Fail)
        ]
    );
    assert_eq!(s.groups["contact"].missing_rules, vec!["ghost".to_string()]);
    assert!(s.diagnostics.iter().any(|d| d.contains("ghost")));
}

#[test]
fn unknown_group_strict_and_fallback() {
    let s = sample();
    assert!(matches!(s.select(Some("nope")), Err(crate::Error::UnknownRuleGroup(g)) if g == "nope"));

    let s = Snapshot::build(
        2,
        Definitions {
            rules: vec![rule("t", "cel", "true")],
            groups: vec![],
            config: vec![config(
                "unknown_group_mode",
                json!("fallback"),
                ConfigScope::System,
            )],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap();
    let sel = s.select(Some("nope")).unwrap();
    assert!(sel.selection.fallback);
    assert_eq!(sel.selection.requested_group.as_deref(), Some("nope"));
    assert_eq!(sel.selection.group, None);
    assert_eq!(sel.plan.len(), 1);
    assert_eq!(sel.diagnostics.len(), 1);
}

#[test]
fn rules_only_see_rule_scoped_config() {
    let s = Snapshot::build(
        1,
        Definitions {
            rules: vec![
                rule("sees_rule", "cel", "config.limit == 5"),
                rule("no_system", "cel", "!has(config.worker_threads)"),
                rule("star", "starlark", "def check(record, config):\n    return config['limit'] == 5 and 'worker_threads' not in config\n"),
            ],
            groups: vec![],
            config: vec![
                config("limit", json!(5), ConfigScope::Rule),
                config("worker_threads", json!(2), ConfigScope::System),
            ],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap();
    let r = eval_all(&s, &s.all, &[json!({})], ReportLevel::All);
    assert_eq!(r[0].outcome, Outcome::Valid, "{:?}", r[0]);
    assert_eq!(s.settings.worker_threads, 2);
}

#[test]
fn invalid_rules_report_compile_errors() {
    let s = Snapshot::build(
        1,
        Definitions {
            rules: vec![
                rule("broken", "cel", "record.age >="),
                rule("lua", "lua", "return true"),
                rule("ok", "cel", "record.x == 1"),
                rule("boom", "cel", "record.missing > 1"),
            ],
            groups: vec![],
            config: vec![],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap();
    assert_eq!(s.invalid_rules().count(), 2);
    let r = eval_all(&s, &s.all, &[json!({"x": 1})], ReportLevel::All);
    assert_eq!(r[0].outcome, Outcome::Error);
    let errors: Vec<_> = r[0]
        .rules
        .iter()
        .filter_map(|o| o.error.as_ref().map(|e| (o.rule.to_string(), e.kind)))
        .collect();
    assert_eq!(
        errors,
        vec![
            ("boom".into(), RuleErrorKind::Runtime),
            ("broken".into(), RuleErrorKind::Compile),
            ("lua".into(), RuleErrorKind::Compile),
        ]
    );
}

#[test]
fn failures_take_priority_over_errors() {
    let s = Snapshot::build(
        1,
        Definitions {
            rules: vec![
                rule("fails", "cel", "false"),
                rule("errors", "cel", "record.nope"),
            ],
            groups: vec![],
            config: vec![],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap();
    let r = eval_all(&s, &s.all, &[json!({})], ReportLevel::Failures);
    assert_eq!(r[0].outcome, Outcome::Invalid);
    assert_eq!(r[0].rules.len(), 2);
}

#[test]
fn disabled_rules_are_not_selected_but_can_be_planned() {
    let mut off = rule("off", "cel", "false");
    off.enabled = false;
    let s = Snapshot::build(
        1,
        Definitions {
            rules: vec![off, rule("on", "cel", "true")],
            groups: vec![group("g", &["off", "on"])],
            config: vec![],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    )
    .unwrap();
    assert_eq!(s.all.len(), 1);
    assert_eq!(s.groups["g"].plan.len(), 1);
    let plan = s.plan_for_rule("off").unwrap();
    let r = eval_all(&s, &plan, &[json!({})], ReportLevel::All);
    assert_eq!(r[0].outcome, Outcome::Invalid);
}

#[test]
fn report_levels_filter_rule_outcomes() {
    let s = sample();
    let rec = [json!({"age": 12, "email": "a@b", "country": "IT"})];
    assert_eq!(eval_all(&s, &s.all, &rec, ReportLevel::All)[0].rules.len(), 3);
    let f = eval_all(&s, &s.all, &rec, ReportLevel::Failures);
    assert_eq!(statuses(&f[0]), vec![("adult".into(), RuleStatus::Fail)]);
    assert!(
        eval_all(&s, &s.all, &rec, ReportLevel::Summary)[0]
            .rules
            .is_empty()
    );
}

#[test]
fn rebuild_reuses_unchanged_programs_and_stats() {
    let first = sample();
    first.rules[0]
        .stats
        .evaluations
        .fetch_add(7, std::sync::atomic::Ordering::Relaxed);
    let second = Snapshot::build(
        2,
        Definitions {
            rules: vec![
                rule("adult", "cel", "record.age >= config.min_age"),
                rule("country", "cel", "true"),
            ],
            groups: vec![],
            config: vec![config("min_age", json!(21), ConfigScope::Rule)],
        },
        first.languages.clone(),
        Some(&first),
    )
    .unwrap();
    let (a, b) = (first.rule("adult").unwrap(), second.rule("adult").unwrap());
    assert!(Arc::ptr_eq(
        a.program.as_ref().unwrap(),
        b.program.as_ref().unwrap()
    ));
    assert_eq!(b.stats.view().evaluations, 7);
    let (a, b) = (first.rule("country").unwrap(), second.rule("country").unwrap());
    assert!(!Arc::ptr_eq(
        a.program.as_ref().unwrap(),
        b.program.as_ref().unwrap()
    ));
    // New configuration is visible to the reused program.
    let r = eval_all(&second, &second.all, &[json!({"age": 19})], ReportLevel::All);
    assert_eq!(r[0].outcome, Outcome::Invalid);
}

#[test]
fn invalid_system_settings_reject_the_snapshot() {
    let e = Snapshot::build(
        1,
        Definitions {
            rules: vec![],
            groups: vec![],
            config: vec![config("worker_threads", json!(-1), ConfigScope::System)],
        },
        Arc::new(LanguageRegistry::builtin()),
        None,
    );
    assert!(e.is_err());
}

#[tokio::test]
async fn parallel_run_preserves_order_and_counts() {
    let s = Arc::new(sample());
    let pool = WorkerPool::new(4);
    let records: Vec<Json> = (0..1000)
        .map(|i| json!({"age": i % 40, "email": if i % 3 == 0 { "x" } else { "a@b" }, "country": "IT"}))
        .collect();
    let out = run(
        &pool,
        CheckJob {
            snapshot: s.clone(),
            plan: s.all.clone(),
            records: records.clone(),
            report: ReportLevel::Failures,
            deadline: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(out.results.len(), 1000);
    assert!(out.results.iter().enumerate().all(|(i, r)| r.index == i));
    let expected_valid = records
        .iter()
        .filter(|r| r["age"].as_i64().unwrap() >= 18 && r["email"] != "x")
        .count();
    assert_eq!(out.summary.valid, expected_valid);
    assert_eq!(out.summary.records, 1000);
    assert_eq!(
        out.summary.valid + out.summary.invalid + out.summary.errored,
        1000
    );
    assert_eq!(out.summary.rule_evaluations, 3000);
    assert_eq!(s.rule("adult").unwrap().stats.view().evaluations, 1000);

    let out = run(
        &pool,
        CheckJob {
            snapshot: s.clone(),
            plan: s.all.clone(),
            records,
            report: ReportLevel::Summary,
            deadline: None,
        },
    )
    .await
    .unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.summary.valid, expected_valid);
}

#[tokio::test]
async fn check_deadline_fails_the_run() {
    let s = Arc::new(
        Snapshot::build(
            1,
            Definitions {
                rules: vec![rule(
                    "slow",
                    "starlark",
                    "def check(record, config):\n    n = 0\n    for i in range(100000):\n        n += i\n    return True\n",
                )],
                groups: vec![],
                config: vec![],
            },
            Arc::new(LanguageRegistry::builtin()),
            None,
        )
        .unwrap(),
    );
    let pool = WorkerPool::new(2);
    let started = Instant::now();
    let r = run(
        &pool,
        CheckJob {
            snapshot: s.clone(),
            plan: s.all.clone(),
            records: vec![json!({}); 10_000],
            report: ReportLevel::All,
            deadline: Some(Instant::now() + Duration::from_millis(100)),
        },
    )
    .await;
    assert_eq!(r.unwrap_err(), RunError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
}
