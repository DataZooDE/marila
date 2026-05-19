//! Shared chunk-stage types.

use std::path::PathBuf;

use crate::parse::DocKind;

#[derive(Debug, Clone)]
pub struct Chunk {
    pub path: PathBuf,
    pub source: String,
    pub kind: DocKind,
    pub chunk_idx: u32,
    pub text: String,
    /// Heading anchors leading to this chunk, deepest-first
    /// (e.g. `["## Methodology", "### Test harness"]`). Populated only
    /// by structure-aware chunkers (Markdown in Phase 4).
    pub section_path: Vec<String>,
}
