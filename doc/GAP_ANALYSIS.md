# Gap Analysis

marila intentionally implements a subset of AWS S3 Tables and S3 Vectors.
This document records known semantic gaps so users do not mistake local
compatibility for full AWS parity.

## Security and Authorization

| Gap | Impact | Current behavior |
| --- | --- | --- |
| SigV4 signatures are not verified | Any caller that can reach the service can act as any principal | Parse and log SigV4 headers; do not authenticate |
| IAM, bucket policies, and table/vector policies are not implemented | Policy-dependent client workflows are unsupported | Return explicit `501 NotImplementedException` for unsupported policy operations |
| Vended credentials are not implemented | Engines that require catalog-issued credentials need configuration changes | DuckDB demos use `ACCESS_DELEGATION_MODE 'none'` and an explicit S3 secret |

## S3 Tables

| Gap | Impact | Current behavior |
| --- | --- | --- |
| Operation coverage is partial | AWS clients using unsupported operations fail | Implemented operations are documented in `REQUIREMENTS.md`; unsupported operations return `501` |
| Table maintenance is not implemented | No compaction, snapshot expiration, or orphan cleanup through marila | Lakekeeper owns catalog state; maintenance remains out of scope |
| Version-token semantics are approximated | Clients relying on exact AWS concurrency token semantics may diverge | Tokens are derived from Iceberg metadata locations |

## S3 Vectors

| Gap | Impact | Current behavior |
| --- | --- | --- |
| Filtered ANN search is post-filtered | Highly selective filters can return fewer than `topK` results | DuckDB query applies metadata filters in SQL around vector distance ordering |
| HNSW rebuild from RustFS snapshots is incomplete | DuckDB state is not yet fully reconstructable on process start | JSON snapshots are durable; rebuild path is tracked as deferred work |
| Performance parity is not a goal | Latency and recall can differ from AWS | Prefer honest results and explicit unsupported behavior over silent parity claims |

## Deployment

| Gap | Impact | Current behavior |
| --- | --- | --- |
| Local-first default credentials | Default stack is unsafe on untrusted networks | `SECURITY.md` documents the boundary; do not expose defaults publicly |
| Fully containerized marila image is not provided | Users run the Rust binary on the host today | Compose covers sidecars; binary runs with `cargo run -p marila` |
