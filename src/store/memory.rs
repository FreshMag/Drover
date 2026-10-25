//! Non-persistent in-memory data sources (development and tests).

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Definition, ExecutionRepository, Repository, StoreError, fail_execution};
use crate::model::{Execution, ExecutionError};

pub struct MemoryRepository<T> {
    items: RwLock<BTreeMap<String, T>>,
    generation: AtomicU64,
}

impl<T> Default for MemoryRepository<T> {
    fn default() -> Self {
        MemoryRepository {
            items: RwLock::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
        }
    }
}

impl<T: Definition> MemoryRepository<T> {
    pub fn with_items(items: impl IntoIterator<Item = T>) -> Self {
        let repo = Self::default();
        repo.items
            .write()
            .unwrap()
            .extend(items.into_iter().map(|i| (i.key().to_string(), i)));
        repo
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait]
impl<T: Definition> Repository<T> for MemoryRepository<T> {
    async fn list(&self) -> Result<Vec<T>, StoreError> {
        Ok(self.items.read().unwrap().values().cloned().collect())
    }

    async fn get(&self, key: &str) -> Result<Option<T>, StoreError> {
        Ok(self.items.read().unwrap().get(key).cloned())
    }

    async fn create(&self, item: &T) -> Result<(), StoreError> {
        let mut items = self.items.write().unwrap();
        if items.contains_key(item.key()) {
            return Err(StoreError::Conflict {
                kind: T::KIND,
                id: item.key().to_string(),
            });
        }
        items.insert(item.key().to_string(), item.clone());
        self.bump();
        Ok(())
    }

    async fn update(&self, item: &T) -> Result<(), StoreError> {
        let mut items = self.items.write().unwrap();
        match items.get_mut(item.key()) {
            Some(existing) => *existing = item.clone(),
            None => {
                return Err(StoreError::NotFound {
                    kind: T::KIND,
                    id: item.key().to_string(),
                });
            }
        }
        self.bump();
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        if self.items.write().unwrap().remove(key).is_none() {
            return Err(StoreError::NotFound {
                kind: T::KIND,
                id: key.to_string(),
            });
        }
        self.bump();
        Ok(())
    }

    async fn fingerprint(&self) -> Result<Option<String>, StoreError> {
        Ok(Some(self.generation.load(Ordering::Relaxed).to_string()))
    }

    fn describe(&self) -> String {
        "memory (not persistent)".to_string()
    }
}

#[derive(Default)]
pub struct MemoryExecutions {
    items: RwLock<HashMap<Uuid, Execution>>,
}

#[async_trait]
impl ExecutionRepository for MemoryExecutions {
    async fn insert(&self, execution: &Execution) -> Result<(), StoreError> {
        let mut items = self.items.write().unwrap();
        if items.contains_key(&execution.execution_id) {
            return Err(StoreError::Conflict {
                kind: "execution",
                id: execution.execution_id.to_string(),
            });
        }
        items.insert(execution.execution_id, execution.clone());
        Ok(())
    }

    async fn update(&self, execution: &Execution) -> Result<(), StoreError> {
        match self.items.write().unwrap().get_mut(&execution.execution_id) {
            Some(existing) => {
                *existing = execution.clone();
                Ok(())
            }
            None => Err(StoreError::NotFound {
                kind: "execution",
                id: execution.execution_id.to_string(),
            }),
        }
    }

    async fn get(&self, id: Uuid) -> Result<Option<Execution>, StoreError> {
        Ok(self.items.read().unwrap().get(&id).cloned())
    }

    async fn fail_unfinished(&self, error: &ExecutionError, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let mut count = 0;
        for execution in self.items.write().unwrap().values_mut() {
            if !execution.status.is_terminal() {
                fail_execution(execution, error, now);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn purge_finished_before(&self, cutoff: DateTime<Utc>) -> Result<u64, StoreError> {
        let mut items = self.items.write().unwrap();
        let before = items.len();
        items.retain(|_, e| !(e.status.is_terminal() && e.finished_at.is_some_and(|f| f < cutoff)));
        Ok((before - items.len()) as u64)
    }

    fn describe(&self) -> String {
        "memory (not persistent)".to_string()
    }
}
