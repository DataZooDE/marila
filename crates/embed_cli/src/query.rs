//! The `query` subcommand. Embeds a query string with the configured
//! provider, calls `QueryVectors`, renders the result as JSON (default)
//! or a small table.

use std::sync::Arc;

use anyhow::{Context, Result};
use aws_sdk_s3vectors::types::VectorData;
use aws_smithy_types::Document;
use tracing::debug;

use crate::cli::{EmbeddingProviderName, OutputFormat, QueryArgs};
use crate::embed::{
    EmbeddingProvider, ollama::OllamaEmbedder, openai::OpenAiEmbedder, stub::StubEmbedder,
};

pub async fn run(args: QueryArgs) -> Result<()> {
    let query_text = read_query(&args)?;
    let provider = build_provider(&args).await?;
    let client = crate::aws::vectors_client(&args.common).await;

    let resp = provider.embed(&[query_text.as_str()]).await?;
    let vec = resp
        .vectors
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;
    debug!(
        provider = provider.name(),
        dim = vec.len(),
        "embedded query"
    );

    let mut req = client
        .query_vectors()
        .vector_bucket_name(&args.common.vector_bucket_name)
        .index_name(&args.common.index_name)
        .top_k(args.k as i32)
        .query_vector(VectorData::Float32(vec))
        .return_distance(args.return_distance)
        .return_metadata(args.return_metadata);

    if let Some(filter) = args.filter.as_deref() {
        let parsed: serde_json::Value =
            serde_json::from_str(filter).context("--filter must be valid JSON")?;
        req = req.filter(json_to_document(parsed));
    }

    let out = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("QueryVectors: {e:?}"))?;

    let metric = out
        .distance_metric
        .as_ref()
        .map(|m| m.as_str())
        .unwrap_or("?")
        .to_string();
    let hits: Vec<Hit> = out
        .vectors()
        .iter()
        .map(|v| Hit {
            key: v.key().to_string(),
            distance: v.distance(),
            metadata: v.metadata().map(smithy_doc_to_json),
        })
        .collect();

    match args.output {
        OutputFormat::Json => render_json(&hits, &metric),
        OutputFormat::Table => render_table(&hits, &metric),
    }
    Ok(())
}

#[derive(Debug)]
struct Hit {
    key: String,
    distance: Option<f32>,
    metadata: Option<serde_json::Value>,
}

async fn build_provider(args: &QueryArgs) -> Result<Arc<dyn EmbeddingProvider>> {
    match args.common.embedding_provider {
        EmbeddingProviderName::Stub => Ok(Arc::new(StubEmbedder::new(
            args.common
                .embedding_model
                .as_deref()
                .and_then(|m| m.strip_prefix("stub-"))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(crate::embed::stub::DEFAULT_DIMENSION),
        ))),
        EmbeddingProviderName::Openai => Ok(Arc::new(OpenAiEmbedder::from_env(
            args.common.embedding_model.clone(),
        )?)),
        EmbeddingProviderName::Ollama => {
            let endpoint = std::env::var("OLLAMA_ENDPOINT").ok();
            Ok(Arc::new(
                OllamaEmbedder::connect(endpoint, args.common.embedding_model.clone()).await?,
            ))
        }
    }
}

fn read_query(args: &QueryArgs) -> Result<String> {
    if let Some(t) = args.text_value.as_deref() {
        return Ok(t.to_string());
    }
    if let Some(p) = args.text.as_deref() {
        return std::fs::read_to_string(p)
            .with_context(|| format!("read query file {}", p.display()));
    }
    anyhow::bail!("either --text-value or --text must be set")
}

fn render_json(hits: &[Hit], metric: &str) {
    let body = serde_json::json!({
        "distanceMetric": metric,
        "vectors": hits.iter().map(|h| {
            let mut o = serde_json::Map::new();
            o.insert("key".into(), serde_json::Value::String(h.key.clone()));
            if let Some(d) = h.distance {
                o.insert(
                    "distance".into(),
                    serde_json::Number::from_f64(d as f64)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            if let Some(m) = &h.metadata {
                o.insert("metadata".into(), m.clone());
            }
            serde_json::Value::Object(o)
        }).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&body).unwrap());
}

fn render_table(hits: &[Hit], metric: &str) {
    println!("distance metric: {metric}");
    println!("{:>4}  {:>10}  {:>32}  snippet", "#", "distance", "key");
    for (i, h) in hits.iter().enumerate() {
        let dist = h
            .distance
            .map(|d| format!("{d:.4}"))
            .unwrap_or_else(|| "-".into());
        let snippet = h
            .metadata
            .as_ref()
            .and_then(|m| m.get("S3VECTORS-EMBED-SRC-CONTENT"))
            .and_then(|v| v.as_str())
            .map(|s| {
                let mut out = String::new();
                for c in s.chars().take(60) {
                    if c == '\n' || c == '\r' {
                        out.push(' ');
                    } else {
                        out.push(c);
                    }
                }
                if s.chars().count() > 60 {
                    out.push('…');
                }
                out
            })
            .unwrap_or_default();
        println!("{i:>4}  {dist:>10}  {:>32}  {snippet}", h.key);
    }
}

fn json_to_document(v: serde_json::Value) -> Document {
    use serde_json::Value;
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else {
                Document::Number(aws_smithy_types::Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s),
        Value::Array(arr) => Document::Array(arr.into_iter().map(json_to_document).collect()),
        Value::Object(obj) => Document::Object(
            obj.into_iter()
                .map(|(k, v)| (k, json_to_document(v)))
                .collect(),
        ),
    }
}

fn smithy_doc_to_json(d: &Document) -> serde_json::Value {
    use aws_smithy_types::Number;
    use serde_json::Value;
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => Value::Number((*u).into()),
            Number::NegInt(i) => Value::Number((*i).into()),
            Number::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        },
        Document::String(s) => Value::String(s.clone()),
        Document::Array(arr) => Value::Array(arr.iter().map(smithy_doc_to_json).collect()),
        Document::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), smithy_doc_to_json(v)))
                .collect(),
        ),
    }
}
