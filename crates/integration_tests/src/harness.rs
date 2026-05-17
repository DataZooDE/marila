//! Test harness shared by all contract tests.
//!
//! Responsibilities:
//! - Build an `aws-sdk-s3vectors` `Client` for either local marila or
//!   real AWS, with the same SDK code path (no test-specific HTTP
//!   plumbing).
//! - Spawn and clean up a marila child process for `Target::Local`.
//! - Skip `Target::Aws` tests cleanly when no AWS creds are configured.
//! - Provide an RAII bucket-name guard so tests can't leak state on
//!   either target.

use std::{
    future::Future,
    net::TcpStream,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use futures::FutureExt;

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3vectors::Client;
use uuid::Uuid;

/// Which back-end to point a test at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The marila binary running on localhost.
    Local,
    /// The real AWS S3 Vectors service in the user's account.
    Aws,
}

/// AWS region used by both targets so ARNs line up.
///
/// Captured in [`CLAUDE.md`] C-5 — the user operates in `eu-west-1`, so
/// marila's local tests use the same region and the wire shapes match.
pub const REGION: &str = "eu-west-1";

/// Local marila base URL. Matches `MARILA_BIND_ADDR` default in `crates/api/src/main.rs`.
pub const LOCAL_ENDPOINT: &str = "http://localhost:8080";

/// Build an S3 Vectors client for the given target.
pub async fn client(target: Target) -> Client {
    match target {
        Target::Local => {
            // Static dummy creds — marila parses but does not verify SigV4
            // (NG-1). The SDK still requires *some* credentials to sign.
            let creds = Credentials::new("marila", "marilasecret", None, None, "marila-local");
            let config = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(REGION))
                .credentials_provider(creds)
                .endpoint_url(LOCAL_ENDPOINT)
                .load()
                .await;
            Client::new(&config)
        }
        Target::Aws => {
            let config = aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(REGION))
                .load()
                .await;
            Client::new(&config)
        }
    }
}

/// Returns `true` if AWS credentials look usable for `Target::Aws`.
///
/// Cheapest possible probe: presence of `AWS_*` env or `~/.aws/credentials`.
/// We deliberately do not call STS — the test that follows will surface a
/// real auth failure with a useful message if creds are broken.
pub fn aws_creds_available() -> bool {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some() {
        return true;
    }
    if std::env::var_os("AWS_PROFILE").is_some() {
        return true;
    }
    home_dir()
        .map(|h| h.join(".aws/credentials").exists() || h.join(".aws/config").exists())
        .unwrap_or(false)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Skip the current test with a clear message if AWS creds are missing.
///
/// Use at the top of any `aws_*` test.
#[macro_export]
macro_rules! require_aws {
    () => {
        if !$crate::harness::aws_creds_available() {
            eprintln!(
                "[skipped] AWS_ACCESS_KEY_ID / AWS_PROFILE / ~/.aws not present \
                 — set up AWS credentials to run this contract test"
            );
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Local marila process management
// ---------------------------------------------------------------------------

/// Path to the compiled `marila` binary in the workspace's `target/debug`.
///
/// We rely on the fact that `cargo test -p marila-integration-tests` is
/// always invoked from the workspace root (or a sub-crate), so
/// `CARGO_MANIFEST_DIR/../../target/debug/marila` resolves.
fn marila_binary_path() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .join("../../target/debug/marila")
        .canonicalize()
        .unwrap_or_else(|_| manifest_dir.join("../../target/debug/marila"))
}

/// RAII guard around a spawned marila process.
///
/// `Drop` kills the child so a panicking test never leaks the server.
pub struct MarilaProcess {
    child: Option<Child>,
}

impl MarilaProcess {
    /// Build (if needed) and spawn the marila binary; wait for `/health`.
    ///
    /// Idempotent across tests: a `OnceLock`-stored handle is reused, so
    /// the suite spawns marila exactly once even when multiple tests run
    /// in parallel.
    pub fn start() -> &'static Self {
        static INSTANCE: OnceLock<MarilaProcess> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            ensure_built();
            spawn_marila().expect("spawn marila binary")
        })
    }
}

impl Drop for MarilaProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn ensure_built() {
    // Best-effort: build the binary if it's missing. We do *not* rebuild
    // on every test invocation — that would tank the dev loop. If the
    // binary is present we trust it; the user runs `cargo build -p marila`
    // explicitly when they want a refresh, or relies on cargo's own
    // dependency tracking when running `cargo test --workspace`.
    let path = marila_binary_path();
    if path.exists() {
        return;
    }
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "marila"])
        .status()
        .expect("invoke cargo build -p marila");
    assert!(status.success(), "cargo build -p marila failed");
}

fn spawn_marila() -> std::io::Result<MarilaProcess> {
    let bin = marila_binary_path();
    // Best-effort kill of any stale marila process left behind by a
    // previous test invocation that crashed before its Drop ran.
    // pkill is in coreutils on Linux; missing on macOS — we ignore
    // failures either way and let the bind attempt below surface the
    // real error if the port is still held.
    let _ = std::process::Command::new("pkill")
        .args(["-f", bin.to_string_lossy().as_ref()])
        .status();
    // Give the OS a tick to release the port after the kill.
    std::thread::sleep(Duration::from_millis(150));

    let child = Command::new(&bin)
        .env("MARILA_BIND_ADDR", "127.0.0.1:8080")
        .env("MARILA_S3_ENDPOINT", "http://localhost:9000")
        .env("MARILA_S3_ACCESS_KEY_ID", "marila")
        .env("MARILA_S3_SECRET_ACCESS_KEY", "marilasecret")
        .env("MARILA_S3_REGION", REGION)
        .env("MARILA_AWS_ACCOUNT_ID", "000000000000")
        .env("MARILA_STATE_DB", state_db_path())
        .env(
            "RUST_LOG",
            std::env::var("MARILA_TEST_RUST_LOG").unwrap_or_else(|_| {
                "info,marila=debug,marila_vectors=debug,tower_http=info".to_owned()
            }),
        )
        // Forward marila's logs to the test harness's stderr so test
        // failures show what the server actually saw.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    wait_for_health(Duration::from_secs(10));
    Ok(MarilaProcess { child: Some(child) })
}

fn state_db_path() -> String {
    // Per-suite-invocation DB so tests don't see each other's state from
    // a prior run. Sits under target/ so `cargo clean` wipes it.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/marila-test-state");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("state-{}.duckdb", Uuid::new_v4()))
        .to_string_lossy()
        .into_owned()
}

fn wait_for_health(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect("127.0.0.1:8080").is_ok() {
            // Port open — but axum may need a beat to start handling.
            // The first real SDK call will retry as needed, so this is
            // enough.
            std::thread::sleep(Duration::from_millis(50));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("marila did not start listening on 127.0.0.1:8080 within {timeout:?}");
}

// ---------------------------------------------------------------------------
// Bucket naming + cleanup
// ---------------------------------------------------------------------------

/// AWS-safe unique vector-bucket name with a stable test prefix so leaked
/// buckets are easy to identify and (manually) bulk-delete in the unlikely
/// case [`with_bucket`] can't run its cleanup.
pub fn unique_bucket_name(label: &str) -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    // Keep total length within AWS's 3..=63 char window.
    let raw = format!("marila-it-{label}-{suffix}");
    raw[..raw.len().min(63)].to_string()
}

/// Run `body` with a freshly-named bucket whose cleanup is guaranteed to
/// happen on the test's own tokio runtime — even when the body panics.
///
/// We previously used a sync `Drop` that spun up its own runtime; that
/// races the `aws-sdk-s3vectors` client's HTTP-pool tasks (which are
/// bound to the test runtime) and silently failed to delete on AWS,
/// leaking real buckets in the user's account. Catching the panic and
/// awaiting the delete on the same reactor that issued the create makes
/// cleanup synchronous and reliable.
pub async fn with_bucket<F, Fut>(client: Client, prefix: &str, body: F)
where
    F: FnOnce(Client, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let name = unique_bucket_name(prefix);
    let outcome = AssertUnwindSafe(body(client.clone(), name.clone()))
        .catch_unwind()
        .await;

    // Always delete, even when the test body panicked. We swallow the
    // delete error: cleanup is a side-channel, not the assertion.
    let _ = client
        .delete_vector_bucket()
        .vector_bucket_name(&name)
        .send()
        .await;

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Like [`with_bucket`] but for tests that need a set of buckets
/// (e.g. pagination, prefix-filter contracts).
///
/// All of `names` are deleted on scope exit — successful, asserted, or
/// panicked. The body is responsible for creating them; that way the
/// test can decide which subset to create with marila vs. AWS-only
/// validation.
pub async fn with_buckets<F, Fut>(client: Client, names: Vec<String>, body: F)
where
    F: FnOnce(Client, Vec<String>) -> Fut,
    Fut: Future<Output = ()>,
{
    let outcome = AssertUnwindSafe(body(client.clone(), names.clone()))
        .catch_unwind()
        .await;

    for n in &names {
        let _ = client
            .delete_vector_bucket()
            .vector_bucket_name(n)
            .send()
            .await;
    }

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Run `body` inside a freshly-created bucket. Tracks any indexes the
/// body creates via `add_index` so cleanup can DROP them before
/// DeleteVectorBucket (which AWS rejects on a non-empty bucket).
///
/// The body receives a [`BucketCtx`] for index bookkeeping; calling
/// `ctx.add_index("name")` after a successful CreateIndex enrolls the
/// name for cleanup.
pub async fn with_bucket_and_indexes<F, Fut>(client: Client, prefix: &str, body: F)
where
    F: FnOnce(Client, BucketCtx) -> Fut,
    Fut: Future<Output = ()>,
{
    let bucket = unique_bucket_name(prefix);
    client
        .create_vector_bucket()
        .vector_bucket_name(&bucket)
        .send()
        .await
        .expect("create bucket for with_bucket_and_indexes");

    let ctx = BucketCtx::new(bucket.clone());
    let ctx_for_body = ctx.clone();
    let outcome = AssertUnwindSafe(body(client.clone(), ctx_for_body))
        .catch_unwind()
        .await;

    for index in ctx.indexes() {
        let _ = client
            .delete_index()
            .vector_bucket_name(&bucket)
            .index_name(&index)
            .send()
            .await;
    }
    let _ = client
        .delete_vector_bucket()
        .vector_bucket_name(&bucket)
        .send()
        .await;

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Bucket lifecycle context handed to a [`with_bucket_and_indexes`] body.
#[derive(Clone)]
pub struct BucketCtx {
    bucket: String,
    indexes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl BucketCtx {
    fn new(bucket: String) -> Self {
        Self {
            bucket,
            indexes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// The bucket name the body should operate on.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Enroll an index for cleanup. Call this *after* a successful
    /// CreateIndex so we don't try to delete things that never existed.
    pub fn add_index(&self, name: impl Into<String>) {
        self.indexes.lock().expect("ctx mutex").push(name.into());
    }

    fn indexes(&self) -> Vec<String> {
        self.indexes.lock().expect("ctx mutex").clone()
    }
}
