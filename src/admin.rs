//! Administration service: validated changes to rules, groups, tests and
//! configuration, applied to the running instance.
//!
//! Every change follows the same sequence:
//!
//! 1. **validate** — identifiers, references, rule compilation, setting
//!    types (`invalid_definition`, `rule_compilation_failed`,
//!    `configuration_invalid`, HTTP 422); nothing is written on failure;
//! 2. **persist** — write to the category's data source
//!    (`data_source_error`, HTTP 503, if it fails: nothing was applied);
//! 3. **apply** — refresh the rule cache from the data sources
//!    (`apply_failed`, HTTP 500, if it fails: the change *is* saved and will
//!    be picked up by the next successful refresh).
//!
//! Operations spanning two data sources (a rule and its group memberships)
//! are not atomic across them; a failure after the first write is reported
//! as `apply_failed` naming what was saved.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::cache::{RefreshResult, RuleCache};
use crate::engine::RuleStatsView;
use crate::engine::pool::WorkerPool;
use crate::error::Error;
use crate::lang::{LanguageRegistry, Limits};
use crate::model::{
    ConfigEntry, ConfigScope, JsonMap, Outcome, Rule, RuleErrorInfo, RuleGroup, RuleStatus, TestCase,
    TestScope, validate_identifier,
};
use crate::settings::{self, ApplyMode};
use crate::store::Stores;
use crate::testing::{TestResult, TestRunReport, TestRunner, TestSelection, scope_plan};

/// Result of a successful change.
#[derive(Debug, Serialize)]
pub struct Applied<T> {
    pub data: T,
    /// Non-blocking remarks (e.g. tests referencing a deleted rule).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The change is saved but only takes effect after a restart.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub restart_required: bool,
    pub rules_version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleState {
    /// Loaded and selected by checks.
    Active,
    /// Loaded but not selected by checks.
    Disabled,
    /// Failed to compile.
    Invalid,
    /// Stored but not (yet) in the active snapshot.
    Pending,
}

#[derive(Debug, Serialize)]
pub struct RuleView {
    #[serde(flatten)]
    pub rule: Rule,
    pub groups: Vec<String>,
    pub state: RuleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<RuleStatsView>,
}

fn default_true() -> bool {
    true
}

/// Rule as submitted by the administration interface.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleInput {
    #[serde(default)]
    pub id: Option<String>,
    pub language: String,
    pub source: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub metadata: JsonMap,
    /// When present, the rule's group memberships are set to exactly these groups.
    #[serde(default)]
    pub groups: Option<Vec<String>>,
}

impl RuleInput {
    fn into_rule(self, id: String) -> (Rule, Option<Vec<String>>) {
        (
            Rule {
                id,
                language: self.language,
                source: self.source,
                description: self.description,
                message: self.message.filter(|m| !m.trim().is_empty()),
                enabled: self.enabled,
                metadata: self.metadata,
            },
            self.groups,
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftRequest {
    pub language: String,
    pub source: String,
    #[serde(default)]
    pub records: Vec<Json>,
}

#[derive(Debug, Serialize)]
pub struct DraftRecordResult {
    pub index: usize,
    pub status: RuleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RuleErrorInfo>,
}

#[derive(Debug, Serialize)]
pub struct DraftResponse {
    pub compiled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compile_error: Option<String>,
    pub results: Vec<DraftRecordResult>,
}

#[derive(Debug, Serialize)]
pub struct TestView {
    #[serde(flatten)]
    pub test: TestCase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<TestResult>,
}

#[derive(Debug, Serialize)]
pub struct ConfigView {
    #[serde(flatten)]
    pub entry: ConfigEntry,
    /// True for entries visible to rules (`scope: rule`).
    pub exposed_to_rules: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplyMode>,
}

pub struct AdminService {
    stores: Stores,
    cache: Arc<RuleCache>,
    languages: Arc<LanguageRegistry>,
    pool: Arc<WorkerPool>,
    tests: Arc<TestRunner>,
}

fn ds(category: &'static str) -> impl FnOnce(crate::store::StoreError) -> Error {
    Error::data_source(category)
}

fn not_found(kind: &'static str, id: &str) -> Error {
    Error::NotFound {
        kind,
        id: id.to_string(),
    }
}

/// Rejects identifiers that collide with fixed administration routes.
fn reserved(kind: &str, id: &str, word: &str) -> Result<(), Error> {
    if id == word {
        return Err(Error::invalid_definition(format!(
            "'{word}' is reserved and cannot be used as a {kind}"
        )));
    }
    Ok(())
}

fn path_matches(kind: &str, path_id: &str, body_id: Option<&str>) -> Result<(), Error> {
    match body_id {
        Some(body) if body != path_id => Err(Error::InvalidRequest(format!(
            "{kind} identifier in the body ('{body}') does not match the URL ('{path_id}'); identifiers cannot be renamed"
        ))),
        _ => Ok(()),
    }
}

impl AdminService {
    pub fn new(
        stores: Stores,
        cache: Arc<RuleCache>,
        languages: Arc<LanguageRegistry>,
        pool: Arc<WorkerPool>,
        tests: Arc<TestRunner>,
    ) -> Self {
        AdminService {
            stores,
            cache,
            languages,
            pool,
            tests,
        }
    }

    async fn apply(&self) -> Result<RefreshResult, Error> {
        self.cache
            .refresh(true)
            .await
            .map_err(|e| Error::ApplyFailed(e.to_string()))
    }

    fn applied<T>(&self, data: T, refresh: RefreshResult) -> Applied<T> {
        Applied {
            data,
            warnings: Vec::new(),
            restart_required: false,
            rules_version: refresh.version,
        }
    }

    pub async fn refresh(&self) -> Result<RefreshResult, Error> {
        self.cache.refresh(true).await
    }

    // --- rules -------------------------------------------------------------

    fn rule_view(&self, rule: Rule, groups: &[RuleGroup]) -> RuleView {
        let snapshot = self.cache.snapshot();
        let loaded = snapshot.rule(&rule.id).filter(|c| c.def == rule);
        let (state, compile_error, stats) = match loaded {
            None => (RuleState::Pending, None, None),
            Some(c) => {
                let state = match (&c.program, c.def.enabled) {
                    (Err(_), _) => RuleState::Invalid,
                    (Ok(_), true) => RuleState::Active,
                    (Ok(_), false) => RuleState::Disabled,
                };
                (state, c.compile_error().map(str::to_owned), Some(c.stats.view()))
            }
        };
        RuleView {
            groups: groups
                .iter()
                .filter(|g| g.rules.contains(&rule.id))
                .map(|g| g.name.clone())
                .collect(),
            rule,
            state,
            compile_error,
            stats,
        }
    }

    pub async fn list_rules(&self) -> Result<Vec<RuleView>, Error> {
        let (rules, groups) = tokio::try_join!(
            async { self.stores.rules.list().await.map_err(ds("rules")) },
            async { self.stores.groups.list().await.map_err(ds("groups")) },
        )?;
        Ok(rules.into_iter().map(|r| self.rule_view(r, &groups)).collect())
    }

    pub async fn get_rule(&self, id: &str) -> Result<RuleView, Error> {
        let rule = self
            .stores
            .rules
            .get(id)
            .await
            .map_err(ds("rules"))?
            .ok_or_else(|| not_found("rule", id))?;
        let groups = self.stores.groups.list().await.map_err(ds("groups"))?;
        Ok(self.rule_view(rule, &groups))
    }

    /// Validates a rule definition, compiling it on the worker pool.
    pub async fn validate_rule(&self, rule: &Rule) -> Result<(), Error> {
        validate_identifier("rule id", &rule.id).map_err(Error::invalid_definition)?;
        reserved("rule id", &rule.id, "evaluate")?;
        let language = self.languages.index_of(&rule.language).ok_or_else(|| {
            Error::invalid_definition(format!(
                "unknown rule language '{}' (available: {})",
                rule.language,
                self.languages.ids().join(", ")
            ))
        })?;
        let (languages, id, source) = (self.languages.clone(), rule.id.clone(), rule.source.clone());
        let limits = Limits::from_settings(&self.cache.snapshot().settings, None);
        self.pool
            .spawn(move || languages.get(language).compile(&id, &source, &limits).map(|_| ()))
            .await
            .map_err(|e| Error::Internal(e.to_string()))?
            .map_err(|message| Error::RuleCompilation {
                rule: rule.id.clone(),
                message,
            })
    }

    async fn check_groups_exist(&self, names: &[String]) -> Result<Vec<RuleGroup>, Error> {
        let groups = self.stores.groups.list().await.map_err(ds("groups"))?;
        let unknown: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| !groups.iter().any(|g| g.name == *n))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::invalid_definition(format!(
                "unknown rule groups: {}",
                unknown.join(", ")
            )));
        }
        Ok(groups)
    }

    /// Sets the groups containing `rule_id` to exactly `wanted`.
    async fn set_memberships(
        &self,
        rule_id: &str,
        wanted: &[String],
        groups: Vec<RuleGroup>,
    ) -> Result<(), Error> {
        for mut group in groups {
            let has = group.rules.iter().any(|r| r == rule_id);
            let want = wanted.contains(&group.name);
            if has == want {
                continue;
            }
            if want {
                group.rules.push(rule_id.to_string());
            } else {
                group.rules.retain(|r| r != rule_id);
            }
            self.stores.groups.update(&group).await.map_err(ds("groups"))?;
        }
        Ok(())
    }

    async fn save_rule(
        &self,
        rule: Rule,
        groups: Option<Vec<String>>,
        create: bool,
    ) -> Result<Applied<RuleView>, Error> {
        self.validate_rule(&rule).await?;
        let existing_groups = match &groups {
            Some(names) => Some(self.check_groups_exist(names).await?),
            None => None,
        };
        if create {
            self.stores.rules.create(&rule).await.map_err(ds("rules"))?;
        } else {
            self.stores.rules.update(&rule).await.map_err(ds("rules"))?;
        }
        if let (Some(names), Some(existing)) = (&groups, existing_groups) {
            self.set_memberships(&rule.id, names, existing)
                .await
                .map_err(|e| {
                    Error::ApplyFailed(format!(
                        "rule '{}' was saved, but updating its group memberships failed: {e}",
                        rule.id
                    ))
                })?;
        }
        let refresh = self.apply().await?;
        let id = rule.id.clone();
        let view = self.get_rule(&id).await?;
        Ok(self.applied(view, refresh))
    }

    pub async fn create_rule(&self, input: RuleInput) -> Result<Applied<RuleView>, Error> {
        let id = input
            .id
            .clone()
            .ok_or_else(|| Error::invalid_definition("rule id is required"))?;
        let (rule, groups) = input.into_rule(id);
        self.save_rule(rule, groups, true).await
    }

    pub async fn update_rule(&self, id: &str, input: RuleInput) -> Result<Applied<RuleView>, Error> {
        path_matches("rule", id, input.id.as_deref())?;
        let (rule, groups) = input.into_rule(id.to_string());
        self.save_rule(rule, groups, false).await
    }

    pub async fn delete_rule(&self, id: &str) -> Result<Applied<()>, Error> {
        let groups = self.stores.groups.list().await.map_err(ds("groups"))?;
        self.stores.rules.delete(id).await.map_err(ds("rules"))?;
        let mut warnings = Vec::new();
        let member_of: Vec<RuleGroup> = groups
            .into_iter()
            .filter(|g| g.rules.iter().any(|r| r == id))
            .collect();
        if !member_of.is_empty() {
            self.set_memberships(id, &[], member_of).await.map_err(|e| {
                Error::ApplyFailed(format!(
                    "rule '{id}' was deleted, but removing it from its groups failed: {e}"
                ))
            })?;
        }
        if let Ok(tests) = self.stores.tests.list().await {
            let referencing: Vec<String> = tests
                .into_iter()
                .filter(|t| {
                    t.scope == TestScope::Rule { rule: id.to_string() }
                        || t.expect.rules.iter().any(|r| r == id)
                })
                .map(|t| t.id)
                .collect();
            if !referencing.is_empty() {
                warnings.push(format!(
                    "these tests reference the deleted rule and will report errors: {}",
                    referencing.join(", ")
                ));
            }
        }
        let refresh = self.apply().await?;
        let mut applied = self.applied((), refresh);
        applied.warnings = warnings;
        Ok(applied)
    }

    /// Compiles a draft rule and evaluates it against sample records, without saving.
    pub async fn evaluate_draft(&self, draft: DraftRequest) -> Result<DraftResponse, Error> {
        let language = self.languages.index_of(&draft.language).ok_or_else(|| {
            Error::invalid_definition(format!("unknown rule language '{}'", draft.language))
        })?;
        let snapshot = self.cache.snapshot();
        self.pool
            .spawn(move || {
                let limits = Limits::from_settings(&snapshot.settings, None);
                let program = match snapshot
                    .languages
                    .get(language)
                    .compile("draft", &draft.source, &limits)
                {
                    Ok(p) => p,
                    Err(e) => {
                        return DraftResponse {
                            compiled: false,
                            compile_error: Some(e),
                            results: Vec::new(),
                        };
                    }
                };
                let results = draft
                    .records
                    .iter()
                    .enumerate()
                    .map(|(index, record)| {
                        match snapshot.evaluate_program(language, &program, record, &limits) {
                            Ok(v) => DraftRecordResult {
                                index,
                                status: if v.pass {
                                    RuleStatus::Pass
                                } else {
                                    RuleStatus::Fail
                                },
                                message: v.message,
                                error: None,
                            },
                            Err(e) => DraftRecordResult {
                                index,
                                status: RuleStatus::Error,
                                message: None,
                                error: Some(e),
                            },
                        }
                    })
                    .collect();
                DraftResponse {
                    compiled: true,
                    compile_error: None,
                    results,
                }
            })
            .await
            .map_err(|e| Error::Internal(e.to_string()))
    }

    // --- groups ------------------------------------------------------------

    pub async fn list_groups(&self) -> Result<Vec<RuleGroup>, Error> {
        self.stores.groups.list().await.map_err(ds("groups"))
    }

    pub async fn get_group(&self, name: &str) -> Result<RuleGroup, Error> {
        self.stores
            .groups
            .get(name)
            .await
            .map_err(ds("groups"))?
            .ok_or_else(|| not_found("rule group", name))
    }

    async fn validate_group(&self, group: &mut RuleGroup) -> Result<(), Error> {
        validate_identifier("rule group name", &group.name).map_err(Error::invalid_definition)?;
        let mut seen = BTreeSet::new();
        group.rules.retain(|r| seen.insert(r.clone()));
        let rules = self.stores.rules.list().await.map_err(ds("rules"))?;
        let unknown: Vec<&str> = group
            .rules
            .iter()
            .map(String::as_str)
            .filter(|id| !rules.iter().any(|r| r.id == *id))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::invalid_definition(format!(
                "unknown rules: {}",
                unknown.join(", ")
            )));
        }
        Ok(())
    }

    pub async fn create_group(&self, mut group: RuleGroup) -> Result<Applied<RuleGroup>, Error> {
        self.validate_group(&mut group).await?;
        self.stores.groups.create(&group).await.map_err(ds("groups"))?;
        let refresh = self.apply().await?;
        Ok(self.applied(group, refresh))
    }

    pub async fn update_group(&self, name: &str, mut group: RuleGroup) -> Result<Applied<RuleGroup>, Error> {
        path_matches("rule group", name, Some(&group.name))?;
        self.validate_group(&mut group).await?;
        self.stores.groups.update(&group).await.map_err(ds("groups"))?;
        let refresh = self.apply().await?;
        Ok(self.applied(group, refresh))
    }

    pub async fn delete_group(&self, name: &str) -> Result<Applied<()>, Error> {
        self.stores.groups.delete(name).await.map_err(ds("groups"))?;
        let refresh = self.apply().await?;
        Ok(self.applied((), refresh))
    }

    pub async fn add_member(&self, name: &str, rule_id: &str) -> Result<Applied<RuleGroup>, Error> {
        let mut group = self.get_group(name).await?;
        if !group.rules.iter().any(|r| r == rule_id) {
            group.rules.push(rule_id.to_string());
        }
        self.update_group(name, group).await
    }

    pub async fn remove_member(&self, name: &str, rule_id: &str) -> Result<Applied<RuleGroup>, Error> {
        let mut group = self.get_group(name).await?;
        if !group.rules.iter().any(|r| r == rule_id) {
            return Err(Error::NotFound {
                kind: "group member",
                id: format!("{name}/{rule_id}"),
            });
        }
        group.rules.retain(|r| r != rule_id);
        self.update_group(name, group).await
    }

    // --- tests -------------------------------------------------------------

    pub async fn list_tests(&self) -> Result<Vec<TestView>, Error> {
        let tests = self.stores.tests.list().await.map_err(ds("tests"))?;
        Ok(tests
            .into_iter()
            .map(|test| TestView {
                latest: self.tests.latest(&test.id),
                test,
            })
            .collect())
    }

    pub async fn get_test(&self, id: &str) -> Result<TestView, Error> {
        let test = self
            .stores
            .tests
            .get(id)
            .await
            .map_err(ds("tests"))?
            .ok_or_else(|| not_found("test", id))?;
        Ok(TestView {
            latest: self.tests.latest(id),
            test,
        })
    }

    fn validate_test(&self, test: &TestCase) -> Result<(), Error> {
        validate_identifier("test id", &test.id).map_err(Error::invalid_definition)?;
        reserved("test id", &test.id, "run")?;
        for tag in &test.tags {
            validate_identifier("tag", tag).map_err(Error::invalid_definition)?;
        }
        let expect = &test.expect;
        if expect.outcome == Outcome::Valid && !expect.rules.is_empty() {
            return Err(Error::invalid_definition(
                "expect.rules can only be used with outcome 'invalid' or 'error'",
            ));
        }
        if expect.exclusive && expect.rules.is_empty() {
            return Err(Error::invalid_definition(
                "expect.exclusive requires expect.rules",
            ));
        }
        let snapshot = self.cache.snapshot();
        let plan = scope_plan(&snapshot, &test.scope).map_err(Error::invalid_definition)?;
        let outside: Vec<&str> = expect
            .rules
            .iter()
            .map(String::as_str)
            .filter(|id| !plan.rules.iter().any(|&i| &*snapshot.rules[i].id == *id))
            .collect();
        if !outside.is_empty() {
            return Err(Error::invalid_definition(format!(
                "expect.rules are not evaluated by the test scope: {}",
                outside.join(", ")
            )));
        }
        Ok(())
    }

    pub async fn create_test(&self, test: TestCase) -> Result<Applied<TestCase>, Error> {
        self.validate_test(&test)?;
        self.stores.tests.create(&test).await.map_err(ds("tests"))?;
        Ok(self.applied(test, self.version()))
    }

    pub async fn update_test(&self, id: &str, test: TestCase) -> Result<Applied<TestCase>, Error> {
        path_matches("test", id, Some(&test.id))?;
        self.validate_test(&test)?;
        self.stores.tests.update(&test).await.map_err(ds("tests"))?;
        self.tests.forget(id);
        Ok(self.applied(test, self.version()))
    }

    pub async fn delete_test(&self, id: &str) -> Result<Applied<()>, Error> {
        self.stores.tests.delete(id).await.map_err(ds("tests"))?;
        self.tests.forget(id);
        Ok(self.applied((), self.version()))
    }

    pub async fn run_tests(&self, selection: &TestSelection) -> Result<TestRunReport, Error> {
        self.tests.run(selection).await
    }

    fn version(&self) -> RefreshResult {
        RefreshResult {
            changed: false,
            version: self.cache.snapshot().version,
        }
    }

    // --- configuration -----------------------------------------------------

    fn config_view(entry: ConfigEntry) -> ConfigView {
        let apply = match entry.scope {
            ConfigScope::System => settings::spec(&entry.key).map(|s| s.apply),
            ConfigScope::Rule => None,
        };
        ConfigView {
            exposed_to_rules: entry.scope == ConfigScope::Rule,
            apply,
            entry,
        }
    }

    pub async fn list_config(&self) -> Result<Vec<ConfigView>, Error> {
        let entries = self.stores.config.list().await.map_err(ds("config"))?;
        Ok(entries.into_iter().map(Self::config_view).collect())
    }

    pub async fn get_config(&self, key: &str) -> Result<ConfigView, Error> {
        self.stores
            .config
            .get(key)
            .await
            .map_err(ds("config"))?
            .map(Self::config_view)
            .ok_or_else(|| not_found("configuration entry", key))
    }

    fn restart_required(entry: &ConfigEntry) -> bool {
        entry.scope == ConfigScope::System
            && settings::spec(&entry.key).is_some_and(|s| s.apply == ApplyMode::Restart)
    }

    async fn save_config(&self, entry: ConfigEntry, create: bool) -> Result<Applied<ConfigView>, Error> {
        settings::validate_entry(&entry).map_err(Error::ConfigValidation)?;
        let previous = self.stores.config.get(&entry.key).await.map_err(ds("config"))?;
        if create {
            self.stores.config.create(&entry).await.map_err(ds("config"))?;
        } else {
            self.stores.config.update(&entry).await.map_err(ds("config"))?;
        }
        let refresh = self.apply().await?;
        let restart = Self::restart_required(&entry) || previous.as_ref().is_some_and(Self::restart_required);
        let mut applied = self.applied(Self::config_view(entry), refresh);
        applied.restart_required = restart;
        Ok(applied)
    }

    pub async fn create_config(&self, entry: ConfigEntry) -> Result<Applied<ConfigView>, Error> {
        self.save_config(entry, true).await
    }

    pub async fn update_config(&self, key: &str, entry: ConfigEntry) -> Result<Applied<ConfigView>, Error> {
        path_matches("configuration key", key, Some(&entry.key))?;
        self.save_config(entry, false).await
    }

    pub async fn delete_config(&self, key: &str) -> Result<Applied<()>, Error> {
        let previous = self.stores.config.get(key).await.map_err(ds("config"))?;
        self.stores.config.delete(key).await.map_err(ds("config"))?;
        let refresh = self.apply().await?;
        let mut applied = self.applied((), refresh);
        applied.restart_required = previous.as_ref().is_some_and(Self::restart_required);
        Ok(applied)
    }
}
