//! Phase 0 acceptance: the binary exists, parses, and `--help` is sane.

use std::path::PathBuf;
use std::process::Command;

fn binary() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("..");
    p.push("..");
    p.push("target");
    p.push("debug");
    p.push("marila-embed");
    p
}

fn ensure_built() {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "marila-embed"])
        .status()
        .expect("cargo build -p marila-embed");
    assert!(status.success(), "cargo build -p marila-embed failed");
}

#[test]
fn help_exits_zero_and_mentions_subcommands() {
    ensure_built();
    let out = Command::new(binary())
        .arg("--help")
        .output()
        .expect("run marila-embed --help");
    assert!(out.status.success(), "--help non-zero: {out:?}");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("put"), "help missing `put` subcommand: {s}");
    assert!(s.contains("query"), "help missing `query` subcommand: {s}");
}

#[test]
fn put_help_lists_key_flags() {
    ensure_built();
    let out = Command::new(binary())
        .args(["put", "--help"])
        .output()
        .expect("run marila-embed put --help");
    assert!(out.status.success(), "put --help non-zero: {out:?}");
    let s = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--vector-bucket-name",
        "--index-name",
        "--embedding-provider",
        "--text-value",
        "--text",
        "--chunk-strategy",
        "--embed-batch",
        "--put-batch",
        "--max-file-bytes",
        "--checkpoint",
        "--resume",
    ] {
        assert!(s.contains(flag), "put help missing {flag}: {s}");
    }
}

#[test]
fn query_help_lists_key_flags() {
    ensure_built();
    let out = Command::new(binary())
        .args(["query", "--help"])
        .output()
        .expect("run marila-embed query --help");
    assert!(out.status.success(), "query --help non-zero: {out:?}");
    let s = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--vector-bucket-name",
        "--index-name",
        "--text-value",
        "--k",
        "--filter",
        "--output",
    ] {
        assert!(s.contains(flag), "query help missing {flag}: {s}");
    }
}
