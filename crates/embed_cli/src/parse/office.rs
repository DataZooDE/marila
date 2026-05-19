//! Office-suite parser. Handles `.docx`, `.pptx`, `.odt`, `.odp` —
//! all of which are zip archives with XML inside. One dispatch table
//! per family picks the entries that hold the user-visible text.
//!
//! Robust by design: missing entries log at WARN and contribute the
//! empty string rather than failing the parse — degraded text is more
//! useful than no text.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::parse::{DocKind, ParsedDoc, Parser};
use crate::source::RawDoc;

pub struct DocxParser;
pub struct PptxParser;
pub struct OdtParser;
pub struct OdpParser;

impl Parser for DocxParser {
    fn name(&self) -> &str {
        "docx"
    }
    fn extensions(&self) -> &[&str] {
        &["docx", "docm"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let text = extract_text_from_zip(&raw, &["word/document.xml"])?;
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Docx,
            text,
            sections: Vec::new(),
        })
    }
}

impl Parser for PptxParser {
    fn name(&self) -> &str {
        "pptx"
    }
    fn extensions(&self) -> &[&str] {
        &["pptx", "pptm"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        // ppt/slides/slide*.xml — enumerate them rather than hard-code
        // the count, since presentations vary in size.
        let entries = list_zip_entries(&raw, |name| {
            name.starts_with("ppt/slides/slide") && name.ends_with(".xml")
        })?;
        let mut sorted = entries.clone();
        sorted.sort();
        let mut text = extract_text_from_zip(&raw, sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice())?;
        if text.is_empty() {
            text = String::new();
        }
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Pptx,
            text,
            sections: Vec::new(),
        })
    }
}

impl Parser for OdtParser {
    fn name(&self) -> &str {
        "odt"
    }
    fn extensions(&self) -> &[&str] {
        &["odt"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let text = extract_text_from_zip(&raw, &["content.xml"])?;
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Odt,
            text,
            sections: Vec::new(),
        })
    }
}

impl Parser for OdpParser {
    fn name(&self) -> &str {
        "odp"
    }
    fn extensions(&self) -> &[&str] {
        &["odp"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let text = extract_text_from_zip(&raw, &["content.xml"])?;
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Pptx, // close-enough kind tag
            text,
            sections: Vec::new(),
        })
    }
}

/// Stream the named XML entries out of `raw`, concatenating their text
/// nodes with single spaces. Missing entries log a WARN and contribute
/// nothing — see module docstring for the robustness contract.
fn extract_text_from_zip(raw: &RawDoc, entries: &[&str]) -> anyhow::Result<String> {
    let mut zip = ZipArchive::new(Cursor::new(&raw.bytes))
        .map_err(|e| anyhow::anyhow!("open zip: {e}"))?;
    let mut acc = String::new();
    for name in entries {
        match zip.by_name(name) {
            Ok(mut f) => {
                let mut buf = Vec::new();
                if let Err(e) = f.read_to_end(&mut buf) {
                    tracing::warn!(source = %raw.source, entry = name, error = %e, "zip entry read failed");
                    continue;
                }
                let text = extract_text_nodes(&buf)?;
                if !text.is_empty() {
                    if !acc.is_empty() {
                        acc.push('\n');
                    }
                    acc.push_str(&text);
                }
            }
            Err(_) => {
                tracing::warn!(source = %raw.source, entry = name, "zip entry missing");
            }
        }
    }
    Ok(acc)
}

fn list_zip_entries(raw: &RawDoc, predicate: impl Fn(&str) -> bool) -> anyhow::Result<Vec<String>> {
    let zip = ZipArchive::new(Cursor::new(&raw.bytes))
        .map_err(|e| anyhow::anyhow!("open zip: {e}"))?;
    Ok((0..zip.len())
        .map(|i| zip.name_for_index(i).unwrap_or("").to_owned())
        .filter(|n| predicate(n))
        .collect())
}

fn extract_text_nodes(xml: &[u8]) -> anyhow::Result<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut out = String::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                let s = e
                    .unescape()
                    .map_err(|e| anyhow::anyhow!("xml text unescape: {e}"))?
                    .into_owned();
                let s = s.trim();
                if !s.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(s);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!("xml parse: {e}"));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}
