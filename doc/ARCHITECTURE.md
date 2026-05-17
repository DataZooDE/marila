# Architecture — Marila (arc42)

> **Marila** — an AWS S3 Tables and S3 Vectors compatibility layer on
> top of RustFS, Lakekeeper, and DuckDB. Named after *Aythya marila*,
> the greater scaup. Sibling of `quack` in the DataZoo family.

Status: v0.1, 2026-05-16. Author: Claude. Companion to
`REQUIREMENTS.md` and `DISCOVERIES.md`. Style follows arc42 because
that's what the sibling `2026-05-14-quack-oauth/architecture.md` uses.

This architecture reflects the **post-verification** design: Lakekeeper
fronts Iceberg, marila is a thin AWS-JSON wrapper. It supersedes the
original scaffold's "implement our own Iceberg REST catalog" approach.

> **Naming note.** The current spike code uses `spike-*` crate names
> (e.g. `spike-api`, `spike-tables`). The diagrams and prose below use
> the **marila-\*** naming the codebase will move to. ADR-7 (§9) tracks
> the rename.

---

## 1. Introduction and goals

Provide AWS-compatible `s3tables` and `s3vectors` HTTP surfaces in
front of:

- **RustFS** (S3-compatible object store), holding all durable bytes.
- **Lakekeeper** (Rust-based Iceberg REST catalog) for tables-side
  catalog + commit semantics.
- **DuckDB** (embedded, with the `iceberg`, `vss`, `lance` core
  extensions) for vectors-side query + index.

Top quality goals (from `REQUIREMENTS.md` §6):

1. Honest semantics — no silent divergence from AWS.
2. Local-first reproducibility — one `docker compose` + one `cargo run`.
3. Wire-shape compatibility with `boto3` clients via `endpoint_url`.
4. Easy to swap a layer (object store, catalog, vector engine).

## 2. Constraints

See `REQUIREMENTS.md` §7. Highlights:

- Rust workspace, `axum`, `duckdb` crate with `bundled`.
- Lakekeeper is consumed as a black box; we do not fork it.
- All work under `2026-05-16-s3-tables-rustfs-spike/`, committed to `main`.

## 3. Context and scope

```mermaid
flowchart LR
  subgraph Clients
    Boto[boto3.client&#40;"s3tables" / "s3vectors", endpoint_url=...&#41;]
    DuckDB[DuckDB / PyIceberg / Spark client]
    AwsCli[aws s3tables / aws s3vectors CLI]
  end

  subgraph MarilaProcess[marila &#40;single axum binary&#41;]
    T[marila-tables: AWS-JSON s3tables façade]
    L[reverse-proxy /iceberg/v1/... -> Lakekeeper]
    V[marila-vectors: AWS-JSON s3vectors façade + DuckDB VSS]
    A[marila-aws-compat: SigV4 parse, error envelope]
  end

  subgraph Sidecars[docker-compose]
    LK[(Lakekeeper :8181)]
    PG[(Postgres :5432)]
    RFS[(RustFS :9000)]
  end

  Boto --> T
  Boto --> V
  AwsCli --> T
  DuckDB --> L
  T --> LK
  L --> LK
  LK --> PG
  T -. ensure bucket .-> RFS
  V --> RFS
  LK --> RFS
```

**External actors**: the three client groups above (boto3, native
Iceberg engines, AWS CLI). System boundary: the `marila` binary plus
the three sidecars under our `docker-compose.yml`'s `lakekeeper`
profile.

## 4. Solution strategy

| Decision | Rationale |
| --- | --- |
| **Front Lakekeeper instead of writing our own Iceberg REST catalog.** | Verified to work end-to-end (CREATE/INSERT/UPDATE/DELETE) against RustFS. Saves ~1–2 engineer-weeks of catalog implementation. |
| **Reverse-proxy `/iceberg/v1/...` straight through.** | DuckDB / PyIceberg / Spark clients want a real Iceberg REST endpoint. Implementing our own added zero value. |
| **Translate AWS-JSON `s3tables` ops to Lakekeeper management + catalog REST.** | The two APIs cover similar concepts (warehouses↔table buckets, namespaces, tables). The mapping is mechanical. |
| **Keep the vectors path in-process, on DuckDB VSS.** | There is no off-the-shelf "S3 Vectors equivalent" the way Lakekeeper is for Iceberg. We own this. |
| **One-process binary, one DuckDB connection.** | Cheap, easy to ship as a static binary, matches the "thin wrapper" thesis on the vectors side. |
| **Permissive SigV4 parse, no verification.** | Per non-goal NG-1. Plug-point exists if someone wires a real verifier later. |

## 5. Building block view

### 5.1 Level 1 — crates

```
crates/api          marila              single binary, axum router, ties everything together
crates/core         marila-core         embedded DuckDB engine (vectors-side), state schema, config
crates/storage      marila-storage      aws-sdk-s3 wrapper for marila's S3 ops against RustFS
crates/tables       marila-tables       AWS-JSON s3tables façade + thin REST proxy to Lakekeeper
crates/vectors      marila-vectors      AWS-JSON s3vectors façade + VSS bridge + Mongo→SQL filter
crates/aws_compat   marila-aws-compat   SigV4 permissive parser, AWS-JSON error envelope
crates/integration_tests  marila-integration-tests  end-to-end tests against the running stack
```

(Today's directory paths shown left; target crate names right. Current
manifests still use the `spike-*` prefix — see ADR-7.)

### 5.2 Level 2 — `crates/tables` internals

```
control_plane.rs    handlers for PUT /buckets, /buckets/{}/namespaces, …
lakekeeper.rs       thin async client for Lakekeeper's /management/v1/* API
iceberg_proxy.rs    reverse-proxy axum router under /iceberg
types.rs            AWS-JSON request/response shapes
state.rs            (vestigial — kept for DML ops not delegated to Lakekeeper)
```

The hand-rolled `iceberg_metadata.rs` from the spike scaffold is
**obsolete** and should be deleted; Lakekeeper writes the initial
metadata.json itself.

### 5.3 Level 2 — `crates/vectors` internals

```
control_plane.rs    handlers for CreateVectorBucket/Index, etc.
data_plane.rs       handlers for PutVectors/QueryVectors/...
filter.rs           MongoDB-style filter expression → SQL WHERE
state.rs            DuckDB CRUD over state.vector_buckets / state.vector_indexes
routes.rs           axum POST /<OperationName> routes
```

## 6. Runtime view

### 6.1 `CreateTable` happy path

```
boto3.client("s3tables")        marila                  Lakekeeper                 RustFS
     │                              │                       │                          │
PUT /buckets/{b}/namespaces/{ns}/tables {name, schema}      │                          │
     │ ─────────────────────────▶  │                       │                          │
     │                              │── POST /catalog/v1/{prefix}/namespaces/{ns}/tables ▶
     │                              │                       │── PUT metadata.json ───▶ │
     │                              │                       │ ◀────── 200              │
     │                              │ ◀─── 200 {metadata-location, metadata} ───       │
     │ ◀──── 201 {tableArn, versionToken=metadata-location} │                          │
```

### 6.2 DuckDB INSERT through the loopback

```
DuckDB client            marila  (reverse-proxy /iceberg → :8181)             Lakekeeper                 RustFS
     │                       │                                                    │                         │
ATTACH 'demo' AS lake (TYPE iceberg, ENDPOINT 'http://marila:8080/iceberg', ACCESS_DELEGATION_MODE 'none');
INSERT INTO lake.sales.orders VALUES (...);
     │                       │                                                    │                         │
     │ POST /iceberg/v1/{prefix}/namespaces/{ns}/tables/{t} (loadTable) ─▶ proxy ─▶                         │
     │                                                                            │ ◀── return metadata ──  │
     │ ◀─────── metadata-location + parsed metadata ───────────────               │                         │
     │── PUT data parquet (SigV4 with user's TYPE s3 secret) ──────────────────────────────────────────▶    │
     │── PUT manifest, manifest-list ────────────────────────────────────────────────────────────────▶      │
     │── PUT next metadata.json ─────────────────────────────────────────────────────────────────────▶      │
     │ POST /iceberg/v1/{prefix}/namespaces/{ns}/tables/{t} (commit) ──▶ proxy ──▶                          │
     │                                                                            │── CAS pointer in PG ▶ ─ │
     │ ◀─── 200 (committed) ───                                                                              │
```

### 6.3 `QueryVectors` with metadata filter

```
boto3.client("s3vectors")          marila  (marila-vectors)             DuckDB engine
     │                                  │                                     │
POST /QueryVectors {queryVector, topK: 10, filter: {"label": "a"}, returnDistance: true}
     │ ────────────────────────────▶    │                                     │
     │                                  │── filter::translate(...) → SQL WHERE
     │                                  │── SELECT key, ..., array_cosine_distance(vec, $q) AS d
     │                                  │     FROM vec_<b>_<i>
     │                                  │     WHERE <sql-where>
     │                                  │     ORDER BY d LIMIT 10;
     │                                  │ ◀── rows
     │ ◀─── {vectors: [{key, distance, metadata}, …]} ──                       │
```

## 7. Deployment view

Default deployment is `docker compose --profile lakekeeper up -d` plus
`cargo run -p spike-api` (post-rename: `cargo run -p marila`) on the host:

```
host (Linux)
├─ marila binary           :8080
└─ docker
   ├─ rustfs               :9000 (S3) :9001 (console)
   ├─ postgres:17          :5432
   ├─ lakekeeper           :8181 (Iceberg REST + management)
   ├─ migrate (oneshot)
   ├─ bootstrap (oneshot)
   ├─ createbuckets (oneshot)
   └─ initialwarehouse (oneshot)
```

Required `/etc/hosts` entry on the host so DuckDB clients can resolve
the storage URL Lakekeeper returns:

```
127.0.0.1 rustfs
```

For a fully-containerized deployment, marila can be added to the compose
graph under its own service (a Dockerfile is left as an exercise; see
the commented-out service block at the bottom of `docker-compose.yml`).

## 8. Cross-cutting concepts

- **Auth**. `crates/aws_compat::sigv4` parses the `Authorization`
  header and logs the principal; nothing verifies. The verification
  callback is a future plug-point modelled on
  `../2026-05-14-quack-oauth/architecture.md`.
- **Errors**. All façade handlers return `Result<_, AwsError>`;
  `AwsError::into_response` emits the `{"__type": "…", "Message": "…"}`
  envelope at the right HTTP status. `From<SpikeError>` /
  `From<serde_json::Error>` impls make `?` ergonomic in handlers.
- **Logging**. `tracing` + `tracing-subscriber` with `RUST_LOG` env
  filter. Tower `TraceLayer` adds per-request spans.
- **State**. The vectors-side state schema lives in a DuckDB file
  (`data/state.duckdb`); the tables-side state lives in Lakekeeper's
  Postgres. The DuckDB file is rebuildable from the JSON snapshots on
  RustFS (rebuild path not yet implemented).
- **Schema for vector indexes**. Each index becomes a real DuckDB
  table `vec_<bucket>_<index> (key VARCHAR PRIMARY KEY, vec FLOAT[N],
  meta JSON)`. The HNSW index is optional and controlled by the
  `--brute-force` flag.

## 9. Architecture decisions (selected)

| # | Decision | Status |
| --- | --- | --- |
| ADR-1 | Use Lakekeeper for the Iceberg REST catalog instead of writing our own. | Verified (see `VERIFICATION.md`). |
| ADR-2 | Pass `ACCESS_DELEGATION_MODE 'none'` on every DuckDB ATTACH against Lakekeeper. | Required by [duckdb-iceberg#594](https://github.com/duckdb/duckdb-iceberg/issues/594) workaround. Encoded in our docs and the demo SQL. |
| ADR-3 | Delete `crates/tables/src/iceberg_metadata.rs` and the AWS-JSON `CreateTable`'s hand-rolled metadata.json writer. | Lakekeeper now owns that path. |
| ADR-4 | Keep the vectors-side stack in-process on DuckDB VSS. | No off-the-shelf equivalent of Lakekeeper exists for S3 Vectors. |
| ADR-5 | Mongo filter → SQL transpiler with conservative field-name grammar. | Spike-quality is fine; revisit if we ever expose this externally untrusted. |
| ADR-6 | RustFS over MinIO for the default. Swap is one section of compose. | Verified RustFS works through Lakekeeper for full Iceberg writes. |
| ADR-7 | Project named **marila**. Crates renamed `spike-*` → `marila-*`. Binary becomes `marila`. | Decided; the rename itself is a deferred one-shot refactor (REQUIREMENTS.md §10). |

## 10. Quality scenarios

- **API-shape compatibility**: a boto3 caller using `endpoint_url=…`
  succeeds for every implemented op without code changes. Verified via
  `demo/demo_tables.py` and `demo/demo_vectors.py`.
- **Catalog-shape compatibility**: a DuckDB caller using
  `ATTACH 'X' AS Y (TYPE iceberg, ENDPOINT 'http://…/iceberg', ACCESS_DELEGATION_MODE 'none')`
  can read and write Iceberg tables. Verified via `demo/lakekeeper_verify.sql`.
- **Layer swap**: replacing RustFS with MinIO in `docker-compose.yml`
  requires changing only the `image:` field and the warehouse
  `endpoint`. Verified during hypothesis testing.

## 11. Risks and tech debt

| # | Risk | Mitigation |
| --- | --- | --- |
| R-1 | RustFS is pre-1.0 (alpha/beta tags). Surface drift possible. | MinIO is a documented drop-in fallback. |
| R-2 | VSS HNSW persistence is experimental, with no WAL recovery. | RustFS JSON snapshots are the durable copy. Rebuild-on-startup path not yet implemented (deferred). |
| R-3 | Filter-while-search vs post-filter for vector queries. | Documented headline gap. Oversample-and-post-filter is the v1 mitigation; structural fix is to swap to usearch or qdrant-segment. |
| R-4 | DuckDB-iceberg writes only on unpartitioned/unsorted tables. | Document; reject partition spec in `CreateTable` until extension lifts the restriction. |
| R-5 | DuckDB bundled build ~30 GB target dir + several minutes. | Document; switch to system `libduckdb` for CI if it becomes painful. |
| R-6 | Permissive auth means anything that can reach the service can change everything. | NG-1. Document. Pluggable verifier is the future plug-point. |
| R-7 | Lakekeeper image `latest-main` floats. | Pin once we settle on a known-working tag. |

## 12. Glossary

- **Marila** — this project. Greater scaup species epithet.
- **AWS-JSON façade** — the part of marila that mimics AWS's
  REST + JSON wire shape so boto3 works against it.
- **Iceberg REST pass-through** — the reverse-proxy from `/iceberg/v1/…`
  on marila to Lakekeeper's catalog endpoint.
- **Backing table** — for vectors, the DuckDB table `vec_<b>_<i>` that
  holds the actual rows + HNSW index.
- **Warehouse** — Lakekeeper's term for a tenant-scoped Iceberg
  catalog (corresponds to our "table bucket").
- **VSS** — DuckDB Vector Similarity Search extension.
- **Access delegation** — the Iceberg REST mechanism for the catalog
  to vend short-lived S3 credentials to clients. We disable it via
  `ACCESS_DELEGATION_MODE 'none'`.
