//! Starlark adapter.
//!
//! A Starlark rule is a module defining `check(record, config)`. The module is
//! evaluated and frozen once at compile time; each evaluation calls the frozen
//! function in a fresh, short-lived heap.
//!
//! Isolation: `load()` is disabled, only side-effect-free standard library
//! extensions are available, `print` goes to the debug log, and the record
//! and configuration are *frozen* values, so a rule cannot mutate what other
//! rules see. Every evaluation is bounded by a tick limit (calls + loop
//! iterations), a best-effort heap limit, a call stack limit and a wall-clock
//! deadline.

use std::cell::Cell;
use std::time::Instant;

use serde_json::Value as Json;
use starlark::PrintHandler;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{FrozenHeap, FrozenValue, OwnedFrozenValue, Value};

use super::{EvalResult, LanguageInfo, Limits, RuleLanguage, Verdict, error, verdict_from_json};
use crate::model::{JsonMap, RuleErrorKind};

pub struct Starlark {
    globals: Globals,
    dialect: Dialect,
}

impl Starlark {
    pub fn new() -> Self {
        let globals = GlobalsBuilder::extended_by(&[
            LibraryExtension::StructType,
            LibraryExtension::RecordType,
            LibraryExtension::EnumType,
            LibraryExtension::Map,
            LibraryExtension::Filter,
            LibraryExtension::Partial,
            LibraryExtension::Json,
            LibraryExtension::Typing,
            LibraryExtension::SetType,
            LibraryExtension::Print,
        ])
        .build();
        let dialect = Dialect {
            enable_load: false,
            enable_f_strings: true,
            ..Dialect::Standard
        };
        Starlark { globals, dialect }
    }
}

impl Default for Starlark {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StarlarkProgram {
    check: OwnedFrozenValue,
    // Keeps the module (and its frozen heap) alive alongside the function.
    _module: FrozenModule,
}

pub struct StarlarkConfig(OwnedFrozenValue);

struct LogPrinter;

impl PrintHandler for LogPrinter {
    fn println(&self, text: &str) -> starlark::Result<()> {
        tracing::debug!(target: "drover::rule", "{text}");
        Ok(())
    }
}

/// Applies the per-evaluator limits. Deadlines are per rule and re-armed
/// through `deadline` before each call.
fn configure<'a>(
    eval: &mut Evaluator<'_, 'a, '_>,
    limits: &Limits,
    max_ticks: u64,
    deadline: &'a Cell<Option<Instant>>,
) {
    // The setters only fail when a limit is set twice or to zero; settings
    // validation guarantees non-zero values.
    let _ = eval.set_max_tick_count(max_ticks);
    let _ = eval.set_max_heap_size(limits.starlark_max_heap_bytes);
    let _ = eval.set_max_callstack_size(limits.starlark_max_callstack);
    eval.set_print_handler(&LogPrinter);
    eval.set_check_cancelled(Box::new(move || {
        deadline.get().is_some_and(|d| Instant::now() >= d)
    }));
}

fn tick_limit_error() -> crate::model::RuleErrorInfo {
    error(
        RuleErrorKind::ResourceLimit,
        "evaluation exceeded the Starlark tick limit (starlark_max_ticks)",
    )
}

fn heap_exceeded(eval: &Evaluator<'_, '_, '_>, limits: &Limits) -> bool {
    eval.heap().peak_allocated_bytes() + eval.frozen_heap().allocated_bytes() > limits.starlark_max_heap_bytes
}

fn classify(
    err: starlark::Error,
    eval: &Evaluator<'_, '_, '_>,
    ticks: u64,
    limits: &Limits,
    deadline: Option<Instant>,
) -> crate::model::RuleErrorInfo {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return error(RuleErrorKind::Timeout, "evaluation exceeded its time limit");
    }
    if ticks > limits.starlark_max_ticks {
        return tick_limit_error();
    }
    if heap_exceeded(eval, limits) {
        return error(
            RuleErrorKind::ResourceLimit,
            "evaluation exceeded the Starlark heap limit (starlark_max_heap_bytes)",
        );
    }
    error(RuleErrorKind::Runtime, err.to_string())
}

impl RuleLanguage for Starlark {
    type Program = StarlarkProgram;
    type Config = StarlarkConfig;

    fn info(&self) -> LanguageInfo {
        LanguageInfo {
            id: "starlark",
            name: "Starlark",
            template: "def check(record, config):\n    if record.get(\"age\", 0) < config.get(\"min_age\", 18):\n        return {\"pass\": False, \"message\": \"too young\"}\n    return True\n",
            help: "A Starlark module defining `check(record, config)`. `record` and `config` are \
                   read-only dicts. Return a bool, or a dict {\"pass\": bool, \"message\": str}. \
                   load() is disabled; print() writes to the debug log.",
        }
    }

    fn compile(&self, rule_id: &str, source: &str, limits: &Limits) -> Result<StarlarkProgram, String> {
        let ast = AstModule::parse(&format!("{rule_id}.star"), source.to_owned(), &self.dialect)
            .map_err(|e| e.to_string())?;
        let deadline = limits.rule_deadline();
        let armed = Cell::new(deadline);
        let module = Module::with_temp_heap(|module| {
            {
                let mut eval = Evaluator::new(&module);
                configure(&mut eval, limits, limits.starlark_max_ticks, &armed);
                if let Err(e) = eval.eval_module(ast, &self.globals) {
                    let ticks = eval.get_total_tick_count();
                    return Err(classify(e, &eval, ticks, limits, deadline).message);
                }
            }
            module.freeze().map_err(|e| format!("{e:?}"))
        })?;
        let check = module
            .get_option("check")
            .map_err(|e| e.to_string())?
            .ok_or("the module must define a function `check(record, config)`")?;
        let kind = check.value().get_type();
        if kind != "function" {
            return Err(format!("`check` must be a function, found {kind}"));
        }
        Ok(StarlarkProgram {
            check,
            _module: module,
        })
    }

    fn prepare_config(&self, config: &JsonMap) -> Result<StarlarkConfig, String> {
        Ok(StarlarkConfig(freeze(|heap| freeze_map(heap, config))))
    }

    fn evaluate(
        &self,
        record: &Json,
        config: &StarlarkConfig,
        programs: &mut dyn ExactSizeIterator<Item = &StarlarkProgram>,
        limits: &Limits,
        sink: &mut dyn FnMut(usize, EvalResult),
    ) {
        let record = freeze(|heap| freeze_json(heap, record));
        let programs: Vec<&StarlarkProgram> = programs.collect();
        // One evaluator serves all rules of the record: creating one costs a
        // 1 MB scratch allocation. Limits stay per rule: deadlines are re-armed
        // before each call, ticks are charged per call against a cap that
        // leaves every rule its full budget, and after an error the remaining
        // rules continue on a fresh evaluator.
        let budget = limits.starlark_max_ticks.saturating_mul(programs.len() as u64);
        let armed = Cell::new(None);
        let mut next = 0;
        let mut retried = usize::MAX;
        while next < programs.len() {
            next = Module::with_temp_heap(|module| {
                let mut eval = Evaluator::new(&module);
                configure(&mut eval, limits, budget, &armed);
                let first = next;
                for (i, program) in programs.iter().enumerate().skip(first) {
                    let deadline = limits.rule_deadline();
                    armed.set(deadline);
                    let before = eval.get_total_tick_count();
                    // SAFETY: `record` (a local), `config` and `program`
                    // (borrowed from the snapshot for the whole call) own the
                    // frozen heaps of these values and outlive this temporary
                    // heap; no value escapes the closure (`interpret` returns
                    // owned data). This skips `access_owned_frozen_value`,
                    // which clones the heaps' shared `Arc`s into a per-heap set
                    // on every call: contended across workers.
                    let (args, check) = unsafe {
                        (
                            [
                                record.unchecked_frozen_value().to_value(),
                                config.0.unchecked_frozen_value().to_value(),
                            ],
                            program.check.unchecked_frozen_value().to_value(),
                        )
                    };
                    let result = eval.eval_function(check, &args, &[]);
                    let ticks = eval.get_total_tick_count() - before;
                    match result {
                        Ok(_) if ticks > limits.starlark_max_ticks => sink(i, Err(tick_limit_error())),
                        Ok(value) => sink(i, interpret(value)),
                        // The heap limit counts garbage left by earlier rules:
                        // retry once on a fresh heap before blaming this rule.
                        Err(_) if i > first && retried != i && heap_exceeded(&eval, limits) => {
                            retried = i;
                            return i;
                        }
                        Err(e) => {
                            sink(i, Err(classify(e, &eval, ticks, limits, deadline)));
                            return i + 1;
                        }
                    }
                }
                programs.len()
            });
        }
    }
}

fn interpret(value: Value<'_>) -> EvalResult {
    if let Some(pass) = value.unpack_bool() {
        return Ok(Verdict { pass, message: None });
    }
    if value.is_none() {
        return Err(error(
            RuleErrorKind::InvalidResult,
            "check() returned None; did you forget a return statement?",
        ));
    }
    match value.to_json_value() {
        Ok(json) => verdict_from_json(&json),
        Err(e) => Err(error(
            RuleErrorKind::InvalidResult,
            format!("check() returned a value that is not JSON-compatible: {e}"),
        )),
    }
}

/// Allocates a value in a dedicated frozen heap.
fn freeze(alloc: impl FnOnce(&FrozenHeap) -> FrozenValue) -> OwnedFrozenValue {
    let heap = FrozenHeap::new();
    let value = alloc(&heap);
    // SAFETY: `value` was allocated in `heap`, which the returned
    // `OwnedFrozenValue` keeps alive.
    unsafe { OwnedFrozenValue::new(heap.into_ref(), value) }
}

fn freeze_json(heap: &FrozenHeap, value: &Json) -> FrozenValue {
    match value {
        Json::Null => FrozenValue::new_none(),
        Json::Bool(b) => FrozenValue::new_bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i)
            } else if let Some(u) = n.as_u64() {
                heap.alloc(u)
            } else {
                heap.alloc(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Json::String(s) => heap.alloc(s.as_str()),
        Json::Array(items) => heap.alloc(AllocList(items.iter().map(|v| freeze_json(heap, v)))),
        Json::Object(map) => freeze_map(heap, map),
    }
}

fn freeze_map(heap: &FrozenHeap, map: &JsonMap) -> FrozenValue {
    heap.alloc(AllocDict(
        map.iter().map(|(k, v)| (k.as_str(), freeze_json(heap, v))),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::LanguageAdapter;
    use serde_json::json;
    use std::time::Duration;

    fn eval_with(source: &str, record: Json, config: Json, limits: &Limits) -> EvalResult {
        let lang = Starlark::new();
        let program = LanguageAdapter::compile(&lang, "r", source, limits).unwrap();
        let config = LanguageAdapter::prepare_config(&lang, config.as_object().unwrap()).unwrap();
        let mut out = None;
        LanguageAdapter::evaluate(&lang, &record, &config, &[program], limits, &mut |_, r| {
            out = Some(r)
        });
        out.unwrap()
    }

    fn eval(source: &str, record: Json, config: Json) -> EvalResult {
        eval_with(source, record, config, &Limits::default())
    }

    #[test]
    fn boolean_and_dict_results() {
        let src = "def check(record, config):\n    return record['age'] >= config['min_age']\n";
        assert!(
            eval(src, json!({"age": 30}), json!({"min_age": 18}))
                .unwrap()
                .pass
        );
        assert!(!eval(src, json!({"age": 3}), json!({"min_age": 18})).unwrap().pass);

        let src = "def check(record, config):\n    bad = [i['sku'] for i in record['items'] if i['qty'] <= 0]\n    if bad:\n        return {'pass': False, 'message': 'bad qty: ' + ', '.join(bad)}\n    return True\n";
        let v = eval(
            src,
            json!({"items": [{"sku": "a", "qty": 0}, {"sku": "b", "qty": 2}]}),
            json!({}),
        )
        .unwrap();
        assert_eq!(
            v,
            Verdict {
                pass: false,
                message: Some("bad qty: a".into())
            }
        );
    }

    #[test]
    fn json_types_round_trip() {
        let src = "def check(record, config):\n    return type(record['i']) == 'int' and type(record['f']) == 'float' and record['n'] == None and record['b'] == True and record['big'] > 0\n";
        assert!(
            eval(
                src,
                json!({"i": 1, "f": 1.5, "n": null, "b": true, "big": u64::MAX}),
                json!({})
            )
            .unwrap()
            .pass
        );
    }

    #[test]
    fn record_and_config_are_immutable() {
        let src = "def check(record, config):\n    record['x'] = 1\n    return True\n";
        let e = eval(src, json!({}), json!({})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Runtime);
        let src = "def check(record, config):\n    config['x'] = 1\n    return True\n";
        assert!(eval(src, json!({}), json!({})).is_err());
    }

    #[test]
    fn compile_errors() {
        let lang = Starlark::new();
        let l = Limits::default();
        let e =
            LanguageAdapter::compile(&lang, "r", "def check(record, config)\n  return True", &l).unwrap_err();
        assert!(!e.is_empty());
        let e = LanguageAdapter::compile(&lang, "r", "x = 1\n", &l).unwrap_err();
        assert!(e.contains("check"));
        let e = LanguageAdapter::compile(&lang, "r", "check = 1\n", &l).unwrap_err();
        assert!(e.contains("function"));
        let e = LanguageAdapter::compile(
            &lang,
            "r",
            "load('x.star', 'y')\ndef check(r, c):\n    return True\n",
            &l,
        );
        assert!(e.is_err(), "load() must be disabled");
    }

    #[test]
    fn runtime_errors_and_invalid_results() {
        let e = eval(
            "def check(record, config):\n    return record['missing']\n",
            json!({}),
            json!({}),
        )
        .unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Runtime);
        let e = eval("def check(record, config):\n    pass\n", json!({}), json!({})).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::InvalidResult);
        let e = eval(
            "def check(record, config):\n    return 'yes'\n",
            json!({}),
            json!({}),
        )
        .unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::InvalidResult);
    }

    #[test]
    fn tick_limit_stops_runaway_loops() {
        let src = "def check(record, config):\n    n = 0\n    for i in range(100000000):\n        n += i\n    return True\n";
        let limits = Limits {
            starlark_max_ticks: 10_000,
            rule_timeout: None,
            ..Limits::default()
        };
        let e = eval_with(src, json!({}), json!({}), &limits).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::ResourceLimit, "{e:?}");
    }

    #[test]
    fn timeout_stops_long_evaluations() {
        let src = "def check(record, config):\n    n = 0\n    for i in range(1000000000):\n        n += i\n    return True\n";
        let limits = Limits {
            starlark_max_ticks: u64::MAX,
            rule_timeout: Some(Duration::from_millis(50)),
            ..Limits::default()
        };
        let started = Instant::now();
        let e = eval_with(src, json!({}), json!({}), &limits).unwrap_err();
        assert_eq!(e.kind, RuleErrorKind::Timeout, "{e:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn eval_many(sources: &[&str], limits: &Limits) -> Vec<EvalResult> {
        let lang = Starlark::new();
        let programs: Vec<_> = sources
            .iter()
            .map(|s| LanguageAdapter::compile(&lang, "r", s, limits).unwrap())
            .collect();
        let config = LanguageAdapter::prepare_config(&lang, &Default::default()).unwrap();
        let mut out = vec![None; sources.len()];
        LanguageAdapter::evaluate(
            &lang,
            &json!({"n": 1}),
            &config,
            &programs,
            limits,
            &mut |i, r| out[i] = Some(r),
        );
        out.into_iter().map(Option::unwrap).collect()
    }

    const OK: &str = "def check(record, config):\n    return record['n'] == 1\n";

    #[test]
    fn rules_sharing_an_evaluator_stay_isolated() {
        let runaway =
            "def check(record, config):\n    for i in range(100000000):\n        pass\n    return True\n";
        let failing = "def check(record, config):\n    return record['missing']\n";
        let limits = Limits {
            starlark_max_ticks: 10_000,
            rule_timeout: None,
            ..Limits::default()
        };
        let r = eval_many(&[OK, runaway, OK, failing, OK], &limits);
        assert!(r[0].as_ref().unwrap().pass);
        assert_eq!(r[1].as_ref().unwrap_err().kind, RuleErrorKind::ResourceLimit);
        assert!(r[2].as_ref().unwrap().pass, "{:?}", r[2]);
        assert_eq!(r[3].as_ref().unwrap_err().kind, RuleErrorKind::Runtime);
        assert!(r[4].as_ref().unwrap().pass, "{:?}", r[4]);
    }

    #[test]
    fn every_rule_gets_its_full_tick_budget() {
        // Each rule uses ~60% of the limit: a shared budget would fail the second.
        let busy = "def check(record, config):\n    for i in range(6000):\n        pass\n    return True\n";
        let limits = Limits {
            starlark_max_ticks: 10_000,
            rule_timeout: None,
            ..Limits::default()
        };
        let r = eval_many(&[busy, busy, busy, busy], &limits);
        assert!(r.iter().all(|x| x.as_ref().is_ok_and(|v| v.pass)), "{r:?}");
        let over = "def check(record, config):\n    for i in range(12000):\n        pass\n    return True\n";
        let r = eval_many(&[busy, over, busy], &limits);
        assert_eq!(r[1].as_ref().unwrap_err().kind, RuleErrorKind::ResourceLimit);
        assert!(r[2].as_ref().unwrap().pass);
    }

    #[test]
    fn timeouts_are_per_rule() {
        let slow =
            "def check(record, config):\n    for i in range(1000000000):\n        pass\n    return True\n";
        let limits = Limits {
            starlark_max_ticks: u64::MAX / 8,
            rule_timeout: Some(Duration::from_millis(30)),
            ..Limits::default()
        };
        let r = eval_many(&[slow, OK, slow, OK], &limits);
        assert_eq!(r[0].as_ref().unwrap_err().kind, RuleErrorKind::Timeout);
        assert!(r[1].as_ref().unwrap().pass);
        assert_eq!(r[2].as_ref().unwrap_err().kind, RuleErrorKind::Timeout);
        assert!(r[3].as_ref().unwrap().pass);
    }

    #[test]
    fn heap_limit_does_not_count_earlier_rules_garbage() {
        let alloc = "def check(record, config):\n    x = ['%d-%s' % (i, 'abcdefgh') for i in range(4000)]\n    return len(x) == 4000\n";
        let limits = Limits {
            starlark_max_heap_bytes: 600 * 1024,
            rule_timeout: None,
            ..Limits::default()
        };
        let alone = eval_many(&[alloc], &limits);
        assert!(
            alone[0].as_ref().is_ok_and(|v| v.pass),
            "a single rule fits the limit: {alone:?}"
        );
        let r = eval_many(&[alloc, alloc, alloc, alloc, alloc, alloc], &limits);
        assert!(r.iter().all(|x| x.as_ref().is_ok_and(|v| v.pass)), "{r:?}");
        let hog = "def check(record, config):\n    x = ['%d-%s' % (i, 'abcdefgh') for i in range(100000)]\n    return True\n";
        let r = eval_many(&[alloc, hog, alloc], &limits);
        assert_eq!(
            r[1].as_ref().unwrap_err().kind,
            RuleErrorKind::ResourceLimit,
            "{r:?}"
        );
        assert!(r[2].as_ref().is_ok_and(|v| v.pass));
    }

    #[test]
    fn recursion_is_bounded() {
        let src = "def f(n):\n    return f(n + 1)\ndef check(record, config):\n    return f(0)\n";
        assert!(eval(src, json!({}), json!({})).is_err());
    }
}
