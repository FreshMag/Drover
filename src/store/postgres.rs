//! PostgreSQL data sources.
//!
//! Each category has its own table (names configurable, optionally
//! schema-qualified). Definitions use one column per field so they can be
//! edited with plain SQL; group memberships live in `<groups table>_members`.
//! Executions keep queryable columns plus the full document as JSONB.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value as Json;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::types::Json as SqlJson;
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Row};
use uuid::Uuid;

use super::{Definition, ExecutionRepository, Repository, StoreError};
use crate::model::{
    ConfigEntry, ConfigScope, Execution, ExecutionError, ExecutionStatus, Rule, RuleGroup, TestCase,
};

/// Builds a dynamic SQL string. Only table names validated by
/// [`validate_table`] are ever interpolated; values are always bound.
macro_rules! sql {
    ($($arg:tt)*) => {
        AssertSqlSafe(format!($($arg)*))
    };
}

pub async fn connect(url: &str, max_connections: u32, timeout: Duration) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections.max(1))
        .acquire_timeout(timeout)
        .connect(url)
        .await
}

fn db(e: sqlx::Error) -> StoreError {
    StoreError::Database(e.to_string())
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

/// Validates a (possibly schema-qualified) table name; names are interpolated
/// into SQL, so only plain identifiers are accepted.
pub fn validate_table(name: &str) -> Result<(), String> {
    let ident = |s: &str| {
        !s.is_empty()
            && s.len() <= 63
            && (s.as_bytes()[0].is_ascii_alphabetic() || s.as_bytes()[0] == b'_')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    };
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() <= 2 && parts.iter().all(|p| ident(p)) {
        Ok(())
    } else {
        Err(format!("invalid table name '{name}'"))
    }
}

/// Mapping of a definition type to PostgreSQL tables.
#[async_trait]
pub trait PgEntity: Definition {
    fn ddl(table: &str) -> Vec<String>;
    async fn load_all(conn: &PgPool, table: &str) -> Result<Vec<Self>, StoreError>;
    async fn load_one(conn: &PgPool, table: &str, key: &str) -> Result<Option<Self>, StoreError>;
    /// Inserts the item; returns `Conflict` if it exists.
    async fn insert(conn: &mut PgConnection, table: &str, item: &Self) -> Result<(), StoreError>;
    /// Updates the item; returns the number of rows updated.
    async fn update(conn: &mut PgConnection, table: &str, item: &Self) -> Result<u64, StoreError>;
    /// Deletes the item; returns the number of rows deleted.
    async fn delete(conn: &mut PgConnection, table: &str, key: &str) -> Result<u64, StoreError>;
}

pub async fn open_repository<T: Definition + PgEntity>(
    pool: PgPool,
    table: &str,
    auto_migrate: bool,
) -> Result<Arc<dyn Repository<T>>, String> {
    validate_table(table)?;
    if auto_migrate {
        for statement in T::ddl(table) {
            sqlx::query(AssertSqlSafe(statement.as_str()))
                .execute(&pool)
                .await
                .map_err(|e| format!("migration of table {table} failed: {e}"))?;
        }
    }
    Ok(Arc::new(PgRepository::<T> {
        pool,
        table: table.to_string(),
        _marker: PhantomData,
    }))
}

pub struct PgRepository<T> {
    pool: PgPool,
    table: String,
    _marker: PhantomData<fn() -> T>,
}

#[async_trait]
impl<T: Definition + PgEntity> Repository<T> for PgRepository<T> {
    async fn list(&self) -> Result<Vec<T>, StoreError> {
        T::load_all(&self.pool, &self.table).await
    }

    async fn get(&self, key: &str) -> Result<Option<T>, StoreError> {
        T::load_one(&self.pool, &self.table, key).await
    }

    async fn create(&self, item: &T) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        T::insert(&mut tx, &self.table, item).await?;
        tx.commit().await.map_err(db)
    }

    async fn update(&self, item: &T) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        if T::update(&mut tx, &self.table, item).await? == 0 {
            return Err(StoreError::NotFound {
                kind: T::KIND,
                id: item.key().to_string(),
            });
        }
        tx.commit().await.map_err(db)
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        if T::delete(&mut tx, &self.table, key).await? == 0 {
            return Err(StoreError::NotFound {
                kind: T::KIND,
                id: key.to_string(),
            });
        }
        tx.commit().await.map_err(db)
    }

    fn describe(&self) -> String {
        format!("postgres table {}", self.table)
    }
}

fn conflict<T: Definition>(item: &T) -> impl FnOnce(sqlx::Error) -> StoreError + '_ {
    move |e| {
        if is_unique_violation(&e) {
            StoreError::Conflict {
                kind: T::KIND,
                id: item.key().to_string(),
            }
        } else {
            db(e)
        }
    }
}

fn decode_err(table: &str) -> impl Fn(sqlx::Error) -> StoreError + '_ {
    move |e| StoreError::Format {
        location: format!("postgres table {table}"),
        message: e.to_string(),
    }
}

// --- rules -------------------------------------------------------------------

fn rule_from_row(row: &PgRow, table: &str) -> Result<Rule, StoreError> {
    let e = decode_err(table);
    let metadata: SqlJson<Json> = row.try_get("metadata").map_err(&e)?;
    Ok(Rule {
        id: row.try_get("id").map_err(&e)?,
        language: row.try_get("language").map_err(&e)?,
        source: row.try_get("source").map_err(&e)?,
        description: row.try_get("description").map_err(&e)?,
        message: row.try_get("message").map_err(&e)?,
        enabled: row.try_get("enabled").map_err(&e)?,
        metadata: match metadata.0 {
            Json::Object(map) => map,
            _ => Default::default(),
        },
    })
}

#[async_trait]
impl PgEntity for Rule {
    fn ddl(table: &str) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                id TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                source TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                message TEXT,
                enabled BOOLEAN NOT NULL DEFAULT TRUE,
                metadata JSONB NOT NULL DEFAULT '{{}}'::jsonb,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        )]
    }

    async fn load_all(pool: &PgPool, table: &str) -> Result<Vec<Self>, StoreError> {
        let rows = sqlx::query(sql!("SELECT * FROM {table} ORDER BY id"))
            .fetch_all(pool)
            .await
            .map_err(db)?;
        rows.iter().map(|r| rule_from_row(r, table)).collect()
    }

    async fn load_one(pool: &PgPool, table: &str, key: &str) -> Result<Option<Self>, StoreError> {
        let row = sqlx::query(sql!("SELECT * FROM {table} WHERE id = $1"))
            .bind(key)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
        row.map(|r| rule_from_row(&r, table)).transpose()
    }

    async fn insert(conn: &mut PgConnection, table: &str, r: &Self) -> Result<(), StoreError> {
        sqlx::query(sql!(
            "INSERT INTO {table} (id, language, source, description, message, enabled, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7)"
        ))
        .bind(&r.id)
        .bind(&r.language)
        .bind(&r.source)
        .bind(&r.description)
        .bind(&r.message)
        .bind(r.enabled)
        .bind(SqlJson(&r.metadata))
        .execute(conn)
        .await
        .map_err(conflict(r))?;
        Ok(())
    }

    async fn update(conn: &mut PgConnection, table: &str, r: &Self) -> Result<u64, StoreError> {
        let done = sqlx::query(sql!(
            "UPDATE {table} SET language = $2, source = $3, description = $4, message = $5,
             enabled = $6, metadata = $7, updated_at = now() WHERE id = $1"
        ))
        .bind(&r.id)
        .bind(&r.language)
        .bind(&r.source)
        .bind(&r.description)
        .bind(&r.message)
        .bind(r.enabled)
        .bind(SqlJson(&r.metadata))
        .execute(conn)
        .await
        .map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn delete(conn: &mut PgConnection, table: &str, key: &str) -> Result<u64, StoreError> {
        delete_by(conn, table, "id", key).await
    }
}

async fn delete_by(conn: &mut PgConnection, table: &str, column: &str, key: &str) -> Result<u64, StoreError> {
    let done = sqlx::query(sql!("DELETE FROM {table} WHERE {column} = $1"))
        .bind(key)
        .execute(conn)
        .await
        .map_err(db)?;
    Ok(done.rows_affected())
}

// --- rule groups --------------------------------------------------------------

fn members_table(table: &str) -> String {
    format!("{table}_members")
}

async fn write_members(conn: &mut PgConnection, table: &str, group: &RuleGroup) -> Result<(), StoreError> {
    let members = members_table(table);
    sqlx::query(sql!("DELETE FROM {members} WHERE group_name = $1"))
        .bind(&group.name)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
    let positions: Vec<i32> = (0..group.rules.len() as i32).collect();
    sqlx::query(sql!(
        "INSERT INTO {members} (group_name, rule_id, position)
         SELECT $1, rule_id, position FROM UNNEST($2::text[], $3::int[]) AS m(rule_id, position)
         ON CONFLICT DO NOTHING"
    ))
    .bind(&group.name)
    .bind(&group.rules)
    .bind(&positions)
    .execute(&mut *conn)
    .await
    .map_err(db)?;
    Ok(())
}

async fn load_groups(pool: &PgPool, table: &str, name: Option<&str>) -> Result<Vec<RuleGroup>, StoreError> {
    let members = members_table(table);
    let e = decode_err(table);
    let groups = sqlx::query(sql!(
        "SELECT name, description FROM {table} WHERE ($1::text IS NULL OR name = $1) ORDER BY name"
    ))
    .bind(name)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let rows = sqlx::query(sql!(
        "SELECT group_name, rule_id FROM {members} WHERE ($1::text IS NULL OR group_name = $1)
         ORDER BY group_name, position, rule_id"
    ))
    .bind(name)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    let mut out = Vec::with_capacity(groups.len());
    for g in &groups {
        out.push(RuleGroup {
            name: g.try_get("name").map_err(&e)?,
            description: g.try_get("description").map_err(&e)?,
            rules: Vec::new(),
        });
    }
    for row in &rows {
        let group: String = row.try_get("group_name").map_err(&e)?;
        let rule: String = row.try_get("rule_id").map_err(&e)?;
        if let Ok(i) = out.binary_search_by(|g| g.name.as_str().cmp(&group)) {
            out[i].rules.push(rule);
        }
    }
    Ok(out)
}

#[async_trait]
impl PgEntity for RuleGroup {
    fn ddl(table: &str) -> Vec<String> {
        let members = members_table(table);
        vec![
            format!(
                "CREATE TABLE IF NOT EXISTS {table} (
                    name TEXT PRIMARY KEY,
                    description TEXT NOT NULL DEFAULT '',
                    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {members} (
                    group_name TEXT NOT NULL REFERENCES {table}(name) ON DELETE CASCADE ON UPDATE CASCADE,
                    rule_id TEXT NOT NULL,
                    position INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (group_name, rule_id)
                )"
            ),
        ]
    }

    async fn load_all(pool: &PgPool, table: &str) -> Result<Vec<Self>, StoreError> {
        load_groups(pool, table, None).await
    }

    async fn load_one(pool: &PgPool, table: &str, key: &str) -> Result<Option<Self>, StoreError> {
        Ok(load_groups(pool, table, Some(key)).await?.pop())
    }

    async fn insert(conn: &mut PgConnection, table: &str, g: &Self) -> Result<(), StoreError> {
        sqlx::query(sql!("INSERT INTO {table} (name, description) VALUES ($1, $2)"))
            .bind(&g.name)
            .bind(&g.description)
            .execute(&mut *conn)
            .await
            .map_err(conflict(g))?;
        write_members(conn, table, g).await
    }

    async fn update(conn: &mut PgConnection, table: &str, g: &Self) -> Result<u64, StoreError> {
        let done = sqlx::query(sql!(
            "UPDATE {table} SET description = $2, updated_at = now() WHERE name = $1"
        ))
        .bind(&g.name)
        .bind(&g.description)
        .execute(&mut *conn)
        .await
        .map_err(db)?;
        if done.rows_affected() > 0 {
            write_members(conn, table, g).await?;
        }
        Ok(done.rows_affected())
    }

    async fn delete(conn: &mut PgConnection, table: &str, key: &str) -> Result<u64, StoreError> {
        delete_by(conn, table, "name", key).await
    }
}

// --- tests --------------------------------------------------------------------

fn test_from_row(row: &PgRow, table: &str) -> Result<TestCase, StoreError> {
    let e = decode_err(table);
    let scope: SqlJson<Json> = row.try_get("scope").map_err(&e)?;
    let expect: SqlJson<Json> = row.try_get("expect").map_err(&e)?;
    let record: SqlJson<Json> = row.try_get("record").map_err(&e)?;
    let id: String = row.try_get("id").map_err(&e)?;
    let field = |name: &str, err: serde_json::Error| StoreError::Format {
        location: format!("postgres table {table}, test '{id}', column {name}"),
        message: err.to_string(),
    };
    Ok(TestCase {
        description: row.try_get("description").map_err(&e)?,
        tags: row.try_get("tags").map_err(&e)?,
        record: record.0,
        scope: serde_json::from_value(scope.0).map_err(|err| field("scope", err))?,
        expect: serde_json::from_value(expect.0).map_err(|err| field("expect", err))?,
        id,
    })
}

#[async_trait]
impl PgEntity for TestCase {
    fn ddl(table: &str) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                id TEXT PRIMARY KEY,
                description TEXT NOT NULL DEFAULT '',
                tags TEXT[] NOT NULL DEFAULT '{{}}',
                record JSONB NOT NULL,
                scope JSONB NOT NULL DEFAULT '{{\"type\": \"all\"}}'::jsonb,
                expect JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        )]
    }

    async fn load_all(pool: &PgPool, table: &str) -> Result<Vec<Self>, StoreError> {
        let rows = sqlx::query(sql!("SELECT * FROM {table} ORDER BY id"))
            .fetch_all(pool)
            .await
            .map_err(db)?;
        rows.iter().map(|r| test_from_row(r, table)).collect()
    }

    async fn load_one(pool: &PgPool, table: &str, key: &str) -> Result<Option<Self>, StoreError> {
        let row = sqlx::query(sql!("SELECT * FROM {table} WHERE id = $1"))
            .bind(key)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
        row.map(|r| test_from_row(&r, table)).transpose()
    }

    async fn insert(conn: &mut PgConnection, table: &str, t: &Self) -> Result<(), StoreError> {
        sqlx::query(sql!(
            "INSERT INTO {table} (id, description, tags, record, scope, expect) VALUES ($1, $2, $3, $4, $5, $6)"
        ))
        .bind(&t.id)
        .bind(&t.description)
        .bind(&t.tags)
        .bind(SqlJson(&t.record))
        .bind(SqlJson(&t.scope))
        .bind(SqlJson(&t.expect))
        .execute(conn)
        .await
        .map_err(conflict(t))?;
        Ok(())
    }

    async fn update(conn: &mut PgConnection, table: &str, t: &Self) -> Result<u64, StoreError> {
        let done = sqlx::query(sql!(
            "UPDATE {table} SET description = $2, tags = $3, record = $4, scope = $5, expect = $6,
             updated_at = now() WHERE id = $1"
        ))
        .bind(&t.id)
        .bind(&t.description)
        .bind(&t.tags)
        .bind(SqlJson(&t.record))
        .bind(SqlJson(&t.scope))
        .bind(SqlJson(&t.expect))
        .execute(conn)
        .await
        .map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn delete(conn: &mut PgConnection, table: &str, key: &str) -> Result<u64, StoreError> {
        delete_by(conn, table, "id", key).await
    }
}

// --- configuration ----------------------------------------------------------------

fn config_from_row(row: &PgRow, table: &str) -> Result<ConfigEntry, StoreError> {
    let e = decode_err(table);
    let key: String = row.try_get("key").map_err(&e)?;
    let scope: String = row.try_get("scope").map_err(&e)?;
    let value: SqlJson<Json> = row.try_get("value").map_err(&e)?;
    Ok(ConfigEntry {
        scope: ConfigScope::parse(&scope).ok_or_else(|| StoreError::Format {
            location: format!("postgres table {table}, key '{key}'"),
            message: format!("unknown scope '{scope}'"),
        })?,
        key,
        value: value.0,
        description: row.try_get("description").map_err(&e)?,
    })
}

#[async_trait]
impl PgEntity for ConfigEntry {
    fn ddl(table: &str) -> Vec<String> {
        vec![format!(
            "CREATE TABLE IF NOT EXISTS {table} (
                key TEXT PRIMARY KEY,
                value JSONB NOT NULL,
                scope TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"
        )]
    }

    async fn load_all(pool: &PgPool, table: &str) -> Result<Vec<Self>, StoreError> {
        let rows = sqlx::query(sql!("SELECT * FROM {table} ORDER BY key"))
            .fetch_all(pool)
            .await
            .map_err(db)?;
        rows.iter().map(|r| config_from_row(r, table)).collect()
    }

    async fn load_one(pool: &PgPool, table: &str, key: &str) -> Result<Option<Self>, StoreError> {
        let row = sqlx::query(sql!("SELECT * FROM {table} WHERE key = $1"))
            .bind(key)
            .fetch_optional(pool)
            .await
            .map_err(db)?;
        row.map(|r| config_from_row(&r, table)).transpose()
    }

    async fn insert(conn: &mut PgConnection, table: &str, c: &Self) -> Result<(), StoreError> {
        sqlx::query(sql!(
            "INSERT INTO {table} (key, value, scope, description) VALUES ($1, $2, $3, $4)"
        ))
        .bind(&c.key)
        .bind(SqlJson(&c.value))
        .bind(c.scope.as_str())
        .bind(&c.description)
        .execute(conn)
        .await
        .map_err(conflict(c))?;
        Ok(())
    }

    async fn update(conn: &mut PgConnection, table: &str, c: &Self) -> Result<u64, StoreError> {
        let done = sqlx::query(sql!(
            "UPDATE {table} SET value = $2, scope = $3, description = $4, updated_at = now() WHERE key = $1"
        ))
        .bind(&c.key)
        .bind(SqlJson(&c.value))
        .bind(c.scope.as_str())
        .bind(&c.description)
        .execute(conn)
        .await
        .map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn delete(conn: &mut PgConnection, table: &str, key: &str) -> Result<u64, StoreError> {
        delete_by(conn, table, "key", key).await
    }
}

// --- executions -------------------------------------------------------------------

pub struct PgExecutions {
    pool: PgPool,
    table: String,
}

impl PgExecutions {
    pub async fn open(pool: PgPool, table: &str, auto_migrate: bool) -> Result<Self, String> {
        validate_table(table)?;
        if auto_migrate {
            let index = table.replace('.', "_");
            for statement in [
                format!(
                    "CREATE TABLE IF NOT EXISTS {table} (
                        id UUID PRIMARY KEY,
                        status TEXT NOT NULL,
                        created_at TIMESTAMPTZ NOT NULL,
                        finished_at TIMESTAMPTZ,
                        document JSONB NOT NULL
                    )"
                ),
                format!(
                    "CREATE INDEX IF NOT EXISTS {index}_unfinished_idx ON {table} (status)
                     WHERE status IN ('pending', 'running')"
                ),
                format!("CREATE INDEX IF NOT EXISTS {index}_finished_idx ON {table} (finished_at)"),
            ] {
                sqlx::query(AssertSqlSafe(statement.as_str()))
                    .execute(&pool)
                    .await
                    .map_err(|e| format!("migration of table {table} failed: {e}"))?;
            }
        }
        Ok(PgExecutions {
            pool,
            table: table.to_string(),
        })
    }
}

#[async_trait]
impl ExecutionRepository for PgExecutions {
    async fn insert(&self, e: &Execution) -> Result<(), StoreError> {
        sqlx::query(sql!(
            "INSERT INTO {} (id, status, created_at, finished_at, document) VALUES ($1, $2, $3, $4, $5)",
            self.table
        ))
        .bind(e.execution_id)
        .bind(e.status.as_str())
        .bind(e.created_at)
        .bind(e.finished_at)
        .bind(SqlJson(e))
        .execute(&self.pool)
        .await
        .map_err(|err| {
            if is_unique_violation(&err) {
                StoreError::Conflict {
                    kind: "execution",
                    id: e.execution_id.to_string(),
                }
            } else {
                db(err)
            }
        })?;
        Ok(())
    }

    async fn update(&self, e: &Execution) -> Result<(), StoreError> {
        let done = sqlx::query(sql!(
            "UPDATE {} SET status = $2, finished_at = $3, document = $4 WHERE id = $1",
            self.table
        ))
        .bind(e.execution_id)
        .bind(e.status.as_str())
        .bind(e.finished_at)
        .bind(SqlJson(e))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if done.rows_affected() == 0 {
            return Err(StoreError::NotFound {
                kind: "execution",
                id: e.execution_id.to_string(),
            });
        }
        Ok(())
    }

    async fn get(&self, id: Uuid) -> Result<Option<Execution>, StoreError> {
        let row = sqlx::query(sql!("SELECT document FROM {} WHERE id = $1", self.table))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db)?;
        match row {
            None => Ok(None),
            Some(row) => {
                let doc: SqlJson<Execution> = row.try_get("document").map_err(decode_err(&self.table))?;
                Ok(Some(doc.0))
            }
        }
    }

    async fn fail_unfinished(&self, error: &ExecutionError, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let patch = serde_json::json!({
            "status": ExecutionStatus::Failed,
            "finished_at": now,
            "error": error,
        });
        let done = sqlx::query(sql!(
            "UPDATE {} SET status = 'failed', finished_at = $1, document = document || $2
             WHERE status IN ('pending', 'running')",
            self.table
        ))
        .bind(now)
        .bind(SqlJson(patch))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(done.rows_affected())
    }

    async fn purge_finished_before(&self, cutoff: DateTime<Utc>) -> Result<u64, StoreError> {
        let done = sqlx::query(sql!(
            "DELETE FROM {} WHERE status IN ('completed', 'failed') AND finished_at < $1",
            self.table
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(done.rows_affected())
    }

    fn describe(&self) -> String {
        format!("postgres table {}", self.table)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn table_names() {
        assert!(super::validate_table("drover_rules").is_ok());
        assert!(super::validate_table("app.drover_rules").is_ok());
        assert!(super::validate_table("rules; DROP TABLE x").is_err());
        assert!(super::validate_table("a.b.c").is_err());
        assert!(super::validate_table("1abc").is_err());
    }
}
