//! CEL (Common Expression Language) adapter.
//!
//! A CEL rule is a single expression over two variables: `record` (the JSON
//! record) and `config` (the `rule`-scoped configuration map). JSON integers
//! become CEL `int` (or `uint` above `i64::MAX`), other numbers `double`.
//!
//! The record is converted once per record for all CEL rules, borrowing its
//! strings from the parsed JSON. The configuration is converted once per
//! cache snapshot and borrowed by every evaluation.

use std::collections::HashMap;
use std::sync::Arc;

use cel::common::types::{
    CelBool, CelDouble, CelInt, CelList, CelMap, CelMapKey, CelNull, CelString, CelUInt,
};
use cel::common::value::{CowVal, Val};
use cel::context::VariableResolver;
use cel::{Context, Env, Program, Value};
use serde_json::Value as Json;

use super::{EvalResult, LanguageInfo, Limits, RuleLanguage, Verdict, error, verdict_from_json};
use crate::model::{JsonMap, RuleErrorKind};

pub struct Cel {
    env: Arc<Env>,
}

impl Cel {
    pub fn new() -> Self {
        Cel {
            env: Arc::new(Env::stdlib()),
        }
    }
}

impl Default for Cel {
    fn default() -> Self {
        Self::new()
    }
}

/// The prepared `config` variable.
pub struct CelConfig(Box<dyn Val>);

thread_local! {
    // One environment per worker thread: contexts clone the `Arc` for every
    // record, and a per-thread allocation keeps that refcount uncontended.
    static THREAD_ENV: Arc<Env> = Arc::new(Env::stdlib());
}

struct Variables<'a> {
    record: Box<dyn Val + 'a>,
    config: &'a dyn Val,
}

impl VariableResolver for Variables<'_> {
    fn resolve<'b>(&'b self, variable: &str) -> Option<CowVal<'b, 'b>> {
        match variable {
            "record" => Some(CowVal::Borrowed(&*self.record)),
            "config" => Some(CowVal::Borrowed(self.config)),
            _ => None,
        }
    }
}

impl RuleLanguage for Cel {
    type Program = Program;
    type Config = CelConfig;

    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            id: "cel",
            name: "CEL",
            template: "record.age >= config.min_age",
            help: "A single CEL expression. Variables: `record` (the JSON record) and `config` \
                   (rule-scoped configuration). Return a bool, or a map \
                   {\"pass\": bool, \"message\": string}. Use has(record.field) to test optional fields.",
        }
    }

    fn compile(&self, _rule_id: &str, source: &str, _limits: &Limits) -> Result<Program, String> {
        if source.trim().is_empty() {
            return Err("the expression is empty".into());
        }
        self.env.compile(source).map_err(|e| e.to_string())
    }

    fn prepare_config(&self, config: &JsonMap) -> Result<CelConfig, String> {
        Ok(CelConfig(owned_map(config)))
    }

    fn evaluate(
        &self,
        record: &Json,
        config: &CelConfig,
        programs: &mut dyn ExactSizeIterator<Item = &Program>,
        _limits: &Limits,
        sink: &mut dyn FnMut(usize, EvalResult),
    ) {
        let vars = Variables {
            record: borrowed(record),
            config: &*config.0,
        };
        let mut ctx = THREAD_ENV.with(|env| Context::with_env(Arc::clone(env)));
        ctx.set_variable_resolver(&vars);
        for (i, program) in programs.enumerate() {
            sink(i, interpret(program.execute(&ctx)));
        }
    }
}

fn interpret(result: Result<Value, cel::ExecutionError>) -> EvalResult {
    match result {
        Ok(Value::Bool(pass)) => Ok(Verdict { pass, message: None }),
        Ok(value) => match value.json() {
            Ok(json) => verdict_from_json(&json),
            Err(e) => Err(error(
                RuleErrorKind::InvalidResult,
                format!("the expression returned a value that is not JSON-compatible: {e}"),
            )),
        },
        Err(e) => Err(error(RuleErrorKind::Runtime, e.to_string())),
    }
}

fn number(n: &serde_json::Number) -> Box<dyn Val + 'static> {
    if let Some(i) = n.as_i64() {
        Box::new(CelInt::from(i))
    } else if let Some(u) = n.as_u64() {
        Box::new(CelUInt::from(u))
    } else {
        Box::new(CelDouble::from(n.as_f64().unwrap_or(f64::NAN)))
    }
}

/// Converts a JSON value, borrowing strings from it.
fn borrowed(value: &Json) -> Box<dyn Val + '_> {
    match value {
        Json::Null => Box::new(CelNull),
        Json::Bool(b) => Box::new(CelBool::from(*b)),
        Json::Number(n) => number(n),
        Json::String(s) => Box::new(CelString::from(s.as_str())),
        Json::Array(items) => Box::new(CelList::from(items.iter().map(borrowed).collect::<Vec<_>>())),
        Json::Object(map) => Box::new(CelMap::from(
            map.iter()
                .map(|(k, v)| (CelMapKey::String(CelString::from(k.as_str())), borrowed(v)))
                .collect::<HashMap<_, _>>(),
        )),
    }
}

/// Converts a JSON value into an owned CEL value.
fn owned(value: &Json) -> Box<dyn Val> {
    match value {
        Json::String(s) => Box::new(CelString::from(s.clone())),
        Json::Array(items) => Box::new(CelList::from(items.iter().map(owned).collect::<Vec<_>>())),
        Json::Object(map) => owned_map(map),
        Json::Null => Box::new(CelNull),
        Json::Bool(b) => Box::new(CelBool::from(*b)),
        Json::Number(n) => number(n),
    }
}

fn owned_map(map: &JsonMap) -> Box<dyn Val> {
    Box::new(CelMap::from(
        map.iter()
            .map(|(k, v)| (CelMapKey::String(CelString::from(k.clone())), owned(v)))
            .collect::<HashMap<_, _>>(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LanguageAdapter;
    use serde_json::json;

    fn eval(source: &str, record: Json, config: Json) -> EvalResult {
        let cel = Cel::new();
        let limits = Limits::default();
        let program = LanguageAdapter::compile(&cel, "r", source, &limits).unwrap();
        let config = LanguageAdapter::prepare_config(&cel, config.as_object().unwrap()).unwrap();
        let mut out = None;
        LanguageAdapter::evaluate(&cel, &record, &config, &[program], &limits, &mut |_, r| {
            out = Some(r)
        });
        out.unwrap()
    }

    #[test]
    fn boolean_results() {
        let r = eval(
            "record.age >= config.min_age",
            json!({"age": 20}),
            json!({"min_age": 18}),
        );
        assert!(r.unwrap().pass);
        let r = eval(
            "record.age >= config.min_age",
            json!({"age": 12}),
            json!({"min_age": 18}),
        );
        assert!(!r.unwrap().pass);
    }

    #[test]
    fn map_results_and_nested_data() {
        let r = eval(
            r#"record.items.all(i, i.qty > 0) ? {"pass": true} : {"pass": false, "message": "bad qty for " + record.name}"#,
            json!({"name": "o-1", "items": [{"qty": 1}, {"qty": 0}]}),
            json!({}),
        )
        .unwrap();
        assert_eq!(r.message.as_deref(), Some("bad qty for o-1"));
        assert!(!r.pass);
    }

    #[test]
    fn strings_doubles_and_has() {
        assert!(
            eval(
                r#"record.email.endsWith("@example.com")"#,
                json!({"email": "a@example.com"}),
                json!({})
            )
            .unwrap()
            .pass
        );
        assert!(
            eval("record.price > 9.5", json!({"price": 9.75}), json!({}))
                .unwrap()
                .pass
        );
        assert!(!eval("has(record.missing)", json!({}), json!({})).unwrap().pass);
        assert!(
            eval(
                r#"record.code.matches("^[A-Z]{3}$")"#,
                json!({"code": "ABC"}),
                json!({})
            )
            .unwrap()
            .pass
        );
    }

    #[test]
    fn errors_are_classified() {
        let e = eval("record.missing > 1", json!({}), json!({})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Runtime);
        let e = eval("record.name", json!({"name": "x"}), json!({})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::InvalidResult);
        let cel = Cel::new();
        assert!(LanguageAdapter::compile(&cel, "r", "record.age >=", &Limits::default()).is_err());
        assert!(LanguageAdapter::compile(&cel, "r", "  ", &Limits::default()).is_err());
    }

    #[test]
    fn rules_cannot_see_undeclared_variables() {
        let e = eval("secret == 1", json!({}), json!({})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Runtime);
    }
}
