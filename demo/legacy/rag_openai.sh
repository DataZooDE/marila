#!/usr/bin/env bash
#
# Realistic S3 Vectors demo — RAG over marila's own documentation,
# driven entirely by the in-tree marila-embed CLI.
#
# Workflow:
#   1. marila-embed put — walks doc/, README.md, doc/DISCOVERIES.md; markdown-aware
#      chunking; embeds via OpenAI text-embedding-3-small; writes the
#      vectors into the auto-created index with the S3VECTORS-EMBED-*
#      metadata envelope (plus marila.section_path on every markdown chunk).
#   2. marila-embed query — embeds three natural-language questions and
#      renders the top-3 hits as a table.
#
# Prerequisites:
#   - docker compose up -d rustfs
#   - cargo run -p marila        (in another terminal)
#   - OPENAI_API_KEY in the environment
#
# Run from the repo root:
#   bash demo/demo_vectors.sh

set -euo pipefail

EMBED="${EMBED:-./target/debug/marila-embed}"
BUCKET="${BUCKET:-marila-rag-$(date +%s)}"
INDEX="${INDEX:-rag-docs}"

if ! [[ -x "$EMBED" ]]; then
    cargo build -p marila-embed >&2
fi

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
    echo "OPENAI_API_KEY not set — bailing." >&2
    exit 1
fi

# AWS SDK requires *some* credentials to sign — marila ignores their
# value but does check they're present. Use the local dummy creds.
export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-marila}"
export AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-marilasecret}"

echo "==> put marila docs into $BUCKET/$INDEX"
"$EMBED" put \
    --vector-bucket-name "$BUCKET" \
    --index-name "$INDEX" \
    --embedding-provider openai \
    --embedding-model text-embedding-3-small \
    --chunk-strategy markdown \
    --chunk-size 400 \
    --chunk-overlap 80 \
    --text README.md \
    --text doc/DISCOVERIES.md \
    --text "doc/*.md"

echo
echo "==> query: How does marila validate vector dimensions on PutVectors?"
"$EMBED" query \
    --vector-bucket-name "$BUCKET" \
    --index-name "$INDEX" \
    --embedding-provider openai \
    --embedding-model text-embedding-3-small \
    --text-value "How does marila validate vector dimensions on PutVectors?" \
    --k 3 \
    --output table

echo
echo "==> query: What's the wire shape of CreateVectorBucket?"
"$EMBED" query \
    --vector-bucket-name "$BUCKET" \
    --index-name "$INDEX" \
    --embedding-provider openai \
    --embedding-model text-embedding-3-small \
    --text-value "What is the wire shape of CreateVectorBucket?" \
    --k 3 \
    --output table

echo
echo "==> query (metadata-filtered to doc/REQUIREMENTS.md):"
echo "         What does requirement FV-7 say about NotImplementedException?"
"$EMBED" query \
    --vector-bucket-name "$BUCKET" \
    --index-name "$INDEX" \
    --embedding-provider openai \
    --embedding-model text-embedding-3-small \
    --text-value "What does requirement FV-7 say about NotImplementedException?" \
    --k 3 \
    --output table \
    --filter '{"S3VECTORS-EMBED-SRC-LOCATION":{"$eq":"doc/REQUIREMENTS.md"}}' \
    || true   # filtered query may legitimately return 0 if the only matches
              # live under a different SRC-LOCATION naming convention

echo
echo "==> cleanup"
aws s3vectors delete-index \
    --endpoint-url "$MARILA_ENDPOINT" --region "${MARILA_REGION:-eu-west-1}" \
    --vector-bucket-name "$BUCKET" --index-name "$INDEX" 2>/dev/null || true
aws s3vectors delete-vector-bucket \
    --endpoint-url "$MARILA_ENDPOINT" --region "${MARILA_REGION:-eu-west-1}" \
    --vector-bucket-name "$BUCKET" 2>/dev/null || true

echo "OK — RAG demo complete."
