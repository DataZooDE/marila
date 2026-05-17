//! axum router for the s3vectors façade.
//!
//! AWS S3 Vectors is a `restJson1` service that exposes one POST endpoint
//! per operation at `/<OperationName>`, all under the service root. The
//! shape is captured live in CLAUDE.md C-2.

use axum::{Router, routing::post};

use crate::control_plane::{
    AppState, create_index, create_vector_bucket, delete_index, delete_vector_bucket, get_index,
    get_vector_bucket, list_indexes, list_vector_buckets,
};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/CreateVectorBucket", post(create_vector_bucket))
        .route("/ListVectorBuckets", post(list_vector_buckets))
        .route("/GetVectorBucket", post(get_vector_bucket))
        .route("/DeleteVectorBucket", post(delete_vector_bucket))
        .route("/CreateIndex", post(create_index))
        .route("/ListIndexes", post(list_indexes))
        .route("/GetIndex", post(get_index))
        .route("/DeleteIndex", post(delete_index))
        .with_state(state)
}
