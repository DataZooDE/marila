# VHS recordings

This directory contains the committed README GIFs and matching VHS tape
scripts.

The GIFs are captured from the real Textual demo layouts with
representative S3 Vectors / S3 Tables events, so GitHub shows the same
panes users see when they run the demos: chat, hops, sources, and pivot
controls.

Install [VHS](https://github.com/charmbracelet/vhs), then render the
command-line walkthrough tapes from the repository root:

```bash
vhs demo/vhs/vector.tape
vhs demo/vhs/tables.tape
vhs demo/vhs/embed-cli.tape
```

The recordings intentionally show the same boto3/AWS-protocol paths used
by the demos instead of mocking calls inside Rust.
