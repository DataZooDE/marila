//! Shared parse-stage types.

use std::path::PathBuf;

/// A parsed document handed to the chunk stage.
#[derive(Debug, Clone)]
pub struct ParsedDoc {
    pub path: PathBuf,
    pub source: String,
    pub kind: DocKind,
    pub text: String,
    /// Optional structural hints — populated by parsers that retain
    /// section / heading info. Markdown is the canonical example.
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Text,
    Markdown,
    Html,
    Pdf,
    Docx,
    Odt,
    Pptx,
    Xlsx,
}

impl DocKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Text => "text",
            DocKind::Markdown => "markdown",
            DocKind::Html => "html",
            DocKind::Pdf => "pdf",
            DocKind::Docx => "docx",
            DocKind::Odt => "odt",
            DocKind::Pptx => "pptx",
            DocKind::Xlsx => "xlsx",
        }
    }
}

/// A heading anchor inside [`ParsedDoc::text`].
///
/// Used by the markdown chunker (Phase 4) to keep chunks aligned with
/// the document's logical structure and tag each chunk with its
/// `section_path`.
#[derive(Debug, Clone)]
pub struct Section {
    /// Inclusive byte offset of where this section's content starts.
    pub start_byte: usize,
    /// Heading level (1 for `#`, 2 for `##`, ...).
    pub level: u8,
    /// The heading text (without the `#` markers).
    pub title: String,
}
