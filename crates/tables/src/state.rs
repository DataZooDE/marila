//! Wire-shape DTOs for the `s3tables` REST+JSON façade.
//!
//! Field names match the camelCase shapes captured live (doc/GAP_ANALYSIS.md).

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTableBucketInput {
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTableBucketOutput {
    pub arn: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTableBucketsOutput {
    pub table_buckets: Vec<TableBucketSummary>,
}

/// Single bucket entry returned by both ListTableBuckets and
/// GetTableBucket (they have the same shape — doc/GAP_ANALYSIS.md).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableBucketSummary {
    pub arn: String,
    /// ISO 8601 with nanosecond precision, UTC `Z` suffix —
    /// matches AWS verbatim (e.g. `"2026-05-17T19:26:46.216057410Z"`).
    pub created_at: String,
    pub name: String,
    pub owner_account_id: String,
    pub table_bucket_id: String,
    /// `"customer"` for buckets marila creates; `"aws"` would be for
    /// service-owned buckets which we don't model.
    #[serde(rename = "type")]
    pub bucket_type: String,
}

impl TableBucketSummary {
    pub fn from_row(row: marila_core::TableBucketRow) -> Self {
        Self {
            arn: row.arn,
            created_at: format_iso8601_nanos(row.created_at),
            name: row.name,
            owner_account_id: row.owner_account_id,
            table_bucket_id: row.table_bucket_id,
            bucket_type: row.bucket_type,
        }
    }
}

/// Render `dt` as ISO 8601 with nanosecond precision + `Z` suffix —
/// the exact shape S3 Tables emits on the wire (doc/GAP_ANALYSIS.md).
///
/// chrono's `to_rfc3339_opts(Nanos, true)` gives us that format
/// with a single allocation.
fn format_iso8601_nanos(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Nanos, true)
}
