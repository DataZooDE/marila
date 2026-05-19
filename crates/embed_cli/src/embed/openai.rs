//! OpenAI `v1/embeddings` provider.
//!
//! Reads `OPENAI_API_KEY`. Default model `text-embedding-3-small`
//! (1536-d, dim-reducible). Cost telemetry is exact when present —
//! OpenAI returns `usage.prompt_tokens` in the response.
//!
//! Retries on 429 / 5xx with exponential backoff (jittered). Non-retry
//! 4xx propagate up so the caller sees the error body.

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::embed::{EmbedResponse, EmbeddingProvider, Usage};
use crate::retry::{self, Attempt, RetryPolicy};

const DEFAULT_MODEL: &str = "text-embedding-3-small";
const DEFAULT_DIM: u32 = 1536;
const ENDPOINT: &str = "https://api.openai.com/v1/embeddings";

pub struct OpenAiEmbedder {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dimension: u32,
    endpoint: String,
}

impl OpenAiEmbedder {
    pub fn from_env(model: Option<String>) -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY not set"))?;
        let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let dimension = guess_dimension(&model);
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dimension,
            endpoint: ENDPOINT.to_string(),
        })
    }

    /// Test seam — override the endpoint (and skip the env lookup).
    pub fn with_endpoint(api_key: String, model: String, dimension: u32, endpoint: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dimension,
            endpoint,
        }
    }
}

fn guess_dimension(model: &str) -> u32 {
    match model {
        "text-embedding-3-small" => 1536,
        "text-embedding-3-large" => 3072,
        "text-embedding-ada-002" => 1536,
        _ => DEFAULT_DIM,
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedder {
    fn name(&self) -> &str {
        "openai"
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
        8191
    }

    async fn embed(&self, inputs: &[&str]) -> anyhow::Result<EmbedResponse> {
        if inputs.is_empty() {
            return Ok(EmbedResponse::default());
        }
        let body = OpenAiEmbedRequest {
            model: &self.model,
            input: inputs,
        };
        let url = self.endpoint.clone();
        let policy = RetryPolicy::default();
        retry::with_backoff("openai/embeddings", policy, || async {
            let resp = match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return Attempt::Retry(anyhow::anyhow!("send: {e}")),
            };
            let status = resp.status();
            if status.is_success() {
                let parsed: OpenAiEmbedResponse = match resp.json().await {
                    Ok(p) => p,
                    Err(e) => return Attempt::Done(Err(anyhow::anyhow!("decode: {e}"))),
                };
                let mut vectors = Vec::with_capacity(parsed.data.len());
                for d in parsed.data {
                    vectors.push(d.embedding);
                }
                let usage = Usage {
                    tokens_in: parsed.usage.prompt_tokens as u64,
                    from_provider: true,
                };
                return Attempt::Done(Ok(EmbedResponse { vectors, usage }));
            }
            // Retry on transient codes; surface everything else.
            if is_transient(status) {
                let body = resp.text().await.unwrap_or_default();
                return Attempt::Retry(anyhow::anyhow!("{status}: {body}"));
            }
            let body = resp.text().await.unwrap_or_default();
            Attempt::Done(Err(anyhow::anyhow!("{status}: {body}")))
        })
        .await
    }
}

fn is_transient(s: StatusCode) -> bool {
    s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error()
}

#[derive(Serialize)]
struct OpenAiEmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct OpenAiEmbedResponse {
    data: Vec<OpenAiEmbedData>,
    usage: OpenAiUsage,
}

#[derive(Deserialize)]
struct OpenAiEmbedData {
    embedding: Vec<f32>,
    #[allow(dead_code)]
    index: usize,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    #[allow(dead_code)]
    total_tokens: u32,
}
