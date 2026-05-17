//! axum router for the s3tables façade.
//!
//! S3 Tables uses REST verbs + path-based routing — distinct from
//! s3vectors' all-POST `/<OperationName>` shape (CLAUDE.md C-9).

use axum::{
    Router,
    routing::{get, put},
};

use crate::control_plane::{
    AppState, create_table_bucket, delete_table_bucket, get_table_bucket, list_table_buckets,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        // Both /buckets variants (with and without trailing arn) are
        // separate routes in axum's path matcher.
        .route("/buckets", put(create_table_bucket).get(list_table_buckets))
        .route(
            "/buckets/{arn}",
            get(get_table_bucket).delete(delete_table_bucket),
        )
        .with_state(state)
}
