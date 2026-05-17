//! Wire-shape DTOs for the `s3vectors` AWS-JSON façade.
//!
//! Field names match the lowercase-camelCase shapes captured live
//! (CLAUDE.md C-2). `serde(rename_all = "camelCase")` would *almost*
//! work but Smithy's `camelCase` for `vectorBucketName` actually maps
//! to `vector_bucket_name` in Rust → `vectorBucketName` on the wire,
//! which is what we want, so we use `rename_all`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateVectorBucketInput {
    pub vector_bucket_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateVectorBucketOutput {
    pub vector_bucket_arn: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ListVectorBucketsInput {
    pub max_results: Option<u32>,
    pub next_token: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListVectorBucketsOutput {
    pub vector_buckets: Vec<VectorBucketSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorBucketSummary {
    pub vector_bucket_name: String,
    pub vector_bucket_arn: String,
    /// Epoch-seconds as a JSON number — the `restJson1` default and the
    /// shape S3 Vectors actually sends on the wire (CLAUDE.md C-2a).
    /// The `aws` CLI re-renders this as ISO 8601, which is misleading.
    pub creation_time: i64,
}

impl VectorBucketSummary {
    pub fn from_row(name: String, arn: String, created_at: DateTime<Utc>) -> Self {
        Self {
            vector_bucket_name: name,
            vector_bucket_arn: arn,
            creation_time: created_at.timestamp(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteVectorBucketInput {
    pub vector_bucket_name: String,
}
