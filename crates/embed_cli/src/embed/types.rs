//! Shared embedding-side types. The trait itself lands in Phase 1.

use std::sync::Arc;

use async_trait::async_trait;

/// A vector embedding plus the upstream token count (if the provider
/// reports it). `tokens_in` is `None` when we have to estimate.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub vector: Vec<f32>,
}

/// Provider-reported usage for cost telemetry.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub tokens_in: u64,
    /// `true` if the provider returned the count, `false` if estimated.
    pub from_provider: bool,
}

/// The pluggable embedding provider. Implementations live in
/// sibling modules (`stub`, `openai`, `ollama`).
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn dimension(&self) -> u32;
    /// Provider's batch ceiling (e.g. 2048 for OpenAI).
    fn max_batch(&self) -> usize {
        100
    }
    /// Per-input token cap (e.g. 8191 for `text-embedding-3-small`).
    fn max_tokens_per_input(&self) -> usize {
        8192
    }

    async fn embed(&self, inputs: &[&str]) -> anyhow::Result<EmbedResponse>;
}

#[derive(Debug, Default, Clone)]
pub struct EmbedResponse {
    pub vectors: Vec<Vec<f32>>,
    pub usage: Usage,
}

pub type DynProvider = Arc<dyn EmbeddingProvider>;
