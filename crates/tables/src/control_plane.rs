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
use marila_storage::{BucketStore, StorageError};
use tracing::instrument;

use crate::{
    arn::{parse_bucket_name_from_arn, table_bucket_arn},
    lakekeeper::LakekeeperClient,
    state::{
        CreateTableBucketInput, CreateTableBucketOutput, ListTableBucketsOutput, TableBucketSummary,
    },
};

/// Message bodies AWS returns on the not-found / duplicate paths
/// (doc/GAP_ANALYSIS.md). We match them byte-for-byte.
pub(crate) const BUCKET_NOT_FOUND_MESSAGE: &str = "The specified bucket does not exist.";
pub(crate) const BUCKET_ALREADY_EXISTS_MESSAGE: &str =
    "The bucket that you tried to create already exists, and you own it.";

/// Wiring for the s3tables crate.
///
/// Distinct from `marila-vectors::AppState` even though the field set
/// looks similar — coupling them would force every future tables-side
/// dependency change through the vectors crate too.
#[derive(Clone)]
pub struct AppState {
    pub state: Arc<dyn StateStore>,
    pub storage: Arc<dyn BucketStore>,
    pub lakekeeper: Arc<LakekeeperClient>,
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

    // 1. Ensure the RustFS bucket exists. Idempotent — repeated
    //    CreateTableBucket calls won't fail here.
    app.storage
        .ensure_bucket(&input.name)
        .await
        .map_err(storage_error)?;

    // 2. Create the Lakekeeper warehouse. Its `warehouse-id` becomes our
    //    `tableBucketId` on the wire so AWS clients see a stable UUID
    //    per bucket without us having to maintain a separate mapping.
    //    If state insertion fails afterward (duplicate name), we leave
    //    the warehouse intact — the retry path picks it up.
    let warehouse_id = app
        .lakekeeper
        .create_warehouse(&input.name)
        .await
        .map_err(|e| match e {
            // Duplicate warehouse means we're retrying — fall back to a
            // direct look-up so the state row is the source of truth on
            // "does this bucket exist already".
            AwsError::Conflict(_) => AwsError::Internal {
                message: format!(
                    "warehouse `{}` already exists in Lakekeeper but no marila state row — \
                     out-of-band state drift, retry to reconcile",
                    input.name
                ),
            },
            other => other,
        })?;

    // 3. Insert state row using the warehouse_id as the table_bucket_id.
    let row = run_state(app.state.clone(), {
        let name = input.name.clone();
        let arn = arn.clone();
        let account = app.account_id.clone();
        let id = warehouse_id.clone();
        move |s| s.create_table_bucket(&name, &arn, &id, &account)
    })
    .await
    .map_err(|e| match e {
        AwsError::Conflict(_) => AwsError::Conflict(BUCKET_ALREADY_EXISTS_MESSAGE.to_owned()),
        other => other,
    });

    // 4. Roll back the Lakekeeper warehouse if the state insert failed,
    //    so a subsequent retry isn't blocked by "warehouse exists, no
    //    state row".
    let row = match row {
        Ok(r) => r,
        Err(e) => {
            let _ = app.lakekeeper.delete_warehouse(&warehouse_id).await;
            return Err(e);
        }
    };

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

    // Look up the warehouse-id so we can drop it from Lakekeeper too.
    // If the state row is missing we surface NotFound straight away.
    let row = run_state(app.state.clone(), {
        let name = name.clone();
        move |s| s.get_table_bucket(&name)
    })
    .await?;
    app.lakekeeper
        .delete_warehouse(&row.table_bucket_id)
        .await?;

    run_state(app.state.clone(), {
        let name = name.clone();
        move |s| s.delete_table_bucket(&name)
    })
    .await?;

    // AWS replies HTTP 204 with no body — match exactly (doc/GAP_ANALYSIS.md).
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

/// Look up the Lakekeeper warehouse-id for a bucket name. Used by the
/// namespace and table handlers to forward requests.
pub(crate) async fn resolve_warehouse_id(
    app: &AppState,
    bucket_name: &str,
) -> Result<String, AwsError> {
    let bucket = bucket_name.to_owned();
    let row = run_state(app.state.clone(), move |s| s.get_table_bucket(&bucket)).await?;
    Ok(row.table_bucket_id)
}

/// Bridge between async axum handlers and the synchronous DuckDB
/// `StateStore`. Same shape as the vectors-crate helper, kept duplicated
/// to avoid a coupling crate between the two façades.
pub(crate) async fn run_state<F, T>(state: Arc<dyn StateStore>, f: F) -> Result<T, AwsError>
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

fn storage_error(e: StorageError) -> AwsError {
    match e {
        StorageError::Backend(e) => AwsError::Internal {
            message: format!("{e:#}"),
        },
    }
}
