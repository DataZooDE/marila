use anyhow::Result;
use clap::Parser;
use marila_embed::cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.init_tracing();
    match cli.command {
        Command::Put(_) => anyhow::bail!(
            "`marila-embed put` is wired in phases 1–6; \
             the Phase 0 skeleton only proves the CLI parses"
        ),
        Command::Query(_) => anyhow::bail!(
            "`marila-embed query` is wired in phase 7"
        ),
    }
}
