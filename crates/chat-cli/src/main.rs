use anyhow::{Context, Result};
use chat_core::MockProvider;
use clap::Parser;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::Store;
use tui::ProviderBox;
use whatsapp::{WhatsAppProvider, WhatsAppProviderOptions};

#[derive(Debug, Parser)]
#[command(author, version, about = "Unified terminal chat client")]
struct Args {
    /// Use synthetic local data for development and demos.
    #[arg(long, env = "CHAT_CLI_MOCK")]
    mock_provider: bool,

    /// Enable the WhatsApp bridge provider.
    #[arg(long, env = "CHAT_CLI_WHATSAPP")]
    whatsapp: bool,

    /// Override the WhatsApp bridge database path.
    #[arg(
        long,
        env = "CHAT_CLI_WHATSAPP_DB",
        default_value = "chat-cli-whatsapp.db"
    )]
    whatsapp_db: PathBuf,

    /// WhatsApp history sync scope: all, today, or none.
    #[arg(
        long,
        env = "CHAT_CLI_WHATSAPP_SYNC",
        default_value = "today",
        value_parser = ["all", "today", "none"]
    )]
    whatsapp_sync: String,

    /// Opt-in debug log file. Logging is off unless this is provided.
    #[arg(long, env = "CHAT_CLI_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Override the SQLite database path.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Remove the selected test databases before startup and after exit.
    #[arg(long)]
    test_cleanup: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cleanup_paths = cleanup_paths(&args);
    if args.test_cleanup {
        cleanup_databases(&cleanup_paths)?;
    }

    let store = match args.db.as_deref() {
        Some(path) => Store::open(path).await?,
        None => Store::open_default().await?,
    };
    let providers = build_providers(&args)?;

    let result = tui::run(Arc::new(store), providers).await;
    if args.test_cleanup {
        cleanup_databases(&cleanup_paths)?;
    }
    result
}

fn build_providers(args: &Args) -> Result<Vec<ProviderBox>> {
    let mut providers: Vec<ProviderBox> = Vec::new();
    if args.mock_provider {
        providers.push(Box::new(MockProvider::new()));
    }
    if args.whatsapp {
        providers.push(Box::new(WhatsAppProvider::with_options(
            WhatsAppProviderOptions {
                db_path: args.whatsapp_db.to_string_lossy().to_string(),
                sync_scope: args.whatsapp_sync.clone(),
                log_path: args.log_file.clone(),
            },
        )?));
    }
    Ok(providers)
}

fn cleanup_paths(args: &Args) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = &args.db {
        paths.push(path.clone());
    }
    if args.whatsapp {
        paths.push(args.whatsapp_db.clone());
        paths.push(whatsapp_avatar_cache_path(&args.whatsapp_db));
        paths.push(whatsapp_media_cache_path(&args.whatsapp_db));
    }
    paths
}

fn whatsapp_avatar_cache_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.avatars", db_path.to_string_lossy()))
}

fn whatsapp_media_cache_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.media", db_path.to_string_lossy()))
}

fn cleanup_databases(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        cleanup_database(path)?;
    }
    Ok(())
}

fn cleanup_database(path: &PathBuf) -> Result<()> {
    if path.as_os_str().is_empty() || path.to_string_lossy() == ":memory:" {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error)
                    .with_context(|| format!("removing test directory {}", path.display())),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => fs::remove_dir_all(path)
            .with_context(|| format!("removing test directory {}", path.display())),
        Err(error) => {
            Err(error).with_context(|| format!("removing test database {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_providers_can_enable_mock_and_whatsapp_together() -> Result<()> {
        let args = Args {
            mock_provider: true,
            whatsapp: true,
            whatsapp_db: PathBuf::from(":memory:"),
            whatsapp_sync: "today".to_owned(),
            log_file: None,
            db: None,
            test_cleanup: false,
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "mock:local");
        assert_eq!(providers[1].id().as_ref(), "whatsapp:bridge");

        Ok(())
    }

    #[test]
    fn build_providers_leaves_provider_list_empty_without_flags() -> Result<()> {
        let args = Args {
            mock_provider: false,
            whatsapp: false,
            whatsapp_db: PathBuf::from(":memory:"),
            whatsapp_sync: "today".to_owned(),
            log_file: None,
            db: None,
            test_cleanup: false,
        };

        assert!(build_providers(&args)?.is_empty());

        Ok(())
    }

    #[test]
    fn cleanup_paths_include_explicit_app_db_and_enabled_whatsapp_db() {
        let args = Args {
            mock_provider: false,
            whatsapp: true,
            whatsapp_db: PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
            whatsapp_sync: "today".to_owned(),
            log_file: None,
            db: Some(PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite")),
            test_cleanup: true,
        };

        assert_eq!(
            cleanup_paths(&args),
            vec![
                PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite"),
                PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
                PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db.avatars"),
                PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db.media")
            ]
        );
    }

    #[test]
    fn cleanup_paths_do_not_remove_default_or_disabled_whatsapp_db() {
        let args = Args {
            mock_provider: false,
            whatsapp: false,
            whatsapp_db: PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
            whatsapp_sync: "today".to_owned(),
            log_file: None,
            db: None,
            test_cleanup: true,
        };

        assert!(cleanup_paths(&args).is_empty());
    }

    #[test]
    fn cleanup_databases_removes_existing_files_directories_and_ignores_missing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app_db = dir.path().join("app.sqlite");
        let whatsapp_db = dir.path().join("whatsapp.db");
        let avatar_dir = dir.path().join("whatsapp.db.avatars");
        let media_dir = dir.path().join("whatsapp.db.media");
        fs::write(&app_db, b"app")?;
        fs::write(&whatsapp_db, b"whatsapp")?;
        fs::create_dir(&avatar_dir)?;
        fs::write(avatar_dir.join("avatar.jpg"), b"avatar")?;
        fs::create_dir(&media_dir)?;
        fs::write(media_dir.join("photo.jpg"), b"photo")?;

        cleanup_databases(&[
            app_db.clone(),
            whatsapp_db.clone(),
            avatar_dir.clone(),
            media_dir.clone(),
            dir.path().join("missing.db"),
        ])?;

        assert!(!app_db.exists());
        assert!(!whatsapp_db.exists());
        assert!(!avatar_dir.exists());
        assert!(!media_dir.exists());
        Ok(())
    }
}
