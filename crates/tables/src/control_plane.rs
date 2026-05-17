//! Handlers for the `s3tables` bucket-level control plane.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use marila_aws_compat::AwsError;
use marila_core::{StateError, StateStore};
use tracing::instrument;

use crate::{
    arn::{parse_bucket_name_from_arn, table_bucket_arn},
    state::{
        CreateTableBucketInput, CreateTableBucketOutput, ListTableBucketsOutput, TableBucketSummary,
    },
};

/// Message bodies AWS returns on the not-found / duplicate paths
/// (CLAUDE.md C-9). We match them byte-for-byte.
const BUCKET_NOT_FOUND_MESSAGE: &str = "The specified bucket does not exist.";
const BUCKET_ALREADY_EXISTS_MESSAGE: &str =
    "The bucket that you tried to create already exists, and you own it.";

/// Wiring for the s3tables crate.
///
/// Distinct from `marila-vectors::AppState` even though the field set
/// is the same — coupling the two would force every future tables-side
/// dependency change through the vectors crate too.
#[derive(Clone)]
pub struct AppState {
    pub state: Arc<dyn StateStore>,
    pub region: String,
    pub account_id: String,
}

// ---------------------------------------------------------------------------
// CreateTableBucket — PUT /buckets
// ---------------------------------------------------------------------------

#[instrument(skip(app, input), fields(name = %input.name))]
pub async fn create_table_bucket(
    State(app): State<AppState>,
    Json(input): Json<CreateTableBucketInput>,
) -> Result<Json<CreateTableBucketOutput>, AwsError> {
    validate_bucket_name(&input.name)?;
    let arn = table_bucket_arn(&app.region, &app.account_id, &input.name);

    let row = run_state(app.state.clone(), {
        let name = input.name.clone();
        let arn = arn.clone();
        let account = app.account_id.clone();
        move |s| s.create_table_bucket(&name, &arn, &account)
    })
    .await
    .map_err(|e| match e {
        AwsError::Conflict(_) => AwsError::Conflict(BUCKET_ALREADY_EXISTS_MESSAGE.to_owned()),
        other => other,
    })?;

    Ok(Json(CreateTableBucketOutput { arn: row.arn }))
}

// ---------------------------------------------------------------------------
// ListTableBuckets — GET /buckets
// ---------------------------------------------------------------------------

#[instrument(skip(app))]
pub async fn list_table_buckets(
    State(app): State<AppState>,
) -> Result<Json<ListTableBucketsOutput>, AwsError> {
    let rows = run_state(app.state.clone(), move |s| s.list_table_buckets()).await?;
    Ok(Json(ListTableBucketsOutput {
        table_buckets: rows.into_iter().map(TableBucketSummary::from_row).collect(),
    }))
}

// ---------------------------------------------------------------------------
// GetTableBucket — GET /buckets/{arn}
// ---------------------------------------------------------------------------

#[instrument(skip(app))]
pub async fn get_table_bucket(
    State(app): State<AppState>,
    Path(arn): Path<String>,
) -> Result<Json<TableBucketSummary>, AwsError> {
    let name = parse_bucket_name_from_arn(&arn)?.to_owned();
    let row = run_state(app.state.clone(), move |s| s.get_table_bucket(&name)).await?;
    Ok(Json(TableBucketSummary::from_row(row)))
}

// ---------------------------------------------------------------------------
// DeleteTableBucket — DELETE /buckets/{arn}
// ---------------------------------------------------------------------------

#[instrument(skip(app))]
pub async fn delete_table_bucket(
    State(app): State<AppState>,
    Path(arn): Path<String>,
) -> Result<impl IntoResponse, AwsError> {
    let name = parse_bucket_name_from_arn(&arn)?.to_owned();
    run_state(app.state.clone(), move |s| s.delete_table_bucket(&name)).await?;
    // AWS replies HTTP 204 with no body — match exactly (CLAUDE.md C-9).
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn validate_bucket_name(name: &str) -> Result<(), AwsError> {
    let n = name.chars().count();
    if !(3..=63).contains(&n) {
        return Err(AwsError::Validation(format!(
            "name length must be between 3 and 63 characters (got {n})"
        )));
    }
    Ok(())
}

/// Bridge between async axum handlers and the synchronous DuckDB
/// `StateStore`. Same shape as the vectors-crate helper, kept duplicated
/// to avoid a coupling crate between the two façades.
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

/// State-error → AWS-error mapping for the s3tables surface.
///
/// `NotFound` maps to the verbatim AWS body ("The specified bucket
/// does not exist."), `AlreadyExists` to ConflictException (the caller
/// then refines the message on CreateTableBucket's specific contract).
fn state_error(e: StateError) -> AwsError {
    match e {
        StateError::AlreadyExists(_) => {
            AwsError::Conflict(BUCKET_ALREADY_EXISTS_MESSAGE.to_owned())
        }
        StateError::NotFound(_) => AwsError::NotFound(BUCKET_NOT_FOUND_MESSAGE.to_owned()),
        StateError::DimensionMismatch { got, expected } => AwsError::Validation(format!(
            "dimension mismatch: got {got}, expected {expected}"
        )),
        StateError::Internal(e) => AwsError::Internal {
            message: format!("{e:#}"),
        },
    }
}
