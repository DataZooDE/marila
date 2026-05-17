//! Local state for marila — a thin DuckDB-backed schema with a
//! storage-agnostic `StateStore` trait so handlers depend on the
//! contract, not on `duckdb` types.

mod duckdb_store;
mod state;

pub use duckdb_store::DuckDbStateStore;
pub use state::{StateError, StateStore, VectorBucketRow};
