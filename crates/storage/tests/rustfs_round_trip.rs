//! Integration test for `S3BucketStore` against a live RustFS instance.
//!
//! Run via `docker compose up -d rustfs` then `cargo test -p marila-storage --test rustfs_round_trip`.
//! Skipped (with a printed message) if RustFS isn't reachable.

use marila_storage::{BucketStore, S3BucketStore, S3Config};

fn skip_if_no_rustfs() -> bool {
    std::net::TcpStream::connect("127.0.0.1:9000")
        .map(|_| false)
        .unwrap_or(true)
}

fn cfg() -> S3Config {
    S3Config {
        endpoint: "http://localhost:9000".into(),
        access_key_id: "marila".into(),
        secret_access_key: "marilasecret".into(),
        region: "eu-west-1".into(),
    }
}

#[tokio::test]
async fn ensure_bucket_is_idempotent() {
    if skip_if_no_rustfs() {
        eprintln!("[skipped] RustFS not reachable on 127.0.0.1:9000");
        return;
    }
    let store = S3BucketStore::connect(cfg()).await.expect("connect");
    let name = format!("marila-storage-it-{}", uuid::Uuid::new_v4().simple());

    store.ensure_bucket(&name).await.expect("first create");
    store
        .ensure_bucket(&name)
        .await
        .expect("second create must be a no-op");
    store.delete_bucket(&name).await.expect("delete");
    // delete is idempotent too:
    store
        .delete_bucket(&name)
        .await
        .expect("second delete must be a no-op");
}
