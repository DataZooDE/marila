//! Contract tests for `QueryVectors`.
//!
//! Wire shape captured in CLAUDE.md C-2f. Tests cover:
//!  - unfiltered topK (anchor is nearest)
//!  - Mongo-filtered query (only matching-metadata vectors returned)
//!  - distanceMetric echo on the response
//!  - missing index → NotFoundException (index body text)
//!  - dimension mismatch → ValidationException

use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{
    DataType, DistanceMetric, PutInputVector, QueryOutputVector, VectorData,
};
use aws_smithy_types::Document;
use marila_integration_tests::{
    harness::{BucketCtx, MarilaProcess, Target, client, with_bucket_and_indexes},
    require_aws,
};

const DIM: usize = 4;

async fn provision(c: &Client, ctx: &BucketCtx, index: &str) {
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

fn put(key: &str, data: [f32; DIM], meta: Option<serde_json::Value>) -> PutInputVector {
    let mut b = PutInputVector::builder()
        .key(key)
        .data(VectorData::Float32(data.to_vec()));
    if let Some(m) = meta {
        b = b.metadata(json_to_document(m));
    }
    b.build().expect("build PutInputVector")
}

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

fn keys_of(vs: &[QueryOutputVector]) -> Vec<&str> {
    vs.iter().map(|v| v.key()).collect()
}

// ---------------------------------------------------------------------------
// Unfiltered topK
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_query_vectors_unfiltered_returns_anchor_first() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "qvtop", unfiltered_top_k).await;
}

#[tokio::test]
async fn aws_query_vectors_unfiltered_returns_anchor_first() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "qvtop", unfiltered_top_k).await;
}

async fn unfiltered_top_k(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision(&c, &ctx, &index).await;

    c.put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(put("anchor", [1.0, 0.0, 0.0, 0.0], None))
        .vectors(put("near", [0.9, 0.1, 0.0, 0.0], None))
        .vectors(put("far", [0.0, 0.0, 0.0, 1.0], None))
        .send()
        .await
        .expect("seed");

    let resp = c
        .query_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .top_k(2)
        .query_vector(VectorData::Float32(vec![1.0, 0.0, 0.0, 0.0]))
        .return_distance(true)
        .send()
        .await
        .expect("QueryVectors");

    // distanceMetric echo (CLAUDE.md C-2f).
    assert_eq!(
        resp.distance_metric(),
        Some(&DistanceMetric::Cosine),
        "response must echo the index's distance metric"
    );

    let vs = resp.vectors();
    assert_eq!(vs.len(), 2, "topK=2 must return at most 2");
    assert_eq!(vs[0].key(), "anchor", "nearest hit must be anchor");
    // anchor==query → cosine distance ~0; "near" has small non-zero distance.
    let d0 = vs[0].distance().expect("returnDistance=true must populate");
    let d1 = vs[1].distance().expect("returnDistance=true must populate");
    assert!(d0 < 0.001, "anchor distance ~0, got {d0}");
    assert!(d1 > d0, "second hit must be farther, got {d0} then {d1}");
}

// ---------------------------------------------------------------------------
// Filtered query
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_query_vectors_metadata_filter_excludes_non_matching() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "qvfilt", metadata_filter).await;
}

#[tokio::test]
async fn aws_query_vectors_metadata_filter_excludes_non_matching() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "qvfilt", metadata_filter).await;
}

async fn metadata_filter(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision(&c, &ctx, &index).await;

    c.put_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .vectors(put(
            "anchor",
            [1.0, 0.0, 0.0, 0.0],
            Some(serde_json::json!({"label":"a","tier":1})),
        ))
        .vectors(put(
            "near",
            [0.9, 0.1, 0.0, 0.0],
            Some(serde_json::json!({"label":"a","tier":2})),
        ))
        .vectors(put(
            "far",
            [0.0, 0.0, 0.0, 1.0],
            Some(serde_json::json!({"label":"b","tier":1})),
        ))
        .send()
        .await
        .expect("seed");

    let resp = c
        .query_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .top_k(5)
        .query_vector(VectorData::Float32(vec![1.0, 0.0, 0.0, 0.0]))
        .filter(json_to_document(serde_json::json!({"label":"a"})))
        .return_metadata(true)
        .send()
        .await
        .expect("filtered QueryVectors");

    let keys = keys_of(resp.vectors());
    assert!(
        keys.contains(&"anchor"),
        "anchor (label=a) must be in result"
    );
    assert!(keys.contains(&"near"), "near (label=a) must be in result");
    assert!(
        !keys.contains(&"far"),
        "far (label=b) must be excluded by filter"
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_query_vectors_missing_index_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "qvmiss", missing_index).await;
}

#[tokio::test]
async fn aws_query_vectors_missing_index_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "qvmiss", missing_index).await;
}

async fn missing_index(c: Client, ctx: BucketCtx) {
    let err = c
        .query_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name("ghost-index")
        .top_k(1)
        .query_vector(VectorData::Float32(vec![1.0, 0.0, 0.0, 0.0]))
        .send()
        .await
        .expect_err("QueryVectors against missing index must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException for missing index"
    );
}

#[tokio::test]
async fn local_query_vectors_dim_mismatch_is_validation() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "qvdim", dim_mismatch).await;
}

#[tokio::test]
async fn aws_query_vectors_dim_mismatch_is_validation() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "qvdim", dim_mismatch).await;
}

async fn dim_mismatch(c: Client, ctx: BucketCtx) {
    let index = "myx".to_owned();
    provision(&c, &ctx, &index).await;

    let err = c
        .query_vectors()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .top_k(1)
        .query_vector(VectorData::Float32(vec![1.0, 0.0])) // wrong dim
        .send()
        .await
        .expect_err("QueryVectors with wrong dim must error");
    assert!(
        err.into_service_error().is_validation_exception(),
        "expected ValidationException for dim mismatch"
    );
}
