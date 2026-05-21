#!/usr/bin/env bash
#
# NYC Yellow Taxi loader for the marila tables-side TUI demo.
#
# Downloads one or more months of TLC Yellow Taxi parquet, creates the
# Iceberg table via marila's s3tables API (boto3), then INSERTs each
# month through marila's `/iceberg/v1/*` proxy via DuckDB. Re-runs
# skip months already loaded.
#
# REQUIRES:
#   docker compose --profile lakekeeper up -d   (rustfs + postgres + lakekeeper)
#   cargo run -p marila &                       (binds :8080)
#   DuckDB 1.5.3+ on PATH                       (you have v1.5.3)
#
# NOT compatible with `cargo run -p marila --features embedded-rustfs`:
# Lakekeeper-in-docker can't reach the ephemeral 127.0.0.1:NNNN port
# that embedded-rustfs binds. Use the docker-compose RustFS instead.
#
# Configurable via env:
#   TAXI_MONTHS         comma-separated YYYY-MM list (default: 2024-01)
#   BUCKET              s3tables bucket name        (default: taxi)
#   NAMESPACE           namespace inside that bucket (default: nyc)
#   TABLE               table name                  (default: yellow)
#   CACHE_DIR           parquet cache                ($XDG_CACHE_HOME/marila-taxi)
#   MARILA_ENDPOINT     marila base URL              (http://localhost:8080)
#   DUCKDB_S3_ENDPOINT  S3 endpoint DuckDB talks to  (localhost:9000)
#
# Examples:
#   bash demo/tables/load.sh                     # default: 2024-01
#   TAXI_MONTHS=2024-01,2024-02,2024-03 \\
#     bash demo/tables/load.sh                   # full Q1 ~9M rows

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ----- env -----
TAXI_MONTHS="${TAXI_MONTHS:-2024-01}"
BUCKET="${BUCKET:-taxi}"
NAMESPACE="${NAMESPACE:-nyc}"
TABLE="${TABLE:-yellow}"
CACHE_DIR="${CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/marila-taxi}"
MARILA_ENDPOINT="${MARILA_ENDPOINT:-http://localhost:8080}"
MARILA_REGION="${MARILA_REGION:-eu-west-1}"
DUCKDB_S3_ENDPOINT="${DUCKDB_S3_ENDPOINT:-localhost:9000}"
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-marila}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-marilasecret}"

mkdir -p "$CACHE_DIR"

# ----- sanity checks -----
if ! command -v duckdb >/dev/null 2>&1; then
    echo "FATAL: duckdb CLI not found on PATH. Need 1.5.3+." >&2
    exit 2
fi
DUCKDB_VERSION="$(duckdb --version | head -1)"
echo "==> duckdb: $DUCKDB_VERSION"
echo "==> cache:  $CACHE_DIR"
echo "==> taxi:   $BUCKET / $NAMESPACE / $TABLE"
echo "==> months: $TAXI_MONTHS"

# Confirm marila is reachable.
if ! curl -sS "$MARILA_ENDPOINT/health" >/dev/null 2>&1; then
    echo "FATAL: marila not reachable at $MARILA_ENDPOINT/health." >&2
    echo "  Start it with: docker compose --profile lakekeeper up -d" >&2
    echo "                 cargo run -p marila" >&2
    exit 2
fi

# Confirm docker RustFS at :9000 (not the embedded variant).
if ! curl -sS "http://$DUCKDB_S3_ENDPOINT/" >/dev/null 2>&1; then
    echo "FATAL: docker RustFS not reachable at $DUCKDB_S3_ENDPOINT." >&2
    echo "  Tables-side needs the docker compose stack, not embedded-rustfs." >&2
    exit 2
fi

# ----- bootstrap bucket / namespace / table via boto3 -----
echo "==> ensuring $BUCKET/$NAMESPACE/$TABLE exists in marila …"
demo_python="$REPO_ROOT/demo/.venv/bin/python"
if [[ ! -x "$demo_python" ]]; then
    echo "FATAL: $demo_python missing. Run \`cd demo && uv sync\` first." >&2
    exit 2
fi

BUCKET_ARN="$(
    "$demo_python" - <<PY
import boto3
from botocore.exceptions import ClientError
c = boto3.client(
    "s3tables",
    endpoint_url="$MARILA_ENDPOINT", region_name="$MARILA_REGION",
    aws_access_key_id="$AWS_ACCESS_KEY_ID",
    aws_secret_access_key="$AWS_SECRET_ACCESS_KEY",
)
# Check-then-create: Lakekeeper returns various non-Conflict shapes
# (InternalServerException with a 'storage profile overlaps' body)
# when the warehouse already exists, so we can't rely on a single
# except clause. List + match by name is the only reliable path.
existing = {b["name"]: b["arn"] for b in c.list_table_buckets()["tableBuckets"]}
if "$BUCKET" in existing:
    arn = existing["$BUCKET"]
else:
    arn = c.create_table_bucket(name="$BUCKET")["arn"]
print(arn, end="")
PY
)"
echo "    bucket arn = $BUCKET_ARN"

"$demo_python" - <<PY
import boto3
from botocore.exceptions import ClientError
c = boto3.client(
    "s3tables",
    endpoint_url="$MARILA_ENDPOINT", region_name="$MARILA_REGION",
    aws_access_key_id="$AWS_ACCESS_KEY_ID",
    aws_secret_access_key="$AWS_SECRET_ACCESS_KEY",
)
# Namespace: same check-then-create pattern.
existing_ns = {
    tuple(ns["namespace"])
    for ns in c.list_namespaces(tableBucketARN="$BUCKET_ARN").get("namespaces", [])
}
if ("$NAMESPACE",) in existing_ns:
    print("    namespace exists")
else:
    c.create_namespace(tableBucketARN="$BUCKET_ARN", namespace=["$NAMESPACE"])
    print("    namespace created")
# canonical TLC yellow-taxi schema
schema = {"iceberg": {"schema": {"fields": [
    {"name": "vendorid", "type": "int"},
    {"name": "tpep_pickup_datetime", "type": "timestamp"},
    {"name": "tpep_dropoff_datetime", "type": "timestamp"},
    {"name": "passenger_count", "type": "long"},
    {"name": "trip_distance", "type": "double"},
    {"name": "ratecodeid", "type": "long"},
    {"name": "store_and_fwd_flag", "type": "string"},
    {"name": "pulocationid", "type": "int"},
    {"name": "dolocationid", "type": "int"},
    {"name": "payment_type", "type": "long"},
    {"name": "fare_amount", "type": "double"},
    {"name": "extra", "type": "double"},
    {"name": "mta_tax", "type": "double"},
    {"name": "tip_amount", "type": "double"},
    {"name": "tolls_amount", "type": "double"},
    {"name": "improvement_surcharge", "type": "double"},
    {"name": "total_amount", "type": "double"},
    {"name": "congestion_surcharge", "type": "double"},
    {"name": "airport_fee", "type": "double"},
]}}}
existing_tables = {
    t["name"]
    for t in c.list_tables(tableBucketARN="$BUCKET_ARN", namespace="$NAMESPACE").get("tables", [])
}
if "$TABLE" in existing_tables:
    print("    table exists")
else:
    arn = c.create_table(
        tableBucketARN="$BUCKET_ARN",
        namespace="$NAMESPACE",
        name="$TABLE",
        format="ICEBERG",
        metadata=schema,
    )["tableARN"]
    print(f"    table created  arn = {arn}")
PY

# ----- per-month download + INSERT loop -----
duckdb_preamble() {
    cat <<SQL
INSTALL iceberg; LOAD iceberg;
CREATE OR REPLACE SECRET s3_warehouse (
    TYPE s3, PROVIDER config,
    KEY_ID '$AWS_ACCESS_KEY_ID', SECRET '$AWS_SECRET_ACCESS_KEY',
    ENDPOINT '$DUCKDB_S3_ENDPOINT',
    REGION '$MARILA_REGION', URL_STYLE 'path', USE_SSL false,
    SCOPE 's3://$BUCKET/'
);
ATTACH '$BUCKET' AS lake (
    TYPE iceberg,
    ENDPOINT '$MARILA_ENDPOINT/iceberg',
    AUTHORIZATION_TYPE 'none',
    ACCESS_DELEGATION_MODE 'none'
);
SQL
}

IFS=',' read -ra MONTHS_ARRAY <<< "$TAXI_MONTHS"
for MONTH in "${MONTHS_ARRAY[@]}"; do
    MONTH="$(echo -n "$MONTH" | tr -d '[:space:]')"
    PARQUET="$CACHE_DIR/yellow_tripdata_${MONTH}.parquet"
    URL="https://d37ci6vzurychx.cloudfront.net/trip-data/yellow_tripdata_${MONTH}.parquet"

    # download (cached)
    if [[ ! -f "$PARQUET" ]]; then
        echo "==> [$MONTH] downloading $URL …"
        curl -fsSL --retry 3 -o "$PARQUET.tmp" "$URL"
        mv "$PARQUET.tmp" "$PARQUET"
        echo "    saved $(du -h "$PARQUET" | cut -f1)"
    else
        echo "==> [$MONTH] cached $(du -h "$PARQUET" | cut -f1)"
    fi

    # Existing row count for this month — used to short-circuit the
    # MERGE when there's nothing to do.
    PARQUET_ROWS="$(duckdb -csv -bail -c "SELECT count(*) FROM read_parquet('$PARQUET');" 2>/dev/null | tail -1 | tr -d '[:space:]')"
    EXISTING_ROWS="$(duckdb -csv -bail -c "$(duckdb_preamble) SELECT count(*) FROM lake.\"$NAMESPACE\".\"$TABLE\" WHERE date_trunc('month', tpep_pickup_datetime) = TIMESTAMP '${MONTH}-01 00:00:00';" 2>/dev/null | tail -1 | tr -d '[:space:]' || echo 0)"
    : "${EXISTING_ROWS:=0}"
    echo "    parquet rows = $PARQUET_ROWS, already in table = $EXISTING_ROWS"

    # MERGE INTO (DuckDB 1.5.3+, Iceberg V3) — upsert by row content
    # hash. Any row already present (same hash) is skipped; net-new
    # rows are inserted. Re-runs are no-ops.
    echo "==> [$MONTH] MERGE through marila /iceberg proxy …"
    START=$(date +%s)
    # The row-hash key has to come from every payload column; we
    # exclude no fields. Using DuckDB's row(*::*) → md5() so we don't
    # depend on a server-side hash impl.
    # SQL helper to assemble a row-hash from the same columns on both
    # sides of the MERGE. concat_ws-of-casts is portable, doesn't need
    # DuckDB's ROW(*) syntax (which doesn't unpack inside scalar fns).
    row_hash() { local p="$1"; echo "md5(concat_ws('|',
        cast(${p}vendorid as varchar),
        cast(${p}tpep_pickup_datetime as varchar),
        cast(${p}tpep_dropoff_datetime as varchar),
        cast(${p}passenger_count as varchar),
        cast(${p}trip_distance as varchar),
        cast(${p}ratecodeid as varchar),
        cast(${p}store_and_fwd_flag as varchar),
        cast(${p}pulocationid as varchar),
        cast(${p}dolocationid as varchar),
        cast(${p}payment_type as varchar),
        cast(${p}fare_amount as varchar),
        cast(${p}extra as varchar),
        cast(${p}mta_tax as varchar),
        cast(${p}tip_amount as varchar),
        cast(${p}tolls_amount as varchar),
        cast(${p}improvement_surcharge as varchar),
        cast(${p}total_amount as varchar),
        cast(${p}congestion_surcharge as varchar),
        cast(${p}airport_fee as varchar)))"; }
    duckdb -bail -c "$(duckdb_preamble)
MERGE INTO lake.\"$NAMESPACE\".\"$TABLE\" AS t
USING (
    SELECT *, $(row_hash "") AS _row_key
      FROM read_parquet('$PARQUET')
) AS s
ON $(row_hash "t.") = s._row_key
WHEN NOT MATCHED THEN INSERT (
    vendorid, tpep_pickup_datetime, tpep_dropoff_datetime,
    passenger_count, trip_distance, ratecodeid,
    store_and_fwd_flag, pulocationid, dolocationid,
    payment_type, fare_amount, extra, mta_tax,
    tip_amount, tolls_amount, improvement_surcharge,
    total_amount, congestion_surcharge, airport_fee
) VALUES (
    s.vendorid, s.tpep_pickup_datetime, s.tpep_dropoff_datetime,
    s.passenger_count, s.trip_distance, s.ratecodeid,
    s.store_and_fwd_flag, s.pulocationid, s.dolocationid,
    s.payment_type, s.fare_amount, s.extra, s.mta_tax,
    s.tip_amount, s.tolls_amount, s.improvement_surcharge,
    s.total_amount, s.congestion_surcharge, s.airport_fee
);
" >/dev/null
    END=$(date +%s)
    NEW_ROWS="$(duckdb -csv -bail -c "$(duckdb_preamble) SELECT count(*) FROM lake.\"$NAMESPACE\".\"$TABLE\" WHERE date_trunc('month', tpep_pickup_datetime) = TIMESTAMP '${MONTH}-01 00:00:00';" 2>/dev/null | tail -1 | tr -d '[:space:]')"
    echo "    OK — $NEW_ROWS rows in $NAMESPACE.$TABLE for $MONTH  (took $((END - START))s)"
done

# ----- summary -----
echo
TOTAL="$(duckdb -csv -bail -c "$(duckdb_preamble) SELECT count(*) FROM lake.\"$NAMESPACE\".\"$TABLE\";" 2>/dev/null | tail -1 | tr -d '[:space:]')"
echo "==> done.  Total rows in $NAMESPACE.$TABLE: $TOTAL"
echo
echo "Run the TUI:"
echo "  cd demo && uv run python -m tables.chat"
