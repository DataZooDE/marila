//! DuckDB VSS extension wiring.
//!
//! Marila uses DuckDB's `vss` extension for HNSW indexes over backing
//! tables `vec_<bucket>__<index> (key VARCHAR PRIMARY KEY, vec FLOAT[N], meta JSON)`.
//! See `doc/DISCOVERIES.md` D-6 / D-11 for the persistence caveats.
//!
//! This module is responsible *only* for installing + loading the
//! extension and enabling HNSW persistence on a [`duckdb::Connection`].
//! The actual index lifecycle (CREATE / DROP) lives in
//! [`crate::duckdb_store`] alongside the rest of the state schema.

use anyhow::Context;
use duckdb::Connection;
use tracing::{debug, warn};

/// INSTALL + LOAD `vss`; enable experimental HNSW persistence so HNSW
/// indexes survive a process restart on file-backed DBs.
///
/// **Network**: the first call after extension cache wipe will reach out
/// to DuckDB's extension repository. We log and continue on failure so a
/// fully air-gapped boot doesn't take the whole binary down; the first
/// CREATE INDEX call will then surface a clearer error.
pub fn enable_vss(conn: &Connection) -> anyhow::Result<()> {
    if let Err(e) = conn.execute_batch("INSTALL vss; LOAD vss;") {
        warn!(error = %e, "INSTALL/LOAD vss failed — first CREATE INDEX will surface the real error");
        return Err(anyhow::Error::new(e).context("install/load vss"));
    }
    conn.execute_batch("SET hnsw_enable_experimental_persistence = true;")
        .context("enable hnsw persistence")?;
    debug!("vss extension loaded; hnsw_enable_experimental_persistence = true");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheapest possible proof that the extension wiring works end-to-end:
    /// install + load, create a tiny table with a FLOAT[N] column, build
    /// an HNSW index, run a nearest-neighbour query.
    #[test]
    fn install_load_and_hnsw_round_trip() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        enable_vss(&conn).expect("enable vss");

        conn.execute_batch(
            r#"
            CREATE TABLE t (key VARCHAR PRIMARY KEY, vec FLOAT[4]);
            INSERT INTO t VALUES
                ('anchor', [1.0, 0.0, 0.0, 0.0]),
                ('near',   [0.9, 0.1, 0.0, 0.0]),
                ('far',    [0.0, 0.0, 0.0, 1.0]);
            CREATE INDEX t_hnsw ON t USING HNSW (vec) WITH (metric = 'cosine');
            "#,
        )
        .expect("seed + create hnsw index");

        // Top-1 nearest to anchor should be anchor itself.
        let top: String = conn
            .query_row(
                "SELECT key FROM t ORDER BY array_cosine_distance(vec, [1.0, 0.0, 0.0, 0.0]::FLOAT[4]) LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("knn query");
        assert_eq!(top, "anchor");
    }
}
