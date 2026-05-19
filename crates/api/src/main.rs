use anyhow::Result;
use marila::{ServerConfig, build_router};
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = ServerConfig::from_env()?;
    let bind = cfg.bind_addr.clone();
    let app = build_router(cfg).await?;
    let listener = TcpListener::bind(&bind).await?;
    info!(bind = %bind, "marila listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
