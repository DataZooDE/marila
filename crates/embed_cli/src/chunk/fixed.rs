//! Fixed-window chunker — split into N-token-ish windows with overlap.
//!
//! v0 estimates tokens as `ceil(chars / 4)`. Real `tiktoken-rs` wiring
//! lands in Phase 6 when the OpenAI provider goes in.

use unicode_segmentation::UnicodeSegmentation;

use crate::chunk::{Chunk, ChunkConfig, Chunker};
use crate::parse::ParsedDoc;

pub struct FixedChunker {
    pub cfg: ChunkConfig,
}

/// Used by `--chunk-strategy off` — emits the whole document as one chunk.
pub struct WholeDocument;

impl Chunker for WholeDocument {
    fn name(&self) -> &str {
        "off"
    }
    fn chunk(&self, doc: &ParsedDoc) -> Vec<Chunk> {
        if doc.text.is_empty() {
            return Vec::new();
        }
        vec![Chunk {
            path: doc.path.clone(),
            source: doc.source.clone(),
            kind: doc.kind,
            chunk_idx: 0,
            text: doc.text.clone(),
            section_path: Vec::new(),
        }]
    }
}

impl Chunker for FixedChunker {
    fn name(&self) -> &str {
        "fixed"
    }

    fn chunk(&self, doc: &ParsedDoc) -> Vec<Chunk> {
        if doc.text.is_empty() {
            return Vec::new();
        }
        let size_chars = self.cfg.size as usize * 4;
        let overlap_chars = self.cfg.overlap as usize * 4;
        let step = size_chars.saturating_sub(overlap_chars).max(1);

        // Word-aware boundaries: split on word edges so we don't slice
        // mid-token. Cheap O(n) over the document.
        let words: Vec<(usize, &str)> = doc.text.unicode_word_indices().collect();
        if words.is_empty() {
            return vec![Chunk {
                path: doc.path.clone(),
                source: doc.source.clone(),
                kind: doc.kind,
                chunk_idx: 0,
                text: doc.text.clone(),
                section_path: Vec::new(),
            }];
        }

        let mut chunks = Vec::new();
        let mut idx: u32 = 0;
        let mut cursor = 0usize; // byte offset where the current window starts
        let text_len = doc.text.len();

        while cursor < text_len {
            let end = clamp_word_end(&doc.text, cursor, size_chars);
            let slice = &doc.text[cursor..end];
            let trimmed = slice.trim();
            if !trimmed.is_empty() {
                chunks.push(Chunk {
                    path: doc.path.clone(),
                    source: doc.source.clone(),
                    kind: doc.kind,
                    chunk_idx: idx,
                    text: trimmed.to_string(),
                    section_path: Vec::new(),
                });
                idx += 1;
            }
            if end >= text_len {
                break;
            }
            let advance = step;
            cursor = clamp_word_start(&doc.text, cursor + advance);
        }

        chunks
    }
}

/// Find the largest valid byte-offset that's no more than `max_chars`
/// chars from `start`, ending on a UTF-8 char boundary.
fn clamp_word_end(text: &str, start: usize, max_chars: usize) -> usize {
    let mut end = start;
    let mut chars = 0;
    for (i, _) in text[start..].char_indices() {
        if chars >= max_chars {
            break;
        }
        end = start + i;
        chars += 1;
    }
    // Walk forward to the end of the last char we counted.
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    if chars >= max_chars {
        // Try to step back to a whitespace boundary so we don't split a
        // word, but don't go more than 32 chars back.
        let cap = end;
        let lower = cap.saturating_sub(32 * 4);
        let mut back = cap;
        while back > lower {
            if back <= text.len() && text.is_char_boundary(back) {
                if let Some(c) = text[back..].chars().next() {
                    if c.is_whitespace() {
                        return back;
                    }
                }
            }
            back -= 1;
        }
    }
    text.len().min(start.saturating_add(text[start..].len()).min(end + max_chars).max(end))
}

/// Snap forward to a char boundary (and skip leading whitespace).
fn clamp_word_start(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    while i < text.len() {
        if let Some(c) = text[i..].chars().next() {
            if c.is_whitespace() {
                i += c.len_utf8();
                continue;
            }
        }
        break;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{DocKind, ParsedDoc};

    fn doc(text: &str) -> ParsedDoc {
        ParsedDoc {
            path: "x.txt".into(),
            source: "x.txt".into(),
            kind: DocKind::Text,
            text: text.to_string(),
            sections: Vec::new(),
            content_hash: "test".into(),
        }
    }

    #[test]
    fn whole_document_emits_one_chunk() {
        let chunks = WholeDocument.chunk(&doc("hello world"));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
    }

    #[test]
    fn fixed_chunker_produces_multiple_chunks_with_overlap() {
        let body = "lorem ipsum ".repeat(800); // ~9600 chars, ~2400 tokens
        let c = FixedChunker {
            cfg: ChunkConfig { size: 100, overlap: 20 },
        };
        let chunks = c.chunk(&doc(&body));
        assert!(chunks.len() >= 5, "expected several chunks, got {}", chunks.len());
        // chunk_idx is monotonic and starts at 0
        for (i, ch) in chunks.iter().enumerate() {
            assert_eq!(ch.chunk_idx as usize, i);
            assert!(!ch.text.is_empty());
        }
    }

    #[test]
    fn empty_text_emits_no_chunks() {
        let c = FixedChunker {
            cfg: ChunkConfig { size: 100, overlap: 20 },
        };
        assert!(c.chunk(&doc("")).is_empty());
    }
}
