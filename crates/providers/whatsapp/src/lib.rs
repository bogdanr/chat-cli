use anyhow::{Result, bail};
use async_trait::async_trait;
use chat_core::{
    Account, AuthChallenge, Chat, ChatId, ChatKind, ChatMembership, Content, EventBus, Media,
    Message, MessageId, OutboundCapabilities, Platform, PlatformData, PlatformId, Poll, PollOption,
    PollVote, Provider, ProviderEvent, ProviderId, Reaction, Sender, Timestamp, WhatsAppData,
};
use chrono::Utc;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{sync::broadcast, task::JoinHandle};

pub mod bridge;

const PROVIDER_ID: &str = "whatsapp:bridge";
const INBOX_CHAT_ID: &str = "whatsapp:bridge:inbox";
const BRIDGE_SENDER_ID: &str = "whatsapp:bridge:sender";
const LOCAL_REACTION_SENDER: &str = "me";

pub struct WhatsAppProvider {
    handle: bridge::ClientHandle,
    id: ProviderId,
    account: Account,
    inbox_chat: Arc<RwLock<Chat>>,
    chats: Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: Arc<RwLock<Vec<Message>>>,
    profiles: Arc<RwLock<HashMap<PlatformId, Sender>>>,
    pending_reactions: Arc<RwLock<HashMap<MessageId, Vec<PendingReaction>>>>,
    log_path: Arc<Option<PathBuf>>,
    events: EventBus,
    connected: AtomicBool,
    next_message: Arc<AtomicU64>,
    callback_sender: Mutex<Option<Box<tokio::sync::mpsc::UnboundedSender<String>>>>,
    forwarder: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone, Debug)]
pub struct WhatsAppProviderOptions {
    pub db_path: String,
    pub sync_scope: String,
    pub log_path: Option<PathBuf>,
}

impl WhatsAppProviderOptions {
    pub fn new(db_path: impl Into<String>) -> Self {
        Self {
            db_path: db_path.into(),
            sync_scope: "today".to_owned(),
            log_path: None,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingReaction {
    emoji: Arc<str>,
    sender: Arc<str>,
    added: bool,
}

impl WhatsAppProvider {
    pub fn new(db_path: &str) -> Result<Self> {
        Self::with_options(WhatsAppProviderOptions::new(db_path))
    }

    pub fn with_options(options: WhatsAppProviderOptions) -> Result<Self> {
        let sync_scope = normalize_sync_scope(&options.sync_scope);
        let log_path_text = options
            .log_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string());
        let handle = bridge::new_client(&options.db_path, &sync_scope, log_path_text.as_deref())?;
        let id = arc_str(PROVIDER_ID);
        let account = Account {
            id: id.clone(),
            platform: Platform::WhatsApp,
            display_name: arc_str("WhatsApp"),
            avatar: None,
        };
        let inbox_chat = Chat {
            id: arc_str(INBOX_CHAT_ID),
            account: id.clone(),
            platform: Platform::WhatsApp,
            name: arc_str("WhatsApp Bridge"),
            avatar: None,
            is_group: false,
            kind: ChatKind::Direct,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: true,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let chats = HashMap::new();

        Ok(Self {
            handle,
            id,
            account,
            inbox_chat: Arc::new(RwLock::new(inbox_chat)),
            chats: Arc::new(RwLock::new(chats)),
            messages: Arc::new(RwLock::new(Vec::new())),
            profiles: Arc::new(RwLock::new(HashMap::new())),
            pending_reactions: Arc::new(RwLock::new(HashMap::new())),
            log_path: Arc::new(options.log_path),
            events: EventBus::new(),
            connected: AtomicBool::new(false),
            next_message: Arc::new(AtomicU64::new(1)),
            callback_sender: Mutex::new(None),
            forwarder: Mutex::new(None),
        })
    }

    fn start_message_forwarder(&self) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut sender_box = Box::new(tx);
        let sender_ptr: *mut tokio::sync::mpsc::UnboundedSender<String> = sender_box.as_mut();

        unsafe {
            bridge::set_message_callback(sender_ptr.cast());
        }
        *lock_mutex(&self.callback_sender) = Some(sender_box);

        let account_id = self.id.clone();
        let inbox_chat = Arc::clone(&self.inbox_chat);
        let chats = Arc::clone(&self.chats);
        let messages = Arc::clone(&self.messages);
        let profiles = Arc::clone(&self.profiles);
        let pending_reactions = Arc::clone(&self.pending_reactions);
        let log_path = Arc::clone(&self.log_path);
        let events = self.events.clone();
        let next_message = Arc::clone(&self.next_message);

        let task = tokio::spawn(async move {
            while let Some(raw_event) = rx.recv().await {
                let context = BridgeForwardContext {
                    account_id: &account_id,
                    inbox_chat: &inbox_chat,
                    chats: &chats,
                    messages: &messages,
                    profiles: &profiles,
                    pending_reactions: &pending_reactions,
                    log_path: log_path.as_ref().as_ref(),
                    events: &events,
                    next_message: &next_message,
                };
                forward_bridge_event(&context, &raw_event);
            }
        });
        *lock_mutex(&self.forwarder) = Some(task);
    }

    fn stop_message_forwarder(&self) {
        unsafe {
            bridge::clear_message_callback();
        }
        lock_mutex(&self.callback_sender).take();
        if let Some(task) = lock_mutex(&self.forwarder).take() {
            task.abort();
        }
    }
}

impl WhatsAppProvider {
    fn send_media_to_bridge(
        &self,
        chat_jid: &str,
        media: &Media,
        content_type: &str,
    ) -> Result<String> {
        let local_path = media
            .local_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("WhatsApp media send requires a local file path"))?;
        let path = local_path.to_string_lossy();
        let file_name = media.file_name.as_ref();
        let caption = media.caption.as_deref().unwrap_or_default();
        log_outbound_media_attempt(
            self.log_path.as_deref(),
            chat_jid,
            local_path.as_path(),
            media.mime_type.as_ref(),
            content_type,
            media.size_bytes,
        );
        bridge::send_media(
            self.handle,
            chat_jid,
            &path,
            media.mime_type.as_ref(),
            file_name,
            caption,
            content_type,
        )
    }
}

impl Drop for WhatsAppProvider {
    fn drop(&mut self) {
        self.stop_message_forwarder();
        bridge::disconnect(self.handle);
    }
}

#[async_trait]
impl Provider for WhatsAppProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn platform(&self) -> Platform {
        Platform::WhatsApp
    }

    fn account_info(&self) -> Account {
        self.account.clone()
    }

    fn outbound_capabilities(&self) -> OutboundCapabilities {
        OutboundCapabilities {
            text: true,
            image: true,
            gif: true,
            video: true,
            audio: true,
            file: true,
            sticker: true,
            max_upload_size: None,
            media_note: Some(Arc::from(
                "WhatsApp GIFs may be sent as documents depending on format",
            )),
        }
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }

        self.start_message_forwarder();
        if !bridge::connect(self.handle) {
            self.stop_message_forwarder();
            bail!("failed to connect WhatsApp bridge client")
        }

        self.connected.store(true, Ordering::Release);
        self.events
            .send(ProviderEvent::AuthRequired(AuthChallenge::Waiting));
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.stop_message_forwarder();
        bridge::disconnect(self.handle);
        self.connected.store(false, Ordering::Release);
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
        let mut chats = lock_rw_read(&self.chats)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        chats.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.last_message_at.cmp(&a.last_message_at))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(chats)
    }

    async fn history(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut messages = lock_rw_read(&self.messages)
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
        content: Content,
        reply_to: Option<&MessageId>,
    ) -> Result<MessageId> {
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            bail!("cannot send WhatsApp message to bridge/system chat")
        }

        let raw_response = match &content {
            Content::Text(text) => bridge::send_text(self.handle, &chat_jid, text)?,
            Content::Image(media) if media.mime_type.as_ref() == "image/gif" => {
                self.send_media_to_bridge(&chat_jid, media, "gif")?
            }
            Content::Image(media) => self.send_media_to_bridge(&chat_jid, media, "image")?,
            Content::Video(media) => self.send_media_to_bridge(&chat_jid, media, "video")?,
            Content::Audio(media) => self.send_media_to_bridge(&chat_jid, media, "audio")?,
            Content::File(media) => self.send_media_to_bridge(&chat_jid, media, "file")?,
            Content::Sticker(media) => self.send_media_to_bridge(&chat_jid, media, "sticker")?,
            Content::LinkPreview(_) => {
                bail!("WhatsApp link preview sending should be sent as plain text first")
            }
            Content::Poll(_) => bail!("WhatsApp poll creation is not wired yet"),
            Content::Deleted => bail!("cannot send a deleted WhatsApp message"),
            Content::Unsupported(_) => bail!("cannot send unsupported WhatsApp content"),
        };
        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            bail!(
                "{}",
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp send failed".to_owned())
            );
        }

        let message_id = event
            .id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "whatsapp:bridge:sent:{}",
                    self.next_message.fetch_add(1, Ordering::Relaxed)
                )
            });
        let message_id = arc_str(message_id);
        let timestamp = event.timestamp().unwrap_or_else(Utc::now);
        let message = Message {
            id: message_id.clone(),
            chat_id: chat_id.clone(),
            account: self.id.clone(),
            sender: Sender {
                platform_id: arc_str(event.sender_jid.unwrap_or_else(|| "me".to_owned())),
                display_name: arc_str("Me"),
                avatar: None,
            },
            timestamp,
            edited_at: None,
            content: content.clone(),
            reply_to: reply_to.cloned(),
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            platform_data: PlatformData {
                whatsapp: Some(WhatsAppData {
                    jid: arc_str(chat_jid),
                }),
                slack: None,
            },
        };
        lock_rw_write(&self.messages).push(message.clone());
        upsert_chat_preview(
            &self.id,
            &self.chats,
            ChatPreviewUpdate {
                chat_id: chat_id.clone(),
                name: chat_id_to_name(chat_id),
                avatar: None,
                is_group: event.is_group,
                muted: event.muted,
                timestamp,
                preview: content_preview(&content),
                increment_unread: false,
            },
        );
        self.events.send(ProviderEvent::Message {
            message,
            is_historical: false,
        });
        Ok(message_id)
    }

    async fn download_media(&self, _media: &Media) -> Result<PathBuf> {
        bail!("WhatsApp media download is not wired yet")
    }

    async fn mark_read(&self, _chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
        Ok(())
    }

    async fn react(&self, chat_id: &ChatId, message: &Message, emoji: &str) -> Result<()> {
        let message_id = message.id.clone();
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            bail!("cannot react to messages in the WhatsApp bridge/system chat")
        }

        let (sender_jid, had_reaction) = {
            let messages = lock_rw_read(&self.messages);
            let loaded_message = messages.iter().find(|loaded| loaded.id == message_id);
            let reaction_target = loaded_message.unwrap_or(message);
            let sender_jid = if reaction_target.is_from_me {
                String::new()
            } else {
                reaction_target.sender.platform_id.to_string()
            };
            let had_reaction =
                message_reacted_by_sender(reaction_target, emoji, LOCAL_REACTION_SENDER);
            (sender_jid, had_reaction)
        };
        let reaction = if had_reaction { "" } else { emoji };
        let raw_response = bridge::send_reaction(
            self.handle,
            &chat_jid,
            &sender_jid,
            message_id.as_ref(),
            reaction,
        )?;
        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            bail!(
                "{}",
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp reaction failed".to_owned())
            );
        }

        let mut changed = None;
        {
            let mut messages = lock_rw_write(&self.messages);
            if let Some(message) = messages.iter_mut().find(|message| message.id == message_id) {
                if had_reaction {
                    remove_message_reaction(
                        message,
                        &arc_str(emoji),
                        &arc_str(LOCAL_REACTION_SENDER),
                    );
                } else {
                    add_message_reaction(message, arc_str(emoji), arc_str(LOCAL_REACTION_SENDER));
                }
                changed = Some(message.clone());
            }
        }
        if let Some(message) = changed {
            self.events.send(ProviderEvent::MessageEdited { message });
        }
        self.events.send(ProviderEvent::ReactionChanged {
            chat_id: chat_id.clone(),
            message_id: message_id.clone(),
            emoji: arc_str(emoji),
            added: !had_reaction,
            sender: arc_str(LOCAL_REACTION_SENDER),
        });
        Ok(())
    }

    async fn vote_poll(
        &self,
        chat_id: &ChatId,
        message: &Message,
        selected_options: &[Arc<str>],
    ) -> Result<()> {
        let message_id = message.id.clone();
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            bail!("cannot vote in polls in the WhatsApp bridge/system chat")
        }
        let Content::Poll(poll) = &message.content else {
            bail!("selected WhatsApp message is not a poll")
        };
        let sender_jid = if message.is_from_me {
            String::new()
        } else {
            message.sender.platform_id.to_string()
        };
        let option_labels = selected_options
            .iter()
            .filter_map(|selected| {
                poll.options
                    .iter()
                    .find(|option| option.id.as_ref() == selected.as_ref())
                    .map(|option| option.label.to_string())
            })
            .collect::<Vec<_>>();
        if option_labels.is_empty() {
            bail!("select at least one poll option")
        }
        let raw_response = bridge::send_poll_vote(
            self.handle,
            &chat_jid,
            &sender_jid,
            message_id.as_ref(),
            &option_labels,
        )?;
        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            bail!(
                "{}",
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp poll vote failed".to_owned())
            );
        }
        forward_poll_vote_event(
            &BridgeForwardContext {
                account_id: &self.id,
                inbox_chat: &self.inbox_chat,
                chats: &self.chats,
                messages: &self.messages,
                profiles: &self.profiles,
                pending_reactions: &self.pending_reactions,
                log_path: self.log_path.as_ref().as_ref(),
                events: &self.events,
                next_message: &self.next_message,
            },
            event,
        );
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let query = query.to_lowercase();
        Ok(lock_rw_read(&self.messages)
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
        if let Some(profile) = lock_rw_read(&self.profiles).get(platform_id).cloned() {
            return Ok(Some(profile));
        }

        Ok(lock_rw_read(&self.messages)
            .iter()
            .find(|message| message.sender.platform_id == *platform_id)
            .map(|message| message.sender.clone()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BridgeReaction {
    emoji: String,
    senders: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BridgeEvent {
    #[serde(rename = "type")]
    kind: String,
    code: Option<String>,
    event: Option<String>,
    message: Option<String>,
    reason: Option<String>,
    jid: Option<String>,
    id: Option<String>,
    chat_jid: Option<String>,
    chat_name: Option<String>,
    sender_jid: Option<String>,
    sender_name: Option<String>,
    avatar_path: Option<PathBuf>,
    text: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    from_me: bool,
    #[serde(default)]
    is_group: bool,
    muted: Option<bool>,
    progress: Option<u8>,
    content_type: Option<String>,
    media_id: Option<String>,
    media_file_name: Option<String>,
    media_mime: Option<String>,
    media_size: Option<u64>,
    media_local_path: Option<PathBuf>,
    media_thumbnail_path: Option<PathBuf>,
    caption: Option<String>,
    reaction_message_id: Option<String>,
    #[serde(default)]
    reaction_message_from_me: bool,
    reaction_emoji: Option<String>,
    #[serde(default)]
    reactions: Vec<BridgeReaction>,
    poll_question: Option<String>,
    #[serde(default)]
    poll_options: Vec<String>,
    #[serde(default)]
    poll_option_ids: Vec<String>,
    poll_selectable_options_count: Option<u32>,
    poll_vote_message_id: Option<String>,
    #[serde(default)]
    poll_vote_options: Vec<String>,
}

impl BridgeEvent {
    fn decode(raw_event: &str) -> Result<Self> {
        if raw_event.trim_start().starts_with('{') {
            Ok(serde_json::from_str(raw_event)?)
        } else {
            Ok(Self {
                kind: "message".to_owned(),
                code: None,
                event: None,
                message: None,
                reason: None,
                jid: None,
                id: None,
                chat_jid: Some(BRIDGE_SENDER_ID.to_owned()),
                chat_name: Some("WhatsApp Bridge".to_owned()),
                sender_jid: Some(BRIDGE_SENDER_ID.to_owned()),
                sender_name: Some("WhatsApp Bridge".to_owned()),
                avatar_path: None,
                text: Some(raw_event.to_owned()),
                timestamp: None,
                from_me: false,
                is_group: false,
                muted: None,
                progress: None,
                content_type: None,
                media_id: None,
                media_file_name: None,
                media_mime: None,
                media_size: None,
                media_local_path: None,
                media_thumbnail_path: None,
                caption: None,
                reaction_message_id: None,
                reaction_message_from_me: false,
                reaction_emoji: None,
                reactions: Vec::new(),
                poll_question: None,
                poll_options: Vec::new(),
                poll_option_ids: Vec::new(),
                poll_selectable_options_count: None,
                poll_vote_message_id: None,
                poll_vote_options: Vec::new(),
            })
        }
    }

    fn timestamp(&self) -> Option<Timestamp> {
        self.timestamp
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }
}

struct BridgeForwardContext<'a> {
    account_id: &'a ProviderId,
    inbox_chat: &'a Arc<RwLock<Chat>>,
    chats: &'a Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: &'a Arc<RwLock<Vec<Message>>>,
    profiles: &'a Arc<RwLock<HashMap<PlatformId, Sender>>>,
    pending_reactions: &'a Arc<RwLock<HashMap<MessageId, Vec<PendingReaction>>>>,
    log_path: Option<&'a PathBuf>,
    events: &'a EventBus,
    next_message: &'a AtomicU64,
}

fn forward_bridge_event(context: &BridgeForwardContext<'_>, raw_event: &str) {
    log_provider_event(context.log_path, raw_event);
    let event = match BridgeEvent::decode(raw_event) {
        Ok(event) => event,
        Err(error) => {
            emit_bridge_status_message(
                context.account_id,
                context.inbox_chat,
                context.messages,
                context.events,
                context.next_message,
                format!("WhatsApp bridge event decode failed: {error}"),
            );
            return;
        }
    };

    match event.kind.as_str() {
        "qr" => {
            if let Some(code) = event.code {
                context
                    .events
                    .send(ProviderEvent::AuthRequired(AuthChallenge::QrCode(arc_str(
                        &code,
                    ))));
                emit_bridge_status_message(
                    context.account_id,
                    context.inbox_chat,
                    context.messages,
                    context.events,
                    context.next_message,
                    format!("WhatsApp QR code: {code}"),
                );
            }
        }
        "login" => {
            let detail = event.event.unwrap_or_else(|| "waiting".to_owned());
            emit_bridge_status_message(
                context.account_id,
                context.inbox_chat,
                context.messages,
                context.events,
                context.next_message,
                format!("WhatsApp login: {detail}"),
            );
        }
        "connected" => {
            context.events.send(ProviderEvent::AuthSucceeded);
            if let Some(jid) = event.jid {
                emit_bridge_status_message(
                    context.account_id,
                    context.inbox_chat,
                    context.messages,
                    context.events,
                    context.next_message,
                    format!("WhatsApp connected as {jid}"),
                );
            }
        }
        "sync" => {
            if let Some(progress) = event.progress {
                context.events.send(ProviderEvent::SyncProgress(progress));
            }
            if event.progress.unwrap_or(100) >= 100 {
                context.events.send(ProviderEvent::SyncComplete);
            }
        }
        "message" => {
            forward_message_event(context, event, false);
        }
        "history" => {
            forward_message_event(context, event, true);
        }
        "sent" => {
            forward_message_event(context, event, false);
        }
        "profile" => {
            forward_profile_event(
                context.account_id,
                context.chats,
                context.messages,
                context.profiles,
                context.events,
                event,
            );
        }
        "reaction" => {
            forward_reaction_event(context, event);
        }
        "poll_vote" => {
            forward_poll_vote_event(context, event);
        }
        "disconnected" => {
            context
                .events
                .send(ProviderEvent::Disconnected(event.reason.map(arc_str)));
        }
        "error" => {
            emit_bridge_status_message(
                context.account_id,
                context.inbox_chat,
                context.messages,
                context.events,
                context.next_message,
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp bridge error".to_owned()),
            );
        }
        _ => emit_bridge_status_message(
            context.account_id,
            context.inbox_chat,
            context.messages,
            context.events,
            context.next_message,
            format!("WhatsApp bridge event: {}", event.kind),
        ),
    }
}

fn forward_message_event(
    context: &BridgeForwardContext<'_>,
    event: BridgeEvent,
    is_historical: bool,
) {
    let chat_jid = event
        .chat_jid
        .clone()
        .unwrap_or_else(|| INBOX_CHAT_ID.to_owned());
    let chat_id = chat_id_from_jid(&chat_jid);
    let text = event
        .text
        .clone()
        .unwrap_or_else(|| "[empty WhatsApp message]".to_owned());
    let timestamp = event.timestamp().unwrap_or_else(Utc::now);
    let message_id = arc_str(event.id.clone().unwrap_or_else(|| {
        format!(
            "whatsapp:bridge:message:{}",
            context.next_message.fetch_add(1, Ordering::Relaxed)
        )
    }));
    let sender_jid = event.sender_jid.clone().unwrap_or_else(|| {
        if event.from_me {
            "me"
        } else {
            BRIDGE_SENDER_ID
        }
        .to_owned()
    });
    let sender_name = event
        .sender_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            if event.from_me {
                "Me".to_owned()
            } else {
                sender_name_from_jid(&sender_jid)
            }
        });
    let sender = upsert_profile(
        context.profiles,
        sender_jid.clone(),
        sender_name.clone(),
        event.avatar_path.clone(),
    );
    let preview = content_preview_for_event(&event, &text);
    let content = content_from_event(&event, text);
    let mut message = Message {
        id: message_id.clone(),
        chat_id: chat_id.clone(),
        account: context.account_id.clone(),
        sender,
        timestamp,
        edited_at: None,
        content,
        reply_to: None,
        thread_id: None,
        reactions: reactions_from_event(&event),
        receipts: Vec::new(),
        is_from_me: event.from_me,
        platform_data: PlatformData {
            whatsapp: Some(WhatsAppData {
                jid: arc_str(chat_jid),
            }),
            slack: None,
        },
    };
    apply_pending_reactions(context.pending_reactions, &mut message);
    lock_rw_write(context.messages).push(message.clone());
    let chat = upsert_chat_preview(
        context.account_id,
        context.chats,
        ChatPreviewUpdate {
            chat_id,
            name: chat_name_for_event(&event, &sender_jid),
            avatar: event.avatar_path.clone(),
            is_group: event.is_group,
            muted: event.muted,
            timestamp,
            preview,
            increment_unread: !event.from_me && !is_historical,
        },
    );
    context.events.send(ProviderEvent::ChatUpdated(chat));
    context.events.send(ProviderEvent::Message {
        message,
        is_historical,
    });
}

fn emit_bridge_status_message(
    account_id: &ProviderId,
    _inbox_chat: &Arc<RwLock<Chat>>,
    messages: &Arc<RwLock<Vec<Message>>>,
    events: &EventBus,
    next_message: &AtomicU64,
    text: impl AsRef<str>,
) {
    let text = arc_str(text);
    let timestamp = Utc::now();
    let message = Message {
        id: arc_str(format!(
            "whatsapp:bridge:status:{}",
            next_message.fetch_add(1, Ordering::Relaxed)
        )),
        chat_id: arc_str(INBOX_CHAT_ID),
        account: account_id.clone(),
        sender: Sender {
            platform_id: arc_str(BRIDGE_SENDER_ID),
            display_name: arc_str("WhatsApp Bridge"),
            avatar: None,
        },
        timestamp,
        edited_at: None,
        content: Content::Text(text.clone()),
        reply_to: None,
        thread_id: None,
        reactions: Vec::new(),
        receipts: Vec::new(),
        is_from_me: false,
        platform_data: PlatformData {
            whatsapp: Some(WhatsAppData {
                jid: arc_str(BRIDGE_SENDER_ID),
            }),
            slack: None,
        },
    };

    lock_rw_write(messages).push(message.clone());
    events.send(ProviderEvent::Message {
        message,
        is_historical: false,
    });
}

struct ChatPreviewUpdate {
    chat_id: ChatId,
    name: Arc<str>,
    avatar: Option<PathBuf>,
    is_group: bool,
    muted: Option<bool>,
    timestamp: Timestamp,
    preview: Arc<str>,
    increment_unread: bool,
}

fn upsert_chat_preview(
    account_id: &ProviderId,
    chats: &Arc<RwLock<HashMap<ChatId, Chat>>>,
    update: ChatPreviewUpdate,
) -> Chat {
    let mut chats = lock_rw_write(chats);
    let chat = chats.entry(update.chat_id.clone()).or_insert_with(|| Chat {
        id: update.chat_id,
        account: account_id.clone(),
        platform: Platform::WhatsApp,
        name: update.name.clone(),
        avatar: update.avatar.clone(),
        is_group: update.is_group,
        kind: if update.is_group {
            ChatKind::Group
        } else {
            ChatKind::Direct
        },
        membership: ChatMembership::Joined,
        is_shared: false,
        unread_count: 0,
        muted: update.muted.unwrap_or(false),
        pinned: false,
        last_message_at: None,
        last_message_preview: None,
        thread_id: None,
    });
    let should_update_preview = chat
        .last_message_at
        .is_none_or(|current| update.timestamp >= current);
    if should_update_preview {
        chat.last_message_at = Some(update.timestamp);
        chat.last_message_preview = Some(update.preview);
    }
    if update.avatar.is_some() {
        chat.avatar = update.avatar;
    }
    if should_replace_chat_name(chat.name.as_ref(), update.name.as_ref(), update.is_group) {
        chat.name = update.name;
    }
    chat.is_group = update.is_group;
    if let Some(muted) = update.muted {
        chat.muted = muted;
    }
    if update.increment_unread {
        chat.unread_count = chat.unread_count.saturating_add(1);
    }
    chat.clone()
}

fn should_replace_chat_name(current: &str, candidate: &str, is_group: bool) -> bool {
    if candidate.is_empty() || current == candidate {
        return false;
    }
    if current.is_empty() || current == "WhatsApp Group" {
        return true;
    }
    if is_group {
        return looks_like_jid_fallback(current) || !looks_like_jid_fallback(candidate);
    }
    looks_like_jid_fallback(current) && !looks_like_jid_fallback(candidate)
}

fn looks_like_jid_fallback(value: &str) -> bool {
    value
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn content_text(content: &Content) -> &str {
    match content {
        Content::Text(text) => text,
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media.caption.as_deref().unwrap_or_default(),
        Content::LinkPreview(preview) => preview
            .title
            .as_deref()
            .or(preview.description.as_deref())
            .unwrap_or(preview.url.as_ref()),
        Content::Poll(poll) => poll.question.as_ref(),
        Content::Deleted => "",
        Content::Unsupported(description) => description,
    }
}

fn content_preview(content: &Content) -> Arc<str> {
    match content {
        Content::Text(text) => text.clone(),
        Content::Image(media) => outbound_media_preview("Photo", media),
        Content::Video(media) => outbound_media_preview("Video", media),
        Content::Audio(media) => outbound_media_preview("Audio", media),
        Content::File(media) => outbound_media_preview("File", media),
        Content::Sticker(_) => arc_str("Sticker"),
        Content::LinkPreview(preview) => {
            arc_str(preview.title.as_deref().unwrap_or(preview.url.as_ref()))
        }
        Content::Poll(poll) => arc_str(format!("Poll: {}", poll.question)),
        Content::Deleted => arc_str("Deleted message"),
        Content::Unsupported(description) => description.clone(),
    }
}

fn outbound_media_preview(label: &str, media: &Media) -> Arc<str> {
    if let Some(caption) = media
        .caption
        .as_deref()
        .filter(|caption| !caption.is_empty())
    {
        arc_str(format!("{label}: {caption}"))
    } else if !media.file_name.is_empty() {
        arc_str(format!("{label}: {}", media.file_name))
    } else {
        arc_str(label)
    }
}

fn content_preview_for_event(event: &BridgeEvent, text: &str) -> Arc<str> {
    match event.content_type.as_deref() {
        Some("image") => media_preview("Photo", event, text),
        Some("video") => media_preview("Video", event, text),
        Some("audio") => arc_str("Voice note"),
        Some("file") => media_preview("File", event, text),
        Some("sticker") => arc_str("Sticker"),
        Some("poll") => arc_str(format!(
            "Poll: {}",
            event.poll_question.as_deref().unwrap_or(text)
        )),
        _ => arc_str(text),
    }
}

fn media_preview(label: &str, event: &BridgeEvent, text: &str) -> Arc<str> {
    let caption = event
        .caption
        .as_deref()
        .or((!text.is_empty()).then_some(text))
        .unwrap_or_default();
    if caption.is_empty() {
        arc_str(label)
    } else {
        arc_str(format!("{label}: {caption}"))
    }
}

fn content_from_event(event: &BridgeEvent, text: String) -> Content {
    let Some(kind) = event.content_type.as_deref() else {
        return Content::Text(arc_str(text));
    };

    if kind == "poll" {
        return Content::Poll(Poll {
            question: arc_str(
                event
                    .poll_question
                    .as_deref()
                    .filter(|question| !question.is_empty())
                    .unwrap_or(&text),
            ),
            options: event
                .poll_options
                .iter()
                .enumerate()
                .filter(|(_, option)| !option.is_empty())
                .map(|(index, option)| PollOption {
                    id: arc_str(
                        event
                            .poll_option_ids
                            .get(index)
                            .filter(|id| !id.is_empty())
                            .map(String::as_str)
                            .unwrap_or(option.as_str()),
                    ),
                    label: arc_str(option.as_str()),
                })
                .collect(),
            selectable_options_count: event.poll_selectable_options_count,
            votes: Vec::new(),
        });
    }

    let media = Media {
        id: arc_str(
            event
                .media_id
                .as_deref()
                .or(event.id.as_deref())
                .unwrap_or("whatsapp-media"),
        ),
        file_name: arc_str(
            event
                .media_file_name
                .as_deref()
                .unwrap_or_else(|| media_default_file_name(kind)),
        ),
        mime_type: arc_str(
            event
                .media_mime
                .as_deref()
                .unwrap_or_else(|| media_default_mime(kind)),
        ),
        size_bytes: event.media_size,
        caption: event
            .caption
            .as_deref()
            .or((!text.is_empty()).then_some(text.as_str()))
            .map(arc_str),
        local_path: event.media_local_path.clone(),
        thumbnail: event.media_thumbnail_path.clone(),
    };

    match kind {
        "image" => Content::Image(media),
        "video" => Content::Video(media),
        "audio" => Content::Audio(media),
        "file" => Content::File(media),
        "sticker" => Content::Sticker(media),
        other => Content::Unsupported(arc_str(format!("WhatsApp {other}"))),
    }
}

fn media_default_file_name(kind: &str) -> &str {
    match kind {
        "image" => "whatsapp-image.jpg",
        "video" => "whatsapp-video.mp4",
        "audio" => "whatsapp-audio.ogg",
        "sticker" => "whatsapp-sticker.webp",
        _ => "whatsapp-file.bin",
    }
}

fn media_default_mime(kind: &str) -> &str {
    match kind {
        "image" => "image/jpeg",
        "video" => "video/mp4",
        "audio" => "audio/ogg",
        "sticker" => "image/webp",
        _ => "application/octet-stream",
    }
}

fn forward_reaction_event(context: &BridgeForwardContext<'_>, event: BridgeEvent) {
    let Some(target_id) = event
        .reaction_message_id
        .clone()
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(emoji) = event
        .reaction_emoji
        .clone()
        .filter(|emoji| !emoji.is_empty())
    else {
        return;
    };
    let chat_jid = event
        .chat_jid
        .clone()
        .unwrap_or_else(|| INBOX_CHAT_ID.to_owned());
    let sender = reaction_sender(&event);
    let chat_id = chat_id_from_jid(&chat_jid);
    let message_id = arc_str(target_id);
    let emoji = arc_str(emoji);
    let sender = arc_str(sender);
    let added = !emoji.is_empty();

    let mut changed = None;
    {
        let mut messages = lock_rw_write(context.messages);
        if let Some(message) = messages.iter_mut().find(|message| message.id == message_id) {
            if added {
                add_message_reaction(message, emoji.clone(), sender.clone());
            } else {
                remove_message_reaction(message, &emoji, &sender);
            }
            changed = Some(message.clone());
        }
    }
    if changed.is_none() {
        lock_rw_write(context.pending_reactions)
            .entry(message_id.clone())
            .or_default()
            .push(PendingReaction {
                emoji: emoji.clone(),
                sender: sender.clone(),
                added,
            });
    }
    if let Some(message) = changed {
        context
            .events
            .send(ProviderEvent::MessageEdited { message });
    }
    context.events.send(ProviderEvent::ReactionChanged {
        chat_id,
        message_id,
        emoji,
        added,
        sender,
    });
}

fn forward_poll_vote_event(context: &BridgeForwardContext<'_>, event: BridgeEvent) {
    let Some(target_id) = event
        .poll_vote_message_id
        .clone()
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let sender = if event.from_me {
        LOCAL_REACTION_SENDER.to_owned()
    } else {
        event
            .sender_jid
            .clone()
            .filter(|sender| !sender.is_empty())
            .unwrap_or_else(|| BRIDGE_SENDER_ID.to_owned())
    };
    let timestamp = event.timestamp();
    let selected_options = event
        .poll_vote_options
        .iter()
        .filter(|option| !option.is_empty())
        .map(arc_str)
        .collect::<Vec<_>>();
    if selected_options.is_empty() {
        return;
    }

    if let Some(sender_name) = event.sender_name.clone().filter(|name| !name.is_empty()) {
        upsert_profile(
            context.profiles,
            sender.clone(),
            sender_name,
            event.avatar_path.clone(),
        );
    }

    let message_id = arc_str(target_id);
    let sender = arc_str(sender);
    let mut changed = None;
    {
        let mut messages = lock_rw_write(context.messages);
        if let Some(message) = messages.iter_mut().find(|message| message.id == message_id) {
            apply_poll_vote(message, sender, selected_options, timestamp);
            changed = Some(message.clone());
        }
    }
    if let Some(message) = changed {
        context
            .events
            .send(ProviderEvent::MessageEdited { message });
    }
}

fn apply_poll_vote(
    message: &mut Message,
    sender: Arc<str>,
    selected_options: Vec<Arc<str>>,
    timestamp: Option<Timestamp>,
) {
    let Content::Poll(poll) = &mut message.content else {
        return;
    };
    poll.votes.retain(|vote| vote.sender != sender);
    poll.votes.push(PollVote {
        sender,
        options: selected_options,
        timestamp,
    });
}

fn reactions_from_event(event: &BridgeEvent) -> Vec<Reaction> {
    event
        .reactions
        .iter()
        .filter(|reaction| !reaction.emoji.is_empty() && !reaction.senders.is_empty())
        .map(|reaction| Reaction {
            emoji: arc_str(&reaction.emoji),
            senders: reaction
                .senders
                .iter()
                .filter(|sender| !sender.is_empty())
                .map(arc_str)
                .collect(),
        })
        .filter(|reaction| !reaction.senders.is_empty())
        .collect()
}

fn apply_pending_reactions(
    pending_reactions: &Arc<RwLock<HashMap<MessageId, Vec<PendingReaction>>>>,
    message: &mut Message,
) {
    let pending = lock_rw_write(pending_reactions).remove(&message.id);
    if let Some(pending) = pending {
        for reaction in pending {
            if reaction.added {
                add_message_reaction(message, reaction.emoji, reaction.sender);
            } else {
                remove_message_reaction(message, &reaction.emoji, &reaction.sender);
            }
        }
    }
}

fn reaction_sender(event: &BridgeEvent) -> String {
    if event.from_me {
        return LOCAL_REACTION_SENDER.to_owned();
    }
    event
        .sender_jid
        .clone()
        .filter(|sender| !sender.is_empty())
        .unwrap_or_else(|| BRIDGE_SENDER_ID.to_owned())
}

fn message_reacted_by_sender(message: &Message, emoji: &str, sender: &str) -> bool {
    message.reactions.iter().any(|reaction| {
        reaction.emoji.as_ref() == emoji
            && reaction
                .senders
                .iter()
                .any(|candidate| candidate.as_ref() == sender)
    })
}

fn add_message_reaction(message: &mut Message, emoji: Arc<str>, sender: Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji == emoji)
    {
        if !reaction.senders.iter().any(|existing| existing == &sender) {
            reaction.senders.push(sender);
        }
    } else {
        message.reactions.push(Reaction {
            emoji,
            senders: vec![sender],
        });
    }
}

fn remove_message_reaction(message: &mut Message, emoji: &Arc<str>, sender: &Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji == *emoji)
    {
        reaction.senders.retain(|existing| existing != sender);
    }
    message
        .reactions
        .retain(|reaction| !reaction.senders.is_empty());
}

fn chat_id_from_jid(jid: &str) -> ChatId {
    arc_str(format!("whatsapp:{jid}"))
}

fn whatsapp_jid_from_chat_id(chat_id: &ChatId) -> String {
    chat_id
        .strip_prefix("whatsapp:")
        .unwrap_or_default()
        .to_owned()
}

fn sender_name_from_jid(jid: &str) -> String {
    jid.split('@').next().unwrap_or(jid).to_owned()
}

fn chat_id_to_name(chat_id: &ChatId) -> Arc<str> {
    arc_str(sender_name_from_jid(&whatsapp_jid_from_chat_id(chat_id)))
}

fn chat_name_for_event(event: &BridgeEvent, sender_jid: &str) -> Arc<str> {
    if event.is_group {
        event
            .chat_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(arc_str)
            .or_else(|| {
                event
                    .chat_jid
                    .as_deref()
                    .map(sender_name_from_jid)
                    .map(arc_str)
            })
            .unwrap_or_else(|| arc_str("WhatsApp Group"))
    } else {
        event
            .chat_name
            .as_deref()
            .or(event.sender_name.as_deref())
            .filter(|name| !name.is_empty())
            .map(arc_str)
            .unwrap_or_else(|| arc_str(sender_name_from_jid(sender_jid)))
    }
}

fn upsert_profile(
    profiles: &Arc<RwLock<HashMap<PlatformId, Sender>>>,
    platform_id: String,
    display_name: String,
    avatar: Option<PathBuf>,
) -> Sender {
    let platform_id = arc_str(platform_id);
    let mut profiles = lock_rw_write(profiles);
    let sender = profiles
        .entry(platform_id.clone())
        .or_insert_with(|| Sender {
            platform_id: platform_id.clone(),
            display_name: arc_str(&display_name),
            avatar: avatar.clone(),
        });
    let candidate_is_fallback = looks_like_jid_fallback(&display_name)
        || display_name == sender_name_from_jid(platform_id.as_ref());
    let current_is_fallback = looks_like_jid_fallback(sender.display_name.as_ref())
        || sender.display_name.as_ref() == sender_name_from_jid(sender.platform_id.as_ref());
    if !display_name.is_empty() && (!candidate_is_fallback || current_is_fallback) {
        sender.display_name = arc_str(display_name);
    }
    if avatar.is_some() {
        sender.avatar = avatar;
    }
    sender.clone()
}

fn forward_profile_event(
    account_id: &ProviderId,
    chats: &Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: &Arc<RwLock<Vec<Message>>>,
    profiles: &Arc<RwLock<HashMap<PlatformId, Sender>>>,
    events: &EventBus,
    event: BridgeEvent,
) {
    let Some(jid) = event.jid.or(event.sender_jid).or(event.chat_jid) else {
        return;
    };
    let name = event
        .sender_name
        .or(event.chat_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| sender_name_from_jid(&jid));
    let avatar = event.avatar_path;
    let sender = upsert_profile(profiles, jid.clone(), name.clone(), avatar.clone());

    let chat_id = chat_id_from_jid(&jid);
    let updated_chat = {
        let mut chats = lock_rw_write(chats);
        if let Some(chat) = chats.get_mut(&chat_id) {
            chat.name = arc_str(&name);
            if avatar.is_some() {
                chat.avatar = avatar.clone();
            }
            if let Some(muted) = event.muted {
                chat.muted = muted;
            }
            Some(chat.clone())
        } else if event.is_group {
            let chat = Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str(&name),
                avatar: avatar.clone(),
                is_group: true,
                kind: ChatKind::Group,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: event.muted.unwrap_or(false),
                pinned: false,
                last_message_at: None,
                last_message_preview: None,
                thread_id: None,
            };
            chats.insert(chat_id, chat.clone());
            Some(chat)
        } else {
            None
        }
    };

    if let Some(updated_chat) = updated_chat {
        events.send(ProviderEvent::ChatUpdated(updated_chat));
    }

    let mut changed_messages = Vec::new();
    {
        let mut messages = lock_rw_write(messages);
        for message in messages.iter_mut() {
            if message.sender.platform_id == sender.platform_id {
                message.sender = sender.clone();
                changed_messages.push(message.clone());
            }
        }
    }
    for message in changed_messages {
        events.send(ProviderEvent::Message {
            message,
            is_historical: true,
        });
    }
}

fn normalize_sync_scope(scope: &str) -> String {
    match scope.trim().to_ascii_lowercase().as_str() {
        "none" | "today" | "all" => scope.trim().to_ascii_lowercase(),
        _ => "today".to_owned(),
    }
}

fn log_provider_event(log_path: Option<&PathBuf>, raw_event: &str) {
    append_provider_log(
        log_path.map(PathBuf::as_path),
        &format!("{} whatsapp-provider {raw_event}", Utc::now().to_rfc3339()),
    );
}

fn log_outbound_media_attempt(
    log_path: Option<&Path>,
    chat_jid: &str,
    path: &Path,
    mime_type: &str,
    content_type: &str,
    size_bytes: Option<u64>,
) {
    let size = size_bytes
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    append_provider_log(
        log_path,
        &format!(
            "{} whatsapp-provider sending outbound media chat={chat_jid} path={} mime={mime_type} content_type={content_type} size={size}",
            Utc::now().to_rfc3339(),
            path.display()
        ),
    );
}

fn append_provider_log(log_path: Option<&Path>, line: &str) {
    let Some(path) = log_path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

fn arc_str(value: impl AsRef<str>) -> Arc<str> {
    Arc::from(value.as_ref())
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_rw_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_rw_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::Provider;
    use std::{fs, sync::OnceLock, time::Duration};

    static FFI_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    async fn ffi_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        FFI_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    #[test]
    fn bridge_event_decodes_qr_messages_history_and_profiles() -> Result<()> {
        let qr = BridgeEvent::decode(r#"{"type":"qr","code":"2@test"}"#)?;
        assert_eq!(qr.kind, "qr");
        assert_eq!(qr.code.as_deref(), Some("2@test"));

        let message = BridgeEvent::decode(
            r#"{"type":"message","id":"abc","chat_jid":"123@s.whatsapp.net","chat_name":"Ada Lovelace","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","avatar_path":"/tmp/ada.jpg","text":"hello","timestamp":"2026-06-05T12:00:00Z"}"#,
        )?;
        assert_eq!(message.kind, "message");
        assert_eq!(message.chat_jid.as_deref(), Some("123@s.whatsapp.net"));
        assert_eq!(message.chat_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(message.sender_name.as_deref(), Some("Ada"));
        assert_eq!(
            message.avatar_path.as_deref(),
            Some(std::path::Path::new("/tmp/ada.jpg"))
        );
        assert_eq!(
            message.timestamp().unwrap().to_rfc3339(),
            "2026-06-05T12:00:00+00:00"
        );

        let history = BridgeEvent::decode(r#"{"type":"history","id":"old-1","text":"old hello"}"#)?;
        assert_eq!(history.kind, "history");

        let image = BridgeEvent::decode(
            r#"{"type":"message","id":"img-1","chat_jid":"123@s.whatsapp.net","content_type":"image","media_id":"media-1","media_file_name":"photo.jpg","media_mime":"image/jpeg","media_size":42,"media_local_path":"/tmp/photo.jpg","media_thumbnail_path":"/tmp/thumb.jpg","caption":"latest photo"}"#,
        )?;
        assert_eq!(image.content_type.as_deref(), Some("image"));
        assert_eq!(image.media_id.as_deref(), Some("media-1"));
        assert_eq!(
            image.media_local_path.as_deref(),
            Some(std::path::Path::new("/tmp/photo.jpg"))
        );

        let reacted_history = BridgeEvent::decode(
            r#"{"type":"history","id":"reacted","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","text":"reacted","reactions":[{"emoji":"👍","senders":["a@s.whatsapp.net","b@s.whatsapp.net","c@s.whatsapp.net","d@s.whatsapp.net","e@s.whatsapp.net","f@s.whatsapp.net","g@s.whatsapp.net"]}]}"#,
        )?;
        let reactions = reactions_from_event(&reacted_history);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji.as_ref(), "👍");
        assert_eq!(reactions[0].senders.len(), 7);

        let reaction = BridgeEvent::decode(
            r#"{"type":"reaction","chat_jid":"123@s.whatsapp.net","reaction_message_id":"abc","reaction_emoji":"🔥","sender_jid":"456@s.whatsapp.net"}"#,
        )?;
        assert_eq!(reaction.kind, "reaction");
        assert_eq!(reaction.reaction_message_id.as_deref(), Some("abc"));
        assert_eq!(reaction.reaction_emoji.as_deref(), Some("🔥"));

        let own_reaction = BridgeEvent::decode(
            r#"{"type":"reaction","chat_jid":"123@s.whatsapp.net","from_me":true,"reaction_message_id":"abc","reaction_message_from_me":true,"reaction_emoji":"👍🏻","sender_jid":"36786512371803@lid"}"#,
        )?;
        assert_eq!(own_reaction.reaction_message_id.as_deref(), Some("abc"));
        assert!(own_reaction.reaction_message_from_me);
        assert_eq!(reaction_sender(&own_reaction), LOCAL_REACTION_SENDER);

        let profile = BridgeEvent::decode(
            r#"{"type":"profile","jid":"123@s.whatsapp.net","sender_name":"Ada","avatar_path":"/tmp/ada.jpg"}"#,
        )?;
        assert_eq!(profile.kind, "profile");
        assert_eq!(profile.jid.as_deref(), Some("123@s.whatsapp.net"));

        let legacy = BridgeEvent::decode("hello from legacy callback")?;
        assert_eq!(legacy.kind, "message");
        assert_eq!(legacy.text.as_deref(), Some("hello from legacy callback"));
        Ok(())
    }
    #[tokio::test]
    async fn whatsapp_provider_connects_and_exposes_bridge_inbox() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:connect")?;
        let mut events = provider.events();

        provider.connect().await?;

        assert!(provider.is_connected());
        assert_eq!(provider.platform(), Platform::WhatsApp);
        assert_eq!(provider.account_info().display_name.as_ref(), "WhatsApp");
        assert_eq!(provider.chats().await?.len(), 0);
        assert!(matches!(
            events.recv().await?,
            ProviderEvent::AuthRequired(_)
        ));
        let mut saw_auth = false;
        let mut saw_sync = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::AuthSucceeded => saw_auth = true,
                ProviderEvent::SyncComplete => saw_sync = true,
                _ => {}
            }
            if saw_auth && saw_sync {
                break;
            }
        }
        assert!(saw_auth);
        assert!(saw_sync);

        provider.disconnect().await?;
        assert!(!provider.is_connected());

        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_forwards_bridge_messages() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:messages")?;
        let mut events = provider.events();
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"remote-1","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","text":"hello from whatsapp","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);

        let message = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message { message, .. }
                    if content_text(&message.content) == "hello from whatsapp" =>
                {
                    break message;
                }
                _ => continue,
            }
        };

        assert_eq!(message.account.as_ref(), PROVIDER_ID);
        assert_eq!(message.chat_id.as_ref(), "whatsapp:123@s.whatsapp.net");
        assert_eq!(message.sender.display_name.as_ref(), "Ada");
        assert!(
            matches!(message.content, Content::Text(text) if text.as_ref() == "hello from whatsapp")
        );
        assert_eq!(
            provider
                .history(&arc_str("whatsapp:123@s.whatsapp.net"), None, 10)
                .await?
                .len(),
            1
        );
        assert!(
            provider
                .chats()
                .await?
                .iter()
                .any(|chat| chat.id.as_ref() == "whatsapp:123@s.whatsapp.net")
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_keeps_newer_chat_previews_and_sorts_latest_first() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:ordering")?;
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"newer","chat_jid":"new@s.whatsapp.net","chat_name":"New Chat","sender_jid":"new@s.whatsapp.net","text":"new message","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"older","chat_jid":"old@s.whatsapp.net","chat_name":"Old Chat","sender_jid":"old@s.whatsapp.net","text":"old message","timestamp":"2026-06-05T11:00:00Z"}"#
        )?);
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"history","id":"very-old","chat_jid":"new@s.whatsapp.net","chat_name":"New Chat","sender_jid":"new@s.whatsapp.net","text":"very old history","timestamp":"2026-06-01T12:00:00Z"}"#
        )?);

        tokio::time::sleep(Duration::from_millis(10)).await;
        let chats = provider.chats().await?;
        assert_eq!(chats[0].id.as_ref(), "whatsapp:new@s.whatsapp.net");
        assert_eq!(
            chats[0].last_message_preview.as_deref(),
            Some("new message")
        );
        assert_eq!(chats[1].id.as_ref(), "whatsapp:old@s.whatsapp.net");
        assert!(!chats.iter().any(|chat| chat.id.as_ref() == INBOX_CHAT_ID));

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_maps_media_and_reactions() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:media-reactions")?;
        let mut events = provider.events();
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"img-1","chat_jid":"123@s.whatsapp.net","chat_name":"Photo Chat","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","content_type":"image","media_id":"media-1","media_file_name":"photo.jpg","media_mime":"image/jpeg","media_size":2048,"media_local_path":"/tmp/photo.jpg","media_thumbnail_path":"/tmp/thumb.jpg","caption":"last image","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);

        let image_message = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message { message, .. } if message.id.as_ref() == "img-1" => {
                    break message;
                }
                _ => continue,
            }
        };
        match image_message.content {
            Content::Image(media) => {
                assert_eq!(media.file_name.as_ref(), "photo.jpg");
                assert_eq!(media.mime_type.as_ref(), "image/jpeg");
                assert_eq!(media.size_bytes, Some(2048));
                assert_eq!(media.caption.as_deref(), Some("last image"));
                assert_eq!(
                    media.local_path.as_deref(),
                    Some(std::path::Path::new("/tmp/photo.jpg"))
                );
                assert_eq!(
                    media.thumbnail.as_deref(),
                    Some(std::path::Path::new("/tmp/thumb.jpg"))
                );
            }
            other => panic!("expected image content, got {other:?}"),
        }

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"reaction","id":"react-1","chat_jid":"123@s.whatsapp.net","sender_jid":"456@s.whatsapp.net","reaction_message_id":"img-1","reaction_emoji":"🔥","timestamp":"2026-06-05T12:01:00Z"}"#
        )?);

        let edited = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::MessageEdited { message } if message.id.as_ref() == "img-1" => {
                    break message;
                }
                _ => continue,
            }
        };
        assert_eq!(edited.reactions.len(), 1);
        assert_eq!(edited.reactions[0].emoji.as_ref(), "🔥");
        assert_eq!(
            edited.reactions[0].senders[0].as_ref(),
            "456@s.whatsapp.net"
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_forwards_history_and_profile_updates() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:history-profile")?;
        let mut events = provider.events();
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"history","id":"old-1","chat_jid":"123@s.whatsapp.net","chat_name":"Ada Lovelace","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","text":"old whatsapp message","timestamp":"2026-06-04T12:00:00Z"}"#
        )?);

        let history_message = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message {
                    message,
                    is_historical,
                } if content_text(&message.content) == "old whatsapp message" => {
                    assert!(is_historical);
                    break message;
                }
                _ => continue,
            }
        };
        assert_eq!(history_message.sender.display_name.as_ref(), "Ada");

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let chats = provider.chats().await?;
        let chat = chats.iter().find(|chat| chat.id == chat_id).unwrap();
        assert_eq!(chat.name.as_ref(), "Ada Lovelace");
        assert_eq!(chat.unread_count, 0);

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"profile","jid":"123@s.whatsapp.net","sender_name":"Ada Updated","avatar_path":"/tmp/ada-updated.jpg"}"#
        )?);

        let updated_message = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message { message, .. }
                    if message.sender.display_name.as_ref() == "Ada Updated" =>
                {
                    break message;
                }
                _ => continue,
            }
        };
        assert_eq!(
            updated_message.sender.avatar.as_deref(),
            Some(std::path::Path::new("/tmp/ada-updated.jpg"))
        );
        let contact = provider
            .contact_info(&arc_str("123@s.whatsapp.net"))
            .await?
            .unwrap();
        assert_eq!(contact.display_name.as_ref(), "Ada Updated");
        assert_eq!(
            contact.avatar.as_deref(),
            Some(std::path::Path::new("/tmp/ada-updated.jpg"))
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_surfaces_qr_payload_as_auth_and_status_message() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:qr")?;
        provider.connect().await?;
        let mut events = provider.events();

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"qr","code":"2@test-qr-payload"}"#
        )?);

        let mut saw_auth = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::AuthRequired(AuthChallenge::QrCode(code))
                    if code.as_ref() == "2@test-qr-payload" =>
                {
                    saw_auth = true;
                    break;
                }
                _ => {}
            }
        }

        let history = provider.history(&arc_str(INBOX_CHAT_ID), None, 10).await?;
        assert!(saw_auth);
        assert!(
            history
                .iter()
                .any(|message| content_text(&message.content).contains("2@test-qr-payload"))
        );
        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_sends_text_through_bridge() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:send")?;
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let sent_id = provider
            .send(&chat_id, Content::Text(arc_str("hello back")), None)
            .await?;
        assert!(sent_id.starts_with("test-sent-"));
        let history = provider.history(&chat_id, None, 10).await?;
        assert_eq!(history.len(), 1);
        assert!(history[0].is_from_me);
        assert_eq!(content_text(&history[0].content), "hello back");

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_sends_media_through_bridge() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("fono-snixembed.log");
        fs::write(&file_path, b"test log")?;
        let provider = WhatsAppProvider::new("test:send-media")?;
        assert!(provider.outbound_capabilities().gif);
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let sent_id = provider
            .send(
                &chat_id,
                Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("fono-snixembed.log"),
                    mime_type: arc_str("text/plain"),
                    size_bytes: Some(8),
                    caption: Some(arc_str("debug log")),
                    local_path: Some(file_path.clone()),
                    thumbnail: None,
                }),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("test-sent-media-"));
        let history = provider.history(&chat_id, None, 10).await?;
        assert_eq!(history.len(), 1);
        assert!(history[0].is_from_me);
        match &history[0].content {
            Content::File(media) => {
                assert_eq!(media.file_name.as_ref(), "fono-snixembed.log");
                assert_eq!(media.mime_type.as_ref(), "text/plain");
                assert_eq!(media.caption.as_deref(), Some("debug log"));
                assert_eq!(media.local_path.as_deref(), Some(file_path.as_path()));
            }
            other => panic!("expected file content, got {other:?}"),
        }
        assert!(
            provider
                .chats()
                .await?
                .iter()
                .any(|chat| chat.last_message_preview.as_deref() == Some("File: debug log"))
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_sends_sticker_through_bridge() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let dir = tempfile::tempdir()?;
        let sticker_path = dir.path().join("shrug.webp");
        fs::write(&sticker_path, b"webp")?;
        let provider = WhatsAppProvider::new("test:send-sticker")?;
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let sent_id = provider
            .send(
                &chat_id,
                Content::Sticker(Media {
                    id: arc_str("sticker-1"),
                    file_name: arc_str("shrug.webp"),
                    mime_type: arc_str("image/webp"),
                    size_bytes: Some(4),
                    caption: None,
                    local_path: Some(sticker_path.clone()),
                    thumbnail: None,
                }),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("test-sent-media-"));
        let history = provider.history(&chat_id, None, 10).await?;
        assert!(matches!(history[0].content, Content::Sticker(_)));

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_media_send_requires_local_path() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:send-media-missing-path")?;
        provider.connect().await?;

        let error = provider
            .send(
                &arc_str("whatsapp:123@s.whatsapp.net"),
                Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("missing.log"),
                    mime_type: arc_str("text/plain"),
                    ..Media::default()
                }),
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("local file path"));

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_sends_reactions_through_bridge() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:react")?;
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"react-target","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","text":"react to this","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let message_id = arc_str("react-target");
        let history = provider.history(&chat_id, None, 10).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap()
            .clone();
        provider.react(&chat_id, &message, "👍").await?;
        let history = provider.history(&chat_id, None, 10).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert!(message_reacted_by_sender(
            message,
            "👍",
            LOCAL_REACTION_SENDER
        ));

        let history = provider.history(&chat_id, None, 10).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap()
            .clone();
        provider.react(&chat_id, &message, "👍").await?;
        let history = provider.history(&chat_id, None, 10).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert!(!message_reacted_by_sender(
            message,
            "👍",
            LOCAL_REACTION_SENDER
        ));

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_logs_bridge_events_only_when_log_path_is_set() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let dir = tempfile::tempdir()?;
        let log_path = dir.path().join("whatsapp-debug.log");

        let provider = WhatsAppProvider::with_options(WhatsAppProviderOptions {
            db_path: "test:logging-enabled".to_owned(),
            sync_scope: "all".to_owned(),
            log_path: Some(log_path.clone()),
        })?;
        provider.connect().await?;
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"log-1","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","text":"logged event"}"#
        )?);
        tokio::time::sleep(Duration::from_millis(10)).await;
        provider.disconnect().await?;

        let logged = fs::read_to_string(&log_path)?;
        assert!(logged.contains("whatsapp-provider"));
        assert!(logged.contains("logged event"));

        let disabled_log_path = dir.path().join("disabled.log");
        let provider = WhatsAppProvider::with_options(WhatsAppProviderOptions {
            db_path: "test:logging-disabled".to_owned(),
            sync_scope: "all".to_owned(),
            log_path: None,
        })?;
        provider.connect().await?;
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"log-2","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","text":"not logged"}"#
        )?);
        tokio::time::sleep(Duration::from_millis(10)).await;
        provider.disconnect().await?;

        assert!(!disabled_log_path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn bridge_forwards_go_callback_into_tokio_channel() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let handle = bridge::new_client("test:callback", "all", None)?;
        assert!(bridge::connect(handle));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let tx = Box::new(tx);
        let tx_ptr = Box::into_raw(tx);

        unsafe {
            bridge::set_message_callback(tx_ptr.cast());
        }
        assert!(bridge::fire_synthetic_message("hello from go")?);

        let received = loop {
            let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await?;
            if let Some(received) = received {
                if received.contains("hello from go") {
                    break received;
                }
            }
        };
        assert!(received.contains("hello from go"));

        unsafe {
            bridge::clear_message_callback();
            drop(Box::from_raw(tx_ptr));
        }
        bridge::disconnect(handle);

        Ok(())
    }
}
