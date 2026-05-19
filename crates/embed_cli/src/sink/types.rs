//! Shared sink-side types.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

/// One vector ready to be persisted.
#[derive(Debug, Clone)]
pub struct EmbeddedChunk {
    pub key: String,
    pub vector: Vec<f32>,
    pub metadata: BTreeMap<String, Value>,
}

/// Where vectors go. `s3vectors` for production; `in_memory` for tests.
#[async_trait]
pub trait Sink: Send + Sync {
    /// Persist a batch of vectors. Implementations decide their own
    /// retry / batching policy under the hood.
    async fn put(&self, chunks: &[EmbeddedChunk]) -> anyhow::Result<()>;
}
