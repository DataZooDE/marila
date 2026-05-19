use anyhow::Result;
use clap::Parser;
use marila_embed::cli::{Cli, Command};
use marila_embed::{put, query};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    cli.init_tracing();
    match cli.command {
        Command::Put(args) => {
            let outcome = put::run(args).await?;
            if let Some(s) = outcome.stats {
                tracing::info!(
                    raw_docs = s.raw_docs,
                    chunks = s.chunks,
                    put = s.put,
                    parse_failures = s.parse_failures,
                    embed_failures = s.embed_failures,
                    dry_run = outcome.dry_run,
                    "put finished"
                );
            } else {
                tracing::info!(dry_run = outcome.dry_run, "put finished");
            }
            Ok(())
        }
        Command::Query(args) => {
            query::run(args).await
        }
    }
}
