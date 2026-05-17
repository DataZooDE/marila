//! Wire-shape DTOs for the `s3vectors` AWS-JSON façade.
//!
//! Field names match the lowercase-camelCase shapes captured live
//! (CLAUDE.md C-2, C-2a, C-2b).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// CreateVectorBucket
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// ListVectorBuckets
// ---------------------------------------------------------------------------

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
    /// **Absent** (not `null`) when there are no further pages — matches
    /// the AWS wire shape (CLAUDE.md C-2b).
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

// ---------------------------------------------------------------------------
// GetVectorBucket
// ---------------------------------------------------------------------------

/// Either `vectorBucketName` or `vectorBucketArn` may be supplied — clients
/// pick exactly one. The handler treats "neither" / "both" as ValidationException.
#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct GetVectorBucketInput {
    pub vector_bucket_name: Option<String>,
    pub vector_bucket_arn: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVectorBucketOutput {
    pub vector_bucket: VectorBucketDescription,
}

/// The fully-populated bucket struct AWS returns from Get/GetByArn —
/// includes the always-present `encryptionConfiguration` (CLAUDE.md C-2b).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorBucketDescription {
    pub vector_bucket_name: String,
    pub vector_bucket_arn: String,
    pub creation_time: i64,
    pub encryption_configuration: EncryptionConfiguration,
}

impl VectorBucketDescription {
    pub fn from_row(name: String, arn: String, created_at: DateTime<Utc>) -> Self {
        Self {
            vector_bucket_name: name,
            vector_bucket_arn: arn,
            creation_time: created_at.timestamp(),
            encryption_configuration: EncryptionConfiguration::default_sse_s3(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionConfiguration {
    pub sse_type: String,
}

impl EncryptionConfiguration {
    /// SSE-S3, AES-256 — the default AWS applies when CreateVectorBucket
    /// is called without an `encryptionConfiguration`.
    pub fn default_sse_s3() -> Self {
        Self {
            sse_type: "AES256".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// DeleteVectorBucket
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DeleteVectorBucketInput {
    pub vector_bucket_name: Option<String>,
    pub vector_bucket_arn: Option<String>,
}

// ---------------------------------------------------------------------------
// CreateIndex
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct CreateIndexInput {
    pub vector_bucket_name: Option<String>,
    pub vector_bucket_arn: Option<String>,
    pub index_name: Option<String>,
    pub data_type: Option<String>,
    pub dimension: Option<i64>,
    pub distance_metric: Option<String>,
    // Accepted but unused for the spike — present so clients that send
    // them don't get a 400 from serde.
    pub encryption_configuration: Option<serde_json::Value>,
    pub metadata_configuration: Option<serde_json::Value>,
    pub tags: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateIndexOutput {
    pub index_arn: String,
}

// ---------------------------------------------------------------------------
// DeleteIndex
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DeleteIndexInput {
    pub vector_bucket_name: Option<String>,
    pub vector_bucket_arn: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
}

// ---------------------------------------------------------------------------
// ListIndexes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ListIndexesInput {
    pub vector_bucket_name: Option<String>,
    pub vector_bucket_arn: Option<String>,
    pub prefix: Option<String>,
    pub max_results: Option<u32>,
    pub next_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListIndexesOutput {
    pub indexes: Vec<IndexSummary>,
    /// Absent (not null) when no further pages — matches the AWS wire
    /// shape captured in CLAUDE.md C-2d.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

/// Summary item returned by `ListIndexes` — note the SHAPE is intentionally
/// narrower than `IndexDescription` (no dataType/dimension/distanceMetric).
/// Those live on GetIndex only (CLAUDE.md C-2d).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSummary {
    pub vector_bucket_name: String,
    pub index_name: String,
    pub index_arn: String,
    pub creation_time: i64,
}

impl IndexSummary {
    pub fn from_row(row: marila_core::IndexRow) -> Self {
        Self {
            vector_bucket_name: row.bucket_name,
            index_name: row.name,
            index_arn: row.arn,
            creation_time: row.created_at.timestamp(),
        }
    }
}

// ---------------------------------------------------------------------------
// GetIndex
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct GetIndexInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetIndexOutput {
    pub index: IndexDescription,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexDescription {
    pub vector_bucket_name: String,
    pub index_name: String,
    pub index_arn: String,
    pub creation_time: i64,
    pub data_type: String,
    pub dimension: u32,
    pub distance_metric: String,
    pub encryption_configuration: EncryptionConfiguration,
}

impl IndexDescription {
    pub fn from_row(row: marila_core::IndexRow) -> Self {
        Self {
            vector_bucket_name: row.bucket_name,
            index_name: row.name,
            index_arn: row.arn,
            creation_time: row.created_at.timestamp(),
            data_type: "float32".to_owned(),
            dimension: row.dimension,
            distance_metric: row.distance_metric.as_wire().to_owned(),
            encryption_configuration: EncryptionConfiguration::default_sse_s3(),
        }
    }
}

// ---------------------------------------------------------------------------
// Data-plane DTOs (PutVectors / GetVectors / ListVectors / DeleteVectors)
// ---------------------------------------------------------------------------

/// `data` is a tagged union — only `float32` is allowed today
/// (CLAUDE.md C-2e). Wrapping as a struct with `Option<Vec<f32>>` lets
/// serde reject unknown variants on deserialise and skip the field on
/// serialise when it's absent.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct VectorData {
    pub float32: Option<Vec<f32>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PutVectorsItem {
    pub key: String,
    pub data: VectorData,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct PutVectorsInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
    pub vectors: Vec<PutVectorsItem>,
}

#[derive(Debug, Serialize, Default)]
pub struct PutVectorsOutput {}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct GetVectorsInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
    pub keys: Vec<String>,
    pub return_data: Option<bool>,
    pub return_metadata: Option<bool>,
}

/// Returned vector — `data`/`metadata` are omitted entirely when the
/// client asked us not to materialise them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnedVector {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<VectorData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ReturnedVector {
    pub fn from_read(v: marila_core::VectorRead) -> Self {
        Self {
            key: v.key,
            data: v.data.map(|d| VectorData { float32: Some(d) }),
            metadata: v.metadata,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVectorsOutput {
    pub vectors: Vec<ReturnedVector>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ListVectorsInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
    pub max_results: Option<u32>,
    pub next_token: Option<String>,
    pub return_data: Option<bool>,
    pub return_metadata: Option<bool>,
    // segmentCount / segmentIndex are accepted-but-ignored — parallel
    // scans aren't part of marila's spike scope.
    pub segment_count: Option<u32>,
    pub segment_index: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListVectorsOutput {
    pub vectors: Vec<ReturnedVector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct DeleteVectorsInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
    pub keys: Vec<String>,
}

// ---------------------------------------------------------------------------
// QueryVectors
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct QueryVectorsInput {
    pub vector_bucket_name: Option<String>,
    pub index_name: Option<String>,
    pub index_arn: Option<String>,
    pub top_k: Option<u32>,
    pub query_vector: Option<VectorData>,
    pub filter: Option<serde_json::Value>,
    pub return_distance: Option<bool>,
    pub return_metadata: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryVectorsOutput {
    /// The index's configured distance metric (echoed back per the AWS
    /// wire shape captured in CLAUDE.md C-2f).
    pub distance_metric: String,
    pub vectors: Vec<QueryVectorsHit>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryVectorsHit {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<VectorData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}
