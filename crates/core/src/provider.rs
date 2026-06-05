use crate::{events::ProviderEvent, types::*};
use anyhow::bail;
use async_trait::async_trait;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::broadcast;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthSubmissionMode {
    UserOAuth,
    ReadOnlyOAuth,
    BotToken,
    ImportedToken,
    ManualApp,
    Webhook,
    ProviderSpecific(Arc<str>),
}

impl AuthSubmissionMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UserOAuth => "user-oauth",
            Self::ReadOnlyOAuth => "read-only-oauth",
            Self::BotToken => "bot-token",
            Self::ImportedToken => "imported-token",
            Self::ManualApp => "manual-app",
            Self::Webhook => "webhook",
            Self::ProviderSpecific(value) => value,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AuthSubmission {
    pub workspace_label: Option<String>,
    pub mode: Option<AuthSubmissionMode>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub oauth_code: Option<String>,
    pub user_token: Option<String>,
    pub bot_token: Option<String>,
    pub app_token: Option<String>,
    pub webhook_url: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboundCapabilities {
    pub text: bool,
    pub image: bool,
    pub gif: bool,
    pub video: bool,
    pub audio: bool,
    pub file: bool,
    pub sticker: bool,
    pub max_upload_size: Option<u64>,
    pub media_note: Option<Arc<str>>,
}

impl Default for OutboundCapabilities {
    fn default() -> Self {
        Self {
            text: true,
            image: false,
            gif: false,
            video: false,
            audio: false,
            file: false,
            sticker: false,
            max_upload_size: None,
            media_note: None,
        }
    }
}

impl OutboundCapabilities {
    pub fn all() -> Self {
        Self {
            text: true,
            image: true,
            gif: true,
            video: true,
            audio: true,
            file: true,
            sticker: true,
            max_upload_size: None,
            media_note: None,
        }
    }

    pub fn text_only(note: impl Into<Arc<str>>) -> Self {
        Self {
            media_note: Some(note.into()),
            ..Self::default()
        }
    }

    pub fn supports_content(&self, content: &Content) -> bool {
        match content {
            Content::Text(_) => self.text,
            Content::Image(media) if media.mime_type.as_ref() == "image/gif" => self.gif || self.image,
            Content::Image(_) => self.image,
            Content::Video(_) => self.video,
            Content::Audio(_) => self.audio,
            Content::File(_) => self.file,
            Content::Sticker(_) => self.sticker,
            Content::LinkPreview(_) => self.text,
            Content::Poll(_) | Content::Deleted | Content::Unsupported(_) => false,
        }
    }

    pub fn unsupported_reason(&self, content: &Content) -> Option<String> {
        if self.supports_content(content) {
            return None;
        }
        let kind = outbound_content_label(content);
        let note = self
            .media_note
            .as_deref()
            .unwrap_or("this provider does not support that outbound content yet");
        Some(format!("{kind} sending is not available: {note}"))
    }
}

fn outbound_content_label(content: &Content) -> &'static str {
    match content {
        Content::Text(_) => "text",
        Content::Image(media) if media.mime_type.as_ref() == "image/gif" => "GIF",
        Content::Image(_) => "image",
        Content::Video(_) => "video",
        Content::Audio(_) => "audio",
        Content::File(_) => "file",
        Content::Sticker(_) => "sticker",
        Content::LinkPreview(_) => "link preview",
        Content::Poll(_) => "poll",
        Content::Deleted => "deleted message",
        Content::Unsupported(_) => "unsupported content",
    }
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Stable identifier for this account instance (e.g. "whatsapp:+1234567890").
    fn id(&self) -> &ProviderId;

    fn platform(&self) -> Platform;

    fn account_info(&self) -> Account;

    fn outbound_capabilities(&self) -> OutboundCapabilities {
        OutboundCapabilities::default()
    }

    /// Connect/authenticate. Emits an auth event if credentials are needed.
    async fn connect(&self) -> anyhow::Result<()>;

    async fn disconnect(&self) -> anyhow::Result<()>;

    fn is_connected(&self) -> bool;

    /// Subscribe to real-time events from this provider.
    fn events(&self) -> broadcast::Receiver<ProviderEvent>;

    /// Enumerate all chats. May return cached data immediately.
    async fn chats(&self) -> anyhow::Result<Vec<Chat>>;

    /// Load message history. `before` enables pagination.
    async fn history(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> anyhow::Result<Vec<Message>>;

    /// Send a message. Returns the sent message ID.
    async fn send(
        &self,
        chat_id: &ChatId,
        content: Content,
        reply_to: Option<&MessageId>,
    ) -> anyhow::Result<MessageId>;

    /// Download media to a local cache path. Returns path to the file.
    async fn download_media(&self, media: &Media) -> anyhow::Result<PathBuf>;

    /// Mark messages as read.
    async fn mark_read(&self, chat_id: &ChatId, up_to: &MessageId) -> anyhow::Result<()>;

    /// Add a reaction emoji.
    async fn react(&self, chat_id: &ChatId, message: &Message, emoji: &str) -> anyhow::Result<()>;

    /// Vote in a poll message.
    async fn vote_poll(
        &self,
        _chat_id: &ChatId,
        _message: &Message,
        _selected_options: &[Arc<str>],
    ) -> anyhow::Result<()> {
        bail!("poll voting is not supported by this provider")
    }

    /// Submit interactive authentication/setup inputs. Providers that support
    /// runtime setup should validate the submission and emit auth/status events.
    async fn submit_auth(&self, _submission: AuthSubmission) -> anyhow::Result<()> {
        bail!("interactive authentication setup is not supported by this provider")
    }

    /// Search messages across this provider.
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Message>>;

    /// Get contact/user info for a platform ID.
    async fn contact_info(&self, platform_id: &PlatformId) -> anyhow::Result<Option<Sender>>;
}
