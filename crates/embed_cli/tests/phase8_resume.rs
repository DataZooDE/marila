//! Phase 8 acceptance: a kill-mid-run resumes cleanly with `--resume`.
//!
//! The "kill" is simulated by setting `--max-chunks` low so the first
//! pass only puts a fraction of the corpus; the second pass with the
//! same checkpoint picks up where the first left off. Final vector
//! count should match the baseline of a single un-capped run.

use std::sync::Arc;

use marila_embed::checkpoint::Checkpoint;
use marila_embed::chunk::{self, ChunkConfig};
use marila_embed::cli::{ChunkStrategy, KeyStrategy};
use marila_embed::embed::stub::StubEmbedder;
use marila_embed::parse;
use marila_embed::pipeline::{ChannelCaps, PipelineConfig, run_local};
use marila_embed::sink::in_memory::InMemorySink;
use marila_embed::source::local::LocalSourceConfig;
use tempfile::tempdir;

fn write(dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

#[tokio::test]
async fn resume_skips_done_sources_without_duplication() {
    // Put sources in a `src/` subdir so the checkpoint files don't get
    // picked up by the source walker as input documents.
    let root = tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    write(&src, "a.txt", "alpha");
    write(&src, "b.txt", "bravo");
    write(&src, "c.txt", "charlie");
    let dir = src;

    let checkpoint_path = root.path().join("ck.jsonl");

    // ----- Baseline: single run, everything goes through -----
    let baseline_sink = InMemorySink::new();
    let baseline_chk =
        Arc::new(Checkpoint::load(root.path().join("ck-baseline.jsonl"), true).await.unwrap());
    let cfg = base_cfg(baseline_sink.clone(), Some(baseline_chk.clone()), 0);
    let stats = run_local(cfg, src_cfg(&dir)).await.unwrap();
    let baseline_count = baseline_sink.len();
    assert_eq!(baseline_count, 3, "baseline put count");
    assert_eq!(stats.put, 3);

    // ----- First pass: cap at 2 chunks to simulate an early crash -----
    let pass1_sink = InMemorySink::new();
    let chk = Arc::new(Checkpoint::load(checkpoint_path.clone(), true).await.unwrap());
    let cfg = base_cfg(pass1_sink.clone(), Some(chk.clone()), 2);
    let stats1 = run_local(cfg, src_cfg(&dir)).await.unwrap();
    assert!(stats1.put <= 2, "pass1 must have stopped early, got {}", stats1.put);
    let pass1_count = pass1_sink.len();
    drop(chk);

    // ----- Second pass: resume, no cap -----
    let pass2_sink = InMemorySink::new();
    let chk2 = Arc::new(Checkpoint::load(checkpoint_path.clone(), true).await.unwrap());
    let cfg = base_cfg(pass2_sink.clone(), Some(chk2), 0);
    let _stats2 = run_local(cfg, src_cfg(&dir)).await.unwrap();

    // Combined sinks: pass1 + pass2 should equal the baseline. With
    // content-hash keys, even if a source was partially put in pass1
    // and re-processed in pass2, dedup happens at PutVectors-time on
    // the same key.
    let combined: std::collections::BTreeSet<String> = pass1_sink
        .chunks()
        .into_iter()
        .chain(pass2_sink.chunks())
        .map(|c| c.key)
        .collect();
    assert_eq!(
        combined.len(),
        baseline_count,
        "combined unique-keys must equal baseline ({})",
        baseline_count
    );

    // Specifically: sources that were marked done in pass1 must not be
    // re-processed in pass2.
    let pass1_keys: std::collections::BTreeSet<String> =
        pass1_sink.chunks().into_iter().map(|c| c.key).collect();
    let pass2_keys: std::collections::BTreeSet<String> =
        pass2_sink.chunks().into_iter().map(|c| c.key).collect();
    assert!(
        pass1_keys.is_disjoint(&pass2_keys) || baseline_count >= pass1_count + pass2_sink.len(),
        "pass1 and pass2 should cover disjoint or non-overlapping sources"
    );
}

fn base_cfg(
    sink: InMemorySink,
    checkpoint: Option<Arc<Checkpoint>>,
    max_chunks: u64,
) -> PipelineConfig {
    PipelineConfig {
        provider: Arc::new(StubEmbedder::new(16)),
        sink: Arc::new(sink),
        chunker: chunk::build(ChunkStrategy::Off, ChunkConfig { size: 100, overlap: 0 }),
        parsers: parse::default_set(),
        key_strategy: KeyStrategy::ContentHash,
        extra_metadata: Default::default(),
        no_source_content: true,
        parse_concurrency: 1,
        embed_concurrency: 1,
        embed_batch: 4,
        put_batch: 4,
        put_flush_ms: 10,
        max_chunks,
        caps: ChannelCaps::default(),
        checkpoint,
        progress: None,
    }
}

fn src_cfg(dir: &std::path::Path) -> LocalSourceConfig {
    LocalSourceConfig {
        inputs: vec![dir.to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 1_000_000,
    }
}
