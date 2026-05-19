//! Token counting. Phase 3 ships a conservative char-based estimate;
//! Phase 6 wires `tiktoken-rs` when the provider is OpenAI.

/// Conservative character-based estimate — assumes 4 chars per token
/// (English mean). Always returns at least 1 for non-empty input so a
/// per-chunk loop can't divide by zero.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        ((text.chars().count() + 3) / 4).max(1)
    }
}
