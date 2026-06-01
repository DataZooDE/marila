//! Contract tests for s3tables table ops (FT-4..7): CreateTable,
//! ListTables, GetTable, GetTableMetadataLocation, DeleteTable.
//!
//! Same body runs against local marila and real AWS. Wire shape
//! captured in doc/GAP_ANALYSIS.md.

use aws_sdk_s3tables::Client;
use aws_sdk_s3tables::types::{
    IcebergMetadata, IcebergSchema, OpenTableFormat, SchemaField, TableMetadata,
};
use marila_integration_tests::{
    harness::{MarilaProcess, Target, tables_client, unique_bucket_name},
    require_aws, require_lakekeeper_shared_storage,
};

/// Create a bucket + namespace, run `body`, always clean up on exit.
async fn with_bucket_and_namespace<F, Fut>(c: Client, prefix: &str, body: F)
where
    F: FnOnce(Client, String, String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;

    let name = unique_bucket_name(prefix);
    let ns = "marila_ns".to_owned();
    let created = c
        .create_table_bucket()
        .name(&name)
        .send()
        .await
        .expect("create table bucket");
    let arn = created.arn().to_owned();
    c.create_namespace()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .send()
        .await
        .expect("create namespace");

    let outcome = AssertUnwindSafe(body(c.clone(), arn.clone(), ns.clone()))
        .catch_unwind()
        .await;

    let _ = c
        .delete_namespace()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .send()
        .await;
    let _ = c.delete_table_bucket().table_bucket_arn(&arn).send().await;

    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

/// Iceberg schema for a 2-column table. AWS s3tables nests it as
/// `{"iceberg":{"schema":{"fields":[...]}}}` per the wire capture.
fn small_schema_metadata() -> TableMetadata {
    let id_field = SchemaField::builder()
        .name("id")
        .r#type("int")
        .required(true)
        .build()
        .expect("build id field");
    let name_field = SchemaField::builder()
        .name("name")
        .r#type("string")
        .required(false)
        .build()
        .expect("build name field");
    let schema = IcebergSchema::builder()
        .fields(id_field)
        .fields(name_field)
        .build()
        .expect("build schema");
    let iceberg = IcebergMetadata::builder().schema(schema).build();
    TableMetadata::Iceberg(iceberg)
}

// ---------------------------------------------------------------------------
// CreateTable + GetTable round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_create_table_round_trips() {
    let _marila = MarilaProcess::start();
    require_lakekeeper_shared_storage!();
    let c = tables_client(Target::Local).await;
    with_bucket_and_namespace(c, "tcreate", create_then_get).await;
}

#[tokio::test]
async fn aws_create_table_round_trips() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_bucket_and_namespace(c, "tcreate", create_then_get).await;
}

async fn create_then_get(c: Client, arn: String, ns: String) {
    let table_name = "marila_tbl";
    let created = c
        .create_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .format(OpenTableFormat::Iceberg)
        .metadata(small_schema_metadata())
        .send()
        .await
        .expect("CreateTable");

    let table_arn = created.table_arn();
    assert!(
        table_arn.starts_with(&arn),
        "tableARN must be prefixed by the bucket ARN: {table_arn} (bucket {arn})"
    );
    assert!(
        table_arn.contains("/table/"),
        "tableARN must include /table/<uuid>: {table_arn}"
    );
    let _ = created.version_token();

    let got = c
        .get_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .send()
        .await
        .expect("GetTable");
    assert_eq!(got.name(), table_name);
    assert_eq!(got.namespace(), &[ns.clone()][..]);
    assert!(
        got.metadata_location().is_some_and(|s| !s.is_empty()),
        "GetTable must surface a non-empty metadataLocation"
    );
    assert!(
        !got.warehouse_location().is_empty(),
        "GetTable must surface a non-empty warehouseLocation"
    );
    let _ = got.created_at();

    let _ = c
        .delete_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// ListTables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_list_tables_shows_created_table() {
    let _marila = MarilaProcess::start();
    require_lakekeeper_shared_storage!();
    let c = tables_client(Target::Local).await;
    with_bucket_and_namespace(c, "tlist", create_then_list).await;
}

#[tokio::test]
async fn aws_list_tables_shows_created_table() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_bucket_and_namespace(c, "tlist", create_then_list).await;
}

async fn create_then_list(c: Client, arn: String, ns: String) {
    let table_name = "marila_tbl";
    c.create_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .format(OpenTableFormat::Iceberg)
        .metadata(small_schema_metadata())
        .send()
        .await
        .expect("CreateTable for list test");

    let list = c
        .list_tables()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .send()
        .await
        .expect("ListTables");
    let names: Vec<&str> = list.tables().iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&table_name),
        "ListTables must include our table, got {names:?}"
    );

    let _ = c
        .delete_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// DeleteTable then-gone
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_delete_table_then_get_is_not_found() {
    let _marila = MarilaProcess::start();
    require_lakekeeper_shared_storage!();
    let c = tables_client(Target::Local).await;
    with_bucket_and_namespace(c, "tdel", delete_then_gone).await;
}

#[tokio::test]
async fn aws_delete_table_then_get_is_not_found() {
    require_aws!();
    let c = tables_client(Target::Aws).await;
    with_bucket_and_namespace(c, "tdel", delete_then_gone).await;
}

async fn delete_then_gone(c: Client, arn: String, ns: String) {
    let table_name = "marila_tbl";
    c.create_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .format(OpenTableFormat::Iceberg)
        .metadata(small_schema_metadata())
        .send()
        .await
        .expect("create to delete");
    c.delete_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .send()
        .await
        .expect("DeleteTable");
    let err = c
        .get_table()
        .table_bucket_arn(&arn)
        .namespace(&ns)
        .name(table_name)
        .send()
        .await
        .expect_err("Get on deleted table must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "deleted table should report NotFoundException"
    );
}
