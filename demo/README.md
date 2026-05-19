# marila demos — realistic workflows

Three demos that satisfy the acceptance criteria in
[`doc/REQUIREMENTS.md`](../doc/REQUIREMENTS.md) §9, designed to mirror
the patterns AWS shows in its own blog posts rather than just proving
the wire works:

- **[`demo_vectors.sh`](demo_vectors.sh)** — RAG over marila's own docs,
  driven by the in-tree [`marila-embed`](../crates/embed_cli/) CLI.
  Walks `README.md`, `CLAUDE.md`, and `doc/*.md`; markdown-aware
  chunking; embeds via OpenAI's `text-embedding-3-small`; auto-creates
  the index; then answers three natural-language questions (one
  metadata-filtered) and renders the top-3 hits as a table.
  - Pattern from the [S3 Vectors GA announcement][vectors-ga] (chunk
    a PDF, embed, answer with citations).
  - The earlier `demo_vectors.py` is preserved at the same path for now
    in case you want to compare implementations; it is no longer part
    of the §9 acceptance demos.
- **[`demo_tables.py`](demo_tables.py)** — sales analytics. Loads a
  deterministic 1,002-row synthetic CSV into an Iceberg table backed
  by Lakekeeper, runs aggregate queries (top region by revenue, top
  product by units, monthly trend), `DELETE`s the two intentionally-
  bad rows, and `UPDATE`s a category rebrand (`widgets` → `accessories`).
  - Pattern from [Transform your data to Amazon S3 Tables with
    Amazon Athena][tables-athena] (insert real dataset → analytical
    SQL → DELETE bad rows → UPDATE rebrand).
- **[`lakekeeper_verify.sql`](lakekeeper_verify.sql)** — standalone
  Iceberg-via-DuckDB smoke (3 rows, CREATE / INSERT / UPDATE / DELETE).
  Useful for diagnosing the `/iceberg/v1/*` reverse-proxy without
  needing Python.

### Bonus: agentic RAG over a local PDF corpus

- **[`index_parlis.sh`](index_parlis.sh) + [`parlis_chat.py`](parlis_chat.py)** —
  point `PARLIS_DIR` at any local directory of PDFs (no path is baked
  in), index them via `marila-embed put` with **local Ollama**
  (`embeddinggemma:latest` for embeddings, default 768-d, free + private),
  then chat with an agentic loop on top:

  ```bash
  # one-time: index ~20k chunks under parlis/drucksachen
  PARLIS_DIR=~/parlis/pdfs bash demo/index_parlis.sh

  # then chat — the model decides when / how often to search
  python demo/parlis_chat.py
  ```

  The chat model (default `gpt-oss:latest`) is given one tool,
  `search_parlis(query, k)`, and an agentic prompt: refine its own
  queries, search multiple times per turn, cite by source path. Inside
  `parlis_chat.py` slash-commands `/sources` (last citations),
  `/verbose` (show tool calls inline), `/reset`, `/model <name>`,
  `/k <n>`, `/quit`. See the docstring for env-var knobs.

[vectors-ga]: https://aws.amazon.com/blogs/aws/amazon-s3-vectors-now-generally-available-with-increased-scale-and-performance/
[tables-athena]: https://aws.amazon.com/blogs/big-data/transform-your-data-to-amazon-s3-tables-with-amazon-athena/

## Prerequisites

```bash
docker compose --profile lakekeeper up -d
cargo build -p marila && ./target/debug/marila &

cd demo
uv venv .venv
uv pip install -e .

# For the RAG demo only:
export OPENAI_API_KEY=sk-...
```

## Running

```bash
bash demo/demo_vectors.sh                      # RAG over marila docs (CLI)
demo/.venv/bin/python demo/demo_tables.py      # sales analytics
duckdb < demo/lakekeeper_verify.sql            # minimal SQL smoke
```

Each is self-contained and cleans up on exit. The sales CSV is
regenerated automatically if missing (`sales_seed.py`, deterministic
with `random.seed(42)`).

## Sample output (truncated)

`demo_vectors.py`:

```
Corpus: 217 chunks from 6 files
Embedding 217 chunks via OpenAI text-embedding-3-small (~21319 tokens, ≈ $0.000426)
Putting 217 vectors into marila-rag-…/rag-docs (dim=1536, metric=cosine)…
ListVectors confirms 217 vectors stored.

Q: How does marila validate vector dimensions on PutVectors?
  1. d=0.4716  CLAUDE.md#chunk77  §## What's done
  2. d=0.4733  CLAUDE.md#chunk37  §### C-2e — Data-plane wire shapes …
  3. d=0.4874  CLAUDE.md#chunk34  §### C-2e — Data-plane wire shapes …
```

`demo_tables.py`:

```
CreateTableBucket  marila-sales-…  (marila → Lakekeeper warehouse)
CreateTable        orders  ARN = arn:aws:s3tables:…/table/…
INSERT 1,002 rows from sales_seed.csv via marila's /iceberg proxy …

=== Analytics on the as-loaded data ===
┌──────────┬─────────────┬────────┐
│  region  │ revenue_usd │ orders │
├──────────┼─────────────┼────────┤
│ us-east  │     2103.45 │    258 │
│ eu-west  │     1987.32 │    241 │
│ ap-south │     1934.18 │    253 │
│ us-west  │     1854.91 │    250 │
└──────────┴─────────────┴────────┘
…
=== DELETE bad rows ===
pre-cleanup bad-row count: 2
post-cleanup row count:    1000

=== UPDATE for category rebrand (widgets → accessories) ===
before:  widgets=326, gadgets=316, tools=358
after:   accessories=326, gadgets=316, tools=358
```

## Gotchas (so you don't re-discover the discoveries)

- The DuckDB demos set `ENDPOINT 'localhost:9000'` on the S3 secret
  because they run from the host. Lakekeeper itself writes via the
  docker-network alias `http://rustfs:9000` — same RustFS instance,
  two valid hostnames. See `CLAUDE.md` C-6 and `doc/DISCOVERIES.md` D-2.
- `ACCESS_DELEGATION_MODE 'none'` + `AUTHORIZATION_TYPE 'none'` on the
  ATTACH are both required: the former so DuckDB uses our `TYPE s3`
  secret for data writes (D-1), the latter so it doesn't try to fetch
  an OAuth2 token from the catalog (Lakekeeper runs `allow-all` authz).
- `DROP SCHEMA … CASCADE` isn't supported on Iceberg schemas yet — drop
  tables individually first (D-5).
- `demo_tables.py` deliberately calls `c.delete_table(...)` after
  DuckDB's `DROP TABLE` and tolerates the resulting `NotFoundException`
  — both ops remove the table; the boto3 call is the AWS-shape
  confirmation, but the actual commit goes through Iceberg REST.
