//! Markdown-aware chunker.
//!
//! Splits the document at heading boundaries and packs the content of
//! each section into chunks ≤ `chunk_size * 4` chars. When a section is
//! larger than the cap, it's split with the same word-aware logic as the
//! fixed chunker so we don't lose any text.
//!
//! Each chunk carries the `section_path` (list of heading titles leading
//! to it, deepest-first as the spec phrases it; here outer-to-inner so
//! the path reads top-down).

use crate::chunk::{Chunk, ChunkConfig, Chunker, fixed::FixedChunker};
use crate::parse::{DocKind, ParsedDoc, Section};

pub struct MarkdownChunker {
    pub cfg: ChunkConfig,
}

impl Chunker for MarkdownChunker {
    fn name(&self) -> &str {
        "markdown"
    }

    fn chunk(&self, doc: &ParsedDoc) -> Vec<Chunk> {
        if doc.text.is_empty() {
            return Vec::new();
        }

        // Fall back to fixed if the parser didn't extract structure.
        if doc.sections.is_empty() {
            return FixedChunker { cfg: self.cfg }.chunk(doc);
        }

        let regions = split_by_headings(&doc.text, &doc.sections);
        let max_chars = self.cfg.size as usize * 4;

        let mut out: Vec<Chunk> = Vec::new();
        let mut next_idx: u32 = 0;
        let mut path_stack: Vec<(u8, String)> = Vec::new();

        for region in regions {
            if let Some(s) = &region.heading {
                while path_stack.last().is_some_and(|(lvl, _)| *lvl >= s.level) {
                    path_stack.pop();
                }
                path_stack.push((s.level, s.title.clone()));
            }
            let section_path: Vec<String> =
                path_stack.iter().map(|(_, t)| t.clone()).collect();

            let body = &doc.text[region.start..region.end];
            let body = body.trim();
            if body.is_empty() {
                continue;
            }

            if body.len() <= max_chars {
                out.push(Chunk {
                    path: doc.path.clone(),
                    source: doc.source.clone(),
                    kind: DocKind::Markdown,
                    chunk_idx: next_idx,
                    text: body.to_string(),
                    section_path,
                });
                next_idx += 1;
            } else {
                // Section is too big — split with the fixed chunker but
                // keep the section_path on every piece.
                let synthetic = ParsedDoc {
                    path: doc.path.clone(),
                    source: doc.source.clone(),
                    kind: DocKind::Markdown,
                    text: body.to_string(),
                    sections: Vec::new(),
                    content_hash: doc.content_hash.clone(),
                };
                let inner = FixedChunker { cfg: self.cfg }.chunk(&synthetic);
                for mut piece in inner {
                    piece.chunk_idx = next_idx;
                    piece.section_path = section_path.clone();
                    out.push(piece);
                    next_idx += 1;
                }
            }
        }
        out
    }
}

struct Region<'a> {
    heading: Option<&'a Section>,
    start: usize,
    end: usize,
}

fn split_by_headings<'a>(text: &str, sections: &'a [Section]) -> Vec<Region<'a>> {
    let mut out = Vec::with_capacity(sections.len() + 1);

    // Pre-amble before the first heading, if any.
    let first = sections.first();
    let preamble_end = first.map(|s| s.start_byte).unwrap_or(text.len());
    if preamble_end > 0 {
        out.push(Region {
            heading: None,
            start: 0,
            end: preamble_end,
        });
    }

    for (i, s) in sections.iter().enumerate() {
        let end = sections
            .get(i + 1)
            .map(|n| n.start_byte)
            .unwrap_or(text.len());
        out.push(Region {
            heading: Some(s),
            start: s.start_byte,
            end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Parser, markdown::MarkdownParser};
    use crate::source::RawDoc;

    fn parse(md: &str) -> ParsedDoc {
        let p = MarkdownParser;
        p.parse(RawDoc {
            path: "x.md".into(),
            source: "x.md".into(),
            ext: "md".into(),
            bytes: md.as_bytes().to_vec(),
            content_hash: "test".into(),
        })
        .unwrap()
    }

    #[test]
    fn chunks_inherit_section_path() {
        let doc = parse(
            "# Top\n\nintro\n\n## Sub A\n\nbody A\n\n## Sub B\n\nbody B\n",
        );
        let chunks = MarkdownChunker {
            cfg: ChunkConfig { size: 1000, overlap: 0 },
        }
        .chunk(&doc);
        assert!(chunks.iter().any(|c| c.section_path == vec!["Top"]));
        assert!(chunks
            .iter()
            .any(|c| c.section_path == vec!["Top", "Sub A"]));
        assert!(chunks
            .iter()
            .any(|c| c.section_path == vec!["Top", "Sub B"]));
    }

    #[test]
    fn big_section_is_subchunked_with_same_path() {
        let big = "lorem ipsum dolor sit amet ".repeat(200);
        let md = format!("# T\n\n## S\n\n{big}");
        let doc = parse(&md);
        let chunks = MarkdownChunker {
            cfg: ChunkConfig { size: 50, overlap: 10 },
        }
        .chunk(&doc);
        let s_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.section_path == vec!["T", "S"])
            .collect();
        assert!(s_chunks.len() > 1, "got {} chunks under T>S", s_chunks.len());
    }
}
