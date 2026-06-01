//! PDF parser — `lopdf` text extraction, one page per call.
//!
//! CPU-bound; the pipeline parse pool already wraps `parse()` in
//! `spawn_blocking` so we don't need our own pool here.
//!
//! Pages are concatenated with form-feed (`\x0C`) separators so the
//! chunker (Phase 4) can keep page-aware citations if it wants to.
//!
//! **Per-page failures are silenced by default.** Many real-world PDFs
//! (especially older German parliamentary docs that embed TrueType
//! fonts without ToUnicode CMaps) can't be fully decoded by lopdf — at
//! parlis scale that's thousands of WARN lines per run. We log a single
//! DEBUG-level summary per document instead, and the pipeline-wide
//! `parse_failures` counter (visible in the final progress line) gives
//! the cumulative count. Re-enable per-page detail with
//! `RUST_LOG=marila_embed::parse::pdf=debug,lopdf=warn`.

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
        let doc =
            Document::load_mem(&raw.bytes).map_err(|e| anyhow::anyhow!("lopdf load_mem: {e}"))?;
        let pages = doc.get_pages();
        let total = pages.len();
        let mut text = String::new();
        let mut page_failures: u32 = 0;
        for (n, _) in pages {
            match doc.extract_text(&[n]) {
                Ok(t) => {
                    if !text.is_empty() {
                        text.push(PAGE_SEPARATOR);
                    }
                    text.push_str(&t);
                }
                Err(e) => {
                    page_failures += 1;
                    tracing::debug!(
                        source = %raw.source,
                        page = n,
                        error = %e,
                        "pdf page text extraction failed; continuing"
                    );
                }
            }
        }
        if page_failures > 0 {
            tracing::debug!(
                source = %raw.source,
                failed = page_failures,
                total_pages = total,
                "pdf page-extraction had failures (commonly missing ToUnicode CMap)"
            );
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
