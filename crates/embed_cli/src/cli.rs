//! CLI surface for `marila-embed`. Mirrors `doc/EMBED_CLI_SPEC.md` §6
//! verbatim — every flag named in the spec is parsed here so each
//! implementation phase only has to fill in the handler.
//!
//! Flag naming follows AWS's `s3vectors-embed-cli` where the surface
//! overlaps (`put`, `query`, `--vector-bucket-name`, `--index-name`,
//! `--text-value`, `--text`, `--k`, `--filter`) so existing AWS-doc
//! readers can switch CLIs without retraining their fingers.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

/// `marila-embed` — streaming ingestion CLI for marila's S3 Vectors façade.
#[derive(Debug, Parser)]
#[command(name = "marila-embed", version, about, long_about = None)]
pub struct Cli {
    /// Increase log verbosity (`-v`=debug, `-vv`=trace).
    ///
    /// Overridden by `RUST_LOG` if it is set.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Force on `debug` logging (equivalent to `-v`). Kept as a top-level
    /// flag for symmetry with AWS's `s3vectors-embed-cli --debug`.
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ingest documents into an s3vectors index.
    Put(PutArgs),
    /// Query an s3vectors index by embedding a text query.
    Query(QueryArgs),
}

// ---------------------------------------------------------------------------
// Common flags — shared by both subcommands (spec §6.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Args, Clone)]
pub struct CommonArgs {
    /// marila base URL.
    #[arg(long, env = "MARILA_ENDPOINT", default_value = "http://localhost:8080")]
    pub endpoint_url: String,

    /// AWS region used by the SDK for SigV4 signing.
    #[arg(long, env = "MARILA_REGION", default_value = "eu-west-1")]
    pub region: String,

    /// s3vectors bucket name.
    #[arg(long)]
    pub vector_bucket_name: String,

    /// Index name within the bucket.
    #[arg(long)]
    pub index_name: String,

    /// Embedding provider — `openai`, `ollama`, or `stub` (deterministic test).
    #[arg(long, env = "MARILA_EMBED_PROVIDER", default_value = "openai")]
    pub embedding_provider: EmbeddingProviderName,

    /// Provider-specific model name. Defaults: openai=text-embedding-3-small,
    /// ollama=embeddinggemma:latest, stub=stub-768.
    #[arg(long, env = "MARILA_EMBED_MODEL")]
    pub embedding_model: Option<String>,

    /// Optional TOML config file (flags > env > config > defaults).
    #[arg(long, default_value = "./marila-embed.toml")]
    pub config: PathBuf,

    /// Structured JSON-lines event log.
    #[arg(long, default_value = "./.marila-embed.jsonl")]
    pub log: PathBuf,

    /// Resume state (JSONL append-only).
    #[arg(long, default_value = "./.marila-embed-checkpoint.jsonl")]
    pub checkpoint: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EmbeddingProviderName {
    Openai,
    Ollama,
    Stub,
}

impl EmbeddingProviderName {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Ollama => "ollama",
            Self::Stub => "stub",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Table,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChunkStrategy {
    Off,
    Fixed,
    Markdown,
    Sentence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KeyStrategy {
    ContentHash,
    Filename,
    Path,
}

// ---------------------------------------------------------------------------
// `put` subcommand (spec §6.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct PutArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    // ----- inputs (one of) -----
    /// A single inline string to embed as one vector.
    #[arg(long, conflicts_with_all = ["text", "s3"])]
    pub text_value: Option<String>,

    /// Local file or glob (repeatable).
    #[arg(long)]
    pub text: Vec<String>,

    /// s3://bucket/prefix source (v0.5 — accepted but not yet wired).
    #[arg(long)]
    pub s3: Vec<String>,

    /// Extension allow-list (default: every parser we ship).
    #[arg(long)]
    pub include: Vec<String>,

    /// Extension deny-list.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Files larger than this are logged at WARN and skipped.
    #[arg(long, default_value_t = 50 * 1024 * 1024)]
    pub max_file_bytes: u64,

    // ----- chunking -----
    #[arg(long, value_enum, default_value = "fixed")]
    pub chunk_strategy: ChunkStrategy,
    #[arg(long, default_value_t = 400)]
    pub chunk_size: u32,
    #[arg(long, default_value_t = 80)]
    pub chunk_overlap: u32,

    // ----- keys + metadata -----
    #[arg(long, value_enum, default_value = "content-hash")]
    pub key_strategy: KeyStrategy,
    /// JSON object merged into every vector's metadata.
    #[arg(long)]
    pub metadata: Option<String>,
    /// Skip the `S3VECTORS-EMBED-SRC-CONTENT` field.
    #[arg(long)]
    pub no_source_content: bool,

    // ----- concurrency -----
    #[arg(long)]
    pub parse_concurrency: Option<usize>,
    #[arg(long, default_value_t = 8)]
    pub embed_concurrency: usize,
    #[arg(long, default_value_t = 100)]
    pub embed_batch: usize,
    #[arg(long, default_value_t = 4)]
    pub put_concurrency: usize,
    #[arg(long, default_value_t = 100)]
    pub put_batch: usize,
    #[arg(long, default_value_t = 250)]
    pub put_flush_ms: u64,

    // ----- behavioural flags -----
    /// Create the index if missing (default on; disable with `--no-auto-create-index`).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub auto_create_index: bool,
    /// Parse + chunk + count, no embed/put.
    #[arg(long)]
    pub dry_run: bool,
    /// Don't exit non-zero on per-chunk failures.
    #[arg(long)]
    pub ignore_errors: bool,
    /// Honour the checkpoint file (default on; disable with `--no-resume`).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub resume: bool,

    /// Stop after putting this many chunks. Useful for sampling a large
    /// corpus. `0` (default) means no cap.
    #[arg(long, default_value_t = 0)]
    pub max_chunks: u64,
    /// Random-sample source files at this fraction (0.0..=1.0). `1.0`
    /// (default) means no sampling. Sampling is deterministic per
    /// `--sample-seed`.
    #[arg(long, default_value_t = 1.0)]
    pub sample: f64,
    /// Seed for `--sample` random sampling.
    #[arg(long, default_value_t = 0)]
    pub sample_seed: u64,
}

// ---------------------------------------------------------------------------
// `query` subcommand (spec §6.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct QueryArgs {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Inline query string.
    #[arg(long, conflicts_with = "text")]
    pub text_value: Option<String>,
    /// Query file path.
    #[arg(long)]
    pub text: Option<PathBuf>,

    #[arg(long, default_value_t = 5)]
    pub k: u32,
    /// QueryVectors filter JSON, passed through verbatim.
    #[arg(long)]
    pub filter: Option<String>,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub return_metadata: bool,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub return_distance: bool,

    #[arg(long, value_enum, default_value = "json")]
    pub output: OutputFormat,
}

// ---------------------------------------------------------------------------

impl Cli {
    /// Initialise tracing. Precedence: `RUST_LOG` env > `--verbose` count >
    /// `--debug` > built-in `info` default.
    pub fn init_tracing(&self) {
        let filter = if let Ok(env) = std::env::var("RUST_LOG") {
            EnvFilter::new(env)
        } else {
            let level = match (self.debug, self.verbose) {
                (_, 2..) => "trace",
                (_, 1) | (true, _) => "debug",
                _ => "info",
            };
            EnvFilter::new(format!("{level},aws_smithy_runtime=warn,hyper_util=warn"))
        };
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}
