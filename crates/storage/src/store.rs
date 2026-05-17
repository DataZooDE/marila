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
}
