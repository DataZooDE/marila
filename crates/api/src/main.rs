use anyhow::Result;
use marila::{ServerConfig, build_router};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    // When built without `embedded-rustfs`, install our own tracing
    // subscriber. With `embedded-rustfs`, RustFS's `init_obs` does it
    // (and would panic on a double `.init()` — same trap the test
    // harness documents in CLAUDE.md C-12).
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
    info!(bind = %bind, "marila listening");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(not(feature = "embedded-rustfs"))]
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
