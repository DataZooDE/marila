# Requirements — Marila

> **Marila** — an AWS S3 Tables and S3 Vectors compatibility layer on top
> of RustFS, with Lakekeeper fronting Iceberg and DuckDB fronting vectors.
> Named after *Aythya marila*, the greater scaup (a diving duck) — sits next
> to its DataZoo sibling `quack`. Etymology: Greek *μαρίλη*, "embers".

Status: v0.1, 2026-06-01. Companion to `ARCHITECTURE.md` and
`DISCOVERIES.md`.

This requirements doc describes the current pre-1.0 project scope. It is
not a production specification.

---

## 1. Purpose and audience

Allow AWS clients (`boto3.client("s3tables")`, `boto3.client("s3vectors")`,
`aws s3tables …`, `aws s3vectors …`) to operate against a locally-hosted
**marila** service that stores all data in a RustFS bucket, with API
behavior close enough to AWS that most client code works unchanged.

Audience: contributors and users evaluating marila for local
compatibility testing.

---

## 2. Goals

| ID | Goal |
| --- | --- |
| G-1 | Implement a subset of the AWS `s3tables` API as an AWS-JSON REST façade in front of Lakekeeper. |
| G-2 | Implement a subset of the AWS `s3vectors` API as an AWS-JSON REST façade in front of an embedded DuckDB VSS engine. |
| G-3 | All durable state lives in RustFS (Iceberg files for tables; JSON snapshots + Iceberg or parquet for vectors). DuckDB and Postgres files are rebuildable caches. |
| G-4 | `boto3` clients with `endpoint_url=…` work against the service unmodified for the implemented op subset. |
| G-5 | DuckDB / PyIceberg / Spark can also query the tables directly by connecting to the Lakekeeper Iceberg REST endpoint exposed by the service. |
| G-6 | Single-binary deployment of marila (the AWS façade); everything else (RustFS, Postgres, Lakekeeper) via docker-compose. |
| G-7 | Honest documentation of every divergence from AWS semantics. |

## 3. Non-goals (carry-over from the brief)

| ID | Non-goal |
| --- | --- |
| NG-1 | SigV4 signature verification. Header is parsed and logged; never verified. |
| NG-2 | IAM, bucket policies, encryption, tagging, replication. |
| NG-3 | Vended credentials / per-request scoped STS tokens. |
| NG-4 | Multi-node distributed correctness. |
| NG-5 | Performance parity with AWS. |
| NG-6 | Server-side table maintenance (compaction, snapshot expiration, unreferenced-file cleanup). |
| NG-7 | Full operation coverage. Anything beyond the lists in §4 / §5 returns 501. |

---

## 4. Functional requirements — S3 Tables

| ID | Op | Behavior |
| --- | --- | --- |
| FT-1 | `CreateTableBucket` | Create a Lakekeeper warehouse pointed at a RustFS bucket; create the RustFS bucket if missing. Return AWS-shaped ARN. |
| FT-2 | `ListTableBuckets`, `GetTableBucket`, `DeleteTableBucket` | List/show/delete warehouses on Lakekeeper. |
| FT-3 | `CreateNamespace`, `ListNamespaces`, `GetNamespace`, `DeleteNamespace` | Proxy to Lakekeeper `/v1/{prefix}/namespaces`. |
| FT-4 | `CreateTable` | Proxy to Lakekeeper `/v1/{prefix}/namespaces/{ns}/tables` with a schema derived from the AWS-JSON `metadata.iceberg.schema.fields`. |
| FT-5 | `ListTables`, `GetTable` | Proxy to Lakekeeper. |
| FT-6 | `GetTableMetadataLocation` | Return `metadata-location` from Lakekeeper's loadTable response. |
| FT-7 | `DeleteTable` | Proxy to Lakekeeper. |
| FT-8 | Iceberg REST pass-through | Mount Lakekeeper's REST endpoint at `/iceberg/v1/…` so DuckDB and other engines can attach via `ATTACH 'X' AS Y (TYPE iceberg, ENDPOINT '…/iceberg', ACCESS_DELEGATION_MODE 'none')`. |
| FT-9 | All other s3tables ops (policy, encryption, replication, maintenance, metrics, RenameTable) | Return `501 NotImplementedException`. |

## 5. Functional requirements — S3 Vectors

| ID | Op | Behavior |
| --- | --- | --- |
| FV-1 | `CreateVectorBucket`, `List`, `Get`, `Delete` | Insert/list/delete state rows; ensure RustFS bucket exists. |
| FV-2 | `CreateIndex` | Validate `dataType: float32`, `dimension ∈ [1,4096]`, `distanceMetric ∈ {cosine, euclidean}`. Create backing DuckDB table `vec_<b>_<i>(key VARCHAR PK, vec FLOAT[N], meta JSON)`; create HNSW index unless `--brute-force` is set. |
| FV-3 | `ListIndexes`, `GetIndex`, `DeleteIndex` | State CRUD; `DELETE` drops the backing table. |
| FV-4 | `PutVectors` (batch ≤ 500) | Write each vector as a JSON blob to RustFS (`<bucket>/<index>/<key>.json`) **before** the DuckDB INSERT. RustFS is the durable source of truth. |
| FV-5 | `QueryVectors` (topK ≤ 100, MongoDB-style filter) | Translate filter to SQL `WHERE` on the `meta` JSON column. Run `ORDER BY array_*_distance(vec, $q) LIMIT k`. For `euclidean`, return `sqrt(d)`. |
| FV-6 | `ListVectors`, `GetVectors` (batch ≤ 100), `DeleteVectors` | Straight SQL on the backing table. |
| FV-7 | All other s3vectors ops (policies, tags) | Return `501`. |

## 6. Quality requirements (priority order)

| Rank | Quality | Definition |
| --- | --- | --- |
| Q-1 | **Honest semantics** | Every API surface either matches AWS or returns an error / is documented in `GAP_ANALYSIS.md`. No silent divergence. |
| Q-2 | **Local-first reproducibility** | `docker compose --profile lakekeeper up -d && cargo run -p marila && cargo test -p marila-integration-tests` runs on a clean checkout. No hidden AWS calls. |
| Q-3 | **Small wire-shape compatibility** | `boto3.client(... endpoint_url=...)` works for every implemented op without bespoke client code. |
| Q-4 | **Easy to swap engine layers** | RustFS → MinIO, DuckDB VSS → usearch, Lakekeeper → Polaris should each be a single-crate change. |
| Q-5 | **Diagnosable failures** | Every error response includes the AWS `__type` code and a human-readable `Message`; service logs the principal + request id of the caller. |
| Q-6 | Performance | Out of scope. Best-effort. |

## 7. Constraints

| ID | Constraint |
| --- | --- |
| C-1 | Rust workspace, `axum` for HTTP, `duckdb` crate with `bundled` for the vectors engine. |
| C-2 | Version-pinned Lakekeeper image as the Iceberg REST catalog. No bespoke catalog implementation. |
| C-3 | RustFS for the object store. MinIO is the named fallback if RustFS regresses. |
| C-4 | Public changes land through normal pull requests against `main`. |
| C-5 | CI must run formatting, clippy, and tests before merge. |
| C-6 | No outbound network at runtime apart from extension auto-install on first DuckDB boot. Pre-bake extensions for air-gapped deploys. |

## 8. External interfaces

- **Inbound (clients)**: HTTP/1.1, JSON, port 8080. `s3tables` AWS-JSON
  REST + `/iceberg/v1/…` pass-through + `s3vectors` AWS-JSON
  `POST /<Op>`. Permissive SigV4 parsing (Q-1 applies — the gap is
  documented, not hidden).
- **Outbound to Lakekeeper**: HTTP, port 8181.
- **Outbound to RustFS**: S3, port 9000, path-style, SigV4 with the
  static credentials configured at service startup.
- **Sidecar Postgres**: only consumed by Lakekeeper; marila does not
  connect.

## 9. Acceptance criteria

A reviewer should be able to:

1. `git clone` the repo on a clean Linux box with Docker + Rust installed,
2. `docker compose --profile lakekeeper up -d`,
3. `cargo run -p marila`,
4. `cargo test -p marila-integration-tests`,
5. follow `demo/README.md` for the vector and tables demos,
6. run `cargo fmt --check`,
7. run `cargo clippy --workspace --all-targets -- -D warnings`,
8. run `cargo test --workspace --no-fail-fast`.

## 10. Open requirements (deliberately deferred)

- VSS HNSW rebuild from the RustFS JSON snapshots on engine open.
  Not blocking the POC; needed before any "production-shaped" claim.
- Oversample-and-post-filter mitigation for the filter-while-search
  gap. Needed before any benchmark against AWS.
- Pluggable auth callback (marila's permissive parser is a placeholder
  for a real verifier in the quack-oauth pattern).
