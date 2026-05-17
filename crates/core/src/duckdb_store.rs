use std::{path::Path, sync::Mutex};

use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use duckdb::{Connection, params};

use crate::{
    state::{
        DistanceMetric, IndexPage, IndexRow, StateError, StateStore, VectorBucketPage,
        VectorBucketRow,
    },
    vss,
};

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
        prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory variant — used by unit tests so they don't touch disk.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory().context("open in-memory duckdb")?;
        prepare(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

fn prepare(conn: &Connection) -> Result<(), StateError> {
    migrate(conn)?;
    // Loading VSS is a soft requirement: the bucket-only operations
    // don't need it. If we ever go air-gapped before the cache is
    // primed, `create_index` will surface the real error.
    let _ = vss::enable_vss(conn);
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), StateError> {
    conn.execute_batch(
        r#"
        CREATE SCHEMA IF NOT EXISTS state;
        CREATE SCHEMA IF NOT EXISTS vec_data;
        CREATE TABLE IF NOT EXISTS state.vector_buckets (
            name        VARCHAR PRIMARY KEY,
            arn         VARCHAR NOT NULL,
            created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS state.vector_indexes (
            bucket_name      VARCHAR NOT NULL,
            name             VARCHAR NOT NULL,
            arn              VARCHAR NOT NULL,
            dimension        INTEGER NOT NULL,
            distance_metric  VARCHAR NOT NULL,
            created_at       TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (bucket_name, name)
        );
        "#,
    )
    .context("migrate state schema")?;
    Ok(())
}

/// Build the DuckDB identifier for an index's backing table.
///
/// AWS validates bucket/index names to `[a-z0-9][a-z0-9-.]{1,61}[a-z0-9]`,
/// so the only awkward characters are `-` and `.`. Replace both with
/// `_` and join with `__` so we can round-trip the pair from the table
/// name if we ever need to.
fn backing_table_ident(bucket: &str, index: &str) -> String {
    let sanitize = |s: &str| s.replace(['-', '.'], "_");
    format!("vec_data.\"{}__{}\"", sanitize(bucket), sanitize(index))
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
            .query_map(params![prefix, prefix, after, after, limit], row_to_bucket)
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

    fn create_index(
        &self,
        bucket: &str,
        index: &str,
        arn: &str,
        dimension: u32,
        distance_metric: DistanceMetric,
    ) -> Result<IndexRow, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");

        // 1. Bucket must exist — surfaces NotFoundException upstream
        //    without ever touching the indexes table.
        let bucket_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM state.vector_buckets WHERE name = ?",
                params![bucket],
                |r| r.get(0),
            )
            .context("probe bucket presence")?;
        if bucket_present == 0 {
            return Err(StateError::NotFound(bucket.to_owned()));
        }

        // 2. Try to insert the index state row first — the composite PK
        //    is what enforces ConflictException on duplicates.
        let now = Utc::now().naive_utc();
        let metric_wire = distance_metric.as_wire();
        let res = conn.execute(
            "INSERT INTO state.vector_indexes
                 (bucket_name, name, arn, dimension, distance_metric, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![bucket, index, arn, dimension, metric_wire, now],
        );
        match res {
            Ok(_) => {}
            Err(e) if is_duplicate_key(&e) => {
                return Err(StateError::AlreadyExists(format!("{bucket}/{index}")));
            }
            Err(e) => {
                return Err(StateError::Internal(
                    anyhow::Error::new(e).context("insert vector_index row"),
                ));
            }
        }

        // 3. Materialise the backing table and the HNSW index. If this
        //    fails after the state row is in, roll back the row so a
        //    retry sees the same "doesn't exist" outcome the client
        //    started with.
        let table = backing_table_ident(bucket, index);
        let create_table = format!(
            "CREATE TABLE {table} (key VARCHAR PRIMARY KEY, vec FLOAT[{dimension}], meta JSON);",
        );
        // We synthesise the index-name from a hash-safe form because
        // DuckDB doesn't allow `.` in identifiers and the backing-table
        // ident already contains a `.`.
        let hnsw_ident = format!(
            "\"hnsw_{}__{}\"",
            bucket.replace(['-', '.'], "_"),
            index.replace(['-', '.'], "_")
        );
        let create_index = format!(
            "CREATE INDEX {hnsw_ident} ON {table} USING HNSW (vec) WITH (metric = '{}');",
            match distance_metric {
                DistanceMetric::Cosine => "cosine",
                DistanceMetric::Euclidean => "l2sq",
            }
        );

        let materialise = conn
            .execute_batch(&create_table)
            .and_then(|()| conn.execute_batch(&create_index));
        if let Err(e) = materialise {
            // Rollback so the retry has a clean slate.
            let _ = conn.execute(
                "DELETE FROM state.vector_indexes WHERE bucket_name = ? AND name = ?",
                params![bucket, index],
            );
            return Err(StateError::Internal(
                anyhow::Error::new(e).context("create backing table + hnsw index"),
            ));
        }

        Ok(IndexRow {
            bucket_name: bucket.to_owned(),
            name: index.to_owned(),
            arn: arn.to_owned(),
            dimension,
            distance_metric,
            created_at: DateTime::<Utc>::from_naive_utc_and_offset(now, Utc),
        })
    }

    fn list_indexes_page(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        after: Option<&str>,
        max: usize,
    ) -> Result<IndexPage, StateError> {
        let limit = max.saturating_add(1) as i64;
        let conn = self.conn.lock().expect("state mutex poisoned");

        // Bucket existence check matches the AWS contract: listing
        // indexes in a non-existent bucket is NotFoundException, not
        // an empty page.
        let bucket_present: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM state.vector_buckets WHERE name = ?",
                params![bucket],
                |r| r.get(0),
            )
            .context("probe bucket presence")?;
        if bucket_present == 0 {
            return Err(StateError::NotFound(bucket.to_owned()));
        }

        let mut stmt = conn
            .prepare(
                "SELECT bucket_name, name, arn, dimension, distance_metric, created_at
                   FROM state.vector_indexes
                  WHERE bucket_name = ?
                    AND (? IS NULL OR name LIKE ? || '%')
                    AND (? IS NULL OR name > ?)
                  ORDER BY name
                  LIMIT ?",
            )
            .context("prepare list_indexes page")?;

        let mut rows: Vec<IndexRow> = stmt
            .query_map(
                params![bucket, prefix, prefix, after, after, limit],
                row_to_index,
            )
            .context("execute list_indexes page")?
            .collect::<Result<Vec<_>, _>>()
            .context("collect list_indexes page")?;

        let next = if rows.len() as i64 > max as i64 {
            rows.truncate(max);
            rows.last().map(|r| r.name.clone())
        } else {
            None
        };

        Ok(IndexPage { rows, next })
    }

    fn get_index(&self, bucket: &str, index: &str) -> Result<IndexRow, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let result = conn.query_row(
            "SELECT bucket_name, name, arn, dimension, distance_metric, created_at
               FROM state.vector_indexes
              WHERE bucket_name = ? AND name = ?",
            params![bucket, index],
            row_to_index,
        );
        match result {
            Ok(row) => Ok(row),
            Err(duckdb::Error::QueryReturnedNoRows) => {
                Err(StateError::NotFound(format!("{bucket}/{index}")))
            }
            Err(e) => Err(StateError::Internal(
                anyhow::Error::new(e).context("select vector_index"),
            )),
        }
    }

    fn count_indexes(&self, bucket: &str) -> Result<u64, StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM state.vector_indexes WHERE bucket_name = ?",
                params![bucket],
                |r| r.get(0),
            )
            .context("count indexes")?;
        Ok(n as u64)
    }

    fn delete_index(&self, bucket: &str, index: &str) -> Result<(), StateError> {
        let conn = self.conn.lock().expect("state mutex poisoned");
        let n = conn
            .execute(
                "DELETE FROM state.vector_indexes WHERE bucket_name = ? AND name = ?",
                params![bucket, index],
            )
            .context("delete vector_index row")?;
        if n == 0 {
            return Err(StateError::NotFound(format!("{bucket}/{index}")));
        }

        // Drop the backing table best-effort: if it's missing we don't
        // complain because the state row is already gone.
        let table = backing_table_ident(bucket, index);
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"));
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

fn row_to_index(row: &duckdb::Row<'_>) -> duckdb::Result<IndexRow> {
    let bucket_name: String = row.get(0)?;
    let name: String = row.get(1)?;
    let arn: String = row.get(2)?;
    let dimension: i32 = row.get(3)?;
    let metric_wire: String = row.get(4)?;
    let created_naive: NaiveDateTime = row.get(5)?;
    // Data corruption — an unrecognised metric — falls back to cosine
    // so we never panic inside a row mapper. Upstream tests will catch
    // the divergence.
    let distance_metric = DistanceMetric::from_wire(&metric_wire).unwrap_or(DistanceMetric::Cosine);
    Ok(IndexRow {
        bucket_name,
        name,
        arn,
        dimension: dimension as u32,
        distance_metric,
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

    fn make_index(store: &DuckDbStateStore, bucket: &str, index: &str, dim: u32) -> IndexRow {
        store
            .create_index(
                bucket,
                index,
                &format!("arn:aws:s3vectors:eu-west-1:0:bucket/{bucket}/index/{index}"),
                dim,
                DistanceMetric::Cosine,
            )
            .unwrap()
    }

    #[test]
    fn create_index_requires_bucket() {
        let store = fresh();
        let err = store
            .create_index(
                "ghost",
                "idx",
                "arn:aws:s3vectors:eu-west-1:0:bucket/ghost/index/idx",
                4,
                DistanceMetric::Cosine,
            )
            .unwrap_err();
        assert!(matches!(err, StateError::NotFound(n) if n == "ghost"));
    }

    #[test]
    fn create_index_then_count_then_delete() {
        let store = fresh();
        seed(&store, &["b"]);
        assert_eq!(store.count_indexes("b").unwrap(), 0);
        make_index(&store, "b", "i", 4);
        assert_eq!(store.count_indexes("b").unwrap(), 1);
        store.delete_index("b", "i").unwrap();
        assert_eq!(store.count_indexes("b").unwrap(), 0);
    }

    #[test]
    fn duplicate_index_is_already_exists() {
        let store = fresh();
        seed(&store, &["b"]);
        make_index(&store, "b", "i", 4);
        let err = store
            .create_index(
                "b",
                "i",
                "arn:aws:s3vectors:eu-west-1:0:bucket/b/index/i",
                4,
                DistanceMetric::Cosine,
            )
            .unwrap_err();
        assert!(matches!(err, StateError::AlreadyExists(s) if s == "b/i"));
    }

    #[test]
    fn delete_missing_index_is_not_found() {
        let store = fresh();
        seed(&store, &["b"]);
        let err = store.delete_index("b", "ghost").unwrap_err();
        assert!(matches!(err, StateError::NotFound(s) if s == "b/ghost"));
    }

    #[test]
    fn get_index_round_trip_and_missing() {
        let store = fresh();
        seed(&store, &["b"]);
        make_index(&store, "b", "i", 4);
        let got = store.get_index("b", "i").unwrap();
        assert_eq!(got.name, "i");
        assert_eq!(got.dimension, 4);
        assert_eq!(got.distance_metric, DistanceMetric::Cosine);

        let err = store.get_index("b", "ghost").unwrap_err();
        assert!(matches!(err, StateError::NotFound(s) if s == "b/ghost"));
    }

    #[test]
    fn list_indexes_page_requires_bucket() {
        let store = fresh();
        let err = store
            .list_indexes_page("nobucket", None, None, 10)
            .unwrap_err();
        assert!(matches!(err, StateError::NotFound(s) if s == "nobucket"));
    }

    #[test]
    fn list_indexes_page_prefix_and_cursor() {
        let store = fresh();
        seed(&store, &["b"]);
        for n in ["alpha", "alphabeta", "gamma"] {
            make_index(&store, "b", n, 4);
        }

        let prefixed = store
            .list_indexes_page("b", Some("alpha"), None, 10)
            .unwrap();
        assert_eq!(prefixed.rows.len(), 2);
        assert!(prefixed.rows.iter().all(|r| r.name.starts_with("alpha")));

        let p1 = store.list_indexes_page("b", None, None, 2).unwrap();
        assert_eq!(p1.rows.len(), 2);
        assert_eq!(p1.next.as_deref(), Some("alphabeta"));

        let p2 = store
            .list_indexes_page("b", None, p1.next.as_deref(), 2)
            .unwrap();
        assert_eq!(p2.rows.len(), 1);
        assert_eq!(p2.rows[0].name, "gamma");
        assert!(p2.next.is_none());
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
