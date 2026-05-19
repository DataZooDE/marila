use std::time::Duration;

use anyhow::Result;
use marila::{ServerConfig, build_router};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // When built without `embedded-rustfs`, install our own tracing
    // subscriber. With `embedded-rustfs`, RustFS's `init_obs` does it
    // (and would panic on a double `.init()` — same trap the test
    // harness documents in CLAUDE.md C-12). In that mode we instead
    // *seed* `RUST_LOG` with sensible defaults so RustFS's own
    // subscriber starts at info-level for marila + warn-level for
    // RustFS's internal scan noise (`metacache_set` / `store_list_objects`
    // log ERROR per missing-volume on a fresh boot — pure startup
    // chatter, not actual failures). Set `RUST_LOG` yourself to dig
    // deeper.
    #[cfg(feature = "embedded-rustfs")]
    if std::env::var_os("RUST_LOG").is_none() {
        // Default everything to WARN — RustFS's per-request tracing
        // emits a handful of INFO-level span-close events per S3 op,
        // which at scale (e.g. rehydrating 200 snapshot objects on
        // boot) drowns out marila's own lines. Then opt the marila
        // crates back in at INFO so the user still sees the
        // load-bearing lines (bind addr, embedded-rustfs endpoint,
        // rehydrate count, shutdown signal). The four RustFS targets
        // below additionally ERROR-log on the first-boot bucket scan;
        // `=off` is the only level that actually mutes ERROR.
        //
        // SAFETY: single-threaded program startup; nothing else is
        // reading the environment yet.
        unsafe {
            std::env::set_var(
                "RUST_LOG",
                concat!(
                    "warn,",
                    "marila=info,",
                    "marila_vectors=info,",
                    "marila_tables=info,",
                    "marila_storage=info,",
                    "marila_core=info,",
                    "rustfs_ecstore::cache_value=off,",
                    "rustfs_ecstore::store_list_objects=off,",
                    "rustfs_ecstore::disk::local=off,",
                    "rustfs_ecstore::rpc::peer_s3_client=off",
                ),
            );
        }
    }
    #[cfg(not(feature = "embedded-rustfs"))]
    init_tracing();

    let cfg = ServerConfig::from_env()?;

    // When built with `--features embedded-rustfs`, boot an in-process
    // RustFS first and point marila at it. The handle is kept alive on
    // the stack for the whole process so its background tasks (and its
    // Drop-time cleanup) survive until exit.
    #[cfg(feature = "embedded-rustfs")]
    let (cfg, _rustfs) = {
        let r = marila::start_embedded_rustfs().await?;
        (
            ServerConfig {
                s3_endpoint: r.endpoint.clone(),
                ..cfg
            },
            r,
        )
    };

    let bind = cfg.bind_addr.clone();
    let app = build_router(cfg).await?;
    let listener = TcpListener::bind(&bind).await?;
    info!(bind = %bind, "marila listening (Ctrl-C to stop, twice to force-exit)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("marila stopped cleanly");
    Ok(())
}

/// Wait for SIGINT or SIGTERM, then arm two safety nets:
///
///   1. **Second SIGINT** — short-circuits whatever cleanup is in
///      progress with `exit(130)`. Useful when a background task is
///      misbehaving and graceful shutdown stalls.
///   2. **15-second deadline** — if graceful shutdown doesn't finish
///      in time (RustFS embedded has dozens of background tasks),
///      force-exit anyway so the terminal returns promptly.
///
/// The returned future resolves on the *first* signal — that's what
/// axum's `with_graceful_shutdown` consumes to stop accepting new
/// connections and drain in-flight requests.
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    info!("shutdown signal received; draining (Ctrl-C again to force-exit)");

    // Force-exit watchdogs. These run in the background; whichever
    // fires first wins. We use `process::exit` rather than panicking
    // because some of RustFS's background tasks live on native threads
    // that wouldn't notice a tokio-level abort.
    tokio::spawn(async {
        let _ = signal::ctrl_c().await;
        eprintln!("second Ctrl-C — forcing exit");
        std::process::exit(130);
    });
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(15)).await;
        eprintln!("graceful shutdown deadline (15s) exceeded — forcing exit");
        std::process::exit(1);
    });
}

#[cfg(not(feature = "embedded-rustfs"))]
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
