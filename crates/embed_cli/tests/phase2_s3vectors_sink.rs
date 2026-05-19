//! Phase 2 acceptance: the s3vectors-backed sink writes against running
//! marila and auto-creates the index on first use. ListVectors confirms
//! the key landed and the standard `S3VECTORS-EMBED-*` metadata round-trips.
//!
//! Reuses the integration-test harness so we don't double-spawn marila
//! when both `marila-embed` and `marila-integration-tests` test suites
//! run in the same `cargo test --workspace` invocation.

use clap::Parser;
use marila_embed::cli::{Cli, Command};
use marila_embed::put;
use marila_integration_tests::harness::{MarilaProcess, Target, client, unique_bucket_name};

fn put_args(bucket: &str, index: &str, text: &str) -> marila_embed::cli::PutArgs {
    let cli = Cli::try_parse_from([
        "marila-embed",
        "put",
        "--endpoint-url", "http://localhost:8080",
        "--vector-bucket-name", bucket,
        "--index-name", index,
        "--embedding-provider", "stub",
        "--embedding-model", "stub-32",
        "--text-value", text,
    ])
    .expect("parse cli");
    match cli.command {
        Command::Put(a) => a,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn local_put_text_value_creates_index_and_lands_key() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;

    let bucket = unique_bucket_name("embedcli");
    let index = "phase2";

    c.create_vector_bucket()
        .vector_bucket_name(&bucket)
        .send()
        .await
        .expect("CreateVectorBucket");

    let outcome = put_then_cleanup(bucket.clone(), index.into(), c.clone()).await;
    outcome.expect("phase2 e2e");
}

async fn put_then_cleanup(
    bucket: String,
    index: String,
    c: aws_sdk_s3vectors::Client,
) -> anyhow::Result<()> {
    let result = run_phase2(bucket.clone(), index.clone()).await;

    // Cleanup: delete index (if present) then bucket. Tolerate errors so
    // an assertion failure isn't masked by cleanup noise.
    let _ = c
        .delete_index()
        .vector_bucket_name(&bucket)
        .index_name(&index)
        .send()
        .await;
    let _ = c
        .delete_vector_bucket()
        .vector_bucket_name(&bucket)
        .send()
        .await;

    result
}

async fn run_phase2(bucket: String, index: String) -> anyhow::Result<()> {
    let args = put_args(&bucket, &index, "phase2-e2e content");

    // Hit the real flow via `put::run`, which will build the s3vectors
    // sink and CreateIndex on first put.
    put::run(args).await?;

    // Confirm via the SDK: index exists with the right dim, list shows
    // exactly one vector, get returns the right metadata + data.
    let c = marila_integration_tests::harness::client(Target::Local).await;
    let idx = c
        .get_index()
        .vector_bucket_name(&bucket)
        .index_name(&index)
        .send()
        .await?;
    let i = idx.index.expect("GetIndex.index");
    assert_eq!(i.dimension(), 32);
    use aws_sdk_s3vectors::types::{DataType, DistanceMetric};
    assert_eq!(i.data_type(), &DataType::Float32);
    assert_eq!(i.distance_metric(), &DistanceMetric::Cosine);

    let listed = c
        .list_vectors()
        .vector_bucket_name(&bucket)
        .index_name(&index)
        .send()
        .await?;
    let keys: Vec<_> = listed.vectors().iter().map(|v| v.key().to_owned()).collect();
    assert_eq!(keys.len(), 1, "expected exactly one vector, got {keys:?}");

    let got = c
        .get_vectors()
        .vector_bucket_name(&bucket)
        .index_name(&index)
        .keys(&keys[0])
        .return_data(true)
        .return_metadata(true)
        .send()
        .await?;
    let v = &got.vectors()[0];
    assert_eq!(v.key(), &keys[0]);
    // Metadata round-trips
    let m = v.metadata().expect("metadata");
    let json = smithy_doc_to_json(m);
    let obj = json.as_object().expect("metadata is object");
    assert_eq!(
        obj.get("S3VECTORS-EMBED-SRC-CONTENT").and_then(|v| v.as_str()),
        Some("phase2-e2e content")
    );
    assert_eq!(
        obj.get("S3VECTORS-EMBED-CHUNK-IDX").and_then(|v| v.as_u64()),
        Some(0)
    );
    Ok(())
}

fn smithy_doc_to_json(d: &aws_smithy_types::Document) -> serde_json::Value {
    use aws_smithy_types::{Document, Number};
    use serde_json::Value;
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::Number((*u).into()),
            Number::NegInt(i) => Value::Number((*i).into()),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(arr) => Value::Array(arr.iter().map(smithy_doc_to_json).collect()),
        Document::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), smithy_doc_to_json(v)))
                .collect(),
        ),
    }
}
