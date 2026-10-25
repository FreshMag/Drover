//! Rule language adapters.
//!
//! A language implements [`RuleLanguage`], a typed interface with its own
//! compiled program, prepared configuration and record representation. The
//! engine only sees the object-safe [`LanguageAdapter`], which a blanket
//! implementation derives from `RuleLanguage`; downcasting between the two is
//! confined to that implementation. Adding a language means implementing
//! `RuleLanguage` and registering it in [`LanguageRegistry::builtin`].
//!
//! # Result contract
//!
//! A rule returns either a boolean (`true` = pass) or a map with a boolean
//! `pass` field and an optional `message` string. Anything else is an
//! `invalid_result` error. Errors raised while evaluating are rule *errors*,
//! distinct from a `false` result, which is an ordinary validation failure.

pub mod cel;
pub mod starlark;

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value as Json;

use crate::model::{JsonMap, RuleErrorInfo, RuleErrorKind};
use crate::settings::Settings;

/// Interpreted result of a rule that evaluated successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub pass: bool,
    pub message: Option<String>,
}

pub type EvalResult = Result<Verdict, RuleErrorInfo>;

/// Execution limits applied to one rule evaluation.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Wall-clock limit per rule evaluation (languages that can be interrupted).
    pub rule_timeout: Option<Duration>,
    /// Deadline of the whole check; evaluation is interrupted past it.
    pub check_deadline: Option<Instant>,
    pub starlark_max_ticks: u64,
    pub starlark_max_heap_bytes: usize,
    pub starlark_max_callstack: usize,
}

impl Limits {
    pub fn from_settings(settings: &Settings, check_deadline: Option<Instant>) -> Self {
        Limits {
            rule_timeout: settings.rule_timeout,
            check_deadline,
            starlark_max_ticks: settings.starlark_max_ticks,
            starlark_max_heap_bytes: settings.starlark_max_heap_bytes,
            starlark_max_callstack: settings.starlark_max_callstack,
        }
    }

    /// Deadline for a rule evaluation starting now.
    pub fn rule_deadline(&self) -> Option<Instant> {
        let rule = self.rule_timeout.map(|t| Instant::now() + t);
        match (rule, self.check_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::from_settings(&Settings::default(), None)
    }
}

/// Description of a language for clients (admin UI editor).
#[derive(Debug, Clone, Serialize)]
pub struct LanguageInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Source used to pre-fill the editor for a new rule.
    pub template: &'static str,
    /// Short explanation of how rules are written in this language.
    pub help: &'static str,
}

/// Typed interface implemented by each rule language.
pub trait RuleLanguage: Send + Sync + 'static {
    /// Reusable compiled representation of a rule.
    type Program: Send + Sync + 'static;
    /// The `rule`-scoped configuration prepared for this language.
    type Config: Send + Sync + 'static;

    fn info(&self) -> LanguageInfo;

    /// Compiles a rule. Called when rules are loaded or refreshed, never per evaluation.
    fn compile(&self, rule_id: &str, source: &str, limits: &Limits) -> Result<Self::Program, String>;

    /// Converts the `rule`-scoped configuration once per cache snapshot.
    fn prepare_config(&self, config: &JsonMap) -> Result<Self::Config, String>;

    /// Evaluates `programs` against one record, calling `sink(i, result)` for
    /// the i-th program, in order. Receiving all programs of the language at
    /// once lets the adapter convert the record a single time.
    fn evaluate(
        &self,
        record: &Json,
        config: &Self::Config,
        programs: &mut dyn ExactSizeIterator<Item = &Self::Program>,
        limits: &Limits,
        sink: &mut dyn FnMut(usize, EvalResult),
    );
}

pub type Program = Arc<dyn Any + Send + Sync>;
pub type PreparedConfig = Arc<dyn Any + Send + Sync>;

/// Object-safe language interface used by the engine.
pub trait LanguageAdapter: Send + Sync {
    fn info(&self) -> LanguageInfo;
    fn compile(&self, rule_id: &str, source: &str, limits: &Limits) -> Result<Program, String>;
    fn prepare_config(&self, config: &JsonMap) -> Result<PreparedConfig, String>;
    /// `programs` must have been compiled by this adapter, `config` prepared by it.
    fn evaluate(
        &self,
        record: &Json,
        config: &PreparedConfig,
        programs: &[Program],
        limits: &Limits,
        sink: &mut dyn FnMut(usize, EvalResult),
    );
}

impl<L: RuleLanguage> LanguageAdapter for L {
    fn info(&self) -> LanguageInfo {
        RuleLanguage::info(self)
    }

    fn compile(&self, rule_id: &str, source: &str, limits: &Limits) -> Result<Program, String> {
        guard(|| RuleLanguage::compile(self, rule_id, source, limits).map(|p| Arc::new(p) as Program))
            .unwrap_or_else(|| Err("the language runtime panicked while compiling the rule".into()))
    }

    fn prepare_config(&self, config: &JsonMap) -> Result<PreparedConfig, String> {
        RuleLanguage::prepare_config(self, config).map(|c| Arc::new(c) as PreparedConfig)
    }

    fn evaluate(
        &self,
        record: &Json,
        config: &PreparedConfig,
        programs: &[Program],
        limits: &Limits,
        sink: &mut dyn FnMut(usize, EvalResult),
    ) {
        let config = config
            .downcast_ref::<L::Config>()
            .expect("configuration prepared by another language adapter");
        let mut iter = programs.iter().map(|p| {
            p.downcast_ref::<L::Program>()
                .expect("program compiled by another language adapter")
        });
        let mut done = 0usize;
        let outcome = guard(|| {
            RuleLanguage::evaluate(self, record, config, &mut iter, limits, &mut |i, r| {
                done = i + 1;
                sink(i, r)
            })
        });
        if outcome.is_none() {
            // A panic escaped the adapter: report it on every rule not yet reported.
            for i in done..programs.len() {
                sink(
                    i,
                    Err(error(RuleErrorKind::Internal, "the language runtime panicked")),
                );
            }
        }
    }
}

/// Runs `f`, converting a panic into `None`.
pub fn guard<R>(f: impl FnOnce() -> R) -> Option<R> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

pub fn error(kind: RuleErrorKind, message: impl Into<String>) -> RuleErrorInfo {
    RuleErrorInfo {
        kind,
        message: message.into(),
    }
}

/// Interprets a rule result expressed as JSON (shared by all adapters).
pub fn verdict_from_json(value: &Json) -> EvalResult {
    match value {
        Json::Bool(pass) => Ok(Verdict {
            pass: *pass,
            message: None,
        }),
        Json::Object(map) => {
            let pass = match map.get("pass") {
                Some(Json::Bool(b)) => *b,
                _ => {
                    return Err(error(
                        RuleErrorKind::InvalidResult,
                        "a map result must contain a boolean 'pass' field",
                    ));
                }
            };
            let message = match map.get("message") {
                None | Some(Json::Null) => None,
                Some(Json::String(s)) => Some(s.clone()),
                Some(other) => Some(other.to_string()),
            };
            Ok(Verdict { pass, message })
        }
        other => Err(error(
            RuleErrorKind::InvalidResult,
            format!(
                "a rule must return a boolean or a map with a boolean 'pass' field, got {}",
                json_type(other)
            ),
        )),
    }
}

pub fn json_type(value: &Json) -> &'static str {
    match value {
        Json::Null => "null",
        Json::Bool(_) => "bool",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "list",
        Json::Object(_) => "map",
    }
}

/// The set of languages available to rules, indexed by position.
pub struct LanguageRegistry {
    adapters: Vec<(&'static str, Arc<dyn LanguageAdapter>)>,
}

impl LanguageRegistry {
    pub fn builtin() -> Self {
        let mut registry = LanguageRegistry { adapters: Vec::new() };
        registry.register(Arc::new(cel::Cel::new()));
        registry.register(Arc::new(starlark::Starlark::new()));
        registry
    }

    pub fn register(&mut self, adapter: Arc<dyn LanguageAdapter>) {
        let id = adapter.info().id;
        self.adapters.retain(|(existing, _)| *existing != id);
        self.adapters.push((id, adapter));
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.adapters.iter().position(|(lang, _)| *lang == id)
    }

    pub fn get(&self, index: usize) -> &Arc<dyn LanguageAdapter> {
        &self.adapters[index].1
    }

    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.adapters.iter().map(|(id, _)| *id).collect()
    }

    pub fn infos(&self) -> Vec<LanguageInfo> {
        self.adapters.iter().map(|(_, a)| a.info()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn verdict_contract() {
        assert!(verdict_from_json(&json!(true)).unwrap().pass);
        let v = verdict_from_json(&json!({"pass": false, "message": "too young"})).unwrap();
        assert_eq!(
            v,
            Verdict {
                pass: false,
                message: Some("too young".into())
            }
        );
        let e = verdict_from_json(&json!({"message": "x"})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::InvalidResult);
        let e = verdict_from_json(&json!(1)).unwrap_err();
        assert!(e.message.contains("number"));
    }

    #[test]
    fn registry_has_builtin_languages() {
        let r = LanguageRegistry::builtin();
        assert_eq!(r.ids(), vec!["cel", "starlark"]);
        assert!(r.index_of("lua").is_none());
    }
}
