# Discoveries — what hypothesis testing taught us

> **Marila** — see `REQUIREMENTS.md` / `ARCHITECTURE.md` for the project
> introduction and naming. This file is the lessons-learned log
> regardless of branding.

Status: v0.1, 2026-05-16. Author: Claude.

A "brief the next marila coding agent so they don't have to re-discover
any of this" log. Every entry has the symptom, the root cause, and the
fix. Sourced from `VERIFICATION.md`, `GAP_ANALYSIS.md`, the original
research memos in `research/`, and live tests done during the spike.

The entries are roughly ordered by how badly each one will trip
someone up if they don't know it.

---

## D-1 ★ DuckDB-iceberg writes to a self-hosted REST catalog need `ACCESS_DELEGATION_MODE 'none'`

**Symptom.** `INSERT INTO lake.x.y …` against an attached Lakekeeper
(or Polaris, or Nessie) returns `403 AccessDenied` from MinIO/RustFS.
Tracing the request shows DuckDB sent the data-file `PUT` with **no
`Authorization` header at all** — not a SigV4 mismatch, just unsigned.

**Root cause.** By default, DuckDB-iceberg asks the catalog for vended
credentials and uses those (or fails). Our `CREATE SECRET … TYPE s3` is
ignored on the data-file writer path. This is
[duckdb-iceberg#594](https://github.com/duckdb/duckdb-iceberg/issues/594).

**Fix.** One ATTACH option:

```sql
ATTACH 'demo' AS lake (
    TYPE iceberg,
    SECRET lake_rest,
    ENDPOINT 'http://localhost:8181/catalog',
    ACCESS_DELEGATION_MODE 'none'   -- <- this
);
```

With it set, DuckDB falls back to the user's `TYPE s3` secret for all
S3 I/O. Issue is closed; fix is permanent. See `demo/lakekeeper_verify.sql`.

## D-2 ★ The S3 SECRET's `ENDPOINT` must match the host Lakekeeper returns, not `localhost`

**Symptom.** Even with D-1 fixed, writes still 403 if the S3 secret
endpoint is `localhost:9000` while Lakekeeper returns the warehouse URL
as `http://rustfs:9000/…`. DuckDB matches secrets by URL prefix +
endpoint host; if the host differs, the secret isn't selected and the
PUT goes out unsigned.

**Fix.** Set the S3 secret `ENDPOINT` to the docker-network host name
Lakekeeper returns (`rustfs:9000`, not `localhost:9000`). Then map that
host to `127.0.0.1` in `/etc/hosts` on the DuckDB-client machine so the
TCP connection actually lands on the local RustFS port.

```bash
echo "127.0.0.1 rustfs" | sudo tee -a /etc/hosts
```

And in the SQL:

```sql
CREATE OR REPLACE SECRET s3_warehouse (
    TYPE s3, PROVIDER config,
    KEY_ID '…', SECRET '…',
    ENDPOINT 'rustfs:9000',
    REGION 'us-east-1',
    URL_STYLE 'path', USE_SSL false,
    SCOPE 's3://spike-warehouse/'
);
```

(Alternative for a fully-containerized client: skip the hosts hack and
run the DuckDB caller inside the same docker network, where `rustfs`
already resolves.)

## D-3 ★ RustFS at v1.0.0-alpha returns 403 on `GET /` — that's fine, fix the healthcheck

**Symptom.** docker-compose says `spike-rustfs` is unhealthy and
dependent services never start. `wget --spider http://localhost:9000/`
exits non-zero.

**Root cause.** RustFS (correctly) returns `403 AccessDenied` for an
unauthenticated `GET /` because S3 requires SigV4 for the
ListBuckets-on-root operation. `wget --spider` rejects 403 as
"unsuccessful" even though the server is healthy.

**Fix.** Use a check that only cares about reachability:

```yaml
healthcheck:
  test: ["CMD-SHELL", "wget -q -O /dev/null http://localhost:9000/ 2>&1 | grep -qv 'Connection refused' && exit 0 || exit 1"]
```

Same idea for MinIO if you swap backends, though `mc ready local` is
the cleaner alternative there.

## D-4 ★ `duckdb-rs` has no `FromSql for Vec<f32>` — read arrays as VARCHAR and parse

**Symptom.** `cargo check` fails with `the trait FromSql is not
implemented for Vec<f32>` on any code that does `let v: Vec<f32> =
row.get(i)?` against a `FLOAT[N]` column.

**Fix.** Cast to `VARCHAR` in the SQL and parse with `serde_json`:

```rust
let sql = "SELECT key, CAST(vec AS VARCHAR), CAST(meta AS VARCHAR) FROM …";
// ...
let vec_str: String = r.get(1)?;
let vec: Vec<f32> = serde_json::from_str(&vec_str)?;
```

Slow for hot paths; fine for spike-scale GetVectors. The "right" fix
would be the Arrow columnar path on duckdb-rs but that's not yet
ergonomic. Already done in `crates/vectors/src/data_plane.rs`.

## D-5 DuckDB-iceberg write limitations to know up front

These come straight from the official docs
([writes overview](https://duckdb.org/2025/11/28/iceberg-writes-in-duckdb),
[REST catalogs page](https://duckdb.org/docs/current/core_extensions/iceberg/iceberg_rest_catalogs)):

- `UPDATE` / `DELETE` only on **unpartitioned, unsorted** tables.
- Only **merge-on-read** deletes. `write.delete.mode = copy-on-write`
  triggers an error.
- `MERGE INTO` not supported.
- `ALTER TABLE` not supported.
- `CREATE OR REPLACE TABLE` not supported — use `DROP TABLE IF EXISTS`
  then `CREATE TABLE` (rejection error: "CREATE OR REPLACE not
  supported in DuckDB-Iceberg").

Marila's `s3tables` `CreateTable` should reject partition specs
explicitly so we don't ship a footgun.

## D-6 DuckDB-VSS HNSW persistence is opt-in and experimental

**What the docs say** (https://duckdb.org/docs/stable/core_extensions/vss):

- Persistence is off by default for file-backed DBs.
- Must `SET hnsw_enable_experimental_persistence = true;`.
- Not buffer-managed (whole index must fit in RAM).
- Full re-serialization on every checkpoint (no incremental writes).
- No WAL recovery for custom indexes — a crash mid-checkpoint can
  corrupt the index in the `.duckdb` file.
- Deletes are tombstones; reclaim with `PRAGMA hnsw_compact_index('…')`.

**Marila's posture.** Treat the `.duckdb` file as a **rebuildable
cache**. Each `PutVectors` writes a JSON snapshot to RustFS *before*
the DuckDB INSERT. The rebuild-from-snapshots path is **not yet
implemented** (deferred per `REQUIREMENTS.md` §10). Don't claim
durability until it is.

## D-7 Lakekeeper bootstrap is a two-shot POST + warehouse creation

**Discovery.** Lakekeeper boots into a "not yet accepted terms of
use" state and refuses real operations until two POSTs land:

```bash
# 1. accept terms
curl -X POST http://lakekeeper:8181/management/v1/bootstrap \
  -H 'Content-Type: application/json' \
  --data '{"accept-terms-of-use": true}'   # → 204

# 2. create a warehouse pointing at our S3 backend
curl -X POST http://lakekeeper:8181/management/v1/warehouse \
  -H 'Content-Type: application/json' \
  --data-binary @lakekeeper-warehouse.json # → 201
```

The warehouse JSON template lives at the repo root
(`lakekeeper-warehouse.json`). Important fields:

- `storage-profile.flavor`: `s3-compat` works for both RustFS and
  MinIO. (Lakekeeper accepts `minio` too but normalizes to `s3-compat`
  in the response.)
- `storage-profile.endpoint`: must be the docker-network host
  (`http://rustfs:9000`); see D-2.
- `storage-profile.path-style-access`: `true`.
- `storage-profile.sts-enabled`: `false`. (RustFS/MinIO don't speak
  STS for arbitrary roles.)
- `storage-profile.remote-signing-enabled`: `false`. (Combined with D-1.)
- `storage-credential.credential-type`: `access-key`, with static key
  + secret. Lakekeeper uses these for its own server-side
  `metadata.json` writes; DuckDB uses the user's `TYPE s3` secret for
  its data-file writes (D-1).

Marila's `docker-compose.yml` under the `lakekeeper` profile runs both
POSTs automatically via `bootstrap` and `initialwarehouse` one-shot
services.

## D-8 Lakekeeper writes the initial `metadata.json`; DuckDB writes the data and the subsequent snapshots

Useful mental model when debugging a failing flow:

| Iceberg artefact | Written by | Sigv4 key used |
| --- | --- | --- |
| Initial `00000-….metadata.json` on `CREATE TABLE` | Lakekeeper itself | `storage-credential` in the warehouse profile |
| Data parquet on `INSERT` | DuckDB | the user's `TYPE s3` secret (after D-1) |
| Positional-delete parquet on `UPDATE` / `DELETE` | DuckDB | same |
| Manifest avros / manifest-list avros | DuckDB | same |
| Subsequent `0000N-….metadata.json` snapshots | DuckDB | same |
| `commit` (CAS in Postgres) | Lakekeeper (called by DuckDB) | n/a |

So if the initial metadata.json appears in RustFS but the data files
don't, you're looking at D-1 (or a credentials mismatch). If nothing
appears at all, it's D-3 / D-7 (RustFS / Lakekeeper not up).

## D-9 RustFS stores objects as `…/key/xl.meta` (MinIO erasure-coded layout)

Useful to know when grepping the volume to verify what was written.

```bash
# What you ran:
docker exec spike-rustfs find /data/spike-warehouse/lakekeeper -type f

# What you'd see for a single Iceberg file:
…/metadata/00000-….gz.metadata.json/xl.meta
```

The `xl.meta` is the chunk-metadata sidecar; the parent directory is
the logical object key. When confirming a write, look for the
directory name, not a file.

## D-10 The bundled `duckdb` crate compile is heavy

- ~30 GB in `target/` for a debug build (peak; settles lower after
  cleanup). Killed the sandbox once during the spike.
- 5–10 minutes wall time on first `cargo build`. Incremental builds
  are fast after that.
- The build emits a giant `libduckdb.a` and `ar` is the slowest step.

If this becomes painful (e.g. on CI), link a system `libduckdb` via
the `duckdb` crate's `loadable_extension` / unbundled mode. Out of
scope for the spike.

## D-11 DuckDB extensions auto-install on first `LOAD` (needs network)

`INSTALL iceberg; LOAD iceberg;` reaches the DuckDB extension repo on
first use. Same for `vss` and `lance`. Air-gapped deploys must
pre-stage the `.duckdb_extension` files. Marila's `Engine::open` calls
`INSTALL` defensively but logs and continues if the network isn't
available; the feature path that needs the extension will then fail
more loudly. See `crates/core/src/engine.rs`.

## D-12 DuckDB-VSS post-filters; AWS S3 Vectors filter-while-search

Already in `GAP_ANALYSIS.md` row V6; restating because it's the
biggest semantic gap on the vectors side.

- AWS evaluates the metadata filter *during* HNSW traversal. With a
  filter that selects 0.1% of vectors and `topK=10`, AWS still
  returns 10 matches (assuming the corpus has them).
- DuckDB-VSS asks HNSW for `~topK` candidates, then applies our
  filter as a `WHERE` post-filter. Restrictive filters collapse
  recall — you'll often see 0–1 results.
- Cheap mitigation: oversample (`LIMIT topK * 100`), post-filter,
  truncate. Buys you ~2 orders of magnitude of selectivity.
- Structural fix: swap the engine for `usearch` (HNSW with filter
  callback) or `qdrant-segment` (first-class filterable payload
  index).

Not yet implemented in marila. Either build the oversample mitigation
in `crates/vectors/src/data_plane.rs::query_vectors`, or swap engines.

## D-13 The hand-rolled `iceberg_metadata.rs` is now obsolete

The original scaffold had `crates/tables/src/iceberg_metadata.rs`
writing a minimal v2 `metadata.json` from a Rust helper. Since
Lakekeeper writes the initial metadata.json itself on
`CreateTable`, the file should be **deleted**, and the AWS-JSON
`CreateTable` handler should proxy to Lakekeeper instead of writing
the JSON.

This is ADR-3 in `ARCHITECTURE.md`. Anyone touching `crates/tables`
should delete the file in the same commit.

## D-14 axum 0.8 path syntax is `{name}`, not `:name`

Trivial but trips people coming from older axum / actix code. We use
the `{name}` form throughout `crates/*/src/routes.rs`. Method
chaining (`get(h1).put(h2)`) still works the same way.

## D-15 Permissive SigV4 — model the verifier as a pluggable callback

We parse but don't verify. The shape of `Principal` in
`crates/aws_compat/src/sigv4.rs` matches what a real verifier would
return. The future verification point is a tower middleware on the
top-level `Router`, with the same separation as
`../2026-05-14-quack-oauth/architecture.md` (check_token, check_authz
as scalar-function-like callbacks). Don't move auth into the
handlers themselves; keep it at the edge.

---

## What we deliberately did NOT discover

These are the experiments we DIDN'T run; flagging them so nobody
treats their absence as evidence either way.

- We did not benchmark RustFS vs MinIO under load.
- We did not measure DuckDB-VSS recall vs FAISS / ScaNN.
- We did not test concurrent writers against the same Iceberg table
  through Lakekeeper.
- We did not stress-test the Mongo-filter parser with adversarial
  inputs (the field-name sanitization is conservative but unproven).
- We did not test the lance fallback engine. The docker image and
  crate are wired up, but no demo exercises it.
- We did not test a fully-containerized marila (a Dockerfile block is
  left commented out in `docker-compose.yml`).

If any of these become important, add them to `VERIFICATION.md` once
done.
