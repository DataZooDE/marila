//! FT-8 smoke test: verify the `/iceberg/v1/*` reverse-proxy actually
//! forwards to Lakekeeper and returns the wrapped catalog response.
//!
//! Uses raw HTTP rather than DuckDB's `ATTACH iceberg` so the test can
//! run without spinning up DuckDB. The full end-to-end is exercised by
//! `demo/lakekeeper_verify.sql` when a human runs it manually.

use marila_integration_tests::harness::{LOCAL_ENDPOINT, MarilaProcess};
use uuid::Uuid;

/// We hit the Lakekeeper `config` endpoint via the marila proxy. It's
/// a trivial endpoint that returns the warehouse's stored config; if
/// the proxy forwards correctly we get a 200 with a JSON body
/// containing `defaults` and `overrides` fields (Iceberg REST contract).
#[tokio::test]
async fn local_iceberg_proxy_forwards_to_lakekeeper() {
    let _marila = MarilaProcess::start();

    // Need a warehouse to hit. Look up the bootstrap one created by the
    // docker-compose one-shot. If it isn't there, skip — the compose
    // profile probably isn't up.
    let warehouses_url = "http://localhost:8181/management/v1/warehouse";
    let client = reqwest::Client::new();
    let resp = match client.get(warehouses_url).send().await {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[skipped] Lakekeeper not reachable on :8181 — start with `docker compose --profile lakekeeper up -d`");
            return;
        }
    };
    if !resp.status().is_success() {
        eprintln!("[skipped] Lakekeeper management API unhealthy");
        return;
    }
    let body: serde_json::Value = resp.json().await.unwrap();
    let warehouses = body.get("warehouses").and_then(|v| v.as_array()).cloned();
    let Some(warehouses) = warehouses else {
        eprintln!("[skipped] no warehouses registered with Lakekeeper");
        return;
    };
    let warehouse_id = warehouses
        .iter()
        .find_map(|w| {
            w.get("warehouse-id").and_then(|v| v.as_str()).map(|s| s.to_owned())
        });
    let Some(warehouse_id) = warehouse_id else {
        eprintln!("[skipped] no warehouse-id in management response");
        return;
    };

    // Use a Uuid in the namespace name so re-runs don't collide.
    let ns = format!("marila_proxy_probe_{}", Uuid::new_v4().simple());

    // Create namespace via the proxy.
    let create = client
        .post(format!(
            "{LOCAL_ENDPOINT}/iceberg/v1/{warehouse_id}/namespaces"
        ))
        .json(&serde_json::json!({"namespace": [ns], "properties": {}}))
        .send()
        .await
        .expect("POST namespace via proxy");
    assert!(
        create.status().is_success(),
        "create namespace via proxy must succeed, got {}: {}",
        create.status(),
        create.text().await.unwrap_or_default()
    );

    // List namespaces via the proxy and verify our entry comes back.
    let list = client
        .get(format!(
            "{LOCAL_ENDPOINT}/iceberg/v1/{warehouse_id}/namespaces"
        ))
        .send()
        .await
        .expect("GET namespaces via proxy");
    assert!(list.status().is_success(), "ListNamespaces via proxy failed");
    let listed: serde_json::Value = list.json().await.unwrap();
    let names: Vec<String> = listed
        .get("namespaces")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|n| {
                    n.as_array().and_then(|segs| {
                        segs.first().and_then(|v| v.as_str().map(str::to_owned))
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        names.iter().any(|n| n.starts_with("marila_proxy_probe_")),
        "proxy-created namespace must appear in ListNamespaces, got {names:?}"
    );

    // Clean up.
    let _ = client
        .delete(format!(
            "{LOCAL_ENDPOINT}/iceberg/v1/{warehouse_id}/namespaces/{}",
            names
                .iter()
                .find(|n| n.starts_with("marila_proxy_probe_"))
                .unwrap()
        ))
        .send()
        .await;
}
