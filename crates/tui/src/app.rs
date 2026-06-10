use crate::{
    event::AppEvent,
    theme::Theme,
    widgets::{chat_list, message_list},
};
use anyhow::{Context, Result, anyhow, bail};
use arboard::Clipboard;
#[cfg(all(
    not(test),
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
use arboard::SetExtLinux;
use chat_core::{
    Account, AccountNoticeSeverity, AuthChallenge, AuthSubmission, AuthSubmissionMode, Card, Chat,
    ChatId, ChatKind, ChatMembership, Content, DiscoveryAction, DiscoveryResult, Media, Message,
    MessageId, NetworkActivityDirection, OutboundCapabilities, Platform, PlatformData, PlatformId,
    Poll, Provider, ProviderEvent, ProviderId, Reaction, Sender, ThreadId, Timestamp,
};
use chat_notify::{DesktopNotifier, MessageNotification};
use chrono::{Duration as ChronoDuration, Utc};
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
    collections::{BTreeMap, HashMap, HashSet, hash_map::DefaultHasher},
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use storage::{
    AppSettings, AvatarThumbnailCacheRecord, AvatarThumbnailCacheUpsert, ChatActivityUpdate,
    ChatInboxStyle, ConversationPresentationSetting, ImagePreviewMode, NetworkActivityDisplay,
    NotificationMode, NotificationPauseState, NotificationScope, Store, UiThemePreset,
};
use tokio::sync::{broadcast, mpsc};
use unicode_width::UnicodeWidthStr;

const IDLE_POLL_TIMEOUT: Duration = Duration::from_millis(250);
const NETWORK_ACTIVITY_PULSE_MS: i64 = 900;
const NETWORK_ACTIVITY_RECENT_WINDOW_SECS: i64 = 60;
const NETWORK_ACTIVITY_RECENT_ARROW_IDLE_SECS: i64 = 3;
const TYPING_INDICATOR_TTL_SECS: i64 = 8;
const HISTORY_LIMIT: usize = 200;
const WHATSAPP_INITIAL_HISTORY_TARGET: usize = 50;
const WHATSAPP_INITIAL_HISTORY_PAGES: usize = 2;
const SELECTED_CHAT_MESSAGE_LIMIT: usize = 5000;
const MAX_PROVIDER_EVENTS_PER_DRAIN: usize = 16;
const MAX_COMPLETION_EVENTS_PER_DRAIN: usize = 4;
const COMPLETION_DRAIN_BUDGET: Duration = Duration::from_millis(8);
const DISCOVERY_RESULT_LIMIT: usize = 16;
const DISCOVERY_PROVIDER_RESULT_LIMIT: usize = 8;
const HISTORY_PREFETCH_SCROLL_THRESHOLD: usize = 12;
const ARCHIVE_SYNC_TICK_INTERVAL: u64 = 8;
const ARCHIVE_BACKFILL_WINDOW_DAYS: i64 = 36500;
const MOUSE_SCROLL_STEP: usize = 1;
const MESSAGE_SCROLL_STEP: usize = 3;
const IMAGE_VIEWER_MAX_WIDTH: u16 = 96;
const EMBEDDED_WHATSAPP_ICON_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x46, 0x08, 0x03, 0x00, 0x00, 0x00, 0x46, 0xf0, 0x12,
    0xb6, 0x00, 0x00, 0x00, 0x54, 0x50, 0x4c, 0x54, 0x45, 0x47, 0x70, 0x4c, 0x24, 0xd4, 0x66, 0x24,
    0xd3, 0x66, 0x20, 0xdf, 0x60, 0x25, 0xd3, 0x65, 0x24, 0xd3, 0x65, 0x24, 0xd4, 0x64, 0x24, 0xd3,
    0x65, 0x23, 0xd3, 0x66, 0x25, 0xd2, 0x65, 0x24, 0xd2, 0x65, 0x20, 0xd7, 0x68, 0x25, 0xd3, 0x66,
    0xff, 0xff, 0xff, 0x24, 0xd3, 0x65, 0xa9, 0xed, 0xc3, 0x33, 0xd6, 0x70, 0x8d, 0xe9, 0xaf, 0xe4,
    0xfa, 0xec, 0x77, 0xe4, 0xa0, 0xc9, 0xf4, 0xd9, 0x5c, 0xde, 0x8d, 0xf2, 0xfc, 0xf6, 0x40, 0xd9,
    0x79, 0x4e, 0xdb, 0x83, 0xd7, 0xf7, 0xe3, 0xbb, 0xf1, 0xcf, 0x69, 0xe1, 0x96, 0x4c, 0xcb, 0x02,
    0x5a, 0x00, 0x00, 0x00, 0x0c, 0x74, 0x52, 0x4e, 0x53, 0x00, 0x70, 0x80, 0x10, 0xd0, 0xe0, 0x38,
    0xbc, 0x54, 0x9f, 0xf0, 0x20, 0xee, 0x27, 0x17, 0xb6, 0x00, 0x00, 0x03, 0x23, 0x49, 0x44, 0x41,
    0x54, 0x58, 0xc3, 0xad, 0x58, 0xe7, 0xc2, 0xaa, 0x30, 0x0c, 0x15, 0x54, 0x96, 0x9d, 0x40, 0x19,
    0xf2, 0xfe, 0xef, 0x79, 0x69, 0x59, 0x25, 0x49, 0x19, 0xde, 0x2f, 0xff, 0x14, 0x3d, 0x24, 0xa7,
    0x27, 0xab, 0x8f, 0x47, 0xd8, 0x92, 0x28, 0x8e, 0xd3, 0xf4, 0xcd, 0xd8, 0x3b, 0x4d, 0xe3, 0x38,
    0x4a, 0x1e, 0xf7, 0xed, 0x99, 0xe5, 0x05, 0x03, 0x56, 0xe4, 0xd9, 0xf3, 0x16, 0x48, 0x96, 0xda,
    0xbf, 0x09, 0xe1, 0x83, 0x4c, 0x9f, 0xd2, 0xec, 0x3a, 0xc8, 0x9b, 0x1d, 0xd8, 0xfb, 0x1a, 0x50,
    0x32, 0x7a, 0x22, 0xc2, 0x28, 0xe3, 0xa3, 0xf4, 0x9c, 0xa5, 0x4f, 0xca, 0x2e, 0x58, 0xfa, 0x39,
    0x89, 0xa7, 0x60, 0x97, 0xac, 0x38, 0x8c, 0x2c, 0x62, 0x97, 0x2d, 0x0a, 0xa3, 0xe4, 0xec, 0x86,
    0xe5, 0x21, 0xa9, 0xbc, 0xd8, 0x2d, 0x7b, 0x3d, 0x03, 0x28, 0xe2, 0x0e, 0x8a, 0xa0, 0x71, 0x6e,
    0xfa, 0xe2, 0xfc, 0xa1, 0x78, 0x11, 0x77, 0x51, 0x04, 0xe6, 0x27, 0x62, 0x3f, 0x19, 0x38, 0xaf,
    0x8c, 0xfd, 0x68, 0x3b, 0xfd, 0x7c, 0x68, 0xd5, 0xb5, 0x95, 0xac, 0xf9, 0x68, 0x65, 0xfd, 0xad,
    0xda, 0x80, 0x0e, 0x7d, 0x3d, 0x93, 0x19, 0x30, 0x7c, 0xb9, 0x6f, 0xbd, 0x56, 0x64, 0x5e, 0x78,
    0xd9, 0x48, 0x3c, 0x36, 0x25, 0x87, 0x56, 0x56, 0x14, 0x4e, 0x72, 0xe0, 0x8c, 0x9a, 0x3d, 0x29,
    0xa5, 0xd6, 0x46, 0x6b, 0xd9, 0xcf, 0x1f, 0xbb, 0x03, 0x77, 0x30, 0xbf, 0x9d, 0x73, 0xa5, 0x91,
    0xdd, 0x7c, 0xb0, 0x62, 0xa4, 0xc9, 0x21, 0x35, 0x43, 0x98, 0x65, 0x54, 0xa5, 0xba, 0xc6, 0xfe,
    0x63, 0xe4, 0x62, 0x27, 0xa5, 0xca, 0x61, 0x6b, 0x5c, 0xc7, 0x16, 0x67, 0x04, 0x81, 0xd2, 0x13,
    0x47, 0x23, 0x2d, 0x4e, 0x85, 0x44, 0x98, 0x91, 0xcc, 0x28, 0x8b, 0x22, 0xc9, 0x63, 0xa9, 0x2c,
    0x8e, 0x21, 0xd9, 0x79, 0xc2, 0x6f, 0xad, 0x54, 0x64, 0x40, 0x6d, 0x83, 0x8d, 0x16, 0xf1, 0x4c,
    0x11, 0xac, 0x6d, 0x44, 0x41, 0xd5, 0xda, 0xa7, 0x35, 0x45, 0x72, 0x8e, 0x43, 0x2a, 0x5d, 0x44,
    0xca, 0x18, 0x85, 0xb3, 0x51, 0x12, 0x61, 0xd9, 0x0c, 0x2d, 0xf0, 0xeb, 0x1c, 0x8b, 0xaa, 0x27,
    0x63, 0xb3, 0xaf, 0xa9, 0xf1, 0x59, 0x25, 0xe0, 0x9c, 0xac, 0x33, 0x2b, 0x45, 0x5c, 0xd1, 0x61,
    0x19, 0xe0, 0x62, 0x32, 0x56, 0x08, 0x01, 0x39, 0x74, 0x12, 0x6b, 0x39, 0xad, 0x12, 0xe7, 0x8e,
    0x04, 0x30, 0xd1, 0x23, 0x46, 0xd2, 0x68, 0xdc, 0x23, 0x3d, 0xa9, 0x9f, 0x91, 0xea, 0x81, 0x5f,
    0xc7, 0x90, 0xe1, 0x51, 0xa9, 0xdf, 0xd5, 0x77, 0x4a, 0x24, 0x93, 0x78, 0x3a, 0x08, 0xb3, 0x17,
    0x9f, 0x5a, 0x65, 0xea, 0x94, 0xc6, 0x1b, 0x82, 0x1c, 0x85, 0xa5, 0x9c, 0x02, 0x18, 0xb3, 0x3a,
    0x30, 0x71, 0x33, 0x50, 0xda, 0x69, 0x10, 0x67, 0x29, 0xc8, 0x4b, 0xb3, 0x39, 0x5c, 0x53, 0x42,
    0x5b, 0x9e, 0x68, 0xd8, 0x22, 0xb0, 0xd8, 0x3d, 0x44, 0xde, 0x05, 0x60, 0x20, 0x3e, 0x80, 0x31,
    0x1b, 0x8c, 0xb0, 0x7a, 0xed, 0xd5, 0x45, 0x18, 0x1c, 0xd4, 0x52, 0x21, 0x44, 0x1f, 0xc0, 0xe9,
    0x97, 0xd3, 0xf4, 0x82, 0x0a, 0x51, 0x6c, 0x59, 0x6e, 0x56, 0x1c, 0xa1, 0xbd, 0x82, 0xce, 0x09,
    0x8a, 0x41, 0xb5, 0xd9, 0xfd, 0xc4, 0xd5, 0xaf, 0xd2, 0x4c, 0xc7, 0xdf, 0xc8, 0xd9, 0xcd, 0x96,
    0x3a, 0xf0, 0xf8, 0xc8, 0x61, 0x87, 0xc3, 0x6b, 0x63, 0xb8, 0x97, 0x1a, 0x95, 0x17, 0xf8, 0x2a,
    0x3f, 0x9c, 0x0c, 0x7e, 0x3e, 0x76, 0xbb, 0x36, 0x33, 0x11, 0xfb, 0xa5, 0x92, 0x61, 0x9f, 0x9a,
    0xa2, 0x03, 0x1e, 0x2b, 0xbf, 0xe7, 0xc9, 0x25, 0x26, 0x9c, 0x9a, 0xb0, 0xd5, 0x95, 0xf0, 0x5d,
    0xc3, 0xe6, 0x50, 0xb7, 0xf8, 0xdb, 0xe1, 0x96, 0x57, 0xe0, 0xc4, 0xd3, 0x64, 0x17, 0x9e, 0x7a,
    0x5d, 0xcb, 0xc9, 0xb2, 0x85, 0x06, 0xbe, 0x92, 0x28, 0xc5, 0x6a, 0xd0, 0x7a, 0x58, 0xcf, 0x80,
    0x2c, 0xa2, 0xb0, 0x4d, 0x95, 0xe1, 0xbe, 0x30, 0x87, 0x24, 0xa9, 0x46, 0x05, 0x1a, 0x0c, 0xa1,
    0x0a, 0x50, 0x41, 0x4b, 0x45, 0x35, 0x98, 0xbd, 0x00, 0x5d, 0xd5, 0x53, 0x47, 0xbe, 0xe0, 0x36,
    0x95, 0x12, 0x93, 0xc0, 0x77, 0xa1, 0x06, 0x37, 0xdf, 0xb6, 0xe7, 0xa4, 0xab, 0x19, 0x31, 0x0a,
    0xb8, 0xd8, 0xc7, 0x31, 0xab, 0xe4, 0x72, 0x0f, 0xa4, 0x5c, 0x59, 0x6d, 0xaa, 0xd0, 0x28, 0xb0,
    0x23, 0xd9, 0xca, 0xbe, 0x5e, 0x94, 0xf2, 0xad, 0xd6, 0xf0, 0x06, 0xe9, 0xf2, 0x02, 0x47, 0x24,
    0xb6, 0xf1, 0x6f, 0x63, 0xc7, 0x15, 0x99, 0xdd, 0x84, 0x55, 0x8f, 0x43, 0x52, 0x5d, 0x2f, 0xb8,
    0xea, 0x68, 0x6a, 0xf3, 0x5a, 0x5e, 0xbf, 0x4c, 0x7a, 0x52, 0xa2, 0xa1, 0xad, 0x36, 0xd4, 0x6c,
    0x9c, 0xf8, 0x23, 0xa4, 0xd8, 0xaa, 0xfe, 0x38, 0xa9, 0xb9, 0xb9, 0xb3, 0x95, 0x7e, 0x5e, 0x36,
    0xd2, 0xb0, 0x93, 0x11, 0x72, 0x1b, 0x68, 0xab, 0xc6, 0x1b, 0x5d, 0x05, 0xeb, 0xc6, 0x89, 0xd6,
    0x9a, 0xd4, 0x86, 0x5d, 0x18, 0x68, 0xd7, 0x43, 0x87, 0xb1, 0x8b, 0xb3, 0x0d, 0x20, 0x83, 0xc3,
    0xfe, 0xed, 0x95, 0x61, 0xaa, 0x10, 0xff, 0xb3, 0x92, 0x1d, 0xac, 0x66, 0xaf, 0xbb, 0xfe, 0x08,
    0x6a, 0x11, 0xba, 0xbd, 0xdc, 0x89, 0x3f, 0x59, 0xef, 0x42, 0x28, 0x7f, 0xb5, 0xb2, 0x4e, 0xcb,
    0xd9, 0x55, 0x87, 0xa2, 0x93, 0x75, 0x5e, 0x5c, 0x09, 0xa8, 0xc8, 0xce, 0x2f, 0x17, 0xc4, 0x19,
    0xc8, 0xe9, 0xe5, 0xc2, 0x7c, 0xd5, 0x71, 0x72, 0x43, 0x91, 0x5c, 0xbf, 0x78, 0x11, 0x01, 0x47,
    0xae, 0x5e, 0xbc, 0x6c, 0xd7, 0x40, 0xa4, 0x27, 0xd9, 0xbd, 0x5b, 0xa9, 0x2c, 0x47, 0x6b, 0xd6,
    0x3b, 0xcf, 0x7e, 0xb8, 0xde, 0x9a, 0xaf, 0xc8, 0x46, 0x71, 0xbf, 0x4e, 0xaf, 0xc8, 0xfe, 0x01,
    0xb6, 0x8d, 0xbf, 0x01, 0x69, 0xa6, 0x27, 0xd4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];
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
    (
        "(ﾉ◕ヮ◕)ﾉ*:・ﾟ✧",
        "sparkle splash magic excited ascii kaomoji",
    ),
    ("ᐛ", "dazed happy ascii kaomoji"),
    ("٩(◕‿◕｡)۶", "overjoyed jump dance happy ascii kaomoji"),
    ("(☞ﾟ∀ﾟ)☞", "ayyyy finger guns point ascii kaomoji"),
    (
        "(╯°□°)╯︵ ┻━┻",
        "classic table flip angry rage ascii kaomoji",
    ),
    ("┬┴┬┴┤(･_├┬┴┬┴", "spy wall peeking ascii kaomoji"),
    ("(ㆆ _ ㆆ)", "stunned deadpan ascii kaomoji"),
    ("(ಥ﹏ಥ)", "torrential weep cry sad tears ascii kaomoji"),
    ("┌∩┐(ಠ_ಠ)┌∩┐", "double bird angry rude ascii kaomoji"),
    ("(づ｡◕‿‿◕｡)づ", "squishy bear hug cute ascii kaomoji"),
    ("( ͡° ͜ʖ ͡°)", "legendary lenny face smirk ascii kaomoji"),
    ("ᕙ(⇀‸↼‶)ᕗ", "exhausted flex strong ascii kaomoji"),
    ("シ", "smile katakana ascii kaomoji"),
    ("¯\\_(ツ)_/¯", "shrug whatever ascii kaomoji"),
    ("(◕‿◕)", "happy content smile ascii kaomoji"),
    ("(⌒‿⌒)", "pleased relaxed closed eyes smile ascii kaomoji"),
    ("(´♡‿♡`)", "in love smitten heart eyes ascii kaomoji"),
    ("٩(ఠ益ఠ)۶", "angry rage fists up ascii kaomoji"),
    ("┐(シ)┌", "shrug dunno indifferent ascii kaomoji"),
    ("ʕ •ᴥ• ʔ", "cute bear ascii kaomoji"),
    (
        "(ノ ˘_˘)ノ　ζ|||ζ　ζ|||ζ　ζ|||ζ",
        "casting spell throwing magic ascii kaomoji",
    ),
    ("( ˘ ɜ˘) ♬♪♫", "humming singing music ascii kaomoji"),
];
const COMPOSE_EMOTICON_MAX_SUGGESTIONS: usize = 8;
const LOCAL_REACTION_SENDER: &str = "me";
const REACTION_OPTION_CELL_WIDTH: u16 = 6;
const NOTIFICATION_TICKS: u8 = 16;
const NOTIFICATION_DELIVERY_DELAY: Duration = Duration::from_secs(5);
const MAX_PENDING_NOTIFICATIONS_PER_TICK: usize = 8;
const NOTIFICATION_PAUSE_RELOAD_TICKS: u64 = 4;
const HELP_PAGE_STEP: usize = 8;
const HELP_MOUSE_SCROLL_STEP: usize = 3;
const QR_QUIET_ZONE: usize = 2;
const MEDIA_SEND_SIZE_LIMIT_BYTES: u64 = 25 * 1024 * 1024;
const LINK_METADATA_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const LINK_METADATA_MAX_BYTES: usize = 256 * 1024;
const PERF_LOG_FILE_ENV: &str = "CHAT_CLI_PERF_LOG_FILE";
const PERF_LOG_SLOW_MS_ENV: &str = "CHAT_CLI_PERF_SLOW_MS";
const DEFAULT_PERF_LOG_SLOW_MS: u64 = 25;
const EVENT_LOOP_STALL_LOG_MS: u64 = 750;
const AVATAR_THUMBNAIL_SIZE: u32 = 32;
const AVATAR_THUMBNAIL_CACHE_VERSION: i64 = 2;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum AvatarPreviewSource {
    Avatar,
    AccountBadge,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AvatarPreviewKey {
    path: PathBuf,
    width: u16,
    rows: u16,
    source: AvatarPreviewSource,
}

#[derive(Debug)]
struct AvatarPreviewFetchResult {
    key: AvatarPreviewKey,
    result: Option<Result<AvatarPreviewData, String>>,
    elapsed: Duration,
    sqlite_hit: bool,
    stale: bool,
    refresh_on_stale: bool,
    generated: bool,
    persisted: bool,
    cache_miss: bool,
}

impl AvatarPreviewFetchResult {
    fn generated(
        key: AvatarPreviewKey,
        result: Result<AvatarPreviewData, String>,
        elapsed: Duration,
        persisted: bool,
    ) -> Self {
        Self {
            key,
            result: Some(result),
            elapsed,
            sqlite_hit: false,
            stale: false,
            refresh_on_stale: false,
            generated: true,
            persisted,
            cache_miss: false,
        }
    }

    fn sqlite_hit(
        key: AvatarPreviewKey,
        result: Result<AvatarPreviewData, String>,
        elapsed: Duration,
        stale: bool,
    ) -> Self {
        Self {
            key,
            result: Some(result),
            elapsed,
            sqlite_hit: true,
            stale,
            refresh_on_stale: stale,
            generated: false,
            persisted: false,
            cache_miss: false,
        }
    }

    fn sqlite_miss(key: AvatarPreviewKey, elapsed: Duration) -> Self {
        Self {
            key,
            result: None,
            elapsed,
            sqlite_hit: false,
            stale: false,
            refresh_on_stale: false,
            generated: false,
            persisted: false,
            cache_miss: true,
        }
    }
}

#[derive(Clone, Debug)]
struct AvatarPreviewData {
    rows: chat_list::AvatarRows,
    thumbnail: Option<Arc<[u8]>>,
}

#[derive(Debug)]
struct MediaPreviewFetchResult {
    key: message_list::MediaPreviewKey,
    result: Result<Vec<Vec<Span<'static>>>, String>,
    elapsed: Duration,
}

struct ImageProtocolFetchResult {
    key: ImageProtocolKey,
    result: Result<Protocol, String>,
    elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SelectedMessagesKey {
    account: ProviderId,
    chat_id: ChatId,
}

#[derive(Debug)]
struct SelectedMessagesFetchResult {
    key: SelectedMessagesKey,
    generation: u64,
    scroll_to_bottom: bool,
    mark_read_after_load: bool,
    result: Result<Vec<Message>, String>,
    elapsed: Duration,
}

#[derive(Debug)]
struct DiscoveryFetchResult {
    query: String,
    generation: u64,
    results: Vec<DiscoveryResult>,
    errors: Vec<String>,
    elapsed: Duration,
}

#[derive(Clone, Debug)]
struct PendingSelectedMessagesLoad {
    key: SelectedMessagesKey,
    generation: u64,
}

#[derive(Default)]
struct ChatAvatarRowsResult {
    rows: HashMap<usize, chat_list::AvatarRows>,
    cached: usize,
    pending: usize,
    queued: usize,
    errors: usize,
}

#[derive(Default)]
struct AccountBadgeRowsResult {
    rows: HashMap<ProviderId, chat_list::AvatarRows>,
    cached: usize,
    pending: usize,
    queued: usize,
    errors: usize,
}

struct PerfLog {
    file: fs::File,
    slow_threshold: Duration,
}

impl PerfLog {
    fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(PERF_LOG_FILE_ENV) else {
            return Ok(None);
        };
        let slow_threshold_ms = std::env::var(PERF_LOG_SLOW_MS_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_PERF_LOG_SLOW_MS);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!("opening performance log {}", PathBuf::from(&path).display())
            })?;
        Ok(Some(Self {
            file,
            slow_threshold: Duration::from_millis(slow_threshold_ms),
        }))
    }

    fn slow_threshold(&self) -> Duration {
        self.slow_threshold
    }

    fn log(&mut self, label: &str, elapsed: Option<Duration>, details: impl AsRef<str>) {
        let details = details.as_ref().replace('\n', "\\n");
        let elapsed_ms = elapsed
            .map(|elapsed| format!(" elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0))
            .unwrap_or_default();
        let _ = writeln!(
            self.file,
            "{} label={label}{elapsed_ms} {details}",
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
    }
}

#[derive(Clone, Debug)]
struct LinkMetadataFetchResult {
    url: Arc<str>,
    metadata: message_list::LinkMetadata,
}

#[derive(Clone, Debug)]
struct HistoryFetchResult {
    account: ProviderId,
    chat_id: ChatId,
    chat_name: Arc<str>,
    before: Option<Timestamp>,
    show_status: bool,
    platform: Platform,
    result: Result<Vec<Message>, String>,
}

#[derive(Clone, Debug)]
struct ChatMembersFetchResult {
    account: ProviderId,
    chat_id: ChatId,
    result: Result<Vec<Sender>, String>,
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

pub type ProviderBox = Arc<dyn Provider>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountProviderKind {
    Slack,
    WhatsApp,
    Demo,
}

impl AccountProviderKind {
    const ALL: [Self; 3] = [Self::Slack, Self::WhatsApp, Self::Demo];

    fn label(self) -> &'static str {
        match self {
            Self::Slack => "Slack",
            Self::WhatsApp => "WhatsApp",
            Self::Demo => "Demo",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::Slack => "Workspaces, channels, DMs",
            Self::WhatsApp => "Personal and group chats",
            Self::Demo => "Explore without connecting accounts",
        }
    }

    fn setup_hint(self) -> &'static str {
        match self {
            Self::Slack => {
                "Starts a guided Slack workspace setup. Sign-in is preferred; token/webhook fallbacks remain advanced."
            }
            Self::WhatsApp => {
                "Starts WhatsApp pairing. Scan the QR code from WhatsApp > Linked devices > Link a device."
            }
            Self::Demo => {
                "Loads local sample chats so users can try the UI before connecting real accounts."
            }
        }
    }
}

pub type AccountProviderFactory =
    Arc<dyn Fn(AccountProviderKind) -> Result<ProviderBox> + Send + Sync>;

pub async fn run(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<()> {
    run_with_factory(store, providers, None).await
}

pub async fn run_with_factory(
    store: Arc<Store>,
    providers: Vec<ProviderBox>,
    provider_factory: Option<AccountProviderFactory>,
) -> Result<()> {
    let mut terminal = init_terminal()?;
    terminal.draw(draw_startup_screen)?;

    let mut app = match App::new_with_factory(store, providers, provider_factory).await {
        Ok(app) => app,
        Err(error) => {
            restore_terminal(&mut terminal)?;
            return Err(error);
        }
    };
    app.initialize_image_renderer();
    let result = run_app_loop(&mut terminal, &mut app).await;
    let restore_result = restore_terminal(&mut terminal);
    restore_result?;
    result
}

fn draw_startup_screen(frame: &mut Frame<'_>) {
    let area = frame.area();
    let paragraph = Paragraph::new(vec![
        Line::from(Span::styled(
            "chat-cli is starting…",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Connecting accounts and loading cached chats."),
        Line::from("Slack names and member details continue loading in the background."),
    ])
    .block(Block::default().title("Starting").borders(Borders::ALL))
    .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusPane {
    #[default]
    ChatList,
    Messages,
    Compose,
    Details,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum FilterScope {
    #[default]
    Chats,
    Messages,
    Thread,
}

impl FilterScope {
    fn label(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Messages => "Messages",
            Self::Thread => "Thread",
        }
    }

    fn status_label(self) -> &'static str {
        match self {
            Self::Chats => "chat",
            Self::Messages => "message",
            Self::Thread => "thread",
        }
    }
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
    fn for_area(area: Rect, compose_height: u16, thread_open: bool) -> Self {
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
            LayoutMode::Wide => Self::wide(body, status, compose_height, thread_open),
            LayoutMode::Medium => Self::medium(body, status, compose_height),
            LayoutMode::Compact => Self::compact(body, status, compose_height),
        }
    }

    fn wide(body: Rect, status: Rect, compose_height: u16, thread_open: bool) -> Self {
        let constraints = if thread_open {
            [
                Constraint::Percentage(24),
                Constraint::Percentage(36),
                Constraint::Percentage(40),
            ]
        } else {
            [
                Constraint::Percentage(30),
                Constraint::Percentage(45),
                Constraint::Percentage(25),
            ]
        };
        let [chat_list, center, details] = *Layout::default()
            .direction(Direction::Horizontal)
            .constraints(constraints)
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TerminalImageResizeMode {
    Fit,
    Scale,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ImageProtocolKey {
    path: PathBuf,
    bytes_hash: Option<u64>,
    width: u16,
    height: u16,
    resize_mode: TerminalImageResizeMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionMenuItem {
    Reply,
    ViewThread,
    React,
    Forward,
    OpenLink,
    VotePoll,
    CopyText,
    OpenImage,
    Cancel,
}

impl ActionMenuItem {
    const COMMON: [Self; 4] = [Self::Reply, Self::React, Self::Forward, Self::CopyText];

    fn label(self) -> &'static str {
        match self {
            Self::Reply => "Reply",
            Self::ViewThread => "View thread",
            Self::React => "React",
            Self::Forward => "Forward",
            Self::OpenLink => "Open link",
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
    items: Vec<ActionMenuItem>,
}

#[derive(Clone, Debug)]
struct ForwardPicker {
    message_id: MessageId,
    selected: usize,
    query: String,
    targets: Vec<ForwardTarget>,
}

#[derive(Clone, Debug)]
struct ForwardTarget {
    account: ProviderId,
    chat_id: ChatId,
    label: String,
    subtitle: String,
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
    Automatic,
    UserOAuth,
    ReadOnlyOAuth,
    BotToken,
    ImportedToken,
    ManualApp,
    Webhook,
}

impl SlackSetupMode {
    const BUNDLED: [Self; 7] = [
        Self::Automatic,
        Self::UserOAuth,
        Self::ReadOnlyOAuth,
        Self::BotToken,
        Self::ImportedToken,
        Self::ManualApp,
        Self::Webhook,
    ];
    const MANUAL: [Self; 6] = [
        Self::UserOAuth,
        Self::ReadOnlyOAuth,
        Self::BotToken,
        Self::ImportedToken,
        Self::ManualApp,
        Self::Webhook,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Automatic => "Automatic (built-in Slack app)",
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
            Self::Automatic => {
                "Recommended: use chat-cli's configured Slack app; no app template, Client ID, or Client Secret."
            }
            Self::UserOAuth => "Advanced: use your own Slack app to send as yourself.",
            Self::ReadOnlyOAuth => {
                "Advanced fallback: read conversations when write scopes are blocked."
            }
            Self::BotToken => "Approved deployment: bot identity with optional Socket Mode.",
            Self::ImportedToken => "Advanced: validate a pre-issued user, bot, or app token.",
            Self::ManualApp => "Advanced: configure client ID, secret, redirect URI, and scopes.",
            Self::Webhook => "Limited fallback: send-only webhook/app identity, no inbox.",
        }
    }

    fn credential_hint(self) -> &'static str {
        match self {
            Self::Automatic => {
                "Press Enter to open Slack's authorization page for the built-in chat-cli app. No Client ID, Client Secret, User token, or app-template URL is needed. Optionally paste an xapp- App-Level Token for realtime."
            }
            Self::UserOAuth => {
                "Create the app from the manifest, then enter Client ID and Client Secret (from Basic Information). chat-cli opens your browser to sign in and captures the token automatically. For realtime, also add an App-Level Token (xapp-...) with connections:write; without it Slack only updates on slow periodic polling."
            }
            Self::ReadOnlyOAuth => {
                "Create the app from the manifest, then enter Client ID and Client Secret (from Basic Information). chat-cli opens your browser to sign in and captures the token automatically. For realtime, also add an App-Level Token (xapp-...) with connections:write; without it Slack only updates on slow periodic polling."
            }
            Self::BotToken => {
                "Enter bot token xoxb-..., then add an App-Level Token (xapp-...) with connections:write for realtime. Without it, Slack only updates on slow periodic polling."
            }
            Self::ImportedToken => {
                "Paste a pre-approved xoxp-, xoxb-, or xapp- token for validation."
            }
            Self::ManualApp => {
                "Paste Client ID, Client Secret, Redirect URL, and token from the Slack app."
            }
            Self::Webhook => "Enter a Slack incoming webhook URL for send-only posting.",
        }
    }

    fn to_auth_submission_mode(self) -> AuthSubmissionMode {
        match self {
            Self::Automatic | Self::UserOAuth => AuthSubmissionMode::UserOAuth,
            Self::ReadOnlyOAuth => AuthSubmissionMode::ReadOnlyOAuth,
            Self::BotToken => AuthSubmissionMode::BotToken,
            Self::ImportedToken => AuthSubmissionMode::ImportedToken,
            Self::ManualApp => AuthSubmissionMode::ManualApp,
            Self::Webhook => AuthSubmissionMode::Webhook,
        }
    }

    fn next_phase(self) -> SlackSetupPhase {
        match self {
            Self::Automatic | Self::UserOAuth | Self::ReadOnlyOAuth | Self::ManualApp => {
                SlackSetupPhase::OAuthPrompt
            }
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
            SlackSetupMode::Automatic | SlackSetupMode::UserOAuth => Self {
                can_read_history: true,
                can_send_as_user: true,
                can_react: true,
                can_download_files: true,
                can_realtime: true,
                can_search: true,
                ..Self::default()
            },
            SlackSetupMode::ReadOnlyOAuth => Self {
                can_read_history: true,
                can_download_files: true,
                can_realtime: true,
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
    /// True when chat-cli ships with a configured official Slack app, so the
    /// normal OAuth path needs no app creation or client ID/secret entry.
    bundled_oauth_app: bool,
    /// True when the provider already has realtime configured externally (for
    /// example via `CHAT_CLI_SLACK_APP_TOKEN`). The setup UI uses this to hide
    /// the optional app-token field in Automatic mode.
    configured_realtime: bool,
    /// When true the overlay renders the Slack help/explanation page instead of
    /// the current phase. Toggled with `?`; non-destructive to phase state.
    show_help: bool,
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
            bundled_oauth_app: false,
            configured_realtime: false,
            show_help: false,
        }
    }

    fn selected_mode(&self) -> SlackSetupMode {
        self.available_modes()
            .get(self.selected_mode)
            .copied()
            .unwrap_or(SlackSetupMode::UserOAuth)
    }

    fn available_modes(&self) -> &'static [SlackSetupMode] {
        if self.bundled_oauth_app {
            &SlackSetupMode::BUNDLED
        } else {
            &SlackSetupMode::MANUAL
        }
    }

    fn credential_fields(&self) -> &'static [SlackSetupCredentialField] {
        match self.selected_mode() {
            SlackSetupMode::Automatic if self.configured_realtime => &[],
            SlackSetupMode::Automatic => &[SlackSetupCredentialField::AppToken],
            SlackSetupMode::UserOAuth | SlackSetupMode::ReadOnlyOAuth => &[
                SlackSetupCredentialField::UserToken,
                SlackSetupCredentialField::AppToken,
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
    avatar: Option<PathBuf>,
    connection: AccountConnection,
    detail: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct AccountNetworkActivity {
    rx_pulse_until: Option<Timestamp>,
    tx_pulse_until: Option<Timestamp>,
    rx_events: Vec<Timestamp>,
    tx_events: Vec<Timestamp>,
}

impl AccountNetworkActivity {
    fn record(&mut self, direction: NetworkActivityDirection, now: Timestamp) {
        let pulse_until = now + ChronoDuration::milliseconds(NETWORK_ACTIVITY_PULSE_MS);
        match direction {
            NetworkActivityDirection::Rx => {
                self.rx_pulse_until = Some(pulse_until);
                self.rx_events.push(now);
            }
            NetworkActivityDirection::Tx => {
                self.tx_pulse_until = Some(pulse_until);
                self.tx_events.push(now);
            }
        }
        self.prune(now);
    }

    fn prune(&mut self, now: Timestamp) {
        let cutoff = network_activity_recent_cutoff(now);
        self.rx_events.retain(|event_at| *event_at >= cutoff);
        self.tx_events.retain(|event_at| *event_at >= cutoff);
        if self
            .rx_pulse_until
            .is_some_and(|expires_at| expires_at <= now)
        {
            self.rx_pulse_until = None;
        }
        if self
            .tx_pulse_until
            .is_some_and(|expires_at| expires_at <= now)
        {
            self.tx_pulse_until = None;
        }
    }

    fn rx_active(&self, now: Timestamp) -> bool {
        self.rx_pulse_until
            .is_some_and(|expires_at| expires_at > now)
    }

    fn tx_active(&self, now: Timestamp) -> bool {
        self.tx_pulse_until
            .is_some_and(|expires_at| expires_at > now)
    }

    fn recent_rx_count(&self, now: Timestamp) -> usize {
        let cutoff = network_activity_recent_cutoff(now);
        self.rx_events
            .iter()
            .filter(|event_at| **event_at >= cutoff)
            .count()
    }

    fn recent_tx_count(&self, now: Timestamp) -> usize {
        let cutoff = network_activity_recent_cutoff(now);
        self.tx_events
            .iter()
            .filter(|event_at| **event_at >= cutoff)
            .count()
    }

    fn last_rx_event_at(&self) -> Option<Timestamp> {
        self.rx_events.iter().copied().max()
    }

    fn last_tx_event_at(&self) -> Option<Timestamp> {
        self.tx_events.iter().copied().max()
    }

    fn recent_rx_changed(&self, now: Timestamp) -> bool {
        event_changed_recently(self.last_rx_event_at(), now)
    }

    fn recent_tx_changed(&self, now: Timestamp) -> bool {
        event_changed_recently(self.last_tx_event_at(), now)
    }
}

fn network_activity_recent_cutoff(now: Timestamp) -> Timestamp {
    now - ChronoDuration::seconds(NETWORK_ACTIVITY_RECENT_WINDOW_SECS)
}

fn event_changed_recently(event_at: Option<Timestamp>, now: Timestamp) -> bool {
    event_at.is_some_and(|event_at| {
        event_at >= network_activity_recent_cutoff(now)
            && now - event_at <= ChronoDuration::seconds(NETWORK_ACTIVITY_RECENT_ARROW_IDLE_SECS)
    })
}

#[derive(Clone, Debug)]
struct TypingIndicator {
    sender: PlatformId,
    display_name: String,
    expires_at: Timestamp,
}

impl AccountStatus {
    fn new(account: &Account, connection: AccountConnection) -> Self {
        Self {
            display_name: account.display_name.to_string(),
            avatar: account.avatar.clone(),
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
    confirm_remove: Option<ProviderId>,
}

/// A single row in the Threads inbox overlay: one thread that currently has
/// unread replies, resolved with its parent chat name for display.
#[derive(Clone, Debug)]
struct ThreadInboxEntry {
    account: ProviderId,
    chat_id: ChatId,
    root_id: MessageId,
    chat_name: String,
    preview: String,
    unread_reply_count: u32,
    reply_count: u32,
    last_reply_at: Option<Timestamp>,
}

/// Aggregated, account-wide view of threads with unread replies. Mirrors the
/// "Threads" entry found in native chat apps so unread replies are discoverable
/// in one place. Navigated with the existing arrows/Enter/Esc; opening an entry
/// selects its chat and opens the thread pane.
#[derive(Clone, Debug)]
struct ThreadsInbox {
    entries: Vec<ThreadInboxEntry>,
    selected: usize,
}

#[derive(Clone, Debug)]
struct SettingsOverlay {
    selected: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsItem {
    InboxStyle,
    Theme,
    ConversationStyle,
    ImagePreviewMode,
    NetworkActivity,
    ShowMutedChats,
    ShowBrowseChannels,
    ShowEmptyChats,
    ArchiveVisibleAccounts,
    Notifications,
    NotificationScope,
}

impl SettingsItem {
    const ALL: [Self; 11] = [
        Self::InboxStyle,
        Self::Theme,
        Self::ConversationStyle,
        Self::ImagePreviewMode,
        Self::NetworkActivity,
        Self::ShowMutedChats,
        Self::ShowBrowseChannels,
        Self::ShowEmptyChats,
        Self::ArchiveVisibleAccounts,
        Self::Notifications,
        Self::NotificationScope,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::InboxStyle => "Inbox style",
            Self::Theme => "Theme",
            Self::ConversationStyle => "Conversation style",
            Self::ImagePreviewMode => "Image previews",
            Self::NetworkActivity => "Network activity",
            Self::ShowMutedChats => "Show muted chats",
            Self::ShowBrowseChannels => "Show browse channels",
            Self::ShowEmptyChats => "Show empty chats",
            Self::ArchiveVisibleAccounts => "Archive current accounts",
            Self::Notifications => "Notifications",
            Self::NotificationScope => "Notify for",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::InboxStyle => {
                "Pick the inbox flow: recent activity first, recent flat timeline, people first, groups/channels first, or account-separated."
            }
            Self::Theme => {
                "Choose the color palette used by global chrome, overlays, lists, and message accents."
            }
            Self::ConversationStyle => {
                "Choose whether each conversation matches its service or always uses a fixed WhatsApp/Slack layout."
            }
            Self::ImagePreviewMode => {
                "Choose Matrix blocks or HD terminal-native image previews. HD falls back to Matrix when the terminal cannot show images."
            }
            Self::NetworkActivity => {
                "Choose whether the status bar hides network activity, shows combined RX/TX lights, or shows recent per-account RX/TX counts."
            }
            Self::ShowMutedChats => {
                "Keep muted chats visible at the bottom when they have no unread activity."
            }
            Self::ShowBrowseChannels => "Show Slack channels you have not joined yet.",
            Self::ShowEmptyChats => "Show chats with no local message preview or timestamp yet.",
            Self::ArchiveVisibleAccounts => {
                "Smart Sync keeps opened and visible chats fresh. Start on-demand archive sync for the current account filter when you want deeper local search or AI context."
            }
            Self::Notifications => {
                "Choose one notification delivery mode: off, desktop, or in-app. Eligible notifications are delayed briefly and cancelled if you attend that chat."
            }
            Self::NotificationScope => {
                "Limit notifications to direct messages and mentions, or allow them for all messages. Group and channel messages without a mention are suppressed when limited. Has no effect while notifications are off."
            }
        }
    }

    fn value_text(self, settings: &AppSettings, archive_running: bool) -> &'static str {
        match self {
            Self::InboxStyle => chat_inbox_style_label(settings.chat_inbox_style),
            Self::Theme => ui_theme_label(settings.ui_theme),
            Self::ConversationStyle => {
                conversation_presentation_label(settings.conversation_presentation)
            }
            Self::ImagePreviewMode => image_preview_mode_label(settings.image_preview_mode),
            Self::NetworkActivity => network_activity_display_label(settings.network_activity),
            Self::ShowMutedChats => on_off(settings.show_muted_chats),
            Self::ShowBrowseChannels => on_off(settings.show_browse_channels),
            Self::ShowEmptyChats => on_off(settings.show_empty_chats),
            Self::ArchiveVisibleAccounts => {
                if archive_running {
                    "running"
                } else {
                    "start"
                }
            }
            Self::Notifications => notification_mode_label(settings.notifications),
            Self::NotificationScope => notification_scope_label(settings.notification_scope),
        }
    }

    fn checkbox(self, settings: &AppSettings) -> Option<bool> {
        match self {
            Self::ShowMutedChats => Some(settings.show_muted_chats),
            Self::ShowBrowseChannels => Some(settings.show_browse_channels),
            Self::ShowEmptyChats => Some(settings.show_empty_chats),
            Self::InboxStyle
            | Self::Theme
            | Self::ConversationStyle
            | Self::ImagePreviewMode
            | Self::NetworkActivity
            | Self::ArchiveVisibleAccounts
            | Self::Notifications
            | Self::NotificationScope => None,
        }
    }

    fn apply(self, settings: &mut AppSettings) -> bool {
        match self {
            Self::InboxStyle => {
                settings.chat_inbox_style = next_chat_inbox_style(settings.chat_inbox_style);
                true
            }
            Self::Theme => {
                settings.ui_theme = next_ui_theme(settings.ui_theme);
                false
            }
            Self::ConversationStyle => {
                settings.conversation_presentation =
                    next_conversation_presentation(settings.conversation_presentation);
                false
            }
            Self::ImagePreviewMode => {
                settings.image_preview_mode = next_image_preview_mode(settings.image_preview_mode);
                false
            }
            Self::NetworkActivity => {
                settings.network_activity =
                    next_network_activity_display(settings.network_activity);
                false
            }
            Self::ShowMutedChats => {
                settings.show_muted_chats = !settings.show_muted_chats;
                true
            }
            Self::ShowBrowseChannels => {
                settings.show_browse_channels = !settings.show_browse_channels;
                true
            }
            Self::ShowEmptyChats => {
                settings.show_empty_chats = !settings.show_empty_chats;
                true
            }
            Self::ArchiveVisibleAccounts => false,
            Self::Notifications => {
                settings.notifications = next_notification_mode(settings.notifications);
                false
            }
            Self::NotificationScope => {
                settings.notification_scope = next_notification_scope(settings.notification_scope);
                false
            }
        }
    }
}

fn notification_mode_label(mode: NotificationMode) -> &'static str {
    match mode {
        NotificationMode::Off => "off",
        NotificationMode::Desktop => "desktop",
        NotificationMode::InApp => "in-app",
    }
}

fn next_notification_mode(mode: NotificationMode) -> NotificationMode {
    match mode {
        NotificationMode::Off => NotificationMode::Desktop,
        NotificationMode::Desktop => NotificationMode::InApp,
        NotificationMode::InApp => NotificationMode::Off,
    }
}

fn notification_scope_label(scope: NotificationScope) -> &'static str {
    match scope {
        NotificationScope::All => "all messages",
        NotificationScope::DirectAndMentions => "direct & mentions",
    }
}

fn next_notification_scope(scope: NotificationScope) -> NotificationScope {
    match scope {
        NotificationScope::All => NotificationScope::DirectAndMentions,
        NotificationScope::DirectAndMentions => NotificationScope::All,
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn chat_inbox_style_label(style: ChatInboxStyle) -> &'static str {
    match style {
        ChatInboxStyle::ActivityFirst => "Activity first",
        ChatInboxStyle::RecentFlat => "Recent flat",
        ChatInboxStyle::PeopleFirst => "People first",
        ChatInboxStyle::GroupsFirst => "Groups & channels first",
        ChatInboxStyle::AccountSeparated => "Account separated",
    }
}

fn ui_theme_label(theme: UiThemePreset) -> &'static str {
    match theme {
        UiThemePreset::DefaultDark => "Default dark",
        UiThemePreset::Light => "Light",
        UiThemePreset::HighContrast => "High contrast",
        UiThemePreset::WhatsApp => "WhatsApp inspired",
        UiThemePreset::Slack => "Slack inspired",
    }
}

fn conversation_presentation_label(style: ConversationPresentationSetting) -> &'static str {
    match style {
        ConversationPresentationSetting::ProviderNative => "Match service",
        ConversationPresentationSetting::WhatsApp => "WhatsApp layout",
        ConversationPresentationSetting::Slack => "Slack layout",
    }
}

fn image_preview_mode_label(mode: ImagePreviewMode) -> &'static str {
    match mode {
        ImagePreviewMode::Matrix => "Matrix",
        ImagePreviewMode::Hd => "HD",
    }
}

fn network_activity_display_label(display: NetworkActivityDisplay) -> &'static str {
    match display {
        NetworkActivityDisplay::Hidden => "Hidden",
        NetworkActivityDisplay::CombinedLights => "Combined lights",
        NetworkActivityDisplay::RecentCounts => "Recent counts",
    }
}

fn next_ui_theme(theme: UiThemePreset) -> UiThemePreset {
    match theme {
        UiThemePreset::DefaultDark => UiThemePreset::Light,
        UiThemePreset::Light => UiThemePreset::HighContrast,
        UiThemePreset::HighContrast => UiThemePreset::WhatsApp,
        UiThemePreset::WhatsApp => UiThemePreset::Slack,
        UiThemePreset::Slack => UiThemePreset::DefaultDark,
    }
}

fn next_conversation_presentation(
    style: ConversationPresentationSetting,
) -> ConversationPresentationSetting {
    match style {
        ConversationPresentationSetting::ProviderNative => {
            ConversationPresentationSetting::WhatsApp
        }
        ConversationPresentationSetting::WhatsApp => ConversationPresentationSetting::Slack,
        ConversationPresentationSetting::Slack => ConversationPresentationSetting::ProviderNative,
    }
}

fn next_image_preview_mode(mode: ImagePreviewMode) -> ImagePreviewMode {
    match mode {
        ImagePreviewMode::Matrix => ImagePreviewMode::Hd,
        ImagePreviewMode::Hd => ImagePreviewMode::Matrix,
    }
}

fn next_network_activity_display(display: NetworkActivityDisplay) -> NetworkActivityDisplay {
    match display {
        NetworkActivityDisplay::Hidden => NetworkActivityDisplay::CombinedLights,
        NetworkActivityDisplay::CombinedLights => NetworkActivityDisplay::RecentCounts,
        NetworkActivityDisplay::RecentCounts => NetworkActivityDisplay::Hidden,
    }
}

fn next_chat_inbox_style(style: ChatInboxStyle) -> ChatInboxStyle {
    match style {
        ChatInboxStyle::ActivityFirst => ChatInboxStyle::RecentFlat,
        ChatInboxStyle::RecentFlat => ChatInboxStyle::PeopleFirst,
        ChatInboxStyle::PeopleFirst => ChatInboxStyle::GroupsFirst,
        ChatInboxStyle::GroupsFirst => ChatInboxStyle::AccountSeparated,
        ChatInboxStyle::AccountSeparated => ChatInboxStyle::ActivityFirst,
    }
}

#[derive(Clone, Debug)]
struct AccountSetupOverlay {
    selected_provider: usize,
    status: Option<String>,
}

impl AccountSetupOverlay {
    fn selected_kind(&self) -> AccountProviderKind {
        AccountProviderKind::ALL
            .get(self.selected_provider)
            .copied()
            .unwrap_or(AccountProviderKind::Slack)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountOptionKind {
    AllAccounts,
    Provider,
    AddAccount,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AccountOption {
    kind: AccountOptionKind,
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
    is_thread_reply: bool,
    ticks_remaining: u8,
}

impl NotificationOverlay {
    fn new(chat: &Chat, message: &Message, show_preview: bool) -> Self {
        let is_thread_reply = is_slack_thread_reply(message);
        Self {
            message_id: message.id.clone(),
            chat_name: chat.name.to_string(),
            sender_name: message.sender.display_name.to_string(),
            preview: if show_preview {
                if is_thread_reply {
                    format!("↪ {}", notification_preview(message))
                } else {
                    notification_preview(message)
                }
            } else {
                "New message".to_owned()
            },
            is_thread_reply,
            ticks_remaining: NOTIFICATION_TICKS,
        }
    }

    fn to_desktop_notification(&self) -> MessageNotification {
        MessageNotification {
            chat_name: self.chat_name.clone(),
            sender_name: if self.is_thread_reply && !self.sender_name.trim().is_empty() {
                format!("{} replied to a thread", self.sender_name)
            } else {
                self.sender_name.clone()
            },
            preview: Some(self.preview.clone()),
        }
    }

    fn account_notice(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            message_id: Arc::from(format!(
                "account-notice:{}",
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            )),
            chat_name: title.into(),
            sender_name: "Account notice".to_owned(),
            preview: body.into(),
            is_thread_reply: false,
            ticks_remaining: NOTIFICATION_TICKS,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingNotification {
    account: ProviderId,
    chat_id: ChatId,
    deliver_at: Instant,
    notification: NotificationOverlay,
}

impl PendingNotification {
    fn key_matches(&self, account: &ProviderId, chat_id: &ChatId) -> bool {
        self.account == *account && self.chat_id == *chat_id
    }
}

#[derive(Debug)]
pub struct AppState {
    chats: Vec<Chat>,
    visible_chat_indices: Vec<usize>,
    messages: Vec<Message>,
    filtered_messages: Vec<Message>,
    selected_chat: usize,
    filter: String,
    filter_scope: FilterScope,
    discovery_results: Vec<DiscoveryResult>,
    filter_mode: bool,
    compose: TextArea<'static>,
    compose_text: String,
    compose_cursor: usize,
    thread_compose: TextArea<'static>,
    thread_compose_text: String,
    thread_compose_cursor: usize,
    focus: FocusPane,
    layout_mode: LayoutMode,
    pane_areas: PaneAreas,
    media_hits: Vec<message_list::MediaHit>,
    message_hits: Vec<message_list::MessageHit>,
    selected_message_id: Option<MessageId>,
    action_menu: Option<ActionMenu>,
    forward_picker: Option<ForwardPicker>,
    reaction_picker: Option<ReactionPicker>,
    compose_emoticon_picker: Option<ComposeEmoticonPicker>,
    compose_attach_menu: Option<ComposeAttachMenu>,
    poll_vote_picker: Option<PollVotePicker>,
    help_overlay: Option<HelpOverlay>,
    auth_overlay: Option<AuthOverlay>,
    account_setup: Option<AccountSetupOverlay>,
    slack_setup: Option<SlackSetupOverlay>,
    // Slack provider whose browser OAuth login was dispatched as a background
    // task. When its sync completes we run the post-auth chat load that the
    // inline submission path would otherwise perform synchronously.
    pending_slack_setup_load: Option<ProviderId>,
    account_switcher: Option<AccountSwitcher>,
    /// The Threads inbox overlay, when open.
    threads_inbox: Option<ThreadsInbox>,
    /// A thread to open once the target chat's messages have loaded. Set when
    /// activating a Threads-inbox entry for a chat that is not yet loaded;
    /// consumed by the message-load drain (and immediately when already loaded).
    pending_thread_open: Option<MessageId>,
    settings_overlay: Option<SettingsOverlay>,
    active_account: Option<ProviderId>,
    notification: Option<NotificationOverlay>,
    pending_notifications: Vec<PendingNotification>,
    notification_pause: NotificationPauseState,
    account_statuses: HashMap<ProviderId, AccountStatus>,
    network_activity: HashMap<ProviderId, AccountNetworkActivity>,
    typing_indicators: HashMap<(ProviderId, ChatId), Vec<TypingIndicator>>,
    reply_to: Option<MessageId>,
    pending_attachment: Option<PendingAttachment>,
    thread_root: Option<MessageId>,
    /// Number of replies that were unread when the currently open thread was
    /// opened. Captured synchronously in `open_thread` (before the async
    /// mark-read flush clears the counter) so the thread pane can render a
    /// "new replies" divider above the first previously-unread reply.
    thread_open_unread: u32,
    /// A thread that was just opened and needs its unread counter cleared. Set
    /// synchronously by `open_thread` and drained asynchronously after event
    /// handling so the sync open path does not block on storage.
    pending_thread_read: Option<(ProviderId, ThreadId)>,
    image_viewer: Option<ImageViewer>,
    message_scroll: usize,
    message_top_padding: usize,
    details_scroll: usize,
    is_loading_older_history: bool,
    older_history_exhausted: bool,
    pending_scroll_to_latest: bool,
    frame_area: Rect,
    should_quit: bool,
    status: String,
    pending_history_sync_chat: Option<(ProviderId, ChatId)>,
    synced_history_chats: HashSet<(ProviderId, ChatId)>,
    loading_history_chats: HashSet<(ProviderId, ChatId, Option<Timestamp>)>,
    monthly_backfill_ready_accounts: HashSet<ProviderId>,
    monthly_backfill_exhausted_chats: HashSet<(ProviderId, ChatId)>,
    monthly_backfill_cursor: usize,
    monthly_backfill_tick: u64,
    chat_members: HashMap<(ProviderId, ChatId), Vec<Sender>>,
    loading_chat_members: HashSet<(ProviderId, ChatId)>,
    /// Per-thread unread reply counts for the selected chat, keyed by the
    /// thread's root message id. Populated asynchronously after messages load
    /// so the draw path can render the "N new" badge without touching storage.
    thread_unread: HashMap<MessageId, u32>,
    /// Aggregated unread thread-reply counts per chat id across loaded
    /// accounts, used to render the sidebar `⤷N` thread-activity marker.
    /// Refreshed off the draw path alongside `thread_unread`.
    thread_unread_by_chat: HashMap<ChatId, u32>,
}

impl Default for AppState {
    fn default() -> Self {
        let mut state = Self {
            chats: Vec::new(),
            visible_chat_indices: Vec::new(),
            messages: Vec::new(),
            filtered_messages: Vec::new(),
            selected_chat: 0,
            filter: String::new(),
            filter_scope: FilterScope::Chats,
            discovery_results: Vec::new(),
            filter_mode: false,
            compose: new_compose_textarea(),
            compose_text: String::new(),
            compose_cursor: 0,
            thread_compose: new_thread_compose_textarea(),
            thread_compose_text: String::new(),
            thread_compose_cursor: 0,
            focus: FocusPane::ChatList,
            layout_mode: LayoutMode::Wide,
            pane_areas: PaneAreas::default(),
            media_hits: Vec::new(),
            message_hits: Vec::new(),
            selected_message_id: None,
            action_menu: None,
            forward_picker: None,
            reaction_picker: None,
            compose_emoticon_picker: None,
            compose_attach_menu: None,
            poll_vote_picker: None,
            help_overlay: None,
            auth_overlay: None,
            account_setup: None,
            slack_setup: None,
            pending_slack_setup_load: None,
            account_switcher: None,
            threads_inbox: None,
            pending_thread_open: None,
            settings_overlay: None,
            active_account: None,
            notification: None,
            pending_notifications: Vec::new(),
            notification_pause: NotificationPauseState::default(),
            account_statuses: HashMap::new(),
            network_activity: HashMap::new(),
            typing_indicators: HashMap::new(),
            reply_to: None,
            pending_attachment: None,
            thread_root: None,
            thread_open_unread: 0,
            pending_thread_read: None,
            image_viewer: None,
            message_scroll: 0,
            message_top_padding: 0,
            details_scroll: 0,
            is_loading_older_history: false,
            older_history_exhausted: false,
            pending_scroll_to_latest: false,
            frame_area: Rect::default(),
            should_quit: false,
            status: String::new(),
            pending_history_sync_chat: None,
            synced_history_chats: HashSet::new(),
            loading_history_chats: HashSet::new(),
            monthly_backfill_ready_accounts: HashSet::new(),
            monthly_backfill_exhausted_chats: HashSet::new(),
            monthly_backfill_cursor: 0,
            monthly_backfill_tick: 0,
            chat_members: HashMap::new(),
            loading_chat_members: HashSet::new(),
            thread_unread: HashMap::new(),
            thread_unread_by_chat: HashMap::new(),
        };
        state.sync_compose_cache();
        state.sync_thread_compose_cache();
        state
    }
}

impl AppState {
    fn sync_compose_cache(&mut self) {
        self.compose_text = self.compose.lines().join("\n");
        self.compose_cursor = textarea_byte_cursor(&self.compose);
    }

    fn sync_thread_compose_cache(&mut self) {
        self.thread_compose_text = self.thread_compose.lines().join("\n");
        self.thread_compose_cursor = textarea_byte_cursor(&self.thread_compose);
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

    pub fn forward_picker_open(&self) -> bool {
        self.forward_picker.is_some()
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

    pub fn account_setup_open(&self) -> bool {
        self.account_setup.is_some()
    }

    pub fn auth_overlay_open(&self) -> bool {
        self.auth_overlay.is_some()
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

    pub fn pending_notification_count(&self) -> usize {
        self.pending_notifications.len()
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
    settings: AppSettings,
    desktop_notifier: DesktopNotifier,
    media_preview_cache: message_list::MediaPreviewCache,
    pending_media_previews: HashSet<message_list::MediaPreviewKey>,
    media_preview_tx: mpsc::UnboundedSender<MediaPreviewFetchResult>,
    media_preview_rx: mpsc::UnboundedReceiver<MediaPreviewFetchResult>,
    message_layout_cache: message_list::MessageLayoutCache,
    avatar_preview_cache: HashMap<AvatarPreviewKey, Result<AvatarPreviewData, String>>,
    pending_avatar_previews: HashSet<AvatarPreviewKey>,
    avatar_preview_tx: mpsc::UnboundedSender<AvatarPreviewFetchResult>,
    avatar_preview_rx: mpsc::UnboundedReceiver<AvatarPreviewFetchResult>,
    link_metadata_cache: message_list::LinkMetadataCache,
    link_metadata_revision: u64,
    pending_link_metadata_fetches: HashSet<Arc<str>>,
    link_metadata_tx: mpsc::UnboundedSender<LinkMetadataFetchResult>,
    link_metadata_rx: mpsc::UnboundedReceiver<LinkMetadataFetchResult>,
    history_tx: mpsc::UnboundedSender<HistoryFetchResult>,
    history_rx: mpsc::UnboundedReceiver<HistoryFetchResult>,
    chat_members_tx: mpsc::UnboundedSender<ChatMembersFetchResult>,
    chat_members_rx: mpsc::UnboundedReceiver<ChatMembersFetchResult>,
    selected_messages_tx: mpsc::UnboundedSender<SelectedMessagesFetchResult>,
    selected_messages_rx: mpsc::UnboundedReceiver<SelectedMessagesFetchResult>,
    selected_messages_generation: u64,
    pending_selected_messages: Option<PendingSelectedMessagesLoad>,
    discovery_tx: mpsc::UnboundedSender<DiscoveryFetchResult>,
    discovery_rx: mpsc::UnboundedReceiver<DiscoveryFetchResult>,
    discovery_generation: u64,
    pending_discovery_query: Option<(String, u64)>,
    image_picker: Option<Picker>,
    image_protocol_cache: HashMap<ImageProtocolKey, Result<Protocol, String>>,
    pending_image_protocols: HashSet<ImageProtocolKey>,
    image_protocol_tx: mpsc::UnboundedSender<ImageProtocolFetchResult>,
    image_protocol_rx: mpsc::UnboundedReceiver<ImageProtocolFetchResult>,
    provider_factory: Option<AccountProviderFactory>,
    theme: Theme,
    perf_log: Option<PerfLog>,
}

impl App {
    pub async fn new(store: Arc<Store>, providers: Vec<ProviderBox>) -> Result<Self> {
        Self::new_with_factory(store, providers, None).await
    }

    pub async fn new_with_factory(
        store: Arc<Store>,
        providers: Vec<ProviderBox>,
        provider_factory: Option<AccountProviderFactory>,
    ) -> Result<Self> {
        let app_started = Instant::now();
        let provider_receivers = providers
            .iter()
            .map(|provider| (provider.id().clone(), provider.events()))
            .collect();
        let (link_metadata_tx, link_metadata_rx) = mpsc::unbounded_channel();
        let (media_preview_tx, media_preview_rx) = mpsc::unbounded_channel();
        let (avatar_preview_tx, avatar_preview_rx) = mpsc::unbounded_channel();
        let (image_protocol_tx, image_protocol_rx) = mpsc::unbounded_channel();
        let (history_tx, history_rx) = mpsc::unbounded_channel();
        let (chat_members_tx, chat_members_rx) = mpsc::unbounded_channel();
        let (selected_messages_tx, selected_messages_rx) = mpsc::unbounded_channel();
        let (discovery_tx, discovery_rx) = mpsc::unbounded_channel();
        let settings_started = Instant::now();
        let settings = store.app_settings().await?;
        let notification_pause = store.notification_pause_state().await?;
        let theme = Theme::from_preset(settings.ui_theme);
        let perf_log = PerfLog::from_env()?;
        let state = AppState {
            notification_pause,
            ..AppState::default()
        };
        let mut app = Self {
            providers,
            provider_receivers,
            store,
            state,
            settings,
            desktop_notifier: DesktopNotifier::new(),
            media_preview_cache: message_list::MediaPreviewCache::default(),
            pending_media_previews: HashSet::new(),
            media_preview_tx,
            media_preview_rx,
            message_layout_cache: message_list::MessageLayoutCache::default(),
            avatar_preview_cache: HashMap::new(),
            pending_avatar_previews: HashSet::new(),
            avatar_preview_tx,
            avatar_preview_rx,
            link_metadata_cache: message_list::LinkMetadataCache::default(),
            link_metadata_revision: 0,
            pending_link_metadata_fetches: HashSet::new(),
            link_metadata_tx,
            link_metadata_rx,
            history_tx,
            history_rx,
            chat_members_tx,
            chat_members_rx,
            selected_messages_tx,
            selected_messages_rx,
            selected_messages_generation: 0,
            pending_selected_messages: None,
            discovery_tx,
            discovery_rx,
            discovery_generation: 0,
            pending_discovery_query: None,
            image_picker: None,
            image_protocol_cache: HashMap::new(),
            pending_image_protocols: HashSet::new(),
            image_protocol_tx,
            image_protocol_rx,
            provider_factory,
            theme,
            perf_log,
        };
        app.log_perf_duration("app.load_settings", settings_started, "");
        app.bootstrap().await?;
        app.log_perf_duration("app.new", app_started, "");
        Ok(app)
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    fn perf_slow_threshold(&self) -> Duration {
        self.perf_log
            .as_ref()
            .map(PerfLog::slow_threshold)
            .unwrap_or(Duration::MAX)
    }

    fn log_perf_marker(&mut self, label: &str, details: impl AsRef<str>) {
        if let Some(perf_log) = &mut self.perf_log {
            perf_log.log(label, None, details);
        }
    }

    fn log_perf_duration(&mut self, label: &str, started: Instant, details: impl AsRef<str>) {
        if let Some(perf_log) = &mut self.perf_log {
            perf_log.log(label, Some(started.elapsed()), details);
        }
    }

    fn clear_message_layout_cache(&mut self) {
        self.message_layout_cache.clear();
        self.state.media_hits.clear();
        self.state.message_hits.clear();
    }

    fn log_slow_perf_duration(&mut self, label: &str, started: Instant, details: impl AsRef<str>) {
        let elapsed = started.elapsed();
        self.log_slow_perf_elapsed(label, elapsed, details);
    }

    fn log_slow_perf_elapsed(&mut self, label: &str, elapsed: Duration, details: impl AsRef<str>) {
        if elapsed >= self.perf_slow_threshold()
            && let Some(perf_log) = &mut self.perf_log
        {
            perf_log.log(label, Some(elapsed), details);
        }
    }

    fn log_draw_step(&mut self, label: &str, elapsed: Duration, details: impl AsRef<str>) {
        self.log_slow_perf_elapsed(label, elapsed, details);
    }

    fn measure_draw_step(
        &mut self,
        label: &str,
        details: impl AsRef<str>,
        draw: impl FnOnce(&mut Self),
    ) {
        let started = Instant::now();
        draw(self);
        self.log_draw_step(label, started.elapsed(), details);
    }

    fn log_event_loop_stall(&mut self, elapsed: Duration, details: impl AsRef<str>) {
        if elapsed >= Duration::from_millis(EVENT_LOOP_STALL_LOG_MS)
            && let Some(perf_log) = &mut self.perf_log
        {
            perf_log.log("event_loop.stall", Some(elapsed), details);
        }
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
        let event_started = Instant::now();
        let event_label = app_event_label(&event);
        self.prune_ephemeral_activity();
        match event {
            AppEvent::Key(key) => {
                self.dismiss_notification();
                let selection_changed = self.handle_key(key).await?;
                if self.should_schedule_navigation_load(selection_changed) {
                    let mark_read_after_load = self.state.focus == FocusPane::Messages;
                    self.schedule_selected_messages_after_navigation_with_history(
                        true,
                        mark_read_after_load,
                    )
                    .await?;
                }
                self.flush_pending_thread_read().await?;
            }
            AppEvent::Mouse(mouse) => {
                self.dismiss_notification();
                if self.handle_mouse(mouse).await? {
                    let mark_read_after_load = self.state.focus == FocusPane::Messages;
                    self.schedule_selected_messages_after_navigation_with_history(
                        true,
                        mark_read_after_load,
                    )
                    .await?;
                }
                self.flush_pending_thread_read().await?;
            }
            AppEvent::Resize(width, height) => {
                self.dismiss_notification();
                self.clear_image_protocol_work();
                self.state.status = format!("terminal resized to {width}x{height}");
            }
            AppEvent::Tick => {
                self.handle_tick().await?;
                self.schedule_on_demand_archive_sync().await?;
            }
            AppEvent::Provider(provider_id, event) => {
                let provider_event_label = provider_event_label(&event);
                self.handle_provider_event(provider_id.clone(), *event)
                    .await?;
                self.log_slow_perf_duration(
                    "event.handle.provider",
                    event_started,
                    format!("provider={provider_id} event={provider_event_label}"),
                );
                return Ok(());
            }
            AppEvent::MediaReady(message_id, path) => {
                self.state.status = format!("media ready for {message_id}: {}", path.display());
            }
        }
        self.log_slow_perf_duration("event.handle", event_started, event_label);
        Ok(())
    }

    fn should_schedule_navigation_load(&self, selection_changed: bool) -> bool {
        selection_changed && !self.state.filter_mode
    }

    fn schedule_discovery_refresh(&mut self) {
        self.state.discovery_results.clear();
        let query = self.state.filter.trim().to_owned();
        if query.len() < 2 {
            self.pending_discovery_query = None;
            return;
        }

        self.discovery_generation = self.discovery_generation.wrapping_add(1);
        let generation = self.discovery_generation;
        self.pending_discovery_query = Some((query.clone(), generation));
        let tx = self.discovery_tx.clone();
        let providers = self
            .providers
            .iter()
            .filter(|provider| {
                self.state
                    .active_account
                    .as_ref()
                    .is_none_or(|active| provider.id() == active)
            })
            .cloned()
            .collect::<Vec<_>>();

        tokio::spawn(async move {
            let started = Instant::now();
            let mut results = Vec::new();
            let mut errors = Vec::new();
            for provider in providers {
                let provider_name = provider.account_info().display_name;
                match provider
                    .discover_destinations(&query, DISCOVERY_PROVIDER_RESULT_LIMIT)
                    .await
                {
                    Ok(mut provider_results) => results.append(&mut provider_results),
                    Err(error) => errors.push(format!("{provider_name}: {error}")),
                }
                if results.len() >= DISCOVERY_RESULT_LIMIT {
                    break;
                }
            }
            results.truncate(DISCOVERY_RESULT_LIMIT);
            let _ = tx.send(DiscoveryFetchResult {
                query,
                generation,
                results,
                errors,
                elapsed: started.elapsed(),
            });
        });
    }

    fn drain_discovery_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        let mut stale = 0;
        let mut errors = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.discovery_rx.try_recv()
        {
            drained += 1;
            self.log_slow_perf_elapsed(
                "discovery.load",
                result.elapsed,
                format!(
                    "query_len={} generation={} results={} errors={}",
                    result.query.len(),
                    result.generation,
                    result.results.len(),
                    result.errors.len()
                ),
            );

            let is_current = self
                .pending_discovery_query
                .as_ref()
                .is_some_and(|pending| pending.0 == result.query && pending.1 == result.generation)
                && self.state.filter.trim() == result.query;
            if !is_current {
                stale += 1;
                continue;
            }

            self.pending_discovery_query = None;
            self.state.discovery_results = result.results;
            errors += result.errors.len();
            if let Some(error) = result.errors.first() {
                self.state.status = format!("destination discovery failed for {error}");
            } else {
                self.state.status = self.filter_status();
            }
            changed = true;
        }
        if drained > 0 {
            self.log_slow_perf_duration(
                "discovery.drain",
                drain_started,
                format!(
                    "count={drained} stale={stale} errors={errors} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn queue_link_metadata_fetches(&mut self, requests: Vec<message_list::LinkPreviewRequest>) {
        for request in requests {
            if self.link_metadata_cache.contains_key(&request.url)
                || !self
                    .pending_link_metadata_fetches
                    .insert(request.url.clone())
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

    fn queue_media_preview_fetches(&mut self, requests: Vec<message_list::MediaPreviewRequest>) {
        for request in requests {
            if self.media_preview_cache.get(&request.key).is_some()
                || !self.pending_media_previews.insert(request.key.clone())
            {
                continue;
            }

            let tx = self.media_preview_tx.clone();
            tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let result = message_list::decode_image_preview_rows_for_key(&request.key);
                let _ = tx.send(MediaPreviewFetchResult {
                    key: request.key,
                    result,
                    elapsed: started.elapsed(),
                });
            });
        }
    }

    fn drain_link_metadata_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.link_metadata_rx.try_recv()
        {
            drained += 1;
            self.pending_link_metadata_fetches.remove(&result.url);
            self.link_metadata_cache.insert(result.url, result.metadata);
            self.link_metadata_revision = self.link_metadata_revision.wrapping_add(1);
            changed = true;
        }
        if changed {
            self.log_slow_perf_duration(
                "link_metadata.drain",
                drain_started,
                format!(
                    "count={drained} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn drain_media_preview_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        let mut errors = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.media_preview_rx.try_recv()
        {
            drained += 1;
            self.pending_media_previews.remove(&result.key);
            if result.result.is_err() {
                errors += 1;
            }
            self.log_slow_perf_elapsed(
                "media_preview.decode",
                result.elapsed,
                format!(
                    "path={} width={} rows={} result={}",
                    result.key.path.display(),
                    result.key.width,
                    result.key.rows,
                    if result.result.is_ok() { "ok" } else { "err" }
                ),
            );
            self.media_preview_cache.insert(result.key, result.result);
            changed = true;
        }
        if changed {
            self.log_slow_perf_duration(
                "media_preview.drain",
                drain_started,
                format!(
                    "count={drained} errors={errors} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn drain_image_protocol_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        let mut errors = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.image_protocol_rx.try_recv()
        {
            drained += 1;
            self.pending_image_protocols.remove(&result.key);
            if result.result.is_err() {
                errors += 1;
            }
            self.log_slow_perf_elapsed(
                "image_protocol.decode",
                result.elapsed,
                format!(
                    "path={} width={} height={} result={}",
                    result.key.path.display(),
                    result.key.width,
                    result.key.height,
                    if result.result.is_ok() { "ok" } else { "err" }
                ),
            );
            self.image_protocol_cache.insert(result.key, result.result);
            changed = true;
        }
        if changed {
            self.log_slow_perf_duration(
                "image_protocol.drain",
                drain_started,
                format!(
                    "count={drained} errors={errors} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn drain_avatar_preview_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        let mut errors = 0;
        let mut sqlite_hits = 0;
        let mut sqlite_misses = 0;
        let mut stale = 0;
        let mut generated = 0;
        let mut persisted = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.avatar_preview_rx.try_recv()
        {
            drained += 1;
            self.pending_avatar_previews.remove(&result.key);
            if result.sqlite_hit {
                sqlite_hits += 1;
            }
            if result.cache_miss {
                sqlite_misses += 1;
                self.queue_avatar_preview_decode(result.key.clone());
            }
            if result.stale {
                stale += 1;
            }
            if result.generated {
                generated += 1;
            }
            if result.persisted {
                persisted += 1;
            }
            if result.refresh_on_stale {
                self.queue_avatar_preview_decode(result.key.clone());
            }
            if result.result.as_ref().is_some_and(Result::is_err) {
                errors += 1;
            }
            self.log_slow_perf_elapsed(
                "avatar_preview.complete",
                result.elapsed,
                format!(
                    "path={} result={} sqlite_hit={} sqlite_miss={} stale={} generated={} persisted={}",
                    result.key.path.display(),
                    match &result.result {
                        Some(Ok(_)) => "ok",
                        Some(Err(_)) => "err",
                        None => "none",
                    },
                    result.sqlite_hit,
                    result.cache_miss,
                    result.stale,
                    result.generated,
                    result.persisted,
                ),
            );
            if let Some(data) = result.result {
                self.avatar_preview_cache.insert(result.key, data);
                changed = true;
            }
        }
        if changed || drained > 0 {
            self.log_slow_perf_duration(
                "avatar_preview.drain",
                drain_started,
                format!(
                    "count={drained} errors={errors} sqlite_hits={sqlite_hits} sqlite_misses={sqlite_misses} stale={stale} generated={generated} persisted={persisted} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn schedule_selected_messages_after_navigation(
        &mut self,
        scroll_to_bottom: bool,
        mark_read_after_load: bool,
    ) {
        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.messages.clear();
            self.state.filtered_messages.clear();
            self.clear_message_layout_cache();
            self.pending_selected_messages = None;
            return;
        };

        let key = SelectedMessagesKey {
            account: chat.account.clone(),
            chat_id: chat.id.clone(),
        };
        self.selected_messages_generation = self.selected_messages_generation.wrapping_add(1);
        let generation = self.selected_messages_generation;
        self.pending_selected_messages = Some(PendingSelectedMessagesLoad {
            key: key.clone(),
            generation,
        });
        self.state.messages.clear();
        self.clear_message_layout_cache();
        let previous_status = self.state.status.clone();
        self.state.status = if previous_status.is_empty() {
            format!("loading messages for {}", chat.name)
        } else {
            format!("{previous_status}; loading messages for {}", chat.name)
        };

        let tx = self.selected_messages_tx.clone();
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let started = Instant::now();
            let result = store
                .get_messages_for_chat(
                    &key.account,
                    &key.chat_id,
                    None,
                    SELECTED_CHAT_MESSAGE_LIMIT,
                )
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(SelectedMessagesFetchResult {
                key,
                generation,
                scroll_to_bottom,
                mark_read_after_load,
                result,
                elapsed: started.elapsed(),
            });
        });
    }

    async fn schedule_selected_messages_after_navigation_with_history(
        &mut self,
        scroll_to_bottom: bool,
        mark_read_after_load: bool,
    ) -> Result<()> {
        let should_sync_history = self.consume_pending_history_sync_for_selected_chat();
        self.request_selected_chat_members();
        self.schedule_selected_messages_after_navigation(scroll_to_bottom, mark_read_after_load);
        if should_sync_history {
            self.sync_selected_chat_history().await?;
        }
        Ok(())
    }

    async fn drain_selected_messages_fetches(&mut self) -> Result<bool> {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        let mut stale = 0;
        let mut errors = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.selected_messages_rx.try_recv()
        {
            drained += 1;
            self.log_slow_perf_elapsed(
                "selected_messages.load",
                result.elapsed,
                format!(
                    "account={} chat={} generation={} result={}",
                    result.key.account,
                    result.key.chat_id,
                    result.generation,
                    if result.result.is_ok() { "ok" } else { "err" }
                ),
            );

            let is_current_pending =
                self.pending_selected_messages
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.key == result.key && pending.generation == result.generation
                    });
            let is_selected_chat = self.state.selected_chat().is_some_and(|chat| {
                chat.account == result.key.account && chat.id == result.key.chat_id
            });
            if !is_current_pending || !is_selected_chat {
                stale += 1;
                continue;
            }

            self.pending_selected_messages = None;
            match result.result {
                Ok(messages) => {
                    let message_count = messages.len();
                    self.state.messages = messages;
                    self.apply_message_filter();
                    self.clear_message_layout_cache();
                    self.apply_cached_member_names_to_selected_messages();
                    self.refresh_thread_unread_for_selected_chat().await?;
                    self.try_open_pending_thread();
                    if result.scroll_to_bottom {
                        self.scroll_messages_to_bottom();
                    } else {
                        self.clamp_message_scroll();
                    }
                    self.schedule_older_history_prefetch_if_needed();
                    if result.mark_read_after_load {
                        self.mark_selected_chat_read().await?;
                    }
                    self.state.status = format!("loaded {message_count} messages");
                    changed = true;
                }
                Err(error) => {
                    errors += 1;
                    self.state.status = format!("message load failed: {error}");
                    changed = true;
                }
            }
        }
        if drained > 0 {
            self.log_slow_perf_duration(
                "selected_messages.drain",
                drain_started,
                format!(
                    "count={drained} stale={stale} errors={errors} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        Ok(changed)
    }

    async fn drain_history_fetches(&mut self) -> Result<bool> {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut changed_count = 0;
        while changed_count < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (changed_count == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.history_rx.try_recv()
        {
            changed_count += 1;
            let chat_key = (result.account.clone(), result.chat_id.clone());
            let fetch_key = (
                result.account.clone(),
                result.chat_id.clone(),
                result.before,
            );
            self.state.loading_history_chats.remove(&fetch_key);
            if result.before.is_none() {
                let mark_recent_sync_complete = result
                    .result
                    .as_ref()
                    .map(|messages| {
                        result.platform != Platform::WhatsApp
                            || messages.len() >= WHATSAPP_INITIAL_HISTORY_TARGET
                    })
                    .unwrap_or(false);
                if mark_recent_sync_complete {
                    self.state.synced_history_chats.insert(chat_key.clone());
                }
            }

            let is_selected_chat = self
                .state
                .selected_chat()
                .is_some_and(|chat| chat.account == result.account && chat.id == result.chat_id);

            match result.result {
                Ok(messages) => {
                    self.refresh_chat_preview_from_messages(
                        &result.account,
                        &result.chat_id,
                        &messages,
                    )
                    .await?;
                    let reached_archive_floor = messages
                        .first()
                        .is_some_and(|message| message.timestamp <= archive_sync_floor());
                    let history_empty = messages.is_empty();
                    let empty_result_means_exhausted = result.platform != Platform::WhatsApp;
                    self.store.upsert_messages(&messages).await?;
                    if result.before.is_some()
                        && ((history_empty && empty_result_means_exhausted)
                            || reached_archive_floor)
                    {
                        self.state
                            .monthly_backfill_exhausted_chats
                            .insert(chat_key.clone());
                    }

                    if is_selected_chat {
                        if result.before.is_some() {
                            self.merge_older_messages_into_current_chat(
                                messages,
                                &result.chat_name,
                                result.show_status,
                                result.platform,
                            );
                        } else {
                            self.reload_chats().await?;
                            self.pending_selected_messages = None;
                            self.selected_messages_generation =
                                self.selected_messages_generation.wrapping_add(1);
                            self.state.messages = messages;
                            self.apply_message_filter();
                            self.clear_message_layout_cache();
                            self.apply_cached_member_names_to_selected_messages();
                            self.scroll_messages_to_bottom();
                            self.schedule_older_history_prefetch_if_needed();
                            if self.state.focus == FocusPane::Messages {
                                self.mark_selected_chat_read().await?;
                            }
                            self.state.status =
                                format!("synced recent messages for {}", result.chat_name);
                        }
                    }
                }
                Err(error) => {
                    self.state
                        .monthly_backfill_exhausted_chats
                        .insert(chat_key.clone());
                    if result.before.is_some() && is_selected_chat {
                        self.state.is_loading_older_history = false;
                    }
                    if is_selected_chat && result.show_status {
                        self.state.status =
                            format!("message sync failed for {}: {error}", result.chat_name);
                    }
                }
            }
            changed = true;
        }
        if changed {
            self.log_slow_perf_duration(
                "history_fetch.drain",
                drain_started,
                format!(
                    "count={changed_count} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        Ok(changed)
    }

    fn drain_chat_member_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut changed = false;
        let mut drained = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.chat_members_rx.try_recv()
        {
            drained += 1;
            let key = (result.account.clone(), result.chat_id.clone());
            self.state.loading_chat_members.remove(&key);
            match result.result {
                Ok(members) => {
                    self.apply_member_names_to_selected_messages(
                        &result.account,
                        &result.chat_id,
                        &members,
                    );
                    self.state.chat_members.insert(key, members);
                }
                Err(error) => {
                    if self.state.selected_chat().is_some_and(|chat| {
                        chat.account == result.account && chat.id == result.chat_id
                    }) {
                        self.state.status = format!("member list unavailable: {error}");
                    }
                }
            }
            changed = true;
        }
        if changed {
            self.log_slow_perf_duration(
                "chat_members.drain",
                drain_started,
                format!(
                    "count={drained} budget_exhausted={}",
                    drain_started.elapsed() >= COMPLETION_DRAIN_BUDGET
                ),
            );
        }
        changed
    }

    fn apply_member_names_to_selected_messages(
        &mut self,
        account: &ProviderId,
        chat_id: &ChatId,
        members: &[Sender],
    ) {
        let Some(chat) = self.state.selected_chat() else {
            return;
        };
        if &chat.account != account || &chat.id != chat_id {
            return;
        }

        let member_names = members
            .iter()
            .map(|member| (member.platform_id.clone(), member.clone()))
            .collect::<HashMap<_, _>>();
        for message in &mut self.state.messages {
            if message.sender.display_name == message.sender.platform_id
                && let Some(sender) = member_names.get(&message.sender.platform_id)
            {
                message.sender = sender.clone();
            }
        }
        self.apply_message_filter();
    }

    fn apply_cached_member_names_to_selected_messages(&mut self) {
        let Some(chat) = self.state.selected_chat() else {
            return;
        };
        let key = (chat.account.clone(), chat.id.clone());
        let members = self.state.chat_members.get(&key).cloned();
        if let Some(members) = members {
            self.apply_member_names_to_selected_messages(&key.0, &key.1, &members);
        } else {
            self.apply_message_filter();
        }
    }

    pub async fn drain_provider_events(&mut self) -> Result<bool> {
        let drain_started = Instant::now();
        let mut events = Vec::new();
        'receivers: for (provider_id, receiver) in &mut self.provider_receivers {
            loop {
                if events.len() >= MAX_PROVIDER_EVENTS_PER_DRAIN {
                    break 'receivers;
                }
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

        let event_type_counts = provider_event_type_counts(&events);
        let provider_event_count = events.len();
        let provider_draw_event_count = events
            .iter()
            .filter(|event| provider_event_requests_draw(event, self.settings.network_activity))
            .count();
        for event in events {
            self.handle_event(event).await?;
        }
        let history_changed = self.drain_history_fetches().await?;
        let selected_messages_changed = self.drain_selected_messages_fetches().await?;
        // A thread queued from the inbox opens once its root is present in the
        // current chat. The root can arrive via the storage fetch above or via
        // historical catch-up appends, so retry here to cover every path.
        self.try_open_pending_thread();
        let discovery_changed = self.drain_discovery_fetches();
        let media_preview_changed = self.drain_media_preview_fetches();
        let image_protocol_changed = self.drain_image_protocol_fetches();
        let avatar_preview_changed = self.drain_avatar_preview_fetches();
        let link_metadata_changed = self.drain_link_metadata_fetches();
        let chat_members_changed = self.drain_chat_member_fetches();
        let changed = provider_draw_event_count > 0
            || selected_messages_changed
            || discovery_changed
            || media_preview_changed
            || image_protocol_changed
            || avatar_preview_changed
            || link_metadata_changed
            || chat_members_changed
            || history_changed;
        if changed {
            self.log_slow_perf_duration(
                "event_drain.batch",
                drain_started,
                format!(
                    "provider_events={provider_event_count} provider_draw_events={provider_draw_event_count} event_types={} history_changed={history_changed} selected_messages_changed={selected_messages_changed} discovery_changed={discovery_changed} media_preview_changed={media_preview_changed} image_protocol_changed={image_protocol_changed} avatar_preview_changed={avatar_preview_changed} link_metadata_changed={link_metadata_changed} chat_members_changed={chat_members_changed}",
                    format_event_type_counts(&event_type_counts)
                ),
            );
        }
        Ok(changed)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let draw_started = Instant::now();
        let layout_started = Instant::now();
        self.state.frame_area = frame.area();
        let compose_height = self.compose_height(frame.area());
        let layout = AppLayout::for_area(
            frame.area(),
            compose_height,
            self.state.thread_root.is_some(),
        );
        self.state.layout_mode = layout.mode;
        self.state.pane_areas = self.visible_pane_areas(layout);
        self.clamp_message_scroll();
        self.clamp_details_scroll();
        self.apply_pending_scroll_to_latest();
        self.log_draw_step(
            "draw.layout",
            layout_started.elapsed(),
            format!(
                "mode={} area={}x{} compose_height={compose_height} chats={} messages={}",
                layout.mode.label(),
                frame.area().width,
                frame.area().height,
                self.state.chats.len(),
                self.state.messages.len()
            ),
        );

        match layout.mode {
            LayoutMode::Compact => self.draw_compact(frame, layout),
            LayoutMode::Medium => {
                self.measure_draw_step(
                    "draw.chat_list",
                    draw_chat_list_details(self, layout.chat_list),
                    |app| {
                        app.draw_chat_list(frame, layout.chat_list);
                    },
                );
                self.measure_draw_step(
                    "draw.messages",
                    draw_messages_details(self, layout.messages),
                    |app| {
                        app.draw_messages(frame, layout.messages);
                    },
                );
                self.measure_draw_step(
                    "draw.compose",
                    draw_compose_details(self, layout.compose),
                    |app| {
                        app.draw_compose(frame, layout.compose);
                    },
                );
            }
            LayoutMode::Wide => {
                self.measure_draw_step(
                    "draw.chat_list",
                    draw_chat_list_details(self, layout.chat_list),
                    |app| {
                        app.draw_chat_list(frame, layout.chat_list);
                    },
                );
                self.measure_draw_step(
                    "draw.messages",
                    draw_messages_details(self, layout.messages),
                    |app| {
                        app.draw_messages(frame, layout.messages);
                    },
                );
                self.measure_draw_step(
                    "draw.compose",
                    draw_compose_details(self, layout.compose),
                    |app| {
                        app.draw_compose(frame, layout.compose);
                    },
                );
                self.measure_draw_step(
                    "draw.details",
                    draw_details_details(self, layout.details),
                    |app| {
                        app.draw_details(frame, layout.details);
                    },
                );
            }
        }
        self.measure_draw_step("draw.status_bar", "", |app| {
            app.draw_status_bar(frame, layout.status)
        });
        self.measure_draw_step("draw.overlays", overlay_draw_details(self), |app| {
            let area = frame.area();
            app.draw_notification_overlay(frame, area);
            app.draw_account_switcher(frame, area);
            app.draw_threads_inbox(frame, area);
            app.draw_settings_overlay(frame, area);
            app.draw_image_viewer(frame, area);
            app.draw_auth_overlay(frame, area);
            app.draw_account_setup_overlay(frame, area);
            app.draw_slack_setup_overlay(frame, area);
            app.draw_help_overlay(frame, area);
            app.draw_action_menu(frame, area);
            app.draw_forward_picker(frame, area);
            app.draw_reaction_picker(frame, area);
            app.draw_compose_attach_menu(frame, area);
            app.draw_compose_emoticon_picker(frame, area);
            app.draw_poll_vote_picker(frame, area);
        });
        self.log_slow_perf_duration(
            "draw.total",
            draw_started,
            format!(
                "mode={} area={}x{} chats={} visible_chats={} messages={} overlays={}",
                layout.mode.label(),
                frame.area().width,
                frame.area().height,
                self.state.chats.len(),
                self.state.visible_chat_indices.len(),
                self.state.messages.len(),
                overlay_draw_details(self)
            ),
        );
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
            FocusPane::ChatList => {
                self.measure_draw_step(
                    "draw.chat_list",
                    draw_chat_list_details(self, layout.chat_list),
                    |app| {
                        app.draw_chat_list(frame, layout.chat_list);
                    },
                );
            }
            FocusPane::Messages | FocusPane::Compose => {
                self.measure_draw_step(
                    "draw.messages",
                    draw_messages_details(self, layout.messages),
                    |app| {
                        app.draw_messages(frame, layout.messages);
                    },
                );
                self.measure_draw_step(
                    "draw.compose",
                    draw_compose_details(self, layout.compose),
                    |app| {
                        app.draw_compose(frame, layout.compose);
                    },
                );
            }
            FocusPane::Details => {
                self.measure_draw_step(
                    "draw.details",
                    draw_details_details(self, layout.chat_list),
                    |app| {
                        app.draw_details(frame, layout.chat_list);
                    },
                );
            }
        }
    }

    fn draw_chat_list(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let chat_layout = chat_list::build_layout(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            area,
            self.settings.chat_inbox_style,
        );
        let rendered_chat_indices = chat_layout.rendered_chat_indices();
        let avatars_started = Instant::now();
        let avatar_rows = self.chat_avatar_rows(&rendered_chat_indices);
        let account_badge_rows = self.account_badge_rows(&rendered_chat_indices);
        self.log_draw_step(
            "draw.chat_list.avatars",
            avatars_started.elapsed(),
            format!(
                "rendered={} cached={} pending={} queued={} errors={} account_badges={} account_cached={} account_pending={} account_queued={} account_errors={}",
                avatar_rows.rows.len(),
                avatar_rows.cached,
                avatar_rows.pending,
                avatar_rows.queued,
                avatar_rows.errors,
                account_badge_rows.rows.len(),
                account_badge_rows.cached,
                account_badge_rows.pending,
                account_badge_rows.queued,
                account_badge_rows.errors
            ),
        );
        let typing_started = Instant::now();
        let typing_previews = self.chat_typing_previews();
        self.log_draw_step(
            "draw.chat_list.typing",
            typing_started.elapsed(),
            format!("active={}", typing_previews.len()),
        );
        let render_started = Instant::now();
        chat_list::render_chat_list(
            frame,
            area,
            chat_list::ChatListProps {
                chats: &self.state.chats,
                visible_chat_indices: &self.state.visible_chat_indices,
                selected_chat_index: self.state.selected_chat,
                filter: self.chat_list_filter(),
                filter_mode: self.state.filter_mode
                    && self.state.filter_scope == FilterScope::Chats,
                discovery_results: &self.state.discovery_results,
                account_filter: &self.account_filter_label(),
                inbox_style: self.settings.chat_inbox_style,
                focused: self.state.focus == FocusPane::ChatList,
                layout: &chat_layout,
                avatar_rows: &avatar_rows.rows,
                account_badge_rows: &account_badge_rows.rows,
                typing_previews: &typing_previews,
                thread_unread_by_chat: &self.state.thread_unread_by_chat,
                theme: self.theme,
            },
        );
        self.render_chat_list_hd_avatars(frame, area, &chat_layout);
        self.log_draw_step(
            "draw.chat_list.render",
            render_started.elapsed(),
            draw_chat_list_details(self, area),
        );
        let scrollbar_started = Instant::now();
        self.draw_vertical_scrollbar(
            frame,
            area,
            chat_layout.content_height,
            chat_layout.scroll_position,
        );
        self.log_draw_step(
            "draw.chat_list.scrollbar",
            scrollbar_started.elapsed(),
            draw_chat_list_details(self, area),
        );
    }

    fn chat_avatar_rows(&mut self, rendered_chat_indices: &[usize]) -> ChatAvatarRowsResult {
        let mut result = ChatAvatarRowsResult::default();
        self.queue_chat_avatar_lookahead(rendered_chat_indices);
        for chat_index in rendered_chat_indices.iter().copied() {
            let Some(chat) = self.state.chats.get(chat_index).cloned() else {
                continue;
            };
            let Some(path) = self.chat_avatar_path(&chat) else {
                continue;
            };

            let key = AvatarPreviewKey {
                path,
                width: chat_list::CHAT_AVATAR_WIDTH,
                rows: chat_list::CHAT_AVATAR_ROWS,
                source: AvatarPreviewSource::Avatar,
            };

            match self.avatar_preview_cache.get(&key) {
                Some(Ok(avatar)) => {
                    result.cached += 1;
                    result.rows.insert(chat_index, avatar.rows.clone());
                }
                Some(Err(_)) => {
                    result.errors += 1;
                }
                None => {
                    if self.pending_avatar_previews.contains(&key) {
                        result.pending += 1;
                    } else {
                        self.queue_avatar_preview(key);
                        result.queued += 1;
                    }
                }
            }
        }
        result
    }

    fn chat_avatar_path(&mut self, chat: &Chat) -> Option<PathBuf> {
        if let Some(path) = chat.avatar.clone() {
            return Some(path);
        }
        if !is_whatsapp_status_chat(chat) {
            return None;
        }
        let account = self.account_for_provider(&chat.account)?;
        if let Some(path) = account.avatar {
            return Some(path);
        }

        let path = static_account_icon_path(&chat.account, Platform::WhatsApp);
        self.queue_static_account_icon_preview(
            AvatarPreviewKey {
                path: path.clone(),
                width: chat_list::CHAT_AVATAR_WIDTH,
                rows: chat_list::CHAT_AVATAR_ROWS,
                source: AvatarPreviewSource::Avatar,
            },
            EMBEDDED_WHATSAPP_ICON_PNG,
        );
        Some(path)
    }

    fn queue_chat_avatar_lookahead(&mut self, rendered_chat_indices: &[usize]) {
        if rendered_chat_indices.is_empty() || self.state.visible_chat_indices.is_empty() {
            return;
        }

        let first_rendered = rendered_chat_indices.iter().min().copied().unwrap_or(0);
        let last_rendered = rendered_chat_indices
            .iter()
            .max()
            .copied()
            .unwrap_or(first_rendered);
        let visible_positions = self
            .state
            .visible_chat_indices
            .iter()
            .enumerate()
            .filter_map(|(position, chat_index)| {
                ((*chat_index >= first_rendered) && (*chat_index <= last_rendered))
                    .then_some(position)
            })
            .collect::<Vec<_>>();
        let first_position = visible_positions.iter().min().copied().unwrap_or(0);
        let last_position = visible_positions
            .iter()
            .max()
            .copied()
            .unwrap_or(first_position);
        let window = rendered_chat_indices.len().max(8);
        let start = first_position.saturating_sub(window);
        let end = (last_position + window + 1).min(self.state.visible_chat_indices.len());

        let chats = self.state.visible_chat_indices[start..end]
            .iter()
            .filter_map(|chat_index| self.state.chats.get(*chat_index).cloned())
            .collect::<Vec<_>>();
        let keys = chats
            .iter()
            .filter_map(|chat| {
                self.chat_avatar_path(chat).map(|path| AvatarPreviewKey {
                    path,
                    width: chat_list::CHAT_AVATAR_WIDTH,
                    rows: chat_list::CHAT_AVATAR_ROWS,
                    source: AvatarPreviewSource::Avatar,
                })
            })
            .collect::<Vec<_>>();
        self.queue_avatar_preview_loads(keys);
    }

    fn account_badge_rows(&mut self, rendered_chat_indices: &[usize]) -> AccountBadgeRowsResult {
        let mut result = AccountBadgeRowsResult::default();
        let mut seen_accounts = HashSet::new();
        let account_ids = rendered_chat_indices
            .iter()
            .filter_map(|chat_index| self.state.chats.get(*chat_index))
            .map(|chat| chat.account.clone())
            .filter(|provider_id| seen_accounts.insert(provider_id.clone()))
            .collect::<Vec<_>>();
        for provider_id in account_ids {
            let Some(account) = self.account_for_provider(&provider_id) else {
                continue;
            };
            let fallback = chat_list::account_badge_placeholder(&account, self.theme);
            result.rows.insert(provider_id.clone(), fallback);

            let Some(path) = self.account_badge_avatar_path(&provider_id, &account) else {
                continue;
            };

            let key = AvatarPreviewKey {
                path,
                width: chat_list::ACCOUNT_BADGE_WIDTH,
                rows: chat_list::ACCOUNT_BADGE_ROWS,
                source: AvatarPreviewSource::AccountBadge,
            };

            match self.avatar_preview_cache.get(&key) {
                Some(Ok(avatar)) => {
                    result.cached += 1;
                    result.rows.insert(provider_id, avatar.rows.clone());
                }
                Some(Err(_)) => {
                    if key.path.exists() {
                        self.avatar_preview_cache.remove(&key);
                        if self.pending_avatar_previews.contains(&key) {
                            result.pending += 1;
                        } else {
                            self.queue_avatar_preview(key);
                            result.queued += 1;
                        }
                    } else {
                        result.errors += 1;
                    }
                }
                None => {
                    if self.pending_avatar_previews.contains(&key) {
                        result.pending += 1;
                    } else {
                        self.queue_avatar_preview(key);
                        result.queued += 1;
                    }
                }
            }
        }
        result
    }

    fn account_badge_avatar_path(
        &mut self,
        provider_id: &ProviderId,
        account: &Account,
    ) -> Option<PathBuf> {
        let status_avatar = self
            .state
            .account_statuses
            .get(provider_id)
            .and_then(|status| status.avatar.as_deref())
            .filter(|_| account.platform != Platform::WhatsApp)
            .map(Path::to_path_buf);
        let account_avatar = account.avatar.as_deref().map(Path::to_path_buf);
        let preferred_avatar = status_avatar.or(account_avatar);
        if preferred_avatar.is_some() {
            return preferred_avatar;
        }

        let icon_bytes = match account.platform {
            Platform::WhatsApp => Some(EMBEDDED_WHATSAPP_ICON_PNG),
            Platform::Slack | Platform::Unknown(_) | Platform::Discord => None,
        }?;
        let path = static_account_icon_path(provider_id, account.platform.clone());
        self.queue_static_account_icon_preview(
            AvatarPreviewKey {
                path: path.clone(),
                width: chat_list::ACCOUNT_BADGE_WIDTH,
                rows: chat_list::ACCOUNT_BADGE_ROWS,
                source: AvatarPreviewSource::AccountBadge,
            },
            icon_bytes,
        );
        Some(path)
    }

    fn queue_static_account_icon_preview(
        &mut self,
        key: AvatarPreviewKey,
        icon_bytes: &'static [u8],
    ) {
        if self.avatar_preview_cache.contains_key(&key)
            || !self.pending_avatar_previews.insert(key.clone())
        {
            return;
        }
        let tx = self.avatar_preview_tx.clone();
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let started = Instant::now();
            let result = tokio::task::spawn_blocking({
                let key = key.clone();
                move || {
                    write_static_account_icon(&key.path, icon_bytes)
                        .map_err(|error| error.to_string())
                        .and_then(|()| decode_avatar_preview_with_thumbnail(&key))
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            let mut persisted = false;
            let result = match (result, &store) {
                (Ok((rows, upsert)), store) => {
                    persisted = store.upsert_avatar_thumbnail_cache(&upsert).await.is_ok();
                    Ok(rows)
                }
                (Err(error), _) => Err(error),
            };
            let _ = tx.send(AvatarPreviewFetchResult::generated(
                key,
                result,
                started.elapsed(),
                persisted,
            ));
        });
    }

    fn queue_avatar_preview(&mut self, key: AvatarPreviewKey) {
        self.queue_avatar_preview_loads(vec![key]);
    }

    fn queue_avatar_preview_loads(&mut self, keys: Vec<AvatarPreviewKey>) {
        let mut seen = HashSet::new();
        let keys = keys
            .into_iter()
            .filter(|key| seen.insert(key.clone()))
            .filter(|key| {
                !self.avatar_preview_cache.contains_key(key)
                    && self.pending_avatar_previews.insert(key.clone())
            })
            .collect::<Vec<_>>();
        if keys.is_empty() {
            return;
        }
        let tx = self.avatar_preview_tx.clone();
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let started = Instant::now();
            let cache_keys = keys
                .iter()
                .map(avatar_thumbnail_cache_key)
                .collect::<Vec<_>>();
            let records = store
                .avatar_thumbnail_cache_entries(&cache_keys)
                .await
                .unwrap_or_default();
            let mut records_by_key = records
                .into_iter()
                .map(|record| (record.cache_key.clone(), record))
                .collect::<HashMap<_, _>>();
            for key in keys {
                let cache_key = avatar_thumbnail_cache_key(&key);
                let Some(record) = records_by_key.remove(&cache_key) else {
                    let _ = tx.send(AvatarPreviewFetchResult::sqlite_miss(
                        key,
                        started.elapsed(),
                    ));
                    continue;
                };
                let stale = avatar_thumbnail_record_is_stale(&key, &record);
                let rows = avatar_thumbnail_record_to_rows(&key, record);
                let _ = tx.send(AvatarPreviewFetchResult::sqlite_hit(
                    key,
                    rows,
                    started.elapsed(),
                    stale,
                ));
            }
        });
    }

    fn queue_avatar_preview_decode(&mut self, key: AvatarPreviewKey) {
        if !self.pending_avatar_previews.insert(key.clone()) {
            return;
        }
        let tx = self.avatar_preview_tx.clone();
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let started = Instant::now();
            let result = tokio::task::spawn_blocking({
                let key = key.clone();
                move || decode_avatar_preview_with_thumbnail(&key)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            let mut persisted = false;
            let result = match result {
                Ok((rows, upsert)) => {
                    persisted = store.upsert_avatar_thumbnail_cache(&upsert).await.is_ok();
                    Ok(rows)
                }
                Err(error) => Err(error),
            };
            let _ = tx.send(AvatarPreviewFetchResult::generated(
                key,
                result,
                started.elapsed(),
                persisted,
            ));
        });
    }

    fn chat_typing_previews(&self) -> HashMap<usize, String> {
        let now = Utc::now();
        self.state
            .chats
            .iter()
            .enumerate()
            .filter_map(|(index, chat)| {
                let key = (chat.account.clone(), chat.id.clone());
                let active = self.state.typing_indicators.get(&key)?;
                let names = active
                    .iter()
                    .filter(|indicator| indicator.expires_at > now)
                    .map(|indicator| indicator.display_name.as_str())
                    .collect::<Vec<_>>();
                typing_preview_text(&names).map(|preview| (index, preview))
            })
            .collect()
    }

    fn active_conversation_presentation_setting(&self) -> ConversationPresentationSetting {
        let Some(chat) = self.state.selected_chat() else {
            return self.settings.conversation_presentation;
        };
        match self.settings.conversation_presentation {
            ConversationPresentationSetting::ProviderNative => match chat.platform {
                Platform::Slack => ConversationPresentationSetting::Slack,
                _ => ConversationPresentationSetting::WhatsApp,
            },
            style => style,
        }
    }

    fn active_message_presentation(&self) -> message_list::ConversationPresentation {
        match self.active_conversation_presentation_setting() {
            ConversationPresentationSetting::Slack => message_list::ConversationPresentation::Flat,
            ConversationPresentationSetting::ProviderNative
            | ConversationPresentationSetting::WhatsApp => {
                message_list::ConversationPresentation::Bubbles
            }
        }
    }

    fn draw_messages(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if area.is_empty() {
            return;
        }

        let title = self
            .state
            .selected_chat()
            .map(|chat| {
                let messages_filter_scope =
                    self.state.filter_mode && self.state.filter_scope == FilterScope::Messages;
                if messages_filter_scope || self.message_filter_active() {
                    let query = if self.state.filter.is_empty() {
                        "(type to filter)".to_owned()
                    } else {
                        self.state.filter.clone()
                    };
                    format!("Messages - {} - filter: {}", chat.name, query)
                } else {
                    format!("Messages - {}", chat.name)
                }
            })
            .unwrap_or_else(|| "Messages".to_owned());
        let presentation = self.active_message_presentation();
        let message_filter_active = self.message_filter_active();
        let messages_empty = if message_filter_active {
            self.state.filtered_messages.is_empty()
        } else {
            self.state.messages.is_empty()
        };
        let line_count_started = Instant::now();
        let total_lines = if messages_empty {
            0
        } else {
            self.cached_message_line_count()
        };
        self.log_draw_step(
            "draw.messages.line_count",
            line_count_started.elapsed(),
            draw_messages_details(self, area),
        );
        let build_started = Instant::now();
        let messages = if message_filter_active {
            &self.state.filtered_messages
        } else {
            &self.state.messages
        };
        let lines = if messages.is_empty() {
            self.state.media_hits.clear();
            self.state.message_hits.clear();
            let empty_message = if message_filter_active && !self.state.messages.is_empty() {
                format!("No messages match {}.", self.state.filter)
            } else {
                "No messages yet. Open or click this chat to sync today's messages.".to_owned()
            };
            vec![Line::from(Span::styled(empty_message, self.theme.muted()))]
        } else {
            let unread_message_ids = self.unread_message_ids();
            let thread_unread = HashMap::new();
            let render = message_list::build_message_lines_with_cache(
                messages,
                area.width.saturating_sub(2),
                self.state.message_scroll,
                area.height.saturating_sub(2) as usize,
                self.state.selected_message_id.as_deref(),
                &unread_message_ids,
                &thread_unread,
                &mut self.media_preview_cache,
                &self.link_metadata_cache,
                self.link_metadata_revision,
                &mut self.message_layout_cache,
                self.theme,
                presentation,
            );
            self.state.media_hits = render.media_hits;
            self.state.message_hits = render.message_hits;
            self.queue_link_metadata_fetches(render.link_preview_requests);
            self.queue_media_preview_fetches(render.media_preview_requests);
            render.lines
        };
        self.log_draw_step(
            "draw.messages.build_lines",
            build_started.elapsed(),
            format!(
                "{} total_lines={total_lines} rendered_lines={}",
                draw_messages_details(self, area),
                lines.len()
            ),
        );
        let viewport_rows = area.height.saturating_sub(2) as usize;
        self.state.message_top_padding = message_top_padding(
            lines.len(),
            total_lines,
            self.state.message_scroll,
            viewport_rows,
        );
        let render_started = Instant::now();
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
        self.render_inline_hd_image_previews(frame, area);
        self.render_message_hd_avatars(frame, area);
        self.log_draw_step(
            "draw.messages.render",
            render_started.elapsed(),
            format!(
                "{} total_lines={total_lines}",
                draw_messages_details(self, area)
            ),
        );
    }

    fn thread_compose_height(&self, area: Rect) -> u16 {
        if area.height < 8 {
            return area.height.min(3);
        }

        let content_lines = self.state.thread_compose.lines().len() as u16;
        let wrapped_extra = self
            .state
            .thread_compose
            .lines()
            .iter()
            .map(|line| {
                let width = area.width.saturating_sub(4).max(24) as usize;
                line.chars().count().saturating_div(width)
            })
            .sum::<usize>() as u16;
        (content_lines + wrapped_extra + 2).clamp(3, 8)
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
        let mut compose =
            self.compose_textarea_for_render(&self.state.compose, is_focused, "Type a message...");
        compose.remove_block();
        frame.render_widget(&compose, editor_area);
    }

    fn compose_textarea_for_render(
        &self,
        source: &TextArea<'static>,
        is_focused: bool,
        placeholder: &'static str,
    ) -> TextArea<'static> {
        let mut compose = source.clone();
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
        compose.set_placeholder_text(placeholder);
        compose.set_placeholder_style(self.theme.muted());
        compose
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

        frame.render_widget(Clear, area);

        if self.thread_filter_active() && !self.state.filter.is_empty() {
            self.draw_filtered_thread_details(frame, area);
            return;
        }

        if let Some(thread_root) = self.state.thread_root.clone() {
            self.draw_thread_details(frame, area, &thread_root);
            return;
        }

        if let Some(message_id) = &self.state.selected_message_id
            && let Some(message) = self.message_by_id(message_id).cloned()
        {
            self.draw_message_details(frame, area, &message);
            return;
        }

        let details = self.overview_detail_lines();
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

    fn overview_detail_lines(&self) -> Vec<Line<'static>> {
        let selected_chat = self.state.selected_chat();
        let selected_chat_name = selected_chat
            .map(|chat| chat.name.as_ref())
            .unwrap_or("None");
        let filter = if self.state.filter.is_empty() {
            "none".to_owned()
        } else {
            self.state.filter.clone()
        };
        let mode = if self.state.filter_mode {
            format!("filtering {}", self.state.filter_scope.status_label())
        } else {
            "normal".to_owned()
        };
        let mut details = vec![
            Line::from(format!("Selected: {selected_chat_name}")),
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
        ];

        if let Some(chat) = selected_chat {
            details.push(Line::from(""));
            details.push(Line::from(Span::styled("Chat", self.theme.pane_title())));
            details.push(Line::from(format!(
                "  Type: {}",
                chat_kind_label(chat.kind)
            )));
            details.push(Line::from(format!(
                "  Membership: {}",
                chat_membership_label(chat.membership)
            )));
            if let Some(last_message_at) = chat.last_message_at {
                details.push(Line::from(format!(
                    "  Last activity: {}",
                    message_list::format_message_datetime(last_message_at)
                )));
            }
            if chat.platform == Platform::Slack && !matches!(chat.kind, ChatKind::Direct) {
                details.push(Line::from(""));
                details.push(Line::from(Span::styled("Members", self.theme.status_key())));
                let key = (chat.account.clone(), chat.id.clone());
                if let Some(members) = self.state.chat_members.get(&key) {
                    details.push(Line::from(format!("  {} members loaded", members.len())));
                    for member in members.iter().take(30) {
                        details.push(Line::from(format!("  {}", member.display_name)));
                    }
                    if members.len() > 30 {
                        details.push(Line::from(format!("  … {} more", members.len() - 30)));
                    }
                } else if self.state.loading_chat_members.contains(&key) {
                    details.push(Line::from("  Loading members…"));
                } else {
                    details.push(Line::from("  Members unavailable"));
                }
            }
        }

        details.extend([
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
            Line::from("  Ctrl+S: settings"),
            Line::from("  Ctrl+X: image previews Matrix/HD"),
            Line::from("  PageUp/PageDown: faster"),
            Line::from("  Home/End: edges"),
            Line::from("  Ctrl+Q or Ctrl+C: quit"),
        ]);
        details
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
            .map(|chat| chat.name.to_string())
            .unwrap_or_else(|| "Unknown chat".to_owned());
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
                let key = message_list::MediaPreviewKey {
                    path: path.to_path_buf(),
                    width: area.width.saturating_sub(4).clamp(1, 16),
                    rows: 6,
                };
                match self.media_preview_cache.get(&key) {
                    Some(Ok(rows)) => Some(rows.clone()),
                    Some(Err(_)) => None,
                    None => {
                        self.queue_media_preview_fetches(vec![message_list::MediaPreviewRequest {
                            key,
                        }]);
                        None
                    }
                }
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
                    format!(
                        "{} {}",
                        message_list::reaction_display_emoji(reaction.emoji.as_ref()),
                        senders
                    )
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
        &mut self,
        frame: &mut Frame<'_>,
        area: ratatui::layout::Rect,
        thread_root: &MessageId,
    ) {
        let root = self.message_by_id(thread_root).cloned();
        let replies = self
            .thread_replies(thread_root)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let reply_title = reply_count_label(replies.len());
        let chat_name = root
            .as_ref()
            .and_then(|message| {
                self.state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            })
            .map(|chat| chat.name.as_ref())
            .unwrap_or("Current chat");

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(self.thread_compose_height(area)),
            ])
            .split(area);

        let thread_area = chunks[0];
        let compose_area = chunks[1];

        let outer = Block::default()
            .title(" Thread ")
            .borders(Borders::ALL)
            .border_style(
                self.theme
                    .focus_border(self.state.focus == FocusPane::Details),
            );
        let inner = outer.inner(thread_area);
        frame.render_widget(outer, thread_area);

        if inner.is_empty() {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(inner);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Thread", self.theme.pane_title()),
                Span::styled(" · ", self.theme.muted()),
                Span::styled(chat_name.to_string(), self.theme.status_key()),
            ]),
            Line::from(Span::styled(
                "Esc close · replies stay in context",
                self.theme.muted(),
            )),
        ]);
        frame.render_widget(header, chunks[0]);

        let mut body_lines = Vec::new();
        body_lines.push(Line::from(Span::styled(
            "Original message",
            self.theme.status_key(),
        )));
        if let Some(root) = root {
            let render = thread_message_card_lines(
                &root,
                self.theme,
                true,
                chunks[1].width,
                &mut self.media_preview_cache,
                &self.link_metadata_cache,
            );
            self.queue_link_metadata_fetches(render.link_preview_requests);
            self.queue_media_preview_fetches(render.media_preview_requests);
            body_lines.extend(render.lines);
        } else {
            body_lines.push(Line::from(Span::styled(
                format!("  Original message {} is not loaded", short_id(thread_root)),
                self.theme.muted(),
            )));
            body_lines.push(Line::from(""));
        }

        body_lines.push(thread_reply_divider(&reply_title, self.theme));
        if replies.is_empty() {
            body_lines.push(Line::from(Span::styled(
                "  No replies yet. Choose Reply from message actions to start one.",
                self.theme.muted(),
            )));
        } else {
            // Index of the first previously-unread reply, so we can draw a
            // "new replies" divider above it (Slack-style). Captured at open
            // time in `thread_open_unread`; clamped to the reply range.
            let unread = (self.state.thread_open_unread as usize).min(replies.len());
            let divider_at = replies.len().saturating_sub(unread);
            for (index, reply) in replies.iter().enumerate() {
                if unread > 0 && index == divider_at {
                    body_lines.push(thread_reply_divider(&new_replies_label(unread), self.theme));
                }
                let render = thread_message_card_lines(
                    reply,
                    self.theme,
                    false,
                    chunks[1].width,
                    &mut self.media_preview_cache,
                    &self.link_metadata_cache,
                );
                self.queue_link_metadata_fetches(render.link_preview_requests);
                self.queue_media_preview_fetches(render.media_preview_requests);
                body_lines.extend(render.lines);
            }
        }

        let content_len = body_lines.len();
        let body = Paragraph::new(body_lines)
            .scroll((self.state.details_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(body, chunks[1]);
        self.draw_vertical_scrollbar(frame, chunks[1], content_len, self.state.details_scroll);

        let is_thread_focused = self.state.focus == FocusPane::Details;
        self.draw_thread_compose(frame, compose_area, is_thread_focused);
    }

    fn filtered_thread_matches(&self) -> Vec<&Message> {
        let Some(thread_root) = self.state.thread_root.as_ref() else {
            return Vec::new();
        };
        let mut matches = Vec::new();
        if let Some(root) = self.message_by_id(thread_root)
            && message_matches_filter(root, &self.state.filter)
        {
            matches.push(root);
        }
        for reply in self.thread_replies(thread_root) {
            if message_matches_filter(reply, &self.state.filter) {
                matches.push(reply);
            }
        }
        matches
    }

    fn draw_filtered_thread_details(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
        let matches = self
            .filtered_thread_matches()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let chat_name = self
            .state
            .thread_root
            .as_ref()
            .and_then(|root| self.message_by_id(root))
            .and_then(|message| {
                self.state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            })
            .map(|chat| chat.name.as_ref())
            .unwrap_or("Current chat");

        let outer = Block::default()
            .title(" Thread ")
            .borders(Borders::ALL)
            .border_style(
                self.theme
                    .focus_border(self.state.focus == FocusPane::Details),
            );
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        if inner.is_empty() {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(inner);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Thread", self.theme.pane_title()),
                Span::styled(" · ", self.theme.muted()),
                Span::styled(chat_name.to_string(), self.theme.status_key()),
            ]),
            Line::from(Span::styled(
                format!(
                    "filter: {} · {} matches · Esc finishes",
                    self.state.filter,
                    matches.len()
                ),
                self.theme.muted(),
            )),
        ]);
        frame.render_widget(header, chunks[0]);

        let mut body_lines = Vec::new();
        if matches.is_empty() {
            body_lines.push(Line::from(Span::styled(
                "  No thread items match this filter.",
                self.theme.muted(),
            )));
        } else {
            for message in matches {
                let is_root = self
                    .state
                    .thread_root
                    .as_ref()
                    .is_some_and(|root| root.as_ref() == message.id.as_ref());
                let render = thread_message_card_lines(
                    &message,
                    self.theme,
                    is_root,
                    chunks[1].width,
                    &mut self.media_preview_cache,
                    &self.link_metadata_cache,
                );
                self.queue_link_metadata_fetches(render.link_preview_requests);
                self.queue_media_preview_fetches(render.media_preview_requests);
                body_lines.extend(render.lines);
            }
        }

        let content_len = body_lines.len();
        let body = Paragraph::new(body_lines)
            .scroll((self.state.details_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(Wrap { trim: false });
        frame.render_widget(body, chunks[1]);
        self.draw_vertical_scrollbar(frame, chunks[1], content_len, self.state.details_scroll);
    }

    fn draw_thread_compose(&self, frame: &mut Frame<'_>, area: Rect, is_focused: bool) {
        if area.is_empty() {
            return;
        }

        let block = Block::default()
            .title("Reply in thread")
            .borders(Borders::ALL)
            .border_style(self.theme.focus_border(is_focused));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut compose = self.compose_textarea_for_render(
            &self.state.thread_compose,
            is_focused,
            "Reply in thread...",
        );
        compose.remove_block();
        frame.render_widget(&compose, inner);
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

        let activity_spans = self.network_activity_status_spans(area.width as usize);
        if activity_spans.is_empty() {
            let paragraph = Paragraph::new(Line::from(spans)).style(self.theme.status_bar());
            frame.render_widget(paragraph, area);
            return;
        }

        let activity_width = spans_width(&activity_spans).min(area.width as usize);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(activity_width.min(u16::MAX as usize) as u16),
            ])
            .split(area);
        let left = Paragraph::new(Line::from(spans)).style(self.theme.status_bar());
        frame.render_widget(left, chunks[0]);
        let right = Paragraph::new(Line::from(activity_spans)).style(self.theme.status_bar());
        frame.render_widget(right, chunks[1]);
    }

    fn network_activity_status_spans(&self, max_width: usize) -> Vec<Span<'static>> {
        network_activity_status_spans(
            self.settings.network_activity,
            &self.providers,
            &self.state.network_activity,
            self.theme,
            Utc::now(),
            max_width,
        )
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
                hint("Delete removes"),
                hint("Esc cancels"),
            ];
        }

        if self.state.account_setup.is_some() {
            return vec![
                hint("Add account"),
                hint("Arrow keys choose"),
                hint("Enter starts setup"),
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
                Span::styled(
                    format!("Filter {}", self.state.filter_scope.label()),
                    self.theme.status_bar(),
                ),
                hint("Type to filter"),
                hint("↑↓/Enter select"),
                hint("←→ switch scope"),
                hint("Esc finishes"),
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

        let modal = self.action_menu_rect(area, menu);
        if modal.is_empty() {
            return;
        }

        let mut lines = vec![Line::from(Span::styled(
            "Message actions",
            self.theme.pane_title(),
        ))];
        for (index, item) in menu.items.iter().enumerate() {
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

    fn draw_forward_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(picker) = &self.state.forward_picker else {
            return;
        };
        let modal = self.forward_picker_rect(area, picker);
        if modal.is_empty() {
            return;
        }
        let matches = forward_picker_filtered_indices(picker);
        let visible_rows = forward_picker_visible_rows(modal);
        let start = forward_picker_scroll_start(picker.selected, visible_rows, matches.len());
        let end = matches.len().min(start.saturating_add(visible_rows));
        let query = if picker.query.is_empty() {
            "type to search".to_owned()
        } else {
            picker.query.clone()
        };
        let mut lines = vec![
            Line::from(Span::styled("Forward message", self.theme.pane_title())),
            Line::from(vec![
                Span::styled("Search: ", self.theme.muted()),
                Span::styled(
                    truncate_chars(&query, modal.width.saturating_sub(12) as usize),
                    if picker.query.is_empty() {
                        self.theme.muted()
                    } else {
                        self.theme.status_key()
                    },
                ),
            ]),
        ];
        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "No matching destinations",
                self.theme.muted(),
            )));
            lines.push(Line::from(Span::styled(
                "Backspace clears · Esc cancels",
                self.theme.muted(),
            )));
        } else {
            for (index, target_index) in matches[start..end].iter().copied().enumerate() {
                let match_index = start + index;
                let target = &picker.targets[target_index];
                let selected = match_index == picker.selected;
                let prefix = if selected { "› " } else { "  " };
                let style = if selected {
                    self.theme.status_key()
                } else {
                    self.theme.status_bar()
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_owned(), style),
                    Span::styled(
                        truncate_chars(&target.label, modal.width.saturating_sub(8) as usize),
                        style,
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        truncate_chars(&target.subtitle, modal.width.saturating_sub(6) as usize),
                        self.theme.muted(),
                    ),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "Enter forwards · type filters · Esc cancels",
                self.theme.muted(),
            )));
        }
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Forward")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
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

        if setup.show_help {
            for line in slack_setup_help_lines(self.theme) {
                lines.push(line);
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "? back · Esc hide",
                self.theme.muted(),
            )));
            let paragraph = Paragraph::new(lines)
                .block(
                    Block::default()
                        .title("Slack help")
                        .borders(Borders::ALL)
                        .border_style(self.theme.overlay_border()),
                )
                .wrap(Wrap { trim: true });
            frame.render_widget(Clear, modal);
            frame.render_widget(paragraph, modal);
            return;
        }

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
                for (index, mode) in setup.available_modes().iter().copied().enumerate() {
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
                if setup.bundled_oauth_app && setup.selected_mode() == SlackSetupMode::Automatic {
                    lines.push(Line::from(Span::styled(
                        "chat-cli has a built-in Slack app — no app creation or Client ID/Secret needed.",
                        self.theme.status_key(),
                    )));
                    lines.push(Line::from(""));
                    lines.push(Line::from(
                        "  1. Press Enter to open your browser and sign in to Slack.",
                    ));
                    lines.push(Line::from(
                        "  2. Approve the requested permissions for this workspace.",
                    ));
                    lines.push(Line::from(
                        "  3. chat-cli captures the result automatically and finishes sign-in.",
                    ));
                    lines.push(Line::from(Span::styled(
                        "Each workspace is approved separately; an admin may need to approve the requested scopes.",
                        self.theme.muted(),
                    )));
                    if setup.configured_realtime {
                        lines.push(Line::from(Span::styled(
                            "Realtime is already configured from startup settings; no App-Level Token input is needed here.",
                            self.theme.status_key(),
                        )));
                    } else {
                        lines.push(Line::from(Span::styled(
                            "Realtime is not configured yet. Slack will use periodic polling unless you provide an xapp- App-Level Token with connections:write.",
                            self.theme.muted(),
                        )));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Primary action: press Enter to open Slack and sign in automatically.",
                        self.theme.status_key(),
                    )));
                    lines.push(Line::from(Span::styled(
                        "Advanced: choose User OAuth/Manual App if you want to use your own Slack app instead.",
                        self.theme.muted(),
                    )));
                    if !setup.credential_fields().is_empty() {
                        lines.push(Line::from(""));
                        lines.push(Line::from(Span::styled(
                            "Optional realtime field",
                            self.theme.status_key(),
                        )));
                        for (index, field) in setup.credential_fields().iter().copied().enumerate()
                        {
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
                    }
                } else if let Some(url) = &setup.oauth_url {
                    lines.push(Line::from(format!(
                        "Slack app creation URL: {}",
                        slack_app_creation_url_display(url)
                    )));
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Next steps in Slack:",
                        self.theme.status_key(),
                    )));
                    lines.push(Line::from(
                        "  1. Click Save Changes in Slack if it is enabled.",
                    ));
                    lines.push(Line::from(
                        "  2. Open OAuth & Permissions from the left sidebar.",
                    ));
                    lines.push(Line::from("  3. Open Basic Information and copy the Client ID and Client Secret into the fields below."));
                    lines.push(Line::from(
                        "  4. Submit: chat-cli opens your browser to sign in and captures the token automatically (no copy/paste).",
                    ));
                    lines.push(Line::from("  5. For realtime: Basic Information → App-Level Tokens → create xapp-... with connections:write, then paste it in App token. Without it, Slack falls back to slow periodic polling."));
                    lines.push(Line::from(Span::styled(
                        "If you later change scopes, Reinstall to Workspace or history reads fail with missing_scope.",
                        self.theme.muted(),
                    )));
                    lines.push(Line::from(Span::styled(
                        "Use xapp-... only in the App token field; ignore Signing Secret and Verification Token.",
                        self.theme.muted(),
                    )));
                    lines.push(Line::from(Span::styled(
                        "The URL was opened in your browser and copied to the clipboard when available.",
                        self.theme.muted(),
                    )));
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "Credential fields",
                        self.theme.status_key(),
                    )));
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
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Connect more workspaces independently of this one:",
                    self.theme.status_key(),
                )));
                lines.push(Line::from("  a  Add another Slack workspace"));
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
            "↑/↓ choose · Tab fields · 1-6 quick select · Enter continue · a add workspace · ? help · Esc hide",
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
            Line::from("  Ctrl+S: open settings"),
            Line::from("  Ctrl+X: toggle image previews Matrix/HD"),
            Line::from("  Ctrl+Q or Ctrl+C: quit"),
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
            Line::from(
                "  Enter: send text, or attach/send if compose is an existing local file path",
            ),
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

    fn draw_settings_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(settings_overlay) = &self.state.settings_overlay else {
            return;
        };
        if area.width < 40 || area.height < 12 {
            return;
        }

        let modal = self.settings_overlay_rect(area);
        let mut lines = vec![
            Line::from(Span::styled(
                "Chat organization and notification settings",
                self.theme.pane_title(),
            )),
            Line::from(Span::styled(
                "Up/Down selects · Enter/Space changes · Esc closes",
                self.theme.muted(),
            )),
            Line::from(""),
        ];
        for (index, item) in SettingsItem::ALL.iter().enumerate() {
            let selected = index == settings_overlay.selected;
            let marker = if selected { "›" } else { " " };
            let value =
                item.value_text(&self.settings, self.archive_running_for_current_accounts());
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            let marker_text = item
                .checkbox(&self.settings)
                .map(|checked| format!("[{}] ", if checked { "x" } else { " " }))
                .unwrap_or_else(|| "    ".to_owned());
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(marker_text, style),
                Span::styled(item.label(), style),
                Span::styled(format!(" ({value})"), self.theme.muted()),
            ]));
            if selected {
                lines.push(Line::from(Span::styled(
                    format!("    {}", item.description()),
                    self.theme.muted(),
                )));
            }
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Settings")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_account_setup_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(setup) = &self.state.account_setup else {
            return;
        };
        if area.width < 40 || area.height < 10 {
            return;
        }

        let modal = self.account_setup_overlay_rect(area);
        let mut lines = vec![
            Line::from(Span::styled("Add account", self.theme.pane_title())),
            Line::from(Span::styled(
                "Connect a chat app without CLI flags or token formats.",
                self.theme.muted(),
            )),
            Line::from(""),
        ];

        for (index, provider) in AccountProviderKind::ALL.iter().copied().enumerate() {
            let selected = index == setup.selected_provider;
            let prefix = if selected { "› " } else { "  " };
            let style = if selected {
                self.theme.status_key()
            } else {
                self.theme.status_bar()
            };
            lines.push(Line::from(Span::styled(
                format!("{prefix}{}", provider.label()),
                style,
            )));
            if selected {
                lines.push(Line::from(Span::styled(
                    format!("    {}", provider.summary()),
                    self.theme.muted(),
                )));
                lines.push(Line::from(Span::styled(
                    format!("    {}", provider.setup_hint()),
                    self.theme.muted(),
                )));
            }
        }

        if let Some(status) = &setup.status
            && !status.is_empty()
        {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(status.clone(), self.theme.muted())));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Enter start setup · Esc cancel",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Connect chat app")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
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
        let height = (options.len() as u16).saturating_add(5).clamp(7, 15);
        let modal = centered_fixed_rect(area, width, height);
        let mut lines = vec![
            Line::from(Span::styled("Accounts", self.theme.pane_title())),
            Line::from(Span::styled(
                "Filter by account or connect another account",
                self.theme.muted(),
            )),
        ];
        for (index, option) in options.iter().enumerate() {
            let selected = index == switcher.selected;
            let marker = if selected { "›" } else { " " };
            let active = match (&self.state.active_account, &option.provider_id, option.kind) {
                (_, _, AccountOptionKind::AddAccount) => " +",
                (None, None, AccountOptionKind::AllAccounts) => " •",
                (Some(active), Some(provider_id), AccountOptionKind::Provider)
                    if active == provider_id =>
                {
                    " •"
                }
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
            "Enter selects · Delete removes account · Esc cancels",
            self.theme.muted(),
        )));
        if let Some(provider_id) = &switcher.confirm_remove {
            lines.push(Line::from(Span::styled(
                format!("Press Delete again to remove {provider_id} and local cache"),
                Style::default().fg(Color::Red),
            )));
        }

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Accounts")
                    .borders(Borders::ALL)
                    .border_style(self.theme.overlay_border()),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, modal);
        frame.render_widget(paragraph, modal);
    }

    fn draw_threads_inbox(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(inbox) = &self.state.threads_inbox else {
            return;
        };
        if area.width < 32 || area.height < 8 {
            return;
        }

        let unread_threads = inbox.entries.len();
        let title = if unread_threads == 0 {
            "Threads · all caught up".to_owned()
        } else {
            format!("Threads · {unread_threads} unread")
        };
        let width = area.width.saturating_sub(4).clamp(40, 80);
        let visible = (area.height.saturating_sub(6)) as usize;
        let height = ((inbox.entries.len().max(1) * 2) as u16)
            .saturating_add(5)
            .clamp(8, area.height.saturating_sub(2));
        let modal = centered_fixed_rect(area, width, height);

        let mut lines = vec![Line::from(Span::styled(
            "Threads with new replies",
            self.theme.muted(),
        ))];

        if inbox.entries.is_empty() {
            lines.push(Line::from(Span::styled(
                "  You're all caught up. No threads have new replies.",
                self.theme.muted(),
            )));
        } else {
            // Keep the selected entry visible with a simple window.
            let start = inbox
                .selected
                .saturating_sub(visible.saturating_sub(1).max(1) / 2);
            for (index, entry) in inbox
                .entries
                .iter()
                .enumerate()
                .skip(start)
                .take(visible.max(1))
            {
                let selected = index == inbox.selected;
                let marker = if selected { "›" } else { " " };
                let header_style = if selected {
                    self.theme.status_key()
                } else {
                    self.theme.status_bar()
                };
                let when = entry
                    .last_reply_at
                    .map(format_relative_time)
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} "), header_style),
                    Span::styled(truncate_chars(&entry.chat_name, 24), header_style),
                    Span::styled(
                        format!(
                            "  {} new of {}",
                            entry.unread_reply_count, entry.reply_count
                        ),
                        self.theme.unread(),
                    ),
                    Span::styled(format!("   {when}"), self.theme.muted()),
                ]));
                lines.push(Line::from(Span::styled(
                    format!("    ↪ {}", truncate_chars(&entry.preview, 52)),
                    self.theme.muted(),
                )));
            }
        }

        lines.push(Line::from(Span::styled(
            "↑/↓ move · Enter open thread · Esc back",
            self.theme.muted(),
        )));

        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
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
        let protocol = if self.settings.image_preview_mode == ImagePreviewMode::Hd {
            self.cached_terminal_image_protocol(&viewer.path, max_inner_size)
                .and_then(Result::ok)
        } else {
            None
        };
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
        let key = self.queue_terminal_image_protocol(path, size, TerminalImageResizeMode::Fit)?;
        self.image_protocol_cache.get(&key).cloned()
    }

    fn cached_scaled_terminal_image_protocol(
        &mut self,
        path: &Path,
        size: Size,
    ) -> Option<std::result::Result<Protocol, String>> {
        let key = self.queue_terminal_image_protocol(path, size, TerminalImageResizeMode::Scale)?;
        self.image_protocol_cache.get(&key).cloned()
    }

    fn cached_terminal_image_protocol_from_bytes(
        &mut self,
        label_path: &Path,
        bytes: Arc<[u8]>,
        size: Size,
    ) -> Option<std::result::Result<Protocol, String>> {
        let key = self.queue_terminal_image_protocol_from_bytes(label_path, bytes, size)?;
        self.image_protocol_cache.get(&key).cloned()
    }

    fn render_inline_hd_image_previews(&mut self, frame: &mut Frame<'_>, messages_area: Rect) {
        if self.settings.image_preview_mode != ImagePreviewMode::Hd {
            return;
        }
        if self
            .image_picker
            .as_ref()
            .is_none_or(|picker| picker.protocol_type() == ProtocolType::Halfblocks)
        {
            return;
        }
        let content_area = inner_area(messages_area);
        if content_area.is_empty() {
            return;
        }

        let hits = self.state.media_hits.clone();
        for hit in hits {
            if hit.end_line < self.state.message_scroll {
                continue;
            }
            let relative_start = hit.start_line.saturating_sub(self.state.message_scroll);
            let top_padding = self.state.message_top_padding;
            if relative_start.saturating_add(top_padding) >= content_area.height as usize {
                continue;
            }
            let relative_end = hit.end_line.saturating_sub(self.state.message_scroll);
            let start_y = content_area
                .y
                .saturating_add(relative_start.saturating_add(top_padding) as u16);
            let end_y = content_area.y.saturating_add(
                relative_end
                    .saturating_add(top_padding)
                    .min(content_area.height as usize - 1) as u16,
            );
            if end_y < start_y {
                continue;
            }
            let preview_start_x = content_area.x.saturating_add(hit.preview_start_col);
            let preview_end_x = content_area
                .x
                .saturating_add(hit.preview_end_col.min(content_area.width));
            let preview_width = preview_end_x.saturating_sub(preview_start_x);
            let height = end_y.saturating_sub(start_y).saturating_add(1);
            if preview_width == 0 || height == 0 {
                continue;
            }
            let preview_area = Rect::new(preview_start_x, start_y, preview_width, height);
            if let Some(Ok(protocol)) = self.cached_scaled_terminal_image_protocol(
                &hit.path,
                Size::new(preview_area.width, preview_area.height),
            ) {
                let clear_start_x = content_area.x.saturating_add(hit.start_col);
                let clear_end_x = content_area
                    .x
                    .saturating_add(hit.end_col.min(content_area.width));
                let clear_width = clear_end_x.saturating_sub(clear_start_x);
                let clear_area = if clear_width > 0 {
                    Rect::new(clear_start_x, start_y, clear_width, height)
                } else {
                    preview_area
                };
                frame.render_widget(Clear, clear_area);

                let protocol_size = protocol.size();
                let image_width = protocol_size.width.min(preview_area.width).max(1);
                let image_height = protocol_size.height.min(preview_area.height).max(1);
                let image_x = preview_area
                    .x
                    .saturating_add(preview_area.width.saturating_sub(image_width) / 2);
                let image_y = preview_area
                    .y
                    .saturating_add(preview_area.height.saturating_sub(image_height) / 2);
                let image_area = Rect::new(image_x, image_y, image_width, image_height);
                let image = TerminalImage::new(&protocol).allow_clipping(true);
                frame.render_widget(image, image_area);
            }
        }
    }

    fn render_chat_list_hd_avatars(
        &mut self,
        frame: &mut Frame<'_>,
        list_area: Rect,
        layout: &chat_list::ChatListLayout,
    ) {
        if self.settings.image_preview_mode != ImagePreviewMode::Hd
            || !self.terminal_hd_images_supported()
        {
            return;
        }
        let inner = inner_area(list_area);
        if inner.is_empty() {
            return;
        }

        let mut y = inner.y;
        for row in &layout.visible_rows {
            match row {
                chat_list::ChatListRow::Section { .. } => {
                    y = y.saturating_add(1);
                }
                chat_list::ChatListRow::Chat { chat_index } => {
                    if y >= inner.y.saturating_add(inner.height) {
                        break;
                    }
                    let Some(chat) = self.state.chats.get(*chat_index).cloned() else {
                        y = y.saturating_add(chat_list::CHAT_AVATAR_ROWS);
                        continue;
                    };
                    if let Some(path) = self.chat_avatar_path(&chat) {
                        let key = AvatarPreviewKey {
                            path,
                            width: chat_list::CHAT_AVATAR_WIDTH,
                            rows: chat_list::CHAT_AVATAR_ROWS,
                            source: AvatarPreviewSource::Avatar,
                        };
                        let area = Rect::new(
                            inner.x,
                            y,
                            chat_list::CHAT_AVATAR_WIDTH,
                            chat_list::CHAT_AVATAR_ROWS
                                .min(inner.y.saturating_add(inner.height).saturating_sub(y)),
                        );
                        self.render_hd_avatar_for_key(frame, area, &key);
                    }

                    if let Some(account) = self.account_for_provider(&chat.account)
                        && let Some(path) = self.account_badge_avatar_path(&chat.account, &account)
                    {
                        let key = AvatarPreviewKey {
                            path,
                            width: chat_list::ACCOUNT_BADGE_WIDTH,
                            rows: chat_list::ACCOUNT_BADGE_ROWS,
                            source: AvatarPreviewSource::AccountBadge,
                        };
                        let area = Rect::new(
                            inner
                                .x
                                .saturating_add(chat_list::CHAT_AVATAR_WIDTH)
                                .saturating_add(1),
                            y,
                            chat_list::ACCOUNT_BADGE_WIDTH,
                            chat_list::ACCOUNT_BADGE_ROWS,
                        );
                        self.render_hd_avatar_for_key(frame, area, &key);
                    }
                    y = y.saturating_add(chat_list::CHAT_AVATAR_ROWS);
                }
            }
        }
    }

    fn render_message_hd_avatars(&mut self, frame: &mut Frame<'_>, messages_area: Rect) {
        if self.settings.image_preview_mode != ImagePreviewMode::Hd
            || !self.terminal_hd_images_supported()
        {
            return;
        }
        let content_area = inner_area(messages_area);
        if content_area.is_empty() {
            return;
        }

        let hits = self.state.message_hits.clone();
        for hit in hits {
            let Some(avatar_hit) = hit.avatar_hit else {
                continue;
            };
            let Some(path) = hit.avatar_path else {
                continue;
            };
            if avatar_hit.line < self.state.message_scroll {
                continue;
            }
            let relative = avatar_hit.line.saturating_sub(self.state.message_scroll);
            let top_padding = self.state.message_top_padding;
            if relative.saturating_add(top_padding) >= content_area.height as usize {
                continue;
            }
            let y = content_area
                .y
                .saturating_add(relative.saturating_add(top_padding) as u16);
            let x = content_area.x.saturating_add(avatar_hit.start_col);
            let width = avatar_hit.end_col.saturating_sub(avatar_hit.start_col).min(
                content_area
                    .x
                    .saturating_add(content_area.width)
                    .saturating_sub(x),
            );
            if width == 0 {
                continue;
            }
            let area = Rect::new(x, y, width, message_list::MESSAGE_AVATAR_ROWS);
            let key = AvatarPreviewKey {
                path,
                width: message_list::MESSAGE_AVATAR_WIDTH,
                rows: message_list::MESSAGE_AVATAR_ROWS,
                source: AvatarPreviewSource::Avatar,
            };
            self.render_hd_avatar_for_key(frame, area, &key);
        }
    }

    fn render_hd_avatar_for_key(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        key: &AvatarPreviewKey,
    ) {
        if area.is_empty() {
            return;
        }
        let Some(bytes) = self
            .avatar_preview_cache
            .get(key)
            .and_then(|result| result.as_ref().ok())
            .and_then(|avatar| avatar.thumbnail.clone())
        else {
            if !self.pending_avatar_previews.contains(key) {
                self.queue_avatar_preview(key.clone());
            }
            return;
        };
        if let Some(Ok(protocol)) = self.cached_terminal_image_protocol_from_bytes(
            &key.path,
            bytes,
            Size::new(area.width, area.height),
        ) {
            frame.render_widget(Clear, area);
            let image = TerminalImage::new(&protocol).allow_clipping(true);
            frame.render_widget(image, area);
        }
    }

    fn terminal_hd_images_supported(&self) -> bool {
        self.image_picker
            .as_ref()
            .is_some_and(|picker| picker.protocol_type() != ProtocolType::Halfblocks)
    }

    fn queue_terminal_image_protocol(
        &mut self,
        path: &Path,
        size: Size,
        resize_mode: TerminalImageResizeMode,
    ) -> Option<ImageProtocolKey> {
        let picker = self.image_picker.as_ref()?;
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return None;
        }

        let key = ImageProtocolKey {
            path: path.to_path_buf(),
            bytes_hash: None,
            width: size.width,
            height: size.height,
            resize_mode,
        };
        if size.width == 0 || size.height == 0 {
            self.image_protocol_cache
                .insert(key.clone(), Err("image area is too small".to_owned()));
            return Some(key);
        }
        if self.image_protocol_cache.contains_key(&key)
            || !self.pending_image_protocols.insert(key.clone())
        {
            return Some(key);
        }

        let picker = picker.clone();
        let tx = self.image_protocol_tx.clone();
        let path = path.to_path_buf();
        let request_key = key.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let result = build_terminal_image_protocol(&picker, &path, size, resize_mode);
            let _ = tx.send(ImageProtocolFetchResult {
                key: request_key,
                result,
                elapsed: started.elapsed(),
            });
        });
        Some(key)
    }

    fn queue_terminal_image_protocol_from_bytes(
        &mut self,
        label_path: &Path,
        bytes: Arc<[u8]>,
        size: Size,
    ) -> Option<ImageProtocolKey> {
        let picker = self.image_picker.as_ref()?;
        if picker.protocol_type() == ProtocolType::Halfblocks {
            return None;
        }
        let key = ImageProtocolKey {
            path: label_path.to_path_buf(),
            bytes_hash: Some(stable_bytes_hash(&bytes)),
            width: size.width,
            height: size.height,
            resize_mode: TerminalImageResizeMode::Fit,
        };
        if size.width == 0 || size.height == 0 {
            self.image_protocol_cache
                .insert(key.clone(), Err("image area is too small".to_owned()));
            return Some(key);
        }
        if self.image_protocol_cache.contains_key(&key)
            || !self.pending_image_protocols.insert(key.clone())
        {
            return Some(key);
        }

        let picker = picker.clone();
        let tx = self.image_protocol_tx.clone();
        let request_key = key.clone();
        tokio::task::spawn_blocking(move || {
            let started = Instant::now();
            let result = build_terminal_image_protocol_from_bytes(&picker, &bytes, size);
            let _ = tx.send(ImageProtocolFetchResult {
                key: request_key,
                result,
                elapsed: started.elapsed(),
            });
        });
        Some(key)
    }

    fn clear_image_protocol_work(&mut self) {
        self.image_protocol_cache.clear();
        self.pending_image_protocols.clear();
        while self.image_protocol_rx.try_recv().is_ok() {}
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
        let key = message_list::MediaPreviewKey {
            path: path.to_path_buf(),
            width: preview_width,
            rows: preview_rows,
        };
        let preview = match self.media_preview_cache.get(&key) {
            Some(Ok(rows)) => Some(Ok(rows.clone())),
            Some(Err(error)) => Some(Err(error.clone())),
            None => {
                self.queue_media_preview_fetches(vec![message_list::MediaPreviewRequest { key }]);
                None
            }
        };
        let mut lines = Vec::new();

        match preview {
            Some(Ok(preview_rows)) => lines.extend(preview_rows.into_iter().map(Line::from)),
            Some(Err(error)) => {
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
            None => {
                lines.extend(
                    message_list::fallback_preview_rows(
                        preview_width,
                        preview_rows,
                        Color::DarkGray,
                        "loading image",
                    )
                    .into_iter()
                    .map(Line::from),
                );
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

    async fn connect_provider_at(&mut self, provider_index: usize) -> Result<()> {
        let account = self.providers[provider_index].account_info();
        self.state.account_statuses.insert(
            account.id.clone(),
            AccountStatus::new(&account, AccountConnection::Connecting),
        );
        let config_json = self.providers[provider_index]
            .config_json()
            .unwrap_or_else(|| "{}".to_owned());
        self.store.upsert_account(&account, &config_json).await?;
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
                return Ok(());
            }
            if account.platform == Platform::WhatsApp {
                return Ok(());
            }
            return Err(error);
        }
        if let Some(status) = self.state.account_statuses.get_mut(&account.id) {
            status.connection = AccountConnection::Syncing(0);
            status.detail = None;
        }

        let refreshed_account = self.providers[provider_index].account_info();
        self.refresh_account_metadata(&refreshed_account, AccountConnection::Syncing(0))
            .await?;

        if refreshed_account.platform == Platform::Slack
            && !self.providers[provider_index].is_connected()
        {
            self.set_account_status(
                &account.id,
                AccountConnection::NeedsAuth,
                Some("complete Slack setup".to_owned()),
            );
            self.open_slack_setup_for_account(&account, None);
            return Ok(());
        }

        self.sync_provider_chats(provider_index).await?;
        if matches!(refreshed_account.platform, Platform::Unknown(_)) {
            let chats = self.store.get_chats(&refreshed_account.id).await?;
            for chat in &chats {
                let messages = self.providers[provider_index]
                    .history(&chat.id, None, HISTORY_LIMIT)
                    .await?;
                self.store.upsert_messages(&messages).await?;
            }
        }
        self.set_account_status(&refreshed_account.id, AccountConnection::Online, None);
        Ok(())
    }

    async fn refresh_account_metadata(
        &mut self,
        account: &Account,
        connection: AccountConnection,
    ) -> Result<()> {
        if let Some(provider) = self.provider_for_id(&account.id) {
            let config_json = provider.config_json().unwrap_or_else(|| "{}".to_owned());
            self.store.upsert_account(account, &config_json).await?;
        }
        if account.platform == Platform::Slack {
            self.log_perf_marker(
                "slack.account_metadata",
                format!(
                    "account={} display_name={} avatar={}",
                    account.id,
                    account.display_name,
                    account
                        .avatar
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<none>".to_owned())
                ),
            );
        }
        self.state
            .account_statuses
            .insert(account.id.clone(), AccountStatus::new(account, connection));
        Ok(())
    }

    async fn refresh_provider_account_metadata(
        &mut self,
        provider_id: &ProviderId,
        connection: AccountConnection,
    ) -> Result<()> {
        if let Some(account) = self.account_for_provider(provider_id) {
            self.refresh_account_metadata(&account, connection).await?;
        } else {
            self.set_account_status(provider_id, connection, None);
        }
        Ok(())
    }

    async fn sync_provider_chats(&mut self, provider_index: usize) -> Result<()> {
        let sync_started = Instant::now();
        let account = self.providers[provider_index].account_info();
        if account.platform == Platform::Slack && !self.providers[provider_index].is_connected() {
            return Ok(());
        }

        let chats_fetch_started = Instant::now();
        let mut chats = self.providers[provider_index].chats().await?;
        self.log_slow_perf_duration(
            "provider.chats_fetch",
            chats_fetch_started,
            format!("account={} count={}", account.id, chats.len()),
        );
        let stored_fetch_started = Instant::now();
        let stored_chats = self.store.get_chats(&account.id).await?;
        self.log_slow_perf_duration(
            "store.get_chats",
            stored_fetch_started,
            format!("account={} count={}", account.id, stored_chats.len()),
        );
        for chat in &mut chats {
            preserve_sidebar_activity_metadata(
                chat,
                stored_chats
                    .iter()
                    .find(|stored| stored.id == chat.id && stored.account == chat.account),
            );
        }
        for chat in &chats {
            self.store.upsert_chat(chat).await?;
        }
        if account.platform == Platform::Slack {
            let chat_ids = chats.iter().map(|chat| chat.id.clone()).collect::<Vec<_>>();
            self.store
                .delete_chats_not_in(&account.id, &chat_ids)
                .await?;
        }
        self.log_slow_perf_duration(
            "provider.sync_chats",
            sync_started,
            format!("account={} count={}", account.id, chats.len()),
        );
        Ok(())
    }

    async fn add_runtime_provider(&mut self, provider: ProviderBox) -> Result<()> {
        let account = provider.account_info();
        if self
            .providers
            .iter()
            .any(|existing| existing.id().as_ref() == account.id.as_ref())
        {
            bail!("account already exists: {}", account.display_name);
        }
        let provider_id = provider.id().clone();
        let receiver = provider.events();
        self.providers.push(provider);
        self.provider_receivers.push((provider_id, receiver));
        let provider_index = self.providers.len().saturating_sub(1);
        self.connect_provider_at(provider_index).await?;
        self.reload_chats().await?;
        self.state.active_account = Some(account.id.clone());
        let selection_changed = self.apply_filter();
        if selection_changed {
            self.reset_history_window_state();
        }
        self.request_selected_chat_history_sync();
        self.request_selected_chat_members();
        self.reload_selected_messages().await?;
        if self.consume_pending_history_sync_for_selected_chat() {
            self.sync_selected_chat_history().await?;
        }
        self.state.pending_scroll_to_latest = true;
        Ok(())
    }

    async fn bootstrap(&mut self) -> Result<()> {
        let bootstrap_started = Instant::now();
        self.log_perf_marker(
            "bootstrap.start",
            format!("providers={}", self.providers.len()),
        );
        for provider_index in 0..self.providers.len() {
            let provider_started = Instant::now();
            let provider_id = self.providers[provider_index].id().clone();
            self.connect_provider_at(provider_index).await?;
            self.log_slow_perf_duration(
                "bootstrap.connect_provider",
                provider_started,
                format!("provider={provider_id}"),
            );
        }

        self.rebuild_sidebar_activity_from_messages().await?;
        let reload_started = Instant::now();
        self.reload_chats().await?;
        self.log_slow_perf_duration(
            "bootstrap.reload_chats",
            reload_started,
            format!("chats={}", self.state.chats.len()),
        );
        self.request_selected_chat_history_sync();
        self.request_selected_chat_members();
        self.reload_selected_messages().await?;
        if self.consume_pending_history_sync_for_selected_chat() {
            self.sync_selected_chat_history().await?;
        }
        self.state.pending_scroll_to_latest = true;
        self.state.status = if self.state.chats.is_empty() {
            "ready - no chats loaded".to_owned()
        } else {
            "ready".to_owned()
        };
        self.log_perf_duration(
            "bootstrap.done",
            bootstrap_started,
            format!("chats={}", self.state.chats.len()),
        );
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
                let is_placeholder_whatsapp = is_placeholder_whatsapp_message(&message);
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_update_selected = selected_chat_id.as_ref() == Some(&message.chat_id);
                if !is_placeholder_whatsapp {
                    let store_started = Instant::now();
                    self.store.upsert_message(&message).await?;
                    self.log_slow_perf_duration(
                        "provider.message.store",
                        store_started,
                        format!(
                            "provider={provider_id} historical={is_historical} selected={should_update_selected}"
                        ),
                    );
                }
                let preview_started = Instant::now();
                self.refresh_chat_preview_from_message(&message).await?;
                if !is_historical {
                    self.mark_slack_live_message_unread_if_needed(&message)
                        .await?;
                    self.mark_live_thread_reply_unread_if_needed(&message)
                        .await?;
                }
                self.log_slow_perf_duration(
                    "provider.message.preview",
                    preview_started,
                    format!(
                        "provider={provider_id} historical={is_historical} selected={should_update_selected}"
                    ),
                );
                if should_update_selected && !is_placeholder_whatsapp {
                    let selected_started = Instant::now();
                    if is_historical {
                        self.append_historical_message_to_current_chat(message.clone());
                    } else {
                        self.reload_selected_messages().await?;
                    }
                    self.log_slow_perf_duration(
                        "provider.message.selected_update",
                        selected_started,
                        format!("provider={provider_id} historical={is_historical}"),
                    );
                }
                if !is_historical {
                    let reload_started = Instant::now();
                    self.reload_chats().await?;
                    self.log_slow_perf_duration(
                        "provider.message.reload_chats",
                        reload_started,
                        format!("provider={provider_id}"),
                    );
                    if !is_placeholder_whatsapp {
                        self.maybe_queue_notification(&message, is_historical);
                    }
                    if !is_placeholder_whatsapp
                        && self
                            .state
                            .pending_notifications
                            .iter()
                            .all(|notification| notification.notification.message_id != message.id)
                        && self
                            .state
                            .notification
                            .as_ref()
                            .is_none_or(|notification| notification.message_id != message.id)
                    {
                        self.state.status = format!("message event from {provider_id}");
                    }
                } else {
                    self.state.status = format!("syncing historical messages from {provider_id}");
                }
            }
            ProviderEvent::MessageEdited { message } => {
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                let should_reload = selected_chat_id.as_ref() == Some(&message.chat_id);
                let store_started = Instant::now();
                self.store.upsert_message(&message).await?;
                self.log_slow_perf_duration(
                    "provider.message_edited.store",
                    store_started,
                    format!("provider={provider_id} selected={should_reload}"),
                );
                let preview_started = Instant::now();
                self.refresh_chat_preview_from_message(&message).await?;
                self.log_slow_perf_duration(
                    "provider.message_edited.preview",
                    preview_started,
                    format!("provider={provider_id} selected={should_reload}"),
                );
                if should_reload {
                    let reload_selected_started = Instant::now();
                    self.reload_selected_messages().await?;
                    self.log_slow_perf_duration(
                        "provider.message_edited.reload_selected",
                        reload_selected_started,
                        format!("provider={provider_id}"),
                    );
                }
                let reload_started = Instant::now();
                self.reload_chats().await?;
                self.log_slow_perf_duration(
                    "provider.message_edited.reload_chats",
                    reload_started,
                    format!("provider={provider_id}"),
                );
                self.state.status = format!("message edited from {provider_id}");
            }
            ProviderEvent::ChatUpdated(mut chat) => {
                let merge_started = Instant::now();
                let state_chat = self
                    .state
                    .chats
                    .iter()
                    .find(|existing| existing.id == chat.id && existing.account == chat.account)
                    .cloned();
                let stored_chat = if state_chat.is_none() {
                    self.store
                        .get_chats(&chat.account)
                        .await?
                        .into_iter()
                        .find(|existing| existing.id == chat.id && existing.account == chat.account)
                } else {
                    None
                };
                preserve_sidebar_activity_metadata(
                    &mut chat,
                    state_chat.as_ref().or(stored_chat.as_ref()),
                );
                self.log_slow_perf_duration(
                    "provider.chat_updated.merge",
                    merge_started,
                    format!(
                        "provider={provider_id} state_hit={} storage_hit={}",
                        state_chat.is_some(),
                        stored_chat.is_some()
                    ),
                );
                let store_started = Instant::now();
                self.store.upsert_chat(&chat).await?;
                self.log_slow_perf_duration(
                    "provider.chat_updated.store",
                    store_started,
                    format!("provider={provider_id}"),
                );
                let state_started = Instant::now();
                self.upsert_chat_in_state(chat);
                self.log_slow_perf_duration(
                    "provider.chat_updated.state",
                    state_started,
                    format!("provider={provider_id} chats={}", self.state.chats.len()),
                );
                self.state.status = format!("chat updated from {provider_id}");
            }
            ProviderEvent::ChatMerged {
                from_chat_id,
                to_chat_id,
                chat,
            } => {
                let account = chat.account.clone();
                self.store
                    .merge_chat(&account, &from_chat_id, &chat)
                    .await?;
                self.merge_chat_in_state(&account, &from_chat_id, chat);
                let selected_chat_id = self.state.selected_chat().map(|chat| chat.id.clone());
                if selected_chat_id.as_ref() == Some(&to_chat_id) {
                    self.reload_selected_messages().await?;
                }
                self.state.status = format!("chat alias merged from {provider_id}");
            }
            ProviderEvent::AuthRequired(challenge) => {
                self.set_account_status(
                    &provider_id,
                    AccountConnection::NeedsAuth,
                    Some(auth_challenge_label(&challenge).to_owned()),
                );
                // During a background browser OAuth login the provider emits an
                // `OAuthUrl` challenge so we open the browser. Don't rebuild the
                // setup overlay (which would wipe the entered credentials and
                // show paste instructions); just launch the authorize URL.
                if self.state.pending_slack_setup_load.as_ref() == Some(&provider_id) {
                    if let AuthChallenge::OAuthUrl(url) = &challenge {
                        open_and_copy_slack_oauth_url(url.as_ref());
                        self.state.status =
                            format!("opened Slack sign-in in browser for {provider_id}");
                    }
                } else if self.account_platform(&provider_id) == Some(Platform::Slack) {
                    self.open_slack_setup_for_provider(&provider_id, Some(&challenge), None);
                } else {
                    self.state.auth_overlay = Some(AuthOverlay {
                        provider_id: provider_id.clone(),
                        challenge,
                    });
                    self.state.status = format!("authentication required for {provider_id}");
                }
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
                self.refresh_provider_account_metadata(&provider_id, AccountConnection::Online)
                    .await?;
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
                self.refresh_provider_account_metadata(&provider_id, AccountConnection::Online)
                    .await?;
                if self.state.pending_slack_setup_load.as_ref() == Some(&provider_id) {
                    self.state.pending_slack_setup_load = None;
                    self.load_slack_setup_account(&provider_id).await?;
                }
                self.state.status = format!("sync complete for {provider_id}");
            }
            ProviderEvent::AccountNotice {
                title,
                body,
                severity,
            } => {
                self.deliver_account_notice(title.as_ref(), body.as_ref(), severity);
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
                if self.state.pending_slack_setup_load.as_ref() == Some(&provider_id) {
                    self.state.pending_slack_setup_load = None;
                }
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
                    self.clear_message_layout_cache();
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
            ProviderEvent::MessageDeleted { .. } | ProviderEvent::Receipt { .. } => {
                self.state.status = format!("event received from {provider_id}");
            }
            ProviderEvent::Typing {
                chat_id,
                sender,
                is_typing,
            } => {
                self.update_typing_indicator(&provider_id, chat_id, sender, is_typing);
                self.state.status = format!("typing event from {provider_id}");
            }
            ProviderEvent::NetworkActivity { direction, .. } => {
                self.record_network_activity(&provider_id, direction);
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
                let max = menu.items.len().saturating_sub(1);
                menu.selected = menu.selected.saturating_add(1).min(max);
            }
            KeyCode::Enter => {
                let message_id = menu.message_id.clone();
                let item = menu.items[menu.selected];
                self.state.action_menu = None;
                self.perform_action_menu_item(message_id, item).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_forward_picker_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(picker) = &mut self.state.forward_picker else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.state.forward_picker = None;
                self.state.status = "forward cancelled".to_owned();
            }
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = forward_picker_filtered_len(picker).saturating_sub(1);
                picker.selected = picker.selected.saturating_add(1).min(max);
            }
            KeyCode::Home => {
                picker.selected = 0;
            }
            KeyCode::End => {
                picker.selected = forward_picker_filtered_len(picker).saturating_sub(1);
            }
            KeyCode::PageUp => {
                picker.selected = picker.selected.saturating_sub(5);
            }
            KeyCode::PageDown => {
                let max = forward_picker_filtered_len(picker).saturating_sub(1);
                picker.selected = picker.selected.saturating_add(5).min(max);
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Delete => {
                picker.query.clear();
                picker.selected = 0;
            }
            KeyCode::Char(character) => {
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    picker.query.push(character);
                    picker.selected = 0;
                }
            }
            KeyCode::Enter => {
                let Some((message_id, target)) = self
                    .state
                    .forward_picker
                    .as_ref()
                    .and_then(forward_picker_selected_target)
                else {
                    self.state.status = "no matching forward destination".to_owned();
                    return Ok(false);
                };
                self.state.forward_picker = None;
                self.forward_message_to_target(message_id, target).await?;
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
            ActionMenuItem::Forward => self.open_forward_picker(message_id),
            ActionMenuItem::OpenLink => {
                if !self.open_message_link(&message_id) {
                    self.state.status = "selected message has no link".to_owned();
                }
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
        if is_ctrl_char(key, 'q') || is_ctrl_char(key, 'c') {
            self.state.should_quit = true;
            return Ok(false);
        }

        if is_ctrl_char(key, 'x') {
            self.toggle_image_preview_mode().await?;
            return Ok(false);
        }

        if self.state.account_switcher.is_some() {
            return self.handle_account_switcher_key(key).await;
        }

        if self.state.threads_inbox.is_some() {
            return self.handle_threads_inbox_key(key).await;
        }

        if self.state.account_setup.is_some() {
            return self.handle_account_setup_key(key).await;
        }

        if self.state.settings_overlay.is_some() {
            return self.handle_settings_overlay_key(key).await;
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

        if self.state.forward_picker.is_some() {
            return self.handle_forward_picker_key(key).await;
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
            return self.handle_filter_key(key).await;
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

        if is_ctrl_char(key, 's') {
            self.open_settings_overlay();
            return Ok(false);
        }

        if is_ctrl_char(key, 'f') {
            return self.enter_filter_mode().await;
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
            FocusPane::Details => self.handle_details_key(key).await,
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
                KeyCode::Enter => self.activate_selected_chat(),
                KeyCode::Down => self.select_next_chat(),
                KeyCode::Up => {
                    // At the top of the chat list, Up reveals the Threads inbox
                    // (an account-wide view of threads with new replies),
                    // mirroring native apps without introducing a new shortcut.
                    if self.state.focus == FocusPane::ChatList
                        && self.selected_visible_position() == Some(0)
                    {
                        self.open_threads_inbox().await?;
                        false
                    } else {
                        self.select_previous_chat()
                    }
                }
                KeyCode::Home => self.select_first_chat(),
                KeyCode::End => self.select_last_chat(),
                KeyCode::PageDown => self.page_down_chats(),
                KeyCode::PageUp => self.page_up_chats(),
                _ => false,
            },
        };
        Ok(selection_changed)
    }

    async fn handle_details_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter
                if self.state.thread_root.is_some()
                    && key
                        .modifiers
                        .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
                self.apply_thread_compose_edit_input(textarea_input(
                    TextAreaKey::Enter,
                    key.modifiers,
                ));
                false
            }
            KeyCode::Enter if self.state.thread_root.is_some() => {
                if let Err(err) = self.send_thread_composed_message().await {
                    self.state.status = format!("send failed: {err}");
                }
                false
            }
            KeyCode::Backspace if self.state.thread_root.is_some() => {
                self.apply_thread_compose_edit_input(textarea_input(
                    TextAreaKey::Backspace,
                    key.modifiers,
                ));
                false
            }
            KeyCode::Delete if self.state.thread_root.is_some() => {
                self.apply_thread_compose_edit_input(textarea_input(
                    TextAreaKey::Delete,
                    key.modifiers,
                ));
                false
            }
            KeyCode::Char(value)
                if self.state.thread_root.is_some()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.apply_thread_compose_edit_input(textarea_input(
                    TextAreaKey::Char(value),
                    key.modifiers,
                ));
                false
            }
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

        // When the help page is open it captures input until dismissed.
        if setup.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) => {
                    setup.show_help = false;
                    self.state.status = "Slack help closed".to_owned();
                }
                _ => {}
            }
            return Ok(false);
        }

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
            KeyCode::F(1) => {
                setup.show_help = true;
                self.state.status = "Slack help".to_owned();
            }
            KeyCode::Char('?')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                setup.show_help = true;
                self.state.status = "Slack help".to_owned();
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = setup
                        .selected_mode
                        .saturating_add(1)
                        .min(setup.available_modes().len().saturating_sub(1));
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt
                    if !setup.credential_fields().is_empty() =>
                {
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
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt
                    if !setup.credential_fields().is_empty() =>
                {
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
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt
                    if !setup.credential_fields().is_empty() =>
                {
                    setup.selected_credential_field = 0;
                    if let Some(field) = setup.selected_credential_field() {
                        self.state.status = format!("editing Slack {}", field.label());
                    }
                }
                _ => {}
            },
            KeyCode::End => match setup.phase {
                SlackSetupPhase::ChooseAuthMode => {
                    setup.selected_mode = setup.available_modes().len().saturating_sub(1);
                    self.state.status = format!("selected {}", setup.selected_mode().label());
                }
                SlackSetupPhase::EnterCredentials | SlackSetupPhase::OAuthPrompt
                    if !setup.credential_fields().is_empty() =>
                {
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
                    && ('1'..='7').contains(&value) =>
            {
                let selected = (value as usize).saturating_sub('1' as usize);
                setup.selected_mode = selected.min(setup.available_modes().len().saturating_sub(1));
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
            KeyCode::Char('a')
                if matches!(
                    setup.phase,
                    SlackSetupPhase::CapabilityReview | SlackSetupPhase::Connected
                ) =>
            {
                self.state.slack_setup = None;
                self.state.status = "add another Slack workspace".to_owned();
                self.start_account_setup(AccountProviderKind::Slack).await?;
                return Ok(false);
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

    async fn handle_settings_overlay_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.state.settings_overlay.is_none() {
            return Ok(false);
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.state.settings_overlay = None;
                self.state.status = "settings closed".to_owned();
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                if let Some(settings_overlay) = &mut self.state.settings_overlay {
                    settings_overlay.selected = settings_overlay
                        .selected
                        .saturating_add(1)
                        .min(SettingsItem::ALL.len().saturating_sub(1));
                }
                self.state.status = "choose setting".to_owned();
            }
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                if let Some(settings_overlay) = &mut self.state.settings_overlay {
                    settings_overlay.selected = settings_overlay.selected.saturating_sub(1);
                }
                self.state.status = "choose setting".to_owned();
            }
            KeyCode::Home => {
                if let Some(settings_overlay) = &mut self.state.settings_overlay {
                    settings_overlay.selected = 0;
                }
                self.state.status = "choose setting".to_owned();
            }
            KeyCode::End => {
                if let Some(settings_overlay) = &mut self.state.settings_overlay {
                    settings_overlay.selected = SettingsItem::ALL.len().saturating_sub(1);
                }
                self.state.status = "choose setting".to_owned();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(selected) = self
                    .state
                    .settings_overlay
                    .as_ref()
                    .map(|overlay| overlay.selected)
                else {
                    return Ok(false);
                };
                let item = SettingsItem::ALL[selected];
                if item == SettingsItem::ArchiveVisibleAccounts {
                    self.toggle_archive_for_current_accounts();
                    return Ok(false);
                }

                let reorganize_chats = item.apply(&mut self.settings);
                self.theme = Theme::from_preset(self.settings.ui_theme);
                self.store.save_app_settings(&self.settings).await?;
                if reorganize_chats {
                    let selection_changed = self.apply_filter();
                    if selection_changed {
                        self.reset_history_window_state();
                    }
                }
                if item == SettingsItem::ConversationStyle {
                    self.clamp_message_scroll();
                }
                if item == SettingsItem::ImagePreviewMode {
                    self.clear_image_protocol_work();
                }
                let value =
                    item.value_text(&self.settings, self.archive_running_for_current_accounts());
                self.state.status = format!("{}: {value}", item.label());
            }
            _ => {}
        }
        Ok(false)
    }

    async fn toggle_image_preview_mode(&mut self) -> Result<()> {
        self.settings.image_preview_mode =
            next_image_preview_mode(self.settings.image_preview_mode);
        self.clear_image_protocol_work();
        self.store.save_app_settings(&self.settings).await?;
        let value = image_preview_mode_label(self.settings.image_preview_mode);
        self.state.status = format!("Image previews: {value}");
        Ok(())
    }
    async fn handle_account_setup_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(setup) = &mut self.state.account_setup else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.state.account_setup = None;
                self.state.status = "account setup cancelled".to_owned();
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                setup.selected_provider = setup
                    .selected_provider
                    .saturating_add(1)
                    .min(AccountProviderKind::ALL.len().saturating_sub(1));
                self.state.status = format!("connect {}", setup.selected_kind().label());
            }
            KeyCode::Up | KeyCode::Left | KeyCode::BackTab => {
                setup.selected_provider = setup.selected_provider.saturating_sub(1);
                self.state.status = format!("connect {}", setup.selected_kind().label());
            }
            KeyCode::Home => {
                setup.selected_provider = 0;
                self.state.status = format!("connect {}", setup.selected_kind().label());
            }
            KeyCode::End => {
                setup.selected_provider = AccountProviderKind::ALL.len().saturating_sub(1);
                self.state.status = format!("connect {}", setup.selected_kind().label());
            }
            KeyCode::Enter => {
                let kind = setup.selected_kind();
                self.start_account_setup(kind).await?;
            }
            _ => {}
        }
        Ok(false)
    }

    async fn handle_account_switcher_key(&mut self, key: KeyEvent) -> Result<bool> {
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
                switcher.confirm_remove = None;
                let selected = switcher.selected;
                Ok(self.apply_account_switcher_selection(selected))
            }
            KeyCode::Delete | KeyCode::Backspace => {
                let (selected, confirmed) = {
                    let Some(switcher) = &self.state.account_switcher else {
                        return Ok(false);
                    };
                    (switcher.selected, switcher.confirm_remove.clone())
                };
                let provider_id = self
                    .account_options()
                    .get(selected)
                    .and_then(|option| option.provider_id.clone());
                let Some(provider_id) = provider_id else {
                    if let Some(switcher) = &mut self.state.account_switcher {
                        switcher.confirm_remove = None;
                    }
                    self.state.status = "select an account to remove".to_owned();
                    return Ok(false);
                };
                if confirmed.as_ref() == Some(&provider_id) {
                    return self.remove_account_by_id(provider_id).await;
                }
                if let Some(switcher) = &mut self.state.account_switcher {
                    switcher.confirm_remove = Some(provider_id.clone());
                }
                self.state.status = format!("press Delete again to remove {provider_id}");
                Ok(false)
            }
            KeyCode::Down => {
                switcher.selected = switcher
                    .selected
                    .saturating_add(1)
                    .min(options_len.saturating_sub(1));
                switcher.confirm_remove = None;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Up => {
                switcher.selected = switcher.selected.saturating_sub(1);
                switcher.confirm_remove = None;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::Home => {
                switcher.selected = 0;
                switcher.confirm_remove = None;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            KeyCode::End => {
                switcher.selected = options_len.saturating_sub(1);
                switcher.confirm_remove = None;
                self.state.status = "choose account filter".to_owned();
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    async fn handle_message_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.attend_selected_chat("message_key");
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

        if self.state.forward_picker.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_forward_picker_click(mouse).await? {
                return Ok(false);
            }
            self.state.forward_picker = None;
            self.state.status = "forward cancelled".to_owned();
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

        if self.state.account_setup.is_some()
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if self.handle_account_setup_click(mouse).await? {
                return Ok(false);
            }
            self.state.account_setup = None;
            self.state.status = "account setup cancelled".to_owned();
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
        let scope_selection_changed = self.sync_filter_scope_to_focus();

        if matches!(
            (pane, mouse.kind),
            (FocusPane::Messages, MouseEventKind::Down(MouseButton::Left))
                | (FocusPane::Messages, MouseEventKind::ScrollDown)
                | (FocusPane::Messages, MouseEventKind::ScrollUp)
                | (FocusPane::Compose, MouseEventKind::Down(MouseButton::Left))
        ) {
            self.attend_selected_chat("chat_mouse");
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
        Ok(changed || scope_selection_changed)
    }

    async fn handle_action_menu_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(menu) = self.state.action_menu.clone() else {
            return Ok(false);
        };
        let Some(index) = self.action_menu_item_at(mouse.column, mouse.row, &menu) else {
            return Ok(false);
        };

        let item = menu.items[index];
        self.state.action_menu = None;
        self.perform_action_menu_item(menu.message_id, item).await?;
        Ok(true)
    }

    async fn handle_forward_picker_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(picker) = self.state.forward_picker.clone() else {
            return Ok(false);
        };
        let Some(index) = self.forward_picker_option_at(mouse.column, mouse.row, &picker) else {
            return Ok(false);
        };
        let Some(target_index) = forward_picker_filtered_indices(&picker).get(index).copied()
        else {
            return Ok(false);
        };
        self.state.forward_picker = None;
        self.forward_message_to_target(picker.message_id, picker.targets[target_index].clone())
            .await?;
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
        if let Some(index) = self.account_switcher_option_at(mouse.column, mouse.row) {
            if let Some(switcher) = &mut self.state.account_switcher {
                switcher.confirm_remove = None;
            }
            return self.apply_account_switcher_selection(index);
        }
        false
    }

    async fn handle_account_setup_click(&mut self, mouse: MouseEvent) -> Result<bool> {
        let Some(index) = self.account_setup_option_at(mouse.column, mouse.row) else {
            return Ok(false);
        };
        if let Some(setup) = &mut self.state.account_setup {
            setup.selected_provider = index;
        }
        let kind = AccountProviderKind::ALL[index];
        self.start_account_setup(kind).await?;
        Ok(true)
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
                if self.open_account_badge_at(mouse.column, mouse.row) {
                    return false;
                }
                if let Some(chat_index) = chat_list::chat_at(
                    &self.state.chats,
                    &self.state.visible_chat_indices,
                    self.state.selected_chat,
                    self.state.pane_areas.chat_list,
                    mouse.column,
                    mouse.row,
                    self.settings.chat_inbox_style,
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
                    if self.state.thread_root.is_none() {
                        self.open_action_menu();
                    }
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

    fn active_filter_scope(&self) -> FilterScope {
        match self.state.focus {
            FocusPane::ChatList => FilterScope::Chats,
            FocusPane::Details if self.state.thread_root.is_some() => FilterScope::Thread,
            FocusPane::Messages | FocusPane::Compose | FocusPane::Details => FilterScope::Messages,
        }
    }

    fn sync_filter_scope_to_focus(&mut self) -> bool {
        if !self.state.filter_mode {
            return false;
        }
        let scope = self.active_filter_scope();
        if scope == self.state.filter_scope {
            return false;
        }
        self.state.filter_scope = scope;
        let selection_changed = self.apply_active_filter();
        self.state.status = self.filter_status();
        selection_changed
    }

    fn chat_filter_active(&self) -> bool {
        self.state.filter_scope == FilterScope::Chats
    }

    fn message_filter_active(&self) -> bool {
        self.state.filter_scope == FilterScope::Messages && !self.state.filter.is_empty()
    }

    fn thread_filter_active(&self) -> bool {
        self.state.filter_scope == FilterScope::Thread
            && self.state.thread_root.is_some()
            && !self.state.filter.is_empty()
    }

    fn chat_list_filter(&self) -> &str {
        if self.chat_filter_active() {
            &self.state.filter
        } else {
            ""
        }
    }

    fn apply_active_filter(&mut self) -> bool {
        let selection_changed = self.apply_filter();
        self.apply_message_filter();
        if self.chat_filter_active() {
            self.schedule_discovery_refresh();
            if selection_changed {
                self.request_selected_chat_history_sync();
            }
        } else {
            self.state.discovery_results.clear();
            self.pending_discovery_query = None;
        }
        if self.thread_filter_active() {
            self.state.details_scroll = self.state.details_scroll.min(self.max_details_scroll());
        }
        selection_changed
    }

    async fn confirm_filter_selection(&mut self) -> Result<bool> {
        match self.state.filter_scope {
            FilterScope::Chats => {
                if self.state.visible_chat_indices.is_empty()
                    && let Some(result) = self.state.discovery_results.first().cloned()
                {
                    self.state.filter_mode = false;
                    return self.open_discovery_result(result).await;
                }
                let load_selected_after_filter = self.state.pending_history_sync_chat.is_some();
                self.state.filter_mode = false;
                self.state.status = self.filter_status();
                Ok(load_selected_after_filter)
            }
            FilterScope::Messages => {
                if self.state.selected_message_id.is_some() {
                    self.open_action_menu();
                } else {
                    self.state.status = self.filter_status();
                }
                Ok(false)
            }
            FilterScope::Thread => {
                self.state.status = self.filter_status();
                Ok(false)
            }
        }
    }

    async fn filter_select_next(&mut self) -> Result<bool> {
        match self.state.filter_scope {
            FilterScope::Chats => Ok(self.select_next_chat()),
            FilterScope::Messages => {
                self.select_next_message();
                Ok(false)
            }
            FilterScope::Thread => {
                self.scroll_details_down(1);
                Ok(false)
            }
        }
    }

    async fn filter_select_previous(&mut self) -> Result<bool> {
        match self.state.filter_scope {
            FilterScope::Chats => Ok(self.select_previous_chat()),
            FilterScope::Messages => {
                self.select_previous_message();
                Ok(false)
            }
            FilterScope::Thread => {
                self.scroll_details_up(1);
                Ok(false)
            }
        }
    }

    fn filter_select_first(&mut self) -> bool {
        match self.state.filter_scope {
            FilterScope::Chats => self.select_first_chat(),
            FilterScope::Messages => {
                self.select_first_message();
                false
            }
            FilterScope::Thread => {
                self.state.details_scroll = 0;
                self.state.status = "details at top".to_owned();
                false
            }
        }
    }

    fn filter_select_last(&mut self) -> bool {
        match self.state.filter_scope {
            FilterScope::Chats => self.select_last_chat(),
            FilterScope::Messages => {
                self.select_last_message();
                false
            }
            FilterScope::Thread => {
                self.state.details_scroll = self.max_details_scroll();
                self.state.status = "details at bottom".to_owned();
                false
            }
        }
    }

    async fn filter_page_down(&mut self) -> Result<bool> {
        match self.state.filter_scope {
            FilterScope::Chats => Ok(self.page_down_chats()),
            FilterScope::Messages => {
                self.scroll_messages_down(self.message_page_step());
                Ok(false)
            }
            FilterScope::Thread => {
                self.scroll_details_down(self.details_page_step());
                Ok(false)
            }
        }
    }

    async fn filter_page_up(&mut self) -> Result<bool> {
        match self.state.filter_scope {
            FilterScope::Chats => Ok(self.page_up_chats()),
            FilterScope::Messages => {
                self.scroll_messages_up(self.message_page_step());
                self.load_older_messages_if_at_top().await?;
                Ok(false)
            }
            FilterScope::Thread => {
                self.scroll_details_up(self.details_page_step());
                Ok(false)
            }
        }
    }

    async fn handle_filter_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Enter => self.confirm_filter_selection().await,
            KeyCode::Esc => {
                self.state.filter_mode = false;
                self.state.status = self.filter_status();
                Ok(false)
            }
            KeyCode::Backspace => {
                self.state.filter.pop();
                let selection_changed = self.apply_active_filter();
                self.state.status = self.filter_status();
                Ok(selection_changed)
            }
            KeyCode::Down => self.filter_select_next().await,
            KeyCode::Up => self.filter_select_previous().await,
            KeyCode::Home => {
                let selection_changed = self.filter_select_first();
                Ok(selection_changed)
            }
            KeyCode::End => {
                let selection_changed = self.filter_select_last();
                Ok(selection_changed)
            }
            KeyCode::PageDown => self.filter_page_down().await,
            KeyCode::PageUp => self.filter_page_up().await,
            KeyCode::Left => {
                self.focus_previous_pane();
                let selection_changed = self.sync_filter_scope_to_focus();
                Ok(selection_changed)
            }
            KeyCode::Right => {
                self.focus_next_pane();
                let selection_changed = self.sync_filter_scope_to_focus();
                Ok(selection_changed)
            }
            KeyCode::Char(value)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.state.filter.push(value);
                let selection_changed = self.apply_active_filter();
                self.state.status = self.filter_status();
                Ok(selection_changed)
            }
            _ => Ok(false),
        }
    }

    async fn handle_compose_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.attend_selected_chat("compose_key");
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
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
            {
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

    fn apply_thread_compose_edit_input(&mut self, input: TextAreaInput) -> bool {
        let modified = self.state.thread_compose.input_without_shortcuts(input);
        self.state.sync_thread_compose_cache();
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

    async fn send_thread_composed_message(&mut self) -> Result<()> {
        let text = self.state.thread_compose_text.trim_end().to_owned();
        if text.trim().is_empty() {
            self.state.status = "type a thread reply before sending".to_owned();
            return Ok(());
        }

        let Some(thread_root) = self.state.thread_root.clone() else {
            self.state.status = "open a thread before replying".to_owned();
            return Ok(());
        };
        let Some(chat) = self.state.selected_chat().cloned() else {
            self.state.status = "select a chat before sending".to_owned();
            return Ok(());
        };
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .cloned()
            .ok_or_else(|| anyhow!("no provider registered for {}", chat.account))?;
        let account = provider.account_info();
        let content = Content::Text(Arc::from(text.as_str()));
        let preview = content_send_preview(&content);
        let message_id = provider
            .send(&chat.id, content.clone(), Some(&thread_root))
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
            reply_to: Some(thread_root.clone()),
            thread_id: Some(thread_root),
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            mentions_me: false,
            platform_data: PlatformData::default(),
        };

        self.store.upsert_message(&message).await?;
        self.update_chat_after_send(&chat, timestamp, &preview)
            .await?;
        self.state.thread_compose = new_thread_compose_textarea();
        self.state.sync_thread_compose_cache();
        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.state.status = "sent thread reply".to_owned();
        Ok(())
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
            .cloned()
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
        if let Some(reason) = provider
            .outbound_capabilities()
            .unsupported_reason(&content)
        {
            if self.state.pending_attachment.is_none()
                && let Some(attachment) = auto_attachment
            {
                let preview = attachment.preview();
                self.state.pending_attachment = Some(attachment);
                self.state.compose = new_compose_textarea();
                self.state.sync_compose_cache();
                self.state.status =
                    format!("{preview} attached, but this account cannot send it yet");
                return Ok(());
            }

            self.state.status = format!("{reason}; attachment kept, press Esc to remove it");
            return Ok(());
        }
        let preview = content_send_preview(&content);
        let effective_reply_to = self.state.reply_to.clone().or_else(|| {
            self.state
                .thread_root
                .clone()
                .filter(|_| self.state.focus == FocusPane::Details)
        });
        let message_id = provider
            .send(&chat.id, content.clone(), effective_reply_to.as_ref())
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
            reply_to: effective_reply_to.clone(),
            thread_id: effective_reply_to,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            mentions_me: false,
            platform_data: PlatformData::default(),
        };

        self.store.upsert_message(&message).await?;
        self.update_chat_after_send(&chat, timestamp, &preview)
            .await?;
        self.state.compose = new_compose_textarea();
        self.state.pending_attachment = None;
        self.state.reply_to = None;
        let sent_in_thread =
            self.state.thread_root.is_some() && self.state.focus == FocusPane::Details;
        self.state.sync_compose_cache();
        self.reload_chats().await?;
        self.reload_selected_messages().await?;
        self.scroll_messages_to_bottom();
        self.state.status = if sent_in_thread {
            "sent thread reply".to_owned()
        } else {
            format!("sent message to {}", chat.name)
        };
        Ok(())
    }

    fn auto_attachment_from_compose_text(
        &mut self,
        text: &str,
    ) -> Result<Option<PendingAttachment>> {
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

    /// Refresh the per-thread unread map for the selected chat from storage.
    /// Runs off the draw path (after message loads, live thread bumps, and
    /// thread-read flushes) so `draw` can render the "N new" badge from cached
    /// state without touching the database.
    async fn refresh_thread_unread_for_selected_chat(&mut self) -> Result<()> {
        let Some((account, chat_id)) = self
            .state
            .selected_chat()
            .map(|chat| (chat.account.clone(), chat.id.clone()))
        else {
            self.state.thread_unread.clear();
            return Ok(());
        };
        let summaries = self
            .store
            .thread_summaries_for_chat(&account, &chat_id)
            .await?;
        self.state.thread_unread = summaries
            .into_iter()
            .filter(|summary| summary.unread_reply_count > 0)
            .map(|summary| (summary.root_id, summary.unread_reply_count))
            .collect();
        self.refresh_thread_unread_by_chat().await?;
        Ok(())
    }

    /// Refresh the account-wide per-chat unread thread map that backs the
    /// sidebar `⤷N` marker. Aggregates unread thread summaries by chat id for
    /// every distinct account currently present in the chat list. Runs off the
    /// draw path.
    async fn refresh_thread_unread_by_chat(&mut self) -> Result<()> {
        let accounts: Vec<ProviderId> = {
            let mut seen = HashSet::new();
            self.state
                .chats
                .iter()
                .filter(|chat| seen.insert(chat.account.clone()))
                .map(|chat| chat.account.clone())
                .collect()
        };
        let mut by_chat: HashMap<ChatId, u32> = HashMap::new();
        for account in accounts {
            let summaries = self.store.unread_thread_summaries(&account).await?;
            for summary in summaries {
                *by_chat.entry(summary.chat_id).or_insert(0) += summary.unread_reply_count;
            }
        }
        self.state.thread_unread_by_chat = by_chat;
        Ok(())
    }

    /// Build and open the Threads inbox overlay from unread thread summaries
    /// across every loaded account. Runs off the draw path. Closing other
    /// overlays first keeps the modal stack predictable.
    async fn open_threads_inbox(&mut self) -> Result<()> {
        let accounts: Vec<ProviderId> = {
            let mut seen = HashSet::new();
            self.state
                .chats
                .iter()
                .filter(|chat| seen.insert(chat.account.clone()))
                .map(|chat| chat.account.clone())
                .collect()
        };
        let mut entries: Vec<ThreadInboxEntry> = Vec::new();
        for account in accounts {
            let summaries = self.store.unread_thread_summaries(&account).await?;
            for summary in summaries {
                let chat_name = self
                    .state
                    .chats
                    .iter()
                    .find(|chat| chat.account == summary.account && chat.id == summary.chat_id)
                    .map(|chat| chat.name.to_string())
                    .unwrap_or_else(|| short_id(&summary.chat_id));
                let preview = summary
                    .root_preview
                    .as_deref()
                    .map(message_list::slack_emoji_shortcodes_to_display)
                    .unwrap_or_else(|| "(no preview)".to_owned());
                entries.push(ThreadInboxEntry {
                    account: summary.account,
                    chat_id: summary.chat_id,
                    root_id: summary.root_id,
                    chat_name,
                    preview,
                    unread_reply_count: summary.unread_reply_count,
                    reply_count: summary.reply_count,
                    last_reply_at: summary.last_reply_at,
                });
            }
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_reply_at));
        let unread_threads = entries.len();
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.account_switcher = None;
        self.state.threads_inbox = Some(ThreadsInbox {
            entries,
            selected: 0,
        });
        self.state.status = if unread_threads == 0 {
            "Threads: all caught up".to_owned()
        } else if unread_threads == 1 {
            "Threads: 1 thread with new replies".to_owned()
        } else {
            format!("Threads: {unread_threads} threads with new replies")
        };
        Ok(())
    }

    /// Handle keys while the Threads inbox overlay is open. Reuses the same
    /// navigation as the rest of the app: arrows move, Enter opens the selected
    /// thread, Esc closes. Returns whether a navigation load should be
    /// scheduled.
    async fn handle_threads_inbox_key(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(inbox) = self.state.threads_inbox.as_mut() else {
            return Ok(false);
        };
        match key.code {
            KeyCode::Esc => {
                self.state.threads_inbox = None;
                self.state.status = "Threads closed".to_owned();
                Ok(false)
            }
            KeyCode::Down => {
                if !inbox.entries.is_empty() {
                    inbox.selected = (inbox.selected + 1).min(inbox.entries.len() - 1);
                }
                Ok(false)
            }
            KeyCode::Up => {
                inbox.selected = inbox.selected.saturating_sub(1);
                Ok(false)
            }
            KeyCode::Home => {
                inbox.selected = 0;
                Ok(false)
            }
            KeyCode::End => {
                if !inbox.entries.is_empty() {
                    inbox.selected = inbox.entries.len() - 1;
                }
                Ok(false)
            }
            KeyCode::Enter => Ok(self.activate_thread_inbox_entry()),
            _ => Ok(false),
        }
    }

    /// Open the thread for the currently selected Threads-inbox entry. Selects
    /// the parent chat (scheduling its message load when needed) and queues the
    /// thread to open once messages are available. Returns whether a navigation
    /// load should be scheduled.
    fn activate_thread_inbox_entry(&mut self) -> bool {
        let Some(inbox) = self.state.threads_inbox.take() else {
            return false;
        };
        let Some(entry) = inbox.entries.get(inbox.selected).cloned() else {
            return false;
        };
        let Some(chat_index) = self
            .state
            .chats
            .iter()
            .position(|chat| chat.account == entry.account && chat.id == entry.chat_id)
        else {
            self.state.status = "thread's chat is no longer available".to_owned();
            return false;
        };
        let changed = self.activate_chat_index(chat_index);
        self.state.pending_thread_open = Some(entry.root_id);
        // When the chat was already loaded, open the thread immediately;
        // otherwise the message-load drain will open it once messages arrive.
        let opened_now = self.try_open_pending_thread();
        changed && !opened_now
    }

    /// Open a queued thread if its root message is already loaded in the
    /// current chat. No-op when nothing is queued or the message is not yet
    /// available. Returns whether a thread was opened.
    fn try_open_pending_thread(&mut self) -> bool {
        let Some(root_id) = self.state.pending_thread_open.clone() else {
            return false;
        };
        if self.message_by_id(&root_id).is_none() {
            return false;
        }
        self.state.pending_thread_open = None;
        self.open_thread(root_id);
        true
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

    async fn enter_filter_mode(&mut self) -> Result<bool> {
        self.state.filter_scope = self.active_filter_scope();
        self.state.filter_mode = true;
        let selection_changed = self.apply_active_filter();
        self.state.status = match self.state.filter_scope {
            FilterScope::Chats => "type to find chats, contacts, or channels; arrows/click still select; Enter opens; Esc finishes".to_owned(),
            FilterScope::Messages => "type to filter messages in this chat; arrows/click still select; Enter opens actions; Esc finishes".to_owned(),
            FilterScope::Thread => "type to filter this thread; arrows or mouse still scroll/select; Esc finishes".to_owned(),
        };
        Ok(selection_changed)
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

        if self.state.forward_picker.is_some() {
            self.state.forward_picker = None;
            self.state.status = "forward cancelled".to_owned();
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
            self.attend_selected_chat("activate_selected_chat");
            self.request_selected_chat_history_sync();
            self.state.status = format!("opened {chat_name}");
        }
        let mark_read_after_load = self.state.focus == FocusPane::Messages || had_unread;
        had_unread || should_sync_if_empty || mark_read_after_load
    }

    fn activate_chat_index(&mut self, chat_index: usize) -> bool {
        let changed = chat_index != self.state.selected_chat;
        let should_sync_if_empty = self.state.messages.is_empty();
        self.state.selected_chat = chat_index;
        self.state.focus = FocusPane::Messages;
        self.attend_selected_chat("activate_chat_index");
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
        self.state.discovery_results.clear();
        self.state.filtered_messages.clear();
        self.pending_discovery_query = None;
        self.clear_message_layout_cache();
        let selection_changed = self.apply_filter();
        self.state.status = "filter cleared".to_owned();
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
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            self.settings.chat_inbox_style,
        )
    }

    fn select_visible_position(&mut self, position: usize) -> bool {
        let ordered_chat_indices = chat_list::ordered_chat_indices(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.settings.chat_inbox_style,
        );
        let Some(&chat_index) = ordered_chat_indices.get(position) else {
            return false;
        };
        let changed = chat_index != self.state.selected_chat;
        self.state.selected_chat = chat_index;
        if changed {
            self.attend_selected_chat("select_chat");
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
            self.request_selected_chat_history_sync();
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

    fn request_selected_chat_members(&mut self) {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return;
        };
        if chat.platform != Platform::Slack || matches!(chat.kind, ChatKind::Direct) {
            return;
        }
        let key = (chat.account.clone(), chat.id.clone());
        if self.state.chat_members.contains_key(&key)
            || !self.state.loading_chat_members.insert(key.clone())
        {
            return;
        }
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .cloned()
        else {
            return;
        };
        let tx = self.chat_members_tx.clone();
        tokio::spawn(async move {
            let result = provider
                .chat_members(&chat.id)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(ChatMembersFetchResult {
                account: key.0,
                chat_id: key.1,
                result,
            });
        });
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
            .cloned()
            && let Some(message) = self.state.messages.last().cloned()
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

    async fn mark_slack_live_message_unread_if_needed(&mut self, message: &Message) -> Result<()> {
        if message.account.is_empty()
            || message.chat_id.is_empty()
            || message.is_from_me
            || message.platform_data.slack.is_none()
            || self.is_chat_currently_attended(&message.account, &message.chat_id)
        {
            return Ok(());
        }

        let Some(chat) = self
            .state
            .chats
            .iter_mut()
            .find(|chat| chat.account == message.account && chat.id == message.chat_id)
        else {
            return Ok(());
        };

        chat.unread_count = chat.unread_count.saturating_add(1);
        let updated = chat.clone();
        self.store.upsert_chat(&updated).await?;
        self.sort_chats_preserving_selection();
        self.apply_filter();
        Ok(())
    }

    fn is_chat_currently_attended(&self, account: &ProviderId, chat_id: &ChatId) -> bool {
        matches!(
            self.state.focus,
            FocusPane::Messages | FocusPane::Compose | FocusPane::Details
        ) && self
            .state
            .selected_chat()
            .is_some_and(|chat| chat.account == *account && chat.id == *chat_id)
    }

    /// True when the user currently has the thread pane open on this thread, in
    /// which case live replies should not be counted as unread.
    fn is_thread_currently_attended(&self, thread_root: &MessageId) -> bool {
        self.state.focus == FocusPane::Details
            && self.state.thread_root.as_ref() == Some(thread_root)
    }

    /// Increment the per-thread unread counter for a live (non-historical)
    /// thread reply, unless the reply is the user's own or the thread is already
    /// open. Mirrors `mark_slack_live_message_unread_if_needed` but tracks the
    /// thread separately so the sidebar, summary line, and Threads inbox can
    /// surface unread replies distinctly from channel activity.
    async fn mark_live_thread_reply_unread_if_needed(&mut self, message: &Message) -> Result<()> {
        if message.is_from_me {
            return Ok(());
        }
        let Some(root) = thread_root_of(message) else {
            return Ok(());
        };
        if self.is_thread_currently_attended(&root) {
            return Ok(());
        }
        let unread = self
            .store
            .bump_thread_unread(&message.account, &root)
            .await?;
        self.log_perf_marker(
            "thread.unread.bump",
            format!(
                "account={} chat={} thread={} unread={}",
                message.account, message.chat_id, root, unread
            ),
        );
        if self
            .state
            .selected_chat()
            .is_some_and(|chat| chat.account == message.account && chat.id == message.chat_id)
        {
            self.state.thread_unread.insert(root, unread);
        }
        *self
            .state
            .thread_unread_by_chat
            .entry(message.chat_id.clone())
            .or_insert(0) += 1;
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
        self.apply_message_filter();
        self.clear_message_layout_cache();
        self.clamp_message_scroll();
    }

    fn scroll_messages_down(&mut self, amount: usize) {
        let max_scroll = self.max_message_scroll();
        self.state.message_scroll = self
            .state
            .message_scroll
            .saturating_add(amount)
            .min(max_scroll);
        self.state.status = if self.state.message_scroll == max_scroll {
            "showing latest messages".to_owned()
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
            self.settings.chat_inbox_style,
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

    fn open_account_badge_at(&mut self, column: u16, row: u16) -> bool {
        let Some(chat_index) = chat_list::account_badge_chat_at(
            &self.state.chats,
            &self.state.visible_chat_indices,
            self.state.selected_chat,
            self.state.pane_areas.chat_list,
            column,
            row,
            self.settings.chat_inbox_style,
        ) else {
            return false;
        };
        let Some(provider_id) = self
            .state
            .chats
            .get(chat_index)
            .map(|chat| chat.account.clone())
        else {
            return false;
        };
        let Some(account) = self.account_for_provider(&provider_id) else {
            return false;
        };
        let Some(path) = self.account_badge_avatar_path(&provider_id, &account) else {
            self.log_perf_marker(
                "slack.account_badge.missing_icon_url",
                format!(
                    "provider={} display_name={} platform={:?}",
                    provider_id, account.display_name, account.platform
                ),
            );
            self.state.status =
                format!("{} has no workspace icon URL loaded", account.display_name);
            return true;
        };
        if !path.exists() {
            self.log_perf_marker(
                "slack.account_badge.cache_missing",
                format!(
                    "provider={} display_name={} path={}",
                    provider_id,
                    account.display_name,
                    path.display()
                ),
            );
            self.state.status = format!(
                "{} workspace icon is still downloading or failed to cache: {}",
                account.display_name,
                path.display()
            );
            return true;
        }
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.status = format!("viewing account icon {}", path.display());
        self.state.image_viewer = Some(ImageViewer { path });
        true
    }

    fn open_media_at(&mut self, column: u16, row: u16) -> bool {
        let content_area = inner_area(self.state.pane_areas.messages);
        if !rect_contains(content_area, column, row) {
            return false;
        }

        let Some(clicked_line) = self.clicked_message_line(content_area, row) else {
            return false;
        };
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

        let Some(clicked_line) = self.clicked_message_line(content_area, row) else {
            return false;
        };
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

        let Some(clicked_line) = self.clicked_message_line(content_area, row) else {
            return false;
        };
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

        let clicked_thread_summary = hit.thread_summary_hit.as_ref().is_some_and(|line_hit| {
            line_hit.line == clicked_line
                && clicked_column_in_hit(content_area, column, line_hit.start_col, line_hit.end_col)
        });
        self.state.selected_message_id = Some(hit.message_id.clone());
        self.state.action_menu = None;
        self.state.forward_picker = None;
        self.state.reaction_picker = None;
        self.state.thread_root = None;
        if clicked_thread_summary {
            self.open_thread(hit.message_id);
        } else {
            self.state.status = format!("selected message {}", short_id(&hit.message_id));
        }
        self.ensure_selected_message_visible();
        true
    }

    fn clicked_message_line(&self, content_area: Rect, row: u16) -> Option<usize> {
        let offset = row.saturating_sub(content_area.y) as usize;
        let adjusted_offset = offset.checked_sub(self.state.message_top_padding)?;
        Some(self.state.message_scroll.saturating_add(adjusted_offset))
    }

    fn visible_timeline_message_indices(&self) -> Vec<usize> {
        self.message_source()
            .iter()
            .enumerate()
            .filter_map(|(index, message)| (!is_slack_thread_reply(message)).then_some(index))
            .collect()
    }

    fn selected_visible_message_position(&self) -> Option<usize> {
        let selected = self.state.selected_message_id.as_ref()?;
        self.visible_timeline_message_indices()
            .iter()
            .position(|index| self.message_source()[*index].id == *selected)
    }

    fn message_source(&self) -> &[Message] {
        if self.message_filter_active() {
            &self.state.filtered_messages
        } else {
            &self.state.messages
        }
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
        let visible_indices = self.visible_timeline_message_indices();
        let Some(index) = visible_indices.first().copied() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(self.message_source()[index].id.clone());
        self.state.message_scroll = 0;
        self.state.status = "selected first message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_last_message(&mut self) {
        let visible_indices = self.visible_timeline_message_indices();
        let Some(index) = visible_indices.last().copied() else {
            self.state.status = "no messages to select".to_owned();
            return;
        };
        self.state.selected_message_id = Some(self.message_source()[index].id.clone());
        self.scroll_messages_to_bottom();
        self.state.status = "selected latest message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_next_message(&mut self) {
        let visible_indices = self.visible_timeline_message_indices();
        if visible_indices.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let next_position = self
            .selected_visible_message_position()
            .map(|position| position.saturating_add(1).min(visible_indices.len() - 1))
            .unwrap_or(0);
        let next = visible_indices[next_position];
        self.state.selected_message_id = Some(self.message_source()[next].id.clone());
        self.state.status = "selected next message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn select_previous_message(&mut self) {
        let visible_indices = self.visible_timeline_message_indices();
        if visible_indices.is_empty() {
            self.state.status = "no messages to select".to_owned();
            return;
        }

        let previous_position = self
            .selected_visible_message_position()
            .map(|position| position.saturating_sub(1))
            .unwrap_or_else(|| visible_indices.len().saturating_sub(1));
        let previous = visible_indices[previous_position];
        self.state.selected_message_id = Some(self.message_source()[previous].id.clone());
        self.state.status = "selected previous message".to_owned();
        self.ensure_selected_message_visible();
    }

    fn ensure_filtered_message_selection(&mut self) {
        if !self.message_filter_active() {
            return;
        }
        if self.state.filtered_messages.is_empty() {
            self.state.selected_message_id = None;
            return;
        }
        if let Some(selected) = self.state.selected_message_id.as_ref()
            && self
                .state
                .filtered_messages
                .iter()
                .any(|message| message.id == *selected)
        {
            return;
        }
        self.state.selected_message_id = self
            .state
            .filtered_messages
            .first()
            .map(|message| message.id.clone());
        self.state.message_scroll = 0;
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
            let Some(message) = self.message_by_id(&message_id) else {
                self.state.status = "selected message was not found".to_owned();
                return;
            };
            let items =
                action_menu_items_for_message(message, self.thread_reply_count(&message_id));
            self.state.reaction_picker = None;
            self.state.poll_vote_picker = None;
            self.state.forward_picker = None;
            self.state.action_menu = Some(ActionMenu {
                message_id,
                selected: 0,
                items,
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
        // Queue clearing this thread's unread counter; the async flush runs
        // after event handling so this sync path stays responsive.
        if let Some(account) = self
            .state
            .selected_chat()
            .map(|chat| chat.account.clone())
            .or_else(|| {
                self.message_by_id(&message_id)
                    .map(|message| message.account.clone())
            })
        {
            self.state.pending_thread_read = Some((account, message_id.clone()));
        }
        self.state.thread_open_unread = self
            .state
            .thread_unread
            .get(&message_id)
            .copied()
            .unwrap_or(0);
        self.state.thread_root = Some(message_id);
        self.state.focus = FocusPane::Details;
        // Land on the newest replies (native thread behaviour); the draw path
        // clamps this to the real maximum once the pane is measured.
        self.state.details_scroll = usize::MAX;
        self.state.status = if reply_count == 0 {
            "thread opened; no replies yet".to_owned()
        } else if reply_count == 1 {
            "thread opened with 1 reply".to_owned()
        } else {
            format!("thread opened with {reply_count} replies")
        };
    }

    /// Clear the unread counter for a thread that was just opened. No-op when
    /// nothing is pending. Mirrors how chat unread is cleared on read.
    async fn flush_pending_thread_read(&mut self) -> Result<()> {
        let Some((account, thread_root)) = self.state.pending_thread_read.take() else {
            return Ok(());
        };
        let last_reply_id = self
            .thread_replies(&thread_root)
            .last()
            .map(|message| message.id.clone());
        self.store
            .mark_thread_read(
                &account,
                &thread_root,
                last_reply_id.as_ref(),
                Some(Utc::now()),
            )
            .await?;
        self.log_perf_marker(
            "thread.unread.clear",
            format!("account={account} thread={thread_root}"),
        );
        self.state.thread_unread.remove(&thread_root);
        // Recompute the sidebar aggregate from storage so it always matches the
        // authoritative per-thread counters after a read.
        self.refresh_thread_unread_by_chat().await?;
        Ok(())
    }

    fn open_forward_picker(&mut self, message_id: MessageId) {
        let Some(message) = self.message_by_id(&message_id).cloned() else {
            self.state.status = "selected message was not found".to_owned();
            return;
        };
        let targets = self.forward_targets_for_message(&message);
        if targets.is_empty() {
            self.state.status = "no compatible forward destinations".to_owned();
            return;
        }
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.poll_vote_picker = None;
        self.state.forward_picker = Some(ForwardPicker {
            message_id,
            selected: 0,
            query: String::new(),
            targets,
        });
        self.state.status = "choose where to forward".to_owned();
    }

    async fn forward_message_to_target(
        &mut self,
        message_id: MessageId,
        target: ForwardTarget,
    ) -> Result<()> {
        let Some(source) = self.message_by_id(&message_id).cloned() else {
            self.state.status = "selected message was not found".to_owned();
            return Ok(());
        };
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == target.account.as_ref())
            .cloned()
        else {
            self.state.status = format!("no provider registered for {}", target.account);
            return Ok(());
        };
        let Some(content) =
            forward_content_for_capabilities(&source.content, &provider.outbound_capabilities())
        else {
            self.state.status = format!("{} cannot send this forwarded content", target.label);
            return Ok(());
        };
        let preview = content_send_preview(&content);
        let account = provider.account_info();
        let sent_id = provider
            .send(&target.chat_id, content.clone(), None)
            .await?;
        let timestamp = Utc::now();
        let message = Message {
            id: sent_id,
            chat_id: target.chat_id.clone(),
            account: target.account.clone(),
            sender: Sender {
                platform_id: Arc::from("me"),
                display_name: account.display_name,
                avatar: account.avatar,
            },
            timestamp,
            edited_at: None,
            content,
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: true,
            mentions_me: false,
            platform_data: PlatformData::default(),
        };
        self.store.upsert_message(&message).await?;
        if let Some(chat) = self
            .state
            .chats
            .iter()
            .find(|chat| chat.account == target.account && chat.id == target.chat_id)
            .cloned()
        {
            self.update_chat_after_send(&chat, timestamp, &preview)
                .await?;
        }
        self.reload_chats().await?;
        if self
            .state
            .selected_chat()
            .is_some_and(|chat| chat.account == target.account && chat.id == target.chat_id)
        {
            self.reload_selected_messages().await?;
            self.scroll_messages_to_bottom();
        }
        self.state.status = format!("forwarded to {}", target.label);
        Ok(())
    }

    fn forward_targets_for_message(&self, message: &Message) -> Vec<ForwardTarget> {
        self.state
            .chats
            .iter()
            .filter_map(|chat| {
                let provider = self.provider_for_id(&chat.account)?;
                let capabilities = provider.outbound_capabilities();
                forward_content_for_capabilities(&message.content, &capabilities)?;
                let account_label = self
                    .state
                    .account_statuses
                    .get(&chat.account)
                    .map(|status| status.display_name.as_str())
                    .unwrap_or(chat.account.as_ref());
                Some(ForwardTarget {
                    account: chat.account.clone(),
                    chat_id: chat.id.clone(),
                    label: chat.name.to_string(),
                    subtitle: format!("{account_label} · {}", platform_label(&chat.platform)),
                })
            })
            .collect()
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
            .cloned()
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
        self.clear_message_layout_cache();
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
            .cloned()
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
        self.clear_message_layout_cache();
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

    fn open_message_link(&mut self, message_id: &MessageId) -> bool {
        let Some(message) = self.message_by_id(message_id) else {
            return false;
        };
        let Some(url) = first_content_url(&message.content) else {
            return false;
        };
        open_url(&url);
        self.state.status = format!("opened link {url}");
        true
    }

    fn record_network_activity(
        &mut self,
        provider_id: &ProviderId,
        direction: NetworkActivityDirection,
    ) {
        self.state
            .network_activity
            .entry(provider_id.clone())
            .or_default()
            .record(direction, Utc::now());
    }

    fn prune_ephemeral_activity(&mut self) {
        let now = Utc::now();
        self.state
            .network_activity
            .values_mut()
            .for_each(|activity| activity.prune(now));
        self.state.typing_indicators.retain(|_, indicators| {
            indicators.retain(|indicator| indicator.expires_at > now);
            !indicators.is_empty()
        });
    }

    fn update_typing_indicator(
        &mut self,
        provider_id: &ProviderId,
        chat_id: ChatId,
        sender: PlatformId,
        is_typing: bool,
    ) {
        let key = (provider_id.clone(), chat_id);
        let sender_name = self
            .sender_display_name(provider_id, &key.1, &sender)
            .unwrap_or_else(|| short_id(&sender).to_owned());
        if is_typing {
            let indicators = self.state.typing_indicators.entry(key).or_default();
            if let Some(indicator) = indicators
                .iter_mut()
                .find(|indicator| indicator.sender == sender)
            {
                indicator.display_name = sender_name;
                indicator.expires_at =
                    Utc::now() + ChronoDuration::seconds(TYPING_INDICATOR_TTL_SECS);
            } else {
                indicators.push(TypingIndicator {
                    sender,
                    display_name: sender_name,
                    expires_at: Utc::now() + ChronoDuration::seconds(TYPING_INDICATOR_TTL_SECS),
                });
            }
        } else if let Some(indicators) = self.state.typing_indicators.get_mut(&key) {
            indicators.retain(|indicator| indicator.sender != sender);
            if indicators.is_empty() {
                self.state.typing_indicators.remove(&key);
            }
        }
    }

    fn sender_display_name(
        &self,
        provider_id: &ProviderId,
        chat_id: &ChatId,
        sender: &PlatformId,
    ) -> Option<String> {
        self.state
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.account == *provider_id
                    && message.chat_id == *chat_id
                    && message.sender.platform_id == *sender
            })
            .map(|message| message.sender.display_name.to_string())
            .or_else(|| {
                self.state
                    .chat_members
                    .get(&(provider_id.clone(), chat_id.clone()))
                    .and_then(|members| {
                        members
                            .iter()
                            .find(|member| member.platform_id == *sender)
                            .map(|member| member.display_name.to_string())
                    })
            })
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
                avatar: None,
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

    fn provider_index_for_id(&self, provider_id: &ProviderId) -> Option<usize> {
        self.providers
            .iter()
            .position(|provider| provider.id() == provider_id)
    }

    async fn submit_current_slack_setup(&mut self) -> Result<()> {
        let Some(setup) = self.state.slack_setup.clone() else {
            return Ok(());
        };
        let selected_mode = setup.selected_mode().to_auth_submission_mode();
        let submission = AuthSubmission {
            workspace_label: trimmed_option(&setup.workspace_label),
            mode: Some(selected_mode.clone()),
            client_id: trimmed_option(&setup.credentials.client_id),
            client_secret: trimmed_option(&setup.credentials.client_secret),
            redirect_uri: trimmed_option(&setup.credentials.redirect_uri),
            oauth_code: None,
            user_token: trimmed_option(&setup.credentials.user_token),
            bot_token: trimmed_option(&setup.credentials.bot_token),
            app_token: trimmed_option(&setup.credentials.app_token),
            webhook_url: trimmed_option(&setup.credentials.webhook_url),
        };

        // Browser OAuth login (no pasted token) blocks on a loopback redirect
        // that can take a while, so dispatch it as a background task to keep the
        // event loop responsive. The provider emits an `OAuthUrl` challenge to
        // open the browser and `AuthSucceeded`/`SyncComplete`/`Disconnected`
        // events drive the UI; `pending_slack_setup_load` defers the post-auth
        // chat load until that sync completes.
        //
        // The browser login needs client credentials. They can come from the
        // user (manual app entry) or from an official/bundled app the provider
        // exposes via `has_bundled_oauth_app()` — when the latter is configured,
        // the normal connect path needs no client ID/secret at all.
        let provider_index = self.provider_index_for_id(&setup.provider_id);
        let has_bundled_oauth_app = provider_index
            .map(|index| self.providers[index].has_bundled_oauth_app())
            .unwrap_or(false);
        let has_client_credentials =
            submission.client_id.is_some() && submission.client_secret.is_some();
        let is_browser_oauth_login = matches!(
            selected_mode,
            AuthSubmissionMode::UserOAuth | AuthSubmissionMode::ReadOnlyOAuth
        ) && (has_client_credentials || has_bundled_oauth_app)
            && submission.user_token.is_none()
            && submission.bot_token.is_none();
        if is_browser_oauth_login && let Some(provider_index) = provider_index {
            let provider = Arc::clone(&self.providers[provider_index]);
            self.state.pending_slack_setup_load = Some(setup.provider_id.clone());
            tokio::spawn(async move {
                let _ = provider.submit_auth(submission).await;
            });
            if let Some(current_setup) = &mut self.state.slack_setup
                && current_setup.provider_id == setup.provider_id
            {
                current_setup.status = Some(
                    "Opening your browser to sign in to Slack; chat-cli will finish automatically."
                        .to_owned(),
                );
            }
            self.state.status = format!(
                "waiting for Slack browser sign-in for {}",
                setup.provider_id
            );
            return Ok(());
        }

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
                self.load_slack_setup_account(&setup.provider_id).await?;
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

    /// Load chats and select the first conversation for a Slack account whose
    /// authentication just succeeded. Shared by the inline submission path and
    /// the background browser-OAuth path (driven by `SyncComplete`).
    async fn load_slack_setup_account(&mut self, provider_id: &ProviderId) -> Result<()> {
        if let Some(provider_index) = self.provider_index_for_id(provider_id) {
            let provider = self.providers[provider_index].as_ref();
            let account = provider.account_info();
            let config_json = provider.config_json().unwrap_or_else(|| "{}".to_owned());
            self.store.upsert_account(&account, &config_json).await?;
            self.state.account_statuses.insert(
                account.id.clone(),
                AccountStatus::new(&account, AccountConnection::Online),
            );
            self.sync_provider_chats(provider_index).await?;
            self.reload_chats().await?;
            self.state.active_account = Some(account.id.clone());
            let selection_changed = self.apply_filter();
            if selection_changed {
                self.reset_history_window_state();
            }
            self.request_selected_chat_history_sync();
            self.reload_selected_messages_after_navigation().await?;
            self.state.pending_scroll_to_latest = true;
        } else {
            self.set_account_status(provider_id, AccountConnection::Online, None);
        }
        Ok(())
    }

    fn account_platform(&self, provider_id: &ProviderId) -> Option<Platform> {
        self.account_for_provider(provider_id)
            .map(|account| account.platform)
    }

    fn open_slack_setup_for_account(&mut self, account: &Account, status: Option<String>) {
        let workspace_label = slack_setup_workspace_seed(&account.display_name);
        let mut overlay = SlackSetupOverlay::new(account.id.clone(), workspace_label);
        let (bundled_oauth_app, configured_realtime) = self
            .provider_for_id(&account.id)
            .map(|provider| {
                (
                    provider.has_bundled_oauth_app(),
                    provider.has_configured_realtime(),
                )
            })
            .unwrap_or((false, false));
        overlay.bundled_oauth_app = bundled_oauth_app;
        overlay.configured_realtime = configured_realtime;
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
            .map(|account| slack_setup_workspace_seed(&account.display_name))
            .unwrap_or_else(|| provider_id.to_string());
        let mut overlay = SlackSetupOverlay::new(provider_id.clone(), workspace_label);
        let (bundled_oauth_app, configured_realtime) = self
            .provider_for_id(provider_id)
            .map(|provider| {
                (
                    provider.has_bundled_oauth_app(),
                    provider.has_configured_realtime(),
                )
            })
            .unwrap_or((false, false));
        overlay.bundled_oauth_app = bundled_oauth_app;
        overlay.configured_realtime = configured_realtime;
        if let Some(challenge) = challenge {
            match challenge {
                AuthChallenge::OAuthUrl(url) => {
                    overlay.phase = SlackSetupPhase::OAuthPrompt;
                    if overlay.bundled_oauth_app {
                        overlay.selected_mode = 0; // Automatic (built-in Slack app).
                        overlay.oauth_url = None;
                        overlay.status = Some(
                            "Press Enter to open Slack and sign in with the built-in chat-cli app."
                                .to_owned(),
                        );
                    } else {
                        overlay.oauth_url = Some(url.to_string());
                        overlay.status = Some(
                            "Create the Slack app, then enter Client ID and Client Secret here; chat-cli opens your browser to finish sign-in automatically.".to_owned(),
                        );
                        open_and_copy_slack_oauth_url(url.as_ref());
                    }
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

    fn maybe_queue_notification(&mut self, message: &Message, is_historical: bool) {
        if is_historical {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=historical account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }
        if message.is_from_me && !self.settings.notify_self_messages {
            // Messages the user sent (including ones echoed back from another
            // device, e.g. replying on the phone) must never notify the user
            // about their own activity, unless `notify_self_messages` is enabled
            // (a testing aid for exercising the notification pipeline by sending
            // yourself a message from a web client).
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=from_me account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }
        if self.settings.notifications == NotificationMode::Off {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=off account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }
        if self.state.notification_pause.is_paused_at(Utc::now()) {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=paused account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }

        let Some(chat) = self.notification_chat_for(message) else {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=chat_missing account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        };
        if chat.muted {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=muted account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }
        if self.settings.notification_scope == NotificationScope::DirectAndMentions
            && !matches!(chat.kind, ChatKind::Direct)
            && !message.mentions_me
        {
            // Scope restricts notifications to direct messages and mentions:
            // group/channel messages are only eligible when they mention the
            // authenticated user.
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=scope_group_no_mention account={} chat={} message={}",
                    message.account, message.chat_id, message.id
                ),
            );
            return;
        }

        let chat_name = chat.name.to_string();
        let notification = NotificationOverlay::new(chat, message, true);
        let deliver_at = Instant::now() + NOTIFICATION_DELIVERY_DELAY;
        if let Some(pending) = self
            .state
            .pending_notifications
            .iter_mut()
            .find(|pending| pending.key_matches(&message.account, &message.chat_id))
        {
            pending.deliver_at = deliver_at;
            pending.notification = notification;
            self.log_perf_marker(
                "notification.coalesce",
                format!(
                    "account={} chat={} message={} delay_ms={}",
                    message.account,
                    message.chat_id,
                    message.id,
                    NOTIFICATION_DELIVERY_DELAY.as_millis()
                ),
            );
        } else {
            self.state.pending_notifications.push(PendingNotification {
                account: message.account.clone(),
                chat_id: message.chat_id.clone(),
                deliver_at,
                notification,
            });
            self.log_perf_marker(
                "notification.queue",
                format!(
                    "account={} chat={} message={} delay_ms={}",
                    message.account,
                    message.chat_id,
                    message.id,
                    NOTIFICATION_DELIVERY_DELAY.as_millis()
                ),
            );
        }
        self.state.status = format!("notification queued for {chat_name}");
    }

    fn notification_chat_for(&self, message: &Message) -> Option<&Chat> {
        self.state
            .chats
            .iter()
            .find(|chat| chat.id == message.chat_id && chat.account == message.account)
            .or_else(|| {
                self.state
                    .chats
                    .iter()
                    .find(|chat| chat.id == message.chat_id)
            })
    }

    fn selected_chat_key(&self) -> Option<(ProviderId, ChatId)> {
        self.state
            .selected_chat()
            .map(|chat| (chat.account.clone(), chat.id.clone()))
    }

    fn attend_selected_chat(&mut self, reason: &str) {
        let Some((account, chat_id)) = self.selected_chat_key() else {
            return;
        };
        self.cancel_pending_notifications_for_chat(&account, &chat_id, reason);
    }

    fn cancel_pending_notifications_for_chat(
        &mut self,
        account: &ProviderId,
        chat_id: &ChatId,
        reason: &str,
    ) {
        let before = self.state.pending_notifications.len();
        self.state
            .pending_notifications
            .retain(|pending| !pending.key_matches(account, chat_id));
        let cancelled = before.saturating_sub(self.state.pending_notifications.len());
        if cancelled > 0 {
            self.log_perf_marker(
                "notification.cancel",
                format!("reason={reason} account={account} chat={chat_id} count={cancelled}"),
            );
        }
    }

    fn drain_due_pending_notifications(&mut self) {
        let now = Instant::now();
        let mut drained = 0usize;
        while drained < MAX_PENDING_NOTIFICATIONS_PER_TICK {
            let Some(index) = self
                .state
                .pending_notifications
                .iter()
                .position(|pending| pending.deliver_at <= now)
            else {
                break;
            };
            let pending = self.state.pending_notifications.remove(index);
            drained += 1;
            self.deliver_pending_notification(pending);
        }
    }

    fn deliver_pending_notification(&mut self, pending: PendingNotification) {
        if self.settings.notifications == NotificationMode::Off {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=off_at_delivery account={} chat={} message={}",
                    pending.account, pending.chat_id, pending.notification.message_id
                ),
            );
            return;
        }
        if self.state.notification_pause.is_paused_at(Utc::now()) {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=paused_at_delivery account={} chat={} message={}",
                    pending.account, pending.chat_id, pending.notification.message_id
                ),
            );
            return;
        }
        if self
            .state
            .chats
            .iter()
            .find(|chat| pending.key_matches(&chat.account, &chat.id))
            .is_some_and(|chat| chat.muted)
        {
            self.log_perf_marker(
                "notification.suppress",
                format!(
                    "reason=muted_at_delivery account={} chat={} message={}",
                    pending.account, pending.chat_id, pending.notification.message_id
                ),
            );
            return;
        }

        let chat_name = pending.notification.chat_name.clone();
        match self.settings.notifications {
            NotificationMode::Off => {}
            NotificationMode::Desktop => {
                let desktop_notification = pending.notification.to_desktop_notification();
                match self.desktop_notifier.send_message(&desktop_notification) {
                    Ok(()) => {
                        self.state.status = format!("new message in {chat_name}");
                        self.log_perf_marker(
                            "notification.deliver",
                            format!(
                                "mode=desktop account={} chat={} message={}",
                                pending.account, pending.chat_id, pending.notification.message_id
                            ),
                        );
                    }
                    Err(error) => {
                        self.state.status =
                            format!("desktop notification failed for {chat_name}: {error}");
                        self.log_perf_marker(
                            "notification.error",
                            format!(
                                "mode=desktop account={} chat={} message={} error={error}",
                                pending.account, pending.chat_id, pending.notification.message_id
                            ),
                        );
                    }
                }
            }
            NotificationMode::InApp => {
                self.state.notification = Some(pending.notification);
                self.state.status = format!("new message in {chat_name}");
                self.log_perf_marker(
                    "notification.deliver",
                    format!(
                        "mode=in_app account={} chat={}",
                        pending.account, pending.chat_id
                    ),
                );
            }
        }
    }

    fn deliver_account_notice(&mut self, title: &str, body: &str, severity: AccountNoticeSeverity) {
        self.state.status = format!("{title}: {body}");
        let notification = NotificationOverlay::account_notice(title, body);

        // System alerts (for example, realtime messaging unavailable) defeat the
        // core purpose of the app, so they must always raise a desktop/system
        // notification regardless of the user's notification mode, in addition to
        // the in-app overlay.
        if severity == AccountNoticeSeverity::SystemAlert {
            self.state.notification = Some(notification.clone());
            let desktop_notification = notification.to_desktop_notification();
            match self.desktop_notifier.send_message(&desktop_notification) {
                Ok(()) => {
                    self.log_perf_marker(
                        "notification.deliver",
                        format!("mode=system_alert account_notice title={title}"),
                    );
                }
                Err(error) => {
                    self.state.status =
                        format!("{title}: {body} (desktop notification failed: {error})");
                    self.log_perf_marker(
                        "notification.error",
                        format!("mode=system_alert account_notice title={title} error={error:#}"),
                    );
                }
            }
            return;
        }

        match self.settings.notifications {
            NotificationMode::Off | NotificationMode::InApp => {
                self.state.notification = Some(notification);
                self.log_perf_marker(
                    "notification.deliver",
                    format!("mode=in_app account_notice title={title}"),
                );
            }
            NotificationMode::Desktop => {
                let desktop_notification = notification.to_desktop_notification();
                match self.desktop_notifier.send_message(&desktop_notification) {
                    Ok(()) => {
                        self.log_perf_marker(
                            "notification.deliver",
                            format!("mode=desktop account_notice title={title}"),
                        );
                    }
                    Err(error) => {
                        self.state.notification = Some(notification);
                        self.state.status =
                            format!("{title}: {body} (desktop notification failed: {error})");
                        self.log_perf_marker(
                            "notification.error",
                            format!("mode=desktop account_notice title={title} error={error:#}"),
                        );
                    }
                }
            }
        }
    }

    fn dismiss_notification(&mut self) {
        self.state.notification = None;
    }

    async fn handle_tick(&mut self) -> Result<()> {
        self.state.monthly_backfill_tick = self.state.monthly_backfill_tick.wrapping_add(1);
        if self
            .state
            .monthly_backfill_tick
            .is_multiple_of(NOTIFICATION_PAUSE_RELOAD_TICKS)
        {
            match self.store.notification_pause_state().await {
                Ok(state) => self.state.notification_pause = state,
                Err(error) => {
                    self.state.status = format!("failed to reload notification pause: {error}");
                    self.log_perf_marker("notification.pause_reload_error", error.to_string());
                }
            }
        }
        self.drain_due_pending_notifications();
        let metadata_changed = self.drain_link_metadata_fetches();
        let members_changed = self.drain_chat_member_fetches();
        let expired = if let Some(notification) = &mut self.state.notification {
            notification.ticks_remaining = notification.ticks_remaining.saturating_sub(1);
            notification.ticks_remaining == 0
        } else {
            false
        };
        if expired {
            self.state.notification = None;
        }
        if metadata_changed || members_changed {
            self.state.status = "background details updated".to_owned();
        }
        Ok(())
    }

    fn open_account_setup(&mut self) {
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.compose_attach_menu = None;
        self.state.compose_emoticon_picker = None;
        self.state.poll_vote_picker = None;
        self.state.help_overlay = None;
        self.state.auth_overlay = None;
        self.state.account_switcher = None;
        self.state.settings_overlay = None;
        self.state.slack_setup = None;
        self.state.account_setup = Some(AccountSetupOverlay {
            selected_provider: 0,
            status: self.provider_factory.is_none().then(|| {
                "Runtime account setup needs the app provider registry; CLI-configured accounts still work.".to_owned()
            }),
        });
        self.state.status = "connect chat app".to_owned();
    }

    async fn start_account_setup(&mut self, kind: AccountProviderKind) -> Result<()> {
        let Some(provider_factory) = self.provider_factory.clone() else {
            if let Some(setup) = &mut self.state.account_setup {
                setup.status = Some(
                    "Runtime account setup is unavailable in this build; start with CLI flags for now."
                        .to_owned(),
                );
            }
            self.state.status = "account setup unavailable".to_owned();
            return Ok(());
        };

        let provider = match provider_factory(kind) {
            Ok(provider) => provider,
            Err(error) => {
                let detail = error.to_string();
                if let Some(setup) = &mut self.state.account_setup {
                    setup.status =
                        Some(format!("Could not start {} setup: {detail}", kind.label()));
                }
                self.state.status = format!("{} setup failed", kind.label());
                return Ok(());
            }
        };
        let account = provider.account_info();
        match self.add_runtime_provider(provider).await {
            Ok(()) => {
                self.state.account_setup = None;
                self.state.status = format!("started {} setup", account.display_name);
            }
            Err(error) => {
                let detail = error.to_string();
                if let Some(setup) = &mut self.state.account_setup {
                    setup.status =
                        Some(format!("Could not add {}: {detail}", account.display_name));
                }
                self.state.status = format!("{} setup failed", account.display_name);
            }
        }
        Ok(())
    }

    fn open_settings_overlay(&mut self) {
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.state.compose_attach_menu = None;
        self.state.compose_emoticon_picker = None;
        self.state.poll_vote_picker = None;
        self.state.help_overlay = None;
        self.state.account_switcher = None;
        self.state.settings_overlay = Some(SettingsOverlay { selected: 0 });
        self.state.status = "settings".to_owned();
    }

    fn open_account_switcher(&mut self) {
        let selected = self
            .account_options()
            .iter()
            .position(|option| option.provider_id == self.state.active_account)
            .unwrap_or_default();
        self.state.action_menu = None;
        self.state.forward_picker = None;
        self.state.reaction_picker = None;
        self.state.account_switcher = Some(AccountSwitcher {
            selected,
            confirm_remove: None,
        });
        self.state.status = "choose account".to_owned();
    }

    async fn remove_account_by_id(&mut self, provider_id: ProviderId) -> Result<bool> {
        let label = self
            .state
            .account_statuses
            .get(&provider_id)
            .map(|status| status.display_name.clone())
            .unwrap_or_else(|| provider_id.to_string());

        if let Some(index) = self
            .providers
            .iter()
            .position(|provider| provider.id() == &provider_id)
        {
            let provider = self.providers.remove(index);
            let _ = provider.disconnect().await;
        }
        self.provider_receivers
            .retain(|(existing_id, _)| existing_id != &provider_id);
        self.state.chats.retain(|chat| chat.account != provider_id);
        self.state
            .messages
            .retain(|message| message.account != provider_id);
        self.state.account_statuses.remove(&provider_id);
        if self.state.active_account.as_ref() == Some(&provider_id) {
            self.state.active_account = None;
        }
        self.store.remove_account(&provider_id).await?;
        self.state.account_switcher = None;
        self.state.selected_chat = 0;
        self.state.selected_message_id = None;
        self.state.message_scroll = 0;
        self.apply_filter();
        self.reload_selected_messages().await?;
        self.state.status = format!("removed {label}");
        Ok(true)
    }

    fn apply_account_switcher_selection(&mut self, selected: usize) -> bool {
        let Some(option) = self.account_options().get(selected).cloned() else {
            return false;
        };
        if option.kind == AccountOptionKind::AddAccount {
            self.state.account_switcher = None;
            self.open_account_setup();
            return false;
        }
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
            kind: AccountOptionKind::AllAccounts,
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
                kind: AccountOptionKind::Provider,
                provider_id: Some(provider_id),
                label,
                summary,
                chat_count,
            }
        }));
        options.push(AccountOption {
            kind: AccountOptionKind::AddAccount,
            provider_id: None,
            label: "Add account".to_owned(),
            summary: if self.provider_factory.is_some() {
                "Connect Slack, WhatsApp, or demo".to_owned()
            } else {
                "Runtime setup unavailable".to_owned()
            },
            chat_count: 0,
        });
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

    fn account_setup_option_at(&self, column: u16, row: u16) -> Option<usize> {
        let modal = self.account_setup_overlay_rect(self.state.frame_area);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_row = modal.y.saturating_add(4);
        let relative = row.checked_sub(first_option_row)? as usize;
        let index = relative / 2;
        (index < AccountProviderKind::ALL.len()).then_some(index)
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

    fn action_menu_rect(&self, area: Rect, menu: &ActionMenu) -> Rect {
        self.anchored_message_popup_rect(area, &menu.message_id, 34, menu.items.len() as u16 + 4)
    }

    fn forward_picker_rect(&self, area: Rect, picker: &ForwardPicker) -> Rect {
        let target_rows = forward_picker_filtered_len(picker).saturating_mul(2);
        let height = target_rows.saturating_add(5).clamp(7, 18) as u16;
        let width = area.width.saturating_sub(4).clamp(42, 78);
        self.anchored_message_popup_rect(area, &picker.message_id, width, height)
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

    fn account_setup_overlay_rect(&self, area: Rect) -> Rect {
        let width = area
            .width
            .saturating_mul(76)
            .saturating_div(100)
            .clamp(40, 78)
            .min(area.width.saturating_sub(2).max(1));
        let height = (AccountProviderKind::ALL.len() as u16)
            .saturating_mul(2)
            .saturating_add(7)
            .clamp(10, 18)
            .min(area.height.saturating_sub(2).max(1));
        centered_fixed_rect(area, width, height)
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

    fn settings_overlay_rect(&self, area: Rect) -> Rect {
        let width = area
            .width
            .saturating_mul(70)
            .saturating_div(100)
            .clamp(44, 78)
            .min(area.width.saturating_sub(2).max(1));
        let height = 20.min(area.height.saturating_sub(2).max(1));
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
        let modal = self.action_menu_rect(self.state.frame_area, menu);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_item_row = modal.y.saturating_add(2);
        let index = row.checked_sub(first_item_row)? as usize;
        (index < menu.items.len()).then_some(index)
    }

    fn forward_picker_option_at(
        &self,
        column: u16,
        row: u16,
        picker: &ForwardPicker,
    ) -> Option<usize> {
        let modal = self.forward_picker_rect(self.state.frame_area, picker);
        if !rect_contains(modal, column, row) {
            return None;
        }
        let first_option_row = modal.y.saturating_add(3);
        let relative = row.checked_sub(first_option_row)? as usize;
        if !relative.is_multiple_of(2) {
            return None;
        }
        let matches = forward_picker_filtered_indices(picker);
        let visible_rows = forward_picker_visible_rows(modal);
        let start = forward_picker_scroll_start(picker.selected, visible_rows, matches.len());
        let index = start.saturating_add(relative / 2);
        (index < matches.len()).then_some(index)
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

    fn cached_message_line_count(&mut self) -> usize {
        let content_width = self.message_content_width();
        let presentation = self.active_message_presentation();
        let messages = if self.message_filter_active() {
            &self.state.filtered_messages
        } else {
            &self.state.messages
        };
        message_list::cached_message_line_count(
            messages,
            content_width,
            &self.link_metadata_cache,
            self.link_metadata_revision,
            &mut self.message_layout_cache,
            presentation,
        )
    }

    #[cfg(test)]
    fn message_line_count(&self) -> usize {
        message_list::message_line_count_with_presentation(
            &self.state.messages,
            self.message_content_width(),
            &self.link_metadata_cache,
            self.active_message_presentation(),
        )
    }

    fn details_line_count(&self) -> usize {
        if self.thread_filter_active() {
            let matches = self.filtered_thread_matches();
            if matches.is_empty() {
                return 4;
            }
            return 3 + matches
                .iter()
                .map(|message| {
                    let is_root = self
                        .state
                        .thread_root
                        .as_ref()
                        .is_some_and(|root| root.as_ref() == message.id.as_ref());
                    thread_message_card_line_count(
                        message,
                        is_root,
                        self.state.pane_areas.details.width,
                        &self.link_metadata_cache,
                    )
                })
                .sum::<usize>();
        }

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
        self.overview_detail_lines().len()
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
            .map(|message| {
                thread_message_card_line_count(
                    message,
                    true,
                    self.state.pane_areas.details.width,
                    &self.link_metadata_cache,
                )
            })
            .unwrap_or(2);
        let replies = self.thread_replies(thread_root);
        let reply_rows = if replies.is_empty() {
            1
        } else {
            replies
                .iter()
                .map(|message| {
                    thread_message_card_line_count(
                        message,
                        false,
                        self.state.pane_areas.details.width,
                        &self.link_metadata_cache,
                    )
                })
                .sum()
        };
        3 + root_rows + reply_rows
    }

    fn message_content_width(&self) -> u16 {
        self.state
            .pane_areas
            .messages
            .width
            .saturating_sub(2)
            .max(1)
    }

    fn max_message_scroll(&mut self) -> usize {
        let viewport_rows = inner_area(self.state.pane_areas.messages).height as usize;
        let total_lines = self.cached_message_line_count();
        bounded_message_scroll(total_lines, viewport_rows)
    }

    fn max_details_scroll(&self) -> usize {
        let viewport_rows = if self.state.thread_root.is_some() {
            let compose_height = self.thread_compose_height(self.state.pane_areas.details);
            inner_area(self.state.pane_areas.details)
                .height
                .saturating_sub(3)
                .saturating_sub(compose_height) as usize
        } else {
            inner_area(self.state.pane_areas.details).height as usize
        };
        bounded_message_scroll(self.details_line_count(), viewport_rows.max(1))
    }

    fn apply_message_filter(&mut self) {
        self.state.filtered_messages = if self.message_filter_active() {
            self.state
                .messages
                .iter()
                .filter(|message| message_matches_filter(message, &self.state.filter))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        self.ensure_filtered_message_selection();
        self.clear_message_layout_cache();
    }

    fn apply_filter(&mut self) -> bool {
        let previous_selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        let chat_filter = if self.chat_filter_active() {
            self.state.filter.as_str()
        } else {
            ""
        };
        self.state.visible_chat_indices =
            chat_list::filter_chat_indices(&self.state.chats, chat_filter)
                .into_iter()
                .filter(|index| self.chat_is_visible_in_sidebar(*index))
                .filter(|index| {
                    self.state.active_account.as_ref().is_none_or(|active| {
                        self.state
                            .chats
                            .get(*index)
                            .is_some_and(|chat| chat.account == *active)
                    })
                })
                .collect();
        self.state.visible_chat_indices.sort_by(|a, b| {
            compare_chats_for_sidebar(&self.state.chats[*a], &self.state.chats[*b])
        });

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

    fn chat_is_visible_in_sidebar(&self, chat_index: usize) -> bool {
        self.state.chats.get(chat_index).is_some_and(|chat| {
            if !self.settings.show_browse_channels
                && chat.platform == Platform::Slack
                && chat.membership == ChatMembership::NotJoined
            {
                return false;
            }
            if !self.settings.show_muted_chats
                && chat.muted
                && !chat.pinned
                && chat.unread_count == 0
            {
                return false;
            }
            if !self.settings.show_empty_chats
                && !chat.pinned
                && chat.unread_count == 0
                && chat.last_message_at.is_none()
                && chat.last_message_preview.is_none()
            {
                return false;
            }
            true
        })
    }

    fn queue_loaded_chat_avatar_previews(&mut self) {
        let mut seen_accounts = HashSet::new();
        let mut keys = self
            .state
            .visible_chat_indices
            .iter()
            .filter_map(|chat_index| {
                self.state
                    .chats
                    .get(*chat_index)
                    .and_then(|chat| chat.avatar.as_deref())
                    .map(|path| AvatarPreviewKey {
                        path: path.to_path_buf(),
                        width: chat_list::CHAT_AVATAR_WIDTH,
                        rows: chat_list::CHAT_AVATAR_ROWS,
                        source: AvatarPreviewSource::Avatar,
                    })
            })
            .collect::<Vec<_>>();
        let account_ids = self
            .state
            .visible_chat_indices
            .iter()
            .filter_map(|chat_index| self.state.chats.get(*chat_index))
            .map(|chat| chat.account.clone())
            .filter(|provider_id| seen_accounts.insert(provider_id.clone()))
            .collect::<Vec<_>>();
        for provider_id in account_ids {
            let Some(account) = self.account_for_provider(&provider_id) else {
                continue;
            };
            let Some(path) = self.account_badge_avatar_path(&provider_id, &account) else {
                continue;
            };
            keys.push(AvatarPreviewKey {
                path,
                width: chat_list::ACCOUNT_BADGE_WIDTH,
                rows: chat_list::ACCOUNT_BADGE_ROWS,
                source: AvatarPreviewSource::AccountBadge,
            });
        }
        let queued = keys.len();
        self.queue_avatar_preview_loads(keys);
        self.log_perf_marker(
            "avatar_preview.prewarm",
            format!(
                "requested={queued} visible_chats={} accounts={}",
                self.state.visible_chat_indices.len(),
                seen_accounts.len()
            ),
        );
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

    fn sort_chats_preserving_selection(&mut self) {
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        self.state.chats.sort_by(compare_chats_for_sidebar);
        if let Some((selected_chat_id, selected_account)) = selected_chat
            && let Some(index) =
                self.state.chats.iter().position(|chat| {
                    chat.id == selected_chat_id && chat.account == selected_account
                })
        {
            self.state.selected_chat = index;
        }
    }

    fn upsert_chat_in_state(&mut self, chat: Chat) {
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
        self.sort_chats_preserving_selection();
        self.apply_filter();
        self.apply_message_filter();
    }

    fn merge_chat_in_state(&mut self, account: &ProviderId, from_chat_id: &ChatId, chat: Chat) {
        let selected_chat = self
            .state
            .selected_chat()
            .map(|chat| (chat.id.clone(), chat.account.clone()));
        let to_chat_id = chat.id.clone();
        self.state
            .chats
            .retain(|existing| !(existing.account == *account && existing.id == *from_chat_id));
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
        for message in &mut self.state.messages {
            if message.account == *account && message.chat_id == *from_chat_id {
                message.chat_id = to_chat_id.clone();
            }
        }
        self.state.chats.sort_by(compare_chats_for_sidebar);
        if let Some((selected_chat_id, selected_account)) = selected_chat {
            let target_id = if selected_chat_id == *from_chat_id && selected_account == *account {
                to_chat_id
            } else {
                selected_chat_id
            };
            if let Some(index) = self
                .state
                .chats
                .iter()
                .position(|chat| chat.id == target_id && chat.account == selected_account)
            {
                self.state.selected_chat = index;
            }
        }
        self.apply_message_filter();
        self.apply_filter();
    }

    fn filter_status(&self) -> String {
        if self.state.filter.is_empty() {
            "filter cleared".to_owned()
        } else {
            match self.state.filter_scope {
                FilterScope::Chats => {
                    let discovery_count = self.state.discovery_results.len();
                    if discovery_count == 0 {
                        format!(
                            "filter {}: {} chats",
                            self.state.filter,
                            self.state.visible_chat_indices.len()
                        )
                    } else {
                        format!(
                            "filter {}: {} chats · {} discoverable destinations",
                            self.state.filter,
                            self.state.visible_chat_indices.len(),
                            discovery_count
                        )
                    }
                }
                FilterScope::Messages => format!(
                    "filter {}: {} messages",
                    self.state.filter,
                    self.state.filtered_messages.len()
                ),
                FilterScope::Thread => format!(
                    "filter {}: {} thread items",
                    self.state.filter,
                    self.filtered_thread_matches().len()
                ),
            }
        }
    }

    async fn open_discovery_result(&mut self, result: DiscoveryResult) -> Result<bool> {
        match result.action {
            DiscoveryAction::Open | DiscoveryAction::CreateChat | DiscoveryAction::OpenDm => {}
            DiscoveryAction::JoinRequired => {
                self.state.status = format!(
                    "{} is discoverable but not joined yet; joining requires an explicit join action",
                    result.label
                );
                return Ok(false);
            }
            DiscoveryAction::Unsupported => {
                self.state.status = format!("{} cannot be opened by this provider", result.label);
                return Ok(false);
            }
        }

        let chat_id = result
            .chat_id
            .clone()
            .unwrap_or_else(|| result.platform_id.clone());
        let chat = Chat {
            id: chat_id.clone(),
            account: result.account.clone(),
            platform: result.platform.clone(),
            name: result.label.clone(),
            avatar: result.avatar.clone(),
            is_group: matches!(
                result.chat_kind,
                Some(
                    ChatKind::Group
                        | ChatKind::PublicChannel
                        | ChatKind::PrivateChannel
                        | ChatKind::GroupDirectMessage
                )
            ),
            kind: result.chat_kind.unwrap_or(ChatKind::Direct),
            membership: result.membership,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: Some(Arc::from("No messages yet")),
            thread_id: None,
        };

        self.store.upsert_chat(&chat).await?;
        self.upsert_chat_in_state(chat);
        if let Some(index) = self
            .state
            .chats
            .iter()
            .position(|chat| chat.id == chat_id && chat.account == result.account)
        {
            self.state.selected_chat = index;
        }
        self.state.filter_mode = false;
        self.state.discovery_results.clear();
        self.state.status = format!("opened {}", result.label);
        self.request_selected_chat_history_sync();
        Ok(true)
    }
    async fn reload_chats(&mut self) -> Result<()> {
        let reload_started = Instant::now();
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
        self.queue_loaded_chat_avatar_previews();
        self.log_slow_perf_duration(
            "chats.reload",
            reload_started,
            format!("count={}", self.state.chats.len()),
        );
        Ok(())
    }
    async fn rebuild_sidebar_activity_from_messages(&mut self) -> Result<()> {
        let rebuild_started = Instant::now();
        let chats_fetch_started = Instant::now();
        let chats = self.store.get_all_chats().await?;
        self.log_slow_perf_duration(
            "sidebar_rebuild.get_all_chats",
            chats_fetch_started,
            format!("count={}", chats.len()),
        );

        let latest_fetch_started = Instant::now();
        let latest_messages = self.store.latest_message_for_each_chat().await?;
        self.log_slow_perf_duration(
            "sidebar_rebuild.latest_messages",
            latest_fetch_started,
            format!("count={}", latest_messages.len()),
        );
        let latest_by_chat = latest_messages
            .into_iter()
            .filter_map(|latest| {
                if is_placeholder_whatsapp_message(&latest.message) {
                    None
                } else {
                    Some(((latest.account_id, latest.chat_id), latest.message))
                }
            })
            .collect::<HashMap<_, _>>();

        let updates = chats
            .into_iter()
            .map(|chat| {
                let latest_message = latest_by_chat.get(&(chat.account.clone(), chat.id.clone()));
                ChatActivityUpdate {
                    account_id: chat.account,
                    chat_id: chat.id,
                    timestamp: latest_message.map(|message| message.timestamp),
                    preview: latest_message.map(message_sidebar_preview),
                }
            })
            .collect::<Vec<_>>();
        let update_count = updates.len();

        let write_started = Instant::now();
        self.store.set_chat_activities(&updates).await?;
        self.log_slow_perf_duration(
            "sidebar_rebuild.write_updates",
            write_started,
            format!("count={update_count}"),
        );
        self.log_perf_duration(
            "sidebar_rebuild.done",
            rebuild_started,
            format!("count={update_count}"),
        );
        Ok(())
    }

    async fn refresh_chat_preview_from_messages(
        &mut self,
        account: &ProviderId,
        chat_id: &ChatId,
        messages: &[Message],
    ) -> Result<()> {
        if let Some(message) = messages
            .iter()
            .filter(|message| !is_placeholder_whatsapp_message(message))
            .max_by_key(|message| message.timestamp)
        {
            self.refresh_chat_preview(
                account,
                chat_id,
                message.timestamp,
                message_sidebar_preview(message),
            )
            .await?;
        }
        Ok(())
    }

    async fn refresh_chat_preview_from_message(&mut self, message: &Message) -> Result<()> {
        if is_placeholder_whatsapp_message(message) {
            return Ok(());
        }
        self.refresh_chat_preview(
            &message.account,
            &message.chat_id,
            message.timestamp,
            message_sidebar_preview(message),
        )
        .await
    }

    async fn refresh_chat_preview(
        &mut self,
        account: &ProviderId,
        chat_id: &ChatId,
        timestamp: Timestamp,
        preview: Arc<str>,
    ) -> Result<()> {
        let refresh_started = Instant::now();
        let Some(chat) = self
            .state
            .chats
            .iter_mut()
            .find(|chat| chat.account == *account && chat.id == *chat_id)
        else {
            return Ok(());
        };

        let should_update = chat
            .last_message_at
            .is_none_or(|current| timestamp >= current)
            || chat.last_message_preview.is_none();
        if !should_update {
            return Ok(());
        }

        chat.last_message_at = Some(timestamp);
        chat.last_message_preview = Some(preview);
        let updated = chat.clone();
        self.store.upsert_chat(&updated).await?;
        self.sort_chats_preserving_selection();
        self.apply_filter();
        self.log_slow_perf_duration(
            "chat_preview.refresh",
            refresh_started,
            format!("account={account} chat={chat_id}"),
        );
        Ok(())
    }

    async fn reload_selected_messages_after_navigation(&mut self) -> Result<()> {
        self.reload_selected_messages_after_navigation_with_options(
            true,
            self.state.focus == FocusPane::Messages,
        )
        .await
    }

    async fn reload_selected_messages_after_navigation_with_options(
        &mut self,
        scroll_to_bottom: bool,
        mark_read_after_load: bool,
    ) -> Result<()> {
        self.schedule_selected_messages_after_navigation_with_history(
            scroll_to_bottom,
            mark_read_after_load,
        )
        .await
    }

    async fn sync_selected_chat_history(&mut self) -> Result<()> {
        let Some(chat) = self.state.selected_chat().cloned() else {
            return Ok(());
        };
        let key = (chat.account.clone(), chat.id.clone());
        let fetch_key = (chat.account.clone(), chat.id.clone(), None);
        if self.state.synced_history_chats.contains(&key)
            || !self.state.loading_history_chats.insert(fetch_key)
        {
            return Ok(());
        }
        let Some(provider) = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .cloned()
        else {
            self.state.status = format!("no provider registered for {}", chat.account);
            return Ok(());
        };

        self.state.status = format!("loading recent messages for {}", chat.name);
        let tx = self.history_tx.clone();
        let store = Arc::clone(&self.store);
        let account = chat.account.clone();
        let chat_id = chat.id.clone();
        let chat_name = chat.name.clone();
        tokio::spawn(async move {
            let result = async {
                let provider_history = provider
                    .history(&chat_id, None, HISTORY_LIMIT)
                    .await
                    .map_err(|error| error.to_string())?;
                if chat.platform != Platform::WhatsApp || provider_history.len() >= HISTORY_LIMIT {
                    return Ok(provider_history);
                }

                let local_history = store
                    .get_messages_for_chat(&account, &chat_id, None, HISTORY_LIMIT)
                    .await
                    .map_err(|error| error.to_string())?;
                let mut merged_history = merge_history_pages(
                    local_history.clone(),
                    provider_history.clone(),
                    Vec::new(),
                    HISTORY_LIMIT,
                );
                let mut anchor = merged_history
                    .first()
                    .cloned()
                    .or_else(|| provider_history.first().cloned())
                    .or_else(|| local_history.first().cloned());

                for _ in 0..WHATSAPP_INITIAL_HISTORY_PAGES {
                    if merged_history.len() >= WHATSAPP_INITIAL_HISTORY_TARGET {
                        break;
                    }
                    let Some(current_anchor) = anchor.clone() else {
                        break;
                    };

                    let older_history = provider
                        .history_before_message(&chat_id, &current_anchor, HISTORY_LIMIT)
                        .await
                        .map_err(|error| error.to_string())?;
                    let before_len = merged_history.len();
                    let next_merged = merge_history_pages(
                        merged_history,
                        Vec::new(),
                        older_history,
                        HISTORY_LIMIT,
                    );
                    let next_anchor = next_merged.first().cloned();
                    let made_progress = next_merged.len() > before_len
                        || next_anchor.as_ref().map(|message| message.id.as_ref())
                            != anchor.as_ref().map(|message| message.id.as_ref());
                    merged_history = next_merged;
                    anchor = next_anchor;
                    if !made_progress {
                        break;
                    }
                }

                Ok(merged_history)
            }
            .await;
            let _ = tx.send(HistoryFetchResult {
                account,
                chat_id,
                chat_name,
                before: None,
                show_status: true,
                platform: chat.platform,
                result,
            });
        });
        Ok(())
    }

    async fn reload_selected_messages(&mut self) -> Result<()> {
        if let Some(chat) = self.state.selected_chat() {
            self.state.messages = self
                .store
                .get_messages_for_chat(&chat.account, &chat.id, None, SELECTED_CHAT_MESSAGE_LIMIT)
                .await?;
            self.apply_message_filter();
            self.clear_message_layout_cache();
            self.apply_cached_member_names_to_selected_messages();
        } else {
            self.state.messages.clear();
            self.state.filtered_messages.clear();
            self.clear_message_layout_cache();
        }
        self.clamp_message_scroll();
        self.schedule_older_history_prefetch_if_needed();
        Ok(())
    }

    async fn load_older_messages_if_at_top(&mut self) -> Result<()> {
        self.schedule_older_history_prefetch_if_needed();
        Ok(())
    }

    fn schedule_older_history_prefetch_if_needed(&mut self) {
        if self.state.is_loading_older_history
            || self.state.older_history_exhausted
            || self.state.messages.is_empty()
        {
            return;
        }

        let should_prefetch = self.state.message_scroll <= HISTORY_PREFETCH_SCROLL_THRESHOLD
            || self.state.messages.len() < HISTORY_LIMIT;
        if !should_prefetch {
            return;
        }

        let Some(chat) = self.state.selected_chat().cloned() else {
            return;
        };
        let Some(before) = self.state.messages.first().map(|message| message.timestamp) else {
            return;
        };
        self.state.is_loading_older_history = true;
        self.spawn_history_fetch(chat, Some(before), true);
    }

    fn archive_running_for_current_accounts(&self) -> bool {
        let accounts = self.current_archive_account_filter();
        !accounts.is_empty()
            && accounts
                .iter()
                .all(|account| self.state.monthly_backfill_ready_accounts.contains(account))
    }

    fn toggle_archive_for_current_accounts(&mut self) {
        let accounts = self.current_archive_account_filter();
        if accounts.is_empty() {
            self.state.status = "no visible accounts to archive".to_owned();
            return;
        }

        let is_running = accounts
            .iter()
            .all(|account| self.state.monthly_backfill_ready_accounts.contains(account));
        if is_running {
            for account in &accounts {
                self.state.monthly_backfill_ready_accounts.remove(account);
            }
            self.state.status = format!("archive sync paused for {} account(s)", accounts.len());
            return;
        }

        for account in &accounts {
            self.state
                .monthly_backfill_ready_accounts
                .insert(account.clone());
        }
        self.state.monthly_backfill_exhausted_chats.clear();
        self.state.monthly_backfill_cursor = 0;
        self.state.status = format!("archive sync started for {} account(s)", accounts.len());
    }

    fn current_archive_account_filter(&self) -> Vec<ProviderId> {
        if let Some(account) = &self.state.active_account {
            return vec![account.clone()];
        }

        let mut seen = HashSet::new();
        self.state
            .chats
            .iter()
            .filter_map(|chat| {
                if chat.membership == ChatMembership::NotJoined
                    || !seen.insert(chat.account.clone())
                {
                    return None;
                }
                Some(chat.account.clone())
            })
            .collect()
    }

    async fn schedule_on_demand_archive_sync(&mut self) -> Result<()> {
        if !self
            .state
            .monthly_backfill_tick
            .is_multiple_of(ARCHIVE_SYNC_TICK_INTERVAL)
            || self.state.monthly_backfill_ready_accounts.is_empty()
        {
            return Ok(());
        }

        let Some((chat, before)) = self.next_archive_sync_candidate().await? else {
            self.state.monthly_backfill_ready_accounts.clear();
            self.state.status = "archive sync complete for current accounts".to_owned();
            return Ok(());
        };
        self.state.status = format!("archive syncing {}", chat.name);
        self.spawn_history_fetch(chat, before, false);
        Ok(())
    }

    async fn next_archive_sync_candidate(&mut self) -> Result<Option<(Chat, Option<Timestamp>)>> {
        if self.state.chats.is_empty() {
            return Ok(None);
        }

        let floor = archive_sync_floor();
        if let Some(chat) = self.state.selected_chat().cloned()
            && let Some(before) = self.archive_candidate_before_for_chat(&chat, floor).await?
        {
            if before.is_some() {
                self.state.is_loading_older_history = true;
            }
            return Ok(Some((chat, before)));
        }

        let chat_count = self.state.chats.len();
        for offset in 0..chat_count {
            let index = (self.state.monthly_backfill_cursor + offset) % chat_count;
            let chat = self.state.chats[index].clone();
            let Some(before) = self.archive_candidate_before_for_chat(&chat, floor).await? else {
                continue;
            };

            self.state.monthly_backfill_cursor = (index + 1) % chat_count;
            return Ok(Some((chat, before)));
        }

        Ok(None)
    }

    async fn archive_candidate_before_for_chat(
        &mut self,
        chat: &Chat,
        floor: Timestamp,
    ) -> Result<Option<Option<Timestamp>>> {
        let chat_key = (chat.account.clone(), chat.id.clone());
        if !self
            .state
            .monthly_backfill_ready_accounts
            .contains(&chat.account)
            || self
                .state
                .monthly_backfill_exhausted_chats
                .contains(&chat_key)
            || chat.membership == ChatMembership::NotJoined
        {
            return Ok(None);
        }

        let Some(oldest) = self
            .store
            .oldest_message_for_chat(&chat.account, &chat.id)
            .await?
        else {
            let fetch_key = (chat.account.clone(), chat.id.clone(), None);
            return Ok((!self.state.loading_history_chats.contains(&fetch_key)).then_some(None));
        };
        if oldest.timestamp <= floor {
            self.state.monthly_backfill_exhausted_chats.insert(chat_key);
            return Ok(None);
        }

        let fetch_key = (
            chat.account.clone(),
            chat.id.clone(),
            Some(oldest.timestamp),
        );
        if self.state.loading_history_chats.contains(&fetch_key) {
            return Ok(None);
        }
        Ok(Some(Some(oldest.timestamp)))
    }

    fn spawn_history_fetch(&mut self, chat: Chat, before: Option<Timestamp>, show_status: bool) {
        let fetch_key = (chat.account.clone(), chat.id.clone(), before);
        if !self.state.loading_history_chats.insert(fetch_key) {
            return;
        }

        let store = Arc::clone(&self.store);
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id().as_ref() == chat.account.as_ref())
            .cloned();
        let tx = self.history_tx.clone();
        let account = chat.account.clone();
        let chat_id = chat.id.clone();
        let chat_name = chat.name.clone();
        let platform = chat.platform.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(before) = before {
                    let local = store
                        .get_messages_for_chat(&account, &chat_id, Some(before), HISTORY_LIMIT)
                        .await
                        .map_err(|error| error.to_string())?;
                    if !local.is_empty() && platform != Platform::WhatsApp {
                        return Ok(local);
                    }
                    if local.len() >= HISTORY_LIMIT {
                        return Ok(local);
                    }
                    let anchor = if let Some(anchor) = local.first().cloned() {
                        Some(anchor)
                    } else {
                        store
                            .oldest_message_for_chat(&account, &chat_id)
                            .await
                            .map_err(|error| error.to_string())?
                    };
                    match (provider, anchor) {
                        (Some(provider), Some(anchor)) => {
                            let older = provider
                                .history_before_message(&chat_id, &anchor, HISTORY_LIMIT)
                                .await
                                .map_err(|error| error.to_string())?;
                            Ok(merge_history_pages(local, Vec::new(), older, HISTORY_LIMIT))
                        }
                        (Some(provider), None) => provider
                            .history(&chat_id, Some(before), HISTORY_LIMIT)
                            .await
                            .map_err(|error| error.to_string()),
                        (None, _) => Ok(local),
                    }
                } else {
                    match provider {
                        Some(provider) => provider
                            .history(&chat_id, None, HISTORY_LIMIT)
                            .await
                            .map_err(|error| error.to_string()),
                        None => Ok(Vec::new()),
                    }
                }
            }
            .await;
            let _ = tx.send(HistoryFetchResult {
                account,
                chat_id,
                chat_name,
                before,
                show_status,
                platform,
                result,
            });
        });
    }

    fn merge_older_messages_into_current_chat(
        &mut self,
        mut older: Vec<Message>,
        chat_name: &str,
        show_status: bool,
        platform: Platform,
    ) {
        self.state.is_loading_older_history = false;
        if older.is_empty() {
            if platform != Platform::WhatsApp {
                self.state.older_history_exhausted = true;
                if show_status {
                    self.state.status = format!("no older messages for {chat_name}");
                }
            } else if show_status {
                self.state.status = format!("requested older messages for {chat_name}");
            }
            return;
        }

        let previous_line_count = self.cached_message_line_count();
        let mut seen = self
            .state
            .messages
            .iter()
            .map(|message| message.id.clone())
            .collect::<HashSet<_>>();
        older.retain(|message| seen.insert(message.id.clone()));

        if older.is_empty() {
            if show_status {
                self.state.status = "older messages already loaded".to_owned();
            }
            return;
        }

        let added_count = older.len();
        older.extend(self.state.messages.iter().cloned());
        older.sort_by_key(|message| message.timestamp);
        self.state.messages = older;
        self.apply_message_filter();
        self.clear_message_layout_cache();

        let new_line_count = self.cached_message_line_count();
        let added_lines = new_line_count.saturating_sub(previous_line_count);
        self.state.message_scroll = self.state.message_scroll.saturating_add(added_lines);
        self.clamp_message_scroll();
        if show_status {
            self.state.status = format!("loaded {added_count} older messages for {chat_name}");
        }
    }
}

fn archive_sync_floor() -> Timestamp {
    Utc::now() - ChronoDuration::days(ARCHIVE_BACKFILL_WINDOW_DAYS)
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

const EMPTY_WHATSAPP_MESSAGE_PLACEHOLDER: &str = "[empty WhatsApp message]";

fn message_sidebar_preview(message: &Message) -> Arc<str> {
    Arc::from(content_send_preview(&message.content))
}

fn is_placeholder_whatsapp_message(message: &Message) -> bool {
    message.platform_data.whatsapp.is_some()
        && matches!(
            &message.content,
            Content::Text(text) | Content::Unsupported(text)
                if text.trim() == EMPTY_WHATSAPP_MESSAGE_PLACEHOLDER
        )
}

fn content_send_preview(content: &Content) -> String {
    match content {
        Content::Text(text) | Content::Unsupported(text) => text.to_string(),
        Content::Image(media) => visible_media_caption(media)
            .map(|caption| format!("Image: {caption}"))
            .unwrap_or_else(|| "Image".to_owned()),
        Content::Video(media) => visible_media_caption(media)
            .map(|caption| format!("Video: {caption}"))
            .unwrap_or_else(|| format!("Video: {}", media.file_name)),
        Content::Audio(media) => visible_media_caption(media)
            .map(|caption| format!("Audio: {caption}"))
            .unwrap_or_else(|| format!("Audio: {}", media.file_name)),
        Content::File(media) => visible_media_caption(media)
            .map(|caption| format!("File: {caption}"))
            .unwrap_or_else(|| format!("File: {}", media.file_name)),
        Content::Sticker(media) => visible_media_caption(media)
            .map(|caption| format!("Sticker: {caption}"))
            .unwrap_or_else(|| "Sticker".to_owned()),
        Content::LinkPreview(link) => link
            .title
            .as_deref()
            .and_then(clean_html_text)
            .unwrap_or_else(|| link.url.to_string()),
        Content::Cards(cards) => cards
            .first()
            .and_then(|card| card.title.as_deref().or(card.body.as_deref()))
            .unwrap_or("Card")
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
    resize_mode: TerminalImageResizeMode,
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
    let resize = match resize_mode {
        TerminalImageResizeMode::Fit => Resize::Fit(Some(FilterType::Triangle)),
        TerminalImageResizeMode::Scale => Resize::Scale(Some(FilterType::Triangle)),
    };
    picker
        .new_protocol(image, size, resize)
        .map_err(|error| format!("rendering {}: {error}", path.display()))
}

fn stable_bytes_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn build_terminal_image_protocol_from_bytes(
    picker: &Picker,
    bytes: &[u8],
    size: Size,
) -> std::result::Result<Protocol, String> {
    if size.width == 0 || size.height == 0 {
        return Err("image area is too small".to_owned());
    }
    let image =
        image::load_from_memory(bytes).map_err(|error| format!("decoding image bytes: {error}"))?;
    picker
        .new_protocol(image, size, Resize::Fit(Some(FilterType::Triangle)))
        .map_err(|error| format!("rendering image bytes: {error}"))
}

async fn run_app_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut needs_draw = true;
    let mut loop_started = Instant::now();

    while !app.state.should_quit() {
        if needs_draw {
            let draw_started = Instant::now();
            terminal.draw(|frame| app.draw(frame))?;
            app.log_slow_perf_duration("terminal.draw", draw_started, "");
            needs_draw = false;
        }

        if event::poll(Duration::ZERO)? {
            let event_started = Instant::now();
            match event::read()? {
                CrosstermEvent::Key(key) => app.handle_event(AppEvent::Key(key)).await?,
                CrosstermEvent::Mouse(mouse) => app.handle_event(AppEvent::Mouse(mouse)).await?,
                CrosstermEvent::Resize(width, height) => {
                    app.handle_event(AppEvent::Resize(width, height)).await?;
                }
                _ => {}
            }
            app.log_slow_perf_duration("terminal.input_event", event_started, "immediate=true");
            needs_draw = true;
            app.log_event_loop_stall(loop_started.elapsed(), "after=immediate_input");
            loop_started = Instant::now();
            tokio::task::yield_now().await;
            continue;
        }

        if app.drain_provider_events().await? {
            needs_draw = true;
            app.log_event_loop_stall(loop_started.elapsed(), "after=provider_drain");
            loop_started = Instant::now();
            tokio::task::yield_now().await;
            continue;
        }

        if event::poll(IDLE_POLL_TIMEOUT)? {
            let event_started = Instant::now();
            match event::read()? {
                CrosstermEvent::Key(key) => app.handle_event(AppEvent::Key(key)).await?,
                CrosstermEvent::Mouse(mouse) => app.handle_event(AppEvent::Mouse(mouse)).await?,
                CrosstermEvent::Resize(width, height) => {
                    app.handle_event(AppEvent::Resize(width, height)).await?;
                }
                _ => {}
            }
            app.log_slow_perf_duration("terminal.input_event", event_started, "immediate=false");
            needs_draw = true;
        } else {
            let notification_visible = app.state.notification_visible();
            app.handle_event(AppEvent::Tick).await?;
            if notification_visible != app.state.notification_visible() {
                needs_draw = true;
            }
        }
        app.log_event_loop_stall(loop_started.elapsed(), "after=idle");
        loop_started = Instant::now();
    }

    terminal.draw(|frame| app.draw(frame))?;
    Ok(())
}

fn draw_chat_list_details(app: &App, area: Rect) -> String {
    format!(
        "area={}x{} chats={} visible={} selected={} inbox_style={:?}",
        area.width,
        area.height,
        app.state.chats.len(),
        app.state.visible_chat_indices.len(),
        app.state.selected_chat,
        app.settings.chat_inbox_style
    )
}

fn draw_messages_details(app: &App, area: Rect) -> String {
    format!(
        "area={}x{} messages={} scroll={} selected_message={} media_hits={} link_cache={}",
        area.width,
        area.height,
        app.state.messages.len(),
        app.state.message_scroll,
        app.state.selected_message_id.as_deref().unwrap_or("none"),
        app.state.media_hits.len(),
        app.link_metadata_cache.len()
    )
}

fn draw_compose_details(app: &App, area: Rect) -> String {
    format!(
        "area={}x{} chars={} lines={} attachment={} reply={}",
        area.width,
        area.height,
        app.state.compose_text.chars().count(),
        app.state.compose.lines().len(),
        app.state.pending_attachment.is_some(),
        app.state.reply_to.is_some()
    )
}

fn draw_details_details(app: &App, area: Rect) -> String {
    format!(
        "area={}x{} scroll={} selected_chat={}",
        area.width,
        area.height,
        app.state.details_scroll,
        app.state
            .selected_chat()
            .map(|chat| chat.id.as_ref())
            .unwrap_or("none")
    )
}

fn overlay_draw_details(app: &App) -> String {
    let mut active = Vec::new();
    if app.state.notification.is_some() {
        active.push("notification");
    }
    if app.state.account_switcher.is_some() {
        active.push("account_switcher");
    }
    if app.state.settings_overlay.is_some() {
        active.push("settings");
    }
    if app.state.image_viewer.is_some() {
        active.push("image_viewer");
    }
    if app.state.auth_overlay.is_some() {
        active.push("auth");
    }
    if app.state.account_setup.is_some() {
        active.push("account_setup");
    }
    if app.state.slack_setup.is_some() {
        active.push("slack_setup");
    }
    if app.state.help_overlay.is_some() {
        active.push("help");
    }
    if app.state.action_menu.is_some() {
        active.push("action_menu");
    }
    if app.state.reaction_picker.is_some() {
        active.push("reaction_picker");
    }
    if app.state.compose_attach_menu.is_some() {
        active.push("compose_attach");
    }
    if app.state.compose_emoticon_picker.is_some() {
        active.push("compose_emoticon");
    }
    if app.state.poll_vote_picker.is_some() {
        active.push("poll_vote");
    }

    if active.is_empty() {
        "none".to_owned()
    } else {
        active.join(",")
    }
}

fn provider_event_type_counts(events: &[AppEvent]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        if let AppEvent::Provider(_, provider_event) = event {
            *counts
                .entry(provider_event_label(provider_event))
                .or_insert(0) += 1;
        }
    }
    counts
}

fn format_event_type_counts(counts: &BTreeMap<&'static str, usize>) -> String {
    if counts.is_empty() {
        return "none".to_owned();
    }

    counts
        .iter()
        .map(|(label, count)| format!("{label}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn provider_event_requests_draw(
    event: &AppEvent,
    network_activity: NetworkActivityDisplay,
) -> bool {
    let AppEvent::Provider(_, provider_event) = event else {
        return false;
    };

    match provider_event.as_ref() {
        ProviderEvent::NetworkActivity { .. } => network_activity != NetworkActivityDisplay::Hidden,
        _ => true,
    }
}

fn app_event_label(event: &AppEvent) -> &'static str {
    match event {
        AppEvent::Key(_) => "key",
        AppEvent::Mouse(_) => "mouse",
        AppEvent::Resize(_, _) => "resize",
        AppEvent::Tick => "tick",
        AppEvent::Provider(_, _) => "provider",
        AppEvent::MediaReady(_, _) => "media_ready",
    }
}

fn provider_event_label(event: &ProviderEvent) -> &'static str {
    match event {
        ProviderEvent::Message { is_historical, .. } if *is_historical => "message.historical",
        ProviderEvent::Message { .. } => "message.live",
        ProviderEvent::MessageEdited { .. } => "message_edited",
        ProviderEvent::MessageDeleted { .. } => "message_deleted",
        ProviderEvent::ReactionChanged { .. } => "reaction_changed",
        ProviderEvent::Receipt { .. } => "receipt",
        ProviderEvent::ChatUpdated(_) => "chat_updated",
        ProviderEvent::ChatMerged { .. } => "chat_merged",
        ProviderEvent::AuthRequired(_) => "auth_required",
        ProviderEvent::AuthSucceeded => "auth_succeeded",
        ProviderEvent::SyncProgress(_) => "sync_progress",
        ProviderEvent::SyncComplete => "sync_complete",
        ProviderEvent::AccountNotice { .. } => "account_notice",
        ProviderEvent::Disconnected(_) => "disconnected",
        ProviderEvent::Reconnecting => "reconnecting",
        ProviderEvent::Typing { .. } => "typing",
        ProviderEvent::NetworkActivity { .. } => "network_activity",
    }
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

fn new_thread_compose_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("Reply in thread...");
    textarea.set_cursor_line_style(Style::default());
    textarea
}

fn preserve_sidebar_activity_metadata(chat: &mut Chat, existing: Option<&Chat>) {
    let Some(existing) = existing else {
        return;
    };

    if chat.platform == Platform::WhatsApp {
        chat.unread_count = chat.unread_count.max(existing.unread_count);
        if existing.last_message_preview.as_deref() == Some(EMPTY_WHATSAPP_MESSAGE_PLACEHOLDER) {
            return;
        }
    }

    let existing_is_newer = match (existing.last_message_at, chat.last_message_at) {
        (Some(existing_at), Some(incoming_at)) => existing_at > incoming_at,
        (Some(_), None) => true,
        _ => false,
    };

    if existing_is_newer {
        chat.last_message_at = existing.last_message_at;
        chat.last_message_preview = existing.last_message_preview.clone();
    } else if chat.last_message_preview.is_none() {
        chat.last_message_preview = existing.last_message_preview.clone();
    }
}

fn chat_kind_label(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Direct => "direct message",
        ChatKind::Group => "group",
        ChatKind::PublicChannel => "public channel",
        ChatKind::PrivateChannel => "private channel",
        ChatKind::GroupDirectMessage => "group DM",
    }
}

fn chat_membership_label(membership: ChatMembership) -> &'static str {
    match membership {
        ChatMembership::Joined => "joined",
        ChatMembership::NotJoined => "not joined",
        ChatMembership::Unknown => "unknown",
    }
}

fn compare_chats_for_sidebar(a: &Chat, b: &Chat) -> std::cmp::Ordering {
    let a_unread = a.unread_count > 0;
    let b_unread = b.unread_count > 0;

    b_unread
        .cmp(&a_unread)
        .then_with(|| b.pinned.cmp(&a.pinned))
        .then_with(|| a.muted.cmp(&b.muted))
        .then_with(|| b.last_message_at.cmp(&a.last_message_at))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.account.cmp(&b.account))
        .then_with(|| a.id.cmp(&b.id))
}

fn merge_history_pages(
    local_history: Vec<Message>,
    provider_history: Vec<Message>,
    older_history: Vec<Message>,
    limit: usize,
) -> Vec<Message> {
    let mut seen = HashSet::new();
    let mut messages = older_history
        .into_iter()
        .chain(local_history)
        .chain(provider_history)
        .filter(|message| {
            seen.insert((
                message.account.clone(),
                message.chat_id.clone(),
                message.id.clone(),
            ))
        })
        .collect::<Vec<_>>();
    messages.sort_by_key(|message| message.timestamp);
    let start = messages.len().saturating_sub(limit);
    messages.split_off(start)
}

fn typing_preview_text(names: &[&str]) -> Option<String> {
    match names {
        [] => None,
        [name] => Some(format!("{name} is typing…")),
        [first, second] => Some(format!("{first}, {second} typing…")),
        _ => Some("Several people typing…".to_owned()),
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn network_activity_status_spans(
    display: NetworkActivityDisplay,
    providers: &[ProviderBox],
    activity: &HashMap<ProviderId, AccountNetworkActivity>,
    theme: Theme,
    now: Timestamp,
    max_width: usize,
) -> Vec<Span<'static>> {
    match display {
        NetworkActivityDisplay::Hidden => Vec::new(),
        NetworkActivityDisplay::CombinedLights => {
            combined_network_activity_spans(providers, activity, theme, now, max_width)
        }
        NetworkActivityDisplay::RecentCounts => {
            recent_network_activity_count_spans(providers, activity, theme, now, max_width)
        }
    }
}

fn combined_network_activity_spans(
    providers: &[ProviderBox],
    activity: &HashMap<ProviderId, AccountNetworkActivity>,
    theme: Theme,
    now: Timestamp,
    max_width: usize,
) -> Vec<Span<'static>> {
    if max_width < 2 || providers.is_empty() {
        return Vec::new();
    }

    let rx_style = providers
        .iter()
        .enumerate()
        .filter_map(|(index, provider)| {
            let account_activity = activity.get(provider.id())?;
            account_activity
                .rx_active(now)
                .then_some(network_activity_account_style(provider.platform(), index))
        })
        .next_back()
        .unwrap_or_else(|| network_activity_idle_style(theme));
    let tx_style = providers
        .iter()
        .enumerate()
        .filter_map(|(index, provider)| {
            let account_activity = activity.get(provider.id())?;
            account_activity
                .tx_active(now)
                .then_some(network_activity_account_style(provider.platform(), index))
        })
        .next_back()
        .unwrap_or_else(|| network_activity_idle_style(theme));

    vec![
        Span::styled("↓".to_owned(), rx_style),
        Span::styled("↑".to_owned(), tx_style),
    ]
}

fn recent_network_activity_count_spans(
    providers: &[ProviderBox],
    activity: &HashMap<ProviderId, AccountNetworkActivity>,
    theme: Theme,
    now: Timestamp,
    max_width: usize,
) -> Vec<Span<'static>> {
    if max_width < 2 || providers.is_empty() {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut used_width = 0;
    for (index, provider) in providers.iter().enumerate() {
        let account_activity = activity.get(provider.id());
        let rx_count = account_activity
            .map(|activity| activity.recent_rx_count(now))
            .unwrap_or_default();
        let tx_count = account_activity
            .map(|activity| activity.recent_tx_count(now))
            .unwrap_or_default();
        let rx_changed = account_activity
            .map(|activity| activity.recent_rx_changed(now))
            .unwrap_or_default();
        let tx_changed = account_activity
            .map(|activity| activity.recent_tx_changed(now))
            .unwrap_or_default();
        let rx_count_text = network_activity_count_text(rx_count);
        let tx_count_text = network_activity_count_text(tx_count);
        let group_width = UnicodeWidthStr::width("↓")
            + UnicodeWidthStr::width(rx_count_text.as_str())
            + 1
            + UnicodeWidthStr::width("↑")
            + UnicodeWidthStr::width(tx_count_text.as_str());
        let separator = if spans.is_empty() { "" } else { "  " };
        let total_width = separator.len() + group_width;
        if used_width + total_width > max_width {
            break;
        }
        if !separator.is_empty() {
            spans.push(Span::styled(separator.to_owned(), theme.status_bar()));
            used_width += separator.len();
        }
        spans.push(Span::styled(
            "↓".to_owned(),
            network_activity_direction_style(theme, provider.platform(), index, rx_changed),
        ));
        spans.push(Span::styled(
            rx_count_text,
            network_activity_count_style(theme, rx_changed),
        ));
        spans.push(Span::styled(" ".to_owned(), theme.status_bar()));
        spans.push(Span::styled(
            "↑".to_owned(),
            network_activity_direction_style(theme, provider.platform(), index, tx_changed),
        ));
        spans.push(Span::styled(
            tx_count_text,
            network_activity_count_style(theme, tx_changed),
        ));
        used_width += group_width;
    }

    spans
}

fn network_activity_idle_style(theme: Theme) -> Style {
    Style::default().fg(theme.muted).bg(Color::Black)
}

fn network_activity_count_style(theme: Theme, active: bool) -> Style {
    if active {
        theme.status_bar()
    } else {
        network_activity_idle_style(theme)
    }
}

fn network_activity_direction_style(
    theme: Theme,
    platform: Platform,
    index: usize,
    active: bool,
) -> Style {
    if active {
        network_activity_account_style(platform, index)
    } else {
        network_activity_idle_style(theme)
    }
}

fn network_activity_count_text(count: usize) -> String {
    count.to_string()
}

fn network_activity_account_style(platform: Platform, index: usize) -> Style {
    Style::default()
        .fg(network_activity_account_color(platform, index))
        .bg(Color::Black)
        .add_modifier(Modifier::BOLD)
}

fn network_activity_account_color(platform: Platform, index: usize) -> Color {
    match platform {
        Platform::WhatsApp => Color::Green,
        Platform::Slack => Color::Magenta,
        Platform::Discord => Color::Blue,
        Platform::Unknown(_) => fallback_network_activity_account_color(index),
    }
}

fn fallback_network_activity_account_color(index: usize) -> Color {
    const COLORS: [Color; 6] = [
        Color::LightGreen,
        Color::LightCyan,
        Color::LightMagenta,
        Color::LightYellow,
        Color::LightBlue,
        Color::Cyan,
    ];
    COLORS[index % COLORS.len()]
}

fn message_top_padding(
    rendered_lines: usize,
    total_lines: usize,
    scroll: usize,
    viewport_rows: usize,
) -> usize {
    if total_lines == 0
        || rendered_lines >= viewport_rows
        || scroll.saturating_add(viewport_rows) < total_lines
    {
        return 0;
    }

    viewport_rows.saturating_sub(rendered_lines)
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

fn format_relative_time(timestamp: Timestamp) -> String {
    let duration = Utc::now().signed_duration_since(timestamp);
    if duration.num_days() >= 1 {
        format!("{}d", duration.num_days())
    } else if duration.num_hours() >= 1 {
        format!("{}h", duration.num_hours())
    } else if duration.num_minutes() >= 1 {
        format!("{}m", duration.num_minutes())
    } else {
        "now".to_owned()
    }
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

fn slack_setup_help_lines(theme: Theme) -> Vec<Line<'static>> {
    let heading = |text: &'static str| Line::from(Span::styled(text, theme.status_key()));
    let body = |text: &'static str| Line::from(Span::styled(text, theme.status_bar()));
    let note = |text: &'static str| Line::from(Span::styled(text, theme.muted()));

    vec![
        heading("How Slack sign-in works"),
        body("chat-cli signs in to one Slack workspace at a time."),
        Line::from(""),
        heading("Multiple workspaces"),
        body("Each workspace is a separate connection with its own approval."),
        body("Use \"Add another Slack workspace\" to connect more than one."),
        note("Workspaces stay isolated; signing into one never affects another."),
        Line::from(""),
        heading("Approvals"),
        body("Some workspaces require an admin to approve the requested scopes."),
        note("If approval is pending, sign-in completes once an admin allows it."),
        Line::from(""),
        heading("Permissions we request"),
        body("team:read lets chat-cli show the real workspace name and icon."),
        body("History scopes let chat-cli read recent messages per channel."),
        Line::from(""),
        heading("Realtime vs polling"),
        body("An App-Level Token (xapp-...) with connections:write enables instant"),
        body("delivery via Socket Mode."),
        note("Without it, chat-cli falls back to periodic history polling."),
    ]
}

fn slack_app_creation_url_display(url: &str) -> String {
    const MARKER: &str = "new_app=1";
    url.find(MARKER)
        .map(|index| url[..index + MARKER.len()].to_owned())
        .unwrap_or_else(|| truncate_chars(url, 72))
}

#[cfg(not(test))]
fn open_and_copy_slack_oauth_url(url: &str) {
    let _ = copy_slack_oauth_url_to_clipboard(url);
    let _ = open::that_detached(url);
}

#[cfg(all(
    not(test),
    unix,
    not(any(target_os = "macos", target_os = "android", target_os = "emscripten"))
))]
fn copy_slack_oauth_url_to_clipboard(url: &str) -> Result<(), arboard::Error> {
    Clipboard::new()?
        .set()
        .wait_until(std::time::Instant::now() + Duration::from_millis(150))
        .text(url.to_owned())
}

#[cfg(all(
    not(test),
    any(
        not(unix),
        target_os = "macos",
        target_os = "android",
        target_os = "emscripten"
    )
))]
fn copy_slack_oauth_url_to_clipboard(url: &str) -> Result<(), arboard::Error> {
    Clipboard::new()?.set_text(url.to_owned())
}

#[cfg(test)]
fn open_and_copy_slack_oauth_url(_url: &str) {}

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

fn slack_setup_workspace_seed(display_name: &str) -> String {
    let mut seed = display_name.trim();
    while let Some(inner) = seed
        .strip_prefix("Slack (")
        .and_then(|rest| rest.strip_suffix(')'))
        .map(str::trim)
    {
        if inner.is_empty() {
            break;
        }
        seed = inner;
    }
    seed.to_owned()
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

fn message_matches_filter(message: &Message, filter: &str) -> bool {
    let terms = filter
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {}",
        content_copy_text(&message.content),
        message.sender.display_name,
    )
    .to_lowercase();
    terms.iter().all(|term| haystack.contains(term))
}

fn is_slack_thread_reply(message: &Message) -> bool {
    message.platform_data.slack.as_ref().is_some_and(|slack| {
        slack
            .thread_ts
            .as_ref()
            .is_some_and(|thread_ts| thread_ts.as_ref() != slack.ts.as_ref())
    }) || message.reply_to.as_ref().is_some_and(|reply_to| {
        message.platform_data.slack.is_some() && reply_to.as_ref() != message.id.as_ref()
    })
}

/// The thread root id a reply belongs to, or `None` when the message is not a
/// thread reply. Prefers the explicit `thread_id`, falling back to `reply_to`.
fn thread_root_of(message: &Message) -> Option<ThreadId> {
    if !is_slack_thread_reply(message) {
        return None;
    }
    message
        .thread_id
        .clone()
        .or_else(|| message.reply_to.clone())
        .filter(|root| root.as_ref() != message.id.as_ref())
}

fn reply_count_label(count: usize) -> String {
    if count == 1 {
        "1 reply".to_owned()
    } else {
        format!("{count} replies")
    }
}

fn new_replies_label(count: usize) -> String {
    if count == 1 {
        "1 new reply".to_owned()
    } else {
        format!("{count} new replies")
    }
}

fn thread_reply_divider(label: &str, theme: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("──── ", theme.muted()),
        Span::styled(label.to_owned(), theme.status_key()),
        Span::styled(" ────", theme.muted()),
    ])
}

fn thread_message_card_lines(
    message: &Message,
    theme: Theme,
    is_root: bool,
    width: u16,
    media_cache: &mut message_list::MediaPreviewCache,
    link_metadata: &message_list::LinkMetadataCache,
) -> message_list::ThreadMessageCardRender {
    let sender_style = if is_root {
        message_list::sender_style(theme, message.is_from_me).add_modifier(Modifier::BOLD)
    } else {
        message_list::sender_style(theme, message.is_from_me)
    };
    let gutter = if is_root { "│ " } else { "  " };
    let body_gutter = if is_root { "│   " } else { "    " };
    let sender = message.sender.display_name.to_string();
    let timestamp = message_list::format_message_time(message.timestamp);
    let reserved_trailing_space = 1usize;
    let fixed_width = UnicodeWidthStr::width(gutter)
        .saturating_add(UnicodeWidthStr::width(sender.as_str()))
        .saturating_add(UnicodeWidthStr::width(timestamp.as_str()))
        .saturating_add(reserved_trailing_space);
    let spacer = (width as usize).saturating_sub(fixed_width).max(2);
    let mut lines = vec![Line::from(vec![
        Span::styled(gutter.to_owned(), theme.muted()),
        Span::styled(sender, sender_style),
        Span::raw(" ".repeat(spacer)),
        Span::styled(timestamp, theme.muted()),
        Span::raw(" "),
    ])];

    let body_width = width.saturating_sub(UnicodeWidthStr::width(body_gutter) as u16);
    let rendered = message_list::build_thread_message_card_lines(
        message,
        body_width,
        media_cache,
        link_metadata,
        theme,
    );
    if rendered.lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(body_gutter.to_owned(), theme.muted()),
            Span::styled("attachment", theme.muted()),
        ]));
    } else {
        for mut line in rendered.lines {
            let mut spans = vec![Span::styled(body_gutter.to_owned(), theme.muted())];
            spans.append(&mut line.spans);
            lines.push(Line::from(spans));
        }
    }

    if !message.reactions.is_empty() {
        let reactions = message
            .reactions
            .iter()
            .map(|reaction| {
                format!(
                    "{} {}",
                    message_list::reaction_display_emoji(reaction.emoji.as_ref()),
                    reaction.senders.len()
                )
            })
            .collect::<Vec<_>>()
            .join("  ");
        lines.push(Line::from(vec![
            Span::styled(body_gutter.to_owned(), theme.muted()),
            Span::styled(reactions, theme.muted()),
        ]));
    }
    lines.push(Line::from(""));
    message_list::ThreadMessageCardRender {
        lines,
        link_preview_requests: rendered.link_preview_requests,
        media_preview_requests: rendered.media_preview_requests,
    }
}

fn thread_message_card_line_count(
    message: &Message,
    is_root: bool,
    width: u16,
    link_metadata: &message_list::LinkMetadataCache,
) -> usize {
    let body_gutter = if is_root { "│   " } else { "    " };
    let body_width = width.saturating_sub(UnicodeWidthStr::width(body_gutter) as u16);
    1 + message_list::thread_message_card_content_line_count(message, body_width, link_metadata)
        + usize::from(!message.reactions.is_empty())
        + 1
}

fn action_menu_items_for_message(
    message: &Message,
    thread_reply_count: usize,
) -> Vec<ActionMenuItem> {
    let mut items = Vec::from(ActionMenuItem::COMMON);
    if thread_reply_count > 0 || message.thread_id.as_ref() == Some(&message.id) {
        items.insert(1, ActionMenuItem::ViewThread);
    }
    if first_content_url(&message.content).is_some() {
        items.push(ActionMenuItem::OpenLink);
    }
    if matches!(message.content, Content::Poll(_)) {
        items.push(ActionMenuItem::VotePoll);
    }
    if message_has_direct_image_preview(&message.content) {
        items.push(ActionMenuItem::OpenImage);
    }
    items.push(ActionMenuItem::Cancel);
    items
}

fn forward_content_for_capabilities(
    content: &Content,
    capabilities: &OutboundCapabilities,
) -> Option<Content> {
    let portable = portable_forward_content(content)?;
    capabilities.supports_content(&portable).then_some(portable)
}

fn portable_forward_content(content: &Content) -> Option<Content> {
    match content {
        Content::Text(text) => non_empty_text_content(text.as_ref()),
        Content::Image(_)
        | Content::Video(_)
        | Content::Audio(_)
        | Content::File(_)
        | Content::Sticker(_) => Some(content.clone()),
        Content::LinkPreview(link) => non_empty_text_content(&link.url),
        Content::Cards(cards) => cards.iter().find_map(card_forward_text).and_then(|text| {
            let text: Arc<str> = Arc::from(text);
            non_empty_text_content(text.as_ref())
        }),
        Content::Poll(_) | Content::Deleted | Content::Unsupported(_) => None,
    }
}

fn non_empty_text_content(text: &str) -> Option<Content> {
    let text = text.trim();
    (!text.is_empty()).then(|| Content::Text(Arc::from(text)))
}

fn card_forward_text(card: &Card) -> Option<String> {
    if let Some(url) = card
        .url
        .as_deref()
        .or_else(|| card.actions.iter().find_map(|action| action.url.as_deref()))
    {
        return Some(url.to_owned());
    }
    card.title
        .as_deref()
        .or(card.body.as_deref())
        .map(str::to_owned)
}

fn platform_label(platform: &Platform) -> &'static str {
    match platform {
        Platform::WhatsApp => "WhatsApp",
        Platform::Slack => "Slack",
        Platform::Discord => "Discord",
        Platform::Unknown(_) => "Unknown",
    }
}

fn forward_picker_visible_rows(modal: Rect) -> usize {
    modal.height.saturating_sub(5).max(1) as usize / 2
}

fn forward_picker_scroll_start(selected: usize, visible_rows: usize, filtered_len: usize) -> usize {
    if visible_rows == 0 || filtered_len == 0 {
        return 0;
    }
    selected
        .min(filtered_len.saturating_sub(1))
        .saturating_add(1)
        .saturating_sub(visible_rows)
}

fn forward_picker_filtered_len(picker: &ForwardPicker) -> usize {
    forward_picker_filtered_indices(picker).len()
}

fn forward_picker_selected_target(picker: &ForwardPicker) -> Option<(MessageId, ForwardTarget)> {
    let target_index = forward_picker_filtered_indices(picker)
        .get(picker.selected)
        .copied()?;
    Some((
        picker.message_id.clone(),
        picker.targets.get(target_index)?.clone(),
    ))
}

fn forward_picker_filtered_indices(picker: &ForwardPicker) -> Vec<usize> {
    let terms = search_terms(&picker.query);
    picker
        .targets
        .iter()
        .enumerate()
        .filter_map(|(index, target)| {
            (terms.is_empty()
                || terms
                    .iter()
                    .all(|term| forward_target_matches(target, term)))
            .then_some(index)
        })
        .collect()
}

fn forward_target_matches(target: &ForwardTarget, term: &str) -> bool {
    target.label.to_lowercase().contains(term) || target.subtitle.to_lowercase().contains(term)
}

fn search_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect()
}

fn first_content_url(content: &Content) -> Option<String> {
    match content {
        Content::LinkPreview(link) => Some(link.url.to_string()),
        Content::Cards(cards) => cards
            .iter()
            .find_map(|card| {
                card.url
                    .as_deref()
                    .or_else(|| card.actions.iter().find_map(|action| action.url.as_deref()))
            })
            .map(str::to_owned),
        Content::Text(text) | Content::Unsupported(text) => first_url_in_text(text),
        Content::Image(_)
        | Content::Video(_)
        | Content::Audio(_)
        | Content::File(_)
        | Content::Sticker(_)
        | Content::Poll(_)
        | Content::Deleted => None,
    }
}

fn message_has_direct_image_preview(content: &Content) -> bool {
    match content {
        Content::Image(media) | Content::Sticker(media) => {
            media.local_path.as_ref().is_some_and(|path| path.exists())
                || media.thumbnail.as_ref().is_some_and(|path| path.exists())
        }
        Content::Text(_)
        | Content::Video(_)
        | Content::Audio(_)
        | Content::File(_)
        | Content::LinkPreview(_)
        | Content::Cards(_)
        | Content::Poll(_)
        | Content::Deleted
        | Content::Unsupported(_) => false,
    }
}

fn first_url_in_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|token| {
            token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | '.'
                )
            })
        })
        .find(|token| is_openable_url(token))
        .map(str::to_owned)
}

fn is_openable_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .is_some_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

#[cfg(not(test))]
fn open_url(url: &str) {
    let _ = open::that_detached(url);
}

#[cfg(test)]
fn open_url(_url: &str) {}

fn content_copy_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.to_string(),
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => visible_media_caption(media)
            .map(str::to_owned)
            .unwrap_or_else(|| media.file_name.to_string()),
        Content::LinkPreview(link) => {
            let title = link
                .title
                .as_deref()
                .and_then(clean_html_text)
                .unwrap_or_else(|| "Link".to_owned());
            let description = link
                .description
                .as_deref()
                .and_then(clean_html_text)
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            format!("{title}: {}{description}", link.url)
        }
        Content::Cards(cards) => cards
            .iter()
            .filter_map(|card| card.title.as_deref().or(card.body.as_deref()))
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
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
        Content::Cards(cards) => cards
            .iter()
            .find_map(|card| card.image.as_ref().or(card.thumbnail.as_ref()))
            .and_then(media_image_preview),
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
        visible_media_caption(media).map(str::to_owned),
    ))
}

fn visible_media_caption(media: &Media) -> Option<&str> {
    media
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .filter(|caption| !caption.eq_ignore_ascii_case(EMPTY_WHATSAPP_MESSAGE_PLACEHOLDER))
}

fn is_whatsapp_status_chat(chat: &Chat) -> bool {
    chat.platform == Platform::WhatsApp
        && (chat.id.as_ref() == "whatsapp:status@broadcast"
            || (chat.name.eq_ignore_ascii_case("status") && chat.id.contains("status")))
}

fn static_account_icon_path(provider_id: &ProviderId, platform: Platform) -> PathBuf {
    let prefix = match platform {
        Platform::Slack => "slack",
        Platform::WhatsApp => "whatsapp",
        Platform::Unknown(_) | Platform::Discord => "unknown",
    };
    let safe_id = provider_id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || matches!(value, '-' | '_') {
                value
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::temp_dir()
        .join("chat-cli-account-icons")
        .join(format!("{prefix}-embedded-v3-{safe_id}.png"))
}

fn avatar_thumbnail_cache_key(key: &AvatarPreviewKey) -> String {
    let source = match key.source {
        AvatarPreviewSource::Avatar => "avatar",
        AvatarPreviewSource::AccountBadge => "account_badge",
    };
    format!(
        "v{}:{source}:{}:{}x{}",
        AVATAR_THUMBNAIL_CACHE_VERSION,
        key.path.to_string_lossy(),
        key.width,
        key.rows
    )
}

fn avatar_thumbnail_source_kind(key: &AvatarPreviewKey) -> &'static str {
    match key.source {
        AvatarPreviewSource::Avatar => "chat_avatar",
        AvatarPreviewSource::AccountBadge => "account_badge",
    }
}

fn avatar_source_metadata(path: &Path) -> (Option<i64>, Option<i64>) {
    let Ok(metadata) = fs::metadata(path) else {
        return (None, None);
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64);
    let size = i64::try_from(metadata.len()).ok();
    (modified, size)
}

fn avatar_thumbnail_record_is_stale(
    key: &AvatarPreviewKey,
    record: &AvatarThumbnailCacheRecord,
) -> bool {
    let (source_mtime, source_size) = avatar_source_metadata(&key.path);
    record.cache_version != AVATAR_THUMBNAIL_CACHE_VERSION
        || record.source_kind != avatar_thumbnail_source_kind(key)
        || record.source_path != key.path
        || record.source_mtime != source_mtime
        || record.source_size != source_size
}

fn avatar_thumbnail_record_to_rows(
    key: &AvatarPreviewKey,
    record: AvatarThumbnailCacheRecord,
) -> Result<AvatarPreviewData, String> {
    if record.image_format != "png" {
        return Err(format!(
            "unsupported cached avatar thumbnail format {}",
            record.image_format
        ));
    }
    let rows =
        message_list::image_preview_rows_from_bytes(&record.image_blob, key.width, key.rows)?;
    Ok(AvatarPreviewData {
        rows,
        thumbnail: Some(Arc::from(record.image_blob.into_boxed_slice())),
    })
}

fn decode_avatar_preview_with_thumbnail(
    key: &AvatarPreviewKey,
) -> Result<(AvatarPreviewData, AvatarThumbnailCacheUpsert), String> {
    let thumbnail = generate_avatar_thumbnail_png(&key.path)?;
    let rows = message_list::image_preview_rows_from_rgba(&thumbnail.image, key.width, key.rows);
    let (source_mtime, source_size) = avatar_source_metadata(&key.path);
    let thumbnail_bytes = thumbnail.png.clone();
    let data = AvatarPreviewData {
        rows,
        thumbnail: Some(Arc::from(thumbnail.png.into_boxed_slice())),
    };
    let upsert = AvatarThumbnailCacheUpsert {
        cache_key: avatar_thumbnail_cache_key(key),
        source_kind: avatar_thumbnail_source_kind(key).to_owned(),
        source_path: key.path.clone(),
        source_mtime,
        source_size,
        image_format: "png".to_owned(),
        image_blob: thumbnail_bytes,
        cache_version: AVATAR_THUMBNAIL_CACHE_VERSION,
    };
    Ok((data, upsert))
}

struct GeneratedAvatarThumbnail {
    image: image::RgbaImage,
    png: Vec<u8>,
}

fn generate_avatar_thumbnail_png(path: &Path) -> Result<GeneratedAvatarThumbnail, String> {
    let image = image::open(path)
        .map_err(|error| format!("decoding avatar image {}: {error}", path.display()))?;
    let mut thumbnail = resize_avatar_cover(image, AVATAR_THUMBNAIL_SIZE).to_rgba8();
    message_list::apply_rounded_thumbnail_mask(&mut thumbnail);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(thumbnail.clone())
        .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| format!("encoding avatar thumbnail {}: {error}", path.display()))?;
    Ok(GeneratedAvatarThumbnail {
        image: thumbnail,
        png,
    })
}

fn resize_avatar_cover(image: image::DynamicImage, size: u32) -> image::DynamicImage {
    let width = image.width().max(1);
    let height = image.height().max(1);
    let crop_size = width.min(height);
    let crop_x = width.saturating_sub(crop_size) / 2;
    let crop_y = height.saturating_sub(crop_size) / 2;
    image
        .crop_imm(crop_x, crop_y, crop_size, crop_size)
        .resize_exact(size, size, image::imageops::FilterType::Triangle)
}

#[cfg(test)]
fn account_badge_image_rows(path: &Path) -> Result<chat_list::AvatarRows, String> {
    let image = image::open(path)
        .map_err(|error| format!("decoding account badge image {}: {error}", path.display()))?
        .to_rgba8();
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Err(format!("account badge image {} is empty", path.display()));
    }

    let left_top = sample_image_region(&image, 0, 0, width / 2, height / 2);
    let left_bottom = sample_image_region(&image, 0, height / 2, width / 2, height);
    let right_top = sample_image_region(&image, width / 2, 0, width, height / 2);
    let right_bottom = sample_image_region(&image, width / 2, height / 2, width, height);

    Ok(vec![vec![
        Span::styled("▀", Style::default().fg(left_top).bg(left_bottom)),
        Span::styled("▀", Style::default().fg(right_top).bg(right_bottom)),
    ]])
}

#[cfg(test)]
fn sample_image_region(
    image: &image::RgbaImage,
    x_start: u32,
    y_start: u32,
    x_end: u32,
    y_end: u32,
) -> Color {
    let x_end = x_end.max(x_start + 1).min(image.width());
    let y_end = y_end.max(y_start + 1).min(image.height());
    let mut total_r = 0_u64;
    let mut total_g = 0_u64;
    let mut total_b = 0_u64;
    let mut total_a = 0_u64;
    let mut count = 0_u64;

    for y in y_start.min(image.height().saturating_sub(1))..y_end {
        for x in x_start.min(image.width().saturating_sub(1))..x_end {
            let pixel = image.get_pixel(x, y);
            let [r, g, b, a] = pixel.0;
            total_r += u64::from(r) * u64::from(a);
            total_g += u64::from(g) * u64::from(a);
            total_b += u64::from(b) * u64::from(a);
            total_a += u64::from(a);
            count += 1;
        }
    }

    if total_a == 0 || count == 0 {
        return Color::Black;
    }

    Color::Rgb(
        (total_r / total_a).min(u64::from(u8::MAX)) as u8,
        (total_g / total_a).min(u64::from(u8::MAX)) as u8,
        (total_b / total_a).min(u64::from(u8::MAX)) as u8,
    )
}

fn write_static_account_icon(path: &Path, icon_bytes: &[u8]) -> Result<()> {
    if path.exists() {
        image::open(path)
            .with_context(|| format!("decoding cached account icon {}", path.display()))?;
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    image::load_from_memory(icon_bytes).context("decoding embedded account icon")?;
    fs::write(path, icon_bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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
    let body = decode_link_response_body(
        response.bytes().await?.to_vec(),
        content_encoding.as_deref(),
    )?;

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
        io::Read::read_to_end(&mut decoder, &mut decoded)
            .context("decoding gzip link preview body")?;
        return Ok(decoded);
    }
    Ok(body)
}

fn is_image_response(url: &str, content_type: Option<&str>) -> bool {
    content_type.is_some_and(|content_type| content_type.to_ascii_lowercase().starts_with("image/"))
        || link_image_extension(url).is_some()
}

fn cached_link_image_media(url: &str, content_type: Option<&str>, body: &[u8]) -> Result<Media> {
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
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
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
            return Some(rest[..end].to_owned());
        }
        let end = value
            .find(|character: char| character.is_whitespace() || character == '>')
            .unwrap_or(value.len());
        return Some(value[..end].to_owned());
    }
    None
}

fn clean_html_text(value: &str) -> Option<String> {
    // Decode entities first (real metadata encodes markup as `&lt;p&gt;`), then
    // drop any resulting tags so card text never shows raw markup.
    let text = strip_html_tags(&html_unescape(value))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() { None } else { Some(text) }
}

/// Removes HTML tags (e.g. `<p>`, `</p>`, `<br>`) so fetched card text never
/// shows raw markup. A `<` is only treated as a tag start when followed by a
/// letter, `/`, or `!`, so plain text like `< 10` is preserved.
fn strip_html_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '<'
            && chars
                .peek()
                .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!'))
        {
            for inner in chars.by_ref() {
                if inner == '>' {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

fn html_unescape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(amp) = rest.find('&') {
        output.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let resolved = after.find(';').and_then(|semi| {
            let entity = &after[..semi];
            if entity.is_empty()
                || entity.len() > 10
                || !entity
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '#')
            {
                return None;
            }
            decode_html_entity(entity).map(|decoded| (decoded, semi))
        });
        match resolved {
            Some((decoded, semi)) => {
                output.push_str(&decoded);
                rest = &after[semi + 1..];
            }
            None => {
                output.push('&');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// Decodes a single HTML entity body (the text between `&` and `;`), supporting
/// named entities and numeric references such as `&#xce;` and `&#206;`.
fn decode_html_entity(entity: &str) -> Option<String> {
    if let Some(numeric) = entity.strip_prefix('#') {
        let code = if let Some(hex) = numeric.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            numeric.parse::<u32>().ok()?
        };
        return char::from_u32(code).map(String::from);
    }
    let decoded = match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => return None,
    };
    Some(decoded.to_string())
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
    use chrono::TimeZone;
    use crossterm::event::{KeyEventKind, KeyEventState, MouseEventKind};
    use ratatui::backend::TestBackend;
    use std::{path::PathBuf, sync::Mutex};

    #[test]
    fn compare_chats_places_unread_chats_first_by_recency_across_platforms() {
        let now = Utc::now();
        let account: ProviderId = Arc::from("test");
        let mut slack_read = test_chat(
            &account,
            Platform::Slack,
            "C-read",
            "slack read recent",
            ChatKind::PublicChannel,
        );
        slack_read.last_message_at = Some(now);

        let mut whatsapp_unread_old = test_chat(
            &account,
            Platform::WhatsApp,
            "W-unread-old",
            "whatsapp unread old",
            ChatKind::Direct,
        );
        whatsapp_unread_old.unread_count = 1;
        whatsapp_unread_old.last_message_at = Some(now - chrono::Duration::minutes(10));

        let mut slack_unread_new = test_chat(
            &account,
            Platform::Slack,
            "C-unread-new",
            "slack unread new",
            ChatKind::PublicChannel,
        );
        slack_unread_new.unread_count = 2;
        slack_unread_new.last_message_at = Some(now - chrono::Duration::minutes(1));

        let mut chats = [slack_read, whatsapp_unread_old, slack_unread_new];
        chats.sort_by(compare_chats_for_sidebar);

        assert_eq!(chats[0].name.as_ref(), "slack unread new");
        assert_eq!(chats[1].name.as_ref(), "whatsapp unread old");
        assert_eq!(chats[2].name.as_ref(), "slack read recent");
    }

    #[tokio::test]
    async fn app_navigation_refreshes_existing_slack_history_for_thread_updates() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:refresh");
        let chat = test_chat(
            &account_id,
            Platform::Slack,
            "C-refresh",
            "general",
            ChatKind::PublicChannel,
        );
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Refresh Slack"),
            avatar: None,
        };
        let mut cached_root = test_incoming_message(&chat, "1710000000.000100", "Alice", "Root");
        cached_root.thread_id = None;
        let mut refreshed_root = cached_root.clone();
        refreshed_root.thread_id = Some(refreshed_root.id.clone());
        let mut reply = test_incoming_message(&chat, "1710000001.000100", "Bob", "Reply");
        reply.reply_to = Some(refreshed_root.id.clone());
        reply.thread_id = Some(refreshed_root.id.clone());

        let provider = StaticTestProvider::with_account(
            account,
            vec![chat.clone()],
            vec![refreshed_root.clone(), reply.clone()],
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        app.store.upsert_message(&cached_root).await?;
        app.reload_selected_messages().await?;

        assert_eq!(app.state().messages().len(), 1);
        assert_eq!(app.state().messages()[0].thread_id, None);

        app.request_selected_chat_history_sync();
        app.reload_selected_messages_after_navigation().await?;
        tokio::task::yield_now().await;
        app.drain_provider_events().await?;

        assert_eq!(app.state().messages().len(), 2);
        let root = app
            .state()
            .messages()
            .iter()
            .find(|message| message.id == refreshed_root.id)
            .unwrap();
        assert_eq!(root.thread_id.as_ref(), Some(&refreshed_root.id));
        assert_eq!(app.thread_reply_count(&refreshed_root.id), 1);

        Ok(())
    }

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
            &[0, 1, 2, 3, 8, 4, 7, 5, 6, 9]
        );
        assert_eq!(app.state().filter(), "");
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        Ok(())
    }

    #[tokio::test]
    async fn app_applies_loaded_slack_member_names_to_raw_message_senders() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:test");
        let chat_id: ChatId = Arc::from("C123");
        let chat = Chat {
            id: chat_id.clone(),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("general"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Test Slack"),
            avatar: None,
        };
        let messages = vec![Message {
            id: Arc::from("1"),
            chat_id: chat_id.clone(),
            account: account_id.clone(),
            sender: Sender {
                platform_id: Arc::from("U123"),
                display_name: Arc::from("U123"),
                avatar: None,
            },
            timestamp: Utc::now(),
            edited_at: None,
            content: Content::Text(Arc::from("hello")),
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            mentions_me: false,
            platform_data: PlatformData::default(),
        }];
        let provider = StaticTestProvider::with_account(
            account,
            vec![chat],
            messages.clone(),
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        app.store.upsert_message(&messages[0]).await?;
        app.reload_selected_messages().await?;

        assert_eq!(
            app.state().messages()[0].sender.display_name.as_ref(),
            "U123"
        );
        app.chat_members_tx.send(ChatMembersFetchResult {
            account: account_id,
            chat_id,
            result: Ok(vec![Sender {
                platform_id: Arc::from("U123"),
                display_name: Arc::from("Alice Designer"),
                avatar: None,
            }]),
        })?;
        assert!(app.drain_chat_member_fetches());

        assert_eq!(
            app.state().messages()[0].sender.display_name.as_ref(),
            "Alice Designer"
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_handles_navigation_and_quit_keys() -> Result<()> {
        let mut app = test_app().await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;
        assert_eq!(app.state().selected_chat_index(), 1);
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Up, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;
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
    async fn filter_typing_schedules_discovery_without_blocking_message_load() -> Result<()> {
        let mut app = test_app().await?;
        let initial_message_count = app.state().messages().len();

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().filter_mode());

        app.handle_event(AppEvent::Key(key(KeyCode::Char('m'), KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Char('e'), KeyModifiers::NONE)))
            .await?;

        assert!(app.state().filter_mode());
        assert_eq!(app.state().filter(), "me");
        assert_eq!(app.state().messages().len(), initial_message_count);
        assert!(app.pending_selected_messages.is_none());
        assert!(app.pending_discovery_query.is_some());

        drain_async_app_work(&mut app).await?;
        assert!(app.pending_discovery_query.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn app_marks_unread_chat_read_when_opened() -> Result<()> {
        let mut app = test_app().await?;
        assert_eq!(app.state().selected_chat().unwrap().unread_count, 4);

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;

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
    async fn slack_polled_live_message_marks_background_channel_unread() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:workspace");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Engineering Slack"),
            avatar: None,
        };
        let active_chat = Chat {
            id: Arc::from("C-active"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("active"),
            avatar: None,
            is_group: true,
            kind: ChatKind::Group,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let background_chat = Chat {
            id: Arc::from("C-background"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("background"),
            avatar: None,
            is_group: true,
            kind: ChatKind::Group,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let provider = StaticTestProvider::with_account(
            account,
            vec![active_chat, background_chat.clone()],
            Vec::new(),
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        assert_eq!(app.state().selected_chat().unwrap().id.as_ref(), "C-active");

        let mut message = test_incoming_message(
            &background_chat,
            "slack:msg:background-live",
            "Maya",
            "new message from polling",
        );
        message.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::from("1710000000.000100"),
            thread_ts: None,
            channel: background_chat.id.clone(),
        });

        app.handle_event(AppEvent::Provider(
            account_id,
            Box::new(ProviderEvent::Message {
                message,
                is_historical: false,
            }),
        ))
        .await?;
        drain_async_app_work(&mut app).await?;

        let unread_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.id.as_ref() == "C-background")
            .unwrap();
        assert_eq!(unread_chat.unread_count, 1);
        let stored = app.store.get_all_chats().await?;
        let stored_chat = stored
            .iter()
            .find(|chat| chat.id.as_ref() == "C-background")
            .unwrap();
        assert_eq!(stored_chat.unread_count, 1);
        assert_eq!(app.state().selected_chat().unwrap().id.as_ref(), "C-active");

        Ok(())
    }

    /// Build a Slack thread-reply message that lands in `chat`, parented to
    /// `root`. Used by the thread-surfacing tests below.
    fn slack_thread_reply(
        chat: &Chat,
        id: &str,
        sender: &str,
        text: &str,
        root: &MessageId,
        ts: &str,
        thread_ts: &str,
    ) -> Message {
        let mut message = test_incoming_message(chat, id, sender, text);
        message.thread_id = Some(root.clone());
        message.reply_to = Some(root.clone());
        message.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::from(ts),
            thread_ts: Some(Arc::from(thread_ts)),
            channel: chat.id.clone(),
        });
        message
    }

    fn slack_thread_test_chats(account_id: &ProviderId) -> (Chat, Chat) {
        let make = |id: &str, name: &str| Chat {
            id: Arc::from(id),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from(name),
            avatar: None,
            is_group: true,
            kind: ChatKind::Group,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        (
            make("C-active", "active"),
            make("C-background", "background"),
        )
    }

    #[tokio::test]
    async fn live_thread_reply_marks_thread_unread_and_clears_on_open() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:workspace");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Engineering Slack"),
            avatar: None,
        };
        let (active_chat, background_chat) = slack_thread_test_chats(&account_id);
        let root: MessageId = Arc::from("slack:msg:root");
        // Seed the thread root so the reply has a parent to open later.
        let mut root_message = test_incoming_message(
            &background_chat,
            "slack:msg:root",
            "Priya",
            "can we finalise the token names?",
        );
        root_message.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::from("1710000000.000100"),
            thread_ts: None,
            channel: background_chat.id.clone(),
        });
        let provider = StaticTestProvider::with_account(
            account,
            vec![active_chat, background_chat.clone()],
            vec![root_message],
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        assert_eq!(app.state().selected_chat().unwrap().id.as_ref(), "C-active");

        let reply = slack_thread_reply(
            &background_chat,
            "slack:msg:reply-1",
            "Maya",
            "shipping it",
            &root,
            "1710000000.000200",
            "1710000000.000100",
        );

        app.handle_event(AppEvent::Provider(
            account_id.clone(),
            Box::new(ProviderEvent::Message {
                message: reply,
                is_historical: false,
            }),
        ))
        .await?;
        drain_async_app_work(&mut app).await?;

        // The per-thread unread is tracked in storage and surfaced in the
        // sidebar aggregate, distinct from channel unread.
        assert_eq!(app.store.thread_unread_count(&account_id, &root).await?, 1);
        assert_eq!(
            app.state
                .thread_unread_by_chat
                .get(&background_chat.id)
                .copied(),
            Some(1)
        );

        // Selecting the chat and opening the thread clears the unread counter.
        let background_index = app
            .state
            .chats
            .iter()
            .position(|chat| chat.id.as_ref() == "C-background")
            .unwrap();
        app.activate_chat_index(background_index);
        drain_async_app_work(&mut app).await?;
        app.open_thread(root.clone());
        // Opening focuses the thread compose so the user can reply immediately.
        assert_eq!(app.state.focus, FocusPane::Details);
        app.flush_pending_thread_read().await?;

        assert_eq!(app.store.thread_unread_count(&account_id, &root).await?, 0);
        assert!(
            app.state
                .thread_unread_by_chat
                .get(&background_chat.id)
                .is_none()
        );

        Ok(())
    }

    #[tokio::test]
    async fn live_thread_reply_notification_says_replied_to_a_thread() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:workspace");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Engineering Slack"),
            avatar: None,
        };
        let (active_chat, background_chat) = slack_thread_test_chats(&account_id);
        let root: MessageId = Arc::from("slack:msg:root");
        let provider = StaticTestProvider::with_account(
            account,
            vec![active_chat, background_chat.clone()],
            Vec::new(),
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        app.settings.notifications = NotificationMode::Desktop;
        let capture = capture_desktop_notifications(&mut app);

        let reply = slack_thread_reply(
            &background_chat,
            "slack:msg:reply-1",
            "Maya",
            "shipping it",
            &root,
            "1710000000.000200",
            "1710000000.000100",
        );

        app.handle_event(AppEvent::Provider(
            account_id.clone(),
            Box::new(ProviderEvent::Message {
                message: reply,
                is_historical: false,
            }),
        ))
        .await?;
        drain_async_app_work(&mut app).await?;
        // Force the queued notification to deliver on the next tick.
        assert_eq!(app.state().pending_notification_count(), 1);
        app.state.pending_notifications[0].deliver_at = Instant::now();
        app.handle_event(AppEvent::Tick).await?;

        let sent = capture.lock().unwrap();
        assert!(
            sent.iter()
                .any(|notification| notification.sender_name.contains("replied to a thread")),
            "expected a thread-aware desktop notification, got {sent:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn historical_thread_reply_replay_does_not_bump_unread() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:workspace");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Engineering Slack"),
            avatar: None,
        };
        let (active_chat, background_chat) = slack_thread_test_chats(&account_id);
        let root: MessageId = Arc::from("slack:msg:root");
        let provider = StaticTestProvider::with_account(
            account,
            vec![active_chat, background_chat.clone()],
            Vec::new(),
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        let reply = slack_thread_reply(
            &background_chat,
            "slack:msg:reply-1",
            "Maya",
            "shipping it",
            &root,
            "1710000000.000200",
            "1710000000.000100",
        );

        // Replaying the same reply as historical catch-up must store it without
        // ever inflating the per-thread unread counter (idempotent replay).
        for _ in 0..3 {
            app.handle_event(AppEvent::Provider(
                account_id.clone(),
                Box::new(ProviderEvent::Message {
                    message: reply.clone(),
                    is_historical: true,
                }),
            ))
            .await?;
        }
        drain_async_app_work(&mut app).await?;

        assert_eq!(app.store.thread_unread_count(&account_id, &root).await?, 0);
        assert!(app.state.thread_unread_by_chat.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn threads_inbox_lists_unread_thread_and_opens_it() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:workspace");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Engineering Slack"),
            avatar: None,
        };
        let (active_chat, background_chat) = slack_thread_test_chats(&account_id);
        let root: MessageId = Arc::from("slack:msg:root");
        let mut root_message = test_incoming_message(
            &background_chat,
            "slack:msg:root",
            "Priya",
            "can we finalise the token names?",
        );
        root_message.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::from("1710000000.000100"),
            thread_ts: None,
            channel: background_chat.id.clone(),
        });
        let provider = StaticTestProvider::with_account(
            account,
            vec![active_chat, background_chat.clone()],
            vec![root_message.clone()],
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        // Deliver the root through the event path so it is reliably persisted
        // (and therefore loadable into the thread pane) regardless of history
        // sync timing.
        app.handle_event(AppEvent::Provider(
            account_id.clone(),
            Box::new(ProviderEvent::Message {
                message: root_message,
                is_historical: true,
            }),
        ))
        .await?;

        let reply = slack_thread_reply(
            &background_chat,
            "slack:msg:reply-1",
            "Maya",
            "shipping it",
            &root,
            "1710000000.000200",
            "1710000000.000100",
        );
        app.handle_event(AppEvent::Provider(
            account_id.clone(),
            Box::new(ProviderEvent::Message {
                message: reply,
                is_historical: false,
            }),
        ))
        .await?;
        drain_async_app_work(&mut app).await?;

        // The Threads inbox aggregates threads with new replies, newest first.
        app.open_threads_inbox().await?;
        let inbox = app
            .state
            .threads_inbox
            .as_ref()
            .expect("threads inbox should be open");
        assert_eq!(inbox.entries.len(), 1);
        assert_eq!(inbox.entries[0].root_id.as_ref(), root.as_ref());
        assert_eq!(inbox.entries[0].unread_reply_count, 1);

        // Activating the entry selects its chat and opens the thread pane via
        // the same navigation the rest of the app uses (no new shortcut):
        // Enter on the selected inbox row.
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;
        assert_eq!(
            app.state.thread_root.as_ref().map(|id| id.as_ref()),
            Some(root.as_ref())
        );
        assert_eq!(app.state.focus, FocusPane::Details);

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
        let mut app = test_app_with_providers(vec![Arc::new(mock), Arc::new(second)]).await?;
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
        assert!(content.contains("Secondary Account"));

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
    async fn app_add_account_wizard_creates_demo_provider_from_factory() -> Result<()> {
        let factory: AccountProviderFactory = Arc::new(|kind| match kind {
            AccountProviderKind::Demo => Ok(Arc::new(MockProvider::new()) as ProviderBox),
            _ => bail!("unexpected provider kind"),
        });
        let mut app = test_app_with_factory(Vec::new(), factory).await?;
        let mut terminal = Terminal::new(TestBackend::new(120, 32))?;

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(app.state().account_setup_open());

        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Connect chat app"));
        assert!(content.contains("Slack"));
        assert!(content.contains("WhatsApp"));
        assert!(content.contains("Demo"));

        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.drain_provider_events().await?;

        assert!(!app.state().account_setup_open());
        assert_eq!(app.state().chats().len(), 10);
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("mock:local")
        );
        assert!(app.state().status().contains("sync complete"));

        Ok(())
    }

    #[tokio::test]
    async fn app_slack_setup_submission_loads_chats_without_restart() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:runtime");
        let chat = Chat {
            id: Arc::from("C123"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("general"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Runtime Slack"),
            avatar: None,
        };
        let provider = StaticTestProvider::with_account(
            account,
            vec![chat],
            Vec::new(),
            OutboundCapabilities::all(),
        )
        .requiring_auth_until_submit();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        app.drain_provider_events().await?;
        assert!(app.state().slack_setup_open());
        assert!(app.state().chats().is_empty());

        app.submit_current_slack_setup().await?;

        assert_eq!(app.state().chats().len(), 1);
        assert_eq!(app.state().messages().len(), 0);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("general")
        );
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("slack:runtime")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_bundled_oauth_app_starts_browser_login_without_client_credentials() -> Result<()> {
        let provider = StaticTestProvider::slack_setup("slack:bundled", "Bundled Slack")
            .requiring_auth_until_submit()
            .with_bundled_oauth_app();
        let submissions = provider.auth_submissions.clone();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        app.drain_provider_events().await?;
        assert!(app.state().slack_setup_open());
        // The overlay records that a bundled official app is available so the UI
        // can skip manual app creation and client ID/secret entry.
        assert!(
            app.state
                .slack_setup
                .as_ref()
                .expect("slack setup overlay")
                .bundled_oauth_app
        );

        // Move to the OAuth prompt for the default User OAuth mode and submit
        // without entering any client ID/secret or token.
        {
            let setup = app.state.slack_setup.as_mut().expect("slack setup overlay");
            setup.phase = SlackSetupPhase::OAuthPrompt;
            setup.selected_mode = 0; // User OAuth
            setup.credentials = SlackSetupCredentials::default();
        }

        app.submit_current_slack_setup().await?;

        // The submission is dispatched as a background browser login (deferred
        // post-auth load) even though no client credentials were entered.
        assert!(app.state.pending_slack_setup_load.is_some());
        assert!(
            app.state()
                .status()
                .contains("waiting for Slack browser sign-in")
        );

        // Let the spawned background submission run, then confirm it ran with no
        // client credentials (the official/bundled app supplies them).
        for _ in 0..16 {
            if !submissions.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let recorded = submissions.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        let submission = &recorded[0];
        assert!(submission.client_id.is_none());
        assert!(submission.client_secret.is_none());
        assert!(submission.user_token.is_none());
        assert_eq!(submission.mode, Some(AuthSubmissionMode::UserOAuth));

        Ok(())
    }

    #[tokio::test]
    async fn app_bundled_oauth_auth_challenge_does_not_open_manual_manifest() -> Result<()> {
        let provider = StaticTestProvider::slack_setup("slack:bundled-auth", "Bundled Slack")
            .with_bundled_oauth_app();
        let provider_id = provider.id().clone();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        let manual_manifest_url =
            Arc::<str>::from("https://api.slack.com/apps?new_app=1&manifest_yaml=manual-template");

        app.open_slack_setup_for_provider(
            &provider_id,
            Some(&AuthChallenge::OAuthUrl(manual_manifest_url)),
            None,
        );

        let setup = app.state.slack_setup.as_ref().expect("slack setup overlay");
        assert!(setup.bundled_oauth_app);
        assert_eq!(setup.phase, SlackSetupPhase::OAuthPrompt);
        assert_eq!(setup.selected_mode(), SlackSetupMode::Automatic);
        assert!(
            setup.oauth_url.is_none(),
            "startup manual manifest URL must not be stored for bundled app flow"
        );
        assert!(
            setup
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("built-in chat-cli app")
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("built-in Slack app"));
        assert!(content.contains("no app creation or Client ID/Secret needed"));
        assert!(!content.contains("api.slack.com/apps?new_app=1"));

        Ok(())
    }

    #[tokio::test]
    async fn app_bundled_oauth_with_configured_realtime_hides_app_token_field() -> Result<()> {
        let provider = StaticTestProvider::slack_setup("slack:bundled-realtime", "Bundled Slack")
            .with_bundled_oauth_app()
            .with_configured_realtime();
        let provider_id = provider.id().clone();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        app.open_slack_setup_for_provider(
            &provider_id,
            Some(&AuthChallenge::OAuthUrl(Arc::from(
                "https://api.slack.com/apps?new_app=1&manifest_yaml=manual-template",
            ))),
            None,
        );

        let setup = app.state.slack_setup.as_ref().expect("slack setup overlay");
        assert!(setup.bundled_oauth_app);
        assert!(setup.configured_realtime);
        assert_eq!(setup.selected_mode(), SlackSetupMode::Automatic);
        assert!(setup.credential_fields().is_empty());

        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Realtime is already configured from startup settings"));
        assert!(!content.contains("Optional realtime field"));
        assert!(!content.contains("App token:"));
        assert!(!content.contains("xapp- App-Level Token"));

        Ok(())
    }

    #[tokio::test]
    async fn app_bundled_oauth_advanced_user_oauth_still_shows_manual_setup() -> Result<()> {
        let provider = StaticTestProvider::slack_setup("slack:bundled-advanced", "Bundled Slack")
            .with_bundled_oauth_app();
        let provider_id = provider.id().clone();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        let manual_manifest_url =
            Arc::<str>::from("https://api.slack.com/apps?new_app=1&manifest_yaml=manual-template");

        app.open_slack_setup_for_provider(
            &provider_id,
            Some(&AuthChallenge::OAuthUrl(manual_manifest_url)),
            None,
        );
        {
            let setup = app.state.slack_setup.as_mut().expect("slack setup overlay");
            assert_eq!(setup.selected_mode(), SlackSetupMode::Automatic);
            setup.selected_mode = 1; // Advanced User OAuth with a self-managed Slack app.
            setup.oauth_url = Some(
                "https://api.slack.com/apps?new_app=1&manifest_yaml=manual-template".to_owned(),
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("User OAuth"));
        assert!(content.contains("Slack app creation URL"));
        assert!(content.contains("api.slack.com/apps?new_app=1"));
        assert!(!content.contains("Primary action: press Enter to open Slack"));

        Ok(())
    }

    #[tokio::test]
    async fn app_slack_setup_help_page_explains_workspaces_and_toggles() -> Result<()> {
        let provider = StaticTestProvider::slack_setup("slack:help", "Help Slack")
            .requiring_auth_until_submit();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        app.drain_provider_events().await?;
        assert!(app.state().slack_setup_open());

        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;

        // '?' opens the contextual help page.
        app.handle_event(AppEvent::Key(key(KeyCode::Char('?'), KeyModifiers::NONE)))
            .await?;
        assert!(
            app.state
                .slack_setup
                .as_ref()
                .expect("slack setup overlay")
                .show_help
        );
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("How Slack sign-in works"));
        assert!(content.contains("Multiple workspaces"));
        assert!(content.contains("Approvals"));
        assert!(content.contains("team:read"));

        // '?' again closes it and returns to the setup flow.
        app.handle_event(AppEvent::Key(key(KeyCode::Char('?'), KeyModifiers::NONE)))
            .await?;
        assert!(
            !app.state
                .slack_setup
                .as_ref()
                .expect("slack setup overlay")
                .show_help
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_slack_setup_connected_can_add_another_workspace() -> Result<()> {
        let factory: AccountProviderFactory = Arc::new(|kind| match kind {
            AccountProviderKind::Slack => Ok(Arc::new(
                StaticTestProvider::slack_setup("slack:second", "Second Slack")
                    .requiring_auth_until_submit(),
            ) as ProviderBox),
            _ => bail!("unexpected provider kind"),
        });
        let first = StaticTestProvider::slack_setup("slack:first", "First Slack");
        let mut app = test_app_with_factory(vec![Arc::new(first)], factory).await?;

        // Simulate a finished first-workspace sign-in sitting on the Connected
        // screen, where the "Add another Slack workspace" action is offered.
        let mut overlay =
            SlackSetupOverlay::new(Arc::from("slack:first"), "First Slack".to_owned());
        overlay.phase = SlackSetupPhase::Connected;
        app.state.slack_setup = Some(overlay);

        // 'a' starts an independent setup for a brand-new workspace.
        app.handle_event(AppEvent::Key(key(KeyCode::Char('a'), KeyModifiers::NONE)))
            .await?;

        // A second Slack provider now exists and its own setup overlay is open.
        assert!(
            app.providers
                .iter()
                .any(|provider| provider.id().as_ref() == "slack:second")
        );
        assert!(app.state().slack_setup_open());
        assert_eq!(
            app.state
                .slack_setup
                .as_ref()
                .expect("slack setup overlay")
                .provider_id
                .as_ref(),
            "slack:second"
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_bootstrap_loads_only_selected_chat_history() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:large");
        let chat_one = Chat {
            id: Arc::from("C001"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("general"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let chat_two = Chat {
            id: Arc::from("C002"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("random"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("Large Slack"),
            avatar: None,
        };
        let message_one =
            test_incoming_message(&chat_one, "slack:msg:one", "Alice", "Loaded first");
        let message_two =
            test_incoming_message(&chat_two, "slack:msg:two", "Bob", "Deferred second");
        let provider = StaticTestProvider::with_account(
            account,
            vec![chat_one, chat_two],
            vec![message_one, message_two],
            OutboundCapabilities::all(),
        );

        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        assert_eq!(app.state().chats().len(), 2);
        assert_eq!(app.state().messages().len(), 0);

        tokio::task::yield_now().await;
        app.drain_provider_events().await?;

        assert_eq!(app.state().messages().len(), 1);
        assert_eq!(
            app.state()
                .messages()
                .first()
                .map(|message| message.id.as_ref()),
            Some("slack:msg:one")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_navigation_loads_chat_history_on_demand_once() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:ondemand");
        let chat_one = Chat {
            id: Arc::from("C101"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("general"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let chat_two = Chat {
            id: Arc::from("C102"),
            account: account_id.clone(),
            platform: Platform::Slack,
            name: Arc::from("random"),
            avatar: None,
            is_group: true,
            kind: ChatKind::PublicChannel,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        };
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: Arc::from("On Demand Slack"),
            avatar: None,
        };
        let message_one =
            test_incoming_message(&chat_one, "slack:msg:first", "Alice", "First channel");
        let message_two =
            test_incoming_message(&chat_two, "slack:msg:second", "Bob", "Second channel");
        let provider = StaticTestProvider::with_account(
            account,
            vec![chat_one, chat_two],
            vec![message_one, message_two],
            OutboundCapabilities::all(),
        );
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;

        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;

        assert_eq!(
            app.state().selected_chat().map(|chat| chat.id.as_ref()),
            Some("C102")
        );
        assert_eq!(app.state().messages().len(), 0);
        assert!(app.state().status().contains("loading recent messages"));

        drain_async_app_work(&mut app).await?;

        assert_eq!(app.state().messages().len(), 1);
        assert_eq!(
            app.state()
                .messages()
                .first()
                .map(|message| message.id.as_ref()),
            Some("slack:msg:second")
        );

        Ok(())
    }

    #[tokio::test]
    async fn app_add_account_wizard_routes_slack_and_whatsapp_auth_challenges() -> Result<()> {
        let factory: AccountProviderFactory = Arc::new(|kind| match kind {
            AccountProviderKind::Slack => Ok(Arc::new(StaticTestProvider::slack_setup(
                "slack:runtime",
                "Runtime Slack",
            )) as ProviderBox),
            AccountProviderKind::WhatsApp => Ok(Arc::new(StaticTestProvider::whatsapp_setup(
                "whatsapp:runtime",
                "Runtime WhatsApp",
            )) as ProviderBox),
            AccountProviderKind::Demo => Ok(Arc::new(MockProvider::new()) as ProviderBox),
        });
        let mut app = test_app_with_factory(Vec::new(), factory).await?;

        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.drain_provider_events().await?;
        assert!(app.state().slack_setup_open());
        assert_eq!(
            app.state().slack_setup_phase_label(),
            Some("enter credentials")
        );
        assert!(!app.state().auth_overlay_open());

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::End, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        app.drain_provider_events().await?;
        assert!(!app.state().account_setup_open());
        assert!(app.state().auth_overlay_open());
        assert_eq!(
            app.state().active_account().map(|id| id.as_ref()),
            Some("whatsapp:runtime")
        );

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
        drain_async_app_work(&mut app).await?;

        assert_eq!(app.state().filter(), "media");
        let expected_media_index = app
            .state()
            .chats()
            .iter()
            .position(|chat| chat.name.as_ref() == "Media Samples")
            .expect("media chat should exist");
        assert_eq!(app.state().visible_chat_indices(), &[expected_media_index]);
        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        assert_eq!(app.state().messages().len(), 2);
        assert!(app.pending_selected_messages.is_none());

        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().filter_mode());
        drain_async_app_work(&mut app).await?;
        assert_eq!(app.state().messages().len(), 3);

        app.handle_event(AppEvent::Key(key(KeyCode::Esc, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().filter(), "");
        assert_eq!(
            app.state().visible_chat_indices(),
            &[0, 1, 2, 3, 8, 4, 7, 5, 6, 9]
        );

        Ok(())
    }

    #[tokio::test]
    async fn ctrl_f_filter_scope_follows_focused_pane() -> Result<()> {
        let mut app = test_app().await?;

        // Filtering from the chat list searches chats/contacts.
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().filter_mode());
        assert_eq!(app.state().focus(), FocusPane::ChatList);

        for value in "media".chars() {
            app.handle_event(AppEvent::Key(key(KeyCode::Char(value), KeyModifiers::NONE)))
                .await?;
        }
        drain_async_app_work(&mut app).await?;
        let media_index = app
            .state()
            .chats()
            .iter()
            .position(|chat| chat.name.as_ref() == "Media Samples")
            .expect("media chat should exist");
        assert_eq!(app.state().visible_chat_indices(), &[media_index]);

        // Moving focus into the Messages pane switches the filter to message
        // scope, so the chat list is no longer narrowed by the query.
        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().filter_mode());
        assert!(app.state().visible_chat_indices().len() > 1);

        // Moving focus back to the chat list restores chat-scoped filtering.
        app.handle_event(AppEvent::Key(key(KeyCode::Left, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().focus(), FocusPane::ChatList);
        assert!(app.state().filter_mode());
        assert_eq!(app.state().visible_chat_indices(), &[media_index]);

        Ok(())
    }

    #[tokio::test]
    async fn filter_mode_allows_arrow_selection_in_messages() -> Result<()> {
        let mut app = test_app().await?;

        // Open a chat so the Messages pane is focused with messages loaded.
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert!(app.state().messages().len() > 1);

        // Enter the filter while focused on Messages.
        app.handle_event(AppEvent::Key(key(
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
        )))
        .await?;
        assert!(app.state().filter_mode());

        // Arrow keys still move the message selection while filtering.
        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        let first = app.state().selected_message_id().cloned();
        assert!(first.is_some());

        app.handle_event(AppEvent::Key(key(KeyCode::Down, KeyModifiers::NONE)))
            .await?;
        let second = app.state().selected_message_id().cloned();
        assert!(second.is_some());

        // The chat list stays unfiltered because the filter is message-scoped.
        assert!(app.state().visible_chat_indices().len() > 1);

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
        drain_async_app_work(&mut app).await?;
        assert_eq!(app.selected_visible_position(), Some(9));
        assert_eq!(app.state().selected_chat_index(), 9);
        assert_eq!(app.state().messages().len(), 1);

        app.handle_event(AppEvent::Key(key(KeyCode::Home, KeyModifiers::NONE)))
            .await?;
        drain_async_app_work(&mut app).await?;
        assert_eq!(app.selected_visible_position(), Some(0));
        assert!(!app.state().messages().is_empty());

        app.handle_event(AppEvent::Key(key(KeyCode::PageDown, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.selected_visible_position(), Some(5));
        assert_eq!(app.state().selected_chat_index(), 4);

        app.handle_event(AppEvent::Key(key(KeyCode::PageUp, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.selected_visible_position(), Some(0));

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
        assert_eq!(
            app.state().compose_text(),
            "Hello compo!se ✅ 😂 (╯°□°)╯︵ ┻━┻"
        );

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
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅ 😂 (╯°□°)╯︵ ┻━┻")
        }));
        let provider_history = app.providers[0]
            .history(&chat_id, None, HISTORY_LIMIT)
            .await?;
        assert!(provider_history.iter().any(|message| {
            message.is_from_me
                && matches!(&message.content, Content::Text(text) if text.as_ref() == "Hello compo!se ✅ 😂 (╯°□°)╯︵ ┻━┻")
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
        let provider =
            StaticTestProvider::from_mock("text:only", "Text Only", &mock, "text:only:")?
                .with_outbound_capabilities(OutboundCapabilities::default());
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
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

        let attachment = app
            .state()
            .pending_attachment()
            .expect("pending attachment");
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
            4,
        )))
        .await?;
        drain_async_app_work(&mut app).await?;
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

        select_chat_by_name(&mut app, "Media Samples").await?;

        assert_eq!(
            app.state().selected_chat().map(|chat| chat.name.as_ref()),
            Some("Media Samples")
        );
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;
        for _ in 0..100 {
            app.drain_media_preview_fetches();
            if app.pending_media_previews.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        terminal.draw(|frame| app.draw(frame))?;
        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!content.contains("actual image:"));
        assert!(!content.contains("mock-screenshot.png"));
        assert!(content.contains("Shared an image attachment"));
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
            4,
        )))
        .await?;
        drain_async_app_work(&mut app).await?;
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
        for _ in 0..100 {
            app.drain_media_preview_fetches();
            if app.pending_media_previews.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
        drain_async_app_work(&mut app).await?;
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
        let menu_rect = app.action_menu_rect(app.state.frame_area, &menu);
        let react_row = menu_rect.y
            + 2
            + menu
                .items
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
        menu.selected = menu
            .items
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
        select_chat_by_name(&mut app, "#project-chat-cli").await?;
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
        assert!(content.contains("#project-chat-cli"));
        assert!(content.contains("Original message"));
        assert!(content.contains("Reply in thread"));
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
    async fn whatsapp_bootstrap_preserves_stored_unread_count_when_provider_snapshot_is_zero()
    -> Result<()> {
        let account_id: ProviderId = Arc::from("whatsapp:preserve-unread");
        let chat_id: ChatId = Arc::from("whatsapp:15551234567@s.whatsapp.net");
        let account = Account {
            id: account_id.clone(),
            platform: Platform::WhatsApp,
            display_name: Arc::from("Personal WhatsApp"),
            avatar: None,
        };
        let mut stored_chat = Chat {
            id: chat_id.clone(),
            account: account_id.clone(),
            platform: Platform::WhatsApp,
            name: Arc::from("Today Contact"),
            avatar: None,
            is_group: false,
            kind: ChatKind::Direct,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 2,
            muted: false,
            pinned: false,
            last_message_at: Some(Utc::now()),
            last_message_preview: Some(Arc::from("two unread messages")),
            thread_id: None,
        };
        let store = Arc::new(Store::open_memory().await?);
        store.upsert_account(&account, "{}").await?;
        store.upsert_chat(&stored_chat).await?;

        stored_chat.unread_count = 0;
        let provider = StaticTestProvider::with_account(
            account,
            vec![stored_chat],
            Vec::new(),
            OutboundCapabilities::all(),
        );

        let app = App::new(store.clone(), vec![Arc::new(provider)]).await?;

        assert_eq!(app.state().selected_chat().unwrap().unread_count, 2);
        assert_eq!(store.get_all_chats().await?[0].unread_count, 2);
        Ok(())
    }

    #[tokio::test]
    async fn app_opens_slack_setup_for_unconfigured_slack_provider() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:engineering", "Engineering Slack");
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        assert!(content.contains("Slack app creation URL"));
        assert!(content.contains("https://slack.com/oauth"));
        assert!(!content.contains("Authentication"));

        Ok(())
    }

    #[tokio::test]
    async fn app_updates_slack_setup_after_validation_events() -> Result<()> {
        let slack = StaticTestProvider::slack_setup("slack:validated", "Validated Slack");
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        let mut app = test_app_with_providers(vec![Arc::new(slack)]).await?;
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
        app.settings.notifications = NotificationMode::InApp;
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

        assert!(!app.state().notification_visible());
        assert_eq!(app.state().pending_notification_count(), 1);
        app.state.pending_notifications[0].deliver_at = Instant::now();
        app.handle_event(AppEvent::Tick).await?;

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
    async fn app_shows_account_notice_overlay_even_when_notifications_are_off() -> Result<()> {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::Off;
        let provider_id = app.state().selected_chat().unwrap().account.clone();

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::AccountNotice {
                title: Arc::from("Slack realtime unavailable"),
                body: Arc::from(
                    "Using periodic Slack history checks because realtime is unavailable.",
                ),
                severity: AccountNoticeSeverity::Info,
            }),
        ))
        .await?;

        assert!(app.state().notification_visible());
        assert_eq!(
            app.state().notification_preview(),
            Some("Using periodic Slack history checks because realtime is unavailable.")
        );
        assert!(app.state().status().contains("Slack realtime unavailable"));

        let mut terminal = Terminal::new(TestBackend::new(120, 30))?;
        terminal.draw(|frame| app.draw(frame))?;
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Notification"));
        assert!(content.contains("Slack realtime unavailable"));
        assert!(content.contains("Using periodic Slack history checks"));

        Ok(())
    }

    #[tokio::test]
    async fn app_system_alert_notice_triggers_system_notification_even_when_off() -> Result<()> {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::Off;
        let capture = capture_desktop_notifications(&mut app);
        let provider_id = app.state().selected_chat().unwrap().account.clone();

        app.handle_event(AppEvent::Provider(
            provider_id,
            Box::new(ProviderEvent::AccountNotice {
                title: Arc::from("Slack realtime unavailable"),
                body: Arc::from(
                    "Using periodic Slack history checks because realtime is unavailable.",
                ),
                severity: AccountNoticeSeverity::SystemAlert,
            }),
        ))
        .await?;

        let sent = capture.lock().expect("capture poisoned").clone();
        assert_eq!(sent.len(), 1, "system alert must dispatch one notification");
        assert_eq!(sent[0].chat_name, "Slack realtime unavailable");
        assert_eq!(
            sent[0].preview.as_deref(),
            Some("Using periodic Slack history checks because realtime is unavailable.")
        );

        // The in-app overlay must still appear alongside the system notification.
        assert!(app.state().notification_visible());
        assert!(app.state().status().contains("Slack realtime unavailable"));

        Ok(())
    }

    #[tokio::test]
    async fn app_suppresses_and_dismisses_notification_overlay() -> Result<()> {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::InApp;
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
                    "Active chat queues unless attended",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert!(!app.state().notification_visible());
        assert_eq!(app.state().pending_notification_count(), 1);
        app.handle_event(AppEvent::Key(key(KeyCode::Enter, KeyModifiers::NONE)))
            .await?;
        assert_eq!(app.state().pending_notification_count(), 0);

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
        assert_eq!(app.state().pending_notification_count(), 0);

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
        assert_eq!(app.state().pending_notification_count(), 0);

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
        app.state.pending_notifications[0].deliver_at = Instant::now();
        app.handle_event(AppEvent::Tick).await?;
        assert!(app.state().notification_visible());

        app.handle_event(AppEvent::Key(key(KeyCode::Right, KeyModifiers::NONE)))
            .await?;
        assert!(!app.state().notification_visible());

        Ok(())
    }

    #[tokio::test]
    async fn app_notification_scope_direct_and_mentions_filters_groups() -> Result<()> {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::InApp;
        app.settings.notification_scope = NotificationScope::DirectAndMentions;

        let direct_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();
        assert!(matches!(direct_chat.kind, ChatKind::Direct));

        let group_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| matches!(chat.kind, ChatKind::Group) && !chat.muted)
            .unwrap()
            .clone();

        // Direct messages always notify under the restricted scope.
        app.handle_event(AppEvent::Provider(
            direct_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &direct_chat,
                    "mock:msg:scope:direct",
                    "Alice Chen",
                    "Direct hello",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);

        // Group messages without a mention are suppressed.
        app.handle_event(AppEvent::Provider(
            group_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: test_incoming_message(
                    &group_chat,
                    "mock:msg:scope:group",
                    "Maya",
                    "Group chatter",
                ),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);

        // Group messages that mention the user are eligible.
        let mut mention = test_incoming_message(
            &group_chat,
            "mock:msg:scope:mention",
            "Maya",
            "Hey @me, look here",
        );
        mention.mentions_me = true;
        app.handle_event(AppEvent::Provider(
            group_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: mention,
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn app_notify_self_messages_defaults_on_and_can_still_be_disabled_by_config() -> Result<()>
    {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::InApp;

        let direct_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();

        // Default (on): a message the user sent themselves is eligible, so
        // sending yourself a note from a web client exercises notifications.
        let mut own_on =
            test_incoming_message(&direct_chat, "mock:msg:self:on", "Me", "note to self");
        own_on.is_from_me = true;
        app.handle_event(AppEvent::Provider(
            direct_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: own_on,
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);

        // Existing config files that explicitly stored false still preserve that
        // value, even though the menu no longer exposes the setting.
        app.settings.notify_self_messages = false;
        let mut own_off = test_incoming_message(
            &direct_chat,
            "mock:msg:self:off",
            "Me",
            "note to self suppressed",
        );
        own_off.is_from_me = true;
        app.handle_event(AppEvent::Provider(
            direct_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: own_off,
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn app_notification_modes_pause_incoming_and_suppress_self() -> Result<()> {
        let mut app = test_app().await?;
        app.settings.notifications = NotificationMode::Off;
        let background_chat = app
            .state()
            .chats()
            .iter()
            .find(|chat| chat.name.as_ref() == "Alice Chen")
            .unwrap()
            .clone();

        let incoming =
            |id: &str, text: &str| test_incoming_message(&background_chat, id, "Alice Chen", text);

        // Notifications off: nothing queued.
        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: incoming("mock:msg:notify:off", "Off mode message"),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 0);

        // Paused: still nothing queued even though notifications are enabled.
        app.settings.notifications = NotificationMode::InApp;
        app.state.notification_pause.paused_until = Some(Utc::now() + ChronoDuration::minutes(25));
        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: incoming("mock:msg:notify:paused", "Paused message"),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 0);

        // Existing config files can still suppress the user's own outgoing
        // messages, even though notify-self defaults to enabled.
        app.settings.notify_self_messages = false;
        app.state.notification_pause.paused_until = Some(Utc::now() - ChronoDuration::seconds(1));
        let mut self_message = incoming("mock:msg:notify:self", "My own reply");
        self_message.is_from_me = true;
        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: self_message,
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 0);

        // A genuine incoming message now queues a notification.
        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: incoming("mock:msg:notify:one", "First incoming"),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);
        assert!(!app.state().notification_visible());

        // A second incoming message for the same chat coalesces into one pending.
        app.handle_event(AppEvent::Provider(
            background_chat.account.clone(),
            Box::new(ProviderEvent::Message {
                message: incoming("mock:msg:notify:two", "Latest coalesced message"),
                is_historical: false,
            }),
        ))
        .await?;
        assert_eq!(app.state().pending_notification_count(), 1);
        app.state.pending_notifications[0].deliver_at = Instant::now();
        app.handle_event(AppEvent::Tick).await?;
        assert_eq!(
            app.state().notification_preview(),
            Some("Latest coalesced message")
        );

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
        let blank_row_offset = (0..content_area.height as usize)
            .find(|offset| {
                let clicked_line = app.state.message_scroll.saturating_add(*offset);
                !app.state.message_hits.iter().any(|hit| {
                    hit.line_hits
                        .iter()
                        .any(|line_hit| line_hit.line == clicked_line)
                        || hit
                            .avatar_hit
                            .as_ref()
                            .is_some_and(|avatar_hit| avatar_hit.line == clicked_line)
                        || hit
                            .thread_summary_hit
                            .as_ref()
                            .is_some_and(|summary_hit| summary_hit.line == clicked_line)
                }) && !app
                    .state
                    .media_hits
                    .iter()
                    .any(|hit| clicked_line >= hit.start_line && clicked_line <= hit.end_line)
            })
            .expect("message pane should contain a blank row");

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x,
            content_area.y + blank_row_offset as u16,
        )))
        .await?;
        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), None);
        assert!(!app.state().action_menu_open());

        let avatar_hit = hit.avatar_hit.as_ref().unwrap();
        let line_row = content_area
            .y
            .saturating_add(app.state.message_top_padding as u16)
            .saturating_add(line_hit.line.saturating_sub(app.state.message_scroll) as u16);
        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            content_area.x + avatar_hit.end_col + 1,
            line_row,
        )))
        .await?;

        assert_eq!(app.state().focus(), FocusPane::Messages);
        assert_eq!(app.state().selected_message_id(), Some(&hit.message_id));
        assert!(app.state().action_menu_open());
        assert_eq!(app.state().status(), "message actions opened");

        Ok(())
    }

    #[tokio::test]
    async fn app_ignores_stale_message_click_hits_after_history_prepend() -> Result<()> {
        let mut app = test_app().await?;
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| app.draw(frame))?;

        assert!(!app.state.message_hits.is_empty());
        let stale_hit_count = app.state.message_hits.len();
        let content_area = inner_area(app.state.pane_areas.messages);
        let hit = app
            .state
            .message_hits
            .iter()
            .find_map(|hit| {
                hit.line_hits
                    .first()
                    .map(|line_hit| (hit.message_id.clone(), line_hit.clone()))
            })
            .expect("draw should produce a message click hit");
        let click_row = content_area
            .y
            .saturating_add(app.state.message_top_padding as u16)
            .saturating_add(hit.1.line.saturating_sub(app.state.message_scroll) as u16);
        let click_column = content_area.x.saturating_add(hit.1.start_col);

        let selected_chat = app
            .state
            .selected_chat()
            .expect("test app should have a selected chat")
            .clone();
        let mut older = test_incoming_message(
            &selected_chat,
            "mock:msg:older:prepend",
            "Alice Chen",
            "Older history page inserted before the visible messages",
        );
        older.timestamp = app
            .state
            .messages
            .first()
            .expect("test app should have loaded messages")
            .timestamp
            - ChronoDuration::minutes(1);

        app.merge_older_messages_into_current_chat(
            vec![older],
            selected_chat.name.as_ref(),
            false,
            selected_chat.platform,
        );

        assert_eq!(app.state.message_hits.len(), 0);
        assert_eq!(app.state.media_hits.len(), 0);
        assert_ne!(stale_hit_count, 0);
        assert!(app.state.selected_message_id() != Some(&hit.0));

        app.handle_event(AppEvent::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            click_column,
            click_row,
        )))
        .await?;

        assert_eq!(app.state().selected_message_id(), None);
        assert!(!app.state().action_menu_open());
        assert_eq!(app.state().status(), "messages focused");

        terminal.draw(|frame| app.draw(frame))?;
        assert!(!app.state.message_hits.is_empty());

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
        select_chat_by_name(&mut app, "Media Samples").await?;
        app.state.message_scroll = 0;

        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| app.draw(frame))?;
        for _ in 0..100 {
            app.drain_media_preview_fetches();
            if app.pending_media_previews.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
        for _ in 0..100 {
            app.drain_avatar_preview_fetches();
            app.drain_media_preview_fetches();
            if app.pending_avatar_previews.is_empty()
                && app.pending_media_previews.is_empty()
                && !app.avatar_preview_cache.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        terminal.draw(|frame| app.draw(frame))?;
        let buffer = terminal.backend().buffer();

        let (avatar_start, avatar_end) =
            chat_list::avatar_column_bounds(app.state().pane_areas.chat_list);
        assert!(
            (avatar_start..avatar_end).any(|x| rgb_cell_at(buffer, x, 2))
                && (avatar_start..avatar_end).any(|x| rgb_cell_at(buffer, x, 3)),
            "chat list should render a square-ish real PNG avatar in the first visible chat row"
        );
        let message_area = app.state().pane_areas.messages;
        let message_inner_x = message_area.x.saturating_add(1)
            ..message_area
                .x
                .saturating_add(message_area.width.saturating_sub(1));
        let message_inner_y = message_area.y.saturating_add(1)
            ..message_area
                .y
                .saturating_add(message_area.height.saturating_sub(1));
        assert!(
            message_inner_x
                .clone()
                .any(|x| message_inner_y.clone().any(|y| rgb_cell_at(buffer, x, y))),
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
        select_chat_by_name(&mut app, "Media Samples").await?;

        let messages = app.state().messages().to_vec();
        let render = message_list::build_message_lines(
            &messages,
            80,
            0,
            200,
            None,
            &HashSet::new(),
            &HashMap::new(),
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
                && line_text(line).contains("Shared an image attachment")
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
    fn html_metadata_parser_decodes_numeric_entities_and_strips_markup() {
        let html = r#"
            <html>
              <head>
                <meta property="og:title" content="&#xce;n drume&#x21b;ie">
                <meta property="og:description" content="&lt;p&gt;Cele 7 Legi Universale &amp; mai mult&lt;/p&gt;">
              </head>
            </html>
        "#;

        assert_eq!(
            html_meta_content(html, "og:title").as_deref(),
            Some("În drumeție")
        );
        assert_eq!(
            html_meta_content(html, "og:description").as_deref(),
            Some("Cele 7 Legi Universale & mai mult")
        );
    }

    #[test]
    fn action_menu_items_are_specific_to_message_content() {
        let link_message =
            test_message_with_content(Content::LinkPreview(chat_core::LinkPreview {
                url: Arc::<str>::from("https://example.com/story"),
                title: Some(Arc::<str>::from("Story")),
                description: None,
                image: None,
            }));
        let link_items = action_menu_items_for_message(&link_message, 0);
        assert!(link_items.contains(&ActionMenuItem::OpenLink));
        assert!(link_items.contains(&ActionMenuItem::Forward));
        assert!(!link_items.contains(&ActionMenuItem::OpenImage));
        assert!(!link_items.contains(&ActionMenuItem::VotePoll));

        let poll_content = Content::Poll(test_poll());
        let poll_message = test_message_with_content(poll_content.clone());
        let poll_items = action_menu_items_for_message(&poll_message, 0);
        assert!(poll_items.contains(&ActionMenuItem::VotePoll));
        assert!(poll_items.contains(&ActionMenuItem::Forward));
        assert!(!poll_items.contains(&ActionMenuItem::OpenLink));
        assert!(!poll_items.contains(&ActionMenuItem::OpenImage));
    }

    #[test]
    fn forward_content_uses_portable_form_and_provider_capabilities() {
        let text_only = OutboundCapabilities::default();
        let link = Content::LinkPreview(chat_core::LinkPreview {
            url: Arc::<str>::from("https://example.com/story"),
            title: Some(Arc::<str>::from("Story")),
            description: None,
            image: None,
        });
        assert!(matches!(
            forward_content_for_capabilities(&link, &text_only),
            Some(Content::Text(text)) if text.as_ref() == "https://example.com/story"
        ));

        let media = Media {
            id: Arc::<str>::from("photo-1"),
            file_name: Arc::<str>::from("photo.jpg"),
            mime_type: Arc::<str>::from("image/jpeg"),
            size_bytes: None,
            caption: None,
            local_path: Some(PathBuf::from("/tmp/photo.jpg")),
            thumbnail: None,
        };
        assert!(
            forward_content_for_capabilities(&Content::Image(media.clone()), &text_only).is_none()
        );
        assert!(matches!(
            forward_content_for_capabilities(&Content::Image(media), &OutboundCapabilities::all()),
            Some(Content::Image(_))
        ));
        let poll_content = Content::Poll(test_poll());
        assert!(
            forward_content_for_capabilities(&poll_content, &OutboundCapabilities::all()).is_none()
        );
    }

    #[test]
    fn forward_picker_filters_destinations_by_chat_and_account_text() {
        let picker = ForwardPicker {
            message_id: Arc::from("source"),
            selected: 0,
            query: "slack maria".to_owned(),
            targets: vec![
                test_forward_target("whatsapp", "Mihai", "WhatsApp · WhatsApp"),
                test_forward_target("slack", "Maria Popescu", "Erepublik · Slack"),
                test_forward_target("slack-2", "General", "Work · Slack"),
            ],
        };

        assert_eq!(forward_picker_filtered_indices(&picker), vec![1]);
        assert_eq!(forward_picker_filtered_len(&picker), 1);
        let (_, target) = forward_picker_selected_target(&picker).expect("matching target");
        assert_eq!(target.label, "Maria Popescu");

        let empty = ForwardPicker {
            query: "missing".to_owned(),
            ..picker
        };
        assert!(forward_picker_filtered_indices(&empty).is_empty());
        assert!(forward_picker_selected_target(&empty).is_none());
    }

    #[test]
    fn forward_picker_scroll_start_uses_filtered_length() {
        assert_eq!(forward_picker_scroll_start(10, 5, 3), 0);
        assert_eq!(forward_picker_scroll_start(7, 5, 20), 3);
        assert_eq!(forward_picker_scroll_start(0, 5, 0), 0);
    }

    #[test]
    fn display_helpers_hide_placeholder_captions_and_clean_link_text() {
        let media = Media {
            id: Arc::<str>::from("photo-1"),
            file_name: Arc::<str>::from("photo.jpg"),
            mime_type: Arc::<str>::from("image/jpeg"),
            size_bytes: None,
            caption: Some(Arc::<str>::from("[empty WhatsApp message]")),
            local_path: Some(PathBuf::from("/tmp/photo.jpg")),
            thumbnail: None,
        };
        let image = Content::Image(media.clone());
        assert_eq!(content_send_preview(&image), "Image");
        assert_eq!(content_copy_text(&image), "photo.jpg");
        assert_eq!(media_image_preview(&media), None);

        let link = Content::LinkPreview(chat_core::LinkPreview {
            url: Arc::<str>::from("https://example.com/story"),
            title: Some(Arc::<str>::from("&#xce;n drume&#x21b;ie")),
            description: Some(Arc::<str>::from("&lt;p&gt;Cele 7 Legi&lt;/p&gt;")),
            image: None,
        });
        assert_eq!(content_send_preview(&link), "În drumeție");
        assert_eq!(
            content_copy_text(&link),
            "În drumeție: https://example.com/story — Cele 7 Legi"
        );
    }

    #[test]
    fn avatar_thumbnail_generation_crops_and_persists_32px_png() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide-avatar.png");
        let mut image = image::RgbaImage::new(64, 32);
        for y in 0..32 {
            for x in 0..64 {
                let pixel = if x < 16 {
                    image::Rgba([255, 0, 0, 255])
                } else if x < 48 {
                    image::Rgba([0, 255, 0, 255])
                } else {
                    image::Rgba([0, 0, 255, 255])
                };
                image.put_pixel(x, y, pixel);
            }
        }
        image.save(&path)?;

        let key = AvatarPreviewKey {
            path,
            width: chat_list::CHAT_AVATAR_WIDTH,
            rows: chat_list::CHAT_AVATAR_ROWS,
            source: AvatarPreviewSource::Avatar,
        };
        let (rows, upsert) =
            decode_avatar_preview_with_thumbnail(&key).map_err(anyhow::Error::msg)?;
        let thumbnail = image::load_from_memory(&upsert.image_blob)?.to_rgba8();

        assert_eq!(
            thumbnail.dimensions(),
            (AVATAR_THUMBNAIL_SIZE, AVATAR_THUMBNAIL_SIZE)
        );
        assert_eq!(thumbnail.get_pixel(0, 0).0, [0, 255, 0, 0]);
        assert_eq!(thumbnail.get_pixel(16, 16).0, [0, 255, 0, 255]);
        assert_eq!(upsert.image_format, "png");
        assert_eq!(upsert.cache_version, AVATAR_THUMBNAIL_CACHE_VERSION);
        assert_eq!(upsert.source_kind, "chat_avatar");
        assert_eq!(rows.rows.len(), usize::from(chat_list::CHAT_AVATAR_ROWS));
        Ok(())
    }

    #[test]
    fn avatar_thumbnail_sqlite_record_hydrates_rows_and_detects_stale_source() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("avatar.png");
        let original = image::RgbaImage::from_pixel(32, 32, image::Rgba([12, 34, 56, 255]));
        original.save(&path)?;
        let key = AvatarPreviewKey {
            path: path.clone(),
            width: chat_list::CHAT_AVATAR_WIDTH,
            rows: chat_list::CHAT_AVATAR_ROWS,
            source: AvatarPreviewSource::Avatar,
        };
        let (_, upsert) = decode_avatar_preview_with_thumbnail(&key).map_err(anyhow::Error::msg)?;
        let fresh_record = AvatarThumbnailCacheRecord {
            cache_key: upsert.cache_key.clone(),
            source_kind: upsert.source_kind.clone(),
            source_path: upsert.source_path.clone(),
            source_mtime: upsert.source_mtime,
            source_size: upsert.source_size,
            image_format: upsert.image_format.clone(),
            image_blob: upsert.image_blob.clone(),
            cache_version: upsert.cache_version,
            updated_at: 1,
        };

        let hydrated = avatar_thumbnail_record_to_rows(&key, fresh_record.clone())
            .map_err(anyhow::Error::msg)?;
        assert_eq!(
            hydrated.rows.len(),
            usize::from(chat_list::CHAT_AVATAR_ROWS)
        );
        assert!(!avatar_thumbnail_record_is_stale(&key, &fresh_record));

        let changed = image::RgbaImage::from_pixel(64, 64, image::Rgba([90, 80, 70, 255]));
        changed.save(&path)?;
        assert!(avatar_thumbnail_record_is_stale(&key, &fresh_record));
        Ok(())
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

    async fn select_chat_by_name(app: &mut App, name: &str) -> Result<()> {
        let position = app
            .state
            .visible_chat_indices
            .iter()
            .position(|index| app.state.chats[*index].name.as_ref() == name)
            .with_context(|| format!("chat {name} should be visible"))?;
        if app.select_visible_position(position) {
            app.reload_selected_messages_after_navigation_with_options(true, false)
                .await?;
            drain_async_app_work(app).await?;
        }
        Ok(())
    }

    async fn drain_async_app_work(app: &mut App) -> Result<()> {
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let changed = app.drain_provider_events().await?;
            if !changed
                && app.pending_selected_messages.is_none()
                && app.pending_discovery_query.is_none()
                && app.state.loading_history_chats.is_empty()
                && app.state.loading_chat_members.is_empty()
                && app.pending_media_previews.is_empty()
                && app.pending_avatar_previews.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    async fn test_app() -> Result<App> {
        test_app_with_providers(vec![Arc::new(MockProvider::new())]).await
    }

    async fn test_app_with_providers(providers: Vec<ProviderBox>) -> Result<App> {
        let store = Arc::new(Store::open_memory().await?);
        App::new(store, providers).await
    }

    async fn test_app_with_factory(
        providers: Vec<ProviderBox>,
        factory: AccountProviderFactory,
    ) -> Result<App> {
        let store = Arc::new(Store::open_memory().await?);
        App::new_with_factory(store, providers, Some(factory)).await
    }

    /// Replace the app's desktop notifier with a capturing one so tests can
    /// assert which system notifications were dispatched.
    fn capture_desktop_notifications(app: &mut App) -> Arc<Mutex<Vec<MessageNotification>>> {
        let (notifier, capture) = DesktopNotifier::with_capture();
        app.desktop_notifier = notifier;
        capture
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
        require_auth_until_submit: bool,
        bundled_oauth_app: bool,
        configured_realtime: bool,
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
            Self::with_account(
                account,
                Vec::new(),
                Vec::new(),
                OutboundCapabilities::default(),
            )
        }

        fn whatsapp_setup(id: &str, display_name: &str) -> Self {
            let id = Arc::<str>::from(id);
            let account = Account {
                id: id.clone(),
                platform: Platform::WhatsApp,
                display_name: Arc::from(display_name),
                avatar: None,
            };
            Self::with_account(account, Vec::new(), Vec::new(), OutboundCapabilities::all())
        }

        fn with_account(
            account: Account,
            chats: Vec<Chat>,
            messages: Vec<Message>,
            outbound_capabilities: OutboundCapabilities,
        ) -> Self {
            let id = account.id.clone();
            Self {
                id,
                account,
                chats,
                messages,
                events: EventBus::new(),
                auth_submissions: Arc::new(Mutex::new(Vec::new())),
                submit_error: None,
                outbound_capabilities,
                require_auth_until_submit: false,
                bundled_oauth_app: false,
                configured_realtime: false,
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
                require_auth_until_submit: false,
                bundled_oauth_app: false,
                configured_realtime: false,
            })
        }

        fn with_submit_error(mut self, error: &str) -> Self {
            self.submit_error = Some(Arc::from(error));
            self
        }

        fn requiring_auth_until_submit(mut self) -> Self {
            self.require_auth_until_submit = true;
            self
        }

        fn with_bundled_oauth_app(mut self) -> Self {
            self.bundled_oauth_app = true;
            self
        }

        fn with_configured_realtime(mut self) -> Self {
            self.configured_realtime = true;
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
            if self.require_auth_until_submit && self.auth_submissions.lock().unwrap().is_empty() {
                self.events
                    .send(ProviderEvent::AuthRequired(AuthChallenge::Waiting));
                return Ok(());
            }
            if self.account.platform == Platform::Slack && self.chats.is_empty() {
                self.events
                    .send(ProviderEvent::AuthRequired(AuthChallenge::Waiting));
                return Ok(());
            }
            if self.account.platform == Platform::WhatsApp && self.chats.is_empty() {
                self.events
                    .send(ProviderEvent::AuthRequired(AuthChallenge::QrCode(
                        Arc::from("test-whatsapp-qr"),
                    )));
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
            !self.require_auth_until_submit || !self.auth_submissions.lock().unwrap().is_empty()
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

        fn has_bundled_oauth_app(&self) -> bool {
            self.bundled_oauth_app
        }

        fn has_configured_realtime(&self) -> bool {
            self.configured_realtime
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

        async fn discover_destinations(
            &self,
            query: &str,
            limit: usize,
        ) -> Result<Vec<DiscoveryResult>> {
            let query = query.trim().to_lowercase();
            if query.is_empty() || limit == 0 {
                return Ok(Vec::new());
            }
            Ok(self
                .chats
                .iter()
                .filter(|chat| {
                    chat.name.to_lowercase().contains(&query)
                        || chat
                            .last_message_preview
                            .as_deref()
                            .is_some_and(|preview| preview.to_lowercase().contains(&query))
                })
                .take(limit)
                .cloned()
                .map(DiscoveryResult::existing_chat)
                .collect())
        }

        async fn chat_members(&self, chat_id: &Arc<str>) -> Result<Vec<Sender>> {
            let mut members = self
                .messages
                .iter()
                .filter(|message| message.chat_id == *chat_id)
                .map(|message| message.sender.clone())
                .collect::<Vec<_>>();
            members.sort_by_key(|member| member.display_name.to_ascii_lowercase());
            members.dedup_by(|left, right| left.platform_id == right.platform_id);
            Ok(members)
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

    fn test_chat(
        account: &ProviderId,
        platform: Platform,
        id: &str,
        name: &str,
        kind: ChatKind,
    ) -> Chat {
        Chat {
            id: Arc::from(id),
            account: account.clone(),
            platform,
            name: Arc::from(name),
            avatar: None,
            is_group: !matches!(kind, ChatKind::Direct),
            kind,
            membership: ChatMembership::Joined,
            is_shared: false,
            unread_count: 0,
            muted: false,
            pinned: false,
            last_message_at: None,
            last_message_preview: None,
            thread_id: None,
        }
    }

    #[tokio::test]
    async fn archive_sync_is_opt_in_for_current_accounts() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:archive");
        let chat = test_chat(
            &account_id,
            Platform::Slack,
            "C-archive",
            "archive channel",
            ChatKind::PublicChannel,
        );
        let mut provider = StaticTestProvider::with_account(
            Account {
                id: account_id.clone(),
                platform: Platform::Slack,
                display_name: Arc::from("Archive Slack"),
                avatar: None,
            },
            vec![chat.clone()],
            vec![test_incoming_message(&chat, "m1", "Ada", "hello archive")],
            OutboundCapabilities::default(),
        );
        provider.events = EventBus::new();
        let mut app = test_app_with_providers(vec![Arc::new(provider)]).await?;
        app.drain_provider_events().await?;

        assert!(app.state.monthly_backfill_ready_accounts.is_empty());
        assert!(!app.archive_running_for_current_accounts());

        app.toggle_archive_for_current_accounts();
        assert!(app.archive_running_for_current_accounts());
        assert!(
            app.state
                .monthly_backfill_ready_accounts
                .contains(&account_id)
        );

        app.toggle_archive_for_current_accounts();
        assert!(!app.archive_running_for_current_accounts());
        assert!(
            !app.state
                .monthly_backfill_ready_accounts
                .contains(&account_id)
        );
        Ok(())
    }

    #[tokio::test]
    async fn archive_candidate_fetches_empty_chats_then_older_history() -> Result<()> {
        let account_id: ProviderId = Arc::from("slack:archive-candidates");
        let chat = test_chat(
            &account_id,
            Platform::Slack,
            "C-empty",
            "empty archive channel",
            ChatKind::PublicChannel,
        );
        let mut app = test_app_with_providers(vec![Arc::new(StaticTestProvider::with_account(
            Account {
                id: account_id.clone(),
                platform: Platform::Slack,
                display_name: Arc::from("Archive Slack"),
                avatar: None,
            },
            vec![chat.clone()],
            Vec::new(),
            OutboundCapabilities::default(),
        ))])
        .await?;
        app.state
            .monthly_backfill_ready_accounts
            .insert(account_id.clone());
        app.state.loading_history_chats.clear();

        assert_eq!(
            app.archive_candidate_before_for_chat(&chat, archive_sync_floor())
                .await?,
            Some(None)
        );

        let mut old_message = test_incoming_message(&chat, "old", "Ada", "older cached message");
        old_message.timestamp = Utc::now() - ChronoDuration::days(5);
        app.store.upsert_message(&old_message).await?;

        let archive_candidate = app
            .archive_candidate_before_for_chat(&chat, archive_sync_floor())
            .await?;
        assert_eq!(
            archive_candidate.map(|before| before.map(|timestamp| timestamp.timestamp_millis())),
            Some(Some(old_message.timestamp.timestamp_millis()))
        );
        Ok(())
    }

    #[test]
    fn merge_history_pages_deduplicates_sorts_and_limits() {
        let account: ProviderId = Arc::from("whatsapp:bridge");
        let chat = test_chat(
            &account,
            Platform::WhatsApp,
            "whatsapp:123@s.whatsapp.net",
            "Ada",
            ChatKind::Direct,
        );
        let mut newest = test_incoming_message(&chat, "newest", "Ada", "newest");
        newest.timestamp = Utc.with_ymd_and_hms(2026, 6, 6, 12, 0, 0).unwrap();
        let mut local = test_incoming_message(&chat, "local", "Ada", "local");
        local.timestamp = Utc.with_ymd_and_hms(2026, 6, 6, 11, 0, 0).unwrap();
        let mut duplicate_local = local.clone();
        duplicate_local.timestamp = Utc.with_ymd_and_hms(2026, 6, 6, 11, 30, 0).unwrap();
        let mut older = test_incoming_message(&chat, "older", "Ada", "older");
        older.timestamp = Utc.with_ymd_and_hms(2026, 6, 6, 10, 0, 0).unwrap();

        let merged =
            merge_history_pages(vec![local], vec![duplicate_local, newest], vec![older], 3);

        assert_eq!(
            merged
                .iter()
                .map(|message| message.id.as_ref())
                .collect::<Vec<_>>(),
            vec!["older", "local", "newest"]
        );
    }

    #[test]
    fn network_activity_recent_counts_use_rolling_window() {
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();
        let mut activity = AccountNetworkActivity::default();
        activity.record(
            NetworkActivityDirection::Rx,
            now - ChronoDuration::seconds(61),
        );
        activity.record(
            NetworkActivityDirection::Rx,
            now - ChronoDuration::seconds(30),
        );
        activity.record(
            NetworkActivityDirection::Tx,
            now - ChronoDuration::seconds(1),
        );

        assert_eq!(activity.recent_rx_count(now), 1);
        assert_eq!(activity.recent_tx_count(now), 1);
        assert!(activity.tx_active(now - ChronoDuration::milliseconds(500)));
        assert!(activity.recent_tx_changed(now + ChronoDuration::seconds(1)));
        assert!(!activity.recent_tx_changed(now + ChronoDuration::seconds(4)));
        assert_eq!(network_activity_count_text(123), "123");
        activity.prune(now + ChronoDuration::seconds(2));
        assert!(!activity.tx_active(now + ChronoDuration::seconds(2)));
    }

    #[test]
    fn network_activity_display_cycles_through_final_modes() {
        assert_eq!(
            next_network_activity_display(NetworkActivityDisplay::Hidden),
            NetworkActivityDisplay::CombinedLights
        );
        assert_eq!(
            next_network_activity_display(NetworkActivityDisplay::CombinedLights),
            NetworkActivityDisplay::RecentCounts
        );
        assert_eq!(
            next_network_activity_display(NetworkActivityDisplay::RecentCounts),
            NetworkActivityDisplay::Hidden
        );
    }

    #[test]
    fn typing_preview_text_formats_single_pair_and_group() {
        assert_eq!(
            typing_preview_text(&["Ada"]).as_deref(),
            Some("Ada is typing…")
        );
        assert_eq!(
            typing_preview_text(&["Ada", "Bob"]).as_deref(),
            Some("Ada, Bob typing…")
        );
        assert_eq!(
            typing_preview_text(&["Ada", "Bob", "Cy"]).as_deref(),
            Some("Several people typing…")
        );
    }

    #[test]
    fn account_badge_image_rows_sample_actual_image_pixels() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("badge.png");
        let mut image = image::RgbaImage::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                let pixel = match (x < 2, y < 2) {
                    (true, true) => image::Rgba([255, 0, 0, 255]),
                    (false, true) => image::Rgba([0, 255, 0, 255]),
                    (true, false) => image::Rgba([0, 0, 255, 255]),
                    (false, false) => image::Rgba([255, 255, 0, 255]),
                };
                image.put_pixel(x, y, pixel);
            }
        }
        image.save(&path)?;

        let rows = account_badge_image_rows(&path).map_err(anyhow::Error::msg)?;

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0][0].content.as_ref(), "▀");
        assert_eq!(rows[0][0].style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(rows[0][0].style.bg, Some(Color::Rgb(0, 0, 255)));
        assert_eq!(rows[0][1].style.fg, Some(Color::Rgb(0, 255, 0)));
        assert_eq!(rows[0][1].style.bg, Some(Color::Rgb(255, 255, 0)));
        Ok(())
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
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn test_message_with_content(content: Content) -> Message {
        Message {
            id: Arc::from("test-message"),
            chat_id: Arc::from("test-chat"),
            account: Arc::from("mock:test"),
            sender: Sender {
                platform_id: Arc::from("mock:user:sender"),
                display_name: Arc::from("Sender"),
                avatar: None,
            },
            timestamp: Utc::now(),
            edited_at: None,
            content,
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: false,
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn test_forward_target(account: &str, label: &str, subtitle: &str) -> ForwardTarget {
        ForwardTarget {
            account: Arc::from(account),
            chat_id: Arc::from(label),
            label: label.to_owned(),
            subtitle: subtitle.to_owned(),
        }
    }

    fn test_poll() -> Poll {
        Poll {
            question: Arc::<str>::from("Lunch?"),
            options: vec![chat_core::PollOption {
                id: Arc::<str>::from("pizza"),
                label: Arc::<str>::from("Pizza"),
            }],
            selectable_options_count: Some(1),
            votes: Vec::new(),
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
