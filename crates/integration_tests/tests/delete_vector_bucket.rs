//! Contract test for `DeleteVectorBucket`.
//!
//! Locks down:
//!  - happy path by name (already covered transitively by cleanup, but
//!    asserted explicitly here so the operation has its own ledger
//!    entry)
//!  - happy path by ARN
//!  - `NotFoundException` for a missing bucket
//!
//! Wire shape captured in CLAUDE.md C-2b.

use aws_sdk_s3vectors::Client;
use marila_integration_tests::{
    harness::{MarilaProcess, Target, client, unique_bucket_name},
    require_aws,
};

#[tokio::test]
async fn local_delete_vector_bucket_by_name_then_gone() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    delete_by_name_then_gone(c).await;
}

#[tokio::test]
async fn aws_delete_vector_bucket_by_name_then_gone() {
    require_aws!();
    let c = client(Target::Aws).await;
    delete_by_name_then_gone(c).await;
}

#[tokio::test]
async fn local_delete_vector_bucket_by_arn() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    delete_by_arn(c).await;
}

#[tokio::test]
async fn aws_delete_vector_bucket_by_arn() {
    require_aws!();
    let c = client(Target::Aws).await;
    delete_by_arn(c).await;
}

#[tokio::test]
async fn local_delete_missing_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    missing_is_not_found(c).await;
}

#[tokio::test]
async fn aws_delete_missing_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    missing_is_not_found(c).await;
}

// ---------------------------------------------------------------------------
// Shared bodies — these manage bucket lifecycle themselves (no with_bucket
// wrapper, because the test *is* the delete).
// ---------------------------------------------------------------------------

async fn delete_by_name_then_gone(c: Client) {
    let name = unique_bucket_name("deln");
    c.create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("create");

    c.delete_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("delete by name");

    // Verify it's gone via Get.
    let err = c
        .get_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect_err("Get on a deleted bucket must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "deleted bucket should report NotFoundException"
    );
}

async fn delete_by_arn(c: Client) {
    let name = unique_bucket_name("dela");
    c.create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("create");

    let listed = c
        .list_vector_buckets()
        .prefix(&name)
        .send()
        .await
        .expect("list to grab ARN");
    let arn = listed
        .vector_buckets()
        .iter()
        .find(|b| b.vector_bucket_name() == name)
        .map(|b| b.vector_bucket_arn().to_owned())
        .expect("just-created bucket should be listable");

    c.delete_vector_bucket()
        .vector_bucket_arn(&arn)
        .send()
        .await
        .expect("delete by arn");
}

async fn missing_is_not_found(c: Client) {
    let missing = unique_bucket_name("delmiss");
    let err = c
        .delete_vector_bucket()
        .vector_bucket_name(&missing)
        .send()
        .await
        .expect_err("Delete on missing bucket must error");
    let svc = err.into_service_error();
    assert!(
        svc.is_not_found_exception(),
        "expected NotFoundException, got: {svc:?}"
    );
}
