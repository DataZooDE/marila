use anyhow::Result;
use clap::Parser;
use marila_embed::cli::{Cli, Command};
use marila_embed::put;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.init_tracing();
    match cli.command {
        Command::Put(args) => {
            let outcome = put::run(args).await?;
            tracing::info!(chunks = outcome.chunks, "put finished");
            Ok(())
        }
        Command::Query(_) => {
            anyhow::bail!("`marila-embed query` lands in phase 7")
        }
    }
}
