use crate::{
    event::AppEvent,
    theme::Theme,
    widgets::{chat_list, message_list},
};
use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use chat_core::{
    Account, AuthChallenge, AuthSubmission, AuthSubmissionMode, Chat, ChatId, Content, Media,
    Message, MessageId, OutboundCapabilities, Platform, PlatformData, Poll, Provider, ProviderEvent,
    ProviderId, Reaction, Sender,
};
use chrono::Utc;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyCode, KeyEvent,
        KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use flate2::read::GzDecoder;
use qrcode::{EcLevel, QrCode, types::Color as QrColor};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui_image::{
    FilterType, Image as TerminalImage, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};
use ratatui_textarea::{Input as TextAreaInput, Key as TextAreaKey, TextArea};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use storage::Store;
use tokio::sync::{broadcast, mpsc};

const IDLE_POLL_TIMEOUT: Duration = Duration::from_millis(250);
const HISTORY_LIMIT: usize = 50;
const MOUSE_SCROLL_STEP: usize = 1;
const MESSAGE_SCROLL_STEP: usize = 3;
const IMAGE_VIEWER_MAX_WIDTH: u16 = 96;
const REACTION_OPTIONS: [&str; 6] = ["👍", "❤️", "😂", "🎉", "😮", "🙏"];
const COMPOSE_EMOTICON_OPTIONS: &[(&str, &str)] = &[
    ("😊", "smile happy"),
    ("😂", "joy laugh tears"),
    ("🤣", "rofl laughing"),
    ("😍", "heart eyes love"),
    ("😘", "kiss"),
    ("🥰", "smiling hearts"),
    ("😎", "cool sunglasses"),
    ("🤔", "thinking"),
    ("😅", "sweat smile"),
    ("😭", "cry sob"),
    ("😢", "sad tear"),
    ("😡", "angry mad"),
    ("😮", "surprised wow"),
    ("🙄", "eyeroll"),
    ("👍", "thumbs up like"),
    ("👎", "thumbs down dislike"),
    ("👏", "clap applause"),
    ("🙌", "raised hands celebrate"),
    ("🙏", "pray thanks please"),
    ("💪", "muscle strong"),
    ("❤️", "heart love"),
    ("💔", "broken heart"),
    ("🔥", "fire hot"),
    ("✨", "sparkles"),
    ("🎉", "party celebrate tada"),
    ("✅", "check done"),
    ("❌", "x no"),
    ("⭐", "star"),
    ("💯", "100 perfect"),
    ("👀", "eyes looking"),
    ("🤷", "person gesture uncertain"),
    ("🤦", "facepalm"),
    ("🚀", "rocket launch"),
    ("☕", "coffee"),
    ("🍕", "pizza"),
    ("🐱", "cat"),
    ("🐶", "dog"),
    ("¯\\_(ツ)_/¯", "shrug ascii kaomoji whatever"),
    ("(╯°□°）╯︵ ┻━┻", "table flip angry rage ascii kaomoji"),
    ("┬─┬ノ( º _ ºノ)", "table unflip fix calm ascii kaomoji"),
    ("(ง'̀-'́)ง", "fight angry square up ascii kaomoji"),
    ("ᕕ( ᐛ )ᕗ", "run happy strut ascii kaomoji"),
    ("(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", "magic sparkle excited ascii kaomoji"),
    ("(づ｡◕‿‿◕｡)づ", "hug cute ascii kaomoji"),
    ("(｡♥‿♥｡)", "love heart eyes ascii kaomoji"),
    ("(ಥ﹏ಥ)", "cry sad tears ascii kaomoji"),
    ("ಠ_ಠ", "disapproval unimpressed judge ascii kaomoji"),
    ("ಠ‿ಠ", "suspicious smug ascii kaomoji"),
    ("(¬_¬)", "side eye suspicious ascii kaomoji"),
    ("( ͡° ͜ʖ ͡°)", "lenny face smirk ascii kaomoji"),
    ("ʕ•ᴥ•ʔ", "bear cute ascii kaomoji"),
    ("ᶘ ᵒᴥᵒᶅ", "otter cute ascii kaomoji"),
    ("(☞ﾟヮﾟ)☞", "point finger right ascii kaomoji"),
    ("☜(ﾟヮﾟ☜)", "point finger left ascii kaomoji"),
    ("(☞ﾟヮﾟ)☞ ☜(ﾟヮﾟ☜)", "finger guns ascii kaomoji"),
    ("ヽ(´▽`)/", "yay happy celebrate ascii kaomoji"),
    ("ヽ(ಠ_ಠ)ノ", "why annoyed ascii kaomoji"),
    ("(ノಠ益ಠ)ノ彡┻━┻", "rage table flip ascii kaomoji"),
    ("┻━┻ ︵ヽ(`Д´)ﾉ︵ ┻━┻", "double table flip rage ascii kaomoji"),
    ("(╥_╥)", "cry sob ascii kaomoji"),
    ("(✿◠‿◠)", "flower happy cute ascii kaomoji"),
    ("(｡◕‿◕｡)", "cute happy smile ascii kaomoji"),
    ("(ﾉ´ヮ`)ﾉ*: ･ﾟ", "celebrate sparkle ascii kaomoji"),
    ("٩(◕‿◕｡)۶", "dance happy ascii kaomoji"),
    ("(￣^￣)ゞ", "salute ascii kaomoji"),
    ("(－‸ლ)", "facepalm ascii kaomoji"),
    ("(っ˘ڡ˘ς)", "food yum ascii kaomoji"),
];
const COMPOSE_EMOTICON_MAX_SUGGESTIONS: usize = 8;
const LOCAL_REACTION_SENDER: &str = "me";
const REACTION_OPTION_CELL_WIDTH: u16 = 6;
const NOTIFICATION_TICKS: u8 = 16;
const HELP_PAGE_STEP: usize = 8;
const HELP_MOUSE_SCROLL_STEP: usize = 3;
const QR_QUIET_ZONE: usize = 2;
const MEDIA_SEND_SIZE_LIMIT_BYTES: u64 = 25 * 1024 * 1024;
const LINK_METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const LINK_METADATA_MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
struct LinkMetadataFetchResult {
    url: Arc<str>,
    metadata: message_list::LinkMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingAttachmentKind {
    Image,
    Video,
    Audio,
    File,
    Sticker,
}

impl PendingAttachmentKind {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::File => "file",
            Self::Sticker => "sticker",
        }
    }
}

#[derive(Clone, Debug)]
struct PendingAttachment {
    kind: PendingAttachmentKind,
    media: Media,
}

impl PendingAttachment {
    fn to_content(&self, caption: Option<Arc<str>>) -> Content {
        let mut media = self.media.clone();
        media.caption = caption;
        match self.kind {
            PendingAttachmentKind::Image => Content::Image(media),
            PendingAttachmentKind::Video => Content::Video(media),
            PendingAttachmentKind::Audio => Content::Audio(media),
            PendingAttachmentKind::File => Content::File(media),
            PendingAttachmentKind::Sticker => Content::Sticker(media),
        }
    }

    fn preview(&self) -> String {
        format!("{}: {}", self.kind.label(), self.media.file_name)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttachCommandKind {
    Auto,
    Image,
    Sticker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposeAttachMenuItem {
    Auto,
    Image,
    Sticker,
    Cancel,
}

impl ComposeAttachMenuItem {
    const ALL: [Self; 4] = [Self::Auto, Self::Image, Self::Sticker, Self::Cancel];

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Attach file from typed path",
            Self::Image => "Attach image/GIF from typed path",
            Self::Sticker => "Attach sticker from typed path",
            Self::Cancel => "Cancel",
        }
    }

    fn attach_command(self) -> Option<AttachCommandKind> {
        match self {
            Self::Auto => Some(AttachCommandKind::Auto),
            Self::Image => Some(AttachCommandKind::Image),
            Self::Sticker => Some(AttachCommandKind::Sticker),
            Self::Cancel => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ComposeAttachMenu {
    selected: usize,
}

pub type ProviderBox = Box<dyn Provider>;

pub async fn run(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<()> {
    let mut app = App::new(store, providers).await?;
    let mut terminal = init_terminal()?;
    app.initialize_image_renderer();
    let result = run_app_loop(&mut terminal, &mut app).await;
    restore_terminal(&mut terminal)?;
    result
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusPane {
    #[default]
    ChatList,
    Messages,
    Compose,
    Details,
}

impl FocusPane {
    fn label(self) -> &'static str {
        match self {
            Self::ChatList => "Chats",
            Self::Messages => "Messages",
            Self::Compose => "Compose",
            Self::Details => "Details",
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::ChatList => Self::ChatList,
            Self::Messages => Self::ChatList,
            Self::Compose => Self::Messages,
            Self::Details => Self::Compose,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::ChatList => Self::Messages,
            Self::Messages => Self::Compose,
            Self::Compose => Self::Details,
            Self::Details => Self::Details,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PaneAreas {
    chat_list: Rect,
    messages: Rect,
    compose: Rect,
    details: Rect,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LayoutMode {
    Compact,
    Medium,
    #[default]
    Wide,
}

impl LayoutMode {
    fn label(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Medium => "medium",
            Self::Wide => "wide",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AppLayout {
    mode: LayoutMode,
    chat_list: Rect,
    messages: Rect,
    compose: Rect,
    details: Rect,
    status: Rect,
}

impl AppLayout {
    fn for_area(area: Rect, compose_height: u16) -> Self {
        if area.is_empty() {
            return Self::default();
        }

        let [body, status] = *Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area)
        else {
            return Self::default();
        };

        match layout_mode(area) {
            LayoutMode::Wide => Self::wide(body, status, compose_height),
            LayoutMode::Medium => Self::medium(body, status, compose_height),
            LayoutMode::Compact => Self::compact(body, status, compose_height),
        }
    }

    fn wide(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [chat_list, center, details] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(45),
                Constraint::Percentage(25),
            ])
            .split(body)
        else {
            return Self::default();
        };
        let [messages, compose] = message_compose_split(center, compose_height);

        Self {
            mode: LayoutMode::Wide,
            chat_list,
            messages,
            compose,
            details,
            status,
        }
    }

    fn medium(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [chat_list, center] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(body)
        else {
            return Self::default();
        };
        let [messages, compose] = message_compose_split(center, compose_height);

        Self {
            mode: LayoutMode::Medium,
            chat_list,
            messages,
            compose,
            details: Rect::default(),
            status,
        }
    }

    fn compact(body: Rect, status: Rect, compose_height: u16) -> Self {
        let [messages, compose] = message_compose_split(body, compose_height);

        Self {
            mode: LayoutMode::Compact,
            chat_list: body,
            messages,
            compose,
            details: Rect::default(),
            status,
        }
    }
}

fn layout_mode(area: Rect) -> LayoutMode {
    if area.width < 72 {
        LayoutMode::Compact
    } else if area.width < 100 {
        LayoutMode::Medium
    } else {
        LayoutMode::Wide
    }
}

fn message_compose_split(area: Rect, compose_height: u16) -> [Rect; 2] {
    let compose_height = compose_height.min(area.height);
    let [messages, compose] = *Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(compose_height)])
        .split(area)
    else {
        return [Rect::default(), Rect::default()];
    };
    [messages, compose]
}

#[derive(Clone, Debug)]
struct ImageViewer {
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ImageProtocolKey {
    path: PathBuf,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionMenuItem {
    Reply,
    ViewThread,
    React,
    VotePoll,
    CopyText,
    OpenImage,
    Cancel,
}

impl ActionMenuItem {
    const ALL: [Self; 7] = [
        Self::Reply,
        Self::ViewThread,
        Self::React,
        Self::VotePoll,
        Self::CopyText,
        Self::OpenImage,
        Self::Cancel,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Reply => "Reply",
            Self::ViewThread => "View thread",
            Self::React => "React",
            Self::VotePoll => "Vote in poll",
            Self::CopyText => "Copy text",
            Self::OpenImage => "Open image",
            Self::Cancel => "Cancel",
        }
    }
}

#[derive(Clone, Debug)]
struct ActionMenu {
    message_id: MessageId,
    selected: usize,
}

#[derive(Clone, Debug)]
struct ReactionPicker {
    message_id: MessageId,
    selected: usize,
}

#[derive(Clone, Debug, Default)]
struct ComposeEmoticonPicker {
    selected: usize,
    query: String,
    matches: Vec<usize>,
    token_char_len: usize,
}

#[derive(Clone, Debug)]
struct PollVotePicker {
    message_id: MessageId,
    selected: usize,
    selected_options: HashSet<usize>,
}

#[derive(Clone, Debug, Default)]
struct HelpOverlay {
    scroll: usize,
}

#[derive(Clone, Debug)]
struct AuthOverlay {
    provider_id: ProviderId,
    challenge: AuthChallenge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlackSetupPhase {
    ChooseWorkspace,
    ChooseAuthMode,
    EnterCredentials,
    OAuthPrompt,
    Validating,
    CapabilityReview,
    Connected,
    Failed,
}

impl SlackSetupPhase {
    fn label(self) -> &'static str {
        match self {
            Self::ChooseWorkspace => "choose workspace",
            Self::ChooseAuthMode => "choose auth method",
            Self::EnterCredentials => "enter credentials",
            Self::OAuthPrompt => "authorize in browser",
            Self::Validating => "validating",
            Self::CapabilityReview => "review capabilities",
            Self::Connected => "connected",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlackSetupMode {
    UserOAuth,
    ReadOnlyOAuth,
    BotToken,
    ImportedToken,
    ManualApp,
    Webhook,
}

impl SlackSetupMode {
    const ALL: [Self; 6] = [
        Self::UserOAuth,
        Self::ReadOnlyOAuth,
        Self::BotToken,
        Self::ImportedToken,
        Self::ManualApp,
        Self::Webhook,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::UserOAuth => "User OAuth",
            Self::ReadOnlyOAuth => "User OAuth read-only",
            Self::BotToken => "Workspace-approved bot/app tokens",
            Self::ImportedToken => "Existing approved token import",
            Self::ManualApp => "Manual Slack app setup",
            Self::Webhook => "Incoming webhook",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::UserOAuth => "Recommended: send as yourself when Slack grants user write scopes.",
            Self::ReadOnlyOAuth => "Fallback: read conversations when write scopes are blocked.",
            Self::BotToken => "Approved deployment: bot identity with optional Socket Mode.",
            Self::ImportedToken => "Advanced: validate a pre-issued user, bot, or app token.",
            Self::ManualApp => "Advanced: configure client ID, secret, redirect URI, and scopes.",
            Self::Webhook => "Limited fallback: send-only webhook/app identity, no inbox.",
        }
    }

    fn credential_hint(self) -> &'static str {
        match self {
            Self::UserOAuth => {
                "OAuth will request user scopes and then validate the resulting user token."
            }
            Self::ReadOnlyOAuth => {
                "OAuth will request read scopes and continue without write capabilities."
            }
            Self::BotToken => {
                "Enter bot token xoxb-... and optional app token xapp-... for realtime later."
            }
            Self::ImportedToken => {
                "Paste a pre-approved xoxp-, xoxb-, or xapp- token for validation."
            }
            Self::ManualApp => "Enter client ID, secret, redirect URI, and scopes before OAuth.",
            Self::Webhook => "Enter a Slack incoming webhook URL for send-only posting.",
        }
    }

    fn to_auth_submission_mode(self) -> AuthSubmissionMode {
        match self {
            Self::UserOAuth => AuthSubmissionMode::UserOAuth,
            Self::ReadOnlyOAuth => AuthSubmissionMode::ReadOnlyOAuth,
            Self::BotToken => AuthSubmissionMode::BotToken,
            Self::ImportedToken => AuthSubmissionMode::ImportedToken,
            Self::ManualApp => AuthSubmissionMode::ManualApp,
            Self::Webhook => AuthSubmissionMode::Webhook,
        }
    }

    fn next_phase(self) -> SlackSetupPhase {
        match self {
            Self::UserOAuth | Self::ReadOnlyOAuth | Self::ManualApp => SlackSetupPhase::OAuthPrompt,
            Self::BotToken | Self::ImportedToken | Self::Webhook => {
                SlackSetupPhase::EnterCredentials
            }
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SlackSetupCapabilities {
    can_read_history: bool,
    can_send_as_user: bool,
    can_send_as_bot: bool,
    can_send_webhook: bool,
    can_react: bool,
    can_download_files: bool,
    can_realtime: bool,
    can_search: bool,
}

impl SlackSetupCapabilities {
    fn from_mode(mode: SlackSetupMode) -> Self {
        match mode {
            SlackSetupMode::UserOAuth => Self {
                can_read_history: true,
                can_send_as_user: true,
                can_react: true,
                can_download_files: true,
                can_search: true,
                ..Self::default()
            },
            SlackSetupMode::ReadOnlyOAuth => Self {
                can_read_history: true,
                can_download_files: true,
                can_search: true,
                ..Self::default()
            },
            SlackSetupMode::BotToken => Self {
                can_read_history: true,
                can_send_as_bot: true,
                can_react: true,
                can_download_files: true,
                can_realtime: true,
                can_search: true,
                ..Self::default()
            },
            SlackSetupMode::ImportedToken | SlackSetupMode::ManualApp => Self {
                can_read_history: true,
                can_send_as_user: true,
                can_send_as_bot: true,
                can_react: true,
                can_download_files: true,
                can_realtime: true,
                can_search: true,
                ..Self::default()
            },
            SlackSetupMode::Webhook => Self {
                can_send_webhook: true,
                ..Self::default()
            },
        }
    }

    fn lines(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("read history", self.can_read_history),
            ("send as user", self.can_send_as_user),
            ("send as bot", self.can_send_as_bot),
            ("send via webhook", self.can_send_webhook),
            ("reactions", self.can_react),
            ("files", self.can_download_files),
            ("realtime", self.can_realtime),
            ("search", self.can_search),
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlackSetupCredentialField {
    UserToken,
    BotToken,
    AppToken,
    WebhookUrl,
    ClientId,
    ClientSecret,
    RedirectUri,
    OAuthCode,
}

impl SlackSetupCredentialField {
    fn label(self) -> &'static str {
        match self {
            Self::UserToken => "User token",
            Self::BotToken => "Bot token",
            Self::AppToken => "App token",
            Self::WebhookUrl => "Webhook URL",
            Self::ClientId => "Client ID",
            Self::ClientSecret => "Client secret",
            Self::RedirectUri => "Redirect URI",
            Self::OAuthCode => "OAuth code",
        }
    }

    fn is_secret(self) -> bool {
        matches!(
            self,
            Self::UserToken
                | Self::BotToken
                | Self::AppToken
                | Self::WebhookUrl
                | Self::ClientSecret
                | Self::OAuthCode
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SlackSetupCredentials {
    user_token: String,
    bot_token: String,
    app_token: String,
    webhook_url: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    oauth_code: String,
}

impl SlackSetupCredentials {
    fn value(&self, field: SlackSetupCredentialField) -> &str {
        match field {
            SlackSetupCredentialField::UserToken => &self.user_token,
            SlackSetupCredentialField::BotToken => &self.bot_token,
            SlackSetupCredentialField::AppToken => &self.app_token,
            SlackSetupCredentialField::WebhookUrl => &self.webhook_url,
            SlackSetupCredentialField::ClientId => &self.client_id,
            SlackSetupCredentialField::ClientSecret => &self.client_secret,
            SlackSetupCredentialField::RedirectUri => &self.redirect_uri,
            SlackSetupCredentialField::OAuthCode => &self.oauth_code,
        }
    }

    fn value_mut(&mut self, field: SlackSetupCredentialField) -> &mut String {
        match field {
            SlackSetupCredentialField::UserToken => &mut self.user_token,
            SlackSetupCredentialField::BotToken => &mut self.bot_token,
            SlackSetupCredentialField::AppToken => &mut self.app_token,
            SlackSetupCredentialField::WebhookUrl => &mut self.webhook_url,
            SlackSetupCredentialField::ClientId => &mut self.client_id,
            SlackSetupCredentialField::ClientSecret => &mut self.client_secret,
            SlackSetupCredentialField::RedirectUri => &mut self.redirect_uri,
            SlackSetupCredentialField::OAuthCode => &mut self.oauth_code,
        }
    }
}

#[derive(Clone, Debug)]
struct SlackSetupOverlay {
    provider_id: ProviderId,
    phase: SlackSetupPhase,
    selected_mode: usize,
    selected_credential_field: usize,
    workspace_label: String,
    credentials: SlackSetupCredentials,
    oauth_url: Option<String>,
    status: Option<String>,
    capabilities: Option<SlackSetupCapabilities>,
}

impl SlackSetupOverlay {
    fn new(provider_id: ProviderId, workspace_label: String) -> Self {
        Self {
            provider_id,
            phase: SlackSetupPhase::ChooseWorkspace,
            selected_mode: 0,
            selected_credential_field: 0,
            workspace_label,
            credentials: SlackSetupCredentials::default(),
            oauth_url: None,
            status: Some("Name this Slack workspace, then choose a sign-in method.".to_owned()),
            capabilities: None,
        }
    }

    fn selected_mode(&self) -> SlackSetupMode {
        SlackSetupMode::ALL
            .get(self.selected_mode)
            .copied()
            .unwrap_or(SlackSetupMode::UserOAuth)
    }

    fn credential_fields(&self) -> &'static [SlackSetupCredentialField] {
        match self.selected_mode() {
            SlackSetupMode::UserOAuth | SlackSetupMode::ReadOnlyOAuth => &[
                SlackSetupCredentialField::UserToken,
                SlackSetupCredentialField::OAuthCode,
            ],
            SlackSetupMode::BotToken => &[
                SlackSetupCredentialField::BotToken,
                SlackSetupCredentialField::AppToken,
            ],
            SlackSetupMode::ImportedToken => &[
                SlackSetupCredentialField::UserToken,
                SlackSetupCredentialField::BotToken,
                SlackSetupCredentialField::AppToken,
            ],
            SlackSetupMode::ManualApp => &[
                SlackSetupCredentialField::ClientId,
                SlackSetupCredentialField::ClientSecret,
                SlackSetupCredentialField::RedirectUri,
                SlackSetupCredentialField::UserToken,
                SlackSetupCredentialField::OAuthCode,
            ],
            SlackSetupMode::Webhook => &[SlackSetupCredentialField::WebhookUrl],
        }
    }

    fn selected_credential_field(&self) -> Option<SlackSetupCredentialField> {
        self.credential_fields()
            .get(self.selected_credential_field)
            .copied()
    }

    fn clamp_credential_selection(&mut self) {
        self.selected_credential_field = self
            .selected_credential_field
            .min(self.credential_fields().len().saturating_sub(1));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AccountConnection {
    Connecting,
    Syncing(u8),
    Online,
    NeedsAuth,
    Reconnecting,
    Offline,
}

impl AccountConnection {
    fn label(&self) -> String {
        match self {
            Self::Connecting => "connecting".to_owned(),
            Self::Syncing(progress) => format!("syncing {progress}%"),
            Self::Online => "online".to_owned(),
            Self::NeedsAuth => "auth needed".to_owned(),
            Self::Reconnecting => "reconnecting".to_owned(),
            Self::Offline => "offline".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountStatus {
    display_name: String,
    connection: AccountConnection,
    detail: Option<String>,
}

impl AccountStatus {
    fn new(account: &Account, connection: AccountConnection) -> Self {
        Self {
            display_name: account.display_name.to_string(),
            connection,
            detail: None,
        }
    }

    fn summary(&self) -> String {
        let mut summary = format!("{} {}", self.display_name, self.connection.label());
        if let Some(detail) = &self.detail
            && !detail.is_empty()
        {
            summary.push_str(": ");
            summary.push_str(detail);
        }
        summary
    }
}

#[derive(Clone, Debug)]
struct AccountSwitcher {
    selected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountOption {
    provider_id: Option<ProviderId>,
    label: String,
    summary: String,
    chat_count: usize,
}

#[derive(Clone, Debug)]
struct NotificationOverlay {
    message_id: MessageId,
    chat_name: String,
    sender_name: String,
    preview: String,
    ticks_remaining: u8,
}

impl NotificationOverlay {
    fn new(chat: &Chat, message: &Message) -> Self {
        Self {
            message_id: message.id.clone(),
            chat_name: chat.name.to_string(),
            sender_name: message.sender.display_name.to_string(),
            preview: notification_preview(message),
            ticks_remaining: NOTIFICATION_TICKS,
        }
    }
}

#[derive(Debug)]
pub struct AppState {
    chats: Vec<Chat>,
    visible_chat_indices: Vec<usize>,
    messages: Vec<Message>,
    selected_chat: usize,
    filter: String,
    filter_mode: bool,
    compose: TextArea<'static>,
    compose_text: String,
    compose_cursor: usize,
    focus: FocusPane,
    layout_mode: LayoutMode,
    pane_areas: PaneAreas,
    media_hits: Vec<message_list::MediaHit>,
    message_hits: Vec<message_list::MessageHit>,
    selected_message_id: Option<MessageId>,
    action_menu: Option<ActionMenu>,
    reaction_picker: Option<ReactionPicker>,
    compose_emoticon_picker: Option<ComposeEmoticonPicker>,
    compose_attach_menu: Option<ComposeAttachMenu>,
    poll_vote_picker: Option<PollVotePicker>,
    help_overlay: Option<HelpOverlay>,
    auth_overlay: Option<AuthOverlay>,
    slack_setup: Option<SlackSetupOverlay>,
    account_switcher: Option<AccountSwitcher>,
    active_account: Option<ProviderId>,
    notification: Option<NotificationOverlay>,
    account_statuses: HashMap<ProviderId, AccountStatus>,
    reply_to: Option<MessageId>,
    pending_attachment: Option<PendingAttachment>,
    thread_root: Option<MessageId>,
    image_viewer: Option<ImageViewer>,
    message_scroll: usize,
    details_scroll: usize,
    is_loading_older_history: bool,
    older_history_exhausted: bool,
    pending_scroll_to_latest: bool,
    frame_area: Rect,
    should_quit: bool,
    status: String,
    pending_history_sync_chat: Option<(ProviderId, ChatId)>,
}

impl Default for AppState {
    fn default() -> Self {
        let mut state = Self {
            chats: Vec::new(),
            visible_chat_indices: Vec::new(),
            messages: Vec::new(),
            selected_chat: 0,
            filter: String::new(),
            filter_mode: false,
            compose: new_compose_textarea(),
            compose_text: String::new(),
            compose_cursor: 0,
            focus: FocusPane::ChatList,
            layout_mode: LayoutMode::Wide,
            pane_areas: PaneAreas::default(),
            media_hits: Vec::new(),
            message_hits: Vec::new(),
            selected_message_id: None,
            action_menu: None,
            reaction_picker: None,
            compose_emoticon_picker: None,
            compose_attach_menu: None,
            poll_vote_picker: None,
            help_overlay: None,
            auth_overlay: None,
            slack_setup: None,
            account_switcher: None,
            active_account: None,
            notification: None,
            account_statuses: HashMap::new(),
            reply_to: None,
            pending_attachment: None,
            thread_root: None,
            image_viewer: None,
            message_scroll: 0,
            details_scroll: 0,
            is_loading_older_history: false,
            older_history_exhausted: false,
            pending_scroll_to_latest: false,
            frame_area: Rect::default(),
            should_quit: false,
            status: String::new(),
            pending_history_sync_chat: None,
        };
        state.sync_compose_cache();
        state
    }
}

impl AppState {
    fn sync_compose_cache(&mut self) {
        self.compose_text = self.compose.lines().join("\n");
        self.compose_cursor = textarea_byte_cursor(&self.compose);
    }

    pub fn chats(&self) -> &[Chat] {
        &self.chats
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn visible_chat_indices(&self) -> &[usize] {
        &self.visible_chat_indices
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn filter_mode(&self) -> bool {
        self.filter_mode
    }

    pub fn focus(&self) -> FocusPane {
        self.focus
    }

    pub fn compose_text(&self) -> &str {
        &self.compose_text
    }

    pub fn compose_cursor(&self) -> usize {
        self.compose_cursor
    }

    fn pending_attachment(&self) -> Option<&PendingAttachment> {
        self.pending_attachment.as_ref()
    }

    pub fn selected_chat_index(&self) -> usize {
        self.selected_chat
    }

    pub fn message_scroll(&self) -> usize {
        self.message_scroll
    }

    pub fn image_viewer_open(&self) -> bool {
        self.image_viewer.is_some()
    }

    pub fn media_hit_count(&self) -> usize {
        self.media_hits.len()
    }

    pub fn selected_message_id(&self) -> Option<&MessageId> {
        self.selected_message_id.as_ref()
    }

    pub fn action_menu_open(&self) -> bool {
        self.action_menu.is_some()
    }

    pub fn reaction_picker_open(&self) -> bool {
        self.reaction_picker.is_some()
    }

    pub fn compose_emoticon_picker_open(&self) -> bool {
        self.compose_emoticon_picker.is_some()
    }

    pub fn compose_attach_menu_open(&self) -> bool {
        self.compose_attach_menu.is_some()
    }

    pub fn help_overlay_open(&self) -> bool {
        self.help_overlay.is_some()
    }

    pub fn help_overlay_scroll(&self) -> Option<usize> {
        self.help_overlay.as_ref().map(|help| help.scroll)
    }

    pub fn account_switcher_open(&self) -> bool {
        self.account_switcher.is_some()
    }

    pub fn slack_setup_open(&self) -> bool {
        self.slack_setup.is_some()
    }

    pub fn slack_setup_phase_label(&self) -> Option<&'static str> {
        self.slack_setup.as_ref().map(|setup| setup.phase.label())
    }

    pub fn active_account(&self) -> Option<&ProviderId> {
        self.active_account.as_ref()
    }

    pub fn account_switcher_selected(&self) -> Option<usize> {
        self.account_switcher
            .as_ref()
            .map(|switcher| switcher.selected)
    }

    pub fn notification_visible(&self) -> bool {
        self.notification.is_some()
    }

    pub fn notification_preview(&self) -> Option<&str> {
        self.notification
            .as_ref()
            .map(|notification| notification.preview.as_str())
    }

    pub fn account_status_summary(&self) -> String {
        account_status_summary(&self.account_statuses)
    }

    pub fn reply_to(&self) -> Option<&MessageId> {
        self.reply_to.as_ref()
    }

    pub fn thread_root(&self) -> Option<&MessageId> {
        self.thread_root.as_ref()
    }

    pub fn selected_chat(&self) -> Option<&Chat> {
        if self.visible_chat_indices.contains(&self.selected_chat) {
            self.chats.get(self.selected_chat)
        } else {
            None
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn provider_for_selected_chat(&self) -> Option<&ProviderId> {
        self.selected_chat().map(|chat| &chat.account)
    }
}

pub struct App {
    providers: Vec<ProviderBox>,
    provider_receivers: Vec<(ProviderId, broadcast::Receiver<ProviderEvent>)>,
    store: Arc<Store>,
    state: AppState,
    media_preview_cache: message_list::MediaPreviewCache,
    link_metadata_cache: message_list::LinkMetadataCache,
    pending_link_metadata_fetches: HashSet<Arc<str>>,
    link_metadata_tx: mpsc::UnboundedSender<LinkMetadataFetchResult>,
    link_metadata_rx: mpsc::UnboundedReceiver<LinkMetadataFetchResult>,
    image_picker: Option<Picker>,
    image_protocol_cache: HashMap<ImageProtocolKey, Result<Protocol, String>>,
    theme: Theme,
}

impl App {
    pub async fn new(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<Self> {
        let provider_receivers = providers
            .iter()
            .map(|provider| (provider.id().clone(), provider.events()))
            .collect();
        let (link_metadata_tx, link_metadata_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            providers,
            provider_receivers,
            store,
            state: AppState::default(),
            media_preview_cache: message_list::MediaPreviewCache::default(),
            link_metadata_cache: message_list::LinkMetadataCache::default(),
            pending_link_metadata_fetches: HashSet::new(),
            link_metadata_tx,
            link_metadata_rx,
            image_picker: None,
            image_protocol_cache: HashMap::new(),
            theme: Theme::default(),
        };
        app.bootstrap().await?;
        Ok(app)
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    fn initialize_image_renderer(&mut self) {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        if picker.protocol_type() == ProtocolType::Halfblocks {
            self.state.status =
                "terminal image protocols unavailable; using block image previews".to_owned();
        }
        self.image_picker = Some(picker);
    }

    pub async fn handle_event(&mut self, event: AppEvent) -> Result<()> {
        match event {
            AppEvent::Key(key) => {
                self.dismiss_notification();
                if self.handle_key(key).await? {
                    self.reload_selected_messages_after_navigation().await?;
                    self.scroll_messages_to_bottom();
                }
            }
            AppEvent::Mouse(mouse) => {
                self.dismiss_notification();
                if self.handle_mouse(mouse).await? {
                    self.reload_selected_messages_after_navigation().await?;
                    self.scroll_messages_to_bottom();
                    if self.state.focus == FocusPane::Messages {
                        self.mark_selected_chat_read().await?;
                    }
                }
            }
            AppEvent::Resize(width, height) => {
                self.dismiss_notification();
                self.image_protocol_cache.clear();
                self.state.status = format!("terminal resized to {width}x{height}");
            }
            AppEvent::Tick => self.handle_tick(),
            AppEvent::Provider(provider_id, event) => {
                self.handle_provider_event(provider_id, *event).await?;
            }
            AppEvent::MediaReady(message_id, path) => {
                self.state.status = format!("media ready for {message_id}: {}", path.display());
            }
        }
        Ok(())
    }

    fn queue_link_metadata_fetches(&mut self, requests: Vec<message_list::LinkPreviewRequest>) {
        for request in requests {
            if self.link_metadata_cache.contains_key(&request.url)
                || !self.pending_link_metadata_fetches.insert(request.url.clone())
            {
                continue;
            }

            let tx = self.link_metadata_tx.clone();
            tokio::spawn(async move {
                let metadata = fetch_link_metadata(request.url.as_ref())
                    .await
                    .unwrap_or_default();
                let _ = tx.send(LinkMetadataFetchResult {
                    url: request.url,
                    metadata,
                });
            });
        }
    }

    fn drain_link_metadata_fetches(&mut self) -> bool {
        let mut changed = false;
        while let Ok(result) = self.link_metadata_rx.try_recv() {
            self.pending_link_metadata_fetches.remove(&result.url);
            self.link_metadata_cache.insert(result.url, result.metadata);
            changed = true;
        }
        changed
    }

    pub async fn drain_provider_events(&mut self) -> Result<bool> {
        let mut events = Vec::new();
        for (provider_id, receiver) in &mut self.provider_receivers {
            loop {
                match receiver.try_recv() {
                    Ok(event) => {
                        events.push(AppEvent::Provider(provider_id.clone(), Box::new(event)))
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                    Err(broadcast::error::TryRecvError::Closed) => break,
                }
            }
        }

        let had_events = !events.is_empty();
        for event in events {
            self.handle_event(event).await?;
        }
        Ok(had_events || self.drain_link_metadata_fetches())
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        self.state.frame_area = frame.area();
        let layout = AppLayout::for_area(frame.area(), self.compose_height(frame.area()));
        self.state.layout_mode = layout.mode;
        self.state.pane_areas = self.visible_pane_areas(layout);
        self.clamp_message_scroll();
        self.clamp_details_scroll();
        self.apply_pending_scroll_to_latest();

        match layout.mode {
            LayoutMode::Compact => self.draw_compact(frame, layout),
            LayoutMode::Medium => {
                self.draw_chat_list(frame, layout.chat_list);
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
            }
            LayoutMode::Wide => {
                self.draw_chat_list(frame, layout.chat_list);
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
                self.draw_details(frame, layout.details);
            }
        }
        self.draw_status_bar(frame, layout.status);
        self.draw_notification_overlay(frame, frame.area());
        self.draw_account_switcher(frame, frame.area());
        self.draw_image_viewer(frame, frame.area());
        self.draw_auth_overlay(frame, frame.area());
        self.draw_slack_setup_overlay(frame, frame.area());
        self.draw_help_overlay(frame, frame.area());
        self.draw_action_menu(frame, frame.area());
        self.draw_reaction_picker(frame, frame.area());
        self.draw_compose_attach_menu(frame, frame.area());
        self.draw_compose_emoticon_picker(frame, frame.area());
        self.draw_poll_vote_picker(frame, frame.area());
    }

    fn visible_pane_areas(&self, layout: AppLayout) -> PaneAreas {
        match layout.mode {
            LayoutMode::Compact => match self.state.focus {
                FocusPane::ChatList => PaneAreas {
                    chat_list: layout.chat_list,
                    ..PaneAreas::default()
                },
                FocusPane::Messages | FocusPane::Compose => PaneAreas {
                    messages: layout.messages,
                    compose: layout.compose,
                    ..PaneAreas::default()
                },
                FocusPane::Details => PaneAreas {
                    details: layout.chat_list,
                    ..PaneAreas::default()
                },
            },
            LayoutMode::Medium => PaneAreas {
                chat_list: layout.chat_list,
                messages: layout.messages,
                compose: layout.compose,
                details: Rect::default(),
            },
            LayoutMode::Wide => PaneAreas {
                chat_list: layout.chat_list,
                messages: layout.messages,
                compose: layout.compose,
                details: layout.details,
            },
        }
    }

    fn draw_compact(&mut self, frame: &mut Frame<'_>, layout: AppLayout) {
        match self.state.focus {
            FocusPane::ChatList => self.draw_chat_list(frame, layout.chat_list),
            FocusPane::Messages | FocusPane::Compose => {
                self.draw_messages(frame, layout.messages);
                self.draw_compose(frame, layout.compose);
            }
            FocusPane::Details => self.draw_details(frame, layout.chat_list),
        }
    }

    fn draw_chat_list(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let avatar_rows = self.chat_avatar_rows();
        chat_list::render_chat_list(
            frame,
            area,
            chat_list::ChatListProps {
                chats: &self.state.chats,
                visible_chat_indices: &self.state.visible_chat_indices,
                selected_chat_index: self.state.selected_chat,
                filter: &self.state.filter,
                filter_mode: self.state.filter_mode,
                account_filter: &self.account_filter_label(),
                focused: self.state.focus == FocusPane::ChatList,
                avatar_rows: &avatar_rows,
                theme: self.theme,
            },
        );
        self.draw_vertical_scrollbar(
            frame,
            area,
            chat_list::content_height(&self.state.chats, &self.state.visible_chat_indices),
            chat_list::scroll_position(
                &self.state.chats,
                &self.state.visible_chat_indices,
                self.state.selected_chat,
                area,
            ),
        );
    }

    fn chat_avatar_rows(&mut self) -> HashMap<usize, chat_list::AvatarRows> {
        let mut rows = HashMap::new();
        let rendered_chat_indices = chat_list::rendered_chat_indices(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            self.state.pane_areas.chat_list,
        );
        for chat_index in rendered_chat_indices {
            let Some(path) = self
                .state
                .chats
                .get(chat_index)
                .and_then(|chat| chat.avatar.as_deref())
                .filter(|path| path.exists())
                .map(Path::to_path_buf)
            else {
                continue;
            };

            if let Ok(avatar) = message_list::cached_image_preview_rows(
                &path,
                &mut self.media_preview_cache,
                chat_list::CHAT_AVATAR_WIDTH,
                chat_list::CHAT_AVATAR_ROWS,
            ) {
                rows.insert(chat_index, avatar);
            }
        }
        rows
    }

    fn draw_messages(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let title = self
            .state
            .selected_chat()
            .map(|chat| format!("Messages - {}", chat.name))
            .unwrap_or_else(|| "Messages".to_owned());
        let total_lines = if self.state.messages.is_empty() {
            0
        } else {
            self.message_line_count()
        };
        let lines = if self.state.messages.is_empty() {
            self.state.media_hits.clear();
            vec![Line::from(Span::styled(
                "No messages yet. Open or click this chat to sync today's messages.",
                self.theme.muted(),
            ))]
        } else {
            let render = message_list::build_message_lines(
                &self.state.messages,
                area.width.saturating_sub(2),
                self.state.message_scroll,
                area.height.saturating_sub(2) as usize,
                self.state.selected_message_id.as_deref(),
                &self.unread_message_ids(),
                &mut self.media_preview_cache,
                &self.link_metadata_cache,
                self.theme,
            );
            self.state.media_hits = render.media_hits;
            self.state.message_hits = render.message_hits;
            self.queue_link_metadata_fetches(render.link_preview_requests);
            render.lines
        };
        message_list::render_message_list(
            frame,
            area,
            message_list::MessageListProps {
                title: &title,
                lines,
                total_lines,
                scroll: self.state.message_scroll,
                focused: self.state.focus == FocusPane::Messages,
                theme: self.theme,
            },
        );
    }

    fn compose_height(&self, area: Rect) -> u16 {
        if area.height < 8 {
            return area.height.min(3);
        }

        let content_lines = self.state.compose.lines().len() as u16;
        let reply_extra = u16::from(self.state.reply_to.is_some());
        let attachment_extra = u16::from(self.state.pending_attachment.is_some());
        let wrapped_extra = self
            .state
            .compose
            .lines()
            .iter()
            .map(|line| {
                let width = area.width.saturating_div(2).max(24) as usize;
                line.chars().count().saturating_div(width)
            })
            .sum::<usize>() as u16;
        (content_lines + wrapped_extra + reply_extra + attachment_extra + 2).clamp(3, 8)
    }

    fn draw_compose(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let title = self
            .state
            .selected_chat()
            .map(|chat| format!("Compose - {}", chat.name))
            .unwrap_or_else(|| "Compose".to_owned());
        let is_focused = self.state.focus == FocusPane::Compose;
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(self.theme.focus_border(is_focused));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let editor_area = {
            let mut constraints = Vec::new();
            if self.state.reply_to.is_some() {
                constraints.push(Constraint::Length(1));
            }
            if self.state.pending_attachment.is_some() {
                constraints.push(Constraint::Length(1));
            }
            constraints.push(Constraint::Min(0));

            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(inner);
            let mut area_index = 0;

            if let Some(reply_to) = &self.state.reply_to {
                let preview = self
                    .message_by_id(reply_to)
                    .map(reply_preview)
                    .unwrap_or_else(|| format!("Replying to {reply_to}"));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("Replying · ", self.theme.status_key()),
                        Span::styled(preview, self.theme.muted()),
                        Span::raw("  "),
                        Span::styled("Esc cancels", self.theme.status_key()),
                    ])),
                    areas[area_index],
                );
                area_index += 1;
            }

            if let Some(attachment) = self.state.pending_attachment() {
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("Attached · ", self.theme.status_key()),
                        Span::styled(attachment.preview(), self.theme.muted()),
                        Span::raw("  "),
                        Span::styled("Esc removes", self.theme.status_key()),
                    ])),
                    areas[area_index],
                );
                area_index += 1;
            }

            areas[area_index]
        };
        let mut compose = self.state.compose.clone();
        compose.remove_block();
        compose.set_style(Style::default().fg(self.theme.foreground));
        compose.set_cursor_line_style(if is_focused {
            Style::default().bg(Color::Black)
        } else {
            Style::default()
        });
        compose.set_cursor_style(if is_focused {
            Style::default().fg(Color::Black).bg(self.theme.accent)
        } else {
            Style::default()
        });
        compose.set_placeholder_text("Type a message...");
        compose.set_placeholder_style(self.theme.muted());
        frame.render_widget(&compose, editor_area);
    }

    fn draw_compose_attach_menu(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(menu) = &self.state.compose_attach_menu else {
            return;
        };

        let modal = self.compose_attach_menu_rect(area);
        if modal.is_empty() {
            return;
        }

        let mut lines = vec![Line::from(Span::styled(
            "Attach from typed path",
            self.theme.pane_title(),
        ))];
        for (index, item) in ComposeAttachMenuItem::ALL.iter().enumerate() {
            let selected = index == menu.selected;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}", item.label()),
                style,
            )));
        }
        lines.push(Line::from(Span::styled(
            "Type or paste a path in compose first · Enter selects · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title("Attach")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_details(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        if let Some(thread_root) = &self.state.thread_root {
            self.draw_thread_details(frame, area, thread_root);
            return;
        }

        if let Some(message_id) = &self.state.selected_message_id
            && let Some(message) = self.message_by_id(message_id).cloned()
        {
            self.draw_message_details(frame, area, &message);
            return;
        }

        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| chat.name.as_ref())
            .unwrap_or("None");
        let filter = if self.state.filter.is_empty() {
            "none".to_owned()
        } else {
            self.state.filter.clone()
        };
        let mode = if self.state.filter_mode {
            "filtering chats"
        } else {
            "normal"
        };
        let details = vec![
            Line::from(format!("Selected: {selected_chat}")),
            Line::from(format!(
                "Focus: {} · Mode: {mode}",
                self.state.focus.label()
            )),
            Line::from(format!("Layout: {}", self.state.layout_mode.label())),
            Line::from(format!("Account: {}", self.state.account_status_summary())),
            Line::from(format!("Account filter: {}", self.account_filter_label())),
            Line::from(format!("Filter: {filter}")),
            Line::from(format!(
                "Chats: {}/{} · Messages: {}",
                self.state.visible_chat_indices.len(),
                self.state.chats.len(),
                self.state.messages.len()
            )),
            Line::from(format!(
                "Message scroll: line {}",
                self.state.message_scroll + 1
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Discoverable controls",
                self.theme.pane_title(),
            )),
            Line::from("  Arrows: move/edit text"),
            Line::from("  Click/tap: focus/open"),
            Line::from("  Scroll/trackpad: browse"),
            Line::from("  Click compose: type"),
            Line::from("  Enter: send"),
            Line::from("  Esc: back/close/clear"),
            Line::from("  Ctrl+A: account filter"),
            Line::from("  Ctrl+F: text filter"),
            Line::from("  PageUp/PageDown: faster"),
            Line::from("  Home/End: edges"),
            Line::from("  Ctrl+Q: quit"),
        ];
        let details_len = details.len();
        let paragraph = Paragraph::new(details)
            .block(
                Block::default()
                    .title("Details")
                    .borders(Borders::ALL)
                    .border_style(
                        self.theme
                            .focus_border(self.state.focus == FocusPane::Details),
                    ),
            )
            .scroll((self.state.details_scroll.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, area);
        self.draw_vertical_scrollbar(frame, area, details_len, self.state.details_scroll);
    }

    fn draw_message_details(
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        message: &Message,
    ) {
        let chat_name = self
            .state
            .chats
            .iter()
            .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            .map(|chat| chat.name.as_ref())
            .unwrap_or("Unknown chat");
        let reply = message
            .reply_to
            .as_ref()
            .map(|id| short_id(id).to_string())
            .unwrap_or_else(|| "none".to_owned());
        let thread = message
            .thread_id
            .as_ref()
            .map(|id| short_id(id).to_string())
            .unwrap_or_else(|| "none".to_owned());
        let avatar_rows = message
            .sender
            .avatar
            .as_deref()
            .filter(|path| path.exists())
            .and_then(|path| {
                message_list::cached_image_preview_rows(
                    path,
                    &mut self.media_preview_cache,
                    area.width.saturating_sub(4).clamp(1, 16),
                    6,
                )
                .ok()
            });
        let avatar_status = if message.sender.avatar.is_some() {
            "avatar preview"
        } else {
            "not loaded"
        };
        let reactions = if message.reactions.is_empty() {
            "none".to_owned()
        } else {
            let reaction_sender_names = reaction_sender_names(&self.state.messages);
            message
                .reactions
                .iter()
                .map(|reaction| {
                    let senders = reaction
                        .senders
                        .iter()
                        .map(|sender| {
                            reaction_sender_names
                                .get(sender)
                                .cloned()
                                .unwrap_or_else(|| reaction_sender_fallback(sender))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{} {}", reaction.emoji, senders)
                })
                .collect::<Vec<_>>()
                .join("  ")
        };

        let mut lines = vec![
            Line::from(Span::styled("Message", self.theme.pane_title())),
            Line::from(Span::styled(
                "Click avatar to preview · Enter opens actions",
                self.theme.muted(),
            )),
            Line::from(""),
            Line::from(format!("Chat: {chat_name}")),
            Line::from(format!("Sender: {}", message.sender.display_name)),
            Line::from(format!("Avatar: {avatar_status}")),
            Line::from(format!(
                "Time: {}",
                message_list::format_message_datetime(message.timestamp)
            )),
            Line::from(format!("From me: {}", bool_label(message.is_from_me))),
            Line::from(format!("Message ID: {}", message.id)),
            Line::from(format!("Reply to: {reply}")),
            Line::from(format!("Thread: {thread}")),
            Line::from(format!("Reactions: {reactions}")),
            Line::from(""),
            Line::from(Span::styled("Content", self.theme.status_key())),
        ];

        if let Some(avatar_rows) = avatar_rows {
            lines.extend(avatar_rows.into_iter().map(Line::from));
            lines.push(Line::from(""));
        }

        let content = content_copy_text(&message.content);
        if content.trim().is_empty() {
            lines.push(Line::from(Span::styled("  attachment", self.theme.muted())));
        } else {
            for line in content.lines() {
                lines.push(Line::from(format!("  {line}")));
            }
        }
        if let Content::Poll(poll) = &message.content {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Poll results",
                self.theme.status_key(),
            )));
            lines.extend(poll_result_lines(
                poll,
                &reaction_sender_names(&self.state.messages),
                self.theme,
            ));
        }

        let content_len = lines.len();
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Details")
                    .borders(Borders::ALL)
                    .border_style(
                        self.theme
                            .focus_border(self.state.focus == FocusPane::Details),
                    ),
            )
            .scroll((self.state.details_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        self.draw_vertical_scrollbar(frame, area, content_len, self.state.details_scroll);
    }

    fn draw_thread_details(
        &self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        thread_root: &MessageId,
    ) {
        let root = self.message_by_id(thread_root);
        let replies = self.thread_replies(thread_root);
        let mut lines = Vec::new();

        lines.push(Line::from(Span::styled("Thread", self.theme.pane_title())));
        lines.push(Line::from(Span::styled(
            "Esc closes thread · Reply starts from message actions",
            self.theme.muted(),
        )));
        lines.push(Line::from(""));

        if let Some(root) = root {
            lines.push(Line::from(Span::styled(
                "Original",
                self.theme.status_key(),
            )));
            lines.extend(thread_message_lines(root, self.theme));
        } else {
            lines.push(Line::from(Span::styled(
                format!("Original message {} is not loaded", short_id(thread_root)),
                self.theme.muted(),
            )));
        }

        lines.push(Line::from(""));
        let reply_title = if replies.len() == 1 {
            "1 reply".to_owned()
        } else {
            format!("{} replies", replies.len())
        };
        lines.push(Line::from(Span::styled(
            reply_title,
            self.theme.status_key(),
        )));
        if replies.is_empty() {
            lines.push(Line::from(Span::styled(
                "No replies yet. Choose Reply from message actions to start one.",
                self.theme.muted(),
            )));
        } else {
            for reply in replies {
                lines.extend(thread_message_lines(reply, self.theme));
            }
        }

        let content_len = lines.len();
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Thread")
                    .borders(Borders::ALL)
                    .border_style(
                        self.theme
                            .focus_border(self.state.focus == FocusPane::Details),
                    ),
            )
            .scroll((self.state.details_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        self.draw_vertical_scrollbar(frame, area, content_len, self.state.details_scroll);
    }

    fn draw_status_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.is_empty() {
            return;
        }

        let hints = self.status_hints();
        let mut spans = Vec::new();
        spans.push(Span::styled(
            format!(" {} ", self.state.focus.label()),
            self.theme.status_key(),
        ));
        for hint in hints {
            spans.push(Span::styled(" · ", self.theme.muted()));
            spans.push(hint);
        }
        if !self.state.status.is_empty() {
            spans.push(Span::styled(" · ", self.theme.muted()));
            spans.push(Span::styled(
                format!("{} ", self.state.status),
                self.theme.muted(),
            ));
        }

        let paragraph = Paragraph::new(Line::from(spans)).style(self.theme.status_bar());
        frame.render_widget(paragraph, area);
    }

    fn status_hints(&self) -> Vec<Span<'static>> {
        let hint = |value: &'static str| Span::styled(value, self.theme.status_bar());

        if self.state.help_overlay.is_some() {
            return vec![
                hint("Help"),
                hint("Scroll or PageUp/PageDown"),
                hint("Esc closes"),
            ];
        }

        if self.state.account_switcher.is_some() {
            return vec![
                hint("Choose account"),
                hint("Arrow keys move"),
                hint("Enter applies"),
                hint("Esc cancels"),
            ];
        }

        if self.state.action_menu.is_some() {
            return vec![
                hint("Choose an action"),
                hint("Arrow keys move"),
                hint("Enter selects"),
                hint("Esc cancels"),
            ];
        }

        if self.state.reaction_picker.is_some() {
            return vec![
                hint("Choose reaction"),
                hint("Arrow keys move"),
                hint("Enter toggles"),
                hint("Esc cancels"),
            ];
        }

        if self.state.compose_attach_menu.is_some() {
            return vec![
                hint("Choose attachment type"),
                hint("Arrow keys move"),
                hint("Enter selects"),
                hint("Esc cancels"),
            ];
        }

        if self.state.thread_root.is_some() {
            return vec![
                hint("Thread open"),
                hint("Esc closes"),
                hint("Reply from actions"),
            ];
        }

        if self.state.filter_mode {
            return vec![
                hint("Type to filter"),
                hint("Backspace deletes"),
                hint("Enter or Esc finishes"),
            ];
        }

        match self.state.focus {
            FocusPane::ChatList => vec![
                hint("↑↓ choose chat"),
                hint("Enter or click opens"),
                hint("Ctrl+A account filter"),
                hint("Ctrl+F text filter"),
                hint("? help"),
            ],
            FocusPane::Messages => {
                if self.state.selected_message_id.is_some() {
                    vec![
                        hint("↑↓ selects messages"),
                        hint("Enter opens actions"),
                        hint("Esc clears selection"),
                        hint("? help"),
                    ]
                } else {
                    vec![
                        hint("Click messages to select"),
                        hint("Scroll/PageUp: browse; top loads older"),
                        hint("Type to reply"),
                        hint("? help"),
                    ]
                }
            }
            FocusPane::Compose => {
                let mut hints = vec![
                    hint("type :emoji"),
                    hint("paste a file path + Enter attaches"),
                    hint("Enter sends text otherwise"),
                    hint("F1 help"),
                ];
                if self.state.reply_to.is_some() {
                    hints.push(hint("Esc cancels reply"));
                } else {
                    hints.push(hint("Esc returns to messages"));
                }
                hints
            }
            FocusPane::Details => vec![
                hint("Scroll/PageUp/PageDown"),
                hint("← returns"),
                hint("? help"),
            ],
        }
    }

    fn draw_vertical_scrollbar(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        content_len: usize,
        position: usize,
    ) {
        let viewport = inner_area(area).height as usize;
        if area.width < 3 || area.height < 3 || content_len <= viewport.max(1) {
            return;
        }

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_style(Style::default().fg(self.theme.muted))
            .track_style(Style::default().fg(Color::Black))
            .begin_symbol(None)
            .end_symbol(None);
        let mut state = ScrollbarState::new(content_len)
            .position(position)
            .viewport_content_length(viewport);
        frame.render_stateful_widget(scrollbar, area, &mut state);
    }

    fn draw_action_menu(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(menu) = &self.state.action_menu else {
            return;
        };

        let modal = self.action_menu_rect(area, &menu.message_id);
        if modal.is_empty() {
            return;
        }

        let mut lines = vec![Line::from(Span::styled(
            "Message actions",
            self.theme.pane_title(),
        ))];
        for (index, item) in ActionMenuItem::ALL.iter().enumerate() {
            let selected = index == menu.selected;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}", item.label()),
                style,
            )));
        }
        lines.push(Line::from(Span::styled(
            "Enter selects · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title("Actions")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_reaction_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.state.reaction_picker else {
            return;
        };

        let modal = self.reaction_picker_rect(area, &picker.message_id);
        if modal.is_empty() {
            return;
        }

        let reacted_by_me = self
            .message_by_id(&picker.message_id)
            .map(|message| {
                REACTION_OPTIONS
                    .iter()
                    .map(|emoji| message_reacted_by_sender(message, emoji, LOCAL_REACTION_SENDER))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![false; REACTION_OPTIONS.len()]);

        let mut spans = vec![Span::raw(" ")];
        for (index, emoji) in REACTION_OPTIONS.iter().enumerate() {
            let under_cursor = index == picker.selected;
            let already_selected = reacted_by_me.get(index).copied().unwrap_or_default();
            let label = if under_cursor {
                format!("›{emoji}‹")
            } else {
                format!(" {emoji} ")
            };
            let style = match (under_cursor, already_selected) {
                (true, true) => Style::default()
                    .fg(Color::Black)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                (true, false) => self.theme.status_key(),
                (false, true) => Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
                (false, false) => self.theme.status_bar(),
            };
            spans.push(Span::styled(label, style));
            if index + 1 < REACTION_OPTIONS.len() {
                spans.push(Span::raw(" "));
            }
        }

        let paragraph = Paragraph::new(vec![
            Line::from(Span::styled("Choose reaction", self.theme.pane_title())),
            Line::from(spans),
            Line::from(Span::styled(
                "Enter toggles selected reaction",
                self.theme.muted(),
            )),
        ])
        .block(
            Block::default()
                .title("React")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_compose_emoticon_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.state.compose_emoticon_picker else {
            return;
        };

        let modal = self.compose_emoticon_picker_rect(area);
        if modal.is_empty() {
            return;
        }

        let mut lines = vec![Line::from(vec![
            Span::styled("Emoji suggestions ", self.theme.pane_title()),
            Span::styled(format!(":{}", picker.query), self.theme.status_key()),
        ])];
        for (row, option_index) in picker.matches.iter().copied().enumerate() {
            let (value, label) = COMPOSE_EMOTICON_OPTIONS[option_index];
            let selected = row == picker.selected;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{value} {label}"),
                style,
            )));
        }
        lines.push(Line::from(Span::styled(
            "Enter/Tab inserts · Esc cancels · keep typing to narrow",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title("Emoji")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_poll_vote_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.state.poll_vote_picker else {
            return;
        };
        let Some(message) = self.message_by_id(&picker.message_id) else {
            return;
        };
        let Content::Poll(poll) = &message.content else {
            return;
        };

        let modal = self.poll_vote_picker_rect(area, picker);
        if modal.is_empty() {
            return;
        }

        let selectable = poll.selectable_options_count.unwrap_or(1).max(1) as usize;
        let mut lines = vec![
            Line::from(Span::styled(
                truncate_chars(&poll.question, modal.width.saturating_sub(4) as usize),
                self.theme.pane_title(),
            )),
            Line::from(Span::styled(
                if selectable == 1 {
                    "Choose one option"
                } else {
                    "Space toggles · Enter submits"
                },
                self.theme.muted(),
            )),
        ];
        for (index, option) in poll.options.iter().enumerate() {
            let under_cursor = index == picker.selected;
            let checked = picker.selected_options.contains(&index);
            let marker = if checked { "[x]" } else { "[ ]" };
            let prefix = if under_cursor { "›" } else { " " };
            let votes = poll_vote_count(poll, &option.id);
            let label = format!("{prefix} {marker} {} ({votes})", option.label);
            let style = if under_cursor {
                self.theme.status_key()
            } else if checked {
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(label, style)));
        }
        lines.push(Line::from(Span::styled(
            "Enter submits · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .title("Vote")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        );
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_slack_setup_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(setup) = &self.state.slack_setup else {
            return;
        };
        if area.width < 44 || area.height < 14 {
            return;
        }

        let modal = self.slack_setup_overlay_rect(area);
        let mut lines = vec![
            Line::from(Span::styled("Slack sign-in", self.theme.pane_title())),
            Line::from(format!("Workspace: {}", setup.workspace_label)),
            Line::from(format!("Provider: {}", setup.provider_id)),
            Line::from(format!("Step: {}", setup.phase.label())),
            Line::from(""),
        ];

        match setup.phase {
            SlackSetupPhase::ChooseWorkspace => {
                lines.extend([
                    Line::from("Name this Slack workspace so multiple workspaces stay separate."),
                    Line::from(format!("Workspace label: {}", setup.workspace_label)),
                    Line::from(Span::styled(
                        "Type to edit · Backspace delete · Enter continue",
                        self.theme.muted(),
                    )),
                ]);
            }
            SlackSetupPhase::ChooseAuthMode => {
                lines.push(Line::from(Span::styled(
                    "Choose a setup method, ordered by robustness:",
                    self.theme.status_key(),
                )));
                for (index, mode) in SlackSetupMode::ALL.iter().copied().enumerate() {
                    let selected = index == setup.selected_mode;
                    let prefix = if selected { "› " } else { "  " };
                    let style = if selected {
                        self.theme.status_key()
                    } else {
                        self.theme.status_bar()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{}. {}", index + 1, mode.label()),
                        style,
                    )));
                    if selected {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", mode.description()),
                            self.theme.muted(),
                        )));
                    }
                }
            }
            SlackSetupPhase::EnterCredentials => {
                let mode = setup.selected_mode();
                lines.extend([
                    Line::from(Span::styled(mode.label(), self.theme.status_key())),
                    Line::from(mode.credential_hint()),
                    Line::from(""),
                    Line::from(Span::styled("Credential fields", self.theme.status_key())),
                ]);
                for (index, field) in setup.credential_fields().iter().copied().enumerate() {
                    let selected = index == setup.selected_credential_field;
                    let prefix = if selected { "› " } else { "  " };
                    let style = if selected {
                        self.theme.status_key()
                    } else {
                        self.theme.status_bar()
                    };
                    let display_value = slack_setup_display_value(
                        setup.credentials.value(field),
                        field.is_secret(),
                    );
                    lines.push(Line::from(Span::styled(
                        format!("{prefix}{}: {display_value}", field.label()),
                        style,
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Type to edit selected field · Tab/↑/↓ switch fields · Backspace delete · Enter validate",
                    self.theme.muted(),
                )));
            }
            SlackSetupPhase::OAuthPrompt => {
                lines.push(Line::from(Span::styled(
                    setup.selected_mode().label(),
                    self.theme.status_key(),
                )));
                if let Some(url) = &setup.oauth_url {
                    lines.push(Line::from("Open this Slack authorization URL:"));
                    lines.push(Line::from(truncate_chars(
                        url,
                        modal.width.saturating_sub(6) as usize,
                    )));
                } else {
                    lines.push(Line::from(
                        "OAuth URL will appear here once Slack app settings are available.",
                    ));
                }
                lines.push(Line::from(Span::styled(
                    "After browser authorization, the setup flow validates the workspace and capabilities.",
                    self.theme.muted(),
                )));
            }
            SlackSetupPhase::Validating => {
                lines.extend([
                    Line::from(Span::styled(
                        "Validating Slack credentials",
                        self.theme.status_key(),
                    )),
                    Line::from("Checking identity, workspace, and granted capabilities..."),
                ]);
            }
            SlackSetupPhase::CapabilityReview | SlackSetupPhase::Connected => {
                lines.push(Line::from(Span::styled(
                    "Capabilities detected for this workspace:",
                    self.theme.status_key(),
                )));
                if let Some(capabilities) = &setup.capabilities {
                    for (label, enabled) in capabilities.lines() {
                        let marker = if enabled { "yes" } else { "no" };
                        lines.push(Line::from(format!("  {label}: {marker}")));
                    }
                } else {
                    lines.push(Line::from("  Waiting for Slack validation results."));
                }
            }
            SlackSetupPhase::Failed => {
                lines.push(Line::from(Span::styled(
                    "Slack setup failed",
                    self.theme.status_key(),
                )));
                lines.push(Line::from(
                    setup
                        .status
                        .clone()
                        .unwrap_or_else(|| "No failure detail was provided.".to_owned()),
                ));
            }
        }

        if let Some(status) = &setup.status
            && !status.is_empty()
            && setup.phase != SlackSetupPhase::Failed
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(status.clone(), self.theme.muted())));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Tab fields · 1-6 quick select · Enter continue · Esc hide",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Slack setup")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_auth_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(overlay) = &self.state.auth_overlay else {
            return;
        };
        if area.width < 34 || area.height < 10 {
            return;
        }

        let modal = self.auth_overlay_rect(area);
        let mut lines = Vec::new();
        let inner_height = modal.height.saturating_sub(2) as usize;
        match &overlay.challenge {
            AuthChallenge::QrCode(code) => {
                lines.extend([
                    Line::from(Span::styled(
                        "WhatsApp QR login required",
                        self.theme.status_key(),
                    )),
                    Line::from("Open WhatsApp > Linked devices > Link a device, then scan below."),
                ]);

                match render_qr_lines(code.as_ref(), modal.width.saturating_sub(4) as usize) {
                    Some(qr_lines) if qr_lines.len() + lines.len() + 4 <= inner_height => {
                        lines.extend(qr_lines);
                        lines.push(Line::from(Span::styled(
                            "Esc, Enter, q, or click outside hides this prompt.",
                            self.theme.muted(),
                        )));
                    }
                    Some(qr_lines) if qr_lines.len() + lines.len() + 2 <= inner_height => {
                        lines.extend(qr_lines);
                    }
                    _ => {
                        lines.extend([
                            Line::from(Span::styled(
                                "QR is too large for this terminal; enlarge the window or use the payload below.",
                                self.theme.status_key(),
                            )),
                            Line::from(Span::styled("QR payload", self.theme.status_key())),
                            Line::from(truncate_chars(
                                code.as_ref(),
                                modal.width.saturating_sub(6) as usize,
                            )),
                            Line::from(Span::styled(
                                "Esc, Enter, q, or click outside hides this prompt.",
                                self.theme.muted(),
                            )),
                        ]);
                    }
                }
            }
            AuthChallenge::PairingCode(code) => {
                lines.extend([
                    Line::from(Span::styled("Pairing code", self.theme.status_key())),
                    Line::from(code.as_ref().to_owned()),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Esc, Enter, q, or click outside hides this prompt.",
                        self.theme.muted(),
                    )),
                ]);
            }
            AuthChallenge::OAuthUrl(url) => {
                lines.extend([
                    Line::from(Span::styled("Open this URL", self.theme.status_key())),
                    Line::from(truncate_chars(
                        url.as_ref(),
                        modal.width.saturating_sub(6) as usize,
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Esc, Enter, q, or click outside hides this prompt.",
                        self.theme.muted(),
                    )),
                ]);
            }
            AuthChallenge::Waiting => {
                lines.extend([
                    Line::from(Span::styled(
                        "Waiting for provider authentication",
                        self.theme.status_key(),
                    )),
                    Line::from("The provider will update this prompt when a QR or code is ready."),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Esc, Enter, q, or click outside hides this prompt.",
                        self.theme.muted(),
                    )),
                ]);
            }
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Authentication")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_help_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(help) = &self.state.help_overlay else {
            return;
        };
        if area.width < 28 || area.height < 10 {
            return;
        }

        let modal = self.help_overlay_rect(area);
        let lines = self.help_overlay_lines();
        let scroll_max = help_scroll_max(lines.len(), modal);
        let scroll = help.scroll.min(scroll_max);
        let title = format!(
            "Help {}/{}",
            scroll.saturating_add(1),
            scroll_max.saturating_add(1)
        );
        let backdrop = Rect::new(area.x, area.y, area.width, area.height.saturating_sub(1));
        let backdrop_widget =
            Paragraph::new("").style(Style::default().fg(self.theme.muted).bg(Color::Black));
        let paragraph = Paragraph::new(lines.clone())
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(self.theme.help_overlay_border()),
            )
            .scroll((scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });

        frame.render_widget(Clear, backdrop);
        frame.render_widget(backdrop_widget, backdrop);
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
        self.draw_vertical_scrollbar(frame, modal, lines.len(), scroll);
    }

    fn help_overlay_lines(&self) -> Vec<Line<'static>> {
        let focus = self.state.focus.label();
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| chat.name.to_string())
            .unwrap_or_else(|| "No chat selected".to_owned());
        let selected_message = self
            .state
            .selected_message_id
            .as_ref()
            .map(|id| short_id(id).to_string())
            .unwrap_or_else(|| "none".to_owned());
        let account_filter = self.account_filter_label();
        let filter = if self.state.filter.is_empty() {
            "none".to_owned()
        } else {
            self.state.filter.clone()
        };

        vec![
            Line::from(Span::styled("chat-cli help", self.theme.pane_title())),
            Line::from(Span::styled(
                "Press Esc, ?, F1, or q to close. Scroll to see more.",
                self.theme.muted(),
            )),
            Line::from(""),
            Line::from(Span::styled("Current context", self.theme.status_key())),
            Line::from(format!("  Focus: {focus}")),
            Line::from(format!("  Chat: {selected_chat}")),
            Line::from(format!("  Selected message: {selected_message}")),
            Line::from(format!("  Account filter: {account_filter}")),
            Line::from(format!("  Text filter: {filter}")),
            Line::from(""),
            Line::from(Span::styled("Everywhere", self.theme.status_key())),
            Line::from("  F1: open or close this help"),
            Line::from("  ?: open or close this help outside compose"),
            Line::from("  Ctrl+Q: quit"),
            Line::from("  Esc: close popup, cancel reply, or move back"),
            Line::from("  Left/Right: move between panes"),
            Line::from("  Mouse/touchpad: click to focus, scroll to browse"),
            Line::from(""),
            Line::from(Span::styled("Chats", self.theme.status_key())),
            Line::from("  Up/Down: choose a chat"),
            Line::from("  Enter or click: open selected chat"),
            Line::from("  PageUp/PageDown: jump through chats"),
            Line::from("  Home/End: first or last chat"),
            Line::from("  Ctrl+F: filter chats by text"),
            Line::from("  Ctrl+A: filter by account"),
            Line::from(""),
            Line::from(Span::styled("Messages", self.theme.status_key())),
            Line::from("  Click/tap a message: select and open actions"),
            Line::from("  Up/Down: select previous or next message"),
            Line::from("  Enter: open actions for selected message"),
            Line::from("  PageUp/PageDown or scroll: browse message history"),
            Line::from("  Reaching the top loads older messages when available"),
            Line::from("  Type a letter: start composing a reply"),
            Line::from(""),
            Line::from(Span::styled("Message actions", self.theme.status_key())),
            Line::from("  Reply: quote the selected message in compose"),
            Line::from("  View thread: open replies in the details pane"),
            Line::from("  React: choose an emoji reaction"),
            Line::from("  Copy text: copy message text when clipboard is available"),
            Line::from("  Open image: preview image media"),
            Line::from(""),
            Line::from(Span::styled("Compose", self.theme.status_key())),
            Line::from("  Enter: send text, or attach/send if compose is an existing local file path"),
            Line::from("  Type :joy, :heart, etc. for emoji suggestions"),
            Line::from("  Paste a local file path and press Enter to send it as media/file"),
            Line::from("  Backspace/Delete: edit text"),
            Line::from("  Esc: return to messages"),
            Line::from(""),
            Line::from(Span::styled("Popups", self.theme.status_key())),
            Line::from("  Arrow keys: move inside action, reaction, and account popups"),
            Line::from("  Enter: apply selected popup option"),
            Line::from("  Click outside: close most popups"),
            Line::from("  Scroll in help: move this page"),
        ]
    }

    fn draw_notification_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(notification) = &self.state.notification else {
            return;
        };
        if area.width < 24 || area.height < 6 {
            return;
        }

        let width = area.width.saturating_sub(4).clamp(24, 48);
        let height = 5;
        let x = area.x + area.width.saturating_sub(width.saturating_add(1));
        let y = area.y.saturating_add(1);
        let popup = Rect::new(x, y, width, height);
        let text_width = popup.width.saturating_sub(4) as usize;
        let title = truncate_chars(&notification.chat_name, text_width);
        let sender = truncate_chars(&notification.sender_name, text_width);
        let preview = truncate_chars(&notification.preview, text_width);

        let paragraph = Paragraph::new(vec![
            Line::from(Span::styled("New message", self.theme.status_key())),
            Line::from(vec![
                Span::styled(sender, self.theme.incoming()),
                Span::styled(format!(" · {title}"), self.theme.muted()),
            ]),
            Line::from(Span::raw(preview)),
        ])
        .block(
            Block::default()
                .title("Notification")
                .borders(Borders::ALL)
                .border_style(self.theme.overlay_border()),
        )
        .wrap(Wrap { trim: true });
        frame.render_widget(Clear, popup);
        frame.render_widget(paragraph, popup);
    }

    fn draw_account_switcher(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(switcher) = &self.state.account_switcher else {
            return;
        };
        if area.width < 32 || area.height < 8 {
            return;
        }

        let options = self.account_options();
        let width = area.width.saturating_sub(4).clamp(32, 64);
        let height = (options.len() as u16).saturating_add(4).clamp(6, 14);
        let modal = centered_fixed_rect(area, width, height);
        let mut lines = vec![
            Line::from(Span::styled("Filter by account", self.theme.pane_title())),
            Line::from(Span::styled("Filters chats only", self.theme.muted())),
        ];
        for (index, option) in options.iter().enumerate() {
            let selected = index == switcher.selected;
            let marker = if selected { "›" } else { " " };
            let active = match (&self.state.active_account, &option.provider_id) {
                (None, None) => " •",
                (Some(active), Some(provider_id)) if active == provider_id => " •",
                _ => "  ",
            };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(truncate_chars(&option.label, 18), style),
                Span::styled(format!(" ({})", option.chat_count), self.theme.muted()),
                Span::styled(active, self.theme.status_key()),
                Span::styled(" - ", self.theme.muted()),
                Span::styled(truncate_chars(&option.summary, 24), self.theme.muted()),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "Enter applies · Esc cancels",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Account filter")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_image_viewer(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(viewer) = self.state.image_viewer.clone() else {
            return;
        };

        if area.width < 4 || area.height < 4 {
            return;
        }

        let max_inner_size = Size::new(
            area.width.saturating_sub(4).min(IMAGE_VIEWER_MAX_WIDTH),
            area.height.saturating_sub(4),
        );
        let protocol = self
            .cached_terminal_image_protocol(&viewer.path, max_inner_size)
            .and_then(Result::ok);
        let inner_size = protocol
            .as_ref()
            .map(Protocol::size)
            .unwrap_or(max_inner_size);
        let viewer_area = centered_fixed_rect(
            area,
            inner_size.width.saturating_add(2),
            inner_size.height.saturating_add(2),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let image_area = block.inner(viewer_area);

        frame.render_widget(Clear, viewer_area);
        frame.render_widget(block, viewer_area);

        if let Some(protocol) = protocol {
            let image = TerminalImage::new(&protocol).allow_clipping(true);
            frame.render_widget(image, image_area);
        } else {
            self.render_halfblock_image(frame, image_area, &viewer.path, None);
        }
    }

    fn cached_terminal_image_protocol(
        &mut self,
        path: &Path,
        size: Size,
    ) -> Option<std::result::Result<Protocol, String>> {
        let picker = self.image_picker.as_ref()?;
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return None;
        }

        let key = ImageProtocolKey {
            path: path.to_path_buf(),
            width: size.width,
            height: size.height,
        };
        let picker = picker.clone();
        Some(
            self.image_protocol_cache
                .entry(key)
                .or_insert_with(|| build_terminal_image_protocol(&picker, path, size))
                .clone(),
        )
    }

    fn render_halfblock_image(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        path: &Path,
        protocol_error: Option<String>,
    ) {
        if area.is_empty() {
            return;
        }
        let preview_width = area.width.clamp(1, IMAGE_VIEWER_MAX_WIDTH);
        let preview_rows = area.height.max(1);
        let preview_area = centered_fixed_rect(area, preview_width, preview_rows);
        let preview = message_list::cached_image_preview_rows(
            path,
            &mut self.media_preview_cache,
            preview_width,
            preview_rows,
        );
        let mut lines = Vec::new();

        match preview {
            Ok(preview_rows) => lines.extend(preview_rows.into_iter().map(Line::from)),
            Err(error) => {
                lines.extend(
                    message_list::fallback_preview_rows(
                        preview_width,
                        preview_rows,
                        Color::DarkGray,
                        "image decode failed",
                    )
                    .into_iter()
                    .map(Line::from),
                );
                lines.push(Line::from(Span::styled(
                    error,
                    Style::default().fg(Color::Red),
                )));
            }
        }

        if let Some(protocol_error) = protocol_error {
            lines.push(Line::from(Span::styled(
                protocol_error,
                Style::default().fg(Color::DarkGray),
            )));
        }

        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            preview_area,
        );
    }

    async fn bootstrap(&mut self) -> Result<()> {
        for provider_index in 0..self.providers.len() {
            let account = self.providers[provider_index].account_info();
            self.state.account_statuses.insert(
                account.id.clone(),
                AccountStatus::new(&account, AccountConnection::Connecting),
            );
            self.store.upsert_account(&account, "{}").await?;
            if let Err(error) = self.providers[provider_index].connect().await {
                let detail = error.to_string();
                self.set_account_status(
                    &account.id,
                    AccountConnection::Offline,
                    Some(detail.clone()),
                );
                self.state.status = format!("{} setup failed: {detail}", account.display_name);
                if account.platform == Platform::Slack {
                    self.open_slack_setup_for_account(&account, Some(detail));
                    continue;
                }
                return Err(error);
            }
            if let Some(status) = self.state.account_statuses.get_mut(&account.id) {
                status.connection = AccountConnection::Syncing(0);
                status.detail = None;
            }

            if account.platform == Platform::Slack && !self.providers[provider_index].is_connected()
            {
                self.set_account_status(
                    &account.id,
                    AccountConnection::NeedsAuth,
                    Some("complete Slack setup".to_owned()),
                );
                self.open_slack_setup_for_account(&account, None);
                continue;
            }

            for chat in self.providers[provider_index].chats().await? {
                self.store.upsert_chat(&chat).await?;
                for message in self.providers[provider_index]
                    .history(&chat.id, None, HISTORY_LIMIT)
                    .await?
                {
                    self.store.upsert_message(&message).await?;
                }
            }
            if let Some(status) = self.state.account_statuses.get_mut(&account.id) {
                status.connection = AccountConnection::Online;
                status.detail = None;
            }
        }

        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.state.pending_scroll_to_latest = true;
        self.state.status = if self.state.chats.is_empty() {
            "ready - no chats loaded".to_owned()
        } else {
            "ready".to_owned()
        };
        Ok(())
    }

    async fn handle_provider_event(
        &mut self,
        provider_id: ProviderId,
        event: ProviderEvent,
    ) -> Result<()> {
        match event {
            ProviderEvent::Message {
                message,
                is_historical,
            } => {
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_update_selected = selected_chat_id.as_ref() == Some(&message.chat_id);
                self.store.upsert_message(&message).await?;
                if should_update_selected {
                    if is_historical {
                        self.append_historical_message_to_current_chat(message.clone());
                    } else {
                        self.reload_selected_messages().await?;
                    }
                }
                if !is_historical {
                    self.reload_chats().await?;
                }
                self.maybe_show_notification(&message, selected_chat_id.as_ref(), is_historical);
                if self
                    .state
                    .notification
                    .as_ref()
                    .is_none_or(|notification| notification.message_id != message.id)
                {
                    self.state.status = if is_historical {
                        format!("historical message from {provider_id}")
                    } else {
                        format!("message event from {provider_id}")
                    };
                }
            }
            ProviderEvent::MessageEdited { message } => {
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_reload = selected_chat_id.as_ref() == Some(&message.chat_id);
                self.store.upsert_message(&message).await?;
                if should_reload {
                    self.reload_selected_messages().await?;
                }
                self.reload_chats().await?;
                self.state.status = format!("message edited from {provider_id}");
            }
            ProviderEvent::ChatUpdated(chat) => {
                self.store.upsert_chat(&chat).await?;
                self.upsert_chat_in_state(chat);
                self.state.status = format!("chat updated from {provider_id}");
            }
            ProviderEvent::AuthRequired(challenge) => {
                self.set_account_status(
                    &provider_id,
                    AccountConnection::NeedsAuth,
                    Some(auth_challenge_label(&challenge).to_owned()),
                );
                if self.account_platform(&provider_id) == Some(Platform::Slack) {
                    self.open_slack_setup_for_provider(&provider_id, Some(&challenge), None);
                } else {
                    self.state.auth_overlay = Some(AuthOverlay {
                        provider_id: provider_id.clone(),
                        challenge,
                    });
                }
                self.state.status = format!("authentication required for {provider_id}");
            }
            ProviderEvent::AuthSucceeded => {
                if self
                    .state
                    .auth_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.provider_id == provider_id)
                {
                    self.state.auth_overlay = None;
                }
                self.update_slack_setup_success(&provider_id, SlackSetupPhase::CapabilityReview);
                self.set_account_status(&provider_id, AccountConnection::Online, None);
                self.state.status = format!("authenticated {provider_id}");
            }
            ProviderEvent::SyncProgress(progress) => {
                if self
                    .state
                    .auth_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.provider_id == provider_id)
                {
                    self.state.auth_overlay = None;
                }
                self.set_account_status(&provider_id, AccountConnection::Syncing(progress), None);
                self.state.status = format!("sync {provider_id}: {progress}%");
            }
            ProviderEvent::SyncComplete => {
                if self
                    .state
                    .auth_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.provider_id == provider_id)
                {
                    self.state.auth_overlay = None;
                }
                self.update_slack_setup_success(&provider_id, SlackSetupPhase::Connected);
                self.set_account_status(&provider_id, AccountConnection::Online, None);
                self.state.status = format!("sync complete for {provider_id}");
            }
            ProviderEvent::Disconnected(reason) => {
                if self
                    .state
                    .auth_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.provider_id == provider_id)
                {
                    self.state.auth_overlay = None;
                }
                let detail = reason.as_deref().map(str::to_owned);
                if self.account_platform(&provider_id) == Some(Platform::Slack) {
                    self.open_slack_setup_for_provider(&provider_id, None, detail.clone());
                }
                self.set_account_status(&provider_id, AccountConnection::Offline, detail.clone());
                self.state.status = detail
                    .map(|reason| format!("{provider_id} disconnected: {reason}"))
                    .unwrap_or_else(|| format!("{provider_id} disconnected"));
            }
            ProviderEvent::Reconnecting => {
                if self
                    .state
                    .auth_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.provider_id == provider_id)
                {
                    self.state.auth_overlay = None;
                }
                self.set_account_status(&provider_id, AccountConnection::Reconnecting, None);
                self.state.status = format!("reconnecting {provider_id}");
            }
            ProviderEvent::ReactionChanged {
                chat_id,
                message_id,
                emoji,
                added,
                sender,
            } => {
                let mut updated = None;
                if let Some(message) = self.message_by_id_mut(&message_id) {
                    if added {
                        add_reaction(message, emoji.as_ref(), sender);
                    } else {
                        remove_reaction(message, emoji.as_ref(), &sender);
                    }
                    updated = Some(message.clone());
                }
                if let Some(message) = updated {
                    self.store.upsert_message(&message).await?;
                } else if self
                    .state
                    .selected_chat()
                    .is_some_and(|chat| chat.id == chat_id)
                {
                    self.reload_selected_messages().await?;
                }
                self.state.status = format!("reaction updated from {provider_id}");
            }
            ProviderEvent::MessageDeleted { .. }
            | ProviderEvent::Receipt { .. }
            | ProviderEvent::Typing { .. } => {
                self.state.status = format!("event received from {provider_id}");
            }
        }
        Ok(())
    }

    async fn handle_action_menu_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(menu) = &mut self.state.action_menu else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.action_menu = None;
                self.state.status = "message actions closed".to_owned();
            }
            KeyCode::Up => {
                menu.selected = menu.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                menu.selected = menu
                    .selected
                    .saturating_add(1)
                    .min(ActionMenuItem::ALL.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let message_id = menu.message_id.clone();
                let item = ActionMenuItem::ALL[menu.selected];
                self.state.action_menu = None;
                self.perform_action_menu_item(message_id, item).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_reaction_picker_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(picker) = &mut self.state.reaction_picker else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.reaction_picker = None;
                self.state.status = "reaction cancelled".to_owned();
            }
            KeyCode::Left | KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Down => {
                picker.selected = picker
                    .selected
                    .saturating_add(1)
                    .min(REACTION_OPTIONS.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let message_id = picker.message_id.clone();
                let emoji = REACTION_OPTIONS[picker.selected];
                self.state.reaction_picker = None;
                self.apply_reaction(message_id, emoji).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_compose_emoticon_picker_key(&mut self, key: KeyEvent) -> bool {
        if self.state.compose_emoticon_picker.is_none() {
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                self.state.compose_emoticon_picker = None;
                self.state.status = "emoji suggestions closed".to_owned();
                true
            }
            KeyCode::Up => {
                if let Some(picker) = &mut self.state.compose_emoticon_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                true
            }
            KeyCode::Down => {
                if let Some(picker) = &mut self.state.compose_emoticon_picker {
                    let max = picker.matches.len().saturating_sub(1);
                    picker.selected = picker.selected.saturating_add(1).min(max);
                }
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.insert_selected_compose_emoticon();
                true
            }
            _ => false,
        }
    }

    fn handle_compose_attach_menu_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(menu) = &mut self.state.compose_attach_menu else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.compose_attach_menu = None;
                self.state.status = "attach menu closed".to_owned();
            }
            KeyCode::Up => {
                menu.selected = menu.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                menu.selected = menu
                    .selected
                    .saturating_add(1)
                    .min(ComposeAttachMenuItem::ALL.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let item = ComposeAttachMenuItem::ALL[menu.selected];
                self.state.compose_attach_menu = None;
                self.perform_compose_attach_menu_item(item)?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_poll_vote_picker_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.state.poll_vote_picker.is_none() {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                self.state.poll_vote_picker = None;
                self.state.status = "poll vote cancelled".to_owned();
            }
            KeyCode::Up => {
                if let Some(picker) = &mut self.state.poll_vote_picker {
                    picker.selected = picker.selected.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                let (message_id, selected) = self
                    .state
                    .poll_vote_picker
                    .as_ref()
                    .map(|picker| (picker.message_id.clone(), picker.selected))
                    .expect("picker exists");
                let max_option = self
                    .message_by_id(&message_id)
                    .and_then(|message| match &message.content {
                        Content::Poll(poll) => poll.options.len().checked_sub(1),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(picker) = &mut self.state.poll_vote_picker {
                    picker.selected = selected.saturating_add(1).min(max_option);
                }
            }
            KeyCode::Char(' ') => {
                self.toggle_poll_vote_picker_selection();
            }
            KeyCode::Enter => {
                let picker = self.state.poll_vote_picker.take().expect("picker exists");
                self.apply_poll_vote(picker.message_id, picker.selected_options)
                    .await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn perform_action_menu_item(
        &mut self,
        message_id: MessageId,
        item: ActionMenuItem,
    ) -> Result<()> {
        match item {
            ActionMenuItem::Reply => self.start_reply(message_id),
            ActionMenuItem::ViewThread => self.open_thread(message_id),
            ActionMenuItem::React => {
                let selected = self
                    .message_by_id(&message_id)
                    .and_then(local_reaction_option)
                    .unwrap_or_default();
                self.state.reaction_picker = Some(ReactionPicker {
                    message_id,
                    selected,
                });
                self.state.status = "choose a reaction".to_owned();
            }
            ActionMenuItem::CopyText => self.copy_message_text(&message_id),
            ActionMenuItem::VotePoll => self.open_poll_vote_picker(message_id),
            ActionMenuItem::OpenImage => {
                if !self.open_message_image(&message_id) {
                    self.state.status = "selected message has no image preview".to_owned();
                }
            }
            ActionMenuItem::Cancel => {
                self.state.status = "message actions cancelled".to_owned();
            }
        }
        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if is_ctrl_char(key, 'q') {
            self.state.should_quit = true;
            return Ok(false);
        }

        if self.state.account_switcher.is_some() {
            return self.handle_account_switcher_key(key);
        }

        if self.state.slack_setup.is_some() {
            return self.handle_slack_setup_key(key).await;
        }

        if self.state.help_overlay.is_some() {
            return Ok(self.handle_help_overlay_key(key));
        }

        if self.state.auth_overlay.is_some()
            && matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'))
        {
            self.state.auth_overlay = None;
            self.state.status = "authentication prompt hidden".to_owned();
            return Ok(false);
        }

        if self.state.action_menu.is_some() {
            return self.handle_action_menu_key(key).await;
        }

        if self.state.reaction_picker.is_some() {
            return self.handle_reaction_picker_key(key).await;
        }

        if self.state.compose_attach_menu.is_some() {
            return self.handle_compose_attach_menu_key(key);
        }

        if self.state.poll_vote_picker.is_some() {
            return self.handle_poll_vote_picker_key(key).await;
        }

        if self.state.filter_mode {
            return Ok(self.handle_filter_key(key));
        }

        if matches!(key.code, KeyCode::F(1))
            || (self.state.focus != FocusPane::Compose
                && matches!(key.code, KeyCode::Char('?'))
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT))
        {
            self.open_help_overlay();
            return Ok(false);
        }

        if self.state.focus != FocusPane::Compose && is_ctrl_char(key, 'a') {
            self.open_account_switcher();
            return Ok(false);
        }

        if is_ctrl_char(key, 'f') {
            return Ok(self.enter_filter_mode());
        }

        if self.state.focus == FocusPane::Compose {
            return self.handle_compose_key(key).await;
        }

        if self.state.focus == FocusPane::Messages
            && let KeyCode::Char(value) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.state.focus = FocusPane::Compose;
            self.apply_compose_edit_input(textarea_input(TextAreaKey::Char(value), key.modifiers));
            self.state.status = "typing message".to_owned();
            return Ok(false);
        }

        let selection_changed = match self.state.focus {
            FocusPane::Messages => self.handle_message_key(key).await?,
            FocusPane::Details => self.handle_details_key(key),
            _ => match key.code {
                KeyCode::Esc => self.handle_escape(),
                KeyCode::Left => {
                    self.focus_previous_pane();
                    false
                }
                KeyCode::Right => {
                    self.focus_next_pane();
                    false
                }
                KeyCode::Enter => {
                    let changed = self.activate_selected_chat();
                    self.mark_selected_chat_read().await?;
                    changed
                }
                KeyCode::Down => self.select_next_chat(),
                KeyCode::Up => self.select_previous_chat(),
                KeyCode::Home => self.select_first_chat(),
                KeyCode::End => self.select_last_chat(),
                KeyCode::PageDown => self.page_down_chats(),
                KeyCode::PageUp => self.page_up_chats(),
                _ => false,
            },
        };
        Ok(selection_changed)
    }

    fn handle_details_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                if self.state.thread_root.take().is_some() {
                    self.state.status = "thread closed".to_owned();
                } else {
                    self.focus_previous_pane();
                }
                false
            }
            KeyCode::Left => {
                self.focus_previous_pane();
                false
            }
            KeyCode::Right => {
                self.focus_next_pane();
                false
            }
            KeyCode::Down => {
                self.scroll_details_down(1);
                false
            }
            KeyCode::Up => {
                self.scroll_details_up(1);
                false
            }
            KeyCode::PageDown => {
                self.scroll_details_down(self.details_page_step());
                false
            }
            KeyCode::PageUp => {
                self.scroll_details_up(self.details_page_step());
                false
            }
            KeyCode::Home => {
                self.state.details_scroll = 0;
                self.state.status = "details at top".to_owned();
                false
            }
            KeyCode::End => {
                self.state.details_scroll = self.max_details_scroll();
                self.state.status = "details at bottom".to_owned();
                false
            }
            _ => false,
        }
    }

    async fn handle_slack_setup_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(setup) = &mut self.state.slack_setup else {
            return Ok(false);
        };

        match key.code {
            KeyCode::Esc => {
                self.state.slack_setup = None;
                self.state.status = "Slack setup hidden".to_owned();
            }
            KeyCode::Char('q')
                if !matches!(
                    setup.phase,
                    SlackSetupPhase::ChooseWorkspace
                        | SlackSetupPhase::EnterCredentials
                        | SlackSetupPhase::OAuthPrompt
                ) =>
            {
                self.state.slack_setup = None;
                self.state.status = "Slack setup hidden".to_owned();
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = setup
                        .selected_mode
                        .saturating_add(1)
                        .min(SlackSetupMode::ALL.len().saturating_sub(1));
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    setup.selected_credential_field = setup
                        .selected_credential_field
                        .saturating_add(1)
                        .min(setup.credential_fields().len().saturating_sub(1));
                    if let Some(field) = setup.selected_credential_field() {
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = setup.selected_mode.saturating_sub(1);
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    setup.selected_credential_field =
                        setup.selected_credential_field.saturating_sub(1);
                    if let Some(field) = setup.selected_credential_field() {
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::Home => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = 0;
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    setup.selected_credential_field = 0;
                    if let Some(field) = setup.selected_credential_field() {
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::End => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = SlackSetupMode::ALL.len().saturating_sub(1);
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    setup.selected_credential_field =
                        setup.credential_fields().len().saturating_sub(1);
                    if let Some(field) = setup.selected_credential_field() {
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::Backspace => match setup.phase {
                SlackSetupPhase::ChooseWorkspace => {
                    setup.workspace_label.pop();
                    self.state.status = "editing Slack workspace label".to_owned();
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    if let Some(field) = setup.selected_credential_field() {
                        setup.credentials.value_mut(field).pop();
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::Delete => match setup.phase {
                SlackSetupPhase::ChooseWorkspace => {
                    setup.workspace_label.clear();
                    self.state.status = "cleared Slack workspace label".to_owned();
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                    if let Some(field) = setup.selected_credential_field() {
                        setup.credentials.value_mut(field).clear();
                        self.state.status = format!("cleared Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::Char(value)
                if setup.phase == SlackSetupPhase::ChooseAuthMode
                    && ('1'..='6').contains(&value) =>
            {
                setup.selected_mode = (value as usize).saturating_sub('1' as usize);
                self.state.status = format!("selected {}", setup.selected_mode().label());
            }
            KeyCode::Enter => {
                let submit = match setup.phase {
                    SlackSetupPhase::ChooseWorkspace => {
                        setup.phase = SlackSetupPhase::ChooseAuthMode;
                        setup.status =
                            Some("Choose how this Slack workspace should sign in.".to_owned());
                        self.state.status = "Slack workspace label accepted".to_owned();
                        false
                    }
                    SlackSetupPhase::ChooseAuthMode => {
                        let mode = setup.selected_mode();
                        setup.phase = mode.next_phase();
                        setup.selected_credential_field = 0;
                        setup.clamp_credential_selection();
                        setup.capabilities = Some(SlackSetupCapabilities::from_mode(mode));
                        setup.status = Some(mode.credential_hint().to_owned());
                        self.state.status = format!("Slack setup: {}", mode.label());
                        false
                    }
                    SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                        setup.phase = SlackSetupPhase::Validating;
                        setup.status = Some("Validating Slack setup submission.".to_owned());
                        self.state.status = "validating Slack setup".to_owned();
                        true
                    }
                    SlackSetupPhase::Validating => {
                        self.state.status =
                            "Slack setup is waiting for provider validation".to_owned();
                        false
                    }
                    SlackSetupPhase::CapabilityReview | SlackSetupPhase::Connected => {
                        self.state.slack_setup = None;
                        self.state.status = "Slack setup complete".to_owned();
                        false
                    }
                    SlackSetupPhase::Failed => {
                        setup.phase = SlackSetupPhase::ChooseAuthMode;
                        setup.status =
                            Some("Choose another Slack setup method or retry.".to_owned());
                        self.state.status = "Retry Slack setup".to_owned();
                        false
                    }
                };
                if submit {
                    self.submit_current_slack_setup().await?;
                }
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match setup.phase {
                    SlackSetupPhase::ChooseWorkspace => {
                        setup.workspace_label.push(value);
                        self.state.status = "editing Slack workspace label".to_owned();
                    }
                    SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt => {
                        if let Some(field) = setup.selected_credential_field() {
                            setup.credentials.value_mut(field).push(value);
                            self.state.status = format!("editing Slack {}", field.label());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        Ok(false)
    }

    fn handle_help_overlay_key(&mut self, key: KeyEvent) -> bool {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) | KeyCode::Char('?')
        ) {
            self.close_help_overlay();
            return false;
        }

        let scroll_max = self.help_scroll_max();
        let Some(help) = &mut self.state.help_overlay else {
            return false;
        };

        match key.code {
            KeyCode::Down => {
                help.scroll = help.scroll.saturating_add(1).min(scroll_max);
            }
            KeyCode::Up => {
                help.scroll = help.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                help.scroll = help.scroll.saturating_add(HELP_PAGE_STEP).min(scroll_max);
            }
            KeyCode::PageUp => {
                help.scroll = help.scroll.saturating_sub(HELP_PAGE_STEP);
            }
            KeyCode::Home => {
                help.scroll = 0;
            }
            KeyCode::End => {
                help.scroll = scroll_max;
            }
            _ => {}
        }
        false
    }

    fn handle_account_switcher_key(&mut self, key: KeyEvent) -> Result<bool> {
        let options_len = self.account_options().len();
        let Some(switcher) = &mut self.state.account_switcher else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.state.account_switcher = None;
                self.state.status = "account filter closed".to_owned();
                Ok(false)
            }
            KeyCode::Enter => {
                let selected = switcher.selected;
                Ok(self.apply_account_switcher_selection(selected))
            }
            KeyCode::Down => {
                switcher.selected = switcher
                    .selected
                    .saturating_add(1)
                    .min(options_len.saturating_sub(1));
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Up => {
                switcher.selected = switcher.selected.saturating_sub(1);
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Home => {
                switcher.selected = 0;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::End => {
                switcher.selected = options_len.saturating_sub(1);
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_message_key(&mut self, key: KeyEvent) -> Result<bool> {
        let changed = match key.code {
            KeyCode::Esc => {
                if self.state.selected_message_id.take().is_some() {
                    self.state.action_menu = None;
                    self.state.reaction_picker = None;
                    self.state.status = "message selection cleared".to_owned();
                    false
                } else {
                    self.handle_escape()
                }
            }
            KeyCode::Left => {
                self.focus_previous_pane();
                false
            }
            KeyCode::Right => {
                self.focus_next_pane();
                false
            }
            KeyCode::Enter => {
                if self.state.selected_message_id.is_some() {
                    self.open_action_menu();
                }
                false
            }
            KeyCode::Down => {
                self.select_next_message();
                false
            }
            KeyCode::Up => {
                self.select_previous_message();
                false
            }
            KeyCode::Home => {
                self.select_first_message();
                false
            }
            KeyCode::End => {
                self.select_last_message();
                false
            }
            KeyCode::PageDown => {
                self.scroll_messages_down(self.message_page_step());
                false
            }
            KeyCode::PageUp => {
                self.scroll_messages_up(self.message_page_step());
                self.load_older_messages_if_at_top().await?;
                false
            }
            _ => false,
        };
        Ok(changed)
    }

    async fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<bool> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.image_viewer.is_some()
        {
            self.close_image_viewer();
            return Ok(false);
        }

        if self.state.help_overlay.is_some() {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if !rect_contains(
                        self.help_overlay_rect(self.state.frame_area),
                        mouse.column,
                        mouse.row,
                    ) {
                        self.close_help_overlay();
                    }
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_help_overlay(HELP_MOUSE_SCROLL_STEP as isize)
                }
                MouseEventKind::ScrollUp => {
                    self.scroll_help_overlay(-(HELP_MOUSE_SCROLL_STEP as isize))
                }
                _ => {}
            }
            return Ok(false);
        }

        if self.state.auth_overlay.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if !rect_contains(
                self.auth_overlay_rect(self.state.frame_area),
                mouse.column,
                mouse.row,
            ) {
                self.state.auth_overlay = None;
                self.state.status = "authentication prompt hidden".to_owned();
            }
            return Ok(false);
        }

        if self.state.action_menu.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_action_menu_click(mouse).await? {
                return Ok(false);
            }
            self.state.action_menu = None;
            self.state.status = "message actions closed".to_owned();
            return Ok(false);
        }

        if self.state.reaction_picker.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_reaction_picker_click(mouse).await? {
                return Ok(false);
            }
            self.state.reaction_picker = None;
            self.state.status = "reaction picker closed".to_owned();
            return Ok(false);
        }

        if self.state.compose_attach_menu.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_compose_attach_menu_click(mouse) {
                return Ok(false);
            }
            self.state.compose_attach_menu = None;
            self.state.status = "attach menu closed".to_owned();
            return Ok(false);
        }

        if self.state.poll_vote_picker.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_poll_vote_picker_click(mouse).await? {
                return Ok(false);
            }
            self.state.poll_vote_picker = None;
            self.state.status = "poll vote picker closed".to_owned();
            return Ok(false);
        }

        if self.state.account_switcher.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_account_switcher_click(mouse) {
                return Ok(true);
            }
            self.state.account_switcher = None;
            self.state.status = "account switcher closed".to_owned();
            return Ok(false);
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.status_bar_contains(mouse.column, mouse.row)
            && self.handle_status_bar_click(mouse)
        {
            return Ok(false);
        }

        let Some(pane) = self.pane_at(mouse.column, mouse.row) else {
            return Ok(false);
        };

        if self.state.focus != pane {
            self.state.focus = pane;
        }

        let changed = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.handle_left_click(pane, mouse),
            MouseEventKind::ScrollDown => self.handle_scroll_down(pane),
            MouseEventKind::ScrollUp => self.handle_scroll_up(pane).await?,
            MouseEventKind::ScrollLeft => {
                self.focus_previous_pane();
                false
            }
            MouseEventKind::ScrollRight => {
                self.focus_next_pane();
                false
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved => false,
        };
        Ok(changed)
    }

    async fn handle_action_menu_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(menu) = self.state.action_menu.clone() else {
            return Ok(false);
        };
        let Some(index) = self.action_menu_item_at(mouse.column, mouse.row, &menu) else {
            return Ok(false);
        };

        let item = ActionMenuItem::ALL[index];
        self.state.action_menu = None;
        self.perform_action_menu_item(menu.message_id, item).await?;
        Ok(true)
    }

    async fn handle_reaction_picker_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(picker) = self.state.reaction_picker.clone() else {
            return Ok(false);
        };
        let Some(index) = self.reaction_picker_option_at(mouse.column, mouse.row, &picker) else {
            return Ok(false);
        };

        let emoji = REACTION_OPTIONS[index];
        self.state.reaction_picker = None;
        self.apply_reaction(picker.message_id, emoji).await?;
        Ok(true)
    }

    async fn handle_poll_vote_picker_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(picker) = self.state.poll_vote_picker.clone() else {
            return Ok(false);
        };
        let Some(index) = self.poll_vote_picker_option_at(mouse.column, mouse.row, &picker) else {
            return Ok(false);
        };
        if let Some(active) = &mut self.state.poll_vote_picker {
            active.selected = index;
        }
        self.toggle_poll_vote_picker_selection();
        Ok(true)
    }

    fn handle_account_switcher_click(&mut self, mouse: MouseEvent) -> bool {
        let Some(index) = self.account_switcher_option_at(mouse.column, mouse.row) else {
            return false;
        };
        self.apply_account_switcher_selection(index)
    }

    fn handle_compose_attach_menu_click(&mut self, mouse: MouseEvent) -> bool {
        let Some(index) = self.compose_attach_menu_item_at(mouse.column, mouse.row) else {
            return false;
        };
        let item = ComposeAttachMenuItem::ALL[index];
        self.state.compose_attach_menu = None;
        if let Err(error) = self.perform_compose_attach_menu_item(item) {
            self.state.status = error.to_string();
        }
        true
    }

    fn handle_status_bar_click(&mut self, _mouse: MouseEvent) -> bool {
        if self.state.focus == FocusPane::ChatList {
            self.open_account_switcher();
            return true;
        }

        if self.state.focus == FocusPane::Compose {
            self.open_compose_attach_menu();
            return true;
        }

        false
    }

    fn open_compose_attach_menu(&mut self) {
        if let Some(capabilities) = self.selected_outbound_capabilities()
            && !outbound_media_supported(&capabilities)
        {
            self.state.status = capabilities
                .media_note
                .as_deref()
                .map(|note| format!("media sending is not available: {note}"))
                .unwrap_or_else(|| "media sending is not available for this account".to_owned());
            return;
        }
        self.state.compose_attach_menu = Some(ComposeAttachMenu::default());
        self.state.status = "choose what to attach from the typed path".to_owned();
    }

    fn perform_compose_attach_menu_item(&mut self, item: ComposeAttachMenuItem) -> Result<()> {
        if let Some(command) = item.attach_command() {
            self.attach_from_compose_text(command)?;
        } else {
            self.state.status = "attach cancelled".to_owned();
        }
        Ok(())
    }

    fn handle_left_click(&mut self, pane: FocusPane, mouse: MouseEvent) -> bool {
        match pane {
            FocusPane::ChatList => {
                if self.open_chat_avatar_at(mouse.column, mouse.row) {
                    return false;
                }
                if let Some(chat_index) = chat_list::chat_at(
                    &self.state.chats,
                    &self.state.visible_chat_indices,
                    self.state.selected_chat,
                    self.state.pane_areas.chat_list,
                    mouse.column,
                    mouse.row,
                ) {
                    return self.activate_chat_index(chat_index);
                }
                self.state.status = "chat list focused".to_owned();
                false
            }
            FocusPane::Messages => {
                if self.open_media_at(mouse.column, mouse.row) {
                    return false;
                }
                if self.open_message_avatar_at(mouse.column, mouse.row) {
                    return false;
                }
                if self.select_message_at(mouse.column, mouse.row) {
                    self.open_action_menu();
                    return false;
                }
                self.state.status = "messages focused".to_owned();
                false
            }
            FocusPane::Compose => {
                self.state.status = "compose focused".to_owned();
                false
            }
            FocusPane::Details => {
                self.state.status = "details focused".to_owned();
                false
            }
        }
    }

    fn handle_scroll_down(&mut self, pane: FocusPane) -> bool {
        match pane {
            FocusPane::ChatList => self.move_chat_selection(MOUSE_SCROLL_STEP as isize),
            FocusPane::Messages => {
                self.scroll_messages_down(MESSAGE_SCROLL_STEP);
                false
            }
            FocusPane::Compose => {
                self.state.status = "compose focused".to_owned();
                false
            }
            FocusPane::Details => {
                self.scroll_details_down(MOUSE_SCROLL_STEP);
                false
            }
        }
    }

    async fn handle_scroll_up(&mut self, pane: FocusPane) -> Result<bool> {
        Ok(match pane {
            FocusPane::ChatList => self.move_chat_selection(-(MOUSE_SCROLL_STEP as isize)),
            FocusPane::Messages => {
                self.scroll_messages_up(MESSAGE_SCROLL_STEP);
                self.load_older_messages_if_at_top().await?;
                false
            }
            FocusPane::Compose => {
                self.state.status = "compose focused".to_owned();
                false
            }
            FocusPane::Details => {
                self.scroll_details_up(MOUSE_SCROLL_STEP);
                false
            }
        })
    }

    fn pane_at(&self, column: u16, row: u16) -> Option<FocusPane> {
        if rect_contains(self.state.pane_areas.chat_list, column, row) {
            Some(FocusPane::ChatList)
        } else if rect_contains(self.state.pane_areas.messages, column, row) {
            Some(FocusPane::Messages)
        } else if rect_contains(self.state.pane_areas.compose, column, row) {
            Some(FocusPane::Compose)
        } else if rect_contains(self.state.pane_areas.details, column, row) {
            Some(FocusPane::Details)
        } else {
            None
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.state.filter_mode = false;
                self.state.status = self.filter_status();
                false
            }
            KeyCode::Backspace => {
                self.state.filter.pop();
                let selection_changed = self.apply_filter();
                self.state.status = self.filter_status();
                selection_changed
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.state.filter.push(value);
                let selection_changed = self.apply_filter();
                self.state.status = self.filter_status();
                selection_changed
            }
            _ => false,
        }
    }

    async fn handle_compose_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.handle_compose_emoticon_picker_key(key) {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc => {
                if self.state.pending_attachment.take().is_some() {
                    self.state.status = "attachment cancelled".to_owned();
                } else if self.state.reply_to.take().is_some() {
                    self.state.status = "reply cancelled".to_owned();
                } else {
                    self.state.focus = FocusPane::Messages;
                    self.state.status = "compose closed".to_owned();
                }
            }
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                self.apply_compose_edit_input(textarea_input(TextAreaKey::Enter, key.modifiers));
            }
            KeyCode::Enter => self.send_composed_message().await?,
            KeyCode::Backspace => {
                self.apply_compose_edit_input(textarea_input(
                    TextAreaKey::Backspace,
                    key.modifiers,
                ));
            }
            KeyCode::Delete => {
                self.apply_compose_edit_input(textarea_input(TextAreaKey::Delete, key.modifiers));
            }
            KeyCode::Left => {
                if self.compose_cursor_at_start() {
                    self.state.focus = FocusPane::Messages;
                    self.state.status = "focused Messages".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Left,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Right => {
                if self.compose_cursor_at_end() {
                    self.state.focus = FocusPane::Details;
                    self.state.status = "focused Details".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Right,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Up => {
                if self.compose_cursor_on_first_line() {
                    self.state.focus = FocusPane::Messages;
                    self.state.status = "focused Messages".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Up,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Down => {
                if self.compose_cursor_on_last_line() {
                    self.state.status = "compose focused".to_owned();
                } else {
                    self.apply_compose_navigation_input(textarea_input(
                        TextAreaKey::Down,
                        key.modifiers,
                    ));
                }
            }
            KeyCode::Home => {
                self.apply_compose_navigation_input(textarea_input(
                    TextAreaKey::Home,
                    key.modifiers,
                ));
            }
            KeyCode::End => {
                self.apply_compose_navigation_input(textarea_input(
                    TextAreaKey::End,
                    key.modifiers,
                ));
            }
            KeyCode::Tab => {
                self.apply_compose_edit_input(textarea_input(TextAreaKey::Tab, key.modifiers));
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.apply_compose_edit_input(textarea_input(
                    TextAreaKey::Char(value),
                    key.modifiers,
                ));
            }
            _ => {}
        }
        Ok(false)
    }

    fn attach_from_compose_text(&mut self, command: AttachCommandKind) -> Result<()> {
        let raw_path = self.state.compose_text.trim();
        if raw_path.is_empty() {
            self.state.status = match command {
                AttachCommandKind::Auto => {
                    "type or paste a file path, then choose [+ Attach]".to_owned()
                }
                AttachCommandKind::Image => {
                    "type or paste an image/GIF path, then choose [+ Attach]".to_owned()
                }
                AttachCommandKind::Sticker => {
                    "type or paste a sticker path, then choose [+ Attach]".to_owned()
                }
            };
            return Ok(());
        }
        if raw_path.lines().count() > 1 {
            self.state.status = "attachment path must be on one line".to_owned();
            return Ok(());
        }

        let attachment = pending_attachment_from_path(raw_path, command)?;
        let preview = attachment.preview();
        self.state.pending_attachment = Some(attachment);
        self.state.compose = new_compose_textarea();
        self.state.sync_compose_cache();
        self.state.status = format!("attached {preview}; type an optional caption and press Enter");
        Ok(())
    }

    fn update_compose_emoticon_completion(&mut self) {
        let Some((query, token_char_len)) =
            compose_emoticon_query(&self.state.compose_text, self.state.compose_cursor)
        else {
            self.state.compose_emoticon_picker = None;
            return;
        };
        let query_lower = query.to_ascii_lowercase();
        let matches = COMPOSE_EMOTICON_OPTIONS
            .iter()
            .enumerate()
            .filter_map(|(index, (value, label))| {
                let label_lower = label.to_ascii_lowercase();
                (label_lower
                    .split_whitespace()
                    .any(|alias| alias.starts_with(&query_lower))
                    || label_lower.contains(&query_lower)
                    || value.contains(&query))
                .then_some(index)
            })
            .take(COMPOSE_EMOTICON_MAX_SUGGESTIONS)
            .collect::<Vec<_>>();

        if matches.is_empty() {
            self.state.compose_emoticon_picker = None;
            return;
        }

        let selected = self
            .state
            .compose_emoticon_picker
            .as_ref()
            .map(|picker| picker.selected.min(matches.len().saturating_sub(1)))
            .unwrap_or_default();
        self.state.compose_emoticon_picker = Some(ComposeEmoticonPicker {
            selected,
            query,
            matches,
            token_char_len,
        });
    }

    fn insert_selected_compose_emoticon(&mut self) {
        let Some(picker) = self.state.compose_emoticon_picker.take() else {
            return;
        };
        let Some(option_index) = picker.matches.get(picker.selected).copied() else {
            return;
        };
        let value = COMPOSE_EMOTICON_OPTIONS[option_index].0;
        for _ in 0..picker.token_char_len {
            self.apply_compose_edit_input_without_completion(textarea_input(
                TextAreaKey::Backspace,
                KeyModifiers::NONE,
            ));
        }
        self.insert_compose_text(value);
        self.state.status = format!("inserted {value}");
    }

    fn insert_compose_text(&mut self, text: &str) {
        for value in text.chars() {
            self.apply_compose_edit_input(textarea_input(
                TextAreaKey::Char(value),
                KeyModifiers::NONE,
            ));
        }
    }

    fn apply_compose_edit_input_without_completion(&mut self, input: TextAreaInput) -> bool {
        let modified = self.state.compose.input_without_shortcuts(input);
        self.state.sync_compose_cache();
        modified
    }

    fn apply_compose_edit_input(&mut self, input: TextAreaInput) -> bool {
        let modified = self.apply_compose_edit_input_without_completion(input);
        self.update_compose_emoticon_completion();
        modified
    }

    fn apply_compose_navigation_input(&mut self, input: TextAreaInput) -> bool {
        let modified = self.state.compose.input(input);
        self.state.sync_compose_cache();
        modified
    }

    fn compose_cursor_at_start(&self) -> bool {
        self.state.compose.cursor() == (0, 0)
    }

    fn compose_cursor_at_end(&self) -> bool {
        let (row, column) = self.state.compose.cursor();
        let lines = self.state.compose.lines();
        let Some(line) = lines.get(row) else {
            return true;
        };

        row + 1 >= lines.len() && column >= line.chars().count()
    }

    fn compose_cursor_on_first_line(&self) -> bool {
        self.state.compose.cursor().0 == 0
    }

    fn compose_cursor_on_last_line(&self) -> bool {
        let (row, _) = self.state.compose.cursor();
        row + 1 >= self.state.compose.lines().len()
    }

    async fn send_composed_message(&mut self) -> Result<()> {
        let text = self.state.compose_text.trim_end().to_owned();
        let auto_attachment = if self.state.pending_attachment.is_none() {
            self.auto_attachment_from_compose_text(&text)?
        } else {
            None
        };
        if text.trim().is_empty()
            && self.state.pending_attachment.is_none()
            && auto_attachment.is_none()
        {
            self.state.status = "type a message or paste a file path before sending".to_owned();
            return Ok(());
        }

        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before sending".to_owned();
            return Ok(());
        };
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .ok_or_else(|| anyhow!("no provider registered for {}", chat.account))?;
        let account = provider.account_info();
        let content = if let Some(attachment) = self
            .state
            .pending_attachment
            .as_ref()
            .or(auto_attachment.as_ref())
        {
            let caption = (self.state.pending_attachment.is_some() && !text.trim().is_empty())
                .then(|| Arc::from(text.as_str()));
            attachment.to_content(caption)
        } else {
            Content::Text(Arc::from(text.as_str()))
        };
        if let Some(reason) = provider.outbound_capabilities().unsupported_reason(&content) {
            if self.state.pending_attachment.is_none()
                && let Some(attachment) = auto_attachment
            {
                let preview = attachment.preview();
                self.state.pending_attachment = Some(attachment);
                self.state.compose = new_compose_textarea();
                self.state.sync_compose_cache();
                self.state.status = format!("{preview} attached, but this account cannot send it yet");
                return Ok(());
            }

            self.state.status = format!("{reason}; attachment kept, press Esc to remove it");
            return Ok(());
        }
        let preview = content_send_preview(&content);
        let reply_to = self.state.reply_to.clone();
        let message_id = provider
            .send(&chat.id, content.clone(), reply_to.as_ref())
            .await?;
        let timestamp = Utc::now();
        let message = Message {
            id: message_id,
            chat_id: chat.id.clone(),
            account: chat.account.clone(),
            sender: Sender {
                platform_id: Arc::from("me"),
                display_name: account.display_name,
                avatar: account.avatar,
            },
            timestamp,
            edited_at: None,
            content,
            reply_to: reply_to.clone(),
            thread_id: reply_to,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            platform_data: PlatformData::default(),
        };

        self.store.upsert_message(&message).await?;
        self.update_chat_after_send(&chat, timestamp, &preview)
            .await?;
        self.state.compose = new_compose_textarea();
        self.state.pending_attachment = None;
        self.state.reply_to = None;
        self.state.sync_compose_cache();
        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.scroll_messages_to_bottom();
        self.state.status = format!("sent message to {}", chat.name);
        Ok(())
    }

    fn auto_attachment_from_compose_text(&mut self, text: &str) -> Result<Option<PendingAttachment>> {
        let raw_path = text.trim();
        if raw_path.is_empty() || raw_path.lines().count() > 1 {
            return Ok(None);
        }

        let path = PathBuf::from(expand_home_path(raw_path));
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                pending_attachment_from_path(raw_path, AttachCommandKind::Auto).map(Some)
            }
            Ok(_) | Err(_) => Ok(None),
        }
    }

    async fn update_chat_after_send(
        &mut self,
        chat: &Chat,
        timestamp: chat_core::Timestamp,
        preview: &str,
    ) -> Result<()> {
        let updated_chat = self
            .state
            .chats
            .iter_mut()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
            .map(|candidate| {
                candidate.last_message_at = Some(timestamp);
                candidate.last_message_preview = Some(Arc::from(preview));
                candidate.clone()
            });

        if let Some(updated_chat) = updated_chat {
            self.store.upsert_chat(&updated_chat).await?;
        }
        Ok(())
    }

    fn unread_message_ids(&self) -> HashSet<Arc<str>> {
        let Some(chat) = self.state.selected_chat() else {
            return HashSet::new();
        };
        self.unread_message_ids_for_chat(chat.unread_count)
    }

    fn unread_message_ids_for_chat(&self, unread_count: u32) -> HashSet<Arc<str>> {
        self.state
            .messages
            .iter()
            .rev()
            .filter(|message| !message.is_from_me)
            .take(unread_count as usize)
            .map(|message| message.id.clone())
            .collect()
    }

    fn scroll_messages_to_bottom(&mut self) {
        self.state.message_scroll = self.max_message_scroll();
        self.state.pending_scroll_to_latest = false;
    }

    fn enter_filter_mode(&mut self) -> bool {
        self.state.focus = FocusPane::ChatList;
        self.state.filter_mode = true;
        self.state.status = "type to filter chats; Enter or Esc closes filtering".to_owned();
        false
    }

    fn open_help_overlay(&mut self) {
        self.state.help_overlay = Some(HelpOverlay::default());
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.account_switcher = None;
        self.state.status = "help opened".to_owned();
    }

    fn close_help_overlay(&mut self) {
        self.state.help_overlay = None;
        self.state.status = "help closed".to_owned();
    }

    fn scroll_help_overlay(&mut self, delta: isize) {
        let scroll_max = self.help_scroll_max();
        let Some(help) = &mut self.state.help_overlay else {
            return;
        };
        help.scroll = help.scroll.saturating_add_signed(delta).min(scroll_max);
    }

    fn help_scroll_max(&self) -> usize {
        let modal = self.help_overlay_rect(self.state.frame_area);
        help_scroll_max(self.help_overlay_lines().len(), modal)
    }

    fn handle_escape(&mut self) -> bool {
        if self.state.help_overlay.is_some() {
            self.close_help_overlay();
            return false;
        }

        if self.state.image_viewer.is_some() {
            self.close_image_viewer();
            return false;
        }

        if self.state.reaction_picker.is_some() {
            self.state.reaction_picker = None;
            self.state.status = "reaction cancelled".to_owned();
            return false;
        }

        if self.state.action_menu.is_some() {
            self.state.action_menu = None;
            self.state.status = "message actions closed".to_owned();
            return false;
        }

        if self.state.account_switcher.is_some() {
            self.state.account_switcher = None;
            self.state.status = "account switcher closed".to_owned();
            return false;
        }

        if self.state.thread_root.take().is_some() {
            self.state.status = "thread closed".to_owned();
            return false;
        }

        if self.state.reply_to.take().is_some() {
            self.state.status = "reply cancelled".to_owned();
            return false;
        }

        if self.state.selected_message_id.take().is_some() {
            self.state.status = "message selection cleared".to_owned();
            return false;
        }

        if self.state.focus != FocusPane::ChatList {
            self.state.focus = FocusPane::ChatList;
            self.state.status = "back to chat list".to_owned();
            return false;
        }

        self.clear_filter()
    }

    fn focus_previous_pane(&mut self) {
        let next_focus = self.state.focus.previous();
        if next_focus != self.state.focus {
            self.state.focus = next_focus;
            self.state.status = format!("focused {}", self.state.focus.label());
        }
    }

    fn focus_next_pane(&mut self) {
        let next_focus = self.state.focus.next();
        if next_focus != self.state.focus {
            self.state.focus = next_focus;
            self.state.status = format!("focused {}", self.state.focus.label());
        }
    }

    fn status_bar_contains(&self, column: u16, row: u16) -> bool {
        let area = self.state.frame_area;
        area.width > 0
            && area.height > 0
            && column >= area.x
            && column < area.x.saturating_add(area.width)
            && row == area.y.saturating_add(area.height.saturating_sub(1))
    }

    fn activate_selected_chat(&mut self) -> bool {
        let had_unread = self.selected_chat_has_unread();
        let should_sync_if_empty = self.state.messages.is_empty();
        if let Some(chat_name) = self.state.selected_chat().map(|chat| chat.name.to_string()) {
            self.state.focus = FocusPane::Messages;
            self.request_selected_chat_history_sync();
            self.state.status = format!("opened {chat_name}");
        }
        had_unread || should_sync_if_empty
    }

    fn activate_chat_index(&mut self, chat_index: usize) -> bool {
        let changed = chat_index != self.state.selected_chat;
        let should_sync_if_empty = self.state.messages.is_empty();
        self.state.selected_chat = chat_index;
        self.state.focus = FocusPane::Messages;
        self.state.message_scroll = 0;
        self.state.pending_scroll_to_latest = true;
        if changed {
            self.state.selected_message_id = None;
            self.state.action_menu = None;
            self.state.reaction_picker = None;
            self.state.account_switcher = None;
            self.state.reply_to = None;
            self.state.thread_root = None;
            self.reset_history_window_state();
        }
        self.state.image_viewer = None;

        if let Some(chat_name) = self.state.selected_chat().map(|chat| chat.name.to_string()) {
            self.request_selected_chat_history_sync();
            self.state.status = format!("opened {chat_name}");
        }

        changed
            || should_sync_if_empty
            || self
                .state
                .chats
                .get(chat_index)
                .is_some_and(|chat| chat.unread_count > 0)
    }

    fn clear_filter(&mut self) -> bool {
        if self.state.filter.is_empty() {
            return false;
        }

        self.state.filter_mode = false;
        self.state.filter.clear();
        let selection_changed = self.apply_filter();
        self.state.status = "chat filter cleared".to_owned();
        selection_changed
    }

    fn select_next_chat(&mut self) -> bool {
        self.move_chat_selection(1)
    }

    fn select_previous_chat(&mut self) -> bool {
        self.move_chat_selection(-1)
    }

    fn select_first_chat(&mut self) -> bool {
        self.select_visible_position(0)
    }

    fn select_last_chat(&mut self) -> bool {
        let Some(last_position) = self.state.visible_chat_indices.len().checked_sub(1) else {
            return false;
        };
        self.select_visible_position(last_position)
    }

    fn page_down_chats(&mut self) -> bool {
        self.move_chat_selection(5)
    }

    fn page_up_chats(&mut self) -> bool {
        self.move_chat_selection(-5)
    }

    fn move_chat_selection(&mut self, offset: isize) -> bool {
        if self.state.visible_chat_indices.is_empty() {
            return false;
        }

        let current_position = self.selected_visible_position().unwrap_or(0);
        let last_position = self.state.visible_chat_indices.len() - 1;
        let next_position = if offset.is_negative() {
            current_position.saturating_sub(offset.unsigned_abs())
        } else {
            current_position
                .saturating_add(offset as usize)
                .min(last_position)
        };

        self.select_visible_position(next_position)
    }

    fn selected_visible_position(&self) -> Option<usize> {
        chat_list::selected_visible_position(
            &self.state.visible_chat_indices,
            self.state.selected_chat,
        )
    }

    fn select_visible_position(&mut self, position: usize) -> bool {
        let Some(&chat_index) = self.state.visible_chat_indices.get(position) else {
            return false;
        };
        let changed = chat_index != self.state.selected_chat;
        self.state.selected_chat = chat_index;
        if changed {
            self.state.message_scroll = 0;
            self.state.details_scroll = 0;
            self.state.pending_scroll_to_latest = true;
            self.state.selected_message_id = None;
            self.state.action_menu = None;
            self.state.reaction_picker = None;
            self.state.account_switcher = None;
            self.state.reply_to = None;
            self.state.thread_root = None;
            self.state.image_viewer = None;
            self.reset_history_window_state();
        }
        changed
            || self
                .state
                .chats
                .get(chat_index)
                .is_some_and(|chat| chat.unread_count > 0)
    }

    fn selected_chat_has_unread(&self) -> bool {
        self.state
            .selected_chat()
            .is_some_and(|chat| chat.unread_count > 0)
    }

    fn request_selected_chat_history_sync(&mut self) {
        self.state.pending_history_sync_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.account.clone(), chat.id.clone()));
    }

    fn consume_pending_history_sync_for_selected_chat(&mut self) -> bool {
        let Some((account, chat_id)) = self.state.pending_history_sync_chat.take() else {
            return false;
        };
        self.state
            .selected_chat()
            .is_some_and(|chat| chat.account == account && chat.id == chat_id)
    }

    async fn mark_selected_chat_read(&mut self) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return Ok(());
        };
        if chat.unread_count == 0 {
            return Ok(());
        }

        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            && let Some(message) = self.state.messages.last()
        {
            provider.mark_read(&chat.id, &message.id).await?;
        }

        if let Some(current) = self
            .state
            .chats
            .iter_mut()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
        {
            current.unread_count = 0;
            let updated = current.clone();
            self.store.upsert_chat(&updated).await?;
        }
        Ok(())
    }

    fn append_historical_message_to_current_chat(&mut self, message: Message) {
        if let Some(existing) = self
            .state
            .messages
            .iter_mut()
            .find(|existing| existing.id == message.id)
        {
            *existing = message;
        } else {
            self.state.messages.push(message);
        }
        self.state.messages.sort_by_key(|message| message.timestamp);
        self.clamp_message_scroll();
    }

    fn scroll_messages_down(&mut self, amount: usize) {
        let max_scroll = self.max_message_scroll();
        let previous_scroll = self.state.message_scroll;
        self.state.message_scroll = self
            .state
            .message_scroll
            .saturating_add(amount)
            .min(max_scroll);
        self.state.status = if self.state.message_scroll == max_scroll {
            "showing latest messages".to_owned()
        } else if self.state.message_scroll == previous_scroll {
            format!("showing message line {}", self.state.message_scroll + 1)
        } else {
            format!("showing message line {}", self.state.message_scroll + 1)
        };
    }

    fn scroll_messages_up(&mut self, amount: usize) {
        self.state.message_scroll = self.state.message_scroll.saturating_sub(amount);
        self.state.status = if self.state.message_scroll == 0 {
            "at earliest loaded messages; loading older messages when available".to_owned()
        } else {
            format!("showing message line {}", self.state.message_scroll + 1)
        };
    }

    fn message_page_step(&self) -> usize {
        inner_area(self.state.pane_areas.messages)
            .height
            .saturating_sub(1)
            .max(1) as usize
    }

    fn scroll_details_down(&mut self, amount: usize) {
        let max_scroll = self.max_details_scroll();
        self.state.details_scroll = self
            .state
            .details_scroll
            .saturating_add(amount)
            .min(max_scroll);
        self.state.status = if self.state.details_scroll == max_scroll {
            "details at bottom".to_owned()
        } else {
            format!("showing details line {}", self.state.details_scroll + 1)
        };
    }

    fn scroll_details_up(&mut self, amount: usize) {
        self.state.details_scroll = self.state.details_scroll.saturating_sub(amount);
        self.state.status = if self.state.details_scroll == 0 {
            "details at top".to_owned()
        } else {
            format!("showing details line {}", self.state.details_scroll + 1)
        };
    }

    fn details_page_step(&self) -> usize {
        inner_area(self.state.pane_areas.details)
            .height
            .saturating_sub(1)
            .max(1) as usize
    }

    fn open_chat_avatar_at(&mut self, column: u16, row: u16) -> bool {
        let (avatar_start, avatar_end) =
            chat_list::avatar_column_bounds(self.state.pane_areas.chat_list);
        if column < avatar_start || column >= avatar_end {
            return false;
        }
        let Some(chat_index) = chat_list::chat_at(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            self.state.pane_areas.chat_list,
            column,
            row,
        ) else {
            return false;
        };
        let Some(chat) = self.state.chats.get(chat_index) else {
            return false;
        };
        let Some(path) = chat.avatar.as_ref().filter(|path| path.exists()).cloned() else {
            return false;
        };
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.status = format!("viewing avatar {}", path.display());
        self.state.image_viewer = Some(ImageViewer { path });
        true
    }

    fn open_media_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let clicked_line = self
            .state
            .message_scroll
            .saturating_add(row.saturating_sub(content_area.y) as usize);
        let Some(hit) = self
            .state
            .media_hits
            .iter()
            .find(|hit| {
                clicked_line >= hit.start_line
                    && clicked_line <= hit.end_line
                    && clicked_column_in_hit(content_area, column, hit.start_col, hit.end_col)
            })
            .cloned()
        else {
            return false;
        };

        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.status = format!("viewing image {}", hit.path.display());
        self.state.image_viewer = Some(ImageViewer { path: hit.path });
        true
    }

    fn close_image_viewer(&mut self) {
        self.state.image_viewer = None;
        self.state.status = "image preview closed".to_owned();
    }

    fn open_message_avatar_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let clicked_line = self
            .state
            .message_scroll
            .saturating_add(row.saturating_sub(content_area.y) as usize);
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| {
                hit.avatar_hit.as_ref().is_some_and(|avatar_hit| {
                    avatar_hit.line == clicked_line
                        && clicked_column_in_hit(
                            content_area,
                            column,
                            avatar_hit.start_col,
                            avatar_hit.end_col,
                        )
                })
            })
            .cloned()
        else {
            return false;
        };

        let (sender_name, avatar_path) = {
            let Some(message) = self.message_by_id(&hit.message_id) else {
                return false;
            };
            (
                message.sender.display_name.clone(),
                message
                    .sender
                    .avatar
                    .as_ref()
                    .filter(|path| path.exists())
                    .cloned(),
            )
        };
        let Some(path) = avatar_path else {
            self.state.selected_message_id = Some(hit.message_id.clone());
            self.state.thread_root = None;
            self.state.status = format!("{sender_name} has no avatar loaded");
            return true;
        };
        self.state.selected_message_id = Some(hit.message_id);
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.thread_root = None;
        self.state.status = format!("viewing avatar {}", path.display());
        self.state.image_viewer = Some(ImageViewer { path });
        true
    }

    fn select_message_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let clicked_line = self
            .state
            .message_scroll
            .saturating_add(row.saturating_sub(content_area.y) as usize);
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| {
                hit.line_hits.iter().any(|line_hit| {
                    line_hit.line == clicked_line
                        && clicked_column_in_hit(
                            content_area,
                            column,
                            line_hit.start_col,
                            line_hit.end_col,
                        )
                })
            })
            .cloned()
        else {
            return false;
        };

        self.state.selected_message_id = Some(hit.message_id.clone());
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.thread_root = None;
        self.state.status = format!("selected message {}", short_id(&hit.message_id));
        self.ensure_selected_message_visible();
        true
    }

    fn selected_message_index(&self) -> Option<usize> {
        let selected = self.state.selected_message_id.as_ref()?;
        self.state
            .messages
            .iter()
            .position(|message| message.id == *selected)
    }

    fn message_by_id(&self, message_id: &MessageId) -> Option<&Message> {
        self.state
            .messages
            .iter()
            .find(|message| message.id == *message_id)
    }

    fn message_by_id_mut(&mut self, message_id: &MessageId) -> Option<&mut Message> {
        self.state
            .messages
            .iter_mut()
            .find(|message| message.id == *message_id)
    }

    fn thread_replies(&self, thread_root: &MessageId) -> Vec<&Message> {
        self.state
            .messages
            .iter()
            .filter(|message| {
                message.reply_to.as_ref() == Some(thread_root)
                    || message.thread_id.as_ref() == Some(thread_root)
            })
            .filter(|message| message.id != *thread_root)
            .collect()
    }

    fn thread_reply_count(&self, thread_root: &MessageId) -> usize {
        self.thread_replies(thread_root).len()
    }

    fn select_first_message(&mut self) {
        let Some(message) = self.state.messages.first() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(message.id.clone());
        self.state.message_scroll = 0;
        self.state.status = "selected first message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_last_message(&mut self) {
        let Some(message) = self.state.messages.last() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(message.id.clone());
        self.scroll_messages_to_bottom();
        self.state.status = "selected latest message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_next_message(&mut self) {
        if self.state.messages.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let next = self
            .selected_message_index()
            .map(|index| index.saturating_add(1).min(self.state.messages.len() - 1))
            .unwrap_or(0);
        self.state.selected_message_id = Some(self.state.messages[next].id.clone());
        self.state.status = "selected next message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_previous_message(&mut self) {
        if self.state.messages.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let previous = self
            .selected_message_index()
            .map(|index| index.saturating_sub(1))
            .unwrap_or_else(|| self.state.messages.len().saturating_sub(1));
        self.state.selected_message_id = Some(self.state.messages[previous].id.clone());
        self.state.status = "selected previous message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn ensure_selected_message_visible(&mut self) {
        let Some(selected) = self.state.selected_message_id.as_deref() else {
            return;
        };
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| hit.message_id.as_ref() == selected)
        else {
            return;
        };
        let viewport_rows = inner_area(self.state.pane_areas.messages).height as usize;
        if viewport_rows == 0 {
            return;
        }
        if hit.start_line < self.state.message_scroll {
            self.state.message_scroll = hit.start_line;
        } else if hit.end_line >= self.state.message_scroll.saturating_add(viewport_rows) {
            self.state.message_scroll =
                hit.end_line.saturating_add(1).saturating_sub(viewport_rows);
        }
        self.clamp_message_scroll();
    }

    fn open_action_menu(&mut self) {
        if let Some(message_id) = self.state.selected_message_id.clone() {
            self.state.reaction_picker = None;
            self.state.poll_vote_picker = None;
            self.state.action_menu = Some(ActionMenu {
                message_id,
                selected: 0,
            });
            self.state.status = "message actions opened".to_owned();
        } else {
            self.state.status = "select a message first".to_owned();
        }
    }

    fn start_reply(&mut self, message_id: MessageId) {
        let preview = self
            .message_by_id(&message_id)
            .map(reply_preview)
            .unwrap_or_else(|| format!("message {}", short_id(&message_id)));
        self.state.reply_to = Some(message_id);
        self.state.focus = FocusPane::Compose;
        self.state.status = format!("replying to {preview}");
    }

    fn open_thread(&mut self, message_id: MessageId) {
        let reply_count = self.thread_reply_count(&message_id);
        self.state.thread_root = Some(message_id);
        self.state.focus = FocusPane::Details;
        self.state.status = if reply_count == 0 {
            "thread opened; no replies yet".to_owned()
        } else if reply_count == 1 {
            "thread opened with 1 reply".to_owned()
        } else {
            format!("thread opened with {reply_count} replies")
        };
    }

    fn open_poll_vote_picker(&mut self, message_id: MessageId) {
        let Some(message) = self.message_by_id(&message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return;
        };
        let Content::Poll(poll) = &message.content else {
            self.state.status = "selected message is not a poll".to_owned();
            return;
        };
        if poll.options.is_empty() {
            self.state.status = "poll has no options".to_owned();
            return;
        }
        let selected_options = poll
            .votes
            .iter()
            .find(|vote| vote.sender.as_ref() == LOCAL_REACTION_SENDER)
            .map(|vote| {
                poll.options
                    .iter()
                    .enumerate()
                    .filter_map(|(index, option)| {
                        vote.options
                            .iter()
                            .any(|selected| selected.as_ref() == option.id.as_ref())
                            .then_some(index)
                    })
                    .collect::<HashSet<_>>()
            })
            .filter(|selected| !selected.is_empty())
            .unwrap_or_else(|| HashSet::from([0]));
        let selected = selected_options.iter().copied().min().unwrap_or_default();
        self.state.poll_vote_picker = Some(PollVotePicker {
            message_id,
            selected,
            selected_options,
        });
        self.state.status = "choose poll option".to_owned();
    }

    fn toggle_poll_vote_picker_selection(&mut self) {
        let Some(snapshot) = self.state.poll_vote_picker.as_ref().cloned() else {
            return;
        };
        let selectable = self
            .message_by_id(&snapshot.message_id)
            .and_then(|message| match &message.content {
                Content::Poll(poll) => {
                    Some(poll.selectable_options_count.unwrap_or(1).max(1) as usize)
                }
                _ => None,
            })
            .unwrap_or(1);
        let Some(picker) = &mut self.state.poll_vote_picker else {
            return;
        };
        if selectable == 1 {
            picker.selected_options.clear();
            picker.selected_options.insert(picker.selected);
            return;
        }
        if !picker.selected_options.remove(&picker.selected) {
            if picker.selected_options.len() >= selectable
                && let Some(first) = picker.selected_options.iter().copied().min()
            {
                picker.selected_options.remove(&first);
            }
            picker.selected_options.insert(picker.selected);
        }
    }

    async fn apply_poll_vote(
        &mut self,
        message_id: MessageId,
        selected_indices: HashSet<usize>,
    ) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before voting".to_owned();
            return Ok(());
        };
        let Some(target_message) = self.message_by_id(&message_id).cloned() else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        let Content::Poll(poll) = &target_message.content else {
            self.state.status = "selected message is not a poll".to_owned();
            return Ok(());
        };
        let mut selected_options = selected_indices
            .into_iter()
            .filter_map(|index| poll.options.get(index).map(|option| option.id.clone()))
            .collect::<Vec<_>>();
        selected_options.sort();
        selected_options.dedup();
        if selected_options.is_empty() {
            self.state.status = "choose at least one poll option".to_owned();
            return Ok(());
        }

        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
        {
            provider
                .vote_poll(&chat.id, &target_message, &selected_options)
                .await?;
        } else {
            self.state.status = format!("no provider registered for {}", chat.account);
            return Ok(());
        }

        let Some(message) = self.message_by_id_mut(&message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        if let Content::Poll(poll) = &mut message.content {
            poll.votes
                .retain(|vote| vote.sender.as_ref() != LOCAL_REACTION_SENDER);
            poll.votes.push(chat_core::PollVote {
                sender: Arc::from(LOCAL_REACTION_SENDER),
                options: selected_options,
                timestamp: Some(Utc::now()),
            });
        }
        let updated = message.clone();
        self.store.upsert_message(&updated).await?;
        self.state.status = "poll vote submitted".to_owned();
        Ok(())
    }

    async fn apply_reaction(&mut self, message_id: MessageId, emoji: &str) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before reacting".to_owned();
            return Ok(());
        };
        let Some(target_message) = self.message_by_id(&message_id).cloned() else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        let had_reaction = message_reacted_by_sender(&target_message, emoji, LOCAL_REACTION_SENDER);

        if let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
        {
            provider.react(&chat.id, &target_message, emoji).await?;
        }

        let Some(message) = self.message_by_id_mut(&message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        if had_reaction {
            remove_reaction(message, emoji, &Arc::from(LOCAL_REACTION_SENDER));
        } else {
            add_reaction(message, emoji, Arc::from(LOCAL_REACTION_SENDER));
        }
        let updated = message.clone();
        self.store.upsert_message(&updated).await?;
        self.state.status = if had_reaction {
            format!("removed reaction {emoji}")
        } else {
            format!("reacted with {emoji}")
        };
        Ok(())
    }

    fn copy_message_text(&mut self, message_id: &MessageId) {
        let Some(message) = self.message_by_id(message_id) else {
            self.state.status = "selected message was not found".to_owned();
            return;
        };
        let text = content_copy_text(&message.content);
        if text.trim().is_empty() {
            self.state.status = "selected message has no copyable text".to_owned();
            return;
        }

        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.clone())) {
            Ok(()) => self.state.status = "copied message text".to_owned(),
            Err(error) => {
                self.state.status = format!("clipboard unavailable; text: {text} ({error})");
            }
        }
    }

    fn open_message_image(&mut self, message_id: &MessageId) -> bool {
        let Some(message) = self.message_by_id(message_id) else {
            return false;
        };
        let Some((path, _, _)) = message_image_preview(&message.content) else {
            return false;
        };

        self.state.image_viewer = Some(ImageViewer { path: path.clone() });
        self.state.status = format!("viewing image {}", path.display());
        true
    }

    fn set_account_status(
        &mut self,
        provider_id: &ProviderId,
        connection: AccountConnection,
        detail: Option<String>,
    ) {
        let status = self
            .state
            .account_statuses
            .entry(provider_id.clone())
            .or_insert_with(|| AccountStatus {
                display_name: provider_id.to_string(),
                connection: AccountConnection::Connecting,
                detail: None,
            });
        status.connection = connection;
        status.detail = detail;
    }

    fn account_for_provider(&self, provider_id: &ProviderId) -> Option<Account> {
        self.providers
            .iter()
            .find(|provider| provider.id() == provider_id)
            .map(|provider| provider.account_info())
    }

    fn provider_for_id(&self, provider_id: &ProviderId) -> Option<&dyn Provider> {
        self.providers
            .iter()
            .find(|provider| provider.id() == provider_id)
            .map(|provider| provider.as_ref())
    }

    async fn submit_current_slack_setup(&mut self) -> Result<()> {
        let Some(setup) = self.state.slack_setup.clone() else {
            return Ok(());
        };
        let selected_mode = setup.selected_mode().to_auth_submission_mode();
        let submission = AuthSubmission {
            workspace_label: trimmed_option(&setup.workspace_label),
            mode: Some(selected_mode),
            client_id: trimmed_option(&setup.credentials.client_id),
            client_secret: trimmed_option(&setup.credentials.client_secret),
            redirect_uri: trimmed_option(&setup.credentials.redirect_uri),
            oauth_code: trimmed_option(&setup.credentials.oauth_code),
            user_token: trimmed_option(&setup.credentials.user_token),
            bot_token: trimmed_option(&setup.credentials.bot_token),
            app_token: trimmed_option(&setup.credentials.app_token),
            webhook_url: trimmed_option(&setup.credentials.webhook_url),
        };
        let result = if let Some(provider) = self.provider_for_id(&setup.provider_id) {
            provider.submit_auth(submission).await
        } else {
            Err(anyhow!(
                "Slack provider {} is not available",
                setup.provider_id
            ))
        };

        match result {
            Ok(()) => {
                self.update_slack_setup_success(
                    &setup.provider_id,
                    SlackSetupPhase::CapabilityReview,
                );
                self.set_account_status(&setup.provider_id, AccountConnection::Online, None);
                self.state.status = format!("Slack setup submitted for {}", setup.provider_id);
            }
            Err(error) => {
                let detail = error.to_string();
                self.set_account_status(
                    &setup.provider_id,
                    AccountConnection::Offline,
                    Some(detail.clone()),
                );
                if let Some(current_setup) = &mut self.state.slack_setup
                    && current_setup.provider_id == setup.provider_id
                {
                    current_setup.phase = SlackSetupPhase::Failed;
                    current_setup.status = Some(detail.clone());
                }
                self.state.status = format!("Slack setup failed: {detail}");
            }
        }
        Ok(())
    }

    fn account_platform(&self, provider_id: &ProviderId) -> Option<Platform> {
        self.account_for_provider(provider_id)
            .map(|account| account.platform)
    }

    fn open_slack_setup_for_account(&mut self, account: &Account, status: Option<String>) {
        let mut overlay =
            SlackSetupOverlay::new(account.id.clone(), account.display_name.to_string());
        if let Some(status) = status {
            overlay.phase = SlackSetupPhase::Failed;
            overlay.status = Some(status);
        }
        self.state.slack_setup = Some(overlay);
    }

    fn open_slack_setup_for_provider(
        &mut self,
        provider_id: &ProviderId,
        challenge: Option<&AuthChallenge>,
        failure: Option<String>,
    ) {
        let account = self.account_for_provider(provider_id);
        let workspace_label = account
            .as_ref()
            .map(|account| account.display_name.to_string())
            .unwrap_or_else(|| provider_id.to_string());
        let mut overlay = SlackSetupOverlay::new(provider_id.clone(), workspace_label);
        if let Some(challenge) = challenge {
            match challenge {
                AuthChallenge::OAuthUrl(url) => {
                    overlay.phase = SlackSetupPhase::OAuthPrompt;
                    overlay.oauth_url = Some(url.to_string());
                    overlay.status = Some(
                        "Open Slack in the browser, authorize, then return for validation."
                            .to_owned(),
                    );
                }
                AuthChallenge::Waiting => {
                    overlay.phase = SlackSetupPhase::EnterCredentials;
                    overlay.status = Some(
                        "Enter or configure Slack credentials for this setup method.".to_owned(),
                    );
                }
                AuthChallenge::QrCode(_) | AuthChallenge::PairingCode(_) => {
                    overlay.status = Some(auth_challenge_label(challenge).to_owned());
                }
            }
        }
        if let Some(failure) = failure {
            overlay.phase = SlackSetupPhase::Failed;
            overlay.status = Some(failure);
        }
        self.state.slack_setup = Some(overlay);
    }

    fn update_slack_setup_success(&mut self, provider_id: &ProviderId, phase: SlackSetupPhase) {
        let Some(setup) = &mut self.state.slack_setup else {
            return;
        };
        if setup.provider_id != *provider_id {
            return;
        }
        let mode = setup.selected_mode();
        setup.phase = phase;
        setup.capabilities = Some(SlackSetupCapabilities::from_mode(mode));
        setup.status = Some(
            "Slack credentials validated; review actual granted capabilities before loading chats."
                .to_owned(),
        );
    }

    fn maybe_show_notification(
        &mut self,
        message: &Message,
        selected_chat_id: Option<&chat_core::ChatId>,
        is_historical: bool,
    ) {
        if is_historical || message.is_from_me || selected_chat_id == Some(&message.chat_id) {
            return;
        }

        let Some(chat) = self
            .state
            .chats
            .iter()
            .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            .or_else(|| {
                self.state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat_id)
            })
        else {
            return;
        };
        if chat.muted {
            return;
        }

        let notification = NotificationOverlay::new(chat, message);
        let chat_name = chat.name.to_string();
        self.state.status = format!("new message in {chat_name}");
        self.state.notification = Some(notification);
    }

    fn dismiss_notification(&mut self) {
        self.state.notification = None;
    }

    fn handle_tick(&mut self) {
        let metadata_changed = self.drain_link_metadata_fetches();
        let expired = if let Some(notification) = &mut self.state.notification {
            notification.ticks_remaining = notification.ticks_remaining.saturating_sub(1);
            notification.ticks_remaining == 0
        } else {
            false
        };
        if expired {
            self.state.notification = None;
        }
        if metadata_changed {
            self.state.status = "link preview updated".to_owned();
        }
    }

    fn open_account_switcher(&mut self) {
        let selected = self
            .account_options()
            .iter()
            .position(|option| option.provider_id == self.state.active_account)
            .unwrap_or_default();
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.account_switcher = Some(AccountSwitcher { selected });
        self.state.status = "choose account".to_owned();
    }

    fn apply_account_switcher_selection(&mut self, selected: usize) -> bool {
        let Some(option) = self.account_options().get(selected).cloned() else {
            return false;
        };
        self.state.active_account = option.provider_id;
        self.state.account_switcher = None;
        let changed = self.apply_filter();
        self.state.selected_message_id = None;
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.help_overlay = None;
        self.state.reply_to = None;
        self.state.thread_root = None;
        self.state.image_viewer = None;
        self.state.message_scroll = 0;
        self.state.status = format!("filtered to {}", option.label);
        changed
    }

    fn account_options(&self) -> Vec<AccountOption> {
        let mut account_ids = self
            .state
            .account_statuses
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for chat in &self.state.chats {
            if !account_ids.iter().any(|id| id == &chat.account) {
                account_ids.push(chat.account.clone());
            }
        }
        account_ids.sort_by(|left, right| {
            let left_name = self
                .state
                .account_statuses
                .get(left)
                .map(|status| status.display_name.as_str())
                .unwrap_or(left.as_ref());
            let right_name = self
                .state
                .account_statuses
                .get(right)
                .map(|status| status.display_name.as_str())
                .unwrap_or(right.as_ref());
            left_name.cmp(right_name).then_with(|| left.cmp(right))
        });

        let mut options = vec![AccountOption {
            provider_id: None,
            label: "All accounts".to_owned(),
            summary: self.state.account_status_summary(),
            chat_count: self.state.chats.len(),
        }];
        options.extend(account_ids.into_iter().map(|provider_id| {
            let status = self.state.account_statuses.get(&provider_id);
            let label = status
                .map(|status| status.display_name.clone())
                .unwrap_or_else(|| provider_id.to_string());
            let summary = status
                .map(AccountStatus::summary)
                .unwrap_or_else(|| "unknown".to_owned());
            let chat_count = self
                .state
                .chats
                .iter()
                .filter(|chat| chat.account == provider_id)
                .count();
            AccountOption {
                provider_id: Some(provider_id),
                label,
                summary,
                chat_count,
            }
        }));
        options
    }

    fn account_filter_label(&self) -> String {
        self.state
            .active_account
            .as_ref()
            .and_then(|provider_id| self.state.account_statuses.get(provider_id))
            .map(|status| status.display_name.clone())
            .unwrap_or_else(|| {
                self.state
                    .active_account
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "All accounts".to_owned())
            })
    }

    fn account_switcher_option_at(&self, column: u16, row: u16) -> Option<usize> {
        let options = self.account_options();
        let width = self.state.frame_area.width.saturating_sub(4).clamp(32, 64);
        let height = (options.len() as u16).saturating_add(4).clamp(6, 14);
        let modal = centered_fixed_rect(self.state.frame_area, width, height);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_row = modal.y.saturating_add(3);
        let index = row.checked_sub(first_option_row)? as usize;
        (index < options.len()).then_some(index)
    }

    fn selected_outbound_capabilities(&self) -> Option<OutboundCapabilities> {
        let chat = self.state.selected_chat()?;
        self.providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .map(|provider| provider.outbound_capabilities())
    }

    fn compose_attach_menu_rect(&self, area: Rect) -> Rect {
        centered_fixed_rect(area, 44, ComposeAttachMenuItem::ALL.len() as u16 + 4)
    }

    fn compose_attach_menu_item_at(&self, column: u16, row: u16) -> Option<usize> {
        let modal = self.compose_attach_menu_rect(self.state.frame_area);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_item_row = modal.y.saturating_add(2);
        let index = row.checked_sub(first_item_row)? as usize;
        (index < ComposeAttachMenuItem::ALL.len()).then_some(index)
    }

    fn action_menu_rect(&self, area: Rect, message_id: &MessageId) -> Rect {
        self.anchored_message_popup_rect(area, message_id, 34, ActionMenuItem::ALL.len() as u16 + 4)
    }

    fn reaction_picker_rect(&self, area: Rect, message_id: &MessageId) -> Rect {
        let width = REACTION_OPTIONS
            .len()
            .saturating_mul(REACTION_OPTION_CELL_WIDTH as usize)
            .saturating_add(4) as u16;
        self.anchored_message_popup_rect(area, message_id, width, 5)
    }

    fn poll_vote_picker_rect(&self, area: Rect, picker: &PollVotePicker) -> Rect {
        let option_count = self
            .message_by_id(&picker.message_id)
            .and_then(|message| match &message.content {
                Content::Poll(poll) => Some(poll.options.len()),
                _ => None,
            })
            .unwrap_or_default();
        let height = option_count.saturating_add(5).clamp(6, 14) as u16;
        let width = area.width.saturating_sub(4).clamp(36, 72);
        self.anchored_message_popup_rect(area, &picker.message_id, width, height)
    }

    fn compose_emoticon_picker_rect(&self, area: Rect) -> Rect {
        let suggestion_count = self
            .state
            .compose_emoticon_picker
            .as_ref()
            .map(|picker| picker.matches.len())
            .unwrap_or_default();
        let height = suggestion_count.saturating_add(4).clamp(4, 12) as u16;
        let width = area.width.saturating_sub(4).clamp(36, 64);
        let compose_area = self.state.pane_areas.compose;
        let x = compose_area.x.min(area.width.saturating_sub(width));
        let fallback_y = area.height.saturating_sub(height.saturating_add(2));
        let y = compose_area.y.saturating_sub(height).max(1).min(fallback_y);
        Rect::new(x, y, width.min(area.width), height.min(area.height))
    }

    fn auth_overlay_rect(&self, area: Rect) -> Rect {
        let is_qr = self
            .state
            .auth_overlay
            .as_ref()
            .is_some_and(|overlay| matches!(overlay.challenge, AuthChallenge::QrCode(_)));
        if is_qr {
            let width = area.width.saturating_sub(2).max(1);
            let height = area.height.saturating_sub(2).max(1);
            return centered_fixed_rect(area, width, height);
        }

        let width = area
            .width
            .saturating_mul(72)
            .saturating_div(100)
            .clamp(42, 76)
            .min(area.width.saturating_sub(2).max(1));
        let height = 13.min(area.height.saturating_sub(2).max(1));
        centered_fixed_rect(area, width, height)
    }

    fn slack_setup_overlay_rect(&self, area: Rect) -> Rect {
        let width = area
            .width
            .saturating_mul(78)
            .saturating_div(100)
            .clamp(48, 92)
            .min(area.width.saturating_sub(2).max(1));
        let height = area
            .height
            .saturating_mul(72)
            .saturating_div(100)
            .clamp(14, 28)
            .min(area.height.saturating_sub(2).max(1));
        centered_fixed_rect(area, width, height)
    }

    fn help_overlay_rect(&self, area: Rect) -> Rect {
        let width = area
            .width
            .saturating_mul(70)
            .saturating_div(100)
            .clamp(34, 72)
            .min(area.width.saturating_sub(2).max(1));
        let height = area
            .height
            .saturating_mul(70)
            .saturating_div(100)
            .clamp(10, 24)
            .min(area.height.saturating_sub(2).max(1));
        centered_fixed_rect(area, width, height)
    }

    fn anchored_message_popup_rect(
        &self,
        area: Rect,
        message_id: &MessageId,
        width: u16,
        height: u16,
    ) -> Rect {
        let fallback = centered_fixed_rect(area, width, height);
        let width = width.max(1).min(area.width);
        let height = height.max(1).min(area.height);
        let content_area = inner_area(self.state.pane_areas.messages);
        let Some(hit) = self
            .state
            .message_hits
            .iter()
            .find(|hit| hit.message_id.as_ref() == message_id.as_ref())
        else {
            return fallback;
        };

        let viewport_start = self.state.message_scroll;
        let viewport_end = viewport_start.saturating_add(content_area.height as usize);
        if hit.end_line < viewport_start || hit.start_line >= viewport_end {
            return fallback;
        }

        let visible_start = hit.start_line.saturating_sub(viewport_start);
        let visible_end = hit.end_line.saturating_sub(viewport_start);
        let anchor_top = content_area.y.saturating_add(visible_start as u16);
        let anchor_bottom = content_area.y.saturating_add(visible_end as u16);
        let area_bottom = area.y.saturating_add(area.height);
        let below = anchor_bottom.saturating_add(1);
        let above = anchor_top.saturating_sub(height);
        let y = if below.saturating_add(height) <= area_bottom {
            below
        } else if anchor_top >= area.y.saturating_add(height) {
            above
        } else {
            fallback.y
        };

        let content_right = content_area.x.saturating_add(content_area.width);
        let preferred_x = if self
            .message_by_id(message_id)
            .is_some_and(|message| message.is_from_me)
        {
            content_right.saturating_sub(width.saturating_add(1))
        } else {
            content_area.x.saturating_add(1)
        };
        let max_x = area.x.saturating_add(area.width.saturating_sub(width));
        let x = preferred_x.clamp(area.x, max_x);

        Rect::new(x, y.min(area_bottom.saturating_sub(height)), width, height)
    }

    fn action_menu_item_at(&self, column: u16, row: u16, menu: &ActionMenu) -> Option<usize> {
        let modal = self.action_menu_rect(self.state.frame_area, &menu.message_id);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_item_row = modal.y.saturating_add(2);
        let index = row.checked_sub(first_item_row)? as usize;
        (index < ActionMenuItem::ALL.len()).then_some(index)
    }

    fn reaction_picker_option_at(
        &self,
        column: u16,
        row: u16,
        picker: &ReactionPicker,
    ) -> Option<usize> {
        let modal = self.reaction_picker_rect(self.state.frame_area, &picker.message_id);
        if row != modal.y.saturating_add(2) || !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_column = modal.x.saturating_add(2);
        let relative_column = column.checked_sub(first_option_column)?;
        let option = (relative_column / REACTION_OPTION_CELL_WIDTH) as usize;
        if relative_column % REACTION_OPTION_CELL_WIDTH >= REACTION_OPTION_CELL_WIDTH - 1 {
            return None;
        }
        (option < REACTION_OPTIONS.len()).then_some(option)
    }

    fn poll_vote_picker_option_at(
        &self,
        column: u16,
        row: u16,
        picker: &PollVotePicker,
    ) -> Option<usize> {
        let modal = self.poll_vote_picker_rect(self.state.frame_area, picker);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let option_count = self
            .message_by_id(&picker.message_id)
            .and_then(|message| match &message.content {
                Content::Poll(poll) => Some(poll.options.len()),
                _ => None,
            })
            .unwrap_or_default();
        let first_option_row = modal.y.saturating_add(4);
        let index = row.checked_sub(first_option_row)? as usize;
        (index < option_count).then_some(index)
    }

    fn message_line_count(&self) -> usize {
        message_list::message_line_count(
            &self.state.messages,
            self.message_content_width(),
            &self.link_metadata_cache,
        )
    }

    fn details_line_count(&self) -> usize {
        if let Some(thread_root) = &self.state.thread_root {
            return self.thread_details_line_count(thread_root);
        }

        if let Some(message_id) = &self.state.selected_message_id
            && let Some(message) = self.message_by_id(message_id)
        {
            return self.message_details_line_count(message);
        }

        self.overview_details_line_count()
    }

    fn overview_details_line_count(&self) -> usize {
        19
    }

    fn message_details_line_count(&self, message: &Message) -> usize {
        let avatar_rows = message
            .sender
            .avatar
            .as_deref()
            .filter(|path| path.exists())
            .map(|_| 7)
            .unwrap_or_default();
        let content_rows = content_copy_text(&message.content).lines().count().max(1);
        let poll_rows = match &message.content {
            Content::Poll(poll) => 2 + poll.options.len(),
            _ => 0,
        };
        14 + avatar_rows + content_rows + poll_rows
    }

    fn thread_details_line_count(&self, thread_root: &MessageId) -> usize {
        let root_rows = self
            .message_by_id(thread_root)
            .map(|message| thread_message_lines(message, self.theme).len())
            .unwrap_or(1);
        let replies = self.thread_replies(thread_root);
        let reply_rows = if replies.is_empty() {
            1
        } else {
            replies
                .iter()
                .map(|message| thread_message_lines(message, self.theme).len())
                .sum()
        };
        5 + root_rows + reply_rows
    }

    fn message_content_width(&self) -> u16 {
        self.state
            .pane_areas
            .messages
            .width
            .saturating_sub(2)
            .max(1)
    }

    fn max_message_scroll(&self) -> usize {
        let viewport_rows = inner_area(self.state.pane_areas.messages).height as usize;
        let total_lines = self.message_line_count();
        bounded_message_scroll(total_lines, viewport_rows)
    }

    fn max_details_scroll(&self) -> usize {
        let viewport_rows = inner_area(self.state.pane_areas.details).height as usize;
        bounded_message_scroll(self.details_line_count(), viewport_rows)
    }

    fn apply_filter(&mut self) -> bool {
        let previous_selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        self.state.visible_chat_indices =
            chat_list::filter_chat_indices(&self.state.chats, &self.state.filter)
                .into_iter()
                .filter(|index| {
                    self.state.active_account.as_ref().is_none_or(|active| {
                        self.state
                            .chats
                            .get(*index)
                            .is_some_and(|chat| chat.account == *active)
                    })
                })
                .collect();

        if !self
            .state
            .visible_chat_indices
            .contains(&self.state.selected_chat)
        {
            self.state.selected_chat = self
                .state
                .visible_chat_indices
                .first()
                .copied()
                .unwrap_or_default();
        }

        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        previous_selected_chat != selected_chat
    }

    fn clamp_message_scroll(&mut self) {
        self.state.message_scroll = self.state.message_scroll.min(self.max_message_scroll());
        if self.state.pending_scroll_to_latest {
            self.scroll_messages_to_bottom();
        }
    }

    fn clamp_details_scroll(&mut self) {
        self.state.details_scroll = self.state.details_scroll.min(self.max_details_scroll());
    }

    fn apply_pending_scroll_to_latest(&mut self) {
        if self.state.pending_scroll_to_latest && !self.state.pane_areas.messages.is_empty() {
            self.scroll_messages_to_bottom();
        }
    }

    fn reset_history_window_state(&mut self) {
        self.state.is_loading_older_history = false;
        self.state.older_history_exhausted = false;
        self.state.pending_scroll_to_latest = true;
    }

    fn upsert_chat_in_state(&mut self, chat: Chat) {
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        if let Some(existing) = self
            .state
            .chats
            .iter_mut()
            .find(|existing| existing.id == chat.id && existing.account == chat.account)
        {
            *existing = chat;
        } else {
            self.state.chats.push(chat);
        }
        self.state.chats.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.last_message_at.cmp(&a.last_message_at))
                .then_with(|| a.name.cmp(&b.name))
        });
        if let Some((selected_chat_id, selected_account)) = selected_chat
            && let Some(index) =
                self.state.chats.iter().position(|chat| {
                    chat.id == selected_chat_id && chat.account == selected_account
                })
        {
            self.state.selected_chat = index;
        }
        self.apply_filter();
    }

    fn filter_status(&self) -> String {
        if self.state.filter.is_empty() {
            "filter cleared".to_owned()
        } else {
            format!(
                "filter {}: {} chats",
                self.state.filter,
                self.state.visible_chat_indices.len()
            )
        }
    }

    async fn reload_chats(&mut self) -> Result<()> {
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        self.state.chats = self.store.get_all_chats().await?;

        if let Some((selected_chat_id, selected_account)) = selected_chat
            && let Some(index) =
                self.state.chats.iter().position(|chat| {
                    chat.id == selected_chat_id && chat.account == selected_account
                })
        {
            self.state.selected_chat = index;
        }

        if self.state.selected_chat >= self.state.chats.len() {
            self.state.selected_chat = self.state.chats.len().saturating_sub(1);
        }
        self.apply_filter();
        Ok(())
    }

    async fn reload_selected_messages_after_navigation(&mut self) -> Result<()> {
        let should_sync_history = self.consume_pending_history_sync_for_selected_chat();
        self.reload_selected_messages().await?;
        if should_sync_history && self.state.messages.is_empty() {
            self.sync_selected_chat_history().await?;
        }
        Ok(())
    }

    async fn sync_selected_chat_history(&mut self) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return Ok(());
        };
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
        else {
            self.state.status = format!("no provider registered for {}", chat.account);
            return Ok(());
        };

        self.state.status = format!("syncing today's messages for {}", chat.name);
        let messages = provider.history(&chat.id, None, HISTORY_LIMIT).await?;
        if messages.is_empty() {
            self.state.status = format!("no messages found for {} today", chat.name);
            return Ok(());
        }

        for message in messages {
            self.store.upsert_message(&message).await?;
        }
        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.state.status = format!("synced today's messages for {}", chat.name);
        Ok(())
    }

    async fn reload_selected_messages(&mut self) -> Result<()> {
        if let Some(chat) = self.state.selected_chat() {
            self.state.messages = self
                .store
                .get_messages_for_chat(&chat.account, &chat.id, None, HISTORY_LIMIT)
                .await?;
        } else {
            self.state.messages.clear();
        }
        self.clamp_message_scroll();
        Ok(())
    }

    async fn load_older_messages_if_at_top(&mut self) -> Result<()> {
        if self.state.message_scroll != 0
            || self.state.is_loading_older_history
            || self.state.older_history_exhausted
            || self.state.messages.is_empty()
        {
            return Ok(());
        }

        self.state.is_loading_older_history = true;
        let result = self.load_older_messages().await;
        self.state.is_loading_older_history = false;
        result
    }

    async fn load_older_messages(&mut self) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return Ok(());
        };
        let Some(before) = self.state.messages.first().map(|message| message.timestamp) else {
            return Ok(());
        };

        let previous_line_count = self.message_line_count();
        let mut older = self
            .store
            .get_messages_for_chat(&chat.account, &chat.id, Some(before), HISTORY_LIMIT)
            .await?;

        if older.is_empty()
            && let Some(provider) = self
                .providers
                .iter()
                .find(|provider| provider.id().as_ref() == chat.account.as_ref())
        {
            older = provider
                .history(&chat.id, Some(before), HISTORY_LIMIT)
                .await?;
            for message in &older {
                self.store.upsert_message(message).await?;
            }
        }

        if older.is_empty() {
            self.state.older_history_exhausted = true;
            self.state.status = format!("no older messages for {}", chat.name);
            return Ok(());
        }

        let mut seen = self
            .state
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<HashSet<_>>();
        older.retain(|message| seen.insert(message.id.clone()));

        if older.is_empty() {
            self.state.status = "older messages already loaded".to_owned();
            return Ok(());
        }

        let added_count = older.len();
        older.extend(self.state.messages.iter().cloned());
        older.sort_by_key(|message| message.timestamp);
        self.state.messages = older;

        let new_line_count = self.message_line_count();
        let added_lines = new_line_count.saturating_sub(previous_line_count);
        self.state.message_scroll = self.state.message_scroll.saturating_add(added_lines);
        self.clamp_message_scroll();
        self.state.status = format!("loaded {added_count} older messages for {}", chat.name);
        Ok(())
    }
}

fn pending_attachment_from_path(
    raw_path: &str,
    command: AttachCommandKind,
) -> Result<PendingAttachment> {
    let path = PathBuf::from(expand_home_path(raw_path));
    let metadata =
        fs::metadata(&path).with_context(|| format!("reading attachment {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("attachment must be a file: {}", path.display());
    }
    if metadata.len() > MEDIA_SEND_SIZE_LIMIT_BYTES {
        anyhow::bail!(
            "attachment is too large: {} bytes exceeds {} bytes",
            metadata.len(),
            MEDIA_SEND_SIZE_LIMIT_BYTES
        );
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("attachment path has no file name: {}", path.display()))?;
    let mime_type = infer_mime_type(&path);
    let kind = match command {
        AttachCommandKind::Sticker => PendingAttachmentKind::Sticker,
        AttachCommandKind::Image => {
            if mime_type.starts_with("image/") {
                PendingAttachmentKind::Image
            } else {
                anyhow::bail!("expected an image file, got {mime_type}");
            }
        }
        AttachCommandKind::Auto => infer_attachment_kind(&mime_type),
    };

    let media = Media {
        id: Arc::from(format!("local:{}", path.display())),
        file_name: Arc::from(file_name),
        mime_type: Arc::from(mime_type),
        size_bytes: Some(metadata.len()),
        caption: None,
        local_path: Some(path.clone()),
        thumbnail: local_thumbnail_for(&path, kind),
    };

    Ok(PendingAttachment { kind, media })
}

fn expand_home_path(raw_path: &str) -> String {
    if raw_path == "~" {
        std::env::var("HOME").unwrap_or_else(|_| raw_path.to_owned())
    } else if let Some(rest) = raw_path.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| raw_path.to_owned())
    } else {
        raw_path.to_owned()
    }
}

fn infer_attachment_kind(mime_type: &str) -> PendingAttachmentKind {
    if mime_type == "image/webp" {
        PendingAttachmentKind::Sticker
    } else if mime_type.starts_with("image/") {
        PendingAttachmentKind::Image
    } else if mime_type.starts_with("video/") {
        PendingAttachmentKind::Video
    } else if mime_type.starts_with("audio/") {
        PendingAttachmentKind::Audio
    } else {
        PendingAttachmentKind::File
    }
}

fn local_thumbnail_for(path: &Path, kind: PendingAttachmentKind) -> Option<PathBuf> {
    matches!(
        kind,
        PendingAttachmentKind::Image | PendingAttachmentKind::Sticker
    )
    .then(|| path.to_path_buf())
}

fn infer_mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("ogg") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn compose_emoticon_query(text: &str, cursor: usize) -> Option<(String, usize)> {
    let before_cursor = text.get(..cursor)?;
    let token = before_cursor
        .rsplit(|value: char| value.is_whitespace())
        .next()
        .unwrap_or_default();
    let query = token.strip_prefix(':')?;
    if query.is_empty() || query.contains(':') {
        return None;
    }
    if !query
        .chars()
        .all(|value| value.is_ascii_alphanumeric() || matches!(value, '_' | '-' | '+'))
    {
        return None;
    }
    Some((query.to_owned(), token.chars().count()))
}

fn content_send_preview(content: &Content) -> String {
    match content {
        Content::Text(text) | Content::Unsupported(text) => text.to_string(),
        Content::Image(media) => media
            .caption
            .as_deref()
            .map(|caption| format!("Image: {caption}"))
            .unwrap_or_else(|| format!("Image: {}", media.file_name)),
        Content::Video(media) => media
            .caption
            .as_deref()
            .map(|caption| format!("Video: {caption}"))
            .unwrap_or_else(|| format!("Video: {}", media.file_name)),
        Content::Audio(media) => media
            .caption
            .as_deref()
            .map(|caption| format!("Audio: {caption}"))
            .unwrap_or_else(|| format!("Audio: {}", media.file_name)),
        Content::File(media) => media
            .caption
            .as_deref()
            .map(|caption| format!("File: {caption}"))
            .unwrap_or_else(|| format!("File: {}", media.file_name)),
        Content::Sticker(media) => media
            .caption
            .as_deref()
            .map(|caption| format!("Sticker: {caption}"))
            .unwrap_or_else(|| format!("Sticker: {}", media.file_name)),
        Content::LinkPreview(link) => link
            .title
            .as_deref()
            .unwrap_or(link.url.as_ref())
            .to_owned(),
        Content::Poll(poll) => format!("Poll: {}", poll.question),
        Content::Deleted => "Deleted message".to_owned(),
    }
}

fn outbound_media_supported(capabilities: &OutboundCapabilities) -> bool {
    capabilities.image
        || capabilities.gif
        || capabilities.video
        || capabilities.audio
        || capabilities.file
        || capabilities.sticker
}

fn build_terminal_image_protocol(
    picker: &Picker,
    path: &Path,
    size: Size,
) -> std::result::Result<Protocol, String> {
    if size.width == 0 || size.height == 0 {
        return Err("image area is too small".to_owned());
    }
    let image = image::ImageReader::open(path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("detecting {}: {error}", path.display()))?
        .decode()
        .map_err(|error| format!("decoding {}: {error}", path.display()))?;
    picker
        .new_protocol(image, size, Resize::Fit(Some(FilterType::Triangle)))
        .map_err(|error| format!("rendering {}: {error}", path.display()))
}

async fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut needs_draw = true;

    while !app.state.should_quit() {
        if needs_draw {
            terminal.draw(|frame| app.draw(frame))?;
            needs_draw = false;
        }

        if app.drain_provider_events().await? {
            needs_draw = true;
            continue;
        }

        if event::poll(IDLE_POLL_TIMEOUT)? {
            match event::read()? {
                CrosstermEvent::Key(key) => app.handle_event(AppEvent::Key(key)).await?,
                CrosstermEvent::Mouse(mouse) => app.handle_event(AppEvent::Mouse(mouse)).await?,
                CrosstermEvent::Resize(width, height) => {
                    app.handle_event(AppEvent::Resize(width, height)).await?;
                }
                _ => {}
            }
            needs_draw = true;
        } else {
            let notification_visible = app.state.notification_visible();
            app.handle_event(AppEvent::Tick).await?;
            if notification_visible != app.state.notification_visible() {
                needs_draw = true;
            }
        }
    }

    terminal.draw(|frame| app.draw(frame))?;
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn new_compose_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("Type a message...");
    textarea.set_cursor_line_style(Style::default());
    textarea
}

fn bounded_message_scroll(total_lines: usize, viewport_rows: usize) -> usize {
    let viewport_rows = viewport_rows.max(1);
    if total_lines <= viewport_rows {
        return 0;
    }

    let keep_visible_rows = viewport_rows.min(total_lines).max(1);
    total_lines.saturating_sub(keep_visible_rows)
}

fn textarea_byte_cursor(textarea: &TextArea<'_>) -> usize {
    let (row, column) = textarea.cursor();
    let lines = textarea.lines();
    let mut cursor = lines
        .iter()
        .take(row)
        .map(|line| line.len() + 1)
        .sum::<usize>();

    if let Some(line) = lines.get(row) {
        cursor += line
            .char_indices()
            .nth(column)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
    }

    cursor
}

fn textarea_input(key: TextAreaKey, modifiers: KeyModifiers) -> TextAreaInput {
    TextAreaInput {
        key,
        ctrl: modifiers.contains(KeyModifiers::CONTROL),
        alt: modifiers.contains(KeyModifiers::ALT),
        shift: modifiers.contains(KeyModifiers::SHIFT),
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn clicked_column_in_hit(content_area: Rect, column: u16, start_col: u16, end_col: u16) -> bool {
    let clicked_col = column.saturating_sub(content_area.x);
    clicked_col >= start_col && clicked_col < end_col
}

fn inner_area(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

fn centered_fixed_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.max(1).min(area.width);
    let height = height.max(1).min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn help_scroll_max(content_len: usize, area: Rect) -> usize {
    let viewport = inner_area(area).height as usize;
    content_len.saturating_sub(viewport.max(1))
}

fn is_ctrl_char(key: KeyEvent, expected: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&expected))
}

fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn trimmed_option(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn slack_setup_display_value(value: &str, is_secret: bool) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "not set".to_owned();
    }
    if is_secret {
        return format!("•••••••• ({} chars)", trimmed.chars().count());
    }
    truncate_chars(trimmed, 72)
}

fn short_id(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .chars()
        .take(10)
        .collect()
}

fn reaction_sender_names(messages: &[Message]) -> HashMap<Arc<str>, String> {
    let mut names = HashMap::new();
    for message in messages {
        names
            .entry(message.sender.platform_id.clone())
            .or_insert_with(|| message.sender.display_name.to_string());
    }
    names.insert(Arc::from(LOCAL_REACTION_SENDER), "Me".to_owned());
    names
}

fn poll_vote_count(poll: &Poll, option_id: &str) -> usize {
    poll.votes
        .iter()
        .filter(|vote| {
            vote.options
                .iter()
                .any(|selected| selected.as_ref() == option_id)
        })
        .count()
}

fn poll_result_lines(
    poll: &Poll,
    sender_names: &HashMap<Arc<str>, String>,
    theme: Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if poll.votes.is_empty() {
        lines.push(Line::from(Span::styled("  No votes yet", theme.muted())));
        return lines;
    }

    for option in &poll.options {
        let voters = poll
            .votes
            .iter()
            .filter(|vote| {
                vote.options
                    .iter()
                    .any(|selected| selected.as_ref() == option.id.as_ref())
            })
            .map(|vote| {
                sender_names
                    .get(&vote.sender)
                    .cloned()
                    .unwrap_or_else(|| reaction_sender_fallback(&vote.sender))
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(format!(
            "  {} — {} vote{}",
            option.label,
            voters.len(),
            if voters.len() == 1 { "" } else { "s" }
        )));
        if voters.is_empty() {
            lines.push(Line::from(Span::styled("    none", theme.muted())));
        } else {
            lines.push(Line::from(Span::styled(
                format!("    {}", voters.join(", ")),
                theme.muted(),
            )));
        }
    }
    lines
}

fn reaction_sender_fallback(sender: &str) -> String {
    if sender == LOCAL_REACTION_SENDER {
        return "Me".to_owned();
    }
    let jid = sender
        .split('@')
        .next()
        .unwrap_or(sender)
        .split(':')
        .next()
        .unwrap_or(sender);
    short_id(jid)
}

fn account_status_summary(statuses: &HashMap<ProviderId, AccountStatus>) -> String {
    if statuses.is_empty() {
        return "none".to_owned();
    }

    let mut statuses = statuses.iter().collect::<Vec<_>>();
    statuses.sort_by(|(left_id, left), (right_id, right)| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left_id.cmp(right_id))
    });
    statuses
        .into_iter()
        .map(|(_, status)| status.summary())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn auth_challenge_label(challenge: &AuthChallenge) -> &'static str {
    match challenge {
        AuthChallenge::QrCode(_) => "scan QR code",
        AuthChallenge::PairingCode(_) => "enter pairing code",
        AuthChallenge::OAuthUrl(_) => "open OAuth URL",
        AuthChallenge::Waiting => "waiting",
    }
}

fn reply_preview(message: &Message) -> String {
    let text = content_copy_text(&message.content);
    let preview = if text.trim().is_empty() {
        "attachment".to_owned()
    } else {
        truncate_chars(text.trim(), 42)
    };
    format!("{}: {preview}", message.sender.display_name)
}

fn notification_preview(message: &Message) -> String {
    let text = content_copy_text(&message.content);
    let preview = if text.trim().is_empty() {
        "attachment".to_owned()
    } else {
        text.trim().to_owned()
    };
    truncate_chars(&preview, 80)
}

fn thread_message_lines(message: &Message, theme: Theme) -> Vec<Line<'static>> {
    let sender_style = message_list::sender_style(theme, message.is_from_me);
    let mut lines = vec![Line::from(vec![
        Span::styled(message.sender.display_name.to_string(), sender_style),
        Span::styled(
            format!(
                " · {}",
                message_list::format_message_time(message.timestamp)
            ),
            theme.muted(),
        ),
    ])];
    for line in content_copy_text(&message.content).lines() {
        let text = if line.trim().is_empty() {
            "attachment"
        } else {
            line
        };
        lines.push(Line::from(Span::raw(format!("  {text}"))));
    }
    if !message.reactions.is_empty() {
        let reactions = message
            .reactions
            .iter()
            .map(|reaction| format!("{} {}", reaction.emoji, reaction.senders.len()))
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(Span::styled(
            format!("  {reactions}"),
            theme.muted(),
        )));
    }
    lines.push(Line::from(""));
    lines
}

fn content_copy_text(content: &Content) -> String {
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
            let description = link
                .description
                .as_deref()
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            format!("{title}: {}{description}", link.url)
        }
        Content::Poll(poll) => {
            let mut text = format!("Poll: {}", poll.question);
            for (index, option) in poll.options.iter().enumerate() {
                text.push_str(&format!("\n{}. {}", index + 1, option.label));
                let votes = poll_vote_count(poll, &option.id);
                if votes > 0 {
                    text.push_str(&format!("  ({votes})"));
                }
            }
            text
        }
        Content::Deleted => String::new(),
        Content::Unsupported(kind) => format!("Unsupported message: {kind}"),
    }
}

fn message_image_preview(content: &Content) -> Option<(PathBuf, String, Option<String>)> {
    match content {
        Content::Image(media) | Content::Sticker(media) => media_image_preview(media),
        Content::LinkPreview(link) => link.image.as_ref().and_then(media_image_preview),
        Content::Video(_)
        | Content::Audio(_)
        | Content::File(_)
        | Content::Text(_)
        | Content::Poll(_)
        | Content::Deleted
        | Content::Unsupported(_) => None,
    }
}

fn media_image_preview(media: &Media) -> Option<(PathBuf, String, Option<String>)> {
    let path = media
        .local_path
        .as_ref()
        .filter(|path| path.exists())
        .or_else(|| media.thumbnail.as_ref().filter(|path| path.exists()))?
        .clone();
    Some((
        path,
        media.file_name.to_string(),
        media.caption.as_deref().map(str::to_owned),
    ))
}

async fn fetch_link_metadata(url: &str) -> Result<message_list::LinkMetadata> {
    let client = reqwest::Client::builder()
        .timeout(LINK_METADATA_FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("chat-cli/0.1 link-preview")
        .build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = decode_link_response_body(response.bytes().await?.to_vec(), content_encoding.as_deref())?;

    if is_image_response(url, content_type.as_deref()) {
        let media = cached_link_image_media(url, content_type.as_deref(), &body)?;
        return Ok(message_list::LinkMetadata {
            title: None,
            description: None,
            image_url: Some(Arc::<str>::from(url)),
            image: Some(media),
        });
    }

    let body = &body[..body.len().min(LINK_METADATA_MAX_BYTES)];
    let html = String::from_utf8_lossy(body);

    Ok(message_list::LinkMetadata {
        title: html_meta_content(&html, "og:title")
            .or_else(|| html_meta_content(&html, "twitter:title"))
            .or_else(|| html_title(&html))
            .map(Arc::<str>::from),
        description: html_meta_content(&html, "og:description")
            .or_else(|| html_meta_content(&html, "twitter:description"))
            .or_else(|| html_meta_name_content(&html, "description"))
            .map(Arc::<str>::from),
        image_url: html_meta_content(&html, "og:image")
            .or_else(|| html_meta_content(&html, "twitter:image"))
            .map(Arc::<str>::from),
        image: None,
    })
}

fn decode_link_response_body(body: Vec<u8>, content_encoding: Option<&str>) -> Result<Vec<u8>> {
    if content_encoding.is_some_and(|value| {
        value
            .split(',')
            .any(|encoding| encoding.trim().eq_ignore_ascii_case("gzip"))
    }) {
        let mut decoder = GzDecoder::new(&body[..]);
        let mut decoded = Vec::new();
        io::Read::read_to_end(&mut decoder, &mut decoded).context("decoding gzip link preview body")?;
        return Ok(decoded);
    }
    Ok(body)
}

fn is_image_response(url: &str, content_type: Option<&str>) -> bool {
    content_type
        .is_some_and(|content_type| content_type.to_ascii_lowercase().starts_with("image/"))
        || link_image_extension(url).is_some()
}

fn cached_link_image_media(
    url: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Media> {
    image::load_from_memory(body).with_context(|| format!("decoding link image {url}"))?;
    let cache_dir = std::env::temp_dir().join("chat-cli-link-previews");
    fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;
    let file_name = link_image_file_name(url, content_type);
    let path = cache_dir.join(&file_name);
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(Media {
        id: Arc::from(format!("link:{url}")),
        file_name: Arc::from(file_name),
        mime_type: Arc::from(link_image_mime_type(url, content_type)),
        size_bytes: Some(body.len() as u64),
        caption: None,
        local_path: Some(path),
        thumbnail: None,
    })
}

fn link_image_file_name(url: &str, content_type: Option<&str>) -> String {
    let extension = link_image_extension(url)
        .or_else(|| link_image_extension_for_content_type(content_type?))
        .unwrap_or("jpg");
    let mut name = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_owned))
        })
        .filter(|segment| !segment.trim().is_empty())
        .unwrap_or_else(|| "preview".to_owned());
    name = name
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.'))
        .collect::<String>();
    if name.is_empty() {
        name.push_str("preview");
    }
    if !name
        .to_ascii_lowercase()
        .ends_with(&format!(".{extension}"))
    {
        name.push('.');
        name.push_str(extension);
    }
    name
}

fn link_image_mime_type(url: &str, content_type: Option<&str>) -> String {
    content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| value.starts_with("image/"))
        .map(str::to_owned)
        .or_else(|| link_image_extension(url).map(|extension| format!("image/{extension}")))
        .unwrap_or_else(|| "image/jpeg".to_owned())
}

fn link_image_extension(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    ["jpg", "jpeg", "png", "webp", "gif"]
        .into_iter()
        .find(|extension| path.ends_with(&format!(".{extension}")))
}

fn link_image_extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let content_type = content_type.split(';').next()?.trim().to_ascii_lowercase();
    match content_type.as_str() {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn html_title(html: &str) -> Option<String> {
    let start = html.find("<title")?;
    let after_start = &html[start..];
    let content_start = after_start.find('>')? + 1;
    let after_content_start = &after_start[content_start..];
    let content_end = after_content_start.to_ascii_lowercase().find("</title>")?;
    clean_html_text(&after_content_start[..content_end])
}

fn html_meta_content(html: &str, property: &str) -> Option<String> {
    html_meta_tag_content(html, &["property", "name"], property)
}

fn html_meta_name_content(html: &str, name: &str) -> Option<String> {
    html_meta_tag_content(html, &["name"], name)
}

fn html_meta_tag_content(html: &str, key_names: &[&str], key_value: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative_start) = lower[offset..].find("<meta") {
        let start = offset + relative_start;
        let Some(relative_end) = lower[start..].find('>') else {
            break;
        };
        let end = start + relative_end + 1;
        let tag = &html[start..end];
        let matches_key = key_names.iter().any(|key| {
            html_attr_value(tag, key).is_some_and(|value| value.eq_ignore_ascii_case(key_value))
        });
        if matches_key
            && let Some(content) =
                html_attr_value(tag, "content").and_then(|value| clean_html_text(&value))
        {
            return Some(content);
        }
        offset = end;
    }
    None
}

fn html_attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = name.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative_start) = lower[offset..].find(&needle) {
        let start = offset + relative_start;
        let after_name = start + needle.len();
        let mut chars = tag[after_name..].char_indices();
        let (_, first) = chars.find(|(_, value)| !value.is_whitespace())?;
        if first != '=' {
            offset = after_name;
            continue;
        }
        let value_start = after_name + tag[after_name..].find('=')? + 1;
        let value = tag[value_start..].trim_start();
        let quote = value.chars().next()?;
        if quote == '"' || quote == '\'' {
            let rest = &value[quote.len_utf8()..];
            let end = rest.find(quote)?;
            return Some(html_unescape(&rest[..end]));
        }
        let end = value
            .find(|character: char| character.is_whitespace() || character == '>')
            .unwrap_or(value.len());
        return Some(html_unescape(&value[..end]));
    }
    None
}

fn clean_html_text(value: &str) -> Option<String> {
    let text = html_unescape(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() { None } else { Some(text) }
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
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

fn local_reaction_option(message: &Message) -> Option<usize> {
    REACTION_OPTIONS
        .iter()
        .position(|emoji| message_reacted_by_sender(message, emoji, LOCAL_REACTION_SENDER))
}

fn add_reaction(message: &mut Message, emoji: &str, sender: Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji.as_ref() == emoji)
    {
        if !reaction
            .senders
            .iter()
            .any(|candidate| candidate == &sender)
        {
            reaction.senders.push(sender);
        }
    } else {
        message.reactions.push(Reaction {
            emoji: Arc::from(emoji),
            senders: vec![sender],
        });
    }
}

fn remove_reaction(message: &mut Message, emoji: &str, sender: &Arc<str>) {
    if let Some(reaction) = message
        .reactions
        .iter_mut()
        .find(|reaction| reaction.emoji.as_ref() == emoji)
    {
        reaction.senders.retain(|candidate| candidate != sender);
    }
    message
        .reactions
        .retain(|reaction| !reaction.senders.is_empty());
}

fn render_qr_lines(payload: &str, max_width: usize) -> Option<Vec<Line<'static>>> {
    let qr = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L).ok()?;
    let symbol_width = qr.width().saturating_add(QR_QUIET_ZONE.saturating_mul(2));
    if symbol_width == 0 || symbol_width > max_width {
        return None;
    }

    let left_padding = max_width.saturating_sub(symbol_width) / 2;
    let row_count = symbol_width.div_ceil(2);
    let mut lines = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let top_y = row.saturating_mul(2);
        let bottom_y = top_y.saturating_add(1);
        let mut spans = Vec::with_capacity(symbol_width.saturating_add(1));
        if left_padding > 0 {
            spans.push(Span::raw(" ".repeat(left_padding)));
        }
        for x in 0..symbol_width {
            let top = qr_module_is_dark(&qr, x, top_y, symbol_width);
            let bottom =
                bottom_y < symbol_width && qr_module_is_dark(&qr, x, bottom_y, symbol_width);
            let cell = match (top, bottom) {
                (true, true) => "█",
                (true, false) => "▀",
                (false, true) => "▄",
                (false, false) => " ",
            };
            spans.push(Span::styled(
                cell,
                Style::default().fg(Color::Black).bg(Color::White),
            ));
        }
        lines.push(Line::from(spans));
    }
    Some(lines)
}

fn qr_module_is_dark(qr: &QrCode, x: usize, y: usize, symbol_width: usize) -> bool {
    if x < QR_QUIET_ZONE
        || y < QR_QUIET_ZONE
        || x >= symbol_width.saturating_sub(QR_QUIET_ZONE)
        || y >= symbol_width.saturating_sub(QR_QUIET_ZONE)
    {
        return false;
    }
    qr[(x - QR_QUIET_ZONE, y - QR_QUIET_ZONE)] == QrColor::Dark
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{EventBus, MockProvider, Platform};
    use crossterm::event::{KeyEventKind, KeyEventState, MouseEventKind};
    use ratatui::backend::TestBackend;
    use std::{path::PathBuf, sync::Mutex};

    #[tokio::test]
    async fn app_bootstraps_mock_provider_into_chat_list() -> Result<()> {
        let app = test_app().await?;

        assert_eq!(app.state().chats().len(), 10);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Family Weekend 🏡")
        );
        assert_eq!(app.state().messages().len(), 2);
        assert_eq!(
            app.state().visible_chat_indices(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );
        assert_eq!(app.state().filter(), "");
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_navigation_and_quit_keys() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 1);
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);
        assert_eq!(app.state().messages().len(), 2);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().should_quit());

        Ok(())
    }

    #[tokio::test]
    async fn app_marks_unread_chat_read_when_opened() -> Result<()> {
        let mut app = test_app().await?;
        assert_eq!(app.state().selected_chat().unwrap().unread_count, 4);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let chat = app.state().selected_chat().unwrap();
        assert_eq!(chat.unread_count, 0);
        let stored = app.store.get_all_chats().await?;
        let stored_chat = stored
            .iter()
            .find(|candidate| candidate.id == chat.id && candidate.account == chat.account)
            .unwrap();
        assert_eq!(stored_chat.unread_count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn app_account_switcher_filters_chats_by_account_and_returns_to_all() -> Result<()> {
        let mock = MockProvider::new();
        let second = StaticTestProvider::from_mock(
            "mock:secondary",
            "Secondary Account",
            &mock,
            "mock:secondary:",
        )?;
        let mut app = test_app_with_providers(vec![Box::new(mock), Box::new(second)]).await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 32))?;
        terminal.draw(|frame| app.draw(frame))?;

        assert_eq!(app.state().chats().len(), 20);
        assert_eq!(app.state().visible_chat_indices().len(), 20);
        assert_eq!(app.state().active_account(), None);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().account_switcher_open());
        assert_eq!(app.state().account_switcher_selected(), Some(0));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Filter by account"));
        assert!(content.contains("All accounts"));
        assert!(content.contains("Mock Account"));
        assert!(content.contains("Secondary Account"));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            30,
            17,
        )))
        .await?;

        assert!(!app.state().account_switcher_open());
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("mock:secondary")
        );
        assert_eq!(app.state().visible_chat_indices().len(), 10);
        assert!(
            app.state()
                .selected_chat()
                .is_some_and(|chat| chat.account.as_ref() == "mock:secondary")
        );
        assert!(app.state().status().contains("Secondary Account"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Account: Secondary Account"));

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        for value in "zzzz-no-match".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert!(app.state().visible_chat_indices().is_empty());
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("No chats match 'zzzz-no-match'"));
        assert!(content.contains("Account: Secondary Account"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().filter_mode());
        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().visible_chat_indices().len(), 10);

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(2));
        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(1));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert!(!app.state().account_switcher_open());
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("mock:local")
        );
        assert_eq!(app.state().visible_chat_indices().len(), 10);
        assert!(
            app.state()
                .selected_chat()
                .is_some_and(|chat| chat.account.as_ref() == "mock:local")
        );
        assert!(app.state().status().contains("Mock Account"));

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert_eq!(app.state().account_switcher_selected(), Some(1));
        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().active_account(), None);
        assert_eq!(app.state().visible_chat_indices().len(), 20);
        assert!(app.state().status().contains("All accounts"));

        Ok(())
    }

    #[tokio::test]
    async fn app_opens_scrolls_and_closes_help_overlay() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;

        app.handle_event(AppEvent::Key(key(KeyCode::Char('?'), KeyModifiers::NONE)))
            .await?;
        assert!(app.state().help_overlay_open());
        assert_eq!(app.state().help_overlay_scroll(), Some(0));
        assert_eq!(app.state().status(), "help opened");

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Help 1/"));
        assert!(content.contains("chat-cli help"));
        assert!(content.contains("Current context"));
        assert!(content.contains("Mouse/touchpad"));

        app.handle_event(AppEvent::Key(key(KeyCode::PageDown, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().help_overlay_scroll(), Some(HELP_PAGE_STEP));
        terminal.draw(|frame| app.draw(frame))?;
        let scrolled_content = buffer_text(terminal.backend().buffer());
        assert!(scrolled_content.contains("Help 9/"));
        assert!(scrolled_content.contains("Messages"));

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 60, 18)))
            .await?;
        assert_eq!(
            app.state().help_overlay_scroll(),
            Some(HELP_PAGE_STEP + HELP_MOUSE_SCROLL_STEP)
        );

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        let max_scroll = app.help_scroll_max();
        assert_eq!(app.state().help_overlay_scroll(), Some(max_scroll));
        app.handle_event(AppEvent::Key(key(KeyCode::PageDown, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().help_overlay_scroll(), Some(max_scroll));

        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().help_overlay_scroll(), Some(0));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().help_overlay_open());
        assert_eq!(app.state().status(), "help closed");

        Ok(())
    }

    #[tokio::test]
    async fn app_keeps_question_mark_as_compose_text_and_uses_f1_for_help() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        app.handle_event(AppEvent::Key(key(KeyCode::Char('H'), KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        app.handle_event(AppEvent::Key(key(KeyCode::Char('?'), KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().help_overlay_open());
        assert_eq!(app.state().compose_text(), "H?");

        app.handle_event(AppEvent::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .await?;
        assert!(app.state().help_overlay_open());
        assert_eq!(app.state().compose_text(), "H?");

        app.handle_event(AppEvent::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().help_overlay_open());

        Ok(())
    }

    #[tokio::test]
    async fn app_closes_help_overlay_when_clicking_outside() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;

        app.handle_event(AppEvent::Key(key(KeyCode::F(1), KeyModifiers::NONE)))
            .await?;
        assert!(app.state().help_overlay_open());
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            1,
            1,
        )))
        .await?;
        assert!(!app.state().help_overlay_open());
        assert_eq!(app.state().status(), "help closed");

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_filtering_and_filter_clear() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().filter_mode());

        for value in "media".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(app.state().filter(), "media");
        assert_eq!(app.state().visible_chat_indices(), &[4]);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().filter_mode());

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().filter(), "");
        assert_eq!(
            app.state().visible_chat_indices(),
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_intuitive_pane_focus_and_standard_navigation() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Details);

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().status().contains("opened Family Weekend"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 9);
        assert_eq!(app.state().messages().len(), 1);

        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);
        assert_eq!(app.state().messages().len(), 2);

        app.handle_event(AppEvent::Key(key(KeyCode::PageDown, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 5);

        app.handle_event(AppEvent::Key(key(KeyCode::PageUp, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn app_replaces_visible_historical_message_updates() -> Result<()> {
        let mut app = test_app().await?;
        let chat = app.state().selected_chat().unwrap().clone();
        let original = app.state().messages()[0].clone();
        let mut updated = original.clone();
        updated.sender.display_name = Arc::from("Updated Sender");
        updated.reactions = vec![Reaction {
            emoji: Arc::from("👍"),
            senders: vec![
                Arc::from("a"),
                Arc::from("b"),
                Arc::from("c"),
                Arc::from("d"),
                Arc::from("e"),
                Arc::from("f"),
                Arc::from("g"),
            ],
        }];

        app.handle_event(AppEvent::Provider(
            chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: updated,
                is_historical: true,
            }),
        ))
        .await?;

        let refreshed = app
            .state()
            .messages()
            .iter()
            .find(|message| message.id == original.id)
            .unwrap();
        assert_eq!(refreshed.sender.display_name.as_ref(), "Updated Sender");
        assert_eq!(refreshed.reactions[0].senders.len(), 7);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_compose_input_editing_and_send_flow() -> Result<()> {
        let mut app = test_app().await?;
        let chat = app.state().selected_chat().unwrap().clone();
        let chat_id = chat.id.clone();
        let account_id = chat.account.clone();
        let initial_count = app.state().messages().len();

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        for value in "Hello compose".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert_eq!(app.state().focus(), FocusPane::Compose);
        assert_eq!(app.state().compose_text(), "Hello compose");
        assert_eq!(app.state().compose_cursor(), "Hello compose".len());

        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('!'), KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "Hello compo!se");

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        for value in " ✨".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Backspace, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('✅'), KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "Hello compo!se ✅");

        for value in " :joy".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert!(app.state().compose_emoticon_picker_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().compose_emoticon_picker_open());
        assert_eq!(app.state().compose_text(), "Hello compo!se ✅ 😂");

        for value in " :table".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        assert!(app.state().compose_emoticon_picker_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "Hello compo!se ✅ 😂 (╯°□°）╯︵ ┻━┻");

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "");
        assert_eq!(app.state().compose_cursor(), 0);
        assert_eq!(app.state().messages().len(), initial_count + 1);
        assert!(
            app.state()
                .status()
                .contains("sent message to Family Weekend")
        );

        let persisted = app
            .store
            .get_messages_for_chat(&account_id, &chat_id, None, HISTORY_LIMIT)
            .await?;
        assert!(persisted.iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅ 😂 (╯°□°）╯︵ ┻━┻")
        }));
        let provider_history = app.providers[0]
            .history(&chat_id, None, HISTORY_LIMIT)
            .await?;
        assert!(provider_history.iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅ 😂 (╯°□°）╯︵ ┻━┻")
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_sends_single_media_attachment_with_caption() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        let temp_dir = tempfile::tempdir()?;
        let image_path = temp_dir.path().join("hello.gif");
        fs::write(&image_path, b"GIF89a")?;

        for value in image_path.to_string_lossy().chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().compose_text(), "");
        assert!(app.state().pending_attachment().is_none());
        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Image(media)
                    if media.file_name.as_ref() == "hello.gif"
                        && media.mime_type.as_ref() == "image/gif"
                        && media.caption.is_none()
                        && media.local_path.as_deref() == Some(image_path.as_path()))
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_sends_pending_media_attachment_with_caption() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);

        let temp_dir = tempfile::tempdir()?;
        let image_path = temp_dir.path().join("captioned.gif");
        fs::write(&image_path, b"GIF89a")?;

        for value in image_path.to_string_lossy().chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.open_compose_attach_menu();
        assert!(app.state().compose_attach_menu_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().compose_text(), "");
        assert!(app.state().pending_attachment().is_some());

        for value in "Look at this :)".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().compose_text(), "");
        assert!(app.state().pending_attachment().is_none());
        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Image(media)
                    if media.file_name.as_ref() == "captioned.gif"
                        && media.mime_type.as_ref() == "image/gif"
                        && media.caption.as_deref() == Some("Look at this :)")
                        && media.local_path.as_deref() == Some(image_path.as_path()))
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_sends_text_when_compose_path_does_not_exist() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let missing_path = "/tmp/chat-cli-missing-file-for-text-send.log";
        for value in missing_path.chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert!(app.state().pending_attachment().is_none());
        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == missing_path)
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_keeps_existing_path_pending_when_provider_cannot_send_files() -> Result<()> {
        let mock = MockProvider::new();
        let provider = StaticTestProvider::from_mock("text:only", "Text Only", &mock, "text:only:")?
            .with_outbound_capabilities(OutboundCapabilities::default());
        let mut app = test_app_with_providers(vec![Box::new(provider)]).await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let temp_dir = tempfile::tempdir()?;
        let file_path = temp_dir.path().join("notes.log");
        fs::write(&file_path, b"hello")?;
        let file_text = file_path.to_string_lossy().to_string();
        for value in file_text.chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let attachment = app.state().pending_attachment().expect("pending attachment");
        assert_eq!(app.state().compose_text(), "");
        assert_eq!(attachment.media.file_name.as_ref(), "notes.log");
        assert!(app.state().status().contains("cannot send it yet"));
        assert!(!app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == file_text)
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_sends_sticker_attachment() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let temp_dir = tempfile::tempdir()?;
        let sticker_path = temp_dir.path().join("ship.webp");
        fs::write(&sticker_path, b"webp")?;

        for value in sticker_path.to_string_lossy().chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.open_compose_attach_menu();
        assert!(app.state().compose_attach_menu_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Sticker(media)
                    if media.file_name.as_ref() == "ship.webp"
                        && media.mime_type.as_ref() == "image/webp"
                        && media.caption.is_none())
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_mouse_focused_compose_send_flow() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            40,
            26,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Compose);

        for value in "Clicked compose".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        assert_eq!(app.state().compose_text(), "");
        assert!(app.state().messages().iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Clicked compose")
        }));
        assert!(app.state().status().contains("sent message"));

        Ok(())
    }

    #[tokio::test]
    async fn app_ignores_removed_vim_style_shortcuts() -> Result<()> {
        let mut app = test_app().await?;

        for code in [
            KeyCode::Char('/'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Char('n'),
            KeyCode::Char('N'),
        ] {
            app.handle_event(AppEvent::Key(key(code, KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )))
        .await?;

        assert_eq!(app.state().selected_chat_index(), 0);
        assert!(!app.state().filter_mode());
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_mouse_clicks_and_touchpad_scrolling() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            8,
            3,
        )))
        .await?;
        assert_eq!(app.state().selected_chat_index(), 1);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Alice Chen")
        );
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 2, 1)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::ChatList);
        assert_eq!(app.state().selected_chat_index(), 2);

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 50, 2)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().message_scroll(), app.max_message_scroll());

        app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollUp, 2, 1)))
            .await?;
        assert_eq!(app.state().selected_chat_index(), 1);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            100,
            2,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Details);

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_visible_image_media_card() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("Photo"));
        assert!(!content.contains("actual image:"));
        assert!(content.contains("mock-screenshot.png"));
        assert!(content.contains("caption: Shared an image attachment"));
        assert!(!content.contains("Click to preview"));
        assert!(content.contains("▀"));
        assert!(content.contains("╭"));
        assert!(content.contains("╰"));
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            matches!(cell.fg, Color::Rgb(_, _, _)) || matches!(cell.bg, Color::Rgb(_, _, _))
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_sender_name_and_avatar_preview_in_details() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            8,
            3,
        )))
        .await?;
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Alice Chen")
        );
        terminal.draw(|frame| app.draw(frame))?;

        let message_id = app
            .state
            .messages
            .iter()
            .find(|message| !message.is_from_me)
            .unwrap()
            .id
            .clone();
        app.state.selected_message_id = Some(message_id);
        terminal.draw(|frame| app.draw(frame))?;

        let content = buffer_text(terminal.backend().buffer());
        for line in content.lines() {
            println!("{line}");
        }
        assert!(content.contains("Sender: Alice Chen"));
        assert!(!content.contains("Sender ID:"));
        assert!(!content.contains("mock:user:alice"));
        assert!(content.contains("Avatar: avatar preview"));
        assert!(content.contains("▀") || content.contains("▄"));
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            matches!(cell.fg, Color::Rgb(_, _, _)) || matches!(cell.bg, Color::Rgb(_, _, _))
        }));

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_message_selection_actions_reply_and_reaction_clicks() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        let selected_message_id = app.state().selected_message_id().cloned().unwrap();
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().selected_message_id().is_some());

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().action_menu_open());

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().reply_to(), Some(&selected_message_id));
        assert_eq!(app.state().focus(), FocusPane::Compose);

        for value in "Reply from test".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().reply_to(), None);
        assert!(app.state().messages().iter().any(|message| {
            message.reply_to.as_ref() == Some(&selected_message_id)
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Reply from test")
        }));

        terminal.draw(|frame| app.draw(frame))?;
        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().action_menu_open());

        let menu = app.state.action_menu.clone().unwrap();
        let menu_rect = app.action_menu_rect(app.state.frame_area, &menu.message_id);
        let react_row = menu_rect.y
            + 2
            + ActionMenuItem::ALL
                .iter()
                .position(|item| *item == ActionMenuItem::React)
                .unwrap() as u16;
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            menu_rect.x + 3,
            react_row,
        )))
        .await?;
        assert!(app.state().reaction_picker_open());

        terminal.draw(|frame| app.draw(frame))?;
        let picker = app.state.reaction_picker.clone().unwrap();
        let picker_rect = app.reaction_picker_rect(app.state.frame_area, &picker.message_id);
        let emoji_index = 3;
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            picker_rect.x + 2 + emoji_index as u16 * REACTION_OPTION_CELL_WIDTH,
            picker_rect.y + 2,
        )))
        .await?;
        assert!(!app.state().reaction_picker_open());
        let reacted = app.message_by_id(&picker.message_id).unwrap();
        assert!(
            reacted
                .reactions
                .iter()
                .any(|reaction| reaction.emoji.as_ref() == REACTION_OPTIONS[emoji_index])
        );

        let selected_message_id = app.state().selected_message_id().cloned().unwrap();
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        let mut menu = app.state.action_menu.clone().unwrap();
        menu.selected = ActionMenuItem::ALL
            .iter()
            .position(|item| *item == ActionMenuItem::React)
            .unwrap();
        app.state.action_menu = Some(menu);
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().reaction_picker_open());
        terminal.draw(|frame| app.draw(frame))?;
        let picker = app.state.reaction_picker.clone().unwrap();
        let picker_rect = app.reaction_picker_rect(app.state.frame_area, &picker.message_id);
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            picker_rect.x + 2 + emoji_index as u16 * REACTION_OPTION_CELL_WIDTH,
            picker_rect.y + 2,
        )))
        .await?;
        let reacted = app.message_by_id(&selected_message_id).unwrap();
        assert!(
            reacted
                .reactions
                .iter()
                .all(|reaction| reaction.emoji.as_ref() != REACTION_OPTIONS[emoji_index])
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_opens_thread_pane_and_shows_replies() -> Result<()> {
        let mut app = test_app().await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("#project-chat-cli")
        );
        let root_message_id = app
            .state()
            .messages()
            .iter()
            .find(|message| message.id.as_ref() == "mock:msg:team:2")
            .unwrap()
            .id
            .clone();
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::ViewThread)
            .await?;
        assert_eq!(app.state().thread_root(), Some(&root_message_id));
        assert_eq!(app.state().focus(), FocusPane::Details);
        assert!(app.state().status().contains("2 replies"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Thread"));
        assert!(content.contains("Original"));
        assert!(content.contains("2 replies"));
        assert!(content.contains("Threaded reply"));

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::Reply)
            .await?;
        for value in "Thread reply".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        app.perform_action_menu_item(root_message_id.clone(), ActionMenuItem::ViewThread)
            .await?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("3 replies"));
        assert!(content.contains("Thread reply"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().thread_root(), None);
        assert_eq!(app.state().status(), "thread closed");

        Ok(())
    }

    fn realistic_whatsapp_qr_payload() -> String {
        format!(
            "2@{},{}==,{}==,{},{}",
            "A".repeat(44),
            "B".repeat(44),
            "C".repeat(44),
            "D".repeat(64),
            "E".repeat(32)
        )
    }

    #[test]
    fn qr_renderer_encodes_payload_when_space_allows() {
        let lines = render_qr_lines("2@test-whatsapp-qr", 96).expect("qr should render");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));
        assert!(render_qr_lines("2@test-whatsapp-qr", 8).is_none());
    }

    #[test]
    fn qr_renderer_handles_realistic_whatsapp_payload_in_fullscreen_terminal() {
        let payload = realistic_whatsapp_qr_payload();
        let lines = render_qr_lines(&payload, 238).expect("realistic WhatsApp QR should render");
        let rendered = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(lines.len() <= 65);
        assert!(rendered.contains('█') || rendered.contains('▀') || rendered.contains('▄'));
    }

    #[tokio::test]
    async fn app_renders_large_auth_qr_overlay_in_fullscreen_terminal() -> Result<()> {
        let mut app = test_app().await?;
        let provider_id = app.state().provider_for_selected_chat().unwrap().clone();
        let payload = realistic_whatsapp_qr_payload();

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                Arc::from(payload.as_str()),
            ))),
        ))
        .await?;

        let mut terminal = Terminal::new(TestBackend::new(240, 70))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("WhatsApp QR login required"));
        assert!(content.contains("Open WhatsApp > Linked devices"));
        assert!(!content.contains("QR is too large"));
        assert!(content.contains("█") || content.contains("▀") || content.contains("▄"));

        Ok(())
    }

    #[tokio::test]
    async fn app_renders_and_dismisses_auth_qr_overlay() -> Result<()> {
        let mut app = test_app().await?;
        let provider_id = app.state().provider_for_selected_chat().unwrap().clone();

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                Arc::from("2@test-whatsapp-qr"),
            ))),
        ))
        .await?;

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("WhatsApp QR login required"));
        assert!(content.contains("Open WhatsApp > Linked devices"));
        assert!(!content.contains("QR is too large"));
        assert!(content.contains("█") || content.contains("▀") || content.contains("▄"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("2@test-whatsapp-qr"));

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                Arc::from("2@test-whatsapp-qr-again"),
            ))),
        ))
        .await?;
        app.handle_event(AppEvent::Provider(
            Arc::from("mock:local"),
            Box::new(ProviderEvent::AuthSucceeded),
        ))
        .await?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("2@test-whatsapp-qr-again"));

        Ok(())
    }

    #[tokio::test]
    async fn app_tracks_provider_connection_status_in_status_bar_and_details() -> Result<()> {
        let mut app = test_app().await?;
        let provider_id = app.state().provider_for_selected_chat().unwrap().clone();

        assert_eq!(app.state().account_status_summary(), "Mock Account online");

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::SyncProgress(42)),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account syncing 42%"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                Arc::from("mock-qr"),
            ))),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account auth needed: scan QR code"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::Disconnected(Some(Arc::from("network down")))),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account offline: network down"
        );

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::Reconnecting),
        ))
        .await?;
        assert_eq!(
            app.state().account_status_summary(),
            "Mock Account reconnecting"
        );

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::SyncComplete),
        ))
        .await?;
        assert_eq!(app.state().account_status_summary(), "Mock Account online");

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("Accounts: Mock Account online"));
        assert!(!content.contains("click/Ctrl+A"));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            4,
            29,
        )))
        .await?;
        assert!(app.state().account_switcher_open());
        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().account_switcher_open());

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            4,
            29,
        )))
        .await?;
        assert!(!app.state().account_switcher_open());

        Ok(())
    }

    #[tokio::test]
    async fn app_opens_slack_setup_for_unconfigured_slack_provider() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:engineering", "Engineering Slack");
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        assert!(app.state().slack_setup_open());
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("enter credentials")
        );
        assert_eq!(
            app.state().account_status_summary(),
            "Engineering Slack auth needed: waiting"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Slack sign-in"));
        assert!(content.contains("Workspace: Engineering Slack"));
        assert!(content.contains("Provider: slack:engineering"));
        assert!(content.contains("Enter or configure Slack credentials"));
        assert!(!content.contains("xoxp-"));

        Ok(())
    }

    #[tokio::test]
    async fn app_navigates_slack_setup_methods_in_robustness_order() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:setup", "Slack Setup");
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        app.state.slack_setup = Some(SlackSetupOverlay::new(
            Arc::from("slack:setup"),
            "Slack Setup".to_owned(),
        ));
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("choose workspace")
        );

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("choose auth method")
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("1. User OAuth"));
        assert!(content.contains("2. User OAuth read-only"));
        assert!(content.contains("3. Workspace-approved bot/app tokens"));
        assert!(content.contains("4. Existing approved token import"));
        assert!(content.contains("5. Manual Slack app setup"));
        assert!(content.contains("6. Incoming webhook"));

        app.handle_event(AppEvent::Key(key(KeyCode::Char('6'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("enter credentials")
        );

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Incoming webhook"));
        assert!(content.contains("send-only"));

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().slack_setup_open());

        Ok(())
    }

    #[tokio::test]
    async fn app_shows_slack_oauth_prompt_inside_setup_flow() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:oauth", "OAuth Slack");
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        let provider_id: ProviderId = Arc::from("slack:oauth");

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::OAuthUrl(
                Arc::from("https://slack.com/oauth/v2/authorize?client_id=123"),
            ))),
        ))
        .await?;

        assert!(app.state().slack_setup_open());
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("authorize in browser")
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Slack sign-in"));
        assert!(content.contains("Open this Slack authorization URL"));
        assert!(content.contains("https://slack.com/oauth"));
        assert!(!content.contains("Authentication"));

        Ok(())
    }

    #[tokio::test]
    async fn app_updates_slack_setup_after_validation_events() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:validated", "Validated Slack");
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        let provider_id: ProviderId = Arc::from("slack:validated");
        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::AuthRequired(AuthChallenge::Waiting)),
        ))
        .await?;

        app.handle_event(AppEvent::Provider(
            provider_id.clone(),
            Box::new(ProviderEvent::AuthSucceeded),
        ))
        .await?;
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("review capabilities")
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Capabilities detected"));
        assert!(content.contains("read history"));

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::SyncComplete),
        ))
        .await?;
        assert_eq!(app.state().slack_setup_phase_label(), Some("connected"));

        Ok(())
    }

    #[tokio::test]
    async fn app_shows_slack_setup_failure_without_generic_auth_overlay() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:failed", "Failed Slack");
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        let provider_id: ProviderId = Arc::from("slack:failed");

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::Disconnected(Some(Arc::from(
                "Slack auth.test failed: invalid_auth",
            )))),
        ))
        .await?;

        assert!(app.state().slack_setup_open());
        assert_eq!(app.state().slack_setup_phase_label(), Some("failed"));
        assert_eq!(
            app.state().account_status_summary(),
            "Failed Slack offline: Slack auth.test failed: invalid_auth"
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Slack setup failed"));
        assert!(content.contains("invalid_auth"));
        assert!(!content.contains("Authentication"));

        Ok(())
    }

    #[tokio::test]
    async fn app_submits_slack_setup_selection_to_provider() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:submit", "Submit Slack");
        let recorder = slack.clone();
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        app.state.slack_setup = Some(SlackSetupOverlay::new(
            Arc::from("slack:submit"),
            "Submit Slack".to_owned(),
        ));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('6'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("enter credentials")
        );

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let submissions = recorder.auth_submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(
            submissions[0].workspace_label.as_deref(),
            Some("Submit Slack")
        );
        assert_eq!(submissions[0].mode, Some(AuthSubmissionMode::Webhook));
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("review capabilities")
        );
        assert_eq!(app.state().account_status_summary(), "Submit Slack online");
        assert!(app.state().status().contains("Slack setup submitted"));

        Ok(())
    }

    #[tokio::test]
    async fn app_edits_and_redacts_slack_webhook_credentials() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:webhook-entry", "Webhook Slack");
        let recorder = slack.clone();
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        app.state.slack_setup = Some(SlackSetupOverlay::new(
            Arc::from("slack:webhook-entry"),
            "Webhook Slack".to_owned(),
        ));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('6'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let webhook_url = "https://hooks.slack.com/services/T000/B000/SECRET";
        for value in webhook_url.chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Webhook URL: ••••••••"));
        assert!(content.contains(&format!("{} chars", webhook_url.len())));
        assert!(!content.contains(webhook_url));
        assert!(!content.contains("SECRET"));

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let submissions = recorder.auth_submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].mode, Some(AuthSubmissionMode::Webhook));
        assert_eq!(submissions[0].webhook_url.as_deref(), Some(webhook_url));

        Ok(())
    }

    #[tokio::test]
    async fn app_edits_multiple_slack_token_fields() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:bot-entry", "Bot Slack");
        let recorder = slack.clone();
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        app.state.slack_setup = Some(SlackSetupOverlay::new(
            Arc::from("slack:bot-entry"),
            "Bot Slack".to_owned(),
        ));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('3'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let bot_token = "xoxb-secret-bot-token";
        for value in bot_token.chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Tab, KeyModifiers::NONE)))
            .await?;
        let app_token = "xapp-secret-app-token";
        for value in app_token.chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Bot token: ••••••••"));
        assert!(content.contains("App token: ••••••••"));
        assert!(!content.contains(bot_token));
        assert!(!content.contains(app_token));

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let submissions = recorder.auth_submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].mode, Some(AuthSubmissionMode::BotToken));
        assert_eq!(submissions[0].bot_token.as_deref(), Some(bot_token));
        assert_eq!(submissions[0].app_token.as_deref(), Some(app_token));

        Ok(())
    }

    #[tokio::test]
    async fn app_reports_slack_setup_submission_failure() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:submit-failed", "Broken Slack")
            .with_submit_error("Slack setup rejected: invalid_auth");
        let recorder = slack.clone();
        let mut app = test_app_with_providers(vec![Box::new(slack)]).await?;
        app.drain_provider_events().await?;

        app.state.slack_setup = Some(SlackSetupOverlay::new(
            Arc::from("slack:submit-failed"),
            "Broken Slack".to_owned(),
        ));
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('3'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;

        let submissions = recorder.auth_submissions();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].mode, Some(AuthSubmissionMode::BotToken));
        assert_eq!(app.state().slack_setup_phase_label(), Some("failed"));
        assert_eq!(
            app.state().account_status_summary(),
            "Broken Slack offline: Slack setup rejected: invalid_auth"
        );
        assert!(
            app.state()
                .status()
                .contains("Slack setup failed: Slack setup rejected: invalid_auth")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_shows_and_expires_notification_overlay_for_background_messages() -> Result<()> {
        let mut app = test_app().await?;
        let background_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();
        let message = test_incoming_message(
            &background_chat,
            "mock:msg:notify:alice",
            "Alice",
            "Background notification preview from Alice",
        );

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: message.clone(),
                is_historical: false,
            }),
        ))
        .await?;

        assert!(app.state().notification_visible());
        assert_eq!(
            app.state().notification_preview(),
            Some("Background notification preview from Alice")
        );
        assert!(app.state().status().contains("new message in Alice Chen"));

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Notification"));
        assert!(content.contains("New message"));
        assert!(content.contains("Alice Chen"));
        assert!(content.contains("Background notification preview from Alice"));

        for _ in 0..NOTIFICATION_TICKS {
            app.handle_event(AppEvent::Tick).await?;
        }
        assert!(!app.state().notification_visible());

        Ok(())
    }

    #[tokio::test]
    async fn app_suppresses_and_dismisses_notification_overlay() -> Result<()> {
        let mut app = test_app().await?;
        let active_chat = app.state().selected_chat().unwrap().clone();
        let muted_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.muted)
            .unwrap()
            .clone();
        let background_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();

        app.handle_event(AppEvent::Provider(
            active_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &active_chat,
                    "mock:msg:notify:active",
                    "Maya",
                    "Active chat should stay quiet",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            muted_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &muted_chat,
                    "mock:msg:notify:muted",
                    "Ops",
                    "Muted chat should stay quiet",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &background_chat,
                    "mock:msg:notify:historical",
                    "Alice",
                    "Historical sync should stay quiet",
                ),
                is_historical: true,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());

        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &background_chat,
                    "mock:msg:notify:dismiss",
                    "Alice",
                    "Dismiss me with input",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(app.state().notification_visible());

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().notification_visible());

        Ok(())
    }

    #[tokio::test]
    async fn app_mouse_click_opens_message_actions_without_opening_media() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;

        let hit = app.state.message_hits.first().cloned().unwrap();
        let line_hit = hit.line_hits.first().cloned().unwrap();
        let content_area = inner_area(app.state.pane_areas.messages);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x + content_area.width.saturating_sub(1),
            content_area.y + line_hit.line as u16,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), None);
        assert!(!app.state().action_menu_open());

        let avatar_hit = hit.avatar_hit.as_ref().unwrap();
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x + avatar_hit.end_col + 1,
            content_area.y + line_hit.line as u16,
        )))
        .await?;

        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), Some(&hit.message_id));
        assert!(app.state().action_menu_open());
        assert_eq!(app.state().status(), "message actions opened");

        Ok(())
    }
    #[tokio::test]
    async fn app_copy_action_reports_clipboard_success_or_friendly_fallback() -> Result<()> {
        let mut app = test_app().await?;
        let message_id = app.state().messages().first().unwrap().id.clone();

        app.copy_message_text(&message_id);

        assert!(
            app.state().status() == "copied message text"
                || app
                    .state()
                    .status()
                    .starts_with("clipboard unavailable; text:")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_clicks_image_thumbnail_to_open_and_close_viewer() -> Result<()> {
        let mut app = test_app().await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;

        assert!(app.state().media_hit_count() > 0);
        assert!(!app.state().image_viewer_open());
        let hit = app.state.media_hits.first().cloned().unwrap();
        let content_area = inner_area(app.state.pane_areas.messages);
        let click_row = content_area
            .y
            .saturating_add(hit.start_line.saturating_sub(app.state.message_scroll) as u16);
        let click_column = content_area.x.saturating_add(4);

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_column,
            click_row,
        )))
        .await?;
        assert!(app.state().image_viewer_open());
        assert!(!app.state().action_menu_open());
        assert!(app.state().status().contains("viewing image"));

        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!content.contains("Image Preview"));
        assert!(!content.contains("Click anywhere or press Esc to close"));
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            matches!(cell.fg, Color::Rgb(_, _, _)) || matches!(cell.bg, Color::Rgb(_, _, _))
        }));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_column,
            click_row,
        )))
        .await?;
        assert!(!app.state().image_viewer_open());
        assert_eq!(app.state().status(), "image preview closed");

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_actual_avatar_images() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| app.draw(frame))?;
        let buffer = terminal.backend().buffer();

        let (avatar_start, avatar_end) =
            chat_list::avatar_column_bounds(app.state().pane_areas.chat_list);
        assert!(
            (avatar_start..avatar_end).any(|x| rgb_cell_at(buffer, x, 1))
                && (avatar_start..avatar_end).any(|x| rgb_cell_at(buffer, x, 2)),
            "chat list should render a square-ish real PNG avatar in the first visible chat row"
        );
        assert!(
            (43..45).any(|x| rgb_cell_at(buffer, x, 1)),
            "message header should render a compact square real PNG sender avatar"
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_three_pane_shell() -> Result<()> {
        let mut app = test_app().await?;
        let backend = TestBackend::new(110, 24);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(content.contains("Messages - Family Weekend"));
        assert!(content.contains("Details"));
        assert!(content.contains("Compose - Family Weekend"));
        assert!(content.contains("Arrows"));
        assert!(content.contains("Ctrl+F"));
        assert!(content.contains("Click/tap"));
        assert!(content.contains("Click compose"));
        assert!(!content.contains("Click image"));
        assert!(!content.contains("Ctrl+J"));
        assert!(content.contains("Ctrl+A"));
        assert!(content.contains("Scroll/trackpad"));
        assert!(!content.contains("INSERT MODE"));
        assert!(!content.contains("normal mode"));
        assert!(!content.contains("type Esc"));
        assert!(!content.contains("j/k"));
        assert!(!content.contains("g/G"));
        assert!(!content.contains("n/N"));
        assert!(!content.contains("Ctrl+D/U"));

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_responsive_layout_modes() -> Result<()> {
        let mut wide_app = test_app().await?;
        let mut wide_terminal = Terminal::new(TestBackend::new(120, 28))?;
        wide_terminal.draw(|frame| wide_app.draw(frame))?;
        let wide = buffer_text(wide_terminal.backend().buffer());
        assert_eq!(wide_app.state.layout_mode, LayoutMode::Wide);
        assert!(wide.contains("Chats"));
        assert!(wide.contains("Messages - Family Weekend"));
        assert!(wide.contains("Compose - Family Weekend"));
        assert!(wide.contains("Details"));

        let mut medium_app = test_app().await?;
        let mut medium_terminal = Terminal::new(TestBackend::new(88, 24))?;
        medium_terminal.draw(|frame| medium_app.draw(frame))?;
        let medium = buffer_text(medium_terminal.backend().buffer());
        assert_eq!(medium_app.state.layout_mode, LayoutMode::Medium);
        assert!(medium.contains("Chats"));
        assert!(medium.contains("Messages - Family Weekend"));
        assert!(medium.contains("Compose - Family Weekend"));
        assert!(!medium.contains("Details"));

        let mut compact_app = test_app().await?;
        let mut compact_terminal = Terminal::new(TestBackend::new(60, 22))?;
        compact_terminal.draw(|frame| compact_app.draw(frame))?;
        let compact_chat = buffer_text(compact_terminal.backend().buffer());
        assert_eq!(compact_app.state.layout_mode, LayoutMode::Compact);
        assert!(compact_chat.contains("Chats"));
        assert!(!compact_chat.contains("Messages - Family Weekend"));

        compact_app
            .handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        compact_terminal.draw(|frame| compact_app.draw(frame))?;
        let compact_messages = buffer_text(compact_terminal.backend().buffer());
        assert_eq!(compact_app.state.layout_mode, LayoutMode::Compact);
        assert!(compact_messages.contains("Messages - Family Weekend"));
        assert!(compact_messages.contains("Compose - Family Weekend"));
        assert!(!compact_messages.contains("Details"));

        Ok(())
    }

    #[tokio::test]
    async fn app_draws_status_bar_scrollbars_and_grouped_bubbles() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 14))?;
        app.state.focus = FocusPane::Messages;
        app.state.pending_scroll_to_latest = false;
        app.state.message_scroll = 0;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());

        assert!(content.contains("Chats"));
        assert!(content.contains("Scroll/PageUp") || content.contains("browse"));
        assert!(content.contains("╭─"));
        assert!(!content.contains("continued"));
        assert!(content.contains("╰─"));

        Ok(())
    }

    #[tokio::test]
    async fn app_compose_supports_multiline_textarea_input() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        for value in "Line one".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::ALT)))
            .await?;
        for value in "Line two".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::SHIFT)))
            .await?;
        for value in "Line three".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }

        assert_eq!(app.state().focus(), FocusPane::Compose);
        assert_eq!(app.state().compose_text(), "Line one\nLine two\nLine three");
        assert_eq!(
            app.state().compose_cursor(),
            "Line one\nLine two\nLine three".len()
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_clamps_message_scroll_at_bottom() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;

        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }
        terminal.draw(|frame| app.draw(frame))?;

        let viewport_rows = inner_area(app.state.pane_areas.messages).height as usize;
        let total_lines = app.message_line_count();
        let expected_max = bounded_message_scroll(total_lines, viewport_rows);

        for _ in 0..50 {
            app.handle_event(AppEvent::Mouse(mouse(MouseEventKind::ScrollDown, 50, 2)))
                .await?;
        }

        assert_eq!(app.state().message_scroll(), expected_max);
        assert!(app.state().message_scroll() + viewport_rows >= total_lines);
        assert!(app.state().status().contains("latest"));

        Ok(())
    }

    #[tokio::test]
    async fn app_aligns_outgoing_bubbles_right_and_incoming_left() -> Result<()> {
        let mut app = test_app().await?;
        for _ in 0..4 {
            app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
                .await?;
        }

        let messages = app.state().messages().to_vec();
        let render = message_list::build_message_lines(
            &messages,
            80,
            0,
            200,
            None,
            &HashSet::new(),
            &mut app.media_preview_cache,
            &app.link_metadata_cache,
            app.theme,
        );
        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Right)
                && line_text(line).contains("Me")
        }));
        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Left)
                && line_text(line).contains("Designer")
        }));
        assert!(render.lines.iter().any(|line| {
            line.alignment == Some(ratatui::layout::Alignment::Left)
                && line_text(line).contains("Photo")
        }));

        Ok(())
    }

    #[test]
    fn html_metadata_parser_prefers_open_graph_values() {
        let html = r#"
            <html>
              <head>
                <title>Fallback Title</title>
                <meta name="description" content="Fallback description">
                <meta property="og:title" content="Open Graph Title &amp; More">
                <meta property="og:description" content="Open Graph description">
                <meta property="og:image" content="https://example.com/card.jpg">
              </head>
            </html>
        "#;

        assert_eq!(
            html_meta_content(html, "og:title").as_deref(),
            Some("Open Graph Title & More")
        );
        assert_eq!(
            html_meta_content(html, "og:description").as_deref(),
            Some("Open Graph description")
        );
        assert_eq!(
            html_meta_content(html, "og:image").as_deref(),
            Some("https://example.com/card.jpg")
        );
        assert_eq!(html_title(html).as_deref(), Some("Fallback Title"));
    }

    #[test]
    fn image_response_detection_uses_content_type_or_url_extension() {
        assert!(is_image_response(
            "https://cdn.example.com/photo",
            Some("image/jpeg; charset=binary")
        ));
        assert!(is_image_response(
            "https://cdn.example.com/photo.webp?token=abc",
            None
        ));
        assert!(!is_image_response(
            "https://example.com/story",
            Some("text/html")
        ));
    }

    #[test]
    fn cached_link_image_media_preserves_image_file_details() -> Result<()> {
        let mut bytes = Vec::new();
        {
            let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
            let dynamic = image::DynamicImage::ImageRgba8(image);
            let mut cursor = std::io::Cursor::new(&mut bytes);
            dynamic.write_to(&mut cursor, image::ImageFormat::Png)?;
        }
        let media = cached_link_image_media(
            "https://cdn.example.com/photos/card.png?signature=abc",
            Some("image/png"),
            &bytes,
        )?;

        assert_eq!(media.file_name.as_ref(), "card.png");
        assert_eq!(media.mime_type.as_ref(), "image/png");
        assert_eq!(media.size_bytes, Some(bytes.len() as u64));
        assert!(media.local_path.as_ref().is_some_and(|path| path.exists()));
        Ok(())
    }

    #[test]
    fn cached_link_image_media_decodes_gzip_encoded_image_body() -> Result<()> {
        let mut bytes = Vec::new();
        {
            let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 255, 0, 255]));
            let dynamic = image::DynamicImage::ImageRgba8(image);
            let mut cursor = std::io::Cursor::new(&mut bytes);
            dynamic.write_to(&mut cursor, image::ImageFormat::Jpeg)?;
        }

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &bytes)?;
        let compressed = encoder.finish()?;
        let decoded = decode_link_response_body(compressed, Some("gzip"))?;
        let media = cached_link_image_media(
            "https://cdn.example.com/photos/fono-hellberg-secure.jpg",
            Some("image/jpg"),
            &decoded,
        )?;

        assert_eq!(decoded, bytes);
        assert_eq!(media.file_name.as_ref(), "fono-hellberg-secure.jpg");
        assert_eq!(media.mime_type.as_ref(), "image/jpg");
        assert!(media.local_path.as_ref().is_some_and(|path| path.exists()));
        Ok(())
    }

    #[test]
    fn idle_poll_timeout_is_low_cpu_oriented() {
        assert!(IDLE_POLL_TIMEOUT >= std::time::Duration::from_millis(200));
    }

    async fn test_app() -> Result<App> {
        test_app_with_providers(vec![Box::new(MockProvider::new())]).await
    }

    async fn test_app_with_providers(providers: Vec<ProviderBox>) -> Result<App> {
        let store = Arc::new(Store::open_memory().await?);
        App::new(store, providers).await
    }

    #[derive(Clone, Debug)]
    struct StaticTestProvider {
        id: ProviderId,
        account: Account,
        chats: Vec<Chat>,
        messages: Vec<Message>,
        events: EventBus,
        auth_submissions: Arc<Mutex<Vec<AuthSubmission>>>,
        submit_error: Option<Arc<str>>,
        outbound_capabilities: OutboundCapabilities,
    }

    impl StaticTestProvider {
        fn slack_setup(id: &str, display_name: &str) -> Self {
            let id = Arc::<str>::from(id);
            let account = Account {
                id: id.clone(),
                platform: Platform::Slack,
                display_name: Arc::from(display_name),
                avatar: None,
            };
            Self {
                id,
                account,
                chats: Vec::new(),
                messages: Vec::new(),
                events: EventBus::new(),
                auth_submissions: Arc::new(Mutex::new(Vec::new())),
                submit_error: None,
                outbound_capabilities: OutboundCapabilities::default(),
            }
        }

        fn from_mock(
            id: &str,
            display_name: &str,
            mock: &MockProvider,
            prefix: &str,
        ) -> Result<Self> {
            let id = Arc::<str>::from(id);
            let account = Account {
                id: id.clone(),
                platform: Platform::Unknown("test".to_owned()),
                display_name: Arc::from(display_name),
                avatar: None,
            };
            let chats = mock
                .seed_chats()
                .into_iter()
                .map(|mut chat| {
                    chat.id = Arc::from(format!("{prefix}{}", chat.id));
                    chat.account = id.clone();
                    chat.name = Arc::from(format!("{} / {display_name}", chat.name));
                    chat
                })
                .collect::<Vec<_>>();
            let messages = mock
                .seed_messages()
                .into_iter()
                .map(|mut message| {
                    message.id = Arc::from(format!("{prefix}{}", message.id));
                    message.chat_id = Arc::from(format!("{prefix}{}", message.chat_id));
                    message.account = id.clone();
                    message.reply_to = message
                        .reply_to
                        .as_ref()
                        .map(|reply_to| Arc::from(format!("{prefix}{reply_to}")) as MessageId);
                    message.thread_id = message
                        .thread_id
                        .as_ref()
                        .map(|thread_id| Arc::from(format!("{prefix}{thread_id}")));
                    message
                })
                .collect::<Vec<_>>();

            Ok(Self {
                id,
                account,
                chats,
                messages,
                events: EventBus::new(),
                auth_submissions: Arc::new(Mutex::new(Vec::new())),
                submit_error: None,
                outbound_capabilities: OutboundCapabilities::all(),
            })
        }

        fn with_submit_error(mut self, error: &str) -> Self {
            self.submit_error = Some(Arc::from(error));
            self
        }

        fn auth_submissions(&self) -> Vec<AuthSubmission> {
            self.auth_submissions.lock().unwrap().clone()
        }

        fn with_outbound_capabilities(mut self, capabilities: OutboundCapabilities) -> Self {
            self.outbound_capabilities = capabilities;
            self
        }
    }

    #[async_trait::async_trait]
    impl Provider for StaticTestProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn platform(&self) -> Platform {
            self.account.platform.clone()
        }

        fn account_info(&self) -> Account {
            self.account.clone()
        }

        fn outbound_capabilities(&self) -> OutboundCapabilities {
            self.outbound_capabilities.clone()
        }

        async fn connect(&self) -> Result<()> {
            if self.account.platform == Platform::Slack && self.chats.is_empty() {
                self.events
                    .send(ProviderEvent::AuthRequired(AuthChallenge::Waiting));
                return Ok(());
            }
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
            Ok(self.chats.clone())
        }

        async fn history(
            &self,
            chat_id: &Arc<str>,
            before: Option<chrono::DateTime<Utc>>,
            limit: usize,
        ) -> Result<Vec<Message>> {
            let mut messages = self
                .messages
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
            _chat_id: &Arc<str>,
            _content: Content,
            _reply_to: Option<&Arc<str>>,
        ) -> Result<Arc<str>> {
            Ok(Arc::from("static:test:sent"))
        }

        async fn download_media(&self, media: &Media) -> Result<PathBuf> {
            Ok(media
                .local_path
                .clone()
                .unwrap_or_else(|| PathBuf::from(&*media.file_name)))
        }

        async fn mark_read(&self, _chat_id: &Arc<str>, _up_to: &Arc<str>) -> Result<()> {
            Ok(())
        }

        async fn react(&self, _chat_id: &Arc<str>, _message: &Message, _emoji: &str) -> Result<()> {
            Ok(())
        }

        async fn submit_auth(&self, submission: AuthSubmission) -> Result<()> {
            self.auth_submissions.lock().unwrap().push(submission);
            if let Some(error) = &self.submit_error {
                return Err(anyhow!(error.to_string()));
            }
            self.events.send(ProviderEvent::AuthSucceeded);
            self.events.send(ProviderEvent::SyncComplete);
            Ok(())
        }

        async fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
            let query = query.to_lowercase();
            Ok(self
                .messages
                .iter()
                .filter(|message| {
                    content_copy_text(&message.content)
                        .to_lowercase()
                        .contains(&query)
                })
                .take(limit)
                .cloned()
                .collect())
        }

        async fn contact_info(&self, platform_id: &Arc<str>) -> Result<Option<Sender>> {
            Ok(Some(Sender {
                platform_id: platform_id.clone(),
                display_name: platform_id.clone(),
                avatar: None,
            }))
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn test_incoming_message(chat: &Chat, id: &str, sender: &str, text: &str) -> Message {
        Message {
            id: Arc::from(id),
            chat_id: chat.id.clone(),
            account: chat.account.clone(),
            sender: Sender {
                platform_id: Arc::from(format!("mock:user:{sender}")),
                display_name: Arc::from(sender),
                avatar: None,
            },
            timestamp: Utc::now(),
            edited_at: None,
            content: Content::Text(Arc::from(text)),
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn rgb_cell_at(buffer: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
        buffer.cell((x, y)).is_some_and(|cell| {
            cell.symbol() == "▀"
                && (matches!(cell.fg, Color::Rgb(_, _, _))
                    || matches!(cell.bg, Color::Rgb(_, _, _)))
        })
    }
}
