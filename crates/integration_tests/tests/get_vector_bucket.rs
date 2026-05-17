//! Contract test for `GetVectorBucket`.
//!
//! Locks down:
//!  - lookup by `vectorBucketName`
//!  - lookup by `vectorBucketArn`
//!  - default `encryptionConfiguration.sseType == "AES256"` in the
//!    response (CLAUDE.md C-2b)
//!  - `NotFoundException` for a missing bucket
//!
//! Wire shape captured in CLAUDE.md C-2b.

use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::SseType;
use marila_integration_tests::{
    harness::{MarilaProcess, Target, client, unique_bucket_name, with_bucket},
    require_aws,
};

#[tokio::test]
async fn local_get_vector_bucket_by_name_returns_default_encryption() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket(c, "getn", get_by_name).await;
}

#[tokio::test]
async fn aws_get_vector_bucket_by_name_returns_default_encryption() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket(c, "getn", get_by_name).await;
}

#[tokio::test]
async fn local_get_vector_bucket_by_arn() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket(c, "geta", get_by_arn).await;
}

#[tokio::test]
async fn aws_get_vector_bucket_by_arn() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket(c, "geta", get_by_arn).await;
}

#[tokio::test]
async fn local_get_vector_bucket_missing_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    not_found(c).await;
}

#[tokio::test]
async fn aws_get_vector_bucket_missing_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    not_found(c).await;
}

// ---------------------------------------------------------------------------
// Shared bodies
// ---------------------------------------------------------------------------

async fn get_by_name(c: Client, name: String) {
    c.create_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("create");
    let resp = c
        .get_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await
        .expect("get by name");
    let bucket = resp.vector_bucket().expect("vectorBucket field present");

    assert_eq!(bucket.vector_bucket_name(), name);
    assert!(bucket.vector_bucket_arn().ends_with(&format!(":bucket/{name}")));
    let _ = bucket.creation_time(); // any sane timestamp; SDK returns &DateTime

    let enc = bucket
        .encryption_configuration()
        .expect("encryptionConfiguration must be present on the response");
    assert_eq!(
        enc.sse_type(),
        &SseType::Aes256,
        "default SSE type must be AES256 per CLAUDE.md C-2b"
    );
}

async fn get_by_arn(c: Client, name: String) {
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

    let resp = c
        .get_vector_bucket()
        .vector_bucket_arn(&arn)
        .send()
        .await
        .expect("get by arn");
    let bucket = resp.vector_bucket().expect("vectorBucket field present");
    assert_eq!(bucket.vector_bucket_name(), name);
    assert_eq!(bucket.vector_bucket_arn(), arn);
}

async fn not_found(c: Client) {
    let missing = unique_bucket_name("missing");
    let err = c
        .get_vector_bucket()
        .vector_bucket_name(&missing)
        .send()
        .await
        .expect_err("GetVectorBucket on missing bucket must error");
    let svc = err.into_service_error();
    assert!(
        svc.is_not_found_exception(),
        "expected NotFoundException, got: {svc:?}"
    );
}
