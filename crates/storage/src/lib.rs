//! Object-storage adapter for marila.
//!
//! Wraps `aws-sdk-s3` to talk to RustFS (or any S3-compatible backend) and
//! exposes a narrow `BucketStore` trait so the rest of the system depends
//! on behaviour, not on AWS SDK types. The trait + impl land alongside
//! the first slice that needs them (`CreateVectorBucket`).
