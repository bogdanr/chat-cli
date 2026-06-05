use anyhow::Result;
use chat_core::MockProvider;
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use storage::Store;
use tui::ProviderBox;

#[derive(Debug, Parser)]
#[command(author, version, about = "Unified terminal chat client")]
struct Args {
    /// Use synthetic local data for development and demos.
    #[arg(long, env = "CHAT_CLI_MOCK")]
    mock_provider: bool,

    /// Override the SQLite database path.
    #[arg(long)]
    db: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let store = match args.db.as_deref() {
        Some(path) => Store::open(path).await?,
        None => Store::open_default().await?,
    };
    let providers = build_providers(args.mock_provider);

    tui::run(Arc::new(store), providers).await
}

fn build_providers(mock_provider: bool) -> Vec<ProviderBox> {
    if mock_provider {
        vec![Box::new(MockProvider::new())]
    } else {
        Vec::new()
    }
}
