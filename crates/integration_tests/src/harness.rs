//! Test harness — embedded RustFS + in-process marila router.
//!
//! Per test binary we boot ONE `EmbeddedStack` (lazily, via
//! `tokio::sync::OnceCell`) consisting of:
//!
//!   * a RustFS server bound to an ephemeral port on 127.0.0.1
//!   * a marila axum router served on a *different* ephemeral port,
//!     pointing at the RustFS endpoint for its S3 backend
//!
//! Tests get an `aws-sdk-s3vectors` (or s3tables) `Client` configured
//! against marila's ephemeral URL. Same wire shape as production —
//! marila signs SigV4 against a real localhost socket — no in-memory
//! tower mock, no protocol shortcut.
//!
//! Why this replaces the old child-process model:
//!
//!   * no docker dependency for `cargo test` (we ship RustFS via the
//!     `rustfs::embedded` module — see its docs for the
//!     one-server-per-process constraint, which our OnceCell respects)
//!   * panics surface in the test binary's own stderr instead of being
//!     buried in a child's stdio
//!   * debuggers + RUST_LOG work uniformly across marila handlers and
//!     test bodies
//!   * tests assert against the same compiled marila code in `crates/api`
//!     (no `cargo build -p marila` skew)

use std::{
    future::Future,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::OnceLock,
};

use futures::FutureExt;

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3vectors::Client;
use uuid::Uuid;

use marila::ServerConfig;

/// AWS S3 Tables client — separate from the S3 Vectors client because
/// the two services have different SDK crates.
pub type TablesClient = aws_sdk_s3tables::Client;

/// Which back-end to point a test at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Marila + RustFS, both embedded in this test process.
    Local,
    /// The real AWS S3 Vectors service in the user's account.
    Aws,
}

/// AWS region used by both targets so ARNs line up.
///
/// Captured in [`CLAUDE.md`] C-5 — the user operates in `eu-west-1`, so
/// marila's local tests use the same region and the wire shapes match.
pub const REGION: &str = "eu-west-1";

/// Compatibility shim — older tests use `let _ = MarilaProcess::start();`
/// as a marker that they need a running marila. The embedded stack now
/// boots lazily inside [`client`]/[`tables_client`], so `start()` is a
/// no-op that returns a unit handle.
///
/// New tests should just `await` [`client`]/[`tables_client`] directly.
#[derive(Debug, Clone, Copy, Default)]
pub struct MarilaProcess;

impl MarilaProcess {
    /// Returns a unit handle so the call site reads identically to the
    /// old `MarilaProcess::start()` API. The actual stack boot happens
    /// on first SDK client construction.
    pub fn start() -> &'static Self {
        static INSTANCE: OnceLock<MarilaProcess> = OnceLock::new();
        INSTANCE.get_or_init(MarilaProcess::default)
    }
}

/// The marila base URL of the currently-running embedded stack.
///
/// Returns the constant local default before the stack has been booted —
/// useful for tests that only inspect the *shape* of a URL and never
/// actually dial it. Once any test has called [`client`]/[`tables_client`]
/// (the moment the stack initialises), this returns the ephemeral
/// `http://127.0.0.1:<port>` of marila's bound socket.
///
/// **Tests that hit the URL directly** (e.g. policy/tag NotImplemented
/// raw-HTTP probes) should call [`local_endpoint`] *after* awaiting
/// [`client`] or [`tables_client`] so the stack is up.
pub const LOCAL_ENDPOINT: &str = "http://127.0.0.1:0";

/// Return the *actual* bound URL of the embedded marila stack. Lazily
/// boots if it isn't already up. Use this from non-SDK tests that need
/// to hand-craft an HTTP request.
///
/// `async` only for source-compat with the previous harness API — the
/// underlying [`embedded`] call is synchronous because the work runs on
/// a dedicated background reactor (see its docstring).
pub async fn local_endpoint() -> String {
    embedded().marila_url.clone()
}

/// Build an S3 Vectors client for the given target.
pub async fn client(target: Target) -> Client {
    let config = aws_sdk_config(target).await;
    Client::new(&config)
}

/// Build an S3 Tables client for the given target.
pub async fn tables_client(target: Target) -> TablesClient {
    let config = aws_sdk_config(target).await;
    TablesClient::new(&config)
}

async fn aws_sdk_config(target: Target) -> aws_config::SdkConfig {
    match target {
        Target::Local => {
            // Dummy creds — marila parses but does not verify SigV4 (NG-1).
            // The SDK still requires *some* credentials to sign.
            let creds = Credentials::new("marila", "marilasecret", None, None, "marila-embedded");
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(REGION))
                .credentials_provider(creds)
                .endpoint_url(embedded().marila_url.clone())
                .load()
                .await
        }
        Target::Aws => {
            aws_config::defaults(BehaviorVersion::latest())
                .region(Region::new(REGION))
                .load()
                .await
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
// Embedded RustFS + in-process marila
// ---------------------------------------------------------------------------

/// One per test binary. Holds the RustFS handle (kept alive for the
/// lifetime of the process) and the bound marila URL.
///
/// Marila's axum server + RustFS run on a *dedicated background thread*
/// with its own tokio runtime, not on the per-test runtimes that
/// `#[tokio::test]` spins up and tears down. That's load-bearing:
/// `axum::serve` tasks live on whatever runtime spawned them, so if we
/// spawned on a per-test runtime the listener would die the moment the
/// first test returned.
pub struct EmbeddedStack {
    pub marila_url: String,
    /// The S3 endpoint marila talks to. Either the docker-compose
    /// RustFS at `http://127.0.0.1:9000` (when it's already up) or
    /// the ephemeral URL of the in-process RustFS we boot below.
    pub rustfs_url: String,
    /// `true` if we booted RustFS in-process; `false` if we reused the
    /// already-running docker RustFS. Tables-side tests use this to
    /// decide whether Lakekeeper can see marila's S3.
    pub using_embedded_rustfs: bool,
    /// Held for the lifetime of the process when we booted RustFS
    /// ourselves; `None` when we're reusing docker.
    _rustfs: Option<rustfs::embedded::RustFSServer>,
}

impl EmbeddedStack {
    /// True when marila uses an in-process RustFS that nothing outside
    /// this test process can reach. Tables-side tests should skip in
    /// this mode because the docker-resident Lakekeeper can't see the
    /// ephemeral 127.0.0.1:NNNN port.
    pub fn lakekeeper_can_see_storage(&self) -> bool {
        !self.using_embedded_rustfs
    }
}

static STACK: OnceLock<EmbeddedStack> = OnceLock::new();

/// Lazily boot the embedded RustFS + marila stack and return a static
/// reference. **Synchronous** — the work happens on a dedicated reactor
/// thread (the only way to keep the axum serve task alive across the
/// per-test tokio runtimes that `#[tokio::test]` builds and tears down).
pub fn embedded() -> &'static EmbeddedStack {
    STACK.get_or_init(boot_embedded_stack_blocking)
}

fn boot_embedded_stack_blocking() -> EmbeddedStack {
    use std::sync::mpsc::sync_channel;
    let (tx, rx) = sync_channel::<EmbeddedStack>(1);
    std::thread::Builder::new()
        .name("marila-test-reactor".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("marila-test-reactor-worker")
                .build()
                .expect("build embedded test runtime");
            rt.block_on(async move {
                let stack = boot_embedded_stack_async().await;
                tx.send(stack).expect("hand stack back to main thread");
                // Park forever so the axum serve task + the RustFS
                // background tasks keep running. The reactor only ever
                // shuts down at process exit.
                std::future::pending::<()>().await;
            });
        })
        .expect("spawn marila-test-reactor thread");
    rx.recv().expect("receive embedded stack from reactor")
}

async fn boot_embedded_stack_async() -> EmbeddedStack {
    // NB: we deliberately do *not* install a tracing subscriber here.
    // The embedded RustFS server's startup calls `rustfs_obs::init_obs`
    // which sets the process-wide global default subscriber via
    // `.init()` (not `try_init`). Installing our own first triggers
    // RustFS's init to panic with "a global default trace dispatcher
    // has already been set". `RUST_LOG` is still honoured because
    // RustFS reads it.

    // ---- S3 backend: prefer the docker-compose RustFS at :9000 if
    // it's already up (lets Lakekeeper share state with marila so the
    // tables-side tests work). Otherwise boot an in-process RustFS for
    // the vectors-side tests; the tables-side tests will skip via
    // `EmbeddedStack::lakekeeper_can_see_storage`. ----
    const DOCKER_S3: &str = "http://127.0.0.1:9000";
    let (s3_endpoint, rustfs, using_embedded_rustfs) =
        if docker_rustfs_reachable().await {
            tracing::info!(s3 = DOCKER_S3, "reusing docker RustFS");
            (DOCKER_S3.to_string(), None, false)
        } else {
            let port = rustfs::embedded::find_available_port()
                .expect("pick a free port for rustfs");
            let r = rustfs::embedded::RustFSServerBuilder::new()
                .address(format!("127.0.0.1:{port}"))
                .access_key("marila")
                .secret_key("marilasecret")
                .region(REGION)
                .build()
                .await
                .expect("start embedded rustfs");
            let url = r.endpoint();
            tracing::info!(s3 = %url, "embedded RustFS ready");
            (url, Some(r), true)
        };

    // ---- marila router, mounted on its own ephemeral port ----
    let state_db = state_db_path();
    let cfg = ServerConfig::for_tests(s3_endpoint.clone(), state_db);
    let router = marila::build_router(cfg)
        .await
        .expect("build marila router");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind marila ephemeral port");
    let bound = listener
        .local_addr()
        .expect("marila local_addr");
    let marila_url = format!("http://{bound}");
    tracing::info!(marila = %marila_url, "embedded marila ready");
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "embedded marila serve loop exited");
        }
    });

    EmbeddedStack {
        marila_url,
        rustfs_url: s3_endpoint,
        using_embedded_rustfs,
        _rustfs: rustfs,
    }
}

/// TCP-connect probe — fast (~50ms) and doesn't generate log noise on
/// the docker-rustfs side if it succeeds.
async fn docker_rustfs_reachable() -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(150),
        tokio::net::TcpStream::connect("127.0.0.1:9000"),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Skip a tables-side test when marila is using an in-process RustFS
/// (the docker-resident Lakekeeper can't reach an ephemeral
/// 127.0.0.1:NNNN port). Tables-side tests pass when the full docker
/// compose stack — rustfs + postgres + lakekeeper — is up.
#[macro_export]
macro_rules! require_lakekeeper_shared_storage {
    () => {
        if !$crate::harness::embedded().lakekeeper_can_see_storage() {
            eprintln!(
                "[skipped] tables-side test needs docker RustFS \
                 + Lakekeeper running — try `docker compose --profile lakekeeper up -d`"
            );
            return;
        }
    };
}

fn state_db_path() -> String {
    // Per-test-binary DB so tests don't see each other's state from a
    // prior run. Sits under target/ so `cargo clean` wipes it.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/marila-test-state");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("state-{}.duckdb", Uuid::new_v4()))
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Bucket naming + cleanup (unchanged from the child-process era)
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

/// Like [`with_bucket`] for the s3tables surface — different SDK client,
/// different cleanup call (`DeleteTableBucket` takes the ARN, not a name).
pub async fn with_table_bucket<F, Fut>(client: TablesClient, prefix: &str, body: F)
where
    F: FnOnce(TablesClient, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let name = unique_bucket_name(prefix);
    let outcome = AssertUnwindSafe(body(client.clone(), name.clone()))
        .catch_unwind()
        .await;

    // Look up the ARN via List (cheapest sequencing) and delete by ARN.
    let list = client.list_table_buckets().send().await;
    if let Ok(list) = list
        && let Some(b) = list.table_buckets().iter().find(|b| b.name() == name)
    {
        let _ = client
            .delete_table_bucket()
            .table_bucket_arn(b.arn())
            .send()
            .await;
    }

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Run `body` with a freshly-named bucket whose cleanup is guaranteed to
/// happen on the test's own tokio runtime — even when the body panics.
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
