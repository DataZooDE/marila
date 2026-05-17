use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

/// Bucket-level operations marila uses against its object-store backend.
///
/// Kept narrow on purpose: callers depend on this trait, not on
/// `aws-sdk-s3`'s types. Swapping the backend (e.g. RustFS → MinIO →
/// real AWS) is then a single-crate change per quality requirement Q-4.
#[async_trait]
pub trait BucketStore: Send + Sync {
    /// Create the bucket if missing. Idempotent: a pre-existing bucket
    /// owned by the same credentials is **not** an error — that lines
    /// up with how `CreateVectorBucket` re-tries should behave (the
    /// state row is what guards duplicates, not the object store).
    async fn ensure_bucket(&self, name: &str) -> Result<(), StorageError>;

    /// Delete the bucket. Missing bucket is not an error so the
    /// `DeleteVectorBucket` path can be replayed safely.
    async fn delete_bucket(&self, name: &str) -> Result<(), StorageError>;

    /// Write `body` as the object at `<bucket>/<key>`. Overwrites if
    /// present (S3 PUT semantics). Used by `PutVectors` to land the
    /// JSON snapshot before the DuckDB INSERT per `REQUIREMENTS.md`
    /// FV-4 ("RustFS is the durable source of truth").
    async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<(), StorageError>;

    /// Delete `<bucket>/<key>`. Missing object is not an error.
    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError>;

    /// Fetch `<bucket>/<key>`. Returns `None` for missing object so the
    /// caller can decide whether absence is an error (it isn't for the
    /// rebuild-from-snapshot path).
    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>, StorageError>;
}
