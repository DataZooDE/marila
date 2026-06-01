//! Contract test for `ListVectorBuckets`.
//!
//! Beyond the smoke coverage that lives inside the Create test, this file
//! locks down:
//!  - `prefix` filter
//!  - `maxResults` pagination round-trip via `nextToken`
//!
//! Wire shape captured in doc/GAP_ANALYSIS.md.

use aws_sdk_s3vectors::Client;
use marila_integration_tests::{
    harness::{MarilaProcess, Target, client, unique_bucket_name, with_buckets},
    require_aws,
};

#[tokio::test]
async fn local_list_vector_buckets_prefix_and_pagination() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    run(c).await;
}

#[tokio::test]
async fn aws_list_vector_buckets_prefix_and_pagination() {
    require_aws!();
    let c = client(Target::Aws).await;
    run(c).await;
}

async fn run(c: Client) {
    // Use a per-run uuid so tests on the same AWS account don't collide
    // and we can filter our own buckets out of the response with a
    // tight prefix.
    let run = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("marila-it-listp-{run}-");
    let other = unique_bucket_name("listother");
    let prefix_names: Vec<String> = (0..3).map(|i| format!("{prefix}{i}")).collect();

    let mut all_names = prefix_names.clone();
    all_names.push(other.clone());

    with_buckets(c, all_names, |c, _| async move {
        // Create the four buckets up front. We do this inside the
        // with_buckets body so panics during create still trigger
        // cleanup of whichever subset got created.
        for n in &prefix_names {
            c.create_vector_bucket()
                .vector_bucket_name(n)
                .send()
                .await
                .expect("create prefix bucket");
        }
        c.create_vector_bucket()
            .vector_bucket_name(&other)
            .send()
            .await
            .expect("create other bucket");

        // Prefix filter: only the three should be visible.
        let filtered = c
            .list_vector_buckets()
            .prefix(&prefix)
            .send()
            .await
            .expect("list with prefix");
        let returned: Vec<&str> = filtered
            .vector_buckets()
            .iter()
            .map(|b| b.vector_bucket_name())
            .collect();
        for n in &prefix_names {
            assert!(
                returned.contains(&n.as_str()),
                "expected {n} in filtered list, got {returned:?}"
            );
        }
        assert!(
            !returned.contains(&other.as_str()),
            "non-prefix bucket leaked into filtered list: {returned:?}"
        );

        // Pagination round-trip: page through the *prefix-filtered* set
        // one bucket at a time and verify we see all three.
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        let mut token: Option<String> = None;
        for _ in 0..10 {
            let req = c.list_vector_buckets().prefix(&prefix).max_results(1);
            let req = if let Some(t) = token.as_ref() {
                req.next_token(t)
            } else {
                req
            };
            let page = req.send().await.expect("paginated list");
            for b in page.vector_buckets() {
                seen.insert(b.vector_bucket_name().to_owned());
            }
            match page.next_token() {
                Some(t) if !t.is_empty() => token = Some(t.to_owned()),
                _ => break,
            }
        }
        assert_eq!(
            seen.len(),
            prefix_names.len(),
            "pagination should yield all {} prefix-matching buckets, got {seen:?}",
            prefix_names.len()
        );
    })
    .await;
}
