//! JSON and YAML file data sources.
//!
//! Definitions live in a document whose top level is a map of sections, each
//! holding a list (`rules: [...]`, `groups: [...]`, ...). Several categories
//! may share one file; they then share a lock and every write rewrites the
//! whole document, preserving the other sections. Writes are atomic
//! (temporary file + rename), so readers and external tools never observe a
//! partially written file.
//!
//! Executions are stored one file per execution in a directory.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as Json;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{DataSourceConfig, Definition, ExecutionRepository, Repository, StoreError, fail_execution};
use crate::model::{Execution, ExecutionError, JsonMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Yaml,
}

impl Format {
    pub fn of(config: &DataSourceConfig) -> Format {
        match config {
            DataSourceConfig::Yaml { .. } => Format::Yaml,
            _ => Format::Json,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Yaml => "yaml",
        }
    }

    fn extension(self) -> &'static str {
        self.name()
    }

    fn parse<T: DeserializeOwned>(self, text: &str) -> Result<T, String> {
        match self {
            Format::Json => serde_json::from_str(text).map_err(|e| e.to_string()),
            Format::Yaml => serde_yaml_ng::from_str(text).map_err(|e| e.to_string()),
        }
    }

    fn render<T: Serialize>(self, value: &T) -> Result<String, String> {
        match self {
            Format::Json => serde_json::to_string_pretty(value)
                .map(|mut s| {
                    s.push('\n');
                    s
                })
                .map_err(|e| e.to_string()),
            Format::Yaml => serde_yaml_ng::to_string(value).map_err(|e| e.to_string()),
        }
    }
}

fn io_error(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// Writes `contents` to `path` atomically.
async fn write_atomic(path: &Path, contents: &[u8], create_new: bool) -> Result<(), StoreError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io_error(dir, e))?;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("data");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(contents).await?;
        file.sync_all().await?;
        drop(file);
        if create_new {
            // Hard-link fails if the target exists, so a concurrent insert of
            // the same id can never overwrite an existing file.
            tokio::fs::hard_link(&tmp, path).await?;
            tokio::fs::remove_file(&tmp).await
        } else {
            tokio::fs::rename(&tmp, path).await
        }
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result.map_err(|e| io_error(path, e))
}

async fn read_optional(path: &Path) -> Result<Option<String>, StoreError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, e)),
    }
}

/// A JSON or YAML file holding one or more definition sections.
pub struct FileDocument {
    path: PathBuf,
    format: Format,
    lock: Mutex<()>,
}

impl FileDocument {
    pub fn new(path: PathBuf, format: Format) -> Self {
        FileDocument {
            path,
            format,
            lock: Mutex::new(()),
        }
    }

    /// Reads the document; a missing or empty file is an empty document.
    async fn read(&self) -> Result<JsonMap, StoreError> {
        let Some(text) = read_optional(&self.path).await? else {
            return Ok(JsonMap::new());
        };
        if text.trim().is_empty() {
            return Ok(JsonMap::new());
        }
        match self.format.parse::<Json>(&text) {
            Ok(Json::Object(map)) => Ok(map),
            Ok(Json::Null) => Ok(JsonMap::new()),
            Ok(_) => Err(self.format_error("the top level must be a map of sections")),
            Err(e) => Err(self.format_error(e)),
        }
    }

    async fn write(&self, doc: &JsonMap) -> Result<(), StoreError> {
        let text = self.format.render(doc).map_err(|e| self.format_error(e))?;
        write_atomic(&self.path, text.as_bytes(), false).await
    }

    fn format_error(&self, message: impl std::fmt::Display) -> StoreError {
        StoreError::Format {
            location: self.path.display().to_string(),
            message: message.to_string(),
        }
    }

    async fn fingerprint(&self) -> Result<String, StoreError> {
        match tokio::fs::metadata(&self.path).await {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or_default();
                Ok(format!("{modified}:{}", meta.len()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("missing".into()),
            Err(e) => Err(io_error(&self.path, e)),
        }
    }
}

pub struct FileRepository<T> {
    doc: std::sync::Arc<FileDocument>,
    section: String,
    _marker: PhantomData<fn() -> T>,
}

impl<T: Definition> FileRepository<T> {
    pub fn new(doc: std::sync::Arc<FileDocument>, section: String) -> Self {
        FileRepository {
            doc,
            section,
            _marker: PhantomData,
        }
    }

    fn parse_section(&self, doc: &JsonMap) -> Result<Vec<T>, StoreError> {
        let location = |suffix: String| format!("{}: {}{suffix}", self.doc.path.display(), self.section);
        let items = match doc.get(&self.section) {
            None | Some(Json::Null) => return Ok(Vec::new()),
            Some(Json::Array(items)) => items,
            Some(_) => {
                return Err(StoreError::Format {
                    location: location(String::new()),
                    message: "the section must be a list".into(),
                });
            }
        };
        let mut parsed: Vec<T> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let value = T::deserialize(item).map_err(|e| StoreError::Format {
                location: location(format!("[{i}]")),
                message: e.to_string(),
            })?;
            parsed.push(value);
        }
        parsed.sort_by(|a, b| a.key().cmp(b.key()));
        if let Some(pair) = parsed.windows(2).find(|w| w[0].key() == w[1].key()) {
            return Err(StoreError::Format {
                location: location(String::new()),
                message: format!("duplicate {} '{}'", T::KIND, pair[0].key()),
            });
        }
        Ok(parsed)
    }

    /// Applies `change` to the section under the document lock and writes it back.
    async fn modify(
        &self,
        change: impl FnOnce(&mut Vec<T>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let _guard = self.doc.lock.lock().await;
        let mut doc = self.doc.read().await?;
        let mut items = self.parse_section(&doc)?;
        change(&mut items)?;
        let section = serde_json::to_value(&items).map_err(|e| self.doc.format_error(e))?;
        doc.insert(self.section.clone(), section);
        self.doc.write(&doc).await
    }
}

#[async_trait]
impl<T: Definition> Repository<T> for FileRepository<T> {
    async fn list(&self) -> Result<Vec<T>, StoreError> {
        let doc = self.doc.read().await?;
        self.parse_section(&doc)
    }

    async fn get(&self, key: &str) -> Result<Option<T>, StoreError> {
        Ok(self.list().await?.into_iter().find(|i| i.key() == key))
    }

    async fn create(&self, item: &T) -> Result<(), StoreError> {
        self.modify(|items| {
            if items.iter().any(|i| i.key() == item.key()) {
                return Err(StoreError::Conflict {
                    kind: T::KIND,
                    id: item.key().to_string(),
                });
            }
            items.push(item.clone());
            items.sort_by(|a, b| a.key().cmp(b.key()));
            Ok(())
        })
        .await
    }

    async fn update(&self, item: &T) -> Result<(), StoreError> {
        self.modify(|items| match items.iter_mut().find(|i| i.key() == item.key()) {
            Some(existing) => {
                *existing = item.clone();
                Ok(())
            }
            None => Err(StoreError::NotFound {
                kind: T::KIND,
                id: item.key().to_string(),
            }),
        })
        .await
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.modify(|items| {
            let before = items.len();
            items.retain(|i| i.key() != key);
            if items.len() == before {
                return Err(StoreError::NotFound {
                    kind: T::KIND,
                    id: key.to_string(),
                });
            }
            Ok(())
        })
        .await
    }

    async fn fingerprint(&self) -> Result<Option<String>, StoreError> {
        self.doc.fingerprint().await.map(Some)
    }

    fn describe(&self) -> String {
        format!(
            "{} file {} (section '{}')",
            self.doc.format.name(),
            self.doc.path.display(),
            self.section
        )
    }
}

/// Executions stored as one JSON or YAML file per execution.
pub struct FileExecutions {
    dir: PathBuf,
    format: Format,
}

impl FileExecutions {
    pub async fn open(dir: PathBuf, format: Format) -> Result<Self, StoreError> {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| io_error(&dir, e))?;
        Ok(FileExecutions { dir, format })
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.{}", self.format.extension()))
    }

    async fn read_path(&self, path: &Path) -> Result<Option<Execution>, StoreError> {
        let Some(text) = read_optional(path).await? else {
            return Ok(None);
        };
        self.format
            .parse(&text)
            .map(Some)
            .map_err(|message| StoreError::Format {
                location: path.display().to_string(),
                message,
            })
    }

    async fn render(&self, execution: &Execution) -> Result<String, StoreError> {
        self.format
            .render(execution)
            .map_err(|message| StoreError::Format {
                location: self.dir.display().to_string(),
                message,
            })
    }

    /// Paths of all execution files with their modification time.
    async fn entries(&self) -> Result<Vec<(PathBuf, SystemTime)>, StoreError> {
        let mut out = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.dir)
            .await
            .map_err(|e| io_error(&self.dir, e))?;
        while let Some(entry) = dir.next_entry().await.map_err(|e| io_error(&self.dir, e))? {
            let path = entry.path();
            let is_execution = path.extension().and_then(|e| e.to_str()) == Some(self.format.extension())
                && !entry.file_name().to_string_lossy().starts_with('.');
            if is_execution {
                let modified = entry
                    .metadata()
                    .await
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((path, modified));
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl ExecutionRepository for FileExecutions {
    async fn insert(&self, execution: &Execution) -> Result<(), StoreError> {
        let text = self.render(execution).await?;
        let path = self.path(execution.execution_id);
        write_atomic(&path, text.as_bytes(), true)
            .await
            .map_err(|e| match e {
                StoreError::Io { .. } if path.exists() => StoreError::Conflict {
                    kind: "execution",
                    id: execution.execution_id.to_string(),
                },
                other => other,
            })
    }

    async fn update(&self, execution: &Execution) -> Result<(), StoreError> {
        let path = self.path(execution.execution_id);
        if tokio::fs::metadata(&path).await.is_err() {
            return Err(StoreError::NotFound {
                kind: "execution",
                id: execution.execution_id.to_string(),
            });
        }
        let text = self.render(execution).await?;
        write_atomic(&path, text.as_bytes(), false).await
    }

    async fn get(&self, id: Uuid) -> Result<Option<Execution>, StoreError> {
        self.read_path(&self.path(id)).await
    }

    async fn fail_unfinished(&self, error: &ExecutionError, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let mut count = 0;
        for (path, _) in self.entries().await? {
            let Ok(Some(mut execution)) = self.read_path(&path).await else {
                continue;
            };
            if !execution.status.is_terminal() {
                fail_execution(&mut execution, error, now);
                self.update(&execution).await?;
                count += 1;
            }
        }
        Ok(count)
    }

    async fn purge_finished_before(&self, cutoff: DateTime<Utc>) -> Result<u64, StoreError> {
        let mut count = 0;
        let cutoff_time = SystemTime::from(cutoff);
        for (path, modified) in self.entries().await? {
            // A file is written when its execution finishes (or later), so a
            // file modified after the cutoff cannot hold an execution that
            // finished before it in normal operation: skip it without parsing.
            // (A file restored with a fresh mtime is purged one retention
            // period after it was restored.)
            if modified >= cutoff_time {
                continue;
            }
            let Ok(Some(execution)) = self.read_path(&path).await else {
                continue;
            };
            if execution.status.is_terminal() && execution.finished_at.is_some_and(|f| f < cutoff) {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| io_error(&path, e))?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn describe(&self) -> String {
        format!("{} files in {}", self.format.name(), self.dir.display())
    }
}
