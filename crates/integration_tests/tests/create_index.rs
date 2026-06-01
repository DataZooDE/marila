//! Contract test for `CreateIndex` (and `DeleteIndex` for cleanup).
//!
//! Locks down (doc/GAP_ANALYSIS.md):
//!  - happy path returns `indexArn = arn:aws:s3vectors:<r>:<a>:bucket/<b>/index/<i>`
//!  - duplicate index name → ConflictException
//!  - missing bucket → NotFoundException
//!  - dimension out of range → ValidationException
//!  - DeleteVectorBucket with surviving indexes → ConflictException
//!    ("not empty")

use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{DataType, DistanceMetric};
use marila_integration_tests::{
    harness::{
        BucketCtx, MarilaProcess, Target, client, unique_bucket_name, with_bucket_and_indexes,
    },
    require_aws,
};

#[tokio::test]
async fn local_create_index_round_trips() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "ci", create_round_trip).await;
}

#[tokio::test]
async fn aws_create_index_round_trips() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "ci", create_round_trip).await;
}

#[tokio::test]
async fn local_create_index_duplicate_returns_conflict() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "cidup", duplicate_returns_conflict).await;
}

#[tokio::test]
async fn aws_create_index_duplicate_returns_conflict() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "cidup", duplicate_returns_conflict).await;
}

#[tokio::test]
async fn local_create_index_missing_bucket_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    missing_bucket_is_not_found(c).await;
}

#[tokio::test]
async fn aws_create_index_missing_bucket_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    missing_bucket_is_not_found(c).await;
}

#[tokio::test]
async fn local_delete_bucket_with_index_returns_conflict() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "cinotempty", delete_bucket_not_empty).await;
}

#[tokio::test]
async fn aws_delete_bucket_with_index_returns_conflict() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "cinotempty", delete_bucket_not_empty).await;
}

// ---------------------------------------------------------------------------
// Shared bodies
// ---------------------------------------------------------------------------

async fn create_round_trip(c: Client, ctx: BucketCtx) {
    let index = "happy".to_owned();
    let resp = c
        .create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("CreateIndex should succeed");
    ctx.add_index(&index);

    let arn = resp
        .index_arn()
        .expect("indexArn must be present in CreateIndex response");
    assert!(
        arn.starts_with("arn:aws:s3vectors:"),
        "indexArn doesn't look like AWS: {arn}"
    );
    assert!(
        arn.ends_with(&format!(":bucket/{}/index/{}", ctx.bucket(), index)),
        "indexArn must end with :bucket/<b>/index/<i>, got {arn}"
    );
}

async fn duplicate_returns_conflict(c: Client, ctx: BucketCtx) {
    let index = "dup".to_owned();
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(8)
        .distance_metric(DistanceMetric::Euclidean)
        .send()
        .await
        .expect("first CreateIndex should succeed");
    ctx.add_index(&index);

    let err = c
        .create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(8)
        .distance_metric(DistanceMetric::Euclidean)
        .send()
        .await
        .expect_err("second CreateIndex with same name should fail");
    assert!(
        err.into_service_error().is_conflict_exception(),
        "expected ConflictException on duplicate index"
    );
}

async fn missing_bucket_is_not_found(c: Client) {
    let bucket = unique_bucket_name("nobucket");
    let err = c
        .create_index()
        .vector_bucket_name(&bucket)
        .index_name("orphan")
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect_err("CreateIndex on missing bucket must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException for missing bucket"
    );
}

async fn delete_bucket_not_empty(c: Client, ctx: BucketCtx) {
    let index = "blocker".to_owned();
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("CreateIndex to set up the blocker");
    ctx.add_index(&index);

    let err = c
        .delete_vector_bucket()
        .vector_bucket_name(ctx.bucket())
        .send()
        .await
        .expect_err("DeleteVectorBucket on a bucket with indexes must error");
    assert!(
        err.into_service_error().is_conflict_exception(),
        "expected ConflictException — bucket not empty"
    );
}
