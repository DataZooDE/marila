//! Plain-text parser. UTF-8 decoded with `from_utf8_lossy` so binary
//! garbage degrades to replacement chars rather than aborting the run.

use crate::parse::{DocKind, ParsedDoc, Parser};
use crate::source::RawDoc;

pub struct TextParser;

impl Parser for TextParser {
    fn name(&self) -> &str {
        "text"
    }
    fn extensions(&self) -> &[&str] {
        &[
            "txt", "text", "log", "rst", "csv", "tsv", "json", "jsonl", "yaml", "yml", "toml",
        ]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let text = String::from_utf8_lossy(&raw.bytes).into_owned();
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Text,
            text,
            sections: Vec::new(),
            content_hash: raw.content_hash,
        })
    }
}
