//! Local-filesystem source. Walks `--text` arguments which may each be
//! either a concrete path or a glob (`doc/**/*.md`). Filters by
//! `--include` / `--exclude` and `--max-file-bytes`. Loads each file
//! into a `RawDoc` and feeds the pipeline.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use globset::{Glob, GlobSetBuilder};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::checkpoint::Checkpoint;
use crate::source::RawDoc;

#[derive(Debug, Clone)]
pub struct LocalSourceConfig {
    pub inputs: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_file_bytes: u64,
}

impl LocalSourceConfig {
    pub fn from_strings(inputs: Vec<String>) -> Self {
        Self {
            inputs,
            include: Vec::new(),
            exclude: Vec::new(),
            max_file_bytes: 50 * 1024 * 1024,
        }
    }
}

/// Walk the configured inputs and push each accepted file into `tx`.
///
/// Returns when every input has been visited and `tx` has been dropped.
pub async fn run(cfg: LocalSourceConfig, tx: mpsc::Sender<RawDoc>) -> Result<()> {
    run_with_checkpoint(cfg, tx, None).await
}

/// Variant that consults a [`Checkpoint`] before reading bytes — paths
/// already marked done are skipped without re-hashing or re-parsing.
pub async fn run_with_checkpoint(
    cfg: LocalSourceConfig,
    tx: mpsc::Sender<RawDoc>,
    checkpoint: Option<Arc<Checkpoint>>,
) -> Result<()> {
    let resolved = resolve_paths(&cfg.inputs)?;
    debug!(file_count = resolved.len(), "local source resolved inputs");

    let include = build_extension_set(&cfg.include);
    let exclude = build_extension_set(&cfg.exclude);

    for path in resolved {
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "stat failed; skipping");
                continue;
            }
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > cfg.max_file_bytes {
            warn!(
                path = %path.display(),
                bytes = meta.len(),
                limit = cfg.max_file_bytes,
                "file exceeds --max-file-bytes; skipping"
            );
            continue;
        }

        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        if include.is_some() && !include.as_ref().unwrap().is_match(&ext) {
            continue;
        }
        if let Some(set) = exclude.as_ref()
            && set.is_match(&ext)
        {
            continue;
        }

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "read failed; skipping");
                continue;
            }
        };

        let content_hash = blake3::hash(&bytes).to_hex().to_string();
        let source = path.display().to_string();

        if let Some(chk) = checkpoint.as_deref()
            && chk.is_done(&source, &content_hash)
        {
            debug!(source = %source, "checkpoint says done; skipping");
            continue;
        }

        let raw = RawDoc {
            source,
            path: path.clone(),
            bytes,
            ext,
            content_hash,
        };
        if tx.send(raw).await.is_err() {
            // Downstream closed (cancellation). Stop walking.
            break;
        }
    }
    Ok(())
}

fn build_extension_set(exts: &[String]) -> Option<globset::GlobSet> {
    if exts.is_empty() {
        return None;
    }
    let mut b = GlobSetBuilder::new();
    for e in exts {
        let e = e.trim_start_matches('.').to_lowercase();
        // Match the bare extension token. `e` is the literal extension
        // string we'll pass to `is_match`.
        b.add(Glob::new(&e).expect("valid extension glob"));
    }
    Some(b.build().expect("build extension globset"))
}

/// Expand each input string to a list of concrete paths.
///
/// Recognised forms:
///   - bare file: returned as-is
///   - directory: recursive walk; emit every regular file
///   - glob pattern (contains `*`, `?` or `[`): walk the longest
///     non-glob prefix and match remaining files against the pattern
fn resolve_paths(inputs: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for raw in inputs {
        if is_glob(raw) {
            out.extend(walk_glob(raw)?);
        } else {
            let p = PathBuf::from(raw);
            if p.is_dir() {
                for entry in WalkDir::new(&p).follow_links(false) {
                    let e = match entry {
                        Ok(e) => e,
                        Err(err) => {
                            warn!(path = %p.display(), error = %err, "walk error");
                            continue;
                        }
                    };
                    if e.file_type().is_file() {
                        out.push(e.into_path());
                    }
                }
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn is_glob(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '*' | '?' | '['))
}

fn walk_glob(pattern: &str) -> Result<Vec<PathBuf>> {
    let glob = Glob::new(pattern)?.compile_matcher();
    let prefix = non_glob_prefix(pattern);
    let root: PathBuf = if prefix.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        prefix
    };

    let mut out = Vec::new();
    for entry in WalkDir::new(&root).follow_links(false) {
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                warn!(root = %root.display(), error = %err, "walk error");
                continue;
            }
        };
        if !e.file_type().is_file() {
            continue;
        }
        let p = e.path();
        if glob.is_match(p) {
            out.push(p.to_path_buf());
        }
    }
    Ok(out)
}

fn non_glob_prefix(pattern: &str) -> PathBuf {
    let mut acc = PathBuf::new();
    for part in Path::new(pattern).components() {
        let s = part.as_os_str().to_string_lossy();
        if is_glob(&s) {
            break;
        }
        acc.push(s.as_ref());
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extension_set_matches() {
        let s = build_extension_set(&["md".to_string(), ".txt".to_string()]).unwrap();
        assert!(s.is_match("md"));
        assert!(s.is_match("txt"));
        assert!(!s.is_match("pdf"));
    }

    #[test]
    fn resolve_directory_collects_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        std::fs::write(dir.path().join("b.md"), b"x").unwrap();
        let resolved = resolve_paths(&[dir.path().to_string_lossy().into_owned()]).unwrap();
        assert_eq!(resolved.len(), 2);
    }
}
