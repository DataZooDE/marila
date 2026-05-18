//! Thin async client for Lakekeeper's management + catalog REST APIs.
//!
//! Marila's tables side maps **one AWS table bucket = one Lakekeeper
//! warehouse**. The warehouse-id Lakekeeper hands back becomes our
//! `tableBucketId` on the wire, so AWS clients see a stable UUID per
//! bucket without us having to keep a separate mapping.
//!
//! We only model the surface we proxy from the s3tables façade —
//! namespaces, tables, and the warehouse lifecycle. Everything else
//! (compaction, snapshot expiry, etc.) is Lakekeeper-internal.

use marila_aws_compat::AwsError;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

/// Default Lakekeeper endpoint. Overridden by `MARILA_LAKEKEEPER_URL`
/// at config load.
pub const DEFAULT_LAKEKEEPER_URL: &str = "http://localhost:8181";

#[derive(Clone, Debug)]
pub struct LakekeeperConfig {
    /// Base URL (no trailing slash), e.g. `http://localhost:8181`.
    pub base_url: String,
    /// RustFS endpoint the new warehouses should write to, e.g.
    /// `http://rustfs:9000` from inside the docker network.
    pub storage_endpoint: String,
    /// Static S3 credentials the warehouse uses for its server-side
    /// metadata.json writes (CLAUDE.md D-8 / D-7).
    pub storage_access_key_id: String,
    pub storage_secret_access_key: String,
    pub storage_region: String,
}

#[derive(Clone)]
pub struct LakekeeperClient {
    http: Client,
    cfg: LakekeeperConfig,
}

impl LakekeeperClient {
    pub fn new(cfg: LakekeeperConfig) -> Self {
        Self {
            http: Client::builder()
                .pool_max_idle_per_host(8)
                .build()
                .expect("build reqwest client"),
            cfg,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.base_url)
    }

    /// `POST /management/v1/warehouse` — creates a Lakekeeper warehouse
    /// whose backing store is `<storage_endpoint>/<bucket_name>`. Returns
    /// the warehouse-id Lakekeeper minted, which marila uses as the
    /// `tableBucketId` it returns to AWS clients.
    ///
    /// Idempotency: if a warehouse with the same name already exists
    /// (e.g. retried CreateTableBucket), Lakekeeper returns 409. The
    /// caller maps that to AWS's `ConflictException`.
    pub async fn create_warehouse(&self, bucket_name: &str) -> Result<String, AwsError> {
        let body = serde_json::json!({
            "warehouse-name": bucket_name,
            "project-id": "00000000-0000-0000-0000-000000000000",
            "storage-profile": {
                "type": "s3",
                "bucket": bucket_name,
                "key-prefix": "warehouse",
                "endpoint": self.cfg.storage_endpoint,
                "region": self.cfg.storage_region,
                "path-style-access": true,
                "flavor": "s3-compat",
                "sts-enabled": false,
                "remote-signing-enabled": false,
            },
            "storage-credential": {
                "type": "s3",
                "credential-type": "access-key",
                "aws-access-key-id": self.cfg.storage_access_key_id,
                "aws-secret-access-key": self.cfg.storage_secret_access_key,
            }
        });

        let resp = self
            .http
            .post(self.url("/management/v1/warehouse"))
            .json(&body)
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::CREATED | StatusCode::OK => {
                #[derive(Deserialize)]
                struct CreateOut {
                    #[serde(rename = "warehouse-id")]
                    warehouse_id: String,
                }
                let out: CreateOut = resp.json().await.map_err(internal)?;
                debug!(%bucket_name, warehouse_id = %out.warehouse_id, "lakekeeper warehouse created");
                Ok(out.warehouse_id)
            }
            StatusCode::CONFLICT => Err(AwsError::Conflict(format!(
                "Lakekeeper warehouse `{bucket_name}` already exists"
            ))),
            other => Err(lakekeeper_error("CreateWarehouse", other, resp).await),
        }
    }

    /// `DELETE /management/v1/warehouse/{id}?purge=true` — removes the
    /// warehouse and its catalog state. `purge=true` also tells Lakekeeper
    /// to skip the soft-delete retention window so re-creating the same
    /// bucket-name works immediately.
    ///
    /// **Tolerates 409 `WarehouseHasUnfinishedTasks`**: Lakekeeper queues
    /// async `tabular_purge` work after `DeleteTable`, and DeleteWarehouse
    /// fails while those are pending. From AWS's perspective the bucket
    /// is gone the instant we drop our state row, so we treat this 409
    /// as success and let Lakekeeper finish its cleanup in the
    /// background. The warehouse name + bucket are unique-per-call (UUID
    /// suffix), so a subsequent CreateTableBucket with the same logical
    /// name won't collide.
    pub async fn delete_warehouse(&self, warehouse_id: &str) -> Result<(), AwsError> {
        let resp = self
            .http
            .delete(self.url(&format!(
                "/management/v1/warehouse/{warehouse_id}?force=true&purge=true"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            StatusCode::CONFLICT => {
                let body = resp.text().await.unwrap_or_default();
                debug!(%warehouse_id, body = %body, "lakekeeper DeleteWarehouse 409 (likely WarehouseHasUnfinishedTasks) — accepting; background cleanup will finish");
                Ok(())
            }
            other => Err(lakekeeper_error("DeleteWarehouse", other, resp).await),
        }
    }

    // -----------------------------------------------------------------
    // Catalog API — namespaces
    // -----------------------------------------------------------------

    pub async fn create_namespace(
        &self,
        warehouse_id: &str,
        namespace: &[String],
    ) -> Result<Value, AwsError> {
        let body = serde_json::json!({"namespace": namespace, "properties": {}});
        let resp = self
            .http
            .post(self.url(&format!("/catalog/v1/{warehouse_id}/namespaces")))
            .json(&body)
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp.json().await.map_err(internal),
            StatusCode::CONFLICT => Err(AwsError::Conflict(
                "Namespace already exists".to_owned(),
            )),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified bucket does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("CreateNamespace", other, resp).await),
        }
    }

    pub async fn list_namespaces(&self, warehouse_id: &str) -> Result<Value, AwsError> {
        let resp = self
            .http
            .get(self.url(&format!("/catalog/v1/{warehouse_id}/namespaces")))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK => resp.json().await.map_err(internal),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified bucket does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("ListNamespaces", other, resp).await),
        }
    }

    pub async fn get_namespace(
        &self,
        warehouse_id: &str,
        namespace: &str,
    ) -> Result<Value, AwsError> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK => resp.json().await.map_err(internal),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified namespace does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("GetNamespace", other, resp).await),
        }
    }

    pub async fn delete_namespace(
        &self,
        warehouse_id: &str,
        namespace: &str,
    ) -> Result<(), AwsError> {
        let resp = self
            .http
            .delete(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified namespace does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("DeleteNamespace", other, resp).await),
        }
    }

    // -----------------------------------------------------------------
    // Catalog API — tables
    // -----------------------------------------------------------------

    /// Create a table via Iceberg REST. `schema` follows Iceberg's
    /// canonical `{"type":"struct","fields":[...]}` shape — the marila
    /// handler translates the AWS `{"iceberg":{"schema":{...}}}` wrapper
    /// before calling this.
    pub async fn create_table(
        &self,
        warehouse_id: &str,
        namespace: &str,
        name: &str,
        schema: &Value,
    ) -> Result<Value, AwsError> {
        let body = serde_json::json!({
            "name": name,
            "schema": schema,
        });
        let resp = self
            .http
            .post(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}/tables"
            )))
            .json(&body)
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK | StatusCode::CREATED => resp.json().await.map_err(internal),
            StatusCode::CONFLICT => Err(AwsError::Conflict(
                "Table already exists".to_owned(),
            )),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified namespace does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("CreateTable", other, resp).await),
        }
    }

    pub async fn list_tables(
        &self,
        warehouse_id: &str,
        namespace: &str,
    ) -> Result<Value, AwsError> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}/tables"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK => resp.json().await.map_err(internal),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified namespace does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("ListTables", other, resp).await),
        }
    }

    pub async fn load_table(
        &self,
        warehouse_id: &str,
        namespace: &str,
        name: &str,
    ) -> Result<Value, AwsError> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}/tables/{name}"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK => resp.json().await.map_err(internal),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified table does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("LoadTable", other, resp).await),
        }
    }

    pub async fn delete_table(
        &self,
        warehouse_id: &str,
        namespace: &str,
        name: &str,
    ) -> Result<(), AwsError> {
        let resp = self
            .http
            .delete(self.url(&format!(
                "/catalog/v1/{warehouse_id}/namespaces/{namespace}/tables/{name}"
            )))
            .send()
            .await
            .map_err(internal)?;

        match resp.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            StatusCode::NOT_FOUND => Err(AwsError::NotFound(
                "The specified table does not exist.".to_owned(),
            )),
            other => Err(lakekeeper_error("DeleteTable", other, resp).await),
        }
    }
}

fn internal<E: std::fmt::Display>(e: E) -> AwsError {
    AwsError::Internal {
        message: format!("lakekeeper transport error: {e}"),
    }
}

/// Read the body of a non-success Lakekeeper response and fold it into
/// an `AwsError::Internal` with the status + best-effort body excerpt
/// so logs are useful for diagnosis.
async fn lakekeeper_error(op: &str, status: StatusCode, resp: reqwest::Response) -> AwsError {
    let body = resp.text().await.unwrap_or_default();
    let excerpt: String = body.chars().take(400).collect();
    AwsError::Internal {
        message: format!("lakekeeper {op} returned {status}: {excerpt}"),
    }
}

// Re-exported so the binary can construct config without depending on
// the module-private fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct LakekeeperResolved {
    pub warehouse_id: String,
}
