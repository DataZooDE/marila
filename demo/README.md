# marila demos

Three end-to-end demos that satisfy the acceptance criteria in
[`doc/REQUIREMENTS.md`](../doc/REQUIREMENTS.md) §9.

## Prerequisites

Bring up the full stack and the marila binary:

```bash
docker compose --profile lakekeeper up -d
cargo build -p marila
./target/debug/marila &
```

Then install the Python deps (`boto3` ≥ 1.41 ships the `s3vectors` and
`s3tables` clients):

```bash
cd demo
uv venv .venv
uv pip install -e .
```

## The three demos

| Demo | What it shows |
| --- | --- |
| `demo_vectors.py` | Round-trips s3vectors via boto3. Creates a bucket + cosine index, puts four vectors with metadata, runs an unfiltered top-3 query then a filtered (`label=target`) top-3, asserts the anchor leads both. |
| `demo_tables.py`  | Round-trips s3tables via boto3. Creates a bucket → namespace → table (ICEBERG schema), reads back `metadataLocation` + `warehouseLocation`, lists, then deletes. |
| `lakekeeper_verify.sql` | Drives the **Iceberg side** end-to-end via DuckDB's `iceberg` extension against marila's `/iceberg/v1/*` reverse-proxy: CREATE → INSERT → SELECT → UPDATE → DELETE. |

Run them:

```bash
demo/.venv/bin/python demo/demo_vectors.py
demo/.venv/bin/python demo/demo_tables.py
duckdb < demo/lakekeeper_verify.sql
```

Each is self-contained and cleans up on exit.

## Gotchas (so you don't re-discover the discoveries)

- The DuckDB demo's S3 SECRET sets `ENDPOINT 'localhost:9000'` because
  it runs from the host. Lakekeeper itself writes via the docker-network
  alias `http://rustfs:9000` — same RustFS instance, two valid hostnames.
  See `CLAUDE.md` C-6, `doc/DISCOVERIES.md` D-2.
- `ACCESS_DELEGATION_MODE 'none'` + `AUTHORIZATION_TYPE 'none'` on the
  ATTACH are both required: the former so DuckDB uses our `TYPE s3` secret
  for data writes (D-1), the latter so it doesn't try to fetch an OAuth2
  token from the catalog (Lakekeeper runs in `allow-all` authz).
- `DROP SCHEMA … CASCADE` isn't supported on Iceberg schemas yet — drop
  tables individually first (D-5).
