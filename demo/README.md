# marila demos

Two end-to-end TUI demos showcasing marila's two API surfaces, plus a
`legacy/` folder with the earlier one-shot narrative scripts. Each TUI
shares the same widget library (`shared/`) so the look and feel
matches.

```
demo/
├── shared/    Splitter widget + pivot-SQL builder reused by both TUIs
├── vector/    agentic RAG over a local PDF corpus (s3-vectors surface)
├── tables/    NYC Yellow Taxi pivot / slice-dice (s3-tables surface)
└── legacy/    older one-shot scripts (sales_demo.py, rag_openai_demo.py, …)
```

## Setup (once)

```bash
cd demo
uv venv .venv
uv pip install -e .
```

Both TUIs default to a local Ollama for chat + embeddings. The
tables TUI can also drive **OpenAI** or **Gemini** via
`CHAT_PROVIDER=openai|gemini` (see "Non-local chat models" below).

| Role | Default | Override |
|---|---|---|
| Chat provider | `ollama` (local) | `CHAT_PROVIDER={ollama,openai,gemini}` |
| Chat model | `gemma4:latest` (ollama) / `gpt-5.4-mini` (openai) / `gemini-3.5-flash` (gemini) | `CHAT_MODEL=…` |
| Embeddings (vector demo only) | `embeddinggemma:latest` | `EMBED_MODEL=...` |

### Non-local chat models (tables TUI)

The agent goes through a thin adapter so OpenAI / Gemini / Ollama
all expose the same `chat(model, messages, tools)` surface. Pick by
exporting `CHAT_PROVIDER` before launching the TUI:

```bash
# OpenAI (requires OPENAI_API_KEY)
CHAT_PROVIDER=openai cd demo && uv run python -m tables.chat

# Gemini (requires GEMINI_API_KEY; goes through Google's OpenAI-
# compat endpoint at /v1beta/openai/)
CHAT_PROVIDER=gemini cd demo && uv run python -m tables.chat

# Override the default model for either provider:
CHAT_PROVIDER=openai CHAT_MODEL=gpt-5.4 uv run python -m tables.chat
CHAT_PROVIDER=gemini CHAT_MODEL=gemini-3.5-pro uv run python -m tables.chat
```

The TUI keybinds and slash commands behave identically across
providers — `/model NEW_NAME` switches model mid-session within the
current provider. To switch *provider* mid-session, restart with a
different `CHAT_PROVIDER`.

The vector TUI is still Ollama-only — it shares the embedding model
with marila-embed and there's no OpenAI/Gemini embedding path wired
in yet.

## 1. `vector/` — agentic RAG over a local PDF corpus

Works against **either** marila variant:

- `cargo run -p marila --features embedded-rustfs` — single binary, no docker
- or `docker compose up -d rustfs && cargo run -p marila` — sidecar

Index your corpus once (point `PARLIS_DIR` at any directory of PDFs —
nothing is baked in):

```bash
PARLIS_DIR=~/parlis/pdfs MAX_CHUNKS=20000 bash demo/vector/index.sh
```

Then chat:

```bash
cd demo && uv run python -m vector.chat
```

You get a three-pane TUI: chat / hops verbose / source list, with
`p` to preview a source's full text in a modal, `F2`-`F4` to focus
panes, `F6`-`F9` (or mouse-drag the splitters) to resize, `↑`/`↓` to
walk through input history. See the chat module's docstring for env
knobs (`BUCKET`, `INDEX`, `MARILA_ENDPOINT`, …).

## 2. `tables/` — NYC Yellow Taxi pivot / slice-dice

Requires the **full docker compose stack** (Lakekeeper + Postgres +
RustFS) — embedded-RustFS won't work here because the Lakekeeper
container can't reach the ephemeral 127.0.0.1 port that the embedded
RustFS binds.

```bash
docker compose --profile lakekeeper up -d
cargo run -p marila &                              # without --features
```

Load NYC Yellow Taxi (default: Q1 2024 ~9M rows, ~3 min;
parquet is cached at `~/.cache/marila-taxi/`). The loader also caches
TLC's tiny zone-lookup CSV in the same dir — the agent reads it to
materialize `pickup_borough` / `dropoff_borough` etc. as real columns:

```bash
bash demo/tables/load.sh                           # default: Q1 2024
TAXI_MONTHS=2024-01 bash demo/tables/load.sh       # single month, ~1 min
TAXI_MONTHS=2024-01,2024-02,2024-03,2024-04,2024-05,2024-06 \
  bash demo/tables/load.sh                         # full H1, ~10 min
```

Then chat:

```bash
cd demo && uv run python -m tables.chat
```

You get a similar three-pane TUI: chat / hops verbose / **controls**.

The controls pane has three lists — **all dimensions**, **rows**, and
**cols** — plus a measure dropdown and an optional WHERE input. Each
turn the agent ran, the **chat pane also lists every query inline** with
its SQL + a compact result preview, numbered `[1] [2] [3] …`; press the
matching digit (with chat focused via `F3`) or `/show N` from the input
to pop the full result in a modal.

Pivot-controls keymap (focus the controls with `F4`):

| Key | Where | Effect |
|---|---|---|
| `j` / `k` | any of the three lists | navigate ↓ / ↑ (vim) |
| `r` | all-dims list | add highlighted dim to rows |
| `c` | all-dims list | add highlighted dim to cols |
| `d` or `del` | rows / cols list | remove highlighted dim |
| `J` / `K` (shift) | rows / cols list | reorder ±1 (= change hierarchy depth) |
| `F5` | anywhere | run the pivot |

The order in the rows list IS the hierarchy depth, so position 1 is the
outermost rollup level (e.g. day-of-week parent) and position 2 is the
inner level (e.g. hour-of-day child). `F5` runs the pivot **directly**
through DuckDB without involving the LLM — same `build_pivot_sql` that
the LLM's `pivot` tool calls, so the SQL is identical either way.

The agent has three tools: `schema_lookup`, `pivot(rows, cols?,
measure, where?)`, `run_sql(sql)` (read-only).

The agent's DuckDB session always exposes a view called **`taxi`**
that materializes every pivot dimension as a real column AND
LEFT-JOINs the TLC zone lookup. So `run_sql` queries against `taxi`
can reference `day_of_week`, `hour_of_day`, `pickup_borough`,
`pickup_zone`, `dropoff_borough`, `dropoff_zone`, `payment_method`,
`passenger_bucket`, `trip_distance_bucket` directly — no need to
hand-write `dayname(tpep_pickup_datetime)` or the zone JOIN. The
raw Iceberg table `lake.nyc.yellow` is also accessible for cases
that need the un-enriched columns.

Sample interactions:

```
you> trips per borough by day of week
  → schema_lookup
  → pivot(rows="day_of_week", cols="pickup_location_id", measure="trip_count")
  → 7-row markdown table

you> /sql SELECT vendorid, count(*) FROM lake.nyc.yellow GROUP BY 1
  → raw SQL escape hatch
```

Slash commands: `/sql`, `/show N` (drill into the Nth query of the
latest turn), `/schema`, `/reset`, `/clear`, `/model`, `/help`,
`/quit`. `p` on the verbose pane opens a modal with the full SQL +
EXPLAIN of the latest query.

## 3. `legacy/` — earlier one-shot narratives

Kept verbatim for comparison; not the primary demos.

- `legacy/rag_openai.sh` — RAG over marila's own docs via the
  `marila-embed` CLI + OpenAI embeddings. Pattern from the
  [S3 Vectors GA announcement][vectors-ga].
- `legacy/rag_openai_demo.py` — the same workflow expressed inline
  in Python (no CLI).
- `legacy/sales_demo.py` — 1,002-row synthetic sales analytics:
  `CREATE` / `INSERT` / `SELECT` / `DELETE` / `UPDATE` on an Iceberg
  table via marila's `/iceberg` proxy. Pattern from
  [Transform your data to S3 Tables with Athena][tables-athena].
- `legacy/lakekeeper_verify.sql` — 3-row standalone DuckDB smoke for
  the `/iceberg/v1/*` proxy, no Python needed.

```bash
demo/.venv/bin/python demo/legacy/sales_demo.py
duckdb < demo/legacy/lakekeeper_verify.sql
```

[vectors-ga]: https://aws.amazon.com/blogs/aws/amazon-s3-vectors-now-generally-available-with-increased-scale-and-performance/
[tables-athena]: https://aws.amazon.com/blogs/big-data/transform-your-data-to-amazon-s3-tables-with-amazon-athena/

## Gotchas (so you don't re-discover the discoveries)

- The DuckDB ATTACH always uses `ENDPOINT 'localhost:9000'` on the S3
  secret because the demos run from the host. Lakekeeper-in-docker
  itself writes via the docker-network alias `http://rustfs:9000` —
  same RustFS instance, two valid hostnames. CLAUDE.md C-6 / D-2.
- `ACCESS_DELEGATION_MODE 'none'` + `AUTHORIZATION_TYPE 'none'` on
  the ATTACH are both required: the former so DuckDB uses our
  `TYPE s3` secret for data writes (D-1), the latter so it doesn't
  fetch OAuth2 from the catalog (Lakekeeper runs allow-all).
- `DROP SCHEMA … CASCADE` isn't supported on Iceberg schemas yet —
  drop tables individually first (D-5).
- Iceberg WRITE through marila's proxy needs DuckDB ≥ **1.5.3** (the
  loader probe checks). `iceberg_schema_properties`, `ALTER TABLE`,
  `MERGE INTO` all became stable in that release —
  see https://duckdb.org/2026/05/20/announcing-duckdb-153.
