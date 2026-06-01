//! In-memory sink used by tests + `--dry-run`-style flows.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::sink::{EmbeddedChunk, Sink};

/// Thread-safe, clonable in-memory sink.
#[derive(Debug, Default, Clone)]
pub struct InMemorySink {
    inner: Arc<Mutex<Vec<EmbeddedChunk>>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn chunks(&self) -> Vec<EmbeddedChunk> {
        self.inner.lock().expect("sink mutex").clone()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("sink mutex").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl Sink for InMemorySink {
    async fn put(&self, chunks: &[EmbeddedChunk]) -> anyhow::Result<()> {
        self.inner
            .lock()
            .expect("sink mutex")
            .extend_from_slice(chunks);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accumulates_across_puts() {
        let sink = InMemorySink::new();
        let one = EmbeddedChunk {
            key: "k1".into(),
            vector: vec![0.0, 1.0],
            metadata: Default::default(),
        };
        let two = EmbeddedChunk {
            key: "k2".into(),
            vector: vec![1.0, 0.0],
            metadata: Default::default(),
        };
        sink.put(std::slice::from_ref(&one)).await.unwrap();
        sink.put(std::slice::from_ref(&two)).await.unwrap();
        let got = sink.chunks();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].key, "k1");
        assert_eq!(got[1].key, "k2");
    }
}
