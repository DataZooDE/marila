//! JSONL append-only checkpoint for resumable `put` runs.
//!
//! Each line is one record: `{source, content_hash, chunks, status, at}`.
//! We only ever append — never rewrite — so a kill -9 cannot corrupt
//! earlier entries. `done` records are fsynced before the call returns
//! so a crashed run can resume from the last fully-completed source.
//!
//! Mirrors the spec §3.6 design.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as TokioMutex;
use tracing::debug;

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    source: String,
    content_hash: String,
    chunks: u32,
    status: Status,
    at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Done,
    Partial,
}

/// In-process checkpoint state. Wrap with `Arc<Checkpoint>`.
pub struct Checkpoint {
    path: PathBuf,
    /// `(source_path, content_hash)` tuples loaded from the file. Look-ups
    /// against this short-circuit the source stage.
    done: Mutex<HashSet<(String, String)>>,
    /// Per-source chunk bookkeeping built up during the run. Sealed by
    /// the chunk stage; counted up by the put stage.
    progress: TokioMutex<HashMap<String, SourceProgress>>,
    /// File handle for appending. Held under a TokioMutex so writes don't
    /// interleave.
    file: TokioMutex<Option<tokio::fs::File>>,
}

#[derive(Debug, Default)]
struct SourceProgress {
    expected: u32,
    actual: u32,
    sealed: bool,
    content_hash: String,
    /// True once a `done` line has been appended — so we don't write
    /// the same source twice.
    written: bool,
}

impl Checkpoint {
    /// Load existing entries from `path` (if it exists). When `resume`
    /// is false, the file's contents are not consulted — the run will
    /// re-process everything — but new `done` lines are still appended.
    pub async fn load(path: PathBuf, resume: bool) -> Result<Self> {
        let mut done: HashSet<(String, String)> = HashSet::new();
        if resume && path.exists() {
            let body = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("read checkpoint {}", path.display()))?;
            for line in body.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let rec: Record = match serde_json::from_str(line) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, line, "skipping malformed checkpoint line");
                        continue;
                    }
                };
                if rec.status == Status::Done {
                    done.insert((rec.source, rec.content_hash));
                }
            }
            debug!(checkpoint = %path.display(), entries = done.len(), "loaded checkpoint");
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("open checkpoint for append {}", path.display()))?;

        Ok(Self {
            path,
            done: Mutex::new(done),
            progress: TokioMutex::new(HashMap::new()),
            file: TokioMutex::new(Some(file)),
        })
    }

    pub fn is_done(&self, source: &str, content_hash: &str) -> bool {
        self.done
            .lock()
            .expect("done mutex")
            .contains(&(source.to_string(), content_hash.to_string()))
    }

    /// Called by the chunker after all chunks for `source` have been
    /// emitted. `expected` is the chunk count; `content_hash` is the
    /// hash of the source bytes (or parsed text, depending on caller).
    pub async fn seal(&self, source: &str, expected: u32, content_hash: &str) {
        let mut write_done = false;
        {
            let mut g = self.progress.lock().await;
            let entry = g.entry(source.to_string()).or_default();
            entry.sealed = true;
            entry.expected = expected;
            entry.content_hash = content_hash.to_string();
            if entry.actual >= entry.expected && !entry.written {
                entry.written = true;
                write_done = true;
            }
        }
        if write_done {
            self.append_done(source, expected, content_hash).await;
        }
    }

    /// Called by the put stage after a chunk for `source` lands.
    pub async fn record_put(&self, source: &str) {
        let mut write_done = false;
        let mut payload: Option<(String, u32, String)> = None;
        {
            let mut g = self.progress.lock().await;
            let entry = g.entry(source.to_string()).or_default();
            entry.actual += 1;
            if entry.sealed && entry.actual >= entry.expected && !entry.written {
                entry.written = true;
                write_done = true;
                payload = Some((source.to_string(), entry.expected, entry.content_hash.clone()));
            }
        }
        if write_done {
            if let Some((s, expected, hash)) = payload {
                self.append_done(&s, expected, &hash).await;
            }
        }
    }

    async fn append_done(&self, source: &str, chunks: u32, content_hash: &str) {
        let rec = Record {
            source: source.to_string(),
            content_hash: content_hash.to_string(),
            chunks,
            status: Status::Done,
            at: Utc::now().to_rfc3339(),
        };
        let line = match serde_json::to_string(&rec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "encode checkpoint record");
                return;
            }
        };
        let mut g = self.file.lock().await;
        let Some(f) = g.as_mut() else { return };
        use tokio::io::AsyncWriteExt;
        if let Err(e) = f.write_all(line.as_bytes()).await {
            tracing::warn!(error = %e, "checkpoint append failed");
            return;
        }
        if let Err(e) = f.write_all(b"\n").await {
            tracing::warn!(error = %e, "checkpoint append failed");
            return;
        }
        if let Err(e) = f.sync_data().await {
            tracing::warn!(error = %e, "checkpoint fsync failed");
        }
        // Also seed the in-memory done set so a same-process resume
        // (used by tests) sees its own writes.
        self.done
            .lock()
            .expect("done mutex")
            .insert((source.to_string(), content_hash.to_string()));
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn round_trip_done_record() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("c.jsonl");
        {
            let chk = Checkpoint::load(p.clone(), true).await.unwrap();
            chk.seal("foo.txt", 2, "abc123").await;
            chk.record_put("foo.txt").await;
            chk.record_put("foo.txt").await;
            assert!(chk.is_done("foo.txt", "abc123"));
        }
        // Reload from disk
        let chk = Checkpoint::load(p, true).await.unwrap();
        assert!(chk.is_done("foo.txt", "abc123"));
        assert!(!chk.is_done("bar.txt", "abc123"));
    }

    #[tokio::test]
    async fn no_resume_means_no_load() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("c.jsonl");
        {
            let chk = Checkpoint::load(p.clone(), true).await.unwrap();
            chk.seal("x", 1, "hash").await;
            chk.record_put("x").await;
        }
        let chk = Checkpoint::load(p, false).await.unwrap();
        assert!(!chk.is_done("x", "hash"));
    }
}
