//! Typed Drover settings derived from `system`-scoped configuration entries.
//!
//! Each known setting has a type, a default, a constraint and an apply mode:
//! `hot` settings take effect on the next cache refresh, `restart` settings
//! are read once at startup.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value as Json, json};

use crate::model::{ConfigEntry, ConfigScope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnknownGroupMode {
    /// Reject the check with `unknown_rule_group`.
    Strict,
    /// Execute all enabled rules and report the fallback in the response.
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StartupTestPolicy {
    /// Do not run tests at startup.
    Off,
    /// Run tests, log failures, start anyway.
    Warn,
    /// Run tests and abort startup if any test does not pass.
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplyMode {
    Hot,
    Restart,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SettingKind {
    /// `max: None` means no upper bound.
    Integer {
        min: i64,
        max: Option<i64>,
    },
    Boolean,
    Enum {
        values: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingSpec {
    pub key: &'static str,
    #[serde(flatten)]
    pub kind: SettingKind,
    /// `None` means "computed at runtime" (see the description).
    pub default: Option<Json>,
    pub apply: ApplyMode,
    pub description: &'static str,
}

/// All known `system` settings.
pub fn specs() -> Vec<SettingSpec> {
    use ApplyMode::*;
    use SettingKind::*;
    vec![
        SettingSpec {
            key: "worker_threads",
            kind: Integer {
                min: 1,
                max: Some(1024),
            },
            default: None,
            apply: Restart,
            description: "Maximum number of OS threads evaluating rules. Defaults to the number of available CPUs.",
        },
        SettingSpec {
            key: "max_queued_checks",
            kind: Integer {
                min: 1,
                max: Some(1_000_000),
            },
            default: Some(json!(256)),
            apply: Restart,
            description: "Maximum number of checks admitted at once (pending + running, sync and async). Further checks are rejected with 503.",
        },
        SettingSpec {
            key: "unknown_group_mode",
            kind: Enum {
                values: &["strict", "fallback"],
            },
            default: Some(json!("strict")),
            apply: Hot,
            description: "Behaviour when a check requests a nonexistent rule group: reject it (strict) or execute all rules (fallback).",
        },
        SettingSpec {
            key: "max_records_per_check",
            kind: Integer {
                min: 1,
                max: Some(10_000_000),
            },
            default: Some(json!(10_000)),
            apply: Hot,
            description: "Maximum number of records accepted by a single check request.",
        },
        SettingSpec {
            key: "check_timeout_ms",
            kind: Integer {
                min: 0,
                max: Some(86_400_000),
            },
            default: Some(json!(30_000)),
            apply: Hot,
            description: "Wall-clock limit for a whole check, including queueing (0 = no limit). Exceeding it fails the execution.",
        },
        SettingSpec {
            key: "rule_timeout_ms",
            kind: Integer {
                min: 0,
                max: Some(3_600_000),
            },
            default: Some(json!(1_000)),
            apply: Hot,
            description: "Wall-clock limit for one Starlark rule evaluation (0 = no limit). CEL expressions are non-Turing-complete and are not interrupted.",
        },
        SettingSpec {
            key: "starlark_max_ticks",
            kind: Integer { min: 1, max: None },
            default: Some(json!(1_000_000)),
            apply: Hot,
            description: "Maximum function calls + loop iterations of one Starlark evaluation.",
        },
        SettingSpec {
            key: "starlark_max_heap_bytes",
            kind: Integer {
                min: 65_536,
                max: None,
            },
            default: Some(json!(64 * 1024 * 1024)),
            apply: Hot,
            description: "Best-effort heap limit of one Starlark evaluation, in bytes.",
        },
        SettingSpec {
            key: "starlark_max_callstack",
            kind: Integer {
                min: 1,
                max: Some(10_000),
            },
            default: Some(json!(100)),
            apply: Hot,
            description: "Maximum Starlark call stack depth.",
        },
        SettingSpec {
            key: "refresh_interval_secs",
            kind: Integer {
                min: 0,
                max: Some(86_400),
            },
            default: Some(json!(30)),
            apply: Hot,
            description: "Interval for polling data sources for external changes to rules, groups and configuration (0 = disabled).",
        },
        SettingSpec {
            key: "startup_tests",
            kind: Enum {
                values: &["off", "warn", "fail"],
            },
            default: Some(json!("warn")),
            apply: Restart,
            description: "Run tests at startup: off, warn (log failures and start) or fail (abort startup on any failure).",
        },
        SettingSpec {
            key: "fail_on_invalid_rules",
            kind: Boolean,
            default: Some(json!(true)),
            apply: Restart,
            description: "Abort startup when a stored rule fails to compile. When false, the rule reports a compile error on every evaluation.",
        },
        SettingSpec {
            key: "execution_retention_secs",
            kind: Integer { min: 0, max: None },
            default: Some(json!(7 * 24 * 3600)),
            apply: Hot,
            description: "Finished executions older than this are purged (0 = keep forever).",
        },
    ]
}

pub fn spec(key: &str) -> Option<SettingSpec> {
    specs().into_iter().find(|s| s.key == key)
}

/// Runtime view of the system settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    pub worker_threads: usize,
    pub max_queued_checks: usize,
    pub unknown_group_mode: UnknownGroupMode,
    pub max_records_per_check: usize,
    pub check_timeout: Option<Duration>,
    pub rule_timeout: Option<Duration>,
    pub starlark_max_ticks: u64,
    pub starlark_max_heap_bytes: usize,
    pub starlark_max_callstack: usize,
    pub refresh_interval: Option<Duration>,
    pub startup_tests: StartupTestPolicy,
    pub fail_on_invalid_rules: bool,
    pub execution_retention: Option<Duration>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::from_entries(&[]).expect("defaults are valid").0
    }
}

/// Validates the value of a system setting against its spec.
pub fn validate_value(key: &str, value: &Json) -> Result<(), String> {
    let spec = spec(key).ok_or_else(|| {
        let known: Vec<_> = specs().iter().map(|s| s.key).collect();
        format!(
            "unknown system setting '{key}' (known settings: {})",
            known.join(", ")
        )
    })?;
    match spec.kind {
        SettingKind::Integer { min, max } => match value.as_i64() {
            Some(v) if v >= min && max.is_none_or(|max| v <= max) => Ok(()),
            Some(v) => Err(match max {
                Some(max) => format!("setting '{key}' must be between {min} and {max}, got {v}"),
                None => format!("setting '{key}' must be at least {min}, got {v}"),
            }),
            None => Err(format!("setting '{key}' must be an integer")),
        },
        SettingKind::Boolean => value
            .as_bool()
            .map(|_| ())
            .ok_or_else(|| format!("setting '{key}' must be a boolean")),
        SettingKind::Enum { values } => match value.as_str() {
            Some(v) if values.contains(&v) => Ok(()),
            _ => Err(format!("setting '{key}' must be one of: {}", values.join(", "))),
        },
    }
}

/// Validates a configuration entry before it is persisted.
pub fn validate_entry(entry: &ConfigEntry) -> Result<(), String> {
    crate::model::validate_identifier("configuration key", &entry.key)?;
    match entry.scope {
        ConfigScope::System => validate_value(&entry.key, &entry.value),
        ConfigScope::Rule => Ok(()),
    }
}

impl Settings {
    /// Builds settings from configuration entries. Returns the settings and
    /// warnings (unknown system keys are ignored with a warning so that a
    /// shared data source can carry settings for other Drover versions).
    pub fn from_entries(entries: &[ConfigEntry]) -> Result<(Settings, Vec<String>), String> {
        let mut warnings = Vec::new();
        let mut errors = Vec::new();
        let mut get = |key: &str| -> Option<Json> {
            entries
                .iter()
                .find(|e| e.scope == ConfigScope::System && e.key == key)
                .map(|e| e.value.clone())
        };
        for e in entries.iter().filter(|e| e.scope == ConfigScope::System) {
            if spec(&e.key).is_none() {
                warnings.push(format!("ignoring unknown system setting '{}'", e.key));
            } else if let Err(err) = validate_value(&e.key, &e.value) {
                errors.push(err);
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        let int = |key: &str, get: &mut dyn FnMut(&str) -> Option<Json>| -> i64 {
            get(key)
                .or_else(|| spec(key).and_then(|s| s.default))
                .and_then(|v| v.as_i64())
                .unwrap_or_default()
        };
        let text = |key: &str, get: &mut dyn FnMut(&str) -> Option<Json>| -> String {
            get(key)
                .or_else(|| spec(key).and_then(|s| s.default))
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        };
        let millis = |v: i64| (v > 0).then(|| Duration::from_millis(v as u64));
        let secs = |v: i64| (v > 0).then(|| Duration::from_secs(v as u64));

        let worker_threads = match get("worker_threads").and_then(|v| v.as_i64()) {
            Some(n) => n as usize,
            None => std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        };
        let settings = Settings {
            worker_threads,
            max_queued_checks: int("max_queued_checks", &mut get) as usize,
            unknown_group_mode: match text("unknown_group_mode", &mut get).as_str() {
                "fallback" => UnknownGroupMode::Fallback,
                _ => UnknownGroupMode::Strict,
            },
            max_records_per_check: int("max_records_per_check", &mut get) as usize,
            check_timeout: millis(int("check_timeout_ms", &mut get)),
            rule_timeout: millis(int("rule_timeout_ms", &mut get)),
            starlark_max_ticks: int("starlark_max_ticks", &mut get) as u64,
            starlark_max_heap_bytes: int("starlark_max_heap_bytes", &mut get) as usize,
            starlark_max_callstack: int("starlark_max_callstack", &mut get) as usize,
            refresh_interval: secs(int("refresh_interval_secs", &mut get)),
            startup_tests: match text("startup_tests", &mut get).as_str() {
                "off" => StartupTestPolicy::Off,
                "fail" => StartupTestPolicy::Fail,
                _ => StartupTestPolicy::Warn,
            },
            fail_on_invalid_rules: get("fail_on_invalid_rules")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            execution_retention: secs(int("execution_retention_secs", &mut get)),
        };
        Ok((settings, warnings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, value: Json) -> ConfigEntry {
        ConfigEntry {
            key: key.into(),
            value,
            scope: ConfigScope::System,
            description: String::new(),
        }
    }

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert_eq!(s.unknown_group_mode, UnknownGroupMode::Strict);
        assert_eq!(s.max_records_per_check, 10_000);
        assert_eq!(s.check_timeout, Some(Duration::from_secs(30)));
        assert!(s.worker_threads >= 1);
        assert!(s.fail_on_invalid_rules);
    }

    #[test]
    fn overrides_and_validation() {
        let (s, warnings) = Settings::from_entries(&[
            entry("unknown_group_mode", json!("fallback")),
            entry("worker_threads", json!(3)),
            entry("check_timeout_ms", json!(0)),
            entry("not_a_setting", json!(1)),
        ])
        .unwrap();
        assert_eq!(s.unknown_group_mode, UnknownGroupMode::Fallback);
        assert_eq!(s.worker_threads, 3);
        assert_eq!(s.check_timeout, None);
        assert_eq!(warnings.len(), 1);

        assert!(Settings::from_entries(&[entry("worker_threads", json!(0))]).is_err());
        assert!(Settings::from_entries(&[entry("unknown_group_mode", json!("lenient"))]).is_err());
        assert!(validate_value("worker_threads", &json!("8")).is_err());
        assert!(validate_value("nope", &json!(1)).is_err());
    }

    #[test]
    fn rule_scope_entries_are_ignored() {
        let mut e = entry("worker_threads", json!("anything"));
        e.scope = ConfigScope::Rule;
        assert!(validate_entry(&e).is_ok());
        let (s, _) = Settings::from_entries(&[e]).unwrap();
        assert_eq!(s, Settings::default());
    }
}
