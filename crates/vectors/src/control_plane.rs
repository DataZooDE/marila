//! Handlers for the `s3vectors` control-plane operations.

use std::sync::Arc;

use axum::{Json, extract::State};
use marila_aws_compat::AwsError;
use marila_core::{DistanceMetric, StateError, StateStore, VectorWrite};
use marila_storage::{BucketStore, StorageError};
use tracing::instrument;

use crate::{
    arn::{parse_bucket_name_from_arn, parse_index_from_arn, vector_bucket_arn, vector_index_arn},
    state::{
        CreateIndexInput, CreateIndexOutput, CreateVectorBucketInput, CreateVectorBucketOutput,
        DeleteIndexInput, DeleteVectorBucketInput, DeleteVectorsInput, GetIndexInput,
        GetIndexOutput, GetVectorBucketInput, GetVectorBucketOutput, GetVectorsInput,
        GetVectorsOutput, IndexDescription, IndexSummary, ListIndexesInput, ListIndexesOutput,
        ListVectorBucketsInput, ListVectorBucketsOutput, ListVectorsInput, ListVectorsOutput,
        PutVectorsInput, PutVectorsOutput, ReturnedVector, VectorBucketDescription,
        VectorBucketSummary,
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

/// Message body AWS sends on bucket-not-found (CLAUDE.md C-2b).
const BUCKET_NOT_FOUND_MESSAGE: &str = "The specified vector bucket could not be found";

/// Message body AWS sends on index-not-found (CLAUDE.md C-2c).
const INDEX_NOT_FOUND_MESSAGE: &str = "The specified index could not be found";

/// Message body AWS sends when DeleteVectorBucket runs on a bucket
/// that still has indexes (CLAUDE.md C-2c).
const BUCKET_NOT_EMPTY_MESSAGE: &str = "The specified vector bucket is not empty";

/// Message body AWS sends on duplicate index name (CLAUDE.md C-2c).
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
    // exact body in CLAUDE.md C-2c.
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

    Ok(Json(serde_json::json!({})))
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
    // `float32` variant. Per CLAUDE.md C-2e, AWS rejects non-finite
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

    run_state(app.state.clone(), {
        let bucket = bucket.clone();
        let index = index.clone();
        move |s| s.put_vectors(&bucket, &index, &writes)
    })
    .await
    .map_err(data_plane_error)?;

    Ok(Json(PutVectorsOutput::default()))
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

    Ok(Json(serde_json::json!({})))
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
/// — same combination rules as `GetIndex` (CLAUDE.md C-2e).
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
/// `NotFoundException` with the **index** body text (CLAUDE.md C-2e).
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
