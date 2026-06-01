//! Sentence-aware chunker — packs whole sentences into chunks of
//! ≤ size tokens. Uses `unicode-segmentation` sentence boundaries so
//! punctuation in non-Latin scripts isn't accidentally ignored.

use unicode_segmentation::UnicodeSegmentation;

use crate::chunk::{Chunk, ChunkConfig, Chunker};
use crate::parse::ParsedDoc;

pub struct SentenceChunker {
    pub cfg: ChunkConfig,
}

impl Chunker for SentenceChunker {
    fn name(&self) -> &str {
        "sentence"
    }

    fn chunk(&self, doc: &ParsedDoc) -> Vec<Chunk> {
        if doc.text.is_empty() {
            return Vec::new();
        }
        let max_chars = self.cfg.size as usize * 4;
        let mut out = Vec::new();
        let mut buf = String::with_capacity(max_chars);
        let mut idx: u32 = 0;

        let sentences: Vec<&str> = doc.text.unicode_sentences().collect();
        for s in sentences {
            // If buf is too full to take s, flush.
            if !buf.is_empty() && buf.len() + s.len() > max_chars {
                push_chunk(&mut out, doc, &mut idx, &buf);
                buf.clear();
            }
            // Sentence on its own larger than the cap: split it.
            if s.len() > max_chars {
                if !buf.is_empty() {
                    push_chunk(&mut out, doc, &mut idx, &buf);
                    buf.clear();
                }
                for slice in hard_split(s, max_chars) {
                    push_chunk(&mut out, doc, &mut idx, slice);
                }
            } else {
                buf.push_str(s);
            }
        }
        if !buf.trim().is_empty() {
            push_chunk(&mut out, doc, &mut idx, &buf);
        }
        out
    }
}

fn push_chunk(out: &mut Vec<Chunk>, doc: &ParsedDoc, idx: &mut u32, body: &str) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }
    out.push(Chunk {
        path: doc.path.clone(),
        source: doc.source.clone(),
        kind: doc.kind,
        chunk_idx: *idx,
        text: trimmed.to_string(),
        section_path: Vec::new(),
    });
    *idx += 1;
}

fn hard_split(s: &str, max_chars: usize) -> Vec<&str> {
    let mut v = Vec::new();
    let mut start = 0usize;
    let mut chars = 0usize;
    for (i, _) in s.char_indices() {
        if chars >= max_chars {
            v.push(&s[start..i]);
            start = i;
            chars = 0;
        }
        chars += 1;
    }
    if start < s.len() {
        v.push(&s[start..]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::DocKind;

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
    fn packs_sentences_until_cap() {
        let body = "Alpha bravo. Charlie delta echo. Foxtrot golf. Hotel india juliet kilo. ";
        let chunks = SentenceChunker {
            cfg: ChunkConfig {
                size: 8,
                overlap: 0,
            }, // ~32 chars cap
        }
        .chunk(&doc(body));
        assert!(chunks.len() >= 2, "got {} chunks", chunks.len());
        for c in &chunks {
            assert!(c.text.len() <= 32 + 1, "chunk too big: {:?}", c.text);
        }
    }

    #[test]
    fn handles_giant_single_sentence() {
        let body = "a".repeat(500);
        let chunks = SentenceChunker {
            cfg: ChunkConfig {
                size: 50,
                overlap: 0,
            }, // 200 chars
        }
        .chunk(&doc(&body));
        assert!(chunks.len() >= 2);
        let joined: String = chunks.iter().map(|c| c.text.clone()).collect();
        assert_eq!(joined.len(), 500);
    }
}
