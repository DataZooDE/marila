//! The four-stage streaming pipeline from `doc/EMBED_CLI_SPEC.md` §3:
//!
//! ```text
//!   Source ─▶ Parse ─▶ Chunk ─▶ Embed ─▶ Put
//! ```
//!
//! Each stage is its own `tokio::task` (parse + embed are pools).
//! Stages are connected by bounded `tokio::sync::mpsc` channels so
//! backpressure flows naturally: when the put-stage slows, embed
//! eventually fills its outbound channel and blocks; that propagates
//! back through chunk, parse, and finally source.
//!
//! Steady-state memory ≈ `sum(channel_cap × avg_element_size)`, which
//! is bounded *independently* of corpus size — the spec's headline
//! invariant.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::checkpoint::Checkpoint;
use crate::chunk::{Chunk, Chunker};
use crate::embed::EmbeddingProvider;
use crate::keys::chunk_key;
use crate::parse::{self, ParsedDoc, Parser};
use crate::progress::ProgressCounters;
use crate::put::{
    META_CHUNK_IDX, META_CONTENT_HASH, META_SRC_CONTENT, META_SRC_LOCATION,
};
use crate::sink::{EmbeddedChunk, Sink};
use crate::source::{RawDoc, local::LocalSourceConfig};

/// Channel capacities — spec §3 defaults.
#[derive(Debug, Clone, Copy)]
pub struct ChannelCaps {
    pub source_to_parse: usize,
    pub parse_to_chunk: usize,
    pub chunk_to_embed: usize,
    pub embed_to_put: usize,
}

impl Default for ChannelCaps {
    fn default() -> Self {
        Self {
            source_to_parse: 64,
            parse_to_chunk: 64,
            chunk_to_embed: 512,
            embed_to_put: 512,
        }
    }
}

/// Pipeline configuration: everything `put::run` knows that the
/// individual stages need.
pub struct PipelineConfig {
    pub provider: Arc<dyn EmbeddingProvider>,
    pub sink: Arc<dyn Sink>,
    pub chunker: Box<dyn Chunker>,
    pub parsers: Vec<Arc<dyn Parser>>,
    pub key_strategy: crate::cli::KeyStrategy,
    pub extra_metadata: BTreeMap<String, Value>,
    pub no_source_content: bool,
    pub parse_concurrency: usize,
    pub embed_concurrency: usize,
    pub embed_batch: usize,
    pub put_batch: usize,
    pub put_flush_ms: u64,
    pub max_chunks: u64,
    pub caps: ChannelCaps,
    /// Resumable-run state. `None` disables checkpointing entirely.
    pub checkpoint: Option<Arc<Checkpoint>>,
    /// Optional externally-owned counters — when present, the pipeline
    /// shares them with the progress reporter instead of allocating
    /// its own.
    pub progress: Option<Arc<ProgressCounters>>,
}

/// Counters returned to the caller for the summary line.
#[derive(Debug, Default, Clone, Copy)]
pub struct PipelineStats {
    pub raw_docs: u64,
    pub parsed_docs: u64,
    pub chunks: u64,
    pub embedded: u64,
    pub put: u64,
    pub parse_failures: u64,
    pub embed_failures: u64,
}

/// Run a pipeline drained by a single local-filesystem source.
pub async fn run_local(cfg: PipelineConfig, source_cfg: LocalSourceConfig) -> Result<PipelineStats> {
    let (raw_tx, raw_rx) = mpsc::channel::<RawDoc>(cfg.caps.source_to_parse);
    let checkpoint = cfg.checkpoint.clone();
    let source_handle = tokio::spawn(async move {
        if let Err(e) =
            crate::source::local::run_with_checkpoint(source_cfg, raw_tx, checkpoint).await
        {
            warn!(error = %e, "local source failed");
        }
    });
    let stats = drain(cfg, raw_rx).await?;
    let _ = source_handle.await;
    Ok(stats)
}

/// Run a pipeline driven by a pre-baked source channel. Used by the
/// `--text-value` path so we don't pay for a walkdir we don't need.
pub async fn run_with_source(
    cfg: PipelineConfig,
    raw_rx: mpsc::Receiver<RawDoc>,
) -> Result<PipelineStats> {
    drain(cfg, raw_rx).await
}

async fn drain(cfg: PipelineConfig, raw_rx: mpsc::Receiver<RawDoc>) -> Result<PipelineStats> {
    let caps = cfg.caps;

    // Reuse the externally-provided counters when the caller passed one
    // — that's how the progress reporter sees live data — otherwise
    // allocate a fresh set just for the final stats snapshot.
    use std::sync::atomic::Ordering;
    let counters = cfg
        .progress
        .clone()
        .unwrap_or_else(|| Arc::new(ProgressCounters::default()));

    // ----- channels -----
    // The embed→put channel carries `(EmbeddedChunk, source)` so the
    // put stage can attribute each put back to its source for the
    // checkpoint's per-source completion tracking.
    let (parsed_tx, parsed_rx) = mpsc::channel::<ParsedDoc>(caps.parse_to_chunk);
    let (chunk_tx, chunk_rx) = mpsc::channel::<Chunk>(caps.chunk_to_embed);
    let (embedded_tx, embedded_rx) =
        mpsc::channel::<(EmbeddedChunk, String)>(caps.embed_to_put);

    // ----- parse pool -----
    let parsers = cfg.parsers.clone();
    let parse_workers = cfg.parse_concurrency.max(1);
    let parse_pool = {
        let counters = counters.clone();
        tokio::spawn(async move {
            // Fan out from a single raw_rx by wrapping it in a tokio::Mutex so
            // each worker pulls in turn. mpsc::Receiver is single-consumer so
            // this is the standard fan-out idiom.
            let raw_rx = Arc::new(tokio::sync::Mutex::new(raw_rx));
            let mut handles = Vec::with_capacity(parse_workers);
            for _ in 0..parse_workers {
                let raw_rx = raw_rx.clone();
                let parsed_tx = parsed_tx.clone();
                let parsers = parsers.clone();
                let counters = counters.clone();
                handles.push(tokio::spawn(async move {
                    loop {
                        let raw = {
                            let mut g = raw_rx.lock().await;
                            g.recv().await
                        };
                        let Some(raw) = raw else { break };
                        counters.raw_docs.fetch_add(1, Ordering::Relaxed);
                        let Some(parser) = parse::dispatch(&parsers, &raw.ext) else {
                            warn!(
                                source = %raw.source,
                                ext = %raw.ext,
                                "no parser for extension; skipping"
                            );
                            continue;
                        };
                        // Parsing can be CPU-bound (PDF, office); run in
                        // a blocking thread so we don't stall the runtime.
                        let parsed = tokio::task::spawn_blocking({
                            let raw = raw.clone();
                            move || parser.parse(raw)
                        })
                        .await;
                        match parsed {
                            Ok(Ok(doc)) => {
                                counters.parsed_docs.fetch_add(1, Ordering::Relaxed);
                                if parsed_tx.send(doc).await.is_err() {
                                    break;
                                }
                            }
                            Ok(Err(e)) => {
                                counters.parse_failures.fetch_add(1, Ordering::Relaxed);
                                warn!(source = %raw.source, error = %e, "parse failed");
                            }
                            Err(e) => {
                                counters.parse_failures.fetch_add(1, Ordering::Relaxed);
                                warn!(source = %raw.source, error = %e, "parse task panicked");
                            }
                        }
                    }
                }));
            }
            drop(parsed_tx);
            for h in handles {
                let _ = h.await;
            }
        })
    };

    // ----- chunk task -----
    let chunker = cfg.chunker;
    let key_strategy = cfg.key_strategy;
    let chunker_checkpoint = cfg.checkpoint.clone();
    let chunk_task = {
        let counters = counters.clone();
        let max_chunks = cfg.max_chunks;
        tokio::spawn(async move {
            let mut parsed_rx = parsed_rx;
            'outer: while let Some(doc) = parsed_rx.recv().await {
                let pieces = chunker.chunk(&doc);
                debug!(source = %doc.source, count = pieces.len(), "chunked");
                let count = pieces.len() as u32;
                let source = doc.source.clone();
                let content_hash = doc.content_hash.clone();
                for piece in pieces {
                    if max_chunks > 0
                        && counters.chunks.load(Ordering::Relaxed) >= max_chunks
                    {
                        debug!("max_chunks cap hit; stopping chunk emission");
                        break 'outer;
                    }
                    counters.chunks.fetch_add(1, Ordering::Relaxed);
                    if chunk_tx.send(piece).await.is_err() {
                        break 'outer;
                    }
                }
                if let Some(chk) = &chunker_checkpoint {
                    chk.seal(&source, count, &content_hash).await;
                }
            }
            drop(chunk_tx);
        })
    };

    // ----- embed pool -----
    let provider = cfg.provider.clone();
    let embed_workers = cfg.embed_concurrency.max(1);
    let embed_batch = cfg.embed_batch.max(1).min(provider.max_batch());
    let embed_pool = {
        let counters = counters.clone();
        let extra_metadata = cfg.extra_metadata.clone();
        let no_source_content = cfg.no_source_content;
        tokio::spawn(async move {
            let chunk_rx = Arc::new(tokio::sync::Mutex::new(chunk_rx));
            let mut handles = Vec::with_capacity(embed_workers);
            for _ in 0..embed_workers {
                let chunk_rx = chunk_rx.clone();
                let embedded_tx = embedded_tx.clone();
                let provider = provider.clone();
                let counters = counters.clone();
                let extra_metadata = extra_metadata.clone();
                handles.push(tokio::spawn(async move {
                    'worker: loop {
                        let mut batch: Vec<Chunk> = Vec::with_capacity(embed_batch);
                        {
                            let mut g = chunk_rx.lock().await;
                            for _ in 0..embed_batch {
                                match g.recv().await {
                                    Some(c) => batch.push(c),
                                    None => break,
                                }
                            }
                        }
                        if batch.is_empty() {
                            break 'worker;
                        }
                        let texts: Vec<&str> = batch.iter().map(|c| c.text.as_str()).collect();
                        match provider.embed(&texts).await {
                            Ok(resp) => {
                                if resp.vectors.len() != batch.len() {
                                    counters
                                        .embed_failures
                                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                                    warn!(
                                        wanted = batch.len(),
                                        got = resp.vectors.len(),
                                        "embedder returned wrong vector count"
                                    );
                                    continue;
                                }
                                for (c, vector) in batch.into_iter().zip(resp.vectors.into_iter()) {
                                    let key = chunk_key(
                                        key_strategy,
                                        &c.source,
                                        c.chunk_idx,
                                        &c.text,
                                    );
                                    let mut metadata = base_metadata(&c, no_source_content);
                                    for (k, v) in &extra_metadata {
                                        metadata.insert(k.clone(), v.clone());
                                    }
                                    let source = c.source.clone();
                                    let ec = EmbeddedChunk { key, vector, metadata };
                                    counters.embedded.fetch_add(1, Ordering::Relaxed);
                                    if embedded_tx.send((ec, source)).await.is_err() {
                                        break 'worker;
                                    }
                                }
                            }
                            Err(e) => {
                                counters
                                    .embed_failures
                                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
                                warn!(error = %e, batch_size = batch.len(), "embed failed");
                            }
                        }
                    }
                }));
            }
            drop(embedded_tx);
            for h in handles {
                let _ = h.await;
            }
        })
    };

    // ----- put batcher -----
    let put_batch = cfg.put_batch.max(1);
    let flush = std::time::Duration::from_millis(cfg.put_flush_ms.max(1));
    let sink = cfg.sink.clone();
    let put_checkpoint = cfg.checkpoint.clone();
    let put_task = {
        let counters = counters.clone();
        tokio::spawn(async move {
            // We need to know which sources each batched chunk came from so
            // we can update the checkpoint on success. Pair them with
            // their EmbeddedChunk in a sibling Vec.
            let mut buf: Vec<EmbeddedChunk> = Vec::with_capacity(put_batch);
            let mut sources: Vec<String> = Vec::with_capacity(put_batch);
            let mut embedded_rx = embedded_rx;
            loop {
                tokio::select! {
                    chunk = embedded_rx.recv() => {
                        match chunk {
                            Some((c, src)) => {
                                buf.push(c);
                                sources.push(src);
                                if buf.len() >= put_batch {
                                    flush_buf(&sink, &mut buf, &mut sources, &counters.put, &put_checkpoint).await;
                                }
                            }
                            None => {
                                flush_buf(&sink, &mut buf, &mut sources, &counters.put, &put_checkpoint).await;
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(flush), if !buf.is_empty() => {
                        flush_buf(&sink, &mut buf, &mut sources, &counters.put, &put_checkpoint).await;
                    }
                }
            }
        })
    };

    // ----- join -----
    let _ = parse_pool.await;
    let _ = chunk_task.await;
    let _ = embed_pool.await;
    let _ = put_task.await;

    Ok(PipelineStats {
        raw_docs: counters.raw_docs.load(Ordering::Relaxed),
        parsed_docs: counters.parsed_docs.load(Ordering::Relaxed),
        chunks: counters.chunks.load(Ordering::Relaxed),
        embedded: counters.embedded.load(Ordering::Relaxed),
        put: counters.put.load(Ordering::Relaxed),
        parse_failures: counters.parse_failures.load(Ordering::Relaxed),
        embed_failures: counters.embed_failures.load(Ordering::Relaxed),
    })
}

async fn flush_buf(
    sink: &Arc<dyn Sink>,
    buf: &mut Vec<EmbeddedChunk>,
    sources: &mut Vec<String>,
    counter: &std::sync::atomic::AtomicU64,
    checkpoint: &Option<Arc<Checkpoint>>,
) {
    if buf.is_empty() {
        return;
    }
    let to_put: Vec<EmbeddedChunk> = std::mem::take(buf);
    let attribution: Vec<String> = std::mem::take(sources);
    let n = to_put.len() as u64;
    if let Err(e) = sink.put(&to_put).await {
        warn!(error = %e, count = n, "sink put failed");
    } else {
        counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        if let Some(chk) = checkpoint {
            for src in &attribution {
                chk.record_put(src).await;
            }
        }
    }
}

fn base_metadata(c: &Chunk, no_source_content: bool) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert(
        META_SRC_LOCATION.to_string(),
        Value::String(c.source.clone()),
    );
    if !no_source_content {
        m.insert(
            META_SRC_CONTENT.to_string(),
            Value::String(truncate_chars(&c.text, 8 * 1024)),
        );
    }
    m.insert(
        META_CHUNK_IDX.to_string(),
        Value::Number(c.chunk_idx.into()),
    );
    let content_hash = blake3::hash(c.text.as_bytes()).to_hex().to_string();
    m.insert(META_CONTENT_HASH.to_string(), Value::String(content_hash));
    if !c.section_path.is_empty() {
        m.insert(
            "marila.section_path".to_string(),
            Value::Array(c.section_path.iter().cloned().map(Value::String).collect()),
        );
    }
    m
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
