//! Token counting. Two backends:
//!   - `tiktoken-rs` for OpenAI (exact)
//!   - char/4 heuristic for everything else (conservative)
//!
//! The interface is a single [`TokenCounter`] trait so the embed and
//! chunk stages don't have to care which one they got.

use std::sync::Arc;

pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> usize;
    /// Human-readable name for telemetry ("cl100k_base", "estimate-4chars").
    fn name(&self) -> &str;
}

/// Conservative char-based estimate — `ceil(chars / 4)`. Always returns
/// at least 1 for non-empty input.
pub struct CharEstimate;

impl TokenCounter for CharEstimate {
    fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            0
        } else {
            text.chars().count().div_ceil(4).max(1)
        }
    }
    fn name(&self) -> &str {
        "estimate-4chars"
    }
}

/// `tiktoken-rs`-backed counter — exact OpenAI tokenisation.
pub struct TiktokenCounter {
    bpe: tiktoken_rs::CoreBPE,
    label: &'static str,
}

impl TiktokenCounter {
    /// Pick the encoding for an OpenAI model name (cl100k_base for the
    /// embedding-3 family + GPT-4; o200k_base for newer models). Falls
    /// back to `cl100k_base` for any unknown name since both embedding
    /// models use it.
    pub fn for_model(model: &str) -> anyhow::Result<Self> {
        let (label, bpe) = if model.contains("o200k") {
            ("o200k_base", tiktoken_rs::o200k_base()?)
        } else {
            ("cl100k_base", tiktoken_rs::cl100k_base()?)
        };
        Ok(Self { bpe, label })
    }
}

impl TokenCounter for TiktokenCounter {
    fn count(&self, text: &str) -> usize {
        self.bpe.encode_with_special_tokens(text).len()
    }
    fn name(&self) -> &str {
        self.label
    }
}

/// Construct the right counter for a provider name. Always succeeds —
/// if tiktoken-rs init fails (it shouldn't), we silently downgrade
/// to the char estimate.
pub fn for_provider(provider: &str, model: Option<&str>) -> Arc<dyn TokenCounter> {
    if provider == "openai"
        && let Some(m) = model
        && let Ok(c) = TiktokenCounter::for_model(m)
    {
        return Arc::new(c);
    }
    Arc::new(CharEstimate)
}

/// Convenience helper — kept for back-compat with Phase 3 callers.
pub fn estimate_tokens(text: &str) -> usize {
    CharEstimate.count(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_estimate_basic() {
        assert_eq!(CharEstimate.count(""), 0);
        assert_eq!(CharEstimate.count("a"), 1);
        assert_eq!(CharEstimate.count("abcd"), 1);
        assert_eq!(CharEstimate.count("abcde"), 2);
    }

    #[test]
    fn tiktoken_counts_simple_string() {
        let t = TiktokenCounter::for_model("text-embedding-3-small").unwrap();
        // "hello world" is 2 BPE tokens under cl100k_base.
        assert_eq!(t.count("hello world"), 2);
    }
}
