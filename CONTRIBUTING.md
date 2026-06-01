# Contributing

Thanks for improving marila. The project is a Rust workspace with a small
Python demo package under `demo/`.

## Development Setup

Prerequisites:

- Rust 1.95.0 with `rustfmt` and `clippy`
- Docker Compose, for end-to-end table tests and demos
- `protobuf-compiler`, `cmake`, and `pkg-config` on Linux build hosts

Recommended checks before opening a pull request:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Real AWS contract tests are opt-in:

```bash
MARILA_RUN_AWS_CONTRACTS=1 cargo test -p marila-integration-tests
```

For the sidecar stack:

```bash
docker compose up -d rustfs
cargo run -p marila
```

For tables-side work:

```bash
docker compose --profile lakekeeper up -d
cargo run -p marila
```

## Pull Requests

- Keep changes scoped to one behavior or maintenance concern.
- Include tests for behavior changes.
- Update `README.md`, `demo/README.md`, or `doc/` when commands,
  configuration, or supported API coverage changes.
- Do not commit generated data, local `.duckdb` files, checkpoints,
  Docker volumes, or secrets.

## API Compatibility

marila intentionally implements a subset of AWS S3 Tables and S3 Vectors.
When behavior diverges from AWS, document the divergence clearly instead
of making a silent approximation.
