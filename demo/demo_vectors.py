#!/usr/bin/env python3
"""Round-trip an s3vectors workflow through marila.

REQUIREMENTS.md §9 step 6: "observe a top-3 cosine query with a metadata
filter returning the seeded 'anchor' vector".

Usage (with `docker compose up -d rustfs && cargo run -p marila`):

    cd demo && .venv/bin/python demo_vectors.py

The script is self-contained — it creates a fresh bucket + index with a
UUID suffix so re-runs don't collide, runs the round-trip, and cleans up
on exit (even on assertion failure).
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

DIM = 4


def client():
    return boto3.client(
        "s3vectors",
        endpoint_url=ENDPOINT,
        region_name=REGION,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
    )


@contextmanager
def bucket_and_index(c, bucket: str, index: str):
    """Create + yield a fresh bucket/index; tear them down on exit."""
    c.create_vector_bucket(vectorBucketName=bucket)
    try:
        c.create_index(
            vectorBucketName=bucket,
            indexName=index,
            dataType="float32",
            dimension=DIM,
            distanceMetric="cosine",
        )
        try:
            yield
        finally:
            c.delete_index(vectorBucketName=bucket, indexName=index)
    finally:
        c.delete_vector_bucket(vectorBucketName=bucket)


def main() -> int:
    c = client()
    run = uuid.uuid4().hex[:12]
    bucket = f"marila-demo-{run}"
    index = "demo-cosine"

    with bucket_and_index(c, bucket, index):
        # Seed 4 vectors: an "anchor" near [1,0,0,0] plus three others.
        # The metadata filter `{"label":"target"}` should match only the
        # anchor and one near-neighbour — top-3 unfiltered would include
        # all three, but with the filter the anchor must come back first.
        c.put_vectors(
            vectorBucketName=bucket,
            indexName=index,
            vectors=[
                {
                    "key": "anchor",
                    "data": {"float32": [1.0, 0.0, 0.0, 0.0]},
                    "metadata": {"label": "target", "tier": 1},
                },
                {
                    "key": "near-target",
                    "data": {"float32": [0.9, 0.1, 0.0, 0.0]},
                    "metadata": {"label": "target", "tier": 2},
                },
                {
                    "key": "near-other",
                    "data": {"float32": [0.95, 0.05, 0.0, 0.0]},
                    "metadata": {"label": "noise", "tier": 1},
                },
                {
                    "key": "far",
                    "data": {"float32": [0.0, 0.0, 0.0, 1.0]},
                    "metadata": {"label": "target", "tier": 3},
                },
            ],
        )

        # Unfiltered top-3: the closest three vectors by cosine distance.
        unfiltered = c.query_vectors(
            vectorBucketName=bucket,
            indexName=index,
            topK=3,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]},
            returnDistance=True,
            returnMetadata=True,
        )
        print(f"distanceMetric: {unfiltered['distanceMetric']}")
        print("Unfiltered top-3 (closest first):")
        for v in unfiltered["vectors"]:
            print(f"  {v['key']:<12} d={v['distance']:.6f} meta={v.get('metadata')}")
        assert unfiltered["vectors"][0]["key"] == "anchor", (
            "anchor must be the nearest neighbour"
        )

        # Filtered top-3: only `label == target` vectors.
        filtered = c.query_vectors(
            vectorBucketName=bucket,
            indexName=index,
            topK=3,
            queryVector={"float32": [1.0, 0.0, 0.0, 0.0]},
            filter={"label": "target"},
            returnDistance=True,
            returnMetadata=True,
        )
        print("Filtered (label=target) top-3:")
        for v in filtered["vectors"]:
            print(f"  {v['key']:<12} d={v['distance']:.6f} meta={v.get('metadata')}")
        keys = [v["key"] for v in filtered["vectors"]]
        assert keys[0] == "anchor", "anchor must lead the filtered result"
        assert "near-other" not in keys, (
            "metadata filter must exclude `label != target`"
        )
        print("OK — anchor leads the filtered query")
    return 0


if __name__ == "__main__":
    sys.exit(main())
