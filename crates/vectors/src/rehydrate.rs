//! Rebuild DuckDB backing-table rows from the RustFS JSON snapshots
//! (REQUIREMENTS.md FV-4 — "RustFS is the durable source of truth").
//!
//! Called once on engine open, after [`marila_core::DuckDbStateStore`]
//! has migrated its schema. For each index already known to state,
//! walk `<bucket>/<index>/*.json` and `INSERT OR REPLACE` the backing
//! table — the rows in DuckDB are then a strict superset of what we
//! had before the .duckdb file was lost.
//!
//! New indexes (state row missing) are *not* re-created here — the
//! caller would have no way to know the index's dimension / metric
//! from a stray snapshot. That edge case lands later if a real
//! disaster-recovery story is needed.

use anyhow::Context;
use marila_core::{StateStore, VectorWrite};
use marila_storage::BucketStore;
use serde::Deserialize;
use tracing::{debug, info, warn};

#[derive(Debug, Deserialize)]
struct SnapshotBody {
    key: String,
    data: Vec<f32>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// Walk every known (bucket, index) in `state` and replay each
/// snapshot in RustFS into the backing table. Returns the total
/// number of vectors restored.
pub async fn rehydrate_from_snapshots(
    state: &dyn StateStore,
    storage: &dyn BucketStore,
) -> anyhow::Result<usize> {
    // Page through every bucket; for each, page through every index;
    // for each index, page through every snapshot key.
    let mut total = 0usize;
    let mut bucket_after: Option<String> = None;
    loop {
        let bucket_page = state
            .list_vector_buckets_page(None, bucket_after.as_deref(), 100)
            .context("list buckets for rehydrate")?;
        for bucket_row in &bucket_page.rows {
            total += rehydrate_bucket(state, storage, &bucket_row.name).await?;
        }
        if bucket_page.next.is_none() {
            break;
        }
        bucket_after = bucket_page.next;
    }
    info!(total_restored = total, "rehydrate complete");
    Ok(total)
}

async fn rehydrate_bucket(
    state: &dyn StateStore,
    storage: &dyn BucketStore,
    bucket: &str,
) -> anyhow::Result<usize> {
    let mut total = 0usize;
    let mut index_after: Option<String> = None;
    loop {
        let page = state
            .list_indexes_page(bucket, None, index_after.as_deref(), 100)
            .with_context(|| format!("list indexes in {bucket}"))?;
        for index_row in &page.rows {
            total += rehydrate_index(state, storage, bucket, &index_row.name).await?;
        }
        if page.next.is_none() {
            break;
        }
        index_after = page.next;
    }
    Ok(total)
}

async fn rehydrate_index(
    state: &dyn StateStore,
    storage: &dyn BucketStore,
    bucket: &str,
    index: &str,
) -> anyhow::Result<usize> {
    let prefix = format!("{index}/");
    let mut writes_count = 0usize;
    let mut after: Option<String> = None;
    loop {
        let page = storage
            .list_objects(bucket, &prefix, after.as_deref())
            .await
            .with_context(|| format!("list_objects {bucket}/{prefix}"))?;
        for object_key in &page.keys {
            // Only `<index>/<vec-key>.json` shape; ignore anything else
            // (lets a future RoundI namespace its own keys under the
            // same prefix without us choking on them).
            let suffix = match object_key.strip_suffix(".json") {
                Some(s) => s,
                None => continue,
            };
            let vector_key = match suffix.strip_prefix(&prefix) {
                Some(k) => k,
                None => continue,
            };
            if vector_key.is_empty() {
                continue;
            }

            match storage.get_object(bucket, object_key).await {
                Ok(Some(body)) => match serde_json::from_slice::<SnapshotBody>(&body) {
                    Ok(snap) => {
                        if let Err(e) = state.put_vectors(
                            bucket,
                            index,
                            &[VectorWrite {
                                key: snap.key,
                                data: snap.data,
                                metadata: snap.metadata,
                            }],
                        ) {
                            warn!(
                                bucket, index, key = vector_key,
                                error = %format!("{e:#}"),
                                "rehydrate insert failed — skipping snapshot"
                            );
                        } else {
                            writes_count += 1;
                            debug!(bucket, index, key = vector_key, "rehydrated");
                        }
                    }
                    Err(e) => warn!(
                        bucket, index, object = %object_key,
                        error = %format!("{e:#}"),
                        "snapshot has malformed JSON; skipping"
                    ),
                },
                Ok(None) => {
                    // Raced with a delete; just move on.
                    debug!(object = %object_key, "snapshot vanished between list and get");
                }
                Err(e) => warn!(
                    object = %object_key,
                    error = %format!("{e:#}"),
                    "get_object failed during rehydrate; skipping"
                ),
            }
        }
        if page.next.is_none() {
            break;
        }
        after = page.next;
    }
    Ok(writes_count)
}
