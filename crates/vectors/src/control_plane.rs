//! Handlers for the `s3vectors` control-plane operations.

use std::sync::Arc;

use axum::{Json, extract::State};
use marila_aws_compat::AwsError;
use marila_core::{DistanceMetric, StateError, StateStore, VectorWrite};
use marila_storage::{BucketStore, StorageError};
use tracing::instrument;

use crate::{
    arn::{parse_bucket_name_from_arn, parse_index_from_arn, vector_bucket_arn, vector_index_arn},
    filter,
    state::{
        CreateIndexInput, CreateIndexOutput, CreateVectorBucketInput, CreateVectorBucketOutput,
        DeleteIndexInput, DeleteVectorBucketInput, DeleteVectorsInput, GetIndexInput,
        GetIndexOutput, GetVectorBucketInput, GetVectorBucketOutput, GetVectorsInput,
        GetVectorsOutput, IndexDescription, IndexSummary, ListIndexesInput, ListIndexesOutput,
        ListVectorBucketsInput, ListVectorBucketsOutput, ListVectorsInput, ListVectorsOutput,
        PutVectorsInput, PutVectorsOutput, QueryVectorsHit, QueryVectorsInput, QueryVectorsOutput,
        ReturnedVector, VectorBucketDescription, VectorBucketSummary,
    },
};

/// Default + ceiling for `List*.maxResults`. AWS doesn't publish a hard
/// ceiling on the page size; 500 is a reasonable cap that still fits
/// comfortably in a single response and matches the "small batch"
/// limits elsewhere in the API (e.g. `PutVectors` ≤ 500).
const DEFAULT_LIST_PAGE_SIZE: u32 = 100;
const MAX_LIST_PAGE_SIZE: u32 = 500;

/// Max vectors per PutVectors / GetVectors / DeleteVectors call.
const MAX_VECTOR_BATCH: usize = 500;
/// Max keys per GetVectors / DeleteVectors call (AWS limit per docs).
const MAX_KEY_BATCH: usize = 500;

/// Message body AWS sends on bucket-not-found (doc/GAP_ANALYSIS.md).
const BUCKET_NOT_FOUND_MESSAGE: &str = "The specified vector bucket could not be found";

/// Message body AWS sends on index-not-found (doc/GAP_ANALYSIS.md).
const INDEX_NOT_FOUND_MESSAGE: &str = "The specified index could not be found";

/// Message body AWS sends when DeleteVectorBucket runs on a bucket
/// that still has indexes (doc/GAP_ANALYSIS.md).
const BUCKET_NOT_EMPTY_MESSAGE: &str = "The specified vector bucket is not empty";

/// Message body AWS sends on duplicate index name (doc/GAP_ANALYSIS.md).
const INDEX_ALREADY_EXISTS_MESSAGE: &str = "An index with the specified name already exists";

/// Wiring for the vectors crate: the two stores plus the AWS-account /
/// region context needed to shape ARNs the way AWS does.
#[derive(Clone)]
pub struct AppState {
    pub state: Arc<dyn StateStore>,
    pub storage: Arc<dyn BucketStore>,
    pub region: String,
    pub account_id: String,
}

// ---------------------------------------------------------------------------
// CreateVectorBucket
// ---------------------------------------------------------------------------

#[instrument(skip(app, input), fields(bucket = %input.vector_bucket_name))]
pub async fn create_vector_bucket(
    State(app): State<AppState>,
    Json(input): Json<CreateVectorBucketInput>,
) -> Result<Json<CreateVectorBucketOutput>, AwsError> {
    validate_bucket_name(&input.vector_bucket_name)?;
    let arn = vector_bucket_arn(&app.region, &app.account_id, &input.vector_bucket_name);

    // State row first — its PK constraint is what enforces uniqueness and
    // surfaces `ConflictException`. Doing the S3 PutBucket first would
    // create an orphan bucket on duplicate.
    let row = run_state(app.state.clone(), {
        let name = input.vector_bucket_name.clone();
        let arn = arn.clone();
        move |s| s.create_vector_bucket(&name, &arn)
    })
    .await?;

    app.storage
        .ensure_bucket(&input.vector_bucket_name)
        .await
        .map_err(storage_error)?;

    Ok(Json(CreateVectorBucketOutput {
        vector_bucket_arn: row.arn,
    }))
}

// ---------------------------------------------------------------------------
// ListVectorBuckets
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn list_vector_buckets(
    State(app): State<AppState>,
    Json(input): Json<ListVectorBucketsInput>,
) -> Result<Json<ListVectorBucketsOutput>, AwsError> {
    if let Some(p) = input.prefix.as_deref() {
        validate_prefix(p)?;
    }
    let max = input
        .max_results
        .unwrap_or(DEFAULT_LIST_PAGE_SIZE)
        .clamp(1, MAX_LIST_PAGE_SIZE) as usize;

    let page = run_state(app.state.clone(), move |s| {
        s.list_vector_buckets_page(input.prefix.as_deref(), input.next_token.as_deref(), max)
    })
    .await?;

    let vector_buckets = page
        .rows
        .into_iter()
        .map(|r| VectorBucketSummary::from_row(r.name, r.arn, r.created_at))
        .collect();

    Ok(Json(ListVectorBucketsOutput {
        vector_buckets,
        next_token: page.next,
    }))
}

// ---------------------------------------------------------------------------
// GetVectorBucket
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn get_vector_bucket(
    State(app): State<AppState>,
    Json(input): Json<GetVectorBucketInput>,
) -> Result<Json<GetVectorBucketOutput>, AwsError> {
    let name = bucket_name_from_either(&input.vector_bucket_name, &input.vector_bucket_arn)?;

    let row = run_state(app.state.clone(), {
        let name = name.clone();
        move |s| s.get_vector_bucket(&name)
    })
    .await?;

    Ok(Json(GetVectorBucketOutput {
        vector_bucket: VectorBucketDescription::from_row(row.name, row.arn, row.created_at),
    }))
}

// ---------------------------------------------------------------------------
// DeleteVectorBucket
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn delete_vector_bucket(
    State(app): State<AppState>,
    Json(input): Json<DeleteVectorBucketInput>,
) -> Result<Json<serde_json::Value>, AwsError> {
    let name = bucket_name_from_either(&input.vector_bucket_name, &input.vector_bucket_arn)?;

    // Emptiness check first — matches the AWS contract that deleting a
    // bucket with surviving indexes returns ConflictException with the
    // exact body in doc/GAP_ANALYSIS.md.
    let indexes = run_state(app.state.clone(), {
        let bucket = name.clone();
        move |s| s.count_indexes(&bucket)
    })
    .await?;
    if indexes > 0 {
        return Err(AwsError::Conflict(BUCKET_NOT_EMPTY_MESSAGE.to_owned()));
    }

    // Delete the state row — if it's missing we return NotFound without
    // touching S3.
    run_state(app.state.clone(), {
        let name = name.clone();
        move |s| s.delete_vector_bucket(&name)
    })
    .await?;

    app.storage
        .delete_bucket(&name)
        .await
        .map_err(storage_error)?;

    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// CreateIndex
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn create_index(
    State(app): State<AppState>,
    Json(input): Json<CreateIndexInput>,
) -> Result<Json<CreateIndexOutput>, AwsError> {
    let bucket = bucket_name_from_either(&input.vector_bucket_name, &input.vector_bucket_arn)?;

    let index_name = input
        .index_name
        .as_deref()
        .ok_or_else(|| AwsError::Validation("indexName is required".to_owned()))?;
    validate_index_name(index_name)?;

    let data_type = input.data_type.as_deref().unwrap_or("float32");
    if data_type != "float32" {
        return Err(AwsError::Validation(format!(
            "dataType must be `float32`, got `{data_type}`"
        )));
    }

    let dimension = input
        .dimension
        .ok_or_else(|| AwsError::Validation("dimension is required".to_owned()))?;
    if !(1..=4096).contains(&dimension) {
        return Err(AwsError::Validation(format!(
            "dimension must be between 1 and 4096 (got {dimension})"
        )));
    }
    let dimension = dimension as u32;

    let metric = input
        .distance_metric
        .as_deref()
        .ok_or_else(|| AwsError::Validation("distanceMetric is required".to_owned()))?;
    let metric = DistanceMetric::from_wire(metric).ok_or_else(|| {
        AwsError::Validation(format!(
            "distanceMetric must be `cosine` or `euclidean` (got `{metric}`)"
        ))
    })?;

    let arn = vector_index_arn(&app.region, &app.account_id, &bucket, index_name);

    let row = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index_name.to_owned();
        let arn = arn.clone();
        move |s| s.create_index(&bucket, &index, &arn, dimension, metric)
    })
    .await
    .map_err(index_create_error)?;

    Ok(Json(CreateIndexOutput { index_arn: row.arn }))
}

// ---------------------------------------------------------------------------
// ListIndexes
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn list_indexes(
    State(app): State<AppState>,
    Json(input): Json<ListIndexesInput>,
) -> Result<Json<ListIndexesOutput>, AwsError> {
    let bucket = bucket_name_from_either(&input.vector_bucket_name, &input.vector_bucket_arn)?;
    if let Some(p) = input.prefix.as_deref() {
        validate_prefix(p)?;
    }
    let max = input
        .max_results
        .unwrap_or(DEFAULT_LIST_PAGE_SIZE)
        .clamp(1, MAX_LIST_PAGE_SIZE) as usize;

    let page = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let prefix = input.prefix.clone();
        let after = input.next_token.clone();
        move |s| s.list_indexes_page(&bucket, prefix.as_deref(), after.as_deref(), max)
    })
    .await?;

    Ok(Json(ListIndexesOutput {
        indexes: page.rows.into_iter().map(IndexSummary::from_row).collect(),
        next_token: page.next,
    }))
}

// ---------------------------------------------------------------------------
// GetIndex
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn get_index(
    State(app): State<AppState>,
    Json(input): Json<GetIndexInput>,
) -> Result<Json<GetIndexOutput>, AwsError> {
    let (bucket, index) = resolve_get_index_target(&input)?;

    let row = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        move |s| s.get_index(&bucket, &index)
    })
    .await
    .map_err(|e| match e {
        AwsError::NotFound(_) => AwsError::NotFound(INDEX_NOT_FOUND_MESSAGE.to_owned()),
        other => other,
    })?;

    Ok(Json(GetIndexOutput {
        index: IndexDescription::from_row(row),
    }))
}

// ---------------------------------------------------------------------------
// DeleteIndex
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn delete_index(
    State(app): State<AppState>,
    Json(input): Json<DeleteIndexInput>,
) -> Result<Json<serde_json::Value>, AwsError> {
    let (bucket, index) = resolve_index_target(&input)?;

    run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        move |s| s.delete_index(&bucket, &index)
    })
    .await
    .map_err(|e| match e {
        AwsError::NotFound(_) => AwsError::NotFound(INDEX_NOT_FOUND_MESSAGE.to_owned()),
        other => other,
    })?;

    // Walk the RustFS prefix `<bucket>/<index>/` and delete every
    // snapshot. Without this DeleteVectorBucket later trips over a
    // non-empty S3 bucket and fails. Errors during the walk are logged
    // but not surfaced — the state row is the source of truth for
    // "does this index exist", and the snapshots are durable-cache
    // material per FV-4.
    purge_index_snapshots(&app, &bucket, &index).await;

    Ok(Json(serde_json::json!({})))
}

/// Drop every `<bucket>/<index>/<key>.json` snapshot from the object
/// store. Best-effort: a failure here doesn't roll back the state row,
/// since by the time we get here marila no longer has any pointer to
/// the orphaned objects. Future rehydration ignores prefixes that
/// don't match a known (bucket, index) pair.
async fn purge_index_snapshots(app: &AppState, bucket: &str, index: &str) {
    let prefix = format!("{index}/");
    let mut after: Option<String> = None;
    loop {
        match app
            .storage
            .list_objects(bucket, &prefix, after.as_deref())
            .await
        {
            Ok(page) => {
                for key in &page.keys {
                    if let Err(e) = app.storage.delete_object(bucket, key).await {
                        tracing::warn!(%bucket, %key, error = %format!("{e:#}"), "purge snapshot failed");
                    }
                }
                if page.next.is_none() {
                    return;
                }
                after = page.next;
            }
            Err(e) => {
                tracing::warn!(%bucket, %prefix, error = %format!("{e:#}"), "list snapshots failed during purge");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PutVectors
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn put_vectors(
    State(app): State<AppState>,
    Json(input): Json<PutVectorsInput>,
) -> Result<Json<PutVectorsOutput>, AwsError> {
    let (bucket, index) = resolve_data_plane_target(
        &input.vector_bucket_name,
        &input.index_name,
        &input.index_arn,
    )?;

    if input.vectors.is_empty() {
        return Err(AwsError::Validation(
            "vectors must contain at least 1 item".into(),
        ));
    }
    if input.vectors.len() > MAX_VECTOR_BATCH {
        return Err(AwsError::Validation(format!(
            "vectors must contain at most {MAX_VECTOR_BATCH} items (got {})",
            input.vectors.len()
        )));
    }

    // Translate the wire shape (tagged union) into the state-store
    // shape, validating each item's key length and the presence of the
    // `float32` variant. Per doc/GAP_ANALYSIS.md, AWS rejects non-finite
    // values; the state layer's `format_float_array_literal` doubles
    // up on this defence in depth.
    let mut writes: Vec<VectorWrite> = Vec::with_capacity(input.vectors.len());
    for (idx, item) in input.vectors.into_iter().enumerate() {
        validate_vector_key(&item.key, idx)?;
        let data = item.data.float32.ok_or_else(|| {
            AwsError::Validation(format!(
                "vectors[{idx}].data must contain a `float32` variant"
            ))
        })?;
        if data.iter().any(|v| !v.is_finite()) {
            return Err(AwsError::Validation(format!(
                "vectors[{idx}].data contains a non-finite value (NaN/Infinity not allowed)"
            )));
        }
        writes.push(VectorWrite {
            key: item.key,
            data,
            metadata: item.metadata,
        });
    }

    // FV-4 / REQUIREMENTS.md: write each vector as a JSON snapshot to
    // RustFS **before** the DuckDB INSERT — RustFS is the source of
    // truth and the rebuild-from-snapshot path uses these files. If a
    // snapshot write fails, the whole batch fails and no state row
    // lands; clients can retry.
    for w in &writes {
        let body = serde_json::to_vec(&SnapshotBody {
            key: &w.key,
            data: &w.data,
            metadata: w.metadata.as_ref(),
        })
        .map_err(|e| AwsError::Internal {
            message: format!("serialise vector snapshot: {e}"),
        })?;
        let key = snapshot_key(&index, &w.key);
        app.storage
            .put_object(&bucket, &key, body)
            .await
            .map_err(storage_error)?;
    }

    run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        move |s| s.put_vectors(&bucket, &index, &writes)
    })
    .await
    .map_err(data_plane_error)?;

    Ok(Json(PutVectorsOutput::default()))
}

/// Path inside the vector bucket where we write the per-vector JSON
/// snapshot. The architecture (FV-4) specifies
/// `<bucket>/<index>/<key>.json`; the bucket is the S3 bucket name
/// itself, the prefix here is just `<index>/<key>.json`.
fn snapshot_key(index: &str, key: &str) -> String {
    format!("{index}/{key}.json")
}

/// Shape we write to the RustFS snapshot. Matches the conceptual
/// `VectorWrite` so the rebuild path (not implemented yet) can deserialize
/// straight back into the state store.
#[derive(serde::Serialize)]
struct SnapshotBody<'a> {
    key: &'a str,
    data: &'a [f32],
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a serde_json::Value>,
}

// ---------------------------------------------------------------------------
// GetVectors
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn get_vectors(
    State(app): State<AppState>,
    Json(input): Json<GetVectorsInput>,
) -> Result<Json<GetVectorsOutput>, AwsError> {
    let (bucket, index) = resolve_data_plane_target(
        &input.vector_bucket_name,
        &input.index_name,
        &input.index_arn,
    )?;

    if input.keys.is_empty() {
        return Err(AwsError::Validation(
            "keys must contain at least 1 item".into(),
        ));
    }
    if input.keys.len() > MAX_KEY_BATCH {
        return Err(AwsError::Validation(format!(
            "keys must contain at most {MAX_KEY_BATCH} items (got {})",
            input.keys.len()
        )));
    }
    for (idx, k) in input.keys.iter().enumerate() {
        validate_vector_key(k, idx)?;
    }

    let return_data = input.return_data.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);
    let rows = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        let keys = input.keys.clone();
        move |s| s.get_vectors(&bucket, &index, &keys, return_data, return_metadata)
    })
    .await
    .map_err(data_plane_error)?;

    Ok(Json(GetVectorsOutput {
        vectors: rows.into_iter().map(ReturnedVector::from_read).collect(),
    }))
}

// ---------------------------------------------------------------------------
// ListVectors
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn list_vectors(
    State(app): State<AppState>,
    Json(input): Json<ListVectorsInput>,
) -> Result<Json<ListVectorsOutput>, AwsError> {
    let (bucket, index) = resolve_data_plane_target(
        &input.vector_bucket_name,
        &input.index_name,
        &input.index_arn,
    )?;

    let max = input
        .max_results
        .unwrap_or(DEFAULT_LIST_PAGE_SIZE)
        .clamp(1, MAX_LIST_PAGE_SIZE) as usize;
    let return_data = input.return_data.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);

    let page = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        let after = input.next_token.clone();
        move |s| {
            s.list_vectors_page(
                &bucket,
                &index,
                after.as_deref(),
                max,
                return_data,
                return_metadata,
            )
        }
    })
    .await
    .map_err(data_plane_error)?;

    Ok(Json(ListVectorsOutput {
        vectors: page
            .rows
            .into_iter()
            .map(ReturnedVector::from_read)
            .collect(),
        next_token: page.next,
    }))
}

// ---------------------------------------------------------------------------
// DeleteVectors
// ---------------------------------------------------------------------------

#[instrument(skip(app, input))]
pub async fn delete_vectors(
    State(app): State<AppState>,
    Json(input): Json<DeleteVectorsInput>,
) -> Result<Json<serde_json::Value>, AwsError> {
    let (bucket, index) = resolve_data_plane_target(
        &input.vector_bucket_name,
        &input.index_name,
        &input.index_arn,
    )?;

    if input.keys.is_empty() {
        return Err(AwsError::Validation(
            "keys must contain at least 1 item".into(),
        ));
    }
    if input.keys.len() > MAX_KEY_BATCH {
        return Err(AwsError::Validation(format!(
            "keys must contain at most {MAX_KEY_BATCH} items (got {})",
            input.keys.len()
        )));
    }
    for (idx, k) in input.keys.iter().enumerate() {
        validate_vector_key(k, idx)?;
    }

    run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        let keys = input.keys.clone();
        move |s| s.delete_vectors(&bucket, &index, &keys)
    })
    .await
    .map_err(data_plane_error)?;

    // Best-effort snapshot cleanup. Per-key delete_object is
    // idempotent, so we don't fail the request when a snapshot was
    // missing — the state-row delete already succeeded.
    for k in &input.keys {
        let object_key = snapshot_key(&index, k);
        if let Err(e) = app.storage.delete_object(&bucket, &object_key).await {
            tracing::warn!(
                bucket = %bucket,
                key = %object_key,
                error = %format!("{e:#}"),
                "snapshot cleanup failed — vector deleted from state but object remains"
            );
        }
    }

    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// QueryVectors
// ---------------------------------------------------------------------------

/// Min / max for `topK`.
const QUERY_TOP_K_MIN: u32 = 1;
const QUERY_TOP_K_MAX: u32 = 100;

#[instrument(skip(app, input))]
pub async fn query_vectors(
    State(app): State<AppState>,
    Json(input): Json<QueryVectorsInput>,
) -> Result<Json<QueryVectorsOutput>, AwsError> {
    let (bucket, index) = resolve_data_plane_target(
        &input.vector_bucket_name,
        &input.index_name,
        &input.index_arn,
    )?;

    let top_k = input
        .top_k
        .ok_or_else(|| AwsError::Validation("topK is required".to_owned()))?;
    if !(QUERY_TOP_K_MIN..=QUERY_TOP_K_MAX).contains(&top_k) {
        return Err(AwsError::Validation(format!(
            "topK must be between {QUERY_TOP_K_MIN} and {QUERY_TOP_K_MAX} (got {top_k})"
        )));
    }
    let top_k = top_k as usize;

    let query_data = input
        .query_vector
        .and_then(|d| d.float32)
        .ok_or_else(|| AwsError::Validation("queryVector.float32 is required".to_owned()))?;
    if query_data.iter().any(|v| !v.is_finite()) {
        return Err(AwsError::Validation(
            "queryVector contains a non-finite value (NaN/Infinity not allowed)".to_owned(),
        ));
    }

    let where_sql = match input.filter.as_ref() {
        Some(f) => Some(filter::translate(f).map_err(|e| AwsError::Validation(e.to_string()))?),
        None => None,
    };

    let return_distance = input.return_distance.unwrap_or(false);
    let return_metadata = input.return_metadata.unwrap_or(false);

    // Run the query + look up the index's metric for the response echo.
    // Two state calls instead of one keeps the StateStore trait narrow;
    // both go through the same spawn_blocking pool so the cost is one
    // extra mutex acquisition.
    //
    // Oversample = 100 when a filter is present (D-12 mitigation —
    // HNSW returns topK*100 candidates that are then filtered down to
    // topK). Without a filter, oversample = 1 so we don't pay for
    // extra HNSW work.
    let oversample = if where_sql.is_some() { 100 } else { 1 };
    let hits = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        let where_sql = where_sql.clone();
        let q = query_data.clone();
        move |s| s.query_vectors(&bucket, &index, &q, top_k, oversample, where_sql.as_deref())
    })
    .await
    .map_err(data_plane_error)?;

    let metric_row = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        move |s| s.get_index(&bucket, &index)
    })
    .await
    .map_err(data_plane_error)?;
    let distance_metric = metric_row.distance_metric.as_wire().to_owned();

    let vectors = hits
        .into_iter()
        .map(|h| QueryVectorsHit {
            key: h.key,
            distance: return_distance.then_some(h.distance),
            data: None, // S3 Vectors' QueryVectors does NOT return raw data.
            metadata: if return_metadata { h.metadata } else { None },
        })
        .collect();

    Ok(Json(QueryVectorsOutput {
        distance_metric,
        vectors,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a bucket reference into its canonical name.
///
/// Mirrors the AWS contract: callers may supply `vectorBucketName` *or*
/// `vectorBucketArn` but not both, and not neither.
fn bucket_name_from_either(
    name: &Option<String>,
    arn: &Option<String>,
) -> Result<String, AwsError> {
    match (name, arn) {
        (Some(n), None) => {
            validate_bucket_name(n)?;
            Ok(n.clone())
        }
        (None, Some(a)) => Ok(parse_bucket_name_from_arn(a)?.to_owned()),
        (Some(_), Some(_)) => Err(AwsError::Validation(
            "specify exactly one of vectorBucketName or vectorBucketArn, not both".into(),
        )),
        (None, None) => Err(AwsError::Validation(
            "one of vectorBucketName or vectorBucketArn is required".into(),
        )),
    }
}

fn validate_bucket_name(name: &str) -> Result<(), AwsError> {
    // Min 3 / max 63 chars per `aws s3vectors create-vector-bucket --help`.
    let n = name.chars().count();
    if !(3..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "vectorBucketName length must be between 3 and 63 characters (got {n})"
        )));
    }
    Ok(())
}

fn validate_index_name(name: &str) -> Result<(), AwsError> {
    // Same 3..=63 window as bucket names (per `aws s3vectors create-index --help`).
    let n = name.chars().count();
    if !(3..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "indexName length must be between 3 and 63 characters (got {n})"
        )));
    }
    Ok(())
}

/// Resolve a DeleteIndex target to `(bucket, index)` regardless of
/// which input combination the client used.
fn resolve_index_target(input: &DeleteIndexInput) -> Result<(String, String), AwsError> {
    // Index ARN wins outright — it carries both pieces.
    if let Some(arn) = input.index_arn.as_deref() {
        let (b, i) = parse_index_from_arn(arn)?;
        return Ok((b.to_owned(), i.to_owned()));
    }
    let bucket = bucket_name_from_either(&input.vector_bucket_name, &input.vector_bucket_arn)?;
    let index = input
        .index_name
        .as_deref()
        .ok_or_else(|| AwsError::Validation("indexName is required".to_owned()))?;
    validate_index_name(index)?;
    Ok((bucket, index.to_owned()))
}

/// Resolve a GetIndex target.
///
/// AWS accepts `(vectorBucketName, indexName)` together OR a standalone
/// `indexArn`. Bucket ARN is **not** an option on GetIndex — that
/// distinguishes it from DeleteIndex which accepts both.
fn resolve_get_index_target(input: &GetIndexInput) -> Result<(String, String), AwsError> {
    match (
        input.index_arn.as_deref(),
        input.vector_bucket_name.as_deref(),
        input.index_name.as_deref(),
    ) {
        (Some(arn), None, None) => {
            let (b, i) = parse_index_from_arn(arn)?;
            Ok((b.to_owned(), i.to_owned()))
        }
        (None, Some(b), Some(i)) => {
            validate_bucket_name(b)?;
            validate_index_name(i)?;
            Ok((b.to_owned(), i.to_owned()))
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(AwsError::Validation(
            "specify indexArn alone, not together with vectorBucketName/indexName".into(),
        )),
        _ => Err(AwsError::Validation(
            "specify indexArn, or both vectorBucketName and indexName".into(),
        )),
    }
}

/// State-error → AWS-error mapping for `CreateIndex` only.
///
/// `StateError::NotFound` here means **the bucket** wasn't found
/// (CreateIndex only consults bucket presence at the `StateError`
/// level); `AlreadyExists` means the index already exists.
fn index_create_error(e: AwsError) -> AwsError {
    match e {
        AwsError::NotFound(_) => AwsError::NotFound(BUCKET_NOT_FOUND_MESSAGE.to_owned()),
        AwsError::Conflict(_) => AwsError::Conflict(INDEX_ALREADY_EXISTS_MESSAGE.to_owned()),
        other => other,
    }
}

fn validate_prefix(prefix: &str) -> Result<(), AwsError> {
    let n = prefix.chars().count();
    if !(1..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "prefix length must be between 1 and 63 characters (got {n})"
        )));
    }
    Ok(())
}

fn validate_vector_key(key: &str, idx: usize) -> Result<(), AwsError> {
    let n = key.chars().count();
    if !(1..=1024).contains(&n) {
        return Err(AwsError::Validation(format!(
            "vectors[{idx}].key length must be between 1 and 1024 characters (got {n})"
        )));
    }
    Ok(())
}

/// Resolve a data-plane target (`PutVectors` etc.) to `(bucket, index)`.
///
/// Accepts either `indexArn` standalone, or `(vectorBucketName, indexName)`
/// — same combination rules as `GetIndex` (doc/GAP_ANALYSIS.md).
fn resolve_data_plane_target(
    bucket_name: &Option<String>,
    index_name: &Option<String>,
    index_arn: &Option<String>,
) -> Result<(String, String), AwsError> {
    match (
        index_arn.as_deref(),
        bucket_name.as_deref(),
        index_name.as_deref(),
    ) {
        (Some(arn), None, None) => {
            let (b, i) = parse_index_from_arn(arn)?;
            Ok((b.to_owned(), i.to_owned()))
        }
        (None, Some(b), Some(i)) => {
            validate_bucket_name(b)?;
            validate_index_name(i)?;
            Ok((b.to_owned(), i.to_owned()))
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(AwsError::Validation(
            "specify indexArn alone, not together with vectorBucketName/indexName".into(),
        )),
        _ => Err(AwsError::Validation(
            "specify indexArn, or both vectorBucketName and indexName".into(),
        )),
    }
}

/// State-error → AWS-error mapping for the data plane.
///
/// AWS collapses bucket-not-found and index-not-found into a single
/// `NotFoundException` with the **index** body text (doc/GAP_ANALYSIS.md).
/// DimensionMismatch becomes ValidationException matching the wire
/// shape AWS emits.
fn data_plane_error(e: AwsError) -> AwsError {
    match e {
        AwsError::NotFound(_) => AwsError::NotFound(INDEX_NOT_FOUND_MESSAGE.to_owned()),
        other => other,
    }
}

/// Bridge between async axum handlers and the synchronous DuckDB
/// `StateStore`. Wraps the closure in `spawn_blocking` and maps the
/// resulting two-layer error into a single `AwsError`.
async fn run_state<F, T>(state: Arc<dyn StateStore>, f: F) -> Result<T, AwsError>
where
    F: FnOnce(&dyn StateStore) -> Result<T, StateError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&*state))
        .await
        .map_err(|e| AwsError::Internal {
            message: format!("state task join error: {e}"),
        })?
        .map_err(state_error)
}

fn state_error(e: StateError) -> AwsError {
    match e {
        StateError::AlreadyExists(n) => AwsError::Conflict(format!(
            "A vector bucket with the name `{n}` already exists"
        )),
        StateError::NotFound(_) => AwsError::NotFound(BUCKET_NOT_FOUND_MESSAGE.to_owned()),
        StateError::DimensionMismatch { got, expected } => AwsError::Validation(format!(
            "vector must have length {expected}, but has length {got}"
        )),
        StateError::Internal(e) => AwsError::Internal {
            message: format!("{e:#}"),
        },
    }
}

fn storage_error(e: StorageError) -> AwsError {
    match e {
        StorageError::Backend(e) => AwsError::Internal {
            message: format!("{e:#}"),
        },
    }
}
