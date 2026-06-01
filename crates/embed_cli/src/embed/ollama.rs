//! Local Ollama `/api/embed` provider.
//!
//! Default model `embeddinggemma:latest` (768-d, multilingual,
//! Matryoshka-reducible). Dimension is auto-probed on construction so
//! the auto-create-index step gets the right CreateIndex shape.
//!
//! Ollama doesn't report token counts, so we estimate via `chars / 4`
//! and mark `Usage::from_provider = false`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::embed::{EmbedResponse, EmbeddingProvider, Usage};
use crate::retry::{self, Attempt, RetryPolicy};

pub const DEFAULT_MODEL: &str = "embeddinggemma:latest";
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

pub struct OllamaEmbedder {
    client: reqwest::Client,
    model: String,
    dimension: u32,
    endpoint: String,
}

impl OllamaEmbedder {
    /// Probe the dimension by embedding a single space and inspecting
    /// the resulting vector length. One round-trip at construction
    /// avoids forcing the caller to hard-code per-model dims.
    pub async fn connect(endpoint: Option<String>, model: Option<String>) -> anyhow::Result<Self> {
        let endpoint = endpoint.unwrap_or_else(|| {
            std::env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into())
        });
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.into());
        let client = reqwest::Client::new();
        let dimension = probe_dimension(&client, &endpoint, &model).await?;
        Ok(Self {
            client,
            model,
            dimension,
            endpoint,
        })
    }

    pub fn with_known_dim(endpoint: String, model: String, dimension: u32) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            model,
            dimension,
        }
    }
}

async fn probe_dimension(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
) -> anyhow::Result<u32> {
    let url = format!("{endpoint}/api/embed");
    let resp = client
        .post(&url)
        .json(&OllamaEmbedRequest {
            model,
            input: OllamaInput::Single(" "),
        })
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("ollama probe: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "ollama probe {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    let body: OllamaEmbedResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("ollama probe decode: {e}"))?;
    let first = body
        .embeddings
        .first()
        .ok_or_else(|| anyhow::anyhow!("ollama probe returned no embedding"))?;
    Ok(first.len() as u32)
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbedder {
    fn name(&self) -> &str {
        "ollama"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn dimension(&self) -> u32 {
        self.dimension
    }
    fn max_batch(&self) -> usize {
        // Ollama doesn't publish a hard cap; keep it modest to bound
        // worker memory.
        256
    }
    fn max_tokens_per_input(&self) -> usize {
        // embeddinggemma's context window is 2048 tokens. Other Ollama
        // embedding models tend to be 512..=8192. The pipeline's
        // pre-embed validation is char-based so this is just advisory.
        8192
    }

    async fn embed(&self, inputs: &[&str]) -> anyhow::Result<EmbedResponse> {
        if inputs.is_empty() {
            return Ok(EmbedResponse::default());
        }
        let url = format!("{}/api/embed", self.endpoint);
        let body = OllamaEmbedRequest {
            model: &self.model,
            input: OllamaInput::Batch(inputs),
        };
        retry::with_backoff("ollama/embed", RetryPolicy::default(), || async {
            let resp = match self.client.post(&url).json(&body).send().await {
                Ok(r) => r,
                Err(e) => return Attempt::Retry(anyhow::anyhow!("send: {e}")),
            };
            let status = resp.status();
            if status.is_success() {
                let parsed: OllamaEmbedResponse = match resp.json().await {
                    Ok(p) => p,
                    Err(e) => return Attempt::Done(Err(anyhow::anyhow!("decode: {e}"))),
                };
                let tokens_in: u64 = inputs
                    .iter()
                    .map(|t| (t.chars().count() as u64).div_ceil(4))
                    .sum();
                return Attempt::Done(Ok(EmbedResponse {
                    vectors: parsed.embeddings,
                    usage: Usage {
                        tokens_in,
                        from_provider: false,
                    },
                }));
            }
            if status.is_server_error() {
                let body = resp.text().await.unwrap_or_default();
                return Attempt::Retry(anyhow::anyhow!("{status}: {body}"));
            }
            let body = resp.text().await.unwrap_or_default();
            Attempt::Done(Err(anyhow::anyhow!("{status}: {body}")))
        })
        .await
    }
}

#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: OllamaInput<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum OllamaInput<'a> {
    Single(&'a str),
    Batch(&'a [&'a str]),
}

#[derive(Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}
