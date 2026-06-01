//! Phase 3 acceptance: the streaming pipeline ingests files via the
//! Source → Parse → Chunk → Embed → Put stages.
//!
//! Uses the in-memory sink + stub provider so the test stays hermetic
//! and doesn't need a running marila.

use std::sync::Arc;

use marila_embed::chunk::{self, ChunkConfig};
use marila_embed::cli::{ChunkStrategy, KeyStrategy};
use marila_embed::embed::EmbeddingProvider;
use marila_embed::embed::stub::StubEmbedder;
use marila_embed::parse;
use marila_embed::pipeline::{ChannelCaps, PipelineConfig, run_local};
use marila_embed::sink::in_memory::InMemorySink;
use marila_embed::source::local::LocalSourceConfig;
use tempfile::tempdir;

fn write(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[tokio::test]
async fn pipeline_ingests_text_directory_into_multiple_chunks() {
    let dir = tempdir().unwrap();
    let body = "lorem ipsum dolor sit amet ".repeat(800); // long
    write(dir.path(), "a.txt", &body);
    write(dir.path(), "b.md", "# Heading\n\nsome content here.");
    write(
        dir.path(),
        "c.html",
        "<html><body><p>greetings</p></body></html>",
    );
    write(dir.path(), "skip.bin", "binary"); // no parser -> skipped

    let provider = Arc::new(StubEmbedder::new(64));
    let sink = InMemorySink::new();
    let cfg = PipelineConfig {
        provider: provider.clone(),
        sink: Arc::new(sink.clone()),
        chunker: chunk::build(
            ChunkStrategy::Fixed,
            ChunkConfig {
                size: 100,
                overlap: 20,
            },
        ),
        parsers: parse::default_set(),
        key_strategy: KeyStrategy::ContentHash,
        extra_metadata: Default::default(),
        no_source_content: false,
        parse_concurrency: 2,
        embed_concurrency: 2,
        embed_batch: 16,
        put_batch: 8,
        put_flush_ms: 50,
        max_chunks: 0,
        caps: ChannelCaps::default(),
        checkpoint: None,
        progress: None,
    };
    let source_cfg = LocalSourceConfig {
        inputs: vec![dir.path().to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 10 * 1024 * 1024,
    };
    let stats = run_local(cfg, source_cfg).await.expect("pipeline");

    assert!(stats.raw_docs >= 4, "saw {} raw docs", stats.raw_docs);
    // Three parseable files (txt, md, html) — .bin has no parser, is
    // counted in raw_docs but skipped (no parse_failure, just warn).
    assert_eq!(stats.parsed_docs, 3);
    assert!(
        stats.chunks > 1,
        "expected >1 chunk total, got {}",
        stats.chunks
    );
    assert_eq!(stats.chunks, stats.embedded);
    assert_eq!(stats.embedded, stats.put);

    let stored = sink.chunks();
    assert_eq!(stored.len() as u64, stats.put);

    // Every stored chunk has the spec metadata
    for c in &stored {
        let meta = &c.metadata;
        assert!(meta.contains_key("S3VECTORS-EMBED-SRC-LOCATION"));
        assert!(meta.contains_key("S3VECTORS-EMBED-SRC-CONTENT"));
        assert!(meta.contains_key("S3VECTORS-EMBED-CHUNK-IDX"));
        assert!(meta.contains_key("S3VECTORS-EMBED-CONTENT-HASH"));
        assert_eq!(c.vector.len(), provider.dimension() as usize);
        assert!(!c.key.is_empty());
    }
}

#[tokio::test]
async fn max_chunks_caps_emission() {
    let dir = tempdir().unwrap();
    let body = "alpha bravo charlie ".repeat(2000);
    write(dir.path(), "big.txt", &body);

    let provider = Arc::new(StubEmbedder::new(16));
    let sink = InMemorySink::new();
    let cfg = PipelineConfig {
        provider,
        sink: Arc::new(sink.clone()),
        chunker: chunk::build(
            ChunkStrategy::Fixed,
            ChunkConfig {
                size: 50,
                overlap: 10,
            },
        ),
        parsers: parse::default_set(),
        key_strategy: KeyStrategy::ContentHash,
        extra_metadata: Default::default(),
        no_source_content: true,
        parse_concurrency: 1,
        embed_concurrency: 1,
        embed_batch: 8,
        put_batch: 4,
        put_flush_ms: 25,
        max_chunks: 5,
        caps: ChannelCaps::default(),
        checkpoint: None,
        progress: None,
    };
    let source_cfg = LocalSourceConfig {
        inputs: vec![dir.path().to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 50 * 1024 * 1024,
    };
    let stats = run_local(cfg, source_cfg).await.unwrap();
    assert!(
        stats.chunks <= 5,
        "expected ≤5 chunks under cap, got {}",
        stats.chunks
    );
    assert!(stats.put <= 5);
}

#[tokio::test]
async fn max_file_bytes_skips_large_files() {
    let dir = tempdir().unwrap();
    write(dir.path(), "tiny.txt", "hello");
    write(dir.path(), "huge.txt", &"x".repeat(2_000_000));

    let provider = Arc::new(StubEmbedder::new(16));
    let sink = InMemorySink::new();
    let cfg = PipelineConfig {
        provider,
        sink: Arc::new(sink.clone()),
        chunker: chunk::build(
            ChunkStrategy::Off,
            ChunkConfig {
                size: 100,
                overlap: 0,
            },
        ),
        parsers: parse::default_set(),
        key_strategy: KeyStrategy::ContentHash,
        extra_metadata: Default::default(),
        no_source_content: true,
        parse_concurrency: 1,
        embed_concurrency: 1,
        embed_batch: 8,
        put_batch: 4,
        put_flush_ms: 25,
        max_chunks: 0,
        caps: ChannelCaps::default(),
        checkpoint: None,
        progress: None,
    };
    let source_cfg = LocalSourceConfig {
        inputs: vec![dir.path().to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 1_000_000, // huge.txt exceeds
    };
    let stats = run_local(cfg, source_cfg).await.unwrap();
    assert_eq!(stats.parsed_docs, 1, "expected huge.txt to be skipped");
    assert_eq!(stats.chunks, 1);
}
