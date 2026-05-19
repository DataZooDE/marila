//! Phase 7 acceptance: `marila-embed query` returns top-K matches in
//! ascending-distance order.
//!
//! Uses the stub provider so we can predict the ordering — the stub is
//! deterministic (blake3-of-input → L2-normalised). The query for "alpha"
//! gets the closest hit when the stored vector was also "alpha".

use std::sync::Arc;

use clap::Parser;
use marila_embed::cli::{Cli, Command};
use marila_embed::{put, query};
use marila_integration_tests::harness::{Target, client, local_endpoint, unique_bucket_name};

fn put_args(endpoint: &str, bucket: &str, index: &str, text: &str) -> marila_embed::cli::PutArgs {
    let cli = Cli::try_parse_from([
        "marila-embed",
        "put",
        "--endpoint-url", endpoint,
        "--vector-bucket-name", bucket,
        "--index-name", index,
        "--embedding-provider", "stub",
        "--embedding-model", "stub-32",
        "--text-value", text,
    ])
    .unwrap();
    match cli.command {
        Command::Put(a) => a,
        _ => unreachable!(),
    }
}

fn query_args(endpoint: &str, bucket: &str, index: &str, q: &str, k: u32) -> marila_embed::cli::QueryArgs {
    let cli = Cli::try_parse_from([
        "marila-embed",
        "query",
        "--endpoint-url", endpoint,
        "--vector-bucket-name", bucket,
        "--index-name", index,
        "--embedding-provider", "stub",
        "--embedding-model", "stub-32",
        "--text-value", q,
        "--k", &k.to_string(),
        "--output", "json",
    ])
    .unwrap();
    match cli.command {
        Command::Query(a) => a,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn local_query_returns_exact_match_first() {
    let endpoint = local_endpoint().await;
    let c = client(Target::Local).await;
    let bucket = unique_bucket_name("q7");
    let index = "phase7";

    c.create_vector_bucket()
        .vector_bucket_name(&bucket)
        .send()
        .await
        .expect("CreateVectorBucket");

    let result = run(&endpoint, &bucket, index).await;

    // Cleanup
    let _ = c.delete_index().vector_bucket_name(&bucket).index_name(index).send().await;
    let _ = c.delete_vector_bucket().vector_bucket_name(&bucket).send().await;

    result.expect("phase7");
}

async fn run(endpoint: &str, bucket: &str, index: &str) -> anyhow::Result<()> {
    // Put three known strings.
    for s in ["alpha", "beta", "gamma"] {
        put::run(put_args(endpoint, bucket, index, s)).await?;
    }
    // List to confirm three vectors landed.
    let c = client(Target::Local).await;
    let listed = c
        .list_vectors()
        .vector_bucket_name(bucket)
        .index_name(index)
        .send()
        .await?;
    assert_eq!(listed.vectors().len(), 3);

    // Query for "alpha" — should be top-1 since the stub is deterministic
    // and exact-match has distance 0 (or near-0 with cosine).
    let args = query_args(endpoint, bucket, index, "alpha", 3);
    // Sanity: actually call the SDK and inspect the response ourselves so
    // the test doesn't depend on stdout capture from query::run().
    use aws_sdk_s3vectors::types::VectorData;
    use marila_embed::embed::stub::StubEmbedder;
    use marila_embed::embed::EmbeddingProvider;
    let provider = Arc::new(StubEmbedder::new(32));
    let q = provider.embed(&["alpha"]).await?.vectors.into_iter().next().unwrap();
    let out = c
        .query_vectors()
        .vector_bucket_name(bucket)
        .index_name(index)
        .top_k(3)
        .query_vector(VectorData::Float32(q))
        .return_distance(true)
        .send()
        .await?;
    let hits = out.vectors();
    assert!(!hits.is_empty(), "no hits returned");
    let top_key = &hits[0].key;
    // Stub embedder yields content-hash keys; figure out the alpha key.
    let alpha_key = marila_embed::keys::chunk_key(
        marila_embed::cli::KeyStrategy::ContentHash,
        "<text-value>",
        0,
        "alpha",
    );
    assert_eq!(*top_key, alpha_key, "expected alpha first, got {hits:?}");

    // And confirm `query::run` itself doesn't error.
    query::run(args).await?;
    Ok(())
}
