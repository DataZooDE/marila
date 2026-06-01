#!/usr/bin/env python3
"""Realistic S3 Tables demo — sales analytics narrative.

Mirrors the canonical pattern from the AWS [Transform Data to S3 Tables
with Athena][1] post: take a real (synthetic) dataset, land it in an
Iceberg-backed table, then run aggregate queries, fix bad rows with
`DELETE`, rebrand a category with `UPDATE`, and re-aggregate.

[1]: https://aws.amazon.com/blogs/big-data/transform-your-data-to-amazon-s3-tables-with-amazon-athena/

The narrative spans **two API surfaces**:

  - **boto3 s3tables** for the control plane (CreateTableBucket,
    CreateNamespace, CreateTable with the right Iceberg schema).
  - **DuckDB** for the data plane (INSERT 1,002 rows from CSV via
    the `/iceberg/v1/*` reverse-proxy, then SELECT/DELETE/UPDATE).

We deliberately split the workload along these lines because that is
how real users approach S3 Tables: catalog management via the
service's SDK, query/analytics via Athena/Spark/DuckDB through the
Iceberg REST endpoint. Marila proxies both.

Prerequisites:
  - `docker compose --profile lakekeeper up -d`
  - `cargo run -p marila`
  - `demo/.venv/bin/python demo/demo_tables.py` (regenerates the CSV
    automatically if missing)
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys
import uuid
from contextlib import contextmanager

import boto3

ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
REGION = os.environ.get("MARILA_REGION", "eu-west-1")
ACCESS_KEY = os.environ.get("MARILA_ACCESS_KEY_ID", "marila")
SECRET_KEY = os.environ.get("MARILA_SECRET_ACCESS_KEY", "marilasecret")

# The S3 endpoint *DuckDB* talks to. Marila itself uses RustFS via
# docker's `rustfs:9000` alias, but DuckDB runs on the host and reaches
# RustFS on localhost — see doc/GAP_ANALYSIS.md / D-2.
DUCKDB_S3_ENDPOINT = os.environ.get("DEMO_DUCKDB_S3_ENDPOINT", "localhost:9000")

DEMO_DIR = pathlib.Path(__file__).parent
CSV_PATH = DEMO_DIR / "sales_seed.csv"


def client():
    return boto3.client(
        "s3tables",
        endpoint_url=ENDPOINT,
        region_name=REGION,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
    )


@contextmanager
def table_bucket(c, name: str):
    """Create a marila table-bucket (which creates a fresh Lakekeeper
    warehouse named after the bucket) and tear it down on exit."""
    resp = c.create_table_bucket(name=name)
    arn = resp["arn"]
    try:
        yield arn
    finally:
        # Best-effort cleanup. If Lakekeeper still has unfinished
        # tabular_purge jobs the warehouse delete returns 409 — marila
        # tolerates that (see crates/tables/src/lakekeeper.rs).
        try:
            c.delete_table_bucket(tableBucketARN=arn)
        except Exception as e:
            print(f"  (delete_table_bucket: {e})")


@contextmanager
def namespace(c, arn: str, ns: str):
    c.create_namespace(tableBucketARN=arn, namespace=[ns])
    try:
        yield
    finally:
        try:
            c.delete_namespace(tableBucketARN=arn, namespace=ns)
        except Exception as e:
            print(f"  (namespace cleanup: {e})")


def run_duckdb(sql: str) -> str:
    """Pipe `sql` to a fresh `duckdb` process and return its stdout."""
    proc = subprocess.run(
        ["duckdb"],
        input=sql,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        raise RuntimeError(f"duckdb exited {proc.returncode}")
    return proc.stdout


def duckdb_preamble(warehouse_name: str) -> str:
    """The shared header: load iceberg, set up the S3 secret with a
    SCOPE matching this run's RustFS bucket, and ATTACH marila's
    warehouse via the /iceberg proxy. The warehouse_name doubles as
    the S3 bucket name because marila uses bucket-name = warehouse-name
    for the AWS table-bucket → Lakekeeper-warehouse mapping."""
    return f"""
INSTALL iceberg; LOAD iceberg;
CREATE OR REPLACE SECRET s3_warehouse (
    TYPE s3, PROVIDER config,
    KEY_ID '{ACCESS_KEY}', SECRET '{SECRET_KEY}',
    ENDPOINT '{DUCKDB_S3_ENDPOINT}',
    REGION '{REGION}', URL_STYLE 'path', USE_SSL false,
    SCOPE 's3://{warehouse_name}/'
);
ATTACH '{warehouse_name}' AS lake (
    TYPE iceberg,
    ENDPOINT '{ENDPOINT}/iceberg',
    AUTHORIZATION_TYPE 'none',
    ACCESS_DELEGATION_MODE 'none'
);
"""


def main() -> int:
    if not CSV_PATH.exists():
        print(f"Generating {CSV_PATH}...")
        subprocess.run([sys.executable, str(DEMO_DIR / "sales_seed.py")], check=True)

    c = client()
    run = uuid.uuid4().hex[:12]

    # ----- Control plane via boto3 s3tables -----
    # `CreateTableBucket` makes marila spin up a fresh Lakekeeper
    # warehouse named after the bucket. We then ATTACH that warehouse
    # in DuckDB by name — the bucket-name → warehouse-name identity is
    # the core of how marila bridges the two services.
    bucket_name = f"marila-sales-{run}"
    ns = "sales"
    table_name = "orders"

    with table_bucket(c, bucket_name) as bucket_arn, namespace(c, bucket_arn, ns):
        print(f"CreateTableBucket  {bucket_name}  (marila → Lakekeeper warehouse)")
        print(f"CreateNamespace    {ns}")

        create = c.create_table(
            tableBucketARN=bucket_arn,
            namespace=ns,
            name=table_name,
            format="ICEBERG",
            metadata={
                "iceberg": {
                    "schema": {
                        "fields": [
                            {"name": "order_id", "type": "int", "required": True},
                            {"name": "order_date", "type": "date", "required": True},
                            {"name": "customer_id", "type": "int"},
                            {"name": "region", "type": "string"},
                            {"name": "product", "type": "string"},
                            {"name": "category", "type": "string"},
                            {"name": "quantity", "type": "int"},
                            {"name": "amount_cents", "type": "long"},
                        ]
                    }
                }
            },
        )
        print(f"CreateTable        {table_name}  ARN = {create['tableARN']}")

        try:
            # ----- Data plane via DuckDB through marila's Iceberg proxy -----
            # Read the CSV, insert into the Iceberg table, then run a
            # battery of analytical queries. Each output is printed.
            insert_sql = duckdb_preamble(bucket_name) + f"""
INSERT INTO lake."{ns}".{table_name}
    SELECT
        CAST(order_id AS INT),
        CAST(order_date AS DATE),
        CAST(customer_id AS INT),
        region,
        product,
        category,
        CAST(quantity AS INT),
        CAST(amount_cents AS BIGINT)
    FROM read_csv('{CSV_PATH}', header=true);

SELECT count(*) AS rows FROM lake."{ns}".{table_name};
"""
            print(f"INSERT 1,002 rows from {CSV_PATH.name} via marila's /iceberg proxy …")
            print(run_duckdb(insert_sql).rstrip())

            analytics_sql = duckdb_preamble(bucket_name) + f"""
.echo on

-- Top regions by revenue.
SELECT region, sum(amount_cents)/100.0 AS revenue_usd, count(*) AS orders
  FROM lake."{ns}".{table_name}
 GROUP BY region
 ORDER BY revenue_usd DESC;

-- Top products by quantity sold.
SELECT product, sum(quantity) AS units, sum(amount_cents)/100.0 AS revenue_usd
  FROM lake."{ns}".{table_name}
 GROUP BY product
 ORDER BY units DESC;

-- Monthly revenue trend.
SELECT date_trunc('month', order_date) AS month,
       sum(amount_cents)/100.0 AS revenue_usd,
       count(*) AS orders
  FROM lake."{ns}".{table_name}
 GROUP BY month
 ORDER BY month;
"""
            print("\n=== Analytics on the as-loaded data ===")
            print(run_duckdb(analytics_sql).rstrip())

            cleanup_sql = duckdb_preamble(bucket_name) + f"""
.echo on

-- Bad rows: anything before 2024 / after 2099, or with a non-positive
-- amount. The seed planted exactly two such rows.
SELECT 'pre-cleanup bad-row count' AS step, count(*) AS bad_rows
  FROM lake."{ns}".{table_name}
 WHERE amount_cents <= 0 OR order_date < DATE '2024-01-01' OR order_date >= DATE '2099-01-01';

DELETE FROM lake."{ns}".{table_name}
 WHERE amount_cents <= 0 OR order_date < DATE '2024-01-01' OR order_date >= DATE '2099-01-01';

SELECT 'post-cleanup row count' AS step, count(*) AS rows
  FROM lake."{ns}".{table_name};
"""
            print("\n=== DELETE bad rows ===")
            print(run_duckdb(cleanup_sql).rstrip())

            rebrand_sql = duckdb_preamble(bucket_name) + f"""
.echo on

-- Rename `widgets` → `accessories` to mirror the Athena blog's
-- `Movies_TV` → `Entertainment_Media` rebrand.
SELECT category, count(*) AS rows
  FROM lake."{ns}".{table_name}
 GROUP BY category
 ORDER BY category;

UPDATE lake."{ns}".{table_name}
   SET category = 'accessories'
 WHERE category = 'widgets';

SELECT 'after rebrand' AS step, category, count(*) AS rows
  FROM lake."{ns}".{table_name}
 GROUP BY category
 ORDER BY category;
"""
            print("\n=== UPDATE for category rebrand (widgets → accessories) ===")
            print(run_duckdb(rebrand_sql).rstrip())
        finally:
            # Mirror the Athena tutorial's cleanup: drop the table
            # explicitly (DROP SCHEMA CASCADE isn't supported on
            # Iceberg schemas in duckdb-iceberg today — D-5).
            #
            # We DROP via DuckDB AND then DeleteTable via boto3 — the
            # DuckDB drop is what actually removes the rows + commits
            # the deletion through the Iceberg REST catalog; the boto3
            # call is the AWS-shaped wrapper. Either one alone removes
            # the table from the catalog, so we tolerate NotFound on
            # the second one.
            print(f"\nDeleteTable        {table_name}")
            try:
                run_duckdb(duckdb_preamble(bucket_name) +
                          f'DROP TABLE lake."{ns}".{table_name};')
            except Exception as e:
                print(f"  (DROP TABLE warning: {e})")
            try:
                c.delete_table(tableBucketARN=bucket_arn, namespace=ns, name=table_name)
            except c.exceptions.NotFoundException:
                pass  # DuckDB's DROP already removed it; AWS shape just confirms

    print("\nOK — full sales analytics narrative across boto3 + DuckDB.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
