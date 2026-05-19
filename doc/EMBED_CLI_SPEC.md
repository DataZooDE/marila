# `marila-embed` — embedding CLI for marila S3 Vectors

> Status: spec v0.1, 2026-05-19. Author: Claude. Companion to
> `REQUIREMENTS.md` (the storage-side façade) and the existing
> `demo/demo_vectors.py` which currently does the same job inline.

## 1. Why

The marila S3 Vectors façade is pure storage. To put data in or query
it, the caller has to (a) parse their documents, (b) chunk them, (c)
call an embedding API, (d) call `PutVectors`. AWS has shipped two
convenience layers above raw S3 Vectors:

- **[`s3vectors-embed-cli`][1]** (open-source, awslabs) — a thin CLI
  wrapping a single `BedrockRuntime::InvokeModel` call plus the
  matching `PutVectors` / `QueryVectors`. **Explicit non-feature:
  "Document chunking is not currently supported."** One file ≈ one
  vector.
- **[Bedrock Knowledge Bases][2]** — the fully-managed RAG service.
  Out of scope for this spec; would be its own façade and crate.

This spec covers the tier-2 equivalent: `marila-embed`, a small CLI
that turns *"I have a directory of documents and want them searchable"*
into one command, **without** taking on the operational weight of a
Knowledge-Bases-style ingestion service.

[1]: https://github.com/awslabs/s3vectors-embed-cli
[2]: https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-bedrock-kb.html

## 2. Design principles

1. **Reuse `magpie-rs` for everything we'd otherwise have to write.**
   The sibling project (`../magpie-rs`, same workspace, MIT-licensed)
   already implements:
   - `magpie-extract` — 57+ document formats incl. PDF, DOCX, HTML,
     Markdown, images via OCR.
   - `magpie-core` — chunking strategies (fixed, sentence, markdown
     header-aware, AST-aware for 7 programming languages).
   - `magpie-embed` — Ollama / OpenAI / Gemini / Candle providers
     behind a single trait.
   We pull these in as `path = "../magpie-rs/crates/..."` dependencies
   and contribute **only the s3vectors-aware glue**. Reimplementing any
   of this is a non-goal.
2. **Match the `s3vectors-embed-cli` surface where it makes sense.**
   AWS already documented the two-command shape (`put` / `query`) and
   the metadata conventions (`S3VECTORS-EMBED-SRC-CONTENT`,
   `S3VECTORS-EMBED-SRC-LOCATION`). Users coming from AWS docs should
   recognise the shape immediately. Deviate only with reason.
3. **Add chunking** — the one place we go *beyond* AWS's CLI. Magpie
   gives it to us essentially for free, and the lack of chunking is the
   single biggest critique of `s3vectors-embed-cli`'s ergonomics.
4. **No new façade routes on marila.** The CLI talks to marila over
   the existing `aws-sdk-s3vectors` client. Marila stays a pure
   storage/query service.
5. **One binary, no daemon.** Stateful state is the S3 Vectors index;
   the CLI is a one-shot orchestrator.

## 3. Scope

In scope:
- `marila-embed put` — ingest text / files / globs → chunks → embeddings → `PutVectors`.
- `marila-embed query` — text → embedding → `QueryVectors` → render hits.
- Embedding providers: **OpenAI** (default, user has a key) and **Ollama** (local, no key needed). Bedrock comes later if we ever need AWS-target parity.
- Document formats: whatever `magpie-extract` already supports (57+).
- Chunking strategies: fixed-size, markdown header-aware, code AST-aware — exposed via `--chunk-strategy`.

Out of scope:
- Hybrid (BM25 + vector) search — marila's QueryVectors is vector-only by design (REQUIREMENTS Q-1: honest semantics).
- Re-ranking with a cross-encoder — magpie does it, but it's a post-processing step the caller can layer on.
- Image / video / audio inputs — magpie's text-only chunkers cover the spec; image embeddings would need an image-capable provider and an embedding-model mode switch. Add later if asked.
- Anything Bedrock Knowledge-Bases-shaped (managed ingestion jobs, source-bucket sync, custom Lambda transforms). That's tier 3 and its own project.

## 4. CLI surface

### 4.1 Common flags

| Flag | Default | Notes |
|---|---|---|
| `--endpoint-url` | `http://localhost:8080` | marila base URL; overridable via `MARILA_ENDPOINT` env |
| `--region` | `eu-west-1` | same shape as `aws --region`, env `MARILA_REGION` |
| `--profile` | (unused) | reserved for future SDK-profile selection |
| `--vector-bucket-name` | required | the s3vectors bucket |
| `--index-name` | required | the index within the bucket |
| `--embedding-provider` | `openai` | `openai` \| `ollama` |
| `--embedding-model` | `text-embedding-3-small` (openai) / `mxbai-embed-large` (ollama) | passed through to the provider |
| `--dimension` | provider-default | only required when the index doesn't exist yet AND the model has multiple dim options (e.g. text-embedding-3-small can produce 512 / 1024 / 1536) |
| `--debug` | false | tracing at debug level |
| `--output` | `json` | `json` \| `table` (matches AWS) |

### 4.2 `marila-embed put`

```text
marila-embed put [common flags]
    [--text-value <string>]              # direct string, one vector
    [--text <path-or-glob>...]           # local file(s), supports glob like ./docs/*.md
    [--key <string>]                     # explicit key (single-input forms only)
    [--key-prefix <string>]              # prepended to generated keys (`<prefix><filename>#<chunk_idx>`)
    [--filename-as-key]                  # use the filename (or text hash) as key
    [--metadata <json>]                  # extra metadata merged into every vector
    [--chunk-strategy <name>]            # off | fixed | markdown | code (default: fixed)
    [--chunk-size <int>]                 # chunk size in tokens; default 400 for fixed/markdown
    [--chunk-overlap <int>]              # overlap in tokens; default chunk-size/5
    [--max-workers <int>]                # parallelism for the embedding calls; default 4
    [--batch-size <int>]                 # PutVectors batch; clamped to 1..=500, default 100
    [--dry-run]                          # parse + chunk + show what would be written, no API calls
```

Behaviour notes:
- `--text-value` is mutually exclusive with `--text`. Exactly one must be given.
- `--text` accepts plain paths, glob patterns (`./docs/**/*.md`), and `s3://bucket/prefix/*` URIs (deferred — for v0, local only).
- If `--key` is given and the input expands to >1 vector, fail clearly (no implicit suffixing — same as AWS).
- Standard metadata (always added; non-overrideable like AWS):
  - `S3VECTORS-EMBED-SRC-CONTENT` — original chunk text (truncated to fit metadata size cap)
  - `S3VECTORS-EMBED-SRC-LOCATION` — local path or s3:// URI
  - `S3VECTORS-EMBED-CHUNK-IDX` — chunk index within the source file (marila addition, absent on `--chunk-strategy off`)
- Index auto-create: if the index doesn't exist, create it with `dataType=float32`, `distanceMetric=cosine`, and the dim derived from a one-shot probe embedding. Print a one-line note when this happens.

### 4.3 `marila-embed query`

```text
marila-embed query [common flags]
    [--text-value <string>]              # direct query string
    [--text <path-or-glob>]              # single file used as query; >1 file is an error
    [--k <int>]                          # topK, default 5
    [--filter <json>]                    # Mongo-style metadata filter (passed through to QueryVectors)
    [--return-metadata <bool>]           # default true
    [--return-distance <bool>]           # default true
```

Output (`--output json`):
```json
{
  "distanceMetric": "cosine",
  "vectors": [
    {"key": "...", "distance": 0.13,
     "metadata": {"S3VECTORS-EMBED-SRC-LOCATION": "doc/REQUIREMENTS.md", ...}}
  ]
}
```

`--output table` renders the same data as `key | distance | source` + a wrapped snippet from `S3VECTORS-EMBED-SRC-CONTENT`.

## 5. Architecture

```
crates/embed-cli/
├── Cargo.toml         depends on path = "../../magpie-rs/crates/magpie-{extract,core,embed}"
└── src/
    ├── main.rs         clap parser, dispatches to put/query
    ├── put.rs          orchestrates extract → chunk → embed → PutVectors
    ├── query.rs        embed query → QueryVectors → render
    ├── client.rs       thin wrapper around aws-sdk-s3vectors::Client (config from env/flags)
    ├── chunking.rs     glue: maps --chunk-strategy onto magpie-core's strategies
    └── output.rs       json vs table rendering
```

Workspace member added to `Cargo.toml`:
```toml
members = [..., "crates/embed-cli"]
```

The CLI is a separate binary from `marila` (the server). They share no
in-process state — the CLI talks to marila over HTTP like any other
client.

## 6. Implementation order

1. **CLI skeleton + `client.rs`** — clap subcommands, common flags
   parsed, `aws-sdk-s3vectors::Client` configured from env. `put` and
   `query` exit with `unimplemented`.
2. **`marila-embed put --text-value` (no chunking)** — single direct
   string, OpenAI embedding, one `PutVectors` call. End-to-end integration
   test: spin up marila, put one string, ListVectors confirms.
3. **`marila-embed query --text-value`** — single query, render top-K
   as JSON. Test: put 3 strings, query the closest, assert ordering.
4. **`marila-embed put --text <glob>` (no chunking, file-per-vector)** —
   matches AWS's CLI exactly. Each matched file becomes one vector with
   the `S3VECTORS-EMBED-SRC-*` standard metadata.
5. **Chunking (`--chunk-strategy fixed`)** — wire magpie-core's
   fixed-size chunker. Each file expands to N vectors.
6. **Auto-create index** — probe-embed to get dim, then `CreateIndex`
   if the index doesn't exist.
7. **`--chunk-strategy markdown` + `code`** — additional magpie
   strategies. Selected per-file by extension when `--chunk-strategy
   auto`.
8. **Ollama provider** — second backend behind the same trait
   `magpie-embed` already exposes.
9. **`--dry-run`** — useful for diff-ing what changed before a re-ingest.
10. **`--batch-size` + `--max-workers`** — performance polish; default
    settings tuned by running against the marila docs corpus.

Each step ends with an integration test in `crates/embed-cli/tests/`
that spins up marila (the existing `MarilaProcess` harness from
`crates/integration_tests`) and runs the CLI as a subprocess.

## 7. Drop-in demo replacement

After step 5 (chunking), `demo/demo_vectors.py` collapses to:

```bash
marila-embed put \
  --vector-bucket-name marila-docs \
  --index-name rag-docs \
  --embedding-provider openai \
  --embedding-model text-embedding-3-small \
  --chunk-strategy markdown \
  --text 'README.md' --text 'CLAUDE.md' --text 'doc/*.md'

marila-embed query \
  --vector-bucket-name marila-docs \
  --index-name rag-docs \
  --embedding-provider openai \
  --embedding-model text-embedding-3-small \
  --text-value "How does marila validate vector dimensions?" \
  --k 3
```

The Python script becomes a thin wrapper that calls these two for the
demo narrative, or goes away entirely.

## 8. Open questions

- **Re-ingest semantics.** Do we de-duplicate by key (overwrite — same as
  AWS PutVectors) or by content hash (skip unchanged)? AWS's CLI
  overwrites by key. Recommendation: match AWS for v0; add
  `--skip-unchanged` later if asked.
- **Magpie versioning.** Pinning to `path = "../magpie-rs/crates/..."`
  ties us to its current API. When magpie publishes to crates.io
  (per its release-binaries roadmap), switch to version pinning.
  Until then we accept the close coupling.
- **Bedrock provider.** AWS's CLI is Bedrock-only. Skipping Bedrock in
  marila's CLI is a deliberate choice (the user has OpenAI; Bedrock
  needs an AWS account + IAM dance for InvokeModel). Add only when
  someone actually needs AWS-target parity for embedding too.

## 9. Definition of done

- `cargo install --path crates/embed-cli` produces a `marila-embed`
  binary.
- A green integration test for each step in §6.
- `demo/demo_vectors.py` is replaced (or wraps) by a shell script that
  calls `marila-embed put` + `marila-embed query` and produces the same
  cited-RAG output.
- `CLAUDE.md` "What's done" gains the entries; the spec lives at
  `doc/EMBED_CLI_SPEC.md` (this file).
