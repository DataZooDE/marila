# marila

An AWS S3 Tables and S3 Vectors compatibility layer on top of RustFS,
with Lakekeeper fronting Iceberg and DuckDB fronting vectors.

> Named after *Aythya marila*, the greater scaup (a diving duck) — sits
> next to its DataZoo sibling `quack`.

## Status

Bootstrap in progress. First vertical slice: `CreateVectorBucket`
round-trips a boto3 / aws-sdk-s3vectors call into a state row and a RustFS
bucket.

## Quick start

```bash
docker compose up -d rustfs
cargo run -p marila
# in another shell:
cargo test -p marila-integration-tests
```

## Documents

- [`doc/REQUIREMENTS.md`](doc/REQUIREMENTS.md) — what to build and why
- [`doc/ARCHITECTURE.md`](doc/ARCHITECTURE.md) — arc42 design
- [`doc/DISCOVERIES.md`](doc/DISCOVERIES.md) — lessons from the prior spike
- [`CLAUDE.md`](CLAUDE.md) — methodology + working notes for the next agent
