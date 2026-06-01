//! Parse stage — extension dispatch over the `Parser` trait.

pub mod html;
pub mod markdown;
pub mod office;
pub mod pdf;
pub mod text;
pub mod types;
pub mod xlsx;

use std::sync::Arc;

pub use types::*;

use crate::source::RawDoc;

/// A parser turns bytes for one document into a [`ParsedDoc`].
///
/// Parsers are stateless and `Send + Sync`; we share one instance across
/// the parse-worker pool.
pub trait Parser: Send + Sync {
    fn name(&self) -> &str;
    /// Lowercase extension tokens this parser claims (e.g. `["md","markdown"]`).
    fn extensions(&self) -> &[&str];
    fn parse(&self, raw: RawDoc) -> anyhow::Result<ParsedDoc>;
}

/// Pick a parser for `ext`, or `None` if no shipped parser handles it.
pub fn dispatch(parsers: &[Arc<dyn Parser>], ext: &str) -> Option<Arc<dyn Parser>> {
    parsers
        .iter()
        .find(|p| p.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .cloned()
}

/// The default set of parsers shipped at v0.
pub fn default_set() -> Vec<Arc<dyn Parser>> {
    vec![
        Arc::new(text::TextParser),
        Arc::new(markdown::MarkdownParser),
        Arc::new(html::HtmlParser),
        Arc::new(pdf::PdfParser),
        Arc::new(office::DocxParser),
        Arc::new(office::PptxParser),
        Arc::new(office::OdtParser),
        Arc::new(office::OdpParser),
        Arc::new(xlsx::XlsxParser),
    ]
}
