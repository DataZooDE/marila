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

    /// Vector data length didn't match the index's configured
    /// dimension. Carries `(got, expected)` so the handler can shape
    /// the AWS message exactly.
    #[error("vector dimension mismatch: got {got}, expected {expected}")]
    DimensionMismatch { got: usize, expected: usize },

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

/// A page of [`IndexRow`]s scoped to one bucket, with an opaque cursor
/// pointing at the next index name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPage {
    pub rows: Vec<IndexRow>,
    pub next: Option<String>,
}

/// One vector to write into a backing table.
#[derive(Debug, Clone)]
pub struct VectorWrite {
    pub key: String,
    pub data: Vec<f32>,
    /// Free-form JSON metadata. `None` is serialised as NULL in DuckDB.
    pub metadata: Option<serde_json::Value>,
}

/// One vector returned from a backing table.
///
/// `data` / `metadata` are `None` when the caller asked us not to
/// materialise them — saves a round-trip through serde for List-large
/// scans that only need keys.
#[derive(Debug, Clone)]
pub struct VectorRead {
    pub key: String,
    pub data: Option<Vec<f32>>,
    pub metadata: Option<serde_json::Value>,
}

/// Cursor-paginated page of vectors. `next` is the last key on the page
/// (the cursor is opaque to the wire — clients just echo it back).
#[derive(Debug, Clone)]
pub struct VectorPage {
    pub rows: Vec<VectorRead>,
    pub next: Option<String>,
}

/// One state row for an s3tables table bucket.
///
/// Distinct from [`VectorBucketRow`] — table buckets have UUID,
/// owner-account, and a `bucket_type` ("customer"|"aws") per AWS's
/// wire shape (CLAUDE.md C-9). They share zero schema with vector
/// buckets so they live in their own table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableBucketRow {
    pub name: String,
    pub arn: String,
    pub table_bucket_id: String,
    pub owner_account_id: String,
    pub bucket_type: String,
    pub created_at: DateTime<Utc>,
}

/// One nearest-neighbour query result.
#[derive(Debug, Clone)]
pub struct QueryHit {
    pub key: String,
    /// Always populated; the handler decides whether to surface it
    /// based on the wire-level `returnDistance` flag.
    pub distance: f64,
    pub data: Option<Vec<f32>>,
    pub metadata: Option<serde_json::Value>,
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

    /// Cursor-paginated list of indexes within a bucket.
    ///
    /// Errors:
    /// - [`StateError::NotFound`] when the bucket itself doesn't exist
    ///   (mapped to AWS `NotFoundException` with the *bucket* body text).
    fn list_indexes_page(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        after: Option<&str>,
        max: usize,
    ) -> Result<IndexPage, StateError>;

    /// Fetch one index by bucket + name.
    ///
    /// Returns [`StateError::NotFound`] for either a missing bucket or
    /// a missing index. The handler maps both to the same
    /// AWS-NotFoundException-with-index-body text (CLAUDE.md C-2d).
    fn get_index(&self, bucket: &str, index: &str) -> Result<IndexRow, StateError>;

    /// Drop an index and its backing table. Idempotent w.r.t. the
    /// backing table — missing index returns `StateError::NotFound`.
    fn delete_index(&self, bucket: &str, index: &str) -> Result<(), StateError>;

    // -----------------------------------------------------------------
    // Data plane — operates on the backing table `vec_<b>__<i>`.
    // All four methods return `StateError::NotFound` when the index
    // doesn't exist; the handler maps that to AWS's
    // "The specified index could not be found" body (CLAUDE.md C-2e).
    // -----------------------------------------------------------------

    /// Upsert one or more vectors. AWS treats PutVectors as a
    /// "replace if key collides" operation; we mirror that with
    /// `INSERT OR REPLACE`.
    ///
    /// Each vector's `data.len()` must match the index's `dimension`;
    /// callers should validate before calling. The store checks anyway
    /// as defence-in-depth and returns a [`StateError::DimensionMismatch`].
    fn put_vectors(
        &self,
        bucket: &str,
        index: &str,
        vectors: &[VectorWrite],
    ) -> Result<(), StateError>;

    /// Fetch vectors by key. Missing keys are silently omitted from the
    /// returned `Vec` — that matches the AWS contract (CLAUDE.md C-2e).
    fn get_vectors(
        &self,
        bucket: &str,
        index: &str,
        keys: &[String],
        return_data: bool,
        return_metadata: bool,
    ) -> Result<Vec<VectorRead>, StateError>;

    /// Cursor-paginated scan of an index. `after` is exclusive
    /// (echo back `page.next` for the next call). `max` is clamped by
    /// the caller.
    fn list_vectors_page(
        &self,
        bucket: &str,
        index: &str,
        after: Option<&str>,
        max: usize,
        return_data: bool,
        return_metadata: bool,
    ) -> Result<VectorPage, StateError>;

    /// Delete one or more vectors by key. Missing keys are not an
    /// error — AWS's contract is silently idempotent (CLAUDE.md C-2e).
    fn delete_vectors(&self, bucket: &str, index: &str, keys: &[String]) -> Result<(), StateError>;

    /// Top-K nearest-neighbour query.
    ///
    /// `query` must have the same dimension as the index. `where_sql`
    /// is an *already-translated* SQL fragment from
    /// `crates/vectors::filter::translate` — the state store doesn't
    /// parse Mongo filters. Pass `None` for an unfiltered query.
    ///
    /// `oversample` is the multiplier applied to `top_k` before
    /// post-filtering — see CLAUDE.md C-2f. Pass 1 for no oversampling;
    /// values above 1 mitigate the post-filter recall collapse described
    /// in `doc/DISCOVERIES.md` D-12.
    fn query_vectors(
        &self,
        bucket: &str,
        index: &str,
        query: &[f32],
        top_k: usize,
        oversample: usize,
        where_sql: Option<&str>,
    ) -> Result<Vec<QueryHit>, StateError>;

    // -----------------------------------------------------------------
    // s3tables — table-bucket control plane (CLAUDE.md C-9).
    // Schema and behaviour are separate from vector buckets so that
    // future tables-side work (namespaces, tables, Lakekeeper proxy)
    // doesn't bleed into the vectors schema.
    // -----------------------------------------------------------------

    fn create_table_bucket(
        &self,
        name: &str,
        arn: &str,
        table_bucket_id: &str,
        owner_account_id: &str,
    ) -> Result<TableBucketRow, StateError>;

    fn list_table_buckets(&self) -> Result<Vec<TableBucketRow>, StateError>;

    /// Fetch a table bucket by name. Returns [`StateError::NotFound`]
    /// when missing.
    fn get_table_bucket(&self, name: &str) -> Result<TableBucketRow, StateError>;

    fn delete_table_bucket(&self, name: &str) -> Result<(), StateError>;
}
