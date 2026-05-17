//! Object-storage adapter for marila.
//!
//! Exposes a narrow `BucketStore` trait so the rest of the system depends
//! on behaviour, not on AWS-SDK types. The `S3BucketStore` impl wraps
//! `aws-sdk-s3` against any S3-compatible backend (RustFS in dev,
//! plausibly MinIO or AWS itself in other environments).

mod s3_store;
mod store;

pub use s3_store::{S3BucketStore, S3Config};
pub use store::{BucketStore, StorageError};
