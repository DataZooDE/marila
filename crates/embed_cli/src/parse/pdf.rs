//! PDF parser — `lopdf` text extraction, one page per call.
//!
//! CPU-bound; the pipeline parse pool already wraps `parse()` in
//! `spawn_blocking` so we don't need our own pool here.
//!
//! Pages are concatenated with form-feed (`\x0C`) separators so the
//! chunker (Phase 4) can keep page-aware citations if it wants to.

use lopdf::Document;

use crate::parse::{DocKind, ParsedDoc, Parser};
use crate::source::RawDoc;

pub struct PdfParser;

const PAGE_SEPARATOR: char = '\x0C';

impl Parser for PdfParser {
    fn name(&self) -> &str {
        "pdf"
    }
    fn extensions(&self) -> &[&str] {
        &["pdf"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let doc = Document::load_mem(&raw.bytes)
            .map_err(|e| anyhow::anyhow!("lopdf load_mem: {e}"))?;
        let pages = doc.get_pages();
        let mut text = String::new();
        for (n, _) in pages {
            match doc.extract_text(&[n]) {
                Ok(t) => {
                    if !text.is_empty() {
                        text.push(PAGE_SEPARATOR);
                    }
                    text.push_str(&t);
                }
                Err(e) => {
                    tracing::warn!(
                        source = %raw.source,
                        page = n,
                        error = %e,
                        "pdf page text extraction failed; continuing"
                    );
                }
            }
        }
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Pdf,
            text,
            sections: Vec::new(),
            content_hash: raw.content_hash,
        })
    }
}
