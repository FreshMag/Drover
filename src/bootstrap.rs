//! Bootstrap configuration: everything Drover needs before it can reach its
//! data sources (listen address, credentials, logging, data sources).
//!
//! Loaded from a YAML or JSON file (`--config`, `DROVER_CONFIG`, default
//! `drover.yaml`). `${VAR}` and `${VAR:-default}` are replaced with
//! environment variables before parsing. These variables override the file:
//! `DROVER_BIND`, `DROVER_ADMIN_TOKEN`, `DROVER_API_TOKENS` (comma-separated),
//! `DROVER_LOG` (log filter).
//!
//! Application settings (`worker_threads`, `unknown_group_mode`, ...) are
//! *not* here: they are `system`-scoped entries of the configuration data
//! source.

use std::path::Path;

use serde::Deserialize;

use crate::store::{DataSourceConfig, DataSourcesConfig};

fn default_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_max_body() -> usize {
    16 * 1024 * 1024
}

fn default_grace() -> u64 {
    30
}

fn default_level() -> String {
    "info".to_string()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Maximum request body size in bytes.
    #[serde(default = "default_max_body")]
    pub max_body_bytes: usize,
    /// Async I/O threads (HTTP and data source I/O). Rule evaluation uses the
    /// separate `worker_threads` pool. Defaults to min(4, CPUs).
    #[serde(default)]
    pub io_threads: Option<usize>,
    /// Time allowed for in-flight checks to finish on shutdown.
    #[serde(default = "default_grace")]
    pub shutdown_grace_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: default_bind(),
            max_body_bytes: default_max_body(),
            io_threads: None,
            shutdown_grace_secs: default_grace(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Bearer token required by the administration API. Without one, the
    /// administration API is disabled unless `allow_unauthenticated_admin`.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Bearer tokens accepted by the main API (`/api/v1`, `/metrics`).
    /// Empty: the main API is unauthenticated.
    #[serde(default)]
    pub api_tokens: Vec<String>,
    /// Development only: serve the administration API without credentials.
    #[serde(default)]
    pub allow_unauthenticated_admin: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// A `tracing` filter such as `info` or `drover=debug,info`.
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: default_level(),
            format: LogFormat::Text,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub log: LogConfig,
    pub datasources: DataSourcesConfig,
}

impl Bootstrap {
    pub fn load(path: &Path) -> Result<Bootstrap, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read configuration file {}: {e}", path.display()))?;
        let json = path.extension().and_then(|e| e.to_str()) == Some("json");
        let mut config = Self::parse(&text, json, &|name| std::env::var(name).ok())
            .map_err(|e| format!("invalid configuration file {}: {e}", path.display()))?;
        if let Some(base) = path.parent() {
            config.resolve_paths(base);
        }
        Ok(config)
    }

    /// Makes relative file data source paths relative to `base` (the
    /// directory of the configuration file) instead of the working directory.
    pub fn resolve_paths(&mut self, base: &Path) {
        let ds = &mut self.datasources;
        for source in [
            &mut ds.rules,
            &mut ds.groups,
            &mut ds.tests,
            &mut ds.config,
            &mut ds.executions,
        ] {
            if let DataSourceConfig::Json { path, .. } | DataSourceConfig::Yaml { path, .. } = source {
                if path.is_relative() {
                    *path = base.join(&*path);
                }
            }
        }
    }

    pub fn parse(text: &str, json: bool, env: &dyn Fn(&str) -> Option<String>) -> Result<Bootstrap, String> {
        let text = interpolate(text, env)?;
        let mut config: Bootstrap = if json {
            serde_json::from_str(&text).map_err(|e| e.to_string())?
        } else {
            serde_yaml_ng::from_str(&text).map_err(|e| e.to_string())?
        };
        if let Some(bind) = env("DROVER_BIND") {
            config.server.bind = bind;
        }
        if let Some(token) = env("DROVER_ADMIN_TOKEN") {
            config.auth.admin_token = Some(token);
        }
        if let Some(tokens) = env("DROVER_API_TOKENS") {
            config.auth.api_tokens = tokens.split(',').map(|t| t.trim().to_string()).collect();
        }
        if let Some(level) = env("DROVER_LOG") {
            config.log.level = level;
        }
        // Empty values (e.g. `${DROVER_ADMIN_TOKEN:-}`) mean "not set".
        config.auth.admin_token = config.auth.admin_token.filter(|t| !t.trim().is_empty());
        config.auth.api_tokens.retain(|t| !t.trim().is_empty());
        if config.server.max_body_bytes == 0 {
            return Err("server.max_body_bytes must be positive".into());
        }
        Ok(config)
    }

    /// Non-fatal configuration problems worth logging at startup.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        match &self.auth.admin_token {
            None if self.auth.allow_unauthenticated_admin => warnings.push(
                "the administration API is served WITHOUT authentication (auth.allow_unauthenticated_admin); never expose this instance to untrusted networks".into(),
            ),
            None => warnings.push(
                "no auth.admin_token configured: the administration API is disabled".into(),
            ),
            Some(t) if t.len() < 16 => warnings.push("auth.admin_token is shorter than 16 characters".into()),
            Some(_) => {}
        }
        if self.auth.api_tokens.is_empty() {
            warnings
                .push("no auth.api_tokens configured: the main API accepts unauthenticated requests".into());
        }
        warnings
    }
}

/// Replaces `${NAME}` and `${NAME:-default}` with environment values.
fn interpolate(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| "unterminated ${...} placeholder".to_string())?;
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (expr, None),
        };
        let valid = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return Err(format!("invalid placeholder '${{{expr}}}'"));
        }
        match (env(name), default) {
            (Some(value), _) => out.push_str(&value),
            (None, Some(default)) => out.push_str(default),
            (None, None) => {
                return Err(format!(
                    "environment variable {name} is not set (use ${{{name}:-default}} for optional values)"
                ));
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server:
  bind: "${BIND:-0.0.0.0:9000}"
auth:
  admin_token: "${TOKEN}"
datasources:
  rules: { type: yaml, path: data/definitions.yaml }
  groups: { type: yaml, path: data/definitions.yaml }
  tests: { type: json, path: data/tests.json }
  config: { type: postgres, url: "postgres://u:${PW}@db/drover" }
  executions: { type: memory }
"#;

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn parses_with_interpolation() {
        let c = Bootstrap::parse(
            SAMPLE,
            false,
            &env(&[("TOKEN", "secret-secret-secret"), ("PW", "pw")]),
        )
        .unwrap();
        assert_eq!(c.server.bind, "0.0.0.0:9000");
        assert_eq!(c.auth.admin_token.as_deref(), Some("secret-secret-secret"));
        assert!(
            matches!(&c.datasources.config, DataSourceConfig::Postgres { url, .. } if url == "postgres://u:pw@db/drover")
        );
        assert!(matches!(c.datasources.executions, DataSourceConfig::Memory));
    }

    #[test]
    fn relative_paths_follow_the_config_file() {
        let mut c = Bootstrap::parse(SAMPLE, false, &env(&[("TOKEN", "t"), ("PW", "x")])).unwrap();
        c.resolve_paths(Path::new("/etc/drover"));
        assert!(
            matches!(&c.datasources.rules, DataSourceConfig::Yaml { path, .. } if path == Path::new("/etc/drover/data/definitions.yaml"))
        );
    }

    #[test]
    fn env_overrides_and_errors() {
        let c = Bootstrap::parse(
            SAMPLE,
            false,
            &env(&[
                ("TOKEN", ""),
                ("PW", "x"),
                ("DROVER_BIND", "1.2.3.4:1"),
                ("DROVER_API_TOKENS", "a, b"),
            ]),
        )
        .unwrap();
        assert_eq!(c.server.bind, "1.2.3.4:1");
        assert_eq!(c.auth.admin_token, None);
        assert_eq!(c.auth.api_tokens, vec!["a", "b"]);

        let e = Bootstrap::parse(SAMPLE, false, &env(&[("PW", "x")])).unwrap_err();
        assert!(e.contains("TOKEN"));
        let e = Bootstrap::parse("datasources: {}\n", false, &env(&[])).unwrap_err();
        assert!(e.contains("missing field"), "{e}");
        let e = Bootstrap::parse(
            &SAMPLE.replace("server:", "sever:"),
            false,
            &env(&[("TOKEN", "t"), ("PW", "x")]),
        )
        .unwrap_err();
        assert!(e.contains("sever"), "{e}");
    }
}
