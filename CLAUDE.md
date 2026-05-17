# CLAUDE.md — working notes for the marila coding agent

This file is the lessons-learned and methodology log for whoever (human or
agent) picks up this repo next. The user-facing intro is in `README.md`;
the design background is in `doc/{REQUIREMENTS,ARCHITECTURE,DISCOVERIES}.md`.

---

## Methodology — AWS-contract-first TDD

**Every feature is implemented by first writing a contract test that runs
the same operation against real AWS and against local marila, then making
marila match AWS's observed behaviour exactly. Feature by feature, function
by function.**

- AWS is the source of truth. The docs lie or omit; the wire shape is what
  we copy.
- A feature is only "done" when the **same test** passes against both
  `--target=local` and `--target=aws`. If behaviour diverges, that's a
  marila bug, not a test bug. Either fix marila or document the deviation
  in `doc/GAP_ANALYSIS.md`.
- Capture wire shapes with `aws <service> <op> --debug 2>&1 | grep -iE
  "body|errortype|HTTP/1\.1"` before writing the test. The captured
  request/response body is more useful than the boto3 docs.
- Definition of done = green integration test in
  `crates/integration_tests/tests/*.rs` for both targets.

## Test harness

- Rust-native, in `crates/integration_tests`. No Python in the gated path.
- Uses `aws-sdk-s3vectors` (and later `aws-sdk-s3tables`) as the client —
  the same boto3 wire shape, type-checked at compile time.
- Each test function exists twice: `local_<name>` (always runs) and
  `aws_<name>` (skipped if no AWS creds detected).
- An optional `uv` venv with `boto3` may live at `/.venv` for ad-hoc human
  exploration, but it isn't on the test path.

## Central discoveries (so we don't re-learn them)

### C-1 — S3 Vectors uses the `restJson1` Smithy protocol, not `aws-json-1.0`

Despite what `doc/ARCHITECTURE.md` §8 implies (`{"__type": "...", "Message": "..."}`),
S3 Vectors error responses look like this:

```
HTTP/1.1 409 Conflict
Content-Type: application/json
x-amzn-errortype: ConflictException
x-amz-request-id: 013bde7a-7e97-4257-b2bd-9d626de6b2df

{"message":"A vector bucket with the specified name already exists"}
```

Note:
- Error type is the `x-amzn-errortype` **header**, not embedded in the body.
- Body uses lowercase `message`, not `Message`.
- No `__type` envelope.

The arc42 doc's envelope description is correct for `s3tables` (which uses
`aws-json-1.0`) but **not** for `s3vectors`. Implement them per-service.

### C-2 — CreateVectorBucket wire shape (probed 2026-05-17, eu-west-1)

Request:
```
POST /CreateVectorBucket HTTP/1.1
Host: s3vectors.<region>.api.aws
Content-Type: application/json
Authorization: AWS4-HMAC-SHA256 ...

{"vectorBucketName":"<name>"}
```

Success response:
```
HTTP/1.1 200 OK
Content-Type: application/json

{"vectorBucketArn":"arn:aws:s3vectors:<region>:<account>:bucket/<name>"}
```

ListVectorBuckets response:
```json
{
  "vectorBuckets": [
    {
      "vectorBucketName": "...",
      "vectorBucketArn": "arn:aws:s3vectors:eu-west-1:625644349722:bucket/...",
      "creationTime": "2026-05-17T15:19:26+02:00"
    }
  ]
}
```

ARN format: `arn:aws:s3vectors:<region>:<account>:bucket/<bucket-name>`.
Bucket-name validation: `min 3 / max 63` chars (per CLI help).

### C-3 — DuckDB is built against system `libduckdb` (Arch package), not bundled

Arch ships `duckdb 1.5.2` with `/usr/include/duckdb.h` and
`/usr/lib/libduckdb.so`. We use the `duckdb` Rust crate **without** the
`bundled` feature to avoid the 30 GB / 5–10 min build cost
(`doc/DISCOVERIES.md` D-10).

Build-time env (set in the root `Cargo.toml`-adjacent `.cargo/config.toml` if
needed):
```
DUCKDB_LIB_DIR=/usr/lib
DUCKDB_INCLUDE_DIR=/usr/include
```

If the default unbundled mode finds these automatically (it usually does on
Arch), no extra env is needed.

### C-4 — Crate naming: `marila-*` from day one

No `spike-*` legacy. The `doc/` files mention a rename ADR; that rename was
already done at bootstrap. Ignore the rename TODO.

### C-5 — Region defaults to `eu-west-1`

The user's AWS account (`625644349722`) operates in `eu-west-1`. Marila's
local tests and `docker-compose.yml` use the same region so the wire shape
matches (region is encoded in the ARN).

### C-6 — RustFS healthcheck must tolerate `403` on `GET /`

Per `doc/DISCOVERIES.md` D-3 — `wget --spider` rejects 403 as failure even
though RustFS is healthy. Use `wget -q -O /dev/null …` and inspect for
`Connection refused` instead.

### C-7 — Lakekeeper / Postgres deferred until tables-side work

The vertical slice in progress is `CreateVectorBucket`; only RustFS is
needed. Lakekeeper + Postgres images can be pre-pulled but their compose
services aren't started yet. Add them when we start a tables-side feature.

---

## What's done

| Slice | Status | Tests |
| --- | --- | --- |
| Repo bootstrap (Cargo workspace, docker-compose, CLAUDE.md) | in progress | — |
| `CreateVectorBucket` end-to-end | in progress | `tests/create_vector_bucket.rs` |

## What's next

Once the bootstrap slice is green, the next feature should follow the same
recipe:

1. `aws s3vectors <op> --debug` to capture the wire shape.
2. Add the operation to `crates/integration_tests/tests/<op>.rs` with both
   `local_*` and `aws_*` variants.
3. Implement minimally in `marila-vectors` until green on both targets.
4. Refactor, commit.

Suggested order (low risk → higher):
- `ListVectorBuckets`
- `GetVectorBucket`
- `DeleteVectorBucket`
- `CreateIndex` (requires DuckDB VSS — bigger jump)
- … then the tables side, starting from `CreateTableBucket` (forces
  Lakekeeper + Postgres into the compose graph).
