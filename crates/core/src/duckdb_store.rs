use std::{path::Path, sync::Mutex};

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use duckdb::{Connection, params};

use crate::state::{StateError, StateStore, VectorBucketRow};

/// DuckDB-backed [`StateStore`].
///
/// Wraps a single [`Connection`] in a `Mutex` because DuckDB connections
/// are not `Sync`. We hold the mutex for the duration of each call;
/// state operations are short and the spike's throughput target is "it
/// works", not "it scales".
pub struct DuckDbStateStore {
    conn: Mutex<Connection>,
}

impl DuckDbStateStore {
    /// Open the state database at `path`, creating parent directories if
    /// missing, and run the in-line schema migration. Idempotent.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create state dir {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open duckdb at {}", path.display()))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory variant — used by unit tests so they don't touch disk.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory().context("open in-memory duckdb")?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn migrate(conn: &Connection) -> Result<(), StateError> {
    conn.execute_batch(
        r#"
        CREATE SCHEMA IF NOT EXISTS state;
        CREATE TABLE IF NOT EXISTS state.vector_buckets (
            name        VARCHAR PRIMARY KEY,
            arn         VARCHAR NOT NULL,
            created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        "#,
    )
    .context("migrate state schema")?;
    Ok(())
}

impl StateStore for DuckDbStateStore {
    fn create_vector_bucket(&self, name: &str, arn: &str) -> Result<VectorBucketRow, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let now = Utc::now().naive_utc();

        let res = conn.execute(
            "INSERT INTO state.vector_buckets (name, arn, created_at) VALUES (?, ?, ?)",
            params![name, arn, now],
        );

        match res {
            Ok(_) => Ok(VectorBucketRow {
                name: name.to_owned(),
                arn: arn.to_owned(),
                created_at: DateTime::<Utc>::from_naive_utc_and_offset(now, Utc),
            }),
            Err(e) if is_duplicate_key(&e) => Err(StateError::AlreadyExists(name.to_owned())),
            Err(e) => Err(StateError::Internal(
                anyhow::Error::new(e).context("insert vector_bucket row"),
            )),
        }
    }

    fn list_vector_buckets(&self) -> Result<Vec<VectorBucketRow>, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT name, arn, created_at FROM state.vector_buckets ORDER BY name")
            .context("prepare list")?;
        let rows = stmt
            .query_map([], row_to_bucket)
            .context("execute list")?
            .collect::<Result<Vec<_>, _>>()
            .context("collect list")?;
        Ok(rows)
    }

    fn delete_vector_bucket(&self, name: &str) -> Result<(), StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let n = conn
            .execute(
                "DELETE FROM state.vector_buckets WHERE name = ?",
                params![name],
            )
            .context("delete vector_bucket row")?;
        if n == 0 {
            return Err(StateError::NotFound(name.to_owned()));
        }
        Ok(())
    }
}

fn row_to_bucket(row: &duckdb::Row<'_>) -> duckdb::Result<VectorBucketRow> {
    let name: String = row.get(0)?;
    let arn: String = row.get(1)?;
    let created_naive: NaiveDateTime = row.get(2)?;
    Ok(VectorBucketRow {
        name,
        arn,
        created_at: DateTime::<Utc>::from_naive_utc_and_offset(created_naive, Utc),
    })
}

fn is_duplicate_key(e: &duckdb::Error) -> bool {
    // DuckDB returns a `ConstraintViolation` error variant for unique
    // and primary-key violations. We don't try to distinguish PRIMARY
    // KEY from UNIQUE — only one constraint exists on this table.
    matches!(
        e,
        duckdb::Error::DuckDBFailure(_, Some(msg)) if msg.contains("PRIMARY KEY") || msg.contains("UNIQUE") || msg.contains("Constraint")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_list_round_trips() {
        let store = DuckDbStateStore::in_memory().unwrap();
        let row = store
            .create_vector_bucket("alpha", "arn:aws:s3vectors:eu-west-1:0:bucket/alpha")
            .unwrap();
        assert_eq!(row.name, "alpha");

        let listed = store.list_vector_buckets().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "alpha");
    }

    #[test]
    fn duplicate_name_is_already_exists() {
        let store = DuckDbStateStore::in_memory().unwrap();
        let arn = "arn:aws:s3vectors:eu-west-1:0:bucket/beta";
        store.create_vector_bucket("beta", arn).unwrap();
        let err = store.create_vector_bucket("beta", arn).unwrap_err();
        assert!(matches!(err, StateError::AlreadyExists(n) if n == "beta"));
    }

    #[test]
    fn delete_then_list_is_empty() {
        let store = DuckDbStateStore::in_memory().unwrap();
        store
            .create_vector_bucket("gamma", "arn:aws:s3vectors:eu-west-1:0:bucket/gamma")
            .unwrap();
        store.delete_vector_bucket("gamma").unwrap();
        assert!(store.list_vector_buckets().unwrap().is_empty());
    }

    #[test]
    fn delete_missing_is_not_found() {
        let store = DuckDbStateStore::in_memory().unwrap();
        let err = store.delete_vector_bucket("missing").unwrap_err();
        assert!(matches!(err, StateError::NotFound(n) if n == "missing"));
    }
}
