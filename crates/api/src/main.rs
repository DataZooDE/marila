use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use marila_core::DuckDbStateStore;
use marila_storage::{S3BucketStore, S3Config};
use marila_vectors::AppState;
use tokio::net::TcpListener;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cfg = Config::from_env()?;

    let state = Arc::new(DuckDbStateStore::open(&cfg.state_db).context("open state db")?);
    let storage = Arc::new(
        S3BucketStore::connect(S3Config {
            endpoint: cfg.s3_endpoint.clone(),
            access_key_id: cfg.s3_access_key_id.clone(),
            secret_access_key: cfg.s3_secret_access_key.clone(),
            region: cfg.s3_region.clone(),
        })
        .await
        .context("connect to s3 backend")?,
    );

    let app_state = AppState {
        state,
        storage,
        region: cfg.s3_region.clone(),
        account_id: cfg.account_id.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .merge(marila_vectors::router(app_state))
        .layer(
            TraceLayer::new_for_http().make_span_with(DefaultMakeSpan::new().include_headers(true)),
        );

    info!(bind = %cfg.bind_addr, "marila listening");
    let listener = TcpListener::bind(cfg.bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"status": "ok"}))
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}

struct Config {
    bind_addr: SocketAddr,
    s3_endpoint: String,
    s3_access_key_id: String,
    s3_secret_access_key: String,
    s3_region: String,
    account_id: String,
    state_db: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        Ok(Self {
            bind_addr: env_or("MARILA_BIND_ADDR", "0.0.0.0:8080").parse()?,
            s3_endpoint: env_or("MARILA_S3_ENDPOINT", "http://localhost:9000"),
            s3_access_key_id: env_or("MARILA_S3_ACCESS_KEY_ID", "marila"),
            s3_secret_access_key: env_or("MARILA_S3_SECRET_ACCESS_KEY", "marilasecret"),
            s3_region: env_or("MARILA_S3_REGION", "eu-west-1"),
            account_id: env_or("MARILA_AWS_ACCOUNT_ID", "000000000000"),
            state_db: env_or("MARILA_STATE_DB", "data/state.duckdb"),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}
