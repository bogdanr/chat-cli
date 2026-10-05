use crate::{
    Account, Card, CardAction, CardColor, CardField, CardKind, CardSource, Chat, ChatDetails,
    ChatId, ChatKind, ChatMember, ChatMemberRole, ChatMembership, ContactProfile, Content,
    EventBus, LinkPreview, Media, Mention, Message, MessageId, OutboundContent, OutboundMentions,
    Platform, PlatformData, PlatformId, Poll, PollOption, PollVote, Provider, ProviderEvent,
    ProviderId, Reaction, Receipt, ReceiptKind, Sender, Timestamp, resolve_mention_tokens,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{Duration, Local, Utc};
use image::{ImageBuffer, ImageFormat, Rgba, RgbaImage};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::broadcast;

#[derive(Clone, Debug)]
pub struct MockProvider {
    id: ProviderId,
    account: Account,
    chats: Arc<Vec<Chat>>,
    messages: Arc<RwLock<Vec<Message>>>,
    sent_mentions: Arc<RwLock<Vec<Vec<Mention>>>>,
    events: EventBus,
    /// Optional edit window, to exercise providers like WhatsApp that only
    /// allow editing recent messages.
    edit_window: Option<chrono::Duration>,
}

impl MockProvider {
    pub fn new() -> Self {
        let id = arc_str("mock:local");
        let account = Account {
            id: id.clone(),
            platform: Platform::Unknown("mock".to_owned()),
            display_name: arc_str("Mock Account"),
            avatar: avatar_path("me"),
        };
        let now = mock_seed_now();
        let chats = Arc::new(mock_chats(&id, now));
        let messages = Arc::new(RwLock::new(mock_messages(&id, now)));

        Self {
            id,
            account,
            chats,
            messages,
            sent_mentions: Arc::new(RwLock::new(Vec::new())),
            events: EventBus::new(),
            edit_window: None,
        }
    }

    /// Limits edits to messages sent within `window`, like WhatsApp.
    pub fn with_edit_window(mut self, window: chrono::Duration) -> Self {
        self.edit_window = Some(window);
        self
    }

    pub fn seed_chats(&self) -> Vec<Chat> {
        self.chats.as_ref().clone()
    }

    pub fn seed_messages(&self) -> Vec<Message> {
        self.read_messages().clone()
    }

    /// Mentions attached to each message sent (or edited) through this
    /// provider, in call order. Used by tests and the `--mock-provider` demo to
    /// exercise the outbound mention path.
    pub fn sent_mentions(&self) -> Vec<Vec<Mention>> {
        self.sent_mentions
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn ensure_mock_assets(&self) -> Result<()> {
        ensure_mock_assets()
    }

    fn read_messages(&self) -> std::sync::RwLockReadGuard<'_, Vec<Message>> {
        self.messages
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_messages(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Message>> {
        self.messages
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn platform(&self) -> Platform {
        self.account.platform.clone()
    }

    fn account_info(&self) -> Account {
        self.account.clone()
    }

    fn outbound_capabilities(&self) -> crate::OutboundCapabilities {
        crate::OutboundCapabilities {
            edit_window: self.edit_window,
            ..crate::OutboundCapabilities::all()
        }
    }

    fn encode_outbound_mentions(
        &self,
        text: &str,
        members: &[ChatMember],
        picks: &[Mention],
    ) -> OutboundMentions {
        let resolved = resolve_mention_tokens(text, members, picks);
        OutboundMentions {
            text: text.to_owned(),
            mentioned: resolved.into_iter().map(|item| item.mention).collect(),
        }
    }

    async fn connect(&self) -> Result<()> {
        self.ensure_mock_assets()?;
        self.events.send(ProviderEvent::AuthSucceeded);
        self.events.send(ProviderEvent::SyncComplete);
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.events.send(ProviderEvent::Disconnected(None));
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn events(&self) -> broadcast::Receiver<ProviderEvent> {
        self.events.subscribe()
    }

    async fn chats(&self) -> Result<Vec<Chat>> {
        Ok(self.seed_chats())
    }

    async fn history(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let all_messages = self.read_messages();
        let mut messages = all_messages
            .iter()
            .filter(|message| message.chat_id == *chat_id)
            .filter(|message| before.is_none_or(|before| message.timestamp < before))
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.timestamp);
        let start = messages.len().saturating_sub(limit);
        Ok(messages.split_off(start))
    }

    async fn send(
        &self,
        chat_id: &ChatId,
        outbound: OutboundContent,
        reply_to: Option<&Message>,
    ) -> Result<MessageId> {
        let OutboundContent { content, mentions } = outbound;
        self.sent_mentions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(mentions);
        // Sequence suffix keeps IDs unique when several items (an album) are
        // sent within the same millisecond, as real providers' IDs are.
        static SEND_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let id = arc_str(format!(
            "mock:sent:{}:{sequence}",
            Utc::now().timestamp_millis()
        ));
        let message = Message {
            id: id.clone(),
            chat_id: chat_id.clone(),
            account: self.id.clone(),
            sender: Sender {
                platform_id: arc_str("me"),
                display_name: arc_str("Me"),
                avatar: avatar_path("me"),
            },
            timestamp: Utc::now(),
            edited_at: None,
            content,
            reply_to: reply_to.map(|message| message.id.clone()),
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            mentions_me: false,
            platform_data: PlatformData::default(),
        };
        self.write_messages().push(message.clone());
        self.events.send(ProviderEvent::Message {
            message,
            is_historical: false,
        });
        Ok(id)
    }

    async fn download_media(&self, media: &Media) -> Result<PathBuf> {
        if let Some(path) = &media.local_path {
            Ok(path.clone())
        } else {
            bail!("mock media is not available locally: {}", media.file_name)
        }
    }

    async fn mark_read(&self, _chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
        Ok(())
    }

    async fn react(&self, chat_id: &ChatId, message: &Message, emoji: &str) -> Result<()> {
        let message_id = message.id.clone();
        let sender = arc_str("me");
        let mut added = false;
        let mut updated = false;
        {
            let mut messages = self.write_messages();
            if let Some(message) = messages
                .iter_mut()
                .find(|candidate| candidate.chat_id == *chat_id && candidate.id == message_id)
            {
                added = toggle_mock_reaction(message, emoji, sender.clone());
                updated = true;
            }
        }

        if updated {
            self.events.send(ProviderEvent::ReactionChanged {
                chat_id: chat_id.clone(),
                message_id: message_id.clone(),
                emoji: arc_str(emoji),
                added,
                sender,
            });
            Ok(())
        } else {
            bail!("mock message not found for reaction: {message_id}")
        }
    }

    async fn vote_poll(
        &self,
        chat_id: &ChatId,
        message: &Message,
        selected_options: &[Arc<str>],
    ) -> Result<()> {
        let message_id = message.id.clone();
        let mut changed = None;
        {
            let mut messages = self.write_messages();
            if let Some(message) = messages
                .iter_mut()
                .find(|candidate| candidate.chat_id == *chat_id && candidate.id == message_id)
            {
                let Content::Poll(poll) = &mut message.content else {
                    bail!("mock message is not a poll: {message_id}")
                };
                poll.votes.retain(|vote| vote.sender.as_ref() != "me");
                poll.votes.push(crate::PollVote {
                    sender: arc_str("me"),
                    options: selected_options.to_vec(),
                    timestamp: Some(Utc::now()),
                });
                changed = Some(message.clone());
            }
        }

        if let Some(message) = changed {
            self.events.send(ProviderEvent::MessageEdited { message });
            Ok(())
        } else {
            bail!("mock message not found for poll vote: {message_id}")
        }
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        message: &Message,
        outbound: OutboundContent,
    ) -> Result<Timestamp> {
        let OutboundContent { content, mentions } = outbound;
        if !matches!(content, Content::Text(_)) {
            bail!("mock provider can only edit text messages");
        }
        let message_id = message.id.clone();
        let edited_at = Utc::now();
        let found = {
            let mut messages = self.write_messages();
            match messages
                .iter_mut()
                .find(|candidate| candidate.chat_id == *chat_id && candidate.id == message_id)
            {
                Some(stored) if !stored.is_from_me => {
                    bail!("mock provider can only edit your own messages")
                }
                Some(stored) => {
                    stored.content = content.clone();
                    stored.edited_at = Some(edited_at);
                    true
                }
                None => false,
            }
        };
        if !found {
            bail!("mock message not found for edit: {message_id}");
        }
        self.sent_mentions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(mentions);
        self.events.send(ProviderEvent::MessageContentEdited {
            chat_id: chat_id.clone(),
            message_id,
            content,
            edited_at,
        });
        Ok(edited_at)
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let query = query.to_lowercase();
        let messages = self.read_messages();
        Ok(messages
            .iter()
            .filter(|message| {
                content_text(&message.content)
                    .to_lowercase()
                    .contains(&query)
            })
            .take(limit)
            .cloned()
            .collect())
    }

    async fn contact_info(&self, platform_id: &PlatformId) -> Result<Option<Sender>> {
        let messages = self.read_messages();
        Ok(messages
            .iter()
            .find(|message| message.sender.platform_id == *platform_id)
            .map(|message| message.sender.clone()))
    }

    async fn chat_members(&self, chat_id: &ChatId) -> Result<Vec<ChatMember>> {
        Ok(mock_chat_members(chat_id.as_ref()))
    }

    async fn chat_details(&self, chat_id: &ChatId) -> Result<ChatDetails> {
        Ok(mock_chat_details(chat_id.as_ref()))
    }

    async fn contact_profile(&self, platform_id: &PlatformId) -> Result<Option<ContactProfile>> {
        if let Some(profile) = mock_contact_profile(platform_id.as_ref()) {
            return Ok(Some(profile));
        }
        Ok(self
            .contact_info(platform_id)
            .await?
            .map(|sender| ContactProfile {
                display_name: Some(sender.display_name),
                ..ContactProfile::default()
            }))
    }
}

struct ChatSeed<'a> {
    id: &'a str,
    platform: Platform,
    name: &'a str,
    avatar: &'a str,
    kind: ChatKind,
    unread_count: u32,
    muted: bool,
    pinned: bool,
    last_seen_minutes_ago: i64,
    preview: &'a str,
}

struct TextMessageSeed<'a> {
    chat_id: &'a str,
    id: &'a str,
    sender_id: &'a str,
    sender_name: &'a str,
    timestamp: Timestamp,
    text: &'a str,
    is_from_me: bool,
    reactions: Vec<Reaction>,
    receipts: Vec<Receipt>,
}

impl Default for TextMessageSeed<'_> {
    fn default() -> Self {
        Self {
            chat_id: "",
            id: "",
            sender_id: "",
            sender_name: "",
            timestamp: Utc::now(),
            text: "",
            is_from_me: false,
            reactions: Vec::new(),
            receipts: Vec::new(),
        }
    }
}

struct MediaMessageSeed<'a> {
    chat_id: &'a str,
    id: &'a str,
    sender_id: &'a str,
    sender_name: &'a str,
    timestamp: Timestamp,
    content: Content,
    is_from_me: bool,
    reactions: Vec<Reaction>,
    receipts: Vec<Receipt>,
}

fn mock_seed_now() -> Timestamp {
    Local::now()
        .date_naive()
        .and_hms_opt(12, 0, 0)
        .and_then(|noon| noon.and_local_timezone(Local).single())
        .map(|noon| noon.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

fn mock_chats(account: &ProviderId, now: Timestamp) -> Vec<Chat> {
    let mut chats = [
        ChatSeed {
            id: "mock:chat:family",
            platform: Platform::WhatsApp,
            name: "Family Weekend 🏡",
            avatar: "family-weekend",
            kind: ChatKind::Group,
            unread_count: 4,
            muted: false,
            pinned: true,
            last_seen_minutes_ago: 2,
            preview: "Maya: Picnic photos are up 📸",
        },
        ChatSeed {
            id: "mock:chat:alice",
            platform: Platform::WhatsApp,
            name: "Alice Chen",
            avatar: "alice",
            kind: ChatKind::Direct,
            unread_count: 2,
            muted: false,
            pinned: true,
            last_seen_minutes_ago: 5,
            preview: "Want to test the new terminal UI? ✨",
        },
        ChatSeed {
            id: "mock:chat:team",
            platform: Platform::Slack,
            name: "#project-chat-cli",
            avatar: "project-chat-cli",
            kind: ChatKind::PublicChannel,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 14,
            preview: "Deploy to production succeeded ✅",
        },
        ChatSeed {
            id: "mock:chat:design",
            platform: Platform::Slack,
            name: "design-review",
            avatar: "design-review",
            kind: ChatKind::PrivateChannel,
            unread_count: 6,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 35,
            preview: "Priya: The new sidebar feels calmer now 🎨",
        },
        ChatSeed {
            id: "mock:chat:media",
            platform: Platform::WhatsApp,
            name: "Media Samples",
            avatar: "media-samples",
            kind: ChatKind::Group,
            unread_count: 1,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 120,
            preview: "Shared an image attachment 🖼️",
        },
        ChatSeed {
            id: "mock:chat:alex",
            platform: Platform::WhatsApp,
            name: "Alex Rivera",
            avatar: "alex",
            kind: ChatKind::Direct,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 180,
            preview: "Voice note received 🎧",
        },
        ChatSeed {
            id: "mock:chat:ops",
            platform: Platform::Discord,
            name: "Ops Room",
            avatar: "ops-room",
            kind: ChatKind::Group,
            unread_count: 3,
            muted: true,
            pinned: false,
            last_seen_minutes_ago: 260,
            preview: "Incident timeline attached 📎",
        },
        ChatSeed {
            id: "mock:chat:travel",
            platform: Platform::WhatsApp,
            name: "Lisbon Trip ✈️",
            avatar: "lisbon-trip",
            kind: ChatKind::Group,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 420,
            preview: "Sofia: Where should we have dinner? 🍽️",
        },
        ChatSeed {
            id: "mock:chat:bot",
            platform: Platform::Slack,
            name: "Release Bot",
            avatar: "release-bot",
            kind: ChatKind::Direct,
            unread_count: 0,
            muted: true,
            pinned: false,
            last_seen_minutes_ago: 900,
            preview: "v0.1.0 nightly build is ready 🚀",
        },
        ChatSeed {
            id: "mock:chat:bookclub",
            platform: Platform::Discord,
            name: "Book Club",
            avatar: "book-club",
            kind: ChatKind::Group,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_seen_minutes_ago: 4_320,
            preview: "Next pick: The Design of Everyday Things 📚",
        },
    ];
    chats.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| a.last_seen_minutes_ago.cmp(&b.last_seen_minutes_ago))
            .then_with(|| a.name.cmp(b.name))
    });
    chats
        .into_iter()
        .map(|seed| chat_from_seed(account, seed, now))
        .collect()
}

fn chat_from_seed(account: &ProviderId, seed: ChatSeed<'_>, now: Timestamp) -> Chat {
    let is_group = seed.kind != ChatKind::Direct;
    Chat {
        id: arc_str(seed.id),
        account: account.clone(),
        platform: seed.platform,
        name: arc_str(seed.name),
        avatar: avatar_path(seed.avatar),
        is_group,
        kind: seed.kind,
        membership: ChatMembership::Joined,
        is_shared: false,
        unread_count: seed.unread_count,
        muted: seed.muted,
        pinned: seed.pinned,
        last_message_at: Some(now - Duration::minutes(seed.last_seen_minutes_ago)),
        last_message_preview: Some(arc_str(seed.preview)),
        thread_id: None,
    }
}

fn mock_messages(account: &ProviderId, now: Timestamp) -> Vec<Message> {
    let mut messages = Vec::new();

    // Family Weekend — a WhatsApp group showing a reply quote, a poll, an
    // edited message, and an inline photo.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:family",
                id: "mock:msg:family:1",
                sender_id: "maya",
                sender_name: "Maya",
                timestamp: now - Duration::minutes(18),
                text: "I booked the picnic table near the lake 🌳",
                is_from_me: false,
                reactions: vec![reaction("👍", &["me", "dad"])],
                receipts: Vec::new(),
            },
        ),
        mock_reply_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:family",
                id: "mock:msg:family:reply",
                sender_id: "dad",
                sender_name: "Dad",
                timestamp: now - Duration::minutes(16),
                text: "Perfect — that's the shady one by the willow 🌥️",
                is_from_me: false,
                reactions: vec![reaction("❤️", &["maya"])],
                receipts: Vec::new(),
            },
            "mock:msg:family:1",
        ),
        mock_poll_message(
            account,
            PollMessageSeed {
                chat_id: "mock:chat:family",
                id: "mock:msg:family:poll",
                sender_id: "mom",
                sender_name: "Mom",
                timestamp: now - Duration::minutes(10),
                poll: poll(
                    "Which day works best for the picnic?",
                    &[
                        ("sat", "Saturday"),
                        ("sun", "Sunday"),
                        ("either", "Either is fine"),
                    ],
                    Some(1),
                    &[
                        ("me", &["sat"]),
                        ("dad", &["sat"]),
                        ("maya", &["sun"]),
                        ("leo", &["either"]),
                    ],
                ),
                reactions: vec![reaction("🗳️", &["maya"])],
            },
        ),
        mock_edited_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:family",
                id: "mock:msg:family:edited",
                sender_id: "leo",
                sender_name: "Leo",
                timestamp: now - Duration::minutes(6),
                text: "Bringing the frisbee — and the kite too! 🪁",
                is_from_me: false,
                reactions: vec![reaction("😄", &["me", "maya"])],
                receipts: Vec::new(),
            },
            now - Duration::minutes(5),
        ),
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:family",
                id: "mock:msg:family:2",
                sender_id: "maya",
                sender_name: "Maya",
                timestamp: now - Duration::minutes(2),
                content: Content::Image(media(
                    "family-photo",
                    "family-picnic.png",
                    "image/png",
                    Some(428_000),
                    Some("Picnic photos are up 📸"),
                )),
                is_from_me: false,
                reactions: vec![reaction("❤️", &["me", "mom", "leo"])],
                receipts: Vec::new(),
            },
        ),
    ]);

    // Alice Chen — a WhatsApp direct chat showing read receipts plus an edited
    // reply quote, and an enriched contact profile in the details pane.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:alice",
                id: "mock:msg:alice:1",
                sender_id: "alice",
                sender_name: "Alice Chen",
                timestamp: now - Duration::minutes(12),
                text: "The mock provider is online with real-looking data now 🎉",
                is_from_me: false,
                reactions: vec![reaction("🎉", &["me"])],
                receipts: Vec::new(),
            },
        ),
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:alice",
                id: "mock:msg:alice:2",
                sender_id: "me",
                sender_name: "Me",
                timestamp: now - Duration::minutes(7),
                text: "Great. I want this to feel natural for first-time users.",
                is_from_me: true,
                reactions: Vec::new(),
                receipts: vec![receipt(
                    "alice",
                    ReceiptKind::Read,
                    now - Duration::minutes(6),
                )],
            },
        ),
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:alice",
                id: "mock:msg:alice:3",
                sender_id: "alice",
                sender_name: "Alice Chen",
                timestamp: now - Duration::minutes(5),
                text: "Want to test the new terminal UI? ✨ Mouse and touchpad should work too.",
                is_from_me: false,
                reactions: vec![reaction("✨", &["me"])],
                receipts: Vec::new(),
            },
        ),
        {
            let mut reply = mock_reply_message(
                account,
                TextMessageSeed {
                    chat_id: "mock:chat:alice",
                    id: "mock:msg:alice:4",
                    sender_id: "me",
                    sender_name: "Me",
                    timestamp: now - Duration::minutes(3),
                    text: "On it — opening it in a split pane right now. 🖥️",
                    is_from_me: true,
                    reactions: Vec::new(),
                    receipts: vec![receipt(
                        "alice",
                        ReceiptKind::Read,
                        now - Duration::minutes(2),
                    )],
                },
                "mock:msg:alice:3",
            );
            reply.edited_at = Some(now - Duration::minutes(2));
            reply
        },
    ]);

    // #project-chat-cli — a Slack public channel showing a thread, an @-mention
    // of the user, and a rich deployment status card.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:1",
                sender_id: "ci-bot",
                sender_name: "CI Bot",
                timestamp: now - Duration::minutes(22),
                text: "CI is green on all platforms ✅",
                is_from_me: false,
                reactions: vec![reaction("✅", &["sam", "me"])],
                receipts: Vec::new(),
            },
        ),
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:2",
                sender_id: "sam",
                sender_name: "Sam",
                timestamp: now - Duration::minutes(20),
                text: "Nice. Next step is smoother scrolling and media cards.",
                is_from_me: false,
                reactions: Vec::new(),
                receipts: Vec::new(),
            },
        ),
        mock_thread_reply(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:2:reply:1",
                sender_id: "me",
                sender_name: "Me",
                timestamp: now - Duration::minutes(19),
                text: "Threaded reply: the media cards feel much more natural now.",
                is_from_me: true,
                reactions: vec![reaction("👍", &["sam"])],
                receipts: Vec::new(),
            },
            "mock:msg:team:2",
        ),
        mock_thread_reply(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:2:reply:2",
                sender_id: "sam",
                sender_name: "Sam",
                timestamp: now - Duration::minutes(18),
                text: "Exactly. The thread pane can show this context without crowding the main chat.",
                is_from_me: false,
                reactions: vec![reaction("✨", &["me"])],
                receipts: Vec::new(),
            },
            "mock:msg:team:2",
        ),
        mock_mention_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:3",
                sender_id: "sam",
                sender_name: "Sam",
                timestamp: now - Duration::minutes(16),
                text: "Heads up @Me — can you review the media-card PR before we deploy?",
                is_from_me: false,
                reactions: vec![reaction("👀", &["me"])],
                receipts: Vec::new(),
            },
        ),
        mock_card_message(
            account,
            CardMessageSeed {
                chat_id: "mock:chat:team",
                id: "mock:msg:team:card",
                sender_id: "deploy-bot",
                sender_name: "Deploy Bot",
                timestamp: now - Duration::minutes(14),
                card: Card {
                    kind: CardKind::ProviderAttachment,
                    source: CardSource::Slack,
                    title: Some(arc_str("Deployment succeeded")),
                    subtitle: Some(arc_str("production · v0.1.0")),
                    body: Some(arc_str(
                        "Rolled out chat-cli v0.1.0 to production with zero downtime.",
                    )),
                    footer: Some(arc_str("GitHub Actions")),
                    url: Some(arc_str("https://example.com/chat-cli/runs/4821")),
                    accent_color: Some(CardColor::Named(arc_str("good"))),
                    thumbnail: None,
                    image: None,
                    fields: vec![
                        card_field("Environment", "production", true),
                        card_field("Duration", "2m 14s", true),
                        card_field("Triggered by", "@sam", true),
                        card_field("Commit", "a1b2c3d", true),
                    ],
                    actions: vec![
                        card_action("View run", "https://example.com/chat-cli/runs/4821"),
                        card_action("Rollback", "https://example.com/chat-cli/rollback"),
                    ],
                },
                reactions: vec![reaction("✅", &["me"]), reaction("🚀", &["priya"])],
            },
        ),
    ]);

    // design-review — a Slack private channel showing a reply quote and a link
    // preview card.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:design",
                id: "mock:msg:design:1",
                sender_id: "priya",
                sender_name: "Priya Shah",
                timestamp: now - Duration::minutes(42),
                text: "The new sidebar feels calmer now 🎨",
                is_from_me: false,
                reactions: vec![reaction("💯", &["me", "alice"])],
                receipts: Vec::new(),
            },
        ),
        mock_reply_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:design",
                id: "mock:msg:design:reply",
                sender_id: "me",
                sender_name: "Me",
                timestamp: now - Duration::minutes(40),
                text: "Agreed — the muted timestamps really help it breathe.",
                is_from_me: true,
                reactions: Vec::new(),
                receipts: Vec::new(),
            },
            "mock:msg:design:1",
        ),
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:design",
                id: "mock:msg:design:2",
                sender_id: "priya",
                sender_name: "Priya Shah",
                timestamp: now - Duration::minutes(35),
                content: Content::LinkPreview(LinkPreview {
                    url: arc_str("https://example.com/chat-cli/design-review"),
                    title: Some(arc_str("Design review notes")),
                    description: Some(arc_str(
                        "Pointer-first UX, avatars, reactions, and media cards",
                    )),
                    image: Some(media(
                        "design-preview",
                        "design-review-card.png",
                        "image/png",
                        Some(96_000),
                        Some("Design review preview"),
                    )),
                }),
                is_from_me: false,
                reactions: vec![reaction("👀", &["me"])],
                receipts: Vec::new(),
            },
        ),
    ]);

    // Media Samples — image, video, and sticker attachments.
    messages.extend([
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:media",
                id: "mock:msg:media:1",
                sender_id: "designer",
                sender_name: "Designer",
                timestamp: now - Duration::hours(2),
                content: Content::Image(media(
                    "mock-media-image-1",
                    "mock-screenshot.png",
                    "image/png",
                    Some(245_760),
                    Some("Shared an image attachment 🖼️"),
                )),
                is_from_me: false,
                reactions: vec![reaction("🔥", &["me"]), reaction("👍", &["alice"])],
                receipts: Vec::new(),
            },
        ),
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:media",
                id: "mock:msg:media:2",
                sender_id: "designer",
                sender_name: "Designer",
                timestamp: now - Duration::minutes(110),
                content: Content::Video(media(
                    "mock-media-video-1",
                    "prototype-scroll.mp4",
                    "video/mp4",
                    Some(2_400_000),
                    Some("Short capture of touchpad scrolling"),
                )),
                is_from_me: false,
                reactions: Vec::new(),
                receipts: Vec::new(),
            },
        ),
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:media",
                id: "mock:msg:media:3",
                sender_id: "me",
                sender_name: "Me",
                timestamp: now - Duration::minutes(105),
                content: Content::Sticker(media(
                    "mock-sticker-1",
                    "ship-it.png",
                    "image/png",
                    Some(18_432),
                    Some("Ship it sticker 🚀"),
                )),
                is_from_me: true,
                reactions: vec![reaction("🚀", &["designer"])],
                receipts: vec![receipt(
                    "designer",
                    ReceiptKind::Delivered,
                    now - Duration::minutes(104),
                )],
            },
        ),
    ]);

    // Alex Rivera — a WhatsApp direct chat with a voice note.
    messages.extend([
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:alex",
                id: "mock:msg:alex:1",
                sender_id: "alex",
                sender_name: "Alex Rivera",
                timestamp: now - Duration::hours(3),
                content: Content::Audio(media(
                    "mock-audio-1",
                    "alex-voice-note.ogg",
                    "audio/ogg",
                    Some(180_000),
                    Some("Voice note received 🎧"),
                )),
                is_from_me: false,
                reactions: Vec::new(),
                receipts: Vec::new(),
            },
        ),
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:alex",
                id: "mock:msg:alex:2",
                sender_id: "me",
                sender_name: "Me",
                timestamp: now - Duration::minutes(175),
                text: "I’ll listen after the review call.",
                is_from_me: true,
                reactions: Vec::new(),
                receipts: vec![receipt(
                    "alex",
                    ReceiptKind::Read,
                    now - Duration::minutes(172),
                )],
            },
        ),
    ]);

    // Ops Room — a Discord group showing a file attachment, a deleted message,
    // and gracefully handled unsupported content.
    messages.extend([
        mock_media_message(
            account,
            MediaMessageSeed {
                chat_id: "mock:chat:ops",
                id: "mock:msg:ops:1",
                sender_id: "nora",
                sender_name: "Nora Ops",
                timestamp: now - Duration::minutes(260),
                content: Content::File(media(
                    "mock-file-1",
                    "incident-timeline.pdf",
                    "application/pdf",
                    Some(720_000),
                    Some("Incident timeline attached 📎"),
                )),
                is_from_me: false,
                reactions: vec![reaction("🙏", &["me", "sam"])],
                receipts: Vec::new(),
            },
        ),
        mock_simple_message(
            account,
            "mock:chat:ops",
            "mock:msg:ops:deleted",
            "nora",
            "Nora Ops",
            now - Duration::minutes(252),
            Content::Deleted,
        ),
        mock_simple_message(
            account,
            "mock:chat:ops",
            "mock:msg:ops:unsupported",
            "statuspage",
            "Statuspage",
            now - Duration::minutes(248),
            Content::Unsupported(arc_str("Interactive incident workflow")),
        ),
    ]);

    // Lisbon Trip — a WhatsApp group with a dinner poll.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:travel",
                id: "mock:msg:travel:1",
                sender_id: "sofia",
                sender_name: "Sofia",
                timestamp: now - Duration::minutes(430),
                text: "Landed! The apartment in Alfama is lovely 🏠",
                is_from_me: false,
                reactions: vec![reaction("🎉", &["me"])],
                receipts: Vec::new(),
            },
        ),
        mock_poll_message(
            account,
            PollMessageSeed {
                chat_id: "mock:chat:travel",
                id: "mock:msg:travel:poll",
                sender_id: "sofia",
                sender_name: "Sofia",
                timestamp: now - Duration::minutes(420),
                poll: poll(
                    "Where should we have dinner tonight?",
                    &[
                        ("market", "Time Out Market"),
                        ("ceviche", "A Cevicheria"),
                        ("avillez", "Cantinho do Avillez"),
                    ],
                    Some(1),
                    &[("me", &["ceviche"]), ("sofia", &["market"])],
                ),
                reactions: vec![reaction("😋", &["me"])],
            },
        ),
    ]);

    // Release Bot — a Slack direct chat from a bot, showing a bot release card.
    messages.extend([
        mock_text_message(
            account,
            TextMessageSeed {
                chat_id: "mock:chat:bot",
                id: "mock:msg:bot:1",
                sender_id: "release-bot",
                sender_name: "Release Bot",
                timestamp: now - Duration::minutes(905),
                text: "v0.1.0 nightly build is ready 🚀",
                is_from_me: false,
                reactions: Vec::new(),
                receipts: Vec::new(),
            },
        ),
        mock_card_message(
            account,
            CardMessageSeed {
                chat_id: "mock:chat:bot",
                id: "mock:msg:bot:card",
                sender_id: "release-bot",
                sender_name: "Release Bot",
                timestamp: now - Duration::minutes(900),
                card: Card {
                    kind: CardKind::BotMessage,
                    source: CardSource::Slack,
                    title: Some(arc_str("Nightly build v0.1.0")),
                    subtitle: Some(arc_str("automated release")),
                    body: Some(arc_str(
                        "• Faster startup and background history sync\n• Inline media cards and stickers\n• Calmer sidebar with muted timestamps",
                    )),
                    footer: Some(arc_str("Release Bot · just now")),
                    url: Some(arc_str("https://example.com/chat-cli/releases/v0.1.0")),
                    accent_color: Some(CardColor::Hex(arc_str("#4070F4"))),
                    thumbnail: None,
                    image: None,
                    fields: vec![
                        card_field("Platforms", "Linux · macOS · Windows", false),
                        card_field("Size", "8.4 MB", true),
                        card_field("Channel", "nightly", true),
                    ],
                    actions: vec![
                        card_action("Download", "https://example.com/chat-cli/releases/v0.1.0"),
                        card_action("Changelog", "https://example.com/chat-cli/changelog"),
                    ],
                },
                reactions: vec![reaction("🚀", &["me"])],
            },
        ),
    ]);

    // Book Club — a quiet Discord group.
    messages.push(mock_text_message(
        account,
        TextMessageSeed {
            chat_id: "mock:chat:bookclub",
            id: "mock:msg:bookclub:1",
            sender_id: "emma",
            sender_name: "Emma",
            timestamp: now - Duration::minutes(1500),
            text: "Next pick: The Design of Everyday Things 📚",
            is_from_me: false,
            reactions: vec![reaction("📚", &["me", "alex"])],
            receipts: Vec::new(),
        },
    ));

    messages
}

fn mock_text_message(account: &ProviderId, seed: TextMessageSeed<'_>) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Text(arc_str(seed.text)),
            reply_to: None,
            thread_id: None,
            is_from_me: seed.is_from_me,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

fn mock_thread_reply(
    account: &ProviderId,
    seed: TextMessageSeed<'_>,
    thread_root: &str,
) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Text(arc_str(seed.text)),
            reply_to: Some(thread_root),
            thread_id: Some(thread_root),
            is_from_me: seed.is_from_me,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

fn mock_media_message(account: &ProviderId, seed: MediaMessageSeed<'_>) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: seed.content,
            reply_to: None,
            thread_id: None,
            is_from_me: seed.is_from_me,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

/// A text message that quotes another message in the same chat (a WhatsApp/
/// Slack reply, distinct from a Slack thread reply).
fn mock_reply_message(account: &ProviderId, seed: TextMessageSeed<'_>, reply_to: &str) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Text(arc_str(seed.text)),
            reply_to: Some(reply_to),
            thread_id: None,
            is_from_me: seed.is_from_me,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

/// A text message carrying an `edited_at` marker so the timeline renders the
/// "edited" indicator.
fn mock_edited_message(
    account: &ProviderId,
    seed: TextMessageSeed<'_>,
    edited_at: Timestamp,
) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: Some(edited_at),
            content: Content::Text(arc_str(seed.text)),
            reply_to: None,
            thread_id: None,
            is_from_me: seed.is_from_me,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

/// A text message that @-mentions the authenticated user.
fn mock_mention_message(account: &ProviderId, seed: TextMessageSeed<'_>) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Text(arc_str(seed.text)),
            reply_to: None,
            thread_id: None,
            is_from_me: seed.is_from_me,
            mentions_me: true,
            reactions: seed.reactions,
            receipts: seed.receipts,
        },
    )
}

struct PollMessageSeed<'a> {
    chat_id: &'a str,
    id: &'a str,
    sender_id: &'a str,
    sender_name: &'a str,
    timestamp: Timestamp,
    poll: Poll,
    reactions: Vec<Reaction>,
}

fn mock_poll_message(account: &ProviderId, seed: PollMessageSeed<'_>) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Poll(seed.poll),
            reply_to: None,
            thread_id: None,
            is_from_me: false,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: Vec::new(),
        },
    )
}

struct CardMessageSeed<'a> {
    chat_id: &'a str,
    id: &'a str,
    sender_id: &'a str,
    sender_name: &'a str,
    timestamp: Timestamp,
    card: Card,
    reactions: Vec<Reaction>,
}

fn mock_card_message(account: &ProviderId, seed: CardMessageSeed<'_>) -> Message {
    message(
        account,
        MessageSeed {
            chat_id: seed.chat_id,
            id: seed.id,
            sender_id: seed.sender_id,
            sender_name: seed.sender_name,
            timestamp: seed.timestamp,
            edited_at: None,
            content: Content::Cards(vec![seed.card]),
            reply_to: None,
            thread_id: None,
            is_from_me: false,
            mentions_me: false,
            reactions: seed.reactions,
            receipts: Vec::new(),
        },
    )
}

/// A bare message with arbitrary [`Content`] (used for deleted/unsupported
/// placeholders that carry no reactions or receipts).
fn mock_simple_message(
    account: &ProviderId,
    chat_id: &str,
    id: &str,
    sender_id: &str,
    sender_name: &str,
    timestamp: Timestamp,
    content: Content,
) -> Message {
    message(
        account,
        MessageSeed {
            chat_id,
            id,
            sender_id,
            sender_name,
            timestamp,
            edited_at: None,
            content,
            reply_to: None,
            thread_id: None,
            is_from_me: false,
            mentions_me: false,
            reactions: Vec::new(),
            receipts: Vec::new(),
        },
    )
}

struct MessageSeed<'a> {
    chat_id: &'a str,
    id: &'a str,
    sender_id: &'a str,
    sender_name: &'a str,
    timestamp: Timestamp,
    edited_at: Option<Timestamp>,
    content: Content,
    reply_to: Option<&'a str>,
    thread_id: Option<&'a str>,
    is_from_me: bool,
    mentions_me: bool,
    reactions: Vec<Reaction>,
    receipts: Vec<Receipt>,
}

fn message(account: &ProviderId, seed: MessageSeed<'_>) -> Message {
    Message {
        id: arc_str(seed.id),
        chat_id: arc_str(seed.chat_id),
        account: account.clone(),
        sender: Sender {
            platform_id: arc_str(seed.sender_id),
            display_name: arc_str(seed.sender_name),
            avatar: avatar_path(seed.sender_id),
        },
        timestamp: seed.timestamp,
        edited_at: seed.edited_at,
        content: seed.content,
        reply_to: seed.reply_to.map(arc_str),
        thread_id: seed.thread_id.map(arc_str),
        reactions: seed.reactions,
        receipts: seed.receipts,
        is_from_me: seed.is_from_me,
        mentions_me: seed.mentions_me,
        platform_data: PlatformData::default(),
    }
}

fn content_text(content: &Content) -> &str {
    match content {
        Content::Text(text) | Content::Unsupported(text) => text,
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media.caption.as_deref().unwrap_or(media.file_name.as_ref()),
        Content::LinkPreview(link) => link.title.as_deref().unwrap_or(link.url.as_ref()),
        Content::Cards(cards) => cards
            .first()
            .and_then(|card| card.title.as_deref().or(card.body.as_deref()))
            .unwrap_or("Card"),
        Content::Poll(poll) => poll.question.as_ref(),
        Content::Deleted => "",
    }
}

fn reaction(emoji: &str, senders: &[&str]) -> Reaction {
    Reaction {
        emoji: arc_str(emoji),
        senders: senders.iter().map(arc_str).collect(),
    }
}

/// Builds a [`Poll`] from `(option_id, label)` pairs and `(voter, option_ids)`
/// votes, so the timeline renders tallies and the user's own choice.
fn poll(
    question: &str,
    options: &[(&str, &str)],
    selectable_options_count: Option<u32>,
    votes: &[(&str, &[&str])],
) -> Poll {
    Poll {
        question: arc_str(question),
        options: options
            .iter()
            .map(|(id, label)| PollOption {
                id: arc_str(id),
                label: arc_str(label),
            })
            .collect(),
        selectable_options_count,
        votes: votes
            .iter()
            .map(|(sender, options)| PollVote {
                sender: arc_str(sender),
                options: options.iter().map(arc_str).collect(),
                timestamp: None,
            })
            .collect(),
    }
}

fn card_field(title: &str, value: &str, short: bool) -> CardField {
    CardField {
        title: Some(arc_str(title)),
        value: arc_str(value),
        short,
    }
}

fn card_action(label: &str, url: &str) -> CardAction {
    CardAction {
        label: arc_str(label),
        url: Some(arc_str(url)),
    }
}

fn chat_member(platform_id: &str, display_name: &str, role: ChatMemberRole) -> ChatMember {
    ChatMember::with_role(
        Sender {
            platform_id: arc_str(platform_id),
            display_name: arc_str(display_name),
            avatar: avatar_path(platform_id),
        },
        role,
    )
}

/// Static member rosters (with roles) for the group/channel mock chats. Direct
/// chats and platforms without member listing return an empty roster.
fn mock_chat_members(chat_id: &str) -> Vec<ChatMember> {
    use ChatMemberRole::{Admin, Member, Owner};
    match chat_id {
        "mock:chat:family" => vec![
            chat_member("mom", "Mom", Owner),
            chat_member("maya", "Maya", Admin),
            chat_member("dad", "Dad", Member),
            chat_member("leo", "Leo", Member),
            chat_member("me", "Me", Member),
        ],
        "mock:chat:media" => vec![
            chat_member("designer", "Designer", Admin),
            chat_member("me", "Me", Member),
        ],
        "mock:chat:travel" => vec![
            chat_member("sofia", "Sofia", Owner),
            chat_member("me", "Me", Member),
        ],
        "mock:chat:team" => vec![
            chat_member("sam", "Sam", Admin),
            chat_member("priya", "Priya Shah", Member),
            chat_member("ci-bot", "CI Bot", Member),
            chat_member("deploy-bot", "Deploy Bot", Member),
            chat_member("me", "Me", Member),
        ],
        "mock:chat:design" => vec![
            chat_member("priya", "Priya Shah", Owner),
            chat_member("alice", "Alice Chen", Member),
            chat_member("me", "Me", Member),
        ],
        _ => Vec::new(),
    }
}

/// Provider-sourced conversation metadata for the details pane. Returns empty
/// details for direct chats (which surface a contact profile instead).
fn mock_chat_details(chat_id: &str) -> ChatDetails {
    let day = |days: i64| Some(mock_seed_now() - Duration::days(days));
    match chat_id {
        "mock:chat:family" => ChatDetails {
            description: Some(arc_str("Planning weekends together by the lake 🌳")),
            created_at: day(420),
            creator: Some(arc_str("Mom")),
            member_count: Some(5),
            admin_count: Some(2),
            disappearing_seconds: Some(7 * 24 * 60 * 60),
            ..ChatDetails::default()
        },
        "mock:chat:team" => ChatDetails {
            description: Some(arc_str(
                "Building chat-cli in the open — releases, reviews, and CI.",
            )),
            created_at: day(210),
            creator: Some(arc_str("Sam")),
            member_count: Some(24),
            admin_count: Some(3),
            workspace: Some(arc_str("Acme Engineering")),
            facts: vec![(arc_str("Topic"), arc_str("Ship v0.1.0 🚀"))],
            ..ChatDetails::default()
        },
        "mock:chat:design" => ChatDetails {
            description: Some(arc_str("Private space for design crits and explorations.")),
            created_at: day(95),
            creator: Some(arc_str("Priya Shah")),
            member_count: Some(8),
            admin_count: Some(1),
            workspace: Some(arc_str("Acme Engineering")),
            ..ChatDetails::default()
        },
        "mock:chat:media" => ChatDetails {
            description: Some(arc_str("Sample attachments that show off media cards.")),
            created_at: day(30),
            member_count: Some(2),
            ..ChatDetails::default()
        },
        "mock:chat:travel" => ChatDetails {
            description: Some(arc_str("Lisbon, here we come ✈️ Itinerary and photos.")),
            created_at: day(12),
            creator: Some(arc_str("Sofia")),
            member_count: Some(2),
            disappearing_seconds: Some(24 * 60 * 60),
            ..ChatDetails::default()
        },
        "mock:chat:ops" => ChatDetails {
            description: Some(arc_str("Incident response and on-call coordination.")),
            created_at: day(540),
            member_count: Some(12),
            facts: vec![(arc_str("On-call"), arc_str("Nora Ops"))],
            ..ChatDetails::default()
        },
        "mock:chat:bookclub" => ChatDetails {
            description: Some(arc_str("One book a month, no spoilers 📚")),
            created_at: day(800),
            member_count: Some(9),
            ..ChatDetails::default()
        },
        _ => ChatDetails::default(),
    }
}

/// Rich contact profiles for the direct-chat peers, surfaced in the details
/// pane. Returns `None` for senders without a curated profile so the caller
/// can fall back to a minimal name-only profile.
fn mock_contact_profile(platform_id: &str) -> Option<ContactProfile> {
    match platform_id {
        "alice" => Some(ContactProfile {
            display_name: Some(arc_str("Alice Chen")),
            handle: Some(arc_str("@alice")),
            title: Some(arc_str("Staff Engineer")),
            status: Some(arc_str("🎧 Heads-down on the TUI")),
            about: Some(arc_str(
                "Terminal enthusiast. Rust, good coffee, and keyboard shortcuts.",
            )),
            timezone: Some(arc_str("America/Los_Angeles")),
            local_time: Some(arc_str("9:14 AM")),
            ..ContactProfile::default()
        }),
        "alex" => Some(ContactProfile {
            display_name: Some(arc_str("Alex Rivera")),
            about: Some(arc_str(
                "On a hiking trip this week 🥾 Replies may be slow.",
            )),
            phone: Some(arc_str("+1 555-0102")),
            timezone: Some(arc_str("Europe/Lisbon")),
            is_business: true,
            ..ContactProfile::default()
        }),
        "release-bot" => Some(ContactProfile {
            display_name: Some(arc_str("Release Bot")),
            handle: Some(arc_str("@release-bot")),
            title: Some(arc_str("Automated release announcements")),
            is_bot: true,
            ..ContactProfile::default()
        }),
        _ => None,
    }
}

fn toggle_mock_reaction(message: &mut Message, emoji: &str, sender: Arc<str>) -> bool {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji.as_ref() == emoji)
    {
        if reaction.senders.iter().any(|existing| existing == &sender) {
            reaction.senders.retain(|existing| existing != &sender);
            message
                .reactions
                .retain(|reaction| !reaction.senders.is_empty());
            return false;
        }
        reaction.senders.push(sender);
        true
    } else {
        message.reactions.push(Reaction {
            emoji: arc_str(emoji),
            senders: vec![sender],
        });
        true
    }
}

fn receipt(platform_id: &str, kind: ReceiptKind, at: Timestamp) -> Receipt {
    Receipt {
        platform_id: arc_str(platform_id),
        kind,
        at: Some(at),
    }
}

fn media(
    id: &str,
    file_name: &str,
    mime_type: &str,
    size_bytes: Option<u64>,
    caption: Option<&str>,
) -> Media {
    Media {
        id: arc_str(id),
        file_name: arc_str(file_name),
        mime_type: arc_str(mime_type),
        size_bytes,
        caption: caption.map(arc_str),
        local_path: Some(mock_asset_path(file_name)),
        thumbnail: Some(mock_asset_path(thumbnail_name(file_name))),
    }
}

fn avatar_path(name: &str) -> Option<PathBuf> {
    Some(mock_asset_path(format!("avatar-{name}.png")))
}

/// Thumbnail dimensions used for the small media previews shown before a
/// full-resolution decode is ready.
const MOCK_THUMB_WIDTH: u32 = 64;
const MOCK_THUMB_HEIGHT: u32 = 40;

fn ensure_mock_assets() -> Result<()> {
    let dir = mock_asset_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    for spec in mock_image_specs() {
        if let Some(bytes) = embedded_media_photo(spec.file_name) {
            // Bundled, higher-resolution real photo for the chat window.
            write_mock_photo_png(bytes, &mock_asset_path(spec.file_name), None)?;
            write_mock_photo_png(
                bytes,
                &mock_asset_path(thumbnail_name(spec.file_name)),
                Some((MOCK_THUMB_WIDTH, MOCK_THUMB_HEIGHT)),
            )?;
        } else {
            // Stylised, procedurally drawn placeholder (screenshot, card, sticker).
            ensure_mock_png(spec)?;
            ensure_mock_png(ImageSpec {
                file_name: thumbnail_name(spec.file_name),
                width: MOCK_THUMB_WIDTH,
                height: MOCK_THUMB_HEIGHT,
                title: spec.title,
                kind: spec.kind,
                primary: spec.primary,
                secondary: spec.secondary,
            })?;
        }
    }

    for name in mock_avatar_names() {
        let path = mock_asset_path(format!("avatar-{name}.png"));
        if let Some(bytes) = embedded_avatar_photo(name) {
            // Bundled, higher-resolution real portrait/scene avatar.
            write_mock_photo_png(bytes, &path, None)?;
        } else {
            ensure_mock_png(ImageSpec {
                file_name: format!("avatar-{name}.png"),
                width: 96,
                height: 96,
                title: name,
                kind: MockImageKind::Avatar,
                primary: color_from_name(name, 0),
                secondary: color_from_name(name, 85),
            })?;
        }
    }

    Ok(())
}

/// Decodes a bundled real photo (JPEG) and installs it as a PNG at `path`,
/// optionally resizing it to `resize` first (used for small thumbnails). The
/// PNG extension is preserved because the TUI decodes avatars with
/// `image::open`, which relies on the file extension for format detection.
fn write_mock_photo_png(bytes: &[u8], path: &Path, resize: Option<(u32, u32)>) -> Result<()> {
    let mut image = image::load_from_memory(bytes)
        .with_context(|| format!("decoding bundled mock photo for {}", path.display()))?;
    if let Some((width, height)) = resize {
        image = image.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3);
    }

    let generation = MOCK_ASSET_GENERATION.fetch_add(1, Ordering::Relaxed);
    let tmp_path = mock_asset_tmp_path(path, generation);
    image
        .save_with_format(&tmp_path, ImageFormat::Png)
        .with_context(|| format!("writing mock photo {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "installing mock photo {} from {}",
            path.display(),
            tmp_path.display()
        )
    })
}

/// Bundled real portrait/scene photos for chat avatars, keyed by avatar name.
/// People use real face portraits; groups, rooms, and bots use real scene
/// photos. All are 256×256 so they stay crisp when scaled in the sidebar.
fn embedded_avatar_photo(name: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match name {
        "me" => include_bytes!("../assets/mock/avatar-me.jpg"),
        "alice" => include_bytes!("../assets/mock/avatar-alice.jpg"),
        "alex" => include_bytes!("../assets/mock/avatar-alex.jpg"),
        "maya" => include_bytes!("../assets/mock/avatar-maya.jpg"),
        "dad" => include_bytes!("../assets/mock/avatar-dad.jpg"),
        "mom" => include_bytes!("../assets/mock/avatar-mom.jpg"),
        "leo" => include_bytes!("../assets/mock/avatar-leo.jpg"),
        "sam" => include_bytes!("../assets/mock/avatar-sam.jpg"),
        "priya" => include_bytes!("../assets/mock/avatar-priya.jpg"),
        "designer" => include_bytes!("../assets/mock/avatar-designer.jpg"),
        "nora" => include_bytes!("../assets/mock/avatar-nora.jpg"),
        "sofia" => include_bytes!("../assets/mock/avatar-sofia.jpg"),
        "emma" => include_bytes!("../assets/mock/avatar-emma.jpg"),
        "family-weekend" => include_bytes!("../assets/mock/avatar-family-weekend.jpg"),
        "project-chat-cli" => include_bytes!("../assets/mock/avatar-project-chat-cli.jpg"),
        "design-review" => include_bytes!("../assets/mock/avatar-design-review.jpg"),
        "media-samples" => include_bytes!("../assets/mock/avatar-media-samples.jpg"),
        "ops-room" => include_bytes!("../assets/mock/avatar-ops-room.jpg"),
        "lisbon-trip" => include_bytes!("../assets/mock/avatar-lisbon-trip.jpg"),
        "book-club" => include_bytes!("../assets/mock/avatar-book-club.jpg"),
        "release-bot" => include_bytes!("../assets/mock/avatar-release-bot.jpg"),
        "ci-bot" => include_bytes!("../assets/mock/avatar-ci-bot.jpg"),
        "deploy-bot" => include_bytes!("../assets/mock/avatar-deploy-bot.jpg"),
        "statuspage" => include_bytes!("../assets/mock/avatar-statuspage.jpg"),
        _ => return None,
    };
    Some(bytes)
}

/// Bundled high-resolution real photos shown inside the conversation, keyed by
/// their mock media file name.
fn embedded_media_photo(file_name: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match file_name {
        "family-picnic.png" => include_bytes!("../assets/mock/family-picnic.jpg"),
        "mock-screenshot.png" => include_bytes!("../assets/mock/mock-screenshot.jpg"),
        _ => return None,
    };
    Some(bytes)
}

#[derive(Clone, Copy)]
enum MockImageKind {
    FamilyPicnic,
    TerminalScreenshot,
    DesignReview,
    RocketSticker,
    Avatar,
}

#[derive(Clone, Copy)]
struct ImageSpec<T: AsRef<str>> {
    file_name: T,
    width: u32,
    height: u32,
    title: &'static str,
    kind: MockImageKind,
    primary: Rgba<u8>,
    secondary: Rgba<u8>,
}

static MOCK_ASSET_GENERATION: AtomicU64 = AtomicU64::new(1);

fn ensure_mock_png(spec: ImageSpec<impl AsRef<str>>) -> Result<()> {
    let path = mock_asset_path(spec.file_name.as_ref());
    let generation = MOCK_ASSET_GENERATION.fetch_add(1, Ordering::Relaxed);
    let tmp_path = mock_asset_tmp_path(&path, generation);
    let image = render_mock_image(&spec, generation);
    image
        .save_with_format(&tmp_path, ImageFormat::Png)
        .with_context(|| format!("writing mock image {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "installing mock image {} from {}",
            path.display(),
            tmp_path.display()
        )
    })
}

fn mock_asset_tmp_path(path: &Path, generation: u64) -> PathBuf {
    let process_id = std::process::id();
    let mut tmp_path = path.to_path_buf();
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("tmp");
    tmp_path.set_extension(format!("{extension}.{process_id}.{generation}.tmp"));
    tmp_path
}

fn render_mock_image(spec: &ImageSpec<impl AsRef<str>>, generation: u64) -> RgbaImage {
    let mut image = ImageBuffer::from_pixel(spec.width, spec.height, spec.secondary);
    match spec.kind {
        MockImageKind::FamilyPicnic => draw_family_picnic(&mut image),
        MockImageKind::TerminalScreenshot => draw_terminal_screenshot(&mut image),
        MockImageKind::DesignReview => draw_design_review(&mut image),
        MockImageKind::RocketSticker => draw_rocket_sticker(&mut image),
        MockImageKind::Avatar => draw_avatar(&mut image, spec.title, spec.primary, spec.secondary),
    }
    stamp_generation_pixel(&mut image, generation);
    image
}

fn stamp_generation_pixel(image: &mut RgbaImage, generation: u64) {
    if image.width() == 0 || image.height() == 0 {
        return;
    }

    let red = ((generation & 0xff) as u8).max(1);
    let green = (((generation >> 8) & 0xff) as u8).max(1);
    let blue = (((generation >> 16) & 0xff) as u8).max(1);
    image.put_pixel(0, 0, Rgba([red, green, blue, 255]));
}

fn draw_family_picnic(image: &mut RgbaImage) {
    let (width, height) = image.dimensions();
    fill_vertical_gradient(
        image,
        Rgba([137, 207, 240, 255]),
        Rgba([210, 239, 255, 255]),
    );
    fill_rect(
        image,
        0,
        pct(height, 58),
        width,
        height - pct(height, 58),
        Rgba([79, 166, 92, 255]),
    );
    fill_rect(
        image,
        0,
        pct(height, 51),
        width,
        pct(height, 8),
        Rgba([93, 184, 211, 255]),
    );
    fill_circle(
        image,
        pct(width, 83) as i32,
        pct(height, 18) as i32,
        pct(width.min(height), 9) as i32,
        Rgba([255, 219, 95, 255]),
    );

    draw_tree(
        image,
        pct(width, 13),
        pct(height, 47),
        pct(width.min(height), 11),
    );
    draw_tree(
        image,
        pct(width, 90),
        pct(height, 52),
        pct(width.min(height), 9),
    );

    let blanket_x = pct(width, 32);
    let blanket_y = pct(height, 68);
    let blanket_w = pct(width, 35);
    let blanket_h = pct(height, 18);
    fill_rect(
        image,
        blanket_x,
        blanket_y,
        blanket_w,
        blanket_h,
        Rgba([210, 61, 72, 255]),
    );
    for row in 0..4 {
        for col in 0..6 {
            if (row + col) % 2 == 0 {
                fill_rect(
                    image,
                    blanket_x + col * blanket_w / 6,
                    blanket_y + row * blanket_h / 4,
                    blanket_w / 6,
                    blanket_h / 4,
                    Rgba([252, 238, 224, 255]),
                );
            }
        }
    }

    draw_person(
        image,
        pct(width, 38),
        pct(height, 61),
        pct(width.min(height), 7),
        Rgba([89, 91, 213, 255]),
    );
    draw_person(
        image,
        pct(width, 55),
        pct(height, 59),
        pct(width.min(height), 7),
        Rgba([234, 95, 137, 255]),
    );
    draw_person(
        image,
        pct(width, 69),
        pct(height, 64),
        pct(width.min(height), 6),
        Rgba([255, 136, 64, 255]),
    );

    fill_rect(
        image,
        pct(width, 48),
        pct(height, 74),
        pct(width, 8),
        pct(height, 6),
        Rgba([137, 91, 43, 255]),
    );
    fill_rect(
        image,
        pct(width, 50),
        pct(height, 70),
        pct(width, 4),
        pct(height, 4),
        Rgba([244, 196, 98, 255]),
    );
}

fn draw_terminal_screenshot(image: &mut RgbaImage) {
    let (width, height) = image.dimensions();
    fill_vertical_gradient(image, Rgba([31, 34, 55, 255]), Rgba([16, 18, 32, 255]));
    fill_rect(
        image,
        pct(width, 7),
        pct(height, 10),
        pct(width, 86),
        pct(height, 80),
        Rgba([239, 242, 248, 255]),
    );
    fill_rect(
        image,
        pct(width, 8),
        pct(height, 13),
        pct(width, 84),
        pct(height, 74),
        Rgba([26, 30, 48, 255]),
    );
    fill_rect(
        image,
        pct(width, 8),
        pct(height, 13),
        pct(width, 84),
        pct(height, 8),
        Rgba([61, 66, 96, 255]),
    );
    for index in 0..3 {
        fill_circle(
            image,
            (pct(width, 12) + index * pct(width, 4)) as i32,
            pct(height, 17) as i32,
            pct(width.min(height), 2) as i32,
            [
                Rgba([255, 95, 87, 255]),
                Rgba([255, 189, 46, 255]),
                Rgba([39, 201, 63, 255]),
            ][index as usize],
        );
    }

    fill_rect(
        image,
        pct(width, 10),
        pct(height, 25),
        pct(width, 25),
        pct(height, 56),
        Rgba([37, 42, 65, 255]),
    );
    for row in 0..5 {
        let y = pct(height, 29) + row * pct(height, 9);
        let selected = row == 1;
        fill_rect(
            image,
            pct(width, 12),
            y,
            pct(width, 21),
            pct(height, 6),
            if selected {
                Rgba([64, 112, 244, 255])
            } else {
                Rgba([55, 61, 88, 255])
            },
        );
        fill_circle(
            image,
            pct(width, 15) as i32,
            (y + pct(height, 3)) as i32,
            pct(width.min(height), 2) as i32,
            Rgba([42, 198, 218, 255]),
        );
    }

    for row in 0..4 {
        let y = pct(height, 29) + row * pct(height, 12);
        let from_me = row % 2 == 1;
        let x = if from_me {
            pct(width, 59)
        } else {
            pct(width, 39)
        };
        fill_rect(
            image,
            x,
            y,
            pct(width, 28),
            pct(height, 7),
            if from_me {
                Rgba([42, 198, 218, 255])
            } else {
                Rgba([77, 89, 130, 255])
            },
        );
    }
    fill_rect(
        image,
        pct(width, 42),
        pct(height, 58),
        pct(width, 25),
        pct(height, 16),
        Rgba([89, 91, 213, 255]),
    );
    fill_rect(
        image,
        pct(width, 45),
        pct(height, 62),
        pct(width, 19),
        pct(height, 8),
        Rgba([42, 198, 218, 255]),
    );
}

fn draw_design_review(image: &mut RgbaImage) {
    let (width, height) = image.dimensions();
    fill_vertical_gradient(
        image,
        Rgba([252, 239, 229, 255]),
        Rgba([237, 226, 255, 255]),
    );
    fill_rect(
        image,
        pct(width, 8),
        pct(height, 10),
        pct(width, 84),
        pct(height, 80),
        Rgba([255, 255, 255, 255]),
    );
    fill_rect(
        image,
        pct(width, 10),
        pct(height, 14),
        pct(width, 18),
        pct(height, 72),
        Rgba([52, 47, 80, 255]),
    );
    for row in 0..6 {
        fill_rect(
            image,
            pct(width, 13),
            pct(height, 19) + row * pct(height, 9),
            pct(width, 11),
            pct(height, 3),
            Rgba([116, 89, 217, 255]),
        );
    }

    for card in 0..3 {
        let x = pct(width, 33) + card * pct(width, 18);
        fill_rect(
            image,
            x,
            pct(height, 22),
            pct(width, 15),
            pct(height, 42),
            Rgba([246, 247, 252, 255]),
        );
        fill_rect(
            image,
            x + pct(width, 2),
            pct(height, 27),
            pct(width, 11),
            pct(height, 12),
            [
                Rgba([234, 95, 137, 255]),
                Rgba([116, 89, 217, 255]),
                Rgba([42, 198, 218, 255]),
            ][card as usize],
        );
        fill_rect(
            image,
            x + pct(width, 2),
            pct(height, 45),
            pct(width, 10),
            pct(height, 3),
            Rgba([184, 190, 210, 255]),
        );
        fill_rect(
            image,
            x + pct(width, 2),
            pct(height, 52),
            pct(width, 8),
            pct(height, 3),
            Rgba([210, 214, 230, 255]),
        );
    }

    draw_line(
        image,
        pct(width, 67) as i32,
        pct(height, 68) as i32,
        pct(width, 83) as i32,
        pct(height, 34) as i32,
        Rgba([255, 136, 64, 255]),
        3,
    );
    fill_circle(
        image,
        pct(width, 84) as i32,
        pct(height, 33) as i32,
        pct(width.min(height), 5) as i32,
        Rgba([255, 211, 83, 255]),
    );
}

fn draw_rocket_sticker(image: &mut RgbaImage) {
    let (width, height) = image.dimensions();
    fill_vertical_gradient(image, Rgba([41, 35, 92, 255]), Rgba([15, 19, 50, 255]));
    for index in 0..18 {
        let x = (index * 37 % width.max(1) as usize) as u32;
        let y = (index * 53 % height.max(1) as usize) as u32;
        fill_rect(image, x, y, 2, 2, Rgba([255, 255, 214, 255]));
    }
    fill_circle(
        image,
        pct(width, 50) as i32,
        pct(height, 50) as i32,
        pct(width.min(height), 38) as i32,
        Rgba([89, 91, 213, 255]),
    );
    fill_triangle(
        image,
        (pct(width, 50) as i32, pct(height, 18) as i32),
        (pct(width, 36) as i32, pct(height, 44) as i32),
        (pct(width, 64) as i32, pct(height, 44) as i32),
        Rgba([245, 245, 245, 255]),
    );
    fill_rect(
        image,
        pct(width, 36),
        pct(height, 42),
        pct(width, 28),
        pct(height, 28),
        Rgba([245, 245, 245, 255]),
    );
    fill_circle(
        image,
        pct(width, 50) as i32,
        pct(height, 47) as i32,
        pct(width.min(height), 7) as i32,
        Rgba([42, 198, 218, 255]),
    );
    fill_triangle(
        image,
        (pct(width, 36) as i32, pct(height, 63) as i32),
        (pct(width, 23) as i32, pct(height, 78) as i32),
        (pct(width, 39) as i32, pct(height, 72) as i32),
        Rgba([255, 136, 64, 255]),
    );
    fill_triangle(
        image,
        (pct(width, 64) as i32, pct(height, 63) as i32),
        (pct(width, 77) as i32, pct(height, 78) as i32),
        (pct(width, 61) as i32, pct(height, 72) as i32),
        Rgba([255, 136, 64, 255]),
    );
    fill_triangle(
        image,
        (pct(width, 42) as i32, pct(height, 70) as i32),
        (pct(width, 58) as i32, pct(height, 70) as i32),
        (pct(width, 50) as i32, pct(height, 94) as i32),
        Rgba([255, 211, 83, 255]),
    );
    fill_triangle(
        image,
        (pct(width, 45) as i32, pct(height, 70) as i32),
        (pct(width, 55) as i32, pct(height, 70) as i32),
        (pct(width, 50) as i32, pct(height, 88) as i32),
        Rgba([255, 95, 87, 255]),
    );
}

fn draw_avatar(image: &mut RgbaImage, title: &str, primary: Rgba<u8>, secondary: Rgba<u8>) {
    let (width, height) = image.dimensions();
    fill_vertical_gradient(image, primary, secondary);
    fill_circle(
        image,
        pct(width, 50) as i32,
        pct(height, 42) as i32,
        pct(width.min(height), 18) as i32,
        Rgba([255, 235, 205, 255]),
    );
    fill_rect(
        image,
        pct(width, 31),
        pct(height, 62),
        pct(width, 38),
        pct(height, 20),
        Rgba([245, 245, 245, 255]),
    );
    let accent = color_from_name(title, 120);
    fill_rect(
        image,
        pct(width, 24),
        pct(height, 80),
        pct(width, 52),
        pct(height, 6),
        accent,
    );
}

fn draw_tree(image: &mut RgbaImage, x: u32, y: u32, size: u32) {
    fill_rect(
        image,
        x.saturating_sub(size / 8),
        y,
        size / 4,
        size,
        Rgba([102, 69, 42, 255]),
    );
    fill_circle(
        image,
        x as i32,
        y.saturating_sub(size / 2) as i32,
        (size / 2) as i32,
        Rgba([49, 128, 72, 255]),
    );
    fill_circle(
        image,
        x.saturating_sub(size / 3) as i32,
        y.saturating_sub(size / 4) as i32,
        (size / 3) as i32,
        Rgba([57, 150, 81, 255]),
    );
    fill_circle(
        image,
        (x + size / 3) as i32,
        y.saturating_sub(size / 4) as i32,
        (size / 3) as i32,
        Rgba([57, 150, 81, 255]),
    );
}

fn draw_person(image: &mut RgbaImage, x: u32, y: u32, size: u32, shirt: Rgba<u8>) {
    fill_circle(
        image,
        x as i32,
        y.saturating_sub(size / 2) as i32,
        (size / 4) as i32,
        Rgba([255, 221, 185, 255]),
    );
    fill_rect(
        image,
        x.saturating_sub(size / 4),
        y.saturating_sub(size / 3),
        size / 2,
        size / 2,
        shirt,
    );
}

fn fill_vertical_gradient(image: &mut RgbaImage, top: Rgba<u8>, bottom: Rgba<u8>) {
    let (width, height) = image.dimensions();
    let denominator = height.saturating_sub(1).max(1);
    for y in 0..height {
        let color = blend(top, bottom, y, denominator);
        for x in 0..width {
            image.put_pixel(x, y, color);
        }
    }
}

fn fill_rect(image: &mut RgbaImage, x: u32, y: u32, width: u32, height: u32, color: Rgba<u8>) {
    let max_x = x.saturating_add(width).min(image.width());
    let max_y = y.saturating_add(height).min(image.height());
    for py in y.min(image.height())..max_y {
        for px in x.min(image.width())..max_x {
            image.put_pixel(px, py, color);
        }
    }
}

fn fill_circle(image: &mut RgbaImage, cx: i32, cy: i32, radius: i32, color: Rgba<u8>) {
    let radius_squared = radius * radius;
    for y in cy.saturating_sub(radius)..=cy.saturating_add(radius) {
        for x in cx.saturating_sub(radius)..=cx.saturating_add(radius) {
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= radius_squared {
                put_pixel_checked(image, x, y, color);
            }
        }
    }
}

fn fill_triangle(
    image: &mut RgbaImage,
    p1: (i32, i32),
    p2: (i32, i32),
    p3: (i32, i32),
    color: Rgba<u8>,
) {
    let min_x = p1.0.min(p2.0).min(p3.0).max(0);
    let max_x = p1.0.max(p2.0).max(p3.0).min(image.width() as i32 - 1);
    let min_y = p1.1.min(p2.1).min(p3.1).max(0);
    let max_y = p1.1.max(p2.1).max(p3.1).min(image.height() as i32 - 1);

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            if point_in_triangle((x, y), p1, p2, p3) {
                put_pixel_checked(image, x, y, color);
            }
        }
    }
}

fn draw_line(
    image: &mut RgbaImage,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    color: Rgba<u8>,
    thickness: i32,
) {
    let steps = (x2 - x1).abs().max((y2 - y1).abs()).max(1);
    for step in 0..=steps {
        let x = x1 + (x2 - x1) * step / steps;
        let y = y1 + (y2 - y1) * step / steps;
        fill_circle(image, x, y, thickness, color);
    }
}

fn point_in_triangle(point: (i32, i32), p1: (i32, i32), p2: (i32, i32), p3: (i32, i32)) -> bool {
    let d1 = sign(point, p1, p2);
    let d2 = sign(point, p2, p3);
    let d3 = sign(point, p3, p1);
    let has_negative = d1 < 0 || d2 < 0 || d3 < 0;
    let has_positive = d1 > 0 || d2 > 0 || d3 > 0;
    !(has_negative && has_positive)
}

fn sign(point: (i32, i32), p1: (i32, i32), p2: (i32, i32)) -> i32 {
    (point.0 - p2.0) * (p1.1 - p2.1) - (p1.0 - p2.0) * (point.1 - p2.1)
}

fn put_pixel_checked(image: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>) {
    if x >= 0 && y >= 0 && (x as u32) < image.width() && (y as u32) < image.height() {
        image.put_pixel(x as u32, y as u32, color);
    }
}

fn blend(start: Rgba<u8>, end: Rgba<u8>, numerator: u32, denominator: u32) -> Rgba<u8> {
    let blend_channel = |from: u8, to: u8| {
        let from = u32::from(from);
        let to = u32::from(to);
        ((from * (denominator - numerator) + to * numerator) / denominator) as u8
    };
    Rgba([
        blend_channel(start.0[0], end.0[0]),
        blend_channel(start.0[1], end.0[1]),
        blend_channel(start.0[2], end.0[2]),
        255,
    ])
}

fn pct(total: u32, percent: u32) -> u32 {
    total.saturating_mul(percent) / 100
}

fn mock_image_specs() -> Vec<ImageSpec<&'static str>> {
    vec![
        ImageSpec {
            file_name: "family-picnic.png",
            width: 320,
            height: 200,
            title: "Family Picnic",
            kind: MockImageKind::FamilyPicnic,
            primary: Rgba([68, 138, 82, 255]),
            secondary: Rgba([247, 198, 91, 255]),
        },
        ImageSpec {
            file_name: "mock-screenshot.png",
            width: 320,
            height: 200,
            title: "Mock Screenshot",
            kind: MockImageKind::TerminalScreenshot,
            primary: Rgba([89, 91, 213, 255]),
            secondary: Rgba([42, 198, 218, 255]),
        },
        ImageSpec {
            file_name: "design-review-card.png",
            width: 320,
            height: 180,
            title: "Design Review",
            kind: MockImageKind::DesignReview,
            primary: Rgba([234, 95, 137, 255]),
            secondary: Rgba([116, 89, 217, 255]),
        },
        ImageSpec {
            file_name: "ship-it.png",
            width: 180,
            height: 180,
            title: "Ship It",
            kind: MockImageKind::RocketSticker,
            primary: Rgba([255, 136, 64, 255]),
            secondary: Rgba([255, 211, 83, 255]),
        },
    ]
}

fn mock_avatar_names() -> &'static [&'static str] {
    &[
        "me",
        "family-weekend",
        "alice",
        "project-chat-cli",
        "design-review",
        "media-samples",
        "alex",
        "ops-room",
        "lisbon-trip",
        "release-bot",
        "book-club",
        "maya",
        "dad",
        "mom",
        "leo",
        "ci-bot",
        "sam",
        "priya",
        "designer",
        "nora",
        "sofia",
        "emma",
        "deploy-bot",
        "statuspage",
    ]
}

fn color_from_name(name: &str, offset: u8) -> Rgba<u8> {
    let hash = name
        .bytes()
        .fold(offset, |acc, byte| acc.wrapping_add(byte));
    Rgba([
        80u8.saturating_add(hash % 120),
        70u8.saturating_add(hash.wrapping_mul(3) % 140),
        90u8.saturating_add(hash.wrapping_mul(7) % 120),
        255,
    ])
}

fn thumbnail_name(file_name: impl AsRef<str>) -> String {
    format!("thumb-{}.png", file_name.as_ref())
}

fn mock_asset_path(name: impl AsRef<str>) -> PathBuf {
    mock_asset_dir().join(name.as_ref())
}

fn mock_asset_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("CHAT_CLI_MOCK_ASSET_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return path;
    }

    let temp_dir = std::env::temp_dir();
    if let Some(dirs) = directories::BaseDirs::new() {
        let data_dir = dirs.data_local_dir().join("chat-cli").join("mock-assets");
        if !data_dir.starts_with(&temp_dir) {
            return data_dir;
        }

        return dirs
            .home_dir()
            .join(".local")
            .join("share")
            .join("chat-cli")
            .join("mock-assets");
    }

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".chat-cli")
        .join("mock-assets")
}

fn arc_str(value: impl AsRef<str>) -> Arc<str> {
    Arc::from(value.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_exposes_seed_data_and_events() -> Result<()> {
        let provider = MockProvider::new();
        let mut events = provider.events();

        provider.connect().await?;
        assert!(matches!(events.recv().await?, ProviderEvent::AuthSucceeded));
        assert!(matches!(events.recv().await?, ProviderEvent::SyncComplete));
        assert!(provider.account_info().avatar.is_some());
        assert_eq!(provider.chats().await?.len(), 10);

        let chat_id = arc_str("mock:chat:alice");
        let messages = provider.history(&chat_id, None, 50).await?;
        assert_eq!(messages.len(), 4);

        let sent_id = provider
            .send(
                &chat_id,
                OutboundContent::new(Content::Text(arc_str("hello"))),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("mock:sent:"));
        assert!(matches!(
            events.recv().await?,
            ProviderEvent::Message {
                is_historical: false,
                ..
            }
        ));
        let messages = provider.history(&chat_id, None, 50).await?;
        assert!(messages.iter().any(|message| {
            message.id == sent_id
                && message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "hello")
        }));

        Ok(())
    }

    #[tokio::test]
    async fn mock_provider_records_outbound_mentions() -> Result<()> {
        let provider = MockProvider::new();
        provider.connect().await?;

        let members = vec![ChatMember::new(Sender {
            platform_id: arc_str("mock:user:bogdan"),
            display_name: arc_str("Bogdan"),
            avatar: None,
        })];

        let encoded = provider.encode_outbound_mentions("hi @Bogdan", &members, &[]);
        assert_eq!(encoded.text, "hi @Bogdan");
        assert_eq!(encoded.mentioned.len(), 1);
        assert_eq!(
            encoded.mentioned[0].platform_id.as_ref(),
            "mock:user:bogdan"
        );

        let chat_id = arc_str("mock:chat:alice");
        let mut outbound = OutboundContent::new(Content::Text(arc_str("hi @Bogdan")));
        outbound.mentions = encoded.mentioned;
        provider.send(&chat_id, outbound, None).await?;

        let recorded = provider.sent_mentions();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].len(), 1);
        assert_eq!(recorded[0][0].display_name.as_ref(), "Bogdan");
        Ok(())
    }

    #[test]
    fn mock_provider_uses_current_local_day_seed_timestamps() {
        let first = MockProvider::new();
        let second = MockProvider::new();

        let first_messages = first.seed_messages();
        let second_messages = second.seed_messages();
        let first_family = first_messages
            .iter()
            .find(|message| message.id.as_ref() == "mock:msg:family:2")
            .expect("family seed message exists");
        let second_family = second_messages
            .iter()
            .find(|message| message.id.as_ref() == "mock:msg:family:2")
            .expect("family seed message exists");

        assert_eq!(first_family.timestamp, second_family.timestamp);
        assert_eq!(
            first_family.timestamp,
            mock_seed_now() - Duration::minutes(2)
        );
        assert_eq!(
            first_family.timestamp.with_timezone(&Local).date_naive(),
            Local::now().date_naive()
        );
    }

    #[tokio::test]
    async fn mock_provider_exposes_rich_media_and_profile_data() -> Result<()> {
        let provider = MockProvider::new();
        let chats = provider.chats().await?;
        let messages = provider.seed_messages();

        assert!(chats.iter().all(|chat| chat.avatar.is_some()));
        assert!(
            messages
                .iter()
                .all(|message| message.sender.avatar.is_some())
        );
        assert!(
            messages
                .iter()
                .any(|message| content_text(&message.content).contains('🎉'))
        );
        assert!(messages.iter().any(|message| !message.reactions.is_empty()));
        assert!(messages.iter().any(|message| !message.receipts.is_empty()));
        let thread_replies = messages
            .iter()
            .filter(|message| message.thread_id.as_deref() == Some("mock:msg:team:2"))
            .collect::<Vec<_>>();
        assert_eq!(thread_replies.len(), 2);
        assert!(
            thread_replies
                .iter()
                .all(|message| { message.reply_to.as_deref() == Some("mock:msg:team:2") })
        );
        assert!(
            thread_replies
                .iter()
                .any(|message| content_text(&message.content).contains("thread pane"))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Image(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Video(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Audio(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::File(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Sticker(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::LinkPreview(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Poll(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Cards(_)))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Deleted))
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content, Content::Unsupported(_)))
        );
        assert!(messages.iter().any(|message| message.edited_at.is_some()));
        assert!(messages.iter().any(|message| message.mentions_me));
        assert!(
            messages
                .iter()
                .any(|message| message.reply_to.is_some() && message.thread_id.is_none())
        );

        Ok(())
    }

    #[tokio::test]
    async fn mock_provider_exposes_details_pane_enrichment() -> Result<()> {
        let provider = MockProvider::new();

        let family = provider.chat_members(&arc_str("mock:chat:family")).await?;
        assert!(family.len() >= 3);
        assert!(family.iter().any(|member| member.role.is_admin()));
        assert!(
            family
                .iter()
                .any(|member| member.role == crate::ChatMemberRole::Owner)
        );

        let team_details = provider.chat_details(&arc_str("mock:chat:team")).await?;
        assert!(!team_details.is_empty());
        assert!(team_details.workspace.is_some());
        assert!(team_details.member_count.is_some());

        let direct_details = provider.chat_details(&arc_str("mock:chat:alice")).await?;
        assert!(direct_details.is_empty());

        let alice_profile = provider
            .contact_profile(&arc_str("alice"))
            .await?
            .expect("alice has a curated profile");
        assert!(alice_profile.has_detail());
        assert!(alice_profile.title.is_some());

        let bot_profile = provider
            .contact_profile(&arc_str("release-bot"))
            .await?
            .expect("release bot has a curated profile");
        assert!(bot_profile.is_bot);

        Ok(())
    }

    #[tokio::test]
    async fn mock_provider_uses_durable_mock_asset_directory() -> Result<()> {
        let provider = MockProvider::new();
        provider.ensure_mock_assets()?;
        let avatar = provider
            .account_info()
            .avatar
            .expect("mock account has avatar");
        let temp_dir = std::env::temp_dir();
        let asset_dir = avatar
            .parent()
            .expect("avatar path should have a mock asset parent");

        assert!(avatar.exists(), "avatar file exists: {}", avatar.display());
        assert!(
            avatar.ends_with("mock-assets/avatar-me.png"),
            "avatar path should be under mock-assets: {}",
            avatar.display()
        );
        assert!(
            !avatar.starts_with(&temp_dir),
            "mock assets should not live in volatile temp storage: {}",
            avatar.display()
        );

        let first_signature = asset_signature(&avatar)?;
        provider.ensure_mock_assets()?;
        let second_signature = asset_signature(&avatar)?;
        assert_ne!(
            second_signature,
            first_signature,
            "mock assets should be regenerated safely if previous files disappear or become stale in {}",
            asset_dir.display()
        );

        Ok(())
    }

    #[tokio::test]
    async fn mock_provider_installs_real_photos_for_avatars_and_chat_window() -> Result<()> {
        let provider = MockProvider::new();
        provider.ensure_mock_assets()?;

        // The chat-window photos are bundled high-resolution real images rather
        // than procedurally drawn placeholders.
        let family = image::ImageReader::open(mock_asset_path("family-picnic.png"))?
            .decode()?
            .to_rgba8();
        assert_eq!(family.dimensions(), (960, 640), "family picnic photo size");
        assert!(
            unique_color_count(&family) > 40,
            "family picnic should be a photographic image"
        );

        let screenshot = image::ImageReader::open(mock_asset_path("mock-screenshot.png"))?
            .decode()?
            .to_rgba8();
        assert_eq!(
            screenshot.dimensions(),
            (960, 600),
            "shared photo attachment size"
        );
        assert!(
            unique_color_count(&screenshot) > 40,
            "shared attachment should be a photographic image"
        );

        // People avatars are real 256×256 portraits.
        let avatar = image::ImageReader::open(mock_asset_path("avatar-alice.png"))?
            .decode()?
            .to_rgba8();
        assert_eq!(avatar.dimensions(), (256, 256), "avatar size");
        assert!(
            unique_color_count(&avatar) > 30,
            "avatar should be a photographic portrait"
        );

        Ok(())
    }

    #[tokio::test]
    async fn mock_provider_generates_stylised_placeholder_images() -> Result<()> {
        let provider = MockProvider::new();
        provider.ensure_mock_assets()?;

        let design = image::ImageReader::open(mock_asset_path("design-review-card.png"))?
            .decode()?
            .to_rgba8();
        assert_pixel_near(&design, 84, 33, [255, 211, 83], "design review annotation");
        assert_pixel_near(&design, 16, 50, [52, 47, 80], "design review sidebar");
        assert!(unique_color_count(&design) > 10);

        let sticker = image::ImageReader::open(mock_asset_path("ship-it.png"))?
            .decode()?
            .to_rgba8();
        assert_pixel_near(&sticker, 50, 90, [255, 211, 83], "ship-it rocket flame");
        assert_pixel_near(&sticker, 50, 47, [42, 198, 218], "ship-it rocket window");
        assert!(unique_color_count(&sticker) > 8);

        Ok(())
    }

    fn asset_signature(path: &std::path::Path) -> Result<(u64, u64)> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Ok((metadata.len(), modified))
    }

    fn assert_pixel_near(
        image: &RgbaImage,
        x_percent: u32,
        y_percent: u32,
        expected: [u8; 3],
        label: &str,
    ) {
        let x = pct(image.width(), x_percent).min(image.width().saturating_sub(1));
        let y = pct(image.height(), y_percent).min(image.height().saturating_sub(1));
        let pixel = image.get_pixel(x, y).0;
        for (actual, expected) in pixel[..3].iter().zip(expected) {
            assert!(
                actual.abs_diff(expected) <= 12,
                "{label} expected channel near {expected}, got {actual} at {x},{y}"
            );
        }
    }

    fn unique_color_count(image: &RgbaImage) -> usize {
        let step_x = (image.width() / 32).max(1);
        let step_y = (image.height() / 32).max(1);
        let mut colors = std::collections::HashSet::new();
        let mut y = 0;
        while y < image.height() {
            let mut x = 0;
            while x < image.width() {
                colors.insert(image.get_pixel(x, y).0);
                x += step_x;
            }
            y += step_y;
        }
        colors.len()
    }
}
