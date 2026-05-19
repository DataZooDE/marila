//! Exponential-backoff retry helper used by every network-bound
//! embedding provider. Jittered to avoid synchronized retries from a
//! pool of concurrent embed workers.

use std::time::Duration;

use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            base_delay_ms: 250,
            max_delay_ms: 30_000,
        }
    }
}

/// Outcome of a single attempt — `Retry` triggers an exponential-backoff
/// sleep before the next try; `Done` short-circuits.
pub enum Attempt<T> {
    Done(anyhow::Result<T>),
    Retry(anyhow::Error),
}

/// Run `op` up to `policy.max_attempts` times, sleeping with jittered
/// exponential backoff between attempts. Each retry is logged at WARN
/// so a pipeline that keeps making forward progress despite transient
/// 429s leaves visible breadcrumbs.
pub async fn with_backoff<F, Fut, T>(label: &str, policy: RetryPolicy, mut op: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Attempt<T>>,
{
    let mut delay = policy.base_delay_ms;
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=policy.max_attempts {
        match op().await {
            Attempt::Done(r) => return r,
            Attempt::Retry(e) => {
                warn!(
                    op = label,
                    attempt,
                    of = policy.max_attempts,
                    error = %e,
                    "transient failure; retrying"
                );
                last_err = Some(e);
                if attempt < policy.max_attempts {
                    let jitter = jitter_for(delay);
                    tokio::time::sleep(Duration::from_millis(delay + jitter)).await;
                    delay = (delay * 2).min(policy.max_delay_ms);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("{label}: retries exhausted")))
}

fn jitter_for(base: u64) -> u64 {
    // 25% jitter, deterministic-ish from the system clock low bits.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    (nanos % (base.max(1) / 4 + 1)).min(base / 4 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread")]
    async fn succeeds_after_transient_failures() {
        let n = Arc::new(AtomicU32::new(0));
        let result: anyhow::Result<u32> = with_backoff(
            "test",
            RetryPolicy {
                max_attempts: 4,
                base_delay_ms: 1,
                max_delay_ms: 2,
            },
            || {
                let n = n.clone();
                async move {
                    let count = n.fetch_add(1, Ordering::SeqCst) + 1;
                    if count < 3 {
                        Attempt::Retry(anyhow::anyhow!("flake {count}"))
                    } else {
                        Attempt::Done(Ok(count))
                    }
                }
            },
        )
        .await;
        assert_eq!(result.unwrap(), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausts_then_errors() {
        let result: anyhow::Result<u32> = with_backoff(
            "test",
            RetryPolicy {
                max_attempts: 2,
                base_delay_ms: 1,
                max_delay_ms: 2,
            },
            || async { Attempt::Retry(anyhow::anyhow!("nope")) },
        )
        .await;
        assert!(result.is_err());
    }
}
