use anyhow::Context;
use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client, config::Builder as S3ConfigBuilder, error::SdkError, primitives::ByteStream,
};
use tracing::debug;

use crate::store::{BucketStore, ObjectListPage, StorageError};

/// Static configuration for the [`S3BucketStore`].
///
/// Values come from `MARILA_S3_*` env in production (loaded by the
/// binary), or hard-coded in unit tests.
#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

/// `BucketStore` backed by `aws-sdk-s3` against a path-style S3-compatible
/// endpoint (RustFS in dev).
pub struct S3BucketStore {
    client: Client,
}

impl S3BucketStore {
    pub async fn connect(cfg: S3Config) -> Result<Self, StorageError> {
        let creds = Credentials::new(
            cfg.access_key_id.clone(),
            cfg.secret_access_key.clone(),
            None,
            None,
            "marila-storage",
        );
        let shared = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .credentials_provider(creds)
            .endpoint_url(cfg.endpoint.clone())
            .load()
            .await;
        // Path-style addressing — RustFS / MinIO require it. Virtual-host
        // style would expect each bucket as a sub-domain, which doesn't
        // work for a local docker container.
        let s3_cfg = S3ConfigBuilder::from(&shared)
            .force_path_style(true)
            .build();
        Ok(Self {
            client: Client::from_conf(s3_cfg),
        })
    }
}

#[async_trait]
impl BucketStore for S3BucketStore {
    async fn ensure_bucket(&self, name: &str) -> Result<(), StorageError> {
        match self.client.create_bucket().bucket(name).send().await {
            Ok(_) => Ok(()),
            Err(SdkError::ServiceError(svc)) => {
                let err = svc.err();
                // Both real AWS and RustFS surface "already owned" as
                // BucketAlreadyOwnedByYou (or a 409 with a code we can
                // detect via the SDK's typed variants).
                if err.is_bucket_already_owned_by_you() || err.is_bucket_already_exists() {
                    debug!(%name, "bucket already present — treating as ensured");
                    return Ok(());
                }
                Err(StorageError::Backend(
                    anyhow::Error::new(SdkError::ServiceError(svc))
                        .context(format!("create bucket {name}")),
                ))
            }
            Err(e) => Err(StorageError::Backend(
                anyhow::Error::new(e).context(format!("create bucket {name}")),
            )),
        }
    }

    async fn delete_bucket(&self, name: &str) -> Result<(), StorageError> {
        match self.client.delete_bucket().bucket(name).send().await {
            Ok(_) => Ok(()),
            Err(SdkError::ServiceError(svc)) if svc.raw().status().as_u16() == 404 => {
                debug!(%name, "bucket already absent — treating delete as no-op");
                Ok(())
            }
            Err(e) => Err(StorageError::Backend(
                anyhow::Error::new(e).context(format!("delete bucket {name}")),
            )),
        }
    }

    async fn put_object(&self, bucket: &str, key: &str, body: Vec<u8>) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .with_context(|| format!("put object {bucket}/{key}"))?;
        Ok(())
    }

    async fn delete_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        // S3 DELETE on a missing key returns 204 / success — it's
        // idempotent on the server side, so we don't need to filter
        // any error variants here.
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("delete object {bucket}/{key}"))?;
        Ok(())
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        after: Option<&str>,
    ) -> Result<ObjectListPage, StorageError> {
        let mut req = self.client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token) = after {
            req = req.continuation_token(token);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("list_objects {bucket} prefix={prefix}"))?;
        let keys = resp
            .contents()
            .iter()
            .filter_map(|o| o.key().map(str::to_owned))
            .collect();
        let next = if resp.is_truncated().unwrap_or(false) {
            resp.next_continuation_token().map(str::to_owned)
        } else {
            None
        };
        Ok(ObjectListPage { keys, next })
    }

    async fn get_object(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        match self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => {
                let bytes = out
                    .body
                    .collect()
                    .await
                    .with_context(|| format!("collect body of {bucket}/{key}"))?
                    .into_bytes()
                    .to_vec();
                Ok(Some(bytes))
            }
            Err(SdkError::ServiceError(svc)) if svc.raw().status().as_u16() == 404 => Ok(None),
            Err(e) => Err(StorageError::Backend(
                anyhow::Error::new(e).context(format!("get object {bucket}/{key}")),
            )),
        }
    }
}
