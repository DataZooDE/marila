//! Reverse-proxy for `/iceberg/v1/*` → Lakekeeper's `/catalog/v1/*`
//! Iceberg REST endpoint (FT-8 in `doc/REQUIREMENTS.md`).
//!
//! With this in place, DuckDB / PyIceberg / Spark can ATTACH the marila
//! endpoint directly:
//!
//! ```sql
//! ATTACH 'demo' AS lake (
//!     TYPE iceberg,
//!     ENDPOINT 'http://localhost:8080/iceberg',
//!     ACCESS_DELEGATION_MODE 'none'   -- doc/DISCOVERIES.md D-1
//! );
//! ```
//!
//! Implementation is intentionally generic: forward the path tail, the
//! query string, the request body, and a curated set of headers. We
//! drop `Host` / `Authorization` because Lakekeeper doesn't require
//! per-request auth in `allow-all` mode (the marila container handles
//! auth at the edge per NG-1).

use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use marila_aws_compat::AwsError;
use reqwest::Client;
use tracing::{instrument, warn};

/// State for the Iceberg proxy: just the Lakekeeper base URL.
#[derive(Clone)]
pub struct IcebergProxyState {
    pub base_url: String,
    pub http: Client,
}

impl IcebergProxyState {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            http: Client::builder()
                .pool_max_idle_per_host(8)
                .build()
                .expect("build reqwest client"),
        }
    }
}

/// axum handler matched against any path under `/iceberg/*tail`.
#[instrument(skip(state, req), fields(method = %req.method(), uri = %req.uri()))]
pub async fn iceberg_proxy(
    State(state): State<IcebergProxyState>,
    req: Request,
) -> Result<Response, AwsError> {
    let (parts, body) = req.into_parts();

    // axum matched on `/iceberg/*tail`; strip the prefix and route the
    // tail under Lakekeeper's `/catalog/v1/`. Tail already has the
    // leading slash from the matched path.
    let tail = parts
        .uri
        .path()
        .strip_prefix("/iceberg/")
        .or_else(|| parts.uri.path().strip_prefix("/iceberg"))
        .unwrap_or(parts.uri.path());

    let query = parts
        .uri
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let target = format!("{}/catalog/{tail}{query}", state.base_url);

    let bytes = to_bytes(body, 50 * 1024 * 1024)
        .await
        .map_err(|e| AwsError::Internal {
            message: format!("read proxy request body: {e}"),
        })?;

    let mut builder = state
        .http
        .request(parts.method.clone(), &target)
        .body(bytes.to_vec());

    // Forward client headers except hop-by-hop / host / authorization.
    for (k, v) in parts.headers.iter() {
        if HOP_BY_HOP.iter().any(|h| *h == k.as_str()) {
            continue;
        }
        if k == header::HOST || k == header::AUTHORIZATION {
            continue;
        }
        builder = builder.header(k, v);
    }

    let upstream = builder.send().await.map_err(|e| AwsError::Internal {
        message: format!("iceberg proxy transport error: {e}"),
    })?;

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let upstream_body = upstream.bytes().await.map_err(|e| AwsError::Internal {
        message: format!("read proxy response body: {e}"),
    })?;

    let mut resp = Response::new(Body::from(upstream_body));
    *resp.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK);
    for (k, v) in upstream_headers.iter() {
        if HOP_BY_HOP.iter().any(|h| *h == k.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            resp.headers_mut().insert(name, val);
        } else {
            warn!(header = %k, "dropping upstream header that didn't round-trip");
        }
    }
    Ok(resp.into_response())
}

/// HTTP/1.1 hop-by-hop headers per RFC 7230 §6.1 — never forwarded.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

// Silence unused-import warnings while keeping the signatures stable
// for future hardening (e.g. SigV4 reverse-signing).
#[allow(dead_code)]
fn _unused(_m: Method, _u: Uri) {}
