# `marila-embed` — embedding CLI for marila S3 Vectors

> Status: spec v0.2, 2026-05-19. Author: Claude.
> Supersedes the earlier "reuse magpie-rs" draft. Companion to
> `REQUIREMENTS.md` (the storage façade) and the existing
> `demo/demo_vectors.py` which currently does this job inline.

## 1. Why

Marila's S3 Vectors façade is pure storage. To put data in or query
it, the caller has to (a) parse their documents, (b) chunk them, (c)
call an embedding API, (d) call `PutVectors`. AWS has shipped two
convenience layers above raw S3 Vectors:

- **[`s3vectors-embed-cli`][1]** (open-source, awslabs) — a thin CLI
  wrapping `BedrockRuntime::InvokeModel` plus `PutVectors` /
  `QueryVectors`. **Explicit non-feature**: *"Document chunking is not
  currently supported"* — one file = one vector. Bedrock-only.
- **[Bedrock Knowledge Bases][2]** — the fully-managed RAG service.
  Out of scope for this spec; would be its own façade.

`marila-embed` is the **tier-2 equivalent**: a small one-shot CLI that
turns *"I have a corpus and want it queryable"* into a single command,
without taking on managed-ingestion-service operational weight.

Crucially, this spec is for a **clean reimplementation in-tree**, not a
shim over an external library. The reason is the four qualities the
user explicitly called out — scalability, parallelization, bounded
memory, and a real pluggable-embeddings trait — are not bolt-ons; they
have to be in the bones of the design.

[1]: https://github.com/awslabs/s3vectors-embed-cli
[2]: https://docs.aws.amazon.com/AmazonS3/latest/userguide/s3-vectors-bedrock-kb.html

## 2. Design goals (the four qualities, ranked)

1. **Bounded memory regardless of corpus size.** A 100 GB corpus uses
   the same peak RAM as a 100 MB corpus, modulo small bookkeeping.
   Streaming top-to-bottom; no `.collect::<Vec<_>>()` on anything
   corpus-sized.
2. **Pipeline parallelism.** Parse, chunk, embed, and put are four
   stages connected by bounded channels. Each stage runs concurrently;
   each can be tuned independently. Backpressure flows naturally:
   when `PutVectors` slows, embedding eventually blocks, then chunking,
   then parsing.
3. **Pluggable embeddings behind one trait.** OpenAI, Ollama, Bedrock,
   and a deterministic test stub all sit behind the same async trait.
   Swapping providers is a CLI flag, not a code change. Adding a new
   provider is a single-file impl.
4. **Resumable, idempotent, observable.** Deterministic keys
   (content-hash) so re-runs overwrite identically. A checkpoint file
   so a crashed run resumes from where it stopped. Structured progress
   to TTY (a single updating line) and to JSON-lines for piping.

Non-goals (deliberate): hybrid search (BM25), cross-encoder reranking,
managed source-bucket sync (that's tier-3 Bedrock-KB territory),
image/audio/video inputs (text is enough for v0).

## 3. Architecture: the pipeline

```
   ┌───────────┐    ┌───────────┐    ┌───────────┐    ┌───────────┐
   │  Source   │───▶│  Parse    │───▶│  Chunk    │───▶│  Embed    │
   │  (walk +  │ ch │  (PDF /   │ ch │  (fixed / │ ch │  (OpenAI/ │
   │  filter)  │ 1  │  md/html/ │ 2  │  md/sent) │ 3  │  Ollama/  │
   └───────────┘    │  txt)     │    └───────────┘    │  Bedrock) │
                    └───────────┘                     └─────┬─────┘
                                                            │
                                                       channel 4
                                                            ▼
                                                       ┌────────────┐
                                                       │ Batch &    │
                                                       │ Put        │
                                                       │ (s3vectors)│
                                                       └─────┬──────┘
                                                             │
                                                       ┌─────▼─────┐
                                                       │ Checkpoint│
                                                       │ + Metrics │
                                                       └───────────┘
```

Each box is a separate `tokio::task` (or a pool, where parallel CPU
work matters). Each `ch N` is a bounded `tokio::sync::mpsc` channel.

**Channel capacities (defaults, all `--*-buffer` overridable):**

| Channel | Default cap | Element | Memory bound |
|---|---|---|---|
| `1 Source → Parse` | 64 | `RawDoc { path, bytes }` | 64 × avg-file-size; user caps per-file with `--max-file-bytes` (default 50 MB; skip with warning) |
| `2 Parse → Chunk` | 64 | `ParsedDoc { path, text, kind }` | text is borrowed Bytes-shaped; we hold at most ~64 docs' worth of plaintext |
| `3 Chunk → Embed` | 512 | `Chunk { id, text, meta }` | 512 chunks × ~2 KB each ≈ 1 MB |
| `4 Embed → Put` | 512 | `EmbeddedChunk { id, vec, meta }` | 512 × (dim×4 B + meta); for 1536-d ≈ 3 MB |

Total steady-state heap ≈ tens of MB even at billion-document scale.
The corpus size never enters the equation.

### 3.1 Source stage

- **Walks** local globs and (eventually) S3 prefixes lazily.
  `walkdir` + `globset` for local; SDK pagination for S3.
- Filters by extension (`--include`, `--exclude`) and size cap.
- Re-emits each `RawDoc` as a `(path, content_hash, lazy_reader)`
  triple. **We do NOT load the bytes here** — we hand a `Read` (or
  async equivalent) to the parser. PDFs in particular benefit from
  streaming page-by-page.
- Skips paths already in the checkpoint set in O(1) without reading
  them.

### 3.2 Parse stage

- **Pool of workers**, sized by `--parse-concurrency` (default
  `num_cpus / 2`). Parsing is CPU-bound (PDF/DOCX especially) so
  separate from the I/O-bound stages.
- Workers pull `RawDoc` from channel 1, dispatch on extension to a
  `Parser` impl, emit `ParsedDoc` to channel 2.
- Parser trait:
  ```rust
  trait Parser: Send + Sync {
      fn name(&self) -> &str;
      fn can_handle(&self, ext: &str) -> bool;
      fn parse(&self, raw: RawDoc) -> Result<ParsedDoc>;
  }
  ```
- v0 parsers: `text`, `markdown` (preserves heading structure as
  hints), `html` (`scraper` → text + headings).
- v0.5 parsers: `pdf` (`lopdf` for fast text-layer extract; warn on
  scan-only PDFs that would need OCR — out of scope), `docx`
  (`docx-rs`).

### 3.3 Chunk stage

- **Single-task** by default — chunking is cheap relative to embedding,
  and a single task preserves the natural ordering useful for stable
  keys.
- Chunker trait:
  ```rust
  trait Chunker: Send + Sync {
      fn name(&self) -> &str;
      fn chunk<'a>(&self, doc: &'a ParsedDoc)
          -> Box<dyn Iterator<Item = Chunk> + 'a>;
  }
  ```
- v0 strategies:
  - `fixed`: N tokens (default 400), overlap (default 80). Token
    counter from `tiktoken-rs` when the provider is OpenAI;
    char-based fallback otherwise. (`tokens ≈ chars / 4` underestimate
    is acceptable for v0 — it's conservative.)
  - `markdown`: split at the highest-priority heading boundary that
    keeps each chunk ≤ N tokens. Preserves `section_path` in metadata
    (e.g. `["# Methodology", "## Test harness"]`).
  - `sentence`: sentence-aware grouping up to N tokens, using
    `unicode-segmentation` for boundary detection.
- A chunk's `id` is `blake3(source_path || chunk_idx || text)` —
  deterministic, content-addressed, the key passed to `PutVectors`.

### 3.4 Embed stage

- **Pool of in-flight requests**, sized by `--embed-concurrency`
  (default 8). Each task pulls up to `--embed-batch` chunks (default
  100), shapes a single API call, sends the embeddings to channel 4.
- Provider trait:
  ```rust
  #[async_trait]
  trait EmbeddingProvider: Send + Sync {
      fn name(&self) -> &str;
      fn dimension(&self) -> u32;
      fn max_batch(&self) -> usize;   // hint, e.g. 2048 for OpenAI
      fn max_tokens_per_input(&self) -> usize; // e.g. 8191 for text-embedding-3-small

      async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>>;
  }
  ```
- Failure handling per request (in order):
  - HTTP 429 / 5xx → exponential backoff (jittered, max 6 retries)
  - HTTP 4xx (other) → fail the whole batch, surface upstream
  - Per-input failures from the provider response (rare; OpenAI returns
    these in the body) → re-emit the offending chunks individually
- Cost telemetry: each provider reports `tokens_in` per call so the
  CLI can show running cost. OpenAI provides this in the response;
  Ollama doesn't (we estimate from `len(input)`).
- **Idempotency:** the chunk's id is content-addressed; an already-put
  vector is overwritten with the same value at no semantic cost.

### 3.5 Put stage

- **Single-task** batcher: accumulates `EmbeddedChunk`s up to
  `--put-batch` (default 100, AWS hard cap 500) or `--put-flush-ms`
  (default 250 ms), then issues one `PutVectors` call.
- Failures: same retry policy as embed. After max retries, log the
  failed batch's keys to the error report and continue (we don't
  abort the whole run for one bad batch).
- After each successful PutVectors, marks every chunk's `source_path`
  as in-progress in the checkpoint; once every chunk for a source is
  in, the path graduates to `done` and won't be re-read on resume.

### 3.6 Checkpoint

- **JSONL append-only** at `--checkpoint` (default
  `./.marila-embed-checkpoint.jsonl`).
- Each line: `{source_path, content_hash, chunk_count, status: "done"|"partial", at: <ts>}`.
- On resume: load into a HashSet, source stage skips paths whose
  `(path, content_hash)` already says `done`. Partials are re-tried
  from scratch (chunks are deterministic so PutVectors overwrites
  cleanly).
- Checkpoint is **fsynced** per write (synchronous, blocking the put
  stage's progress only when we mark a path done — bounded amortised
  cost).

### 3.7 Progress + observability

- TTY: a single in-place line updated at 4 Hz:
  ```
  parsed 1242/1250  chunked 14823  embedded 14801  put 14600  rate 480 vec/s  cost $0.12
  ```
- JSON-lines log to `--log` (default `./.marila-embed.jsonl`) — one
  event per stage transition for grep-ability.
- Final summary on exit: count, peak RAM (via `procfs`), total
  duration, total cost, error count. Non-zero exit if any chunks
  failed (`--ignore-errors` overrides).

## 4. Concurrency model summary

| Stage | Default workers | Tunable via | Rationale |
|---|---|---|---|
| Source walk | 1 | n/a | I/O on a tree; serial avoids excess `stat` |
| Parse | `num_cpus / 2` | `--parse-concurrency` | CPU-bound (PDF/DOCX) |
| Chunk | 1 | `--chunk-concurrency` (rare) | Cheap; serial preserves ordering |
| Embed | 8 | `--embed-concurrency` | I/O-bound on remote API; tune up if provider rate-limit allows |
| Put | 1 batcher + 4 in-flight | `--put-concurrency` | AWS-style; batches up to 500 vectors |

Total OS threads ≈ tokio runtime + `num_cpus / 2` blocking workers.
The numbers are defaults — every stage has a flag, so users tune for
their provider's rate-limit and their machine's cores without
recompiling.

## 5. Pluggable embeddings — concrete

Three providers shipped at v0:

| Provider | Crate | Auth | Notes |
|---|---|---|---|
| `openai` | `reqwest` + custom client | `OPENAI_API_KEY` | Models: `text-embedding-3-small` (default, 1536-d, dim-reducible), `text-embedding-3-large` (3072-d), `text-embedding-ada-002` |
| `ollama` | `reqwest` | none (local) | Models: anything `ollama pull`-ed; dim auto-detected via one probe call |
| `stub` | none | none | Deterministic hash-based pseudo-embedding for tests; no network |

Adding a new provider:

```rust
// crates/embed-cli/src/embed/cohere.rs
pub struct CohereEmbedder { /* ... */ }

#[async_trait]
impl EmbeddingProvider for CohereEmbedder {
    fn name(&self) -> &str { "cohere" }
    fn dimension(&self) -> u32 { self.dim }
    fn max_batch(&self) -> usize { 96 }
    fn max_tokens_per_input(&self) -> usize { 512 }
    async fn embed(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>> { /* ... */ }
}
```

Register in `embed::factory::for_name("cohere")`. CLI dispatch picks it
up. No other file changes.

## 6. CLI surface

### 6.1 Common flags

| Flag | Default | Env | Notes |
|---|---|---|---|
| `--endpoint-url` | `http://localhost:8080` | `MARILA_ENDPOINT` | marila base URL |
| `--region` | `eu-west-1` | `MARILA_REGION` | for SDK signing |
| `--vector-bucket-name` | required | — | s3vectors bucket |
| `--index-name` | required | — | index within bucket |
| `--embedding-provider` | `openai` | `MARILA_EMBED_PROVIDER` | `openai` \| `ollama` \| `stub` |
| `--embedding-model` | provider-default | `MARILA_EMBED_MODEL` | passed to provider |
| `--config` | `./marila-embed.toml` | — | optional TOML config (flags > env > config > defaults) |
| `--log` | `./.marila-embed.jsonl` | — | structured event log |
| `--checkpoint` | `./.marila-embed-checkpoint.jsonl` | — | resume state |
| `--debug` | false | `RUST_LOG=marila_embed=debug` | tracing |
| `--output` | `json` | — | `json` \| `table` (for `query` only; `put` uses progress + summary) |

### 6.2 `marila-embed put`

```
marila-embed put [common flags]
    [INPUT]                              # one or more of:
        --text-value <string>            # direct string, one vector
        --text <path-or-glob>...         # local files/globs
        --s3 <s3-uri>...                 # s3://bucket/prefix (lazy walked) — v0.5
    [--include <ext>...]                 # extension allow-list (default: all known parsers)
    [--exclude <ext>...]
    [--max-file-bytes <int>]             # default 50 MiB; oversized files logged + skipped

    [--chunk-strategy <name>]            # off | fixed | markdown | sentence  (default: fixed)
    [--chunk-size <tokens>]              # default 400
    [--chunk-overlap <tokens>]           # default 80

    [--key-strategy <name>]              # content-hash (default) | filename | path
    [--metadata <json>]                  # merged into every vector
    [--no-source-content]                # skip the S3VECTORS-EMBED-SRC-CONTENT field

    [--parse-concurrency <int>]          # default num_cpus / 2
    [--embed-concurrency <int>]          # default 8
    [--embed-batch <int>]                # default 100, clamped to provider.max_batch()
    [--put-concurrency <int>]            # default 4
    [--put-batch <int>]                  # default 100, AWS hard cap 500
    [--put-flush-ms <int>]               # default 250

    [--auto-create-index]                # CreateIndex if missing (default on; disable with --no-auto-create-index)
    [--dry-run]                          # parse + chunk + count tokens, no embed/put
    [--ignore-errors]                    # non-zero-exit suppression
    [--resume]                           # honour checkpoint (default on)
    [--no-resume]                        # ignore checkpoint, re-process everything
```

Standard metadata always added (per-vector; non-overrideable):

- `S3VECTORS-EMBED-SRC-LOCATION` — source path or `s3://...` URI
- `S3VECTORS-EMBED-SRC-CONTENT` — original chunk text (truncated to fit
  per-key metadata cap), unless `--no-source-content`
- `S3VECTORS-EMBED-CHUNK-IDX` — chunk index within source
- `S3VECTORS-EMBED-CONTENT-HASH` — blake3 of the original chunk text
- `marila.section_path` — array of headings (for markdown-chunked
  sources only)

The first three names match AWS's `s3vectors-embed-cli` conventions
verbatim so doc readers can swap CLIs without reshaping their
filters.

### 6.3 `marila-embed query`

```
marila-embed query [common flags]
    (--text-value <string> | --text <path>)
    [--k <int>]                          # topK, default 5
    [--filter <json>]                    # passed to QueryVectors verbatim
    [--return-metadata <bool>]           # default true
    [--return-distance <bool>]           # default true
```

Output (`--output json`): the raw `QueryVectors` response. `--output
table` renders `key | distance | source | snippet`.

### 6.4 Config file (TOML, optional)

```toml
# marila-embed.toml — repeatable invocation defaults
endpoint_url   = "http://localhost:8080"
vector_bucket  = "marila-docs"
index_name     = "rag"

[embed]
provider = "openai"
model    = "text-embedding-3-small"
concurrency = 16            # tune for your OpenAI tier
batch    = 200

[chunk]
strategy = "markdown"
size     = 512
overlap  = 100

[put]
concurrency = 6
batch    = 250
```

Precedence: CLI flag > env var > config file > built-in default.

## 7. Scalability targets (the user's headline concern)

| Corpus | Doc count | Total bytes | Target wall-time | Peak RSS |
|---|---|---|---|---|
| Small (the marila repo) | 6 | 200 KB | < 5 s | < 50 MB |
| Medium (~10k md/html docs) | 10 000 | 100 MB | < 5 min | < 150 MB |
| Large (1 M docs) | 1 000 000 | 10 GB | < 8 h (provider-rate-limited) | < 256 MB |
| XL (10 M docs) | 10 000 000 | 100 GB | provider rate-limit dominates | < 256 MB |

The last two are aspirational; we accept that real-world wall-time is
dominated by the embedding provider's rate limit at this scale. The
**peak RSS column is the spec**: it doesn't grow with corpus size,
full stop. Anything that breaks that invariant in v0 is a v0 bug.

A streaming smoke test in CI ingests a 1 GB synthetic corpus
(`/dev/urandom` → 100k 10 KB plaintext files) and asserts peak RSS
stays under 256 MB and throughput stays above 200 vec/s with the
`stub` provider.

## 8. Crate layout

```
crates/embed-cli/
├── Cargo.toml         binary `marila-embed`, depends on aws-sdk-s3vectors
├── README.md
└── src/
    ├── main.rs         clap parse + dispatch
    ├── config.rs       layered config (flags > env > toml > defaults)
    ├── pipeline.rs     spawn tasks + wire channels; owns the cancellation token
    ├── source/
    │   ├── mod.rs
    │   ├── local.rs    walkdir + globset
    │   └── s3.rs       (v0.5)
    ├── parse/
    │   ├── mod.rs      Parser trait + dispatch by extension
    │   ├── text.rs
    │   ├── markdown.rs pulldown-cmark
    │   ├── html.rs     scraper
    │   └── pdf.rs      (v0.5; lopdf for now, pdfium later for OCR)
    ├── chunk/
    │   ├── mod.rs      Chunker trait + dispatch
    │   ├── fixed.rs
    │   ├── markdown.rs heading-aware
    │   └── sentence.rs unicode-segmentation
    ├── tokenize.rs     tiktoken-rs for OpenAI, fallback elsewhere
    ├── embed/
    │   ├── mod.rs      EmbeddingProvider trait + factory
    │   ├── openai.rs
    │   ├── ollama.rs
    │   └── stub.rs
    ├── sink/
    │   ├── mod.rs      Sink trait (so tests can swap in an in-memory sink)
    │   └── s3vectors.rs aws-sdk-s3vectors backed
    ├── checkpoint.rs   JSONL load + append + fsync
    ├── progress.rs     TTY in-place renderer + JSON-lines writer
    ├── retry.rs        exponential-backoff helper used by embed + put
    ├── put.rs          `put` subcommand
    └── query.rs        `query` subcommand
```

External deps (new to the workspace):
- `clap` (already in workspace eligible)
- `tokio` (already used by marila)
- `reqwest` (already used)
- `pulldown-cmark`, `scraper`, `lopdf` (v0.5), `docx-rs` (v0.5)
- `tiktoken-rs` (for OpenAI token counting)
- `walkdir`, `globset`
- `unicode-segmentation`
- `blake3`
- `indicatif` (TTY progress)
- `figment` or hand-rolled (layered config)

## 9. Test plan

Per-stage unit tests (in-crate):
- `chunk::fixed` round-trips a fixed-width text into N+1 windows with the right overlap.
- `chunk::markdown` honours heading boundaries and stays under the
  size cap.
- `embed::stub` is deterministic and same-input ↔ same-output.
- `checkpoint` round-trips entries and skips them on a second pass.
- `retry::with_backoff` retries on 429, gives up on 4xx.

Integration tests (talk to a real marila):
- `put_text_value_round_trip` — single `--text-value`, ListVectors
  confirms the vector landed.
- `put_glob_with_chunking` — `--text 'doc/**/*.md' --chunk-strategy
  markdown`; assert chunk count > file count, every vector has a
  `section_path`.
- `resume_after_crash` — kill the process mid-run, restart with
  `--resume`, assert no duplicate keys and the final count matches the
  no-crash baseline.
- `query_returns_relevant_hits` — put a known set, query for a known
  target, assert top-1 distance < some threshold.

Scalability test (gated behind `--ignored` to keep CI fast):
- `large_corpus_bounded_rss` — generate 100k synthetic 10 KB files,
  run `put --embedding-provider stub`, assert peak RSS < 256 MB and
  throughput > 200 vec/s.

## 10. Implementation order

Each step ends with green tests for that step.

1. Crate skeleton + clap + common flags + `pipeline::run_noop` (all
   stages wired with the stub provider; produces no vectors).
2. `embed::stub` + `sink::in_memory` + the `put_text_value` happy path.
3. Real `sink::s3vectors` + `auto-create-index` + `put_text_value`
   integration test against a running marila.
4. Source/Parse/Chunk for plain text + `chunk::fixed`. `--text <glob>`
   works.
5. `chunk::markdown` + `parse::markdown`. Section-path metadata.
6. `embed::openai` (the demo-replacement milestone).
7. `query` subcommand, both output formats.
8. Checkpoint + resume.
9. Progress + cost telemetry.
10. `parse::html`, `parse::pdf` (v0.5 — separable, can land later).
11. `embed::ollama`.

## 11. Drop-in demo replacement

After step 6 the `demo_vectors.py` workflow becomes:

```bash
marila-embed put \
    --vector-bucket-name marila-docs --index-name rag \
    --embedding-provider openai --embedding-model text-embedding-3-small \
    --chunk-strategy markdown --chunk-size 400 \
    --text README.md --text CLAUDE.md --text 'doc/*.md'

marila-embed query \
    --vector-bucket-name marila-docs --index-name rag \
    --embedding-provider openai --embedding-model text-embedding-3-small \
    --text-value "How does marila validate vector dimensions?" \
    --k 3 --output table
```

Python script becomes a 30-line shell wrapper, or goes away entirely.

## 12. Open questions

- **Token counting for non-OpenAI providers.** Ollama doesn't return a
  token count; we estimate `chars / 4`. Cost telemetry will be
  approximate for non-OpenAI providers — call this out in `--summary`
  output, don't fake precision.
- **S3 source.** v0.5 walks an `s3://bucket/prefix`. Should it use
  marila's own storage or `aws-sdk-s3` directly? Probably the latter
  — the CLI shouldn't assume marila is the only S3 in the world.
- **Update vs. insert semantics on PutVectors.** AWS overwrites by key.
  Our deterministic content-hash keys mean identical-content re-runs
  are no-ops; renames produce a new key without removing the old.
  v0.5 could add `--prune` that diffs against the index and deletes
  keys that no longer appear in the source.
- **Streaming PDF text extraction.** `lopdf` is fast but loads the
  whole PDF document tree. For 500 MB PDFs we'd want a true streaming
  parser. v1 problem.

## 13. Definition of done (v0)

- `cargo install --path crates/embed-cli` produces a `marila-embed`
  binary that:
  - Ingests 217 chunks from the marila docs (today's demo corpus) in
    under 10 seconds via `embed-cli put --chunk-strategy markdown ...`
    with the OpenAI provider.
  - Round-trips the same four queries the current `demo_vectors.py`
    asks, producing equivalent citations.
  - Survives `kill -9` mid-run and finishes the rest on `--resume`.
- All §9 integration + unit tests green.
- The scalability smoke test (100k synthetic files, stub provider)
  asserts peak RSS < 256 MB on a developer laptop.
- `demo/demo_vectors.py` is replaced (or wraps) the new CLI.
- `CLAUDE.md` "What's done" notes the new binary; this spec stays at
  `doc/EMBED_CLI_SPEC.md`.
