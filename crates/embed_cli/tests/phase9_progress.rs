//! Phase 9 acceptance: ProgressCounters increment as the pipeline runs,
//! and `peak_rss_bytes()` returns a sensible Linux value.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use marila_embed::chunk::{self, ChunkConfig};
use marila_embed::cli::{ChunkStrategy, KeyStrategy};
use marila_embed::embed::stub::StubEmbedder;
use marila_embed::parse;
use marila_embed::pipeline::{ChannelCaps, PipelineConfig, run_local};
use marila_embed::progress::{ProgressCounters, peak_rss_bytes};
use marila_embed::sink::in_memory::InMemorySink;
use marila_embed::source::local::LocalSourceConfig;
use tempfile::tempdir;

#[tokio::test]
async fn counters_reach_expected_totals_after_run() {
    let dir = tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("body {i}")).unwrap();
    }

    let counters = Arc::new(ProgressCounters::default());
    let sink = InMemorySink::new();
    let cfg = PipelineConfig {
        provider: Arc::new(StubEmbedder::new(16)),
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
        embed_batch: 4,
        put_batch: 4,
        put_flush_ms: 10,
        max_chunks: 0,
        caps: ChannelCaps::default(),
        checkpoint: None,
        progress: Some(counters.clone()),
    };
    let source_cfg = LocalSourceConfig {
        inputs: vec![dir.path().to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 1_000_000,
    };
    let _ = run_local(cfg, source_cfg).await.unwrap();

    assert_eq!(counters.raw_docs.load(Ordering::Relaxed), 5);
    assert_eq!(counters.parsed_docs.load(Ordering::Relaxed), 5);
    assert_eq!(counters.chunks.load(Ordering::Relaxed), 5);
    assert_eq!(counters.embedded.load(Ordering::Relaxed), 5);
    assert_eq!(counters.put.load(Ordering::Relaxed), 5);
    assert_eq!(counters.parse_failures.load(Ordering::Relaxed), 0);
    assert_eq!(counters.embed_failures.load(Ordering::Relaxed), 0);
}

#[test]
fn peak_rss_returns_nonzero_on_linux() {
    let r = peak_rss_bytes();
    if cfg!(target_os = "linux") {
        let bytes = r.expect("VmHWM readable on Linux");
        assert!(bytes > 1024 * 100, "implausibly small RSS: {bytes}");
    }
}
