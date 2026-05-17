use chrono::{DateTime, Utc};
use thiserror::Error;

/// One state row for a vector bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorBucketRow {
    pub name: String,
    pub arn: String,
    pub created_at: DateTime<Utc>,
}

/// One state row for a vector index. Indexes live under a bucket;
/// `(bucket_name, name)` is the composite primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub bucket_name: String,
    pub name: String,
    pub arn: String,
    pub dimension: u32,
    pub distance_metric: DistanceMetric,
    pub created_at: DateTime<Utc>,
}

/// The two distance metrics S3 Vectors supports today (CLAUDE.md C-2c).
///
/// Modelled as an enum (not a string) so handlers and the DuckDB
/// HNSW backing-table DDL can't drift apart on spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    Cosine,
    Euclidean,
}

impl DistanceMetric {
    /// String the AWS API uses on the wire.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::Euclidean => "euclidean",
        }
    }

    /// Parse the wire form. Returns `None` for an unknown metric; the
    /// vectors handler maps that to a `ValidationException`.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "cosine" => Some(Self::Cosine),
            "euclidean" => Some(Self::Euclidean),
            _ => None,
        }
    }
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

/// A page of [`VectorBucketRow`]s and an opaque cursor.
///
/// `next` is `Some` when there is more data — the value is meant to be
/// echoed back into the next `list_vector_buckets_page` call as
/// `after`. Marila's cursor is just the last name on the page, which is
/// stable because we order by name; the AWS SDK only requires it to be
/// opaque and round-trip-safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorBucketPage {
    pub rows: Vec<VectorBucketRow>,
    pub next: Option<String>,
}

/// CRUD over marila's local state.
///
/// All methods are synchronous: DuckDB is sync, and forcing async at this
/// boundary just hides a `spawn_blocking` somewhere. Handlers above
/// wrap calls in `tokio::task::spawn_blocking`, keeping concurrency
/// concerns in the HTTP layer where they belong.
pub trait StateStore: Send + Sync {
    fn create_vector_bucket(&self, name: &str, arn: &str) -> Result<VectorBucketRow, StateError>;

    /// Cursor-paginated list with an optional name prefix.
    ///
    /// `after` is exclusive — pass the previous page's `next` to get
    /// the next page. `max` is clamped to `[1, 500]` by the caller.
    fn list_vector_buckets_page(
        &self,
        prefix: Option<&str>,
        after: Option<&str>,
        max: usize,
    ) -> Result<VectorBucketPage, StateError>;

    /// Fetch one bucket by name.
    fn get_vector_bucket(&self, name: &str) -> Result<VectorBucketRow, StateError>;

    fn delete_vector_bucket(&self, name: &str) -> Result<(), StateError>;

    /// Create an index under `bucket`. The DuckDB impl also creates the
    /// backing table `vec_<bucket>__<index>` and the HNSW index on it.
    ///
    /// Errors:
    ///  - [`StateError::NotFound`] when the bucket doesn't exist
    ///    (mapped to AWS `NotFoundException`).
    ///  - [`StateError::AlreadyExists`] when the index exists
    ///    (mapped to `ConflictException`).
    #[allow(clippy::too_many_arguments)]
    fn create_index(
        &self,
        bucket: &str,
        index: &str,
        arn: &str,
        dimension: u32,
        distance_metric: DistanceMetric,
    ) -> Result<IndexRow, StateError>;

    /// Number of indexes under a bucket. Used by `DeleteVectorBucket`
    /// to enforce AWS's "bucket must be empty" rule (CLAUDE.md C-2c).
    fn count_indexes(&self, bucket: &str) -> Result<u64, StateError>;

    /// Drop an index and its backing table. Idempotent w.r.t. the
    /// backing table — missing index returns `StateError::NotFound`.
    fn delete_index(&self, bucket: &str, index: &str) -> Result<(), StateError>;
}
