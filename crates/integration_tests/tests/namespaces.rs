//! Contract tests for s3tables namespace ops (FT-3): CreateNamespace,
//! ListNamespaces, GetNamespace, DeleteNamespace.
//!
//! Same body runs against local marila and real AWS. Wire shape
//! captured in CLAUDE.md C-10.

use aws_sdk_s3tables::Client;
use marila_integration_tests::{
    harness::{MarilaProcess, Target, tables_client, unique_bucket_name},
    require_aws,
};

/// Bucket scope helper — creates a fresh table bucket, runs `body`,
/// always cleans up on exit (panic-safe).
async fn with_table_bucket<F, Fut>(c: Client, prefix: &str, body: F)
where
    F: FnOnce(Client, String) -> Fut,
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

    let outcome = AssertUnwindSafe(body(c.clone(), arn.clone()))
        .catch_unwind()
        .await;

    let _ = c.delete_table_bucket().table_bucket_arn(&arn).send().await;

    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

// ---------------------------------------------------------------------------
// CreateNamespace + ListNamespaces round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_create_namespace_round_trips() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    with_table_bucket(c, "nscreate", create_then_list).await;
}

#[tokio::test]
async fn aws_create_namespace_round_trips() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_table_bucket(c, "nscreate", create_then_list).await;
}

async fn create_then_list(c: Client, arn: String) {
    let ns_name = "marila_ns";
    let resp = c
        .create_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect("CreateNamespace");

    assert_eq!(
        resp.namespace(),
        &[ns_name.to_string()][..],
        "namespace must round-trip as a single-segment list"
    );
    assert_eq!(resp.table_bucket_arn(), arn);

    let list = c
        .list_namespaces()
        .table_bucket_arn(&arn)
        .send()
        .await
        .expect("ListNamespaces");
    let names: Vec<Vec<String>> = list
        .namespaces()
        .iter()
        .map(|n| n.namespace().to_vec())
        .collect();
    assert!(
        names.iter().any(|n| n.as_slice() == [ns_name.to_string()]),
        "namespace must be visible in ListNamespaces, got {names:?}"
    );

    let _ = c
        .delete_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// GetNamespace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_get_namespace_returns_full_shape() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    with_table_bucket(c, "nsget", get_round_trip).await;
}

#[tokio::test]
async fn aws_get_namespace_returns_full_shape() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_table_bucket(c, "nsget", get_round_trip).await;
}

async fn get_round_trip(c: Client, arn: String) {
    let ns_name = "marila_ns";
    c.create_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect("create namespace");

    let got = c
        .get_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect("GetNamespace");
    assert_eq!(got.namespace(), &[ns_name.to_string()][..]);
    let _ = got.owner_account_id();
    let _ = got.table_bucket_id();

    let _ = c
        .delete_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// DeleteNamespace then-gone
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_delete_namespace_then_gone() {
    let _marila = MarilaProcess::start();
    let c = tables_client(Target::Local).await;
    with_table_bucket(c, "nsdel", delete_then_gone).await;
}

#[tokio::test]
async fn aws_delete_namespace_then_gone() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_table_bucket(c, "nsdel", delete_then_gone).await;
}

async fn delete_then_gone(c: Client, arn: String) {
    let ns_name = "marila_ns";
    c.create_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect("create");
    c.delete_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect("delete");

    let err = c
        .get_namespace()
        .table_bucket_arn(&arn)
        .namespace(ns_name)
        .send()
        .await
        .expect_err("Get on deleted namespace must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "deleted namespace should report NotFoundException"
    );
}
