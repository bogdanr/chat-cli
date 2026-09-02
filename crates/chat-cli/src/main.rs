use anyhow::{Context, Result, anyhow, bail};
use chat_core::{MockProvider, Provider};
use chrono::{Duration as ChronoDuration, Utc};
use clap::{Parser, Subcommand};
use clickup::{ClickUpProvider, ClickUpProviderOptions};
use serde::{Deserialize, Serialize};
use slack::{SlackAuthMode, SlackProvider, SlackProviderOptions};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use storage::Store;
use tui::{AccountProviderFactory, AccountProviderKind, ProviderBox, run_with_factory};
use whatsapp::{WhatsAppProvider, WhatsAppProviderOptions};

#[derive(Parser)]
#[command(author, version, about = "Unified terminal chat client")]
struct Args {
    /// Use synthetic local data for development and demos.
    #[arg(long, env = "CHAT_CLI_MOCK")]
    mock_provider: bool,

    /// Enable the Slack provider setup/integration.
    #[arg(long, env = "CHAT_CLI_SLACK")]
    slack: bool,

    /// Slack auth mode, ordered by robustness: user-oauth, read-only-oauth, bot-token, imported-token, manual-app, webhook.
    #[arg(long, env = "CHAT_CLI_SLACK_AUTH_MODE", default_value = "user-oauth")]
    slack_auth_mode: SlackAuthMode,

    /// Slack OAuth client ID for user or manual app setup.
    #[arg(long, env = "CHAT_CLI_SLACK_CLIENT_ID")]
    slack_client_id: Option<String>,

    /// Slack OAuth client secret for token exchange. Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_SLACK_CLIENT_SECRET")]
    slack_client_secret: Option<String>,

    /// Slack OAuth redirect URI.
    #[arg(long, env = "CHAT_CLI_SLACK_REDIRECT_URI")]
    slack_redirect_uri: Option<String>,

    /// Existing Slack user token. Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_SLACK_USER_TOKEN")]
    slack_user_token: Option<String>,

    /// Existing Slack bot token. Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_SLACK_BOT_TOKEN")]
    slack_bot_token: Option<String>,

    /// Slack app-level token for Socket Mode. Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_SLACK_APP_TOKEN")]
    slack_app_token: Option<String>,

    /// Slack incoming webhook URL for send-only fallback mode. Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_SLACK_WEBHOOK_URL")]
    slack_webhook_url: Option<String>,

    /// Optional Slack workspace label shown in the account list.
    #[arg(long, env = "CHAT_CLI_SLACK_WORKSPACE")]
    slack_workspace: Option<String>,

    /// TOML file containing multiple Slack workspace profiles.
    #[arg(long, env = "CHAT_CLI_SLACK_WORKSPACES_FILE")]
    slack_workspaces_file: Option<PathBuf>,

    /// Inline Slack workspace profile. Repeat for multiple workspaces. Format: label=team,auth=user-oauth,user_token=xoxp-...
    #[arg(
        long = "slack-workspace-profile",
        env = "CHAT_CLI_SLACK_WORKSPACE_PROFILES"
    )]
    slack_workspace_profiles: Vec<String>,

    /// Enable the WhatsApp bridge provider.
    #[arg(long, env = "CHAT_CLI_WHATSAPP")]
    whatsapp: bool,

    /// Enable the ClickUp Chat provider.
    #[arg(long, env = "CHAT_CLI_CLICKUP")]
    clickup: bool,

    /// ClickUp personal API token (starts with `pk_`). Redacted from debug output.
    #[arg(long, env = "CHAT_CLI_CLICKUP_TOKEN")]
    clickup_token: Option<String>,

    /// ClickUp workspace ("team") id. Required only when the token reaches several workspaces.
    #[arg(long, env = "CHAT_CLI_CLICKUP_WORKSPACE_ID")]
    clickup_workspace_id: Option<String>,

    /// Optional ClickUp workspace label shown in the account list.
    #[arg(long, env = "CHAT_CLI_CLICKUP_WORKSPACE")]
    clickup_workspace: Option<String>,

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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Control notification pause state without launching the TUI.
    Notifications {
        #[command(subcommand)]
        action: NotificationCommand,
    },
}

#[derive(Subcommand)]
enum NotificationCommand {
    /// Pause notifications for a duration such as 25m, 1h, or 90s.
    Pause { duration: String },
    /// Resume notifications immediately.
    Resume,
    /// Show current notification pause status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    configure_diagnostic_log(&args);
    let cleanup_paths = cleanup_paths(&args);
    if args.test_cleanup {
        cleanup_databases(&cleanup_paths)?;
    }

    let store = match args.db.as_deref() {
        Some(path) => Store::open(path).await?,
        None => Store::open_default().await?,
    };

    if let Some(command) = &args.command {
        handle_command(command, &store).await?;
        if args.test_cleanup {
            cleanup_databases(&cleanup_paths)?;
        }
        return Ok(());
    }

    let mut persisted_accounts = store.get_account_configs().await?;
    // An account whose provider id changed after setup resolved its real
    // identity leaves its chats and messages behind on the old id. Fold them
    // forward before any provider starts, so the sidebar never shows stale
    // rows that no running provider owns.
    if migrate_superseded_accounts(&store, &persisted_accounts).await? {
        persisted_accounts = store.get_account_configs().await?;
    }
    let providers = build_providers_with_persisted(&args, persisted_accounts)?;
    let existing_provider_ids = providers
        .iter()
        .map(|provider| provider.id().to_string())
        .collect();
    let provider_factory = build_account_provider_factory(&args, existing_provider_ids);

    let result = run_with_factory(Arc::new(store), providers, Some(provider_factory)).await;
    if args.test_cleanup {
        cleanup_databases(&cleanup_paths)?;
    }
    result
}

/// Environment variable consumed by the shared diagnostic/perf log used across
/// the TUI and the Slack provider.
const PERF_LOG_FILE_ENV: &str = "CHAT_CLI_PERF_LOG_FILE";

/// Route diagnostics for every crate (TUI perf markers and Slack realtime
/// tracing) into the same `--log-file` the user already tails. Without this,
/// only the WhatsApp bridge writes there and the Slack realtime path is
/// completely silent, making "no realtime messages" impossible to diagnose.
fn configure_diagnostic_log(args: &Args) {
    let Some(log_file) = args.log_file.as_ref() else {
        return;
    };
    // Start each run with a clean log so the file only ever contains the
    // current session. The shared writers (TUI perf markers, Slack realtime
    // diagnostics, WhatsApp bridge) all open the path in append mode, so
    // truncating once here — before any of them spawn — keeps the session's
    // entries intact while discarding stale output from previous runs. A
    // failure to truncate must not abort startup; logging is best-effort.
    if let Err(error) = fs::write(log_file, b"") {
        eprintln!(
            "warning: could not truncate diagnostic log {}: {error}",
            log_file.display()
        );
    }
    if std::env::var_os(PERF_LOG_FILE_ENV).is_some() {
        return;
    }
    // Safe: executed at the very top of `main`, before any provider tasks,
    // background threads, or socket-mode loops are spawned, so no other thread
    // is concurrently reading or writing the process environment.
    unsafe {
        std::env::set_var(PERF_LOG_FILE_ENV, log_file);
    }
}

async fn handle_command(command: &Command, store: &Store) -> Result<()> {
    match command {
        Command::Notifications { action } => handle_notification_command(action, store).await,
    }
}

async fn handle_notification_command(action: &NotificationCommand, store: &Store) -> Result<()> {
    match action {
        NotificationCommand::Pause { duration } => {
            let duration = parse_pause_duration(duration)?;
            let paused_until = Utc::now() + duration;
            store.pause_notifications_until(paused_until).await?;
            println!("notifications paused until {}", paused_until.to_rfc3339());
        }
        NotificationCommand::Resume => {
            store.resume_notifications().await?;
            println!("notifications resumed");
        }
        NotificationCommand::Status => {
            let state = store.notification_pause_state().await?;
            if let Some(paused_until) = state
                .paused_until
                .filter(|paused_until| *paused_until > Utc::now())
            {
                println!("notifications paused until {}", paused_until.to_rfc3339());
            } else {
                println!("notifications not paused");
            }
        }
    }
    Ok(())
}

fn parse_pause_duration(value: &str) -> Result<ChronoDuration> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("pause duration cannot be empty");
    }
    let (number, unit) = trimmed.split_at(
        trimmed
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(trimmed.len()),
    );
    let amount: i64 = number
        .parse()
        .with_context(|| format!("invalid pause duration {value:?}"))?;
    if amount <= 0 {
        bail!("pause duration must be positive");
    }
    match unit.trim().to_ascii_lowercase().as_str() {
        "" | "m" | "min" | "mins" | "minute" | "minutes" => Ok(ChronoDuration::minutes(amount)),
        "s" | "sec" | "secs" | "second" | "seconds" => Ok(ChronoDuration::seconds(amount)),
        "h" | "hr" | "hrs" | "hour" | "hours" => Ok(ChronoDuration::hours(amount)),
        _ => bail!("unsupported pause duration unit in {value:?}; use s, m, or h"),
    }
}

fn build_account_provider_factory(
    args: &Args,
    existing_provider_ids: Vec<String>,
) -> AccountProviderFactory {
    let whatsapp_db = args.whatsapp_db.clone();
    let whatsapp_sync = args.whatsapp_sync.clone();
    let log_path = args.log_file.clone();
    let clickup_token = args.clickup_token.clone();
    let clickup_workspace_id = args.clickup_workspace_id.clone();
    let slack_counter = Arc::new(AtomicU64::new(1));
    let whatsapp_counter = Arc::new(AtomicU64::new(1));
    let clickup_counter = Arc::new(AtomicU64::new(1));
    let used_slack_provider_ids = Arc::new(Mutex::new(
        existing_provider_ids
            .into_iter()
            .filter(|id| id.starts_with("slack:"))
            .collect::<HashSet<_>>(),
    ));

    Arc::new(move |kind| match kind {
        AccountProviderKind::Slack => loop {
            let ordinal = slack_counter.fetch_add(1, Ordering::Relaxed);
            let provider = SlackProvider::with_options(SlackProviderOptions {
                workspace: Some(format!("Workspace {ordinal}")),
                ..SlackProviderOptions::new(SlackAuthMode::UserOAuth)
            })?;
            let provider_id = provider.id().to_string();
            let mut used_ids = used_slack_provider_ids
                .lock()
                .map_err(|_| anyhow!("Slack provider id registry is unavailable"))?;
            if used_ids.insert(provider_id) {
                break Ok(Arc::new(provider) as ProviderBox);
            }
        },
        AccountProviderKind::WhatsApp => {
            let ordinal = whatsapp_counter.fetch_add(1, Ordering::Relaxed);
            let db_path = if ordinal == 1 {
                whatsapp_db.clone()
            } else {
                let mut path = whatsapp_db.clone();
                let extension = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("db");
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("chat-cli-whatsapp");
                path.set_file_name(format!("{stem}-{ordinal}.{extension}"));
                path
            };
            Ok(
                Arc::new(WhatsAppProvider::with_options(WhatsAppProviderOptions {
                    db_path: db_path.to_string_lossy().to_string(),
                    sync_scope: whatsapp_sync.clone(),
                    log_path: log_path.clone(),
                })?) as ProviderBox,
            )
        }
        AccountProviderKind::ClickUp => {
            // Distinct labels keep runtime-added ClickUp accounts from
            // colliding on the workspace-derived provider id before setup has
            // resolved the real workspace name.
            let ordinal = clickup_counter.fetch_add(1, Ordering::Relaxed);
            Ok(
                Arc::new(ClickUpProvider::with_options(ClickUpProviderOptions {
                    workspace: Some(format!("ClickUp {ordinal}")),
                    workspace_id: clickup_workspace_id.clone(),
                    personal_token: clickup_token.clone(),
                    ..ClickUpProviderOptions::default()
                })?) as ProviderBox,
            )
        }
        AccountProviderKind::Demo => Ok(Arc::new(MockProvider::new()) as ProviderBox),
    })
}

#[cfg(test)]
fn build_providers(args: &Args) -> Result<Vec<ProviderBox>> {
    build_providers_with_persisted(args, Vec::new())
}

fn build_providers_with_persisted(
    args: &Args,
    persisted_accounts: Vec<storage::StoredAccountConfig>,
) -> Result<Vec<ProviderBox>> {
    let mut providers: Vec<ProviderBox> = Vec::new();
    let enable_default_providers = !provider_flags_specified(args);
    if args.mock_provider {
        providers.push(Arc::new(MockProvider::new()));
    }

    for options in slack_provider_options(args, &persisted_accounts, enable_default_providers)? {
        let provider = SlackProvider::with_options(options)?;
        let provider_id = provider.id().clone();
        if providers
            .iter()
            .any(|existing| existing.id().as_ref() == provider_id.as_ref())
        {
            bail!("duplicate provider id configured: {}", provider_id);
        }
        providers.push(Arc::new(provider));
    }

    for options in clickup_provider_options(args, &persisted_accounts)? {
        let provider = ClickUpProvider::with_options(options)?;
        let provider_id = provider.id().clone();
        if providers
            .iter()
            .any(|existing| existing.id().as_ref() == provider_id.as_ref())
        {
            bail!("duplicate provider id configured: {}", provider_id);
        }
        providers.push(Arc::new(provider));
    }

    if args.whatsapp || enable_default_providers {
        providers.push(Arc::new(WhatsAppProvider::with_options(
            WhatsAppProviderOptions {
                db_path: args.whatsapp_db.to_string_lossy().to_string(),
                sync_scope: args.whatsapp_sync.clone(),
                log_path: args.log_file.clone(),
            },
        )?));
    }
    Ok(providers)
}

/// Collects the ClickUp accounts to start with: the one described by CLI flags
/// (if any) plus every stored account, deduplicated by provider id.
///
/// Unlike Slack, ClickUp has no "default on" mode: it is only started when the
/// user explicitly configured it or previously added it in the app. Restoring
/// stored accounts here is what keeps a ClickUp account alive across restarts.
fn clickup_provider_options(
    args: &Args,
    persisted_accounts: &[storage::StoredAccountConfig],
) -> Result<Vec<ClickUpProviderOptions>> {
    let mut options = Vec::new();
    let mut seen = HashSet::<String>::new();

    if args.clickup || args.clickup_token.is_some() {
        options.push(ClickUpProviderOptions {
            workspace: args.clickup_workspace.clone(),
            workspace_id: args.clickup_workspace_id.clone(),
            personal_token: args.clickup_token.clone(),
            ..ClickUpProviderOptions::default()
        });
    }

    for configured in &options {
        seen.insert(clickup_provider_id_for_options(configured)?);
    }

    for account in persisted_accounts {
        if account.platform != chat_core::Platform::ClickUp {
            continue;
        }
        let stored: ClickUpProviderOptions = serde_json::from_str(&account.config_json)
            .with_context(|| format!("parsing stored ClickUp account {}", account.id))?;
        if seen.insert(clickup_provider_id_for_options(&stored)?) {
            options.push(stored);
        }
    }

    Ok(options)
}

fn clickup_provider_id_for_options(options: &ClickUpProviderOptions) -> Result<String> {
    Ok(ClickUpProvider::with_options(options.clone())?
        .id()
        .to_string())
}

/// Canonical provider id for a stored account, recomputed from its config.
///
/// Returns `None` for platforms whose id does not depend on resolvable
/// options, so they are never considered superseded.
fn canonical_provider_id(account: &storage::StoredAccountConfig) -> Result<Option<String>> {
    match account.platform {
        chat_core::Platform::ClickUp => {
            let options: ClickUpProviderOptions = serde_json::from_str(&account.config_json)
                .with_context(|| format!("parsing stored ClickUp account {}", account.id))?;
            clickup_provider_id_for_options(&options).map(Some)
        }
        chat_core::Platform::Slack => {
            let options: SlackProviderOptions = serde_json::from_str(&account.config_json)
                .with_context(|| format!("parsing stored Slack account {}", account.id))?;
            slack_provider_id_for_options(&options).map(Some)
        }
        _ => Ok(None),
    }
}

/// Folds stored rows from superseded provider ids into the canonical id.
///
/// A provider id is derived from its options, so it changes when setup
/// resolves the authoritative identity: a ClickUp account first started
/// without a workspace id keys on its label, then re-keys on the workspace id
/// once `validate` persists it. The old id keeps every chat and message while
/// the new one starts empty, leaving stale rows in the sidebar that no running
/// provider can ever rename or give an avatar to.
///
/// Reports whether anything moved, so the caller knows to reload configs.
async fn migrate_superseded_accounts(
    store: &Store,
    persisted_accounts: &[storage::StoredAccountConfig],
) -> Result<bool> {
    // Only ids that still have an account row can be merge targets; the fold
    // is skipped otherwise and retried on a later start.
    let known: HashSet<&str> = persisted_accounts
        .iter()
        .map(|account| account.id.as_ref())
        .collect();

    let mut merged = false;
    for account in persisted_accounts {
        let Some(canonical) = canonical_provider_id(account)? else {
            continue;
        };
        if canonical == account.id.as_ref() || !known.contains(canonical.as_str()) {
            continue;
        }
        store
            .merge_account(&account.id, &Arc::from(canonical.as_str()))
            .await
            .with_context(|| {
                format!("merging superseded account {} into {canonical}", account.id)
            })?;
        merged = true;
    }
    Ok(merged)
}

fn provider_flags_specified(args: &Args) -> bool {
    args.mock_provider
        || args.slack
        || args.whatsapp
        || args.clickup
        || args.clickup_token.is_some()
        || args.slack_workspaces_file.is_some()
        || !args.slack_workspace_profiles.is_empty()
}

#[derive(Debug, Deserialize, Serialize)]
struct SlackWorkspacesFile {
    workspaces: Vec<SlackWorkspaceProfile>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct SlackWorkspaceProfile {
    workspace: Option<String>,
    label: Option<String>,
    auth_mode: Option<String>,
    auth: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
    user_token: Option<String>,
    bot_token: Option<String>,
    app_token: Option<String>,
    webhook_url: Option<String>,
}

fn slack_provider_options(
    args: &Args,
    persisted_accounts: &[storage::StoredAccountConfig],
    enable_default_providers: bool,
) -> Result<Vec<SlackProviderOptions>> {
    let persisted_options = dedupe_persisted_slack_options(persisted_accounts)?;
    let mut profiles = Vec::new();

    if let Some(path) = &args.slack_workspaces_file {
        profiles.extend(read_slack_workspace_profiles(path)?);
    }

    for profile in &args.slack_workspace_profiles {
        profiles.push(parse_slack_workspace_profile(profile)?);
    }

    let mut configured_options = Vec::new();
    if args.slack || (enable_default_providers && persisted_options.is_empty()) {
        configured_options.push(single_slack_options(args));
    }
    configured_options.extend(
        profiles
            .into_iter()
            .map(SlackWorkspaceProfile::into_options)
            .collect::<Result<Vec<_>>>()?,
    );

    ensure_unique_slack_provider_ids(&configured_options)?;
    let mut seen_ids = slack_provider_id_set(&configured_options)?;
    let mut options = configured_options;
    let has_explicit_slack_config = args.slack || !options.is_empty() || enable_default_providers;
    for persisted in persisted_options {
        let id = slack_provider_id_for_options(&persisted)?;
        if seen_ids.insert(id.clone()) {
            options.push(persisted);
        } else if !has_explicit_slack_config {
            eprintln!("ignoring duplicate stored Slack workspace provider id '{id}'");
        }
    }
    Ok(options)
}

fn single_slack_options(args: &Args) -> SlackProviderOptions {
    SlackProviderOptions {
        auth_mode: args.slack_auth_mode.clone(),
        client_id: args.slack_client_id.clone(),
        client_secret: args.slack_client_secret.clone(),
        redirect_uri: args.slack_redirect_uri.clone(),
        bot_token: args.slack_bot_token.clone(),
        app_token: args.slack_app_token.clone(),
        user_token: args.slack_user_token.clone(),
        webhook_url: args.slack_webhook_url.clone(),
        workspace: args.slack_workspace.clone(),
    }
}

fn read_slack_workspace_profiles(path: &Path) -> Result<Vec<SlackWorkspaceProfile>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading Slack workspaces file {}", path.display()))?;
    let parsed: SlackWorkspacesFile = toml::from_str(&contents)
        .with_context(|| format!("parsing Slack workspaces file {}", path.display()))?;
    Ok(parsed.workspaces)
}

fn parse_slack_workspace_profile(value: &str) -> Result<SlackWorkspaceProfile> {
    let mut profile = SlackWorkspaceProfile::default();
    for segment in value.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (key, value) = segment
            .split_once('=')
            .ok_or_else(|| anyhow!("Slack workspace profile entries must use key=value pairs"))?;
        let key = key.trim().replace('-', "_");
        let value = value.trim().to_owned();
        match key.as_str() {
            "workspace" | "label" => profile.workspace = Some(value),
            "auth" | "auth_mode" => profile.auth_mode = Some(value),
            "client_id" => profile.client_id = Some(value),
            "client_secret" => profile.client_secret = Some(value),
            "redirect_uri" => profile.redirect_uri = Some(value),
            "user_token" => profile.user_token = Some(value),
            "bot_token" => profile.bot_token = Some(value),
            "app_token" => profile.app_token = Some(value),
            "webhook_url" => profile.webhook_url = Some(value),
            _ => bail!("unsupported Slack workspace profile key '{key}'"),
        }
    }
    Ok(profile)
}

fn ensure_unique_slack_provider_ids(options: &[SlackProviderOptions]) -> Result<()> {
    let mut seen = HashSet::<String>::new();
    for options in options {
        let id = slack_provider_id_for_options(options)?;
        if !seen.insert(id.clone()) {
            bail!("duplicate Slack workspace provider id '{id}'. Set unique workspace labels.");
        }
    }
    Ok(())
}

fn slack_provider_id_set(options: &[SlackProviderOptions]) -> Result<HashSet<String>> {
    options.iter().map(slack_provider_id_for_options).collect()
}

fn slack_provider_id_for_options(options: &SlackProviderOptions) -> Result<String> {
    Ok(SlackProvider::with_options(options.clone())?
        .id()
        .to_string())
}

fn dedupe_persisted_slack_options(
    persisted_accounts: &[storage::StoredAccountConfig],
) -> Result<Vec<SlackProviderOptions>> {
    let mut entries = Vec::<(String, bool, SlackProviderOptions)>::new();
    for account in persisted_accounts {
        if account.platform != chat_core::Platform::Slack {
            continue;
        }
        let options: SlackProviderOptions = serde_json::from_str(&account.config_json)
            .with_context(|| format!("parsing stored Slack account {}", account.id))?;
        let id = slack_provider_id_for_options(&options)?;
        let stored_id_matches = account.id.as_ref() == id;
        if let Some(existing) = entries
            .iter_mut()
            .find(|(existing_id, _, _)| existing_id == &id)
        {
            if !existing.1 && stored_id_matches {
                *existing = (id, stored_id_matches, options);
            }
        } else {
            entries.push((id, stored_id_matches, options));
        }
    }
    Ok(entries.into_iter().map(|(_, _, options)| options).collect())
}

impl SlackWorkspaceProfile {
    fn into_options(self) -> Result<SlackProviderOptions> {
        let auth_mode = match self.auth_mode.or(self.auth) {
            Some(auth_mode) => auth_mode.parse::<SlackAuthMode>()?,
            None => SlackAuthMode::UserOAuth,
        };

        Ok(SlackProviderOptions {
            auth_mode,
            client_id: self.client_id,
            client_secret: self.client_secret,
            redirect_uri: self.redirect_uri,
            bot_token: self.bot_token,
            app_token: self.app_token,
            user_token: self.user_token,
            webhook_url: self.webhook_url,
            workspace: self.workspace.or(self.label),
        })
    }
}

fn cleanup_paths(args: &Args) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = &args.db {
        paths.push(path.clone());
    }
    if args.whatsapp || !provider_flags_specified(args) {
        paths.push(args.whatsapp_db.clone());
    }
    paths
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

    fn base_args() -> Args {
        Args {
            mock_provider: false,
            slack: false,
            slack_auth_mode: SlackAuthMode::UserOAuth,
            slack_client_id: None,
            slack_client_secret: None,
            slack_redirect_uri: None,
            slack_user_token: None,
            slack_bot_token: None,
            slack_app_token: None,
            slack_webhook_url: None,
            slack_workspace: None,
            slack_workspaces_file: None,
            slack_workspace_profiles: Vec::new(),
            whatsapp: false,
            clickup: false,
            clickup_token: None,
            clickup_workspace_id: None,
            clickup_workspace: None,
            whatsapp_db: PathBuf::from(":memory:"),
            whatsapp_sync: "today".to_owned(),
            log_file: None,
            db: None,
            test_cleanup: false,
            command: None,
        }
    }

    fn stored_slack_account(
        id: &str,
        workspace: &str,
        token: &str,
    ) -> Result<storage::StoredAccountConfig> {
        let options = SlackProviderOptions {
            workspace: Some(workspace.to_owned()),
            user_token: Some(token.to_owned()),
            ..SlackProviderOptions::new(SlackAuthMode::UserOAuth)
        };
        Ok(storage::StoredAccountConfig {
            id: Arc::from(id),
            platform: chat_core::Platform::Slack,
            display_name: Arc::from(format!("Slack ({workspace})")),
            config_json: serde_json::to_string(&options)?,
        })
    }

    fn stored_clickup_account(
        id: &str,
        workspace: &str,
        workspace_id: &str,
    ) -> Result<storage::StoredAccountConfig> {
        let options = ClickUpProviderOptions {
            workspace: Some(workspace.to_owned()),
            workspace_id: Some(workspace_id.to_owned()),
            personal_token: Some("pk_1_stored".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        Ok(storage::StoredAccountConfig {
            id: Arc::from(id),
            platform: chat_core::Platform::ClickUp,
            display_name: Arc::from(format!("ClickUp ({workspace})")),
            config_json: serde_json::to_string(&options)?,
        })
    }

    #[test]
    fn clickup_stays_off_unless_asked_for() -> Result<()> {
        // ClickUp has no realtime transport, so polling it by default would
        // burn a stranger's rate budget. It only starts on request.
        assert!(clickup_provider_options(&base_args(), &[])?.is_empty());
        assert!(!provider_flags_specified(&base_args()));

        let flagged = Args {
            clickup: true,
            ..base_args()
        };
        assert_eq!(clickup_provider_options(&flagged, &[])?.len(), 1);
        assert!(provider_flags_specified(&flagged));
        Ok(())
    }

    #[test]
    fn clickup_accounts_are_restored_from_storage_and_deduplicated() -> Result<()> {
        // A ClickUp account added inside the app lives only in storage, so the
        // restore path is the only thing keeping it alive across restarts.
        let stored = vec![
            stored_clickup_account("clickup:acme", "Acme", "9001")?,
            stored_clickup_account("clickup:acme-again", "Acme", "9001")?,
            stored_clickup_account("clickup:globex", "Globex", "9002")?,
            stored_slack_account("slack:acme", "acme", "xoxp-1")?,
        ];

        let restored = clickup_provider_options(&base_args(), &stored)?;
        let ids = restored
            .iter()
            .map(clickup_provider_id_for_options)
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(ids.len(), 2, "duplicate workspace ids collapse: {ids:?}");
        assert_eq!(
            ids.iter().collect::<HashSet<_>>().len(),
            2,
            "restored ids must be unique: {ids:?}"
        );
        Ok(())
    }

    #[test]
    fn a_configured_clickup_workspace_is_not_restored_twice() -> Result<()> {
        // Flags and storage can describe the same workspace; starting two
        // providers for it would double the polling cost and duplicate events.
        let args = Args {
            clickup_token: Some("pk_1_flagged".to_owned()),
            clickup_workspace_id: Some("9001".to_owned()),
            ..base_args()
        };
        let stored = vec![stored_clickup_account("clickup:acme", "Acme", "9001")?];

        assert_eq!(clickup_provider_options(&args, &stored)?.len(), 1);
        Ok(())
    }

    #[test]
    fn build_providers_starts_one_clickup_provider_per_workspace() -> Result<()> {
        // Provider ids key storage rows and event routing, so the same
        // workspace reached from both flags and storage must start once.
        let args = Args {
            clickup_token: Some("pk_1_a".to_owned()),
            clickup_workspace_id: Some("9001".to_owned()),
            ..base_args()
        };
        let providers = build_providers_with_persisted(
            &args,
            vec![stored_clickup_account("clickup:acme", "Acme", "9001")?],
        )?;
        assert_eq!(
            providers
                .iter()
                .filter(|provider| provider.platform() == chat_core::Platform::ClickUp)
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn parse_pause_duration_accepts_seconds_minutes_and_hours() -> Result<()> {
        assert_eq!(parse_pause_duration("90s")?, ChronoDuration::seconds(90));
        assert_eq!(parse_pause_duration("25m")?, ChronoDuration::minutes(25));
        assert_eq!(parse_pause_duration("1h")?, ChronoDuration::hours(1));
        assert_eq!(parse_pause_duration("15")?, ChronoDuration::minutes(15));
        Ok(())
    }

    #[test]
    fn parse_pause_duration_rejects_empty_non_positive_and_unknown_units() {
        assert!(parse_pause_duration("").is_err());
        assert!(parse_pause_duration("0m").is_err());
        assert!(parse_pause_duration("25d").is_err());
    }

    #[test]
    fn build_providers_can_enable_mock_and_whatsapp_together() -> Result<()> {
        let args = Args {
            mock_provider: true,
            whatsapp: true,
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "mock:local");
        assert_eq!(providers[1].id().as_ref(), "whatsapp:bridge");

        Ok(())
    }

    #[test]
    fn build_providers_can_enable_slack_alone() -> Result<()> {
        let args = Args {
            slack: true,
            slack_auth_mode: SlackAuthMode::ReadOnlyOAuth,
            slack_user_token: Some("xoxp-user".to_owned()),
            slack_workspace: Some("example".to_owned()),
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id().as_ref(), "slack:example");
        assert_eq!(providers[0].platform(), chat_core::Platform::Slack);

        Ok(())
    }

    #[test]
    fn build_providers_can_enable_all_provider_flags() -> Result<()> {
        let args = Args {
            mock_provider: true,
            slack: true,
            slack_auth_mode: SlackAuthMode::Webhook,
            slack_webhook_url: Some("https://hooks.slack.com/services/T000/B000/secret".to_owned()),
            whatsapp: true,
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].id().as_ref(), "mock:local");
        assert_eq!(providers[1].id().as_ref(), "slack:setup");
        assert_eq!(providers[2].id().as_ref(), "whatsapp:bridge");

        Ok(())
    }

    #[test]
    fn build_providers_can_enable_multiple_inline_slack_workspaces() -> Result<()> {
        let args = Args {
            slack_workspace_profiles: vec![
                "label=Team Alpha,auth=user-oauth,user_token=xoxp-alpha".to_owned(),
                "label=Team Beta,auth=bot-token,bot_token=xoxb-beta".to_owned(),
            ],
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "slack:team-alpha");
        assert_eq!(providers[1].id().as_ref(), "slack:team-beta");
        assert!(
            providers
                .iter()
                .all(|provider| provider.platform() == chat_core::Platform::Slack)
        );
        Ok(())
    }

    #[test]
    fn build_providers_can_combine_single_and_profile_slack_workspaces() -> Result<()> {
        let args = Args {
            slack: true,
            slack_workspace: Some("Primary".to_owned()),
            slack_user_token: Some("xoxp-primary".to_owned()),
            slack_workspace_profiles: vec![
                "label=Team Alpha,auth=user-oauth,user_token=xoxp-alpha".to_owned(),
                "label=Team Beta,auth=webhook,webhook_url=https://hooks.slack.com/services/T000/B000/secret".to_owned(),
            ],
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 3);
        assert_eq!(providers[0].id().as_ref(), "slack:primary");
        assert_eq!(providers[1].id().as_ref(), "slack:team-alpha");
        assert_eq!(providers[2].id().as_ref(), "slack:team-beta");
        Ok(())
    }

    #[test]
    fn build_providers_reads_multiple_slack_workspaces_from_toml_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("slack-workspaces.toml");
        fs::write(
            &path,
            r#"
[[workspaces]]
workspace = "Engineering"
auth_mode = "read-only-oauth"
user_token = "xoxp-engineering"

[[workspaces]]
workspace = "Ops"
auth_mode = "bot-token"
bot_token = "xoxb-ops"
"#,
        )?;
        let args = Args {
            slack_workspaces_file: Some(path),
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "slack:engineering");
        assert_eq!(providers[1].id().as_ref(), "slack:ops");
        Ok(())
    }

    #[test]
    fn duplicate_slack_workspace_labels_are_rejected() {
        let args = Args {
            slack_workspace_profiles: vec![
                "label=Team Alpha,user_token=xoxp-alpha".to_owned(),
                "label=Team Alpha,user_token=xoxp-alpha-2".to_owned(),
            ],
            ..base_args()
        };

        let error = match build_providers(&args) {
            Ok(_) => panic!("expected duplicate Slack workspace labels to be rejected"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("duplicate Slack workspace provider id"));
    }

    #[test]
    fn duplicate_persisted_slack_accounts_are_deduped() -> Result<()> {
        let args = base_args();
        let providers = build_providers_with_persisted(
            &args,
            vec![
                stored_slack_account("slack:slack-workspace-1", "Slack Workspace 1", "xoxp-one")?,
                stored_slack_account(
                    "slack:slack-workspace-1-stale",
                    "Slack Workspace 1",
                    "xoxp-two",
                )?,
            ],
        )?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "slack:slack-workspace-1");
        assert_eq!(providers[1].id().as_ref(), "whatsapp:bridge");
        Ok(())
    }

    #[test]
    fn explicit_slack_profiles_override_matching_persisted_accounts() -> Result<()> {
        let args = Args {
            slack_workspace_profiles: vec!["label=Team Alpha,user_token=xoxp-new".to_owned()],
            ..base_args()
        };
        let providers = build_providers_with_persisted(
            &args,
            vec![stored_slack_account(
                "slack:team-alpha",
                "Team Alpha",
                "xoxp-old",
            )?],
        )?;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id().as_ref(), "slack:team-alpha");
        Ok(())
    }

    #[test]
    fn runtime_slack_factory_skips_existing_workspace_ordinals() -> Result<()> {
        let args = base_args();
        let factory = build_account_provider_factory(&args, vec!["slack:workspace-1".to_owned()]);

        let provider = factory(AccountProviderKind::Slack)?;

        assert_eq!(provider.id().as_ref(), "slack:workspace-2");
        Ok(())
    }

    #[test]
    fn build_providers_defaults_to_real_chat_providers_without_flags() -> Result<()> {
        let providers = build_providers(&base_args())?;

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id().as_ref(), "slack:setup");
        assert_eq!(providers[0].platform(), chat_core::Platform::Slack);
        assert_eq!(providers[1].id().as_ref(), "whatsapp:bridge");
        assert_eq!(providers[1].platform(), chat_core::Platform::WhatsApp);

        Ok(())
    }

    #[test]
    fn explicit_provider_flag_disables_default_provider_set() -> Result<()> {
        let args = Args {
            slack: true,
            slack_auth_mode: SlackAuthMode::ReadOnlyOAuth,
            slack_user_token: Some("xoxp-user".to_owned()),
            slack_workspace: Some("example".to_owned()),
            ..base_args()
        };

        let providers = build_providers(&args)?;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id().as_ref(), "slack:example");
        Ok(())
    }

    #[test]
    fn cleanup_paths_include_whatsapp_db_for_default_provider_startup() {
        let args = Args {
            whatsapp_db: PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
            db: Some(PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite")),
            test_cleanup: true,
            ..base_args()
        };

        assert_eq!(
            cleanup_paths(&args),
            vec![
                PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite"),
                PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db")
            ]
        );
    }

    #[test]
    fn cleanup_paths_include_explicit_app_db_and_enabled_whatsapp_db() {
        let args = Args {
            whatsapp: true,
            whatsapp_db: PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
            db: Some(PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite")),
            test_cleanup: true,
            ..base_args()
        };

        assert_eq!(
            cleanup_paths(&args),
            vec![
                PathBuf::from("/tmp/chat-cli-app-cleanup.sqlite"),
                PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db")
            ]
        );
    }

    #[test]
    fn cleanup_paths_include_default_whatsapp_db() {
        let args = Args {
            whatsapp_db: PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db"),
            test_cleanup: true,
            ..base_args()
        };

        assert_eq!(
            cleanup_paths(&args),
            vec![PathBuf::from("/tmp/chat-cli-whatsapp-cleanup.db")]
        );
    }

    #[test]
    fn cleanup_databases_removes_existing_files_directories_and_ignores_missing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app_db = dir.path().join("app.sqlite");
        let whatsapp_db = dir.path().join("whatsapp.db");
        fs::write(&app_db, b"app")?;
        fs::write(&whatsapp_db, b"whatsapp")?;

        cleanup_databases(&[
            app_db.clone(),
            whatsapp_db.clone(),
            dir.path().join("missing.db"),
        ])?;

        assert!(!app_db.exists());
        assert!(!whatsapp_db.exists());
        Ok(())
    }
}
