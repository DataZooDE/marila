#!/usr/bin/env python3
"""Round-trip an s3tables workflow through marila.

REQUIREMENTS.md §9 step 4: "observe rows round-trip through marila's
AWS-JSON façade". Exercises CreateTableBucket → CreateNamespace →
CreateTable → GetTable → GetTableMetadataLocation → cleanup.

The Iceberg INSERT/SELECT side is covered by `lakekeeper_verify.sql`
because boto3 doesn't drive Iceberg data writes — DuckDB does, via the
`/iceberg/v1/*` reverse-proxy this script doesn't exercise.

Usage (with `docker compose --profile lakekeeper up -d && cargo run -p marila`):

    cd demo && .venv/bin/python demo_tables.py
"""

from __future__ import annotations

import os
import sys
import uuid
from contextlib import contextmanager

import boto3

ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
REGION = os.environ.get("MARILA_REGION", "eu-west-1")
ACCESS_KEY = os.environ.get("MARILA_ACCESS_KEY_ID", "marila")
SECRET_KEY = os.environ.get("MARILA_SECRET_ACCESS_KEY", "marilasecret")


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
    resp = c.create_table_bucket(name=name)
    arn = resp["arn"]
    try:
        yield arn
    finally:
        c.delete_table_bucket(tableBucketARN=arn)


@contextmanager
def namespace(c, arn: str, ns: str):
    c.create_namespace(tableBucketARN=arn, namespace=[ns])
    try:
        yield
    finally:
        c.delete_namespace(tableBucketARN=arn, namespace=ns)


def main() -> int:
    c = client()
    run = uuid.uuid4().hex[:12]
    bucket = f"marila-demo-{run}"
    ns = "demo_ns"
    table_name = "orders"

    with table_bucket(c, bucket) as arn:
        print(f"CreateTableBucket  arn = {arn}")

        listed = c.list_table_buckets()
        names = [b["name"] for b in listed["tableBuckets"]]
        assert bucket in names, f"bucket missing from ListTableBuckets: {names}"

        got_bucket = c.get_table_bucket(tableBucketARN=arn)
        print(f"GetTableBucket     type = {got_bucket['type']}, "
              f"id = {got_bucket['tableBucketId']}")

        with namespace(c, arn, ns):
            print(f"CreateNamespace    namespace = [{ns}]")

            create_table = c.create_table(
                tableBucketARN=arn,
                namespace=ns,
                name=table_name,
                format="ICEBERG",
                metadata={
                    "iceberg": {
                        "schema": {
                            "fields": [
                                {"name": "id", "type": "int", "required": True},
                                {"name": "customer", "type": "string"},
                                {"name": "amount_cents", "type": "long"},
                            ]
                        }
                    }
                },
            )
            print(f"CreateTable        tableARN     = {create_table['tableARN']}")
            print(f"                   versionToken = {create_table['versionToken']}")

            try:
                got_table = c.get_table(
                    tableBucketARN=arn, namespace=ns, name=table_name
                )
                print(f"GetTable           format          = {got_table['format']}")
                print(f"                   metadataLocation  = {got_table['metadataLocation']}")
                print(f"                   warehouseLocation = {got_table['warehouseLocation']}")
                assert got_table["format"] == "ICEBERG"
                assert got_table["metadataLocation"].endswith(".metadata.json")

                mloc = c.get_table_metadata_location(
                    tableBucketARN=arn, namespace=ns, name=table_name
                )
                print(f"GetTableMetadataLocation -> {mloc['metadataLocation']}")
                assert mloc["metadataLocation"] == got_table["metadataLocation"]

                listed_tables = c.list_tables(tableBucketARN=arn, namespace=ns)
                tnames = [t["name"] for t in listed_tables["tables"]]
                assert table_name in tnames
                print(f"ListTables         tables = {tnames}")
            finally:
                c.delete_table(
                    tableBucketARN=arn, namespace=ns, name=table_name
                )
                print(f"DeleteTable        OK")
    print("\nOK — full s3tables round-trip through marila")
    return 0


if __name__ == "__main__":
    sys.exit(main())
