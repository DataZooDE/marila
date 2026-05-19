//! Layered config: CLI flags > env vars > TOML file > built-in defaults.
//!
//! Phase 0 stub — the parser is here, the merge into [`crate::cli::PutArgs`]
//! lands in a later phase. Keeping the type wired now means later phases
//! don't have to invent a load path.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    pub endpoint_url: Option<String>,
    pub vector_bucket: Option<String>,
    pub index_name: Option<String>,
    pub embed: Option<EmbedSection>,
    pub chunk: Option<ChunkSection>,
    pub put: Option<PutSection>,
}

#[derive(Debug, Default, Deserialize)]
pub struct EmbedSection {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub concurrency: Option<usize>,
    pub batch: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChunkSection {
    pub strategy: Option<String>,
    pub size: Option<u32>,
    pub overlap: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PutSection {
    pub concurrency: Option<usize>,
    pub batch: Option<usize>,
}

impl FileConfig {
    /// Load + parse `path`; returns `Ok(default)` if the file does not
    /// exist (config files are optional).
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parse config {}", path.display()))
    }
}
