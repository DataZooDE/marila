//! AWS S3 Vectors façade.
//!
//! Each operation (`CreateVectorBucket`, `ListVectorBuckets`, …) is a thin
//! handler that translates the AWS wire shape into calls against
//! [`marila_core`] (state) and [`marila_storage`] (object store).

mod arn;
mod control_plane;
mod filter;
mod rehydrate;
mod routes;
mod state;

pub use control_plane::AppState;
pub use rehydrate::rehydrate_from_snapshots;
pub use routes::router;
