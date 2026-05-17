//! AWS S3 Tables façade.
//!
//! Distinct from `marila-vectors` in two ways:
//!
//! 1. The wire protocol is REST+JSON (verb-and-path routing) rather
//!    than the all-POST `/<OperationName>` shape s3vectors uses
//!    (CLAUDE.md C-9).
//! 2. Buckets here are *table buckets* — analogues of Lakekeeper
//!    warehouses, separate state schema from vector buckets.
//!
//! Today this crate covers bucket-level control plane only. Namespace
//! and table operations land in a later round when Lakekeeper integration
//! is wired in.

mod arn;
mod control_plane;
mod routes;
mod state;

pub use control_plane::AppState;
pub use routes::router;
