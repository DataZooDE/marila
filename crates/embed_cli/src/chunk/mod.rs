//! Chunk stage — Chunker trait + dispatch over `--chunk-strategy`.

pub mod fixed;
pub mod types;

pub use types::*;

use crate::cli::ChunkStrategy;
use crate::parse::ParsedDoc;

/// Stateless chunker. Returns owned `Chunk`s so the channel doesn't
/// have to deal with lifetime gymnastics.
pub trait Chunker: Send + Sync {
    fn name(&self) -> &str;
    fn chunk(&self, doc: &ParsedDoc) -> Vec<Chunk>;
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub size: u32,
    pub overlap: u32,
}

/// Build the chunker for a CLI `--chunk-strategy` choice. `Off` returns
/// a chunker that emits the whole document as one chunk.
pub fn build(strategy: ChunkStrategy, cfg: ChunkConfig) -> Box<dyn Chunker> {
    match strategy {
        ChunkStrategy::Off => Box::new(fixed::WholeDocument),
        // Phase 3 only ships `fixed`; Phase 4 wires markdown + sentence.
        ChunkStrategy::Fixed | ChunkStrategy::Markdown | ChunkStrategy::Sentence => {
            Box::new(fixed::FixedChunker { cfg })
        }
    }
}
