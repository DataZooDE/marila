//! End-to-end test for `rehydrate_from_snapshots`.
//!
//! Drives a real RustFS instance (skipped if unreachable) to prove
//! the FV-4 promise: with a wiped DuckDB but intact RustFS snapshots,
//! `ListVectors` returns the same set after rehydrate.

use marila_core::{DistanceMetric, StateStore};
use marila_storage::{BucketStore, S3BucketStore, S3Config};

fn skip_if_no_rustfs() -> bool {
    std::net::TcpStream::connect("127.0.0.1:9000")
        .map(|_| false)
        .unwrap_or(true)
}

fn s3_cfg() -> S3Config {
    S3Config {
        endpoint: "http://localhost:9000".into(),
        access_key_id: "marila".into(),
        secret_access_key: "marilasecret".into(),
        region: "eu-west-1".into(),
    }
}

#[tokio::test]
async fn rehydrate_restores_vectors_from_rustfs_snapshots() {
    if skip_if_no_rustfs() {
        eprintln!("[skipped] RustFS not reachable on 127.0.0.1:9000");
        return;
    }

    // Pick fresh, isolated names so we don't collide with other tests.
    let bucket = format!("marila-rehy-{}", uuid::Uuid::new_v4().simple());
    let index = "rehy".to_owned();

    let storage = S3BucketStore::connect(s3_cfg())
        .await
        .expect("storage connect");
    storage.ensure_bucket(&bucket).await.expect("ensure bucket");

    // Seed the state + write some snapshots manually (mimicking
    // what PutVectors does today: snapshot first, then state insert).
    let tmpdir = tempfile::tempdir().expect("tmpdir");
    // Filename must not be "state.duckdb" — DuckDB names the catalog
    // after the file stem, and we already create a schema called
    // "state", which makes references like `state.vector_buckets`
    // ambiguous. Pick a neutral name.
    let state_path = tmpdir.path().join("marila.duckdb");
    let state = marila_core::DuckDbStateStore::open(&state_path).expect("open state");
    state
        .create_vector_bucket(
            &bucket,
            &format!("arn:aws:s3vectors:eu-west-1:000000000000:bucket/{bucket}"),
        )
        .expect("create bucket row");
    state
        .create_index(
            &bucket,
            &index,
            &format!("arn:aws:s3vectors:eu-west-1:000000000000:bucket/{bucket}/index/{index}"),
            4,
            DistanceMetric::Cosine,
        )
        .expect("create index");

    for k in ["alpha", "beta", "gamma"] {
        let body = serde_json::json!({
            "key": k,
            "data": [1.0, 0.0, 0.0, 0.0],
            "metadata": { "label": k }
        });
        storage
            .put_object(
                &bucket,
                &format!("{index}/{k}.json"),
                serde_json::to_vec(&body).unwrap(),
            )
            .await
            .expect("put snapshot");
    }

    // Sanity: state has no vectors yet (we only wrote snapshots).
    let pre = state
        .list_vectors_page(&bucket, &index, None, 100, false, false)
        .expect("list pre");
    assert!(
        pre.rows.is_empty(),
        "no vectors should exist before rehydrate"
    );

    let n = marila_vectors::rehydrate_from_snapshots(&state, &storage)
        .await
        .expect("rehydrate");
    assert_eq!(n, 3, "should restore all 3 snapshots");

    let post = state
        .list_vectors_page(&bucket, &index, None, 100, true, true)
        .expect("list post");
    let mut keys: Vec<&str> = post.rows.iter().map(|r| r.key.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["alpha", "beta", "gamma"]);

    // Metadata round-tripped from the snapshot body.
    let alpha = post.rows.iter().find(|r| r.key == "alpha").unwrap();
    assert_eq!(
        alpha.metadata.as_ref().unwrap()["label"],
        serde_json::json!("alpha")
    );

    // Cleanup: best-effort drop the bucket. The bucket may still have
    // objects so issue per-object deletes first.
    for k in ["alpha", "beta", "gamma"] {
        let _ = storage
            .delete_object(&bucket, &format!("{index}/{k}.json"))
            .await;
    }
    let _ = storage.delete_bucket(&bucket).await;
}
