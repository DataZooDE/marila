//! Local state for marila — a thin DuckDB-backed schema with a
//! storage-agnostic `StateStore` trait so handlers depend on the
//! contract, not on `duckdb` types.

mod duckdb_store;
mod state;
mod vss;

pub use duckdb_store::DuckDbStateStore;
pub use state::{
    DistanceMetric, IndexPage, IndexRow, StateError, StateStore, VectorBucketPage, VectorBucketRow,
};
