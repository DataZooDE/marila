//! Phase 5 acceptance: every shipped parser round-trips a known phrase.
//!
//! Fixtures are built in-process so we don't ship binary blobs through
//! git (see `fixtures.rs`).

mod fixtures;

use std::path::PathBuf;

use marila_embed::parse::{
    DocKind, Parser,
    office::{DocxParser, OdtParser, PptxParser},
    pdf::PdfParser,
    xlsx::XlsxParser,
};
use marila_embed::source::RawDoc;

fn raw(bytes: Vec<u8>, ext: &str) -> RawDoc {
    RawDoc {
        path: PathBuf::from(format!("fixture.{ext}")),
        source: format!("fixture.{ext}"),
        ext: ext.to_string(),
        bytes,
    }
}

#[test]
fn pdf_parser_extracts_text() {
    let bytes = fixtures::build_pdf("the quick brown fox");
    let out = PdfParser.parse(raw(bytes, "pdf")).expect("pdf parse");
    assert_eq!(out.kind, DocKind::Pdf);
    assert!(
        out.text.contains("quick brown fox"),
        "missing phrase: {:?}",
        out.text
    );
}

#[test]
fn docx_parser_extracts_text() {
    let bytes = fixtures::build_docx("hello marila from docx");
    let out = DocxParser.parse(raw(bytes, "docx")).expect("docx parse");
    assert_eq!(out.kind, DocKind::Docx);
    assert!(out.text.contains("hello marila from docx"), "got: {:?}", out.text);
}

#[test]
fn pptx_parser_extracts_text() {
    let bytes = fixtures::build_pptx("slide says hi");
    let out = PptxParser.parse(raw(bytes, "pptx")).expect("pptx parse");
    assert_eq!(out.kind, DocKind::Pptx);
    assert!(out.text.contains("slide says hi"), "got: {:?}", out.text);
}

#[test]
fn odt_parser_extracts_text() {
    let bytes = fixtures::build_odt("opendocument hello");
    let out = OdtParser.parse(raw(bytes, "odt")).expect("odt parse");
    assert_eq!(out.kind, DocKind::Odt);
    assert!(out.text.contains("opendocument hello"), "got: {:?}", out.text);
}

#[test]
fn xlsx_parser_extracts_text() {
    let bytes = fixtures::build_xlsx("Sales", "spreadsheet cell content");
    let out = XlsxParser.parse(raw(bytes, "xlsx")).expect("xlsx parse");
    assert_eq!(out.kind, DocKind::Xlsx);
    assert!(
        out.text.contains("spreadsheet cell content"),
        "got: {:?}",
        out.text
    );
    assert!(out.text.contains("Sales"), "missing sheet name header");
}

#[test]
fn oversize_files_are_skipped_not_panicked() {
    use marila_embed::source::local::LocalSourceConfig;
    use tokio::sync::mpsc;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.pdf"), vec![0u8; 2_000_000]).unwrap();
    std::fs::write(dir.path().join("small.txt"), "hello").unwrap();

    let cfg = LocalSourceConfig {
        inputs: vec![dir.path().to_string_lossy().into_owned()],
        include: vec![],
        exclude: vec![],
        max_file_bytes: 1_000_000,
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (tx, mut rx) = mpsc::channel(8);
    rt.block_on(async {
        let walker = tokio::spawn(async move {
            marila_embed::source::local::run(cfg, tx).await.unwrap();
        });
        let mut count = 0;
        while let Some(_) = rx.recv().await {
            count += 1;
        }
        walker.await.unwrap();
        assert_eq!(count, 1, "expected the oversize file to be skipped");
    });
}
