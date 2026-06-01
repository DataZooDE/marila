//! Table handlers — proxy AWS s3tables table ops to Lakekeeper's
//! Iceberg REST catalog API.
//!
//! AWS shape (doc/GAP_ANALYSIS.md):
//!   PUT    /tables/{arn}/{ns}                body `{"name":...,"format":"ICEBERG","metadata":{...}}`
//!   GET    /tables/{arn}/{ns}                → list of tables
//!   GET    /tables/{arn}/{ns}/{name}         → full table description
//!   DELETE /tables/{arn}/{ns}/{name}         → 204
//!
//! Lakekeeper Iceberg REST shape:
//!   POST   /catalog/v1/{warehouse-id}/namespaces/{ns}/tables
//!   GET    /catalog/v1/{warehouse-id}/namespaces/{ns}/tables
//!   GET    /catalog/v1/{warehouse-id}/namespaces/{ns}/tables/{name}
//!   DELETE /catalog/v1/{warehouse-id}/namespaces/{ns}/tables/{name}

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{SecondsFormat, Utc};
use marila_aws_compat::AwsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::instrument;

use crate::{
    arn::parse_bucket_name_from_arn,
    control_plane::{AppState, resolve_warehouse_id, run_state},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTableInput {
    pub name: String,
    /// Accepted but unused — AWS only supports `"ICEBERG"` and we treat
    /// anything else as a ValidationException in the handler.
    #[serde(default)]
    pub format: Option<String>,
    /// AWS wraps the Iceberg schema as
    /// `{"iceberg": {"schema": {"fields": [...]}}}`. We unwrap the
    /// nesting and forward the Iceberg-shape schema to Lakekeeper.
    pub metadata: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTableOutput {
    #[serde(rename = "tableARN")]
    pub table_arn: String,
    pub version_token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListTablesOutput {
    pub tables: Vec<TableSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSummary {
    pub name: String,
    pub namespace: Vec<String>,
    pub namespace_id: String,
    pub table_bucket_id: String,
    pub created_at: String,
    pub modified_at: String,
    pub created_by: String,
    pub owner_account_id: String,
    #[serde(rename = "tableARN")]
    pub table_arn: String,
    #[serde(rename = "type")]
    pub table_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTableOutput {
    pub name: String,
    pub namespace: Vec<String>,
    pub namespace_id: String,
    pub table_bucket_id: String,
    pub created_at: String,
    pub modified_at: String,
    pub created_by: String,
    pub owner_account_id: String,
    pub format: String,
    pub metadata_location: String,
    pub warehouse_location: String,
    pub version_token: String,
    #[serde(rename = "tableARN")]
    pub table_arn: String,
    #[serde(rename = "type")]
    pub table_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTableMetadataLocationOutput {
    pub metadata_location: String,
    pub version_token: String,
    pub warehouse_location: String,
}

// ---------------------------------------------------------------------------
// CreateTable — PUT /tables/{arn}/{namespace}
// ---------------------------------------------------------------------------

#[instrument(skip(app, input), fields(name = %input.name))]
pub async fn create_table(
    State(app): State<AppState>,
    Path((arn, namespace)): Path<(String, String)>,
    Json(input): Json<CreateTableInput>,
) -> Result<Json<CreateTableOutput>, AwsError> {
    let bucket = parse_bucket_name_from_arn(&arn)?.to_owned();
    let warehouse_id = resolve_warehouse_id(&app, &bucket).await?;

    if let Some(fmt) = input.format.as_deref()
        && fmt != "ICEBERG"
    {
        return Err(AwsError::Validation(format!(
            "format must be `ICEBERG`, got `{fmt}`"
        )));
    }

    let schema = extract_iceberg_schema(input.metadata.as_ref())?;
    let lake = app
        .lakekeeper
        .create_table(&warehouse_id, &namespace, &input.name, &schema)
        .await?;

    // Iceberg REST's loadTable response has a `metadata-location` and a
    // big `metadata` blob with a stable `table-uuid`. We map both onto
    // AWS's `tableARN`/`versionToken` (doc/GAP_ANALYSIS.md — versionToken
    // doesn't 1:1 with Iceberg commit tokens; we use the metadata
    // location as a stable monotonic version handle).
    let table_uuid = lake
        .get("metadata")
        .and_then(|m| m.get("table-uuid"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let metadata_location = lake
        .get("metadata-location")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let table_arn = format!("{arn}/table/{table_uuid}");
    let version_token = make_version_token(&metadata_location);

    Ok(Json(CreateTableOutput {
        table_arn,
        version_token,
    }))
}

// ---------------------------------------------------------------------------
// ListTables — GET /tables/{tableBucketARN}?namespace=...
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListTablesQuery {
    pub namespace: Option<String>,
    #[serde(rename = "continuationToken")]
    #[allow(dead_code)]
    pub continuation_token: Option<String>,
    #[serde(rename = "maxTables")]
    #[allow(dead_code)]
    pub max_tables: Option<u32>,
    #[allow(dead_code)]
    pub prefix: Option<String>,
}

#[instrument(skip(app, query))]
pub async fn list_tables(
    State(app): State<AppState>,
    Path(arn): Path<String>,
    Query(query): Query<ListTablesQuery>,
) -> Result<Json<ListTablesOutput>, AwsError> {
    let namespace = query.namespace.ok_or_else(|| {
        AwsError::Validation("ListTables requires a `namespace` query parameter".to_owned())
    })?;
    let bucket = parse_bucket_name_from_arn(&arn)?.to_owned();
    let row = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        move |s| s.get_table_bucket(&bucket)
    })
    .await?;
    let warehouse_id = row.table_bucket_id.clone();

    let lake = app
        .lakekeeper
        .list_tables(&warehouse_id, &namespace)
        .await?;
    let identifiers = lake
        .get("identifiers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let tables = identifiers
        .into_iter()
        .filter_map(|ident| {
            let name = ident.get("name")?.as_str()?.to_owned();
            let ns_segs: Vec<String> = ident
                .get("namespace")?
                .as_array()?
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect();
            Some(TableSummary {
                table_arn: format!("{arn}/table/{name}"),
                name: name.clone(),
                namespace: ns_segs,
                namespace_id: String::new(),
                table_bucket_id: warehouse_id.clone(),
                created_at: now_iso8601(),
                modified_at: now_iso8601(),
                created_by: row.owner_account_id.clone(),
                owner_account_id: row.owner_account_id.clone(),
                table_type: "customer".to_owned(),
            })
        })
        .collect();

    Ok(Json(ListTablesOutput {
        tables,
        continuation_token: None,
    }))
}

// ---------------------------------------------------------------------------
// GetTable — GET /get-table?tableBucketARN=&namespace=&name=
//   (note: AWS uses an RPC-style URL here, distinct from the REST shape
//    of CreateTable / DeleteTable / GetTableMetadataLocation)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GetTableQuery {
    #[serde(rename = "tableBucketARN")]
    pub table_bucket_arn: Option<String>,
    pub namespace: Option<String>,
    pub name: Option<String>,
}

#[instrument(skip(app, query))]
pub async fn get_table(
    State(app): State<AppState>,
    Query(query): Query<GetTableQuery>,
) -> Result<Json<GetTableOutput>, AwsError> {
    let arn = query
        .table_bucket_arn
        .ok_or_else(|| AwsError::Validation("tableBucketARN query param is required".into()))?;
    let namespace = query
        .namespace
        .ok_or_else(|| AwsError::Validation("namespace query param is required".into()))?;
    let name = query
        .name
        .ok_or_else(|| AwsError::Validation("name query param is required".into()))?;

    let bucket = parse_bucket_name_from_arn(&arn)?.to_owned();
    let row = run_state(app.state.clone(), {
        let bucket = bucket.clone();
        move |s| s.get_table_bucket(&bucket)
    })
    .await?;
    let warehouse_id = row.table_bucket_id.clone();

    let lake = app
        .lakekeeper
        .load_table(&warehouse_id, &namespace, &name)
        .await?;

    let metadata = lake.get("metadata").cloned().unwrap_or(Value::Null);
    let metadata_location = lake
        .get("metadata-location")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let table_uuid = metadata
        .get("table-uuid")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let warehouse_location = metadata
        .get("location")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    Ok(Json(GetTableOutput {
        table_arn: format!("{arn}/table/{table_uuid}"),
        name,
        namespace: vec![namespace],
        namespace_id: String::new(),
        table_bucket_id: warehouse_id,
        created_at: now_iso8601(),
        modified_at: now_iso8601(),
        created_by: row.owner_account_id.clone(),
        owner_account_id: row.owner_account_id,
        format: "ICEBERG".to_owned(),
        version_token: make_version_token(&metadata_location),
        metadata_location,
        warehouse_location,
        table_type: "customer".to_owned(),
    }))
}

// ---------------------------------------------------------------------------
// GetTableMetadataLocation — GET /tables/{arn}/{namespace}/{name}/metadata-location
// ---------------------------------------------------------------------------

#[instrument(skip(app))]
pub async fn get_table_metadata_location(
    State(app): State<AppState>,
    Path((arn, namespace, name)): Path<(String, String, String)>,
) -> Result<Json<GetTableMetadataLocationOutput>, AwsError> {
    let bucket = parse_bucket_name_from_arn(&arn)?.to_owned();
    let warehouse_id = resolve_warehouse_id(&app, &bucket).await?;

    let lake = app
        .lakekeeper
        .load_table(&warehouse_id, &namespace, &name)
        .await?;

    let metadata_location = lake
        .get("metadata-location")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let warehouse_location = lake
        .get("metadata")
        .and_then(|m| m.get("location"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();

    Ok(Json(GetTableMetadataLocationOutput {
        version_token: make_version_token(&metadata_location),
        metadata_location,
        warehouse_location,
    }))
}

// ---------------------------------------------------------------------------
// DeleteTable — DELETE /tables/{arn}/{namespace}/{name}
// ---------------------------------------------------------------------------

#[instrument(skip(app))]
pub async fn delete_table(
    State(app): State<AppState>,
    Path((arn, namespace, name)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AwsError> {
    let bucket = parse_bucket_name_from_arn(&arn)?.to_owned();
    let warehouse_id = resolve_warehouse_id(&app, &bucket).await?;
    app.lakekeeper
        .delete_table(&warehouse_id, &namespace, &name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// AWS s3tables takes the schema as
/// `{"iceberg":{"schema":{"fields":[{"name":"x","type":"int","required":true},...]}}}`.
/// Lakekeeper's Iceberg REST expects the schema as the inner
/// `{"type":"struct","fields":[...]}` shape, with `id` and optional
/// `required` on each field. We translate by:
///   1. Lifting the nested `iceberg.schema` out of the wrapper.
///   2. Filling `id` (auto-incremented) and ensuring `type:"struct"`.
fn extract_iceberg_schema(metadata: Option<&Value>) -> Result<Value, AwsError> {
    let inner = metadata
        .and_then(|m| m.get("iceberg"))
        .and_then(|v| v.get("schema"))
        .ok_or_else(|| {
            AwsError::Validation(
                "metadata.iceberg.schema is required for ICEBERG-format tables".to_owned(),
            )
        })?;

    let fields = inner
        .get("fields")
        .and_then(|f| f.as_array())
        .ok_or_else(|| {
            AwsError::Validation(
                "metadata.iceberg.schema.fields must be an array of field descriptors".to_owned(),
            )
        })?;

    let normalised: Vec<Value> = fields
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let mut obj = f.as_object().cloned().unwrap_or_default();
            obj.entry("id".to_owned())
                .or_insert(Value::Number(serde_json::Number::from(idx + 1)));
            obj.entry("required".to_owned())
                .or_insert(Value::Bool(false));
            Value::Object(obj)
        })
        .collect();

    Ok(serde_json::json!({
        "type": "struct",
        "fields": normalised,
    }))
}

/// Lakekeeper doesn't return per-table createdAt over the catalog API,
/// so we synthesise a current-time placeholder in the AWS-expected
/// nanosecond+Z format (doc/GAP_ANALYSIS.md). Contract tests assert on
/// shape, not specific timestamp values.
fn now_iso8601() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// AWS exposes a `versionToken` opaque CAS handle. Iceberg's commit
/// flow uses a different mechanism; we derive a stable monotonic token
/// from the metadata-location filename (which Lakekeeper bumps on
/// every commit). Documented in `doc/GAP_ANALYSIS.md` (TODO) as a
/// known deviation.
fn make_version_token(metadata_location: &str) -> String {
    // metadata_location ends with .../00000-<uuid>.gz.metadata.json
    // — the leading 5-digit counter monotonically increases. Use that
    // plus a short hash of the full path so the token is unique even
    // across re-created tables.
    let leaf = metadata_location
        .rsplit('/')
        .next()
        .unwrap_or(metadata_location);
    let counter: String = leaf.chars().take_while(|c| c.is_ascii_digit()).collect();
    let mut digest = 0u64;
    for b in metadata_location.bytes() {
        digest = digest.wrapping_mul(31).wrapping_add(b as u64);
    }
    format!("{counter}-{digest:016x}")
}
