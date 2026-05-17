//! Contract tests for `ListIndexes`, `GetIndex`, and standalone
//! `DeleteIndex`. Wire shapes captured in CLAUDE.md C-2d (List/Get)
//! and C-2c (Delete).

use aws_sdk_s3vectors::Client;
use aws_sdk_s3vectors::types::{DataType, DistanceMetric, SseType};
use marila_integration_tests::{
    harness::{
        BucketCtx, MarilaProcess, Target, client, unique_bucket_name, with_bucket_and_indexes,
    },
    require_aws,
};

// ---------------------------------------------------------------------------
// ListIndexes — prefix + pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_list_indexes_prefix_and_pagination() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "lix", list_prefix_pagination).await;
}

#[tokio::test]
async fn aws_list_indexes_prefix_and_pagination() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "lix", list_prefix_pagination).await;
}

#[tokio::test]
async fn local_list_indexes_missing_bucket_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    list_missing_bucket_is_not_found(c).await;
}

#[tokio::test]
async fn aws_list_indexes_missing_bucket_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    list_missing_bucket_is_not_found(c).await;
}

// ---------------------------------------------------------------------------
// GetIndex — by name, by arn, missing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_get_index_by_name_returns_full_description() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "gix", get_by_name_full).await;
}

#[tokio::test]
async fn aws_get_index_by_name_returns_full_description() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "gix", get_by_name_full).await;
}

#[tokio::test]
async fn local_get_index_by_arn() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "gixa", get_by_arn).await;
}

#[tokio::test]
async fn aws_get_index_by_arn() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "gixa", get_by_arn).await;
}

#[tokio::test]
async fn local_get_index_missing_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "gixmiss", get_missing_is_not_found).await;
}

#[tokio::test]
async fn aws_get_index_missing_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "gixmiss", get_missing_is_not_found).await;
}

// ---------------------------------------------------------------------------
// DeleteIndex — standalone happy + not-found
// ---------------------------------------------------------------------------

#[tokio::test]
async fn local_delete_index_then_get_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dix", delete_then_get_missing).await;
}

#[tokio::test]
async fn aws_delete_index_then_get_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dix", delete_then_get_missing).await;
}

#[tokio::test]
async fn local_delete_missing_index_is_not_found() {
    let _marila = MarilaProcess::start();
    let c = client(Target::Local).await;
    with_bucket_and_indexes(c, "dixmiss", delete_missing).await;
}

#[tokio::test]
async fn aws_delete_missing_index_is_not_found() {
    require_aws!();
    let c = client(Target::Aws).await;
    with_bucket_and_indexes(c, "dixmiss", delete_missing).await;
}

// ---------------------------------------------------------------------------
// Shared bodies
// ---------------------------------------------------------------------------

async fn list_prefix_pagination(c: Client, ctx: BucketCtx) {
    let prefix = format!("p{}-", uuid::Uuid::new_v4().simple());
    let other_name = "other".to_owned();
    let prefix_names: Vec<String> = (0..3).map(|i| format!("{prefix}{i}")).collect();

    for n in &prefix_names {
        c.create_index()
            .vector_bucket_name(ctx.bucket())
            .index_name(n)
            .data_type(DataType::Float32)
            .dimension(4)
            .distance_metric(DistanceMetric::Cosine)
            .send()
            .await
            .expect("create prefix index");
        ctx.add_index(n);
    }
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&other_name)
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("create other index");
    ctx.add_index(&other_name);

    // Prefix filter: only the three should be visible.
    let filtered = c
        .list_indexes()
        .vector_bucket_name(ctx.bucket())
        .prefix(&prefix)
        .send()
        .await
        .expect("list indexes with prefix");
    let returned: Vec<&str> = filtered.indexes().iter().map(|i| i.index_name()).collect();
    for n in &prefix_names {
        assert!(
            returned.contains(&n.as_str()),
            "expected {n} in filtered list, got {returned:?}"
        );
    }
    assert!(
        !returned.contains(&other_name.as_str()),
        "non-prefix index leaked into filtered list: {returned:?}"
    );

    // Pagination round-trip through the prefix-filtered set one at a time.
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut req = c
            .list_indexes()
            .vector_bucket_name(ctx.bucket())
            .prefix(&prefix)
            .max_results(1);
        if let Some(t) = token.as_ref() {
            req = req.next_token(t);
        }
        let page = req.send().await.expect("paginated list");
        for i in page.indexes() {
            seen.insert(i.index_name().to_owned());
        }
        match page.next_token() {
            Some(t) if !t.is_empty() => token = Some(t.to_owned()),
            _ => break,
        }
    }
    assert_eq!(
        seen.len(),
        prefix_names.len(),
        "pagination should yield all {} prefix-matching indexes, got {seen:?}",
        prefix_names.len()
    );
}

async fn list_missing_bucket_is_not_found(c: Client) {
    let bucket = unique_bucket_name("lixghost");
    let err = c
        .list_indexes()
        .vector_bucket_name(&bucket)
        .send()
        .await
        .expect_err("ListIndexes against a missing bucket must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException for missing bucket"
    );
}

async fn get_by_name_full(c: Client, ctx: BucketCtx) {
    let index = "full".to_owned();
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(16)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("create");
    ctx.add_index(&index);

    let resp = c
        .get_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .send()
        .await
        .expect("GetIndex by name");
    let desc = resp.index().expect("index field present");

    assert_eq!(desc.vector_bucket_name(), ctx.bucket());
    assert_eq!(desc.index_name(), index);
    assert!(
        desc.index_arn()
            .ends_with(&format!(":bucket/{}/index/{}", ctx.bucket(), index)),
        "indexArn shape: {}",
        desc.index_arn()
    );
    assert_eq!(desc.data_type(), &DataType::Float32);
    assert_eq!(desc.dimension(), 16);
    assert_eq!(desc.distance_metric(), &DistanceMetric::Cosine);

    let enc = desc
        .encryption_configuration()
        .expect("encryptionConfiguration must be present on the response");
    assert_eq!(
        enc.sse_type(),
        &SseType::Aes256,
        "default SSE type must be AES256 per CLAUDE.md C-2b"
    );
}

async fn get_by_arn(c: Client, ctx: BucketCtx) {
    let index = "byarn".to_owned();
    let created = c
        .create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Euclidean)
        .send()
        .await
        .expect("create");
    ctx.add_index(&index);

    let arn = created.index_arn().expect("indexArn on CreateIndex output");
    let resp = c
        .get_index()
        .index_arn(arn)
        .send()
        .await
        .expect("GetIndex by arn");
    let desc = resp.index().expect("index field present");
    assert_eq!(desc.index_name(), index);
    assert_eq!(desc.distance_metric(), &DistanceMetric::Euclidean);
}

async fn get_missing_is_not_found(c: Client, ctx: BucketCtx) {
    let err = c
        .get_index()
        .vector_bucket_name(ctx.bucket())
        .index_name("ghost-index")
        .send()
        .await
        .expect_err("GetIndex on missing index must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException for missing index"
    );
}

async fn delete_then_get_missing(c: Client, ctx: BucketCtx) {
    let index = "dropme".to_owned();
    c.create_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .data_type(DataType::Float32)
        .dimension(4)
        .distance_metric(DistanceMetric::Cosine)
        .send()
        .await
        .expect("create to delete");
    // No add_index — we delete here explicitly.

    c.delete_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .send()
        .await
        .expect("DeleteIndex");

    let err = c
        .get_index()
        .vector_bucket_name(ctx.bucket())
        .index_name(&index)
        .send()
        .await
        .expect_err("Get on deleted index must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "deleted index should return NotFoundException"
    );
}

async fn delete_missing(c: Client, ctx: BucketCtx) {
    let err = c
        .delete_index()
        .vector_bucket_name(ctx.bucket())
        .index_name("never-existed")
        .send()
        .await
        .expect_err("DeleteIndex on missing index must error");
    assert!(
        err.into_service_error().is_not_found_exception(),
        "expected NotFoundException for missing index"
    );
}
