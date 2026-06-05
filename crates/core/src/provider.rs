use crate::{events::ProviderEvent, types::*};
use async_trait::async_trait;
use std::path::PathBuf;
use tokio::sync::broadcast;

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Stable identifier for this account instance (e.g. "whatsapp:+1234567890").
    fn id(&self) -> &ProviderId;

    fn platform(&self) -> Platform;

    fn account_info(&self) -> Account;

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
    async fn react(
        &self,
        chat_id: &ChatId,
        message_id: &MessageId,
        emoji: &str,
    ) -> anyhow::Result<()>;

    /// Search messages across this provider.
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Message>>;

    /// Get contact/user info for a platform ID.
    async fn contact_info(&self, platform_id: &PlatformId) -> anyhow::Result<Option<Sender>>;
}
