use chrono::{DateTime, Utc};
use thiserror::Error;

/// One state row for a vector bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorBucketRow {
    pub name: String,
    pub arn: String,
    pub created_at: DateTime<Utc>,
}

/// Things that can go wrong talking to the state store.
///
/// `AlreadyExists` is its own variant so callers can map it to the
/// `ConflictException` wire shape without scraping error messages.
#[derive(Debug, Error)]
pub enum StateError {
    #[error("vector bucket `{0}` already exists")]
    AlreadyExists(String),

    #[error("vector bucket `{0}` not found")]
    NotFound(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// CRUD over marila's local state.
///
/// All methods are synchronous: DuckDB is sync, and forcing async at this
/// boundary just hides a `spawn_blocking` somewhere. Handlers above
/// wrap calls in `tokio::task::spawn_blocking`, keeping concurrency
/// concerns in the HTTP layer where they belong.
pub trait StateStore: Send + Sync {
    fn create_vector_bucket(&self, name: &str, arn: &str) -> Result<VectorBucketRow, StateError>;
    fn list_vector_buckets(&self) -> Result<Vec<VectorBucketRow>, StateError>;
    fn delete_vector_bucket(&self, name: &str) -> Result<(), StateError>;
}
