//! Rule engine: rule selection, evaluation and result aggregation.
//!
//! * **Selection** — a [`Snapshot`] holds compiled rules and one precomputed
//!   [`Plan`] per group plus one for all enabled rules. Selecting rules for a
//!   check is a map lookup.
//! * **Evaluation** — [`Snapshot::evaluate_record`] runs a plan against one
//!   record, delegating to language adapters per language partition.
//! * **Aggregation** — rule outcomes are folded into a [`RecordResult`] and
//!   [`Summary`]; [`run`] executes whole checks on the bounded worker pool.

pub mod pool;
pub mod run;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as Json;

use crate::lang::{LanguageRegistry, Limits, PreparedConfig, Program};
use crate::model::{
    ConfigEntry, ConfigScope, JsonMap, Outcome, RecordResult, ReportLevel, Rule, RuleErrorInfo,
    RuleErrorKind, RuleGroup, RuleOutcome, RuleStatus, Summary,
};
use crate::settings::{Settings, UnknownGroupMode};

/// Cumulative per-rule statistics (kept across snapshots for unchanged rules).
#[derive(Debug, Default)]
pub struct RuleStats {
    pub evaluations: AtomicU64,
    pub failures: AtomicU64,
    pub errors: AtomicU64,
    pub nanos: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleStatsView {
    pub evaluations: u64,
    pub failures: u64,
    pub errors: u64,
    pub mean_micros: f64,
}

impl RuleStats {
    pub fn view(&self) -> RuleStatsView {
        let evaluations = self.evaluations.load(Ordering::Relaxed);
        let nanos = self.nanos.load(Ordering::Relaxed);
        RuleStatsView {
            evaluations,
            failures: self.failures.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            mean_micros: if evaluations == 0 {
                0.0
            } else {
                nanos as f64 / evaluations as f64 / 1000.0
            },
        }
    }
}

pub struct CompiledRule {
    pub def: Rule,
    pub id: Arc<str>,
    pub language: Option<usize>,
    /// The compiled program, or the compile error reported on evaluation.
    pub program: Result<Program, Arc<str>>,
    pub default_message: Option<Arc<str>>,
    pub stats: Arc<RuleStats>,
}

impl CompiledRule {
    pub fn compile_error(&self) -> Option<&str> {
        self.program.as_ref().err().map(|e| &**e)
    }
}

/// Rules of one language within a plan.
struct Partition {
    language: usize,
    /// Positions in `Plan::rules` of the partition's rules.
    positions: Vec<usize>,
    programs: Vec<Program>,
}

/// A precomputed selection of rules, grouped by language for evaluation.
pub struct Plan {
    /// Indices into `Snapshot::rules`, in evaluation/report order.
    pub rules: Vec<usize>,
    partitions: Vec<Partition>,
    /// Rules that cannot run (compile error, unknown language).
    invalid: Vec<(usize, RuleErrorInfo)>,
}

impl Plan {
    fn build(rules: &[CompiledRule], selected: Vec<usize>) -> Plan {
        let mut partitions: Vec<Partition> = Vec::new();
        let mut invalid = Vec::new();
        for (pos, &idx) in selected.iter().enumerate() {
            let rule = &rules[idx];
            match (&rule.program, rule.language) {
                (Ok(program), Some(language)) => {
                    let part = match partitions.iter_mut().position(|p| p.language == language) {
                        Some(i) => &mut partitions[i],
                        None => {
                            partitions.push(Partition {
                                language,
                                positions: Vec::new(),
                                programs: Vec::new(),
                            });
                            partitions.last_mut().unwrap()
                        }
                    };
                    part.positions.push(pos);
                    part.programs.push(program.clone());
                }
                (Err(message), _) => {
                    invalid.push((
                        pos,
                        crate::lang::error(RuleErrorKind::Compile, message.to_string()),
                    ));
                }
                (Ok(_), None) => invalid.push((
                    pos,
                    crate::lang::error(
                        RuleErrorKind::Compile,
                        format!("unknown rule language '{}'", rule.def.language),
                    ),
                )),
            }
        }
        Plan {
            rules: selected,
            partitions,
            invalid,
        }
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

pub struct GroupEntry {
    pub group: RuleGroup,
    pub plan: Arc<Plan>,
    /// Member ids that do not match any rule.
    pub missing_rules: Vec<String>,
}

/// Rules selected for a check.
#[derive(Clone)]
pub struct Selected {
    pub plan: Arc<Plan>,
    pub selection: crate::model::Selection,
    pub diagnostics: Vec<String>,
}

/// An immutable, consistent view of rules, groups and configuration.
///
/// Snapshots are replaced atomically by the cache; a check holds an `Arc`
/// to the snapshot it started with for its whole duration.
pub struct Snapshot {
    pub version: u64,
    pub loaded_at: DateTime<Utc>,
    pub rules: Vec<CompiledRule>,
    pub by_id: HashMap<String, usize>,
    pub groups: BTreeMap<String, GroupEntry>,
    pub all: Arc<Plan>,
    /// The `rule`-scoped configuration exposed to rules.
    pub rule_config: JsonMap,
    pub settings: Arc<Settings>,
    pub languages: Arc<LanguageRegistry>,
    lang_configs: Vec<PreparedConfig>,
    /// Problems found while building the snapshot (invalid rules, dangling members).
    pub diagnostics: Vec<String>,
}

/// Inputs for building a snapshot.
pub struct Definitions {
    pub rules: Vec<Rule>,
    pub groups: Vec<RuleGroup>,
    pub config: Vec<ConfigEntry>,
}

impl Snapshot {
    /// Compiles definitions into a snapshot. Programs and statistics of rules
    /// whose language and source are unchanged are reused from `previous`.
    pub fn build(
        version: u64,
        defs: Definitions,
        languages: Arc<LanguageRegistry>,
        previous: Option<&Snapshot>,
    ) -> Result<Snapshot, String> {
        let (settings, mut diagnostics) = Settings::from_entries(&defs.config)?;
        let limits = Limits::from_settings(&settings, None);

        let rule_config: JsonMap = defs
            .config
            .iter()
            .filter(|e| e.scope == ConfigScope::Rule)
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect();
        let mut lang_configs = Vec::with_capacity(languages.len());
        for i in 0..languages.len() {
            lang_configs.push(
                languages
                    .get(i)
                    .prepare_config(&rule_config)
                    .map_err(|e| format!("cannot prepare rule configuration: {e}"))?,
            );
        }
        let mut rule_defs = defs.rules;
        rule_defs.sort_by(|a, b| a.id.cmp(&b.id));
        rule_defs.dedup_by(|a, b| a.id == b.id);
        let mut rules = Vec::with_capacity(rule_defs.len());
        for def in rule_defs {
            let language = languages.index_of(&def.language);
            let reusable = previous
                .and_then(|p| p.rule(&def.id))
                .filter(|old| old.def.language == def.language && old.def.source == def.source);
            let stats = reusable.map(|old| old.stats.clone()).unwrap_or_default();
            let program = match (reusable, language) {
                // Programs do not depend on configuration (it is passed per
                // evaluation), so unchanged rules keep their compiled form.
                // Failed compilations are retried: limits may have changed.
                (Some(old), _) if old.program.is_ok() => old.program.clone(),
                (_, Some(lang)) => languages
                    .get(lang)
                    .compile(&def.id, &def.source, &limits)
                    .map_err(Arc::from),
                (_, None) => Err(Arc::from(format!(
                    "unknown rule language '{}' (available: {})",
                    def.language,
                    languages.ids().join(", ")
                ))),
            };
            if let Err(e) = &program {
                diagnostics.push(format!("rule '{}' is invalid: {e}", def.id));
            }
            rules.push(CompiledRule {
                id: Arc::from(def.id.as_str()),
                default_message: def.message.as_deref().map(Arc::from),
                language,
                program,
                stats,
                def,
            });
        }
        let by_id: HashMap<String, usize> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| (r.def.id.clone(), i))
            .collect();

        let all = Arc::new(Plan::build(
            &rules,
            (0..rules.len()).filter(|&i| rules[i].def.enabled).collect(),
        ));
        let mut groups = BTreeMap::new();
        for group in defs.groups {
            let mut members = Vec::new();
            let mut missing = Vec::new();
            for id in &group.rules {
                match by_id.get(id) {
                    Some(&i) if !members.contains(&i) => members.push(i),
                    Some(_) => {}
                    None => missing.push(id.clone()),
                }
            }
            if !missing.is_empty() {
                diagnostics.push(format!(
                    "group '{}' references unknown rules: {}",
                    group.name,
                    missing.join(", ")
                ));
            }
            members.retain(|&i| rules[i].def.enabled);
            members.sort_unstable();
            let plan = Arc::new(Plan::build(&rules, members));
            groups.insert(
                group.name.clone(),
                GroupEntry {
                    group,
                    plan,
                    missing_rules: missing,
                },
            );
        }

        Ok(Snapshot {
            version,
            loaded_at: Utc::now(),
            rules,
            by_id,
            groups,
            all,
            rule_config,
            settings: Arc::new(settings),
            languages,
            lang_configs,
            diagnostics,
        })
    }

    pub fn rule(&self, id: &str) -> Option<&CompiledRule> {
        self.by_id.get(id).map(|&i| &self.rules[i])
    }

    pub fn invalid_rules(&self) -> impl Iterator<Item = &CompiledRule> {
        self.rules.iter().filter(|r| r.program.is_err())
    }

    /// Selects the rules of a check.
    pub fn select(&self, group: Option<&str>) -> Result<Selected, crate::Error> {
        let make = |plan: &Arc<Plan>, applied: Option<&str>, fallback: bool| crate::model::Selection {
            requested_group: group.map(str::to_owned),
            group: applied.map(str::to_owned),
            fallback,
            rule_count: plan.len(),
            rules_version: self.version,
        };
        match group {
            None => Ok(Selected {
                selection: make(&self.all, None, false),
                plan: self.all.clone(),
                diagnostics: Vec::new(),
            }),
            Some(name) => match self.groups.get(name) {
                Some(entry) => Ok(Selected {
                    selection: make(&entry.plan, Some(name), false),
                    plan: entry.plan.clone(),
                    diagnostics: Vec::new(),
                }),
                None => match self.settings.unknown_group_mode {
                    UnknownGroupMode::Strict => Err(crate::Error::UnknownRuleGroup(name.to_string())),
                    UnknownGroupMode::Fallback => Ok(Selected {
                        selection: make(&self.all, None, true),
                        plan: self.all.clone(),
                        diagnostics: vec![format!(
                            "rule group '{name}' does not exist; all rules were executed (fallback mode)"
                        )],
                    }),
                },
            },
        }
    }

    /// Evaluates a program that is not part of the snapshot (rule drafts in
    /// the administration interface) with the snapshot's configuration.
    pub fn evaluate_program(
        &self,
        language: usize,
        program: &Program,
        record: &Json,
        limits: &Limits,
    ) -> crate::lang::EvalResult {
        let mut out = None;
        self.languages.get(language).evaluate(
            record,
            &self.lang_configs[language],
            std::slice::from_ref(program),
            limits,
            &mut |_, r| out = Some(r),
        );
        out.unwrap_or_else(|| Err(crate::lang::error(RuleErrorKind::Internal, "no result")))
    }

    /// A plan for a single rule, including disabled ones (used by tests).
    pub fn plan_for_rule(&self, id: &str) -> Option<Plan> {
        self.by_id.get(id).map(|&i| Plan::build(&self.rules, vec![i]))
    }

    /// Evaluates a plan against one record.
    pub fn evaluate_record(
        &self,
        plan: &Plan,
        record: &Json,
        index: usize,
        limits: &Limits,
        report: ReportLevel,
        stats: &mut LocalStats,
    ) -> RecordResult {
        let mut kept: Vec<(usize, RuleOutcome)> = Vec::new();
        let mut any_fail = false;
        let mut any_error = false;
        let mut record_stat =
            |pos: usize, status: RuleStatus, outcome: &mut dyn FnMut() -> RuleOutcome, nanos: u64| {
                let rule_idx = plan.rules[pos];
                stats.record(rule_idx, status, nanos);
                match status {
                    RuleStatus::Pass => {}
                    RuleStatus::Fail => any_fail = true,
                    RuleStatus::Error => any_error = true,
                }
                if report.keeps(status) {
                    kept.push((pos, outcome()));
                }
            };

        for part in &plan.partitions {
            let adapter = self.languages.get(part.language);
            let config = &self.lang_configs[part.language];
            let mut last = Instant::now();
            adapter.evaluate(record, config, &part.programs, limits, &mut |i, result| {
                let now = Instant::now();
                let nanos = now.duration_since(last).as_nanos() as u64;
                last = now;
                let pos = part.positions[i];
                let rule = &self.rules[plan.rules[pos]];
                let status = match &result {
                    Ok(v) if v.pass => RuleStatus::Pass,
                    Ok(_) => RuleStatus::Fail,
                    Err(_) => RuleStatus::Error,
                };
                let mut result = Some(result);
                record_stat(
                    pos,
                    status,
                    &mut || to_outcome(rule, result.take().unwrap()),
                    nanos,
                );
            });
        }
        for (pos, err) in &plan.invalid {
            let rule = &self.rules[plan.rules[*pos]];
            record_stat(
                *pos,
                RuleStatus::Error,
                &mut || RuleOutcome {
                    rule: rule.id.clone(),
                    status: RuleStatus::Error,
                    message: None,
                    error: Some(err.clone()),
                },
                0,
            );
        }
        if plan.partitions.len() > 1 || !plan.invalid.is_empty() {
            kept.sort_unstable_by_key(|(pos, _)| *pos);
        }
        RecordResult {
            index,
            outcome: Outcome::from_flags(any_fail, any_error),
            rules: kept.into_iter().map(|(_, o)| o).collect(),
        }
    }
}

fn to_outcome(rule: &CompiledRule, result: crate::lang::EvalResult) -> RuleOutcome {
    match result {
        Ok(v) => RuleOutcome {
            rule: rule.id.clone(),
            status: if v.pass {
                RuleStatus::Pass
            } else {
                RuleStatus::Fail
            },
            message: if v.pass {
                None
            } else {
                v.message.map(Arc::from).or_else(|| rule.default_message.clone())
            },
            error: None,
        },
        Err(err) => RuleOutcome {
            rule: rule.id.clone(),
            status: RuleStatus::Error,
            message: None,
            error: Some(err),
        },
    }
}

/// Statistics accumulated by one worker and flushed once per chunk, so
/// workers do not contend on shared counters per evaluation.
pub struct LocalStats {
    per_rule: Vec<[u64; 4]>,
    pub summary: Summary,
}

impl LocalStats {
    pub fn new(rule_count: usize) -> Self {
        LocalStats {
            per_rule: vec![[0; 4]; rule_count],
            summary: Summary::default(),
        }
    }

    fn record(&mut self, rule_idx: usize, status: RuleStatus, nanos: u64) {
        let s = &mut self.per_rule[rule_idx];
        s[0] += 1;
        s[3] += nanos;
        self.summary.rule_evaluations += 1;
        match status {
            RuleStatus::Pass => {}
            RuleStatus::Fail => {
                s[1] += 1;
                self.summary.rule_failures += 1;
            }
            RuleStatus::Error => {
                s[2] += 1;
                self.summary.rule_errors += 1;
            }
        }
    }

    pub fn count_record(&mut self, outcome: Outcome) {
        self.summary.records += 1;
        match outcome {
            Outcome::Valid => self.summary.valid += 1,
            Outcome::Invalid => self.summary.invalid += 1,
            Outcome::Error => self.summary.errored += 1,
        }
    }

    /// Adds the per-rule counters to the snapshot's shared statistics.
    pub fn flush(&mut self, snapshot: &Snapshot) {
        for (idx, s) in self.per_rule.iter_mut().enumerate() {
            if s[0] == 0 {
                continue;
            }
            let stats = &snapshot.rules[idx].stats;
            stats.evaluations.fetch_add(s[0], Ordering::Relaxed);
            stats.failures.fetch_add(s[1], Ordering::Relaxed);
            stats.errors.fetch_add(s[2], Ordering::Relaxed);
            stats.nanos.fetch_add(s[3], Ordering::Relaxed);
            *s = [0; 4];
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
