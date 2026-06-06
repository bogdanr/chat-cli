use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use chat_core::{
    Account, AuthChallenge, AuthSubmission, AuthSubmissionMode, Chat, ChatId, ChatKind,
    ChatMembership, Content, EventBus, Media, Message, MessageId, OutboundCapabilities, Platform,
    PlatformData, PlatformId, Provider, ProviderEvent, ProviderId, Sender, SlackData, Timestamp,
};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt, fs,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message as WebSocketMessage};

const PROVIDER_ID: &str = "slack:setup";
const PROVIDER_ID_PREFIX: &str = "slack";
const SLACK_CONVERSATION_TYPES: &str = "public_channel,private_channel,mpim,im";
const SLACK_HTTP_TIMEOUT: Duration = Duration::from_secs(12);

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
    user_id: Option<String>,
    bot_id: Option<String>,
}

#[derive(Clone, Debug)]
struct SlackValidatedConnection {
    capabilities: SlackCapabilities,
    connection: SlackConnectionState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackUser {
    pub id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
    pub deleted: bool,
    pub is_bot: bool,
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
    pub updated: Option<i64>,
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
    channels: Vec<SlackConversationResponse>,
    #[serde(default)]
    response_metadata: SlackResponseMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SlackResponseMetadata {
    next_cursor: Option<String>,
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
    updated: Option<i64>,
    topic: Option<SlackTextValue>,
    purpose: Option<SlackTextValue>,
    num_members: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SlackConversationsHistoryResponse {
    ok: bool,
    error: Option<String>,
    messages: Vec<SlackHistoryMessageResponse>,
}

#[derive(Debug, Deserialize)]
struct SlackHistoryMessageResponse {
    #[serde(rename = "type")]
    message_type: Option<String>,
    subtype: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
    hidden: Option<bool>,
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
}

#[derive(Debug, Deserialize)]
struct SlackUserProfileResponse {
    real_name: Option<String>,
    display_name: Option<String>,
    image_72: Option<String>,
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
            avatar: None,
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

    async fn upload_file(
        &self,
        credential: SlackCredential,
        request: SlackUploadFileRequest,
    ) -> Result<SlackUploadedFile>;

    async fn list_conversations(
        &self,
        credential: SlackCredential,
    ) -> Result<Vec<SlackConversation>>;

    async fn user_info(
        &self,
        credential: SlackCredential,
        user_id: &str,
    ) -> Result<Option<SlackUser>>;

    async fn conversation_members(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Vec<String>>;

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
    ts: Option<String>,
    event_ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
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
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
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
            updated: response.updated,
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
        list_web_api_conversations(credential).await
    }

    async fn user_info(
        &self,
        credential: SlackCredential,
        user_id: &str,
    ) -> Result<Option<SlackUser>> {
        get_web_api_user_info(credential, user_id).await
    }

    async fn conversation_members(
        &self,
        credential: SlackCredential,
        channel: &str,
    ) -> Result<Vec<String>> {
        list_web_api_conversation_members(credential, channel).await
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

async fn list_web_api_conversations(credential: SlackCredential) -> Result<Vec<SlackConversation>> {
    if !matches!(
        credential.kind,
        SlackCredentialKind::UserToken
            | SlackCredentialKind::BotToken
            | SlackCredentialKind::Unknown
    ) {
        bail!("Slack conversations.list requires a user or bot Web API token");
    }

    let token = credential.value;
    tokio::task::spawn_blocking(move || {
        let mut conversations = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let mut request = slack_http_agent()
                .get("https://slack.com/api/conversations.list")
                .header("Authorization", format!("Bearer {token}"))
                .query("types", SLACK_CONVERSATION_TYPES)
                .query("exclude_archived", "true")
                .query("limit", "200");

            if let Some(cursor) = cursor.as_deref().filter(|cursor| !cursor.is_empty()) {
                request = request.query("cursor", cursor);
            }

            let mut response = request.call().context("calling Slack conversations.list")?;
            let listed: SlackConversationsListResponse = response
                .body_mut()
                .read_json()
                .context("decoding Slack conversations.list response")?;
            if !listed.ok {
                bail!(
                    "Slack conversations.list failed: {}",
                    listed.error.unwrap_or_else(|| "unknown_error".to_owned())
                );
            }

            conversations.extend(
                listed
                    .channels
                    .into_iter()
                    .filter_map(SlackConversation::from_response),
            );

            cursor = listed
                .response_metadata
                .next_cursor
                .filter(|cursor| !cursor.trim().is_empty());
            if cursor.is_none() {
                break;
            }
        }

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
        let mut request = slack_http_agent()
            .get("https://slack.com/api/conversations.history")
            .header("Authorization", format!("Bearer {token}"))
            .query("channel", &chat_id)
            .query("limit", &limit)
            .query("inclusive", "false");

        if let Some(latest) = latest.as_deref() {
            request = request.query("latest", latest);
        }

        let mut response = request
            .call()
            .context("calling Slack conversations.history")?;
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

        let mut messages = history
            .messages
            .into_iter()
            .filter_map(|message| {
                slack_history_message(
                    account.clone(),
                    current_user_id.as_deref(),
                    chat_id.clone(),
                    message,
                    users.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.timestamp);
        Ok(messages)
    })
    .await
    .context("joining Slack history task")?
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
}

impl SlackProvider {
    pub fn with_options(options: SlackProviderOptions) -> Result<Self> {
        Self::with_api_client(options, Arc::new(SlackWebApiClient::default()))
    }

    pub fn with_api_client(
        options: SlackProviderOptions,
        api_client: Arc<dyn SlackApiClient>,
    ) -> Result<Self> {
        let id = provider_id_for_options(&options);
        let display_name = options
            .workspace
            .as_deref()
            .filter(|workspace| !workspace.trim().is_empty())
            .map(|workspace| format!("Slack ({workspace})"))
            .unwrap_or_else(|| format!("Slack ({})", options.auth_mode.label()));
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

    pub async fn validate_submission(
        &self,
        submission: AuthSubmission,
    ) -> Result<SlackCapabilities> {
        let options = self.options_for_submission(submission)?;
        let validated = Self::validate_options_with_client(&*self.api_client, &options).await;
        match validated {
            Ok(validated) => {
                *write_lock(&self.options) = options;
                let workspace = read_lock(&self.options).workspace.clone();
                if let Some(workspace) = workspace
                    .as_deref()
                    .map(str::trim)
                    .filter(|workspace| !workspace.is_empty())
                {
                    *write_lock(&self.account) = Account {
                        id: self.id.clone(),
                        platform: Platform::Slack,
                        display_name: arc_str(format!("Slack ({workspace})")),
                        avatar: None,
                    };
                }
                *write_lock(&self.capabilities) = validated.capabilities.clone();
                *write_lock(&self.connection) = validated.connection;
                self.connected.store(true, Ordering::Release);
                self.events.send(ProviderEvent::AuthSucceeded);
                self.events.send(ProviderEvent::SyncComplete);
                if validated.capabilities.can_realtime {
                    self.start_realtime();
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
        if submission
            .oauth_code
            .as_deref()
            .is_some_and(|code| !code.trim().is_empty())
        {
            bail!(
                "Slack OAuth code exchange is not implemented yet; paste the resulting user token or use an approved token import"
            )
        }
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
        match (
            options.client_id.as_deref(),
            options.redirect_uri.as_deref(),
        ) {
            (Some(client_id), Some(redirect_uri)) if !client_id.is_empty() => format!(
                "https://slack.com/oauth/v2/authorize?client_id={}&scope={}&user_scope={}&redirect_uri={}",
                url_component(client_id),
                url_component(options.auth_mode.bot_scopes()),
                url_component(options.auth_mode.user_scopes()),
                url_component(redirect_uri)
            ),
            _ => slack_app_manifest_url(&options.auth_mode),
        }
    }

    async fn validate_connection(&self) -> Result<SlackValidatedConnection> {
        let options = self.options();
        Self::validate_options_with_client(&*self.api_client, &options).await
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
                api_client.validate_webhook(&webhook_url).await?;
                capabilities.can_send_webhook = true;
                connection.webhook_url = Some(webhook_url);
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
            .api_client
            .list_conversations(credential.clone())
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
                    match api_client.user_info(credential.clone(), &user_id).await {
                        Ok(user) => {
                            if let Some(user) = user.as_ref() {
                                write_lock(&users).insert(user.id.clone(), user.clone());
                            }
                            user
                        }
                        Err(_) => None,
                    }
                };

                if let Some(user) = user
                    && chat.name.as_ref() != user.best_name()
                {
                    chat.name = arc_str(user.best_name());
                    events.send(ProviderEvent::ChatUpdated(chat));
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

        let user = self
            .api_client
            .user_info(credential.clone(), user_id)
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
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
            .api_client
            .conversation_members(credential.clone(), chat_id)
            .await
            .map_err(|error| anyhow!(sanitize_slack_error(&error)))?;
        write_lock(&self.chat_members).insert(chat_id.to_owned(), members.clone());
        Ok(members)
    }

    fn chat_from_conversation(&self, conversation: SlackConversation) -> Option<Chat> {
        let id = conversation.id.trim();
        if id.is_empty() || conversation.is_archived {
            return None;
        }

        let name = conversation_display_name(&conversation);
        let last_message_at = conversation.updated.and_then(slack_updated_to_timestamp);
        let last_message_preview = conversation_preview(&conversation).map(arc_str);
        let kind = conversation_chat_kind(&conversation);
        let membership = conversation_membership(&conversation);

        Some(Chat {
            id: arc_str(id),
            account: self.id.clone(),
            platform: Platform::Slack,
            name: arc_str(name),
            avatar: None,
            is_group: conversation.is_channel || conversation.is_group || conversation.is_mpim,
            kind,
            membership,
            is_shared: conversation.is_ext_shared,
            unread_count: 0,
            muted: conversation.is_muted,
            pinned: conversation.is_pinned,
            last_message_at,
            last_message_preview,
            thread_id: None,
        })
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
                self.api_client
                    .post_message(
                        SlackCredential::new(SlackCredentialKind::UserToken, token),
                        chat_id.as_ref(),
                        text.as_ref(),
                        reply_to.map(|message_id| message_id.as_ref()),
                    )
                    .await
            }
            SlackSendIdentity::Bot => {
                let token = connection
                    .bot_token
                    .ok_or_else(|| self.unsupported("bot sending"))?;
                self.api_client
                    .post_message(
                        SlackCredential::new(SlackCredentialKind::BotToken, token),
                        chat_id.as_ref(),
                        text.as_ref(),
                        reply_to.map(|message_id| message_id.as_ref()),
                    )
                    .await
            }
            SlackSendIdentity::Webhook => {
                let webhook_url = connection
                    .webhook_url
                    .ok_or_else(|| self.unsupported("webhook sending"))?;
                self.api_client
                    .post_webhook(&webhook_url, text.as_ref())
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
                self.api_client
                    .upload_file(
                        SlackCredential::new(SlackCredentialKind::UserToken, token),
                        request,
                    )
                    .await
            }
            SlackSendIdentity::Bot => {
                let token = connection
                    .bot_token
                    .ok_or_else(|| self.unsupported("bot file upload"))?;
                self.api_client
                    .upload_file(
                        SlackCredential::new(SlackCredentialKind::BotToken, token),
                        request,
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
            return;
        };

        self.stop_realtime();
        let api_client = self.api_client.clone();
        let events = self.events.clone();
        let account = self.id.clone();
        let user_id = read_lock(&self.connection).user_id.clone();
        let users = Arc::clone(&self.users);
        let handle = tokio::spawn(async move {
            run_socket_mode_loop(api_client, events, account, user_id, app_token, users).await;
        });
        *write_lock(&self.realtime_task) = Some(handle);
    }

    fn stop_realtime(&self) {
        if let Some(handle) = write_lock(&self.realtime_task).take() {
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
                "users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,files:read,search:read"
            }
            Self::UserOAuth | Self::ManualApp | Self::ImportedToken => {
                "users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,chat:write,reactions:read,reactions:write,files:read,files:write,search:read"
            }
            Self::BotToken | Self::Webhook => "",
        }
    }

    fn bot_scopes(&self) -> &'static str {
        match self {
            Self::BotToken | Self::ManualApp => {
                "users:read,users.profile:read,channels:read,groups:read,im:read,mpim:read,channels:history,groups:history,im:history,mpim:history,chat:write,reactions:read,reactions:write,files:read,files:write"
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
        if matches!(self.send_identity(), SlackSendIdentity::None) {
            OutboundCapabilities {
                text: false,
                media_note: Some(Arc::from("Slack is not configured for sending")),
                ..OutboundCapabilities::default()
            }
        } else {
            OutboundCapabilities {
                text: true,
                image: true,
                gif: true,
                video: true,
                audio: true,
                file: true,
                sticker: true,
                max_upload_size: None,
                media_note: Some(Arc::from("Slack stickers are sent as file uploads")),
            }
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
                self.connected.store(true, Ordering::Release);
                self.events.send(ProviderEvent::AuthSucceeded);
                self.events.send(ProviderEvent::SyncComplete);
                if capabilities.can_realtime {
                    self.start_realtime();
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
                .api_client
                .history(
                    credential.clone(),
                    &self.id,
                    current_user_id,
                    chat_id,
                    before,
                    limit,
                    Some(Arc::clone(&self.users)),
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

    async fn send(
        &self,
        chat_id: &ChatId,
        content: Content,
        reply_to: Option<&MessageId>,
    ) -> Result<MessageId> {
        match content {
            Content::Text(text) => self.send_text_message(chat_id, text, reply_to).await,
            Content::Image(media)
            | Content::Video(media)
            | Content::Audio(media)
            | Content::File(media)
            | Content::Sticker(media) => self.send_file_message(chat_id, media, reply_to).await,
            Content::LinkPreview(_) => Err(anyhow!(
                "Slack link preview sending should be sent as plain text first"
            )),
            Content::Poll(_) => Err(anyhow!("Slack poll sending is not implemented yet")),
            Content::Deleted => Err(anyhow!("cannot send deleted Slack message content")),
            Content::Unsupported(kind) => {
                Err(anyhow!("cannot send unsupported Slack content: {kind}"))
            }
        }
    }

    async fn download_media(&self, _media: &Media) -> Result<PathBuf> {
        if self.capabilities().can_download_files {
            Err(anyhow!("Slack file download is not implemented yet"))
        } else {
            Err(self.unsupported("file download"))
        }
    }

    async fn mark_read(&self, _chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
        if self.capabilities().can_read_history {
            Ok(())
        } else {
            Err(self.unsupported("read receipts"))
        }
    }

    async fn react(&self, _chat_id: &ChatId, _message: &Message, _emoji: &str) -> Result<()> {
        if self.capabilities().can_react {
            Err(anyhow!(
                "Slack reactions are configured but live API updates are not implemented yet"
            ))
        } else {
            Err(self.unsupported("reactions"))
        }
    }

    async fn submit_auth(&self, submission: AuthSubmission) -> Result<()> {
        self.validate_submission(submission).await.map(|_| ())
    }

    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Message>> {
        if self.capabilities().can_search {
            Ok(Vec::new())
        } else {
            Err(self.unsupported("search"))
        }
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

    async fn chat_members(&self, chat_id: &ChatId) -> Result<Vec<Sender>> {
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
            Ok(members)
        } else {
            Err(self.unsupported("member listing"))
        }
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

    if let Some(name) = conversation.name.as_deref() {
        if conversation.is_mpim {
            return format!("mpdm-{name}");
        }
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

fn conversation_preview(conversation: &SlackConversation) -> Option<String> {
    if let Some(topic) = conversation.topic.as_deref() {
        return Some(topic.to_owned());
    }
    if let Some(purpose) = conversation.purpose.as_deref() {
        return Some(purpose.to_owned());
    }
    if conversation.is_ext_shared {
        return Some("Shared Slack Connect conversation".to_owned());
    }
    conversation
        .num_members
        .map(|members| format!("{members} members"))
}

fn slack_updated_to_timestamp(updated: i64) -> Option<Timestamp> {
    let millis = if updated > 10_000_000_000 {
        updated
    } else {
        updated.saturating_mul(1_000)
    };
    DateTime::<Utc>::from_timestamp_millis(millis)
}

async fn run_socket_mode_loop(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    user_id: Option<String>,
    app_token: String,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
) {
    loop {
        let result = run_socket_mode_once(
            api_client.clone(),
            events.clone(),
            account.clone(),
            user_id.clone(),
            app_token.clone(),
            Arc::clone(&users),
        )
        .await;
        if result.is_err() {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
}

async fn run_socket_mode_once(
    api_client: Arc<dyn SlackApiClient>,
    events: EventBus,
    account: ProviderId,
    user_id: Option<String>,
    app_token: String,
    users: Arc<RwLock<HashMap<String, SlackUser>>>,
) -> Result<()> {
    let connection = api_client
        .open_socket_mode(SlackCredential::new(
            SlackCredentialKind::AppToken,
            app_token,
        ))
        .await?;
    let (mut socket, _) = connect_async(&connection.url)
        .await
        .context("connecting Slack Socket Mode WebSocket")?;

    while let Some(message) = socket.next().await {
        let message = message.context("reading Slack Socket Mode frame")?;
        match message {
            WebSocketMessage::Text(text) => {
                if let Some(ack) = handle_socket_mode_text(
                    &events,
                    &account,
                    user_id.as_deref(),
                    text.as_ref(),
                    &users,
                )? {
                    socket
                        .send(WebSocketMessage::Text(ack.into()))
                        .await
                        .context("acking Slack Socket Mode envelope")?;
                }
            }
            WebSocketMessage::Ping(payload) => {
                socket
                    .send(WebSocketMessage::Pong(payload))
                    .await
                    .context("replying to Slack Socket Mode ping")?;
            }
            WebSocketMessage::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}

fn handle_socket_mode_text(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    text: &str,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
) -> Result<Option<String>> {
    let envelope: SlackSocketEnvelope =
        serde_json::from_str(text).context("decoding Slack Socket Mode envelope")?;
    let ack = envelope
        .envelope_id
        .as_deref()
        .map(|id| format!(r#"{{"envelope_id":"{}"}}"#, json_escape(id)));

    if envelope.envelope_type.as_deref() == Some("events_api")
        && envelope
            .payload
            .as_ref()
            .and_then(|payload| payload.event_type.as_deref())
            == Some("event_callback")
        && let Some(event) = envelope.payload.and_then(|payload| payload.event)
    {
        emit_realtime_event(events, account, current_user_id, event, users);
    }

    Ok(ack)
}

fn emit_realtime_event(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    event: SlackRealtimeEvent,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
) {
    match event.event_type.as_str() {
        "message" => emit_realtime_message(events, account, current_user_id, event, users),
        "reaction_added" | "reaction_removed" => emit_realtime_reaction(events, event),
        _ => {}
    }
}

fn emit_realtime_message(
    events: &EventBus,
    account: &ProviderId,
    current_user_id: Option<&str>,
    event: SlackRealtimeEvent,
    users: &Arc<RwLock<HashMap<String, SlackUser>>>,
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
            slack_message_from_parts(
                account,
                current_user_id,
                message.channel.or(event.channel),
                message.user,
                message.bot_id,
                message.ts,
                message.thread_ts,
                message.text,
                Some(users),
            )
        }) {
            events.send(ProviderEvent::MessageEdited { message });
        }
        return;
    }

    if subtype.is_some() {
        return;
    }

    if let Some(message) = slack_message_from_parts(
        account,
        current_user_id,
        event.channel,
        event.user,
        event.bot_id,
        event.ts.or(event.event_ts),
        event.thread_ts,
        event.text,
        Some(users),
    ) {
        events.send(ProviderEvent::Message {
            message,
            is_historical: false,
        });
    }
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
        emoji: arc_str(emoji),
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
) -> Option<Message> {
    if message.hidden.unwrap_or(false)
        || message
            .message_type
            .as_deref()
            .is_some_and(|kind| kind != "message")
        || matches!(
            message.subtype.as_deref(),
            Some("message_deleted" | "message_changed" | "channel_join" | "channel_leave")
        )
    {
        return None;
    }

    slack_message_from_parts(
        &account,
        current_user_id,
        Some(channel),
        message.user,
        message.bot_id,
        message.ts,
        message.thread_ts,
        message.text,
        users,
    )
}

fn slack_message_from_parts(
    account: &ProviderId,
    current_user_id: Option<&str>,
    channel: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
    text: Option<String>,
    users: Option<&Arc<RwLock<HashMap<String, SlackUser>>>>,
) -> Option<Message> {
    let channel = non_empty_option(&channel)?;
    let ts = non_empty_option(&ts)?;
    let sender_id = non_empty_option(&user)
        .or_else(|| non_empty_option(&bot_id))
        .unwrap_or_else(|| "slack".to_owned());
    let text = text.unwrap_or_default();
    let timestamp = slack_ts_to_timestamp(&ts).unwrap_or_else(Utc::now);
    let thread_id = non_empty_option(&thread_ts).filter(|thread_ts| thread_ts != &ts);
    let mut sender = Sender {
        platform_id: arc_str(&sender_id),
        display_name: arc_str(&sender_id),
        avatar: None,
    };
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
        content: Content::Text(arc_str(text)),
        reply_to: thread_id.as_deref().map(arc_str),
        thread_id: thread_id.map(arc_str),
        reactions: Vec::new(),
        receipts: Vec::new(),
        is_from_me: current_user_id.is_some_and(|current_user_id| current_user_id == sender_id),
        platform_data: PlatformData {
            slack: Some(SlackData {
                ts: arc_str(&ts),
                thread_ts: thread_ts.map(arc_str),
                channel: arc_str(channel),
            }),
            ..PlatformData::default()
        },
    })
}

fn unresolved_slack_user_ids(messages: &[Message]) -> Vec<String> {
    let mut user_ids = Vec::new();
    for message in messages {
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
}

fn is_slack_user_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('U' | 'W'))
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
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(SLACK_HTTP_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
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
    let manifest = format!(
        "_metadata:\n  major_version: 2\n  minor_version: 1\ndisplay_information:\n  name: {}\n  description: Terminal chat client Slack integration\n{}oauth_config:\n  scopes:\n{}{}settings:\n  org_deploy_enabled: false\n  socket_mode_enabled: true\n  token_rotation_enabled: false\n",
        manifest_yaml_string("chat-cli"),
        bot_user_section,
        user_scope_section,
        bot_scope_section
    );
    format!(
        "https://api.slack.com/apps?new_app=1&manifest_yaml={}",
        url_component(&manifest)
    )
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
        posted_messages: Mutex<Vec<PostedCall>>,
        posted_webhooks: Mutex<Vec<(String, String)>>,
        listed_conversations: Mutex<Vec<SlackCredentialKind>>,
        conversations: Mutex<Vec<SlackConversation>>,
        users: Mutex<HashMap<String, SlackUser>>,
        conversation_members: Mutex<HashMap<String, Vec<String>>>,
        history_calls: Mutex<Vec<(SlackCredentialKind, ChatId, Option<Timestamp>, usize)>>,
        history_messages: Mutex<Vec<Message>>,
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
            if !app_token.value().starts_with("xapp-") {
                bail!("invalid app token")
            }
            Ok(SlackSocketModeConnection {
                url: "ws://127.0.0.1:9/socket-mode-test".to_owned(),
            })
        }
    }

    fn provider_with_fake_client(
        options: SlackProviderOptions,
        client: Arc<FakeSlackApiClient>,
    ) -> Result<SlackProvider> {
        SlackProvider::with_api_client(options, client)
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
            updated: Some(updated),
            topic: None,
            purpose: None,
            num_members: None,
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
            updated: Some(updated),
            topic: None,
            purpose: None,
            num_members: None,
        }
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
        assert!(matches!(events.recv().await?, ProviderEvent::AuthSucceeded));
        assert!(matches!(events.recv().await?, ProviderEvent::SyncComplete));
        Ok(())
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
            events.recv().await?,
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
            events.recv().await?,
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
        assert!(matches!(events.recv().await?, ProviderEvent::AuthSucceeded));
        assert!(matches!(events.recv().await?, ProviderEvent::SyncComplete));
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
            events.recv().await?,
            ProviderEvent::Disconnected(Some(reason))
                if reason.contains("<redacted>") && !reason.contains("xoxp-invalid-submitted-secret")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn submit_auth_oauth_code_reports_unimplemented_without_storing_code() -> Result<()> {
        let provider = provider_with_fake_client(
            SlackProviderOptions::new(SlackAuthMode::UserOAuth),
            Arc::new(FakeSlackApiClient::default()),
        )?;

        let error = provider
            .submit_auth(AuthSubmission {
                mode: Some(AuthSubmissionMode::UserOAuth),
                oauth_code: Some("temporary-code".to_owned()),
                ..AuthSubmission::default()
            })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("OAuth code exchange is not implemented yet"));
        assert!(!error.contains("temporary-code"));
        assert!(!provider.is_connected());
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
                Content::Text(arc_str("hello from user")),
                Some(&arc_str("1710000000.000001")),
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
                Content::Text(arc_str("hello from bot")),
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
    async fn webhook_mode_posts_text_to_webhook_only() -> Result<()> {
        let mut options = SlackProviderOptions::new(SlackAuthMode::Webhook);
        options.webhook_url = Some("https://hooks.slack.com/services/T000/B000/secret".to_owned());
        let client = Arc::new(FakeSlackApiClient::default());
        let provider = provider_with_fake_client(options, client.clone())?;
        provider.connect().await?;

        let message_id = provider
            .send(
                &arc_str("ignored-channel"),
                Content::Text(arc_str("hello webhook")),
                Some(&arc_str("ignored-thread")),
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
            .send(&arc_str("C123"), Content::Text(arc_str("blocked")), None)
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
        assert_eq!(
            chats[1].last_message_preview.as_deref(),
            Some("Company announcements")
        );
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
                Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("report.pdf"),
                    mime_type: arc_str("application/pdf"),
                    ..Media::default()
                }),
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
                Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("fono-snixembed.log"),
                    mime_type: arc_str("text/plain"),
                    caption: Some(arc_str("log file")),
                    local_path: Some(PathBuf::from("/tmp/fono-snixembed.log")),
                    ..Media::default()
                }),
                Some(&arc_str("1710000000.000001")),
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
                Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("report.pdf"),
                    mime_type: arc_str("application/pdf"),
                    local_path: Some(PathBuf::from("/tmp/report.pdf")),
                    ..Media::default()
                }),
                None,
            )
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("Slack file upload is not available"));
        assert!(error.contains("webhook"));
        Ok(())
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
