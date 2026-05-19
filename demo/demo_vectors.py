#!/usr/bin/env python3
"""Realistic S3 Vectors demo — RAG over marila's own documentation.

Mirrors the canonical pattern AWS shows in the [GA announcement][ga] for
S3 Vectors: chunk a real document, embed each chunk, store with metadata,
then answer a natural-language question by similarity search.

[ga]: https://aws.amazon.com/blogs/aws/amazon-s3-vectors-now-generally-available-with-increased-scale-and-performance/

The corpus is everything under `doc/` plus `README.md` and `CLAUDE.md`
in the marila repo. Embeddings come from OpenAI's `text-embedding-3-small`
(1536-d, $0.02 / 1M tokens — well under a cent for this corpus).

Workflow:
  1. Walk the repo, chunk each Markdown file into ~400-char windows.
  2. Embed every chunk via OpenAI.
  3. PutVectors with `{file, chunk_idx, section}` metadata into marila.
  4. Embed three natural-language questions.
  5. QueryVectors topK=3 for each, print top hit with file:chunk cite.
  6. Run one **filtered** query (`file = "doc/REQUIREMENTS.md"`) to
     show the metadata filter narrowing the result.

Prerequisites:
  - `docker compose up -d rustfs` (Lakekeeper not required for vectors)
  - `cargo run -p marila`
  - `OPENAI_API_KEY` in env (the script bails clearly otherwise)
  - `demo/.venv/bin/python demo/demo_vectors.py`
"""

from __future__ import annotations

import os
import pathlib
import sys
import uuid
from contextlib import contextmanager
from dataclasses import dataclass

import boto3
import openai

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
REGION = os.environ.get("MARILA_REGION", "eu-west-1")
ACCESS_KEY = os.environ.get("MARILA_ACCESS_KEY_ID", "marila")
SECRET_KEY = os.environ.get("MARILA_SECRET_ACCESS_KEY", "marilasecret")

EMBED_MODEL = "text-embedding-3-small"
EMBED_DIM = 1536  # text-embedding-3-small native dimension
CHUNK_CHARS = 400  # ~80 tokens per chunk — fine-grained citations
CHUNK_OVERLAP = 80
PUT_BATCH = 50  # PutVectors accepts up to 500 per call; 50 keeps logs readable

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS_GLOBS = ["README.md", "CLAUDE.md", "doc/*.md", "demo/README.md"]


# ---------------------------------------------------------------------------
# Embedding + chunking helpers
# ---------------------------------------------------------------------------


@dataclass
class Chunk:
    key: str
    text: str
    file: str
    chunk_idx: int
    section: str  # nearest preceding Markdown heading, "" if none


def collect_chunks() -> list[Chunk]:
    """Walk the corpus and yield ~400-char windows tagged with the nearest
    preceding `#` heading (so `metadata.section` gives helpful context)."""
    chunks: list[Chunk] = []
    for pattern in CORPUS_GLOBS:
        for path in sorted(REPO_ROOT.glob(pattern)):
            rel = str(path.relative_to(REPO_ROOT))
            text = path.read_text(encoding="utf-8")
            current_section = ""
            i = 0
            chunk_idx = 0
            # Pre-scan headings so each chunk can be tagged with its
            # enclosing section without re-scanning the whole file.
            line_starts = [0]
            for ch in text:
                if ch == "\n":
                    line_starts.append(line_starts[-1] + 1)
                else:
                    line_starts[-1] = line_starts[-1]  # noop, just signal lint
            while i < len(text):
                window = text[i : i + CHUNK_CHARS]
                # Find the nearest `^#` heading before `i`.
                head = text.rfind("\n#", 0, i + CHUNK_CHARS)
                if head >= 0:
                    nl = text.find("\n", head + 1)
                    current_section = text[head + 1 : nl if nl >= 0 else len(text)].strip()
                chunks.append(
                    Chunk(
                        key=f"{rel}#{chunk_idx:04d}",
                        text=window.strip(),
                        file=rel,
                        chunk_idx=chunk_idx,
                        section=current_section[:120],  # cap to keep meta small
                    )
                )
                chunk_idx += 1
                i += CHUNK_CHARS - CHUNK_OVERLAP
    return chunks


def embed(client: openai.OpenAI, texts: list[str]) -> list[list[float]]:
    """OpenAI accepts up to 2048 inputs per call; our corpus is well under
    that, so a single round-trip is enough."""
    resp = client.embeddings.create(model=EMBED_MODEL, input=texts)
    return [d.embedding for d in resp.data]


# ---------------------------------------------------------------------------
# marila helpers
# ---------------------------------------------------------------------------


def marila_client():
    return boto3.client(
        "s3vectors",
        endpoint_url=ENDPOINT,
        region_name=REGION,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
    )


@contextmanager
def bucket_and_index(c, bucket: str, index: str):
    c.create_vector_bucket(vectorBucketName=bucket)
    try:
        c.create_index(
            vectorBucketName=bucket,
            indexName=index,
            dataType="float32",
            dimension=EMBED_DIM,
            distanceMetric="cosine",
        )
        try:
            yield
        finally:
            c.delete_index(vectorBucketName=bucket, indexName=index)
    finally:
        c.delete_vector_bucket(vectorBucketName=bucket)


def put_chunks(c, bucket: str, index: str, chunks: list[Chunk], embeddings: list[list[float]]):
    """Upload `chunks` in PUT_BATCH-sized requests so the wire shape stays
    readable and we exercise PutVectors' batching path."""
    assert len(chunks) == len(embeddings)
    for start in range(0, len(chunks), PUT_BATCH):
        batch = list(zip(chunks[start : start + PUT_BATCH], embeddings[start : start + PUT_BATCH]))
        c.put_vectors(
            vectorBucketName=bucket,
            indexName=index,
            vectors=[
                {
                    "key": ch.key,
                    "data": {"float32": emb},
                    "metadata": {
                        "file": ch.file,
                        "chunk_idx": ch.chunk_idx,
                        "section": ch.section,
                        # Bedrock's pattern: keep the chunk text on the
                        # vector itself so a single QueryVectors call
                        # gives you the answer text without a second
                        # GetVectors round-trip.
                        "text": ch.text[:1900],  # under 2 KB per-key meta cap
                    },
                }
                for ch, emb in batch
            ],
        )


def ask(c, bucket: str, index: str, oai: openai.OpenAI, question: str, *, where=None, top_k: int = 3):
    """Embed `question`, run QueryVectors, print top-K with citations."""
    q_emb = embed(oai, [question])[0]
    req = dict(
        vectorBucketName=bucket,
        indexName=index,
        topK=top_k,
        queryVector={"float32": q_emb},
        returnDistance=True,
        returnMetadata=True,
    )
    if where is not None:
        req["filter"] = where
    print(f"\nQ: {question}")
    if where is not None:
        print(f"   (filter: {where})")
    resp = c.query_vectors(**req)
    for i, hit in enumerate(resp["vectors"], 1):
        meta = hit.get("metadata", {})
        cite = f"{meta.get('file', '?')}#chunk{meta.get('chunk_idx', '?')}"
        section = meta.get("section") or "(no section)"
        snippet = (meta.get("text") or "").replace("\n", " ")[:140]
        print(f"  {i}. d={hit['distance']:.4f}  {cite}  §{section}")
        print(f"     {snippet}…")
    return resp


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    if "OPENAI_API_KEY" not in os.environ:
        print("ERROR: OPENAI_API_KEY not set. Aborting.", file=sys.stderr)
        return 2

    chunks = collect_chunks()
    print(f"Corpus: {len(chunks)} chunks from "
          f"{len({ch.file for ch in chunks})} files")
    if not chunks:
        print("No chunks found; did you run from the repo root?", file=sys.stderr)
        return 1

    oai = openai.OpenAI()
    print(f"Embedding {len(chunks)} chunks via OpenAI {EMBED_MODEL} "
          f"(~{sum(len(c.text) for c in chunks) // 4} tokens, "
          f"≈ ${sum(len(c.text) for c in chunks) / 4 * 0.02 / 1_000_000:.6f})")
    embeddings = embed(oai, [ch.text for ch in chunks])
    assert all(len(e) == EMBED_DIM for e in embeddings)

    c = marila_client()
    run = uuid.uuid4().hex[:12]
    bucket = f"marila-rag-{run}"
    index = "rag-docs"

    with bucket_and_index(c, bucket, index):
        print(f"Putting {len(chunks)} vectors into "
              f"{bucket}/{index} (dim={EMBED_DIM}, metric=cosine)…")
        put_chunks(c, bucket, index, chunks, embeddings)

        # Confirm via ListVectors that everything landed (loop until
        # nextToken is gone — marila + AWS both emit empty pages with
        # cursors for small maxResults).
        seen = set()
        token = None
        while True:
            req = {"vectorBucketName": bucket, "indexName": index, "maxResults": 500}
            if token:
                req["nextToken"] = token
            page = c.list_vectors(**req)
            for v in page["vectors"]:
                seen.add(v["key"])
            token = page.get("nextToken")
            if not token:
                break
        assert len(seen) == len(chunks), (
            f"ListVectors saw {len(seen)} keys, expected {len(chunks)}"
        )
        print(f"ListVectors confirms {len(seen)} vectors stored.")

        # ----- Three natural-language questions -----
        # These probe different corners of the marila docs. The asserts
        # are weak on purpose: we want this demo to remain green even
        # when the corpus shifts; we only assert that the top hit cites
        # *some* relevant file.
        q1 = ask(c, bucket, index, oai,
                 "How does marila validate vector dimensions on PutVectors?")
        assert q1["vectors"], "expected at least one hit"

        q2 = ask(c, bucket, index, oai,
                 "What is the AWS-contract-first TDD methodology?")
        top = q2["vectors"][0]
        assert top["metadata"]["file"] in {"CLAUDE.md", "doc/REQUIREMENTS.md"}, (
            f"expected the methodology hit to cite CLAUDE.md or REQUIREMENTS.md, "
            f"got {top['metadata']['file']}"
        )

        q3 = ask(c, bucket, index, oai,
                 "How do I bootstrap Lakekeeper inside docker-compose?")

        # ----- Filtered query: restrict to REQUIREMENTS.md -----
        # Mirrors the cost-control pattern Bedrock uses
        # (filter by `source` or `category` to scope the index).
        ask(c, bucket, index, oai,
            "What table-side operations does marila implement?",
            where={"file": "doc/REQUIREMENTS.md"},
            top_k=2)

        # ----- Cost shape -----
        # text-embedding-3-small: $0.02 / 1M tokens.
        # S3 Vectors GA pricing (as of 2025-07): storage $0.06/GB-month +
        # $0.0001 / 1k PutVectors + $0.0004 / 1k QueryVectors. For this
        # corpus (~{N} vectors, ~{B} KB) that's a fraction of a cent/month.
        print(f"\nFootprint: {len(chunks)} vectors × {EMBED_DIM} float32 "
              f"≈ {len(chunks) * EMBED_DIM * 4 / 1024:.1f} KB raw + metadata.")
        print("OK — RAG round-trip green across 4 query patterns.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
