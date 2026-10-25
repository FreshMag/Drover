//! Cache manager: keeps the current [`Snapshot`] of rules, groups and
//! configuration and refreshes it from the data sources.
//!
//! # Consistency
//!
//! A refresh loads rules, groups and configuration, compiles a complete new
//! snapshot off the async threads and swaps it in with a single pointer
//! replacement. Requests clone the `Arc` of the current snapshot when they
//! start, so they never observe partially updated rules or memberships; a
//! check that started on the old snapshot finishes on it. Refreshes are
//! serialised. If loading or validation fails, the current snapshot stays in
//! place and the error is recorded in [`RefreshStatus`].
//!
//! # Refresh triggers
//!
//! * successful administration changes (explicit, immediate);
//! * `POST /admin/api/refresh` (explicit invalidation);
//! * polling every `refresh_interval_secs` for changes made outside Drover.
//!   File backends are compared by modification time and size first, so an
//!   unchanged file costs one `stat`; PostgreSQL tables are re-read and
//!   compared with the cached definitions. Only actual changes rebuild the
//!   snapshot, and unchanged rules keep their compiled programs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::engine::{Definitions, Snapshot};
use crate::error::Error;
use crate::lang::LanguageRegistry;
use crate::model::{ConfigEntry, Rule, RuleGroup};
use crate::store::Stores;

#[derive(Debug, Clone, Default, Serialize)]
pub struct RefreshStatus {
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_success: Option<DateTime<Utc>>,
    pub last_change: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub refreshes: u64,
    pub changes: u64,
    pub failures: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RefreshResult {
    pub changed: bool,
    pub version: u64,
}

/// Raw definitions of the current snapshot, used to detect changes.
#[derive(Default)]
struct Loaded {
    rules: Vec<Rule>,
    groups: Vec<RuleGroup>,
    config: Vec<ConfigEntry>,
    fingerprints: Option<[String; 3]>,
}

pub struct RuleCache {
    current: RwLock<Arc<Snapshot>>,
    loaded: tokio::sync::Mutex<Loaded>,
    stores: Stores,
    languages: Arc<LanguageRegistry>,
    version: AtomicU64,
    status: Mutex<RefreshStatus>,
}

impl RuleCache {
    /// Loads the initial snapshot.
    pub async fn load(stores: Stores, languages: Arc<LanguageRegistry>) -> Result<RuleCache, Error> {
        let fingerprints = fingerprints(&stores).await?;
        let (rules, groups, config) = load_all(&stores).await?;
        let snapshot = build(1, &rules, &groups, &config, languages.clone(), None).await?;
        let now = Utc::now();
        Ok(RuleCache {
            current: RwLock::new(Arc::new(snapshot)),
            loaded: tokio::sync::Mutex::new(Loaded {
                rules,
                groups,
                config,
                fingerprints,
            }),
            stores,
            languages,
            version: AtomicU64::new(1),
            status: Mutex::new(RefreshStatus {
                last_attempt: Some(now),
                last_success: Some(now),
                last_change: Some(now),
                ..Default::default()
            }),
        })
    }

    /// The current snapshot. Cheap: clones an `Arc` under a read lock.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.current.read().unwrap().clone()
    }

    pub fn status(&self) -> RefreshStatus {
        self.status.lock().unwrap().clone()
    }

    /// Reloads definitions and swaps in a new snapshot if anything changed.
    /// With `force`, the fingerprint shortcut is skipped and definitions are
    /// always re-read.
    pub async fn refresh(&self, force: bool) -> Result<RefreshResult, Error> {
        let mut loaded = self.loaded.lock().await;
        self.status.lock().unwrap().last_attempt = Some(Utc::now());
        let result = self.refresh_locked(&mut loaded, force).await;
        let mut status = self.status.lock().unwrap();
        status.refreshes += 1;
        match &result {
            Ok(r) => {
                let now = Utc::now();
                status.last_success = Some(now);
                status.last_error = None;
                if r.changed {
                    status.changes += 1;
                    status.last_change = Some(now);
                }
            }
            Err(e) => {
                status.failures += 1;
                status.last_error = Some(e.to_string());
            }
        }
        result
    }

    async fn refresh_locked(&self, loaded: &mut Loaded, force: bool) -> Result<RefreshResult, Error> {
        let version = self.version.load(Ordering::Relaxed);
        let unchanged = RefreshResult {
            changed: false,
            version,
        };
        // Fingerprints are taken before reading so that a change racing with
        // the read is picked up by the next refresh rather than missed.
        let fingerprints = fingerprints(&self.stores).await?;
        if !force && fingerprints.is_some() && fingerprints == loaded.fingerprints {
            return Ok(unchanged);
        }
        let (rules, groups, config) = load_all(&self.stores).await?;
        if rules == loaded.rules && groups == loaded.groups && config == loaded.config {
            loaded.fingerprints = fingerprints;
            return Ok(unchanged);
        }
        let previous = self.snapshot();
        let version = version + 1;
        let snapshot = build(
            version,
            &rules,
            &groups,
            &config,
            self.languages.clone(),
            Some(previous),
        )
        .await?;
        for diagnostic in &snapshot.diagnostics {
            tracing::warn!(version, "{diagnostic}");
        }
        *self.current.write().unwrap() = Arc::new(snapshot);
        self.version.store(version, Ordering::Relaxed);
        *loaded = Loaded {
            rules,
            groups,
            config,
            fingerprints,
        };
        tracing::info!(version, "rule cache refreshed");
        Ok(RefreshResult {
            changed: true,
            version,
        })
    }
}

async fn fingerprints(stores: &Stores) -> Result<Option<[String; 3]>, Error> {
    let (r, g, c) = tokio::try_join!(
        async {
            stores
                .rules
                .fingerprint()
                .await
                .map_err(Error::data_source("rules"))
        },
        async {
            stores
                .groups
                .fingerprint()
                .await
                .map_err(Error::data_source("groups"))
        },
        async {
            stores
                .config
                .fingerprint()
                .await
                .map_err(Error::data_source("config"))
        },
    )?;
    Ok(match (r, g, c) {
        (Some(r), Some(g), Some(c)) => Some([r, g, c]),
        _ => None,
    })
}

async fn load_all(stores: &Stores) -> Result<(Vec<Rule>, Vec<RuleGroup>, Vec<ConfigEntry>), Error> {
    tokio::try_join!(
        async { stores.rules.list().await.map_err(Error::data_source("rules")) },
        async { stores.groups.list().await.map_err(Error::data_source("groups")) },
        async { stores.config.list().await.map_err(Error::data_source("config")) },
    )
}

/// Compiles a snapshot on the blocking thread pool (compilation is CPU work).
async fn build(
    version: u64,
    rules: &[Rule],
    groups: &[RuleGroup],
    config: &[ConfigEntry],
    languages: Arc<LanguageRegistry>,
    previous: Option<Arc<Snapshot>>,
) -> Result<Snapshot, Error> {
    let defs = Definitions {
        rules: rules.to_vec(),
        groups: groups.to_vec(),
        config: config.to_vec(),
    };
    tokio::task::spawn_blocking(move || Snapshot::build(version, defs, languages, previous.as_deref()))
        .await
        .map_err(|e| Error::Internal(format!("snapshot build failed: {e}")))?
        .map_err(|e| Error::ConfigValidation(format!("invalid configuration: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::rule;
    use crate::store::file::{FileDocument, FileRepository, Format};
    use crate::store::memory::MemoryRepository;

    fn stores_with(rules: Vec<Rule>) -> Stores {
        let mut stores = Stores::memory();
        stores.rules = Arc::new(MemoryRepository::with_items(rules));
        stores
    }

    #[tokio::test]
    async fn refresh_swaps_snapshot_only_on_change() {
        let stores = stores_with(vec![rule("a", "cel", "true")]);
        let cache = RuleCache::load(stores.clone(), Arc::new(LanguageRegistry::builtin()))
            .await
            .unwrap();
        let first = cache.snapshot();
        assert_eq!(first.version, 1);

        assert!(!cache.refresh(false).await.unwrap().changed);
        assert!(!cache.refresh(true).await.unwrap().changed);
        assert!(Arc::ptr_eq(&first, &cache.snapshot()));

        stores.rules.create(&rule("b", "cel", "false")).await.unwrap();
        let r = cache.refresh(false).await.unwrap();
        assert!(r.changed);
        assert_eq!(r.version, 2);
        let second = cache.snapshot();
        assert_eq!(second.rules.len(), 2);
        // The old snapshot is untouched for checks still using it.
        assert_eq!(first.rules.len(), 1);
    }

    #[tokio::test]
    async fn failed_refresh_keeps_current_snapshot() {
        let stores = stores_with(vec![rule("a", "cel", "true")]);
        let cache = RuleCache::load(stores.clone(), Arc::new(LanguageRegistry::builtin()))
            .await
            .unwrap();
        stores
            .config
            .create(&ConfigEntry {
                key: "worker_threads".into(),
                value: serde_json::json!("many"),
                scope: crate::model::ConfigScope::System,
                description: String::new(),
            })
            .await
            .unwrap();
        assert!(cache.refresh(false).await.is_err());
        assert_eq!(cache.snapshot().version, 1);
        let status = cache.status();
        assert_eq!(status.failures, 1);
        assert!(status.last_error.unwrap().contains("worker_threads"));
    }

    #[tokio::test]
    async fn detects_external_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.yaml");
        std::fs::write(
            &path,
            "rules:\n  - id: a\n    language: cel\n    source: 'true'\n",
        )
        .unwrap();
        let doc = Arc::new(FileDocument::new(path.clone(), Format::Yaml));
        let mut stores = Stores::memory();
        stores.rules = Arc::new(FileRepository::<Rule>::new(doc, "rules".into()));
        let cache = RuleCache::load(stores, Arc::new(LanguageRegistry::builtin()))
            .await
            .unwrap();
        assert!(!cache.refresh(false).await.unwrap().changed);

        std::fs::write(
            &path,
            "rules:\n  - id: a\n    language: cel\n    source: 'false'\n  - id: b\n    language: starlark\n    source: |\n      def check(record, config):\n          return True\n",
        )
        .unwrap();
        assert!(cache.refresh(false).await.unwrap().changed);
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.rules.len(), 2);
        assert_eq!(snapshot.rule("a").unwrap().def.source, "false");
    }
}
