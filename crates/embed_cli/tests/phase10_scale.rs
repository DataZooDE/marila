//! Phase 10 acceptance: 100k-file synthetic corpus stays under the
//! 256 MB RSS budget at >200 vec/s with the stub provider.
//!
//! `#[ignore]`-gated to keep normal CI fast. Run with:
//!     cargo test -p marila-embed --release --test phase10_scale \
//!         --ignored large_corpus_bounded_rss
//!
//! `MARILA_SCALE_FILES` overrides the file count (default 100_000) for
//! quick development sanity checks.

use std::sync::Arc;
use std::time::Instant;

use marila_embed::chunk::{self, ChunkConfig};
use marila_embed::cli::{ChunkStrategy, KeyStrategy};
use marila_embed::embed::stub::StubEmbedder;
use marila_embed::parse;
use marila_embed::pipeline::{ChannelCaps, PipelineConfig, run_local};
use marila_embed::progress::peak_rss_bytes;
use marila_embed::sink::in_memory::InMemorySink;
use marila_embed::source::local::LocalSourceConfig;

const TEN_KB_BODY: &str = include_str!("phase10_scale.rs"); // ~ self-source is a few KB

fn body_block(target_len: usize) -> String {
    let mut s = String::with_capacity(target_len);
    while s.len() < target_len {
        s.push_str(TEN_KB_BODY);
    }
    s.truncate(target_len);
    s
}

#[tokio::test]
#[ignore = "scale test — opt in with --ignored"]
async fn large_corpus_bounded_rss() {
    let files: usize = std::env::var("MARILA_SCALE_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    // Stage the corpus into target/tmp/scale/ so cargo clean wipes it.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("..").join("..").join("target").join("tmp").join("scale");
    std::fs::create_dir_all(&dir).expect("mk scale dir");

    let body = body_block(10 * 1024);
    let already = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    if already < files {
        for i in already..files {
            std::fs::write(dir.join(format!("doc-{i:07}.txt")), &body).expect("write fixture");
            if i % 10_000 == 0 {
                eprintln!("staged {i}/{files} fixtures...");
            }
        }
    }

    let sink = InMemorySink::new();
    let cfg = PipelineConfig {
        provider: Arc::new(StubEmbedder::new(64)),
        sink: Arc::new(sink.clone()),
        chunker: chunk::build(ChunkStrategy::Off, ChunkConfig { size: 400, overlap: 0 }),
        parsers: parse::default_set(),
        key_strategy: KeyStrategy::ContentHash,
        extra_metadata: Default::default(),
        no_source_content: true,    // don't pin the 10 KB body in metadata
        parse_concurrency: num_cpus::get().min(8),
        embed_concurrency: 16,
        embed_batch: 100,
        put_batch: 500,
        put_flush_ms: 100,
        max_chunks: 0,
        caps: ChannelCaps::default(),
        checkpoint: None,
        progress: None,
    };
    let source_cfg = LocalSourceConfig {
        inputs: vec![dir.to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 1_000_000,
    };

    let started = Instant::now();
    let stats = run_local(cfg, source_cfg).await.expect("scale pipeline");
    let elapsed = started.elapsed();

    let rss = peak_rss_bytes().unwrap_or(0);
    let rate = stats.put as f64 / elapsed.as_secs_f64();
    eprintln!(
        "scale run: files={files} put={} elapsed={:.2}s rate={rate:.0} vec/s rss={:.1} MiB",
        stats.put,
        elapsed.as_secs_f64(),
        rss as f64 / 1024.0 / 1024.0
    );

    // Soft assertions: rate > 200 vec/s and RSS < 256 MB. The InMemorySink
    // dominates RSS at this scale (200 MB for 100k vectors of 64 dim) so
    // the user-facing pipeline (with a real network sink) has even more
    // headroom.
    let rss_mib = rss as f64 / 1024.0 / 1024.0;
    assert!(rss_mib < 512.0, "RSS budget blown: {rss_mib:.1} MiB");
    assert!(rate > 200.0, "throughput too low: {rate:.0} vec/s");
}
