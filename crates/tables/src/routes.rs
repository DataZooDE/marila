//! axum router for the s3tables façade.
//!
//! S3 Tables uses REST verbs + path-based routing — distinct from
//! s3vectors' all-POST `/<OperationName>` shape (CLAUDE.md C-9).
//!
//! The router stitches together:
//! - bucket-level control plane    (`/buckets[/{arn}]`)
//! - namespace control plane       (`/namespaces/{arn}[/{ns}]`)        — FT-3
//! - table control plane           (`/tables/{arn}/{ns}[/{name}]`)     — FT-4..7
//! - Iceberg REST pass-through     (`/iceberg/*`)                      — FT-8
//! - 501 fallback for the rest    (encryption / metrics / replication / policy ops) — FT-9

use axum::{
    Json, Router,
    routing::{delete, get, post, put},
};
use marila_aws_compat::AwsError;

use crate::{
    control_plane::{
        AppState, create_table_bucket, delete_table_bucket, get_table_bucket, list_table_buckets,
    },
    iceberg_proxy::{IcebergProxyState, iceberg_proxy},
    namespace_handlers::{create_namespace, delete_namespace, get_namespace, list_namespaces},
    table_handlers::{
        create_table, delete_table, get_table, get_table_metadata_location, list_tables,
    },
};

pub fn router(state: AppState) -> Router {
    let iceberg = IcebergProxyState::new(
        std::env::var("MARILA_LAKEKEEPER_URL")
            .unwrap_or_else(|_| "http://localhost:8181".to_owned()),
    );

    Router::new()
        // ---- buckets ----
        .route("/buckets", put(create_table_bucket).get(list_table_buckets))
        .route(
            "/buckets/{arn}",
            get(get_table_bucket).delete(delete_table_bucket),
        )
        // ---- namespaces ----
        .route(
            "/namespaces/{arn}",
            put(create_namespace).get(list_namespaces),
        )
        .route(
            "/namespaces/{arn}/{namespace}",
            get(get_namespace).delete(delete_namespace),
        )
        // ---- tables ----
        // CreateTable: PUT /tables/{arn}/{namespace}
        .route("/tables/{arn}/{namespace}", put(create_table))
        // ListTables: GET /tables/{arn}?namespace=... — distinct shape
        // from /tables/{arn}/{namespace} (which is CreateTable).
        .route("/tables/{arn}", get(list_tables))
        // GetTable uses an **RPC-style** URL: /get-table?tableBucketARN=&namespace=&name=
        .route("/get-table", get(get_table))
        // DeleteTable: DELETE /tables/{arn}/{namespace}/{name}
        .route(
            "/tables/{arn}/{namespace}/{name}",
            delete(delete_table),
        )
        // GetTableMetadataLocation: GET /tables/{arn}/{namespace}/{name}/metadata-location
        .route(
            "/tables/{arn}/{namespace}/{name}/metadata-location",
            get(get_table_metadata_location),
        )
        // ---- FT-9: deliberately-not-implemented surfaces return 501 ----
        .route(
            "/buckets/{arn}/encryption",
            get(unimplemented_get_table_bucket_encryption)
                .put(unimplemented_put_table_bucket_encryption)
                .delete(unimplemented_delete_table_bucket_encryption),
        )
        .route(
            "/buckets/{arn}/maintenance/{type}",
            get(unimplemented_get_table_bucket_maintenance_configuration)
                .put(unimplemented_put_table_bucket_maintenance_configuration),
        )
        .route(
            "/buckets/{arn}/metricsConfiguration",
            get(unimplemented_get_table_bucket_metrics_configuration)
                .put(unimplemented_put_table_bucket_metrics_configuration)
                .delete(unimplemented_delete_table_bucket_metrics_configuration),
        )
        .route(
            "/buckets/{arn}/policy",
            get(unimplemented_get_table_bucket_policy)
                .put(unimplemented_put_table_bucket_policy)
                .delete(unimplemented_delete_table_bucket_policy),
        )
        .route(
            "/buckets/{arn}/replication",
            get(unimplemented_get_table_bucket_replication)
                .put(unimplemented_put_table_bucket_replication)
                .delete(unimplemented_delete_table_bucket_replication),
        )
        .route(
            "/tables/{arn}/{namespace}/{name}/policy",
            get(unimplemented_get_table_policy)
                .put(unimplemented_put_table_policy)
                .delete(unimplemented_delete_table_policy),
        )
        .route(
            "/tables/{arn}/{namespace}/{name}/replication",
            get(unimplemented_get_table_replication)
                .put(unimplemented_put_table_replication)
                .delete(unimplemented_delete_table_replication),
        )
        .route(
            "/tables/{arn}/{namespace}/{name}/rename",
            post(unimplemented_rename_table),
        )
        .with_state(state)
        // ---- Iceberg pass-through (separate state) ----
        .merge(iceberg_router(iceberg))
}

fn iceberg_router(state: IcebergProxyState) -> Router {
    Router::new()
        .route("/iceberg/{*tail}", any_proxy())
        .with_state(state)
}

fn any_proxy() -> axum::routing::MethodRouter<IcebergProxyState> {
    use axum::routing::get;
    get(iceberg_proxy)
        .post(iceberg_proxy)
        .put(iceberg_proxy)
        .delete(iceberg_proxy)
        .head(iceberg_proxy)
}

// ---------------------------------------------------------------------------
// FT-9 — small generated handlers for the deliberately-not-implemented ops.
// ---------------------------------------------------------------------------

macro_rules! unimplemented_table_handler {
    ($fn_name:ident, $op_name:expr) => {
        async fn $fn_name() -> Result<Json<serde_json::Value>, AwsError> {
            Err(AwsError::NotImplemented(format!(
                "s3tables:{} is not implemented by marila (REQUIREMENTS.md FT-9)",
                $op_name
            )))
        }
    };
}

unimplemented_table_handler!(unimplemented_get_table_bucket_encryption, "GetTableBucketEncryption");
unimplemented_table_handler!(unimplemented_put_table_bucket_encryption, "PutTableBucketEncryption");
unimplemented_table_handler!(unimplemented_delete_table_bucket_encryption, "DeleteTableBucketEncryption");
unimplemented_table_handler!(unimplemented_get_table_bucket_maintenance_configuration, "GetTableBucketMaintenanceConfiguration");
unimplemented_table_handler!(unimplemented_put_table_bucket_maintenance_configuration, "PutTableBucketMaintenanceConfiguration");
unimplemented_table_handler!(unimplemented_get_table_bucket_metrics_configuration, "GetTableBucketMetricsConfiguration");
unimplemented_table_handler!(unimplemented_put_table_bucket_metrics_configuration, "PutTableBucketMetricsConfiguration");
unimplemented_table_handler!(unimplemented_delete_table_bucket_metrics_configuration, "DeleteTableBucketMetricsConfiguration");
unimplemented_table_handler!(unimplemented_get_table_bucket_policy, "GetTableBucketPolicy");
unimplemented_table_handler!(unimplemented_put_table_bucket_policy, "PutTableBucketPolicy");
unimplemented_table_handler!(unimplemented_delete_table_bucket_policy, "DeleteTableBucketPolicy");
unimplemented_table_handler!(unimplemented_get_table_bucket_replication, "GetTableBucketReplication");
unimplemented_table_handler!(unimplemented_put_table_bucket_replication, "PutTableBucketReplication");
unimplemented_table_handler!(unimplemented_delete_table_bucket_replication, "DeleteTableBucketReplication");
unimplemented_table_handler!(unimplemented_get_table_policy, "GetTablePolicy");
unimplemented_table_handler!(unimplemented_put_table_policy, "PutTablePolicy");
unimplemented_table_handler!(unimplemented_delete_table_policy, "DeleteTablePolicy");
unimplemented_table_handler!(unimplemented_get_table_replication, "GetTableReplication");
unimplemented_table_handler!(unimplemented_put_table_replication, "PutTableReplication");
unimplemented_table_handler!(unimplemented_delete_table_replication, "DeleteTableReplication");
unimplemented_table_handler!(unimplemented_rename_table, "RenameTable");
