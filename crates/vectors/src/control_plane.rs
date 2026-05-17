//! Handlers for the `s3vectors` control-plane operations.

use std::sync::Arc;

use axum::{Json, extract::State};
use marila_aws_compat::AwsError;
use marila_core::{StateError, StateStore};
use marila_storage::{BucketStore, StorageError};
use tracing::instrument;

use crate::{
    arn::{parse_bucket_name_from_arn, vector_bucket_arn},
    state::{
        CreateVectorBucketInput, CreateVectorBucketOutput, DeleteVectorBucketInput,
        GetVectorBucketInput, GetVectorBucketOutput, ListVectorBucketsInput,
        ListVectorBucketsOutput, VectorBucketDescription, VectorBucketSummary,
    },
};

/// Default + ceiling for `ListVectorBuckets.maxResults`. AWS doesn't
/// publish a hard ceiling on the page size; 500 is a reasonable cap
/// that still fits comfortably in a single response and matches the
/// "small batch" limits elsewhere in the API (e.g. `PutVectors` ≤ 500).
const DEFAULT_LIST_PAGE_SIZE: u32 = 100;
const MAX_LIST_PAGE_SIZE: u32 = 500;

/// Message body AWS sends on bucket-not-found (CLAUDE.md C-2b).
const NOT_FOUND_MESSAGE: &str = "The specified vector bucket could not be found";

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

    // Delete the state row first — if it's missing we return NotFound
    // without touching S3.
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

fn validate_prefix(prefix: &str) -> Result<(), AwsError> {
    let n = prefix.chars().count();
    if !(1..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "prefix length must be between 1 and 63 characters (got {n})"
        )));
    }
    Ok(())
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
        StateError::NotFound(_) => AwsError::NotFound(NOT_FOUND_MESSAGE.to_owned()),
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

