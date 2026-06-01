//! Progress + telemetry. One in-place TTY line at 4 Hz showing the
//! parse / chunk / embed / put counters; a final summary with peak RSS
//! + elapsed + counts on exit.
//!
//! Implementation note: the pipeline already updates atomics for every
//! stage. The reporter just polls those atomics and renders. No
//! cross-task channels = no extra backpressure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};

/// Atomic counters shared with the pipeline. The reporter task reads
/// these on a tick; the stages bump them as work happens.
#[derive(Debug, Default)]
pub struct ProgressCounters {
    pub raw_docs: AtomicU64,
    pub parsed_docs: AtomicU64,
    pub chunks: AtomicU64,
    pub embedded: AtomicU64,
    pub put: AtomicU64,
    pub parse_failures: AtomicU64,
    pub embed_failures: AtomicU64,
}

/// Handle for the progress task. Drop calls `finish()` to print the
/// summary line.
pub struct ProgressHandle {
    counters: Arc<ProgressCounters>,
    stop: Arc<AtomicBool>,
    bar: ProgressBar,
    started: Instant,
}

impl ProgressHandle {
    /// Start a 4 Hz progress reporter. The handle owns the renderer
    /// thread; drop it to finalize.
    pub fn start(counters: Arc<ProgressCounters>, label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::with_template("{spinner}  {prefix}  parsed {wide_msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.set_prefix(label.to_string());
        bar.enable_steady_tick(Duration::from_millis(250));

        let handle = Self {
            counters: counters.clone(),
            stop: stop.clone(),
            bar: bar.clone(),
            started: Instant::now(),
        };

        // Renderer task: poll counters and update the bar.
        tokio::spawn(async move {
            let mut last_put = 0u64;
            let mut last_tick = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let parsed = counters.parsed_docs.load(Ordering::Relaxed);
                let chunks = counters.chunks.load(Ordering::Relaxed);
                let embedded = counters.embedded.load(Ordering::Relaxed);
                let put = counters.put.load(Ordering::Relaxed);
                let now = Instant::now();
                let elapsed = (now - last_tick).as_secs_f64().max(0.001);
                let rate = (put.saturating_sub(last_put)) as f64 / elapsed;
                last_put = put;
                last_tick = now;
                bar.set_message(format!(
                    "{parsed}  chunked {chunks}  embedded {embedded}  put {put}  rate {rate:.0} vec/s"
                ));
            }
        });

        handle
    }

    /// Stop the renderer and print a one-line summary.
    pub fn finish(self) {
        self.stop.store(true, Ordering::Relaxed);
        let elapsed = self.started.elapsed();
        let counters = &self.counters;
        let line = format!(
            "done — raw={} parsed={} chunks={} embedded={} put={} \
             parse_failures={} embed_failures={} elapsed={:.2}s peak_rss={} \
             rate={:.0} vec/s",
            counters.raw_docs.load(Ordering::Relaxed),
            counters.parsed_docs.load(Ordering::Relaxed),
            counters.chunks.load(Ordering::Relaxed),
            counters.embedded.load(Ordering::Relaxed),
            counters.put.load(Ordering::Relaxed),
            counters.parse_failures.load(Ordering::Relaxed),
            counters.embed_failures.load(Ordering::Relaxed),
            elapsed.as_secs_f64(),
            format_bytes(peak_rss_bytes().unwrap_or(0)),
            counters.put.load(Ordering::Relaxed) as f64 / elapsed.as_secs_f64().max(0.001),
        );
        self.bar.finish_with_message(line.clone());
        tracing::info!(target: "marila_embed::summary", "{line}");
    }
}

/// Read `VmHWM` (Linux peak resident set size) from /proc/self/status.
/// Returns bytes. `None` if the file isn't readable (non-Linux or proc
/// not mounted).
pub fn peak_rss_bytes() -> Option<u64> {
    let body = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            for tok in rest.split_whitespace() {
                if let Ok(kb) = tok.parse::<u64>() {
                    return Some(kb * 1024);
                }
            }
        }
    }
    None
}

fn format_bytes(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b == 0 {
        return "0".into();
    }
    if (b as f64) < MB {
        format!("{} KiB", b / 1024)
    } else {
        format!("{:.1} MiB", b as f64 / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_handles_zero_and_units() {
        assert_eq!(format_bytes(0), "0");
        assert_eq!(format_bytes(1024 * 200), "200 KiB");
        assert!(format_bytes(50 * 1024 * 1024).starts_with("50."));
    }

    #[test]
    fn peak_rss_is_at_least_nonzero_on_linux() {
        if cfg!(target_os = "linux") {
            assert!(peak_rss_bytes().unwrap_or(0) > 0);
        }
    }
}
