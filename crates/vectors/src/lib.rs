//! AWS S3 Vectors façade.
//!
//! Each operation (`CreateVectorBucket`, `ListVectorBuckets`, …) is a thin
//! handler that translates the AWS wire shape into calls against
//! [`marila_core`] (state) and [`marila_storage`] (object store). Handlers
//! land alongside their integration tests.
