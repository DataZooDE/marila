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

ListVectorBuckets **raw** response body:
```json
{
  "vectorBuckets": [
    {
      "vectorBucketName": "...",
      "vectorBucketArn": "arn:aws:s3vectors:eu-west-1:625644349722:bucket/...",
      "creationTime": 1779025868
    }
  ]
}
```

ARN format: `arn:aws:s3vectors:<region>:<account>:bucket/<bucket-name>`.
Bucket-name validation: `min 3 / max 63` chars (per CLI help).

### C-2a — Timestamp wire format is **epoch-seconds (JSON number)**

The `aws` CLI pretty-prints `creationTime` as `"2026-05-17T15:19:26+02:00"`,
which is misleading — the **actual wire body** is `1779025868` (integer
seconds since the Unix epoch). The SDK fails to deserialize an ISO 8601
string into its `DateTime` shape, with this error:

```
only `Infinity`, `-Infinity`, `NaN` can represent a float as a string
but found `2026-05-17T13:50:41Z`
```

This matches the Smithy `restJson1` default — `epoch-seconds` is the
implicit `@timestampFormat`. Always send JSON numbers, never strings.

**General rule.** When AWS CLI output looks like a human-readable
timestamp, double-check the **raw HTTP body** with `--debug` before
trusting it. The CLI deserialises then re-serialises, and the
re-serialisation is the lie.

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

### C-2b — ListVectorBuckets / GetVectorBucket / DeleteVectorBucket wire shapes (probed 2026-05-17)

**ListVectorBuckets**
- Request: `POST /ListVectorBuckets` body any of `{}`, `{"prefix": "..."}`,
  `{"maxResults": N}`, `{"nextToken": "..."}`. Constraints: prefix 1..=63
  chars; `maxResults` is the *underlying* API field (the CLI exposes
  `--page-size` + `--max-items` on top).
- Success: `{"vectorBuckets": [...], "nextToken": "..."}`. `nextToken` is
  **absent** (not null) when no more pages — match this by
  `#[serde(skip_serializing_if = "Option::is_none")]`.
- Empty state: `{"vectorBuckets":[]}` with no `nextToken`.

**GetVectorBucket**
- Request: `POST /GetVectorBucket` with exactly one of `{"vectorBucketName": "..."}`
  or `{"vectorBucketArn": "..."}`. Both must be accepted.
- Success: `{"vectorBucket":{"vectorBucketName":"...","vectorBucketArn":"...","creationTime":<int>,"encryptionConfiguration":{"sseType":"AES256"}}}`.
  The `encryptionConfiguration` is **always present** on the response,
  defaulting to `{"sseType": "AES256"}` (SSE-S3) when CreateVectorBucket
  was called without one. We mirror that default.
- Not found: HTTP 404, header `x-amzn-errortype: NotFoundException`,
  body `{"message":"The specified vector bucket could not be found"}`.

**DeleteVectorBucket**
- Request: `POST /DeleteVectorBucket` with exactly one of `{"vectorBucketName": "..."}`
  or `{"vectorBucketArn": "..."}`.
- Success: HTTP 200, empty body `{}` (Content-Length 0 over the wire).
- Not found: same shape as GetVectorBucket above (404 / NotFoundException
  / same message string).
- AWS also blocks delete when indexes still exist on the bucket — we
  don't have indexes yet so this code path is deferred until Round B.

### C-8 — Test-bucket cleanup must run on the test's own tokio runtime

First attempt at cleanup used a sync `Drop` impl that spun up a brand-new
`tokio::runtime::Runtime` in a separate thread and `block_on`'d the
`DeleteVectorBucket` call. **It silently failed against real AWS**, leaking
6+ vector buckets in the account before we noticed via `aws s3vectors
list-vector-buckets`.

Likely cause: the `aws-sdk-s3vectors` `Client` owns HTTP connection-pool
state and timers that are bound to the **test's** tokio runtime; calling
it from a thread-local runtime gives the delete request no executor for
its sub-tasks. The thread joins, the test exits, the delete never lands.

The fix is `harness::with_bucket(client, prefix, body)` — wraps the test
body in `futures::FutureExt::catch_unwind`, awaits the delete on the
test's runtime even when the body panics, then re-raises. Cleanup is
now synchronous with the test's reactor.

**General rule.** If you need RAII-style cleanup around AWS SDK calls in
an async test, *don't* use a sync `Drop` that spins its own runtime.
Use an async scope helper instead.

---

## What's done

| Slice | Status | Tests (local + AWS) |
| --- | --- | --- |
| Repo bootstrap (Cargo workspace, docker-compose, CLAUDE.md, /health) | ✅ done | — |
| `CreateVectorBucket` round-trip | ✅ done | `tests/create_vector_bucket.rs::*_create_vector_bucket_round_trips` |
| `CreateVectorBucket` duplicate-name → `ConflictException` | ✅ done | `*_create_vector_bucket_duplicate_returns_conflict` |
| `ListVectorBuckets` w/ `prefix` filter + `maxResults`/`nextToken` pagination | ✅ done | `tests/list_vector_buckets.rs::*_list_vector_buckets_prefix_and_pagination` |
| `GetVectorBucket` by name (incl. default `AES256` encryption shape) | ✅ done | `tests/get_vector_bucket.rs::*_get_vector_bucket_by_name_returns_default_encryption` |
| `GetVectorBucket` by ARN | ✅ done | `*_get_vector_bucket_by_arn` |
| `GetVectorBucket` missing → `NotFoundException` | ✅ done | `*_get_vector_bucket_missing_is_not_found` |
| `DeleteVectorBucket` by name (idempotent, then-gone) | ✅ done | `tests/delete_vector_bucket.rs::*_delete_vector_bucket_by_name_then_gone` |
| `DeleteVectorBucket` by ARN | ✅ done | `*_delete_vector_bucket_by_arn` |
| `DeleteVectorBucket` missing → `NotFoundException` | ✅ done | `*_delete_missing_is_not_found` |

Crates currently in the workspace: `api` (bin `marila`), `aws_compat`,
`core`, `storage`, `vectors`, `integration_tests`. Tables side
(`crates/tables`) is not present yet.

## What's next

Once the bootstrap slice is green, the next feature should follow the same
recipe:

1. `aws s3vectors <op> --debug` to capture the wire shape.
2. Add the operation to `crates/integration_tests/tests/<op>.rs` with both
   `local_*` and `aws_*` variants.
3. Implement minimally in `marila-vectors` until green on both targets.
4. Refactor, commit.

Suggested order (low risk → higher):
- `CreateIndex` (forces DuckDB VSS extension on engine open — big jump,
  see `doc/DISCOVERIES.md` D-11 / D-6). This is the next slice the user
  has signed off on.
- `ListIndexes`, `GetIndex`, `DeleteIndex` (mirrors the bucket-CRUD
  contract pattern).
- `PutVectors` / `GetVectors` / `DeleteVectors` / `ListVectors` (the
  data-plane on top of `vec_<b>_<i>` backing tables).
- `QueryVectors` (the headline op — needs the Mongo-filter → SQL bridge
  per `doc/ARCHITECTURE.md` §5.3 and the post-filter caveat in D-12).
- … then the tables side, starting from `CreateTableBucket` (forces
  Lakekeeper + Postgres into the compose graph; uncomment the deferred
  service blocks in `docker-compose.yml` and add the bootstrap one-shots
  per `doc/DISCOVERIES.md` D-7).

## Operational notes

- Run the full suite end-to-end:
  ```bash
  docker compose up -d rustfs        # if not already
  cargo test --workspace -- --test-threads=1
  ```
  Single-threaded because the local target re-uses one bound port (`:8080`)
  via a `OnceLock`-cached marila child process — parallel tests would
  collide on bucket-name state in the shared DuckDB.
- Skip the AWS contract tests by unsetting `AWS_*` env / removing
  `~/.aws/credentials`; the `aws_*` tests print `[skipped]` and the
  suite stays green.
- Test buckets in the AWS account are auto-deleted via
  `harness::with_bucket`. If a panic *outside* `with_bucket` ever leaks
  one, look for `marila-it-create-*` and `marila-it-dup-*` names in
  `aws s3vectors list-vector-buckets` and delete by hand.
