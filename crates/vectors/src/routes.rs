//! axum router for the s3vectors façade.
//!
//! AWS S3 Vectors is a `restJson1` service that exposes one POST endpoint
//! per operation at `/<OperationName>`, all under the service root. The
//! shape is captured live in CLAUDE.md C-2.

use axum::{Router, Json, routing::post};
use marila_aws_compat::AwsError;

use crate::control_plane::{
    AppState, create_index, create_vector_bucket, delete_index, delete_vector_bucket,
    delete_vectors, get_index, get_vector_bucket, get_vectors, list_indexes, list_vector_buckets,
    list_vectors, put_vectors, query_vectors,
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
        .route("/PutVectors", post(put_vectors))
        .route("/GetVectors", post(get_vectors))
        .route("/ListVectors", post(list_vectors))
        .route("/DeleteVectors", post(delete_vectors))
        .route("/QueryVectors", post(query_vectors))
        // ---- FV-7: deliberately-not-implemented ops return 501 ----
        // Policies + tagging exist on AWS S3 Vectors but marila doesn't
        // model them. The `NotImplementedException` envelope tells SDK
        // clients we recognised the op but won't handle it, so they can
        // distinguish from `UnknownOperationException` (which is 404).
        .route("/PutVectorBucketPolicy", post(unimplemented_put_vector_bucket_policy))
        .route("/GetVectorBucketPolicy", post(unimplemented_get_vector_bucket_policy))
        .route("/DeleteVectorBucketPolicy", post(unimplemented_delete_vector_bucket_policy))
        .route("/ListTagsForResource", post(unimplemented_list_tags_for_resource))
        .route("/TagResource", post(unimplemented_tag_resource))
        .route("/UntagResource", post(unimplemented_untag_resource))
        .with_state(state)
}

/// Generates a 501 handler for each unimplemented op. Each generated fn
/// returns a `NotImplementedException` carrying the op name so clients
/// can disambiguate without inspecting the URL.
macro_rules! unimplemented_handler {
    ($fn_name:ident, $op_name:expr) => {
        async fn $fn_name(
            _body: Option<Json<serde_json::Value>>,
        ) -> Result<Json<serde_json::Value>, AwsError> {
            Err(AwsError::NotImplemented(format!(
                "s3vectors:{} is not implemented by marila (REQUIREMENTS.md FV-7)",
                $op_name
            )))
        }
    };
}

unimplemented_handler!(unimplemented_put_vector_bucket_policy, "PutVectorBucketPolicy");
unimplemented_handler!(unimplemented_get_vector_bucket_policy, "GetVectorBucketPolicy");
unimplemented_handler!(unimplemented_delete_vector_bucket_policy, "DeleteVectorBucketPolicy");
unimplemented_handler!(unimplemented_list_tags_for_resource, "ListTagsForResource");
unimplemented_handler!(unimplemented_tag_resource, "TagResource");
unimplemented_handler!(unimplemented_untag_resource, "UntagResource");
