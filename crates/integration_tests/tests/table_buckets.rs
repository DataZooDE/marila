//! Contract tests for the s3tables bucket-level control plane:
//! CreateTableBucket / ListTableBuckets / GetTableBucket / DeleteTableBucket.
//!
//! Wire shape captured in CLAUDE.md C-9. The s3tables service is REST
//! (verb + path) — distinct from s3vectors' all-POST shape — so this
//! file uses `aws-sdk-s3tables` (not s3vectors).

use aws_sdk_s3tables::Client;
use marila_integration_tests::{
    harness::{MarilaProcess, Target, tables_client, unique_bucket_name},
    require_aws,
};

/// Create-and-cleanup helper that always deletes by ARN, even on panic,
/// running on the test's own tokio runtime per CLAUDE.md C-8.
async fn with_table_bucket<F, Fut>(c: Client, prefix: &str, body: F)
where
    F: FnOnce(Client, String, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;

    let name = unique_bucket_name(prefix);
    let created = c
        .create_table_bucket()
        .name(&name)
        .send()
        .await
        .expect("create table bucket");
    let arn = created.arn().to_owned();

    let outcome = AssertUnwindSafe(body(c.clone(), name.clone(), arn.clone()))
        .catch_unwind()
        .await;

    let _ = c.delete_table_bucket().table_bucket_arn(&arn).send().await;

    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

// ---------------------------------------------------------------------------
// Create + list round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_create_table_bucket_round_trips() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    with_table_bucket(c, "tbcreate", create_then_list).await;
}

#[tokio::test]
async fn aws_create_table_bucket_round_trips() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_table_bucket(c, "tbcreate", create_then_list).await;
}

async fn create_then_list(c: Client, name: String, arn: String) {
    assert!(arn.starts_with("arn:aws:s3tables:"), "ARN: {arn}");
    assert!(
        arn.ends_with(&format!(":bucket/{name}")),
        "ARN must end :bucket/{name}, got {arn}"
    );

    let list = c
        .list_table_buckets()
        .send()
        .await
        .expect("ListTableBuckets");
    let found = list
        .table_buckets()
        .iter()
        .find(|b| b.name() == name)
        .expect("created bucket missing from List");
    assert_eq!(found.arn(), arn);
    assert_eq!(found.r#type().map(|t| t.as_str()), Some("customer"));
    let _ = found.created_at();
    let _ = found.owner_account_id();
    let _ = found.table_bucket_id();
}

// ---------------------------------------------------------------------------
// Get by ARN
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_get_table_bucket_by_arn() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    with_table_bucket(c, "tbget", get_round_trip).await;
}

#[tokio::test]
async fn aws_get_table_bucket_by_arn() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_table_bucket(c, "tbget", get_round_trip).await;
}

async fn get_round_trip(c: Client, name: String, arn: String) {
    let got = c
        .get_table_bucket()
        .table_bucket_arn(&arn)
        .send()
        .await
        .expect("GetTableBucket");
    assert_eq!(got.arn(), arn);
    assert_eq!(got.name(), name);
    assert_eq!(got.r#type().map(|t| t.as_str()), Some("customer"));
}

// ---------------------------------------------------------------------------
// Get missing → NotFoundException
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_get_missing_table_bucket_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    missing_returns_not_found(c).await;
}

#[tokio::test]
async fn aws_get_missing_table_bucket_is_not_found() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    missing_returns_not_found(c).await;
}

async fn missing_returns_not_found(c: Client) {
    let arn = format!(
        "arn:aws:s3tables:eu-west-1:625644349722:bucket/{}",
        unique_bucket_name("tbmiss")
    );
    let err = c
        .get_table_bucket()
        .table_bucket_arn(&arn)
        .send()
        .await
        .expect_err("GetTableBucket on missing must error");
    let svc = err.into_service_error();
    assert!(
        svc.is_not_found_exception(),
        "expected NotFoundException, got: {svc:?}"
    );
}

// ---------------------------------------------------------------------------
// Delete missing → NotFoundException
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_delete_missing_table_bucket_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    delete_missing_is_not_found(c).await;
}

#[tokio::test]
async fn aws_delete_missing_table_bucket_is_not_found() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    delete_missing_is_not_found(c).await;
}

async fn delete_missing_is_not_found(c: Client) {
    let arn = format!(
        "arn:aws:s3tables:eu-west-1:625644349722:bucket/{}",
        unique_bucket_name("tbdelmiss")
    );
    let err = c
        .delete_table_bucket()
        .table_bucket_arn(&arn)
        .send()
        .await
        .expect_err("DeleteTableBucket on missing must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException on missing delete"
    );
}
