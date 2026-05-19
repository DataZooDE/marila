//! Shared source-stage types.

use std::path::PathBuf;

/// A document discovered by the source stage and handed to parse.
///
/// We carry the bytes — for tiny v0 — rather than a `Read` handle.
/// Stage 1's mpsc cap (64) plus the per-file size cap (default 50 MiB)
/// bounds the steady-state memory. Page-streaming for huge PDFs lands
/// later if the spec-13 stress case shows we need it.
#[derive(Debug, Clone)]
pub struct RawDoc {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// Display path used in metadata (often == `path.display().to_string()`
    /// but may be the `--text-value` sentinel `"<text-value>"`).
    pub source: String,
    /// File extension (lowercased, no dot). Used by the parse-stage
    /// dispatch to pick a Parser impl.
    pub ext: String,
}
