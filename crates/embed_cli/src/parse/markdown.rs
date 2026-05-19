//! Markdown parser — preserves heading anchors so the markdown chunker
//! (Phase 4) can split on section boundaries.
//!
//! Output text is the original input verbatim (so chunk offsets stay
//! aligned with the user's source). Headings are captured into
//! `sections` with their byte offsets + level.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser as MdParser, Tag, TagEnd};

use crate::parse::{DocKind, ParsedDoc, Parser, Section};
use crate::source::RawDoc;

pub struct MarkdownParser;

impl Parser for MarkdownParser {
    fn name(&self) -> &str {
        "markdown"
    }
    fn extensions(&self) -> &[&str] {
        &["md", "markdown", "mdown", "mkd"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let text = String::from_utf8_lossy(&raw.bytes).into_owned();
        let sections = collect_sections(&text);
        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Markdown,
            text,
            sections,
            content_hash: raw.content_hash,
        })
    }
}

fn collect_sections(text: &str) -> Vec<Section> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = MdParser::new_ext(text, opts).into_offset_iter();
    let mut out: Vec<Section> = Vec::new();
    let mut pending: Option<(u8, std::ops::Range<usize>)> = None;
    let mut title = String::new();

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                pending = Some((level_to_u8(level), range));
                title.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, range)) = pending.take() {
                    out.push(Section {
                        start_byte: range.start,
                        level,
                        title: title.trim().to_string(),
                    });
                }
            }
            Event::Text(t) if pending.is_some() => title.push_str(&t),
            Event::Code(t) if pending.is_some() => title.push_str(&t),
            _ => {}
        }
    }
    out
}

fn level_to_u8(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_heading_levels_and_titles() {
        let md = "# A\n\nsome para\n\n## B\n\nmore\n\n### C\n";
        let secs = collect_sections(md);
        assert_eq!(secs.len(), 3);
        assert_eq!((secs[0].level, secs[0].title.as_str()), (1, "A"));
        assert_eq!((secs[1].level, secs[1].title.as_str()), (2, "B"));
        assert_eq!((secs[2].level, secs[2].title.as_str()), (3, "C"));
    }
}
