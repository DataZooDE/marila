//! Deterministic, network-free embedding provider for tests.
//!
//! `vector[i] = (blake3(text)[i % 32] as f32) / 255.0`, then L2-normalised.
//! Same text in → same vector out, across runs, machines, and crate
//! versions. The dimension is configurable so tests can probe both the
//! 768-d (Gemma-like) and 1536-d (text-embedding-3-small-like) shapes
//! without paying for a real provider.

use async_trait::async_trait;

use crate::embed::{EmbedResponse, EmbeddingProvider, Usage};

pub const DEFAULT_DIMENSION: u32 = 768;

#[derive(Debug, Clone)]
pub struct StubEmbedder {
    model: String,
    dimension: u32,
}

impl StubEmbedder {
    pub fn new(dimension: u32) -> Self {
        Self {
            model: format!("stub-{dimension}"),
            dimension,
        }
    }
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_DIMENSION)
    }
}

fn embed_one(text: &str, dimension: u32) -> Vec<f32> {
    let hash = blake3::hash(text.as_bytes());
    let bytes = hash.as_bytes(); // 32 bytes
    let mut v = Vec::with_capacity(dimension as usize);
    for i in 0..dimension as usize {
        v.push(bytes[i % bytes.len()] as f32 / 255.0);
    }
    // L2-normalise so cosine distance works.
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[async_trait]
impl EmbeddingProvider for StubEmbedder {
    fn name(&self) -> &str {
        "stub"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn dimension(&self) -> u32 {
        self.dimension
    }
    fn max_batch(&self) -> usize {
        2048
    }
    fn max_tokens_per_input(&self) -> usize {
        usize::MAX
    }

    async fn embed(&self, inputs: &[&str]) -> anyhow::Result<EmbedResponse> {
        let vectors: Vec<Vec<f32>> = inputs.iter().map(|t| embed_one(t, self.dimension)).collect();
        let estimated_tokens: u64 = inputs.iter().map(|t| (t.len() as u64).div_ceil(4)).sum();
        Ok(EmbedResponse {
            vectors,
            usage: Usage {
                tokens_in: estimated_tokens,
                from_provider: false,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_input_same_vector() {
        let e = StubEmbedder::new(64);
        let a = e.embed(&["hello"]).await.unwrap();
        let b = e.embed(&["hello"]).await.unwrap();
        assert_eq!(a.vectors, b.vectors);
        assert_eq!(a.vectors[0].len(), 64);
    }

    #[tokio::test]
    async fn different_input_different_vector() {
        let e = StubEmbedder::default();
        let a = e.embed(&["hello"]).await.unwrap();
        let b = e.embed(&["world"]).await.unwrap();
        assert_ne!(a.vectors[0], b.vectors[0]);
    }

    #[tokio::test]
    async fn batch_preserves_order() {
        let e = StubEmbedder::new(32);
        let solo_a = e.embed(&["a"]).await.unwrap();
        let solo_b = e.embed(&["b"]).await.unwrap();
        let solo_c = e.embed(&["c"]).await.unwrap();
        let batch = e.embed(&["a", "b", "c"]).await.unwrap();
        assert_eq!(batch.vectors[0], solo_a.vectors[0]);
        assert_eq!(batch.vectors[1], solo_b.vectors[0]);
        assert_eq!(batch.vectors[2], solo_c.vectors[0]);
    }

    #[tokio::test]
    async fn vectors_are_unit_length() {
        let e = StubEmbedder::new(128);
        let r = e.embed(&["any text at all"]).await.unwrap();
        let n: f32 = r.vectors[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5, "expected unit-norm, got {n}");
    }
}
