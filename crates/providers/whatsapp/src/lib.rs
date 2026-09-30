use anyhow::{Result, bail};
use async_trait::async_trait;
use chat_core::{
    Account, AuthChallenge, Chat, ChatDetails, ChatId, ChatKind, ChatMember, ChatMemberRole,
    ChatMembership, ContactProfile, Content, DiscoveryAction, DiscoveryCapabilities,
    DiscoveryResult, DiscoveryResultKind, EventBus, Media, Mention, Message, MessageId,
    NetworkActivityDirection, NetworkActivityKind, OutboundCapabilities, OutboundContent,
    OutboundMentions, Platform, PlatformData, PlatformId, Poll, PollOption, PollVote, Provider,
    ProviderEvent, ProviderId, Reaction, Sender, Timestamp, WhatsAppData, can_edit_message,
    resolve_mention_tokens, rewrite_mention_tokens,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
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
use tokio::{
    sync::broadcast,
    task::JoinHandle,
    time::{Duration, sleep},
};

const WHATSAPP_ON_DEMAND_HISTORY_WAIT: Duration = Duration::from_millis(12_000);
const WHATSAPP_ON_DEMAND_HISTORY_POLL: Duration = Duration::from_millis(250);
/// Upper bound on how many read receipts a single mark-read call sends.
const WHATSAPP_MARK_READ_MAX_MESSAGES: usize = 100;
/// Mirrors whatsmeow's `EditWindow` (20 minutes after sending).
const WHATSAPP_EDIT_WINDOW_MINUTES: i64 = 20;

pub mod bridge;

const PROVIDER_ID: &str = "whatsapp:bridge";
const INBOX_CHAT_ID: &str = "whatsapp:bridge:inbox";
const BRIDGE_SENDER_ID: &str = "whatsapp:bridge:sender";
const LOCAL_REACTION_SENDER: &str = "me";
const EMPTY_MESSAGE_PLACEHOLDER: &str = "[empty WhatsApp message]";

pub struct WhatsAppProvider {
    handle: bridge::ClientHandle,
    id: ProviderId,
    account: Arc<RwLock<Account>>,
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
        let account = Arc::new(RwLock::new(Account {
            id: id.clone(),
            platform: Platform::WhatsApp,
            display_name: arc_str("WhatsApp"),
            avatar: None,
        }));
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

        let account = Arc::clone(&self.account);
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
                    account: &account,
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
    fn emit_network_activity(
        &self,
        direction: NetworkActivityDirection,
        kind: NetworkActivityKind,
    ) {
        self.events
            .send(ProviderEvent::NetworkActivity { direction, kind });
    }

    fn bridge_call<T, F>(&self, kind: NetworkActivityKind, call: F) -> Result<T>
    where
        F: FnOnce() -> Result<T>,
    {
        self.emit_network_activity(NetworkActivityDirection::Tx, kind);
        let result = call();
        if result.is_ok() {
            self.emit_network_activity(NetworkActivityDirection::Rx, kind);
        }
        result
    }

    fn send_media_to_bridge(
        &self,
        chat_jid: &str,
        media: &Media,
        content_type: &str,
        reply: Option<&bridge::ReplyTarget>,
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
        self.bridge_call(NetworkActivityKind::Media, || {
            bridge::send_media(
                self.handle,
                chat_jid,
                &path,
                media.mime_type.as_ref(),
                file_name,
                caption,
                content_type,
                reply,
            )
        })
    }

    async fn request_history_before_anchor(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        anchor: &Message,
        limit: usize,
        mut page: Vec<Message>,
    ) -> Result<Vec<Message>> {
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() || anchor.id.is_empty() {
            return Ok(page);
        }

        let raw_response = self.bridge_call(NetworkActivityKind::History, || {
            bridge::request_history(
                self.handle,
                &chat_jid,
                anchor.id.as_ref(),
                anchor.is_from_me,
                anchor.timestamp.timestamp(),
                limit,
            )
        })?;
        let response = BridgeEvent::decode(&raw_response)?;
        if response.kind == "error" {
            if page.is_empty() {
                bail!(
                    "{}",
                    response
                        .message
                        .unwrap_or_else(|| "WhatsApp history request failed".to_owned())
                );
            }
            return Ok(page);
        }

        let deadline = tokio::time::Instant::now() + WHATSAPP_ON_DEMAND_HISTORY_WAIT;
        while tokio::time::Instant::now() < deadline {
            sleep(WHATSAPP_ON_DEMAND_HISTORY_POLL).await;
            let updated = history_page_from_cache(&self.messages, chat_id, before, limit);
            if updated.len() > page.len()
                || updated.first().map(|message| message.timestamp)
                    != page.first().map(|message| message.timestamp)
            {
                page = updated;
                if page.len() >= limit {
                    break;
                }
            }
        }

        Ok(page)
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
        self.account
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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
            mentions: true,
            edit: true,
            // whatsmeow's `EditWindow`: WhatsApp rejects edits older than this.
            edit_window: Some(chrono::Duration::minutes(WHATSAPP_EDIT_WINDOW_MINUTES)),
            max_upload_size: None,
            media_note: Some(Arc::from(
                "WhatsApp GIFs may be sent as documents depending on format",
            )),
        }
    }

    /// Rewrite `@DisplayName` tokens into WhatsApp's `@<jid-user>` form and
    /// report the mentioned JIDs so `send` can attach them as `MentionedJID`.
    fn encode_outbound_mentions(&self, text: &str, members: &[ChatMember]) -> OutboundMentions {
        let resolved = resolve_mention_tokens(text, members);
        let mut mentioned: Vec<Mention> = Vec::new();
        for item in &resolved {
            if !mentioned
                .iter()
                .any(|existing| existing.platform_id == item.mention.platform_id)
            {
                mentioned.push(item.mention.clone());
            }
        }
        let text = rewrite_mention_tokens(text, &resolved, |mention| {
            format!("@{}", whatsapp_mention_token(&mention.platform_id))
        });
        OutboundMentions { text, mentioned }
    }

    fn discovery_capabilities(&self) -> DiscoveryCapabilities {
        DiscoveryCapabilities {
            existing_chats: true,
            contacts: true,
            users: false,
            public_channels: false,
            private_channels: false,
            open_dm: false,
            join_public_channel: false,
        }
    }

    async fn connect(&self) -> Result<()> {
        if self.connected.load(Ordering::Acquire) {
            return Ok(());
        }

        self.start_message_forwarder();
        if !self.bridge_call(NetworkActivityKind::Connect, || {
            Ok(bridge::connect(self.handle))
        })? {
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
        Ok(history_page_from_cache(
            &self.messages,
            chat_id,
            before,
            limit,
        ))
    }

    async fn history_before_message(
        &self,
        chat_id: &ChatId,
        before_message: &Message,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let before = Some(before_message.timestamp);
        let page = history_page_from_cache(&self.messages, chat_id, before, limit);
        if page.len() >= limit {
            return Ok(page);
        }
        self.request_history_before_anchor(chat_id, before, before_message, limit, page)
            .await
    }

    async fn send(
        &self,
        chat_id: &ChatId,
        outbound: OutboundContent,
        reply_to: Option<&Message>,
    ) -> Result<MessageId> {
        let OutboundContent { content, mentions } = outbound;
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            bail!("cannot send WhatsApp message to bridge/system chat")
        }
        // whatsmeow rejects recipients that still carry a device/agent suffix
        // (e.g. "<phone>:73@s.whatsapp.net" -> "message recipient must be a user
        // JID with no device part"). Message-routing chat ids can retain that
        // suffix, so normalize to the device-less identity before sending.
        // Group JIDs have no device part, so this is a no-op for them.
        let chat_jid = normalize_whatsapp_jid(&chat_jid);

        let reply = reply_to.map(whatsapp_reply_target);
        let reply = reply.as_ref();

        let mentioned_jids = mentions
            .iter()
            .map(|mention| normalize_whatsapp_jid(&mention.platform_id))
            .collect::<Vec<_>>();

        let raw_response = match &content {
            Content::Text(text) => self.bridge_call(NetworkActivityKind::Send, || {
                bridge::send_text(self.handle, &chat_jid, text, reply, &mentioned_jids)
            })?,
            Content::Image(media) if media.mime_type.as_ref() == "image/gif" => {
                self.send_media_to_bridge(&chat_jid, media, "gif", reply)?
            }
            Content::Image(media) => self.send_media_to_bridge(&chat_jid, media, "image", reply)?,
            Content::Video(media) => self.send_media_to_bridge(&chat_jid, media, "video", reply)?,
            Content::Audio(media) => self.send_media_to_bridge(&chat_jid, media, "audio", reply)?,
            Content::File(media) => self.send_media_to_bridge(&chat_jid, media, "file", reply)?,
            Content::Sticker(media) => {
                self.send_media_to_bridge(&chat_jid, media, "sticker", reply)?
            }
            Content::LinkPreview(_) | Content::Cards(_) => {
                bail!("WhatsApp link preview/card sending should be sent as plain text first")
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
            reply_to: reply_to.map(|message| message.id.clone()),
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            mentions_me: false,
            platform_data: PlatformData {
                whatsapp: Some(WhatsAppData {
                    jid: arc_str(chat_jid),
                }),
                slack: None,
                clickup: None,
                cards: Vec::new(),
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
                activity: Some((timestamp, content_preview(&content))),
                increment_unread: false,
            },
        );
        self.events.send(ProviderEvent::Message {
            message,
            is_historical: false,
        });
        Ok(message_id)
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        message: &Message,
        outbound: OutboundContent,
    ) -> Result<Timestamp> {
        let OutboundContent { content, mentions } = outbound;
        let Content::Text(text) = content else {
            bail!("WhatsApp can only edit the text of a message")
        };
        if text.trim().is_empty() {
            bail!("cannot save an empty WhatsApp message")
        }
        if !message.is_from_me {
            bail!("WhatsApp only allows editing your own messages")
        }
        if !can_edit_message(&self.outbound_capabilities(), message, Utc::now()) {
            bail!(
                "WhatsApp messages can only be edited within {WHATSAPP_EDIT_WINDOW_MINUTES} minutes of sending"
            )
        }
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            bail!("cannot edit messages in the WhatsApp bridge/system chat")
        }
        let chat_jid = normalize_whatsapp_jid(&chat_jid);
        let mentioned_jids = mentions
            .iter()
            .map(|mention| normalize_whatsapp_jid(&mention.platform_id))
            .collect::<Vec<_>>()
            .join(",");

        // The bridge call does network IO; keep it off the async runtime.
        let handle = self.handle;
        let message_id = message.id.to_string();
        let body = text.to_string();
        self.emit_network_activity(NetworkActivityDirection::Tx, NetworkActivityKind::Send);
        let raw_response = tokio::task::spawn_blocking(move || {
            bridge::edit_message(handle, &chat_jid, &message_id, &body, &mentioned_jids)
        })
        .await
        .map_err(|error| anyhow::anyhow!("WhatsApp edit worker failed: {error}"))??;
        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            bail!(
                "{}",
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp edit failed".to_owned())
            );
        }
        self.emit_network_activity(NetworkActivityDirection::Rx, NetworkActivityKind::Send);

        let edited_at = event.edited_at().unwrap_or_else(Utc::now);
        apply_cached_message_edit(
            &self.messages,
            &self.events,
            chat_id.clone(),
            message.id.clone(),
            text,
            edited_at,
        );
        Ok(edited_at)
    }

    async fn download_media(&self, _media: &Media) -> Result<PathBuf> {
        bail!("WhatsApp media download is not wired yet")
    }

    async fn mark_read(&self, chat_id: &ChatId, up_to: &MessageId) -> Result<()> {
        // Clear the cached unread for this chat first so later chat snapshots
        // report it as read instead of re-inflating the count through the
        // sidebar's max() merge.
        let unread_count = {
            let mut chats = lock_rw_write(&self.chats);
            match chats.get_mut(chat_id) {
                Some(chat) => {
                    let unread = chat.unread_count;
                    chat.unread_count = 0;
                    unread
                }
                None => 0,
            }
        };

        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if chat_jid.is_empty() {
            return Ok(());
        }

        // Send read receipts for the most recent inbound messages so the chat
        // also shows as read on the user's other WhatsApp clients. The bridge
        // call does network IO, so it runs detached on a blocking worker and
        // never delays the caller.
        let entries = {
            let messages = lock_rw_read(&self.messages);
            let up_to_timestamp = messages
                .iter()
                .find(|message| message.id == *up_to)
                .map(|message| message.timestamp);
            let limit = (unread_count.max(1) as usize).min(WHATSAPP_MARK_READ_MAX_MESSAGES);
            let mut entries = messages
                .iter()
                .rev()
                .filter(|message| message.chat_id == *chat_id && !message.is_from_me)
                .filter(|message| up_to_timestamp.is_none_or(|up_to| message.timestamp <= up_to))
                .take(limit)
                .map(|message| MarkReadEntry {
                    id: message.id.to_string(),
                    sender_jid: message.sender.platform_id.to_string(),
                })
                .collect::<Vec<_>>();
            entries.reverse();
            entries
        };
        if entries.is_empty() {
            return Ok(());
        }

        let payload = serde_json::to_string(&entries)?;
        let handle = self.handle;
        let events = self.events.clone();
        let log_path = Arc::clone(&self.log_path);
        self.emit_network_activity(NetworkActivityDirection::Tx, NetworkActivityKind::Receipt);
        tokio::task::spawn_blocking(move || {
            let outcome = bridge::mark_read(handle, &chat_jid, &payload)
                .map_err(|error| error.to_string())
                .and_then(|raw| BridgeEvent::decode(&raw).map_err(|error| error.to_string()))
                .and_then(|event| {
                    if event.kind == "error" {
                        Err(event
                            .message
                            .unwrap_or_else(|| "WhatsApp mark-read failed".to_owned()))
                    } else {
                        Ok(())
                    }
                });
            match outcome {
                Ok(()) => {
                    events.send(ProviderEvent::NetworkActivity {
                        direction: NetworkActivityDirection::Rx,
                        kind: NetworkActivityKind::Receipt,
                    });
                }
                Err(error) => {
                    log_provider_event(
                        log_path.as_ref().as_ref(),
                        &format!("mark-read failed chat={chat_jid}: {error}"),
                    );
                }
            }
        });
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
        let raw_response = self.bridge_call(NetworkActivityKind::Reaction, || {
            bridge::send_reaction(
                self.handle,
                &chat_jid,
                &sender_jid,
                message_id.as_ref(),
                reaction,
            )
        })?;
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
        let raw_response = self.bridge_call(NetworkActivityKind::Other, || {
            bridge::send_poll_vote(
                self.handle,
                &chat_jid,
                &sender_jid,
                message_id.as_ref(),
                &option_labels,
            )
        })?;
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
                account: &self.account,
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
        let mut seen = std::collections::HashSet::<String>::new();
        for chat in lock_rw_read(&self.chats).values().cloned() {
            if !whatsapp_chat_matches_query(&chat, &query) {
                continue;
            }
            seen.insert(chat.id.to_string());
            results.push(DiscoveryResult::existing_chat(chat));
            if results.len() >= limit {
                return Ok(results);
            }
        }

        for profile in lock_rw_read(&self.profiles).values().cloned() {
            if results.len() >= limit {
                break;
            }
            if !whatsapp_sender_matches_query(&profile, &query) {
                continue;
            }
            append_whatsapp_contact_result(&mut results, &mut seen, &self.id, profile);
        }

        if results.len() < limit {
            let bridge_limit = limit.saturating_sub(results.len()).max(limit);
            let handle = self.handle;
            let bridge_query = query.clone();
            self.emit_network_activity(NetworkActivityDirection::Tx, NetworkActivityKind::Other);
            let raw_response = tokio::task::spawn_blocking(move || {
                bridge::search_contacts(handle, &bridge_query, bridge_limit)
            })
            .await??;
            if !raw_response.trim_start().starts_with('{') {
                bail!("WhatsApp contact search returned an invalid bridge response");
            }
            self.emit_network_activity(NetworkActivityDirection::Rx, NetworkActivityKind::Other);
            let event = BridgeEvent::decode(&raw_response)?;
            if event.kind == "error" {
                bail!(
                    "{}",
                    event
                        .message
                        .unwrap_or_else(|| "WhatsApp contact search failed".to_owned())
                );
            }

            for contact in event.contacts {
                if results.len() >= limit {
                    break;
                }
                if contact.jid.is_empty() {
                    continue;
                }
                let display_name = if contact.name.is_empty() {
                    sender_name_from_jid(&contact.jid)
                } else {
                    contact.name
                };
                let profile = upsert_profile(
                    &self.profiles,
                    contact.jid,
                    display_name,
                    contact.avatar_path,
                );
                append_whatsapp_contact_result(&mut results, &mut seen, &self.id, profile);
            }
        }

        Ok(results)
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

    async fn chat_members(&self, chat_id: &ChatId) -> Result<Vec<ChatMember>> {
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if !chat_jid.contains("@g.us") {
            bail!("WhatsApp member listing is only available for groups");
        }

        let handle = self.handle;
        let bridge_jid = chat_jid.clone();
        self.emit_network_activity(NetworkActivityDirection::Tx, NetworkActivityKind::Other);
        let raw_response =
            tokio::task::spawn_blocking(move || bridge::group_members(handle, &bridge_jid))
                .await??;
        if !raw_response.trim_start().starts_with('{') {
            bail!("WhatsApp group member listing returned an invalid bridge response");
        }
        self.emit_network_activity(NetworkActivityDirection::Rx, NetworkActivityKind::Other);

        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            bail!(
                "{}",
                event
                    .message
                    .unwrap_or_else(|| "WhatsApp group member listing failed".to_owned())
            );
        }

        let mut members = Vec::with_capacity(event.members.len());
        for member in event.members {
            if member.jid.is_empty() {
                continue;
            }
            let display_name = if member.name.trim().is_empty() {
                sender_name_from_jid(&member.jid)
            } else {
                member.name.clone()
            };
            let sender =
                upsert_profile(&self.profiles, member.jid, display_name, member.avatar_path);
            let role = if member.is_super_admin {
                ChatMemberRole::Owner
            } else if member.is_admin {
                ChatMemberRole::Admin
            } else {
                ChatMemberRole::Member
            };
            members.push(ChatMember::with_role(sender, role));
        }
        Ok(members)
    }

    async fn chat_details(&self, chat_id: &ChatId) -> Result<ChatDetails> {
        let chat_jid = whatsapp_jid_from_chat_id(chat_id);
        if !chat_jid.contains("@g.us") {
            return Ok(ChatDetails::default());
        }

        let handle = self.handle;
        let bridge_jid = chat_jid.clone();
        self.emit_network_activity(NetworkActivityDirection::Tx, NetworkActivityKind::Other);
        let raw_response =
            tokio::task::spawn_blocking(move || bridge::group_members(handle, &bridge_jid))
                .await??;
        if !raw_response.trim_start().starts_with('{') {
            return Ok(ChatDetails::default());
        }
        self.emit_network_activity(NetworkActivityDirection::Rx, NetworkActivityKind::Other);

        let event = BridgeEvent::decode(&raw_response)?;
        if event.kind == "error" {
            return Ok(ChatDetails::default());
        }

        let member_count = u32::try_from(event.members.len()).ok();
        let admin_count = u32::try_from(
            event
                .members
                .iter()
                .filter(|member| member.is_admin || member.is_super_admin)
                .count(),
        )
        .ok();
        let description = event
            .group_topic
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(arc_str);
        let created_at = event
            .group_created
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc));
        let creator = event.group_owner.as_deref().and_then(|owner| {
            let owner = owner.trim();
            if owner.is_empty() {
                return None;
            }
            let resolved = lock_rw_read(&self.profiles)
                .get(owner)
                .map(|sender| sender.display_name.clone());
            Some(resolved.unwrap_or_else(|| arc_str(sender_name_from_jid(owner))))
        });

        Ok(ChatDetails {
            description,
            created_at,
            creator,
            member_count,
            admin_count,
            workspace: None,
            is_archived: false,
            is_externally_shared: false,
            only_admins_can_send: event.group_only_admins_send,
            only_admins_can_edit: event.group_only_admins_edit,
            disappearing_seconds: event
                .group_disappearing_seconds
                .filter(|seconds| *seconds > 0),
            facts: Vec::new(),
        })
    }

    async fn contact_profile(&self, platform_id: &PlatformId) -> Result<Option<ContactProfile>> {
        let display_name = lock_rw_read(&self.profiles)
            .get(platform_id)
            .map(|sender| sender.display_name.clone())
            .or_else(|| {
                lock_rw_read(&self.messages)
                    .iter()
                    .find(|message| message.sender.platform_id == *platform_id)
                    .map(|message| message.sender.display_name.clone())
            });
        let phone = whatsapp_phone_from_jid(platform_id);
        if display_name.is_none() && phone.is_none() {
            return Ok(None);
        }
        Ok(Some(ContactProfile {
            display_name,
            phone,
            ..ContactProfile::default()
        }))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BridgeReaction {
    emoji: String,
    senders: Vec<String>,
}

/// One inbound message acknowledged by a mark-read call, serialized for the
/// Go bridge.
#[derive(Clone, Debug, Serialize)]
struct MarkReadEntry {
    id: String,
    sender_jid: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BridgeContact {
    jid: String,
    name: String,
    avatar_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BridgeMember {
    jid: String,
    #[serde(default)]
    name: String,
    avatar_path: Option<PathBuf>,
    #[serde(default)]
    is_admin: bool,
    #[serde(default)]
    is_super_admin: bool,
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
    canonical_jid: Option<String>,
    alt_jid: Option<String>,
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
    mentions_me: bool,
    #[serde(default)]
    is_group: bool,
    muted: Option<bool>,
    progress: Option<u8>,
    unread_count: Option<u32>,
    last_message_at: Option<String>,
    last_message_preview: Option<String>,
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
    #[serde(default)]
    contacts: Vec<BridgeContact>,
    #[serde(default)]
    members: Vec<BridgeMember>,
    group_topic: Option<String>,
    group_owner: Option<String>,
    group_created: Option<String>,
    #[serde(default)]
    group_only_admins_send: bool,
    #[serde(default)]
    group_only_admins_edit: bool,
    group_disappearing_seconds: Option<u32>,
    edited_at: Option<String>,
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
                canonical_jid: None,
                alt_jid: None,
                chat_jid: Some(BRIDGE_SENDER_ID.to_owned()),
                chat_name: Some("WhatsApp Bridge".to_owned()),
                sender_jid: Some(BRIDGE_SENDER_ID.to_owned()),
                sender_name: Some("WhatsApp Bridge".to_owned()),
                avatar_path: None,
                text: Some(raw_event.to_owned()),
                timestamp: None,
                from_me: false,
                mentions_me: false,
                is_group: false,
                muted: None,
                progress: None,
                unread_count: None,
                last_message_at: None,
                last_message_preview: None,
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
                contacts: Vec::new(),
                members: Vec::new(),
                group_topic: None,
                group_owner: None,
                group_created: None,
                group_only_admins_send: false,
                group_only_admins_edit: false,
                group_disappearing_seconds: None,
                edited_at: None,
            })
        }
    }

    fn timestamp(&self) -> Option<Timestamp> {
        self.timestamp
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }

    fn last_message_timestamp(&self) -> Option<Timestamp> {
        self.last_message_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }

    fn edited_at(&self) -> Option<Timestamp> {
        self.edited_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
    }
}

struct BridgeForwardContext<'a> {
    account: &'a Arc<RwLock<Account>>,
    inbox_chat: &'a Arc<RwLock<Chat>>,
    chats: &'a Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: &'a Arc<RwLock<Vec<Message>>>,
    profiles: &'a Arc<RwLock<HashMap<PlatformId, Sender>>>,
    pending_reactions: &'a Arc<RwLock<HashMap<MessageId, Vec<PendingReaction>>>>,
    log_path: Option<&'a PathBuf>,
    events: &'a EventBus,
    next_message: &'a AtomicU64,
}

fn context_account_id(context: &BridgeForwardContext<'_>) -> ProviderId {
    lock_rw_read(context.account).id.clone()
}

fn update_whatsapp_account_identity(
    account: &Arc<RwLock<Account>>,
    jid: &str,
    avatar: Option<PathBuf>,
) {
    let mut account = lock_rw_write(account);
    account.display_name = arc_str(format!("WhatsApp ({})", sender_name_from_jid(jid)));
    if avatar.is_some() {
        account.avatar = avatar;
    }
}

fn forward_bridge_event(context: &BridgeForwardContext<'_>, raw_event: &str) {
    log_provider_event(context.log_path, raw_event);
    let event = match BridgeEvent::decode(raw_event) {
        Ok(event) => event,
        Err(error) => {
            emit_bridge_status_message(
                &context_account_id(context),
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
                    &context_account_id(context),
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
            // The passkey hybrid/caBLE ceremony emits a second QR (`FIDO:/…`)
            // that the user must scan with their phone's *camera* to authorise
            // the link over Bluetooth. It arrives as a `login` event rather than
            // a top-level `qr` event, so route its `code` through the same
            // AuthRequired(QrCode) path the primary WhatsApp QR uses; otherwise
            // it renders as an unscannable text blob and the link can never
            // complete.
            if detail == "passkey-cable-qr" {
                if let Some(code) = event.code.filter(|code| !code.is_empty()) {
                    context
                        .events
                        .send(ProviderEvent::AuthRequired(AuthChallenge::QrCode(arc_str(
                            &code,
                        ))));
                    emit_bridge_status_message(
                        &context_account_id(context),
                        context.inbox_chat,
                        context.messages,
                        context.events,
                        context.next_message,
                        "WhatsApp passkey: scan this QR with your phone's camera \
                         (not WhatsApp) and approve with your fingerprint/PIN to \
                         finish linking over Bluetooth.",
                    );
                }
                return;
            }
            // The passkey pairing verification code (whatsmeow's
            // PairPasskeyConfirmation). WhatsApp shows this code on the phone
            // and asks the user to check it matches the linking device. Surface
            // it as a PairingCode challenge so it renders in the auth modal;
            // linking auto-confirms on our side, so the modal is dismissed a few
            // seconds later by AuthSucceeded/SyncProgress once pairing completes.
            if detail == "passkey-confirmation" {
                if let Some(code) = event.code.filter(|code| !code.is_empty()) {
                    context
                        .events
                        .send(ProviderEvent::AuthRequired(AuthChallenge::PairingCode(
                            arc_str(&code),
                        )));
                    emit_bridge_status_message(
                        &context_account_id(context),
                        context.inbox_chat,
                        context.messages,
                        context.events,
                        context.next_message,
                        format!(
                            "WhatsApp passkey: verify this code matches the one on your \
                             phone: {code}. Linking confirms automatically in a few seconds."
                        ),
                    );
                }
                return;
            }
            let message = match event.code {
                Some(code) if !code.is_empty() => {
                    format!("WhatsApp login: {detail} (verification code: {code})")
                }
                _ => format!("WhatsApp login: {detail}"),
            };
            emit_bridge_status_message(
                &context_account_id(context),
                context.inbox_chat,
                context.messages,
                context.events,
                context.next_message,
                message,
            );
        }
        "connected" => {
            context.events.send(ProviderEvent::AuthSucceeded);
            if let Some(jid) = event.jid {
                update_whatsapp_account_identity(context.account, &jid, event.avatar_path.clone());
                emit_bridge_status_message(
                    &lock_rw_read(context.account).id,
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
            forward_message_event(context, event, MessageDelivery::Live);
        }
        "history" => {
            forward_message_event(context, event, MessageDelivery::HistorySync);
        }
        "offline" => {
            forward_message_event(context, event, MessageDelivery::OfflineBacklog);
        }
        "sent" => {
            forward_message_event(context, event, MessageDelivery::Live);
        }
        "profile" => {
            forward_profile_event(
                &context_account_id(context),
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
        "edit" => {
            forward_edit_event(context, event);
        }
        "poll_vote" => {
            forward_poll_vote_event(context, event);
        }
        "read" => {
            forward_read_event(context, event);
        }
        "chat_unread" => {
            forward_chat_unread_event(context, event);
        }
        "disconnected" => {
            context
                .events
                .send(ProviderEvent::Disconnected(event.reason.map(arc_str)));
        }
        "error" => {
            emit_bridge_status_message(
                &context_account_id(context),
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
            &context_account_id(context),
            context.inbox_chat,
            context.messages,
            context.events,
            context.next_message,
            format!("WhatsApp bridge event: {}", event.kind),
        ),
    }
}

/// How an incoming bridge message should be treated by the rest of the app.
///
/// WhatsApp surfaces three distinct delivery situations that need different
/// handling for notifications versus unread state:
/// - [`MessageDelivery::Live`]: a message arriving in steady state. It both
///   raises a notification and increments unread.
/// - [`MessageDelivery::HistorySync`]: bulk history replay (initial sync /
///   on-demand backfill). It is silent and does not affect unread, because it
///   reflects already-seen conversation history.
/// - [`MessageDelivery::OfflineBacklog`]: messages the client missed while it
///   was disconnected, replayed by the server on reconnect. It must stay
///   silent (the phone already alerted the user, who may have already read
///   them) yet still increment unread so genuinely-unread messages keep their
///   badge; matching read receipts replayed in the same batch clear the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessageDelivery {
    Live,
    HistorySync,
    OfflineBacklog,
}

impl MessageDelivery {
    /// Whether the message is replayed (not a fresh live arrival). Replayed
    /// messages never raise notifications.
    fn is_historical(self) -> bool {
        !matches!(self, MessageDelivery::Live)
    }

    /// Whether the message should contribute to the chat's unread count.
    fn counts_toward_unread(self) -> bool {
        matches!(
            self,
            MessageDelivery::Live | MessageDelivery::OfflineBacklog
        )
    }
}

fn forward_message_event(
    context: &BridgeForwardContext<'_>,
    event: BridgeEvent,
    delivery: MessageDelivery,
) {
    let is_historical = delivery.is_historical();
    let chat_jid = event
        .chat_jid
        .clone()
        .unwrap_or_else(|| INBOX_CHAT_ID.to_owned());
    let chat_id = chat_id_from_jid(&chat_jid);
    let text = event
        .text
        .clone()
        .unwrap_or_else(|| EMPTY_MESSAGE_PLACEHOLDER.to_owned());
    let timestamp = event.timestamp().unwrap_or_else(Utc::now);
    let message_id = arc_str(event.id.clone().unwrap_or_else(|| {
        format!(
            "whatsapp:bridge:message:{}",
            context.next_message.fetch_add(1, Ordering::Relaxed)
        )
    }));
    let sender_jid = normalize_whatsapp_jid(&event.sender_jid.clone().unwrap_or_else(|| {
        if event.from_me {
            "me"
        } else {
            BRIDGE_SENDER_ID
        }
        .to_owned()
    }));
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
    let is_placeholder_message = is_empty_message_placeholder(&content);
    if !is_placeholder_message {
        let mut message = Message {
            id: message_id.clone(),
            chat_id: chat_id.clone(),
            account: context_account_id(context),
            sender,
            timestamp,
            edited_at: None,
            content,
            reply_to: None,
            thread_id: None,
            reactions: reactions_from_event(&event),
            receipts: Vec::new(),
            is_from_me: event.from_me,
            mentions_me: event.mentions_me,
            platform_data: PlatformData {
                whatsapp: Some(WhatsAppData {
                    jid: arc_str(chat_jid),
                }),
                slack: None,
                clickup: None,
                cards: Vec::new(),
            },
        };
        apply_pending_reactions(context.pending_reactions, &mut message);
        lock_rw_write(context.messages).push(message.clone());
        context.events.send(ProviderEvent::Message {
            message,
            is_historical,
        });
    }
    let chat = upsert_chat_preview(
        &context_account_id(context),
        context.chats,
        ChatPreviewUpdate {
            chat_id,
            name: chat_name_for_event(&event, &sender_jid),
            avatar: event.avatar_path.clone(),
            is_group: event.is_group,
            muted: event.muted,
            activity: (!is_placeholder_message).then_some((timestamp, preview)),
            increment_unread: delivery.counts_toward_unread()
                && !event.from_me
                && !is_placeholder_message,
        },
    );
    context
        .events
        .send(ProviderEvent::ChatUpdated(chat.clone()));
    if let Some(alt_chat_id) = alias_chat_id_for_event(&event) {
        merge_alias_chat(
            &context_account_id(context),
            context.chats,
            context.messages,
            context.events,
            alt_chat_id,
            chat.clone(),
        );
    }
}

/// Handles a bridge "read" event: the chat was read on another WhatsApp
/// client (or this one acknowledged it), so clear the cached unread count and
/// tell consumers without carrying any activity metadata.
fn forward_read_event(context: &BridgeForwardContext<'_>, event: BridgeEvent) {
    let Some(chat_jid) = event.chat_jid.as_deref().filter(|jid| !jid.is_empty()) else {
        return;
    };
    let chat_id = chat_id_from_jid(chat_jid);
    if let Some(chat) = lock_rw_write(context.chats).get_mut(&chat_id) {
        chat.unread_count = 0;
    }
    context
        .events
        .send(ProviderEvent::ChatMarkedRead { chat_id });
}

/// Handles a bridge "chat_unread" event: WhatsApp reported the phone's
/// authoritative unread count for a conversation (history-sync
/// `Conversation.UnreadCount`). This is the source of truth for the chat's
/// read state, so it overwrites the provider's cached count outright — it may
/// raise *or lower* it — and is surfaced as [`ProviderEvent::ChatUnreadSynced`]
/// so the app reconciles any locally-accumulated count that drifted (for
/// example when an offline reconnect re-counted messages the user had already
/// read on their phone).
fn forward_chat_unread_event(context: &BridgeForwardContext<'_>, event: BridgeEvent) {
    let Some(chat_jid) = event.chat_jid.as_deref().filter(|jid| !jid.is_empty()) else {
        return;
    };
    let Some(unread_count) = event.unread_count else {
        return;
    };
    let chat_id = chat_id_from_jid(chat_jid);
    if let Some(chat) = lock_rw_write(context.chats).get_mut(&chat_id) {
        chat.unread_count = unread_count;
    }
    context.events.send(ProviderEvent::ChatUnreadSynced {
        chat_id,
        unread_count,
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
        mentions_me: false,
        platform_data: PlatformData {
            whatsapp: Some(WhatsAppData {
                jid: arc_str(BRIDGE_SENDER_ID),
            }),
            slack: None,
            clickup: None,
            cards: Vec::new(),
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
    activity: Option<(Timestamp, Arc<str>)>,
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
    if let Some((timestamp, preview)) = update.activity {
        let should_update_preview = chat
            .last_message_at
            .is_none_or(|current| timestamp >= current);
        if should_update_preview {
            chat.last_message_at = Some(timestamp);
            chat.last_message_preview = Some(preview);
        }
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
        Content::Cards(cards) => cards
            .first()
            .and_then(|card| card.title.as_deref().or(card.body.as_deref()))
            .unwrap_or("Card"),
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
        Content::Cards(cards) => arc_str(
            cards
                .first()
                .and_then(|card| card.title.as_deref().or(card.body.as_deref()))
                .unwrap_or("Card"),
        ),
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
        .filter(|caption| !caption.is_empty())
        .or((!text.is_empty() && text != EMPTY_MESSAGE_PLACEHOLDER).then_some(text))
        .unwrap_or_default();
    if caption.is_empty() {
        arc_str(label)
    } else {
        arc_str(format!("{label}: {caption}"))
    }
}

fn is_empty_message_placeholder(content: &Content) -> bool {
    matches!(
        content,
        Content::Text(text) | Content::Unsupported(text)
            if text.trim() == EMPTY_MESSAGE_PLACEHOLDER
    )
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
            .filter(|caption| !caption.is_empty())
            .or((!text.is_empty() && text != EMPTY_MESSAGE_PLACEHOLDER).then_some(text.as_str()))
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

/// Routes an incoming `edit` bridge event (made on another device, or replayed
/// by history sync) to a narrow content update of the original message. It
/// never creates a new message, so replayed edits cannot duplicate or reorder
/// the timeline.
fn forward_edit_event(context: &BridgeForwardContext<'_>, event: BridgeEvent) {
    let Some(message_id) = event.id.clone().filter(|id| !id.is_empty()) else {
        return;
    };
    let Some(text) = event.text.clone() else {
        return;
    };
    let chat_jid = event
        .chat_jid
        .clone()
        .unwrap_or_else(|| INBOX_CHAT_ID.to_owned());
    let edited_at = event
        .edited_at()
        .or_else(|| event.timestamp())
        .unwrap_or_else(Utc::now);
    apply_cached_message_edit(
        context.messages,
        context.events,
        chat_id_from_jid(&chat_jid),
        arc_str(message_id),
        arc_str(text),
        edited_at,
    );
}

/// Applies an edit to the in-memory history cache (monotonically, keeping
/// media kind and replacing only text/caption) and emits the narrow
/// `MessageContentEdited` event consumers persist.
fn apply_cached_message_edit(
    messages: &Arc<RwLock<Vec<Message>>>,
    events: &EventBus,
    chat_id: ChatId,
    message_id: MessageId,
    text: Arc<str>,
    edited_at: Timestamp,
) {
    {
        let mut messages = lock_rw_write(messages);
        if let Some(cached) = messages.iter_mut().find(|cached| cached.id == message_id)
            && !matches!(cached.content, Content::Deleted)
            && cached.edited_at.is_none_or(|previous| previous < edited_at)
        {
            match &mut cached.content {
                Content::Image(media)
                | Content::Video(media)
                | Content::Audio(media)
                | Content::File(media)
                | Content::Sticker(media) => media.caption = Some(text.clone()),
                other => *other = Content::Text(text.clone()),
            }
            cached.edited_at = Some(edited_at);
        }
    }
    events.send(ProviderEvent::MessageContentEdited {
        chat_id,
        message_id,
        content: Content::Text(text),
        edited_at,
    });
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
                .map(|sender| arc_str(normalize_whatsapp_jid(sender)))
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
        .map(|sender| normalize_whatsapp_jid(&sender))
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

fn history_page_from_cache(
    messages: &Arc<RwLock<Vec<Message>>>,
    chat_id: &ChatId,
    before: Option<Timestamp>,
    limit: usize,
) -> Vec<Message> {
    let mut messages = lock_rw_read(messages)
        .iter()
        .filter(|message| message.chat_id == *chat_id)
        .filter(|message| before.is_none_or(|before| message.timestamp < before))
        .cloned()
        .collect::<Vec<_>>();
    messages.sort_by_key(|message| message.timestamp);
    let start = messages.len().saturating_sub(limit);
    messages.split_off(start)
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

/// Builds the reply target the bridge needs to attach a WhatsApp reply
/// `ContextInfo`: the quoted message's stanza id, the quoted sender's JID
/// (passed through as-is; "me"/empty is resolved to our own JID inside the
/// bridge), and a plain-text fallback preview of the quoted content.
fn whatsapp_reply_target(message: &Message) -> bridge::ReplyTarget {
    bridge::ReplyTarget {
        id: message.id.to_string(),
        participant: message.sender.platform_id.to_string(),
        quoted_text: whatsapp_quoted_preview(&message.content),
    }
}

/// A short plain-text preview of quoted content, used only as the reply's
/// fallback quote. Recipients resolve the real message by stanza id, so a
/// simple label is sufficient for non-text content.
fn whatsapp_quoted_preview(content: &Content) -> String {
    match content {
        Content::Text(text) => text.to_string(),
        Content::Image(media) => quoted_media_preview(media, "Photo"),
        Content::Video(media) => quoted_media_preview(media, "Video"),
        Content::Audio(_) => "Audio".to_owned(),
        Content::File(media) => quoted_media_preview(media, "Document"),
        Content::Sticker(_) => "Sticker".to_owned(),
        Content::LinkPreview(_) | Content::Cards(_) => String::new(),
        Content::Poll(_) => "Poll".to_owned(),
        Content::Deleted => String::new(),
        Content::Unsupported(_) => String::new(),
    }
}

fn quoted_media_preview(media: &Media, label: &str) -> String {
    media
        .caption
        .as_deref()
        .filter(|caption| !caption.trim().is_empty())
        .map(|caption| caption.to_owned())
        .unwrap_or_else(|| label.to_owned())
}

fn whatsapp_chat_matches_query(chat: &Chat, query: &str) -> bool {
    chat.name.to_lowercase().contains(query)
        || chat
            .last_message_preview
            .as_deref()
            .is_some_and(|preview| preview.to_lowercase().contains(query))
        || chat.id.to_lowercase().contains(query)
}

fn whatsapp_sender_matches_query(sender: &Sender, query: &str) -> bool {
    sender.display_name.to_lowercase().contains(query)
        || sender.platform_id.to_lowercase().contains(query)
}

fn append_whatsapp_contact_result(
    results: &mut Vec<DiscoveryResult>,
    seen: &mut std::collections::HashSet<String>,
    account_id: &ProviderId,
    profile: Sender,
) {
    let chat_id = chat_id_from_jid(profile.platform_id.as_ref());
    if seen.contains(chat_id.as_ref()) {
        return;
    }
    seen.insert(chat_id.to_string());
    results.push(DiscoveryResult {
        account: account_id.clone(),
        platform: Platform::WhatsApp,
        kind: DiscoveryResultKind::Contact,
        action: DiscoveryAction::CreateChat,
        id: arc_str(format!("whatsapp:contact:{}", profile.platform_id)),
        platform_id: profile.platform_id.clone(),
        chat_id: Some(chat_id),
        label: profile.display_name.clone(),
        subtitle: Some(arc_str(profile.platform_id.as_ref())),
        avatar: profile.avatar.clone(),
        chat_kind: Some(ChatKind::Direct),
        membership: ChatMembership::Joined,
        metadata: Default::default(),
    });
}

fn sender_name_from_jid(jid: &str) -> String {
    jid.split('@').next().unwrap_or(jid).to_owned()
}

/// Extract a phone number from an individual WhatsApp JID
/// (`<phone>@s.whatsapp.net`). Returns `None` for group, LID, or non-numeric
/// JIDs where the user part is not a real phone number.
fn whatsapp_phone_from_jid(jid: &str) -> Option<Arc<str>> {
    let (user, server) = jid.split_once('@')?;
    if server != "s.whatsapp.net" {
        return None;
    }
    let digits = user.split(['.', ':']).next().unwrap_or(user);
    if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    Some(arc_str(format!("+{digits}")))
}

/// Normalizes a WhatsApp JID to its device-less (non-AD) form by stripping any
/// `:<device>` suffix from the user part (e.g.
/// `40721274801:42@s.whatsapp.net` -> `40721274801@s.whatsapp.net`).
///
/// Message-routing JIDs carry this device suffix while contact, profile and
/// group-member JIDs never do. Without normalization the same person enters
/// the app under two distinct `platform_id` identities, so a sender's avatar
/// (known only under the device-less member/profile JID) can never be matched
/// to their messages and the message-header avatar silently falls back to the
/// initials placeholder.
fn normalize_whatsapp_jid(jid: &str) -> String {
    let Some((user, server)) = jid.split_once('@') else {
        return jid.to_owned();
    };
    let user = user.split(':').next().unwrap_or(user);
    format!("{user}@{server}")
}

/// The bare user-part token WhatsApp uses inside message text for a mention
/// (e.g. `34819417346247` for `34819417346247@s.whatsapp.net`). Device/agent
/// suffixes are stripped first so the token matches the JID sent alongside it.
fn whatsapp_mention_token(jid: &str) -> String {
    let normalized = normalize_whatsapp_jid(jid);
    normalized
        .split('@')
        .next()
        .unwrap_or(normalized.as_str())
        .to_owned()
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

fn alias_chat_id_for_event(event: &BridgeEvent) -> Option<ChatId> {
    if event.is_group {
        return None;
    }
    let alias_jid = event.alt_jid.as_deref()?;
    let canonical_jid = event
        .canonical_jid
        .as_deref()
        .or(event.chat_jid.as_deref())
        .or(event.jid.as_deref())?;
    if alias_jid.is_empty() || alias_jid == canonical_jid {
        return None;
    }
    Some(chat_id_from_jid(alias_jid))
}

fn merge_alias_chat(
    account_id: &ProviderId,
    chats: &Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: &Arc<RwLock<Vec<Message>>>,
    events: &EventBus,
    from_chat_id: ChatId,
    to_chat: Chat,
) {
    if from_chat_id == to_chat.id {
        return;
    }

    let mut merged_chat = to_chat.clone();
    {
        let mut chats = lock_rw_write(chats);
        if let Some(alias_chat) = chats.remove(&from_chat_id) {
            let canonical = chats
                .entry(to_chat.id.clone())
                .or_insert_with(|| to_chat.clone());
            canonical.unread_count = canonical
                .unread_count
                .saturating_add(alias_chat.unread_count);
            canonical.pinned |= alias_chat.pinned;
            if canonical.avatar.is_none() {
                canonical.avatar = alias_chat.avatar;
            }
            if canonical.last_message_at.is_none_or(|current| {
                alias_chat
                    .last_message_at
                    .is_some_and(|alias_time| alias_time > current)
            }) {
                canonical.last_message_at = alias_chat.last_message_at;
                canonical.last_message_preview = alias_chat.last_message_preview;
            }
            merged_chat = canonical.clone();
        } else {
            chats.insert(to_chat.id.clone(), to_chat.clone());
        }
    }

    let mut changed_messages = Vec::new();
    {
        let mut messages = lock_rw_write(messages);
        for message in messages.iter_mut() {
            if message.account == *account_id && message.chat_id == from_chat_id {
                message.chat_id = merged_chat.id.clone();
                if let Some(whatsapp) = &mut message.platform_data.whatsapp {
                    whatsapp.jid = arc_str(whatsapp_jid_from_chat_id(&merged_chat.id));
                }
                changed_messages.push(message.clone());
            }
        }
    }

    events.send(ProviderEvent::ChatMerged {
        from_chat_id,
        to_chat_id: merged_chat.id.clone(),
        chat: merged_chat,
    });
    for message in changed_messages {
        events.send(ProviderEvent::Message {
            message,
            is_historical: true,
        });
    }
}

fn forward_profile_event(
    account_id: &ProviderId,
    chats: &Arc<RwLock<HashMap<ChatId, Chat>>>,
    messages: &Arc<RwLock<Vec<Message>>>,
    profiles: &Arc<RwLock<HashMap<PlatformId, Sender>>>,
    events: &EventBus,
    event: BridgeEvent,
) {
    let Some(jid) = event
        .jid
        .clone()
        .or_else(|| event.sender_jid.clone())
        .or_else(|| event.chat_jid.clone())
    else {
        return;
    };
    let name = event
        .sender_name
        .clone()
        .or_else(|| event.chat_name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| sender_name_from_jid(&jid));
    let avatar = event.avatar_path.clone();
    let sender = upsert_profile(profiles, jid.clone(), name.clone(), avatar.clone());
    let activity_at = event.last_message_timestamp();
    let activity_preview = event
        .last_message_preview
        .clone()
        .filter(|preview| !preview.is_empty())
        .map(arc_str);

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
            if let Some(timestamp) = activity_at {
                apply_profile_activity(chat, timestamp, activity_preview);
            }
            Some(chat.clone())
        } else {
            let chat = Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str(&name),
                avatar: avatar.clone(),
                is_group: event.is_group,
                kind: if event.is_group {
                    ChatKind::Group
                } else {
                    ChatKind::Direct
                },
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: event.muted.unwrap_or(false),
                pinned: false,
                last_message_at: activity_at,
                last_message_preview: activity_at.and(activity_preview),
                thread_id: None,
            };
            chats.insert(chat_id, chat.clone());
            Some(chat)
        }
    };

    if let Some(updated_chat) = updated_chat {
        events.send(ProviderEvent::ChatUpdated(updated_chat.clone()));
        if let Some(alt_chat_id) = alias_chat_id_for_event(&event) {
            merge_alias_chat(
                account_id,
                chats,
                messages,
                events,
                alt_chat_id,
                updated_chat,
            );
        }
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

/// Applies chat-level last-message activity reported by the bridge (derived
/// from real messages during history sync). Never downgrades newer existing
/// activity; only fills the preview at an equal timestamp when it is missing.
fn apply_profile_activity(chat: &mut Chat, timestamp: Timestamp, preview: Option<Arc<str>>) {
    let is_newer = chat
        .last_message_at
        .is_none_or(|current| timestamp > current);
    if is_newer {
        chat.last_message_at = Some(timestamp);
        if preview.is_some() {
            chat.last_message_preview = preview;
        }
    } else if chat.last_message_at == Some(timestamp) && chat.last_message_preview.is_none() {
        chat.last_message_preview = preview;
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

    #[test]
    fn normalize_whatsapp_jid_strips_device_suffix() {
        // Message-routing JIDs carry a `:<device>` suffix; contact, profile and
        // group-member JIDs never do. Both must collapse to the same identity
        // so a sender's avatar resolves from the member list.
        assert_eq!(
            normalize_whatsapp_jid("40721274801:42@s.whatsapp.net"),
            "40721274801@s.whatsapp.net"
        );
        assert_eq!(
            normalize_whatsapp_jid("34819417346247:42@lid"),
            "34819417346247@lid"
        );
        // Already-canonical JIDs are unchanged.
        assert_eq!(
            normalize_whatsapp_jid("40721274801@s.whatsapp.net"),
            "40721274801@s.whatsapp.net"
        );
        // Group JIDs (the user part contains a `-`, never a device) and bare
        // identifiers without a server are left intact.
        assert_eq!(
            normalize_whatsapp_jid("40723372866-1607983561@g.us"),
            "40723372866-1607983561@g.us"
        );
        assert_eq!(normalize_whatsapp_jid("me"), "me");
    }

    #[test]
    fn reaction_senders_are_normalized_to_device_less_identity() {
        // History-synced reactions can carry a device-suffixed participant JID.
        // It must collapse to the same device-less identity used for message
        // senders and group members so the reactor resolves to a contact name
        // instead of a raw phone number.
        let history = BridgeEvent::decode(
            r#"{"type":"history","id":"reacted","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","text":"reacted","reactions":[{"emoji":"😂","senders":["40745211188:42@s.whatsapp.net"]}]}"#,
        )
        .expect("decode reacted history");
        let reactions = reactions_from_event(&history);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].senders.len(), 1);
        assert_eq!(
            reactions[0].senders[0].as_ref(),
            "40745211188@s.whatsapp.net"
        );

        // Live reaction events expose the sender through `sender_jid`, which is
        // normalized the same way.
        let live = BridgeEvent::decode(
            r#"{"type":"reaction","chat_jid":"123@s.whatsapp.net","reaction_message_id":"abc","reaction_emoji":"😂","sender_jid":"40745211188:42@s.whatsapp.net"}"#,
        )
        .expect("decode live reaction");
        assert_eq!(reaction_sender(&live), "40745211188@s.whatsapp.net");
    }

    #[test]
    fn bridge_event_decodes_qr_messages_history_and_profiles() -> Result<()> {
        let qr = BridgeEvent::decode(r#"{"type":"qr","code":"2@test"}"#)?;
        assert_eq!(qr.kind, "qr");
        assert_eq!(qr.code.as_deref(), Some("2@test"));

        // Passkey device-linking (whatsmeow PR #1186): the bridge reports the
        // authentication phase and, when the server requires it, a pairing
        // verification code carried on the login event's `code` field.
        let authenticating =
            BridgeEvent::decode(r#"{"type":"login","event":"passkey-authenticating"}"#)?;
        assert_eq!(authenticating.kind, "login");
        assert_eq!(
            authenticating.event.as_deref(),
            Some("passkey-authenticating")
        );

        let confirmation = BridgeEvent::decode(
            r#"{"type":"login","event":"passkey-confirmation","code":"ABCD-EF"}"#,
        )?;
        assert_eq!(confirmation.event.as_deref(), Some("passkey-confirmation"));
        assert_eq!(confirmation.code.as_deref(), Some("ABCD-EF"));

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

    #[test]
    fn bridge_event_decodes_mentions_me_flag() -> Result<()> {
        let mentioned = BridgeEvent::decode(
            r#"{"type":"message","id":"m1","chat_jid":"group@g.us","text":"<@me> ping","mentions_me":true,"is_group":true}"#,
        )?;
        assert!(mentioned.mentions_me);
        assert!(mentioned.is_group);

        // Absent flag defaults to false (legacy/non-mention messages).
        let plain = BridgeEvent::decode(
            r#"{"type":"message","id":"m2","chat_jid":"group@g.us","text":"hi","is_group":true}"#,
        )?;
        assert!(!plain.mentions_me);
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
            next_non_network_event(&mut events).await?,
            ProviderEvent::AuthRequired(_)
        ));
        let mut saw_auth = false;
        let mut saw_sync = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(1), next_non_network_event(&mut events))
                .await??
            {
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
                .history(&arc_str("whatsapp:123@s.whatsapp.net"), None, 1)
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
    async fn whatsapp_provider_applies_inbound_edits_without_new_messages() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:inbound-edit")?;
        let mut events = provider.events();
        provider.connect().await?;

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"edit-target","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","text":"teh typo","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"edit","id":"edit-target","chat_jid":"123@s.whatsapp.net","sender_jid":"123@s.whatsapp.net","text":"the typo","timestamp":"2026-06-05T12:05:00Z","edited_at":"2026-06-05T12:05:00Z"}"#
        )?);

        let mut new_messages = 0;
        let (message_id, content, edited_at) = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message { message, .. } if message.id.as_ref() == "edit-target" => {
                    new_messages += 1;
                }
                ProviderEvent::MessageContentEdited {
                    message_id,
                    content,
                    edited_at,
                    ..
                } => break (message_id, content, edited_at),
                _ => continue,
            }
        };
        assert_eq!(new_messages, 1, "an edit must never add a message");
        assert_eq!(message_id.as_ref(), "edit-target");
        assert!(matches!(&content, Content::Text(text) if text.as_ref() == "the typo"));
        assert_eq!(edited_at.to_rfc3339(), "2026-06-05T12:05:00+00:00");

        let history = provider
            .history(&chat_id_from_jid("123@s.whatsapp.net"), None, 10)
            .await?;
        let cached = history
            .iter()
            .find(|message| message.id.as_ref() == "edit-target")
            .expect("original message stays cached");
        assert!(matches!(&cached.content, Content::Text(text) if text.as_ref() == "the typo"));
        assert_eq!(cached.edited_at, Some(edited_at));
        assert_eq!(
            cached.timestamp.to_rfc3339(),
            "2026-06-05T12:00:00+00:00",
            "the edit must not move the message in the timeline"
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_edits_own_messages_within_window() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:outbound-edit")?;
        let mut events = provider.events();
        provider.connect().await?;
        let capabilities = provider.outbound_capabilities();
        assert!(capabilities.edit);
        assert_eq!(
            capabilities.edit_window,
            Some(chrono::Duration::minutes(20))
        );

        let chat_id = chat_id_from_jid("123@s.whatsapp.net");
        let sent_id = provider
            .send(
                &chat_id,
                OutboundContent::new(Content::Text(arc_str("draft"))),
                None,
            )
            .await?;
        let sent = provider
            .history(&chat_id, None, 10)
            .await?
            .into_iter()
            .find(|message| message.id == sent_id)
            .expect("sent message cached");

        provider
            .edit_message(
                &chat_id,
                &sent,
                OutboundContent::new(Content::Text(arc_str("final"))),
            )
            .await?;
        let edited_id = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::MessageContentEdited { message_id, .. } => break message_id,
                _ => continue,
            }
        };
        assert_eq!(edited_id, sent_id);

        // Outside whatsmeow's edit window the bridge is never called.
        let stale = Message {
            timestamp: Utc::now() - chrono::Duration::minutes(30),
            ..sent.clone()
        };
        let error = provider
            .edit_message(
                &chat_id,
                &stale,
                OutboundContent::new(Content::Text(arc_str("late"))),
            )
            .await
            .expect_err("stale edits are rejected");
        assert!(error.to_string().contains("20 minutes"));

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

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"profile","jid":"456@s.whatsapp.net","sender_name":"Grace Hopper"}"#
        )?);

        let direct_chat = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUpdated(chat)
                    if chat.id.as_ref() == "whatsapp:456@s.whatsapp.net" =>
                {
                    break chat;
                }
                _ => continue,
            }
        };
        assert_eq!(direct_chat.name.as_ref(), "Grace Hopper");
        assert_eq!(direct_chat.kind, ChatKind::Direct);
        assert!(
            provider
                .chats()
                .await?
                .iter()
                .any(|chat| chat.id.as_ref() == "whatsapp:456@s.whatsapp.net")
        );

        let discoveries = provider.discover_destinations("Katherine", 5).await?;
        let contact = discoveries
            .iter()
            .find(|result| result.platform_id.as_ref() == "447700900456@s.whatsapp.net")
            .expect("expected bridge contact search result");
        assert_eq!(contact.label.as_ref(), "Katherine Johnson");
        assert_eq!(
            contact.chat_id.as_deref(),
            Some("whatsapp:447700900456@s.whatsapp.net")
        );
        assert_eq!(contact.kind, DiscoveryResultKind::Contact);
        assert_eq!(contact.action, DiscoveryAction::CreateChat);

        let cached = provider
            .contact_info(&arc_str("447700900456@s.whatsapp.net"))
            .await?
            .unwrap();
        assert_eq!(cached.display_name.as_ref(), "Katherine Johnson");

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_chat_unread_sync_overrides_local_count() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:chat-unread-sync")?;
        let mut events = provider.events();
        provider.connect().await?;

        // Two live arrivals inflate the locally-accumulated unread count, as an
        // offline-backlog replay of already-read messages would.
        for id in ["m1", "m2"] {
            assert!(bridge::fire_synthetic_message(&format!(
                r#"{{"type":"message","id":"{id}","chat_jid":"555@s.whatsapp.net","chat_name":"Alan","sender_jid":"555@s.whatsapp.net","sender_name":"Alan","text":"hi {id}","timestamp":"2026-07-13T12:00:00Z"}}"#
            ))?);
        }
        let chat_id = arc_str("whatsapp:555@s.whatsapp.net");
        loop {
            let chats = provider.chats().await?;
            if chats
                .iter()
                .any(|chat| chat.id == chat_id && chat.unread_count == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The phone reports its authoritative unread count of 1; it must lower
        // (not max-merge with) the inflated local count.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"chat_unread","chat_jid":"555@s.whatsapp.net","unread_count":1}"#
        )?);
        let synced = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUnreadSynced {
                    chat_id: id,
                    unread_count,
                } if id == chat_id => break unread_count,
                _ => continue,
            }
        };
        assert_eq!(synced, 1);

        let chats = provider.chats().await?;
        let chat = chats.iter().find(|chat| chat.id == chat_id).unwrap();
        assert_eq!(chat.unread_count, 1);

        // A zero count clears it entirely.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"chat_unread","chat_jid":"555@s.whatsapp.net","unread_count":0}"#
        )?);
        let cleared = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUnreadSynced {
                    chat_id: id,
                    unread_count,
                } if id == chat_id => break unread_count,
                _ => continue,
            }
        };
        assert_eq!(cleared, 0);

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_profile_activity_marks_chats_without_downgrading() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:profile-activity")?;
        let mut events = provider.events();
        provider.connect().await?;

        // History-sync profile event carrying conversation activity creates a
        // chat with real last-message metadata even though no message bodies
        // were replayed (sync scope skipped them).
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"profile","jid":"321@s.whatsapp.net","sender_name":"Recent Contact","last_message_at":"2026-06-09T18:30:00Z","last_message_preview":"see you tomorrow"}"#
        )?);
        let chat = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUpdated(chat)
                    if chat.id.as_ref() == "whatsapp:321@s.whatsapp.net" =>
                {
                    break chat;
                }
                _ => continue,
            }
        };
        let expected_at = chrono::DateTime::parse_from_rfc3339("2026-06-09T18:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(chat.last_message_at, Some(expected_at));
        assert_eq!(
            chat.last_message_preview.as_deref(),
            Some("see you tomorrow")
        );

        // A staler activity snapshot must never downgrade the chat.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"profile","jid":"321@s.whatsapp.net","sender_name":"Recent Contact","last_message_at":"2026-06-01T08:00:00Z","last_message_preview":"old preview"}"#
        )?);
        let chat = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUpdated(chat)
                    if chat.id.as_ref() == "whatsapp:321@s.whatsapp.net" =>
                {
                    break chat;
                }
                _ => continue,
            }
        };
        assert_eq!(chat.last_message_at, Some(expected_at));
        assert_eq!(
            chat.last_message_preview.as_deref(),
            Some("see you tomorrow")
        );

        // Newer metadata-only activity (no preview available) bumps the
        // timestamp but keeps the last known preview text.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"profile","jid":"321@s.whatsapp.net","sender_name":"Recent Contact","last_message_at":"2026-06-10T07:00:00Z"}"#
        )?);
        let chat = loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::ChatUpdated(chat)
                    if chat.id.as_ref() == "whatsapp:321@s.whatsapp.net" =>
                {
                    break chat;
                }
                _ => continue,
            }
        };
        let newer_at = chrono::DateTime::parse_from_rfc3339("2026-06-10T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(chat.last_message_at, Some(newer_at));
        assert_eq!(
            chat.last_message_preview.as_deref(),
            Some("see you tomorrow")
        );

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_offline_backlog_is_silent_but_keeps_unread() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:offline-backlog")?;
        let mut events = provider.events();
        provider.connect().await?;

        // A message replayed during offline-sync catch-up must be flagged
        // historical so the UI never raises an audio/desktop notification for
        // it (the phone already alerted the user)...
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"offline","id":"offline-1","chat_jid":"555@s.whatsapp.net","chat_name":"Backlog Chat","sender_jid":"555@s.whatsapp.net","sender_name":"Pat","text":"missed you","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);

        loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message {
                    message,
                    is_historical,
                } if content_text(&message.content) == "missed you" => {
                    assert!(
                        is_historical,
                        "offline backlog message must be historical (no notification)"
                    );
                    assert!(!message.is_from_me);
                    break;
                }
                _ => continue,
            }
        }

        // ...yet it must still increment unread so a genuinely-unread missed
        // message keeps its badge (unlike bulk history sync, which does not).
        let chat_id = arc_str("whatsapp:555@s.whatsapp.net");
        let unread_chat = provider
            .chats()
            .await?
            .into_iter()
            .find(|chat| chat.id == chat_id)
            .expect("backlog chat present");
        assert_eq!(unread_chat.unread_count, 1);

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_history_sync_does_not_increment_unread() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:history-no-unread")?;
        let mut events = provider.events();
        provider.connect().await?;

        // Bulk history replay reflects already-seen conversation history, so it
        // must neither notify nor inflate unread.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"history","id":"hist-1","chat_jid":"777@s.whatsapp.net","chat_name":"History Chat","sender_jid":"777@s.whatsapp.net","sender_name":"Pat","text":"old line","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);

        loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message {
                    message,
                    is_historical,
                } if content_text(&message.content) == "old line" => {
                    assert!(is_historical, "history sync message must be historical");
                    break;
                }
                _ => continue,
            }
        }

        let chat_id = arc_str("whatsapp:777@s.whatsapp.net");
        let history_chat = provider
            .chats()
            .await?
            .into_iter()
            .find(|chat| chat.id == chat_id)
            .expect("history chat present");
        assert_eq!(history_chat.unread_count, 0);

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_mark_read_clears_cached_unread_count() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:mark-read")?;
        let mut events = provider.events();
        provider.connect().await?;

        // Inbound (not from me) message increments the chat's unread count.
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"message","id":"unread-1","chat_jid":"999@s.whatsapp.net","chat_name":"Unread Chat","sender_jid":"999@s.whatsapp.net","sender_name":"Pat","text":"ping","timestamp":"2026-06-05T12:00:00Z"}"#
        )?);

        loop {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::Message { message, .. }
                    if content_text(&message.content) == "ping" =>
                {
                    assert!(!message.is_from_me);
                    break;
                }
                _ => continue,
            }
        }

        let chat_id = arc_str("whatsapp:999@s.whatsapp.net");
        let unread_chat = provider
            .chats()
            .await?
            .into_iter()
            .find(|chat| chat.id == chat_id)
            .expect("unread chat present");
        assert_eq!(unread_chat.unread_count, 1);

        // Marking the chat read must clear the cached unread so later chat
        // snapshots report it as read instead of re-inflating via the sidebar
        // max() merge.
        provider.mark_read(&chat_id, &arc_str("unread-1")).await?;

        let read_chat = provider
            .chats()
            .await?
            .into_iter()
            .find(|chat| chat.id == chat_id)
            .expect("read chat present");
        assert_eq!(read_chat.unread_count, 0);

        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_fills_history_before_message_with_on_demand_backfill() -> Result<()>
    {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:recent-history-backfill")?;
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        assert!(bridge::fire_synthetic_message(
            r#"{"type":"history","id":"recent-1","chat_jid":"123@s.whatsapp.net","chat_name":"Ada Lovelace","sender_jid":"123@s.whatsapp.net","sender_name":"Ada","text":"recent whatsapp message","timestamp":"2026-06-06T12:00:00Z"}"#
        )?);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let recent_history = provider.history(&chat_id, None, 2).await?;
        assert_eq!(recent_history.len(), 1);

        let history = provider
            .history_before_message(&chat_id, &recent_history[0], 2)
            .await?;

        assert!(history.len() > 1);
        assert!(
            history
                .iter()
                .any(|message| content_text(&message.content).contains("test older history"))
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

        let history = provider.history(&arc_str(INBOX_CHAT_ID), None, 1).await?;
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
    async fn whatsapp_provider_renders_passkey_cable_qr_as_scannable_challenge() -> Result<()> {
        // The passkey hybrid/caBLE second QR arrives as a `login` event, not a
        // top-level `qr` event. It must still surface as an AuthRequired(QrCode)
        // so the TUI renders it scannably; otherwise the user only ever sees the
        // primary WhatsApp QR and the passkey link can never complete.
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:cable-qr")?;
        provider.connect().await?;
        let mut events = provider.events();

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"login","event":"passkey-cable-qr","code":"FIDO:/12345"}"#
        )?);

        let mut saw_auth = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::AuthRequired(AuthChallenge::QrCode(code))
                    if code.as_ref() == "FIDO:/12345" =>
                {
                    saw_auth = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(saw_auth, "caBLE QR must be routed as a scannable QrCode");
        provider.disconnect().await?;
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_provider_surfaces_passkey_confirmation_code() -> Result<()> {
        // WhatsApp's passkey linking shows a verification code on the phone and
        // asks the user to confirm it matches the linking device. It arrives as a
        // `passkey-confirmation` login event and must surface as a PairingCode
        // challenge so the user actually sees it (previously invisible on the
        // SkipHandoffUX auto-confirm path).
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:passkey-confirm")?;
        provider.connect().await?;
        let mut events = provider.events();

        assert!(bridge::fire_synthetic_message(
            r#"{"type":"login","event":"passkey-confirmation","code":"AB12-CD34"}"#
        )?);

        let mut saw_code = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(1), events.recv()).await?? {
                ProviderEvent::AuthRequired(AuthChallenge::PairingCode(code))
                    if code.as_ref() == "AB12-CD34" =>
                {
                    saw_code = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(
            saw_code,
            "passkey confirmation code must surface as a PairingCode challenge"
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
            .send(
                &chat_id,
                OutboundContent::new(Content::Text(arc_str("hello back"))),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("test-sent-"));
        let history = provider.history(&chat_id, None, 1).await?;
        assert_eq!(history.len(), 1);
        assert!(history[0].is_from_me);
        assert_eq!(content_text(&history[0].content), "hello back");

        provider.disconnect().await?;
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
    async fn whatsapp_encodes_mentions_as_phone_tokens_and_reports_jids() -> Result<()> {
        // `new` creates a real bridge client; serialize with the other FFI
        // tests so it cannot race their global synthetic-event hook.
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:mentions")?;
        let members = vec![mention_member("40721274801@s.whatsapp.net", "Bogdan")];

        // The body carries the bare phone number, and the resolved identity is
        // reported so the send path can populate `MentionedJID`.
        let encoded = provider.encode_outbound_mentions("hi @Bogdan", &members);
        assert_eq!(encoded.text, "hi @40721274801");
        assert_eq!(
            encoded
                .mentioned
                .iter()
                .map(|mention| mention.platform_id.as_ref())
                .collect::<Vec<_>>(),
            vec!["40721274801@s.whatsapp.net"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn whatsapp_send_carries_mentioned_jids_through_bridge() -> Result<()> {
        let _guard = ffi_test_guard().await;
        let provider = WhatsAppProvider::new("test:send-mentions")?;
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:123@s.whatsapp.net");
        let mut outbound = OutboundContent::new(Content::Text(arc_str("hi @40721274801")));
        outbound.mentions = vec![Mention {
            platform_id: arc_str("40721274801:42@s.whatsapp.net"),
            display_name: arc_str("Bogdan"),
        }];

        let sent_id = provider.send(&chat_id, outbound, None).await?;
        assert!(sent_id.starts_with("test-sent-"));

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
                OutboundContent::new(Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("fono-snixembed.log"),
                    mime_type: arc_str("text/plain"),
                    size_bytes: Some(8),
                    caption: Some(arc_str("debug log")),
                    local_path: Some(file_path.clone()),
                    thumbnail: None,
                })),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("test-sent-media-"));
        let history = provider.history(&chat_id, None, 1).await?;
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
    async fn whatsapp_send_normalizes_device_suffixed_recipient_jid() -> Result<()> {
        // Regression: forwarding to a contact whose chat id retained a device
        // suffix (e.g. "<phone>:73@s.whatsapp.net") previously reached whatsmeow
        // with the device part and failed with "message recipient must be a user
        // JID with no device part". The recipient must be normalized first.
        let _guard = ffi_test_guard().await;
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("photo.png");
        fs::write(&file_path, b"png-bytes")?;
        let provider = WhatsAppProvider::new("test:send-device-suffix")?;
        provider.connect().await?;

        let chat_id = arc_str("whatsapp:40723372866:73@s.whatsapp.net");
        provider
            .send(
                &chat_id,
                OutboundContent::new(Content::Image(Media {
                    id: arc_str("img-1"),
                    file_name: arc_str("photo.png"),
                    mime_type: arc_str("image/png"),
                    size_bytes: Some(9),
                    caption: None,
                    local_path: Some(file_path.clone()),
                    thumbnail: None,
                })),
                None,
            )
            .await?;

        let history = provider.history(&chat_id, None, 1).await?;
        assert_eq!(history.len(), 1);
        let jid = history[0]
            .platform_data
            .whatsapp
            .as_ref()
            .map(|data| data.jid.as_ref());
        assert_eq!(jid, Some("40723372866@s.whatsapp.net"));

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
                OutboundContent::new(Content::Sticker(Media {
                    id: arc_str("sticker-1"),
                    file_name: arc_str("shrug.webp"),
                    mime_type: arc_str("image/webp"),
                    size_bytes: Some(4),
                    caption: None,
                    local_path: Some(sticker_path.clone()),
                    thumbnail: None,
                })),
                None,
            )
            .await?;
        assert!(sent_id.starts_with("test-sent-media-"));
        let history = provider.history(&chat_id, None, 1).await?;
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
                OutboundContent::new(Content::File(Media {
                    id: arc_str("file-1"),
                    file_name: arc_str("missing.log"),
                    mime_type: arc_str("text/plain"),
                    ..Media::default()
                })),
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
        let history = provider.history(&chat_id, None, 1).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap()
            .clone();
        provider.react(&chat_id, &message, "👍").await?;
        let history = provider.history(&chat_id, None, 1).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap();
        assert!(message_reacted_by_sender(
            message,
            "👍",
            LOCAL_REACTION_SENDER
        ));

        let history = provider.history(&chat_id, None, 1).await?;
        let message = history
            .iter()
            .find(|message| message.id == message_id)
            .unwrap()
            .clone();
        provider.react(&chat_id, &message, "👍").await?;
        let history = provider.history(&chat_id, None, 1).await?;
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
            if let Some(received) = received
                && received.contains("hello from go")
            {
                break received;
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
