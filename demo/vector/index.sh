#!/usr/bin/env bash
#
# Index a local directory of PDFs into marila vectors using the in-tree
# `marila-embed` CLI with local Ollama (embeddinggemma:latest) so the
# whole pipeline is free + private.
#
# The companion `demo/parlis_chat.py` queries this index for agentic RAG.
#
# Required:
#   PARLIS_DIR      — directory of PDFs to ingest (NOT committed; point
#                     this at your own local copy of any PDF corpus)
#
# Optional (with sane defaults):
#   BUCKET          — vector bucket name (default: parlis)
#   INDEX           — index name within the bucket (default: drucksachen)
#   MAX_CHUNKS      — cap on chunks emitted; 0 = no cap (default: 20000)
#   MAX_FILE_BYTES  — skip files larger than this (default: 50 MiB)
#   CHUNK_SIZE      — chunk size in tokens (default: 400)
#   CHUNK_OVERLAP   — overlap in tokens (default: 80)
#   PARSE_WORKERS   — parse-pool concurrency (default: cpus / 2)
#   OLLAMA_ENDPOINT — defaults to http://localhost:11434
#   EMBED_MODEL     — embedding model (default: embeddinggemma:latest)
#
# Prerequisites:
#   - docker compose up -d rustfs                 (marila storage)
#   - cargo run -p marila                         (façade on :8080)
#   - ollama serve + `ollama pull embeddinggemma`
#
# Run from the marila repo root:
#   PARLIS_DIR=~/parlis/pdfs bash demo/index_parlis.sh

set -euo pipefail

: "${PARLIS_DIR:?set PARLIS_DIR to your local directory of PDFs}"
[[ -d "$PARLIS_DIR" ]] || { echo "PARLIS_DIR=$PARLIS_DIR is not a directory" >&2; exit 1; }

# Resolve the workspace root from this script's location so the binary
# path doesn't depend on the caller's cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EMBED="${EMBED:-$REPO_ROOT/target/debug/marila-embed}"

# lopdf is loud about pages without a ToUnicode CMap — common in older
# German PDFs that embed TrueType fonts. Hide its per-page warnings by
# default; the pipeline summary still reports the cumulative
# parse-failure count. Override RUST_LOG to dig in for a specific run.
export RUST_LOG="${RUST_LOG:-info,lopdf=error,marila_embed::parse::pdf=error}"
BUCKET="${BUCKET:-parlis}"
INDEX="${INDEX:-drucksachen}"
MAX_CHUNKS="${MAX_CHUNKS:-20000}"
MAX_FILE_BYTES="${MAX_FILE_BYTES:-$((50 * 1024 * 1024))}"
CHUNK_SIZE="${CHUNK_SIZE:-400}"
CHUNK_OVERLAP="${CHUNK_OVERLAP:-80}"
PARSE_WORKERS="${PARSE_WORKERS:-$(( $(nproc 2>/dev/null || echo 4) / 2 ))}"
EMBED_MODEL="${EMBED_MODEL:-embeddinggemma:latest}"
CHECKPOINT="${CHECKPOINT:-./.parlis-checkpoint.jsonl}"

# Make sure the binary exists.
[[ -x "$EMBED" ]] || (cd "$REPO_ROOT" && cargo build -p marila-embed)

# marila ignores SigV4 but the SDK still demands creds; supply dummies.
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-marila}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-marilasecret}"

# Make sure the bucket exists (idempotent — 409 means it's already there
# and we move on).
aws s3vectors create-vector-bucket \
    --endpoint-url "${MARILA_ENDPOINT:-http://localhost:8080}" \
    --region "${MARILA_REGION:-eu-west-1}" \
    --vector-bucket-name "$BUCKET" 2>/dev/null || true

echo "==> indexing $PARLIS_DIR into $BUCKET/$INDEX"
echo "    embed: $EMBED_MODEL  chunk: $CHUNK_SIZE/$CHUNK_OVERLAP  cap: $MAX_CHUNKS chunks"
echo "    checkpoint: $CHECKPOINT (resumable — re-run skips done sources)"

exec "$EMBED" put \
    --vector-bucket-name "$BUCKET" \
    --index-name "$INDEX" \
    --embedding-provider ollama \
    --embedding-model "$EMBED_MODEL" \
    --chunk-strategy sentence \
    --chunk-size "$CHUNK_SIZE" \
    --chunk-overlap "$CHUNK_OVERLAP" \
    --text "$PARLIS_DIR" \
    --include pdf \
    --max-file-bytes "$MAX_FILE_BYTES" \
    --max-chunks "$MAX_CHUNKS" \
    --parse-concurrency "$PARSE_WORKERS" \
    --embed-concurrency 4 \
    --embed-batch 32 \
    --put-batch 250 \
    --checkpoint "$CHECKPOINT"
