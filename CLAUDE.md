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

### C-2c — CreateIndex / DeleteIndex + bucket-non-empty wire shapes (probed 2026-05-17)

**CreateIndex**
- Request: `POST /CreateIndex` body:
  ```json
  {
    "vectorBucketName": "<bucket>",    // OR vectorBucketArn
    "indexName": "<index>",
    "dataType": "float32",             // only allowed value
    "dimension": <1..=4096>,
    "distanceMetric": "cosine"|"euclidean"
    // optional: encryptionConfiguration, metadataConfiguration, tags
  }
  ```
- Success: HTTP 200, body
  `{"indexArn":"arn:aws:s3vectors:<region>:<account>:bucket/<bucket>/index/<index>"}`.
  Note the **nested** ARN — `:bucket/<b>/index/<i>` (not a separate `:index/` resource type).
- Duplicate index: HTTP 409 `ConflictException`,
  body `{"message":"An index with the specified name already exists"}`.
- Bucket missing: HTTP 404 `NotFoundException`,
  body `{"message":"The specified vector bucket could not be found"}` (identical text to GetVectorBucket).
- Dimension out of range (e.g. 9999): HTTP 400 `ValidationException`
  with smithy-shaped `fieldList` detail.

**DeleteIndex**
- Request: `POST /DeleteIndex` body
  `{"vectorBucketName":"<b>","indexName":"<i>"}` (or `vectorBucketArn`).
- Success: HTTP 200, empty.
- Missing index: HTTP 404 `NotFoundException`,
  body `{"message":"The specified index could not be found"}` (note: **index**, not **vector bucket**).

**DeleteVectorBucket-with-indexes**
- HTTP 409 `ConflictException`,
  body `{"message":"The specified vector bucket is not empty"}`.
  Once we track indexes in marila state, the `DeleteVectorBucket`
  handler must check for them and refuse with this exact message.

### C-2d — ListIndexes / GetIndex wire shapes (probed 2026-05-17)

**ListIndexes**
- Request: `POST /ListIndexes` body
  `{"vectorBucketName":"<b>", "prefix":"...", "maxResults":N, "nextToken":"..."}`
  (or `vectorBucketArn`). Only `vectorBucketName` (or its arn) is required.
- Success: `{"indexes":[...], "nextToken":"..."}`. `nextToken` is
  **absent** on the last page. Each summary entry has only
  `{vectorBucketName, indexName, indexArn, creationTime}` — **no**
  dataType/dimension/distanceMetric (those are GetIndex-only).
- Bucket missing: HTTP 404 `NotFoundException`,
  body `{"message":"The specified vector bucket could not be found"}`.

**GetIndex**
- Request: `POST /GetIndex` body either
  `{"vectorBucketName":"<b>", "indexName":"<i>"}` (both required together)
  OR `{"indexArn":"arn:...:bucket/<b>/index/<i>"}`. Mixing them is a
  ValidationException at AWS — we treat name+arn-of-bucket as invalid
  too, mirroring the spirit of GetVectorBucket.
- Success: `{"index": {vectorBucketName, indexName, indexArn,
  creationTime, dataType, dimension, distanceMetric,
  encryptionConfiguration: {sseType}}}`. The dimension is a JSON
  number; encryptionConfiguration defaults to `{"sseType": "AES256"}`
  per CLAUDE.md C-2b.
- Missing index: HTTP 404 `NotFoundException`,
  body `{"message":"The specified index could not be found"}`.

### C-2e — Data-plane wire shapes: PutVectors / GetVectors / ListVectors / DeleteVectors (probed 2026-05-17)

**PutVectors**
- Request: `POST /PutVectors` body
  ```json
  {
    "vectorBucketName": "<b>",            // or vectorBucketArn
    "indexName": "<i>",                   // or indexArn
    "vectors": [
      {
        "key": "<key>",
        "data": {"float32": [<float>, ...]},   // dim must match index
        "metadata": {...}                       // optional, free-form JSON
      },
      ...
    ]
  }
  ```
  `vectors` is min 1 / max 500. `key` is 1..=1024 chars.
- Success: HTTP 200, body `{}`.
- Validation (dim mismatch): HTTP 400 `ValidationException`, body
  `{"fieldList":[{"path":"vectors[0]","message":"vector must have length 4, but has length 2"}],"message":"Invalid record for key 'x': vector must have length 4, but has length 2"}`
  Note the **fieldList** detail — restJson1 standard shape for
  per-field errors. We may simplify to top-level `message` only.

**GetVectors**
- Request: `POST /GetVectors` body
  `{"vectorBucketName":"<b>", "indexName":"<i>", "keys":["k1","k2"], "returnData": bool, "returnMetadata": bool}`.
  Both `returnData` and `returnMetadata` default to `false`.
- Success: `{"vectors":[{"key":"k1","data":{"float32":[...]},"metadata":{...}}, ...]}`.
- **Missing keys are silently omitted** — no error. The response is
  shorter than the request.
- **Order is indeterminate** — don't assume request order.
- **float32 round-trip widens precision** in the JSON — e.g. `0.9`
  becomes `0.8999999761581421` because the value goes f64→f32→f64.

**ListVectors**
- Request: `POST /ListVectors` body
  `{"vectorBucketName":"<b>", "indexName":"<i>", "maxResults": N, "nextToken": "...", "returnData": bool, "returnMetadata": bool, "segmentCount": N, "segmentIndex": N}`.
  Segment-* params are for parallel-scan and out of scope for marila.
- Success: `{"vectors":[{key, data?, metadata?}, ...], "nextToken":"..."}`.
- Without `returnData/returnMetadata`, summary items are just `{"key":"..."}`.
- AWS's pagination is more aggressive than ours: requesting
  maxResults=1 against an index with 4 vectors may return
  `{"vectors":[], "nextToken":"..."}` — i.e. an **empty page with a
  cursor** is valid. Clients must loop until `nextToken` is absent.

**DeleteVectors**
- Request: `POST /DeleteVectors` body
  `{"vectorBucketName":"<b>", "indexName":"<i>", "keys":["k1","k2"]}`.
- Success: HTTP 200, body `{}`. **No error for missing keys** —
  delete is silently idempotent.

**Data-plane NotFound collapse**
- ALL four data-plane ops return the same error body for both
  "bucket doesn't exist" AND "index doesn't exist within bucket":
  `x-amzn-errortype: NotFoundException`, body
  `{"message":"The specified index could not be found"}`.
  AWS doesn't expose the distinction. Our handlers must emit the
  **index** message even when the marila state knows only the bucket
  is missing.

### C-2f — QueryVectors wire shape (probed 2026-05-17)

- Request: `POST /QueryVectors` body
  ```json
  {
    "vectorBucketName":"<b>",            // or vectorBucketArn
    "indexName":"<i>",                   // or indexArn
    "topK": N,                            // required, min 1
    "queryVector":{"float32":[...]},      // dim must match index
    "filter":{...},                       // optional, Mongo-style
    "returnDistance": bool,               // default false
    "returnMetadata": bool                // default false
  }
  ```
- Success: `{"distanceMetric":"cosine"|"euclidean","vectors":[
      {"key":"...", "distance":<float>?, "metadata":{...}?, "data":{...}?}, ...
  ]}`
  Note the **distanceMetric echo** on the response — always present
  (the index's configured metric). `distance` only included when
  `returnDistance=true`. `metadata` / `data` follow the same
  request flags.
- Results are ordered by ascending distance (nearest first).
- Filter language is Mongo-style:
  - Implicit `$eq`: `{"field":"value"}` ≡ `{"field":{"$eq":"value"}}`
  - Comparison: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`
  - Set: `$in`, `$nin` (array values)
  - Logical: `$and` (array of sub-filters), `$or`, `$not`
- Missing index: HTTP 404, body `{"message":"The specified index could not be found"}`.
- Dimension mismatch: HTTP 400 `ValidationException`,
  body `{"fieldList":[{"path":"vector","message":"Invalid input: vector must have length 4, but has length 2"}],"message":"Query vector contains invalid values or is invalid for this index"}`
- **Filter-while-search caveat** (`doc/DISCOVERIES.md` D-12): AWS
  evaluates the filter during HNSW traversal so a restrictive filter
  with `topK=10` still returns 10 matches. DuckDB-VSS post-filters by
  default, collapsing recall under restrictive filters. Marila's
  v0 implementation oversamples (e.g. `LIMIT topK * 100`) then
  post-filters to buy headroom; structural fix deferred.

### C-9 — S3 Tables uses REST+JSON (not awsJson1.0); different envelope from s3vectors

Probed 2026-05-17, eu-west-1. Despite what `doc/ARCHITECTURE.md` §8
implies, S3 Tables doesn't speak `aws-json-1.0`. It's a **RESTful
JSON** API with verb-and-path routing, like a stripped-down REST
service:

**CreateTableBucket**
- `PUT /buckets` body `{"name":"<name>"}`
- Success: HTTP 200, body `{"arn":"arn:aws:s3tables:<region>:<account>:bucket/<name>"}`
- Duplicate: HTTP 409, body `{"message":"The bucket that you tried to create already exists, and you own it."}`

**ListTableBuckets**
- `GET /buckets` (no body)
- Success: `{"tableBuckets":[{"arn","createdAt","name","ownerAccountId","tableBucketId","type"}, ...]}`
- `createdAt`: ISO 8601 with **nanosecond precision + UTC `Z`** suffix,
  e.g. `"2026-05-17T19:26:46.216057410Z"`. Distinct from s3vectors'
  epoch-seconds (CLAUDE.md C-2a).
- `tableBucketId`: server-minted UUID (we mint one with `uuid::Uuid::new_v4()`).
- `type`: `"customer"` for buckets we created (vs. `"aws"` for service-owned).
- `ownerAccountId`: AWS account id (we use `MARILA_AWS_ACCOUNT_ID`).

**GetTableBucket**
- `GET /buckets/{url-encoded-arn}`
- Success: same shape as a list item.
- Missing: HTTP 404, body `{"message":"The specified bucket does not exist."}`.

**DeleteTableBucket**
- `DELETE /buckets/{url-encoded-arn}`
- Success: HTTP 204, empty body.
- Missing: same 404 body as GetTableBucket.

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
| `DeleteVectorBucket` with surviving indexes → `ConflictException` ("not empty") | ✅ done | `*_delete_bucket_with_index_returns_conflict` |
| `CreateIndex` round-trip (returns nested `:bucket/<b>/index/<i>` ARN) | ✅ done | `tests/create_index.rs::*_create_index_round_trips` |
| `CreateIndex` duplicate → `ConflictException` ("An index with the specified name already exists") | ✅ done | `*_create_index_duplicate_returns_conflict` |
| `CreateIndex` missing bucket → `NotFoundException` (bucket-not-found body text) | ✅ done | `*_create_index_missing_bucket_is_not_found` |
| `ListIndexes` w/ `prefix` + cursor pagination | ✅ done | `tests/list_get_delete_index.rs::*_list_indexes_prefix_and_pagination` |
| `ListIndexes` missing bucket → `NotFoundException` (bucket body) | ✅ done | `*_list_indexes_missing_bucket_is_not_found` |
| `GetIndex` by name (full `IndexDescription` shape: dataType, dimension, distanceMetric, AES256) | ✅ done | `*_get_index_by_name_returns_full_description` |
| `GetIndex` by `indexArn` | ✅ done | `*_get_index_by_arn` |
| `GetIndex` missing → `NotFoundException` (index body) | ✅ done | `*_get_index_missing_is_not_found` |
| `DeleteIndex` happy path (then-gone via GetIndex 404) | ✅ done | `*_delete_index_then_get_is_not_found` |
| `DeleteIndex` missing → `NotFoundException` (index body) | ✅ done | `*_delete_missing_index_is_not_found` |
| `PutVectors` / `GetVectors` round-trip (incl. silent-omit of missing keys) | ✅ done | `tests/data_plane.rs::*_put_then_get_round_trips` |
| `DeleteVectors` silently idempotent (mixed-existing/missing keys) | ✅ done | `*_delete_vectors_is_silently_idempotent` |
| `ListVectors` w/ `maxResults`/`nextToken` pagination (loop-until-absent) | ✅ done | `*_list_vectors_paginates` |
| `PutVectors` missing index → `NotFoundException` (collapsed bucket/index) | ✅ done | `*_put_vectors_on_missing_index_is_not_found` |
| `PutVectors` dim mismatch → `ValidationException` | ✅ done | `*_put_vectors_dim_mismatch_is_validation` |
| `GetVectors` returns metadata when `returnMetadata=true` | ✅ done | `*_get_vectors_returns_metadata_when_requested` |
| `QueryVectors` unfiltered topK + `distanceMetric` echo + `returnDistance` | ✅ done | `tests/query_vectors.rs::*_query_vectors_unfiltered_returns_anchor_first` |
| `QueryVectors` Mongo `filter` (implicit `$eq`, `$gt/$lt/$gte/$lte`, `$in/$nin`, `$and/$or/$not`) | ✅ done | `*_query_vectors_metadata_filter_excludes_non_matching` |
| `QueryVectors` missing index → `NotFoundException` (collapsed index body) | ✅ done | `*_query_vectors_missing_index_is_not_found` |
| `QueryVectors` dim mismatch → `ValidationException` | ✅ done | `*_query_vectors_dim_mismatch_is_validation` |

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
- Add a RustFS snapshot path for `PutVectors` per FV-4 (today marila
  stores vectors only in DuckDB; AWS contract is satisfied but the
  durability promise — "RustFS is the source of truth" — isn't yet).
- Tables side, starting from `CreateTableBucket` (forces Lakekeeper +
  Postgres into the compose graph; uncomment the deferred service
  blocks in `docker-compose.yml` and add the bootstrap one-shots per
  `doc/DISCOVERIES.md` D-7).
- VSS HNSW recall under restrictive filter — D-12 oversample
  mitigation (current marila inlines the WHERE clause and lets DuckDB
  decide; recall divergence vs. AWS is unproven and only matters at
  scale).

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
