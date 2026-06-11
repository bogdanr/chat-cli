use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use uuid::Uuid;

pub type ProviderId = Arc<str>;
pub type ChatId = Arc<str>;
pub type MessageId = Arc<str>;
pub type ThreadId = Arc<str>;
pub type PersonId = Uuid;
pub type PlatformId = Arc<str>;
pub type Timestamp = DateTime<Utc>;

/// Media at or below this size may be downloaded automatically by providers
/// when a message referencing it is rendered. Larger media must be fetched on
/// demand (for example via [`crate::Provider::download_media`]) after an
/// explicit user action, such as activating a "Retrieve media" card.
pub const MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

/// How the authenticated user relates to a conversation thread. Computed
/// locally from stored messages: the user is a participant when they authored
/// the thread root, replied inside the thread, or were mentioned by any
/// message in it. Ordered by strength so the strongest applicable
/// classification wins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ThreadParticipation {
    /// The user authored the thread's root message.
    Author,
    /// The user wrote at least one reply in the thread.
    Replied,
    /// The user was @-mentioned (or broadcast-pinged) in the thread.
    Mentioned,
    /// The thread happens around the user without involving them.
    #[default]
    None,
}

impl ThreadParticipation {
    /// Whether the user is part of the thread in any capacity.
    pub fn is_participant(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Short human-readable label for UI badges; empty for `None`.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Author => "you started",
            Self::Replied => "you replied",
            Self::Mentioned => "mentioned you",
            Self::None => "",
        }
    }
}

/// Aggregated view of a single conversation thread, used to surface thread
/// activity (reply counts, unread replies, participants) in the sidebar, the
/// timeline summary line, and the dedicated Threads inbox view.
///
/// A thread is identified by its [`ThreadId`] (the root message id / Slack
/// `thread_ts`). The `root_id` is the id of the thread's first message; replies
/// are all messages sharing the `thread_id` whose id differs from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSummary {
    pub account: ProviderId,
    pub chat_id: ChatId,
    pub thread_id: ThreadId,
    pub root_id: MessageId,
    /// Preview text of the thread's root message, when available.
    pub root_preview: Option<Arc<str>>,
    /// Number of replies in the thread (excludes the root message).
    pub reply_count: u32,
    /// Replies the user has not yet seen (excludes the user's own replies).
    pub unread_reply_count: u32,
    /// Timestamp of the most recent reply, when the thread has any.
    pub last_reply_at: Option<Timestamp>,
    /// Distinct display names of reply authors, ordered by first appearance.
    pub participants: Vec<Arc<str>>,
    /// How the authenticated user relates to this thread.
    pub participation: ThreadParticipation,
}

impl ThreadSummary {
    /// Whether the thread has replies the user has not yet seen.
    pub fn has_unread(&self) -> bool {
        self.unread_reply_count > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Platform {
    WhatsApp,
    Slack,
    Discord,
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: ProviderId,
    pub platform: Platform,
    pub display_name: Arc<str>,
    pub avatar: Option<PathBuf>,
}

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ChatKind {
    #[default]
    Direct,
    Group,
    PublicChannel,
    PrivateChannel,
    GroupDirectMessage,
}

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum ChatMembership {
    #[default]
    Joined,
    NotJoined,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Chat {
    pub id: ChatId,
    pub account: ProviderId,
    pub platform: Platform,
    pub name: Arc<str>,
    pub avatar: Option<PathBuf>,
    pub is_group: bool,
    pub kind: ChatKind,
    pub membership: ChatMembership,
    pub is_shared: bool,
    pub unread_count: u32,
    pub muted: bool,
    pub pinned: bool,
    pub last_message_at: Option<Timestamp>,
    pub last_message_preview: Option<Arc<str>>,
    pub thread_id: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryResult {
    pub account: ProviderId,
    pub platform: Platform,
    pub kind: DiscoveryResultKind,
    pub action: DiscoveryAction,
    pub id: Arc<str>,
    pub platform_id: PlatformId,
    pub chat_id: Option<ChatId>,
    pub label: Arc<str>,
    pub subtitle: Option<Arc<str>>,
    pub avatar: Option<PathBuf>,
    pub chat_kind: Option<ChatKind>,
    pub membership: ChatMembership,
    pub metadata: BTreeMap<Arc<str>, Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DiscoveryResultKind {
    ExistingChat,
    Contact,
    User,
    DirectMessage,
    PublicChannel,
    PrivateChannel,
    Group,
    ManualDestination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DiscoveryAction {
    Open,
    CreateChat,
    OpenDm,
    JoinRequired,
    Unsupported,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryCapabilities {
    pub existing_chats: bool,
    pub contacts: bool,
    pub users: bool,
    pub public_channels: bool,
    pub private_channels: bool,
    pub open_dm: bool,
    pub join_public_channel: bool,
}

impl DiscoveryResult {
    pub fn existing_chat(chat: Chat) -> Self {
        let mut metadata = BTreeMap::new();
        if let Some(preview) = chat.last_message_preview.clone() {
            metadata.insert(Arc::from("preview"), preview);
        }
        Self {
            account: chat.account.clone(),
            platform: chat.platform.clone(),
            kind: discovery_kind_for_chat(chat.kind),
            action: DiscoveryAction::Open,
            id: Arc::from(format!("chat:{}:{}", chat.account, chat.id)),
            platform_id: chat.id.clone(),
            chat_id: Some(chat.id.clone()),
            label: chat.name.clone(),
            subtitle: chat.last_message_preview.clone(),
            avatar: chat.avatar.clone(),
            chat_kind: Some(chat.kind),
            membership: chat.membership,
            metadata,
        }
    }
}

fn discovery_kind_for_chat(kind: ChatKind) -> DiscoveryResultKind {
    match kind {
        ChatKind::Direct => DiscoveryResultKind::ExistingChat,
        ChatKind::Group => DiscoveryResultKind::Group,
        ChatKind::PublicChannel => DiscoveryResultKind::PublicChannel,
        ChatKind::PrivateChannel => DiscoveryResultKind::PrivateChannel,
        ChatKind::GroupDirectMessage => DiscoveryResultKind::DirectMessage,
    }
}
#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub chat_id: ChatId,
    pub account: ProviderId,
    pub sender: Sender,
    pub timestamp: Timestamp,
    pub edited_at: Option<Timestamp>,
    pub content: Content,
    pub reply_to: Option<MessageId>,
    pub thread_id: Option<Arc<str>>,
    pub reactions: Vec<Reaction>,
    pub receipts: Vec<Receipt>,
    pub is_from_me: bool,
    /// True when this message mentions the authenticated user (an explicit
    /// @-mention of the user, or a provider broadcast ping such as Slack's
    /// `@here`/`@channel`/`@everyone`). Computed by each provider because only
    /// the provider knows the account's authenticated identity. Used by the
    /// notification scope filter to decide whether group/channel messages are
    /// notification-eligible.
    pub mentions_me: bool,
    pub platform_data: PlatformData,
}

#[derive(Clone, Debug)]
pub struct Sender {
    pub platform_id: PlatformId,
    pub display_name: Arc<str>,
    pub avatar: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub enum Content {
    Text(Arc<str>),
    Image(Media),
    Video(Media),
    Audio(Media),
    File(Media),
    Sticker(Media),
    LinkPreview(LinkPreview),
    Cards(Vec<Card>),
    Poll(Poll),
    Deleted,
    Unsupported(Arc<str>),
}

#[derive(Clone, Debug)]
pub struct Card {
    pub kind: CardKind,
    pub source: CardSource,
    pub title: Option<Arc<str>>,
    pub subtitle: Option<Arc<str>>,
    pub body: Option<Arc<str>>,
    pub footer: Option<Arc<str>>,
    pub url: Option<Arc<str>>,
    pub accent_color: Option<CardColor>,
    pub thumbnail: Option<Media>,
    pub image: Option<Media>,
    pub fields: Vec<CardField>,
    pub actions: Vec<CardAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CardKind {
    LinkPreview,
    ProviderAttachment,
    BotMessage,
    MediaPreview,
    SocialPreview,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardSource {
    Slack,
    WhatsApp,
    OpenGraph,
    YouTube,
    Instagram,
    Facebook,
    GenericUrl,
    Unknown(Arc<str>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CardColor {
    Named(Arc<str>),
    Hex(Arc<str>),
}

#[derive(Clone, Debug)]
pub struct CardField {
    pub title: Option<Arc<str>>,
    pub value: Arc<str>,
    pub short: bool,
}

#[derive(Clone, Debug)]
pub struct CardAction {
    pub label: Arc<str>,
    pub url: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub struct Poll {
    pub question: Arc<str>,
    pub options: Vec<PollOption>,
    pub selectable_options_count: Option<u32>,
    pub votes: Vec<PollVote>,
}

#[derive(Clone, Debug)]
pub struct PollOption {
    pub id: Arc<str>,
    pub label: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct PollVote {
    pub sender: PlatformId,
    pub options: Vec<Arc<str>>,
    pub timestamp: Option<Timestamp>,
}

#[derive(Clone, Debug, Default)]
pub struct Media {
    pub id: Arc<str>,
    pub file_name: Arc<str>,
    pub mime_type: Arc<str>,
    pub size_bytes: Option<u64>,
    pub caption: Option<Arc<str>>,
    pub local_path: Option<PathBuf>,
    pub thumbnail: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct LinkPreview {
    pub url: Arc<str>,
    pub title: Option<Arc<str>>,
    pub description: Option<Arc<str>>,
    pub image: Option<Media>,
}

#[derive(Clone, Debug)]
pub struct Reaction {
    pub emoji: Arc<str>,
    pub senders: Vec<PlatformId>,
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub platform_id: PlatformId,
    pub kind: ReceiptKind,
    pub at: Option<Timestamp>,
}

#[derive(Clone, Debug)]
pub enum ReceiptKind {
    Delivered,
    Read,
}

#[derive(Clone, Debug, Default)]
pub struct PlatformData {
    pub whatsapp: Option<WhatsAppData>,
    pub slack: Option<SlackData>,
    pub cards: Vec<Card>,
}

#[derive(Clone, Debug)]
pub struct WhatsAppData {
    pub jid: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct SlackData {
    pub ts: Arc<str>,
    pub thread_ts: Option<Arc<str>>,
    pub channel: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct Person {
    pub id: PersonId,
    pub display_name: Arc<str>,
    pub avatar: Option<PathBuf>,
    pub handles: Vec<Handle>,
}

#[derive(Clone, Debug)]
pub struct Handle {
    pub platform: Platform,
    pub account: ProviderId,
    pub platform_id: PlatformId,
    pub display_name: Arc<str>,
}
