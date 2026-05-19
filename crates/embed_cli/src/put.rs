//! The `put` subcommand. Phase 1 wires the simplest end-to-end path:
//!
//! - `--text-value "hello"` → one chunk → stub embed → in-memory sink.
//!
//! Later phases swap the in-memory sink for s3vectors, add the
//! Source/Parse/Chunk stages, and bring in real providers.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tracing::{debug, info};

use crate::cli::{EmbeddingProviderName, PutArgs};
use crate::embed::{EmbeddingProvider, stub::StubEmbedder};
use crate::keys::chunk_key;
use crate::sink::{EmbeddedChunk, Sink, in_memory::InMemorySink, s3vectors::S3VectorsSink};

/// Stable per-vector metadata keys. Match AWS's `s3vectors-embed-cli`
/// naming verbatim so consumers of `--filter` don't need to retrain.
pub const META_SRC_LOCATION: &str = "S3VECTORS-EMBED-SRC-LOCATION";
pub const META_SRC_CONTENT: &str = "S3VECTORS-EMBED-SRC-CONTENT";
pub const META_CHUNK_IDX: &str = "S3VECTORS-EMBED-CHUNK-IDX";
pub const META_CONTENT_HASH: &str = "S3VECTORS-EMBED-CONTENT-HASH";

/// Upper bound on the inline source-content metadata field. AWS limits
/// per-vector metadata to ~40 KB total; staying small keeps headroom for
/// caller-provided fields too. (Real value tunable later via the spec.)
const MAX_INLINE_CONTENT_BYTES: usize = 8 * 1024;

/// `marila-embed put` entry point. Wires the configured provider to the
/// s3vectors sink. Phase 1 only handled `--text-value`; later phases
/// expand `collect_chunks` to walk + parse + chunk files.
pub async fn run(args: PutArgs) -> Result<PutOutcome> {
    let provider = build_provider(&args).await?;
    let sink = build_sink(&args, provider.as_ref()).await?;
    let dry_run = args.dry_run;
    run_with(args, provider, sink).await?;
    Ok(PutOutcome {
        dry_run,
    })
}

async fn build_provider(args: &PutArgs) -> Result<Arc<dyn EmbeddingProvider>> {
    match args.common.embedding_provider {
        EmbeddingProviderName::Stub => {
            let dim = args
                .common
                .embedding_model
                .as_deref()
                .and_then(|m| m.strip_prefix("stub-"))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(crate::embed::stub::DEFAULT_DIMENSION);
            Ok(Arc::new(StubEmbedder::new(dim)))
        }
        EmbeddingProviderName::Openai => anyhow::bail!(
            "openai provider is wired in phase 6 — pass --embedding-provider stub for now"
        ),
        EmbeddingProviderName::Ollama => anyhow::bail!(
            "ollama provider is wired in phase 6 — pass --embedding-provider stub for now"
        ),
    }
}

async fn build_sink(
    args: &PutArgs,
    provider: &dyn EmbeddingProvider,
) -> Result<Arc<dyn Sink>> {
    if args.dry_run {
        return Ok(Arc::new(InMemorySink::new()));
    }
    let client = crate::aws::vectors_client(&args.common).await;
    let mut sink = S3VectorsSink::new(
        client,
        args.common.vector_bucket_name.clone(),
        args.common.index_name.clone(),
    );
    if args.auto_create_index {
        sink = sink.with_auto_create(provider.dimension());
    }
    Ok(sink.into_arc())
}

/// Test-friendly variant: caller supplies the provider and sink, so the
/// in-memory sink contents can be inspected directly.
pub async fn run_with(
    args: PutArgs,
    provider: Arc<dyn EmbeddingProvider>,
    sink: Arc<dyn Sink>,
) -> Result<()> {
    let extra_metadata = parse_metadata(args.metadata.as_deref())?;

    let chunks = collect_chunks(&args)?;
    if chunks.is_empty() {
        anyhow::bail!("no input — pass --text-value or --text <path>");
    }

    debug!(
        provider = provider.name(),
        model = provider.model(),
        dim = provider.dimension(),
        chunk_count = chunks.len(),
        "starting put"
    );

    let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
    let resp = provider.embed(&texts).await?;
    anyhow::ensure!(
        resp.vectors.len() == chunks.len(),
        "embedder returned {} vectors for {} inputs",
        resp.vectors.len(),
        chunks.len()
    );

    let embedded: Vec<EmbeddedChunk> = chunks
        .into_iter()
        .zip(resp.vectors.into_iter())
        .map(|(c, vector)| {
            let mut metadata = base_metadata(&c, args.no_source_content);
            for (k, v) in &extra_metadata {
                metadata.insert(k.clone(), v.clone());
            }
            EmbeddedChunk {
                key: c.key,
                vector,
                metadata,
            }
        })
        .collect();

    sink.put(&embedded).await?;
    info!(stored = embedded.len(), "put complete");
    Ok(())
}

/// Result handed back to `main` so it can log a final summary.
#[derive(Debug)]
pub struct PutOutcome {
    pub dry_run: bool,
}

/// A pre-embed unit of work — the smallest thing the chunker emits and
/// the largest thing we hand to the embedder.
#[derive(Debug)]
pub(crate) struct PreEmbedChunk {
    pub key: String,
    pub text: String,
    pub source: String,
    pub chunk_idx: u32,
    pub content_hash: String,
}

fn collect_chunks(args: &PutArgs) -> Result<Vec<PreEmbedChunk>> {
    if let Some(text) = args.text_value.as_deref() {
        let source = "<text-value>".to_string();
        let chunk_idx = 0;
        let content_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        let key = chunk_key(args.key_strategy, &source, chunk_idx, text);
        Ok(vec![PreEmbedChunk {
            key,
            text: text.to_string(),
            source,
            chunk_idx,
            content_hash,
        }])
    } else {
        // File globs land in Phase 3.
        Ok(Vec::new())
    }
}

fn base_metadata(c: &PreEmbedChunk, no_source_content: bool) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert(
        META_SRC_LOCATION.to_string(),
        Value::String(c.source.clone()),
    );
    if !no_source_content {
        let truncated = truncate_chars(&c.text, MAX_INLINE_CONTENT_BYTES);
        m.insert(META_SRC_CONTENT.to_string(), Value::String(truncated));
    }
    m.insert(
        META_CHUNK_IDX.to_string(),
        Value::Number(c.chunk_idx.into()),
    );
    m.insert(
        META_CONTENT_HASH.to_string(),
        Value::String(c.content_hash.clone()),
    );
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

fn parse_metadata(json: Option<&str>) -> Result<BTreeMap<String, Value>> {
    let Some(s) = json else {
        return Ok(BTreeMap::new());
    };
    let v: Value = serde_json::from_str(s)?;
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("--metadata must be a JSON object"))?;
    Ok(obj
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

