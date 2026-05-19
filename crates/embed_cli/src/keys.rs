//! Deterministic vector keys.
//!
//! `content-hash` (default) — `blake3(source_path || chunk_idx || text)`
//! truncated to 32 hex chars (128-bit, enough for global uniqueness in
//! the worst-case 10^10-chunk regime).
//!
//! `filename` / `path` strategies fall back to the source path when the
//! source isn't a file (e.g. `--text-value`), suffixed with the chunk
//! index when there's more than one chunk.

use crate::cli::KeyStrategy;

/// 128 bits of blake3 is enough to be free of collisions at billion-chunk
/// scale.
const HASH_HEX_LEN: usize = 32;

pub fn chunk_key(strategy: KeyStrategy, source: &str, chunk_idx: u32, text: &str) -> String {
    match strategy {
        KeyStrategy::ContentHash => {
            let mut hasher = blake3::Hasher::new();
            hasher.update(source.as_bytes());
            hasher.update(&chunk_idx.to_le_bytes());
            hasher.update(text.as_bytes());
            let hex = hasher.finalize().to_hex();
            hex[..HASH_HEX_LEN].to_string()
        }
        KeyStrategy::Filename => {
            let base = std::path::Path::new(source)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| source.to_owned());
            format!("{base}#{chunk_idx}")
        }
        KeyStrategy::Path => format!("{source}#{chunk_idx}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_short() {
        let a = chunk_key(KeyStrategy::ContentHash, "src", 0, "hello");
        let b = chunk_key(KeyStrategy::ContentHash, "src", 0, "hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn content_hash_differs_per_text_and_idx() {
        let a = chunk_key(KeyStrategy::ContentHash, "src", 0, "hello");
        let b = chunk_key(KeyStrategy::ContentHash, "src", 0, "world");
        let c = chunk_key(KeyStrategy::ContentHash, "src", 1, "hello");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn filename_strategy() {
        assert_eq!(
            chunk_key(KeyStrategy::Filename, "/a/b/c.md", 3, "x"),
            "c.md#3"
        );
    }
}
