//! `marila-embed` — streaming ingestion CLI for marila's S3 Vectors façade.
//!
//! The crate is laid out as the pipeline diagram in
//! [`doc/EMBED_CLI_SPEC.md`](../../doc/EMBED_CLI_SPEC.md) §3:
//!
//! ```text
//!   Source ─▶ Parse ─▶ Chunk ─▶ Embed ─▶ Put
//! ```
//!
//! Each stage is its own module. The `pipeline` module wires them via
//! bounded `tokio::sync::mpsc` channels so steady-state memory is
//! independent of corpus size.
//!
//! Everything that follows is built incrementally per the plan in
//! `/home/jr/.claude/plans/start-bootstrapping-this-new-elegant-bentley.md`.

pub mod cli;
pub mod config;
pub mod embed;
pub mod sink;
