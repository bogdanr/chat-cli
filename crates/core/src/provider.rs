use crate::{events::ProviderEvent, types::*};
use anyhow::bail;
use async_trait::async_trait;
use std::{ops::Range, path::PathBuf, sync::Arc};
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
    /// Whether this identity can deliver outbound @-mentions (e.g. Slack user
    /// or bot tokens can; a Slack incoming webhook cannot). The compose UI uses
    /// this to decide whether to offer the mention autocomplete.
    pub mentions: bool,
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
            mentions: false,
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
            mentions: true,
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
            Content::Image(media) if media.mime_type.as_ref() == "image/gif" => {
                self.gif || self.image
            }
            Content::Image(_) => self.image,
            Content::Video(_) => self.video,
            Content::Audio(_) => self.audio,
            Content::File(_) => self.file,
            Content::Sticker(_) => self.sticker,
            Content::LinkPreview(_) | Content::Cards(_) => self.text,
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
        Content::Cards(_) => "card",
        Content::Poll(_) => "poll",
        Content::Deleted => "deleted message",
        Content::Unsupported(_) => "unsupported content",
    }
}

/// The result of encoding outbound text for a provider: the provider-native
/// text (mention tokens rewritten) plus the mentions that were resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutboundMentions {
    pub text: String,
    pub mentioned: Vec<Mention>,
}

/// An outbound message payload: the content to send plus any resolved mentions.
/// Providers that need a separate mentioned-id list (e.g. WhatsApp's
/// `MentionedJID`) read `mentions`; others ignore it.
#[derive(Clone, Debug)]
pub struct OutboundContent {
    pub content: Content,
    pub mentions: Vec<Mention>,
}

impl OutboundContent {
    /// Wrap content with no mentions.
    pub fn new(content: Content) -> Self {
        Self {
            content,
            mentions: Vec::new(),
        }
    }

    /// Wrap content with an explicit mention list.
    pub fn with_mentions(content: Content, mentions: Vec<Mention>) -> Self {
        Self { content, mentions }
    }
}

impl From<Content> for OutboundContent {
    fn from(content: Content) -> Self {
        Self::new(content)
    }
}

/// A mention token located in outbound text.
#[derive(Clone, Debug)]
pub struct ResolvedMention {
    /// Byte range of the whole `@DisplayName` token in the source text.
    pub range: Range<usize>,
    /// The resolved identity the token refers to.
    pub mention: Mention,
}

/// Scan `text` for `@DisplayName` tokens and resolve them against `members`.
///
/// Matching is case-insensitive and longest-name-first, so a name that is a
/// prefix of another (e.g. "Ada" vs "Ada Lovelace") resolves to the longest
/// match. A token matches only when the `@` starts a word and the name ends on a
/// word boundary, so `user@host` and `@Bogdanr` do not match. Unresolved tokens
/// are simply not returned and are left untouched by callers.
pub fn resolve_mention_tokens(text: &str, members: &[ChatMember]) -> Vec<ResolvedMention> {
    if members.is_empty() || !text.contains('@') {
        return Vec::new();
    }

    // Candidate names as char vectors, longest first so the longest match wins.
    let mut candidates = members
        .iter()
        .filter(|member| !member.sender.display_name.is_empty())
        .map(|member| {
            (
                member.sender.display_name.chars().collect::<Vec<char>>(),
                member,
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0.len()));

    let chars = text.char_indices().collect::<Vec<_>>();
    let mut resolved = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index].1 != '@' {
            index += 1;
            continue;
        }
        // The `@` must start a token (start of text or after a non-word char).
        if index > 0 && chars[index - 1].1.is_alphanumeric() {
            index += 1;
            continue;
        }
        let name_start = index + 1;
        let mut matched: Option<(usize, &ChatMember)> = None;
        for (name, member) in &candidates {
            if name.is_empty() || name_start + name.len() > chars.len() {
                continue;
            }
            let is_match = chars[name_start..name_start + name.len()]
                .iter()
                .zip(name.iter())
                .all(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()));
            if !is_match {
                continue;
            }
            let after = name_start + name.len();
            if after >= chars.len() || !chars[after].1.is_alphanumeric() {
                matched = Some((name.len(), member));
                break;
            }
        }
        let Some((name_len, member)) = matched else {
            index += 1;
            continue;
        };
        let end_char = name_start + name_len;
        let start_byte = chars[index].0;
        let end_byte = chars.get(end_char).map_or(text.len(), |(byte, _)| *byte);
        resolved.push(ResolvedMention {
            range: start_byte..end_byte,
            mention: Mention {
                platform_id: member.sender.platform_id.clone(),
                display_name: member.sender.display_name.clone(),
            },
        });
        index = end_char;
    }
    resolved
}

/// Rewrite resolved mention tokens in `text` using `replacement`, preserving all
/// other text. `resolved` must be in ascending, non-overlapping range order (as
/// produced by [`resolve_mention_tokens`]).
pub fn rewrite_mention_tokens(
    text: &str,
    resolved: &[ResolvedMention],
    mut replacement: impl FnMut(&Mention) -> String,
) -> String {
    if resolved.is_empty() {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for item in resolved {
        if item.range.start < cursor || item.range.end > text.len() {
            continue;
        }
        output.push_str(&text[cursor..item.range.start]);
        output.push_str(&replacement(&item.mention));
        cursor = item.range.end;
    }
    output.push_str(&text[cursor..]);
    output
}

#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// Stable identifier for this account instance (e.g. "whatsapp:+1234567890").
    fn id(&self) -> &ProviderId;

    fn platform(&self) -> Platform;

    fn account_info(&self) -> Account;

    fn config_json(&self) -> Option<String> {
        None
    }

    fn outbound_capabilities(&self) -> OutboundCapabilities {
        OutboundCapabilities::default()
    }

    fn discovery_capabilities(&self) -> DiscoveryCapabilities {
        DiscoveryCapabilities::default()
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

    /// Load history before a concrete anchor message.
    ///
    /// Providers that need the full message identity for pagination can override this.
    async fn history_before_message(
        &self,
        chat_id: &ChatId,
        before_message: &Message,
        limit: usize,
    ) -> anyhow::Result<Vec<Message>> {
        self.history(chat_id, Some(before_message.timestamp), limit)
            .await
    }

    /// Rewrite `@DisplayName` mention tokens in outbound `text` into the
    /// provider-native form, resolving names against the chat's `members`
    /// roster. The default leaves the text unchanged and reports no mentions,
    /// which is correct for providers without outbound mention support.
    fn encode_outbound_mentions(&self, text: &str, _members: &[ChatMember]) -> OutboundMentions {
        OutboundMentions {
            text: text.to_owned(),
            mentioned: Vec::new(),
        }
    }

    /// Send a message. Returns the sent message ID.
    ///
    /// `outbound` carries the content plus any resolved mentions (for providers
    /// that need a separate mentioned-id list). `reply_to` is the full message
    /// being replied to (when any), so providers that need more than the id —
    /// e.g. WhatsApp, which must include the quoted sender and content in its
    /// reply `ContextInfo` — can build a native quote.
    async fn send(
        &self,
        chat_id: &ChatId,
        outbound: OutboundContent,
        reply_to: Option<&Message>,
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

    /// Whether this provider has an official/bundled OAuth application
    /// configured, so the normal "connect workspace" path can run browser
    /// OAuth without the user creating their own app or entering a client
    /// ID/secret. Providers that ship or are configured with official app
    /// credentials should override this to report `true` when those are
    /// available; the setup UI uses it to skip manual app-creation steps.
    fn has_bundled_oauth_app(&self) -> bool {
        false
    }

    /// Whether this provider already has realtime delivery credentials configured
    /// outside the setup UI (for example via launch environment variables or a
    /// bundled/distributor configuration). Setup screens use this to avoid
    /// asking the user to paste redundant realtime-only credentials.
    fn has_configured_realtime(&self) -> bool {
        false
    }

    /// Search messages across this provider.
    async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Message>>;

    /// Discover reachable chats, contacts, users, and channels without adding them to the sidebar.
    async fn discover_destinations(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DiscoveryResult>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        Ok(self
            .chats()
            .await?
            .into_iter()
            .filter(|chat| {
                chat.name.to_lowercase().contains(&query)
                    || chat
                        .last_message_preview
                        .as_deref()
                        .is_some_and(|preview| preview.to_lowercase().contains(&query))
            })
            .take(limit)
            .map(DiscoveryResult::existing_chat)
            .collect())
    }

    /// Get contact/user info for a platform ID.
    async fn contact_info(&self, platform_id: &PlatformId) -> anyhow::Result<Option<Sender>>;

    /// List known members/participants for a chat when the provider supports it.
    async fn chat_members(&self, _chat_id: &ChatId) -> anyhow::Result<Vec<ChatMember>> {
        bail!("chat member listing is not supported by this provider")
    }

    /// Optional rich metadata about a conversation (description, creation,
    /// counts, settings) used to enrich the details pane. The default returns
    /// empty details; providers override it to surface what they can fetch.
    async fn chat_details(&self, _chat_id: &ChatId) -> anyhow::Result<ChatDetails> {
        Ok(ChatDetails::default())
    }

    /// Optional rich profile for a single user/contact used to enrich the
    /// details pane. The default derives a minimal profile from
    /// [`Provider::contact_info`]; providers override it to add title, status,
    /// timezone, about, phone, and similar fields.
    async fn contact_profile(
        &self,
        platform_id: &PlatformId,
    ) -> anyhow::Result<Option<ContactProfile>> {
        Ok(self
            .contact_info(platform_id)
            .await?
            .map(|sender| ContactProfile {
                display_name: Some(sender.display_name),
                ..ContactProfile::default()
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ProviderEvent;
    use anyhow::Result;

    fn member(id: &str, name: &str) -> ChatMember {
        ChatMember::new(Sender {
            platform_id: Arc::from(id),
            display_name: Arc::from(name),
            avatar: None,
        })
    }

    /// A provider that relies on the trait's default `encode_outbound_mentions`
    /// (no mention support), used to pin the pass-through baseline.
    struct PassthroughProvider;

    #[async_trait]
    impl Provider for PassthroughProvider {
        fn id(&self) -> &ProviderId {
            unimplemented!()
        }
        fn platform(&self) -> Platform {
            Platform::Slack
        }
        fn account_info(&self) -> Account {
            unimplemented!()
        }
        async fn connect(&self) -> Result<()> {
            unimplemented!()
        }
        async fn disconnect(&self) -> Result<()> {
            unimplemented!()
        }
        fn is_connected(&self) -> bool {
            false
        }
        fn events(&self) -> broadcast::Receiver<ProviderEvent> {
            unimplemented!()
        }
        async fn chats(&self) -> Result<Vec<Chat>> {
            unimplemented!()
        }
        async fn history(
            &self,
            _chat_id: &ChatId,
            _before: Option<Timestamp>,
            _limit: usize,
        ) -> Result<Vec<Message>> {
            unimplemented!()
        }
        async fn send(
            &self,
            _chat_id: &ChatId,
            _outbound: OutboundContent,
            _reply_to: Option<&Message>,
        ) -> Result<MessageId> {
            unimplemented!()
        }
        async fn download_media(&self, _media: &Media) -> Result<PathBuf> {
            unimplemented!()
        }
        async fn mark_read(&self, _chat_id: &ChatId, _up_to: &MessageId) -> Result<()> {
            unimplemented!()
        }
        async fn react(&self, _chat_id: &ChatId, _message: &Message, _emoji: &str) -> Result<()> {
            unimplemented!()
        }
        async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<Message>> {
            unimplemented!()
        }
        async fn contact_info(&self, _platform_id: &PlatformId) -> Result<Option<Sender>> {
            unimplemented!()
        }
    }

    #[test]
    fn default_encode_outbound_mentions_is_passthrough() {
        let provider = PassthroughProvider;
        let members = vec![member("U1", "Bogdan")];
        let encoded = provider.encode_outbound_mentions("hi @Bogdan", &members);
        assert_eq!(encoded.text, "hi @Bogdan");
        assert!(encoded.mentioned.is_empty());
    }

    #[test]
    fn outbound_content_round_trips_mentions() {
        let mention = Mention {
            platform_id: Arc::from("U1"),
            display_name: Arc::from("Bogdan"),
        };
        let content =
            OutboundContent::with_mentions(Content::Text(Arc::from("hi")), vec![mention.clone()]);
        assert_eq!(content.mentions, vec![mention]);
        assert!(
            OutboundContent::new(Content::Text(Arc::from("hi")))
                .mentions
                .is_empty()
        );
    }

    #[test]
    fn outbound_capabilities_mentions_defaults_false() {
        assert!(!OutboundCapabilities::default().mentions);
        assert!(OutboundCapabilities::all().mentions);
    }

    #[test]
    fn resolve_mention_tokens_matches_longest_name_first() {
        let members = vec![member("U1", "Ada"), member("U2", "Ada Lovelace")];
        let resolved = resolve_mention_tokens("hi @Ada Lovelace and @Ada", &members);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].mention.platform_id.as_ref(), "U2");
        assert_eq!(resolved[1].mention.platform_id.as_ref(), "U1");
    }

    #[test]
    fn resolve_mention_tokens_ignores_emails_and_partial_words() {
        let members = vec![member("U1", "Bogdan")];
        assert!(resolve_mention_tokens("mail user@host", &members).is_empty());
        assert!(resolve_mention_tokens("@Bogdanr", &members).is_empty());
        assert_eq!(resolve_mention_tokens("@bogdan", &members).len(), 1);
    }

    #[test]
    fn rewrite_mention_tokens_replaces_only_matched_ranges() {
        let members = vec![member("U1", "Bogdan")];
        let resolved = resolve_mention_tokens("hey @Bogdan, ping", &members);
        let text = rewrite_mention_tokens("hey @Bogdan, ping", &resolved, |mention| {
            format!("<@{}>", mention.platform_id)
        });
        assert_eq!(text, "hey <@U1>, ping");
    }
}
