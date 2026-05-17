//! Handlers for the `s3vectors` control-plane operations.

use std::sync::Arc;

use axum::{Json, extract::State};
use marila_aws_compat::AwsError;
use marila_core::{StateError, StateStore};
use marila_storage::{BucketStore, StorageError};
use tracing::instrument;

use crate::{
    arn::vector_bucket_arn,
    state::{
        CreateVectorBucketInput, CreateVectorBucketOutput, DeleteVectorBucketInput,
        ListVectorBucketsInput, ListVectorBucketsOutput, VectorBucketSummary,
    },
};

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
    let row = {
        let state = app.state.clone();
        let name = input.vector_bucket_name.clone();
        let arn = arn.clone();
        tokio::task::spawn_blocking(move || state.create_vector_bucket(&name, &arn))
            .await
            .map_err(|e| AwsError::Internal {
                message: format!("state task join error: {e}"),
            })?
            .map_err(state_error)?
    };

    // Ensure the RustFS bucket exists. If this fails after the state row
    // was inserted, the caller's retry will hit ConflictException — which
    // is *almost* the AWS contract; documented in CLAUDE.md (TODO).
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

#[instrument(skip(app, _input))]
pub async fn list_vector_buckets(
    State(app): State<AppState>,
    Json(_input): Json<ListVectorBucketsInput>,
) -> Result<Json<ListVectorBucketsOutput>, AwsError> {
    let rows = {
        let state = app.state.clone();
        tokio::task::spawn_blocking(move || state.list_vector_buckets())
            .await
            .map_err(|e| AwsError::Internal {
                message: format!("state task join error: {e}"),
            })?
            .map_err(state_error)?
    };

    let vector_buckets = rows
        .into_iter()
        .map(|r| VectorBucketSummary::from_row(r.name, r.arn, r.created_at))
        .collect();

    Ok(Json(ListVectorBucketsOutput {
        vector_buckets,
        next_token: None,
    }))
}

// ---------------------------------------------------------------------------
// DeleteVectorBucket
// ---------------------------------------------------------------------------

#[instrument(skip(app, input), fields(bucket = %input.vector_bucket_name))]
pub async fn delete_vector_bucket(
    State(app): State<AppState>,
    Json(input): Json<DeleteVectorBucketInput>,
) -> Result<Json<serde_json::Value>, AwsError> {
    // Delete the state row first — if it's missing we return NotFound
    // without touching S3.
    let res = {
        let state = app.state.clone();
        let name = input.vector_bucket_name.clone();
        tokio::task::spawn_blocking(move || state.delete_vector_bucket(&name))
            .await
            .map_err(|e| AwsError::Internal {
                message: format!("state task join error: {e}"),
            })?
    };

    // We accept "NotFound from state but bucket actually exists" as a
    // tolerated drift: clients that hit AWS expect idempotent deletes
    // too. For now, surface NotFound.
    if let Err(e) = res {
        return Err(state_error(e));
    }

    app.storage
        .delete_bucket(&input.vector_bucket_name)
        .await
        .map_err(storage_error)?;

    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------------
// Validation + error mapping
// ---------------------------------------------------------------------------

fn validate_bucket_name(name: &str) -> Result<(), AwsError> {
    // Min 3 / max 63 chars per `aws s3vectors create-vector-bucket --help`.
    // We leave the character-set rules to a future iteration — AWS will
    // surface a clearer error than we can right now if a client smuggles
    // an invalid name past us.
    let n = name.chars().count();
    if !(3..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "vectorBucketName length must be between 3 and 63 characters (got {n})"
        )));
    }
    Ok(())
}

fn state_error(e: StateError) -> AwsError {
    match e {
        StateError::AlreadyExists(n) => AwsError::Conflict(format!(
            "A vector bucket with the name `{n}` already exists"
        )),
        StateError::NotFound(n) => AwsError::NotFound(format!("vector bucket `{n}` not found")),
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
