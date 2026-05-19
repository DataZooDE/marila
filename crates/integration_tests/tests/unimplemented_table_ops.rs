//! FT-9: 501 NotImplementedException fallback for tables policy /
//! encryption / metrics / replication / rename ops. Mirrors the
//! s3vectors equivalent (`tests/unimplemented_ops.rs`).

use marila_integration_tests::harness::local_endpoint;

/// (method, path, op-name) tuples. Path uses a synthetic ARN — we only
/// care about the envelope shape, not the bucket actually existing.
const TUPLES: &[(&str, &str, &str)] = &[
    ("GET", "/buckets/dummy-arn/encryption", "GetTableBucketEncryption"),
    ("PUT", "/buckets/dummy-arn/encryption", "PutTableBucketEncryption"),
    ("DELETE", "/buckets/dummy-arn/encryption", "DeleteTableBucketEncryption"),
    ("GET", "/buckets/dummy-arn/maintenance/profile", "GetTableBucketMaintenanceConfiguration"),
    ("PUT", "/buckets/dummy-arn/maintenance/profile", "PutTableBucketMaintenanceConfiguration"),
    ("GET", "/buckets/dummy-arn/metricsConfiguration", "GetTableBucketMetricsConfiguration"),
    ("PUT", "/buckets/dummy-arn/metricsConfiguration", "PutTableBucketMetricsConfiguration"),
    ("DELETE", "/buckets/dummy-arn/metricsConfiguration", "DeleteTableBucketMetricsConfiguration"),
    ("GET", "/buckets/dummy-arn/policy", "GetTableBucketPolicy"),
    ("PUT", "/buckets/dummy-arn/policy", "PutTableBucketPolicy"),
    ("DELETE", "/buckets/dummy-arn/policy", "DeleteTableBucketPolicy"),
    ("GET", "/buckets/dummy-arn/replication", "GetTableBucketReplication"),
    ("PUT", "/buckets/dummy-arn/replication", "PutTableBucketReplication"),
    ("DELETE", "/buckets/dummy-arn/replication", "DeleteTableBucketReplication"),
    ("GET", "/tables/dummy-arn/ns/name/policy", "GetTablePolicy"),
    ("PUT", "/tables/dummy-arn/ns/name/policy", "PutTablePolicy"),
    ("DELETE", "/tables/dummy-arn/ns/name/policy", "DeleteTablePolicy"),
    ("GET", "/tables/dummy-arn/ns/name/replication", "GetTableReplication"),
    ("PUT", "/tables/dummy-arn/ns/name/replication", "PutTableReplication"),
    ("DELETE", "/tables/dummy-arn/ns/name/replication", "DeleteTableReplication"),
    ("POST", "/tables/dummy-arn/ns/name/rename", "RenameTable"),
];

#[tokio::test]
async fn local_unimplemented_table_ops_return_501_with_envelope() {
    let endpoint = local_endpoint().await;
    let client = reqwest::Client::new();

    for (method, path, op_name) in TUPLES {
        let url = format!("{endpoint}{path}");
        let req = match *method {
            "GET" => client.get(&url),
            "PUT" => client.put(&url).body("{}"),
            "DELETE" => client.delete(&url),
            "POST" => client.post(&url).body("{}"),
            other => panic!("unknown method {other}"),
        };
        let resp = req
            .header("Content-Type", "application/json")
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method} {path} transport error: {e}"));

        assert_eq!(
            resp.status().as_u16(),
            501,
            "{method} {path} must return 501, got {}",
            resp.status()
        );
        assert_eq!(
            resp.headers()
                .get("x-amzn-errortype")
                .map(|v| v.to_str().unwrap_or_default()),
            Some("NotImplementedException"),
            "{method} {path} missing/wrong x-amzn-errortype"
        );

        let body: serde_json::Value = resp.json().await.unwrap();
        let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            message.contains(op_name),
            "{method} {path} message must mention `{op_name}`, got {message:?}"
        );
    }
}
