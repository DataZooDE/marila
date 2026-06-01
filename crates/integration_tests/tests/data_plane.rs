//! Contract tests for the s3vectors data plane:
//! PutVectors / GetVectors / ListVectors / DeleteVectors.
//!
//! Same body runs against both local marila and real AWS. Wire shapes
//! captured in doc/GAP_ANALYSIS.md.

use std::collections::HashMap;

use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{DataType, DistanceMetric, PutInputVector, VectorData};
use aws_smithy_types::Document;
use marila_integration_tests::{
    harness::{BucketCtx, MarilaProcess, Target, client, with_bucket_and_indexes},
    require_aws,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DIM: usize = 4;

async fn provision_index(c: &Client, ctx: &BucketCtx, index: &str) {
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(index)
        .data_type(DataType::Float32)
        .dimension(DIM as i32)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("create test index");
    ctx.add_index(index);
}

fn vec_with(key: &str, data: [f32; DIM]) -> PutInputVector {
    PutInputVector::builder()
        .key(key)
        .data(VectorData::Float32(data.to_vec()))
        .build()
        .expect("build PutInputVector")
}

fn vec_with_meta(key: &str, data: [f32; DIM], meta: serde_json::Value) -> PutInputVector {
    PutInputVector::builder()
        .key(key)
        .data(VectorData::Float32(data.to_vec()))
        .metadata(json_to_document(meta))
        .build()
        .expect("build PutInputVector")
}

/// AWS's S3 Vectors API requires `metadata` to be a Document::Object
/// (Smithy document type), not a JSON-encoded string. Validation error
/// otherwise: "Metadata must be an object".
fn json_to_document(v: serde_json::Value) -> Document {
    use serde_json::Value;
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else {
                Document::Number(aws_smithy_types::Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s),
        Value::Array(arr) => Document::Array(arr.into_iter().map(json_to_document).collect()),
        Value::Object(obj) => Document::Object(
            obj.into_iter()
                .map(|(k, v)| (k, json_to_document(v)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------------
// PutVectors / GetVectors round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_put_then_get_round_trips() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dpput", put_then_get).await;
}

#[tokio::test]
async fn aws_put_then_get_round_trips() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dpput", put_then_get).await;
}

async fn put_then_get(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision_index(&c, &ctx, &index).await;

    c.put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(vec_with("a", [1.0, 0.0, 0.0, 0.0]))
        .vectors(vec_with("b", [0.0, 1.0, 0.0, 0.0]))
        .send()
        .await
        .expect("PutVectors");

    let got = c
        .get_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .keys("a")
        .keys("b")
        .keys("c-missing")
        .return_data(true)
        .send()
        .await
        .expect("GetVectors");

    let by_key: HashMap<&str, _> = got.vectors().iter().map(|v| (v.key(), v)).collect();
    assert!(by_key.contains_key("a"));
    assert!(by_key.contains_key("b"));
    assert!(
        !by_key.contains_key("c-missing"),
        "missing keys must be silently omitted (doc/GAP_ANALYSIS.md)"
    );

    // Vector dims round-trip with float32 precision.
    let a_data = by_key["a"].data().expect("data on GetVectors");
    match a_data {
        VectorData::Float32(v) => {
            assert_eq!(v.len(), DIM);
            // 1.0 is exactly representable in float32 — strict equality OK.
            assert_eq!(v[0], 1.0);
        }
        _ => panic!("expected float32 variant"),
    }
}

// ---------------------------------------------------------------------------
// DeleteVectors is silently idempotent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_delete_vectors_is_silently_idempotent() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dpdel", delete_idempotent).await;
}

#[tokio::test]
async fn aws_delete_vectors_is_silently_idempotent() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dpdel", delete_idempotent).await;
}

async fn delete_idempotent(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision_index(&c, &ctx, &index).await;

    c.put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(vec_with("a", [1.0, 0.0, 0.0, 0.0]))
        .send()
        .await
        .expect("seed");

    // First delete removes a; second delete is a no-op.
    c.delete_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .keys("a")
        .keys("never-existed")
        .send()
        .await
        .expect("delete with mixed keys");
    c.delete_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .keys("a")
        .send()
        .await
        .expect("second delete is silent");

    // Verify list is empty.
    let listed = c
        .list_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .send()
        .await
        .expect("ListVectors");
    assert!(listed.vectors().is_empty());
}

// ---------------------------------------------------------------------------
// ListVectors pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_list_vectors_paginates() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dplist", list_paginates).await;
}

#[tokio::test]
async fn aws_list_vectors_paginates() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dplist", list_paginates).await;
}

async fn list_paginates(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision_index(&c, &ctx, &index).await;

    for k in ["k1", "k2", "k3", "k4", "k5"] {
        c.put_vectors()
            .vector_bucket_name(ctx.bucket())
            .index_name(&index)
            .vectors(vec_with(k, [1.0, 0.0, 0.0, 0.0]))
            .send()
            .await
            .expect("seed");
    }

    // Loop until nextToken absent — AWS may emit empty pages with a
    // cursor (doc/GAP_ANALYSIS.md), so the loop is the contract.
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut token: Option<String> = None;
    for _ in 0..20 {
        let mut req = c
            .list_vectors()
            .vector_bucket_name(ctx.bucket())
            .index_name(&index)
            .max_results(2);
        if let Some(t) = token.as_ref() {
            req = req.next_token(t);
        }
        let page = req.send().await.expect("ListVectors page");
        for v in page.vectors() {
            seen.insert(v.key().to_owned());
        }
        match page.next_token() {
            Some(t) if !t.is_empty() => token = Some(t.to_owned()),
            _ => break,
        }
    }
    assert_eq!(
        seen,
        ["k1", "k2", "k3", "k4", "k5"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
}

// ---------------------------------------------------------------------------
// Error paths — missing index, dim mismatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_put_vectors_on_missing_index_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dpidx", put_missing_index).await;
}

#[tokio::test]
async fn aws_put_vectors_on_missing_index_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dpidx", put_missing_index).await;
}

async fn put_missing_index(c: Client, ctx: BucketCtx) {
    let err = c
        .put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name("ghost-index")
        .vectors(vec_with("a", [1.0, 0.0, 0.0, 0.0]))
        .send()
        .await
        .expect_err("PutVectors on missing index must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException with the index-not-found body (doc/GAP_ANALYSIS.md)"
    );
}

#[tokio::test]
async fn local_put_vectors_dim_mismatch_is_validation() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dpdim", put_dim_mismatch).await;
}

#[tokio::test]
async fn aws_put_vectors_dim_mismatch_is_validation() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dpdim", put_dim_mismatch).await;
}

async fn put_dim_mismatch(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision_index(&c, &ctx, &index).await;

    let err = c
        .put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(
            PutInputVector::builder()
                .key("bad")
                .data(VectorData::Float32(vec![1.0, 2.0])) // wrong dim
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect_err("PutVectors with wrong dim must error");
    let svc = err.into_service_error();
    assert!(
        svc.is_validation_exception(),
        "expected ValidationException for dim mismatch, got: {svc:?}"
    );
}

// ---------------------------------------------------------------------------
// Metadata round-trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_get_vectors_returns_metadata_when_requested() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dpmeta", meta_round_trip).await;
}

#[tokio::test]
async fn aws_get_vectors_returns_metadata_when_requested() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dpmeta", meta_round_trip).await;
}

async fn meta_round_trip(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision_index(&c, &ctx, &index).await;

    c.put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(vec_with_meta(
            "a",
            [1.0, 0.0, 0.0, 0.0],
            serde_json::json!({"label": "alpha"}),
        ))
        .send()
        .await
        .expect("seed with meta");

    let got = c
        .get_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .keys("a")
        .return_metadata(true)
        .send()
        .await
        .expect("GetVectors with metadata");
    let v = &got.vectors()[0];
    assert!(
        v.metadata().is_some(),
        "metadata must be present when requested"
    );

    // Asserting deep equality of Document shapes is brittle across
    // string-vs-object representations; we only check presence here.
    // The metadata is fully exercised by QueryVectors' filter tests
    // when those land.
}

// ---------------------------------------------------------------------------
// FV-4: PutVectors writes a JSON snapshot to RustFS before the DuckDB
// insert. This is marila-internal behaviour (no AWS analogue) so it
// only has a `local_*` variant.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_put_vectors_writes_snapshot_to_rustfs() {
    use marila_integration_tests::harness::embedded;
    use marila_storage::{BucketStore, S3BucketStore, S3Config};

    let c = client(Target::Local).await;
    let rustfs_endpoint = embedded().rustfs_url.clone();
    with_bucket_and_indexes(c, "dpsnap", |c, ctx| async move {
        let index = "myx".to_owned();
        provision_index(&c, &ctx, &index).await;

        c.put_vectors()
            .vector_bucket_name(ctx.bucket())
            .index_name(&index)
            .vectors(vec_with("snap", [0.25, 0.5, 0.75, 1.0]))
            .send()
            .await
            .expect("PutVectors");

        // Read the snapshot directly from RustFS via the storage adapter
        // — the path is `<bucket>/<index>/<key>.json`. We use the
        // *embedded* RustFS's ephemeral URL, not the docker default,
        // so the test runs without docker.
        let storage = S3BucketStore::connect(S3Config {
            endpoint: rustfs_endpoint,
            access_key_id: "marila".into(),
            secret_access_key: "marilasecret".into(),
            region: "eu-west-1".into(),
        })
        .await
        .expect("storage connect");
        let object_key = format!("{index}/snap.json");
        let body = storage
            .get_object(ctx.bucket(), &object_key)
            .await
            .expect("get_object")
            .expect("snapshot must exist on RustFS after PutVectors");
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("snapshot is valid JSON");
        assert_eq!(parsed["key"], serde_json::json!("snap"));
        assert_eq!(parsed["data"], serde_json::json!([0.25, 0.5, 0.75, 1.0]));

        // DeleteVectors must remove the snapshot too (best-effort but
        // observable here).
        c.delete_vectors()
            .vector_bucket_name(ctx.bucket())
            .index_name(&index)
            .keys("snap")
            .send()
            .await
            .expect("DeleteVectors");
        let after = storage
            .get_object(ctx.bucket(), &object_key)
            .await
            .expect("get_object after delete");
        assert!(
            after.is_none(),
            "DeleteVectors must remove the RustFS snapshot"
        );
    })
    .await;
}
