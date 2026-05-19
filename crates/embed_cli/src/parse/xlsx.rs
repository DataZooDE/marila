//! XLSX parser — `calamine` per-sheet streaming. Each sheet becomes
//! one logical block; rows are joined by tab so the chunker can do
//! something sensible with tabular data.

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx};

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
        let cursor = Cursor::new(raw.bytes.clone());
        let mut wb: Xlsx<_> = calamine::open_workbook_from_rs(cursor)
            .map_err(|e| anyhow::anyhow!("open xlsx: {e}"))?;

        let mut text = String::new();
        let sheet_names = wb.sheet_names();
        for name in sheet_names {
            let range = match wb.worksheet_range(&name) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(source = %raw.source, sheet = %name, error = %e, "open sheet failed");
                    continue;
                }
            };
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("# ");
            text.push_str(&name);
            text.push('\n');
            for row in range.rows() {
                let cells: Vec<String> = row.iter().map(data_to_string).collect();
                let line = cells.join("\t");
                if !line.trim().is_empty() {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
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

fn data_to_string(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR({e:?})"),
    }
}
