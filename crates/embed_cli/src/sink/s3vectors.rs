//! `aws-sdk-s3vectors`-backed [`Sink`].
//!
//! Calls `PutVectors` for each batch handed to [`Sink::put`]. Auto-creates
//! the index on first use if [`Self::with_auto_create`] is set.
//!
//! Metadata Smithy-shape conversion is here so the rest of the CLI can
//! deal in `serde_json::Value`.

use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{DataType, DistanceMetric, PutInputVector, VectorData};
use aws_smithy_types::Document;
use tracing::{debug, info};

use crate::sink::{EmbeddedChunk, Sink};

pub struct S3VectorsSink {
    client: Client,
    bucket: String,
    index: String,
    auto_create_dim: Option<u32>,
    /// Guards the one-time CreateIndex probe so concurrent puts don't
    /// stampede the catalog.
    init: tokio::sync::OnceCell<()>,
}

impl S3VectorsSink {
    pub fn new(client: Client, bucket: impl Into<String>, index: impl Into<String>) -> Self {
        Self {
            client,
            bucket: bucket.into(),
            index: index.into(),
            auto_create_dim: None,
            init: tokio::sync::OnceCell::new(),
        }
    }

    /// Enable auto-create-index: on first put, probe the index and
    /// `CreateIndex` (Float32, Cosine, the given dimension) if absent.
    pub fn with_auto_create(mut self, dimension: u32) -> Self {
        self.auto_create_dim = Some(dimension);
        self
    }

    pub fn into_arc(self) -> Arc<dyn Sink> {
        Arc::new(self)
    }

    async fn ensure_index(&self) -> anyhow::Result<()> {
        if let Some(dim) = self.auto_create_dim {
            self.init
                .get_or_try_init(|| async {
                    if self.index_exists().await? {
                        debug!(index = %self.index, "index already present");
                        return Ok::<(), anyhow::Error>(());
                    }
                    info!(
                        index = %self.index,
                        bucket = %self.bucket,
                        dim,
                        "creating missing index (auto-create)"
                    );
                    self.client
                        .create_index()
                        .vector_bucket_name(&self.bucket)
                        .index_name(&self.index)
                        .data_type(DataType::Float32)
                        .dimension(dim as i32)
                        .distance_metric(DistanceMetric::Cosine)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("CreateIndex: {}", display_err(&e)))?;
                    Ok(())
                })
                .await
                .map_err(|e| anyhow::anyhow!("ensure_index: {e}"))?;
        }
        Ok(())
    }

    async fn index_exists(&self) -> anyhow::Result<bool> {
        match self
            .client
            .get_index()
            .vector_bucket_name(&self.bucket)
            .index_name(&self.index)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) => {
                let txt = display_err(&e);
                if txt.contains("NotFound") || txt.contains("not be found") {
                    Ok(false)
                } else {
                    Err(anyhow::anyhow!("GetIndex: {txt}"))
                }
            }
        }
    }
}

#[async_trait]
impl Sink for S3VectorsSink {
    async fn put(&self, chunks: &[EmbeddedChunk]) -> anyhow::Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }
        self.ensure_index().await?;

        // PutVectors is hard-capped at 500 per call. Slice if needed.
        for batch in chunks.chunks(500) {
            let mut req = self
                .client
                .put_vectors()
                .vector_bucket_name(&self.bucket)
                .index_name(&self.index);
            for c in batch {
                req = req.vectors(to_put_input(c)?);
            }
            req.send()
                .await
                .map_err(|e| anyhow::anyhow!("PutVectors: {}", display_err(&e)))?;
        }
        Ok(())
    }
}

fn to_put_input(c: &EmbeddedChunk) -> anyhow::Result<PutInputVector> {
    let mut b = PutInputVector::builder()
        .key(&c.key)
        .data(VectorData::Float32(c.vector.clone()));
    if !c.metadata.is_empty() {
        let doc = json_to_document(serde_json::Value::Object(
            c.metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ));
        b = b.metadata(doc);
    }
    Ok(b.build()?)
}

/// AWS S3 Vectors requires `metadata` to be a Smithy `Document::Object`,
/// not a JSON string. Mirrors the conversion in
/// `crates/integration_tests/tests/data_plane.rs`.
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

/// SDK errors don't have a useful Display; this digs out the service
/// error message so logs / our `bail!` strings carry the AWS-shape body.
fn display_err<E: std::fmt::Debug>(e: &E) -> String {
    format!("{e:?}")
}
