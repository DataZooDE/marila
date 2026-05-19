//! HTML parser — strips tags, returns plain text with whitespace
//! collapsed.

use scraper::Html;

use crate::parse::{DocKind, ParsedDoc, Parser};
use crate::source::RawDoc;

pub struct HtmlParser;

impl Parser for HtmlParser {
    fn name(&self) -> &str {
        "html"
    }
    fn extensions(&self) -> &[&str] {
        &["html", "htm", "xhtml"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let body = String::from_utf8_lossy(&raw.bytes);
        let doc = Html::parse_document(&body);
        let mut text = String::with_capacity(body.len() / 2);
        for node in doc.tree.values() {
            if let scraper::Node::Text(t) = node {
                let s: &str = t;
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(trimmed);
                }
            }
        }
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Html,
            text,
            sections: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_visible_text() {
        let raw = RawDoc {
            path: "x".into(),
            source: "x".into(),
            ext: "html".into(),
            bytes: b"<html><body><h1>Hi</h1><p>there <b>world</b></p></body></html>".to_vec(),
        };
        let p = HtmlParser;
        let out = p.parse(raw).unwrap();
        assert!(out.text.contains("Hi"));
        assert!(out.text.contains("there"));
        assert!(out.text.contains("world"));
    }
}
