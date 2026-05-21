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
import os, sys
import boto3
endpoint = "$MARILA_ENDPOINT"
region = "$MARILA_REGION"
c = boto3.client(
    "s3tables",
    endpoint_url=endpoint, region_name=region,
    aws_access_key_id="$AWS_ACCESS_KEY_ID",
    aws_secret_access_key="$AWS_SECRET_ACCESS_KEY",
)
# idempotent bucket
try:
    arn = c.create_table_bucket(name="$BUCKET")["arn"]
except c.exceptions.ConflictException:
    arn = next(
        b["arn"] for b in c.list_table_buckets()["tableBuckets"] if b["name"] == "$BUCKET"
    )
print(arn, end="")
PY
)"
echo "    bucket arn = $BUCKET_ARN"

"$demo_python" - <<PY
import boto3
c = boto3.client(
    "s3tables",
    endpoint_url="$MARILA_ENDPOINT", region_name="$MARILA_REGION",
    aws_access_key_id="$AWS_ACCESS_KEY_ID",
    aws_secret_access_key="$AWS_SECRET_ACCESS_KEY",
)
try:
    c.create_namespace(tableBucketARN="$BUCKET_ARN", namespace=["$NAMESPACE"])
    print("    namespace created")
except c.exceptions.ConflictException:
    print("    namespace exists")
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
try:
    arn = c.create_table(
        tableBucketARN="$BUCKET_ARN",
        namespace="$NAMESPACE",
        name="$TABLE",
        format="ICEBERG",
        metadata=schema,
    )["tableARN"]
    print(f"    table created  arn = {arn}")
except c.exceptions.ConflictException:
    print("    table exists")
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

    # skip if already loaded
    PARQUET_ROWS="$(duckdb -noheader -bail -c "SELECT count(*) FROM read_parquet('$PARQUET');" | tr -d '[:space:]')"
    EXISTING_ROWS="$(duckdb -noheader -bail -c "$(duckdb_preamble) SELECT count(*) FROM lake.\"$NAMESPACE\".\"$TABLE\" WHERE date_trunc('month', tpep_pickup_datetime) = TIMESTAMP '${MONTH}-01 00:00:00';" 2>/dev/null | tr -d '[:space:]' || echo 0)"
    echo "    parquet rows = $PARQUET_ROWS, already in table = $EXISTING_ROWS"
    if [[ "$EXISTING_ROWS" -ge "$PARQUET_ROWS" ]]; then
        echo "    skip: month already loaded"
        continue
    fi

    # bulk INSERT — read_parquet into lake.<ns>.<table> through /iceberg proxy
    echo "==> [$MONTH] INSERT through marila /iceberg proxy …"
    START=$(date +%s)
    duckdb -bail -c "$(duckdb_preamble) INSERT INTO lake.\"$NAMESPACE\".\"$TABLE\" SELECT * FROM read_parquet('$PARQUET');"
    END=$(date +%s)
    NEW_ROWS="$(duckdb -noheader -bail -c "$(duckdb_preamble) SELECT count(*) FROM lake.\"$NAMESPACE\".\"$TABLE\" WHERE date_trunc('month', tpep_pickup_datetime) = TIMESTAMP '${MONTH}-01 00:00:00';" | tr -d '[:space:]')"
    echo "    OK — $NEW_ROWS rows now in $NAMESPACE.$TABLE for $MONTH  (took $((END - START))s)"
done

# ----- summary -----
echo
echo "==> done.  Final tally:"
duckdb -bail -c "$(duckdb_preamble) SELECT count(*) AS total_rows FROM lake.\"$NAMESPACE\".\"$TABLE\";"
echo
echo "Run the TUI:"
echo "  cd demo && .venv/bin/python -m tables.chat"
