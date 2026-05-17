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

#[tokio::test]
async fn put_get_delete_object_round_trip() {
    if skip_if_no_rustfs() {
        eprintln!("[skipped] RustFS not reachable on 127.0.0.1:9000");
        return;
    }
    let store = S3BucketStore::connect(cfg()).await.expect("connect");
    let bucket = format!("marila-storage-obj-{}", uuid::Uuid::new_v4().simple());
    let key = "nested/path/to/snapshot.json";
    let body = br#"{"key":"a","data":[1.0,0.0,0.0,0.0]}"#.to_vec();

    store.ensure_bucket(&bucket).await.expect("create bucket");

    // Missing object returns None — not an error.
    let pre = store
        .get_object(&bucket, key)
        .await
        .expect("get_object on missing key must not error");
    assert!(pre.is_none(), "missing key must return None");

    store
        .put_object(&bucket, key, body.clone())
        .await
        .expect("put_object");

    let got = store
        .get_object(&bucket, key)
        .await
        .expect("get_object after put")
        .expect("must materialise the written body");
    assert_eq!(got, body, "round-tripped body must match what we wrote");

    store
        .delete_object(&bucket, key)
        .await
        .expect("delete_object");

    let after = store
        .get_object(&bucket, key)
        .await
        .expect("get after delete");
    assert!(after.is_none(), "deleted key must return None");

    // delete_object on missing key must be idempotent.
    store
        .delete_object(&bucket, key)
        .await
        .expect("second delete must not error");

    store.delete_bucket(&bucket).await.expect("cleanup bucket");
}
