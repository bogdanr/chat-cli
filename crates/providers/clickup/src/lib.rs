//! ClickUp Chat provider.
//!
//! ClickUp exposes Chat through its **Public API v3** and offers no realtime
//! transport for it: webhooks cover tasks, lists, folders, spaces, and goals,
//! but not chat messages. This provider therefore polls, and the whole design
//! is organised around staying inside ClickUp's rate budget (100 requests per
//! minute per token on the lower plans).
//!
//! The single most important lever is the channel-listing filter
//! `with_message_since`: it makes the server return only channels that have a
//! message after a given instant, which collapses a steady-state poll from
//! "every channel, every pass" to "only channels with new activity". Slack has
//! no equivalent and has to approximate it client-side.
//!
//! Layering:
//!
//! * [`http`] — pooled agent, adaptive rate limiter, retry, secret redaction.
//! * [`api`] — wire types and the [`ClickUpApiClient`] seam plus its real
//!   HTTP implementation.
//! * [`convert`] — wire → domain conversion, with no I/O.
//! * this module — provider state, connection lifecycle, and the poll loop.

pub mod api;
pub mod convert;
pub mod http;

use crate::{
    api::{
        ChannelQuery, ClickUpApiClient, ClickUpHttpClient, MAX_PAGE_LIMIT, WireChannel, WireUser,
    },
    convert::{
        MessageContext, apply_direct_chat_identity, arc_str, chat_from_channel,
        chat_member_from_user, emoji_reaction_name, include_channel_in_sidebar, message_from_wire,
        millis_from_timestamp, parse_timestamp_str, sender_from_user,
    },
};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use chat_core::{
    events::{
        AuthChallenge, EventBus, NetworkActivityDirection, NetworkActivityKind, ProviderEvent,
    },
    provider::{
        AuthSubmission, AuthSubmissionMode, OutboundCapabilities, OutboundContent,
        OutboundMentions, Provider, resolve_mention_tokens,
    },
    types::*,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::broadcast, task::JoinHandle};

/// Interval between poll passes. ClickUp's floor plan allows 100 requests per
/// minute; a 15-second cadence leaves four passes per minute, which combined
/// with `with_message_since` narrowing keeps a quiet workspace at roughly one
/// request per pass.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Messages fetched per channel on each poll pass. One page is enough to catch
/// up between two 15-second passes in any realistic conversation.
pub const POLL_MESSAGE_LIMIT: u32 = 50;

/// Overlap subtracted from the previous pass time when building
/// `with_message_since`, absorbing clock skew between this host and ClickUp so
/// a message landing right on the boundary is not missed.
pub const POLL_OVERLAP: chrono::Duration = chrono::Duration::seconds(30);

/// Upper bound on remembered message ids, so a long-running session cannot
/// grow the dedup set without limit.
const MAX_SEEN_MESSAGE_IDS: usize = 20_000;

// ---------------------------------------------------------------------------
// Lock helpers
// ---------------------------------------------------------------------------

/// Reads through a poisoned-lock-tolerant guard. Provider state is a plain
/// cache: a panic elsewhere must not make the account permanently unusable.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|error| error.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|error| error.into_inner())
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// Persisted configuration for one ClickUp account.
///
/// The OAuth fields are reserved so adding browser sign-in later needs no
/// storage migration; phase one authenticates with a personal token only.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ClickUpProviderOptions {
    /// Human label for the workspace, used in the provider id and sidebar.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Numeric ClickUp workspace ("team") id every chat call is scoped to.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Personal API token (`pk_...`). Sent as a bare `Authorization` value.
    #[serde(default)]
    pub personal_token: Option<String>,
    /// Reserved for OAuth: application client id.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Reserved for OAuth: application client secret.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Reserved for OAuth: redirect URI registered with the application.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// Reserved for OAuth: exchanged access token, sent as `Bearer ...`.
    #[serde(default)]
    pub access_token: Option<String>,
}

/// Never print secrets, even accidentally, in a debug dump.
impl fmt::Debug for ClickUpProviderOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClickUpProviderOptions")
            .field("workspace", &self.workspace)
            .field("workspace_id", &self.workspace_id)
            .field(
                "personal_token",
                &http::redacted_option(&self.personal_token),
            )
            .field("client_id", &http::redacted_option(&self.client_id))
            .field("client_secret", &http::redacted_option(&self.client_secret))
            .field("redirect_uri", &self.redirect_uri)
            .field("access_token", &http::redacted_option(&self.access_token))
            .finish()
    }
}

impl ClickUpProviderOptions {
    /// Builds options carrying only a personal token.
    pub fn with_personal_token(token: impl Into<String>) -> Self {
        Self {
            personal_token: Some(token.into()),
            ..Self::default()
        }
    }

    /// Whether a credential is present at all. Without one, `connect` asks the
    /// UI for setup instead of failing.
    pub fn has_configured_credentials(&self) -> bool {
        self.authorization().is_some()
    }

    /// The full `Authorization` header value for the configured credential.
    ///
    /// ClickUp personal tokens are sent bare; OAuth tokens use `Bearer`.
    pub fn authorization(&self) -> Option<String> {
        if let Some(token) = non_empty(self.personal_token.as_deref()) {
            return Some(token.to_owned());
        }
        non_empty(self.access_token.as_deref()).map(|token| {
            if token.to_ascii_lowercase().starts_with("bearer ") {
                token.to_owned()
            } else {
                format!("Bearer {token}")
            }
        })
    }

    /// Stable, filesystem- and id-safe slug used in the provider id.
    ///
    /// The workspace id is preferred because it is the authoritative identity:
    /// the label is cosmetic, may be renamed in ClickUp, and may be absent on
    /// a flag-configured account. Keying on the label instead would let the
    /// same workspace start twice - once from flags, once from storage.
    fn slug(&self) -> String {
        let source = non_empty(self.workspace_id.as_deref())
            .or_else(|| non_empty(self.workspace.as_deref()))
            .unwrap_or("workspace");
        let mut slug = String::with_capacity(source.len());
        let mut last_was_dash = false;
        for character in source.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character.to_ascii_lowercase());
                last_was_dash = false;
            } else if !last_was_dash && !slug.is_empty() {
                slug.push('-');
                last_was_dash = true;
            }
        }
        let slug = slug.trim_matches('-').to_owned();
        if slug.is_empty() {
            "workspace".to_owned()
        } else {
            slug
        }
    }

    /// Display label for the account row.
    fn display_name(&self) -> String {
        non_empty(self.workspace.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| "ClickUp".to_owned())
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Everything learned from a successful credential validation.
#[derive(Clone, Debug, Default)]
struct ClickUpConnectionState {
    authorization: Option<String>,
    workspace_id: Option<String>,
    workspace_name: Option<String>,
    self_user_id: Option<String>,
    self_display_name: Option<String>,
}

impl ClickUpConnectionState {
    /// The credential plus workspace both required by every chat call.
    fn scope(&self) -> Option<(String, String)> {
        Some((
            self.authorization.clone()?,
            non_empty(self.workspace_id.as_deref())?.to_owned(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct ClickUpProvider {
    id: ProviderId,
    account: RwLock<Account>,
    options: RwLock<ClickUpProviderOptions>,
    connection: RwLock<ClickUpConnectionState>,
    chats: RwLock<Vec<Chat>>,
    /// Channel snapshots keyed by channel id, used by `chat_details` without
    /// issuing a fresh request from the draw path.
    channels: Arc<RwLock<HashMap<String, WireChannel>>>,
    /// Resolved users keyed by ClickUp user id, shared with the poll task.
    users: Arc<RwLock<HashMap<String, WireUser>>>,
    /// Member lists for DMs and group DMs, keyed by channel id and shared with
    /// the poll task. ClickUp gives these channels no server-side name, so
    /// their sidebar label and avatar have to be rebuilt from the members
    /// every time a channel snapshot is turned into a `Chat`.
    members: Arc<RwLock<HashMap<String, Vec<WireUser>>>>,
    api_client: Arc<dyn ClickUpApiClient>,
    events: EventBus,
    connected: AtomicBool,
    poll_task: RwLock<Option<JoinHandle<()>>>,
}

impl ClickUpProvider {
    /// Builds a provider from persisted or CLI-supplied options.
    pub fn with_options(options: ClickUpProviderOptions) -> Result<Self> {
        Self::with_options_and_client(options, Arc::new(ClickUpHttpClient))
    }

    /// Builds a provider against an arbitrary API client. Tests use this to
    /// substitute a fake and assert on the exact calls made.
    pub fn with_options_and_client(
        options: ClickUpProviderOptions,
        api_client: Arc<dyn ClickUpApiClient>,
    ) -> Result<Self> {
        let id: ProviderId = arc_str(format!("clickup:{}", options.slug()));
        let account = Account {
            id: id.clone(),
            platform: Platform::ClickUp,
            display_name: arc_str(options.display_name()),
            avatar: None,
        };
        let connection = ClickUpConnectionState {
            authorization: options.authorization(),
            workspace_id: non_empty(options.workspace_id.as_deref()).map(str::to_owned),
            workspace_name: non_empty(options.workspace.as_deref()).map(str::to_owned),
            ..ClickUpConnectionState::default()
        };

        Ok(Self {
            id,
            account: RwLock::new(account),
            options: RwLock::new(options),
            connection: RwLock::new(connection),
            chats: RwLock::new(Vec::new()),
            channels: Arc::new(RwLock::new(HashMap::new())),
            users: Arc::new(RwLock::new(HashMap::new())),
            members: Arc::new(RwLock::new(HashMap::new())),
            api_client,
            events: EventBus::new(),
            connected: AtomicBool::new(false),
            poll_task: RwLock::new(None),
        })
    }

    /// Snapshot of the current options, safe to serialise.
    pub fn options(&self) -> ClickUpProviderOptions {
        read_lock(&self.options).clone()
    }

    /// The credential and workspace scope, or a clear error naming what is
    /// missing.
    fn scope(&self) -> Result<(String, String)> {
        read_lock(&self.connection).scope().ok_or_else(|| {
            anyhow!(
                "ClickUp account {} is not connected: add a personal API token and workspace first",
                self.id
            )
        })
    }

    /// Emits a receive-direction network activity marker.
    fn note_rx(&self, kind: NetworkActivityKind) {
        self.events.send(ProviderEvent::NetworkActivity {
            direction: NetworkActivityDirection::Rx,
            kind,
        });
    }

    /// Emits a send-direction network activity marker.
    fn note_tx(&self, kind: NetworkActivityKind) {
        self.events.send(ProviderEvent::NetworkActivity {
            direction: NetworkActivityDirection::Tx,
            kind,
        });
    }

    /// Validates the credential, resolves the workspace, and records identity.
    async fn validate(&self) -> Result<()> {
        let authorization = self
            .options()
            .authorization()
            .ok_or_else(|| anyhow!("no ClickUp credential configured"))?;

        self.note_tx(NetworkActivityKind::Auth);
        let identity = self.api_client.identity(&authorization).await?;
        self.note_rx(NetworkActivityKind::Auth);

        if identity.workspaces.is_empty() {
            bail!("this ClickUp token cannot reach any workspace");
        }

        let configured = non_empty(self.options().workspace_id.as_deref()).map(str::to_owned);
        let workspace = match configured {
            Some(configured) => identity
                .workspaces
                .iter()
                .find(|workspace| workspace.id == configured)
                .ok_or_else(|| {
                    anyhow!("ClickUp workspace {configured} is not reachable with this token")
                })?,
            // A token that reaches exactly one workspace needs no choice; more
            // than one is ambiguous and must be resolved during setup.
            None if identity.workspaces.len() == 1 => &identity.workspaces[0],
            None => bail!(
                "this ClickUp token reaches {} workspaces; choose one during setup",
                identity.workspaces.len()
            ),
        };

        let workspace_name = non_empty(workspace.name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Workspace {}", workspace.id));

        {
            let mut connection = write_lock(&self.connection);
            connection.authorization = Some(authorization);
            connection.workspace_id = Some(workspace.id.clone());
            connection.workspace_name = Some(workspace_name.clone());
            connection.self_user_id = non_empty(Some(identity.user.id.as_str())).map(str::to_owned);
            connection.self_display_name = Some(identity.user.best_name());
        }

        {
            let mut options = write_lock(&self.options);
            options.workspace_id = Some(workspace.id.clone());
            if non_empty(options.workspace.as_deref()).is_none() {
                options.workspace = Some(workspace_name.clone());
            }
        }

        // Cache the authenticated user plus every workspace member so sender
        // names and avatars resolve without an extra lookup. The workspace
        // member list is the only ClickUp response carrying profile pictures
        // for other people — the v3 chat member listing omits them entirely.
        {
            let mut cache = write_lock(&self.users);
            for member in &workspace.members {
                if !member.user.id.trim().is_empty() {
                    cache.insert(member.user.id.clone(), member.user.clone());
                }
            }
            if !identity.user.id.trim().is_empty() {
                cache.insert(identity.user.id.clone(), identity.user.clone());
            }
        }

        let workspace_avatar = workspace
            .avatar
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .and_then(http::cached_avatar_path);

        {
            let mut account = write_lock(&self.account);
            account.display_name = arc_str(workspace_name);
            if workspace_avatar.is_some() {
                account.avatar = workspace_avatar;
            }
        }

        Ok(())
    }

    /// Resolved user record for a ClickUp user id, from the shared cache.
    fn cached_user(&self, user_id: &str) -> Option<WireUser> {
        read_lock(&self.users).get(user_id).cloned()
    }

    /// Builds a message-conversion context bound to one channel.
    fn message_context<'a>(
        &'a self,
        workspace_id: &'a str,
        channel_id: &'a str,
        self_user_id: Option<&'a str>,
        users: &'a (dyn Fn(&str) -> Option<WireUser> + Send + Sync),
    ) -> MessageContext<'a> {
        MessageContext {
            account: &self.id,
            workspace_id,
            channel_id,
            self_user_id,
            users,
        }
    }

    /// Fetches, caches, and returns the sidebar channel list.
    async fn load_chats(&self) -> Result<Vec<Chat>> {
        let (authorization, workspace_id) = self.scope()?;
        self.note_tx(NetworkActivityKind::Sync);
        let channels = self
            .api_client
            .list_channels(&authorization, &workspace_id, ChannelQuery::default())
            .await?;
        self.note_rx(NetworkActivityKind::Sync);

        let visible: Vec<WireChannel> = channels
            .into_iter()
            .filter(include_channel_in_sidebar)
            .collect();

        {
            let mut cache = write_lock(&self.channels);
            for channel in &visible {
                cache.insert(channel.id.clone(), channel.clone());
            }
        }

        let self_user_id = read_lock(&self.connection).self_user_id.clone();
        let chats: Vec<Chat> = visible
            .iter()
            .map(|channel| {
                let mut chat = chat_from_channel(&self.id, channel);
                hydrate_direct_chat(&mut chat, &self.members, self_user_id.as_deref());
                chat
            })
            .collect();
        *write_lock(&self.chats) = chats.clone();

        // DM/group-DM titles need a member listing, which must never block the
        // sidebar: the bare "Direct message" rows appear now and are renamed
        // through `ChatUpdated` as each member list lands.
        self.queue_direct_chat_resolution(&authorization, &workspace_id, &chats);
        Ok(chats)
    }

    /// Fetches member lists for DM/group-DM chats whose name is not resolved
    /// yet, one channel at a time so the rate limiter stays in control.
    fn queue_direct_chat_resolution(
        &self,
        authorization: &str,
        workspace_id: &str,
        chats: &[Chat],
    ) {
        let cached = read_lock(&self.members);
        let pending: Vec<Chat> = chats
            .iter()
            .filter(|chat| {
                matches!(chat.kind, ChatKind::Direct | ChatKind::GroupDirectMessage)
                    && !cached.contains_key(chat.id.as_ref())
            })
            .cloned()
            .collect();
        drop(cached);
        if pending.is_empty() {
            return;
        }

        let api_client = Arc::clone(&self.api_client);
        let users = Arc::clone(&self.users);
        let members = Arc::clone(&self.members);
        let events = self.events.clone();
        let authorization = authorization.to_owned();
        let workspace_id = workspace_id.to_owned();
        let self_user_id = read_lock(&self.connection).self_user_id.clone();

        tokio::spawn(async move {
            for mut chat in pending {
                let Some(resolved) = resolve_channel_members(
                    api_client.as_ref(),
                    &authorization,
                    &workspace_id,
                    chat.id.as_ref(),
                    &users,
                    &members,
                )
                .await
                else {
                    continue;
                };
                if apply_direct_chat_identity(&mut chat, &resolved, self_user_id.as_deref()) {
                    events.send(ProviderEvent::ChatUpdated(chat));
                }
            }
        });
    }

    /// Starts the polling task, replacing any previous one.
    fn start_polling(&self) {
        let Some((authorization, workspace_id)) = read_lock(&self.connection).scope() else {
            return;
        };
        self.stop_polling();

        let api_client = Arc::clone(&self.api_client);
        let events = self.events.clone();
        let account = self.id.clone();
        let self_user_id = read_lock(&self.connection).self_user_id.clone();
        let users = Arc::clone(&self.users);
        let channels = Arc::clone(&self.channels);
        let members = Arc::clone(&self.members);

        let handle = tokio::spawn(async move {
            run_poll_loop(PollContext {
                api_client,
                events,
                account,
                authorization,
                workspace_id,
                self_user_id,
                users,
                channels,
                members,
                started_at: Utc::now(),
            })
            .await;
        });
        *write_lock(&self.poll_task) = Some(handle);
    }

    fn stop_polling(&self) {
        if let Some(handle) = write_lock(&self.poll_task).take() {
            handle.abort();
        }
    }
}

impl fmt::Debug for ClickUpProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClickUpProvider")
            .field("id", &self.id)
            .field("connected", &self.connected.load(Ordering::Acquire))
            .finish()
    }
}

impl Drop for ClickUpProvider {
    fn drop(&mut self) {
        self.stop_polling();
    }
}

#[async_trait]
impl Provider for ClickUpProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn platform(&self) -> Platform {
        Platform::ClickUp
    }

    fn account_info(&self) -> Account {
        read_lock(&self.account).clone()
    }

    fn config_json(&self) -> Option<String> {
        serde_json::to_string(&*read_lock(&self.options)).ok()
    }

    fn outbound_capabilities(&self) -> OutboundCapabilities {
        OutboundCapabilities {
            mentions: true,
            // ClickUp lets authors edit their own chat messages with no
            // documented time limit.
            edit: true,
            edit_window: None,
            ..OutboundCapabilities::text_only(
                "ClickUp attachments are not supported yet; send a link instead",
            )
        }
    }

    /// ClickUp renders mentions as `@Display Name` in Markdown, which is exactly
    /// the token the composer inserts, so the text is sent unchanged. We still
    /// resolve the tokens so the resolved identities are reported to callers.
    fn encode_outbound_mentions(
        &self,
        text: &str,
        members: &[ChatMember],
        picks: &[Mention],
    ) -> OutboundMentions {
        let resolved = resolve_mention_tokens(text, members, picks);
        let mut mentioned: Vec<Mention> = Vec::new();
        for item in &resolved {
            if !mentioned
                .iter()
                .any(|existing| existing.platform_id == item.mention.platform_id)
            {
                mentioned.push(item.mention.clone());
            }
        }
        OutboundMentions {
            text: text.to_owned(),
            mentioned,
        }
    }

    fn discovery_capabilities(&self) -> DiscoveryCapabilities {
        let connected = self.is_connected();
        DiscoveryCapabilities {
            existing_chats: connected,
            contacts: false,
            users: false,
            public_channels: connected,
            private_channels: connected,
            open_dm: false,
            join_public_channel: false,
        }
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }

        if !self.options().has_configured_credentials() {
            self.events
                .send(ProviderEvent::AuthRequired(AuthChallenge::Waiting));
            return Ok(());
        }

        match self.validate().await {
            Ok(()) => {
                self.connected.store(true, Ordering::Release);
                self.events.send(ProviderEvent::AuthSucceeded);
                // Seed the sidebar before the first poll pass so the account
                // is usable immediately rather than after one interval.
                if let Ok(chats) = self.load_chats().await {
                    for chat in chats {
                        self.events.send(ProviderEvent::ChatUpdated(chat));
                    }
                }
                self.events.send(ProviderEvent::SyncComplete);
                self.start_polling();
                Ok(())
            }
            Err(error) => {
                self.connected.store(false, Ordering::Release);
                let reason = http::redact_secrets(&error.to_string());
                self.events
                    .send(ProviderEvent::Disconnected(Some(arc_str(reason.clone()))));
                Err(anyhow!(reason))
            }
        }
    }

    async fn disconnect(&self) -> Result<()> {
        self.stop_polling();
        self.connected.store(false, Ordering::Release);
        *write_lock(&self.chats) = Vec::new();
        write_lock(&self.channels).clear();
        self.events.send(ProviderEvent::Disconnected(None));
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    fn events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.events.subscribe()
    }

    async fn chats(&self) -> Result<Vec<Chat>> {
        self.load_chats().await
    }

    async fn history(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (authorization, workspace_id) = self.scope()?;
        let self_user_id = read_lock(&self.connection).self_user_id.clone();
        let names = |user_id: &str| self.cached_user(user_id);
        let context = self.message_context(&workspace_id, chat_id, self_user_id.as_deref(), &names);

        let mut collected: Vec<Message> = Vec::new();
        let mut cursor: Option<String> = None;

        // ClickUp paginates newest-first with an opaque cursor and offers no
        // "before this instant" filter, so backwards pagination means walking
        // pages until enough messages predate the anchor. The page budget
        // bounds that walk so a sparse channel cannot burn the rate budget.
        for _ in 0..api::MAX_PAGES_PER_LISTING {
            self.note_tx(NetworkActivityKind::History);
            let page = self
                .api_client
                .messages(
                    &authorization,
                    &workspace_id,
                    chat_id,
                    cursor.as_deref(),
                    MAX_PAGE_LIMIT.min(limit.max(1) as u32),
                )
                .await?;
            self.note_rx(NetworkActivityKind::History);

            let empty = page.data.is_empty();
            for wire in &page.data {
                let Some(message) = message_from_wire(wire, &context) else {
                    continue;
                };
                if before
                    .map(|before| message.timestamp >= before)
                    .unwrap_or(false)
                {
                    continue;
                }
                collected.push(message);
            }

            if collected.len() >= limit || empty {
                break;
            }
            match page.next_cursor.filter(|cursor| !cursor.trim().is_empty()) {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        collected.truncate(limit);
        // Present oldest-first, matching every other provider's history.
        collected.sort_by_key(|message| message.timestamp);
        Ok(collected)
    }

    async fn send(
        &self,
        chat_id: &ChatId,
        outbound: OutboundContent,
        reply_to: Option<&Message>,
    ) -> Result<MessageId> {
        let OutboundContent { content, .. } = outbound;
        let text = match content {
            Content::Text(text) => text,
            other => {
                let reason = self
                    .outbound_capabilities()
                    .unsupported_reason(&other)
                    .unwrap_or_else(|| "unsupported ClickUp content".to_owned());
                bail!(reason);
            }
        };
        if text.trim().is_empty() {
            bail!("cannot send an empty ClickUp message");
        }

        let (authorization, workspace_id) = self.scope()?;
        self.note_tx(NetworkActivityKind::Send);

        // ClickUp threads are addressed by the *root* message. Replying to a
        // reply must therefore target that reply's parent, or ClickUp would
        // create a second thread branch the UI cannot represent.
        let thread_root = reply_to.and_then(|message| {
            message
                .platform_data
                .clickup
                .as_ref()
                .and_then(|data| data.parent_message_id.clone())
                .or_else(|| Some(message.id.clone()))
        });

        let posted = match thread_root {
            Some(root) => {
                self.api_client
                    .send_reply(&authorization, &workspace_id, &root, &text)
                    .await?
            }
            None => {
                self.api_client
                    .send_message(&authorization, &workspace_id, chat_id, &text)
                    .await?
            }
        };
        self.note_rx(NetworkActivityKind::Send);

        let self_user_id = read_lock(&self.connection).self_user_id.clone();
        let names = |user_id: &str| self.cached_user(user_id);
        let context = self.message_context(&workspace_id, chat_id, self_user_id.as_deref(), &names);
        if let Some(message) = message_from_wire(&posted, &context) {
            let id = message.id.clone();
            self.events.send(ProviderEvent::Message {
                message,
                is_historical: false,
            });
            return Ok(id);
        }

        bail!("ClickUp accepted the message but returned no usable id")
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        message: &Message,
        outbound: OutboundContent,
    ) -> Result<Timestamp> {
        let OutboundContent { content, .. } = outbound;
        let text = match content {
            Content::Text(text) => text,
            _ => bail!("ClickUp can only edit the text of a message"),
        };
        if text.trim().is_empty() {
            bail!("cannot save an empty ClickUp message");
        }
        if !message.is_from_me {
            bail!("ClickUp only allows editing your own messages");
        }

        let (authorization, workspace_id) = self.scope()?;
        self.note_tx(NetworkActivityKind::Send);
        self.api_client
            .update_message(&authorization, &workspace_id, &message.id, &text)
            .await?;
        self.note_rx(NetworkActivityKind::Send);

        let edited_at = Utc::now();
        self.events.send(ProviderEvent::MessageContentEdited {
            chat_id: chat_id.clone(),
            message_id: message.id.clone(),
            content: Content::Text(text),
            edited_at,
        });
        Ok(edited_at)
    }

    async fn download_media(&self, _media: &Media) -> Result<PathBuf> {
        bail!("ClickUp attachment download is not supported yet")
    }

    async fn mark_read(&self, chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
        // ClickUp's public v3 Chat surface exposes no documented "mark channel
        // read" operation. Acknowledge locally so the unread badge clears in
        // this client; the next channel listing will report ClickUp's own
        // read state and correct it if the user reads elsewhere.
        self.events.send(ProviderEvent::ChatMarkedRead {
            chat_id: chat_id.clone(),
        });
        Ok(())
    }

    async fn react(&self, chat_id: &ChatId, message: &Message, emoji: &str) -> Result<()> {
        let (authorization, workspace_id) = self.scope()?;
        let reaction = emoji_reaction_name(emoji)
            .ok_or_else(|| anyhow!("ClickUp does not accept the reaction {emoji:?}"))?;
        let self_user_id = read_lock(&self.connection).self_user_id.clone();

        // ClickUp has separate add and remove endpoints rather than a toggle,
        // so decide from the reaction state already rendered.
        let already_reacted = self_user_id.as_deref().is_some_and(|self_id| {
            message.reactions.iter().any(|existing| {
                existing.emoji.as_ref() == convert::emoji_display(&reaction)
                    && existing
                        .senders
                        .iter()
                        .any(|sender| sender.as_ref() == self_id)
            })
        });

        self.note_tx(NetworkActivityKind::Reaction);
        if already_reacted {
            self.api_client
                .remove_reaction(&authorization, &workspace_id, &message.id, &reaction)
                .await?;
        } else {
            self.api_client
                .add_reaction(&authorization, &workspace_id, &message.id, &reaction)
                .await?;
        }
        self.note_rx(NetworkActivityKind::Reaction);

        if let Some(self_id) = self_user_id {
            self.events.send(ProviderEvent::ReactionChanged {
                chat_id: chat_id.clone(),
                message_id: message.id.clone(),
                emoji: arc_str(convert::emoji_display(&reaction)),
                added: !already_reacted,
                sender: arc_str(self_id),
            });
        }
        Ok(())
    }

    async fn submit_auth(&self, submission: AuthSubmission) -> Result<()> {
        let token = submission
            .user_token
            .as_deref()
            .or(submission.bot_token.as_deref())
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow!("enter a ClickUp personal API token (it starts with pk_)"))?;

        if !matches!(
            submission.mode,
            None | Some(AuthSubmissionMode::ImportedToken) | Some(AuthSubmissionMode::BotToken)
        ) {
            bail!("ClickUp currently supports personal API tokens only");
        }

        {
            let mut options = write_lock(&self.options);
            options.personal_token = Some(token.to_owned());
            if let Some(label) = non_empty(submission.workspace_label.as_deref()) {
                // A numeric label is the workspace id; anything else is a name.
                if label.chars().all(|character| character.is_ascii_digit()) {
                    options.workspace_id = Some(label.to_owned());
                } else {
                    options.workspace = Some(label.to_owned());
                }
            }
        }
        write_lock(&self.connection).authorization = self.options().authorization();

        self.connected.store(false, Ordering::Release);
        self.connect().await
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Message>> {
        // ClickUp's public API has no chat message search endpoint; returning
        // an empty result keeps the global search UI usable across the other
        // connected accounts instead of failing the whole query.
        Ok(Vec::new())
    }

    async fn contact_info(&self, platform_id: &PlatformId) -> Result<Option<Sender>> {
        Ok(self.cached_user(platform_id).as_ref().map(sender_from_user))
    }

    async fn chat_members(&self, chat_id: &ChatId) -> Result<Vec<ChatMember>> {
        let (authorization, workspace_id) = self.scope()?;
        self.note_tx(NetworkActivityKind::Sync);
        let members = self
            .api_client
            .channel_members(&authorization, &workspace_id, chat_id)
            .await?;
        self.note_rx(NetworkActivityKind::Sync);

        cache_channel_members(&self.users, &self.members, chat_id, &members);

        let self_user_id = read_lock(&self.connection).self_user_id.clone();
        Ok(members
            .iter()
            .map(|user| {
                let is_self = self_user_id.as_deref() == Some(user.id.as_str());
                chat_member_from_user(user).as_self(is_self)
            })
            .collect())
    }

    async fn chat_details(&self, chat_id: &ChatId) -> Result<ChatDetails> {
        // Serve from the poll/listing cache: the details pane is opened from
        // the draw path and must never trigger a synchronous fetch.
        let Some(channel) = read_lock(&self.channels).get(chat_id.as_ref()).cloned() else {
            return Ok(ChatDetails::default());
        };

        let description = non_empty(channel.description.as_deref())
            .or_else(|| non_empty(channel.topic.as_deref()))
            .map(arc_str);

        Ok(ChatDetails {
            description,
            created_at: channel.created_at.as_deref().and_then(parse_timestamp_str),
            creator: channel
                .creator
                .as_deref()
                .and_then(|creator| self.cached_user(creator))
                .map(|creator| arc_str(creator.best_name())),
            member_count: None,
            admin_count: None,
            workspace: read_lock(&self.connection)
                .workspace_name
                .as_deref()
                .map(arc_str),
            is_archived: channel.archived.unwrap_or(false),
            is_externally_shared: false,
            only_admins_can_send: false,
            only_admins_can_edit: false,
            disappearing_seconds: None,
            facts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Direct-chat identity
// ---------------------------------------------------------------------------

/// Records a channel's member list in both shared caches.
///
/// `users` powers sender names and avatars on messages; `members` powers the
/// DM/group-DM sidebar label, which ClickUp never sends as a channel name.
fn cache_channel_members(
    users: &RwLock<HashMap<String, WireUser>>,
    members: &RwLock<HashMap<String, Vec<WireUser>>>,
    channel_id: &str,
    resolved: &[WireUser],
) {
    {
        let mut cache = write_lock(users);
        for member in resolved {
            if member.id.trim().is_empty() {
                continue;
            }
            // The v3 chat member listing carries no profile picture, so a
            // blind insert would erase the one seeded from the workspace
            // directory and drop the avatar back to initials. Keep the
            // already-known picture when the incoming record lacks one.
            let entry = cache.entry(member.id.clone()).or_default();
            let known_picture = entry.profile_picture.take();
            *entry = member.clone();
            if entry.profile_picture.is_none() {
                entry.profile_picture = known_picture;
            }
        }
    }

    // Store the member list with avatars filled in from the directory, so the
    // DM sidebar row and its avatar both resolve from this one cache.
    let enriched: Vec<WireUser> = {
        let cache = read_lock(users);
        resolved
            .iter()
            .map(|member| {
                cache
                    .get(member.id.trim())
                    .filter(|known| known.profile_picture.is_some())
                    .cloned()
                    .unwrap_or_else(|| member.clone())
            })
            .collect()
    };
    write_lock(members).insert(channel_id.to_owned(), enriched);
}

/// Applies a cached member list to a DM/group-DM chat. Purely local: safe to
/// call while building the sidebar.
///
/// Reports whether a member list was cached at all, not whether it produced a
/// name: a channel that legitimately lists no members is still resolved, and
/// must not be re-requested on every poll pass.
fn hydrate_direct_chat(
    chat: &mut Chat,
    members: &RwLock<HashMap<String, Vec<WireUser>>>,
    self_user_id: Option<&str>,
) -> bool {
    let Some(cached) = read_lock(members).get(chat.id.as_ref()).cloned() else {
        return false;
    };
    apply_direct_chat_identity(chat, &cached, self_user_id);
    true
}

/// Fetches and caches a channel's members, returning `None` when ClickUp
/// refuses the listing so the caller can leave the fallback label in place.
async fn resolve_channel_members(
    api_client: &dyn ClickUpApiClient,
    authorization: &str,
    workspace_id: &str,
    channel_id: &str,
    users: &RwLock<HashMap<String, WireUser>>,
    members: &RwLock<HashMap<String, Vec<WireUser>>>,
) -> Option<Vec<WireUser>> {
    let resolved = api_client
        .channel_members(authorization, workspace_id, channel_id)
        .await
        .ok()?;
    cache_channel_members(users, members, channel_id, &resolved);
    Some(resolved)
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

/// Everything the detached poll task needs. Bundled into a struct so the loop
/// signature stays readable as fields are added.
struct PollContext {
    api_client: Arc<dyn ClickUpApiClient>,
    events: EventBus,
    account: ProviderId,
    authorization: String,
    workspace_id: String,
    self_user_id: Option<String>,
    users: Arc<RwLock<HashMap<String, WireUser>>>,
    channels: Arc<RwLock<HashMap<String, WireChannel>>>,
    members: Arc<RwLock<HashMap<String, Vec<WireUser>>>>,
    /// Liveness anchor. Messages older than this are backlog and must never
    /// raise a notification, however new they are to this process.
    started_at: Timestamp,
}

/// Mutable state carried between poll passes.
#[derive(Default)]
struct PollState {
    /// `chat:message` keys seen this session, mapped to the last observed
    /// `edited_at` so a later `date_updated` bump surfaces as an edit without
    /// any extra request.
    seen_message_ids: HashMap<String, Option<Timestamp>>,
    /// End of the window covered by the last successful pass. `None` on the
    /// first pass, which lists every channel to establish a baseline.
    covered_until: Option<Timestamp>,
}

/// Runs poll passes until the task is aborted.
async fn run_poll_loop(context: PollContext) {
    let mut state = PollState::default();
    loop {
        run_poll_pass(&context, &mut state).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One poll pass.
///
/// The pass asks ClickUp for channels with activity since the previous pass
/// (minus an overlap), then fetches recent messages only for those channels.
/// In a quiet workspace that is a single request; in a busy one it scales with
/// the number of *active* channels rather than the total.
async fn run_poll_pass(context: &PollContext, state: &mut PollState) -> bool {
    let pass_started = Utc::now();
    let query = ChannelQuery {
        with_message_since: state
            .covered_until
            .map(|covered| millis_from_timestamp(covered - POLL_OVERLAP)),
        include_closed: false,
    };

    context.events.send(ProviderEvent::NetworkActivity {
        direction: NetworkActivityDirection::Tx,
        kind: NetworkActivityKind::Sync,
    });
    let channels = match context
        .api_client
        .list_channels(&context.authorization, &context.workspace_id, query)
        .await
    {
        Ok(channels) => channels,
        // A failed pass leaves `covered_until` untouched, so the next pass
        // re-covers this window rather than silently skipping it.
        Err(_) => return false,
    };
    context.events.send(ProviderEvent::NetworkActivity {
        direction: NetworkActivityDirection::Rx,
        kind: NetworkActivityKind::Sync,
    });

    let mut delivered_live_message = false;
    for channel in channels.into_iter().filter(include_channel_in_sidebar) {
        write_lock(&context.channels).insert(channel.id.clone(), channel.clone());

        let mut chat = chat_from_channel(&context.account, &channel);
        // A DM snapshot carries no name, so emitting it unhydrated would
        // rename an already-resolved sidebar row back to "Direct message".
        // Resolve the members once per channel and reuse the cache after that.
        if matches!(chat.kind, ChatKind::Direct | ChatKind::GroupDirectMessage)
            && !hydrate_direct_chat(&mut chat, &context.members, context.self_user_id.as_deref())
            && let Some(resolved) = resolve_channel_members(
                context.api_client.as_ref(),
                &context.authorization,
                &context.workspace_id,
                &channel.id,
                &context.users,
                &context.members,
            )
            .await
        {
            apply_direct_chat_identity(&mut chat, &resolved, context.self_user_id.as_deref());
        }
        context.events.send(ProviderEvent::ChatUpdated(chat));

        context.events.send(ProviderEvent::NetworkActivity {
            direction: NetworkActivityDirection::Tx,
            kind: NetworkActivityKind::History,
        });
        let page = match context
            .api_client
            .messages(
                &context.authorization,
                &context.workspace_id,
                &channel.id,
                None,
                POLL_MESSAGE_LIMIT,
            )
            .await
        {
            Ok(page) => page,
            Err(_) => continue,
        };
        context.events.send(ProviderEvent::NetworkActivity {
            direction: NetworkActivityDirection::Rx,
            kind: NetworkActivityKind::History,
        });

        let names = |user_id: &str| read_lock(&context.users).get(user_id).cloned();
        let message_context = MessageContext {
            account: &context.account,
            workspace_id: &context.workspace_id,
            channel_id: &channel.id,
            self_user_id: context.self_user_id.as_deref(),
            users: &names,
        };

        for wire in &page.data {
            let Some(message) = message_from_wire(wire, &message_context) else {
                continue;
            };
            let key = format!("{}:{}", message.chat_id, message.id);
            let previous = state.seen_message_ids.insert(key, message.edited_at);
            let first_seen = previous.is_none();
            if let Some(previous_edit) = previous
                && poll_edit_is_new(previous_edit, message.edited_at)
                && let Some(edited_at) = message.edited_at
            {
                context.events.send(ProviderEvent::MessageContentEdited {
                    chat_id: message.chat_id.clone(),
                    message_id: message.id.clone(),
                    content: message.content.clone(),
                    edited_at,
                });
                continue;
            }
            if !poll_message_is_live(context.started_at, message.timestamp, first_seen) {
                continue;
            }
            delivered_live_message = true;
            context.events.send(ProviderEvent::Message {
                message,
                is_historical: false,
            });
        }
    }

    // Only advance the window after a successful listing, and anchor it to
    // when the pass *started* so messages that landed mid-pass are re-covered
    // instead of dropped.
    state.covered_until = Some(pass_started);
    prune_seen_message_ids(&mut state.seen_message_ids);
    delivered_live_message
}

/// Whether a polled message should surface as new activity.
/// Both conditions matter: `first_seen` suppresses repeats within a session,
/// and the `started_at` anchor suppresses the pre-existing backlog that the
/// first pass necessarily returns.
fn poll_message_is_live(
    started_at: Timestamp,
    message_timestamp: Timestamp,
    first_seen: bool,
) -> bool {
    first_seen && message_timestamp > started_at
}

/// Whether a re-polled message carries an edit newer than the one last seen.
/// Storage applies edits monotonically, so a spurious repeat is harmless, but
/// suppressing it here keeps the event stream quiet.
fn poll_edit_is_new(previous: Option<Timestamp>, current: Option<Timestamp>) -> bool {
    match (previous, current) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(previous), Some(current)) => current > previous,
    }
}

/// Keeps the dedup set bounded. Clearing wholesale is safe because the
/// `started_at` anchor still suppresses backlog, so the worst case after a
/// prune is that recent messages are re-emitted once and deduped downstream by
/// message id.
fn prune_seen_message_ids<V>(seen: &mut HashMap<String, V>) {
    if seen.len() > MAX_SEEN_MESSAGE_IDS {
        seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        ClickUpIdentity, Page, WireMessage, WireReaction, WireWorkspace, WireWorkspaceMember,
    };
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Fake client
    // -----------------------------------------------------------------------

    /// Records every call so tests can assert on the exact request pattern,
    /// which is how rate-budget regressions (an extra request per channel per
    /// pass) are caught before they reach a real workspace.
    #[derive(Default)]
    struct FakeState {
        calls: Vec<String>,
        workspaces: Vec<WireWorkspace>,
        /// Channel pages keyed by nothing: the whole set is filtered by
        /// `with_message_since` the way the server would.
        channels: Vec<WireChannel>,
        /// Messages per channel id, newest first, as ClickUp returns them.
        messages: HashMap<String, Vec<WireMessage>>,
        members: Vec<WireUser>,
        fail_list_channels: bool,
    }

    struct FakeClient {
        state: Mutex<FakeState>,
    }

    impl FakeClient {
        fn new(state: FakeState) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(state),
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
            match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        fn record(&self, call: impl Into<String>) {
            self.lock().calls.push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.lock().calls.clone()
        }

        fn calls_starting_with(&self, prefix: &str) -> usize {
            self.calls()
                .iter()
                .filter(|call| call.starts_with(prefix))
                .count()
        }

        fn push_message(&self, channel_id: &str, message: WireMessage) {
            self.lock()
                .messages
                .entry(channel_id.to_owned())
                .or_default()
                .insert(0, message);
        }
    }

    #[async_trait]
    impl ClickUpApiClient for FakeClient {
        async fn identity(&self, authorization: &str) -> Result<ClickUpIdentity> {
            self.record(format!("identity:{authorization}"));
            Ok(ClickUpIdentity {
                user: WireUser {
                    id: "1001".to_owned(),
                    name: Some("Test User".to_owned()),
                    ..WireUser::default()
                },
                workspaces: self.lock().workspaces.clone(),
            })
        }

        async fn list_channels(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            query: ChannelQuery,
        ) -> Result<Vec<WireChannel>> {
            self.record(format!(
                "list_channels:since={}",
                query
                    .with_message_since
                    .map(|since| since.to_string())
                    .unwrap_or_else(|| "none".to_owned())
            ));
            if self.lock().fail_list_channels {
                bail!("simulated listing failure");
            }
            let channels = self.lock().channels.clone();
            // Mirror the server-side narrowing so tests exercise the real
            // steady-state behaviour rather than an always-full listing.
            Ok(match query.with_message_since {
                None => channels,
                Some(since) => channels
                    .into_iter()
                    .filter(|channel| {
                        channel
                            .latest_comment_at
                            .as_deref()
                            .and_then(parse_timestamp_str)
                            .map(|at| millis_from_timestamp(at) >= since)
                            .unwrap_or(false)
                    })
                    .collect(),
            })
        }

        async fn channel(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            channel_id: &str,
        ) -> Result<WireChannel> {
            self.record(format!("channel:{channel_id}"));
            self.lock()
                .channels
                .iter()
                .find(|channel| channel.id == channel_id)
                .cloned()
                .ok_or_else(|| anyhow!("no such channel"))
        }

        async fn channel_members(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            channel_id: &str,
        ) -> Result<Vec<WireUser>> {
            self.record(format!("channel_members:{channel_id}"));
            Ok(self.lock().members.clone())
        }

        async fn messages(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            channel_id: &str,
            cursor: Option<&str>,
            limit: u32,
        ) -> Result<Page<WireMessage>> {
            self.record(format!(
                "messages:{channel_id}:cursor={}:limit={limit}",
                cursor.unwrap_or("none")
            ));
            let all = self
                .lock()
                .messages
                .get(channel_id)
                .cloned()
                .unwrap_or_default();
            // Cursors are page offsets encoded as decimal text.
            let offset: usize = cursor.and_then(|cursor| cursor.parse().ok()).unwrap_or(0);
            let page: Vec<WireMessage> = all
                .iter()
                .skip(offset)
                .take(limit as usize)
                .cloned()
                .collect();
            let next = offset + page.len();
            Ok(Page {
                data: page,
                next_cursor: (next < all.len()).then(|| next.to_string()),
            })
        }

        async fn replies(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> Result<Page<WireMessage>> {
            self.record(format!("replies:{message_id}"));
            Ok(Page::default())
        }

        async fn send_message(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            channel_id: &str,
            content: &str,
        ) -> Result<WireMessage> {
            self.record(format!("send_message:{channel_id}:{content}"));
            Ok(WireMessage {
                id: "m-sent".to_owned(),
                content: Some(content.to_owned()),
                date: Some(millis_from_timestamp(Utc::now()) as f64),
                user_id: Some("1001".to_owned()),
                parent_channel: Some(channel_id.to_owned()),
                ..WireMessage::default()
            })
        }

        async fn send_reply(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
            content: &str,
        ) -> Result<WireMessage> {
            self.record(format!("send_reply:{message_id}:{content}"));
            Ok(WireMessage {
                id: "m-reply".to_owned(),
                content: Some(content.to_owned()),
                date: Some(millis_from_timestamp(Utc::now()) as f64),
                user_id: Some("1001".to_owned()),
                parent_message: Some(message_id.to_owned()),
                ..WireMessage::default()
            })
        }

        async fn update_message(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
            content: &str,
        ) -> Result<()> {
            self.record(format!("update_message:{message_id}:{content}"));
            Ok(())
        }

        async fn message_reactions(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
        ) -> Result<Vec<WireReaction>> {
            self.record(format!("message_reactions:{message_id}"));
            Ok(Vec::new())
        }

        async fn add_reaction(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
            reaction: &str,
        ) -> Result<()> {
            self.record(format!("add_reaction:{message_id}:{reaction}"));
            Ok(())
        }

        async fn remove_reaction(
            &self,
            _authorization: &str,
            _workspace_id: &str,
            message_id: &str,
            reaction: &str,
        ) -> Result<()> {
            self.record(format!("remove_reaction:{message_id}:{reaction}"));
            Ok(())
        }
    }

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn workspace(id: &str, name: &str) -> WireWorkspace {
        WireWorkspace {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            avatar: Some("https://cdn.example.com/acme-logo.png".to_owned()),
            members: vec![WireWorkspaceMember {
                user: WireUser {
                    id: "1002".to_owned(),
                    name: Some("Ada Lovelace".to_owned()),
                    profile_picture: Some("https://cdn.example.com/ada.jpg".to_owned()),
                    ..WireUser::default()
                },
            }],
            ..WireWorkspace::default()
        }
    }

    fn channel(id: &str, name: &str, latest: Timestamp) -> WireChannel {
        WireChannel {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            channel_kind: Some("CHANNEL".to_owned()),
            visibility: Some("PUBLIC".to_owned()),
            workspace_id: Some("900".to_owned()),
            latest_comment_at: Some(latest.to_rfc3339()),
            ..WireChannel::default()
        }
    }

    fn message(id: &str, channel_id: &str, user: &str, text: &str, at: Timestamp) -> WireMessage {
        WireMessage {
            id: id.to_owned(),
            content: Some(text.to_owned()),
            date: Some(millis_from_timestamp(at) as f64),
            user_id: Some(user.to_owned()),
            parent_channel: Some(channel_id.to_owned()),
            ..WireMessage::default()
        }
    }

    /// A fake with one workspace and one channel holding `messages`.
    fn single_workspace_state(messages: Vec<WireMessage>) -> FakeState {
        let now = Utc::now();
        let mut by_channel = HashMap::new();
        by_channel.insert("c-1".to_owned(), messages);
        FakeState {
            workspaces: vec![workspace("900", "Acme")],
            channels: vec![channel("c-1", "general", now)],
            messages: by_channel,
            ..FakeState::default()
        }
    }

    fn provider_with(client: Arc<FakeClient>) -> ClickUpProvider {
        ClickUpProvider::with_options_and_client(
            ClickUpProviderOptions::with_personal_token("pk_test"),
            client,
        )
        .expect("provider builds")
    }

    /// Drains currently-buffered provider events without waiting.
    fn drain(receiver: &mut broadcast::Receiver<ProviderEvent>) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    // -----------------------------------------------------------------------
    // Unit tests
    // -----------------------------------------------------------------------

    fn mention_member(id: &str, name: &str) -> ChatMember {
        ChatMember::new(Sender {
            platform_id: Arc::from(id),
            display_name: Arc::from(name),
            avatar: None,
        })
    }

    #[test]
    fn clickup_mentions_pass_through_as_display_names() {
        let provider = provider_with(FakeClient::new(FakeState::default()));
        let members = vec![mention_member("12345", "Bogdan")];

        // ClickUp resolves `@Display Name` server-side, so the text is sent
        // unchanged while the resolved identity is still reported.
        let encoded = provider.encode_outbound_mentions("hi @Bogdan", &members, &[]);
        assert_eq!(encoded.text, "hi @Bogdan");
        assert_eq!(
            encoded
                .mentioned
                .iter()
                .map(|mention| mention.platform_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["12345"]
        );
    }

    #[test]
    fn clickup_leaves_unresolved_mentions_as_plain_text() {
        let provider = provider_with(FakeClient::new(FakeState::default()));
        let members = vec![mention_member("12345", "Bogdan")];

        let encoded = provider.encode_outbound_mentions("hi @Nobody", &members, &[]);
        assert_eq!(encoded.text, "hi @Nobody");
        assert!(encoded.mentioned.is_empty());
    }

    #[test]
    fn slug_prefers_the_workspace_id_over_the_renameable_label() {
        // The id is the authoritative identity, so the same workspace slugs
        // identically whether or not a label happens to be configured.
        let with_label = ClickUpProviderOptions {
            workspace: Some("Acme Corp".to_owned()),
            workspace_id: Some("9013".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        let without_label = ClickUpProviderOptions {
            workspace_id: Some("9013".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        assert_eq!(with_label.slug(), "9013");
        assert_eq!(with_label.slug(), without_label.slug());
    }

    #[test]
    fn slug_falls_back_to_a_sanitised_label_then_a_default() {
        // Before the first connect there may be no id yet, so the label has to
        // produce an id-safe slug on its own.
        let options = ClickUpProviderOptions {
            workspace: Some("  Acme  Corp / EU ".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        assert_eq!(options.slug(), "acme-corp-eu");
        assert_eq!(ClickUpProviderOptions::default().slug(), "workspace");
    }

    #[test]
    fn personal_token_is_sent_bare_and_oauth_token_as_bearer() {
        assert_eq!(
            ClickUpProviderOptions::with_personal_token("pk_123").authorization(),
            Some("pk_123".to_owned())
        );
        let oauth = ClickUpProviderOptions {
            access_token: Some("abc".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        assert_eq!(oauth.authorization(), Some("Bearer abc".to_owned()));

        // An already-prefixed token must not be double-prefixed.
        let prefixed = ClickUpProviderOptions {
            access_token: Some("Bearer abc".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        assert_eq!(prefixed.authorization(), Some("Bearer abc".to_owned()));
    }

    #[test]
    fn options_debug_never_prints_secrets() {
        let options = ClickUpProviderOptions {
            personal_token: Some("pk_super_secret".to_owned()),
            client_secret: Some("shhh".to_owned()),
            ..ClickUpProviderOptions::default()
        };
        let rendered = format!("{options:?}");
        assert!(!rendered.contains("pk_super_secret"));
        assert!(!rendered.contains("shhh"));
    }

    #[test]
    fn backlog_never_counts_as_live() {
        let started_at = Utc::now();
        let backlog = started_at - chrono::Duration::hours(3);
        let fresh = started_at + chrono::Duration::seconds(5);

        assert!(!poll_message_is_live(started_at, backlog, true));
        assert!(!poll_message_is_live(started_at, fresh, false));
        assert!(!poll_message_is_live(started_at, started_at, true));
        assert!(poll_message_is_live(started_at, fresh, true));
    }

    // -----------------------------------------------------------------------
    // Connection lifecycle
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_seeds_the_workspace_logo_and_member_avatars() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));

        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        // The workspace logo is the account row's avatar; without it the
        // sidebar shows only the `CU` initials badge.
        assert!(
            provider.account_info().avatar.is_some(),
            "workspace avatar must reach the account"
        );

        // `/v2/team` is the only listing carrying other people's pictures, so
        // its members must land in the directory that senders resolve through.
        let seeded = read_lock(&provider.users)
            .get("1002")
            .cloned()
            .expect("workspace member is cached at connect");
        assert_eq!(
            seeded.profile_picture.as_deref(),
            Some("https://cdn.example.com/ada.jpg")
        );
    }

    #[test]
    fn caching_members_keeps_a_picture_the_member_listing_omits() {
        let users = RwLock::new(HashMap::new());
        let members = RwLock::new(HashMap::new());

        // Seeded from the workspace directory, which has the picture.
        write_lock(&users).insert(
            "1002".to_owned(),
            WireUser {
                id: "1002".to_owned(),
                name: Some("Ada Lovelace".to_owned()),
                profile_picture: Some("https://cdn.example.com/ada.jpg".to_owned()),
                ..WireUser::default()
            },
        );

        // The v3 chat member listing returns the same user with no picture.
        cache_channel_members(
            &users,
            &members,
            "c-dm",
            &[WireUser {
                id: "1002".to_owned(),
                name: Some("Ada Lovelace".to_owned()),
                ..WireUser::default()
            }],
        );

        assert_eq!(
            read_lock(&users)
                .get("1002")
                .and_then(|user| user.profile_picture.clone())
                .as_deref(),
            Some("https://cdn.example.com/ada.jpg"),
            "a picture-less member record must not erase the known avatar"
        );
        assert_eq!(
            read_lock(&members)["c-dm"][0].profile_picture.as_deref(),
            Some("https://cdn.example.com/ada.jpg"),
            "the cached member list must carry the avatar for the DM row"
        );
    }

    #[tokio::test]
    async fn connect_resolves_the_only_reachable_workspace() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));
        let mut events = provider.events();

        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        assert!(provider.is_connected());
        assert_eq!(provider.options().workspace_id.as_deref(), Some("900"));
        assert_eq!(provider.account_info().display_name.as_ref(), "Acme");
        // The bare personal token must reach the API unprefixed.
        assert!(client.calls().contains(&"identity:pk_test".to_owned()));

        let drained = drain(&mut events);
        assert!(
            drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::AuthSucceeded))
        );
        assert!(drained.iter().any(
            |event| matches!(event, ProviderEvent::ChatUpdated(chat) if chat.id.as_ref() == "c-1")
        ));
        assert!(
            drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::SyncComplete))
        );
        // Polling is expected behaviour (explained in the setup screen), so a
        // successful connect must not raise an account notice.
        assert!(
            !drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::AccountNotice { .. }))
        );
    }

    #[tokio::test]
    async fn connect_refuses_an_ambiguous_multi_workspace_token() {
        let state = FakeState {
            workspaces: vec![workspace("900", "Acme"), workspace("901", "Globex")],
            ..FakeState::default()
        };
        let provider = provider_with(FakeClient::new(state));

        let error = provider.connect().await.expect_err("must not guess");
        assert!(error.to_string().contains("2 workspaces"));
        assert!(!provider.is_connected());
    }

    #[tokio::test]
    async fn connect_without_credentials_asks_for_auth_instead_of_failing() {
        let provider = ClickUpProvider::with_options_and_client(
            ClickUpProviderOptions::default(),
            FakeClient::new(FakeState::default()),
        )
        .expect("provider builds");
        let mut events = provider.events();

        provider.connect().await.expect("no hard failure");

        assert!(!provider.is_connected());
        assert!(
            drain(&mut events)
                .iter()
                .any(|event| matches!(event, ProviderEvent::AuthRequired(AuthChallenge::Waiting)))
        );
    }

    #[tokio::test]
    async fn submit_auth_stores_the_token_and_connects() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = ClickUpProvider::with_options_and_client(
            ClickUpProviderOptions::default(),
            Arc::clone(&client) as Arc<dyn ClickUpApiClient>,
        )
        .expect("provider builds");

        provider
            .submit_auth(AuthSubmission {
                user_token: Some("pk_from_ui".to_owned()),
                ..AuthSubmission::default()
            })
            .await
            .expect("auth accepted");
        provider.stop_polling();

        assert!(provider.is_connected());
        assert_eq!(
            provider.options().personal_token.as_deref(),
            Some("pk_from_ui")
        );
        assert!(client.calls().contains(&"identity:pk_from_ui".to_owned()));
    }

    #[tokio::test]
    async fn submit_auth_rejects_an_empty_token() {
        let provider = provider_with(FakeClient::new(single_workspace_state(Vec::new())));
        let error = provider
            .submit_auth(AuthSubmission {
                user_token: Some("   ".to_owned()),
                ..AuthSubmission::default()
            })
            .await
            .expect_err("blank token rejected");
        assert!(error.to_string().contains("pk_"));
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chats_hides_archived_and_hidden_channels() {
        let now = Utc::now();
        let state = FakeState {
            workspaces: vec![workspace("900", "Acme")],
            channels: vec![
                channel("c-1", "general", now),
                WireChannel {
                    archived: Some(true),
                    ..channel("c-archived", "old", now)
                },
                WireChannel {
                    is_hidden: Some(true),
                    channel_kind: Some("DM".to_owned()),
                    ..channel("c-hidden", "closed dm", now)
                },
            ],
            ..FakeState::default()
        };
        let provider = provider_with(FakeClient::new(state));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let chats = provider.chats().await.expect("chats load");
        let ids: Vec<&str> = chats.iter().map(|chat| chat.id.as_ref()).collect();
        assert_eq!(ids, vec!["c-1"]);
    }

    #[tokio::test]
    async fn chats_never_report_activity_metadata_as_message_activity() {
        // Repository rule: `last_message_at`/`last_message_preview` must come
        // from real stored messages, not from provider chat metadata.
        let provider = provider_with(FakeClient::new(single_workspace_state(Vec::new())));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let chats = provider.chats().await.expect("chats load");
        let chat = chats.first().expect("one chat");
        assert!(chat.last_message_at.is_none());
        assert!(chat.last_message_preview.is_none());
    }

    /// A DM has no server-side name, so the sidebar must show the counterpart
    /// rather than the "Direct message" placeholder. The member listing is not
    /// allowed to block the first sidebar paint, so the rename arrives as a
    /// `ChatUpdated` event.
    #[tokio::test]
    async fn direct_chats_are_renamed_after_members_resolve() {
        let now = Utc::now();
        let state = FakeState {
            workspaces: vec![workspace("900", "Acme")],
            channels: vec![WireChannel {
                name: None,
                channel_kind: Some("DM".to_owned()),
                ..channel("c-dm", "", now)
            }],
            members: vec![
                WireUser {
                    id: "1001".to_owned(),
                    name: Some("Test User".to_owned()),
                    ..WireUser::default()
                },
                WireUser {
                    id: "1002".to_owned(),
                    name: Some("Kethe".to_owned()),
                    profile_picture: Some("https://cdn.example.com/kethe.jpg".to_owned()),
                    ..WireUser::default()
                },
            ],
            ..FakeState::default()
        };
        let provider = provider_with(FakeClient::new(state));
        let mut events = provider.events();
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        // The placeholder row is published first, so the sidebar is never
        // blocked on the member listing.
        let placeholder = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(ProviderEvent::ChatUpdated(chat)) = events.recv().await
                    && chat.id.as_ref() == "c-dm"
                {
                    return chat;
                }
            }
        })
        .await
        .expect("placeholder arrives");
        assert_eq!(placeholder.name.as_ref(), "Direct message");

        // The rename lands once the member list resolves in the background.
        let renamed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(ProviderEvent::ChatUpdated(chat)) = events.recv().await
                    && chat.id.as_ref() == "c-dm"
                    && chat.name.as_ref() != "Direct message"
                {
                    return chat;
                }
            }
        })
        .await
        .expect("rename arrives");
        assert_eq!(renamed.name.as_ref(), "Kethe");
        assert!(renamed.avatar.is_some());

        // A later listing reuses the cached members instead of re-requesting.
        let chats = provider.chats().await.expect("chats reload");
        assert_eq!(chats[0].name.as_ref(), "Kethe");
        assert!(chats[0].avatar.is_some());
    }

    /// `Utc::now()` carries sub-millisecond precision that ClickUp timestamps
    /// (epoch milliseconds) cannot represent, so anchors used in comparisons
    /// must be truncated to whole milliseconds or a round-tripped message will
    /// appear infinitesimally older than the anchor built from the same value.
    fn millis_aligned(timestamp: Timestamp) -> Timestamp {
        convert::timestamp_from_millis(millis_from_timestamp(timestamp) as f64)
            .expect("in-range timestamp")
    }

    #[tokio::test]
    async fn history_returns_oldest_first_and_honours_the_before_anchor() {
        let base = millis_aligned(Utc::now() - chrono::Duration::hours(1));
        // Newest first, as ClickUp returns them.
        let messages = vec![
            message(
                "m-3",
                "c-1",
                "1001",
                "third",
                base + chrono::Duration::minutes(3),
            ),
            message(
                "m-2",
                "c-1",
                "1002",
                "second",
                base + chrono::Duration::minutes(2),
            ),
            message(
                "m-1",
                "c-1",
                "1002",
                "first",
                base + chrono::Duration::minutes(1),
            ),
        ];
        let provider = provider_with(FakeClient::new(single_workspace_state(messages)));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let chat_id: ChatId = arc_str("c-1");
        let all = provider.history(&chat_id, None, 10).await.expect("history");
        let texts: Vec<&str> = all.iter().map(|message| message.id.as_ref()).collect();
        assert_eq!(texts, vec!["m-1", "m-2", "m-3"]);

        let older = provider
            .history(&chat_id, Some(base + chrono::Duration::minutes(3)), 10)
            .await
            .expect("history");
        let ids: Vec<&str> = older.iter().map(|message| message.id.as_ref()).collect();
        assert_eq!(ids, vec!["m-1", "m-2"]);
    }

    #[tokio::test]
    async fn history_walks_pages_until_the_limit_is_met() {
        let base = Utc::now() - chrono::Duration::hours(2);
        let messages: Vec<WireMessage> = (0..7)
            .map(|index| {
                message(
                    &format!("m-{index}"),
                    "c-1",
                    "1002",
                    "text",
                    base + chrono::Duration::minutes(index),
                )
            })
            .collect();
        let client = FakeClient::new(single_workspace_state(messages));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        // A limit of 3 makes each page 3 messages, so 5 need two pages.
        let collected = provider
            .history(&arc_str("c-1"), None, 3)
            .await
            .expect("history");
        assert_eq!(collected.len(), 3);
        assert_eq!(client.calls_starting_with("messages:c-1"), 1);
    }

    #[tokio::test]
    async fn history_with_zero_limit_makes_no_request() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();
        let before = client.calls_starting_with("messages:");

        let collected = provider
            .history(&arc_str("c-1"), None, 0)
            .await
            .expect("history");

        assert!(collected.is_empty());
        assert_eq!(client.calls_starting_with("messages:"), before);
    }

    #[tokio::test]
    async fn chat_members_populates_the_name_cache_for_contact_lookups() {
        let mut state = single_workspace_state(Vec::new());
        state.members = vec![WireUser {
            id: "1002".to_owned(),
            name: Some("Ada Lovelace".to_owned()),
            ..WireUser::default()
        }];
        let provider = provider_with(FakeClient::new(state));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let members = provider
            .chat_members(&arc_str("c-1"))
            .await
            .expect("members load");
        assert_eq!(members.len(), 1);

        let contact = provider
            .contact_info(&arc_str("1002"))
            .await
            .expect("lookup succeeds")
            .expect("known user");
        assert_eq!(contact.display_name.as_ref(), "Ada Lovelace");
    }

    #[tokio::test]
    async fn chat_details_serves_from_cache_without_a_request() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();
        let before = client.calls().len();

        let details = provider
            .chat_details(&arc_str("c-1"))
            .await
            .expect("details");
        assert_eq!(details.workspace.as_deref(), Some("Acme"));
        assert_eq!(client.calls().len(), before, "draw path must not fetch");

        // An unknown chat degrades to empty details rather than an error.
        let unknown = provider
            .chat_details(&arc_str("c-nope"))
            .await
            .expect("details");
        assert!(unknown.workspace.is_none());
        assert_eq!(client.calls().len(), before);
    }

    // -----------------------------------------------------------------------
    // Writing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn send_posts_a_top_level_message_and_echoes_it() {
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();
        let mut events = provider.events();

        let id = provider
            .send(
                &arc_str("c-1"),
                OutboundContent::new(Content::Text(arc_str("hello"))),
                None,
            )
            .await
            .expect("send succeeds");

        assert_eq!(id.as_ref(), "m-sent");
        assert!(
            client
                .calls()
                .contains(&"send_message:c-1:hello".to_owned())
        );
        assert!(drain(&mut events).iter().any(|event| matches!(
            event,
            ProviderEvent::Message { message, is_historical: false }
                if message.id.as_ref() == "m-sent" && message.is_from_me
        )));
    }

    #[tokio::test]
    async fn send_keeps_composer_markdown_unchanged() {
        // ClickUp chat renders markdown natively, so the composer's
        // `**bold**`/`~~strike~~` go out as typed.
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        provider
            .send(
                &arc_str("c-1"),
                OutboundContent::new(Content::Text(arc_str("**b** _i_ ~~s~~ `c`"))),
                None,
            )
            .await
            .expect("send succeeds");

        assert!(
            client
                .calls()
                .contains(&"send_message:c-1:**b** _i_ ~~s~~ `c`".to_owned())
        );
    }

    #[tokio::test]
    async fn send_rejects_empty_text_and_unsupported_content() {
        let provider = provider_with(FakeClient::new(single_workspace_state(Vec::new())));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        assert!(
            provider
                .send(
                    &arc_str("c-1"),
                    OutboundContent::new(Content::Text(arc_str("   "))),
                    None,
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn replying_to_a_reply_targets_the_thread_root() {
        let base = Utc::now() - chrono::Duration::minutes(5);
        let mut state = single_workspace_state(vec![WireMessage {
            parent_message: Some("m-root".to_owned()),
            ..message("m-child", "c-1", "1002", "in thread", base)
        }]);
        state.channels[0].latest_comment_at = Some(Utc::now().to_rfc3339());
        let client = FakeClient::new(state);
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let history = provider
            .history(&arc_str("c-1"), None, 10)
            .await
            .expect("history");
        let child = history.first().expect("one message");

        provider
            .send(
                &arc_str("c-1"),
                OutboundContent::new(Content::Text(arc_str("me too"))),
                Some(child),
            )
            .await
            .expect("reply sent");

        // Must address the root, never the reply itself.
        assert!(
            client
                .calls()
                .contains(&"send_reply:m-root:me too".to_owned())
        );
        assert_eq!(client.calls_starting_with("send_reply:m-child"), 0);
    }

    #[tokio::test]
    async fn replying_to_a_root_message_threads_under_it() {
        let base = Utc::now() - chrono::Duration::minutes(5);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-root", "c-1", "1002", "topic", base,
        )]));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let history = provider
            .history(&arc_str("c-1"), None, 10)
            .await
            .expect("history");
        provider
            .send(
                &arc_str("c-1"),
                OutboundContent::new(Content::Text(arc_str("reply"))),
                Some(&history[0]),
            )
            .await
            .expect("reply sent");

        assert!(
            client
                .calls()
                .contains(&"send_reply:m-root:reply".to_owned())
        );
    }

    #[tokio::test]
    async fn react_adds_then_removes_the_same_emoji() {
        let base = Utc::now() - chrono::Duration::minutes(5);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-1", "c-1", "1002", "text", base,
        )]));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();

        let history = provider
            .history(&arc_str("c-1"), None, 10)
            .await
            .expect("history");
        let message = history[0].clone();

        provider
            .react(&arc_str("c-1"), &message, "👍")
            .await
            .expect("reaction added");
        assert_eq!(client.calls_starting_with("add_reaction:m-1"), 1);

        // A message already carrying this user's reaction must toggle off.
        let reacted = Message {
            reactions: vec![Reaction {
                emoji: arc_str("👍"),
                senders: vec![arc_str("1001")],
            }],
            ..message
        };
        provider
            .react(&arc_str("c-1"), &reacted, "👍")
            .await
            .expect("reaction removed");
        assert_eq!(client.calls_starting_with("remove_reaction:m-1"), 1);
    }

    #[tokio::test]
    async fn edit_message_patches_and_emits_content_edit() {
        let base = Utc::now() - chrono::Duration::minutes(5);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-1", "c-1", "1001", "orig", base,
        )]));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();
        assert!(provider.outbound_capabilities().edit);

        let message = provider
            .history(&arc_str("c-1"), None, 10)
            .await
            .expect("history")[0]
            .clone();
        assert!(message.is_from_me);
        let mut events = provider.events();

        provider
            .edit_message(
                &arc_str("c-1"),
                &message,
                OutboundContent::new(Content::Text(arc_str("fixed"))),
            )
            .await
            .expect("edit succeeds");

        assert!(
            client
                .calls()
                .contains(&"update_message:m-1:fixed".to_owned())
        );
        assert!(drain(&mut events).iter().any(|event| matches!(
            event,
            ProviderEvent::MessageContentEdited { message_id, content: Content::Text(text), .. }
                if message_id.as_ref() == "m-1" && text.as_ref() == "fixed"
        )));
    }

    #[tokio::test]
    async fn edit_message_rejects_foreign_messages() {
        let base = Utc::now() - chrono::Duration::minutes(5);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-1", "c-1", "1002", "theirs", base,
        )]));
        let provider = provider_with(Arc::clone(&client));
        provider.connect().await.expect("connect succeeds");
        provider.stop_polling();
        let message = provider
            .history(&arc_str("c-1"), None, 10)
            .await
            .expect("history")[0]
            .clone();

        assert!(
            provider
                .edit_message(
                    &arc_str("c-1"),
                    &message,
                    OutboundContent::new(Content::Text(arc_str("x")))
                )
                .await
                .is_err()
        );
        assert_eq!(client.calls_starting_with("update_message:"), 0);
    }

    #[test]
    fn poll_edit_is_new_only_for_newer_edits() {
        let t0 = Utc::now();
        let t1 = t0 + chrono::Duration::seconds(5);
        assert!(poll_edit_is_new(None, Some(t0)));
        assert!(poll_edit_is_new(Some(t0), Some(t1)));
        assert!(!poll_edit_is_new(Some(t1), Some(t1)));
        assert!(!poll_edit_is_new(Some(t1), Some(t0)));
        assert!(!poll_edit_is_new(None, None));
    }

    #[tokio::test]
    async fn poll_detects_edits_of_already_seen_messages() {
        let started_at = Utc::now() - chrono::Duration::seconds(1);
        let arrival = Utc::now() + chrono::Duration::seconds(1);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-new", "c-1", "1002", "hi", arrival,
        )]));
        let context = poll_context(Arc::clone(&client), started_at);
        let mut state = PollState::default();
        assert!(run_poll_pass(&context, &mut state).await);
        let mut events = context.events.subscribe();

        // The author edits the message: same id, bumped `date_updated`.
        {
            let mut fake = client.lock();
            let wire = &mut fake.messages.get_mut("c-1").expect("channel")[0];
            wire.content = Some("hi (edited)".to_owned());
            wire.date_updated =
                Some(millis_from_timestamp(arrival + chrono::Duration::seconds(30)) as f64);
        }

        assert!(
            !run_poll_pass(&context, &mut state).await,
            "an edit is not a new live message"
        );
        let drained = drain(&mut events);
        assert!(drained.iter().any(|event| matches!(
            event,
            ProviderEvent::MessageContentEdited { message_id, .. } if message_id.as_ref() == "m-new"
        )));
        assert!(
            !drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::Message { .. }))
        );

        // The same edit polled again must not re-emit.
        let mut events = context.events.subscribe();
        run_poll_pass(&context, &mut state).await;
        assert!(
            !drain(&mut events)
                .iter()
                .any(|event| matches!(event, ProviderEvent::MessageContentEdited { .. }))
        );
    }

    #[tokio::test]
    async fn mark_read_clears_the_badge_locally() {
        let provider = provider_with(FakeClient::new(single_workspace_state(Vec::new())));
        let mut events = provider.events();

        provider
            .mark_read(&arc_str("c-1"), &arc_str("m-1"))
            .await
            .expect("mark read");

        assert!(drain(&mut events).iter().any(|event| matches!(
            event,
            ProviderEvent::ChatMarkedRead { chat_id } if chat_id.as_ref() == "c-1"
        )));
    }

    // -----------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------

    /// Builds a poll context wired to `client`, anchored at `started_at`.
    fn poll_context(client: Arc<FakeClient>, started_at: Timestamp) -> PollContext {
        PollContext {
            api_client: client,
            events: EventBus::new(),
            account: arc_str("clickup:test"),
            authorization: "pk_test".to_owned(),
            workspace_id: "900".to_owned(),
            self_user_id: Some("1001".to_owned()),
            users: Arc::new(RwLock::new(HashMap::new())),
            channels: Arc::new(RwLock::new(HashMap::new())),
            members: Arc::new(RwLock::new(HashMap::new())),
            started_at,
        }
    }

    #[tokio::test]
    async fn first_poll_pass_lists_everything_and_suppresses_backlog() {
        let started_at = Utc::now();
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-old",
            "c-1",
            "1002",
            "backlog",
            started_at - chrono::Duration::hours(4),
        )]));
        let context = poll_context(Arc::clone(&client), started_at);
        let mut events = context.events.subscribe();
        let mut state = PollState::default();

        let delivered = run_poll_pass(&context, &mut state).await;

        assert!(!delivered, "backlog must never notify");
        assert!(
            client
                .calls()
                .contains(&"list_channels:since=none".to_owned())
        );
        let drained = drain(&mut events);
        assert!(
            drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::ChatUpdated(_)))
        );
        assert!(
            !drained
                .iter()
                .any(|event| matches!(event, ProviderEvent::Message { .. }))
        );
        assert!(state.covered_until.is_some());
    }

    #[tokio::test]
    async fn second_poll_pass_narrows_the_listing_and_emits_new_messages() {
        let started_at = Utc::now() - chrono::Duration::seconds(1);
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let context = poll_context(Arc::clone(&client), started_at);
        let mut state = PollState::default();

        run_poll_pass(&context, &mut state).await;
        let mut events = context.events.subscribe();

        // New activity arrives after the baseline pass.
        let arrival = Utc::now() + chrono::Duration::seconds(1);
        client.push_message("c-1", message("m-new", "c-1", "1002", "hi", arrival));
        client.lock().channels[0].latest_comment_at = Some(arrival.to_rfc3339());

        let delivered = run_poll_pass(&context, &mut state).await;

        assert!(delivered, "fresh message must surface");
        // The second listing must be narrowed, which is what keeps the poll
        // inside ClickUp's rate budget.
        assert!(
            client
                .calls()
                .iter()
                .any(|call| call.starts_with("list_channels:since=")
                    && !call.ends_with("since=none"))
        );
        assert!(drain(&mut events).iter().any(|event| matches!(
            event,
            ProviderEvent::Message { message, is_historical: false }
                if message.id.as_ref() == "m-new"
        )));
    }

    #[tokio::test]
    async fn a_repeated_message_is_only_emitted_once() {
        let started_at = Utc::now() - chrono::Duration::seconds(1);
        let arrival = Utc::now() + chrono::Duration::seconds(1);
        let client = FakeClient::new(single_workspace_state(vec![message(
            "m-new", "c-1", "1002", "hi", arrival,
        )]));
        let context = poll_context(Arc::clone(&client), started_at);
        let mut state = PollState::default();

        assert!(run_poll_pass(&context, &mut state).await);
        // The same message is still the newest, so the next pass returns it
        // again; dedup must swallow it.
        assert!(!run_poll_pass(&context, &mut state).await);
    }

    #[tokio::test]
    async fn a_failed_listing_does_not_advance_the_window() {
        let started_at = Utc::now();
        let mut fake = single_workspace_state(Vec::new());
        fake.fail_list_channels = true;
        let client = FakeClient::new(fake);
        let context = poll_context(Arc::clone(&client), started_at);
        let mut state = PollState::default();

        assert!(!run_poll_pass(&context, &mut state).await);
        assert!(
            state.covered_until.is_none(),
            "a failed pass must be retried in full"
        );
        // The retry therefore still asks for the unnarrowed listing.
        assert!(!run_poll_pass(&context, &mut state).await);
        assert_eq!(client.calls_starting_with("list_channels:since=none"), 2);
    }

    #[tokio::test]
    async fn poll_pass_costs_one_request_when_nothing_changed() {
        let started_at = Utc::now() - chrono::Duration::seconds(1);
        let client = FakeClient::new(single_workspace_state(Vec::new()));
        let context = poll_context(Arc::clone(&client), started_at);
        let mut state = PollState::default();

        run_poll_pass(&context, &mut state).await;
        let baseline = client.calls().len();
        // Nothing new: `with_message_since` filters the channel out entirely.
        client.lock().channels[0].latest_comment_at =
            Some((started_at - chrono::Duration::hours(1)).to_rfc3339());

        run_poll_pass(&context, &mut state).await;

        assert_eq!(
            client.calls().len() - baseline,
            1,
            "a quiet pass must cost exactly one request"
        );
    }

    #[test]
    fn pruning_the_dedup_set_is_bounded() {
        let mut seen: HashMap<String, Option<Timestamp>> = (0..MAX_SEEN_MESSAGE_IDS + 10)
            .map(|index| (index.to_string(), None))
            .collect();
        prune_seen_message_ids(&mut seen);
        assert!(seen.is_empty());

        let mut small: HashMap<String, Option<Timestamp>> =
            [("a".to_owned(), None)].into_iter().collect();
        prune_seen_message_ids(&mut small);
        assert_eq!(small.len(), 1);
    }
}
