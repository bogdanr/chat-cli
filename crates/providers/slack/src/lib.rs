#![allow(
    clippy::collapsible_if,
    clippy::default_constructed_unit_structs,
    clippy::let_and_return,
    clippy::redundant_closure,
    clippy::too_many_arguments,
    clippy::type_complexity
)]

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use chat_core::{
    Account, AccountNoticeSeverity, AuthChallenge, AuthSubmission, AuthSubmissionMode, Card,
    CardAction, CardColor, CardField, CardKind, CardSource, Chat, ChatDetails, ChatId, ChatKind,
    ChatMember, ChatMembership, ContactProfile, Content, DiscoveryAction, DiscoveryCapabilities,
    DiscoveryResult, DiscoveryResultKind, EventBus, Media, Mention, Message, MessageId,
    NetworkActivityDirection, NetworkActivityKind, OutboundCapabilities, OutboundContent,
    OutboundMentions, Platform, PlatformData, PlatformId, Provider, ProviderEvent, ProviderId,
    Reaction, Sender, SlackData, Timestamp, resolve_mention_tokens, rewrite_mention_tokens,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    fmt, fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message as WebSocketMessage};

const PROVIDER_ID: &str = "slack:setup";
const PROVIDER_ID_PREFIX: &str = "slack";
const SLACK_CONVERSATION_TYPES: &str = "public_channel,private_channel,mpim,im";
const SLACK_HTTP_TIMEOUT: Duration = Duration::from_secs(12);
// Minimum spacing between Slack Web API requests. Slack rate-limits
// `conversations.history`/`conversations.list` per method, so issuing one call
// per conversation every few seconds trips HTTP 429s. Those 429s previously
// surfaced as opaque transport errors and were retried into a storm. Pacing
// every request keeps the aggregate rate under Slack's limits.
const SLACK_MIN_REQUEST_SPACING: Duration = Duration::from_millis(1100);
// How many times a rate-limited (HTTP 429) request is retried, honoring the
// server's `Retry-After`, before giving up for this pass.
const SLACK_RATE_LIMIT_MAX_RETRIES: usize = 4;
// Backoff used when a 429 response omits a usable `Retry-After` header.
const SLACK_RATE_LIMIT_DEFAULT_BACKOFF: Duration = Duration::from_secs(5);
// Files at or below this size are cached eagerly in the background when a
// message referencing them is rendered. Larger uploads are only fetched when
// the user explicitly retrieves them from the media card, so a single huge
// attachment cannot saturate bandwidth or blow up the cache unprompted.
const SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES: u64 = chat_core::MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES;
// Safety ceiling for explicit on-demand downloads so a malformed response
// cannot fill the disk; generous enough for any realistic chat attachment.
const SLACK_MEDIA_ON_DEMAND_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
// After a background media download fails, do not re-queue the same URL until
// this cooldown elapses. Prevents render-driven retry storms for URLs that
// fail deterministically. Explicit on-demand retrieval bypasses the cooldown.
const SLACK_MEDIA_FAILURE_RETRY_COOLDOWN: Duration = Duration::from_secs(300);
// Slack OAuth advertises the HTTPS relay URL by default because Slack matches
// redirect URIs exactly and distributed apps cannot register insecure loopback
// redirects. The relay forwards the browser to the fixed local listener port
// below, which is where chat-cli receives the OAuth code.
const SLACK_OAUTH_REDIRECT_PORT: u16 = 41419;
const SLACK_OAUTH_REDIRECT_URI: &str = "https://chat-cli.vpn.cafe/slack/oauth/callback";
// How long the loopback listener waits for the browser to complete the OAuth
// redirect before giving up.
const SLACK_OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const SLACK_SOCKET_MODE_IDLE_DIAGNOSTIC_AFTER: Duration = Duration::from_secs(60);
const SLACK_HISTORY_POLL_INTERVAL: Duration = Duration::from_secs(20);
// Direct/group-direct conversations are polled on a tighter interval than the
// channel history fallback. Socket Mode never delivers a user's own
// human-to-human DMs (realtime events are bot-scoped), so this user-token poll
// is the only path that surfaces them; keeping it brisk avoids noticeable DM
// latency. The set of im/mpim conversations is tiny, so the extra calls are
// cheap, and channels stay instant on realtime.
const SLACK_DM_POLL_INTERVAL: Duration = Duration::from_secs(8);
// Fetch a small window (not just the single newest message) per conversation
// each poll so a burst of messages arriving between polls is not collapsed to
// only the last one. The dedup set plus the `started_at` timestamp gate in
// `run_history_poll_loop` keep this safe: already-seen or pre-startup messages
// are never re-surfaced as live, so a larger window only improves catch-up.
const SLACK_HISTORY_POLL_LIMIT: usize = 15;
const PERF_LOG_FILE_ENV: &str = "CHAT_CLI_PERF_LOG_FILE";

// Official distributed chat-cli Slack app credentials. Distributors bake these
// in at build time via environment variables (read by `option_env!`), and
// operators can override them at runtime via the same variable names. When a
// client ID and secret are available, the default "Connect Slack workspace"
// path uses them directly so users never create their own Slack app. Open
// builds typically leave them unset and fall back to manual app setup.
const SLACK_OFFICIAL_CLIENT_ID_ENV: &str = "CHAT_CLI_SLACK_CLIENT_ID";
const SLACK_OFFICIAL_CLIENT_SECRET_ENV: &str = "CHAT_CLI_SLACK_CLIENT_SECRET";
const SLACK_OFFICIAL_REDIRECT_URI_ENV: &str = "CHAT_CLI_SLACK_REDIRECT_URI";
const BUNDLED_SLACK_CLIENT_ID: Option<&str> = option_env!("CHAT_CLI_SLACK_CLIENT_ID");
const BUNDLED_SLACK_CLIENT_SECRET: Option<&str> = option_env!("CHAT_CLI_SLACK_CLIENT_SECRET");
const BUNDLED_SLACK_REDIRECT_URI: Option<&str> = option_env!("CHAT_CLI_SLACK_REDIRECT_URI");

/// Credentials for the official distributed chat-cli Slack app. When present,
/// the normal workspace-connect path can run browser OAuth without asking the
/// user to create a Slack app or paste a client ID/secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfficialSlackApp {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

/// Resolve the official chat-cli Slack app credentials, if configured. Runtime
/// environment variables take precedence over compile-time bundled values so a
/// build can be overridden without recompiling. Both a client ID and secret are
/// required (the `oauth.v2.access` exchange needs the secret); otherwise this
/// returns `None` and callers fall back to the manual app-creation flow.
pub fn official_slack_app() -> Option<OfficialSlackApp> {
    resolve_official_slack_app(
        resolved_official_value(SLACK_OFFICIAL_CLIENT_ID_ENV, BUNDLED_SLACK_CLIENT_ID),
        resolved_official_value(
            SLACK_OFFICIAL_CLIENT_SECRET_ENV,
            BUNDLED_SLACK_CLIENT_SECRET,
        ),
        resolved_official_value(SLACK_OFFICIAL_REDIRECT_URI_ENV, BUNDLED_SLACK_REDIRECT_URI),
    )
}

/// Pure resolution of official app credentials from already-resolved values.
/// Requires both a client ID and secret; the redirect URI defaults to the
/// HTTPS relay callback when not explicitly configured. Kept separate from env
/// reads so the precedence and required-field rules are unit-testable.
fn resolve_official_slack_app(
    client_id: Option<String>,
    client_secret: Option<String>,
    redirect_uri: Option<String>,
) -> Option<OfficialSlackApp> {
    Some(OfficialSlackApp {
        client_id: client_id?,
        client_secret: client_secret?,
        redirect_uri: redirect_uri.unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned()),
    })
}

/// True when an official Slack app is bundled/configured, meaning the normal
/// connect path can skip manual app creation and client ID/secret entry.
pub fn official_slack_app_is_configured() -> bool {
    official_slack_app().is_some()
}

fn resolved_official_value(env_key: &str, bundled: Option<&str>) -> Option<String> {
    std::env::var(env_key)
        .ok()
        .and_then(non_empty_string)
        .or_else(|| bundled.and_then(|value| non_empty_string(value.to_owned())))
}

fn slack_diagnostic_log(label: &str, details: impl AsRef<str>) {
    let Some(path) = std::env::var_os(PERF_LOG_FILE_ENV).map(PathBuf::from) else {
        return;
    };
    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} label={} {}", timestamp, label, details.as_ref());
    }
}

pub struct SlackProvider {
    id: ProviderId,
    account: RwLock<Account>,
    options: RwLock<SlackProviderOptions>,
    capabilities: RwLock<SlackCapabilities>,
    connection: RwLock<SlackConnectionState>,
    chats: RwLock<Vec<Chat>>,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
    chat_members: Arc<RwLock<HashMap<String, Vec<String>>>>,
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    connected: AtomicBool,
    realtime_task: RwLock<Option<JoinHandle<()>>>,
    history_poll_task: RwLock<Option<JoinHandle<()>>>,
    dm_poll_task: RwLock<Option<JoinHandle<()>>>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct SlackProviderOptions {
    pub auth_mode: SlackAuthMode,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub bot_token: Option<String>,
    pub app_token: Option<String>,
    pub user_token: Option<String>,
    pub webhook_url: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum SlackAuthMode {
    UserOAuth,
    ReadOnlyOAuth,
    BotToken,
    ImportedToken,
    ManualApp,
    Webhook,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlackCapabilities {
    pub can_read_history: bool,
    pub can_send_as_user: bool,
    pub can_send_as_bot: bool,
    pub can_send_webhook: bool,
    pub can_react: bool,
    pub can_download_files: bool,
    pub can_realtime: bool,
    pub can_search: bool,
    pub requires_admin_approval: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackSendIdentity {
    User,
    Bot,
    Webhook,
    None,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SlackCredentialKind {
    UserToken,
    BotToken,
    AppToken,
    Webhook,
    Unknown,
}

#[derive(Clone)]
pub struct SlackCredential {
    pub kind: SlackCredentialKind,
    value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackValidatedCredential {
    pub kind: SlackCredentialKind,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub bot_id: Option<String>,
}

impl SlackValidatedCredential {
    pub fn user(team_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            kind: SlackCredentialKind::UserToken,
            team_id: Some(team_id.into()),
            team_name: None,
            user_id: Some(user_id.into()),
            user_name: None,
            bot_id: None,
        }
    }

    pub fn bot(team_id: impl Into<String>, bot_id: impl Into<String>) -> Self {
        Self {
            kind: SlackCredentialKind::BotToken,
            team_id: Some(team_id.into()),
            team_name: None,
            user_id: None,
            user_name: None,
            bot_id: Some(bot_id.into()),
        }
    }

    pub fn app() -> Self {
        Self {
            kind: SlackCredentialKind::AppToken,
            team_id: None,
            team_name: None,
            user_id: None,
            user_name: None,
            bot_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackWebhookValidation {
    pub url_host: String,
}

#[derive(Clone, Debug, Default)]
struct SlackConnectionState {
    user_token: Option<String>,
    bot_token: Option<String>,
    app_token: Option<String>,
    webhook_url: Option<String>,
    team_id: Option<String>,
    team_name: Option<String>,
    team_icon_url: Option<String>,
    user_id: Option<String>,
    bot_id: Option<String>,
}

#[derive(Clone, Debug)]
struct SlackValidatedConnection {
    capabilities: SlackCapabilities,
    connection: SlackConnectionState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlackUser {
    pub id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
    pub deleted: bool,
    pub is_bot: bool,
    pub title: Option<String>,
    pub status_text: Option<String>,
    pub status_emoji: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub tz: Option<String>,
    pub tz_label: Option<String>,
    pub tz_offset: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackPostedMessage {
    pub channel: Option<String>,
    pub ts: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackUploadedFile {
    pub id: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SlackUploadFileRequest {
    pub channel: String,
    pub path: PathBuf,
    pub filename: String,
    pub title: String,
    pub mime_type: String,
    pub initial_comment: Option<String>,
    pub thread_ts: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackConversation {
    pub id: String,
    pub name: Option<String>,
    pub user: Option<String>,
    pub is_channel: bool,
    pub is_group: bool,
    pub is_im: bool,
    pub is_mpim: bool,
    pub is_member: Option<bool>,
    pub is_private: bool,
    pub is_archived: bool,
    pub is_ext_shared: bool,
    pub is_muted: bool,
    pub is_pinned: bool,
    pub unread_count: u32,
    pub updated: Option<i64>,
    pub created: Option<i64>,
    pub creator: Option<String>,
    pub topic: Option<String>,
    pub purpose: Option<String>,
    pub num_members: Option<u32>,
}

#[derive(Debug, Serialize)]
struct SlackPostMessageRequest<'a> {
    channel: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct SlackPostMessageResponse {
    ok: bool,
    error: Option<String>,
    channel: Option<String>,
    ts: Option<String>,
}

#[derive(Debug, Serialize)]
struct SlackGetUploadUrlRequest<'a> {
    filename: &'a str,
    length: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    alt_txt: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct SlackGetUploadUrlResponse {
    ok: bool,
    error: Option<String>,
    upload_url: Option<String>,
    file_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SlackCompleteUploadRequest<'a> {
    files: Vec<SlackCompleteUploadFile<'a>>,
    channel_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_comment: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct SlackCompleteUploadFile<'a> {
    id: &'a str,
    title: &'a str,
}

#[derive(Debug, Deserialize)]
struct SlackCompleteUploadResponse {
    ok: bool,
    error: Option<String>,
    files: Option<Vec<SlackCompleteUploadFileResponse>>,
}

#[derive(Debug, Deserialize)]
struct SlackCompleteUploadFileResponse {
    id: Option<String>,
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct SlackWebhookPostRequest<'a> {
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct SlackConversationsListResponse {
    ok: bool,
    error: Option<String>,
    // Slack omits `channels` on error responses, so default it to let the
    // `ok`/`error` check surface the real cause instead of an opaque decode
    // failure.
    #[serde(default)]
    channels: Vec<SlackConversationResponse>,
    #[serde(default)]
    response_metadata: SlackResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SlackResponseMetadata {
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackConversationInfoResponse {
    ok: bool,
    error: Option<String>,
    channel: Option<SlackConversationResponse>,
}

#[derive(Debug, Deserialize)]
struct SlackConversationResponse {
    id: String,
    name: Option<String>,
    user: Option<String>,
    is_channel: Option<bool>,
    is_group: Option<bool>,
    is_im: Option<bool>,
    is_mpim: Option<bool>,
    is_member: Option<bool>,
    is_private: Option<bool>,
    is_archived: Option<bool>,
    is_ext_shared: Option<bool>,
    is_muted: Option<bool>,
    is_pinned: Option<bool>,
    unread_count: Option<u32>,
    unread_count_display: Option<u32>,
    updated: Option<i64>,
    created: Option<i64>,
    creator: Option<String>,
    topic: Option<SlackTextValue>,
    purpose: Option<SlackTextValue>,
    num_members: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SlackConversationsHistoryResponse {
    ok: bool,
    error: Option<String>,
    // Slack omits `messages` entirely on error responses (for example
    // `{"ok":false,"error":"missing_scope"}`). Without a default, serde fails
    // with "missing field `messages`", masking the real Slack error behind an
    // opaque decode failure. Defaulting lets the `ok`/`error` check below
    // surface the actual cause.
    //
    // Individual messages are decoded leniently: a single entry whose shape
    // does not match (an unexpected field type, a new subtype payload, etc.)
    // must not discard every other message in the conversation. Without this,
    // one odd message silently drops the whole history response, so genuine
    // messages never get stored or surfaced in the sidebar.
    #[serde(default, deserialize_with = "deserialize_lenient_slack_messages")]
    messages: Vec<SlackHistoryMessageResponse>,
    #[serde(default)]
    response_metadata: SlackResponseMetadata,
}

#[derive(Debug, Deserialize)]
struct SlackHistoryMessageResponse {
    #[serde(rename = "type")]
    message_type: Option<String>,
    subtype: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    username: Option<String>,
    icons: Option<SlackMessageIconsResponse>,
    bot_profile: Option<SlackBotProfileResponse>,
    ts: Option<String>,
    thread_ts: Option<String>,
    reply_count: Option<u32>,
    text: Option<String>,
    blocks: Option<Vec<serde_json::Value>>,
    attachments: Option<Vec<SlackAttachmentResponse>>,
    files: Option<Vec<SlackFileResponse>>,
    hidden: Option<bool>,
    reactions: Option<Vec<SlackReactionResponse>>,
    /// Present only when the author edited the message text.
    #[serde(default)]
    edited: Option<SlackEditedResponse>,
}

/// Slack's `edited` marker (`{"user": "U123", "ts": "1700000000.000100"}`).
/// Other `message_changed` causes (unfurls, thread metadata) omit it, so it
/// is the reliable signal that the text itself was edited.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackEditedResponse {
    #[serde(default)]
    ts: Option<String>,
}

fn slack_edited_at(edited: Option<&SlackEditedResponse>) -> Option<Timestamp> {
    edited
        .and_then(|edited| non_empty_option(&edited.ts))
        .and_then(|ts| slack_ts_to_timestamp(&ts))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackFileResponse {
    id: Option<String>,
    name: Option<String>,
    title: Option<String>,
    mimetype: Option<String>,
    filetype: Option<String>,
    size: Option<u64>,
    url_private: Option<String>,
    url_private_download: Option<String>,
    thumb_360: Option<String>,
    thumb_720: Option<String>,
    thumb_1024: Option<String>,
    permalink: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

/// Decodes the `messages` array of a `conversations.history`/`conversations.replies`
/// response one entry at a time, skipping (and logging) any message that fails to
/// deserialize. Slack occasionally returns messages with field shapes we do not
/// model yet; decoding the array strictly would fail the entire response and drop
/// every valid message in the conversation, so the history poll would never
/// surface them in the sidebar or notify on them.
fn deserialize_lenient_slack_messages<'de, D>(
    deserializer: D,
) -> Result<Vec<SlackHistoryMessageResponse>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    let mut messages = Vec::with_capacity(raw.len());
    for value in raw {
        match serde_json::from_value::<SlackHistoryMessageResponse>(value) {
            Ok(message) => messages.push(message),
            Err(error) => {
                slack_diagnostic_log("slack.history.message_decode_skipped", error.to_string())
            }
        }
    }
    Ok(messages)
}

fn deserialize_optional_slack_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(value)) => non_empty_string(value),
        Some(serde_json::Value::Number(value)) => Some(value.to_string()),
        Some(serde_json::Value::Bool(value)) => Some(value.to_string()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    })
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackMessageIconsResponse {
    image_36: Option<String>,
    image_48: Option<String>,
    image_72: Option<String>,
    image_original: Option<String>,
    icon_url: Option<String>,
    emoji: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackBotProfileResponse {
    id: Option<String>,
    name: Option<String>,
    real_name: Option<String>,
    icons: Option<SlackMessageIconsResponse>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SlackMessageSenderMetadata {
    display_name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackAttachmentResponse {
    pretext: Option<String>,
    title: Option<String>,
    title_link: Option<String>,
    text: Option<String>,
    fallback: Option<String>,
    color: Option<String>,
    image_url: Option<String>,
    thumb_url: Option<String>,
    author_name: Option<String>,
    author_link: Option<String>,
    footer: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_slack_string")]
    ts: Option<String>,
    fields: Option<Vec<SlackAttachmentFieldResponse>>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
struct SlackAttachmentFieldResponse {
    title: Option<String>,
    value: Option<String>,
    short: Option<bool>,
}

/// A single Block Kit layout block from a message's `blocks` array. Every
/// field is optional so unfamiliar block shapes still decode; blocks whose
/// `type` we do not model are skipped (and logged) during card conversion.
/// Free-form parts (`elements`, `accessory`) stay as raw JSON values because
/// their shape varies per block type.
#[derive(Clone, Debug, Default, Deserialize)]
struct SlackBlockResponse {
    #[serde(rename = "type")]
    block_type: Option<String>,
    text: Option<SlackBlockTextResponse>,
    fields: Option<Vec<SlackBlockTextResponse>>,
    elements: Option<Vec<serde_json::Value>>,
    accessory: Option<serde_json::Value>,
    image_url: Option<String>,
    alt_text: Option<String>,
    title: Option<SlackBlockTextResponse>,
}

/// A Block Kit text object (`mrkdwn` or `plain_text`).
#[derive(Clone, Debug, Default, Deserialize)]
struct SlackBlockTextResponse {
    #[serde(rename = "type")]
    text_type: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackReactionResponse {
    name: Option<String>,
    users: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SlackReactionsApiResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackTextValue {
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackUsersInfoResponse {
    ok: bool,
    error: Option<String>,
    user: Option<SlackUserResponse>,
}

#[derive(Debug, Deserialize)]
struct SlackConversationsMembersResponse {
    ok: bool,
    error: Option<String>,
    members: Vec<String>,
    #[serde(default)]
    response_metadata: SlackResponseMetadata,
}

#[derive(Debug, Deserialize)]
struct SlackUserResponse {
    id: String,
    name: Option<String>,
    real_name: Option<String>,
    profile: Option<SlackUserProfileResponse>,
    deleted: Option<bool>,
    is_bot: Option<bool>,
    tz: Option<String>,
    tz_label: Option<String>,
    tz_offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SlackUserProfileResponse {
    real_name: Option<String>,
    display_name: Option<String>,
    image_72: Option<String>,
    title: Option<String>,
    status_text: Option<String>,
    status_emoji: Option<String>,
    phone: Option<String>,
    email: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SlackTeamInfo {
    pub id: Option<String>,
    pub name: Option<String>,
    pub domain: Option<String>,
    pub email_domain: Option<String>,
    pub icon_url: Option<String>,
}

impl SlackTeamInfo {
    fn from_response(team: SlackTeamInfoTeam) -> Self {
        let icon_url = team.icon.and_then(SlackTeamIconResponse::best_image_url);
        Self {
            id: team.id.and_then(non_empty_string),
            name: team.name.and_then(non_empty_string),
            domain: team.domain.and_then(non_empty_string),
            email_domain: team.email_domain.and_then(non_empty_string),
            icon_url,
        }
    }

    /// Best human-readable workspace name, preferring the display name and
    /// falling back to the workspace domain.
    fn display_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.domain.as_deref())
            .or(self.email_domain.as_deref())
    }
}

#[derive(Debug, Deserialize)]
struct SlackTeamInfoResponse {
    ok: bool,
    error: Option<String>,
    team: Option<SlackTeamInfoTeam>,
}

#[derive(Debug, Deserialize)]
struct SlackTeamInfoTeam {
    id: Option<String>,
    name: Option<String>,
    domain: Option<String>,
    email_domain: Option<String>,
    icon: Option<SlackTeamIconResponse>,
}

#[derive(Debug, Deserialize)]
struct SlackTeamIconResponse {
    image_34: Option<String>,
    image_44: Option<String>,
    image_68: Option<String>,
    image_88: Option<String>,
    image_102: Option<String>,
    image_132: Option<String>,
    image_230: Option<String>,
    image_original: Option<String>,
}

impl SlackTeamIconResponse {
    fn best_image_url(self) -> Option<String> {
        self.image_original
            .or(self.image_230)
            .or(self.image_132)
            .or(self.image_102)
            .or(self.image_88)
            .or(self.image_68)
            .or(self.image_44)
            .or(self.image_34)
            .and_then(non_empty_string)
    }
}

impl SlackUser {
    fn from_response(response: SlackUserResponse) -> Option<Self> {
        let id = non_empty_string(response.id)?;
        let profile = response.profile;
        let profile_real_name = profile
            .as_ref()
            .and_then(|profile| profile.real_name.clone())
            .and_then(non_empty_string);
        let display_name = profile
            .as_ref()
            .and_then(|profile| profile.display_name.clone())
            .and_then(non_empty_string);
        let title = profile
            .as_ref()
            .and_then(|profile| profile.title.clone())
            .and_then(non_empty_string);
        let status_text = profile
            .as_ref()
            .and_then(|profile| profile.status_text.clone())
            .and_then(non_empty_string);
        let status_emoji = profile
            .as_ref()
            .and_then(|profile| profile.status_emoji.clone())
            .and_then(non_empty_string);
        let phone = profile
            .as_ref()
            .and_then(|profile| profile.phone.clone())
            .and_then(non_empty_string);
        let email = profile
            .as_ref()
            .and_then(|profile| profile.email.clone())
            .and_then(non_empty_string);
        let avatar = profile
            .and_then(|profile| profile.image_72)
            .and_then(non_empty_string);

        Some(Self {
            id,
            name: response.name.and_then(non_empty_string),
            real_name: response
                .real_name
                .and_then(non_empty_string)
                .or(profile_real_name),
            display_name,
            avatar,
            deleted: response.deleted.unwrap_or(false),
            is_bot: response.is_bot.unwrap_or(false),
            title,
            status_text,
            status_emoji,
            phone,
            email,
            tz: response.tz.and_then(non_empty_string),
            tz_label: response.tz_label.and_then(non_empty_string),
            tz_offset: response.tz_offset,
        })
    }

    fn best_name(&self) -> &str {
        self.display_name
            .as_deref()
            .or(self.real_name.as_deref())
            .or(self.name.as_deref())
            .unwrap_or(&self.id)
    }

    fn sender(&self) -> Sender {
        Sender {
            platform_id: arc_str(&self.id),
            display_name: arc_str(self.best_name()),
            avatar: self.avatar.as_deref().and_then(slack_avatar_path),
        }
    }

    /// Build an enriched [`ContactProfile`] from the resolved Slack user,
    /// combining status emoji + text and computing the contact's local time
    /// from the reported timezone offset.
    fn profile(&self) -> ContactProfile {
        let status = match (self.status_emoji.as_deref(), self.status_text.as_deref()) {
            (Some(emoji), Some(text)) => Some(format!("{emoji} {text}")),
            (Some(emoji), None) => Some(emoji.to_owned()),
            (None, Some(text)) => Some(text.to_owned()),
            (None, None) => None,
        };
        let local_time = self.tz_offset.and_then(slack_local_time_for_offset);
        let timezone = self
            .tz_label
            .clone()
            .or_else(|| self.tz.clone())
            .map(|value| arc_str(&value));
        ContactProfile {
            display_name: Some(arc_str(self.best_name())),
            handle: self.name.as_deref().map(arc_str),
            title: self.title.as_deref().map(arc_str),
            status: status.map(|value| arc_str(&value)),
            about: None,
            phone: self.phone.as_deref().map(arc_str),
            email: self.email.as_deref().map(arc_str),
            timezone,
            local_time: local_time.map(|value| arc_str(&value)),
            is_bot: self.is_bot,
            is_business: false,
            is_deactivated: self.deleted,
            facts: Vec::new(),
        }
    }
}

#[async_trait]
pub trait SlackApiClient: Send + Sync {
    async fn validate_token(&self, credential: SlackCredential)
    -> Result<SlackValidatedCredential>;

    async fn validate_webhook(&self, webhook_url: &str) -> Result<SlackWebhookValidation>;

    async fn post_message(
        &self,
        credential: SlackCredential,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<SlackPostedMessage>;

    async fn post_webhook(&self, webhook_url: &str, text: &str) -> Result<SlackPostedMessage>;

    /// Replace the text of an existing message via `chat.update`. Slack only
    /// allows the identity that posted a message to edit it.
    async fn update_message(
        &self,
        credential: SlackCredential,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<SlackPostedMessage>;

    async fn upload_file(
        &self,
        credential: SlackCredential,
        request: SlackUploadFileRequest,
    ) -> Result<SlackUploadedFile>;

    async fn list_conversations(
        &self,
        credential: SlackCredential,
    ) -> Result<Vec<SlackConversation>>;

    /// Conversations the authenticated user belongs to, limited to `scope`.
    /// Sidebar loads and pollers only ever need these, so the real client
    /// uses `users.conversations` instead of paging every public channel in
    /// the workspace through `conversations.list` (slow at startup and the
    /// main source of `conversations.list` 429s). The default filters
    /// [`Self::list_conversations`] so fakes keep working unchanged.
    async fn list_member_conversations(
        &self,
        credential: SlackCredential,
        scope: SlackMemberScope,
    ) -> Result<Vec<SlackConversation>> {
        Ok(self
            .list_conversations(credential)
            .await?
            .into_iter()
            .filter(|conversation| scope.includes(conversation))
            .collect())
    }

    async fn user_info(
        &self,
        credential: SlackCredential,
        user_id: &str,
    ) -> Result<Option<SlackUser>>;

    async fn team_info(
        &self,
        credential: SlackCredential,
        team_id: Option<&str>,
    ) -> Result<Option<SlackTeamInfo>>;

    async fn conversation_members(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Vec<String>>;

    /// Fetch enriched metadata for a single conversation (topic, purpose,
    /// member count, created, creator). The default falls back to scanning
    /// `conversations.list`; the real Web API client overrides this with a
    /// direct `conversations.info` call so creation metadata is populated.
    async fn conversation_info(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Option<SlackConversation>> {
        let channel = channel.to_owned();
        Ok(self
            .list_conversations(credential)
            .await?
            .into_iter()
            .find(|conversation| conversation.id == channel))
    }

    async fn add_reaction(
        &self,
        credential: SlackCredential,
        channel: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<()>;

    async fn remove_reaction(
        &self,
        credential: SlackCredential,
        channel: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<()>;

    async fn history(
        &self,
        credential: SlackCredential,
        account: &ProviderId,
        current_user_id: Option<&str>,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
        users: Option<Arc<RwLock<HashMap<String, SlackUser>>>>,
    ) -> Result<Vec<Message>>;

    async fn open_socket_mode(
        &self,
        app_token: SlackCredential,
    ) -> Result<SlackSocketModeConnection>;

    /// Exchange an OAuth authorization `code` for Slack tokens via
    /// `oauth.v2.access`. Returns both bot and user tokens (whichever the app
    /// requested) plus the workspace identity and granted scopes.
    async fn exchange_oauth_code(
        &self,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
        code: &str,
    ) -> Result<SlackOAuthTokens>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackSocketModeConnection {
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct SlackSocketModeOpenResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
}

/// Tokens and workspace identity returned by a successful `oauth.v2.access`
/// exchange. A single OAuth install can yield both a bot token (top-level
/// `access_token`) and a user token (`authed_user.access_token`), along with
/// the granted scopes for each, which let the provider report accurate
/// capabilities without asking the user to paste anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlackOAuthTokens {
    pub user_token: Option<String>,
    pub bot_token: Option<String>,
    pub user_scopes: Option<String>,
    pub bot_scopes: Option<String>,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub user_id: Option<String>,
    pub bot_user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackOAuthAccessResponse {
    ok: bool,
    error: Option<String>,
    access_token: Option<String>,
    scope: Option<String>,
    bot_user_id: Option<String>,
    team: Option<SlackOAuthTeam>,
    authed_user: Option<SlackOAuthAuthedUser>,
}

#[derive(Debug, Deserialize)]
struct SlackOAuthTeam {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackOAuthAuthedUser {
    id: Option<String>,
    scope: Option<String>,
    access_token: Option<String>,
}

impl SlackOAuthAccessResponse {
    fn into_tokens(self) -> SlackOAuthTokens {
        let (team_id, team_name) = self
            .team
            .map(|team| (team.id, team.name))
            .unwrap_or((None, None));
        let (user_id, user_scopes, user_token) = self
            .authed_user
            .map(|user| (user.id, user.scope, user.access_token))
            .unwrap_or((None, None, None));
        SlackOAuthTokens {
            user_token: user_token.and_then(non_empty_string),
            bot_token: self.access_token.and_then(non_empty_string),
            user_scopes: user_scopes.and_then(non_empty_string),
            bot_scopes: self.scope.and_then(non_empty_string),
            team_id: team_id.and_then(non_empty_string),
            team_name: team_name.and_then(non_empty_string),
            user_id: user_id.and_then(non_empty_string),
            bot_user_id: self.bot_user_id.and_then(non_empty_string),
        }
    }
}

/// Parameters captured from the loopback OAuth redirect request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlackOAuthCallback {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackSocketEnvelope {
    envelope_id: Option<String>,
    #[serde(rename = "type")]
    envelope_type: Option<String>,
    payload: Option<SlackSocketPayload>,
}

#[derive(Debug, Deserialize)]
struct SlackSocketPayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    event: Option<SlackRealtimeEvent>,
}

#[derive(Debug, Deserialize)]
struct SlackRealtimeEvent {
    #[serde(rename = "type")]
    event_type: String,
    channel: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    username: Option<String>,
    icons: Option<SlackMessageIconsResponse>,
    bot_profile: Option<SlackBotProfileResponse>,
    ts: Option<String>,
    event_ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
    blocks: Option<Vec<serde_json::Value>>,
    attachments: Option<Vec<SlackAttachmentResponse>>,
    files: Option<Vec<SlackFileResponse>>,
    subtype: Option<String>,
    hidden: Option<bool>,
    deleted_ts: Option<String>,
    message: Option<SlackRealtimeInnerMessage>,
    reaction: Option<String>,
    item: Option<SlackReactionItem>,
    item_user: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackRealtimeInnerMessage {
    channel: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    username: Option<String>,
    icons: Option<SlackMessageIconsResponse>,
    bot_profile: Option<SlackBotProfileResponse>,
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
    blocks: Option<Vec<serde_json::Value>>,
    attachments: Option<Vec<SlackAttachmentResponse>>,
    files: Option<Vec<SlackFileResponse>>,
    #[serde(default)]
    edited: Option<SlackEditedResponse>,
}

#[derive(Debug, Deserialize)]
struct SlackReactionItem {
    channel: Option<String>,
    ts: Option<String>,
}

#[derive(Debug, Default)]
pub struct SlackWebApiClient;

#[derive(Debug, Deserialize)]
struct SlackAuthTestResponse {
    ok: bool,
    error: Option<String>,
    team: Option<String>,
    user: Option<String>,
    team_id: Option<String>,
    user_id: Option<String>,
    bot_id: Option<String>,
}

impl SlackCredential {
    pub fn new(kind: SlackCredentialKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl SlackConversation {
    fn from_response(response: SlackConversationResponse) -> Option<Self> {
        let id = response.id.trim();
        if id.is_empty() || response.is_archived.unwrap_or(false) {
            return None;
        }

        Some(Self {
            id: id.to_owned(),
            name: response.name.and_then(non_empty_string),
            user: response.user.and_then(non_empty_string),
            is_channel: response.is_channel.unwrap_or(false),
            is_group: response.is_group.unwrap_or(false),
            is_im: response.is_im.unwrap_or(false),
            is_mpim: response.is_mpim.unwrap_or(false),
            is_member: response.is_member,
            is_private: response.is_private.unwrap_or(false),
            is_archived: response.is_archived.unwrap_or(false),
            is_ext_shared: response.is_ext_shared.unwrap_or(false),
            is_muted: response.is_muted.unwrap_or(false),
            is_pinned: response.is_pinned.unwrap_or(false),
            unread_count: response
                .unread_count_display
                .or(response.unread_count)
                .unwrap_or(0),
            updated: response.updated,
            created: response.created,
            creator: response.creator.and_then(non_empty_string),
            topic: response
                .topic
                .and_then(|topic| topic.value)
                .and_then(non_empty_string),
            purpose: response
                .purpose
                .and_then(|purpose| purpose.value)
                .and_then(non_empty_string),
            num_members: response.num_members,
        })
    }
}

impl fmt::Debug for SlackCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackCredential")
            .field("kind", &self.kind)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl SlackApiClient for SlackWebApiClient {
    async fn validate_token(
        &self,
        credential: SlackCredential,
    ) -> Result<SlackValidatedCredential> {
        match credential.kind {
            SlackCredentialKind::AppToken => validate_app_token(&credential),
            SlackCredentialKind::Webhook => {
                bail!("webhook credentials are not Slack Web API tokens")
            }
            SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown => validate_web_api_token(credential).await,
        }
    }

    async fn validate_webhook(&self, webhook_url: &str) -> Result<SlackWebhookValidation> {
        validate_webhook_url(webhook_url)
    }

    async fn post_message(
        &self,
        credential: SlackCredential,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<SlackPostedMessage> {
        post_web_api_message(credential, channel, text, thread_ts).await
    }

    async fn post_webhook(&self, webhook_url: &str, text: &str) -> Result<SlackPostedMessage> {
        post_webhook_message(webhook_url, text).await
    }

    async fn update_message(
        &self,
        credential: SlackCredential,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<SlackPostedMessage> {
        update_web_api_message(credential, channel, ts, text).await
    }

    async fn upload_file(
        &self,
        credential: SlackCredential,
        request: SlackUploadFileRequest,
    ) -> Result<SlackUploadedFile> {
        upload_web_api_file(credential, request).await
    }

    async fn list_conversations(
        &self,
        credential: SlackCredential,
    ) -> Result<Vec<SlackConversation>> {
        list_web_api_conversations(credential, SlackListingEndpoint::AllConversations).await
    }

    async fn list_member_conversations(
        &self,
        credential: SlackCredential,
        scope: SlackMemberScope,
    ) -> Result<Vec<SlackConversation>> {
        let member =
            list_web_api_conversations(credential.clone(), SlackListingEndpoint::Member(scope))
                .await?;
        let direct_messages =
            list_web_api_conversations(credential, SlackListingEndpoint::DirectMessages).await?;
        Ok(merge_member_listings(scope, member, direct_messages))
    }

    async fn user_info(
        &self,
        credential: SlackCredential,
        user_id: &str,
    ) -> Result<Option<SlackUser>> {
        get_web_api_user_info(credential, user_id).await
    }

    async fn team_info(
        &self,
        credential: SlackCredential,
        team_id: Option<&str>,
    ) -> Result<Option<SlackTeamInfo>> {
        get_web_api_team_info(credential, team_id).await
    }

    async fn conversation_members(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Vec<String>> {
        list_web_api_conversation_members(credential, channel).await
    }

    async fn conversation_info(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Option<SlackConversation>> {
        get_web_api_conversation_info(credential, channel).await
    }

    async fn add_reaction(
        &self,
        credential: SlackCredential,
        channel: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<()> {
        post_web_api_reaction("reactions.add", credential, channel, timestamp, emoji).await
    }

    async fn remove_reaction(
        &self,
        credential: SlackCredential,
        channel: &str,
        timestamp: &str,
        emoji: &str,
    ) -> Result<()> {
        post_web_api_reaction("reactions.remove", credential, channel, timestamp, emoji).await
    }

    async fn history(
        &self,
        credential: SlackCredential,
        account: &ProviderId,
        current_user_id: Option<&str>,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
        users: Option<Arc<RwLock<HashMap<String, SlackUser>>>>,
    ) -> Result<Vec<Message>> {
        list_web_api_history(
            credential,
            account,
            current_user_id,
            chat_id,
            before,
            limit,
            users,
        )
        .await
    }

    async fn open_socket_mode(
        &self,
        app_token: SlackCredential,
    ) -> Result<SlackSocketModeConnection> {
        open_socket_mode(app_token).await
    }

    async fn exchange_oauth_code(
        &self,
        client_id: &str,
        client_secret: &str,
        redirect_uri: &str,
        code: &str,
    ) -> Result<SlackOAuthTokens> {
        exchange_web_api_oauth_code(client_id, client_secret, redirect_uri, code).await
    }
}

async fn validate_web_api_token(credential: SlackCredential) -> Result<SlackValidatedCredential> {
    let token_kind = credential.kind.clone();
    let token = credential.value;
    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .get("https://slack.com/api/auth.test")
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .context("calling Slack auth.test")?;
        let auth: SlackAuthTestResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack auth.test response")?;
        if !auth.ok {
            bail!(
                "Slack auth.test failed: {}",
                auth.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }

        let actual_kind = classify_validated_token(&token_kind, &token, auth.bot_id.as_deref());
        Ok(SlackValidatedCredential {
            kind: actual_kind,
            team_id: auth.team_id,
            team_name: auth.team,
            user_id: auth.user_id,
            user_name: auth.user,
            bot_id: auth.bot_id,
        })
    })
    .await
    .context("joining Slack token validation task")?
}

async fn post_web_api_message(
    credential: SlackCredential,
    channel: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<SlackPostedMessage> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack chat.postMessage requires a user or bot Web API token");
    }

    let token = credential.value;
    let channel = channel.to_owned();
    let text = text.to_owned();
    let thread_ts = thread_ts.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let request = SlackPostMessageRequest {
            channel: &channel,
            text: &text,
            thread_ts: thread_ts.as_deref(),
        };
        let mut response = slack_http_agent()
            .post("https://slack.com/api/chat.postMessage")
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&request)
            .context("calling Slack chat.postMessage")?;
        let posted: SlackPostMessageResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack chat.postMessage response")?;
        if !posted.ok {
            bail!(
                "Slack chat.postMessage failed: {}",
                posted.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }

        let ts = posted
            .ts
            .filter(|ts| !ts.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("Slack chat.postMessage response did not include a message timestamp")
            })?;
        Ok(SlackPostedMessage {
            channel: posted.channel,
            ts,
        })
    })
    .await
    .context("joining Slack message posting task")?
}

#[derive(Debug, Serialize)]
struct SlackUpdateMessageRequest<'a> {
    channel: &'a str,
    ts: &'a str,
    text: &'a str,
}

/// Readable explanation for the `chat.update` errors a user can trigger.
fn slack_update_error_message(code: &str) -> String {
    match code {
        "cant_update_message" => {
            "Slack only lets the account that posted a message edit it".to_owned()
        }
        "edit_window_closed" => "Slack's edit window for this message has closed".to_owned(),
        "message_not_found" => "the message no longer exists in Slack".to_owned(),
        other => format!("Slack chat.update failed: {other}"),
    }
}

async fn update_web_api_message(
    credential: SlackCredential,
    channel: &str,
    ts: &str,
    text: &str,
) -> Result<SlackPostedMessage> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack chat.update requires a user or bot Web API token");
    }

    let token = credential.value;
    let channel = channel.to_owned();
    let ts = ts.to_owned();
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        let request = SlackUpdateMessageRequest {
            channel: &channel,
            ts: &ts,
            text: &text,
        };
        let mut response = slack_http_agent()
            .post("https://slack.com/api/chat.update")
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&request)
            .context("calling Slack chat.update")?;
        // chat.update shares chat.postMessage's `{ok, error, channel, ts}` shape.
        let updated: SlackPostMessageResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack chat.update response")?;
        if !updated.ok {
            bail!(slack_update_error_message(
                updated.error.as_deref().unwrap_or("unknown_error")
            ));
        }
        Ok(SlackPostedMessage {
            channel: updated.channel,
            ts: updated.ts.unwrap_or(ts),
        })
    })
    .await
    .context("joining Slack message update task")?
}

async fn post_webhook_message(webhook_url: &str, text: &str) -> Result<SlackPostedMessage> {
    let webhook_url = webhook_url.to_owned();
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        let request = SlackWebhookPostRequest { text: &text };
        let mut response = slack_http_agent()
            .post(&webhook_url)
            .send_json(&request)
            .context("posting Slack webhook message")?;
        let body = response
            .body_mut()
            .read_to_string()
            .context("reading Slack webhook response")?;
        if body.trim() != "ok" {
            bail!("Slack webhook post failed: {body}");
        }

        Ok(SlackPostedMessage {
            channel: None,
            ts: synthetic_webhook_message_id(&text),
        })
    })
    .await
    .context("joining Slack webhook posting task")?
}

async fn upload_web_api_file(
    credential: SlackCredential,
    request: SlackUploadFileRequest,
) -> Result<SlackUploadedFile> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack file upload requires a user or bot Web API token");
    }

    let token = credential.value;
    tokio::task::spawn_blocking(move || {
        let metadata = fs::metadata(&request.path)
            .with_context(|| format!("reading metadata for {}", request.path.display()))?;
        if !metadata.is_file() {
            bail!(
                "Slack upload path is not a regular file: {}",
                request.path.display()
            );
        }
        let length = metadata.len();
        let upload_request = SlackGetUploadUrlRequest {
            filename: &request.filename,
            length,
            alt_txt: None,
        };
        let mut response = slack_http_agent()
            .post("https://slack.com/api/files.getUploadURLExternal")
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&upload_request)
            .context("calling Slack files.getUploadURLExternal")?;
        let upload_ticket: SlackGetUploadUrlResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack files.getUploadURLExternal response")?;
        if !upload_ticket.ok {
            bail!(
                "Slack files.getUploadURLExternal failed: {}",
                upload_ticket
                    .error
                    .unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        let upload_url = upload_ticket
            .upload_url
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| anyhow!("Slack upload response did not include an upload URL"))?;
        let file_id = upload_ticket
            .file_id
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow!("Slack upload response did not include a file ID"))?;
        let bytes = fs::read(&request.path)
            .with_context(|| format!("reading upload file {}", request.path.display()))?;
        let mut upload_response = slack_http_agent()
            .post(&upload_url)
            .content_type(&request.mime_type)
            .send(bytes.as_slice())
            .with_context(|| format!("uploading {} to Slack", request.path.display()))?;
        let _ = upload_response
            .body_mut()
            .read_to_string()
            .context("reading Slack file upload response")?;

        let complete_request = SlackCompleteUploadRequest {
            files: vec![SlackCompleteUploadFile {
                id: &file_id,
                title: &request.title,
            }],
            channel_id: &request.channel,
            initial_comment: request.initial_comment.as_deref(),
            thread_ts: request.thread_ts.as_deref(),
        };
        let mut response = slack_http_agent()
            .post("https://slack.com/api/files.completeUploadExternal")
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&complete_request)
            .context("calling Slack files.completeUploadExternal")?;
        let completed: SlackCompleteUploadResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack files.completeUploadExternal response")?;
        if !completed.ok {
            bail!(
                "Slack files.completeUploadExternal failed: {}",
                completed
                    .error
                    .unwrap_or_else(|| "unknown_error".to_owned())
            );
        }

        let uploaded = completed
            .files
            .unwrap_or_default()
            .into_iter()
            .next()
            .unwrap_or(SlackCompleteUploadFileResponse {
                id: Some(file_id),
                title: Some(request.title),
            });
        let id = uploaded
            .id
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow!("Slack complete upload response did not include a file ID"))?;
        Ok(SlackUploadedFile {
            id,
            title: uploaded.title,
        })
    })
    .await
    .context("joining Slack file upload task")?
}

/// Which subset of conversations a member listing returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlackMemberScope {
    /// Everything the sidebar shows: joined channels, private channels, DMs
    /// and group DMs.
    Sidebar,
    /// Only DMs and group DMs (what the always-on DM poll watches).
    Direct,
}

impl SlackMemberScope {
    #[cfg(test)]
    fn types(self) -> &'static str {
        match self {
            Self::Sidebar => SLACK_CONVERSATION_TYPES,
            Self::Direct => "im,mpim",
        }
    }

    /// Types fetched through `users.conversations`. 1:1 DMs are excluded:
    /// that endpoint omits IMs with bots, deactivated users and long-idle
    /// DMs, which made them vanish from the sidebar. They are listed via
    /// `conversations.list types=im` instead, which only returns the user's
    /// own DMs (a single cheap page, unlike public channels).
    fn member_types(self) -> &'static str {
        match self {
            Self::Sidebar => "public_channel,private_channel,mpim",
            Self::Direct => "mpim",
        }
    }

    fn includes(self, conversation: &SlackConversation) -> bool {
        match self {
            Self::Sidebar => include_conversation_in_sidebar(conversation),
            Self::Direct => conversation.is_im || conversation.is_mpim,
        }
    }
}

/// Web API method backing a paginated conversation listing.
#[derive(Clone, Copy, Debug)]
enum SlackListingEndpoint {
    /// `conversations.list`: every visible conversation, including public
    /// channels the user never joined (needed for discovery only).
    AllConversations,
    /// `conversations.list types=im`: every 1:1 DM of the user.
    DirectMessages,
    /// `users.conversations`: only conversations the user is a member of
    /// (without 1:1 DMs, see [`SlackMemberScope::member_types`]).
    Member(SlackMemberScope),
}

impl SlackListingEndpoint {
    fn method(self) -> &'static str {
        match self {
            Self::AllConversations | Self::DirectMessages => "conversations.list",
            Self::Member(_) => "users.conversations",
        }
    }

    fn context_label(self) -> &'static str {
        match self {
            Self::AllConversations | Self::DirectMessages => "calling Slack conversations.list",
            Self::Member(_) => "calling Slack users.conversations",
        }
    }

    fn types(self) -> &'static str {
        match self {
            Self::AllConversations => SLACK_CONVERSATION_TYPES,
            Self::DirectMessages => "im",
            Self::Member(scope) => scope.member_types(),
        }
    }

    /// Page size. `users.conversations` accepts up to 1000, so a typical
    /// account's joined conversations arrive in a single paced request.
    fn page_limit(self) -> &'static str {
        match self {
            Self::AllConversations => "200",
            Self::DirectMessages | Self::Member(_) => "999",
        }
    }
}

/// Combines the `users.conversations` listing with the separately listed 1:1
/// DMs, dropping duplicates and anything outside `scope`.
fn merge_member_listings(
    scope: SlackMemberScope,
    member: Vec<SlackConversation>,
    direct_messages: Vec<SlackConversation>,
) -> Vec<SlackConversation> {
    let mut seen = HashSet::new();
    member
        .into_iter()
        .chain(direct_messages)
        .filter(|conversation| seen.insert(conversation.id.clone()))
        .filter(|conversation| scope.includes(conversation))
        .collect()
}

async fn list_web_api_conversations(
    credential: SlackCredential,
    endpoint: SlackListingEndpoint,
) -> Result<Vec<SlackConversation>> {
    let method = endpoint.method();
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack {method} requires a user or bot Web API token");
    }

    let token = credential.value;
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let mut conversations = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        let url = format!("https://slack.com/api/{method}");

        loop {
            pages += 1;
            let mut response = slack_get_with_retry(endpoint.context_label(), || {
                let mut request = slack_http_agent()
                    .get(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .query("types", endpoint.types())
                    .query("exclude_archived", "true")
                    .query("limit", endpoint.page_limit());

                if let Some(cursor) = cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
                    request = request.query("cursor", cursor);
                }

                request.call()
            })?;
            let listed: SlackConversationsListResponse = response
                .body_mut()
                .read_json()
                .with_context(|| format!("decoding Slack {method} response"))?;
            if !listed.ok {
                bail!(
                    "Slack {method} failed: {}",
                    listed.error.unwrap_or_else(|| "unknown_error".to_owned())
                );
            }

            let member_listing = matches!(endpoint, SlackListingEndpoint::Member(_));
            conversations.extend(
                listed
                    .channels
                    .into_iter()
                    .filter_map(SlackConversation::from_response)
                    .map(|mut conversation| {
                        // `users.conversations` only returns joined
                        // conversations but may omit `is_member`.
                        if member_listing {
                            conversation.is_member.get_or_insert(true);
                        }
                        conversation
                    }),
            );

            cursor = listed
                .response_metadata
                .next_cursor
                .filter(|cursor| !cursor.trim().is_empty());
            if cursor.is_none() {
                break;
            }
        }

        slack_diagnostic_log(
            "slack.web_api.list_conversations",
            format!(
                "method={method} types={} pages={pages} count={} elapsed_ms={:.1}",
                endpoint.types(),
                conversations.len(),
                started.elapsed().as_secs_f64() * 1000.0
            ),
        );
        Ok(conversations)
    })
    .await
    .context("joining Slack conversation listing task")?
}

async fn get_web_api_user_info(
    credential: SlackCredential,
    user_id: &str,
) -> Result<Option<SlackUser>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack users.info requires a user or bot Web API token");
    }

    let token = credential.value;
    let user_id = user_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .get("https://slack.com/api/users.info")
            .header("Authorization", format!("Bearer {token}"))
            .query("user", &user_id)
            .call()
            .context("calling Slack users.info")?;
        let info: SlackUsersInfoResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack users.info response")?;
        if !info.ok {
            bail!(
                "Slack users.info failed: {}",
                info.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        Ok(info.user.and_then(SlackUser::from_response))
    })
    .await
    .context("joining Slack user info task")?
}

async fn get_web_api_team_info(
    credential: SlackCredential,
    team_id: Option<&str>,
) -> Result<Option<SlackTeamInfo>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack team.info requires a user or bot Web API token");
    }

    let token = credential.value;
    let team_id = team_id.map(str::to_owned).and_then(non_empty_string);
    tokio::task::spawn_blocking(move || {
        let mut request = slack_http_agent()
            .get("https://slack.com/api/team.info")
            .header("Authorization", format!("Bearer {token}"));
        if let Some(team_id) = team_id.as_deref() {
            request = request.query("team", team_id);
        }
        let mut response = request.call().context("calling Slack team.info")?;
        let info: SlackTeamInfoResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack team.info response")?;
        if !info.ok {
            bail!(
                "Slack team.info failed: {}",
                info.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        Ok(info.team.map(SlackTeamInfo::from_response))
    })
    .await
    .context("joining Slack team info task")?
}

async fn get_web_api_conversation_info(
    credential: SlackCredential,
    channel: &str,
) -> Result<Option<SlackConversation>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack conversations.info requires a user or bot Web API token");
    }

    let token = credential.value;
    let channel = channel.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .get("https://slack.com/api/conversations.info")
            .header("Authorization", format!("Bearer {token}"))
            .query("channel", &channel)
            .call()
            .context("calling Slack conversations.info")?;
        let info: SlackConversationInfoResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack conversations.info response")?;
        if !info.ok {
            bail!(
                "Slack conversations.info failed: {}",
                info.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        Ok(info.channel.and_then(SlackConversation::from_response))
    })
    .await
    .context("joining Slack conversation info task")?
}

async fn list_web_api_conversation_members(
    credential: SlackCredential,
    channel: &str,
) -> Result<Vec<String>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack conversations.members requires a user or bot Web API token");
    }

    let token = credential.value;
    let channel = channel.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut members = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut request = slack_http_agent()
                .get("https://slack.com/api/conversations.members")
                .header("Authorization", format!("Bearer {token}"))
                .query("channel", &channel)
                .query("limit", "200");

            if let Some(cursor) = cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
                request = request.query("cursor", cursor);
            }

            let mut response = request
                .call()
                .context("calling Slack conversations.members")?;
            let listed: SlackConversationsMembersResponse = response
                .body_mut()
                .read_json()
                .context("decoding Slack conversations.members response")?;
            if !listed.ok {
                bail!(
                    "Slack conversations.members failed: {}",
                    listed.error.unwrap_or_else(|| "unknown_error".to_owned())
                );
            }

            members.extend(listed.members.into_iter().filter_map(non_empty_string));
            cursor = listed
                .response_metadata
                .next_cursor
                .filter(|cursor| !cursor.trim().is_empty());
            if cursor.is_none() {
                break;
            }
        }

        Ok(members)
    })
    .await
    .context("joining Slack conversation members task")?
}

async fn post_web_api_reaction(
    method: &'static str,
    credential: SlackCredential,
    channel: &str,
    timestamp: &str,
    emoji: &str,
) -> Result<()> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack {method} requires a user or bot Web API token");
    }

    let token = credential.value;
    let channel = channel.to_owned();
    let timestamp = timestamp.to_owned();
    let emoji = normalize_slack_reaction_name(emoji)
        .ok_or_else(|| anyhow!("Slack reaction emoji cannot be empty"))?;
    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .post(format!("https://slack.com/api/{method}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_form([
                ("channel", channel.as_str()),
                ("timestamp", timestamp.as_str()),
                ("name", emoji.as_str()),
            ])
            .with_context(|| format!("calling Slack {method}"))?;
        let result: SlackReactionsApiResponse = response
            .body_mut()
            .read_json()
            .with_context(|| format!("decoding Slack {method} response"))?;
        if !result.ok {
            bail!(
                "Slack {method} failed: {}",
                result.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        Ok(())
    })
    .await
    .with_context(|| format!("joining Slack {method} task"))?
}

async fn list_web_api_history(
    credential: SlackCredential,
    account: &ProviderId,
    current_user_id: Option<&str>,
    chat_id: &ChatId,
    before: Option<Timestamp>,
    limit: usize,
    users: Option<Arc<RwLock<HashMap<String, SlackUser>>>>,
) -> Result<Vec<Message>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack conversations.history requires a user or bot Web API token");
    }

    let token = credential.value;
    let account = account.clone();
    let current_user_id = current_user_id.map(str::to_owned);
    let chat_id = chat_id.to_string();
    let latest = before.map(slack_timestamp_from_datetime);
    let limit = limit.clamp(1, 200).to_string();

    tokio::task::spawn_blocking(move || {
        let mut response = slack_get_with_retry("calling Slack conversations.history", || {
            let mut request = slack_http_agent()
                .get("https://slack.com/api/conversations.history")
                .header("Authorization", format!("Bearer {token}"))
                .query("channel", &chat_id)
                .query("limit", &limit)
                .query("inclusive", "false");

            if let Some(latest) = latest.as_deref() {
                request = request.query("latest", latest);
            }

            request.call()
        })?;
        let history: SlackConversationsHistoryResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack conversations.history response")?;
        if !history.ok {
            bail!(
                "Slack conversations.history failed: {}",
                history.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }

        let mut messages = Vec::new();
        let mut thread_roots = Vec::new();
        for message in history.messages {
            let thread_ts = message.thread_ts.clone();
            let ts = message.ts.clone();
            let reply_count = message.reply_count.unwrap_or(0);
            if let Some(message) = slack_history_message(
                account.clone(),
                current_user_id.as_deref(),
                chat_id.clone(),
                message,
                users.as_ref(),
                Some(token.as_str()),
            ) {
                if reply_count > 0
                    && thread_ts
                        .as_deref()
                        .or(ts.as_deref())
                        .is_some_and(|thread_ts| thread_ts == message.id.as_ref())
                {
                    thread_roots.push(message.id.to_string());
                }
                messages.push(message);
            }
        }

        let mut seen = messages
            .iter()
            .map(|message| message.id.to_string())
            .collect::<HashSet<_>>();
        for thread_ts in thread_roots {
            let replies = fetch_web_api_thread_replies(
                &token,
                &account,
                current_user_id.as_deref(),
                &chat_id,
                &thread_ts,
                users.as_ref(),
            )?;
            for reply in replies {
                if seen.insert(reply.id.to_string()) {
                    messages.push(reply);
                }
            }
        }

        messages.sort_by_key(|message| message.timestamp);
        Ok(messages)
    })
    .await
    .context("joining Slack history task")?
}

fn fetch_web_api_thread_replies(
    token: &str,
    account: &ProviderId,
    current_user_id: Option<&str>,
    chat_id: &str,
    thread_ts: &str,
    users: Option<&Arc<RwLock<HashMap<String, SlackUser>>>>,
) -> Result<Vec<Message>> {
    let mut replies = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let mut request = slack_http_agent()
            .get("https://slack.com/api/conversations.replies")
            .header("Authorization", format!("Bearer {token}"))
            .query("channel", chat_id)
            .query("ts", thread_ts)
            .query("limit", "200");

        if let Some(cursor) = cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
            request = request.query("cursor", cursor);
        }

        let mut response = request
            .call()
            .context("calling Slack conversations.replies")?;
        let listed: SlackConversationsHistoryResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack conversations.replies response")?;
        if !listed.ok {
            bail!(
                "Slack conversations.replies failed: {}",
                listed.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }

        replies.extend(listed.messages.into_iter().filter_map(|message| {
            slack_history_message(
                account.clone(),
                current_user_id,
                chat_id.to_owned(),
                message,
                users,
                Some(token),
            )
        }));

        cursor = listed
            .response_metadata
            .next_cursor
            .filter(|cursor| !cursor.trim().is_empty());
        if cursor.is_none() {
            break;
        }
    }

    Ok(replies)
}

async fn exchange_web_api_oauth_code(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
) -> Result<SlackOAuthTokens> {
    let client_id = client_id.trim().to_owned();
    let client_secret = client_secret.trim().to_owned();
    let redirect_uri = redirect_uri.trim().to_owned();
    let code = code.trim().to_owned();
    if client_id.is_empty() || client_secret.is_empty() {
        bail!("Slack OAuth requires a client ID and client secret");
    }
    if code.is_empty() {
        bail!("Slack OAuth code is empty");
    }

    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .post("https://slack.com/api/oauth.v2.access")
            .send_form([
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .context("calling Slack oauth.v2.access")?;
        let parsed: SlackOAuthAccessResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack oauth.v2.access response")?;
        if !parsed.ok {
            bail!(
                "Slack oauth.v2.access failed: {}",
                parsed.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        let tokens = parsed.into_tokens();
        if tokens.user_token.is_none() && tokens.bot_token.is_none() {
            bail!("Slack oauth.v2.access succeeded but returned no usable token");
        }
        Ok(tokens)
    })
    .await
    .context("joining Slack oauth.v2.access task")?
}

/// Generate an unguessable OAuth `state` value for CSRF protection on the
/// loopback redirect. No `rand` dependency is available, so entropy is mixed
/// from a high-resolution timestamp, a monotonic counter, and a stack address.
fn slack_oauth_state() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let stack_marker = &counter as *const _ as usize;

    let mut hasher = DefaultHasher::new();
    nanos.hash(&mut hasher);
    counter.hash(&mut hasher);
    stack_marker.hash(&mut hasher);
    let high = hasher.finish();
    // Hash again with the first digest folded in for a second 64-bit chunk so
    // the resulting token is 128 bits of derived state.
    high.hash(&mut hasher);
    nanos.hash(&mut hasher);
    let low = hasher.finish();
    format!("{high:016x}{low:016x}")
}

/// Constant-length-ish comparison of OAuth state values. Both sides come from
/// our own generator (hex), so this rejects empty/mismatched callback state.
fn slack_oauth_state_matches(expected: &str, received: Option<&str>) -> bool {
    match received {
        Some(received) => !expected.is_empty() && expected == received.trim(),
        None => false,
    }
}

/// Bind the fixed loopback OAuth callback port. Returned to the caller so the
/// browser can be opened only after the listener is ready to accept the
/// redirect (avoiding a race where Slack redirects before we are listening).
fn bind_oauth_callback_listener() -> Result<TcpListener> {
    let listener =
        TcpListener::bind(("127.0.0.1", SLACK_OAUTH_REDIRECT_PORT)).with_context(|| {
            format!(
                "binding Slack OAuth callback listener on 127.0.0.1:{SLACK_OAUTH_REDIRECT_PORT}; \
             another instance may be mid-login"
            )
        })?;
    Ok(listener)
}

/// Accept a single loopback HTTP request, parse the OAuth `code`/`state`/`error`
/// from its query string, and reply with a small confirmation page so the
/// browser tab shows a friendly message. Runs to completion on a blocking
/// thread; callers should invoke it via `spawn_blocking`.
fn accept_oauth_callback(listener: &TcpListener, timeout: Duration) -> Result<SlackOAuthCallback> {
    listener
        .set_nonblocking(false)
        .context("configuring Slack OAuth callback listener")?;
    let deadline = SystemTime::now() + timeout;
    loop {
        let (mut stream, _addr) = listener
            .accept()
            .context("waiting for Slack OAuth callback request")?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut buffer = [0u8; 4096];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]);
        let Some(target) = request.lines().next().and_then(parse_http_request_target) else {
            // Ignore non-HTTP or preflight noise (e.g. favicon) and keep waiting
            // until the real redirect arrives or the deadline passes.
            write_oauth_callback_response(&mut stream, "Waiting for Slack…");
            if SystemTime::now() >= deadline {
                bail!("Timed out waiting for the Slack OAuth callback");
            }
            continue;
        };
        let callback = parse_oauth_callback_query(&target);
        let body = if callback.error.is_some() {
            "Slack sign-in failed. You can close this tab and return to chat-cli."
        } else if callback.code.is_some() {
            "Slack connected. You can close this tab and return to chat-cli."
        } else {
            "Slack sign-in is missing an authorization code. Return to chat-cli to retry."
        };
        write_oauth_callback_response(&mut stream, body);
        return Ok(callback);
    }
}

fn parse_http_request_target(request_line: &str) -> Option<String> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("GET") {
        return None;
    }
    Some(parts.next()?.to_owned())
}

fn parse_oauth_callback_query(target: &str) -> SlackOAuthCallback {
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
    let mut callback = SlackOAuthCallback::default();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode_form(value);
        match key {
            "code" => callback.code = non_empty_string(value),
            "state" => callback.state = non_empty_string(value),
            "error" => callback.error = non_empty_string(value),
            _ => {}
        }
    }
    callback
}

fn percent_decode_form(value: &str) -> String {
    let bytes = value.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = (bytes[index + 1] as char).to_digit(16);
                let lo = (bytes[index + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    index += 3;
                    continue;
                }
                out.push(bytes[index]);
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_oauth_callback_response(stream: &mut impl Write, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>chat-cli</title></head>\
         <body style=\"font-family:sans-serif;padding:2rem\">{message}</body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// A browser-based Slack OAuth login in progress.
///
/// Created by [`begin_slack_oauth_login`], which binds the fixed loopback
/// listener and builds the authorize URL (including a CSRF `state`) up front.
/// The caller opens [`SlackOAuthLoginFlow::authorize_url`] in a browser, then
/// awaits [`SlackOAuthLoginFlow::wait_for_authorization_code`] to receive the
/// authorization code without the user copy/pasting anything.
pub struct SlackOAuthLoginFlow {
    authorize_url: String,
    listener: TcpListener,
    state: String,
    redirect_uri: String,
}

impl SlackOAuthLoginFlow {
    /// The Slack authorize URL to open in the user's browser.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The redirect URI advertised to Slack in the authorize request. Slack
    /// matches this value exactly during the `oauth.v2.access` exchange, so the
    /// same value must be reused there. For distributed apps this is an HTTPS
    /// relay URL that 302-redirects the browser back to the local loopback
    /// listener; for single-workspace/dev use it defaults to the loopback URL.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for Slack to redirect to the loopback listener, validate the CSRF
    /// `state`, and return the authorization `code`. Runs the blocking accept on
    /// a worker thread so the async runtime is never blocked.
    pub async fn wait_for_authorization_code(self) -> Result<String> {
        let SlackOAuthLoginFlow {
            listener, state, ..
        } = self;
        let callback = tokio::task::spawn_blocking(move || {
            accept_oauth_callback(&listener, SLACK_OAUTH_CALLBACK_TIMEOUT)
        })
        .await
        .context("joining Slack OAuth callback listener task")??;
        if let Some(error) = callback.error {
            bail!("Slack sign-in failed: {error}");
        }
        if !slack_oauth_state_matches(&state, callback.state.as_deref()) {
            bail!(
                "Slack OAuth state mismatch; aborting login to avoid a cross-request token mixup"
            );
        }
        callback
            .code
            .ok_or_else(|| anyhow!("Slack OAuth callback did not include an authorization code"))
    }
}

/// Begin a browser-based Slack OAuth login: bind the loopback callback listener
/// and build the authorize URL with a fresh CSRF `state`. The listener is bound
/// before returning so the browser can be opened without racing the redirect.
///
/// `redirect_uri` is the value advertised to Slack. Slack requires distributed
/// apps to use an HTTPS redirect, so production builds pass an HTTPS relay URL
/// that forwards the browser back to the local loopback listener; the listener
/// itself always binds the fixed loopback port regardless of this value. When
/// `redirect_uri` is empty the loopback URL is used (single-workspace/dev).
pub fn begin_slack_oauth_login(
    client_id: &str,
    redirect_uri: &str,
    bot_scopes: &str,
    user_scopes: &str,
) -> Result<SlackOAuthLoginFlow> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        bail!("Slack OAuth requires a client ID to start the browser login");
    }
    let redirect_uri = non_empty_string(redirect_uri.to_owned())
        .unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned());
    let listener = bind_oauth_callback_listener()?;
    let state = slack_oauth_state();
    let authorize_url =
        slack_authorize_url(client_id, &redirect_uri, bot_scopes, user_scopes, &state);
    Ok(SlackOAuthLoginFlow {
        authorize_url,
        listener,
        state,
        redirect_uri,
    })
}

/// Build the Slack `oauth/v2/authorize` URL. Pure (no socket binding) so the
/// URL/redirect construction is unit-testable without racing the fixed loopback
/// port. `redirect_uri` is assumed already normalized to a non-empty value.
fn slack_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    bot_scopes: &str,
    user_scopes: &str,
    state: &str,
) -> String {
    format!(
        "https://slack.com/oauth/v2/authorize?client_id={}&scope={}&user_scope={}&redirect_uri={}&state={}",
        url_component(client_id.trim()),
        url_component(bot_scopes),
        url_component(user_scopes),
        url_component(redirect_uri),
        url_component(state),
    )
}

async fn open_socket_mode(app_token: SlackCredential) -> Result<SlackSocketModeConnection> {
    if !matches!(app_token.kind, SlackCredentialKind::AppToken) {
        bail!("Slack Socket Mode requires an app-level token");
    }

    let token = app_token.value;
    tokio::task::spawn_blocking(move || {
        let mut response = slack_http_agent()
            .post("https://slack.com/api/apps.connections.open")
            .header("Authorization", format!("Bearer {token}"))
            .send_empty()
            .context("calling Slack apps.connections.open")?;
        let opened: SlackSocketModeOpenResponse = response
            .body_mut()
            .read_json()
            .context("decoding Slack apps.connections.open response")?;
        if !opened.ok {
            bail!(
                "Slack apps.connections.open failed: {}",
                opened.error.unwrap_or_else(|| "unknown_error".to_owned())
            );
        }
        let url = opened
            .url
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| anyhow!("Slack Socket Mode response did not include a WebSocket URL"))?;
        Ok(SlackSocketModeConnection { url })
    })
    .await
    .context("joining Slack Socket Mode open task")?
}

fn synthetic_webhook_message_id(text: &str) -> String {
    format!(
        "webhook:{}:{}",
        chrono::Utc::now().timestamp_millis(),
        text.len()
    )
}

/// Build the account display label, preferring the real Slack workspace name
/// over a manually-entered label. Always derives from a raw name and unwraps
/// any previously double-wrapped `Slack (...)` value so it cannot compound.
fn slack_account_display_name(
    team_name: Option<&str>,
    workspace: Option<&str>,
    auth_mode: &SlackAuthMode,
) -> String {
    let resolved = team_name
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| workspace.map(str::trim).filter(|value| !value.is_empty()));
    match resolved {
        Some(name) => format!("Slack ({})", strip_slack_label_wrapper(name)),
        None => format!("Slack ({})", auth_mode.label()),
    }
}

/// Strip one or more enclosing `Slack (...)` wrappers from a stored label so a
/// previously double-wrapped value renders cleanly.
fn strip_slack_label_wrapper(name: &str) -> &str {
    let trimmed = name.trim();
    match trimmed
        .strip_prefix("Slack (")
        .and_then(|rest| rest.strip_suffix(')'))
        .map(str::trim)
    {
        Some(inner) if !inner.is_empty() => strip_slack_label_wrapper(inner),
        _ => trimmed,
    }
}

fn provider_id_for_options(options: &SlackProviderOptions) -> ProviderId {
    let workspace = options
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
        .map(sanitize_provider_id_segment);

    match workspace {
        Some(workspace) => arc_str(format!("{PROVIDER_ID_PREFIX}:{workspace}")),
        None => arc_str(PROVIDER_ID),
    }
}

fn sanitize_provider_id_segment(value: &str) -> String {
    let mut sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while sanitized.contains("--") {
        sanitized = sanitized.replace("--", "-");
    }
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "workspace".to_owned()
    } else {
        sanitized.to_owned()
    }
}

fn validate_app_token(credential: &SlackCredential) -> Result<SlackValidatedCredential> {
    if !credential.value().trim_start().starts_with("xapp-") {
        bail!("Slack app-level token must start with xapp-");
    }

    Ok(SlackValidatedCredential {
        kind: SlackCredentialKind::AppToken,
        team_id: None,
        team_name: None,
        user_id: None,
        user_name: None,
        bot_id: None,
    })
}

fn validate_webhook_url(webhook_url: &str) -> Result<SlackWebhookValidation> {
    let trimmed = webhook_url.trim();
    if !trimmed.starts_with("https://hooks.slack.com/services/") {
        bail!("Slack webhook URL must start with https://hooks.slack.com/services/");
    }

    Ok(SlackWebhookValidation {
        url_host: "hooks.slack.com".to_owned(),
    })
}

fn classify_validated_token(
    requested_kind: &SlackCredentialKind,
    token: &str,
    bot_id: Option<&str>,
) -> SlackCredentialKind {
    let trimmed = token.trim_start();
    if trimmed.starts_with("xoxb-") {
        return SlackCredentialKind::BotToken;
    }
    if trimmed.starts_with("xoxp-") || trimmed.starts_with("xoxa-") {
        return SlackCredentialKind::UserToken;
    }
    if trimmed.starts_with("xapp-") {
        return SlackCredentialKind::AppToken;
    }

    match requested_kind {
        SlackCredentialKind::Unknown if bot_id.is_some() => SlackCredentialKind::BotToken,
        SlackCredentialKind::Unknown => SlackCredentialKind::UserToken,
        other => other.clone(),
    }
}

impl SlackConnectionState {
    fn is_empty(&self) -> bool {
        self.user_token.is_none() && self.bot_token.is_none() && self.webhook_url.is_none()
    }

    fn remember_validated_credential(
        &mut self,
        credential: &SlackCredential,
        validated: &SlackValidatedCredential,
    ) {
        match validated.kind {
            SlackCredentialKind::UserToken => self.user_token = Some(credential.value().to_owned()),
            SlackCredentialKind::BotToken => self.bot_token = Some(credential.value().to_owned()),
            SlackCredentialKind::AppToken => self.app_token = Some(credential.value().to_owned()),
            SlackCredentialKind::Webhook | SlackCredentialKind::Unknown => {}
        }

        self.team_id = self.team_id.clone().or_else(|| validated.team_id.clone());
        self.team_name = self
            .team_name
            .clone()
            .or_else(|| validated.team_name.clone());
        self.user_id = self.user_id.clone().or_else(|| validated.user_id.clone());
        self.bot_id = self.bot_id.clone().or_else(|| validated.bot_id.clone());
    }

    fn read_credential(&self) -> Option<SlackCredential> {
        if let Some(token) = &self.user_token {
            Some(SlackCredential::new(
                SlackCredentialKind::UserToken,
                token.clone(),
            ))
        } else {
            self.bot_token
                .as_ref()
                .map(|token| SlackCredential::new(SlackCredentialKind::BotToken, token.clone()))
        }
    }

    /// User-token credential only, or `None` for bot-only connections. A
    /// user's own (human-to-human) direct messages live in their personal IM
    /// channels, which Socket Mode never surfaces because realtime events are
    /// bot-scoped. Only the user token can read those via `conversations.history`,
    /// so the dedicated DM poll requires this credential and skips itself when
    /// no user token is present.
    fn user_credential(&self) -> Option<SlackCredential> {
        self.user_token
            .as_ref()
            .map(|token| SlackCredential::new(SlackCredentialKind::UserToken, token.clone()))
    }

    fn web_api_credentials(&self) -> Vec<SlackCredential> {
        let mut credentials = Vec::new();
        if let Some(token) = &self.user_token {
            credentials.push(SlackCredential::new(
                SlackCredentialKind::UserToken,
                token.clone(),
            ));
        }
        if let Some(token) = &self.bot_token {
            credentials.push(SlackCredential::new(
                SlackCredentialKind::BotToken,
                token.clone(),
            ));
        }
        credentials
    }
}

impl SlackProvider {
    fn emit_network_activity(
        &self,
        direction: NetworkActivityDirection,
        kind: NetworkActivityKind,
    ) {
        self.events
            .send(ProviderEvent::NetworkActivity { direction, kind });
    }

    async fn call_api<T, Fut>(&self, kind: NetworkActivityKind, call: Fut) -> Result<T>
    where
        Fut: std::future::Future<Output = Result<T>>,
    {
        self.emit_network_activity(NetworkActivityDirection::Tx, kind);
        let result = call.await;
        if result.is_ok() {
            self.emit_network_activity(NetworkActivityDirection::Rx, kind);
        }
        result
    }

    pub fn with_options(options: SlackProviderOptions) -> Result<Self> {
        Self::with_api_client(options, Arc::new(SlackWebApiClient::default()))
    }

    pub fn with_api_client(
        options: SlackProviderOptions,
        api_client: Arc<dyn SlackApiClient>,
    ) -> Result<Self> {
        let id = provider_id_for_options(&options);
        let display_name =
            slack_account_display_name(None, options.workspace.as_deref(), &options.auth_mode);
        let capabilities = SlackCapabilities::default();
        let account = Account {
            id: id.clone(),
            platform: Platform::Slack,
            display_name: arc_str(display_name),
            avatar: None,
        };

        Ok(Self {
            id,
            account: RwLock::new(account),
            options: RwLock::new(options),
            capabilities: RwLock::new(capabilities),
            connection: RwLock::new(SlackConnectionState::default()),
            chats: RwLock::new(Vec::new()),
            users: Arc::new(RwLock::new(HashMap::new())),
            chat_members: Arc::new(RwLock::new(HashMap::new())),
            api_client,
            events: EventBus::new(),
            connected: AtomicBool::new(false),
            realtime_task: RwLock::new(None),
            history_poll_task: RwLock::new(None),
            dm_poll_task: RwLock::new(None),
        })
    }

    pub fn options(&self) -> SlackProviderOptions {
        read_lock(&self.options).clone()
    }

    pub fn capabilities(&self) -> SlackCapabilities {
        read_lock(&self.capabilities).clone()
    }

    pub fn send_identity(&self) -> SlackSendIdentity {
        self.capabilities().send_identity()
    }

    /// Rebuild the cached account display name and avatar from the latest
    /// connection metadata (real Slack workspace name + icon) without ever
    /// changing the provider id, so stored chats/messages stay associated.
    fn refresh_account_identity(&self) {
        let (display_name, avatar) = {
            let options = read_lock(&self.options);
            let connection = read_lock(&self.connection);
            let display_name = slack_account_display_name(
                connection.team_name.as_deref(),
                options.workspace.as_deref(),
                &options.auth_mode,
            );
            let avatar = connection
                .team_icon_url
                .as_deref()
                .and_then(slack_avatar_path);
            (display_name, avatar)
        };
        *write_lock(&self.account) = Account {
            id: self.id.clone(),
            platform: Platform::Slack,
            display_name: arc_str(display_name),
            avatar,
        };
    }

    pub async fn validate_submission(
        &self,
        submission: AuthSubmission,
    ) -> Result<SlackCapabilities> {
        let submission = self.resolve_oauth_submission(submission).await?;
        let options = self.options_for_submission(submission)?;
        let validated = self
            .call_api(
                NetworkActivityKind::Auth,
                Self::validate_options_with_client(&*self.api_client, &options),
            )
            .await;
        match validated {
            Ok(validated) => {
                let supports_realtime = options.auth_mode.supports_realtime();
                *write_lock(&self.options) = options;
                *write_lock(&self.capabilities) = validated.capabilities.clone();
                *write_lock(&self.connection) = validated.connection;
                self.refresh_account_identity();
                let account = self.account_info();
                slack_diagnostic_log(
                    "slack.provider.account_identity",
                    format!(
                        "id={} display_name={} avatar={}",
                        account.id,
                        account.display_name,
                        account
                            .avatar
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "<none>".to_owned())
                    ),
                );
                self.connected.store(true, Ordering::Release);
                self.events.send(ProviderEvent::AuthSucceeded);
                self.events.send(ProviderEvent::SyncComplete);
                if validated.capabilities.can_realtime {
                    self.start_realtime();
                    self.start_dm_polling();
                } else if validated.capabilities.can_read_history {
                    self.start_history_polling();
                } else if supports_realtime {
                    self.start_history_polling_with_realtime_notice();
                }
                Ok(validated.capabilities)
            }
            Err(error) => {
                self.connected.store(false, Ordering::Release);
                let reason = sanitize_slack_error(&error);
                self.events
                    .send(ProviderEvent::Disconnected(arc_opt(reason.clone())));
                Err(anyhow!(reason))
            }
        }
    }

    /// If the submission carries an OAuth authorization `code`, exchange it for
    /// real Slack tokens via `oauth.v2.access` and fold the results into the
    /// submission (user/bot tokens + workspace name) so the rest of the
    /// connection flow proceeds exactly like a token submission. Submissions
    /// without a code are returned unchanged.
    async fn resolve_oauth_submission(
        &self,
        mut submission: AuthSubmission,
    ) -> Result<AuthSubmission> {
        submission = self.maybe_run_browser_oauth_login(submission).await?;
        let Some(code) = submission
            .oauth_code
            .as_deref()
            .map(str::trim)
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
        else {
            return Ok(submission);
        };

        let options = self.options();
        let official = official_slack_app();
        let client_id = submission
            .client_id
            .clone()
            .or_else(|| options.client_id.clone())
            .or_else(|| official.as_ref().map(|app| app.client_id.clone()))
            .and_then(non_empty_string)
            .ok_or_else(|| {
                anyhow!("Slack OAuth requires a client ID before exchanging the authorization code")
            })?;
        let client_secret = submission
            .client_secret
            .clone()
            .or_else(|| options.client_secret.clone())
            .or_else(|| official.as_ref().map(|app| app.client_secret.clone()))
            .and_then(non_empty_string)
            .ok_or_else(|| {
                anyhow!(
                    "Slack OAuth requires a client secret before exchanging the authorization code"
                )
            })?;
        let redirect_uri = submission
            .redirect_uri
            .clone()
            .or_else(|| options.redirect_uri.clone())
            .or_else(|| official.as_ref().map(|app| app.redirect_uri.clone()))
            .and_then(non_empty_string)
            .unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned());

        let tokens = self
            .call_api(
                NetworkActivityKind::Auth,
                self.api_client.exchange_oauth_code(
                    &client_id,
                    &client_secret,
                    &redirect_uri,
                    &code,
                ),
            )
            .await?;

        // Never overwrite a token the user explicitly pasted alongside a code.
        if submission.user_token.is_none() {
            submission.user_token = tokens.user_token;
        }
        if submission.bot_token.is_none() {
            submission.bot_token = tokens.bot_token;
        }
        if non_empty_option(&submission.workspace_label).is_none() {
            submission.workspace_label = tokens.team_name;
        }
        // The code is single-use; clear it so downstream option building treats
        // this as a normal token submission.
        submission.oauth_code = None;
        Ok(submission)
    }

    /// If this is a browser-login-capable OAuth submission (a user-OAuth mode
    /// with client credentials but no pasted token or code yet), run the
    /// loopback browser login: bind the callback listener, emit an `OAuthUrl`
    /// challenge so the UI opens the browser, then wait for Slack to redirect
    /// back with an authorization `code`. The code is folded into the
    /// submission so the existing `oauth.v2.access` exchange path completes the
    /// login without the user copy/pasting anything. Submissions that already
    /// carry a code/token, or that lack a client ID/secret, are returned
    /// unchanged so the manual paths still work.
    async fn maybe_run_browser_oauth_login(
        &self,
        mut submission: AuthSubmission,
    ) -> Result<AuthSubmission> {
        if non_empty_option(&submission.oauth_code).is_some() {
            return Ok(submission);
        }
        // A pasted token means the user opted into the manual path.
        if non_empty_option(&submission.user_token).is_some()
            || non_empty_option(&submission.bot_token).is_some()
        {
            return Ok(submission);
        }

        let options = self.options();
        let official = official_slack_app();
        let mode = match submission.mode.clone() {
            Some(mode) => SlackAuthMode::try_from(mode)?,
            None => options.auth_mode.clone(),
        };
        if !mode.supports_oauth_code_exchange() {
            return Ok(submission);
        }

        let Some(client_id) = submission
            .client_id
            .clone()
            .or_else(|| options.client_id.clone())
            .or_else(|| official.as_ref().map(|app| app.client_id.clone()))
            .and_then(non_empty_string)
        else {
            return Ok(submission);
        };
        // Without a client secret we cannot exchange the code, so leave the
        // submission untouched and let validation surface the missing secret.
        if submission
            .client_secret
            .clone()
            .or_else(|| options.client_secret.clone())
            .or_else(|| official.as_ref().map(|app| app.client_secret.clone()))
            .and_then(non_empty_string)
            .is_none()
        {
            return Ok(submission);
        }

        // Resolve the redirect URI to advertise to Slack. Slack requires HTTPS
        // for distributed apps, so a distributor-configured HTTPS relay (via
        // `redirect_uri`) takes precedence; otherwise this falls back to the
        // loopback URL for single-workspace/dev use. The same value is reused in
        // the token exchange because Slack matches `redirect_uri` exactly.
        let advertised_redirect_uri = submission
            .redirect_uri
            .clone()
            .or_else(|| options.redirect_uri.clone())
            .or_else(|| official.as_ref().map(|app| app.redirect_uri.clone()))
            .and_then(non_empty_string)
            .unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned());

        let flow = begin_slack_oauth_login(
            &client_id,
            &advertised_redirect_uri,
            mode.bot_scopes(),
            mode.user_scopes(),
        )?;
        let redirect_uri = flow.redirect_uri().to_owned();
        slack_diagnostic_log(
            "slack.oauth.browser_login.start",
            format!("mode={} redirect_uri={redirect_uri}", mode.label()),
        );
        // Ask the UI to open the browser at the authorize URL while we wait for
        // the loopback redirect. This is a non-blocking channel send.
        self.events
            .send(ProviderEvent::AuthRequired(AuthChallenge::OAuthUrl(
                arc_str(flow.authorize_url()),
            )));

        let code = flow.wait_for_authorization_code().await?;
        slack_diagnostic_log("slack.oauth.browser_login.code_received", "ok");
        submission.oauth_code = Some(code);
        submission.redirect_uri = Some(redirect_uri);
        Ok(submission)
    }

    fn options_for_submission(&self, submission: AuthSubmission) -> Result<SlackProviderOptions> {
        let mut options = self.options();
        if let Some(workspace_label) = non_empty_option(&submission.workspace_label) {
            options.workspace = Some(workspace_label);
        }
        if let Some(mode) = submission.mode {
            options.auth_mode = SlackAuthMode::try_from(mode)?;
        }
        if submission.client_id.is_some() {
            options.client_id = submission.client_id;
        }
        if submission.client_secret.is_some() {
            options.client_secret = submission.client_secret;
        }
        if submission.redirect_uri.is_some() {
            options.redirect_uri = submission.redirect_uri;
        }
        if submission.user_token.is_some() {
            options.user_token = submission.user_token;
        }
        if submission.bot_token.is_some() {
            options.bot_token = submission.bot_token;
        }
        if submission.app_token.is_some() {
            options.app_token = submission.app_token;
        }
        if submission.webhook_url.is_some() {
            options.webhook_url = submission.webhook_url;
        }
        // Any `oauth_code` is consumed earlier by `resolve_oauth_submission`,
        // which turns it into real tokens before this runs.
        Ok(options)
    }

    pub fn setup_options_in_robustness_order() -> Vec<SlackSetupOption> {
        vec![
            SlackSetupOption::new(
                SlackAuthMode::UserOAuth,
                "User OAuth",
                "Recommended: send as yourself when your workspace grants user write scopes.",
            ),
            SlackSetupOption::new(
                SlackAuthMode::ReadOnlyOAuth,
                "User OAuth read-only",
                "Fallback: read conversations when write scopes are blocked or unavailable.",
            ),
            SlackSetupOption::new(
                SlackAuthMode::BotToken,
                "Workspace-approved bot/app tokens",
                "Robust approved deployment with bot identity and optional Socket Mode.",
            ),
            SlackSetupOption::new(
                SlackAuthMode::ImportedToken,
                "Existing approved token import",
                "Advanced: validate a pre-issued Slack token and use its actual capabilities.",
            ),
            SlackSetupOption::new(
                SlackAuthMode::ManualApp,
                "Manual Slack app setup",
                "Advanced: create or configure a Slack app, OAuth redirect, and requested scopes.",
            ),
            SlackSetupOption::new(
                SlackAuthMode::Webhook,
                "Incoming webhook",
                "Limited fallback: send-only posting as webhook/app identity, no inbox.",
            ),
        ]
    }

    fn auth_challenge(&self) -> AuthChallenge {
        let options = self.options();
        match options.auth_mode {
            SlackAuthMode::UserOAuth | SlackAuthMode::ReadOnlyOAuth => {
                AuthChallenge::OAuthUrl(arc_str(self.oauth_setup_url()))
            }
            SlackAuthMode::ManualApp => {
                AuthChallenge::OAuthUrl(arc_str(slack_app_manifest_url(&options.auth_mode)))
            }
            SlackAuthMode::BotToken | SlackAuthMode::ImportedToken | SlackAuthMode::Webhook => {
                AuthChallenge::Waiting
            }
        }
    }

    fn oauth_setup_url(&self) -> String {
        let options = self.options();
        let official = official_slack_app();
        let client_id = options
            .client_id
            .as_deref()
            .and_then(|value| non_empty_string(value.to_owned()))
            .or_else(|| official.as_ref().map(|app| app.client_id.clone()));
        let redirect_uri = options
            .redirect_uri
            .as_deref()
            .and_then(|value| non_empty_string(value.to_owned()))
            .or_else(|| official.as_ref().map(|app| app.redirect_uri.clone()))
            .unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned());

        match client_id {
            Some(client_id) => format!(
                "https://slack.com/oauth/v2/authorize?client_id={}&scope={}&user_scope={}&redirect_uri={}",
                url_component(&client_id),
                url_component(options.auth_mode.bot_scopes()),
                url_component(options.auth_mode.user_scopes()),
                url_component(&redirect_uri)
            ),
            None => slack_app_manifest_url(&options.auth_mode),
        }
    }

    async fn validate_connection(&self) -> Result<SlackValidatedConnection> {
        let options = self.options();
        self.call_api(
            NetworkActivityKind::Auth,
            Self::validate_options_with_client(&*self.api_client, &options),
        )
        .await
    }

    async fn validate_options_with_client(
        api_client: &dyn SlackApiClient,
        options: &SlackProviderOptions,
    ) -> Result<SlackValidatedConnection> {
        let mut capabilities = SlackCapabilities::default();
        let mut connection = SlackConnectionState::default();

        if options.auth_mode.accepts_user_token() {
            if let Some(user_token) = non_empty_option(&options.user_token) {
                let credential = SlackCredential::new(SlackCredentialKind::UserToken, user_token);
                let validated = api_client.validate_token(credential.clone()).await?;
                capabilities.apply_validated_credential(&options.auth_mode, &validated);
                connection.remember_validated_credential(&credential, &validated);
            }
        }

        if options.auth_mode.accepts_bot_token() {
            if let Some(bot_token) = non_empty_option(&options.bot_token) {
                let credential = SlackCredential::new(SlackCredentialKind::BotToken, bot_token);
                let validated = api_client.validate_token(credential.clone()).await?;
                capabilities.apply_validated_credential(&options.auth_mode, &validated);
                connection.remember_validated_credential(&credential, &validated);
            }
        }

        if options.auth_mode.accepts_app_token() {
            if let Some(app_token) = non_empty_option(&options.app_token) {
                let credential = SlackCredential::new(SlackCredentialKind::AppToken, app_token);
                let validated = api_client.validate_token(credential.clone()).await;
                match validated {
                    Ok(validated) => {
                        capabilities.apply_validated_credential(&options.auth_mode, &validated);
                        connection.remember_validated_credential(&credential, &validated);
                    }
                    Err(error) if capabilities.has_non_realtime() => {
                        let _ = sanitize_slack_error(&error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        if options.auth_mode.accepts_webhook() {
            if let Some(webhook_url) = non_empty_option(&options.webhook_url) {
                let validation = api_client.validate_webhook(&webhook_url).await?;
                capabilities.can_send_webhook = true;
                connection.webhook_url = Some(webhook_url);
                if connection.team_name.is_none() {
                    connection.team_name = non_empty_string(validation.url_host);
                }
            }
        }

        if connection.is_empty() {
            bail!(
                "Slack {} mode has no configured credential for that mode",
                options.auth_mode
            );
        }

        if !capabilities.has_any() {
            bail!(
                "Slack credential was valid but does not provide usable capabilities in {} mode",
                options.auth_mode
            );
        }

        capabilities.requires_admin_approval = false;

        // Best-effort: enrich the connection with the real Slack workspace name
        // and icon. Failures (e.g. missing team:read scope on one token) are
        // non-fatal, but try every configured Web API credential before giving
        // up so mixed user/bot setups still get the workspace icon. Pass the
        // validated team id explicitly because some token/app combinations need
        // it to return complete team metadata.
        let mut team_info_errors = Vec::new();
        let team_id = connection.team_id.clone();
        for credential in connection.web_api_credentials() {
            let credential_kind = credential.kind.clone();
            match api_client.team_info(credential, team_id.as_deref()).await {
                Ok(Some(team)) => {
                    let team_id_label = team.id.as_deref().unwrap_or("<none>").to_owned();
                    let has_icon = team.icon_url.is_some();
                    if let Some(name) = team
                        .display_name()
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                    {
                        connection.team_name = Some(name.to_owned());
                    }
                    if connection.team_icon_url.is_none() {
                        connection.team_icon_url = team.icon_url;
                    }
                    slack_diagnostic_log(
                        "slack.provider.team_info",
                        format!(
                            "credential={:?} requested_team_id={} returned_team_id={} has_icon={}",
                            credential_kind,
                            team_id.as_deref().unwrap_or("<none>"),
                            team_id_label,
                            has_icon
                        ),
                    );
                    if connection.team_icon_url.is_some() {
                        break;
                    }
                }
                Ok(None) => slack_diagnostic_log(
                    "slack.provider.team_info",
                    format!(
                        "credential={:?} requested_team_id={} result=no_team",
                        credential_kind,
                        team_id.as_deref().unwrap_or("<none>")
                    ),
                ),
                Err(error) => team_info_errors.push(sanitize_slack_error(&error)),
            }
        }

        if connection.team_icon_url.is_none() && !team_info_errors.is_empty() {
            slack_diagnostic_log(
                "slack.provider.team_info_failed",
                team_info_errors.join("; "),
            );
        }

        Ok(SlackValidatedConnection {
            capabilities,
            connection,
        })
    }

    async fn load_chats(&self) -> Result<Vec<Chat>> {
        let credential = read_lock(&self.connection)
            .read_credential()
            .ok_or_else(|| self.unsupported("conversation listing"))?;
        let conversations = self
            .call_api(
                NetworkActivityKind::History,
                self.api_client
                    .list_member_conversations(credential.clone(), SlackMemberScope::Sidebar),
            )
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        let mut chats = conversations
            .into_iter()
            .filter(|conversation| include_conversation_in_sidebar(conversation))
            .filter_map(|conversation| self.chat_from_conversation(conversation))
            .collect::<Vec<_>>();

        self.apply_cached_user_names(&mut chats);
        self.queue_chat_name_resolution(credential, &chats);

        sort_chats(&mut chats);
        *write_lock(&self.chats) = chats.clone();
        Ok(chats)
    }

    fn apply_cached_user_names(&self, chats: &mut [Chat]) {
        for chat in chats {
            if let Some(user_id) = direct_chat_user_id(chat)
                && let Some(user) = read_lock(&self.users).get(user_id)
            {
                chat.name = arc_str(user.best_name());
                chat.avatar = user.sender().avatar;
            }
        }
    }

    fn queue_chat_name_resolution(&self, credential: SlackCredential, chats: &[Chat]) {
        let pending = chats
            .iter()
            .filter_map(|chat| {
                direct_chat_user_id(chat).map(|user_id| (chat.clone(), user_id.to_owned()))
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return;
        }

        let api_client = Arc::clone(&self.api_client);
        let users = Arc::clone(&self.users);
        let events = self.events.clone();
        tokio::spawn(async move {
            for (mut chat, user_id) in pending {
                let cached = read_lock(&users).get(&user_id).cloned();
                let user = if cached.is_some() {
                    cached
                } else {
                    events.send(ProviderEvent::NetworkActivity {
                        direction: NetworkActivityDirection::Tx,
                        kind: NetworkActivityKind::Other,
                    });
                    match api_client.user_info(credential.clone(), &user_id).await {
                        Ok(user) => {
                            events.send(ProviderEvent::NetworkActivity {
                                direction: NetworkActivityDirection::Rx,
                                kind: NetworkActivityKind::Other,
                            });
                            if let Some(user) = user.as_ref() {
                                write_lock(&users).insert(user.id.clone(), user.clone());
                            }
                            user
                        }
                        Err(_) => None,
                    }
                };

                if let Some(user) = user {
                    let avatar = user.sender().avatar;
                    if chat.name.as_ref() != user.best_name() || chat.avatar != avatar {
                        chat.name = arc_str(user.best_name());
                        chat.avatar = avatar;
                        events.send(ProviderEvent::ChatUpdated(chat));
                    }
                }
            }
        });
    }

    async fn resolve_user(
        &self,
        credential: &SlackCredential,
        user_id: &str,
    ) -> Result<Option<SlackUser>> {
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return Ok(None);
        }
        if let Some(user) = read_lock(&self.users).get(user_id).cloned() {
            return Ok(Some(user));
        }
        // `users.info` only resolves user ids (U.../W...); bot ids (B...) make
        // it fail with `user_not_found` on every call. Bot display names arrive
        // through message `bot_profile` metadata instead, so synthesize a
        // placeholder rather than burning a doomed API round trip.
        if is_slack_bot_id(user_id) {
            let user = fallback_slack_bot_user(user_id);
            write_lock(&self.users).insert(user.id.clone(), user.clone());
            return Ok(Some(user));
        }

        let user = self
            .call_api(
                NetworkActivityKind::Other,
                self.api_client.user_info(credential.clone(), user_id),
            )
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        let user = user.or_else(|| fallback_slack_user(user_id));
        if let Some(user) = user.as_ref() {
            write_lock(&self.users).insert(user.id.clone(), user.clone());
        }
        Ok(user)
    }

    async fn resolve_chat_member_ids(
        &self,
        credential: &SlackCredential,
        chat_id: &str,
    ) -> Result<Vec<String>> {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(members) = read_lock(&self.chat_members).get(chat_id).cloned() {
            return Ok(members);
        }

        let members = self
            .call_api(
                NetworkActivityKind::Other,
                self.api_client
                    .conversation_members(credential.clone(), chat_id),
            )
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        write_lock(&self.chat_members).insert(chat_id.to_owned(), members.clone());
        Ok(members)
    }

    fn chat_from_conversation(&self, conversation: SlackConversation) -> Option<Chat> {
        chat_from_slack_conversation(&self.id, &conversation)
    }

    fn discovery_result_from_conversation(
        &self,
        conversation: SlackConversation,
    ) -> Option<DiscoveryResult> {
        let chat = self.chat_from_conversation(conversation.clone())?;
        let action = match chat.membership {
            ChatMembership::NotJoined if chat.kind == ChatKind::PublicChannel => {
                DiscoveryAction::JoinRequired
            }
            _ => DiscoveryAction::Open,
        };
        let kind = discovery_kind_from_slack_chat_kind(chat.kind);
        let subtitle = slack_conversation_subtitle(&conversation, chat.membership);
        let mut result = DiscoveryResult::existing_chat(chat);
        result.kind = kind;
        result.action = action;
        result.id = arc_str(format!(
            "slack:destination:{}:{}",
            result.account, result.platform_id
        ));
        result.subtitle = subtitle;
        Some(result)
    }

    async fn send_text_message(
        &self,
        chat_id: &ChatId,
        text: Arc<str>,
        reply_to: Option<&MessageId>,
    ) -> Result<MessageId> {
        let identity = self.send_identity();
        let connection = read_lock(&self.connection).clone();
        let posted = match identity {
            SlackSendIdentity::User => {
                let token = connection
                    .user_token
                    .ok_or_else(|| self.unsupported("user sending"))?;
                self.call_api(
                    NetworkActivityKind::Send,
                    self.api_client.post_message(
                        SlackCredential::new(SlackCredentialKind::UserToken, token),
                        chat_id.as_ref(),
                        text.as_ref(),
                        reply_to.map(|message_id| message_id.as_ref()),
                    ),
                )
                .await
            }
            SlackSendIdentity::Bot => {
                let token = connection
                    .bot_token
                    .ok_or_else(|| self.unsupported("bot sending"))?;
                self.call_api(
                    NetworkActivityKind::Send,
                    self.api_client.post_message(
                        SlackCredential::new(SlackCredentialKind::BotToken, token),
                        chat_id.as_ref(),
                        text.as_ref(),
                        reply_to.map(|message_id| message_id.as_ref()),
                    ),
                )
                .await
            }
            SlackSendIdentity::Webhook => {
                let webhook_url = connection
                    .webhook_url
                    .ok_or_else(|| self.unsupported("webhook sending"))?;
                self.call_api(
                    NetworkActivityKind::Send,
                    self.api_client.post_webhook(&webhook_url, text.as_ref()),
                )
                .await
            }
            SlackSendIdentity::None => Err(self.unsupported("sending")),
        };

        posted
            .map(|message| arc_str(message.ts))
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))
    }

    async fn send_file_message(
        &self,
        chat_id: &ChatId,
        media: Media,
        reply_to: Option<&MessageId>,
    ) -> Result<MessageId> {
        let identity = self.send_identity();
        let connection = read_lock(&self.connection).clone();
        let local_path = media
            .local_path
            .clone()
            .ok_or_else(|| anyhow!("Slack file upload requires a local file path"))?;
        let filename = media.file_name.as_ref().trim();
        let filename = if filename.is_empty() {
            local_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("upload")
                .to_owned()
        } else {
            filename.to_owned()
        };
        let request = SlackUploadFileRequest {
            channel: chat_id.as_ref().to_owned(),
            path: local_path,
            title: filename.clone(),
            filename,
            mime_type: media.mime_type.as_ref().to_owned(),
            initial_comment: media.caption.as_deref().map(str::to_owned),
            thread_ts: reply_to.map(|message_id| message_id.as_ref().to_owned()),
        };
        let uploaded = match identity {
            SlackSendIdentity::User => {
                let token = connection
                    .user_token
                    .ok_or_else(|| self.unsupported("user file upload"))?;
                self.call_api(
                    NetworkActivityKind::Media,
                    self.api_client.upload_file(
                        SlackCredential::new(SlackCredentialKind::UserToken, token),
                        request,
                    ),
                )
                .await
            }
            SlackSendIdentity::Bot => {
                let token = connection
                    .bot_token
                    .ok_or_else(|| self.unsupported("bot file upload"))?;
                self.call_api(
                    NetworkActivityKind::Media,
                    self.api_client.upload_file(
                        SlackCredential::new(SlackCredentialKind::BotToken, token),
                        request,
                    ),
                )
                .await
            }
            SlackSendIdentity::Webhook | SlackSendIdentity::None => {
                Err(self.unsupported("file upload"))
            }
        };

        uploaded
            .map(|file| arc_str(file.id))
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))
    }

    fn start_realtime(&self) {
        let app_token = read_lock(&self.connection).app_token.clone();
        let Some(app_token) = app_token else {
            slack_diagnostic_log(
                "slack.realtime.no_app_token",
                format!("account={}", self.id),
            );
            self.events.send(ProviderEvent::AccountNotice {
                title: arc_str("Slack realtime unavailable"),
                body: arc_str(
                    "Configure an app-level xapp token and enable Socket Mode/Event Subscriptions to receive Slack messages in realtime.",
                ),
                severity: AccountNoticeSeverity::SystemAlert,
            });
            return;
        };

        self.stop_realtime();
        let api_client = self.api_client.clone();
        let events = self.events.clone();
        let account = self.id.clone();
        let connection = read_lock(&self.connection).clone();
        let user_id = connection.user_id.clone();
        let fallback_credential = connection.read_credential();
        let history_user_id = connection.user_id.clone().or(connection.bot_id.clone());
        let users = Arc::clone(&self.users);
        slack_diagnostic_log(
            "slack.realtime.start",
            format!(
                "account={account} user_id={} has_history_fallback={}",
                user_id.as_deref().unwrap_or("<none>"),
                fallback_credential.is_some(),
            ),
        );
        let handle = tokio::spawn(async move {
            run_socket_mode_loop(
                api_client,
                events,
                account,
                user_id,
                app_token,
                users,
                fallback_credential,
                history_user_id,
            )
            .await;
        });
        *write_lock(&self.realtime_task) = Some(handle);
    }

    fn stop_realtime(&self) {
        if let Some(handle) = write_lock(&self.realtime_task).take() {
            handle.abort();
        }
    }

    fn start_history_polling(&self) {
        let connection = read_lock(&self.connection).clone();
        let Some(credential) = connection.read_credential() else {
            return;
        };
        self.stop_history_polling();
        let api_client = self.api_client.clone();
        let events = self.events.clone();
        let account = self.id.clone();
        let current_user_id = connection.user_id.or(connection.bot_id);
        let users = Arc::clone(&self.users);
        let handle = tokio::spawn(async move {
            run_history_poll_loop(
                api_client,
                events,
                account,
                credential,
                current_user_id,
                users,
                Utc::now(),
                None,
            )
            .await;
        });
        *write_lock(&self.history_poll_task) = Some(handle);
    }

    fn start_history_polling_with_realtime_notice(&self) {
        self.events.send(ProviderEvent::AccountNotice {
            title: arc_str("Slack realtime unavailable"),
            body: arc_str(
                "Using periodic Slack history checks because realtime is unavailable. Configure an app-level xapp token and enable Socket Mode/Event Subscriptions for instant Slack messages.",
            ),
            severity: AccountNoticeSeverity::SystemAlert,
        });
        self.start_history_polling();
    }

    fn stop_history_polling(&self) {
        if let Some(handle) = write_lock(&self.history_poll_task).take() {
            handle.abort();
        }
    }

    /// Start the always-on DM poll alongside realtime. Realtime (Socket Mode)
    /// is bot-scoped and never delivers the user's own human-to-human DMs, so
    /// this user-token poll over `im`/`mpim` conversations is the only path
    /// that surfaces them. Bot-only connections have no user token and cannot
    /// see those DMs at all, so the poll skips itself. It is not started when
    /// the full history poll is running, because that already covers DMs.
    fn start_dm_polling(&self) {
        let connection = read_lock(&self.connection).clone();
        let Some(credential) = connection.user_credential() else {
            slack_diagnostic_log(
                "slack.dm_poll.skip",
                format!("account={} reason=no_user_token", self.id),
            );
            return;
        };
        self.stop_dm_polling();
        let api_client = self.api_client.clone();
        let events = self.events.clone();
        let account = self.id.clone();
        let current_user_id = connection.user_id.clone();
        let users = Arc::clone(&self.users);
        let handle = tokio::spawn(async move {
            run_dm_poll_loop(
                api_client,
                events,
                account,
                credential,
                current_user_id,
                users,
                Utc::now(),
            )
            .await;
        });
        *write_lock(&self.dm_poll_task) = Some(handle);
    }

    fn stop_dm_polling(&self) {
        if let Some(handle) = write_lock(&self.dm_poll_task).take() {
            handle.abort();
        }
    }

    fn unsupported(&self, action: &str) -> anyhow::Error {
        let options = self.options();
        anyhow!(
            "Slack {} is not available in {} mode ({})",
            action,
            options.auth_mode,
            self.capabilities().summary()
        )
    }
}

impl fmt::Debug for SlackProviderOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SlackProviderOptions")
            .field("auth_mode", &self.auth_mode)
            .field("client_id", &redacted_option(&self.client_id))
            .field("client_secret", &redacted_option(&self.client_secret))
            .field("redirect_uri", &self.redirect_uri)
            .field("bot_token", &redacted_option(&self.bot_token))
            .field("app_token", &redacted_option(&self.app_token))
            .field("user_token", &redacted_option(&self.user_token))
            .field("webhook_url", &redacted_option(&self.webhook_url))
            .field("workspace", &self.workspace)
            .finish()
    }
}

impl SlackProviderOptions {
    pub fn new(auth_mode: SlackAuthMode) -> Self {
        Self {
            auth_mode,
            client_id: None,
            client_secret: None,
            redirect_uri: None,
            bot_token: None,
            app_token: None,
            user_token: None,
            webhook_url: None,
            workspace: None,
        }
    }

    pub fn derive_capabilities(&self) -> SlackCapabilities {
        let has_user_token = has_value(&self.user_token);
        let has_bot_token = has_value(&self.bot_token);
        let has_app_token = has_value(&self.app_token);
        let has_webhook = has_value(&self.webhook_url);

        match self.auth_mode {
            SlackAuthMode::UserOAuth => SlackCapabilities {
                can_read_history: has_user_token,
                can_send_as_user: has_user_token,
                can_react: has_user_token,
                can_download_files: has_user_token,
                can_search: has_user_token,
                can_realtime: has_app_token,
                requires_admin_approval: !has_user_token,
                ..SlackCapabilities::default()
            },
            SlackAuthMode::ReadOnlyOAuth => SlackCapabilities {
                can_read_history: has_user_token,
                can_download_files: has_user_token,
                can_search: has_user_token,
                can_realtime: has_app_token,
                requires_admin_approval: !has_user_token,
                ..SlackCapabilities::default()
            },
            SlackAuthMode::BotToken => SlackCapabilities {
                can_read_history: has_bot_token,
                can_send_as_bot: has_bot_token,
                can_react: has_bot_token,
                can_download_files: has_bot_token,
                can_search: has_bot_token,
                can_realtime: has_app_token,
                requires_admin_approval: !has_bot_token,
                ..SlackCapabilities::default()
            },
            SlackAuthMode::ImportedToken => SlackCapabilities {
                can_read_history: has_user_token || has_bot_token,
                can_send_as_user: has_user_token,
                can_send_as_bot: has_bot_token,
                can_react: has_user_token || has_bot_token,
                can_download_files: has_user_token || has_bot_token,
                can_realtime: has_app_token,
                can_search: has_user_token || has_bot_token,
                requires_admin_approval: !(has_user_token || has_bot_token),
                ..SlackCapabilities::default()
            },
            SlackAuthMode::ManualApp => SlackCapabilities {
                can_read_history: has_user_token || has_bot_token,
                can_send_as_user: has_user_token,
                can_send_as_bot: has_bot_token,
                can_react: has_user_token || has_bot_token,
                can_download_files: has_user_token || has_bot_token,
                can_realtime: has_app_token,
                can_search: has_user_token || has_bot_token,
                requires_admin_approval: !(has_user_token || has_bot_token),
                ..SlackCapabilities::default()
            },
            SlackAuthMode::Webhook => SlackCapabilities {
                can_send_webhook: has_webhook,
                requires_admin_approval: !has_webhook,
                ..SlackCapabilities::default()
            },
        }
    }

    pub fn has_configured_credentials(&self) -> bool {
        match self.auth_mode {
            SlackAuthMode::UserOAuth | SlackAuthMode::ReadOnlyOAuth => has_value(&self.user_token),
            SlackAuthMode::BotToken => has_value(&self.bot_token),
            SlackAuthMode::ImportedToken | SlackAuthMode::ManualApp => {
                has_value(&self.user_token)
                    || has_value(&self.bot_token)
                    || has_value(&self.app_token)
            }
            SlackAuthMode::Webhook => has_value(&self.webhook_url),
        }
    }
}

impl SlackCapabilities {
    fn apply_validated_credential(
        &mut self,
        auth_mode: &SlackAuthMode,
        credential: &SlackValidatedCredential,
    ) {
        match (&credential.kind, auth_mode) {
            (SlackCredentialKind::UserToken, SlackAuthMode::UserOAuth) => {
                self.can_read_history = true;
                self.can_send_as_user = true;
                self.can_react = true;
                self.can_download_files = true;
                self.can_search = true;
            }
            (SlackCredentialKind::UserToken, SlackAuthMode::ReadOnlyOAuth) => {
                self.can_read_history = true;
                self.can_download_files = true;
                self.can_search = true;
            }
            (
                SlackCredentialKind::UserToken,
                SlackAuthMode::ImportedToken | SlackAuthMode::ManualApp,
            ) => {
                self.can_read_history = true;
                self.can_send_as_user = true;
                self.can_react = true;
                self.can_download_files = true;
                self.can_search = true;
            }
            (SlackCredentialKind::BotToken, SlackAuthMode::BotToken)
            | (SlackCredentialKind::BotToken, SlackAuthMode::ImportedToken)
            | (SlackCredentialKind::BotToken, SlackAuthMode::ManualApp) => {
                self.can_read_history = true;
                self.can_send_as_bot = true;
                self.can_react = true;
                self.can_download_files = true;
                self.can_search = true;
            }
            (SlackCredentialKind::AppToken, mode) if mode.accepts_app_token() => {
                self.can_realtime = true;
            }
            (SlackCredentialKind::Webhook, SlackAuthMode::Webhook) => {
                self.can_send_webhook = true;
            }
            _ => {}
        }
    }

    fn has_any(&self) -> bool {
        self.has_non_realtime() || self.can_realtime || self.can_search
    }

    fn has_non_realtime(&self) -> bool {
        self.can_read_history
            || self.can_send_as_user
            || self.can_send_as_bot
            || self.can_send_webhook
            || self.can_react
            || self.can_download_files
    }

    pub fn send_identity(&self) -> SlackSendIdentity {
        if self.can_send_as_user {
            SlackSendIdentity::User
        } else if self.can_send_as_bot {
            SlackSendIdentity::Bot
        } else if self.can_send_webhook {
            SlackSendIdentity::Webhook
        } else {
            SlackSendIdentity::None
        }
    }

    pub fn summary(&self) -> &'static str {
        match self.send_identity() {
            SlackSendIdentity::User => "sends as user",
            SlackSendIdentity::Bot if self.can_read_history => "bot mode with inbox access",
            SlackSendIdentity::Bot => "send-only app identity",
            SlackSendIdentity::Webhook => "send-only webhook identity",
            SlackSendIdentity::None if self.can_read_history => "read-only",
            SlackSendIdentity::None => "setup required",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackSetupOption {
    pub mode: SlackAuthMode,
    pub title: &'static str,
    pub description: &'static str,
}

impl SlackSetupOption {
    fn new(mode: SlackAuthMode, title: &'static str, description: &'static str) -> Self {
        Self {
            mode,
            title,
            description,
        }
    }
}

impl SlackAuthMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::UserOAuth => "user-oauth",
            Self::ReadOnlyOAuth => "read-only-oauth",
            Self::BotToken => "bot-token",
            Self::ImportedToken => "imported-token",
            Self::ManualApp => "manual-app",
            Self::Webhook => "webhook",
        }
    }

    fn user_scopes(&self) -> &'static str {
        match self {
            Self::ReadOnlyOAuth => {
                "team:read,users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,files:read,search:read"
            }
            Self::UserOAuth | Self::ManualApp | Self::ImportedToken => {
                "team:read,users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,chat:write,reactions:read,reactions:write,files:read,files:write,search:read"
            }
            Self::BotToken | Self::Webhook => "",
        }
    }

    fn bot_scopes(&self) -> &'static str {
        match self {
            Self::BotToken | Self::ManualApp => {
                "team:read,users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,chat:write,reactions:read,reactions:write,files:read,files:write"
            }
            Self::UserOAuth | Self::ReadOnlyOAuth | Self::ImportedToken | Self::Webhook => "",
        }
    }

    fn accepts_user_token(&self) -> bool {
        matches!(
            self,
            Self::UserOAuth | Self::ReadOnlyOAuth | Self::ImportedToken | Self::ManualApp
        )
    }

    fn accepts_bot_token(&self) -> bool {
        matches!(self, Self::BotToken | Self::ImportedToken | Self::ManualApp)
    }

    fn accepts_app_token(&self) -> bool {
        matches!(
            self,
            Self::UserOAuth
                | Self::ReadOnlyOAuth
                | Self::BotToken
                | Self::ImportedToken
                | Self::ManualApp
        )
    }

    fn supports_realtime(&self) -> bool {
        self.accepts_app_token()
    }

    fn supports_oauth_code_exchange(&self) -> bool {
        matches!(self, Self::UserOAuth | Self::ReadOnlyOAuth)
    }

    fn accepts_webhook(&self) -> bool {
        matches!(self, Self::Webhook)
    }
}

impl TryFrom<AuthSubmissionMode> for SlackAuthMode {
    type Error = anyhow::Error;

    fn try_from(value: AuthSubmissionMode) -> Result<Self, Self::Error> {
        match value {
            AuthSubmissionMode::UserOAuth => Ok(Self::UserOAuth),
            AuthSubmissionMode::ReadOnlyOAuth => Ok(Self::ReadOnlyOAuth),
            AuthSubmissionMode::BotToken => Ok(Self::BotToken),
            AuthSubmissionMode::ImportedToken => Ok(Self::ImportedToken),
            AuthSubmissionMode::ManualApp => Ok(Self::ManualApp),
            AuthSubmissionMode::Webhook => Ok(Self::Webhook),
            AuthSubmissionMode::ProviderSpecific(value) => Self::from_str(value.as_ref()),
        }
    }
}

impl fmt::Display for SlackAuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

impl FromStr for SlackAuthMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "user-oauth" | "user" | "oauth" => Ok(Self::UserOAuth),
            "read-only-oauth" | "readonly-oauth" | "read-only" | "readonly" => {
                Ok(Self::ReadOnlyOAuth)
            }
            "bot-token" | "bot" => Ok(Self::BotToken),
            "imported-token" | "import" | "token" => Ok(Self::ImportedToken),
            "manual-app" | "manual" | "app" => Ok(Self::ManualApp),
            "webhook" | "incoming-webhook" => Ok(Self::Webhook),
            other => bail!(
                "unsupported Slack auth mode '{other}'. Expected one of: user-oauth, read-only-oauth, bot-token, imported-token, manual-app, webhook"
            ),
        }
    }
}

#[async_trait]
impl Provider for SlackProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn platform(&self) -> Platform {
        Platform::Slack
    }

    fn account_info(&self) -> Account {
        read_lock(&self.account).clone()
    }

    fn config_json(&self) -> Option<String> {
        serde_json::to_string(&*read_lock(&self.options)).ok()
    }

    fn outbound_capabilities(&self) -> OutboundCapabilities {
        match self.send_identity() {
            SlackSendIdentity::User | SlackSendIdentity::Bot => OutboundCapabilities {
                text: true,
                image: true,
                gif: true,
                video: true,
                audio: true,
                file: true,
                sticker: false,
                mentions: true,
                edit: true,
                edit_window: None,
                max_upload_size: None,
                media_note: Some(Arc::from(
                    "Slack uploads files for image, GIF, video, audio, and document content",
                )),
            },
            SlackSendIdentity::Webhook => OutboundCapabilities::text_only(
                "incoming webhooks can only post text through this app",
            ),
            SlackSendIdentity::None => OutboundCapabilities::text_only(
                "configure a Slack user token, bot token, or webhook before sending",
            ),
        }
    }

    fn discovery_capabilities(&self) -> DiscoveryCapabilities {
        let can_read = self.capabilities().can_read_history;
        DiscoveryCapabilities {
            existing_chats: can_read,
            contacts: false,
            users: can_read,
            public_channels: can_read,
            private_channels: can_read,
            open_dm: false,
            join_public_channel: false,
        }
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }

        let options = self.options();
        if !options.has_configured_credentials() {
            self.events
                .send(ProviderEvent::AuthRequired(self.auth_challenge()));
            return Ok(());
        }

        let result = match self.validate_connection().await {
            Ok(validated) => {
                let capabilities = validated.capabilities.clone();
                *write_lock(&self.capabilities) = capabilities.clone();
                *write_lock(&self.connection) = validated.connection;
                self.refresh_account_identity();
                let account = self.account_info();
                slack_diagnostic_log(
                    "slack.provider.account_identity",
                    format!(
                        "id={} display_name={} avatar={}",
                        account.id,
                        account.display_name,
                        account
                            .avatar
                            .as_ref()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "<none>".to_owned())
                    ),
                );
                self.connected.store(true, Ordering::Release);
                self.events.send(ProviderEvent::AuthSucceeded);
                self.events.send(ProviderEvent::SyncComplete);
                slack_diagnostic_log(
                    "slack.connect.realtime_decision",
                    format!(
                        "account={} can_realtime={} can_read_history={} has_app_token={} supports_realtime={}",
                        self.id,
                        capabilities.can_realtime,
                        capabilities.can_read_history,
                        read_lock(&self.connection).app_token.is_some(),
                        options.auth_mode.supports_realtime(),
                    ),
                );
                if capabilities.can_realtime {
                    self.start_realtime();
                    self.start_dm_polling();
                } else if capabilities.can_read_history {
                    self.start_history_polling();
                } else if options.auth_mode.supports_realtime() && capabilities.has_non_realtime() {
                    self.start_history_polling_with_realtime_notice();
                }
                Ok(())
            }
            Err(error) => {
                self.connected.store(false, Ordering::Release);
                let reason = sanitize_slack_error(&error);
                self.events
                    .send(ProviderEvent::Disconnected(arc_opt(reason.clone())));
                Err(anyhow!(reason))
            }
        };

        result
    }

    async fn disconnect(&self) -> Result<()> {
        self.stop_realtime();
        self.stop_history_polling();
        self.stop_dm_polling();
        self.connected.store(false, Ordering::Release);
        *write_lock(&self.connection) = SlackConnectionState::default();
        *write_lock(&self.chats) = Vec::new();
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
        if self.capabilities().can_read_history {
            self.load_chats().await
        } else {
            Err(self.unsupported("conversation history"))
        }
    }

    async fn history(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        if self.capabilities().can_read_history {
            let connection = read_lock(&self.connection).clone();
            let credential = connection
                .read_credential()
                .ok_or_else(|| self.unsupported("message history"))?;
            let current_user_id = connection
                .user_id
                .as_deref()
                .or(connection.bot_id.as_deref());
            let mut messages = self
                .call_api(
                    NetworkActivityKind::History,
                    self.api_client.history(
                        credential.clone(),
                        &self.id,
                        current_user_id,
                        chat_id,
                        before,
                        limit,
                        Some(Arc::clone(&self.users)),
                    ),
                )
                .await
                .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;

            for user_id in unresolved_slack_user_ids(&messages) {
                let _ = self.resolve_user(&credential, &user_id).await;
            }

            for message in &mut messages {
                apply_cached_user_to_message(&self.users, message);
            }

            Ok(messages)
        } else {
            Err(self.unsupported("message history"))
        }
    }

    fn encode_outbound_mentions(
        &self,
        text: &str,
        members: &[ChatMember],
        picks: &[Mention],
    ) -> OutboundMentions {
        if !self.outbound_capabilities().mentions {
            return OutboundMentions {
                text: text.to_owned(),
                mentioned: Vec::new(),
            };
        }
        // Broadcast tokens first, so `@here`/`@channel`/`@everyone` become
        // `<!here>` etc. before name resolution runs.
        let text = rewrite_slack_broadcast_mentions(text);
        let resolved = resolve_mention_tokens(&text, members, picks);
        let mut mentioned: Vec<Mention> = Vec::new();
        for item in &resolved {
            if !mentioned
                .iter()
                .any(|existing| existing.platform_id == item.mention.platform_id)
            {
                mentioned.push(item.mention.clone());
            }
        }
        let text = rewrite_mention_tokens(&text, &resolved, |mention| {
            format!("<@{}>", mention.platform_id)
        });
        OutboundMentions { text, mentioned }
    }

    async fn send(
        &self,
        chat_id: &ChatId,
        outbound: OutboundContent,
        reply_to: Option<&Message>,
    ) -> Result<MessageId> {
        let OutboundContent { content, .. } = outbound;
        let reply_to = reply_to.map(|message| &message.id);
        match content {
            Content::Text(text) => {
                let text = Arc::from(chat_core::markup::markdown_to_chat_markup(&text));
                self.send_text_message(chat_id, text, reply_to).await
            }
            Content::Image(media)
            | Content::Video(media)
            | Content::Audio(media)
            | Content::File(media)
            | Content::Sticker(media) => self.send_file_message(chat_id, media, reply_to).await,
            Content::LinkPreview(_) | Content::Cards(_) => Err(anyhow!(
                "Slack link preview/card sending should be sent as plain text first"
            )),
            Content::Poll(_) => Err(anyhow!("Slack poll sending is not implemented yet")),
            Content::Deleted => Err(anyhow!("cannot send deleted Slack message content")),
            Content::Unsupported(kind) => {
                Err(anyhow!("cannot send unsupported Slack content: {kind}"))
            }
        }
    }

    async fn download_media(&self, media: &Media) -> Result<PathBuf> {
        if !self.capabilities().can_download_files {
            return Err(self.unsupported("file download"));
        }
        let url = media.id.trim().to_owned();
        if !url.starts_with("https://") {
            bail!("Slack media id is not a downloadable URL");
        }
        let path = slack_media_cache_file_path(&url, "files")
            .ok_or_else(|| anyhow!("cannot determine Slack media cache path"))?;
        if path.exists() {
            return Ok(path);
        }
        let connection = read_lock(&self.connection).clone();
        let token = connection
            .read_credential()
            .ok_or_else(|| self.unsupported("file download"))?
            .value;

        // User-initiated retrieval retries through the failure cooldown, but
        // never races an in-flight download for the same URL.
        if !slack_begin_media_download(&url, true) {
            bail!("this file is already being downloaded");
        }
        let download_path = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = slack_download_media_to_path(
                &url,
                Some(&token),
                &download_path,
                SLACK_MEDIA_ON_DEMAND_LIMIT_BYTES,
            );
            slack_finish_media_download(&url, result.is_ok());
            match &result {
                Ok(()) => slack_diagnostic_log(
                    "slack.provider.media_retrieved",
                    format!("url={} path={}", url, download_path.display()),
                ),
                Err(error) => slack_diagnostic_log(
                    "slack.provider.media_retrieve_failed",
                    format!(
                        "url={} path={} error={:#}",
                        url,
                        download_path.display(),
                        error
                    ),
                ),
            }
            result
        })
        .await
        .context("joining Slack media download task")?;
        result.map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        Ok(path)
    }

    async fn mark_read(&self, _chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
        if self.capabilities().can_read_history {
            Ok(())
        } else {
            Err(self.unsupported("read receipts"))
        }
    }

    async fn react(&self, chat_id: &ChatId, message: &Message, emoji: &str) -> Result<()> {
        if !self.capabilities().can_react {
            return Err(self.unsupported("reactions"));
        }

        let connection = read_lock(&self.connection).clone();
        let credential = connection
            .read_credential()
            .ok_or_else(|| self.unsupported("reactions"))?;
        let slack_data = message
            .platform_data
            .slack
            .as_ref()
            .ok_or_else(|| anyhow!("selected message is not a Slack message"))?;
        let channel = slack_data.channel.as_ref();
        let timestamp = slack_data.ts.as_ref();
        let emoji = normalize_slack_reaction_name(emoji)
            .ok_or_else(|| anyhow!("Slack reaction emoji cannot be empty"))?;
        let current_user_id = connection
            .user_id
            .as_deref()
            .or(connection.bot_id.as_deref());
        let already_reacted = current_user_id.is_some_and(|current_user_id| {
            message.reactions.iter().any(|reaction| {
                slack_reaction_matches(reaction.emoji.as_ref(), &emoji)
                    && reaction
                        .senders
                        .iter()
                        .any(|sender| sender.as_ref() == current_user_id)
            })
        });

        let result = if already_reacted {
            self.call_api(
                NetworkActivityKind::Reaction,
                self.api_client
                    .remove_reaction(credential, channel, timestamp, &emoji),
            )
            .await
        } else {
            self.call_api(
                NetworkActivityKind::Reaction,
                self.api_client
                    .add_reaction(credential, channel, timestamp, &emoji),
            )
            .await
        };
        result.map_err(|error| anyhow!(sanitize_slack_error(&error)))?;

        self.events.send(ProviderEvent::ReactionChanged {
            chat_id: chat_id.clone(),
            message_id: message.id.clone(),
            emoji: arc_str(slack_emoji_display(&emoji)),
            added: !already_reacted,
            sender: current_user_id
                .map(arc_str)
                .unwrap_or_else(|| arc_str("slack")),
        });
        Ok(())
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        message: &Message,
        outbound: OutboundContent,
    ) -> Result<Timestamp> {
        let Content::Text(text) = outbound.content else {
            bail!("only Slack text messages can be edited");
        };
        let text = chat_core::markup::markdown_to_chat_markup(&text);
        let connection = read_lock(&self.connection).clone();
        let credential = match self.send_identity() {
            SlackSendIdentity::User => connection
                .user_token
                .map(|token| SlackCredential::new(SlackCredentialKind::UserToken, token)),
            SlackSendIdentity::Bot => connection
                .bot_token
                .map(|token| SlackCredential::new(SlackCredentialKind::BotToken, token)),
            SlackSendIdentity::Webhook | SlackSendIdentity::None => None,
        }
        .ok_or_else(|| self.unsupported("message editing"))?;
        // Locally sent messages are stored before any realtime echo fills in
        // `platform_data`; their id is the Slack `ts` and the chat is the
        // channel, which is exactly what `chat.update` needs.
        let slack_data = message.platform_data.slack.as_ref();
        let channel = slack_data
            .map(|data| data.channel.as_ref())
            .filter(|channel| !channel.is_empty())
            .unwrap_or(chat_id.as_ref());
        let ts = slack_data
            .map(|data| data.ts.as_ref())
            .filter(|ts| !ts.is_empty())
            .unwrap_or(message.id.as_ref());
        self.call_api(
            NetworkActivityKind::Send,
            self.api_client
                .update_message(credential, channel, ts, text.as_ref()),
        )
        .await
        .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        slack_diagnostic_log(
            "slack.provider.message_edited",
            format!("account={} chat={channel} ts={ts}", self.id),
        );
        Ok(Utc::now())
    }

    async fn submit_auth(&self, submission: AuthSubmission) -> Result<()> {
        self.validate_submission(submission).await.map(|_| ())
    }

    fn has_bundled_oauth_app(&self) -> bool {
        official_slack_app_is_configured()
    }

    fn has_configured_realtime(&self) -> bool {
        self.options()
            .app_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty())
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Message>> {
        if self.capabilities().can_search {
            bail!("Slack message search is not wired yet")
        } else {
            bail!("Slack message search requires search:read scope")
        }
    }

    async fn discover_destinations(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DiscoveryResult>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        for chat in read_lock(&self.chats).iter().cloned() {
            if slack_chat_matches_query(&chat, &query) {
                results.push(DiscoveryResult::existing_chat(chat));
                if results.len() >= limit {
                    return Ok(results);
                }
            }
        }

        let credential = match read_lock(&self.connection).read_credential() {
            Some(credential) => credential,
            None => return Ok(results),
        };

        let conversations = self
            .call_api(
                NetworkActivityKind::Other,
                self.api_client.list_conversations(credential),
            )
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        let existing_chat_ids = results
            .iter()
            .filter_map(|result| result.chat_id.as_deref().map(str::to_owned))
            .collect::<HashSet<_>>();

        for conversation in conversations {
            if results.len() >= limit {
                break;
            }
            if conversation.is_archived || existing_chat_ids.contains(&conversation.id) {
                continue;
            }
            if !slack_conversation_matches_query(&conversation, &query) {
                continue;
            }
            if let Some(result) = self.discovery_result_from_conversation(conversation) {
                results.push(result);
            }
        }

        Ok(results)
    }

    async fn contact_info(&self, platform_id: &PlatformId) -> Result<Option<Sender>> {
        if self.capabilities().can_read_history {
            let connection = read_lock(&self.connection).clone();
            let credential = connection
                .read_credential()
                .ok_or_else(|| self.unsupported("contact lookup"))?;
            self.resolve_user(&credential, platform_id.as_ref())
                .await
                .map(|user| user.map(|user| user.sender()))
        } else {
            Err(self.unsupported("contact lookup"))
        }
    }

    async fn chat_members(&self, chat_id: &ChatId) -> Result<Vec<ChatMember>> {
        if self.capabilities().can_read_history {
            let connection = read_lock(&self.connection).clone();
            let credential = connection
                .read_credential()
                .ok_or_else(|| self.unsupported("member listing"))?;
            let member_ids = self
                .resolve_chat_member_ids(&credential, chat_id.as_ref())
                .await?;
            let mut members = Vec::new();
            for member_id in member_ids {
                if let Some(user) = self.resolve_user(&credential, &member_id).await? {
                    members.push(user.sender());
                } else {
                    members.push(Sender {
                        platform_id: arc_str(&member_id),
                        display_name: arc_str(&member_id),
                        avatar: None,
                    });
                }
            }
            members.sort_by_key(|member| member.display_name.to_ascii_lowercase());
            let own_user_id = connection.user_id.clone();
            Ok(members
                .into_iter()
                .map(|sender| {
                    let is_self = own_user_id
                        .as_deref()
                        .is_some_and(|own| own == sender.platform_id.as_ref());
                    ChatMember::new(sender).as_self(is_self)
                })
                .collect())
        } else {
            Err(self.unsupported("member listing"))
        }
    }

    async fn chat_details(&self, chat_id: &ChatId) -> Result<ChatDetails> {
        if !self.capabilities().can_read_history {
            return Ok(ChatDetails::default());
        }
        let connection = read_lock(&self.connection).clone();
        let Some(credential) = connection.read_credential() else {
            return Ok(ChatDetails::default());
        };
        let Some(conversation) = self
            .api_client
            .conversation_info(credential.clone(), chat_id.as_ref())
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?
        else {
            return Ok(ChatDetails::default());
        };

        let description = non_empty_option(&conversation.topic)
            .or_else(|| non_empty_option(&conversation.purpose))
            .map(|value| arc_str(&value));
        let created_at = conversation
            .created
            .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
        let creator = match conversation.creator.as_deref() {
            Some(creator_id) if !creator_id.trim().is_empty() => self
                .resolve_user(&credential, creator_id)
                .await
                .ok()
                .flatten()
                .map(|user| arc_str(user.best_name()))
                .or_else(|| Some(arc_str(creator_id))),
            _ => None,
        };
        let workspace = self
            .options()
            .workspace
            .as_deref()
            .and_then(|value| non_empty_string(value.to_owned()))
            .map(|value| arc_str(&value));

        Ok(ChatDetails {
            description,
            created_at,
            creator,
            member_count: conversation.num_members,
            admin_count: None,
            workspace,
            is_archived: conversation.is_archived,
            is_externally_shared: conversation.is_ext_shared,
            only_admins_can_send: false,
            only_admins_can_edit: false,
            disappearing_seconds: None,
            facts: Vec::new(),
        })
    }

    async fn contact_profile(&self, platform_id: &PlatformId) -> Result<Option<ContactProfile>> {
        if !self.capabilities().can_read_history {
            return Ok(None);
        }
        let connection = read_lock(&self.connection).clone();
        let Some(credential) = connection.read_credential() else {
            return Ok(None);
        };
        Ok(self
            .resolve_user(&credential, platform_id.as_ref())
            .await?
            .map(|user| user.profile()))
    }
}

fn redacted_option(value: &Option<String>) -> Option<&'static str> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|_| "<redacted>")
}

fn non_empty_option(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn include_conversation_in_sidebar(conversation: &SlackConversation) -> bool {
    if conversation.is_archived {
        return false;
    }
    if conversation.is_im
        || conversation.is_mpim
        || conversation.is_group
        || conversation.is_private
    {
        return true;
    }
    conversation.is_member.unwrap_or(false)
}

fn slack_chat_matches_query(chat: &Chat, query: &str) -> bool {
    chat.name.to_lowercase().contains(query)
        || chat
            .last_message_preview
            .as_deref()
            .is_some_and(|preview| preview.to_lowercase().contains(query))
}

fn slack_conversation_matches_query(conversation: &SlackConversation, query: &str) -> bool {
    conversation_display_name(conversation)
        .to_lowercase()
        .contains(query)
        || conversation
            .name
            .as_deref()
            .is_some_and(|name| name.to_lowercase().contains(query))
        || conversation
            .user
            .as_deref()
            .is_some_and(|user| user.to_lowercase().contains(query))
        || conversation
            .topic
            .as_deref()
            .is_some_and(|topic| topic.to_lowercase().contains(query))
        || conversation
            .purpose
            .as_deref()
            .is_some_and(|purpose| purpose.to_lowercase().contains(query))
}

fn discovery_kind_from_slack_chat_kind(kind: ChatKind) -> DiscoveryResultKind {
    match kind {
        ChatKind::Direct => DiscoveryResultKind::DirectMessage,
        ChatKind::Group => DiscoveryResultKind::Group,
        ChatKind::PublicChannel => DiscoveryResultKind::PublicChannel,
        ChatKind::PrivateChannel => DiscoveryResultKind::PrivateChannel,
        ChatKind::GroupDirectMessage => DiscoveryResultKind::DirectMessage,
    }
}

fn slack_conversation_subtitle(
    conversation: &SlackConversation,
    membership: ChatMembership,
) -> Option<Arc<str>> {
    let mut parts = Vec::new();
    match membership {
        ChatMembership::Joined => parts.push("joined".to_owned()),
        ChatMembership::NotJoined => parts.push("not joined".to_owned()),
        ChatMembership::Unknown => {}
    }
    if let Some(num_members) = conversation.num_members {
        parts.push(format!("{num_members} members"));
    }
    if let Some(topic) = conversation
        .topic
        .as_deref()
        .or(conversation.purpose.as_deref())
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(topic.to_owned());
    }
    if parts.is_empty() {
        None
    } else {
        Some(arc_str(parts.join(" · ")))
    }
}

fn direct_chat_user_id(chat: &Chat) -> Option<&str> {
    if !matches!(chat.kind, ChatKind::Direct) {
        return None;
    }
    chat.name
        .strip_prefix("DM ")
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
}

fn conversation_chat_kind(conversation: &SlackConversation) -> ChatKind {
    if conversation.is_im {
        ChatKind::Direct
    } else if conversation.is_mpim {
        ChatKind::GroupDirectMessage
    } else if conversation.is_private || conversation.is_group {
        ChatKind::PrivateChannel
    } else if conversation.is_channel {
        ChatKind::PublicChannel
    } else {
        ChatKind::Group
    }
}

fn conversation_membership(conversation: &SlackConversation) -> ChatMembership {
    if conversation.is_im
        || conversation.is_mpim
        || conversation.is_group
        || conversation.is_private
    {
        return ChatMembership::Joined;
    }

    match conversation.is_member {
        Some(true) => ChatMembership::Joined,
        Some(false) => ChatMembership::NotJoined,
        None => ChatMembership::Unknown,
    }
}

fn conversation_display_name(conversation: &SlackConversation) -> String {
    if conversation.is_im {
        return conversation
            .name
            .as_deref()
            .or(conversation.user.as_deref())
            .map(|name| format!("DM {name}"))
            .unwrap_or_else(|| format!("DM {}", conversation.id));
    }

    if conversation.is_mpim {
        return conversation
            .name
            .as_deref()
            .and_then(slack_mpim_display_name)
            .unwrap_or_else(|| format!("Group DM {}", conversation.id));
    }

    if let Some(name) = conversation.name.as_deref() {
        if conversation.is_channel && !name.starts_with('#') {
            return format!("#{name}");
        }
        return name.to_owned();
    }

    if let Some(user) = conversation.user.as_deref() {
        return user.to_owned();
    }

    conversation.id.clone()
}

fn slack_mpim_display_name(name: &str) -> Option<String> {
    let mut cleaned = name.trim().trim_start_matches('#').trim();
    while cleaned
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("mpdm-"))
    {
        cleaned = cleaned[5..].trim_start_matches('-').trim();
    }

    if cleaned.is_empty() {
        return None;
    }

    let raw_parts: Vec<&str> = cleaned.split("--").collect();
    let last_index = raw_parts.len().saturating_sub(1);
    let parts = raw_parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let part = part.trim_matches('-').trim();
            let part = if index == last_index {
                strip_slack_mpim_collision_suffix(part)
            } else {
                part
            };
            (!part.is_empty()).then(|| part.to_owned())
        })
        .collect::<Vec<_>>();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn strip_slack_mpim_collision_suffix(value: &str) -> &str {
    let value = value.trim();
    let Some((head, tail)) = value.rsplit_once('-') else {
        return value;
    };

    if !head.is_empty() && !tail.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit()) {
        head.trim()
    } else {
        value
    }
}

fn chat_from_slack_conversation(
    account: &ProviderId,
    conversation: &SlackConversation,
) -> Option<Chat> {
    let id = conversation.id.trim();
    if id.is_empty() || conversation.is_archived {
        return None;
    }

    let name = conversation_display_name(conversation);
    let kind = conversation_chat_kind(conversation);
    let membership = conversation_membership(conversation);

    Some(Chat {
        id: arc_str(id),
        account: account.clone(),
        platform: Platform::Slack,
        name: arc_str(name),
        avatar: None,
        is_group: conversation.is_channel || conversation.is_group || conversation.is_mpim,
        kind,
        membership,
        is_shared: conversation.is_ext_shared,
        unread_count: conversation.unread_count,
        muted: conversation.is_muted,
        pinned: conversation.is_pinned,
        last_message_at: None,
        last_message_preview: None,
        thread_id: None,
    })
}

/// Per-conversation activity snapshot used to skip polling conversations that
/// have not changed since the previous pass. Both fields are reported cheaply by
/// `conversations.list` in a single call, so comparing them lets the poller
/// avoid a `conversations.history` request — and the UI-waking `ChatUpdated`
/// event it triggers — for every quiet conversation. A new message bumps
/// `updated` and/or the unread counter, so an unchanged snapshot reliably means
/// there is nothing new to fetch.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ConversationPollState {
    updated: Option<i64>,
    unread_count: u32,
}

async fn run_history_poll_loop(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    credential: SlackCredential,
    current_user_id: Option<String>,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
    started_at: Timestamp,
    realtime_events_seen: Option<Arc<AtomicBool>>,
) {
    // History polling is a fallback for missing realtime, so it must only
    // surface messages that genuinely arrive *after* the relevant fallback
    // baseline. Gating on a "first poll" flag alone is unsafe: if the baseline
    // history fetch fails (for example a transient decode error), those old
    // messages are not recorded and would later look brand new, notifying the
    // user about long-past conversations. Anchoring to an explicit timestamp
    // makes the baseline robust against such failures. When polling starts
    // after a connected-but-idle realtime socket, callers pass the realtime
    // connection timestamp so messages sent during the idle window are still
    // eligible for delivery.
    //
    // `realtime_events_seen` is Some only when this loop runs as the safety
    // net behind a connected-but-silent realtime socket. In that mode the
    // loop shuts itself down once realtime proves it delivers events, and it
    // alerts the user only on hard evidence of missed realtime messages (see
    // `should_alert_undelivered_realtime_messages`).
    let mut seen_message_ids = HashSet::new();
    // Conversations whose history can never be polled (for example the bot is
    // not a member, or the channel was archived/deleted). Slack returns the
    // same hard error for these on every pass, so without remembering them the
    // fallback re-issues dozens of doomed `conversations.history` calls each
    // cycle. Because those calls have real latency, a pass over a large
    // sidebar never finishes before the next one is due, turning a 20s safety
    // net into a continuous stream of failing requests (and the per-call
    // network-activity/chat-updated events that wake the UI). Skipping them
    // keeps the fallback bounded without losing any deliverable message.
    let mut inaccessible_conversations = HashSet::new();
    let mut conversation_activity = HashMap::new();
    let mut alerted_missing_realtime = false;
    loop {
        if let Some(flag) = &realtime_events_seen
            && flag.load(Ordering::Acquire)
        {
            slack_diagnostic_log(
                "slack.realtime.idle_history_fallback.stop",
                format!("account={account} reason=realtime_events_active"),
            );
            return;
        }

        let delivered_live_message = run_history_poll_pass(
            &api_client,
            &events,
            &account,
            &credential,
            current_user_id.as_deref(),
            &users,
            started_at,
            &mut seen_message_ids,
            &mut inaccessible_conversations,
            &mut conversation_activity,
            realtime_events_seen.as_deref(),
        )
        .await;

        if should_alert_undelivered_realtime_messages(
            delivered_live_message,
            realtime_events_seen
                .as_ref()
                .map(|flag| flag.load(Ordering::Acquire)),
            alerted_missing_realtime,
        ) {
            alerted_missing_realtime = true;
            slack_diagnostic_log(
                "slack.realtime.idle_fallback_missed_messages",
                format!("account={account}"),
            );
            events.send(ProviderEvent::AccountNotice {
                title: arc_str("Slack realtime is not delivering messages"),
                body: arc_str(
                    "A new Slack message arrived via periodic history checks, but the open realtime connection never delivered it. Check the Slack app's Event Subscriptions (message events) and reinstall the app to the workspace. Messages keep arriving through history checks in the meantime.",
                ),
                severity: AccountNoticeSeverity::SystemAlert,
            });
        }

        tokio::time::sleep(SLACK_HISTORY_POLL_INTERVAL).await;
    }
}

/// Decides whether the idle history-poll safety net should alert the user
/// that realtime is not delivering messages.
///
/// A silent socket alone is ambiguous: broken Event Subscriptions and a
/// simply quiet workspace look identical, and alerting on silence confuses
/// users whose setup is perfectly fine. The alert therefore requires hard
/// evidence — a live message surfaced by polling (`delivered_live_message`)
/// while the connected realtime socket has still delivered no events at all
/// (`realtime_events_seen == Some(false)`). Plain polling modes without a
/// realtime socket (`None`) never alert, and the alert fires at most once.
fn should_alert_undelivered_realtime_messages(
    delivered_live_message: bool,
    realtime_events_seen: Option<bool>,
    already_alerted: bool,
) -> bool {
    delivered_live_message && realtime_events_seen == Some(false) && !already_alerted
}

/// Returns whether a `conversations.history` error means the conversation can
/// never be polled successfully, so it should be dropped from future poll
/// passes instead of retried forever. These are membership/existence errors:
/// retrying them only wastes API calls and wakes the UI with network-activity
/// events. Accessible channels stay covered by realtime and the periodic poll.
fn slack_error_is_permanently_inaccessible(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    [
        "channel_not_found",
        "not_in_channel",
        "is_archived",
        "method_not_supported_for_channel_type",
    ]
    .iter()
    .any(|code| message.contains(code))
}

/// Always-on poll over the user's own direct and group-direct conversations,
/// using the user token, that runs concurrently with realtime.
///
/// Socket Mode / the Events API is bot-scoped: it only delivers events for
/// conversations the app is a member of, so a user's personal
/// (human-to-human) DMs never arrive over realtime. `conversations.history`
/// with the user token is the only Slack API that surfaces them, so this loop
/// keeps polling the `im`/`mpim` conversations regardless of whether realtime
/// is healthy. Channels stay instant on realtime and are deliberately excluded
/// here so this poll stays small.
///
/// Liveness gating mirrors the channel fallback: a private per-loop dedup set
/// plus the `started_at` anchor mean already-seen or pre-startup messages are
/// never re-surfaced as live. Delivering a message that realtime also delivers
/// is harmless because storage upserts messages idempotently by id.
async fn run_dm_poll_loop(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    credential: SlackCredential,
    current_user_id: Option<String>,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
    started_at: Timestamp,
) {
    slack_diagnostic_log(
        "slack.dm_poll.start",
        format!(
            "account={account} interval_s={}",
            SLACK_DM_POLL_INTERVAL.as_secs()
        ),
    );
    let mut seen_message_ids = HashSet::new();
    let mut inaccessible_conversations = HashSet::new();
    let mut conversation_activity = HashMap::new();
    loop {
        run_conversation_poll_pass(
            &api_client,
            &events,
            &account,
            &credential,
            current_user_id.as_deref(),
            &users,
            started_at,
            &mut seen_message_ids,
            &mut inaccessible_conversations,
            &mut conversation_activity,
            None,
            SlackMemberScope::Direct,
        )
        .await;

        tokio::time::sleep(SLACK_DM_POLL_INTERVAL).await;
    }
}

/// One full poll over every sidebar conversation. Returns whether at least one
/// live (notify-worthy) message was delivered during this pass.
async fn run_history_poll_pass(
    api_client: &Arc<dyn SlackApiClient>,
    events: &EventBus,
    account: &ProviderId,
    credential: &SlackCredential,
    current_user_id: Option<&str>,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    started_at: Timestamp,
    seen_message_ids: &mut HashSet<String>,
    inaccessible: &mut HashSet<String>,
    activity: &mut HashMap<String, ConversationPollState>,
    stop_signal: Option<&AtomicBool>,
) -> bool {
    run_conversation_poll_pass(
        api_client,
        events,
        account,
        credential,
        current_user_id,
        users,
        started_at,
        seen_message_ids,
        inaccessible,
        activity,
        stop_signal,
        SlackMemberScope::Sidebar,
    )
    .await
}

/// Shared body for the conversation polling loops. Polls every conversation
/// the `include` predicate accepts, emits `ChatUpdated` for each, and surfaces
/// any newly observed message that is also newer than `started_at`. Returns
/// whether at least one live (notify-worthy) message was delivered.
///
/// The scope is what separates the two pollers: the channel history
/// fallback includes the full sidebar, while the always-on DM poll narrows to
/// `im`/`mpim` conversations that Socket Mode cannot deliver. Both list only
/// joined conversations, never the whole workspace.
async fn run_conversation_poll_pass(
    api_client: &Arc<dyn SlackApiClient>,
    events: &EventBus,
    account: &ProviderId,
    credential: &SlackCredential,
    current_user_id: Option<&str>,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    started_at: Timestamp,
    seen_message_ids: &mut HashSet<String>,
    inaccessible: &mut HashSet<String>,
    activity: &mut HashMap<String, ConversationPollState>,
    stop_signal: Option<&AtomicBool>,
    scope: SlackMemberScope,
) -> bool {
    let include = |conversation: &SlackConversation| scope.includes(conversation);
    let mut delivered_live_message = false;
    match api_client
        .list_member_conversations(credential.clone(), scope)
        .await
    {
        Ok(conversations) => {
            events.send(ProviderEvent::NetworkActivity {
                direction: NetworkActivityDirection::Rx,
                kind: NetworkActivityKind::History,
            });
            let pollable: Vec<SlackConversation> = conversations
                .into_iter()
                .filter(|conversation| {
                    include(conversation) && !inaccessible.contains(&conversation.id)
                })
                .collect();
            for conversation in pollable {
                if let Some(stop) = stop_signal
                    && stop.load(Ordering::Acquire)
                {
                    // Realtime began delivering events mid-pass. Abandon the
                    // rest of this safety-net pass immediately instead of
                    // issuing dozens of now-redundant history calls.
                    break;
                }

                let snapshot = ConversationPollState {
                    updated: conversation.updated,
                    unread_count: conversation.unread_count,
                };
                // Skip conversations that have not changed since the previous
                // pass. A new message bumps `updated` and/or the unread
                // counter, so an unchanged snapshot means there is nothing new
                // to fetch. This collapses steady-state polling from "every
                // conversation every pass" to "only conversations with new
                // activity", which is the dominant idle-CPU cost in a connected
                // but silent workspace. Newly observed conversations have no
                // prior snapshot and are always polled once to establish a
                // baseline (and surface any startup backlog).
                if activity.get(&conversation.id).copied() == Some(snapshot) {
                    continue;
                }

                let Some(mut chat) = chat_from_slack_conversation(account, &conversation) else {
                    continue;
                };
                // Realtime is unavailable here, so the `load_chats`
                // name-resolution path never runs for these chats. Resolve
                // direct-message names inline (cached after the first
                // lookup) so DMs do not stay labelled with the raw Slack
                // user id, for example "DM U01ABC".
                resolve_slack_dm_chat_name(api_client, credential, users, events, &mut chat).await;
                events.send(ProviderEvent::ChatUpdated(chat));
                match api_client
                    .history(
                        credential.clone(),
                        account,
                        current_user_id,
                        &arc_str(&conversation.id),
                        None,
                        SLACK_HISTORY_POLL_LIMIT,
                        Some(Arc::clone(users)),
                    )
                    .await
                {
                    Ok(messages) => {
                        // Commit the baseline only after a successful fetch so a
                        // transient error is retried on the next pass instead of
                        // being silently skipped until the next activity bump.
                        activity.insert(conversation.id.clone(), snapshot);
                        for message in messages {
                            let first_seen = seen_message_ids
                                .insert(format!("{}:{}", message.chat_id, message.id));
                            // Only notify for messages first observed by
                            // this loop AND newer than when polling began,
                            // so historical backlog never triggers alerts.
                            if history_poll_message_is_live(
                                started_at,
                                message.timestamp,
                                first_seen,
                            ) {
                                delivered_live_message = true;
                                events.send(ProviderEvent::Message {
                                    message,
                                    is_historical: false,
                                });
                            }
                        }
                    }
                    Err(error) => {
                        if slack_error_is_permanently_inaccessible(&error) {
                            // Drop the conversation from future passes: it can
                            // never deliver history, and retrying it every cycle
                            // is what keeps the fallback (and the UI) busy.
                            inaccessible.insert(conversation.id.clone());
                            // Record a baseline too, so that if it ever becomes
                            // accessible again only genuinely new activity
                            // re-triggers a fetch.
                            activity.insert(conversation.id.clone(), snapshot);
                            slack_diagnostic_log(
                                "slack.history_poll.skip_inaccessible",
                                format!(
                                    "conversation={} {}",
                                    conversation.id,
                                    sanitize_slack_error(&error)
                                ),
                            );
                        } else {
                            // Transient failure: leave the baseline untouched so
                            // the conversation is retried on the next pass.
                            slack_diagnostic_log(
                                "slack.history_poll.history_failed",
                                sanitize_slack_error(&error),
                            );
                        }
                    }
                }
            }
        }
        Err(error) => slack_diagnostic_log(
            "slack.history_poll.conversations_failed",
            sanitize_slack_error(&error),
        ),
    }
    delivered_live_message
}

/// Resolves a direct-message chat's display name from the user cache, fetching
/// `users.info` once when the user is not yet cached and falling back to a
/// synthetic user when the lookup yields nothing. Without this, the
/// history-poll fallback leaves DM chats labelled with the raw Slack user id
/// (for example "DM U01ABC") because the realtime/`load_chats` resolution path
/// does not run while polling. The resolved user is cached so later polls reuse
/// it without another API call and never downgrade the name back to the id.
async fn resolve_slack_dm_chat_name(
    api_client: &Arc<dyn SlackApiClient>,
    credential: &SlackCredential,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    events: &EventBus,
    chat: &mut Chat,
) {
    let Some(user_id) = direct_chat_user_id(chat).map(str::to_owned) else {
        return;
    };

    let user = if let Some(cached) = read_lock(users).get(&user_id).cloned() {
        Some(cached)
    } else {
        events.send(ProviderEvent::NetworkActivity {
            direction: NetworkActivityDirection::Tx,
            kind: NetworkActivityKind::Other,
        });
        let fetched = match api_client.user_info(credential.clone(), &user_id).await {
            Ok(user) => {
                events.send(ProviderEvent::NetworkActivity {
                    direction: NetworkActivityDirection::Rx,
                    kind: NetworkActivityKind::Other,
                });
                user
            }
            Err(error) => {
                slack_diagnostic_log(
                    "slack.history_poll.user_info_failed",
                    sanitize_slack_error(&error),
                );
                None
            }
        }
        .or_else(|| fallback_slack_user(&user_id));
        if let Some(user) = fetched.as_ref() {
            write_lock(users).insert(user.id.clone(), user.clone());
        }
        fetched
    };

    if let Some(user) = user {
        chat.name = arc_str(user.best_name());
        chat.avatar = user.sender().avatar;
    }
}

/// Decides whether a message returned by the history-poll fallback should be
/// surfaced as a live (notify-worthy) message.
///
/// A message qualifies only when it is observed for the first time by this loop
/// *and* was sent after polling began. Anchoring to the loop's start time keeps
/// the baseline robust even if the very first history fetch fails: messages
/// that were already in the backlog (and only become visible on a later
/// successful poll) are still older than `started_at`, so they never trigger a
/// notification about a long-past conversation.
fn history_poll_message_is_live(
    started_at: Timestamp,
    message_timestamp: Timestamp,
    first_seen: bool,
) -> bool {
    first_seen && message_timestamp > started_at
}

async fn run_socket_mode_loop(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    user_id: Option<String>,
    app_token: String,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
    fallback_credential: Option<SlackCredential>,
    history_user_id: Option<String>,
) {
    let mut transient_alerted = false;
    // Shared across reconnects of this realtime loop:
    // - `saw_event_callback` records whether Slack has ever delivered an
    //   Events API payload on this account's socket, so the idle safety-net
    //   poller can distinguish "quiet workspace" from "events not flowing"
    //   and shut itself down once realtime is proven to work.
    // - `idle_fallback_started` guarantees at most one safety-net poller per
    //   realtime loop even when the socket reconnects and goes idle again.
    let saw_event_callback = Arc::new(AtomicBool::new(false));
    let idle_fallback_started = Arc::new(AtomicBool::new(false));
    loop {
        let result = run_socket_mode_once(
            api_client.clone(),
            events.clone(),
            account.clone(),
            user_id.clone(),
            app_token.clone(),
            Arc::clone(&users),
            fallback_credential.clone(),
            Arc::clone(&saw_event_callback),
            Arc::clone(&idle_fallback_started),
        )
        .await;
        match &result {
            Ok(()) => {
                slack_diagnostic_log(
                    "slack.realtime.disconnected",
                    format!("account={account} reason=stream_closed reconnect_in_s=5"),
                );
            }
            Err(error) => {
                let reason = sanitize_slack_error(error);
                slack_diagnostic_log(
                    "slack.realtime.error",
                    format!("account={account} reconnect_in_s=5 error={reason}"),
                );

                // A bad app-level token (for example `invalid_auth` from
                // apps.connections.open) never recovers by retrying, so retrying
                // forever leaves the user with no realtime *and* no notice. Alert
                // them with a system notification and degrade to history polling
                // so messages still arrive, then stop the dead realtime loop.
                if is_fatal_realtime_auth_error(&reason) {
                    slack_diagnostic_log(
                        "slack.realtime.fatal_auth",
                        format!(
                            "account={account} reason={reason} history_fallback={}",
                            fallback_credential.is_some()
                        ),
                    );
                    events.send(ProviderEvent::AccountNotice {
                        title: arc_str("Slack realtime unavailable"),
                        body: arc_str(format!(
                            "Slack rejected the realtime connection ({reason}). Configure a valid app-level (xapp-) token with connections:write and enable Socket Mode. Falling back to periodic history checks."
                        )),
                        severity: AccountNoticeSeverity::SystemAlert,
                    });
                    if let Some(credential) = fallback_credential {
                        run_history_poll_loop(
                            api_client,
                            events,
                            account,
                            credential,
                            history_user_id,
                            users,
                            Utc::now(),
                            None,
                        )
                        .await;
                    }
                    return;
                }

                // Transient errors (network blips, Slack restarts) can recover, so
                // keep retrying, but still alert the user once so a prolonged
                // outage is visible instead of silent.
                if !transient_alerted {
                    transient_alerted = true;
                    events.send(ProviderEvent::AccountNotice {
                        title: arc_str("Slack realtime interrupted"),
                        body: arc_str(format!(
                            "Lost the Slack realtime connection ({reason}). Reconnecting automatically."
                        )),
                        severity: AccountNoticeSeverity::SystemAlert,
                    });
                }
            }
        }
        if result.is_ok() {
            transient_alerted = false;
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Classifies a sanitized Slack error string as a non-recoverable realtime
/// authentication/authorization failure. These never succeed on retry, so the
/// realtime loop should alert the user and fall back instead of spinning.
fn is_fatal_realtime_auth_error(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "invalid_auth",
        "not_authed",
        "account_inactive",
        "token_revoked",
        "token_expired",
        "no_permission",
        "missing_scope",
    ]
    .iter()
    .any(|needle| reason.contains(needle))
}

async fn run_socket_mode_once(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    user_id: Option<String>,
    app_token: String,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
    fallback_credential: Option<SlackCredential>,
    saw_event_callback: Arc<AtomicBool>,
    idle_fallback_started: Arc<AtomicBool>,
) -> Result<()> {
    events.send(ProviderEvent::NetworkActivity {
        direction: NetworkActivityDirection::Tx,
        kind: NetworkActivityKind::Connect,
    });
    slack_diagnostic_log(
        "slack.realtime.open_socket_mode.request",
        format!("account={account}"),
    );
    let connection = api_client
        .open_socket_mode(SlackCredential::new(
            SlackCredentialKind::AppToken,
            app_token,
        ))
        .await?;
    events.send(ProviderEvent::NetworkActivity {
        direction: NetworkActivityDirection::Rx,
        kind: NetworkActivityKind::Connect,
    });
    slack_diagnostic_log(
        "slack.realtime.websocket.connecting",
        format!("account={account}"),
    );
    let (mut socket, _) = connect_async(&connection.url)
        .await
        .context("connecting Slack Socket Mode WebSocket")?;
    slack_diagnostic_log(
        "slack.realtime.websocket.connected",
        format!("account={account}"),
    );

    let realtime_started_at = Utc::now();
    let connected_at = tokio::time::Instant::now();
    let mut logged_idle_no_events = false;
    loop {
        if !saw_event_callback.load(Ordering::Acquire)
            && !logged_idle_no_events
            && connected_at.elapsed() >= SLACK_SOCKET_MODE_IDLE_DIAGNOSTIC_AFTER
        {
            logged_idle_no_events = true;
            // A connected-but-silent socket is ambiguous: broken Event
            // Subscriptions and a simply quiet workspace look identical from
            // here, so alarming the user on silence alone would be a false
            // positive for perfectly healthy setups. Log a diagnostic and
            // quietly start a history-poll safety net instead; that poller
            // alerts only on hard evidence (a message it delivered that
            // realtime missed) and stops itself once realtime delivers its
            // first event.
            slack_diagnostic_log(
                "slack.realtime.idle_no_events",
                format!(
                    "account={account} connected_s={} history_fallback={} hint=quiet_workspace_or_missing_event_subscriptions",
                    connected_at.elapsed().as_secs(),
                    fallback_credential.is_some()
                ),
            );
            if let Some(credential) = fallback_credential.clone()
                && !idle_fallback_started.swap(true, Ordering::AcqRel)
            {
                slack_diagnostic_log(
                    "slack.realtime.idle_history_fallback.start",
                    format!("account={account}"),
                );
                tokio::spawn(run_history_poll_loop(
                    Arc::clone(&api_client),
                    events.clone(),
                    account.clone(),
                    credential,
                    user_id.clone(),
                    Arc::clone(&users),
                    realtime_started_at,
                    Some(Arc::clone(&saw_event_callback)),
                ));
            }
        }

        let timed_message =
            tokio::time::timeout(SLACK_SOCKET_MODE_IDLE_DIAGNOSTIC_AFTER, socket.next()).await;
        let Some(message) = (match timed_message {
            Ok(Some(message)) => Some(message),
            Ok(None) => None,
            Err(_) => continue,
        }) else {
            break;
        };
        let message = message.context("reading Slack Socket Mode frame")?;
        match message {
            WebSocketMessage::Text(text) => {
                events.send(ProviderEvent::NetworkActivity {
                    direction: NetworkActivityDirection::Rx,
                    kind: NetworkActivityKind::Realtime,
                });
                let handled = handle_socket_mode_text(
                    &events,
                    &account,
                    user_id.as_deref(),
                    text.as_ref(),
                    &users,
                    fallback_credential.as_ref(),
                )?;
                if handled.received_event_callback {
                    saw_event_callback.store(true, Ordering::Release);
                }
                if let Some(ack) = handled.ack {
                    socket
                        .send(WebSocketMessage::Text(ack.into()))
                        .await
                        .context("acking Slack Socket Mode envelope")?;
                    events.send(ProviderEvent::NetworkActivity {
                        direction: NetworkActivityDirection::Tx,
                        kind: NetworkActivityKind::Realtime,
                    });
                }
            }
            WebSocketMessage::Ping(payload) => {
                socket
                    .send(WebSocketMessage::Pong(payload))
                    .await
                    .context("replying to Slack Socket Mode ping")?;
                events.send(ProviderEvent::NetworkActivity {
                    direction: NetworkActivityDirection::Tx,
                    kind: NetworkActivityKind::Realtime,
                });
            }
            WebSocketMessage::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

struct SlackSocketModeHandleResult {
    ack: Option<String>,
    received_event_callback: bool,
}

fn handle_socket_mode_text(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    text: &str,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    web_api_credential: Option<&SlackCredential>,
) -> Result<SlackSocketModeHandleResult> {
    let envelope: SlackSocketEnvelope =
        serde_json::from_str(text).context("decoding Slack Socket Mode envelope")?;
    let ack = envelope
        .envelope_id
        .as_deref()
        .map(|id| format!(r#"{{"envelope_id":"{}"}}"#, json_escape(id)));

    slack_diagnostic_log(
        "slack.realtime.envelope",
        format!(
            "account={account} type={} payload_event={}",
            envelope.envelope_type.as_deref().unwrap_or("<none>"),
            envelope
                .payload
                .as_ref()
                .and_then(|payload| payload.event_type.as_deref())
                .unwrap_or("<none>"),
        ),
    );

    let received_event_callback = envelope.envelope_type.as_deref() == Some("events_api")
        && envelope
            .payload
            .as_ref()
            .and_then(|payload| payload.event_type.as_deref())
            == Some("event_callback");

    if received_event_callback
        && let Some(event) = envelope.payload.and_then(|payload| payload.event)
    {
        let events = events.clone();
        let account = account.clone();
        let current_user_id = current_user_id.map(str::to_owned);
        let users = Arc::clone(users);
        let web_api_credential = web_api_credential.cloned();
        tokio::spawn(async move {
            emit_realtime_event(
                &events,
                &account,
                current_user_id.as_deref(),
                event,
                &users,
                web_api_credential.as_ref(),
            )
            .await;
        });
    }

    Ok(SlackSocketModeHandleResult {
        ack,
        received_event_callback,
    })
}

async fn emit_realtime_event(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    event: SlackRealtimeEvent,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    web_api_credential: Option<&SlackCredential>,
) {
    match event.event_type.as_str() {
        "message" => {
            emit_realtime_message(
                events,
                account,
                current_user_id,
                event,
                users,
                web_api_credential,
            )
            .await
        }
        "reaction_added" | "reaction_removed" => emit_realtime_reaction(events, event),
        other => slack_diagnostic_log(
            "slack.realtime.event.unhandled",
            format!("account={account} event_type={other}"),
        ),
    }
}

fn is_ignored_slack_message_subtype(subtype: Option<&str>) -> bool {
    matches!(subtype, Some("message_deleted" | "message_changed"))
}

async fn emit_realtime_message(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    event: SlackRealtimeEvent,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    web_api_credential: Option<&SlackCredential>,
) {
    let subtype = event.subtype.as_deref();
    if event.hidden.unwrap_or(false) || matches!(subtype, Some("message_deleted")) {
        if let (Some(channel), Some(message_id)) = (
            non_empty_option(&event.channel),
            non_empty_option(&event.deleted_ts).or_else(|| non_empty_option(&event.ts)),
        ) {
            events.send(ProviderEvent::MessageDeleted {
                chat_id: arc_str(channel),
                message_id: arc_str(message_id),
            });
        }
        return;
    }

    if matches!(subtype, Some("message_changed")) {
        if let Some(message) = event.message.and_then(|message| {
            let edited_at = slack_edited_at(message.edited.as_ref());
            slack_message_from_parts(
                account,
                current_user_id,
                message.channel.or(event.channel),
                message.user,
                message.bot_id,
                slack_message_sender_metadata(message.username, message.icons, message.bot_profile),
                message.ts,
                message.thread_ts,
                message.text,
                message.blocks,
                message.attachments,
                message.files,
                Vec::new(),
                Some(users),
                false,
                web_api_credential.map(SlackCredential::value),
            )
            .map(|mut built| {
                built.edited_at = edited_at;
                built
            })
        }) {
            let mut message = message;
            resolve_realtime_message_users(&mut message, users, events, web_api_credential).await;
            events.send(ProviderEvent::MessageEdited { message });
        }
        return;
    }

    if is_ignored_slack_message_subtype(subtype) {
        slack_diagnostic_log(
            "slack.realtime.message.dropped",
            format!(
                "account={account} reason=ignored_subtype subtype={}",
                subtype.unwrap_or("<none>")
            ),
        );
        return;
    }

    let channel = event.channel.clone();
    if let Some(mut message) = slack_message_from_parts(
        account,
        current_user_id,
        event.channel,
        event.user,
        event.bot_id,
        slack_message_sender_metadata(event.username, event.icons, event.bot_profile),
        event.ts.or(event.event_ts),
        event.thread_ts,
        event.text,
        event.blocks,
        event.attachments,
        event.files,
        Vec::new(),
        Some(users),
        false,
        web_api_credential.map(SlackCredential::value),
    ) {
        resolve_realtime_message_users(&mut message, users, events, web_api_credential).await;
        slack_diagnostic_log(
            "slack.realtime.message.emit",
            format!(
                "account={account} chat={} message={} from_me={} subtype={}",
                message.chat_id,
                message.id,
                message.is_from_me,
                subtype.unwrap_or("<none>")
            ),
        );
        events.send(ProviderEvent::Message {
            message,
            is_historical: false,
        });
    } else {
        slack_diagnostic_log(
            "slack.realtime.message.dropped",
            format!(
                "account={account} reason=unbuildable channel={} subtype={}",
                channel.as_deref().unwrap_or("<none>"),
                subtype.unwrap_or("<none>")
            ),
        );
    }
}

async fn resolve_realtime_message_users(
    message: &mut Message,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    events: &EventBus,
    credential: Option<&SlackCredential>,
) {
    let unresolved = unresolved_slack_user_ids(std::slice::from_ref(message));
    if !unresolved.is_empty()
        && let Some(credential) = credential
    {
        for user_id in unresolved {
            if read_lock(users).contains_key(&user_id) {
                continue;
            }
            events.send(ProviderEvent::NetworkActivity {
                direction: NetworkActivityDirection::Tx,
                kind: NetworkActivityKind::Other,
            });
            let fetched = match get_web_api_user_info(credential.clone(), &user_id).await {
                Ok(fetched) => {
                    events.send(ProviderEvent::NetworkActivity {
                        direction: NetworkActivityDirection::Rx,
                        kind: NetworkActivityKind::Other,
                    });
                    fetched
                }
                Err(error) => {
                    slack_diagnostic_log(
                        "slack.realtime.user_info_failed",
                        sanitize_slack_error(&error),
                    );
                    None
                }
            }
            .or_else(|| fallback_slack_user(&user_id));
            if let Some(user) = fetched {
                write_lock(users).insert(user.id.clone(), user);
            }
        }
    }
    apply_cached_user_to_message(users, message);
}

fn emit_realtime_reaction(events: &EventBus, event: SlackRealtimeEvent) {
    let Some(item) = event.item else {
        return;
    };
    let (Some(channel), Some(message_id), Some(emoji), Some(sender)) = (
        non_empty_option(&item.channel),
        non_empty_option(&item.ts),
        non_empty_option(&event.reaction),
        non_empty_option(&event.user).or_else(|| non_empty_option(&event.item_user)),
    ) else {
        return;
    };

    events.send(ProviderEvent::ReactionChanged {
        chat_id: arc_str(channel),
        message_id: arc_str(message_id),
        emoji: arc_str(slack_emoji_display(&emoji)),
        added: event.event_type == "reaction_added",
        sender: arc_str(sender),
    });
}

fn slack_history_message(
    account: ProviderId,
    current_user_id: Option<&str>,
    channel: String,
    message: SlackHistoryMessageResponse,
    users: Option<&Arc<RwLock<HashMap<String, SlackUser>>>>,
    web_api_token: Option<&str>,
) -> Option<Message> {
    if message.hidden.unwrap_or(false)
        || message
            .message_type
            .as_deref()
            .is_some_and(|kind| kind != "message")
        || is_ignored_slack_message_subtype(message.subtype.as_deref())
    {
        return None;
    }

    let is_thread_root = message.reply_count.unwrap_or(0) > 0;
    let edited_at = slack_edited_at(message.edited.as_ref());
    let mut built = slack_message_from_parts(
        &account,
        current_user_id,
        Some(channel),
        message.user,
        message.bot_id,
        slack_message_sender_metadata(message.username, message.icons, message.bot_profile),
        message.ts,
        message.thread_ts,
        message.text,
        message.blocks,
        message.attachments,
        message.files,
        slack_reactions(message.reactions),
        users,
        is_thread_root,
        web_api_token,
    )?;
    built.edited_at = edited_at;
    Some(built)
}

fn slack_message_from_parts(
    account: &ProviderId,
    current_user_id: Option<&str>,
    channel: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    sender_metadata: SlackMessageSenderMetadata,
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
    blocks: Option<Vec<serde_json::Value>>,
    attachments: Option<Vec<SlackAttachmentResponse>>,
    files: Option<Vec<SlackFileResponse>>,
    reactions: Vec<Reaction>,
    users: Option<&Arc<RwLock<HashMap<String, SlackUser>>>>,
    is_thread_root: bool,
    web_api_token: Option<&str>,
) -> Option<Message> {
    let channel = non_empty_option(&channel)?;
    let ts = non_empty_option(&ts)?;
    let sender_id = non_empty_option(&user)
        .or_else(|| non_empty_option(&bot_id))
        .unwrap_or_else(|| "slack".to_owned());
    let attachments = attachments.unwrap_or_default();
    let files = files.unwrap_or_default();
    let own_text = slack_message_own_text(text);
    let text = slack_message_text(Some(own_text.as_str()), &attachments);
    // Block Kit messages carry their real layout in `blocks`; the top-level
    // `text` is only a notification fallback there, so prefer the structured
    // cards and drop the (duplicate, flattened) fallback text.
    let block_cards = slack_block_cards(&slack_block_responses(blocks.unwrap_or_default()));
    let has_block_cards = !block_cards.is_empty();
    let mut cards = slack_attachment_cards(&attachments);
    let mut file_cards = slack_file_cards(&files, web_api_token);
    if has_block_cards {
        let mut merged = block_cards;
        merged.append(&mut cards);
        cards = merged;
    } else if !own_text.trim().is_empty() && !cards.is_empty() {
        cards.insert(0, slack_message_text_card(&own_text));
    }
    // Preserve the message's own text as a caption on the first file card when
    // there are no attachment cards. Otherwise a message like "<text> + 2 images"
    // would render the images but silently drop the text once content becomes
    // `Cards`.
    if cards.is_empty()
        && let Some(first) = file_cards.first_mut()
        && first.body.is_none()
        && !text.trim().is_empty()
    {
        first.body = Some(arc_str(text.clone()));
    }
    cards.append(&mut file_cards);
    // Detect self-mentions from the raw text while `<@U123>`/`<!here>` tokens
    // are still present (mention substitution happens later, in
    // `apply_cached_user_to_message`). Used by the notification scope filter.
    // Block messages may only carry the mention inside their blocks, so scan
    // the converted card text as well.
    let mention_text = if has_block_cards {
        let card_text = cards
            .iter()
            .map(slack_card_fallback_text)
            .collect::<Vec<_>>()
            .join("\n");
        format!("{text}\n{card_text}")
    } else {
        text.clone()
    };
    let mentions_me = slack_text_mentions_user(&mention_text, current_user_id);
    let timestamp = slack_ts_to_timestamp(&ts).unwrap_or_else(Utc::now);
    let thread_id = non_empty_option(&thread_ts)
        .filter(|thread_ts| thread_ts != &ts)
        .or_else(|| is_thread_root.then(|| ts.clone()));
    let reply_to = thread_id
        .as_deref()
        .filter(|thread_id| *thread_id != ts.as_str())
        .map(arc_str);
    let mut sender = slack_message_sender(&sender_id, sender_metadata);
    if let Some(users) = users
        && let Some(user) = read_lock(users).get(sender_id.as_str())
    {
        sender = user.sender();
    }

    Some(Message {
        id: arc_str(&ts),
        chat_id: arc_str(&channel),
        account: account.clone(),
        sender,
        timestamp,
        edited_at: None,
        content: if cards.is_empty() {
            Content::Text(arc_str(text))
        } else {
            Content::Cards(cards.clone())
        },
        reply_to,
        thread_id: thread_id.map(arc_str),
        reactions,
        receipts: Vec::new(),
        is_from_me: current_user_id.is_some_and(|current_user_id| current_user_id == sender_id),
        mentions_me,
        platform_data: PlatformData {
            slack: Some(SlackData {
                ts: arc_str(&ts),
                thread_ts: thread_ts.map(arc_str),
                channel: arc_str(channel),
            }),
            cards,
            ..PlatformData::default()
        },
    })
}

fn slack_message_own_text(text: Option<String>) -> String {
    text.and_then(non_empty_string)
        .map(|text| replace_slack_emoji_codes(&slack_mrkdwn_styles_to_markdown(&text)))
        .unwrap_or_default()
}

fn slack_message_text(text: Option<&str>, attachments: &[SlackAttachmentResponse]) -> String {
    let mut parts = Vec::new();
    if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
        parts.push(text.to_owned());
    }

    for attachment in attachments {
        parts.extend(slack_attachment_text_parts(attachment));
    }

    parts.join("\n")
}

fn slack_message_text_card(text: &str) -> Card {
    Card {
        kind: CardKind::BotMessage,
        source: CardSource::Slack,
        title: None,
        subtitle: None,
        body: Some(arc_str(text)),
        footer: None,
        url: None,
        accent_color: None,
        thumbnail: None,
        image: None,
        fields: Vec::new(),
        actions: Vec::new(),
    }
}

fn slack_attachment_text_parts(attachment: &SlackAttachmentResponse) -> Vec<String> {
    let mut parts = Vec::new();
    push_unique_text_part(&mut parts, attachment.pretext.clone());
    push_unique_text_part(
        &mut parts,
        attachment
            .title
            .clone()
            .and_then(non_empty_string)
            .map(|title| format!("**{title}**")),
    );
    push_unique_text_part(&mut parts, attachment.text.clone());

    for field in attachment.fields.clone().unwrap_or_default() {
        match (
            field.title.and_then(non_empty_string),
            field.value.and_then(non_empty_string),
        ) {
            (Some(title), Some(value)) => {
                push_unique_text_part(&mut parts, Some(format!("{title}: {value}")))
            }
            (Some(title), None) => push_unique_text_part(&mut parts, Some(title)),
            (None, Some(value)) => push_unique_text_part(&mut parts, Some(value)),
            (None, None) => {}
        }
    }

    if parts.is_empty() {
        push_unique_text_part(&mut parts, attachment.fallback.clone());
    }

    parts
}

fn slack_attachment_cards(attachments: &[SlackAttachmentResponse]) -> Vec<Card> {
    attachments
        .iter()
        .filter_map(slack_attachment_card)
        .collect()
}

fn slack_attachment_card(attachment: &SlackAttachmentResponse) -> Option<Card> {
    let title = attachment
        .title
        .clone()
        .and_then(non_empty_string)
        .map(|text| arc_str(replace_slack_emoji_codes(&text)));
    let subtitle = attachment
        .pretext
        .clone()
        .and_then(non_empty_string)
        .map(|text| arc_str(replace_slack_emoji_codes(&text)))
        .or_else(|| {
            attachment
                .author_name
                .clone()
                .and_then(non_empty_string)
                .map(|text| arc_str(replace_slack_emoji_codes(&text)))
        });
    let body = attachment
        .text
        .clone()
        .and_then(non_empty_string)
        .map(|text| arc_str(replace_slack_emoji_codes(&text)));
    let footer = attachment
        .footer
        .clone()
        .and_then(non_empty_string)
        .or_else(|| attachment.ts.clone().and_then(non_empty_string))
        .map(|text| arc_str(replace_slack_emoji_codes(&text)));
    let url = attachment
        .title_link
        .clone()
        .and_then(non_empty_string)
        .or_else(|| attachment.author_link.clone().and_then(non_empty_string))
        .map(arc_str);
    let accent_color = attachment
        .color
        .clone()
        .and_then(non_empty_string)
        .map(slack_attachment_color);
    let image = attachment
        .image_url
        .clone()
        .and_then(non_empty_string)
        .map(|url| slack_card_media("image", &url));
    let thumbnail = attachment
        .thumb_url
        .clone()
        .and_then(non_empty_string)
        .map(|url| slack_card_media("thumbnail", &url));
    let fields = attachment
        .fields
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(slack_attachment_card_field)
        .collect::<Vec<_>>();

    if title.is_none()
        && subtitle.is_none()
        && body.is_none()
        && footer.is_none()
        && fields.is_empty()
        && image.is_none()
        && thumbnail.is_none()
    {
        return attachment
            .fallback
            .clone()
            .and_then(non_empty_string)
            .map(|fallback| Card {
                kind: CardKind::ProviderAttachment,
                source: CardSource::Slack,
                title: None,
                subtitle: None,
                body: Some(arc_str(replace_slack_emoji_codes(&fallback))),
                footer: None,
                url: None,
                accent_color,
                thumbnail: None,
                image: None,
                fields: Vec::new(),
                actions: Vec::new(),
            });
    }

    Some(Card {
        kind: CardKind::ProviderAttachment,
        source: CardSource::Slack,
        title,
        subtitle,
        body,
        footer,
        url,
        accent_color,
        thumbnail,
        image,
        fields,
        actions: Vec::new(),
    })
}

fn slack_attachment_card_field(field: SlackAttachmentFieldResponse) -> Option<CardField> {
    let value = field.value.and_then(non_empty_string)?;
    Some(CardField {
        title: field
            .title
            .and_then(non_empty_string)
            .map(|title| arc_str(replace_slack_emoji_codes(&title))),
        value: arc_str(replace_slack_emoji_codes(&value)),
        short: field.short.unwrap_or(false),
    })
}

fn slack_attachment_color(color: String) -> CardColor {
    let color = color.trim().trim_start_matches('#');
    if color.len() == 6 && color.chars().all(|ch| ch.is_ascii_hexdigit()) {
        CardColor::Hex(arc_str(color))
    } else {
        CardColor::Named(arc_str(color.to_ascii_lowercase()))
    }
}

fn slack_card_media(kind: &str, url: &str) -> Media {
    let file_name = url
        .split(['/', '?', '#'])
        .next_back()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(kind);
    Media {
        id: arc_str(url),
        file_name: arc_str(file_name),
        mime_type: arc_str("image/*"),
        size_bytes: None,
        caption: None,
        // Attachment/block images are public CDN URLs (no Bearer auth), so
        // reserve a deterministic cache path and fetch the bytes eagerly on a
        // background thread. Without a local path the preview pipeline can
        // never render these images.
        local_path: slack_cached_media_path(url, "cards", None),
        thumbnail: None,
    }
}

/// Decodes a message's raw `blocks` array one entry at a time, skipping (and
/// logging) entries that fail to decode so a single unfamiliar block can never
/// drop the whole message.
fn slack_block_responses(values: Vec<serde_json::Value>) -> Vec<SlackBlockResponse> {
    values
        .into_iter()
        .filter_map(
            |value| match serde_json::from_value::<SlackBlockResponse>(value) {
                Ok(block) => Some(block),
                Err(error) => {
                    slack_diagnostic_log("slack.blocks.decode_skipped", error.to_string());
                    None
                }
            },
        )
        .collect()
}

/// Accumulates consecutive Block Kit blocks into one provider-neutral card so
/// related blocks (status line, linked title, buttons, context footer) render
/// as a single visual group, mirroring Slack's own layout.
#[derive(Default)]
struct SlackBlockCardBuilder {
    title: Option<String>,
    body: Vec<String>,
    footer: Vec<String>,
    fields: Vec<CardField>,
    actions: Vec<CardAction>,
    thumbnail: Option<Media>,
}

impl SlackBlockCardBuilder {
    fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_empty()
            && self.footer.is_empty()
            && self.fields.is_empty()
            && self.actions.is_empty()
            && self.thumbnail.is_none()
    }

    fn flush(&mut self, cards: &mut Vec<Card>) {
        if self.is_empty() {
            return;
        }
        let builder = std::mem::take(self);
        cards.push(Card {
            kind: CardKind::BotMessage,
            source: CardSource::Slack,
            title: builder.title.and_then(non_empty_string).map(arc_str),
            subtitle: None,
            body: non_empty_string(builder.body.join("\n")).map(arc_str),
            footer: non_empty_string(builder.footer.join(" ")).map(arc_str),
            url: None,
            accent_color: None,
            thumbnail: builder.thumbnail,
            image: None,
            fields: builder.fields,
            actions: builder.actions,
        });
    }
}

/// Converts a message's Block Kit blocks into renderable cards. `header`
/// blocks start a new card, `section`/`rich_text` text accumulates into the
/// card body, `actions` buttons attach to the current group and close it,
/// `context` becomes the footer, `image` blocks become standalone media
/// preview cards, and `divider` flushes the group. Unknown block types are
/// skipped (logged) so they degrade to the fallback text path when nothing
/// else parses.
fn slack_block_cards(blocks: &[SlackBlockResponse]) -> Vec<Card> {
    let mut cards = Vec::new();
    let mut builder = SlackBlockCardBuilder::default();
    for block in blocks {
        match block.block_type.as_deref() {
            Some("header") => {
                builder.flush(&mut cards);
                builder.title = block.text.as_ref().and_then(slack_block_text);
            }
            Some("section") => {
                if let Some(text) = block.text.as_ref().and_then(slack_block_text) {
                    builder.body.push(text);
                }
                for field in block.fields.as_deref().unwrap_or_default() {
                    if let Some(text) = slack_block_text(field) {
                        builder.fields.push(CardField {
                            title: None,
                            value: arc_str(text),
                            short: true,
                        });
                    }
                }
                if let Some(accessory) = block.accessory.as_ref() {
                    if let Some(action) = slack_block_button_action(accessory) {
                        builder.actions.push(action);
                    } else if builder.thumbnail.is_none() {
                        builder.thumbnail = slack_block_accessory_image(accessory);
                    }
                }
            }
            Some("rich_text") => {
                if let Some(text) = non_empty_string(slack_rich_text_elements_text(
                    block.elements.as_deref().unwrap_or_default(),
                )) {
                    builder.body.push(text);
                }
            }
            Some("image") => {
                builder.flush(&mut cards);
                let Some(url) = block.image_url.clone().and_then(non_empty_string) else {
                    continue;
                };
                cards.push(Card {
                    kind: CardKind::MediaPreview,
                    source: CardSource::Slack,
                    // Kept for the details pane; media preview cards hide the
                    // title in the transcript.
                    title: block
                        .title
                        .as_ref()
                        .and_then(slack_block_text)
                        .or_else(|| block.alt_text.clone().and_then(non_empty_string))
                        .map(arc_str),
                    subtitle: None,
                    body: None,
                    footer: None,
                    url: None,
                    accent_color: None,
                    thumbnail: None,
                    image: Some(slack_card_media("image", &url)),
                    fields: Vec::new(),
                    actions: Vec::new(),
                });
            }
            Some("actions") => {
                builder.actions.extend(
                    block
                        .elements
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .filter_map(slack_block_button_action),
                );
                // Buttons visually close a group in Slack's layout.
                builder.flush(&mut cards);
            }
            Some("context") => {
                if let Some(text) = non_empty_string(slack_context_elements_text(
                    block.elements.as_deref().unwrap_or_default(),
                )) {
                    builder.footer.push(text);
                }
            }
            Some("divider") => builder.flush(&mut cards),
            other => slack_diagnostic_log(
                "slack.blocks.unsupported_type",
                format!("type={}", other.unwrap_or("<none>")),
            ),
        }
    }
    builder.flush(&mut cards);

    // Alert bots lead with a colored-square severity emoji; surface it as the
    // card accent so themed presentations can color the card chrome.
    if let Some(first) = cards.first_mut()
        && first.accent_color.is_none()
    {
        first.accent_color = first
            .title
            .as_deref()
            .or(first.body.as_deref())
            .and_then(slack_severity_accent_color);
    }
    cards
}

/// Renders a Block Kit text object into the renderer's markdown dialect with
/// emoji shortcodes resolved. `plain_text` is taken verbatim; `mrkdwn` goes
/// through the dialect translation.
fn slack_block_text(text: &SlackBlockTextResponse) -> Option<String> {
    let raw = text.text.clone().and_then(non_empty_string)?;
    let converted = if text.text_type.as_deref() == Some("plain_text") {
        raw
    } else {
        slack_mrkdwn_to_markdown(&raw)
    };
    non_empty_string(replace_slack_emoji_codes(&converted))
}

/// Extracts a `button` element as a card action. Interactive buttons without
/// a `url` still surface as (inert) labels so the message reads like Slack's
/// layout instead of the "Acknowledge button" fallback prose.
fn slack_block_button_action(value: &serde_json::Value) -> Option<CardAction> {
    if value.get("type").and_then(|kind| kind.as_str()) != Some("button") {
        return None;
    }
    let label = value
        .get("text")
        .and_then(|text| text.get("text"))
        .and_then(|text| text.as_str())?;
    let label = non_empty_string(replace_slack_emoji_codes(label.trim()))?;
    Some(CardAction {
        label: arc_str(label),
        url: value
            .get("url")
            .and_then(|url| url.as_str())
            .map(str::to_owned)
            .and_then(non_empty_string)
            .map(arc_str),
    })
}

fn slack_block_accessory_image(value: &serde_json::Value) -> Option<Media> {
    if value.get("type").and_then(|kind| kind.as_str()) != Some("image") {
        return None;
    }
    let url = value
        .get("image_url")
        .and_then(|url| url.as_str())
        .map(str::to_owned)
        .and_then(non_empty_string)?;
    Some(slack_card_media("image", &url))
}

/// Joins the text elements of a `context` block. Context image elements (tiny
/// icons) carry no useful transcript text and are skipped.
fn slack_context_elements_text(elements: &[serde_json::Value]) -> String {
    elements
        .iter()
        .filter_map(|element| {
            let element_type = element.get("type").and_then(|kind| kind.as_str())?;
            let text = element.get("text").and_then(|text| text.as_str())?;
            let converted = match element_type {
                "plain_text" => text.to_owned(),
                "mrkdwn" => slack_mrkdwn_to_markdown(text),
                _ => return None,
            };
            non_empty_string(replace_slack_emoji_codes(&converted))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Flattens a `rich_text` block's element tree to plain text, one line per
/// top-level section and one bulleted line per list item. A lossy but safe
/// degradation path: unknown leaves are recursed into for any nested text.
fn slack_rich_text_elements_text(elements: &[serde_json::Value]) -> String {
    let mut parts = Vec::new();
    for element in elements {
        let element_type = element
            .get("type")
            .and_then(|kind| kind.as_str())
            .unwrap_or_default();
        let children = element
            .get("elements")
            .and_then(|elements| elements.as_array())
            .map(Vec::as_slice)
            .unwrap_or_default();
        if element_type == "rich_text_list" {
            for item in children {
                let mut line = String::from("• ");
                slack_rich_text_leaf_text(item, &mut line);
                if line.trim() != "•" {
                    parts.push(line);
                }
            }
        } else {
            let mut text = String::new();
            for child in children {
                slack_rich_text_leaf_text(child, &mut text);
            }
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

fn slack_rich_text_leaf_text(element: &serde_json::Value, output: &mut String) {
    let element_type = element
        .get("type")
        .and_then(|kind| kind.as_str())
        .unwrap_or_default();
    match element_type {
        "text" => {
            if let Some(text) = element.get("text").and_then(|text| text.as_str()) {
                output.push_str(text);
            }
        }
        "link" => {
            if let Some(label) = element
                .get("text")
                .and_then(|text| text.as_str())
                .or_else(|| element.get("url").and_then(|url| url.as_str()))
            {
                output.push_str(label);
            }
        }
        "emoji" => {
            if let Some(name) = element.get("name").and_then(|name| name.as_str()) {
                output.push_str(&slack_emoji_display(name));
            }
        }
        // Emit the raw mention token so the existing user-mention substitution
        // (`replace_slack_mentions_in_cards`) resolves it to a display name.
        "user" => {
            if let Some(id) = element.get("user_id").and_then(|id| id.as_str()) {
                output.push_str("<@");
                output.push_str(id);
                output.push('>');
            }
        }
        "broadcast" => {
            if let Some(range) = element.get("range").and_then(|range| range.as_str()) {
                output.push('@');
                output.push_str(range);
            }
        }
        "channel" => {
            if let Some(id) = element.get("channel_id").and_then(|id| id.as_str()) {
                output.push('#');
                output.push_str(id);
            }
        }
        _ => {
            for child in element
                .get("elements")
                .and_then(|elements| elements.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                slack_rich_text_leaf_text(child, output);
            }
        }
    }
}

/// Maps a leading severity emoji (as emitted by alerting bots like NewRelic)
/// to a named card accent color.
fn slack_severity_accent_color(text: &str) -> Option<CardColor> {
    let trimmed = text.trim_start_matches(['*', '_', '~', '`', ' ']);
    let named = if trimmed.starts_with('🟥') || trimmed.starts_with('🔴') {
        "danger"
    } else if trimmed.starts_with('🟧')
        || trimmed.starts_with('🟠')
        || trimmed.starts_with('🟨')
        || trimmed.starts_with('🟡')
    {
        "warning"
    } else if trimmed.starts_with('🟩') || trimmed.starts_with('🟢') {
        "good"
    } else if trimmed.starts_with('🟦') || trimmed.starts_with('🔵') {
        "primary"
    } else {
        return None;
    };
    Some(CardColor::Named(arc_str(named)))
}

/// Translates Slack mrkdwn into the renderer's markdown dialect: `*bold*` →
/// `**bold**`, `~strike~` → `~~strike~~`, `<url|label>` → `label`, and
/// `<#C123|name>` → `#name`. Mention tokens (`<@U…>`, `<!here>`) pass through
/// untouched for the later user-substitution pass, and code spans are copied
/// verbatim.
fn slack_mrkdwn_to_markdown(text: &str) -> String {
    chat_core::markup::map_outside_code_spans(text, |segment| {
        convert_mrkdwn_delimiters(&convert_slack_angle_tokens(segment))
    })
}

/// Style-only mrkdwn translation (`*bold*`, `~strike~`) for plain message
/// text, where angle-bracket link tokens must survive for link-preview and
/// mention handling.
fn slack_mrkdwn_styles_to_markdown(text: &str) -> String {
    chat_core::markup::chat_markup_to_markdown(text)
}

fn convert_mrkdwn_delimiters(text: &str) -> String {
    let text = convert_mrkdwn_delimiter(text, '*', "**");
    convert_mrkdwn_delimiter(&text, '~', "~~")
}

fn convert_mrkdwn_delimiter(text: &str, delimiter: char, replacement: &str) -> String {
    text.split('\n')
        .map(|line| chat_core::markup::convert_single_delimiter_line(line, delimiter, replacement))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rewrites Slack angle-bracket tokens: `<url|label>` → `label`, bare `<url>`
/// → `url`, `<#C123|name>` → `#name`. Mention tokens (`<@…>`, `<!…>`) are
/// preserved for the later mention-substitution pass.
fn convert_slack_angle_tokens(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        output.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let token = &after[..end];
        if token.starts_with('@') || token.starts_with('!') {
            output.push('<');
            output.push_str(token);
            output.push('>');
        } else if let Some(channel) = token.strip_prefix('#') {
            let label = channel
                .split_once('|')
                .map(|(_, label)| label)
                .unwrap_or(channel);
            output.push('#');
            output.push_str(label);
        } else if token.contains("://") || token.starts_with("mailto:") {
            match token.split_once('|') {
                Some((_, label)) if !label.trim().is_empty() => output.push_str(label),
                _ => output.push_str(token.split_once('|').map_or(token, |(url, _)| url)),
            }
        } else {
            output.push('<');
            output.push_str(token);
            output.push('>');
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    output
}

/// Builds attachment cards for the `files` array of a Slack message. Image
/// uploads become `MediaPreview` cards whose image points at a locally cached
/// (and lazily, authenticated-downloaded) copy of the file; other file types
/// become a lightweight `ProviderAttachment` card with a download link.
fn slack_file_cards(files: &[SlackFileResponse], token: Option<&str>) -> Vec<Card> {
    files
        .iter()
        .filter_map(|file| slack_file_card(file, token))
        .collect()
}

fn slack_file_card(file: &SlackFileResponse, token: Option<&str>) -> Option<Card> {
    // Slack uses `mode: "tombstone"` (and similar) for deleted/expired files.
    if file
        .mode
        .as_deref()
        .is_some_and(|mode| matches!(mode, "tombstone" | "hidden_by_limit"))
    {
        return None;
    }

    let title = file
        .title
        .clone()
        .and_then(non_empty_string)
        .or_else(|| file.name.clone().and_then(non_empty_string))
        .map(|text| arc_str(replace_slack_emoji_codes(&text)));
    let permalink = file
        .permalink
        .clone()
        .and_then(non_empty_string)
        .map(arc_str);

    if slack_file_is_image(file) {
        let image = slack_file_image_media(file, token)?;
        return Some(Card {
            kind: CardKind::MediaPreview,
            source: CardSource::Slack,
            title,
            subtitle: None,
            body: None,
            footer: None,
            url: permalink,
            accent_color: None,
            thumbnail: None,
            image: Some(image),
            fields: Vec::new(),
            actions: Vec::new(),
        });
    }

    let url = file
        .url_private_download
        .clone()
        .and_then(non_empty_string)
        .map(arc_str)
        .or_else(|| permalink.clone());
    // Without at least a title or a link there is nothing useful to render.
    if title.is_none() && url.is_none() {
        return None;
    }
    Some(Card {
        kind: CardKind::ProviderAttachment,
        source: CardSource::Slack,
        title,
        subtitle: file
            .filetype
            .clone()
            .and_then(non_empty_string)
            .map(|filetype| arc_str(filetype.to_ascii_uppercase())),
        body: None,
        footer: None,
        url,
        accent_color: None,
        thumbnail: None,
        image: None,
        fields: Vec::new(),
        actions: Vec::new(),
    })
}

fn slack_file_is_image(file: &SlackFileResponse) -> bool {
    if file
        .mimetype
        .as_deref()
        .is_some_and(|mimetype| mimetype.starts_with("image/"))
    {
        return true;
    }
    file.filetype.as_deref().is_some_and(|filetype| {
        matches!(
            filetype.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "heic" | "heif" | "tiff"
        )
    })
}

fn slack_file_image_media(file: &SlackFileResponse, token: Option<&str>) -> Option<Media> {
    // Prefer the full-resolution private URL; fall back to the largest available
    // thumbnail. All of these require Bearer auth to download.
    let url = file
        .url_private
        .clone()
        .or_else(|| file.thumb_1024.clone())
        .or_else(|| file.thumb_720.clone())
        .or_else(|| file.thumb_360.clone())
        .and_then(non_empty_string)?;
    let file_name = file
        .name
        .clone()
        .and_then(non_empty_string)
        .or_else(|| file.title.clone().and_then(non_empty_string))
        .unwrap_or_else(|| "image".to_owned());
    let mime_type = file
        .mimetype
        .clone()
        .and_then(non_empty_string)
        .unwrap_or_else(|| "image/*".to_owned());
    Some(Media {
        id: arc_str(&url),
        file_name: arc_str(file_name),
        mime_type: arc_str(mime_type),
        size_bytes: file.size,
        caption: None,
        // Large uploads are not fetched eagerly: only the deterministic cache
        // path is reserved so the UI can offer on-demand retrieval (see
        // `download_media`) and detect when the bytes have arrived.
        local_path: if file
            .size
            .is_some_and(|size| size > SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES)
        {
            slack_media_cache_file_path(&url, "files")
        } else {
            slack_cached_media_path(&url, "files", token)
        },
        thumbnail: None,
    })
}

fn push_unique_text_part(parts: &mut Vec<String>, value: Option<String>) {
    let Some(value) = value.and_then(non_empty_string) else {
        return;
    };
    if !parts.iter().any(|existing| existing == &value) {
        parts.push(value);
    }
}

fn slack_message_sender(sender_id: &str, metadata: SlackMessageSenderMetadata) -> Sender {
    Sender {
        platform_id: arc_str(sender_id),
        display_name: arc_str(
            metadata
                .display_name
                .unwrap_or_else(|| sender_id.to_owned()),
        ),
        avatar: metadata.avatar_url.as_deref().and_then(slack_avatar_path),
    }
}

fn slack_message_sender_metadata(
    username: Option<String>,
    icons: Option<SlackMessageIconsResponse>,
    bot_profile: Option<SlackBotProfileResponse>,
) -> SlackMessageSenderMetadata {
    let bot_name = bot_profile.as_ref().and_then(|profile| {
        profile
            .real_name
            .clone()
            .and_then(non_empty_string)
            .or_else(|| profile.name.clone().and_then(non_empty_string))
    });
    let bot_icons = bot_profile
        .as_ref()
        .and_then(|profile| profile.icons.clone());
    let icons = icons.or(bot_icons);

    SlackMessageSenderMetadata {
        display_name: bot_name.or_else(|| username.and_then(non_empty_string)),
        avatar_url: icons.and_then(|icons| icons.best_image_url()),
    }
}

impl SlackMessageIconsResponse {
    fn best_image_url(self) -> Option<String> {
        self.image_72
            .and_then(non_empty_string)
            .or_else(|| self.image_48.and_then(non_empty_string))
            .or_else(|| self.image_36.and_then(non_empty_string))
            .or_else(|| self.image_original.and_then(non_empty_string))
            .or_else(|| self.icon_url.and_then(non_empty_string))
    }
}

fn slack_reactions(reactions: Option<Vec<SlackReactionResponse>>) -> Vec<Reaction> {
    reactions
        .unwrap_or_default()
        .into_iter()
        .filter_map(|reaction| {
            let emoji = reaction.name.and_then(non_empty_string)?;
            let emoji = slack_emoji_display(&emoji);
            let senders = reaction
                .users
                .unwrap_or_default()
                .into_iter()
                .filter_map(non_empty_string)
                .map(arc_str)
                .collect::<Vec<_>>();
            Some(Reaction {
                emoji: arc_str(emoji),
                senders,
            })
        })
        .collect()
}

fn unresolved_slack_user_ids(messages: &[Message]) -> Vec<String> {
    let mut user_ids = Vec::new();
    for message in messages {
        for user_id in slack_user_ids_in_text(&content_text(&message.content)) {
            if !user_ids.iter().any(|existing| existing == &user_id) {
                user_ids.push(user_id);
            }
        }

        let user_id = message.sender.platform_id.as_ref();
        if message.sender.display_name.as_ref() == user_id
            && is_slack_user_id(user_id)
            && !user_ids.iter().any(|existing| existing == user_id)
        {
            user_ids.push(user_id.to_owned());
        }
    }
    user_ids
}

fn apply_cached_user_to_message(
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
    message: &mut Message,
) {
    if let Some(user) = read_lock(users).get(message.sender.platform_id.as_ref()) {
        message.sender = user.sender();
    }

    if matches!(&message.content, Content::Text(_)) {
        if let Content::Text(text) = &message.content {
            let replaced = replace_slack_user_mentions(text, users);
            if replaced != text.as_ref() {
                message.content = Content::Text(arc_str(replaced));
            }
        }
    } else if let Content::Cards(cards) = &message.content {
        let replaced_cards = replace_slack_mentions_in_cards(cards, users);
        message.platform_data.cards = replaced_cards.clone();
        message.content = Content::Cards(replaced_cards);
    }
}

fn replace_slack_mentions_in_cards(
    cards: &[Card],
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
) -> Vec<Card> {
    cards
        .iter()
        .cloned()
        .map(|mut card| {
            card.title = card
                .title
                .map(|text| arc_str(replace_slack_user_mentions(&text, users)));
            card.subtitle = card
                .subtitle
                .map(|text| arc_str(replace_slack_user_mentions(&text, users)));
            card.body = card
                .body
                .map(|text| arc_str(replace_slack_user_mentions(&text, users)));
            card.footer = card
                .footer
                .map(|text| arc_str(replace_slack_user_mentions(&text, users)));
            for field in &mut card.fields {
                field.title = field
                    .title
                    .clone()
                    .map(|text| arc_str(replace_slack_user_mentions(&text, users)));
                field.value = arc_str(replace_slack_user_mentions(&field.value, users));
            }
            card
        })
        .collect()
}

fn content_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.to_string(),
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media
            .caption
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| media.file_name.to_string()),
        Content::LinkPreview(link) => {
            let title = link.title.as_deref().unwrap_or("Link");
            format!("{title}: {}", link.url)
        }
        Content::Cards(cards) => cards
            .iter()
            .map(slack_card_fallback_text)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Content::Poll(poll) => format!("Poll: {}", poll.question),
        Content::Deleted => "deleted message".to_owned(),
        Content::Unsupported(text) => text.to_string(),
    }
}

fn slack_card_fallback_text(card: &Card) -> String {
    let mut parts = Vec::new();
    if let Some(subtitle) = &card.subtitle {
        parts.push(subtitle.to_string());
    }
    if let Some(title) = &card.title {
        parts.push(title.to_string());
    }
    if let Some(body) = &card.body {
        parts.push(body.to_string());
    }
    for field in &card.fields {
        parts.push(
            field
                .title
                .as_deref()
                .map(|title| format!("{title}: {}", field.value))
                .unwrap_or_else(|| field.value.to_string()),
        );
    }
    if let Some(footer) = &card.footer {
        parts.push(footer.to_string());
    }
    if let Some(url) = &card.url {
        parts.push(url.to_string());
    }
    parts.join("\n")
}

/// Rewrite `@here`/`@channel`/`@everyone` tokens (at a word boundary) into
/// Slack's native broadcast mention form (`<!here>` etc.) for outbound text.
fn rewrite_slack_broadcast_mentions(text: &str) -> String {
    if !text.contains('@') {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '@' && (index == 0 || !chars[index - 1].is_alphanumeric()) {
            let rest: String = chars[index + 1..].iter().collect();
            let lower = rest.to_lowercase();
            let mut matched = None;
            for keyword in ["everyone", "channel", "here"] {
                if lower.starts_with(keyword) {
                    let after = index + 1 + keyword.len();
                    if after >= chars.len() || !chars[after].is_alphanumeric() {
                        matched = Some(keyword);
                        break;
                    }
                }
            }
            if let Some(keyword) = matched {
                output.push_str("<!");
                output.push_str(keyword);
                output.push('>');
                index += 1 + keyword.len();
                continue;
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

fn slack_user_ids_in_text(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let token = &rest[..end];
        let id = token.split('|').next().unwrap_or_default().trim();
        if is_slack_user_id(id) && !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_owned());
        }
        rest = &rest[end + 1..];
    }
    ids
}

/// Returns true when `text` mentions the authenticated user. This covers both
/// explicit `<@U123>` mentions of the current user id and Slack broadcast pings
/// (`<!here>`, `<!channel>`, `<!everyone>`), which directly target the user's
/// attention and are therefore treated as mentions for notification scoping.
fn slack_text_mentions_user(text: &str, current_user_id: Option<&str>) -> bool {
    if slack_text_has_broadcast_mention(text) {
        return true;
    }
    let Some(current_user_id) = current_user_id.filter(|id| !id.is_empty()) else {
        return false;
    };
    slack_user_ids_in_text(text)
        .iter()
        .any(|id| id == current_user_id)
}

/// Detects Slack broadcast mention tokens of the form `<!here>`, `<!channel>`,
/// and `<!everyone>` (optionally carrying a `|label`, e.g. `<!here|here>`).
fn slack_text_has_broadcast_mention(text: &str) -> bool {
    let mut rest = text;
    while let Some(start) = rest.find("<!") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            break;
        };
        let token = &rest[..end];
        let keyword = token.split('|').next().unwrap_or_default().trim();
        if matches!(keyword, "here" | "channel" | "everyone") {
            return true;
        }
        rest = &rest[end + 1..];
    }
    false
}

fn replace_slack_user_mentions(
    text: &str,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
) -> String {
    let users = read_lock(users);
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        output.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else {
            output.push_str("<@");
            output.push_str(rest);
            return output;
        };
        let token = &rest[..end];
        let id = token.split('|').next().unwrap_or_default().trim();
        if let Some(user) = users.get(id) {
            output.push('@');
            output.push_str(user.best_name());
        } else if let Some(label) = token
            .split_once('|')
            .and_then(|(_, label)| non_empty_string(label.to_owned()))
        {
            output.push('@');
            output.push_str(&label);
        } else if is_slack_user_id(id) {
            output.push('@');
            output.push_str(id);
        } else {
            output.push_str("<@");
            output.push_str(token);
            output.push('>');
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

fn slack_reaction_matches(reaction_emoji: &str, slack_name: &str) -> bool {
    normalize_slack_reaction_name(reaction_emoji).as_deref() == Some(slack_name)
        || slack_emoji_display(slack_name) == reaction_emoji
}

fn normalize_slack_reaction_name(emoji: &str) -> Option<String> {
    let trimmed = emoji.trim().trim_matches(':').trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(slack_unicode_emoji_name(trimmed).unwrap_or_else(|| trimmed.to_owned()))
}

fn slack_unicode_emoji_name(value: &str) -> Option<String> {
    emojis::get(value)
        .and_then(|emoji| emoji.shortcode())
        .map(str::to_owned)
}

fn slack_emoji_display(name: &str) -> String {
    let name = name.trim().trim_matches(':').trim();
    if name.is_empty() {
        return String::new();
    }

    slack_emoji_lookup(name)
        .map(str::to_owned)
        .unwrap_or_else(|| format_slack_custom_emoji(name))
}

fn slack_emoji_lookup(name: &str) -> Option<&'static str> {
    slack_builtin_emoji_alias(name)
        .or_else(|| slack_skin_tone_emoji(name))
        .or_else(|| slack_composite_skin_tone_emoji(name))
        .or_else(|| emojis::get_by_shortcode(name).map(|emoji| emoji.as_str()))
        .or_else(|| slack_cldr_name_emoji(name))
}

fn slack_cldr_name_emoji(name: &str) -> Option<&'static str> {
    let normalized = name.replace('-', "_").to_ascii_lowercase();
    emojis::iter().find_map(|emoji| {
        let emoji_name = emoji.name().replace([' ', '-'], "_").to_ascii_lowercase();
        (emoji_name == normalized).then_some(emoji.as_str())
    })
}

fn slack_composite_skin_tone_emoji(name: &str) -> Option<&'static str> {
    let (base, tone) = name.split_once("::skin-tone-")?;
    let base = slack_emoji_lookup(base.trim_matches(':'))?;
    let tone = slack_skin_tone(tone.trim_matches(':'))?;
    emojis::get(base)?
        .with_skin_tone(tone)
        .map(|emoji| emoji.as_str())
}

fn slack_skin_tone_emoji(name: &str) -> Option<&'static str> {
    match name.trim_matches(':') {
        "skin-tone-2" => Some("🏻"),
        "skin-tone-3" => Some("🏼"),
        "skin-tone-4" => Some("🏽"),
        "skin-tone-5" => Some("🏾"),
        "skin-tone-6" => Some("🏿"),
        _ => None,
    }
}

fn slack_skin_tone(name: &str) -> Option<emojis::SkinTone> {
    match name {
        "2" | "skin-tone-2" => Some(emojis::SkinTone::Light),
        "3" | "skin-tone-3" => Some(emojis::SkinTone::MediumLight),
        "4" | "skin-tone-4" => Some(emojis::SkinTone::Medium),
        "5" | "skin-tone-5" => Some(emojis::SkinTone::MediumDark),
        "6" | "skin-tone-6" => Some(emojis::SkinTone::Dark),
        _ => None,
    }
}

fn replace_slack_emoji_codes(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find(':') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];

        let Some(end) = rest.find(':') else {
            output.push(':');
            output.push_str(rest);
            return output;
        };

        let candidate = &rest[..end];
        if is_slack_emoji_name(candidate) {
            output.push_str(&slack_emoji_display(candidate));
            rest = &rest[end + 1..];
        } else {
            output.push(':');
            output.push_str(candidate);
            output.push(':');
            rest = &rest[end + 1..];
        }
    }

    output.push_str(rest);
    output
}

fn is_slack_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        })
}

fn format_slack_custom_emoji(name: &str) -> String {
    format!(":{}:", name.trim().trim_matches(':').trim())
}

fn slack_builtin_emoji_alias(name: &str) -> Option<&'static str> {
    match name {
        "+1" | "thumbsup" => Some("👍"),
        "-1" | "thumbsdown" => Some("👎"),
        "white_check_mark" => Some("✅"),
        "large_green_circle" => Some("🟢"),
        "large_yellow_circle" => Some("🟡"),
        "large_orange_circle" => Some("🟠"),
        "large_red_square" => Some("🟥"),
        "large_blue_square" => Some("🟦"),
        "large_green_square" => Some("🟩"),
        "large_yellow_square" => Some("🟨"),
        "large_orange_square" => Some("🟧"),
        "large_purple_square" => Some("🟪"),
        "large_brown_square" => Some("🟫"),
        "heavy_check_mark" => Some("✔️"),
        "heavy_multiplication_x" => Some("✖️"),
        "heavy_plus_sign" => Some("➕"),
        "heavy_minus_sign" => Some("➖"),
        "heavy_division_sign" => Some("➗"),
        _ => None,
    }
}

fn slack_avatar_path(url: &str) -> Option<PathBuf> {
    slack_cached_media_path(url, "avatars", None)
}

/// Resolve (and lazily download) a Slack media URL into a local cache path.
///
/// Returns the eventual cache path immediately; the bytes are fetched on a
/// background thread if not already cached. When `auth_token` is provided the
/// download includes a `Bearer` header, which is required for authenticated
/// file uploads (`url_private`). Public CDN assets (avatars) pass `None`.
///
/// Background fetches are bounded by [`SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES`]
/// and deduplicated: a URL that is already downloading, or that failed within
/// [`SLACK_MEDIA_FAILURE_RETRY_COOLDOWN`], is not queued again.
fn slack_cached_media_path(url: &str, subdir: &str, auth_token: Option<&str>) -> Option<PathBuf> {
    let url = url.trim();
    let path = slack_media_cache_file_path(url, subdir)?;
    if path.exists() {
        return Some(path);
    }
    if !slack_begin_media_download(url, false) {
        // Already in flight or in failure cooldown; the reserved path is still
        // the right answer for callers, the bytes just are not there (yet).
        return Some(path);
    }

    let url = url.to_owned();
    let subdir = subdir.to_owned();
    let auth_token = auth_token.map(str::to_owned);
    slack_diagnostic_log(
        "slack.provider.media_path",
        format!(
            "subdir={} url={} path={} status=queued",
            subdir,
            url,
            path.display()
        ),
    );
    let path_for_download = path.clone();
    std::thread::spawn(move || {
        let result = slack_download_media_to_path(
            &url,
            auth_token.as_deref(),
            &path_for_download,
            SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES,
        );
        slack_finish_media_download(&url, result.is_ok());
        match result {
            Ok(()) => slack_diagnostic_log(
                "slack.provider.media_cached",
                format!(
                    "subdir={} url={} path={}",
                    subdir,
                    url,
                    path_for_download.display()
                ),
            ),
            Err(error) => slack_diagnostic_log(
                "slack.provider.media_download_failed",
                format!(
                    "subdir={} url={} path={} error={:#}",
                    subdir,
                    url,
                    path_for_download.display(),
                    error
                ),
            ),
        };
    });

    Some(path)
}

/// Computes the deterministic cache path for a Slack media URL without
/// downloading anything. Used both to reserve a destination for on-demand
/// (user-initiated) downloads of large files and by the eager cache path.
fn slack_media_cache_file_path(url: &str, subdir: &str) -> Option<PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("chat-cli")
        .join("slack")
        .join(subdir);
    let extension = url
        .split('?')
        .next()
        .and_then(|path| path.rsplit('.').next())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 5
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or("img");
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    Some(cache_dir.join(format!("{:016x}.{extension}", hasher.finish())))
}

#[derive(Default)]
struct SlackMediaDownloadRegistry {
    in_flight: HashSet<String>,
    failed_at: HashMap<String, Instant>,
}

fn slack_media_download_registry() -> &'static Mutex<SlackMediaDownloadRegistry> {
    static REGISTRY: OnceLock<Mutex<SlackMediaDownloadRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Registers `url` as downloading. Returns `false` (caller must not download)
/// when the URL is already in flight, or when it failed recently and `force`
/// is not set. `force` is used by explicit user-initiated retrieval, which may
/// retry through the failure cooldown but never alongside an active download.
fn slack_begin_media_download(url: &str, force: bool) -> bool {
    let mut registry = slack_media_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.in_flight.contains(url) {
        return false;
    }
    if !force
        && registry
            .failed_at
            .get(url)
            .is_some_and(|failed_at| failed_at.elapsed() < SLACK_MEDIA_FAILURE_RETRY_COOLDOWN)
    {
        return false;
    }
    registry.in_flight.insert(url.to_owned());
    true
}

fn slack_finish_media_download(url: &str, success: bool) {
    let mut registry = slack_media_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.in_flight.remove(url);
    if success {
        registry.failed_at.remove(url);
    } else {
        registry.failed_at.insert(url.to_owned(), Instant::now());
    }
}

/// Downloads a Slack media URL to `path`, streaming the body to a temporary
/// sibling file and renaming it into place so `path.exists()` never observes a
/// partially written cache entry. `limit_bytes` bounds the body size
/// explicitly (ureq otherwise rejects bodies above 10 MiB).
fn slack_download_media_to_path(
    url: &str,
    auth_token: Option<&str>,
    path: &Path,
    limit_bytes: u64,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Slack media cache path has no parent directory"))?;
    fs::create_dir_all(parent).context("creating Slack media cache directory")?;

    let mut request = slack_http_agent().get(url);
    if let Some(token) = auth_token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    let mut response = request.call().context("downloading Slack media")?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(limit_bytes)
        .reader();

    let temp_path = path.with_extension("part");
    let result = (|| -> Result<()> {
        let mut file = fs::File::create(&temp_path).context("creating Slack media cache file")?;
        std::io::copy(&mut reader, &mut file).context("reading Slack media body")?;
        file.flush().context("flushing Slack media cache file")?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    fs::rename(&temp_path, path).context("storing Slack media cache file")?;
    Ok(())
}

fn fallback_slack_user(user_id: &str) -> Option<SlackUser> {
    is_slack_user_id(user_id).then(|| SlackUser {
        id: user_id.to_owned(),
        name: Some(user_id.to_owned()),
        real_name: None,
        display_name: Some(user_id.to_owned()),
        avatar: None,
        is_bot: false,
        deleted: false,
        ..SlackUser::default()
    })
}

/// Placeholder for bot senders (`B...` ids), which `users.info` cannot
/// resolve. The id doubles as the display name until richer `bot_profile`
/// metadata supplies a real one.
fn fallback_slack_bot_user(bot_id: &str) -> SlackUser {
    SlackUser {
        id: bot_id.to_owned(),
        name: Some(bot_id.to_owned()),
        real_name: None,
        display_name: Some(bot_id.to_owned()),
        avatar: None,
        is_bot: true,
        deleted: false,
        ..SlackUser::default()
    }
}

fn is_slack_user_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('U' | 'W'))
        && chars.all(|character| character.is_ascii_alphanumeric())
}

fn is_slack_bot_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('B'))
        && chars.clone().next().is_some()
        && chars.all(|character| character.is_ascii_alphanumeric())
}

fn slack_timestamp_from_datetime(timestamp: Timestamp) -> String {
    format!(
        "{}.{:06}",
        timestamp.timestamp(),
        timestamp.timestamp_subsec_micros()
    )
}

fn slack_ts_to_timestamp(ts: &str) -> Option<Timestamp> {
    let seconds = ts.split('.').next()?.parse::<i64>().ok()?;
    DateTime::<Utc>::from_timestamp(seconds, 0)
}

/// Render the contact's current local clock time (HH:MM) from their Slack
/// `tz_offset` (seconds east of UTC). Returns `None` for implausible offsets.
fn slack_local_time_for_offset(offset_seconds: i64) -> Option<String> {
    if offset_seconds.abs() > 14 * 3600 {
        return None;
    }
    let local = Utc::now() + chrono::Duration::seconds(offset_seconds);
    Some(local.format("%H:%M").to_string())
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            character => vec![character],
        })
        .collect()
}

fn sort_chats(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| slack_chat_sort_bucket(a).cmp(&slack_chat_sort_bucket(b)))
            .then_with(|| b.unread_count.cmp(&a.unread_count))
            .then_with(|| b.last_message_at.cmp(&a.last_message_at))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn slack_chat_sort_bucket(chat: &Chat) -> u8 {
    if chat.membership == ChatMembership::NotJoined {
        return 5;
    }
    match chat.kind {
        ChatKind::PublicChannel | ChatKind::PrivateChannel => 0,
        ChatKind::Direct => 1,
        ChatKind::GroupDirectMessage => 2,
        ChatKind::Group => 3,
    }
}

fn slack_http_agent() -> ureq::Agent {
    // A single pooled agent is reused for every request so kept-alive
    // connections (and their TLS handshakes) are shared across calls. Building
    // a fresh agent per request, as this used to, forced a new TLS handshake
    // for every poll and was a major source of background CPU.
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let config = ureq::Agent::config_builder()
                .timeout_global(Some(SLACK_HTTP_TIMEOUT))
                // Surface 4xx/5xx as ordinary responses so callers can inspect
                // the status (notably 429) and honor Retry-After instead of
                // collapsing it into an opaque transport error.
                .http_status_as_error(false)
                .build();
            ureq::Agent::new_with_config(config)
        })
        .clone()
}

/// Paces Slack Web API requests to a global minimum spacing so concurrent poll
/// loops never burst past Slack's per-method rate limits. Each caller reserves
/// the next slot and sleeps (on its blocking worker thread) until then.
fn slack_throttle_web_api() {
    static NEXT_ALLOWED: Mutex<Option<Instant>> = Mutex::new(None);
    let now = Instant::now();
    let proceed_at = {
        let mut guard = NEXT_ALLOWED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let at = guard.filter(|next| *next > now).unwrap_or(now);
        *guard = Some(at + SLACK_MIN_REQUEST_SPACING);
        at
    };
    if let Some(wait) = proceed_at.checked_duration_since(now) {
        std::thread::sleep(wait);
    }
}

/// Sends a throttled Slack GET, retrying on HTTP 429 while honoring the
/// server's `Retry-After`. `send` is re-invoked per attempt because each ureq
/// request builder is consumed by `.call()`. Must run on a blocking thread.
fn slack_get_with_retry(
    context_label: &'static str,
    send: impl Fn() -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let mut attempt = 0;
    loop {
        slack_throttle_web_api();
        let response = send().context(context_label)?;
        if response.status().as_u16() == 429 && attempt < SLACK_RATE_LIMIT_MAX_RETRIES {
            attempt += 1;
            let backoff = slack_retry_after(&response).unwrap_or(SLACK_RATE_LIMIT_DEFAULT_BACKOFF);
            slack_diagnostic_log(
                "slack.web_api.rate_limited",
                format!(
                    "context={context_label} retry_after_s={} attempt={attempt}",
                    backoff.as_secs()
                ),
            );
            std::thread::sleep(backoff);
            continue;
        }
        return Ok(response);
    }
}

fn slack_retry_after(response: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    let seconds = response
        .headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds.clamp(1, 60)))
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn arc_opt(value: String) -> Option<Arc<str>> {
    if value.trim().is_empty() {
        None
    } else {
        Some(arc_str(value))
    }
}

fn sanitize_slack_error(error: &anyhow::Error) -> String {
    redact_slack_secrets(&error.to_string())
}

fn redact_slack_secrets(value: &str) -> String {
    let mut redacted = value.to_owned();
    for prefix in ["xoxp-", "xoxb-", "xapp-", "xoxa-"] {
        redacted = redact_prefixed_secret(&redacted, prefix);
    }
    redact_prefixed_secret(&redacted, "https://hooks.slack.com/services/")
}

fn redact_prefixed_secret(value: &str, prefix: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;

    while let Some(index) = remaining.find(prefix) {
        let (before, from_secret) = remaining.split_at(index);
        output.push_str(before);
        output.push_str("<redacted>");

        let secret_end = from_secret
            .char_indices()
            .find_map(|(offset, character)| {
                if offset > 0 && is_secret_delimiter(character) {
                    Some(offset)
                } else {
                    None
                }
            })
            .unwrap_or(from_secret.len());
        remaining = &from_secret[secret_end..];
    }

    output.push_str(remaining);
    output
}

fn is_secret_delimiter(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            ',' | ';' | ')' | ']' | '}' | '\'' | '"' | '<' | '>'
        )
}

fn has_value(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

fn slack_app_manifest_url(auth_mode: &SlackAuthMode) -> String {
    let user_scopes = auth_mode.user_scopes();
    let bot_scopes = auth_mode.bot_scopes();
    let user_scope_section = manifest_scope_section("user", user_scopes, 4);
    let bot_scope_section = manifest_scope_section("bot", bot_scopes, 4);
    let bot_user_section = if bot_scopes.is_empty() {
        String::new()
    } else {
        "features:\n  bot_user:\n    display_name: chat-cli\n    always_online: false\n".to_owned()
    };
    let event_subscriptions_section = manifest_event_subscriptions_section(auth_mode);
    let redirect_urls_section = manifest_redirect_urls_section(auth_mode);
    let manifest = format!(
        "_metadata:\n  major_version: 2\n  minor_version: 1\ndisplay_information:\n  name: {}\n  description: Terminal chat client Slack integration\n{}oauth_config:\n{}  scopes:\n{}{}settings:\n  org_deploy_enabled: false\n  socket_mode_enabled: true\n{}  token_rotation_enabled: false\n",
        manifest_yaml_string("chat-cli"),
        bot_user_section,
        redirect_urls_section,
        user_scope_section,
        bot_scope_section,
        event_subscriptions_section
    );
    format!(
        "https://api.slack.com/apps?new_app=1&manifest_yaml={}",
        url_component(&manifest)
    )
}

fn manifest_redirect_urls_section(auth_mode: &SlackAuthMode) -> String {
    if !auth_mode.supports_oauth_code_exchange() {
        return String::new();
    }

    format!(
        "  redirect_urls:\n    - {}\n",
        default_advertised_oauth_redirect_uri()
    )
}

/// The redirect URI a freshly generated app manifest should register, and the
/// default advertised to Slack when no per-submission/options override exists.
/// A distributor-configured official app takes precedence; otherwise this is
/// the HTTPS relay URL that Slack accepts for the default app flow.
fn default_advertised_oauth_redirect_uri() -> String {
    official_slack_app()
        .map(|app| app.redirect_uri)
        .and_then(non_empty_string)
        .unwrap_or_else(|| SLACK_OAUTH_REDIRECT_URI.to_owned())
}

fn manifest_event_subscriptions_section(auth_mode: &SlackAuthMode) -> String {
    if !auth_mode.supports_realtime() {
        return String::new();
    }

    let user_events = [
        "message.channels",
        "message.groups",
        "message.im",
        "message.mpim",
        "reaction_added",
        "reaction_removed",
    ];
    let bot_events = if bot_scopes_support_realtime_events(auth_mode.bot_scopes()) {
        user_events.to_vec()
    } else {
        Vec::new()
    };
    let user_events_section = manifest_event_list_section("user_events", &user_events, 4);
    let bot_events_section = manifest_event_list_section("bot_events", &bot_events, 4);

    format!("  event_subscriptions:\n{user_events_section}{bot_events_section}")
}

fn bot_scopes_support_realtime_events(bot_scopes: &str) -> bool {
    let scopes = bot_scopes
        .split(',')
        .map(str::trim)
        .collect::<std::collections::HashSet<_>>();

    [
        "channels:history",
        "groups:history",
        "im:history",
        "mpim:history",
        "reactions:read",
    ]
    .into_iter()
    .all(|scope| scopes.contains(scope))
}

fn manifest_event_list_section(label: &str, events: &[&str], indent: usize) -> String {
    if events.is_empty() {
        return String::new();
    }

    let prefix = " ".repeat(indent);
    let item_prefix = " ".repeat(indent + 2);
    let mut lines = vec![format!("{prefix}{label}:")];
    lines.extend(events.iter().map(|event| format!("{item_prefix}- {event}")));
    format!("{}\n", lines.join("\n"))
}

fn manifest_scope_section(label: &str, scopes: &str, indent: usize) -> String {
    let scopes = scopes
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return String::new();
    }

    let prefix = " ".repeat(indent);
    let item_prefix = " ".repeat(indent + 2);
    let mut lines = vec![format!("{prefix}{label}:")];
    lines.extend(
        scopes
            .into_iter()
            .map(|scope| format!("{item_prefix}- {scope}")),
    );
    format!("{}\n", lines.join("\n"))
}

fn manifest_yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn url_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn arc_str(value: impl AsRef<str>) -> Arc<str> {
    Arc::from(value.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::Provider;
    use std::sync::Mutex;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct PostedCall {
        kind: SlackCredentialKind,
        token: String,
        channel: String,
        text: String,
        thread_ts: Option<String>,
    }

    #[derive(Default)]
    struct FakeSlackApiClient {
        validated_tokens: Mutex<Vec<SlackCredentialKind>>,
        validated_webhooks: Mutex<Vec<String>>,
        team_info_calls: Mutex<Vec<(SlackCredentialKind, Option<String>)>>,
        fail_team_info_for: Mutex<Vec<SlackCredentialKind>>,
        posted_messages: Mutex<Vec<PostedCall>>,
        updated_messages: Mutex<Vec<PostedCall>>,
        posted_webhooks: Mutex<Vec<(String, String)>>,
        listed_conversations: Mutex<Vec<SlackCredentialKind>>,
        conversations: Mutex<Vec<SlackConversation>>,
        users: Mutex<HashMap<String, SlackUser>>,
        user_info_calls: Mutex<Vec<String>>,
        conversation_members: Mutex<HashMap<String, Vec<String>>>,
        added_reactions: Mutex<Vec<(String, String, String)>>,
        removed_reactions: Mutex<Vec<(String, String, String)>>,
        history_calls: Mutex<Vec<(SlackCredentialKind, ChatId, Option<Timestamp>, usize)>>,
        history_messages: Mutex<Vec<Message>>,
        opened_socket_modes: Mutex<Vec<SlackCredentialKind>>,
        oauth_exchanges: Mutex<Vec<(String, String, String, String)>>,
        oauth_tokens: Mutex<Option<SlackOAuthTokens>>,
        fail_oauth_exchange: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl SlackApiClient for FakeSlackApiClient {
        async fn validate_token(
            &self,
            credential: SlackCredential,
        ) -> Result<SlackValidatedCredential> {
            let kind = credential.kind.clone();
            let value = credential.value().to_owned();
            self.validated_tokens.lock().unwrap().push(kind);

            if value.contains("invalid") {
                bail!("Slack auth.test failed for token {value}: invalid_auth");
            }

            if value.starts_with("xoxb-") {
                let mut credential = SlackValidatedCredential::bot("T123", "B123");
                credential.team_name = Some("Example Workspace".to_owned());
                Ok(credential)
            } else if value.starts_with("xapp-") {
                Ok(SlackValidatedCredential::app())
            } else {
                let mut credential = SlackValidatedCredential::user("T123", "U123");
                credential.team_name = Some("Example Workspace".to_owned());
                Ok(credential)
            }
        }

        async fn team_info(
            &self,
            credential: SlackCredential,
            team_id: Option<&str>,
        ) -> Result<Option<SlackTeamInfo>> {
            let kind = credential.kind.clone();
            self.team_info_calls
                .lock()
                .unwrap()
                .push((kind.clone(), team_id.map(str::to_owned)));
            if self.fail_team_info_for.lock().unwrap().contains(&kind) {
                bail!("Slack team.info failed for {:?}: missing_scope", kind);
            }
            Ok(Some(SlackTeamInfo {
                id: Some("T123".to_owned()),
                name: Some("Example Workspace".to_owned()),
                domain: Some("example".to_owned()),
                email_domain: Some("example.com".to_owned()),
                icon_url: Some("https://example.com/team-icon-230.png".to_owned()),
            }))
        }

        async fn validate_webhook(&self, webhook_url: &str) -> Result<SlackWebhookValidation> {
            self.validated_webhooks
                .lock()
                .unwrap()
                .push(webhook_url.to_owned());
            validate_webhook_url(webhook_url)
        }

        async fn post_message(
            &self,
            credential: SlackCredential,
            channel: &str,
            text: &str,
            thread_ts: Option<&str>,
        ) -> Result<SlackPostedMessage> {
            if credential.value().contains("post-fail") {
                bail!(
                    "Slack chat.postMessage failed for token {}",
                    credential.value()
                );
            }

            self.posted_messages.lock().unwrap().push(PostedCall {
                kind: credential.kind,
                token: credential.value,
                channel: channel.to_owned(),
                text: text.to_owned(),
                thread_ts: thread_ts.map(str::to_owned),
            });
            Ok(SlackPostedMessage {
                channel: Some(channel.to_owned()),
                ts: "1710000000.000100".to_owned(),
            })
        }

        async fn update_message(
            &self,
            credential: SlackCredential,
            channel: &str,
            ts: &str,
            text: &str,
        ) -> Result<SlackPostedMessage> {
            if credential.value().contains("update-fail") {
                bail!(slack_update_error_message("cant_update_message"));
            }
            // `thread_ts` records the edited message's `ts` for assertions.
            self.updated_messages.lock().unwrap().push(PostedCall {
                kind: credential.kind,
                token: credential.value,
                channel: channel.to_owned(),
                text: text.to_owned(),
                thread_ts: Some(ts.to_owned()),
            });
            Ok(SlackPostedMessage {
                channel: Some(channel.to_owned()),
                ts: ts.to_owned(),
            })
        }

        async fn post_webhook(&self, webhook_url: &str, text: &str) -> Result<SlackPostedMessage> {
            self.posted_webhooks
                .lock()
                .unwrap()
                .push((webhook_url.to_owned(), text.to_owned()));
            Ok(SlackPostedMessage {
                channel: None,
                ts: "webhook:test".to_owned(),
            })
        }

        async fn upload_file(
            &self,
            credential: SlackCredential,
            request: SlackUploadFileRequest,
        ) -> Result<SlackUploadedFile> {
            self.posted_messages.lock().unwrap().push(PostedCall {
                kind: credential.kind,
                token: credential.value,
                channel: request.channel,
                text: request.initial_comment.unwrap_or_default(),
                thread_ts: request.thread_ts,
            });
            Ok(SlackUploadedFile {
                id: "F123UPLOAD".to_owned(),
                title: Some(request.title),
            })
        }

        async fn list_conversations(
            &self,
            credential: SlackCredential,
        ) -> Result<Vec<SlackConversation>> {
            if credential.value().contains("list-fail") {
                bail!(
                    "Slack conversations.list failed for token {}",
                    credential.value()
                );
            }
            self.listed_conversations
                .lock()
                .unwrap()
                .push(credential.kind);
            Ok(self.conversations.lock().unwrap().clone())
        }

        async fn user_info(
            &self,
            _credential: SlackCredential,
            user_id: &str,
        ) -> Result<Option<SlackUser>> {
            self.user_info_calls
                .lock()
                .unwrap()
                .push(user_id.to_owned());
            Ok(self.users.lock().unwrap().get(user_id).cloned())
        }

        async fn conversation_members(
            &self,
            _credential: SlackCredential,
            channel: &str,
        ) -> Result<Vec<String>> {
            Ok(self
                .conversation_members
                .lock()
                .unwrap()
                .get(channel)
                .cloned()
                .unwrap_or_default())
        }

        async fn add_reaction(
            &self,
            _credential: SlackCredential,
            channel: &str,
            timestamp: &str,
            emoji: &str,
        ) -> Result<()> {
            self.added_reactions.lock().unwrap().push((
                channel.to_owned(),
                timestamp.to_owned(),
                emoji.to_owned(),
            ));
            Ok(())
        }

        async fn remove_reaction(
            &self,
            _credential: SlackCredential,
            channel: &str,
            timestamp: &str,
            emoji: &str,
        ) -> Result<()> {
            self.removed_reactions.lock().unwrap().push((
                channel.to_owned(),
                timestamp.to_owned(),
                emoji.to_owned(),
            ));
            Ok(())
        }

        async fn history(
            &self,
            credential: SlackCredential,
            _account: &ProviderId,
            _current_user_id: Option<&str>,
            chat_id: &ChatId,
            before: Option<Timestamp>,
            limit: usize,
            _users: Option<Arc<RwLock<HashMap<String, SlackUser>>>>,
        ) -> Result<Vec<Message>> {
            self.history_calls.lock().unwrap().push((
                credential.kind,
                chat_id.clone(),
                before,
                limit,
            ));
            // Simulate a conversation the caller can never read so tests can
            // assert the poller stops retrying it.
            if chat_id.as_ref().contains("NOTFOUND") {
                bail!("Slack conversations.history failed: channel_not_found");
            }
            let mut messages = self
                .history_messages
                .lock()
                .unwrap()
                .iter()
                .filter(|message| message.chat_id == *chat_id)
                .filter(|message| before.is_none_or(|before| message.timestamp < before))
                .cloned()
                .collect::<Vec<_>>();
            messages.sort_by_key(|message| message.timestamp);
            let start = messages.len().saturating_sub(limit);
            Ok(messages.split_off(start))
        }

        async fn open_socket_mode(
            &self,
            app_token: SlackCredential,
        ) -> Result<SlackSocketModeConnection> {
            self.opened_socket_modes
                .lock()
                .unwrap()
                .push(app_token.kind.clone());
            if !app_token.value().starts_with("xapp-") {
                bail!("invalid app token")
            }
            Ok(SlackSocketModeConnection {
                url: "ws://127.0.0.1:9/socket-mode-test".to_owned(),
            })
        }

        async fn exchange_oauth_code(
            &self,
            client_id: &str,
            client_secret: &str,
            redirect_uri: &str,
            code: &str,
        ) -> Result<SlackOAuthTokens> {
            self.oauth_exchanges.lock().unwrap().push((
                client_id.to_owned(),
                client_secret.to_owned(),
                redirect_uri.to_owned(),
                code.to_owned(),
            ));
            if let Some(error) = self.fail_oauth_exchange.lock().unwrap().clone() {
                bail!("Slack oauth.v2.access failed: {error}");
            }
            Ok(self
                .oauth_tokens
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| SlackOAuthTokens {
                    user_token: Some("xoxp-oauth-user".to_owned()),
                    user_scopes: Some("channels:history,im:history".to_owned()),
                    team_id: Some("T123".to_owned()),
                    team_name: Some("Example Workspace".to_owned()),
                    user_id: Some("U123".to_owned()),
                    ..SlackOAuthTokens::default()
                }))
        }
    }

    fn provider_with_fake_client(
        options: SlackProviderOptions,
        client: Arc<FakeSlackApiClient>,
    ) -> Result<SlackProvider> {
        SlackProvider::with_api_client(options, client)
    }

    #[test]
    fn member_scopes_request_only_what_each_caller_needs() {
        // The DM poll must never page through channels, and the sidebar scope
        // hides public channels the user has not joined.
        assert_eq!(SlackMemberScope::Direct.types(), "im,mpim");
        assert_eq!(SlackMemberScope::Sidebar.types(), SLACK_CONVERSATION_TYPES);
        let joined = channel_conversation("C1", "general", 1);
        let not_joined = SlackConversation {
            is_member: Some(false),
            ..channel_conversation("C2", "random", 1)
        };
        let dm = dm_conversation("D1", "U1", 1);
        assert!(SlackMemberScope::Sidebar.includes(&joined));
        assert!(!SlackMemberScope::Sidebar.includes(&not_joined));
        assert!(SlackMemberScope::Sidebar.includes(&dm));
        assert!(SlackMemberScope::Direct.includes(&dm));
        assert!(!SlackMemberScope::Direct.includes(&joined));
        assert_eq!(
            SlackListingEndpoint::Member(SlackMemberScope::Direct).method(),
            "users.conversations"
        );
        assert_eq!(
            SlackListingEndpoint::AllConversations.method(),
            "conversations.list"
        );
        // 1:1 DMs never go through `users.conversations`, which drops DMs
        // with bots and deactivated users.
        assert!(!SlackMemberScope::Sidebar.member_types().contains("im,"));
        assert!(!SlackMemberScope::Sidebar.member_types().ends_with(",im"));
        assert_eq!(SlackMemberScope::Direct.member_types(), "mpim");
        assert_eq!(SlackListingEndpoint::DirectMessages.types(), "im");
        assert_eq!(
            SlackListingEndpoint::DirectMessages.method(),
            "conversations.list"
        );
    }

    #[test]
    fn member_listing_merge_keeps_dms_missing_from_users_conversations() {
        let channel = channel_conversation("C1", "general", 1);
        let bot_dm = dm_conversation("D1", "UBOT", 1);
        let shared_dm = dm_conversation("D2", "U2", 1);
        let merged = merge_member_listings(
            SlackMemberScope::Sidebar,
            vec![channel, shared_dm.clone()],
            vec![bot_dm, shared_dm],
        );
        let ids = merged
            .iter()
            .map(|conversation| conversation.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["C1", "D2", "D1"]);

        let direct = merge_member_listings(
            SlackMemberScope::Direct,
            vec![channel_conversation("C1", "general", 1)],
            vec![dm_conversation("D1", "U1", 1)],
        );
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].id, "D1");
    }

    fn channel_conversation(id: &str, name: &str, updated: i64) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            user: None,
            is_channel: true,
            is_group: false,
            is_im: false,
            is_mpim: false,
            is_member: Some(true),
            is_private: false,
            is_archived: false,
            is_ext_shared: false,
            is_muted: false,
            is_pinned: false,
            unread_count: 0,
            updated: Some(updated),
            topic: None,
            purpose: None,
            num_members: None,
            created: None,
            creator: None,
        }
    }

    fn dm_conversation(id: &str, user: &str, updated: i64) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: None,
            user: Some(user.to_owned()),
            is_channel: false,
            is_group: false,
            is_im: true,
            is_mpim: false,
            is_member: Some(true),
            is_private: true,
            is_archived: false,
            is_ext_shared: false,
            is_muted: false,
            is_pinned: false,
            unread_count: 0,
            updated: Some(updated),
            topic: None,
            purpose: None,
            num_members: None,
            created: None,
            creator: None,
        }
    }

    fn mpim_conversation(id: &str, name: Option<&str>, updated: i64) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: name.map(ToOwned::to_owned),
            user: None,
            is_channel: false,
            is_group: false,
            is_im: false,
            is_mpim: true,
            is_member: Some(true),
            is_private: true,
            is_archived: false,
            is_ext_shared: false,
            is_muted: false,
            is_pinned: false,
            unread_count: 0,
            updated: Some(updated),
            topic: None,
            purpose: None,
            num_members: None,
            created: None,
            creator: None,
        }
    }

    #[test]
    fn slack_mpim_display_name_removes_provider_prefixes_and_suffix() {
        assert_eq!(
            slack_mpim_display_name("mpdm-mpdm-bogdan--mihai--ana-1").as_deref(),
            Some("bogdan, mihai, ana")
        );
        assert_eq!(
            slack_mpim_display_name("#mpdm-bogdan--mihai").as_deref(),
            Some("bogdan, mihai")
        );
        assert_eq!(
            slack_mpim_display_name("bogdan--mihai-adamut").as_deref(),
            Some("bogdan, mihai-adamut")
        );
    }

    #[test]
    fn slack_mpim_chat_uses_readable_group_dm_name() {
        let conversation = mpim_conversation("G123", Some("mpdm-bogdan--mihai--ana-1"), 123);
        let chat = chat_from_slack_conversation(&arc_str("slack:test"), &conversation)
            .expect("mpim conversation should produce chat");

        assert_eq!(chat.kind, ChatKind::GroupDirectMessage);
        assert!(chat.is_group);
        assert_eq!(chat.name.as_ref(), "bogdan, mihai, ana");
    }

    #[test]
    fn slack_mpim_chat_falls_back_to_group_dm_id_without_name() {
        let conversation = mpim_conversation("G123", None, 123);
        let chat = chat_from_slack_conversation(&arc_str("slack:test"), &conversation)
            .expect("mpim conversation should produce chat");

        assert_eq!(chat.kind, ChatKind::GroupDirectMessage);
        assert_eq!(chat.name.as_ref(), "Group DM G123");
    }

    #[test]
    fn setup_options_are_ordered_by_robustness() {
        let modes = SlackProvider::setup_options_in_robustness_order()
            .into_iter()
            .map(|option| option.mode)
            .collect::<Vec<_>>();

        assert_eq!(
            modes,
            vec![
                SlackAuthMode::UserOAuth,
                SlackAuthMode::ReadOnlyOAuth,
                SlackAuthMode::BotToken,
                SlackAuthMode::ImportedToken,
                SlackAuthMode::ManualApp,
                SlackAuthMode::Webhook,
            ]
        );
    }

    async fn next_non_network_event(
        events: &mut tokio::sync::broadcast::Receiver<ProviderEvent>,
    ) -> Result<ProviderEvent> {
        loop {
            let event = events.recv().await?;
            if !matches!(event, ProviderEvent::NetworkActivity { .. }) {
                return Ok(event);
            }
        }
    }

    #[tokio::test]
    async fn connect_validates_user_token_and_ignores_bot_fallback_in_user_mode() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        options.bot_token = Some("xoxb-bot".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        let mut events = provider.events();

        provider.connect().await?;

        assert!(provider.is_connected());
        assert_eq!(provider.send_identity(), SlackSendIdentity::User);
        assert!(provider.capabilities().can_send_as_user);
        assert!(provider.capabilities().can_read_history);
        assert!(!provider.capabilities().can_send_as_bot);
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![SlackCredentialKind::UserToken]
        );
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::AuthSucceeded
        ));
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::SyncComplete
        ));
        Ok(())
    }

    #[tokio::test]
    async fn connect_uses_bot_team_icon_when_user_team_info_lacks_scope() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::ManualApp);
        options.user_token = Some("xoxp-user".to_owned());
        options.bot_token = Some("xoxb-bot".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        client
            .fail_team_info_for
            .lock()
            .unwrap()
            .push(SlackCredentialKind::UserToken);
        let provider = provider_with_fake_client(options, client.clone())?;

        provider.connect().await?;

        let account = provider.account_info();
        assert_eq!(
            *client.team_info_calls.lock().unwrap(),
            vec![
                (SlackCredentialKind::UserToken, Some("T123".to_owned())),
                (SlackCredentialKind::BotToken, Some("T123".to_owned()))
            ]
        );
        assert_eq!(account.display_name.as_ref(), "Slack (Example Workspace)");
        assert!(account.avatar.is_some());
        Ok(())
    }

    #[test]
    fn slack_user_oauth_scopes_include_team_read_for_workspace_icon() {
        assert!(SlackAuthMode::UserOAuth.user_scopes().contains("team:read"));
        assert!(
            SlackAuthMode::ReadOnlyOAuth
                .user_scopes()
                .contains("team:read")
        );
        assert!(SlackAuthMode::ManualApp.user_scopes().contains("team:read"));
    }

    #[test]
    fn slack_bot_scopes_include_team_read_for_workspace_icon() {
        assert!(SlackAuthMode::BotToken.bot_scopes().contains("team:read"));
        assert!(SlackAuthMode::ManualApp.bot_scopes().contains("team:read"));
    }

    #[test]
    fn fatal_realtime_auth_errors_are_classified_for_alert_and_fallback() {
        // The exact failure observed from apps.connections.open in the field.
        assert!(is_fatal_realtime_auth_error(
            "Slack apps.connections.open failed: invalid_auth"
        ));
        assert!(is_fatal_realtime_auth_error("not_authed"));
        assert!(is_fatal_realtime_auth_error("token_revoked"));
        assert!(is_fatal_realtime_auth_error("ACCOUNT_INACTIVE"));
        assert!(is_fatal_realtime_auth_error("missing_scope"));

        // Recoverable/transient conditions must keep retrying instead of
        // permanently degrading to history polling.
        assert!(!is_fatal_realtime_auth_error(
            "connecting Slack Socket Mode WebSocket: connection reset"
        ));
        assert!(!is_fatal_realtime_auth_error(
            "reading Slack Socket Mode frame"
        ));
        assert!(!is_fatal_realtime_auth_error("timed out"));
    }

    #[test]
    fn history_poll_only_notifies_for_messages_newer_than_loop_start() {
        let started_at = Utc::now();
        let older = started_at - chrono::Duration::hours(6);
        let newer = started_at + chrono::Duration::seconds(30);

        // Backlog message (older than startup) must never notify, even the
        // first time it is observed — this is the regression that produced
        // notifications for long-past conversations when the baseline history
        // fetch failed and the message only surfaced on a later poll.
        assert!(!history_poll_message_is_live(started_at, older, true));

        // A genuinely new message (after startup), seen for the first time,
        // is the only case that should notify.
        assert!(history_poll_message_is_live(started_at, newer, true));

        // Re-observing the same new message on a subsequent poll must not
        // notify again.
        assert!(!history_poll_message_is_live(started_at, newer, false));

        // A message exactly at the startup instant is treated as backlog, not
        // new, so it is not re-announced on startup.
        assert!(!history_poll_message_is_live(started_at, started_at, true));
    }

    #[test]
    fn idle_fallback_alerts_only_with_evidence_of_missed_realtime_messages() {
        // A quiet workspace (no live messages found by polling) must never
        // alert: silence is indistinguishable from a healthy-but-idle setup,
        // and alarming the user about it was pure confusion.
        assert!(!should_alert_undelivered_realtime_messages(
            false,
            Some(false),
            false
        ));

        // Hard evidence — polling delivered a live message while the open
        // realtime socket has still seen no events — is the only alert case.
        assert!(should_alert_undelivered_realtime_messages(
            true,
            Some(false),
            false
        ));

        // Once realtime has proven it delivers events, polling finding a
        // message is normal overlap, not a failure.
        assert!(!should_alert_undelivered_realtime_messages(
            true,
            Some(true),
            false
        ));

        // The alert fires at most once per fallback loop.
        assert!(!should_alert_undelivered_realtime_messages(
            true,
            Some(false),
            true
        ));

        // Plain history polling without a realtime socket never alerts.
        assert!(!should_alert_undelivered_realtime_messages(
            true, None, false
        ));
    }

    fn poll_history_message(chat_id: &str, id: &str, timestamp: Timestamp) -> Message {
        Message {
            id: arc_str(id),
            chat_id: arc_str(chat_id),
            account: arc_str("slack:test"),
            sender: Sender {
                platform_id: arc_str("U999"),
                display_name: arc_str("Teammate"),
                avatar: None,
            },
            timestamp,
            edited_at: None,
            content: Content::Text(arc_str("hello")),
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    /// Simulates new activity in a conversation by advancing the `updated`
    /// marker the activity-gated poller compares against, so a test can force
    /// the conversation to be re-polled on the next pass.
    fn bump_conversation_updated(client: &FakeSlackApiClient, id: &str, updated: i64) {
        let mut conversations = client.conversations.lock().unwrap();
        for conversation in conversations.iter_mut() {
            if conversation.id == id {
                conversation.updated = Some(updated);
            }
        }
    }

    #[tokio::test]
    async fn history_poll_pass_reports_live_deliveries_and_dedupes_repeats() {
        let client = Arc::new(FakeSlackApiClient::default());
        client
            .conversations
            .lock()
            .unwrap()
            .push(channel_conversation("C123", "general", 1_710_000_000));
        let started_at = Utc::now();
        let backlog = started_at - chrono::Duration::hours(2);
        let live = started_at + chrono::Duration::seconds(5);
        client
            .history_messages
            .lock()
            .unwrap()
            .push(poll_history_message("C123", "1710000001.000100", backlog));

        let api_client: Arc<dyn SlackApiClient> = client.clone();
        let events = EventBus::new();
        let account = arc_str("slack:test");
        let credential =
            SlackCredential::new(SlackCredentialKind::UserToken, "xoxp-test".to_owned());
        let users = Arc::new(RwLock::new(HashMap::new()));
        let mut seen_message_ids = HashSet::new();
        let mut inaccessible = HashSet::new();
        let mut activity = HashMap::new();

        // Backlog-only pass: the conversation is observed for the first time, so
        // it is polled, but nothing is delivered live and the idle fallback has
        // no evidence of missed realtime messages and must not alert.
        assert!(
            !run_history_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut inaccessible,
                &mut activity,
                None,
            )
            .await
        );

        // A genuinely new message, surfaced after the conversation's activity
        // markers advance, is reported as a live delivery exactly once.
        client
            .history_messages
            .lock()
            .unwrap()
            .push(poll_history_message("C123", "1710000010.000200", live));
        bump_conversation_updated(&client, "C123", 1_710_000_100);
        assert!(
            run_history_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut inaccessible,
                &mut activity,
                None,
            )
            .await
        );

        // Re-observing the same message on a later pass — even when activity
        // advances again so the conversation is re-polled — is not new evidence.
        bump_conversation_updated(&client, "C123", 1_710_000_200);
        assert!(
            !run_history_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut inaccessible,
                &mut activity,
                None,
            )
            .await
        );
    }

    #[tokio::test]
    async fn poll_pass_skips_unchanged_conversations() {
        // The activity gate must not re-poll a conversation whose activity
        // markers are unchanged since the previous pass. This is what stops the
        // safety-net poller from issuing a `conversations.history` call (and the
        // UI-waking events it triggers) for every quiet conversation on every
        // pass — the dominant idle-CPU cost in a connected but silent workspace.
        let client = Arc::new(FakeSlackApiClient::default());
        client
            .conversations
            .lock()
            .unwrap()
            .push(channel_conversation("C123", "general", 1_710_000_000));

        let started_at = Utc::now();
        let api_client: Arc<dyn SlackApiClient> = client.clone();
        let events = EventBus::new();
        let account = arc_str("slack:test");
        let credential =
            SlackCredential::new(SlackCredentialKind::UserToken, "xoxp-test".to_owned());
        let users = Arc::new(RwLock::new(HashMap::new()));
        let mut seen_message_ids = HashSet::new();
        let mut inaccessible = HashSet::new();
        let mut activity = HashMap::new();

        for _ in 0..3 {
            run_history_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut inaccessible,
                &mut activity,
                None,
            )
            .await;
        }

        let calls = client
            .history_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, chat_id, _, _)| chat_id.as_ref() == "C123")
            .count();
        assert_eq!(
            calls, 1,
            "an unchanged conversation must be polled once to establish a baseline, then skipped"
        );

        // Once its activity markers advance (a new message bumps `updated`), it
        // is polled again so the message can be delivered.
        bump_conversation_updated(&client, "C123", 1_710_000_500);
        run_history_poll_pass(
            &api_client,
            &events,
            &account,
            &credential,
            Some("U123"),
            &users,
            started_at,
            &mut seen_message_ids,
            &mut inaccessible,
            &mut activity,
            None,
        )
        .await;

        let calls_after_activity = client
            .history_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, chat_id, _, _)| chat_id.as_ref() == "C123")
            .count();
        assert_eq!(
            calls_after_activity, 2,
            "a conversation with new activity must be re-polled"
        );
    }

    #[tokio::test]
    async fn poll_pass_stops_polling_inaccessible_conversations() {
        // A conversation that always returns channel_not_found must be dropped
        // from future passes instead of being retried forever, which is what
        // otherwise keeps the fallback (and the UI) busy.
        let client = Arc::new(FakeSlackApiClient::default());
        {
            let mut conversations = client.conversations.lock().unwrap();
            conversations.push(channel_conversation("C-NOTFOUND", "ghost", 1_710_000_000));
            conversations.push(channel_conversation("C123", "general", 1_710_000_000));
        }

        let started_at = Utc::now();
        let api_client: Arc<dyn SlackApiClient> = client.clone();
        let events = EventBus::new();
        let account = arc_str("slack:test");
        let credential =
            SlackCredential::new(SlackCredentialKind::UserToken, "xoxp-test".to_owned());
        let users = Arc::new(RwLock::new(HashMap::new()));
        let mut seen_message_ids = HashSet::new();
        let mut inaccessible = HashSet::new();
        let mut activity = HashMap::new();

        for pass in 0..3 {
            // Advance activity each pass so the accessible conversation keeps
            // qualifying for a poll; the inaccessible one is dropped via the
            // `inaccessible` set regardless of its activity markers.
            let updated = 1_710_000_001 + pass as i64;
            bump_conversation_updated(&client, "C123", updated);
            bump_conversation_updated(&client, "C-NOTFOUND", updated);
            run_history_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut inaccessible,
                &mut activity,
                None,
            )
            .await;
        }

        assert!(
            inaccessible.contains("C-NOTFOUND"),
            "inaccessible conversation should be remembered"
        );

        let ghost_calls = client
            .history_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, chat_id, _, _)| chat_id.as_ref() == "C-NOTFOUND")
            .count();
        assert_eq!(
            ghost_calls, 1,
            "inaccessible conversation must be polled at most once, not every pass"
        );

        let good_calls = client
            .history_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, chat_id, _, _)| chat_id.as_ref() == "C123")
            .count();
        assert_eq!(
            good_calls, 3,
            "accessible conversation must keep being polled every pass"
        );
    }

    fn collect_message_chat_ids(receiver: &mut broadcast::Receiver<ProviderEvent>) -> Vec<String> {
        let mut chat_ids = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let ProviderEvent::Message { message, .. } = event {
                chat_ids.push(message.chat_id.to_string());
            }
        }
        chat_ids
    }

    #[tokio::test]
    async fn dm_poll_pass_surfaces_dms_and_mpims_but_excludes_channels() {
        // The DM poll runs concurrently with realtime. Realtime already
        // delivers channel traffic, so the DM poll must only fetch the
        // user's own im/mpim conversations (which Socket Mode never
        // surfaces) and must leave channels alone.
        let client = Arc::new(FakeSlackApiClient::default());
        {
            let mut conversations = client.conversations.lock().unwrap();
            conversations.push(channel_conversation("C123", "general", 1_710_000_000));
            conversations.push(dm_conversation("D456", "U999", 1_710_000_000));
            conversations.push(mpim_conversation("G789", Some("mpdm-team"), 1_710_000_000));
        }

        let started_at = Utc::now();
        let live = started_at + chrono::Duration::seconds(5);
        {
            let mut history = client.history_messages.lock().unwrap();
            // A channel message that realtime would handle; the DM poll must
            // never fetch or deliver it.
            history.push(poll_history_message("C123", "1710000001.000100", live));
            history.push(poll_history_message("D456", "1710000002.000200", live));
            history.push(poll_history_message("G789", "1710000003.000300", live));
        }

        let api_client: Arc<dyn SlackApiClient> = client.clone();
        let events = EventBus::new();
        let mut receiver = events.subscribe();
        let account = arc_str("slack:test");
        let credential =
            SlackCredential::new(SlackCredentialKind::UserToken, "xoxp-test".to_owned());
        let users = Arc::new(RwLock::new(HashMap::new()));
        let mut seen_message_ids = HashSet::new();
        let mut activity = HashMap::new();

        let dm_filter = SlackMemberScope::Direct;

        // First pass: both DM-class messages are delivered live exactly once;
        // the channel message is never delivered by the DM poll.
        assert!(
            run_conversation_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut HashSet::new(),
                &mut activity,
                None,
                dm_filter,
            )
            .await
        );
        let mut delivered = collect_message_chat_ids(&mut receiver);
        delivered.sort();
        assert_eq!(delivered, vec!["D456".to_owned(), "G789".to_owned()]);

        // The DM poll must only ever query im/mpim history, never channels.
        let polled_channels: Vec<String> = client
            .history_calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, chat_id, _, _)| chat_id.to_string())
            .collect();
        assert!(polled_channels.contains(&"D456".to_owned()));
        assert!(polled_channels.contains(&"G789".to_owned()));
        assert!(
            !polled_channels.contains(&"C123".to_owned()),
            "DM poll must not fetch channel history, polled: {polled_channels:?}"
        );

        // Second pass: the conversations' activity markers are unchanged, so
        // the gate skips them entirely — nothing is re-fetched or re-delivered
        // even though realtime may also have delivered them.
        assert!(
            !run_conversation_poll_pass(
                &api_client,
                &events,
                &account,
                &credential,
                Some("U123"),
                &users,
                started_at,
                &mut seen_message_ids,
                &mut HashSet::new(),
                &mut activity,
                None,
                dm_filter,
            )
            .await
        );
        assert!(collect_message_chat_ids(&mut receiver).is_empty());
    }

    #[test]
    fn history_error_response_without_messages_decodes_and_surfaces_error() {
        // Slack returns this shape when the token lacks the *:history scopes.
        // It must decode (not fail as an opaque "decoding ... response" error)
        // so the real `missing_scope` reason can be reported to the user.
        let response: SlackConversationsHistoryResponse =
            serde_json::from_str(r#"{"ok":false,"error":"missing_scope"}"#)
                .expect("error response without `messages` must still decode");

        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("missing_scope"));
        assert!(response.messages.is_empty());
    }

    #[test]
    fn history_response_decodes_integer_attachment_timestamp() {
        // Slack sometimes returns attachment `ts` as an integer. Treat that as a
        // string rather than skipping the whole message; otherwise history poll
        // can miss valid messages entirely.
        let raw = r#"{
            "ok": true,
            "messages": [
                {"type": "message", "user": "U123", "ts": "1700000000.000100", "text": "hello self"},
                {"type": "message", "user": "U123", "ts": "1700000001.000200", "text": "with attachment", "attachments": [{"ts": 1700000001}]}
            ]
        }"#;
        let response: SlackConversationsHistoryResponse =
            serde_json::from_str(raw).expect("response decodes integer attachment ts");

        assert!(response.ok);
        assert_eq!(response.messages.len(), 2);
        assert_eq!(response.messages[0].text.as_deref(), Some("hello self"));
        assert_eq!(
            response.messages[1].text.as_deref(),
            Some("with attachment")
        );
        assert_eq!(
            response.messages[1].attachments.as_ref().unwrap()[0]
                .ts
                .as_deref(),
            Some("1700000001")
        );
    }

    #[test]
    fn conversations_list_error_response_without_channels_decodes_and_surfaces_error() {
        let response: SlackConversationsListResponse =
            serde_json::from_str(r#"{"ok":false,"error":"missing_scope"}"#)
                .expect("error response without `channels` must still decode");

        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("missing_scope"));
        assert!(response.channels.is_empty());
    }

    #[test]
    fn slack_generated_manifest_keeps_realtime_features_for_chat_updates() {
        let url = slack_app_manifest_url(&SlackAuthMode::ReadOnlyOAuth);
        let encoded_manifest = url
            .split("manifest_yaml=")
            .nth(1)
            .expect("manifest query is present");
        let manifest = encoded_manifest
            .replace("%0A", "\n")
            .replace("%20", " ")
            .replace("%3A", ":");

        assert!(manifest.contains("team:read"));
        assert!(manifest.contains("socket_mode_enabled: true"));
        assert!(manifest.contains("  event_subscriptions:\n    user_events:"));
        assert!(manifest.contains("    - message.channels"));
        // Self-DM realtime relies on the im message event, and the history
        // fallback relies on the *:history scopes. A manifest missing either
        // reproduces the realtime/missing_scope failures, so lock them in.
        assert!(manifest.contains("message.im"));
        assert!(manifest.contains("channels:history"));
        assert!(manifest.contains("im:history"));
        assert!(manifest.contains("groups:history"));
        assert!(manifest.contains("mpim:history"));
    }

    #[test]
    fn slack_user_oauth_manifest_requests_realtime_and_history_capabilities() {
        let url = slack_app_manifest_url(&SlackAuthMode::UserOAuth);
        let encoded_manifest = url
            .split("manifest_yaml=")
            .nth(1)
            .expect("manifest query is present");
        let manifest = encoded_manifest
            .replace("%0A", "\n")
            .replace("%20", " ")
            .replace("%3A", ":");

        assert!(manifest.contains("socket_mode_enabled: true"));
        assert!(manifest.contains("  event_subscriptions:\n    user_events:"));
        assert!(manifest.contains("    - message.im"));
        assert!(!manifest.contains("bot_events:"));
        assert!(manifest.contains("im:history"));
        assert!(manifest.contains("team:read"));
    }

    #[test]
    fn slack_user_oauth_manifest_registers_advertised_redirect_url() {
        let url = slack_app_manifest_url(&SlackAuthMode::UserOAuth);
        let manifest = url
            .split("manifest_yaml=")
            .nth(1)
            .expect("manifest query is present")
            .replace("%0A", "\n")
            .replace("%20", " ")
            .replace("%2F", "/")
            .replace("%3A", ":");
        // Slack matches redirect URIs exactly, so the redirect URI advertised
        // during browser OAuth must be pre-registered in the app manifest or
        // the exchange fails with a redirect mismatch.
        assert!(manifest.contains("redirect_urls:"));
        assert!(manifest.contains(&default_advertised_oauth_redirect_uri()));

        // Bot-token apps use a pasted token, not the loopback browser login, so
        // they must not advertise a redirect URL.
        let bot = slack_app_manifest_url(&SlackAuthMode::BotToken);
        let bot_manifest = bot
            .split("manifest_yaml=")
            .nth(1)
            .expect("manifest query is present");
        assert!(!bot_manifest.contains("redirect_urls"));
    }

    #[test]
    fn slack_oauth_callback_query_parses_code_state_and_error() {
        let callback =
            parse_oauth_callback_query("/slack/oauth/callback?code=abc%20123&state=deadbeef");
        assert_eq!(callback.code.as_deref(), Some("abc 123"));
        assert_eq!(callback.state.as_deref(), Some("deadbeef"));
        assert!(callback.error.is_none());

        let denied = parse_oauth_callback_query("/slack/oauth/callback?error=access_denied");
        assert_eq!(denied.error.as_deref(), Some("access_denied"));
        assert!(denied.code.is_none());
    }

    #[test]
    fn slack_oauth_state_is_unguessable_and_validated() {
        let first = slack_oauth_state();
        let second = slack_oauth_state();
        assert_ne!(first, second, "state values must not repeat");
        assert_eq!(first.len(), 32, "128 bits of derived hex state");
        assert!(slack_oauth_state_matches(&first, Some(first.as_str())));
        assert!(!slack_oauth_state_matches(&first, Some("mismatch")));
        assert!(!slack_oauth_state_matches(&first, None));
        // An empty expected state must never match (rejects a missing/cleared
        // state instead of accepting an empty callback value).
        assert!(!slack_oauth_state_matches("", Some("")));
    }

    #[test]
    fn official_slack_app_requires_client_id_and_secret() {
        // Both a client ID and secret are mandatory; a missing secret falls
        // back to manual app creation rather than a half-configured app.
        assert!(resolve_official_slack_app(None, None, None).is_none());
        assert!(
            resolve_official_slack_app(Some("client-id".to_owned()), None, None).is_none(),
            "client id without secret must not configure an official app"
        );
        assert!(
            resolve_official_slack_app(None, Some("secret".to_owned()), None).is_none(),
            "secret without client id must not configure an official app"
        );
    }

    #[test]
    fn official_slack_app_defaults_redirect_to_https_relay_callback() {
        let app = resolve_official_slack_app(
            Some("client-id".to_owned()),
            Some("client-secret".to_owned()),
            None,
        )
        .expect("client id + secret configure an official app");
        assert_eq!(app.client_id, "client-id");
        assert_eq!(app.client_secret, "client-secret");
        assert_eq!(app.redirect_uri, SLACK_OAUTH_REDIRECT_URI);
    }

    #[test]
    fn official_slack_app_keeps_explicit_redirect_uri() {
        let app = resolve_official_slack_app(
            Some("client-id".to_owned()),
            Some("client-secret".to_owned()),
            Some("https://chat.example/callback".to_owned()),
        )
        .expect("client id + secret configure an official app");
        assert_eq!(app.redirect_uri, "https://chat.example/callback");
    }

    #[test]
    fn slack_authorize_url_advertises_https_relay_redirect_for_distribution() {
        // Slack requires distributed apps to use an HTTPS redirect. The relay
        // URL must be advertised exactly (percent-encoded) in the authorize URL
        // so it matches the registered redirect and the later token exchange.
        let relay = "https://relay.example/slack/oauth/callback";
        let url = slack_authorize_url(
            "client-123",
            relay,
            "team:read",
            "channels:history",
            "abc123",
        );
        assert!(url.starts_with("https://slack.com/oauth/v2/authorize?"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains(&format!("redirect_uri={}", url_component(relay))));
        // The insecure loopback URL must not leak into a distribution authorize URL.
        assert!(!url.contains("localhost"));
        assert!(url.contains("state=abc123"));
    }

    #[test]
    fn official_app_keeps_https_relay_redirect_for_distribution() {
        // With an explicit HTTPS relay redirect configured, the resolved official
        // app must keep that exact URL so Slack accepts the distributed app and
        // the authorize/exchange/manifest all advertise the same value.
        let relay = "https://relay.example/slack/oauth/callback".to_owned();
        let resolved = resolve_official_slack_app(
            Some("client-id".to_owned()),
            Some("client-secret".to_owned()),
            Some(relay.clone()),
        )
        .expect("client id + secret configure an official app");
        assert_eq!(resolved.redirect_uri, relay);
    }

    #[tokio::test]
    async fn slack_oauth_relay_flow_returns_authorization_code() -> Result<()> {
        // An empty redirect falls back to the default advertised HTTPS relay,
        // while the callback listener still binds locally for the relay target.
        let flow = begin_slack_oauth_login("client-123", "", "team:read", "channels:history")?;
        let authorize_url = flow.authorize_url().to_owned();
        assert!(authorize_url.contains("client_id=client-123"));
        assert!(authorize_url.contains("redirect_uri="));
        assert_eq!(flow.redirect_uri(), SLACK_OAUTH_REDIRECT_URI);
        let state = authorize_url
            .split("state=")
            .nth(1)
            .expect("authorize URL carries CSRF state")
            .to_owned();

        let waiter = tokio::spawn(async move { flow.wait_for_authorization_code().await });

        // The listener is already bound (begin_slack_oauth_login bound it), so
        // simulate Slack's browser redirect to the loopback callback.
        tokio::task::spawn_blocking(move || {
            let mut stream = loop {
                if let Ok(stream) =
                    std::net::TcpStream::connect(("127.0.0.1", SLACK_OAUTH_REDIRECT_PORT))
                {
                    break stream;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let request = format!(
                "GET /slack/oauth/callback?code=auth-code-xyz&state={state} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).unwrap();
            let _ = stream.flush();
        })
        .await
        .unwrap();

        let code = waiter.await.unwrap()?;
        assert_eq!(code, "auth-code-xyz");
        Ok(())
    }

    #[tokio::test]
    async fn browser_oauth_login_skips_manual_and_unsupported_submissions() -> Result<()> {
        let provider = provider_with_fake_client(
            SlackProviderOptions::new(SlackAuthMode::UserOAuth),
            Arc::new(FakeSlackApiClient::default()),
        )?;

        // A pasted user token means the user opted into the manual path; the
        // browser login must not run (and must not bind a socket).
        let manual = AuthSubmission {
            mode: Some(AuthSubmissionMode::UserOAuth),
            client_id: Some("client-123".to_owned()),
            client_secret: Some("secret".to_owned()),
            user_token: Some("xoxp-existing".to_owned()),
            ..AuthSubmission::default()
        };
        assert!(
            provider
                .maybe_run_browser_oauth_login(manual)
                .await?
                .oauth_code
                .is_none()
        );

        // Bot-token mode does not support browser-based code exchange.
        let bot = AuthSubmission {
            mode: Some(AuthSubmissionMode::BotToken),
            client_id: Some("client-123".to_owned()),
            client_secret: Some("secret".to_owned()),
            ..AuthSubmission::default()
        };
        assert!(
            provider
                .maybe_run_browser_oauth_login(bot)
                .await?
                .oauth_code
                .is_none()
        );

        // Without a client secret we cannot exchange a code, so the submission
        // is left untouched for validation to report the missing secret.
        let no_secret = AuthSubmission {
            mode: Some(AuthSubmissionMode::UserOAuth),
            client_id: Some("client-123".to_owned()),
            ..AuthSubmission::default()
        };
        assert!(
            provider
                .maybe_run_browser_oauth_login(no_secret)
                .await?
                .oauth_code
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn slack_history_poll_limit_catches_more_than_one_message_per_cycle() {
        // A limit of 1 only fetches the newest message per conversation each
        // cycle, dropping every earlier message in a burst between polls.
        let limit = SLACK_HISTORY_POLL_LIMIT;
        assert!(
            limit > 1,
            "history fallback must fetch a window, not just the newest message"
        );
    }

    #[test]
    fn slack_team_icon_prefers_original_image_url() {
        let icon = SlackTeamIconResponse {
            image_34: Some("https://example.com/icon-34.png".to_owned()),
            image_44: None,
            image_68: None,
            image_88: None,
            image_102: None,
            image_132: None,
            image_230: None,
            image_original: Some("https://example.com/icon-original.png".to_owned()),
        };

        assert_eq!(
            icon.best_image_url().as_deref(),
            Some("https://example.com/icon-original.png")
        );
    }

    #[test]
    fn slack_team_icon_uses_available_image_even_without_custom_workspace_icon() {
        let icon = SlackTeamIconResponse {
            image_34: Some("https://example.com/default-34.png".to_owned()),
            image_44: None,
            image_68: None,
            image_88: None,
            image_102: None,
            image_132: None,
            image_230: None,
            image_original: None,
        };

        assert_eq!(
            icon.best_image_url().as_deref(),
            Some("https://example.com/default-34.png")
        );
    }
    #[tokio::test]
    async fn connect_user_oauth_stays_connected_when_optional_app_token_is_invalid() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        options.app_token = Some("xapp-invalid-realtime".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;

        provider.connect().await?;

        assert!(provider.is_connected());
        assert!(provider.capabilities().can_read_history);
        assert!(provider.capabilities().can_send_as_user);
        assert!(!provider.capabilities().can_realtime);
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![
                SlackCredentialKind::UserToken,
                SlackCredentialKind::AppToken
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn connect_uses_history_polling_without_alert_when_history_is_available() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        let mut events = provider.events();

        provider.connect().await?;
        for _ in 0..20 {
            if !client.listed_conversations.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        provider.disconnect().await?;

        assert!(
            !client.listed_conversations.lock().unwrap().is_empty(),
            "OAuth history polling is the primary inbound path when history scopes are available"
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, ProviderEvent::AccountNotice { .. }),
                "history-capable OAuth should not warn about realtime before using polling"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn connect_prefers_realtime_for_channels_and_polls_dms_concurrently() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        options.app_token = Some("xapp-realtime".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        let mut events = provider.events();

        provider.connect().await?;
        // Realtime (Socket Mode) is the primary path for channels, and the
        // user-token DM poll runs concurrently because Socket Mode cannot
        // surface the user's own direct messages. Wait for both to start.
        for _ in 0..50 {
            if !client.opened_socket_modes.lock().unwrap().is_empty()
                && !client.listed_conversations.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        provider.disconnect().await?;

        assert!(provider.capabilities().can_read_history);
        assert!(provider.capabilities().can_realtime);
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![
                SlackCredentialKind::UserToken,
                SlackCredentialKind::AppToken
            ]
        );
        assert_eq!(
            *client.opened_socket_modes.lock().unwrap(),
            vec![SlackCredentialKind::AppToken],
            "Socket Mode is the primary interactive chat path when an app token is configured"
        );
        // The DM poll lists conversations with the user token (the only token
        // that can read the user's personal DMs), concurrently with realtime.
        assert!(
            client
                .listed_conversations
                .lock()
                .unwrap()
                .contains(&SlackCredentialKind::UserToken),
            "the DM poll must list conversations with the user token while realtime handles channels"
        );
        // It must not degrade to the "realtime unavailable" history fallback,
        // which would emit a system notice.
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, ProviderEvent::AccountNotice { .. }),
                "realtime-capable connect must not warn about realtime being unavailable"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn connect_read_only_oauth_never_sends_as_user() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::ReadOnlyOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;

        provider.connect().await?;

        assert_eq!(provider.send_identity(), SlackSendIdentity::None);
        assert!(provider.capabilities().can_read_history);
        assert!(!provider.capabilities().can_send_as_user);
        assert!(!provider.capabilities().can_react);
        Ok(())
    }

    #[tokio::test]
    async fn connect_bot_mode_sends_as_bot_only() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::BotToken);
        options.bot_token = Some("xoxb-bot".to_owned());
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;

        provider.connect().await?;

        assert_eq!(provider.send_identity(), SlackSendIdentity::Bot);
        assert!(provider.capabilities().can_send_as_bot);
        assert!(!provider.capabilities().can_send_as_user);
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![SlackCredentialKind::BotToken]
        );
        Ok(())
    }

    #[tokio::test]
    async fn connect_webhook_mode_is_send_only_app_identity() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;

        provider.connect().await?;

        assert_eq!(provider.send_identity(), SlackSendIdentity::Webhook);
        assert!(provider.capabilities().can_send_webhook);
        assert!(!provider.capabilities().can_read_history);
        assert_eq!(client.validated_webhooks.lock().unwrap().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn connect_without_credentials_emits_auth_required() -> Result<()> {
        let provider =
            SlackProvider::with_options(SlackProviderOptions::new(SlackAuthMode::UserOAuth))?;
        let mut events = provider.events();

        provider.connect().await?;

        assert!(!provider.is_connected());
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::AuthRequired(AuthChallenge::OAuthUrl(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn connect_invalid_token_returns_redacted_error_and_disconnect_event() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-invalid-secret".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        let mut events = provider.events();

        let error = provider.connect().await.unwrap_err().to_string();

        assert!(error.contains("<redacted>"));
        assert!(!error.contains("xoxp-invalid-secret"));
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::Disconnected(Some(reason))
                if reason.contains("<redacted>") && !reason.contains("xoxp-invalid-secret")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn submit_auth_validates_user_token_and_updates_capabilities() -> Result<()> {
        let options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        let mut events = provider.events();

        provider
            .submit_auth(AuthSubmission {
                workspace_label: Some("Engineering".to_owned()),
                mode: Some(AuthSubmissionMode::UserOAuth),
                user_token: Some("xoxp-submitted-user".to_owned()),
                ..AuthSubmission::default()
            })
            .await?;

        assert!(provider.is_connected());
        assert_eq!(provider.options().workspace.as_deref(), Some("Engineering"));
        assert_eq!(
            provider.options().user_token.as_deref(),
            Some("xoxp-submitted-user")
        );
        assert_eq!(provider.send_identity(), SlackSendIdentity::User);
        assert!(provider.capabilities().can_read_history);
        assert!(provider.capabilities().can_send_as_user);
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![SlackCredentialKind::UserToken]
        );
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::AuthSucceeded
        ));
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::SyncComplete
        ));
        Ok(())
    }

    #[tokio::test]
    async fn submit_auth_webhook_mode_validates_webhook_only() -> Result<()> {
        let options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;

        provider
            .submit_auth(AuthSubmission {
                workspace_label: Some("Alerts".to_owned()),
                mode: Some(AuthSubmissionMode::Webhook),
                webhook_url: Some("https://hooks.slack.com/services/T000/B000/secret".to_owned()),
                ..AuthSubmission::default()
            })
            .await?;

        assert_eq!(provider.options().auth_mode, SlackAuthMode::Webhook);
        assert_eq!(provider.send_identity(), SlackSendIdentity::Webhook);
        assert!(provider.capabilities().can_send_webhook);
        assert!(!provider.capabilities().can_read_history);
        assert!(client.validated_tokens.lock().unwrap().is_empty());
        assert_eq!(client.validated_webhooks.lock().unwrap().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn submit_auth_invalid_token_returns_redacted_error() -> Result<()> {
        let provider = provider_with_fake_client(
            SlackProviderOptions::new(SlackAuthMode::UserOAuth),
            Arc::new(FakeSlackApiClient::default()),
        )?;
        let mut events = provider.events();

        let error = provider
            .submit_auth(AuthSubmission {
                mode: Some(AuthSubmissionMode::UserOAuth),
                user_token: Some("xoxp-invalid-submitted-secret".to_owned()),
                ..AuthSubmission::default()
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("<redacted>"));
        assert!(!error.contains("xoxp-invalid-submitted-secret"));
        assert!(!provider.is_connected());
        assert!(matches!(
            next_non_network_event(&mut events).await?,
            ProviderEvent::Disconnected(Some(reason))
                if reason.contains("<redacted>") && !reason.contains("xoxp-invalid-submitted-secret")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn submit_auth_oauth_code_is_exchanged_into_tokens_without_storing_the_code() -> Result<()>
    {
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(
            SlackProviderOptions::new(SlackAuthMode::UserOAuth),
            client.clone(),
        )?;

        provider
            .submit_auth(AuthSubmission {
                mode: Some(AuthSubmissionMode::UserOAuth),
                client_id: Some("client-123".to_owned()),
                client_secret: Some("client-secret".to_owned()),
                oauth_code: Some("temporary-code".to_owned()),
                ..AuthSubmission::default()
            })
            .await?;

        // The authorization code was exchanged via oauth.v2.access.
        let exchanges = client.oauth_exchanges.lock().unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].0, "client-123");
        assert_eq!(exchanges[0].3, "temporary-code");
        drop(exchanges);

        // The resulting user token is what gets stored and validated; the raw,
        // single-use code is never persisted in provider options.
        assert!(provider.is_connected());
        assert_eq!(
            provider.options().user_token.as_deref(),
            Some("xoxp-oauth-user")
        );
        assert_eq!(
            provider.options().workspace.as_deref(),
            Some("Example Workspace")
        );
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![SlackCredentialKind::UserToken]
        );
        Ok(())
    }

    #[test]
    fn provider_options_debug_redacts_sensitive_values() {
        let mut options = SlackProviderOptions::new(SlackAuthMode::ImportedToken);
        options.client_id = Some("123.456".to_owned());
        options.client_secret = Some("supersecret".to_owned());
        options.user_token = Some("xoxp-user".to_owned());
        options.bot_token = Some("xoxb-bot".to_owned());
        options.app_token = Some("xapp-token".to_owned());
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());

        let debug = format!("{options:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("xoxp-user"));
        assert!(!debug.contains("xoxb-bot"));
        assert!(!debug.contains("xapp-token"));
        assert!(!debug.contains("hooks.slack.com"));
        assert!(!debug.contains("supersecret"));
    }

    #[tokio::test]
    async fn user_mode_sends_text_with_user_token_and_thread_ts() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let message_id = provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::Text(arc_str("hello from user"))),
                Some(&poll_history_message(
                    "C123",
                    "1710000000.000001",
                    Utc::now(),
                )),
            )
            .await?;

        assert_eq!(message_id.as_ref(), "1710000000.000100");
        assert_eq!(
            *client.posted_messages.lock().unwrap(),
            vec![PostedCall {
                kind: SlackCredentialKind::UserToken,
                token: "xoxp-user".to_owned(),
                channel: "C123".to_owned(),
                text: "hello from user".to_owned(),
                thread_ts: Some("1710000000.000001".to_owned()),
            }]
        );
        Ok(())
    }

    fn mention_member(id: &str, name: &str) -> ChatMember {
        ChatMember::new(Sender {
            platform_id: arc_str(id),
            display_name: arc_str(name),
            avatar: None,
        })
    }

    #[tokio::test]
    async fn user_mode_encodes_mentions_as_slack_user_tokens() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        let members = vec![
            mention_member("U123", "Bogdan"),
            mention_member("U456", "Bogdan Adamut"),
        ];

        // Longest name wins, so `@Bogdan Adamut` is not split into `@Bogdan`.
        let encoded =
            provider.encode_outbound_mentions("hi @Bogdan Adamut and @Bogdan", &members, &[]);
        assert_eq!(encoded.text, "hi <@U456> and <@U123>");
        assert_eq!(
            encoded
                .mentioned
                .iter()
                .map(|mention| mention.platform_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["U456", "U123"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_encodes_broadcast_mentions() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        let encoded = provider.encode_outbound_mentions("ping @here and @channel", &[], &[]);
        assert_eq!(encoded.text, "ping <!here> and <!channel>");
        assert!(encoded.mentioned.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_leaves_unresolved_mentions_as_plain_text() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        let members = vec![mention_member("U123", "Bogdan")];
        let encoded =
            provider.encode_outbound_mentions("hi @Nobody and mail me@host", &members, &[]);
        assert_eq!(encoded.text, "hi @Nobody and mail me@host");
        assert!(encoded.mentioned.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn webhook_identity_does_not_support_mentions() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T/B/X".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        assert!(!provider.outbound_capabilities().mentions);
        let members = vec![mention_member("U123", "Bogdan")];
        let encoded = provider.encode_outbound_mentions("hi @Bogdan", &members, &[]);
        assert_eq!(encoded.text, "hi @Bogdan");
        assert!(encoded.mentioned.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bot_mode_sends_text_with_bot_token_only() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::BotToken);
        options.user_token = Some("xoxp-user".to_owned());
        options.bot_token = Some("xoxb-bot".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::Text(arc_str("hello from bot"))),
                None,
            )
            .await?;

        let posted = client.posted_messages.lock().unwrap().clone();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].kind, SlackCredentialKind::BotToken);
        assert_eq!(posted[0].token, "xoxb-bot");
        assert_eq!(
            *client.validated_tokens.lock().unwrap(),
            vec![SlackCredentialKind::BotToken]
        );
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_edits_message_via_chat_update() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;
        assert!(provider.outbound_capabilities().edit);
        assert!(provider.outbound_capabilities().edit_window.is_none());

        let members = vec![mention_member("U123", "Bogdan")];
        let encoded = provider.encode_outbound_mentions("fixed @Bogdan", &members, &[]);
        // A locally sent message has no Slack platform data yet: the id is the
        // `ts` and the chat id is the channel.
        let mut message = poll_history_message("C123", "1710000000.000100", Utc::now());
        message.is_from_me = true;
        provider
            .edit_message(
                &arc_str("C123"),
                &message,
                OutboundContent::with_mentions(
                    Content::Text(arc_str(encoded.text.clone())),
                    encoded.mentioned,
                ),
            )
            .await?;

        assert_eq!(
            *client.updated_messages.lock().unwrap(),
            vec![PostedCall {
                kind: SlackCredentialKind::UserToken,
                token: "xoxp-user".to_owned(),
                channel: "C123".to_owned(),
                text: "fixed <@U123>".to_owned(),
                thread_ts: Some("1710000000.000100".to_owned()),
            }]
        );
        assert!(client.posted_messages.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn composer_formatting_is_sent_and_edited_as_slack_mrkdwn() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let members = vec![mention_member("U123", "Bogdan")];
        let encoded = provider.encode_outbound_mentions(
            "**hi** @Bogdan ~~old~~ _it_ `**raw**`",
            &members,
            &[],
        );
        provider
            .send(
                &arc_str("C123"),
                OutboundContent::with_mentions(
                    Content::Text(arc_str(encoded.text.clone())),
                    encoded.mentioned.clone(),
                ),
                None,
            )
            .await?;
        let posted = client.posted_messages.lock().unwrap().clone();
        assert_eq!(posted[0].text, "*hi* <@U123> ~old~ _it_ `**raw**`");

        let mut message = poll_history_message("C123", "1710000000.000100", Utc::now());
        message.is_from_me = true;
        provider
            .edit_message(
                &arc_str("C123"),
                &message,
                OutboundContent::new(Content::Text(arc_str("now **bold**"))),
            )
            .await?;
        assert_eq!(
            client.updated_messages.lock().unwrap()[0].text,
            "now *bold*"
        );
        Ok(())
    }

    #[tokio::test]
    async fn slack_edit_failures_are_readable() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user-update-fail".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        let message = poll_history_message("C123", "1710000000.000100", Utc::now());
        let error = provider
            .edit_message(
                &arc_str("C123"),
                &message,
                OutboundContent::new(Content::Text(arc_str("new"))),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("only lets the account that posted a message edit it"),
            "{error:#}"
        );
        assert_eq!(
            slack_update_error_message("edit_window_closed"),
            "Slack's edit window for this message has closed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_identity_cannot_edit() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T/B/X".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        assert!(!provider.outbound_capabilities().edit);
        let message = poll_history_message("C123", "1710000000.000100", Utc::now());
        assert!(
            provider
                .edit_message(
                    &arc_str("C123"),
                    &message,
                    OutboundContent::new(Content::Text(arc_str("new"))),
                )
                .await
                .is_err()
        );
        assert!(client.updated_messages.lock().unwrap().is_empty());
        Ok(())
    }

    #[test]
    fn slack_history_message_parses_edited_marker() {
        let parse = |json: &str| {
            let response: SlackHistoryMessageResponse = serde_json::from_str(json).unwrap();
            slack_history_message(
                arc_str("slack:test"),
                Some("U123"),
                "C123".to_owned(),
                response,
                None,
                None,
            )
            .unwrap()
        };
        let edited = parse(
            r#"{"type":"message","user":"U123","ts":"1710000000.000100","text":"v2",
                "edited":{"user":"U123","ts":"1710000060.000000"}}"#,
        );
        assert_eq!(
            edited.edited_at.map(|at| at.timestamp()),
            Some(1_710_000_060)
        );
        assert_eq!(edited.timestamp.timestamp(), 1_710_000_000);

        let unedited =
            parse(r#"{"type":"message","user":"U123","ts":"1710000000.000100","text":"v1"}"#);
        assert!(unedited.edited_at.is_none());
    }

    #[test]
    fn slack_realtime_inner_message_parses_edited_marker() {
        let inner: SlackRealtimeInnerMessage = serde_json::from_str(
            r#"{"user":"U1","ts":"1710000000.000100","text":"v2",
                "edited":{"user":"U1","ts":"1710000060.000000"}}"#,
        )
        .unwrap();
        assert_eq!(
            slack_edited_at(inner.edited.as_ref()).map(|at| at.timestamp()),
            Some(1_710_000_060)
        );
        let unfurl: SlackRealtimeInnerMessage =
            serde_json::from_str(r#"{"user":"U1","ts":"1710000000.000100","text":"v1"}"#).unwrap();
        assert!(slack_edited_at(unfurl.edited.as_ref()).is_none());
    }

    #[tokio::test]
    async fn webhook_mode_posts_text_to_webhook_only() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let reply_target = poll_history_message("ignored-channel", "ignored-thread", Utc::now());
        let message_id = provider
            .send(
                &arc_str("ignored-channel"),
                OutboundContent::new(Content::Text(arc_str("hello webhook"))),
                Some(&reply_target),
            )
            .await?;

        assert_eq!(message_id.as_ref(), "webhook:test");
        assert!(client.posted_messages.lock().unwrap().is_empty());
        assert_eq!(
            *client.posted_webhooks.lock().unwrap(),
            vec![(
                "https://hooks.slack.com/services/T000/B000/secret".to_owned(),
                "hello webhook".to_owned()
            )]
        );
        Ok(())
    }

    #[tokio::test]
    async fn read_only_mode_rejects_text_send() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::ReadOnlyOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        provider.connect().await?;

        let error = provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::Text(arc_str("blocked"))),
                None,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Slack sending is not available"));
        assert!(error.contains("read-only"));
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_lists_and_normalizes_conversations() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        options.workspace = Some("Team Alpha".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        {
            let mut conversations = client.conversations.lock().unwrap();
            let mut general = channel_conversation("C123", "general", 1_710_000_000);
            general.topic = Some("Company announcements".to_owned());
            general.unread_count = 3;
            let mut random = channel_conversation("C999", "random", 1_700_000_000);
            random.is_muted = true;
            let mut dm = dm_conversation("D123", "U234", 1_720_000_000_000);
            dm.is_pinned = true;
            conversations.extend([general, random, dm]);
        }
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let chats = provider.chats().await?;

        assert_eq!(
            *client.listed_conversations.lock().unwrap(),
            vec![SlackCredentialKind::UserToken]
        );
        assert_eq!(chats.len(), 3);
        assert_eq!(chats[0].id.as_ref(), "D123");
        assert_eq!(chats[0].name.as_ref(), "DM U234");
        assert_eq!(chats[0].kind, ChatKind::Direct);
        assert!(chats[0].pinned);
        assert!(!chats[0].is_group);
        assert_eq!(chats[0].account.as_ref(), "slack:team-alpha");
        assert_eq!(chats[1].id.as_ref(), "C123");
        assert_eq!(chats[1].name.as_ref(), "#general");
        assert_eq!(chats[1].kind, ChatKind::PublicChannel);
        assert_eq!(chats[1].membership, ChatMembership::Joined);
        assert_eq!(chats[1].unread_count, 3);
        assert_eq!(chats[1].last_message_at, None);
        assert_eq!(chats[1].last_message_preview, None);
        assert!(chats[1].is_group);
        assert!(chats[2].muted);
        Ok(())
    }

    #[tokio::test]
    async fn slack_sidebar_hides_unjoined_public_channels() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        {
            let mut conversations = client.conversations.lock().unwrap();
            let mut joined = channel_conversation("CJOIN", "joined", 1_710_000_000);
            joined.is_member = Some(true);
            let mut unjoined = channel_conversation("CNOPE", "unjoined", 1_720_000_000);
            unjoined.is_member = Some(false);
            let dm = dm_conversation("D123", "U234", 1_730_000_000);
            conversations.extend([joined, unjoined, dm]);
        }
        let provider = provider_with_fake_client(options, client)?;
        provider.connect().await?;

        let chats = provider.chats().await?;
        let names = chats
            .iter()
            .map(|chat| chat.name.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["#joined", "DM U234"]);
        assert!(
            chats
                .iter()
                .all(|chat| chat.membership == ChatMembership::Joined)
        );
        Ok(())
    }

    #[tokio::test]
    async fn bot_mode_lists_conversations_with_bot_token_only() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::BotToken);
        options.user_token = Some("xoxp-user".to_owned());
        options.bot_token = Some("xoxb-bot".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        client
            .conversations
            .lock()
            .unwrap()
            .push(channel_conversation("C123", "bot-visible", 1_710_000_000));
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let chats = provider.chats().await?;

        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].name.as_ref(), "#bot-visible");
        assert_eq!(
            *client.listed_conversations.lock().unwrap(),
            vec![SlackCredentialKind::BotToken]
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_mode_rejects_conversation_listing() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        provider.connect().await?;

        let error = provider.chats().await.unwrap_err().to_string();

        assert!(error.contains("Slack conversation history is not available"));
        assert!(error.contains("webhook"));
        Ok(())
    }

    #[tokio::test]
    async fn conversation_listing_errors_are_redacted() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-list-fail-secret".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        provider.connect().await?;

        let error = provider.chats().await.unwrap_err().to_string();

        assert!(error.contains("<redacted>"));
        assert!(!error.contains("xoxp-list-fail-secret"));
        Ok(())
    }

    #[tokio::test]
    async fn file_content_without_local_path_rejects_clearly() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        provider.connect().await?;

        let error = provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("report.pdf"),
                    mime_type: arc_str("application/pdf"),
                    ..Media::default()
                })),
                None,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Slack file upload requires a local file path"));
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_uploads_file_with_user_token_and_caption() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let message_id = provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("fono-snixembed.log"),
                    mime_type: arc_str("text/plain"),
                    caption: Some(arc_str("log file")),
                    local_path: Some(PathBuf::from("/tmp/fono-snixembed.log")),
                    ..Media::default()
                })),
                Some(&poll_history_message(
                    "ignored-channel",
                    "1710000000.000001",
                    Utc::now(),
                )),
            )
            .await?;

        assert_eq!(message_id.as_ref(), "F123UPLOAD");
        assert_eq!(
            *client.posted_messages.lock().unwrap(),
            vec![PostedCall {
                kind: SlackCredentialKind::UserToken,
                token: "xoxp-user".to_owned(),
                channel: "C123".to_owned(),
                text: "log file".to_owned(),
                thread_ts: Some("1710000000.000001".to_owned()),
            }]
        );
        Ok(())
    }

    #[tokio::test]
    async fn webhook_mode_rejects_file_upload() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());
        let provider = provider_with_fake_client(options, Arc::new(FakeSlackApiClient::default()))?;
        provider.connect().await?;

        let error = provider
            .send(
                &arc_str("C123"),
                OutboundContent::new(Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("report.pdf"),
                    mime_type: arc_str("application/pdf"),
                    local_path: Some(PathBuf::from("/tmp/report.pdf")),
                    ..Media::default()
                })),
                None,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Slack file upload is not available"));
        assert!(error.contains("webhook"));
        Ok(())
    }

    #[tokio::test]
    async fn user_mode_reacts_and_toggles_existing_reaction() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let mut message = slack_message_from_parts(
            provider.id(),
            Some("U123"),
            Some("C123".to_owned()),
            Some("U234".to_owned()),
            None,
            SlackMessageSenderMetadata::default(),
            Some("1710000000.000200".to_owned()),
            None,
            Some("hello".to_owned()),
            None,
            None,
            None,
            Vec::new(),
            None,
            false,
            None,
        )
        .expect("message should parse");

        provider
            .react(&arc_str("C123"), &message, ":thumbsup:")
            .await?;
        assert_eq!(
            *client.added_reactions.lock().unwrap(),
            vec![(
                "C123".to_owned(),
                "1710000000.000200".to_owned(),
                "thumbsup".to_owned()
            )]
        );

        message.reactions.push(Reaction {
            emoji: arc_str("👍"),
            senders: vec![arc_str("U123")],
        });
        provider
            .react(&arc_str("C123"), &message, "thumbsup")
            .await?;
        assert_eq!(
            *client.removed_reactions.lock().unwrap(),
            vec![(
                "C123".to_owned(),
                "1710000000.000200".to_owned(),
                "thumbsup".to_owned()
            )]
        );
        Ok(())
    }

    #[test]
    fn slack_message_sender_uses_bot_username_and_icon_metadata() {
        let sender = slack_message_sender(
            "B123",
            slack_message_sender_metadata(
                Some("Incoming Bot".to_owned()),
                Some(SlackMessageIconsResponse {
                    image_72: Some("https://example.com/icon72.png".to_owned()),
                    image_48: Some("https://example.com/icon48.png".to_owned()),
                    ..SlackMessageIconsResponse::default()
                }),
                None,
            ),
        );

        assert_eq!(sender.platform_id.as_ref(), "B123");
        assert_eq!(sender.display_name.as_ref(), "Incoming Bot");
        assert!(sender.avatar.is_some());
    }

    #[test]
    fn slack_message_sender_prefers_bot_profile_metadata() {
        let sender = slack_message_sender(
            "B123",
            slack_message_sender_metadata(
                Some("fallback username".to_owned()),
                None,
                Some(SlackBotProfileResponse {
                    id: Some("B123".to_owned()),
                    name: Some("bot-name".to_owned()),
                    real_name: Some("Bot Real Name".to_owned()),
                    icons: Some(SlackMessageIconsResponse {
                        image_48: Some("https://example.com/bot48.png".to_owned()),
                        ..SlackMessageIconsResponse::default()
                    }),
                }),
            ),
        );

        assert_eq!(sender.display_name.as_ref(), "Bot Real Name");
        assert!(sender.avatar.is_some());
    }

    #[test]
    fn historical_slack_message_uses_message_sender_metadata_when_user_is_uncached() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: None,
                bot_id: Some("B123".to_owned()),
                username: Some("Deploy Bot".to_owned()),
                icons: Some(SlackMessageIconsResponse {
                    image_72: Some("https://example.com/deploy.png".to_owned()),
                    ..SlackMessageIconsResponse::default()
                }),
                bot_profile: None,
                ts: Some("1710000002.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("deployed :large_green_circle:".to_owned()),
                blocks: None,
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse");

        assert_eq!(message.sender.platform_id.as_ref(), "B123");
        assert_eq!(message.sender.display_name.as_ref(), "Deploy Bot");
        assert!(message.sender.avatar.is_some());
        assert_eq!(content_text(&message.content), "deployed 🟢");
    }

    #[tokio::test]
    async fn socket_mode_handler_marks_event_callbacks_for_idle_diagnostics() {
        let events = EventBus::new();
        let account = arc_str("slack:test");
        let users = Arc::new(RwLock::new(HashMap::new()));
        let handled = handle_socket_mode_text(
            &events,
            &account,
            Some("U123"),
            r#"{"envelope_id":"env-1","type":"events_api","payload":{"type":"event_callback","event":{"type":"message","channel":"C123","user":"U999","ts":"1710000005.000200","text":"hello"}}}"#,
            &users,
            None,
        )
        .expect("socket envelope parses");

        assert_eq!(handled.ack.as_deref(), Some(r#"{"envelope_id":"env-1"}"#));
        assert!(handled.received_event_callback);
    }

    #[tokio::test]
    async fn socket_mode_handler_does_not_mark_hello_as_event_callback() {
        let events = EventBus::new();
        let account = arc_str("slack:test");
        let users = Arc::new(RwLock::new(HashMap::new()));
        let handled = handle_socket_mode_text(
            &events,
            &account,
            Some("U123"),
            r#"{"type":"hello"}"#,
            &users,
            None,
        )
        .expect("hello envelope parses");

        assert!(handled.ack.is_none());
        assert!(!handled.received_event_callback);
    }

    #[tokio::test]
    async fn realtime_slack_bot_message_subtype_emits_live_message() {
        let events = EventBus::new();
        let mut receiver = events.subscribe();
        let account = arc_str("slack:test");
        let users = Arc::new(RwLock::new(HashMap::new()));

        emit_realtime_message(
            &events,
            &account,
            Some("U123"),
            SlackRealtimeEvent {
                event_type: "message".to_owned(),
                channel: Some("C123".to_owned()),
                user: None,
                bot_id: Some("BDEPLOY".to_owned()),
                username: Some("deploy".to_owned()),
                icons: None,
                bot_profile: None,
                ts: Some("1710000004.000200".to_owned()),
                event_ts: None,
                thread_ts: None,
                text: Some("deployment finished".to_owned()),
                blocks: None,
                attachments: None,
                subtype: Some("bot_message".to_owned()),
                files: None,
                hidden: None,
                deleted_ts: None,
                message: None,
                reaction: None,
                item: None,
                item_user: None,
            },
            &users,
            None,
        )
        .await;

        let event = receiver.try_recv().expect("live message event");
        let ProviderEvent::Message {
            message,
            is_historical,
        } = event
        else {
            panic!("expected live message event");
        };
        assert!(!is_historical);
        assert_eq!(message.id.as_ref(), "1710000004.000200");
        assert_eq!(message.chat_id.as_ref(), "C123");
        assert_eq!(message.sender.platform_id.as_ref(), "BDEPLOY");
        assert_eq!(message.sender.display_name.as_ref(), "deploy");
        assert_eq!(content_text(&message.content), "deployment finished");
    }

    #[test]
    fn historical_slack_message_uses_attachment_title_text_and_fields() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: Some("bot_message".to_owned()),
                user: None,
                bot_id: Some("BDEPLOY".to_owned()),
                username: Some("deploy".to_owned()),
                icons: None,
                bot_profile: None,
                ts: Some("1710000003.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: None,
                blocks: None,
                attachments: Some(vec![SlackAttachmentResponse {
                    pretext: None,
                    title: Some("Partition maintenance successful on deploy".to_owned()),
                    title_link: None,
                    color: Some("good".to_owned()),
                    image_url: None,
                    thumb_url: None,
                    author_name: None,
                    author_link: None,
                    footer: None,
                    ts: None,
                    text: Some(
                        "Script: /var/www/wap/partition_maintenance.sh, Elapsed time: 415 seconds"
                            .to_owned(),
                    ),
                    fallback: Some("fallback should not duplicate content".to_owned()),
                    fields: None,
                }]),
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse");

        assert_eq!(message.sender.display_name.as_ref(), "deploy");
        let Content::Cards(cards) = &message.content else {
            panic!("Slack attachment should be preserved as cards");
        };
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.source, CardSource::Slack);
        assert_eq!(card.kind, CardKind::ProviderAttachment);
        assert_eq!(
            card.title.as_deref(),
            Some("Partition maintenance successful on deploy")
        );
        assert_eq!(
            card.body.as_deref(),
            Some("Script: /var/www/wap/partition_maintenance.sh, Elapsed time: 415 seconds")
        );
        assert_eq!(card.accent_color, Some(CardColor::Named(arc_str("good"))));
        assert_eq!(
            content_text(&message.content),
            "Partition maintenance successful on deploy\nScript: /var/www/wap/partition_maintenance.sh, Elapsed time: 415 seconds"
        );
    }

    // A Block Kit bot message (NewRelic alert shape) must render as structured
    // cards: status/title group with action buttons, standalone chart image
    // card with a reserved cache path, detail sections, and a context footer
    // with link labels instead of raw `<url|label>` tokens. The top-level
    // `text` is only Slack's notification fallback ("... Acknowledge button
    // ...") and must not leak into the content.
    #[test]
    fn historical_slack_message_with_blocks_renders_structured_cards() {
        let blocks = vec![
            serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": ":large_red_square: *Critical priority issue is active*"
                }
            }),
            serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": "<https://radar-api.service.newrelic.com/accounts/20424/issues/abc?notifier=SLACK|*Metric query deviated from the baseline for at least 5 minutes on 'WaP Web Shop Production - Error rate'*>"
                }
            }),
            serde_json::json!({
                "type": "actions",
                "elements": [
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": ":toolbox: Acknowledge", "emoji": true },
                        "value": "ack"
                    },
                    {
                        "type": "button",
                        "text": { "type": "plain_text", "text": ":heavy_check_mark: Close", "emoji": true },
                        "value": "close"
                    }
                ]
            }),
            serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": "*1 alert event* · Metric query deviated from the baseline"
                }
            }),
            serde_json::json!({
                "type": "image",
                "image_url": "https://chart-embed.example.invalid/charts/violation.png",
                "alt_text": "Violation chart",
                "title": { "type": "plain_text", "text": "UTC TIME (10 kB)", "emoji": true }
            }),
            serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": "*1 policy* · Webshop" }
            }),
            serde_json::json!({
                "type": "context",
                "elements": [{
                    "type": "mrkdwn",
                    "text": "This notification was sent via the \"Policy: 3630765 - Webshop\" workflow. <https://radar-api.service.newrelic.com/accounts/20424/workflows/abc?notifier=SLACK|⚙️ Edit workflow>"
                }]
            }),
        ];
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: Some("bot_message".to_owned()),
                user: None,
                bot_id: Some("BNEWRELIC".to_owned()),
                username: Some("New Relic".to_owned()),
                icons: None,
                bot_profile: None,
                ts: Some("1710000004.000300".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some(
                    "Critical priority issue is active :toolbox: Acknowledge button :heavy_check_mark: Close button"
                        .to_owned(),
                ),
                blocks: Some(blocks),
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse");

        let Content::Cards(cards) = &message.content else {
            panic!("Block Kit message should render as cards");
        };
        assert_eq!(
            cards.len(),
            4,
            "status+buttons, alert event, image, details"
        );

        // Group 1: status line + linked title + buttons.
        let lead = &cards[0];
        assert_eq!(lead.kind, CardKind::BotMessage);
        let lead_body = lead.body.as_deref().expect("lead card has a body");
        assert!(lead_body.contains("🟥 **Critical priority issue is active**"));
        assert!(
            lead_body.contains(
                "**Metric query deviated from the baseline for at least 5 minutes on 'WaP Web Shop Production - Error rate'**"
            ),
            "title link must render its (bold) label: {lead_body}"
        );
        assert!(
            !lead_body.contains("<https://"),
            "raw angle-bracket URL tokens must not leak: {lead_body}"
        );
        assert_eq!(lead.accent_color, Some(CardColor::Named(arc_str("danger"))));
        let labels = lead
            .actions
            .iter()
            .map(|action| action.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["🧰 Acknowledge", "✔️ Close"]);
        assert!(lead.actions.iter().all(|action| action.url.is_none()));

        // Group 2: the "1 alert event" section renders above the chart, in
        // block order.
        assert!(
            cards[1]
                .body
                .as_deref()
                .unwrap()
                .contains("**1 alert event**")
        );

        // Group 3: chart image with a reserved local cache path so the preview
        // pipeline can render it (or show retrieve/placeholder states).
        let chart = &cards[2];
        assert_eq!(chart.kind, CardKind::MediaPreview);
        assert_eq!(chart.title.as_deref(), Some("UTC TIME (10 kB)"));
        let image = chart.image.as_ref().expect("image block becomes media");
        assert!(image.local_path.is_some());

        // Group 4: detail sections + context footer with link label.
        let details = &cards[3];
        assert!(
            details
                .body
                .as_deref()
                .unwrap()
                .contains("**1 policy** · Webshop")
        );
        let footer = details.footer.as_deref().expect("context becomes footer");
        assert!(footer.contains("⚙️ Edit workflow"));
        assert!(!footer.contains("<https://"));

        // The notification fallback must not appear anywhere in the content.
        assert!(!content_text(&message.content).contains("Acknowledge button"));
    }

    // A message whose blocks are all unmodeled must keep today's behavior and
    // fall back to the plain `text`, never dropping the message.
    #[test]
    fn unknown_block_types_degrade_to_fallback_text() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: Some("bot_message".to_owned()),
                user: None,
                bot_id: Some("BBOT".to_owned()),
                username: Some("bot".to_owned()),
                icons: None,
                bot_profile: None,
                ts: Some("1710000005.000400".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("fallback text".to_owned()),
                blocks: Some(vec![
                    serde_json::json!({ "type": "fancy_new_block", "payload": { "x": 1 } }),
                    serde_json::json!({ "type": ["not", "a", "string"] }),
                ]),
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse despite unknown blocks");

        let Content::Text(text) = &message.content else {
            panic!("unparseable blocks should fall back to text content");
        };
        assert_eq!(text.as_ref(), "fallback text");
    }

    // mrkdwn -> renderer-markdown dialect translation: Slack single-character
    // delimiters become the renderer's doubled markers, link tokens render
    // their labels, and code spans plus literal characters pass through.
    #[test]
    fn slack_mrkdwn_translation_converts_styles_links_and_preserves_code() {
        assert_eq!(
            slack_mrkdwn_to_markdown("*1 alert event* and ~old~"),
            "**1 alert event** and ~~old~~"
        );
        assert_eq!(
            slack_mrkdwn_to_markdown("<https://example.com/a|Edit workflow> done"),
            "Edit workflow done"
        );
        assert_eq!(
            slack_mrkdwn_to_markdown("see <https://example.com/bare>"),
            "see https://example.com/bare"
        );
        assert_eq!(slack_mrkdwn_to_markdown("<#C123|general>"), "#general");
        // Mention tokens survive for the later user-substitution pass.
        assert_eq!(
            slack_mrkdwn_to_markdown("<@U123> and <!here>"),
            "<@U123> and <!here>"
        );
        // Code spans are verbatim; literal asterisks in prose stay literal.
        assert_eq!(
            slack_mrkdwn_to_markdown("`*not bold*` and 2*3*4"),
            "`*not bold*` and 2*3*4"
        );
    }

    // Attachment `image_url`/`thumb_url` images are public CDN assets; they
    // must reserve a local cache path so the preview pipeline can render them
    // instead of dead-ending on `local_path: None`.
    #[test]
    fn slack_attachment_image_card_reserves_local_cache_path() {
        let media = slack_card_media(
            "image",
            "https://attachment-image.example.invalid/chart.png",
        );
        assert!(media.local_path.is_some());
    }

    // With a token, private Slack files get an authenticated lazy cache path so
    // history-loaded and realtime official-app uploads render the same way.
    #[test]
    fn slack_file_image_media_uses_authenticated_cache_path_when_token_available() {
        let file = SlackFileResponse {
            id: Some("F1".to_owned()),
            name: Some("image.png".to_owned()),
            title: Some("image".to_owned()),
            mimetype: Some("image/png".to_owned()),
            filetype: Some("png".to_owned()),
            size: Some(1234),
            url_private: Some("https://files.slack.com/image.png".to_owned()),
            url_private_download: None,
            thumb_360: None,
            thumb_720: None,
            thumb_1024: None,
            permalink: Some("https://slack.com/files/image".to_owned()),
            mode: Some("hosted".to_owned()),
        };

        let media = slack_file_image_media(&file, Some("xoxb-test-token"))
            .expect("image file should become media");

        assert_eq!(media.mime_type.as_ref(), "image/png");
        assert!(media.local_path.is_some());
    }

    // Files above the auto-download limit must still reserve their cache path
    // (so the UI can offer on-demand retrieval and detect arrival), but no
    // background download may be queued for them.
    #[test]
    fn oversized_slack_file_reserves_cache_path_without_eager_download() {
        let url = "https://files.slack.com/oversized-eager-download-test.png";
        let file = SlackFileResponse {
            id: Some("F9".to_owned()),
            name: Some("huge.png".to_owned()),
            title: Some("huge".to_owned()),
            mimetype: Some("image/png".to_owned()),
            filetype: Some("png".to_owned()),
            size: Some(SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES + 1),
            url_private: Some(url.to_owned()),
            url_private_download: None,
            thumb_360: None,
            thumb_720: None,
            thumb_1024: None,
            permalink: None,
            mode: Some("hosted".to_owned()),
        };

        let media = slack_file_image_media(&file, Some("xoxb-test-token"))
            .expect("oversized image file should still become media");

        assert_eq!(
            media.size_bytes,
            Some(SLACK_MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES + 1)
        );
        let reserved = slack_media_cache_file_path(url, "files").expect("cache path");
        assert_eq!(media.local_path.as_deref(), Some(reserved.as_path()));
        assert!(!reserved.exists());
        // The URL was never registered with the download registry: claiming it
        // now succeeds, proving no eager background download was spawned.
        assert!(
            slack_begin_media_download(url, false),
            "oversized files must not enqueue a background download"
        );
        slack_finish_media_download(url, true);
    }

    #[test]
    fn media_download_registry_dedups_in_flight_and_applies_failure_cooldown() {
        let url = "https://files.slack.com/registry-dedup-cooldown-test.png";

        assert!(slack_begin_media_download(url, false));
        // Already in flight: neither background nor forced (user-initiated)
        // retrieval may start a concurrent download of the same URL.
        assert!(!slack_begin_media_download(url, false));
        assert!(!slack_begin_media_download(url, true));

        // A failure puts the URL into cooldown for background retries...
        slack_finish_media_download(url, false);
        assert!(!slack_begin_media_download(url, false));
        // ...but explicit user-initiated retrieval may retry through it.
        assert!(slack_begin_media_download(url, true));

        // Success clears both the in-flight entry and the failure marker.
        slack_finish_media_download(url, true);
        assert!(slack_begin_media_download(url, false));
        slack_finish_media_download(url, true);
    }

    #[test]
    fn bot_sender_ids_get_placeholder_users_instead_of_user_fallback() {
        assert!(is_slack_bot_id("B0N6Y87V0"));
        assert!(!is_slack_bot_id("U123"));
        assert!(!is_slack_bot_id("W123"));
        assert!(!is_slack_bot_id("B"));
        assert!(!is_slack_bot_id(""));

        // The user fallback keeps rejecting bot ids; the bot placeholder
        // carries the id as its display name and is flagged as a bot.
        assert!(fallback_slack_user("B0N6Y87V0").is_none());
        let bot = fallback_slack_bot_user("B0N6Y87V0");
        assert!(bot.is_bot);
        assert_eq!(bot.display_name.as_deref(), Some("B0N6Y87V0"));
    }

    #[tokio::test]
    async fn resolve_user_synthesizes_bot_placeholder_without_users_info_call() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::UserOAuth);
        options.user_token = Some("xoxp-user".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;
        let credential = SlackCredential::new(SlackCredentialKind::UserToken, "xoxp-user");

        let bot = provider
            .resolve_user(&credential, "B0N6Y87V0")
            .await?
            .expect("bot sender should resolve to a placeholder");

        assert!(bot.is_bot);
        assert_eq!(bot.display_name.as_deref(), Some("B0N6Y87V0"));
        assert!(
            client.user_info_calls.lock().unwrap().is_empty(),
            "bot ids must not be sent to users.info"
        );

        // Regular user ids still go through users.info.
        provider.resolve_user(&credential, "U123").await?;
        assert_eq!(
            *client.user_info_calls.lock().unwrap(),
            vec!["U123".to_owned()]
        );
        Ok(())
    }

    #[test]
    fn historical_slack_message_preserves_text_before_link_attachment_card() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000006.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("Comentariu înainte de link :white_check_mark:".to_owned()),
                blocks: None,
                attachments: Some(vec![SlackAttachmentResponse {
                    pretext: None,
                    title: Some("How to debug invisible text".to_owned()),
                    title_link: Some("https://example.com/debug".to_owned()),
                    color: None,
                    image_url: None,
                    thumb_url: None,
                    author_name: None,
                    author_link: None,
                    footer: None,
                    ts: None,
                    text: Some("Preview description".to_owned()),
                    fallback: Some("fallback should not replace visible text".to_owned()),
                    fields: None,
                }]),
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse");

        let Content::Cards(cards) = &message.content else {
            panic!("message with text and a link attachment should render as cards");
        };
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].kind, CardKind::BotMessage);
        assert_eq!(
            cards[0].body.as_deref(),
            Some("Comentariu înainte de link ✅")
        );
        assert_eq!(cards[1].kind, CardKind::ProviderAttachment);
        assert_eq!(
            cards[1].title.as_deref(),
            Some("How to debug invisible text")
        );
        assert_eq!(
            content_text(&message.content),
            "Comentariu înainte de link ✅\nHow to debug invisible text\nPreview description\nhttps://example.com/debug"
        );
    }

    #[test]
    fn historical_slack_message_renders_image_files_as_media_cards_with_caption() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000005.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("Cica vecini gospodari".to_owned()),
                blocks: None,
                attachments: None,
                files: Some(vec![
                    SlackFileResponse {
                        id: Some("F1".to_owned()),
                        name: Some("first.png".to_owned()),
                        title: Some("first".to_owned()),
                        mimetype: Some("image/png".to_owned()),
                        filetype: Some("png".to_owned()),
                        size: Some(1234),
                        url_private: Some("https://files.slack.com/first.png".to_owned()),
                        url_private_download: None,
                        thumb_360: None,
                        thumb_720: None,
                        thumb_1024: None,
                        permalink: Some("https://slack.com/files/first".to_owned()),
                        mode: Some("hosted".to_owned()),
                    },
                    SlackFileResponse {
                        id: Some("F2".to_owned()),
                        name: Some("second.jpg".to_owned()),
                        title: Some("second".to_owned()),
                        mimetype: Some("image/jpeg".to_owned()),
                        filetype: Some("jpg".to_owned()),
                        size: Some(5678),
                        url_private: Some("https://files.slack.com/second.jpg".to_owned()),
                        url_private_download: None,
                        thumb_360: None,
                        thumb_720: None,
                        thumb_1024: None,
                        permalink: Some("https://slack.com/files/second".to_owned()),
                        mode: Some("hosted".to_owned()),
                    },
                ]),
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message with image files should parse");

        let Content::Cards(cards) = &message.content else {
            panic!("Slack image files should be preserved as cards");
        };
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().all(|card| card.kind == CardKind::MediaPreview));
        assert!(cards.iter().all(|card| card.image.is_some()));
        // The message text is preserved as a caption on the first image card.
        assert_eq!(cards[0].body.as_deref(), Some("Cica vecini gospodari"));
        assert_eq!(
            cards[0]
                .image
                .as_ref()
                .map(|image| image.mime_type.as_ref()),
            Some("image/png")
        );
    }

    #[test]
    fn slack_channel_join_messages_are_kept_for_resolved_sidebar_previews() {
        assert!(!is_ignored_slack_message_subtype(Some("channel_join")));
        assert!(!is_ignored_slack_message_subtype(Some("channel_leave")));
        assert!(is_ignored_slack_message_subtype(Some("message_deleted")));
        assert!(is_ignored_slack_message_subtype(Some("message_changed")));
    }

    #[test]
    fn slack_user_mentions_prefer_cache_label_then_id_fallback() {
        let users = Arc::new(RwLock::new(HashMap::from([(
            "U123".to_owned(),
            SlackUser {
                id: "U123".to_owned(),
                name: Some("ada".to_owned()),
                real_name: Some("Ada Lovelace".to_owned()),
                display_name: Some("Ada".to_owned()),
                avatar: None,
                is_bot: false,
                deleted: false,
                ..SlackUser::default()
            },
        )])));

        assert_eq!(
            replace_slack_user_mentions("hi <@U123> <@U456|Grace> <@U789>", &users),
            "hi @Ada @Grace @U789"
        );
    }

    #[test]
    fn slack_text_mentions_user_matches_self_and_broadcast_tokens() {
        let me = Some("UME123");

        // Explicit mention of the authenticated user.
        assert!(slack_text_mentions_user("hey <@UME123> look", me));
        // Mention of someone else does not count.
        assert!(!slack_text_mentions_user("hey <@UOTHER1> look", me));
        // No mention at all.
        assert!(!slack_text_mentions_user("plain channel chatter", me));

        // Broadcast pings count as mentions regardless of the current user id.
        assert!(slack_text_mentions_user("heads up <!here>", me));
        assert!(slack_text_mentions_user("ship it <!channel>", me));
        assert!(slack_text_mentions_user("all hands <!everyone>", me));
        assert!(slack_text_mentions_user("labelled <!here|here>", me));
        assert!(slack_text_mentions_user("broadcast <!channel>", None));

        // A non-broadcast bang token is not a mention.
        assert!(!slack_text_mentions_user("see <!date^123^{date}>", me));
        // Without a known self id and no broadcast, nothing matches.
        assert!(!slack_text_mentions_user("hey <@UME123>", None));
    }

    #[test]
    fn slack_emoji_display_maps_standard_and_preserves_custom_names() {
        assert_eq!(slack_emoji_display("eyes"), "👀");
        assert_eq!(slack_emoji_display("money_with_wings"), "💸");
        assert_eq!(slack_emoji_display(":white_check_mark:"), "✅");
        assert_eq!(slack_emoji_display("slightly_smiling_face"), "🙂");
        assert_eq!(slack_emoji_display("rolling_on_the_floor_laughing"), "🤣");
        assert_eq!(slack_emoji_display("large_green_circle"), "🟢");
        assert_eq!(slack_emoji_display("large_orange_square"), "🟧");
        assert_eq!(slack_emoji_display("party-parrot"), ":party-parrot:");
    }

    #[test]
    fn slack_text_replaces_known_codes_and_keeps_custom_codes_readable() {
        assert_eq!(
            replace_slack_emoji_codes(
                "done :white_check_mark: watched :eyes: paid :money_with_wings:"
            ),
            "done ✅ watched 👀 paid 💸"
        );
        assert_eq!(
            replace_slack_emoji_codes("custom :party-parrot: and not emoji :hello world:"),
            "custom :party-parrot: and not emoji :hello world:"
        );
    }

    #[test]
    fn slack_reaction_matching_accepts_unicode_display_and_slack_names() {
        assert!(slack_reaction_matches("👀", "eyes"));
        assert!(slack_reaction_matches("💸", "money_with_wings"));
        assert!(slack_reaction_matches("✅", "white_check_mark"));
        assert!(slack_reaction_matches(":party-parrot:", "party-parrot"));
        assert!(!slack_reaction_matches("👀", "money_with_wings"));
    }

    #[test]
    fn slack_history_reactions_use_display_icons_with_custom_fallbacks() {
        let reactions = slack_reactions(Some(vec![
            SlackReactionResponse {
                name: Some("eyes".to_owned()),
                users: Some(vec!["U123".to_owned()]),
            },
            SlackReactionResponse {
                name: Some("money_with_wings".to_owned()),
                users: Some(vec!["U234".to_owned(), "U345".to_owned()]),
            },
            SlackReactionResponse {
                name: Some("party-parrot".to_owned()),
                users: Some(vec!["U456".to_owned()]),
            },
        ]));

        assert_eq!(reactions.len(), 3);
        assert_eq!(reactions[0].emoji.as_ref(), "👀");
        assert_eq!(reactions[0].senders.len(), 1);
        assert_eq!(reactions[1].emoji.as_ref(), "💸");
        assert_eq!(reactions[1].senders.len(), 2);
        assert_eq!(reactions[2].emoji.as_ref(), ":party-parrot:");
    }

    #[test]
    fn slack_realtime_reaction_events_emit_display_icons() {
        let events = EventBus::new();
        let mut receiver = events.subscribe();
        emit_realtime_reaction(
            &events,
            SlackRealtimeEvent {
                event_type: "reaction_added".to_owned(),
                channel: None,
                user: Some("U123".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: None,
                event_ts: None,
                thread_ts: None,
                text: None,
                blocks: None,
                attachments: None,
                subtype: None,
                files: None,
                hidden: None,
                deleted_ts: None,
                message: None,
                reaction: Some("eyes".to_owned()),
                item: Some(SlackReactionItem {
                    channel: Some("C123".to_owned()),
                    ts: Some("1710000000.000200".to_owned()),
                }),
                item_user: None,
            },
        );

        match receiver.try_recv().expect("event should be emitted") {
            ProviderEvent::ReactionChanged {
                chat_id,
                message_id,
                emoji,
                added,
                sender,
            } => {
                assert_eq!(chat_id.as_ref(), "C123");
                assert_eq!(message_id.as_ref(), "1710000000.000200");
                assert_eq!(emoji.as_ref(), "👀");
                assert!(added);
                assert_eq!(sender.as_ref(), "U123");
            }
            event => panic!("unexpected event: {event:?}"),
        }
    }

    #[test]
    fn historical_slack_message_converts_text_emoji_shortcodes() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000000.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("done :white_check_mark: custom :party-parrot:".to_owned()),
                blocks: None,
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("message should parse");

        assert_eq!(
            content_text(&message.content),
            "done ✅ custom :party-parrot:"
        );
    }

    #[test]
    fn historical_slack_message_maps_thread_reactions_and_avatar() {
        let users = Arc::new(RwLock::new(HashMap::from([(
            "U234".to_owned(),
            SlackUser {
                id: "U234".to_owned(),
                name: Some("alice".to_owned()),
                real_name: None,
                display_name: Some("Alice".to_owned()),
                avatar: Some("https://example.com/alice.png".to_owned()),
                deleted: false,
                is_bot: false,
                ..SlackUser::default()
            },
        )])));
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000001.000200".to_owned()),
                thread_ts: Some("1710000000.000100".to_owned()),
                reply_count: None,
                text: Some("thread reply".to_owned()),
                blocks: None,
                attachments: None,
                files: None,
                hidden: None,
                reactions: Some(vec![SlackReactionResponse {
                    name: Some("eyes".to_owned()),
                    users: Some(vec!["U123".to_owned(), "U234".to_owned()]),
                }]),
            },
            Some(&users),
            None,
        )
        .expect("message should parse");

        assert_eq!(message.reply_to.as_deref(), Some("1710000000.000100"));
        assert_eq!(message.thread_id.as_deref(), Some("1710000000.000100"));
        assert_eq!(message.sender.display_name.as_ref(), "Alice");
        assert!(message.sender.avatar.is_some());
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].emoji.as_ref(), "👀");
        assert_eq!(message.reactions[0].senders.len(), 2);
    }

    #[test]
    fn historical_slack_thread_root_keeps_thread_identity_without_reply_target() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000000.000100".to_owned()),
                thread_ts: Some("1710000000.000100".to_owned()),
                reply_count: Some(2),
                text: Some("thread root".to_owned()),
                blocks: None,
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("thread root should parse");

        assert_eq!(message.id.as_ref(), "1710000000.000100");
        assert_eq!(message.reply_to, None);
        assert_eq!(message.thread_id.as_deref(), Some("1710000000.000100"));
    }

    #[test]
    fn historical_slack_unthreaded_message_has_no_thread_identity() {
        let message = slack_history_message(
            arc_str("slack:test"),
            Some("U123"),
            "C123".to_owned(),
            SlackHistoryMessageResponse {
                edited: None,
                message_type: Some("message".to_owned()),
                subtype: None,
                user: Some("U234".to_owned()),
                bot_id: None,
                username: None,
                icons: None,
                bot_profile: None,
                ts: Some("1710000000.000200".to_owned()),
                thread_ts: None,
                reply_count: None,
                text: Some("plain message".to_owned()),
                blocks: None,
                attachments: None,
                files: None,
                hidden: None,
                reactions: None,
            },
            None,
            None,
        )
        .expect("plain message should parse");

        assert_eq!(message.reply_to, None);
        assert_eq!(message.thread_id, None);
    }

    #[test]
    fn workspace_label_creates_distinct_provider_ids() -> Result<()> {
        let mut alpha = SlackProviderOptions::new(SlackAuthMode::ImportedToken);
        alpha.workspace = Some("Team Alpha".to_owned());
        let mut beta = SlackProviderOptions::new(SlackAuthMode::ImportedToken);
        beta.workspace = Some("Team Beta".to_owned());

        let alpha = SlackProvider::with_options(alpha)?;
        let beta = SlackProvider::with_options(beta)?;

        assert_eq!(alpha.id().as_ref(), "slack:team-alpha");
        assert_eq!(beta.id().as_ref(), "slack:team-beta");
        assert_ne!(alpha.id(), beta.id());
        Ok(())
    }

    #[test]
    fn provider_uses_stable_setup_id_and_slack_platform() -> Result<()> {
        let provider =
            SlackProvider::with_options(SlackProviderOptions::new(SlackAuthMode::ImportedToken))?;

        assert_eq!(provider.id().as_ref(), PROVIDER_ID);
        assert_eq!(provider.platform(), Platform::Slack);
        Ok(())
    }
}
