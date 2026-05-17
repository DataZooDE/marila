use std::{path::Path, sync::Mutex};

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use duckdb::{Connection, params};

use crate::state::{StateError, StateStore, VectorBucketPage, VectorBucketRow};

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

    fn list_vector_buckets_page(
        &self,
        prefix: Option<&str>,
        after: Option<&str>,
        max: usize,
    ) -> Result<VectorBucketPage, StateError> {
        // Fetch max+1 rows so we can detect whether more remain without
        // a separate COUNT(*) trip. The extra row is dropped from the
        // returned page and its name becomes the next cursor.
        let limit = max.saturating_add(1) as i64;

        let conn = self.conn.lock().expect("state mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT name, arn, created_at
                   FROM state.vector_buckets
                  WHERE (? IS NULL OR name LIKE ? || '%')
                    AND (? IS NULL OR name > ?)
                  ORDER BY name
                  LIMIT ?",
            )
            .context("prepare list page")?;

        let mut rows: Vec<VectorBucketRow> = stmt
            .query_map(
                params![prefix, prefix, after, after, limit],
                row_to_bucket,
            )
            .context("execute list page")?
            .collect::<Result<Vec<_>, _>>()
            .context("collect list page")?;

        let next = if rows.len() as i64 > max as i64 {
            // Drop the probe row; keep the page short.
            rows.truncate(max);
            rows.last().map(|r| r.name.clone())
        } else {
            None
        };

        Ok(VectorBucketPage { rows, next })
    }

    fn get_vector_bucket(&self, name: &str) -> Result<VectorBucketRow, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let result = conn.query_row(
            "SELECT name, arn, created_at FROM state.vector_buckets WHERE name = ?",
            params![name],
            row_to_bucket,
        );
        match result {
            Ok(row) => Ok(row),
            Err(duckdb::Error::QueryReturnedNoRows) => Err(StateError::NotFound(name.to_owned())),
            Err(e) => Err(StateError::Internal(
                anyhow::Error::new(e).context("select vector_bucket"),
            )),
        }
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

    fn fresh() -> DuckDbStateStore {
        DuckDbStateStore::in_memory().unwrap()
    }

    fn seed(store: &DuckDbStateStore, names: &[&str]) {
        for n in names {
            store
                .create_vector_bucket(n, &format!("arn:aws:s3vectors:eu-west-1:0:bucket/{n}"))
                .unwrap();
        }
    }

    #[test]
    fn create_then_get_round_trips() {
        let store = fresh();
        seed(&store, &["alpha"]);
        let row = store.get_vector_bucket("alpha").unwrap();
        assert_eq!(row.name, "alpha");
    }

    #[test]
    fn duplicate_name_is_already_exists() {
        let store = fresh();
        seed(&store, &["beta"]);
        let err = store
            .create_vector_bucket("beta", "arn:aws:s3vectors:eu-west-1:0:bucket/beta")
            .unwrap_err();
        assert!(matches!(err, StateError::AlreadyExists(n) if n == "beta"));
    }

    #[test]
    fn delete_then_get_is_not_found() {
        let store = fresh();
        seed(&store, &["gamma"]);
        store.delete_vector_bucket("gamma").unwrap();
        let err = store.get_vector_bucket("gamma").unwrap_err();
        assert!(matches!(err, StateError::NotFound(n) if n == "gamma"));
    }

    #[test]
    fn delete_missing_is_not_found() {
        let store = fresh();
        let err = store.delete_vector_bucket("missing").unwrap_err();
        assert!(matches!(err, StateError::NotFound(n) if n == "missing"));
    }

    #[test]
    fn list_page_no_prefix_no_cursor_returns_everything_in_name_order() {
        let store = fresh();
        seed(&store, &["c", "a", "b"]);
        let page = store.list_vector_buckets_page(None, None, 10).unwrap();
        assert_eq!(
            page.rows.iter().map(|r| &*r.name).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(page.next.is_none(), "single page should not have a cursor");
    }

    #[test]
    fn list_page_prefix_filters() {
        let store = fresh();
        seed(&store, &["foo-1", "foo-2", "bar-1"]);
        let page = store
            .list_vector_buckets_page(Some("foo-"), None, 10)
            .unwrap();
        assert_eq!(page.rows.len(), 2);
        assert!(
            page.rows.iter().all(|r| r.name.starts_with("foo-")),
            "prefix filter must exclude non-matching rows"
        );
    }

    #[test]
    fn list_page_cursor_round_trip_yields_disjoint_pages() {
        let store = fresh();
        seed(&store, &["a", "b", "c", "d", "e"]);

        let page1 = store.list_vector_buckets_page(None, None, 2).unwrap();
        assert_eq!(page1.rows.len(), 2);
        assert_eq!(page1.next.as_deref(), Some("b"));

        let page2 = store
            .list_vector_buckets_page(None, page1.next.as_deref(), 2)
            .unwrap();
        assert_eq!(page2.rows.len(), 2);
        assert_eq!(page2.next.as_deref(), Some("d"));

        let page3 = store
            .list_vector_buckets_page(None, page2.next.as_deref(), 2)
            .unwrap();
        assert_eq!(page3.rows.len(), 1);
        assert!(page3.next.is_none());
    }
}
