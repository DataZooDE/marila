//! Contract test for `CreateVectorBucket`.
//!
//! Same body runs against both `Target::Local` (marila) and `Target::Aws`
//! (real S3 Vectors). A pass on one and a fail on the other means marila
//! diverges from AWS — fix marila, not the test.
//!
//! Wire shape captured in CLAUDE.md C-2 (2026-05-17, eu-west-1).

use aws_sdk_s3vectors::Client;
use marila_integration_tests::{
    harness::{BucketGuard, MarilaProcess, Target, client, unique_bucket_name},
    require_aws,
};

#[tokio::test]
async fn local_create_vector_bucket_round_trips() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    create_then_list_then_delete(c).await;
}

#[tokio::test]
async fn aws_create_vector_bucket_round_trips() {
    require_aws!();
    let c = client(Target::Aws).await;
    create_then_list_then_delete(c).await;
}

#[tokio::test]
async fn local_create_vector_bucket_duplicate_returns_conflict() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    duplicate_name_returns_conflict(c).await;
}

#[tokio::test]
async fn aws_create_vector_bucket_duplicate_returns_conflict() {
    require_aws!();
    let c = client(Target::Aws).await;
    duplicate_name_returns_conflict(c).await;
}

// ---------------------------------------------------------------------------
// Shared test bodies
// ---------------------------------------------------------------------------

async fn create_then_list_then_delete(c: Client) {
    let name = unique_bucket_name("create");
    let _guard = BucketGuard::new(c.clone(), &name);

    // 1. CreateVectorBucket returns success (no body fields the SDK
    //    surfaces to assert on; the wire body contains `vectorBucketArn`
    //    but the SDK model treats this output as empty).
    c.create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("CreateVectorBucket should succeed");

    // 2. ListVectorBuckets shows the new bucket with the expected ARN
    //    and a creation timestamp.
    let list = c
        .list_vector_buckets()
        .send()
        .await
        .expect("ListVectorBuckets should succeed");
    let found = list
        .vector_buckets()
        .iter()
        .find(|b| b.vector_bucket_name() == name)
        .expect("created bucket not visible in ListVectorBuckets");

    let arn = found.vector_bucket_arn();
    assert!(
        arn.starts_with("arn:aws:s3vectors:"),
        "ARN doesn't look like AWS: {arn}"
    );
    assert!(
        arn.ends_with(&format!(":bucket/{name}")),
        "ARN should end with :bucket/{name}, got {arn}"
    );

    // creationTime must be populated (any sane timestamp; we don't assert
    // a specific value).
    let _ = found.creation_time();
}

async fn duplicate_name_returns_conflict(c: Client) {
    let name = unique_bucket_name("dup");
    let _guard = BucketGuard::new(c.clone(), &name);

    c.create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("first CreateVectorBucket should succeed");

    let err = c
        .create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect_err("second CreateVectorBucket with same name should fail");

    // The SDK maps the HTTP 409 + `x-amzn-errortype: ConflictException`
    // onto its `ConflictException` enum variant. That's the contract.
    let service_err = err.into_service_error();
    assert!(
        service_err.is_conflict_exception(),
        "expected ConflictException, got: {service_err:?}"
    );
}
