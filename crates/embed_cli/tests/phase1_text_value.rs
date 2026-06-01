//! Phase 1 acceptance: `marila-embed put --text-value "hello"` with the
//! stub provider lands exactly one EmbeddedChunk in an in-memory sink,
//! complete with deterministic key and `S3VECTORS-EMBED-*` metadata.

use std::sync::Arc;

use clap::Parser;
use marila_embed::cli::{Cli, Command};
use marila_embed::embed::EmbeddingProvider;
use marila_embed::embed::stub::StubEmbedder;
use marila_embed::put;
use marila_embed::sink::in_memory::InMemorySink;

fn put_args(text: &str) -> marila_embed::cli::PutArgs {
    let cli = Cli::try_parse_from([
        "marila-embed",
        "put",
        "--vector-bucket-name",
        "doesnt-matter-for-in-memory",
        "--index-name",
        "doesnt-matter-either",
        "--embedding-provider",
        "stub",
        "--text-value",
        text,
    ])
    .expect("parse cli");
    match cli.command {
        Command::Put(a) => a,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn put_text_value_lands_one_vector_with_standard_metadata() {
    let provider = Arc::new(StubEmbedder::default());
    let sink = InMemorySink::new();
    let args = put_args("hello marila");

    put::run_with(args, provider.clone(), Arc::new(sink.clone()))
        .await
        .expect("put");

    let chunks = sink.chunks();
    assert_eq!(
        chunks.len(),
        1,
        "expected exactly one vector, got {chunks:?}"
    );
    let c = &chunks[0];
    assert_eq!(c.vector.len(), provider.dimension() as usize);
    assert_eq!(c.key.len(), 32, "content-hash key should be 32 hex chars");

    use marila_embed::put::*;
    let meta = &c.metadata;
    assert_eq!(
        meta.get(META_SRC_LOCATION).unwrap(),
        &serde_json::json!("<text-value>")
    );
    assert_eq!(
        meta.get(META_SRC_CONTENT).unwrap(),
        &serde_json::json!("hello marila")
    );
    assert_eq!(meta.get(META_CHUNK_IDX).unwrap(), &serde_json::json!(0));
    assert!(meta.contains_key(META_CONTENT_HASH));
}

#[tokio::test]
async fn put_no_input_errors() {
    // Build PutArgs without --text-value or --text. Parser-level required
    // checks don't catch this — the put handler does.
    let cli = Cli::try_parse_from([
        "marila-embed",
        "put",
        "--vector-bucket-name",
        "b",
        "--index-name",
        "i",
        "--embedding-provider",
        "stub",
    ])
    .expect("parse");
    let Command::Put(args) = cli.command else {
        unreachable!()
    };
    let provider = Arc::new(StubEmbedder::default());
    let sink = Arc::new(InMemorySink::new());
    let err = put::run_with(args, provider, sink).await.unwrap_err();
    assert!(
        err.to_string().contains("no input"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn put_text_value_deterministic_key_across_runs() {
    let provider = Arc::new(StubEmbedder::default());
    let sink_a = InMemorySink::new();
    let sink_b = InMemorySink::new();

    put::run_with(
        put_args("repeat me"),
        provider.clone(),
        Arc::new(sink_a.clone()),
    )
    .await
    .unwrap();
    put::run_with(put_args("repeat me"), provider, Arc::new(sink_b.clone()))
        .await
        .unwrap();

    let a = sink_a.chunks();
    let b = sink_b.chunks();
    assert_eq!(a[0].key, b[0].key);
    assert_eq!(a[0].vector, b[0].vector);
}
