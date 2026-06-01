//! XLSX parser — minimal hand-rolled OOXML reader.
//!
//! Why not [`calamine`]? Calamine hard-enables the `encoding` feature on
//! `quick-xml`, which feature-unifies the whole workspace's quick-xml.
//! That gates out `Attribute::unescape_value`, which the s3s codegen
//! used by the embedded-RustFS test harness still calls — so adding
//! calamine breaks the test build. The format is simple enough to read
//! ourselves with the `zip` + `quick-xml` deps we already pull in.
//!
//! We extract:
//!   - sheet names (used as section headings)
//!   - shared-string table (`xl/sharedStrings.xml`)
//!   - inline + indexed cell text from each sheet
//!
//! We don't try to render numbers / dates / formulas — RAG embeddings
//! don't need them, and our test corpus only exercises the string path.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::parse::{DocKind, ParsedDoc, Parser};
use crate::source::RawDoc;

pub struct XlsxParser;

impl Parser for XlsxParser {
    fn name(&self) -> &str {
        "xlsx"
    }
    fn extensions(&self) -> &[&str] {
        &["xlsx", "xlsm"]
    }
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc> {
        let mut zip = ZipArchive::new(Cursor::new(&raw.bytes))
            .map_err(|e| anyhow::anyhow!("open xlsx: {e}"))?;

        // Shared strings — most workbooks store text here and reference
        // by index from <c t="s">. Optional file: a workbook with only
        // inline-string cells won't have it.
        let shared = match read_entry(&mut zip, "xl/sharedStrings.xml") {
            Ok(Some(bytes)) => extract_shared_strings(&bytes)?,
            _ => Vec::new(),
        };

        // Workbook gives us the ordered sheet names. We don't currently
        // use them to enforce sheet order (the entries already sort by
        // name), but we use them as headings in the output.
        let workbook = read_entry(&mut zip, "xl/workbook.xml").ok().flatten();
        let sheet_names = workbook
            .as_deref()
            .map(extract_sheet_names)
            .unwrap_or_default();

        let mut sheet_entries: Vec<String> = (0..zip.len())
            .filter_map(|i| {
                let n = zip.name_for_index(i).unwrap_or("").to_owned();
                (n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml")).then_some(n)
            })
            .collect();
        sheet_entries.sort();

        let mut text = String::new();
        for (idx, entry) in sheet_entries.iter().enumerate() {
            let bytes = match read_entry(&mut zip, entry) {
                Ok(Some(b)) => b,
                _ => continue,
            };
            let heading = sheet_names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("Sheet{}", idx + 1));
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("# ");
            text.push_str(&heading);
            text.push('\n');
            extract_sheet_text(&bytes, &shared, &mut text)?;
        }

        Ok(ParsedDoc {
            path: raw.path,
            source: raw.source,
            kind: DocKind::Xlsx,
            text,
            sections: Vec::new(),
            content_hash: raw.content_hash,
        })
    }
}

fn read_entry(
    zip: &mut ZipArchive<Cursor<&Vec<u8>>>,
    name: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    match zip.by_name(name) {
        Ok(mut f) => {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(Some(buf))
        }
        Err(_) => Ok(None),
    }
}

fn extract_shared_strings(xml: &[u8]) -> anyhow::Result<Vec<String>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out: Vec<String> = Vec::new();
    let mut in_si = false;
    let mut current = String::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"si" => {
                in_si = true;
                current.clear();
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"si" => {
                in_si = false;
                out.push(std::mem::take(&mut current));
            }
            Ok(Event::Text(t)) if in_si => {
                let s = t
                    .unescape()
                    .map_err(|e| anyhow::anyhow!("sst unescape: {e}"))?;
                current.push_str(&s);
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("sst xml: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn extract_sheet_names(xml: &[u8]) -> Vec<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.local_name().as_ref() == b"sheet" => {
                for attr in e.attributes().flatten() {
                    if attr.key.local_name().as_ref() == b"name" {
                        if let Ok(s) = attr.unescape_value() {
                            out.push(s.into_owned());
                        } else if let Ok(raw) = std::str::from_utf8(&attr.value) {
                            out.push(raw.to_string());
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn extract_sheet_text(xml: &[u8], shared: &[String], out: &mut String) -> anyhow::Result<()> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut row_text = String::new();
    let mut in_row = false;
    let mut in_cell = false;
    let mut cell_type = CellType::Number; // default; only "s" / "inlineStr" matter to us
    let mut in_v = false;
    let mut in_t = false;
    let mut cell_buf = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"row" => {
                    in_row = true;
                    row_text.clear();
                }
                b"c" => {
                    in_cell = true;
                    cell_type = CellType::Number;
                    cell_buf.clear();
                    for attr in e.attributes().flatten() {
                        if attr.key.local_name().as_ref() == b"t" {
                            cell_type = match attr.value.as_ref() {
                                b"s" => CellType::Shared,
                                b"inlineStr" | b"str" => CellType::Inline,
                                _ => CellType::Number,
                            };
                        }
                    }
                }
                b"v" if in_cell => in_v = true,
                b"t" if in_cell => in_t = true,
                _ => {}
            },
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"row" => {
                    in_row = false;
                    let trimmed = row_text.trim();
                    if !trimmed.is_empty() {
                        out.push_str(trimmed);
                        out.push('\n');
                    }
                }
                b"c" => {
                    in_cell = false;
                    if !cell_buf.is_empty() {
                        if !row_text.is_empty() {
                            row_text.push('\t');
                        }
                        row_text.push_str(&cell_buf);
                    } else if in_row {
                        // preserve cell positions a little so multi-col rows
                        // are visually parseable
                        if !row_text.is_empty() {
                            row_text.push('\t');
                        }
                    }
                }
                b"v" => in_v = false,
                b"t" => in_t = false,
                _ => {}
            },
            Ok(Event::Text(t)) if in_cell && (in_v || in_t) => {
                let s = t
                    .unescape()
                    .map_err(|e| anyhow::anyhow!("sheet unescape: {e}"))?;
                match cell_type {
                    CellType::Shared if in_v => {
                        if let Ok(idx) = s.trim().parse::<usize>()
                            && let Some(v) = shared.get(idx)
                        {
                            cell_buf.push_str(v);
                        }
                    }
                    CellType::Inline if in_t => cell_buf.push_str(&s),
                    CellType::Number if in_v => cell_buf.push_str(&s),
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("sheet xml: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CellType {
    Number,
    Shared,
    Inline,
}
