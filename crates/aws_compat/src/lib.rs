//! Cross-service AWS wire-compatibility primitives.
//!
//! Currently exposes a `restJson1`-flavoured error envelope used by all
//! `s3vectors` handlers (`s3tables` uses `awsJson1_0` and will get its
//! own envelope when that side comes online).
//!
//! See `CLAUDE.md` C-1 for the captured wire shape:
//!
//! ```text
//! HTTP/1.1 409 Conflict
//! Content-Type: application/json
//! x-amzn-errortype: ConflictException
//!
//! {"message":"A vector bucket with the specified name already exists"}
//! ```

mod error;

pub use error::{AwsError, RestJsonError};
