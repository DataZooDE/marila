//! End-to-end contract tests for marila.
//!
//! Each test runs against two targets — local marila and real AWS — using
//! the same `aws-sdk-s3vectors` client code. A divergence is a marila
//! bug, not a test bug.
//!
//! See `doc/DISCOVERIES.md` "Methodology — AWS-contract-first TDD" for the why.

pub mod harness;
