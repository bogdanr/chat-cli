use crate::theme::Theme;
use chat_core::{Content, Message, MessageId, ReceiptKind, Sender, Timestamp};
use chrono::{DateTime, Datelike, Local};
use image::imageops::FilterType;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::video::{self, VideoInfo};

const MEDIA_PREVIEW_MAX_WIDTH: u16 = 48;
const MEDIA_PREVIEW_ROWS: u16 = 8;
/// Videos get a taller preview than generic media: a 16:9 poster at 12 rows
/// fills most of the 48-cell card instead of a postage stamp.
const VIDEO_PREVIEW_ROWS: u16 = 12;
const ROUNDED_THUMBNAIL_CORNER_RADIUS_RATIO: f32 = 10.0 / 32.0;
const ROUNDED_THUMBNAIL_MASK_SAMPLES: u32 = 4;
const LINK_PREVIEW_CARD_WIDTH: u16 = 42;
const LINK_PREVIEW_THUMBNAIL_ROWS: u16 = 4;
/// Width (in terminal cells) of a single image thumbnail when laying inline
/// image cards out side by side, and the gap between adjacent thumbnails. Fixed
/// so the line-count pass can mirror the rendered layout without decoding.
const INLINE_IMAGE_COLUMN_WIDTH: u16 = 24;
const INLINE_IMAGE_COLUMN_GAP: u16 = 2;
pub const MESSAGE_AVATAR_WIDTH: u16 = 2;
pub const MESSAGE_AVATAR_ROWS: u16 = 1;
const BUBBLE_MAX_PERCENT: u16 = 72;
const BUBBLE_MIN_WIDTH: usize = 10;
const SLACK_TIMESTAMP_WIDTH: usize = 5;
const SLACK_GUTTER_GAP: usize = 2;
const SLACK_AVATAR_GAP: usize = 1;
const SLACK_BODY_INDENT_WIDTH: usize =
    SLACK_TIMESTAMP_WIDTH + SLACK_GUTTER_GAP + MESSAGE_AVATAR_WIDTH as usize + SLACK_AVATAR_GAP;
const SLACK_MESSAGE_SPACER_LINES: usize = 1;
/// Label shown next to messages whose text was edited after sending.
const EDITED_MARKER: &str = "edited";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationPresentation {
    Bubbles,
    Flat,
}

pub struct MessageListProps<'a> {
    pub title: &'a str,
    pub lines: Vec<Line<'static>>,
    pub total_lines: usize,
    pub scroll: usize,
    pub focused: bool,
    pub theme: Theme,
}

#[derive(Clone, Debug, Default)]
pub struct LinkMetadata {
    pub title: Option<Arc<str>>,
    pub description: Option<Arc<str>>,
    pub image_url: Option<Arc<str>>,
    pub image: Option<chat_core::Media>,
}

pub type LinkMetadataCache = HashMap<Arc<str>, LinkMetadata>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkPreviewRequest {
    pub message_id: Arc<str>,
    pub url: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct MediaHit {
    pub start_line: usize,
    pub end_line: usize,
    pub start_col: u16,
    pub end_col: u16,
    pub preview_start_col: u16,
    pub preview_end_col: u16,
    pub path: PathBuf,
    pub preview_path: PathBuf,
    pub title: String,
    pub caption: Option<String>,
    /// When set, the media bytes are not cached locally and must be fetched
    /// on demand: activating the hit should trigger a provider download of
    /// this media instead of opening the (missing) file.
    pub retrieve: Option<chat_core::Media>,
    /// When set, the hit is a locally cached video: activating it should hand
    /// the file to the system video player instead of the image viewer.
    pub play: Option<PathBuf>,
    /// Rows at the top of the hit (from `start_line`) that belong to the card
    /// chrome rather than the image, e.g. a clickable title row. HD overlays
    /// start below them so they never cover text.
    pub preview_skip_rows: usize,
    /// Draw a play badge over the preview. The halfblock rows already carry
    /// one; HD overlays must redraw it on top of the terminal image.
    pub play_badge: bool,
    /// When set, the hit is a locally cached document or voice note: the
    /// file is handed to the system app (PDF viewer, player, …) the way the
    /// native apps open attachments. Such hits carry no inline image.
    pub open: Option<PathBuf>,
}

/// True when this media is too large for automatic caching and its bytes are
/// not available locally yet, i.e. the card should offer on-demand retrieval.
/// Requires a reserved `local_path` (the provider's download destination) so
/// media that cannot be downloaded at all never advertises retrieval.
pub fn media_awaits_retrieve(media: &chat_core::Media) -> bool {
    media
        .size_bytes
        .is_some_and(|size| size > chat_core::MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES)
        && media.local_path.as_ref().is_some_and(|path| !path.exists())
        && media.thumbnail.as_ref().is_none_or(|path| !path.exists())
}

/// Resolves the open/preview paths and the optional retrieve payload for a
/// media hit. Returns `None` when the media is neither previewable nor
/// awaiting on-demand retrieval, in which case no hit should be registered.
fn media_hit_paths(
    media: &chat_core::Media,
    source: &Option<PathBuf>,
    error: &Option<String>,
) -> Option<(PathBuf, PathBuf, Option<chat_core::Media>)> {
    let preview = match (source, error) {
        (Some(path), None) => Some(path.clone()),
        _ => None,
    };
    let retrieve = media_awaits_retrieve(media).then(|| media.clone());
    if preview.is_none() && retrieve.is_none() {
        return None;
    }
    let anchor = preview
        .clone()
        .or_else(|| media.local_path.clone())
        .unwrap_or_default();
    let path = media_open_source(media).unwrap_or_else(|| anchor.clone());
    Some((path, anchor, retrieve))
}

#[derive(Clone, Debug)]
pub struct MessageLineHit {
    pub line: usize,
    pub start_col: u16,
    pub end_col: u16,
}

#[derive(Clone, Debug)]
pub struct MessageHit {
    pub start_line: usize,
    pub end_line: usize,
    pub message_id: Arc<str>,
    pub line_hits: Vec<MessageLineHit>,
    pub avatar_hit: Option<MessageLineHit>,
    pub avatar_path: Option<PathBuf>,
    pub thread_summary_hit: Option<MessageLineHit>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MediaPreviewKey {
    pub path: PathBuf,
    pub width: u16,
    pub rows: u16,
}

#[derive(Clone, Debug)]
pub struct MediaPreviewRequest {
    pub key: MediaPreviewKey,
}

#[derive(Debug, Default)]
pub struct MediaPreviewCache {
    previews: HashMap<MediaPreviewKey, Result<Vec<Vec<Span<'static>>>, String>>,
    /// Probed video metadata/poster keyed by the local video path. Filled by
    /// background ffprobe/ffmpeg workers; draw code only reads it.
    videos: HashMap<PathBuf, Result<Arc<VideoInfo>, String>>,
    /// Local video paths the draw pass saw without cached info. The app drains
    /// this queue after drawing and spawns blocking probe workers.
    video_probe_queue: Vec<PathBuf>,
    /// Shared animation clock for the current draw, in milliseconds. Set by
    /// the app before each draw so frame selection is a pure cache lookup.
    animation_clock_ms: u64,
    /// Whether the last draw showed a running animation, i.e. whether the app
    /// should schedule another draw for the next frame.
    animation_active: bool,
}

impl MediaPreviewCache {
    pub fn video_info(&self, path: &Path) -> Option<&Result<Arc<VideoInfo>, String>> {
        self.videos.get(path)
    }

    pub fn insert_video_info(&mut self, path: PathBuf, result: Result<VideoInfo, String>) {
        self.videos.insert(path, result.map(Arc::new));
    }

    /// Starts a draw at `clock_ms` on the shared animation clock.
    pub fn begin_animation_frame(&mut self, clock_ms: u64) {
        self.animation_clock_ms = clock_ms;
        self.animation_active = false;
    }

    pub fn animation_clock_ms(&self) -> u64 {
        self.animation_clock_ms
    }

    /// True when the most recent draw rendered at least one animated frame.
    pub fn animation_active(&self) -> bool {
        self.animation_active
    }

    /// Marks that a frame of a running animation is on screen (used by the
    /// terminal-graphics overlay, which picks frames outside this module).
    pub fn mark_animation_active(&mut self) {
        self.animation_active = true;
    }

    /// Probed info for a looping GIF-style animation at `path`, if ready.
    pub fn gif_animation(&self, path: &Path) -> Option<Arc<VideoInfo>> {
        match self.videos.get(path) {
            Some(Ok(info)) if info.is_gif_like() && info.animation.is_some() => Some(info.clone()),
            _ => None,
        }
    }

    fn request_video_probe(&mut self, path: PathBuf) {
        if !self.video_probe_queue.contains(&path) {
            self.video_probe_queue.push(path);
        }
    }

    /// Takes the video probe requests queued by draw passes since the last call.
    pub fn take_video_probe_requests(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.video_probe_queue)
    }

    pub fn get(&self, key: &MediaPreviewKey) -> Option<&Result<Vec<Vec<Span<'static>>>, String>> {
        self.previews.get(key)
    }

    pub fn insert(
        &mut self,
        key: MediaPreviewKey,
        result: Result<Vec<Vec<Span<'static>>>, String>,
    ) {
        self.previews.insert(key, result);
    }
}

#[derive(Clone, Debug)]
struct MessageLayoutEntry {
    message_index: usize,
    start_line: usize,
    line_count: usize,
    grouped: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MessageLayoutKey {
    content_width: u16,
    presentation: ConversationPresentation,
    link_metadata_revision: u64,
    messages_hash: u64,
}

/// Structured preview of the message a reply quotes. Kept split into the
/// quoted sender's display name and a single-line snippet so the nested reply
/// quote box can style them independently (bold colored title in the inner
/// box border, muted snippet inside).
#[derive(Clone, Debug)]
struct ReplyPreview {
    sender: Arc<str>,
    snippet: Arc<str>,
}

#[derive(Debug, Default)]
pub struct MessageLayoutCache {
    key: Option<MessageLayoutKey>,
    entries: Vec<MessageLayoutEntry>,
    total_lines: usize,
    reply_previews: HashMap<Arc<str>, ReplyPreview>,
    thread_summaries: HashMap<MessageId, ThreadSummary>,
    /// Token of the draw currently in progress, set by [`Self::begin_frame`]
    /// and cleared by [`Self::end_frame`]. While a frame is active, repeated
    /// cache lookups within that frame can skip the O(history) layout hash.
    frame_token: Option<u64>,
    /// Token at which [`Self::key`] was last confirmed against the message
    /// history. When this matches the active `frame_token`, the cache is known
    /// to be valid for the current frame without re-hashing.
    validated_token: Option<u64>,
}

impl MessageLayoutCache {
    pub fn clear(&mut self) {
        self.key = None;
        self.entries.clear();
        self.total_lines = 0;
        self.reply_previews.clear();
        self.thread_summaries.clear();
        // Force the next query to re-validate against the history.
        self.validated_token = None;
    }

    /// Begin a draw frame. Within a frame, the message set cannot change, so
    /// the first cache validation is reused by later lookups in the same frame,
    /// avoiding repeated whole-history hashing (the layout hash currently runs
    /// 3-4 times per draw).
    pub fn begin_frame(&mut self, token: u64) {
        self.frame_token = Some(token);
    }

    /// End the current draw frame. Outside a frame, every lookup re-hashes the
    /// history (the original always-validate behaviour) so message mutations
    /// made during event handling are never missed.
    pub fn end_frame(&mut self) {
        self.frame_token = None;
    }
}

pub struct MessageListRender {
    pub lines: Vec<Line<'static>>,
    pub media_hits: Vec<MediaHit>,
    pub message_hits: Vec<MessageHit>,
    pub total_lines: usize,
    pub link_preview_requests: Vec<LinkPreviewRequest>,
    pub media_preview_requests: Vec<MediaPreviewRequest>,
}

pub struct ThreadMessageCardRender {
    pub lines: Vec<Line<'static>>,
    pub link_preview_requests: Vec<LinkPreviewRequest>,
    pub media_preview_requests: Vec<MediaPreviewRequest>,
}

pub fn thread_message_card_content_line_count(
    message: &Message,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
) -> usize {
    content_lines_len(
        &message.content,
        content_width,
        link_metadata,
        ConversationPresentation::Flat,
    )
}

pub fn build_thread_message_card_lines(
    message: &Message,
    content_width: u16,
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    theme: Theme,
) -> ThreadMessageCardRender {
    let mut media_hits = Vec::new();
    let mut link_preview_requests = Vec::new();
    let mut media_preview_requests = Vec::new();
    let reply_previews = HashMap::new();
    let thread_summaries = HashMap::new();
    let thread_unread = HashMap::new();
    let mut context = MessageRenderContext {
        media_cache,
        content_width,
        media_hits: &mut media_hits,
        theme,
        previous_sender: None,
        reply_previews: &reply_previews,
        thread_summaries: &thread_summaries,
        thread_unread: &thread_unread,
        link_metadata,
        link_preview_requests: &mut link_preview_requests,
        media_preview_requests: &mut media_preview_requests,
        presentation: ConversationPresentation::Flat,
    };

    let lines = content_lines(
        &message.content,
        &mut context,
        0,
        false,
        bubble_accent(theme, message.is_from_me, false),
        Some(&message.id),
    );

    ThreadMessageCardRender {
        lines,
        link_preview_requests,
        media_preview_requests,
    }
}

pub fn render_message_list(frame: &mut Frame<'_>, area: Rect, props: MessageListProps<'_>) {
    frame.render_widget(Clear, area);
    let viewport_rows = inner_area(area).height as usize;
    let lines =
        bottom_aligned_message_lines(props.lines, props.total_lines, props.scroll, viewport_rows);
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(Line::from(Span::styled(
                crate::widgets::padded_title(props.title),
                props.theme.pane_title_for(props.focused),
            )))
            .borders(Borders::ALL)
            .border_style(props.theme.focus_border(props.focused)),
    );
    frame.render_widget(paragraph, area);
    render_scrollbar(frame, area, props.total_lines, props.scroll, props.theme);
}

#[allow(clippy::too_many_arguments)]
fn bottom_aligned_message_lines(
    lines: Vec<Line<'static>>,
    total_lines: usize,
    scroll: usize,
    viewport_rows: usize,
) -> Vec<Line<'static>> {
    if total_lines == 0
        || lines.len() >= viewport_rows
        || scroll.saturating_add(viewport_rows) < total_lines
    {
        return lines;
    }

    let top_padding = viewport_rows.saturating_sub(lines.len());
    let mut padded = Vec::with_capacity(viewport_rows);
    padded.extend(std::iter::repeat_with(Line::default).take(top_padding));
    padded.extend(lines);
    padded
}

#[allow(clippy::too_many_arguments)]
pub fn build_message_lines(
    messages: &[Message],
    content_width: u16,
    scroll: usize,
    viewport_rows: usize,
    selected_message_id: Option<&str>,
    unread_message_ids: &HashSet<Arc<str>>,
    thread_unread: &HashMap<MessageId, u32>,
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    theme: Theme,
) -> MessageListRender {
    build_message_lines_with_presentation(
        messages,
        content_width,
        scroll,
        viewport_rows,
        selected_message_id,
        unread_message_ids,
        thread_unread,
        media_cache,
        link_metadata,
        theme,
        ConversationPresentation::Bubbles,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_message_lines_with_cache(
    messages: &[Message],
    content_width: u16,
    scroll: usize,
    viewport_rows: usize,
    selected_message_id: Option<&str>,
    unread_message_ids: &HashSet<Arc<str>>,
    thread_unread: &HashMap<MessageId, u32>,
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    theme: Theme,
    presentation: ConversationPresentation,
) -> MessageListRender {
    ensure_layout_cache(
        messages,
        content_width,
        link_metadata,
        link_metadata_revision,
        cache,
        presentation,
    );

    build_message_lines_from_layout_cache(
        messages,
        content_width,
        scroll,
        viewport_rows,
        selected_message_id,
        unread_message_ids,
        thread_unread,
        media_cache,
        link_metadata,
        cache,
        theme,
        presentation,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_message_lines_with_presentation(
    messages: &[Message],
    content_width: u16,
    scroll: usize,
    viewport_rows: usize,
    selected_message_id: Option<&str>,
    unread_message_ids: &HashSet<Arc<str>>,
    thread_unread: &HashMap<MessageId, u32>,
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    theme: Theme,
    presentation: ConversationPresentation,
) -> MessageListRender {
    let thread_summaries = thread_summaries(messages);
    let visible_messages = timeline_messages(messages);
    let total_lines = message_line_count_for_visible_messages(
        &visible_messages,
        content_width,
        link_metadata,
        &thread_summaries,
        presentation,
    );
    let render_start = scroll.saturating_sub(1);
    let render_end = scroll
        .saturating_add(viewport_rows.max(1))
        .saturating_add(1)
        .min(total_lines);
    let mut all_lines = Vec::new();
    let mut media_hits = Vec::new();
    let mut message_hits = Vec::new();
    let mut link_preview_requests = Vec::new();
    let mut media_preview_requests = Vec::new();
    let reply_previews = messages
        .iter()
        .map(|message| (message.id.clone(), reply_preview(message)))
        .collect::<HashMap<_, _>>();
    let mut context = MessageRenderContext {
        media_cache,
        content_width,
        media_hits: &mut media_hits,
        theme,
        previous_sender: None,
        reply_previews: &reply_previews,
        thread_summaries: &thread_summaries,
        thread_unread,
        link_metadata,
        link_preview_requests: &mut link_preview_requests,
        media_preview_requests: &mut media_preview_requests,
        presentation,
    };

    let mut line_cursor: usize = 0;
    for message in visible_messages {
        let has_previous_message = context.previous_sender.is_some();
        let grouped = context
            .previous_sender
            .as_ref()
            .is_some_and(|previous| previous == &message.sender.platform_id);
        let line_count = message_lines_len(
            message,
            grouped,
            has_previous_message,
            content_width,
            link_metadata,
            context.thread_summaries.get(&message.id),
            presentation,
        );
        let message_start = line_cursor;
        let message_end_exclusive = message_start.saturating_add(line_count);
        context.previous_sender = Some(message.sender.platform_id.clone());
        line_cursor = message_end_exclusive;

        if message_end_exclusive <= render_start || message_start >= render_end {
            continue;
        }

        let selected = selected_message_id == Some(message.id.as_ref());
        let unread = unread_message_ids.contains(&message.id);
        let message_lines = message_lines(
            message,
            &mut context,
            message_start,
            selected,
            grouped,
            has_previous_message,
            unread,
        );
        let line_hits = message_line_hits(
            &message_lines,
            message_start,
            content_width,
            grouped,
            presentation,
        );
        let avatar_hit = message_avatar_hit(
            &message_lines,
            message_start,
            content_width,
            grouped,
            presentation,
        );
        let avatar_path = avatar_hit
            .as_ref()
            .and_then(|_| message.sender.avatar.clone())
            .filter(|path| path.exists());
        let thread_summary_hit = message_thread_summary_hit(
            &message_lines,
            message_start,
            content_width,
            context.thread_summaries.contains_key(&message.id),
        );
        let end_line = message_start + message_lines.len().saturating_sub(1);
        if end_line >= message_start {
            message_hits.push(MessageHit {
                start_line: message_start,
                end_line,
                message_id: message.id.clone(),
                line_hits,
                avatar_hit,
                avatar_path,
                thread_summary_hit,
            });
        }
        for (offset, line) in message_lines.into_iter().enumerate() {
            let line_index = message_start + offset;
            if line_index >= scroll && line_index < render_end {
                all_lines.push(line);
            }
        }
    }

    MessageListRender {
        lines: all_lines,
        media_hits,
        message_hits,
        total_lines,
        link_preview_requests,
        media_preview_requests,
    }
}

fn rebuild_message_layout_cache(
    cache: &mut MessageLayoutCache,
    key: MessageLayoutKey,
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    presentation: ConversationPresentation,
) {
    cache.key = Some(key);
    cache.entries.clear();
    cache.total_lines = 0;
    cache.reply_previews = messages
        .iter()
        .map(|message| (message.id.clone(), reply_preview(message)))
        .collect();
    cache.thread_summaries = thread_summaries(messages);

    let mut previous_sender: Option<&Arc<str>> = None;
    for (message_index, message) in messages.iter().enumerate() {
        if is_slack_thread_reply(message) {
            continue;
        }
        let has_previous_message = previous_sender.is_some();
        let grouped =
            previous_sender.is_some_and(|previous| previous == &message.sender.platform_id);
        let line_count = message_lines_len(
            message,
            grouped,
            has_previous_message,
            content_width,
            link_metadata,
            cache.thread_summaries.get(&message.id),
            presentation,
        );
        cache.entries.push(MessageLayoutEntry {
            message_index,
            start_line: cache.total_lines,
            line_count,
            grouped,
        });
        cache.total_lines = cache.total_lines.saturating_add(line_count);
        previous_sender = Some(&message.sender.platform_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn build_message_lines_from_layout_cache(
    messages: &[Message],
    content_width: u16,
    scroll: usize,
    viewport_rows: usize,
    selected_message_id: Option<&str>,
    unread_message_ids: &HashSet<Arc<str>>,
    thread_unread: &HashMap<MessageId, u32>,
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    cache: &mut MessageLayoutCache,
    theme: Theme,
    presentation: ConversationPresentation,
) -> MessageListRender {
    let total_lines = cache.total_lines;
    let render_start = scroll.saturating_sub(1);
    let render_end = scroll
        .saturating_add(viewport_rows.max(1))
        .saturating_add(1)
        .min(total_lines);
    let mut all_lines = Vec::new();
    let mut media_hits = Vec::new();
    let mut message_hits = Vec::new();
    let mut link_preview_requests = Vec::new();
    let mut media_preview_requests = Vec::new();
    let mut context = MessageRenderContext {
        media_cache,
        content_width,
        media_hits: &mut media_hits,
        theme,
        previous_sender: None,
        reply_previews: &cache.reply_previews,
        thread_summaries: &cache.thread_summaries,
        thread_unread,
        link_metadata,
        link_preview_requests: &mut link_preview_requests,
        media_preview_requests: &mut media_preview_requests,
        presentation,
    };

    let start_index = cache
        .entries
        .partition_point(|entry| entry.start_line.saturating_add(entry.line_count) <= render_start)
        .saturating_sub(1);

    for entry in cache.entries.iter().skip(start_index) {
        let message_start = entry.start_line;
        let message_end_exclusive = message_start.saturating_add(entry.line_count);
        if message_start >= render_end {
            break;
        }
        let Some(message) = messages.get(entry.message_index) else {
            continue;
        };
        context.previous_sender = Some(message.sender.platform_id.clone());
        if message_end_exclusive <= render_start {
            continue;
        }

        let selected = selected_message_id == Some(message.id.as_ref());
        let unread = unread_message_ids.contains(&message.id);
        let message_lines = message_lines(
            message,
            &mut context,
            message_start,
            selected,
            entry.grouped,
            message_start > 0,
            unread,
        );
        let line_hits = message_line_hits(
            &message_lines,
            message_start,
            content_width,
            entry.grouped,
            presentation,
        );
        let avatar_hit = message_avatar_hit(
            &message_lines,
            message_start,
            content_width,
            entry.grouped,
            presentation,
        );
        let avatar_path = avatar_hit
            .as_ref()
            .and_then(|_| message.sender.avatar.clone())
            .filter(|path| path.exists());
        let thread_summary_hit = message_thread_summary_hit(
            &message_lines,
            message_start,
            content_width,
            cache.thread_summaries.contains_key(&message.id),
        );
        let end_line = message_start + message_lines.len().saturating_sub(1);
        if end_line >= message_start {
            message_hits.push(MessageHit {
                start_line: message_start,
                end_line,
                message_id: message.id.clone(),
                line_hits,
                avatar_hit,
                avatar_path,
                thread_summary_hit,
            });
        }
        for (offset, line) in message_lines.into_iter().enumerate() {
            let line_index = message_start + offset;
            if line_index >= scroll && line_index < render_end {
                all_lines.push(line);
            }
        }
    }

    MessageListRender {
        lines: all_lines,
        media_hits,
        message_hits,
        total_lines,
        link_preview_requests,
        media_preview_requests,
    }
}

fn messages_layout_hash(messages: &[Message]) -> u64 {
    let mut hasher = DefaultHasher::new();
    messages.len().hash(&mut hasher);
    for message in messages {
        message.id.hash(&mut hasher);
        message.sender.platform_id.hash(&mut hasher);
        message.sender.display_name.hash(&mut hasher);
        message.timestamp.timestamp_millis().hash(&mut hasher);
        message
            .edited_at
            .map(|timestamp| timestamp.timestamp_millis())
            .hash(&mut hasher);
        message.reply_to.hash(&mut hasher);
        message.thread_id.hash(&mut hasher);
        message.is_from_me.hash(&mut hasher);
        message_content_layout_hash(&message.content, &mut hasher);
        for reaction in &message.reactions {
            reaction.emoji.hash(&mut hasher);
            reaction.senders.hash(&mut hasher);
        }
        for receipt in &message.receipts {
            std::mem::discriminant(&receipt.kind).hash(&mut hasher);
            receipt
                .at
                .map(|timestamp| timestamp.timestamp_millis())
                .hash(&mut hasher);
        }
        if let Some(slack) = &message.platform_data.slack {
            slack.ts.hash(&mut hasher);
            slack.thread_ts.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn message_content_layout_hash(content: &Content, hasher: &mut DefaultHasher) {
    std::mem::discriminant(content).hash(hasher);
    match content {
        Content::Text(text) | Content::Unsupported(text) => text.hash(hasher),
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media_layout_hash(media, hasher),
        Content::LinkPreview(link) => {
            link.url.hash(hasher);
            link.title.hash(hasher);
            link.description.hash(hasher);
            if let Some(media) = &link.image {
                media_layout_hash(media, hasher);
            }
        }
        Content::Cards(cards) => {
            for card in cards {
                card.title.hash(hasher);
                card.subtitle.hash(hasher);
                card.body.hash(hasher);
                card.footer.hash(hasher);
                card.url.hash(hasher);
                for field in &card.fields {
                    field.title.hash(hasher);
                    field.value.hash(hasher);
                    field.short.hash(hasher);
                }
            }
        }
        Content::Poll(poll) => {
            poll.question.hash(hasher);
            poll.selectable_options_count.hash(hasher);
            for option in &poll.options {
                option.id.hash(hasher);
                option.label.hash(hasher);
            }
            for vote in &poll.votes {
                vote.sender.hash(hasher);
                vote.options.hash(hasher);
            }
        }
        Content::Deleted => {}
    }
}

fn media_layout_hash(media: &chat_core::Media, hasher: &mut DefaultHasher) {
    media.file_name.hash(hasher);
    media.mime_type.hash(hasher);
    media.size_bytes.hash(hasher);
    media.caption.hash(hasher);
    media.local_path.hash(hasher);
    media.thumbnail.hash(hasher);
}

pub fn cached_message_line_count(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    presentation: ConversationPresentation,
) -> usize {
    ensure_layout_cache(
        messages,
        content_width,
        link_metadata,
        link_metadata_revision,
        cache,
        presentation,
    );
    cache.total_lines
}

fn ensure_layout_cache(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    presentation: ConversationPresentation,
) {
    // Fast path: if the cache was already validated earlier in the current draw
    // frame and the non-content key fields still match, the message set cannot
    // have changed mid-frame, so skip the expensive whole-history layout hash.
    if let Some(token) = cache.frame_token
        && cache.validated_token == Some(token)
        && let Some(existing) = &cache.key
        && existing.content_width == content_width
        && existing.presentation == presentation
        && existing.link_metadata_revision == link_metadata_revision
    {
        return;
    }

    let key = MessageLayoutKey {
        content_width,
        presentation,
        link_metadata_revision,
        messages_hash: messages_layout_hash(messages),
    };
    if cache.key.as_ref() != Some(&key) {
        rebuild_message_layout_cache(
            cache,
            key,
            messages,
            content_width,
            link_metadata,
            presentation,
        );
    }
    // Record the token this validation belongs to. Outside a draw frame this is
    // `None`, so subsequent event-handling lookups always re-hash.
    cache.validated_token = cache.frame_token;
}

/// Returns the message id rendered at the absolute layout line `line` plus the
/// line offset inside that message. Used to anchor the viewport on a stable
/// message while history merges mutate the timeline above or below it.
#[allow(clippy::too_many_arguments)]
pub fn cached_message_anchor_at_line(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    presentation: ConversationPresentation,
    line: usize,
) -> Option<(MessageId, usize)> {
    ensure_layout_cache(
        messages,
        content_width,
        link_metadata,
        link_metadata_revision,
        cache,
        presentation,
    );
    let index = cache
        .entries
        .partition_point(|entry| entry.start_line.saturating_add(entry.line_count) <= line);
    let entry = cache.entries.get(index)?;
    let message = messages.get(entry.message_index)?;
    Some((message.id.clone(), line.saturating_sub(entry.start_line)))
}

/// Returns the first layout line of the message with `message_id`, or `None`
/// when the message is not part of the rendered timeline.
#[allow(clippy::too_many_arguments)]
pub fn cached_message_start_line(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    presentation: ConversationPresentation,
    message_id: &str,
) -> Option<usize> {
    ensure_layout_cache(
        messages,
        content_width,
        link_metadata,
        link_metadata_revision,
        cache,
        presentation,
    );
    cache.entries.iter().find_map(|entry| {
        messages
            .get(entry.message_index)
            .filter(|message| message.id.as_ref() == message_id)
            .map(|_| entry.start_line)
    })
}

fn message_line_hits(
    lines: &[Line<'static>],
    start_line: usize,
    content_width: u16,
    grouped: bool,
    presentation: ConversationPresentation,
) -> Vec<MessageLineHit> {
    lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            (*index == 0 && !grouped)
                || is_clickable_message_content_line(line)
                || (presentation == ConversationPresentation::Flat
                    && !line.spans.is_empty()
                    && !line_text(line).trim().is_empty())
        })
        .filter_map(|(index, line)| {
            let width = if index == 0 && !grouped {
                header_click_width(line)
            } else {
                line_width(line)
            }
            .min(content_width as usize) as u16;
            if width == 0 {
                return None;
            }
            let start_col = aligned_line_start_col(line, width, content_width);
            Some(MessageLineHit {
                line: start_line + index,
                start_col,
                end_col: start_col.saturating_add(width),
            })
        })
        .collect()
}

fn message_avatar_hit(
    lines: &[Line<'static>],
    start_line: usize,
    content_width: u16,
    grouped: bool,
    presentation: ConversationPresentation,
) -> Option<MessageLineHit> {
    if grouped {
        return None;
    }
    let header = lines.iter().find(|line| !line.spans.is_empty())?;
    let header_offset = lines
        .iter()
        .position(|line| std::ptr::eq(line, header))
        .unwrap_or_default();
    let width = line_width(header).min(content_width as usize) as u16;
    if width == 0 {
        return None;
    }
    let start_col = aligned_line_start_col(header, width, content_width);
    let avatar_start = if presentation == ConversationPresentation::Flat {
        start_col
            .saturating_add(SLACK_TIMESTAMP_WIDTH as u16)
            .saturating_add(SLACK_GUTTER_GAP as u16)
    } else {
        let selected_prefix = header
            .spans
            .first()
            .filter(|span| span.content.as_ref() == "▏ ")
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
            .unwrap_or_default();
        start_col.saturating_add(selected_prefix)
    };
    Some(MessageLineHit {
        line: start_line.saturating_add(header_offset),
        start_col: avatar_start,
        end_col: avatar_start.saturating_add(MESSAGE_AVATAR_WIDTH),
    })
}

fn message_thread_summary_hit(
    lines: &[Line<'_>],
    start_line: usize,
    content_width: u16,
    has_summary: bool,
) -> Option<MessageLineHit> {
    if !has_summary {
        return None;
    }
    let summary_offset = lines
        .iter()
        .position(|line| line_text(line).contains("Last reply"))?;
    let line = lines.get(summary_offset)?;
    let width = line_width(line).min(content_width as usize) as u16;
    if width == 0 {
        return None;
    }
    let start_col = aligned_line_start_col(line, width, content_width);
    Some(MessageLineHit {
        line: start_line.saturating_add(summary_offset),
        start_col,
        end_col: start_col.saturating_add(width),
    })
}

fn aligned_line_start_col(line: &Line<'_>, width: u16, content_width: u16) -> u16 {
    match line.alignment.unwrap_or(Alignment::Left) {
        Alignment::Right => content_width.saturating_sub(width),
        Alignment::Center => content_width.saturating_sub(width) / 2,
        Alignment::Left => 0,
    }
}

fn is_clickable_message_content_line(line: &Line<'_>) -> bool {
    let text = line_text(line);
    let content = text
        .trim_start()
        .strip_prefix('▏')
        .unwrap_or_else(|| text.trim_start())
        .trim_start();
    content.starts_with('╭')
        || content.starts_with('│')
        || content.starts_with('╰')
        || content.contains(" replies")
        || content.contains("1 reply")
}

fn header_click_width(line: &Line<'_>) -> usize {
    let total = line_width(line);
    let trailing_spaces = line
        .spans
        .iter()
        .rev()
        .skip(1)
        .take_while(|span| span.content.as_ref().chars().all(char::is_whitespace))
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    total.saturating_sub(trailing_spaces)
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn push_right_aligned_spans(
    line_spans: &mut Vec<Span<'static>>,
    content_width: u16,
    trailing_spans: &[Span<'static>],
) {
    let leading_width = spans_width(line_spans);
    let trailing_width = spans_width(trailing_spans);
    let spacer = (content_width as usize)
        .saturating_sub(leading_width)
        .saturating_sub(trailing_width)
        .max(1);
    line_spans.push(Span::raw(" ".repeat(spacer)));
    line_spans.extend(trailing_spans.iter().cloned());
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

pub fn message_line_count(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
) -> usize {
    message_line_count_with_presentation(
        messages,
        content_width,
        link_metadata,
        ConversationPresentation::Bubbles,
    )
}

pub fn message_line_count_with_presentation(
    messages: &[Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    presentation: ConversationPresentation,
) -> usize {
    let thread_summaries = thread_summaries(messages);
    let visible_messages = timeline_messages(messages);
    message_line_count_for_visible_messages(
        &visible_messages,
        content_width,
        link_metadata,
        &thread_summaries,
        presentation,
    )
}

fn message_line_count_for_visible_messages(
    messages: &[&Message],
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    thread_summaries: &HashMap<MessageId, ThreadSummary>,
    presentation: ConversationPresentation,
) -> usize {
    let mut previous_sender: Option<&Arc<str>> = None;
    messages
        .iter()
        .map(|message| {
            let has_previous_message = previous_sender.is_some();
            let grouped =
                previous_sender.is_some_and(|previous| previous == &message.sender.platform_id);
            previous_sender = Some(&message.sender.platform_id);
            message_lines_len(
                message,
                grouped,
                has_previous_message,
                content_width,
                link_metadata,
                thread_summaries.get(&message.id),
                presentation,
            )
        })
        .sum()
}

pub fn cached_image_preview_rows(
    path: &Path,
    media_cache: &mut MediaPreviewCache,
    width: u16,
    rows: u16,
) -> Result<Vec<Vec<Span<'static>>>, String> {
    let key = MediaPreviewKey {
        path: path.to_path_buf(),
        width,
        rows,
    };
    media_cache
        .previews
        .entry(key)
        .or_insert_with(|| decode_image_preview_rows(path, width, rows))
        .clone()
}

pub fn decode_image_preview_rows_for_key(
    key: &MediaPreviewKey,
) -> Result<Vec<Vec<Span<'static>>>, String> {
    decode_image_preview_rows(&key.path, key.width, key.rows)
}

pub fn image_preview_rows_from_rgba(
    image: &image::RgbaImage,
    width: u16,
    rows: u16,
) -> Vec<Vec<Span<'static>>> {
    let (fit_width, fit_rows) =
        fit_halfblock_cell_size(image.width(), image.height(), width.max(1), rows.max(1));
    let mut resized = image::DynamicImage::ImageRgba8(image.clone())
        .resize_exact(
            u32::from(fit_width.max(1)),
            u32::from(fit_rows.max(1)) * 2,
            FilterType::Triangle,
        )
        .to_rgba8();
    apply_rounded_thumbnail_mask(&mut resized);
    preview_rows_from_resized_rgba(&resized, fit_width, fit_rows, width.max(1), rows.max(1))
}

pub fn image_preview_rows_from_bytes(
    bytes: &[u8],
    width: u16,
    rows: u16,
) -> Result<Vec<Vec<Span<'static>>>, String> {
    let image = image::load_from_memory(bytes)
        .map_err(|error| format!("decoding cached avatar thumbnail: {error}"))?
        .to_rgba8();
    Ok(image_preview_rows_from_rgba(&image, width, rows))
}

pub fn image_cell_size(path: &Path, max_width: u16, max_rows: u16) -> Result<(u16, u16), String> {
    let reader = image::ImageReader::open(path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("detecting {}: {error}", path.display()))?;
    let dimensions = reader
        .into_dimensions()
        .map_err(|error| format!("reading dimensions for {}: {error}", path.display()))?;
    Ok(fit_halfblock_cell_size(
        dimensions.0,
        dimensions.1,
        max_width,
        max_rows,
    ))
}

pub fn fallback_preview_rows(
    width: u16,
    rows: u16,
    accent: Color,
    message: &str,
) -> Vec<Vec<Span<'static>>> {
    let message_row = rows / 2;
    (0..rows)
        .map(|row| {
            let text = if row == message_row { message } else { "" };
            vec![Span::styled(
                fit_cell_text(text, width),
                Style::default().fg(accent),
            )]
        })
        .collect()
}

pub fn sender_style(theme: Theme, is_from_me: bool) -> Style {
    if is_from_me {
        theme.outgoing()
    } else {
        theme.incoming()
    }
}

/// Age-aware message timestamp. Messages from today or yesterday keep the
/// bare clock time (the surrounding ordering and the sidebar's Yesterday
/// section make the day obvious); older messages carry the date so the
/// reader is never tricked into thinking an old message is recent. Full date
/// and time (with seconds) for detail views comes from
/// [`format_message_datetime`].
pub fn format_message_time(timestamp: chat_core::Timestamp) -> String {
    format_message_time_at(timestamp, Local::now())
}

fn format_message_time_at(timestamp: chat_core::Timestamp, now: DateTime<Local>) -> String {
    let local = timestamp.with_timezone(&Local);
    match local_days_ago(local, now) {
        ..=1 => local.format("%H:%M").to_string(),
        _ if local.year() == now.year() => local.format("%d %b %H:%M").to_string(),
        _ => local.format("%d %b %Y %H:%M").to_string(),
    }
}

/// Compact age-aware stamp for narrow, fixed-width columns (chat sidebar
/// meta column, Slack-style gutter). Always at most 5 cells wide.
pub fn format_timestamp_compact(timestamp: chat_core::Timestamp) -> String {
    format_timestamp_compact_at(timestamp, Local::now())
}

fn format_timestamp_compact_at(timestamp: chat_core::Timestamp, now: DateTime<Local>) -> String {
    let local = timestamp.with_timezone(&Local);
    match local_days_ago(local, now) {
        ..=1 => local.format("%H:%M").to_string(),
        2..=6 => local.format("%a").to_string(),
        _ if local.year() == now.year() => local.format("%d/%m").to_string(),
        _ => local.format("%Y").to_string(),
    }
}

fn local_days_ago(timestamp: DateTime<Local>, now: DateTime<Local>) -> i64 {
    (now.date_naive() - timestamp.date_naive()).num_days()
}

pub fn format_message_datetime(timestamp: chat_core::Timestamp) -> String {
    timestamp
        .with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

pub fn bubble_accent(theme: Theme, is_from_me: bool, unread: bool) -> Style {
    if unread {
        if is_from_me {
            Style::default().fg(theme.outgoing)
        } else {
            Style::default().fg(theme.incoming)
        }
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

#[derive(Clone, Debug)]
struct ThreadSummary {
    reply_count: usize,
    participants: Vec<Sender>,
    last_reply_at: Timestamp,
}

struct MessageRenderContext<'a> {
    media_cache: &'a mut MediaPreviewCache,
    content_width: u16,
    media_hits: &'a mut Vec<MediaHit>,
    theme: Theme,
    previous_sender: Option<Arc<str>>,
    reply_previews: &'a HashMap<Arc<str>, ReplyPreview>,
    thread_summaries: &'a HashMap<MessageId, ThreadSummary>,
    /// Per-thread unread reply counts keyed by thread root message id. Applied
    /// at render time (not cached) so marking a thread read clears its badge.
    thread_unread: &'a HashMap<MessageId, u32>,
    link_metadata: &'a LinkMetadataCache,
    link_preview_requests: &'a mut Vec<LinkPreviewRequest>,
    media_preview_requests: &'a mut Vec<MediaPreviewRequest>,
    presentation: ConversationPresentation,
}

fn message_lines(
    message: &Message,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    selected: bool,
    grouped: bool,
    has_previous_message: bool,
    unread: bool,
) -> Vec<Line<'static>> {
    if context.presentation == ConversationPresentation::Flat {
        return slack_message_lines(
            message,
            context,
            start_line,
            selected,
            grouped,
            has_previous_message,
            unread,
        );
    }

    let accent_style = bubble_accent(context.theme, message.is_from_me, unread);
    let mut lines = Vec::new();

    if !grouped {
        let mut header_spans = Vec::new();
        header_spans.extend(message_avatar_spans(&message.sender, context));
        header_spans.extend([
            Span::raw(" "),
            Span::styled(
                message.sender.display_name.to_string(),
                sender_style(context.theme, message.is_from_me),
            ),
            Span::raw(if message.is_from_me { " (me)" } else { "" }),
        ]);
        // Own messages carry the marker on their per-bubble status line, so
        // only incoming headers show it (avoids a duplicate marker).
        let timestamp = if message.edited_at.is_some() && !message.is_from_me {
            format!(
                "{EDITED_MARKER} · {}",
                format_message_time(message.timestamp)
            )
        } else {
            format_message_time(message.timestamp)
        };
        push_right_aligned_spans(
            &mut header_spans,
            context.content_width,
            &[Span::styled(timestamp, context.theme.muted())],
        );
        lines.push(Line::from(header_spans));
    }

    if let Some(reply_to) = &message.reply_to {
        let (preview, color) = resolve_reply_preview(context, reply_to);
        if let Some(text) = reply_inline_text(&message.content) {
            lines.extend(reply_text_bubble_lines(
                &preview,
                &text,
                context.content_width,
                accent_style,
                context.theme,
                color,
            ));
        } else {
            let box_width = bubble_inner_width(context.content_width);
            for row in reply_quote_box_rows(&preview, box_width, context.theme, color) {
                lines.push(Line::from(row));
            }
            lines.extend(content_lines(
                &message.content,
                context,
                start_line + lines.len(),
                message.is_from_me,
                accent_style,
                Some(&message.id),
            ));
        }
    } else {
        lines.extend(content_lines(
            &message.content,
            context,
            start_line + lines.len(),
            message.is_from_me,
            accent_style,
            Some(&message.id),
        ));
    }

    let receipts = receipt_summary(message);
    if message.is_from_me {
        let time = if message.edited_at.is_some() {
            format!(
                "{} · {EDITED_MARKER}",
                format_message_time(message.timestamp)
            )
        } else {
            format_message_time(message.timestamp)
        };
        let status = if receipts.is_empty() {
            time
        } else {
            format!("{time} · {receipts}")
        };
        lines.push(status_line(&status, context.theme.muted()));
    } else if grouped && message.edited_at.is_some() {
        // Grouped incoming bubbles have no header, so the marker moves here.
        let status = if receipts.is_empty() {
            EDITED_MARKER.to_owned()
        } else {
            format!("{EDITED_MARKER} · {receipts}")
        };
        lines.push(status_line(&status, context.theme.muted()));
    } else if !receipts.is_empty() {
        lines.push(status_line(&receipts, context.theme.muted()));
    }

    if !message.reactions.is_empty() {
        lines.push(reaction_pill_line(message, context.theme));
    }

    if let Some(summary) = context.thread_summaries.get(&message.id) {
        let unread = context.thread_unread.get(&message.id).copied().unwrap_or(0) as usize;
        lines.push(thread_summary_line(
            summary,
            message.is_from_me,
            unread,
            context,
        ));
    }

    for line in &mut lines {
        line.alignment = Some(if message.is_from_me {
            Alignment::Right
        } else {
            Alignment::Left
        });
        if selected && !line.spans.is_empty() {
            let marker = Span::styled(" ▕", Style::default().fg(context.theme.accent));
            if message.is_from_me {
                line.spans.push(marker);
            } else {
                line.spans.insert(
                    0,
                    Span::styled("▏ ", Style::default().fg(context.theme.accent)),
                );
            }
        }
    }

    if !lines.is_empty() {
        lines.push(Line::from(""));
    }

    for line in lines.iter_mut().filter(|line| line.spans.is_empty()) {
        line.alignment = None;
        line.style = accent_style;
    }

    lines
}

fn slack_message_lines(
    message: &Message,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    selected: bool,
    grouped: bool,
    has_previous_message: bool,
    unread: bool,
) -> Vec<Line<'static>> {
    let accent_style = bubble_accent(context.theme, message.is_from_me, unread);
    let mut lines = Vec::new();
    let spacer_lines = slack_message_spacer_lines(grouped, has_previous_message);
    lines.extend((0..spacer_lines).map(|_| Line::default()));
    let body_start_line = start_line
        + spacer_lines
        + usize::from(!grouped)
        + if message.reply_to.is_some() {
            QUOTE_BOX_ROWS
        } else {
            0
        };
    let mut body_lines = slack_content_lines(message, context, body_start_line, accent_style);
    if body_lines.is_empty() {
        body_lines.push(Line::from(""));
    }
    let merge_first_body_line =
        !grouped && message.reply_to.is_none() && slack_content_can_merge(&message.content);
    match slack_edited_marker_placement(
        message,
        grouped,
        context.content_width,
        context.link_metadata,
    ) {
        FlatEditedMarker::None => {}
        FlatEditedMarker::Inline => {
            if let Some(last) = body_lines.last_mut() {
                last.spans.push(Span::styled(
                    format!(" ({EDITED_MARKER})"),
                    context.theme.muted(),
                ));
            }
        }
        FlatEditedMarker::OwnRow => {
            body_lines.push(Line::from(Span::styled(
                format!("({EDITED_MARKER})"),
                context.theme.muted(),
            )));
        }
        FlatEditedMarker::BestEffort => {
            // Rich content whose last row can't be predicted without drawing:
            // trail the marker only when it fits, never adding a row.
            let header_width = if merge_first_body_line && body_lines.len() == 1 {
                slack_merged_header_width(message)
            } else {
                0
            };
            let available = slack_body_width(context.content_width).saturating_sub(header_width);
            if let Some(last) = body_lines.last_mut() {
                let suffix = format!(" ({EDITED_MARKER})");
                if UnicodeWidthStr::width(line_text(last).as_str())
                    + UnicodeWidthStr::width(suffix.as_str())
                    <= available
                {
                    last.spans.push(Span::styled(suffix, context.theme.muted()));
                }
            }
        }
    }

    if !grouped {
        let mut header_spans = slack_header_spans(message, context);
        if merge_first_body_line {
            let first_body_line = body_lines.remove(0);
            if !line_text(&first_body_line).trim().is_empty() {
                header_spans.push(Span::raw(" "));
                header_spans.extend(first_body_line.spans);
            }
        }
        lines.push(Line::from(header_spans));
    }

    if let Some(reply_to) = &message.reply_to {
        let (preview, color) = resolve_reply_preview(context, reply_to);
        let box_width = slack_body_width(context.content_width);
        for row in reply_quote_box_rows(&preview, box_width, context.theme, color) {
            lines.push(slack_indented_line(row));
        }
    }

    for (index, mut line) in body_lines.into_iter().enumerate() {
        if grouped && index == 0 {
            prefix_line_spans(
                &mut line,
                slack_grouped_prefix_spans(message, context.theme),
            );
        } else {
            prefix_line_spans(&mut line, slack_body_indent_spans());
        }
        lines.push(line);
    }

    if !message.reactions.is_empty() {
        lines.push(slack_indented_line(reaction_pill_spans(message)));
    }

    if let Some(summary) = context.thread_summaries.get(&message.id) {
        let unread = context.thread_unread.get(&message.id).copied().unwrap_or(0) as usize;
        let summary_spans = thread_summary_spans(summary, unread, context);
        lines.push(slack_indented_line(summary_spans));
    }

    for line in &mut lines {
        line.alignment = Some(Alignment::Left);
        if selected && !line.spans.is_empty() {
            apply_slack_timestamp_selection(line, context.theme);
        }
    }

    lines
}

/// Where the flat layout shows the "(edited)" marker. Computed from message
/// data only, so the draw pass and `slack_message_lines_len` always agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlatEditedMarker {
    None,
    /// Trailing the last body row.
    Inline,
    /// On a row of its own because the last body row is full.
    OwnRow,
    /// Rich content: trailed only if it fits once drawn; never adds a row.
    BestEffort,
}

/// Width the sender header adds in front of a merged single body row.
fn slack_merged_header_width(message: &Message) -> usize {
    UnicodeWidthStr::width(message.sender.display_name.as_ref())
        + if message.is_from_me { 5 } else { 0 }
        + 1
}

fn slack_edited_marker_placement(
    message: &Message,
    grouped: bool,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
) -> FlatEditedMarker {
    if message.edited_at.is_none() || !slack_content_can_merge(&message.content) {
        return FlatEditedMarker::None;
    }
    let Content::Text(text) = &message.content else {
        return FlatEditedMarker::BestEffort;
    };
    let has_preview = first_url_in_text(text)
        .and_then(|url| link_metadata.get(url.url))
        .is_some_and(link_metadata_is_useful);
    if has_preview {
        return FlatEditedMarker::BestEffort;
    }
    let body_width = slack_body_width(content_width);
    let wrapped = wrap_text(text, flat_text_width(body_width as u16));
    let last_width = wrapped
        .last()
        .map_or(0, |line| UnicodeWidthStr::width(line.as_str()));
    let merged_single_row = !grouped && message.reply_to.is_none() && wrapped.len() <= 1;
    let header_width = if merged_single_row {
        slack_merged_header_width(message)
    } else {
        0
    };
    let suffix_width = UnicodeWidthStr::width(EDITED_MARKER) + 3;
    if last_width + suffix_width <= body_width.saturating_sub(header_width) {
        FlatEditedMarker::Inline
    } else {
        FlatEditedMarker::OwnRow
    }
}

fn slack_message_spacer_lines(grouped: bool, has_previous_message: bool) -> usize {
    usize::from(has_previous_message && !grouped) * SLACK_MESSAGE_SPACER_LINES
}

fn apply_slack_timestamp_selection(line: &mut Line<'static>, theme: Theme) {
    let selection_style = theme.selection();
    let mut remaining = SLACK_TIMESTAMP_WIDTH;
    let mut spans = Vec::new();

    for span in line.spans.drain(..) {
        if remaining == 0 {
            spans.push(span);
            continue;
        }

        let content = span.content.to_string();
        let width = UnicodeWidthStr::width(content.as_str());
        if width <= remaining {
            let mut selected_span = span;
            selected_span.style = selected_span.style.patch(selection_style);
            spans.push(selected_span);
            remaining = remaining.saturating_sub(width);
            continue;
        }

        let (selected_text, rest_text) = split_by_display_width(&content, remaining);
        if !selected_text.is_empty() {
            spans.push(Span::styled(
                selected_text,
                span.style.patch(selection_style),
            ));
        }
        if !rest_text.is_empty() {
            spans.push(Span::styled(rest_text, span.style));
        }
        remaining = 0;
    }

    if remaining > 0 {
        spans.insert(0, Span::styled(" ".repeat(remaining), selection_style));
    }

    line.spans = spans;
}

fn slack_content_lines(
    message: &Message,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    accent: Style,
) -> Vec<Line<'static>> {
    let original_width = context.content_width;
    let body_width = slack_body_width(original_width) as u16;
    let media_hit_start = context.media_hits.len();
    context.content_width = body_width;
    let lines = content_lines(
        &message.content,
        context,
        start_line,
        false,
        accent,
        Some(&message.id),
    );
    context.content_width = original_width;
    for hit in &mut context.media_hits[media_hit_start..] {
        let offset = SLACK_BODY_INDENT_WIDTH as u16;
        hit.start_col = hit.start_col.saturating_add(offset);
        hit.end_col = hit.end_col.saturating_add(offset);
        hit.preview_start_col = hit.preview_start_col.saturating_add(offset);
        hit.preview_end_col = hit.preview_end_col.saturating_add(offset);
    }
    lines
}

fn slack_header_spans(
    message: &Message,
    context: &mut MessageRenderContext<'_>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    spans.extend(slack_timestamp_spans(message, context.theme));
    spans.extend(message_avatar_spans(&message.sender, context));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        message.sender.display_name.to_string(),
        sender_style(context.theme, message.is_from_me).add_modifier(Modifier::BOLD),
    ));
    if message.is_from_me {
        spans.push(Span::raw(" (me)"));
    }
    spans
}

fn slack_grouped_prefix_spans(message: &Message, theme: Theme) -> Vec<Span<'static>> {
    let mut spans = slack_timestamp_spans(message, theme);
    spans.push(Span::raw(
        " ".repeat(MESSAGE_AVATAR_WIDTH as usize + SLACK_AVATAR_GAP),
    ));
    spans
}

fn slack_timestamp_spans(message: &Message, theme: Theme) -> Vec<Span<'static>> {
    let stamp = format_timestamp_compact(message.timestamp);
    let padding = SLACK_TIMESTAMP_WIDTH.saturating_sub(UnicodeWidthStr::width(stamp.as_str()));
    vec![
        Span::styled(stamp, theme.muted()),
        Span::raw(" ".repeat(padding + SLACK_GUTTER_GAP)),
    ]
}

fn slack_body_indent_spans() -> Vec<Span<'static>> {
    vec![Span::raw(" ".repeat(SLACK_BODY_INDENT_WIDTH))]
}

fn slack_indented_line(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut line = Line::from(spans);
    prefix_line_spans(&mut line, slack_body_indent_spans());
    line
}

fn prefix_line_spans(line: &mut Line<'static>, mut prefix: Vec<Span<'static>>) {
    prefix.append(&mut line.spans);
    line.spans = prefix;
}

fn slack_content_can_merge(content: &Content) -> bool {
    matches!(
        content,
        Content::Text(_) | Content::Poll(_) | Content::Deleted | Content::Unsupported(_)
    )
}

fn content_lines(
    content: &Content,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
    accent: Style,
    message_id: Option<&Arc<str>>,
) -> Vec<Line<'static>> {
    match content {
        Content::Text(text) => {
            text_with_link_preview_lines(text, accent, context, start_line, is_from_me, message_id)
        }
        Content::Image(media) => media_card_lines(
            "Photo", media, accent, context, start_line, is_from_me, true,
        ),
        Content::Video(media) => video_card_lines(media, accent, context, start_line, is_from_me),
        Content::Audio(media) if uses_document_card(media, context.presentation) => {
            document_card_lines(true, media, accent, context, start_line, is_from_me)
        }
        Content::Audio(media) => media_card_lines(
            "Voice note",
            media,
            accent,
            context,
            start_line,
            is_from_me,
            false,
        ),
        Content::File(media) if uses_document_card(media, context.presentation) => {
            document_card_lines(false, media, accent, context, start_line, is_from_me)
        }
        Content::File(media) => media_card_lines(
            "File", media, accent, context, start_line, is_from_me, false,
        ),
        Content::Sticker(media) => media_card_lines(
            "Sticker", media, accent, context, start_line, is_from_me, true,
        ),
        Content::LinkPreview(link) => {
            link_preview_card_lines(link, accent, context, start_line, is_from_me)
        }
        Content::Cards(cards) => {
            card_collection_lines(cards, accent, context, start_line, is_from_me)
        }
        Content::Poll(poll) => text_content_lines(
            &poll_text(poll),
            context.content_width,
            accent,
            context.presentation,
        ),
        Content::Deleted => text_content_lines(
            "[deleted]",
            context.content_width,
            accent,
            context.presentation,
        ),
        Content::Unsupported(kind) => text_content_lines(
            &format!("[unsupported: {kind}]"),
            context.content_width,
            accent,
            context.presentation,
        ),
    }
}

fn poll_text(poll: &chat_core::Poll) -> String {
    let mut text = format!("POLL: {}", poll.question);
    for (index, option) in poll.options.iter().enumerate() {
        text.push('\n');
        text.push_str(&format!("{}. {}", index + 1, option.label));
        let votes = poll
            .votes
            .iter()
            .filter(|vote| vote.options.iter().any(|selected| selected == &option.id))
            .count();
        if votes > 0 {
            text.push_str(&format!("  ({votes})"));
        }
    }
    if let Some(selectable) = poll.selectable_options_count
        && selectable > 0
    {
        text.push('\n');
        if selectable == 1 {
            text.push_str("Choose one option");
        } else {
            text.push_str(&format!("Choose up to {selectable} options"));
        }
    }
    text
}

fn text_with_link_preview_lines(
    text: &str,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
    message_id: Option<&Arc<str>>,
) -> Vec<Line<'static>> {
    let Some(detected_url) = first_url_in_text(text) else {
        return text_content_lines(text, context.content_width, accent, context.presentation);
    };

    let metadata = context.link_metadata.get(detected_url.url);
    if metadata.is_none()
        && let Some(message_id) = message_id
    {
        context.link_preview_requests.push(LinkPreviewRequest {
            message_id: message_id.clone(),
            url: Arc::<str>::from(detected_url.url),
        });
        return text_content_lines(text, context.content_width, accent, context.presentation);
    }

    let Some(metadata) = metadata else {
        return text_content_lines(text, context.content_width, accent, context.presentation);
    };

    if !link_metadata_is_useful(metadata) {
        return text_content_lines(text, context.content_width, accent, context.presentation);
    }

    let mut lines = Vec::new();
    let text_without_url = remove_first_url_from_text(text);
    if !text_without_url.is_empty() {
        lines.extend(text_content_lines(
            &text_without_url,
            context.content_width,
            accent,
            context.presentation,
        ));
    }

    if let Some(image) = &metadata.image {
        let Some(image_lines) = link_image_card_lines(
            "Photo",
            image,
            accent,
            context,
            start_line + lines.len(),
            is_from_me,
        ) else {
            return text_content_lines(text, context.content_width, accent, context.presentation);
        };
        lines.extend(image_lines);
    } else {
        let link = chat_core::LinkPreview {
            url: Arc::<str>::from(detected_url.url),
            title: metadata.title.clone(),
            description: metadata.description.clone(),
            image: None,
        };
        lines.extend(link_preview_card_lines(
            &link,
            accent,
            context,
            start_line + lines.len(),
            is_from_me,
        ));
    }
    lines
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UrlInText<'a> {
    token: &'a str,
    url: &'a str,
}

fn first_url_in_text(text: &str) -> Option<UrlInText<'_>> {
    slack_url_in_text(text).or_else(|| text.split_whitespace().find_map(url_in_token))
}

fn slack_url_in_text(text: &str) -> Option<UrlInText<'_>> {
    let mut search_start = 0;
    while let Some(relative_start) = text[search_start..].find('<') {
        let start = search_start + relative_start;
        let inner_start = start + 1;
        let Some(relative_end) = text[inner_start..].find('>') else {
            break;
        };
        let end = inner_start + relative_end;
        let token = &text[start..=end];
        let inner = &text[inner_start..end];
        let url = inner
            .split_once('|')
            .map(|(url, _label)| url)
            .unwrap_or(inner)
            .trim();

        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(UrlInText { token, url });
        }
        search_start = end + 1;
    }
    None
}

fn url_in_token(token: &str) -> Option<UrlInText<'_>> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }

    let candidate = token
        .trim_start_matches(['<', '(', '[', '{'])
        .trim_end_matches(['.', ',', ')', ']', '}', '>']);
    let url = candidate
        .split_once('|')
        .map(|(url, _label)| url)
        .unwrap_or(candidate);

    (url.starts_with("https://") || url.starts_with("http://"))
        .then_some(UrlInText { token, url })
        .filter(|detected| !detected.url.is_empty())
}

fn remove_first_url_from_text(text: &str) -> String {
    let Some(detected_url) = first_url_in_text(text) else {
        return text.trim().to_owned();
    };
    text.replacen(detected_url.token, "", 1)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn link_metadata_is_useful(metadata: &LinkMetadata) -> bool {
    metadata
        .image
        .as_ref()
        .is_some_and(link_metadata_image_is_useful)
        || link_metadata_has_text(metadata)
}

fn link_metadata_image_is_useful(media: &chat_core::Media) -> bool {
    media_preview_source(media).is_some()
}

fn link_metadata_has_text(metadata: &LinkMetadata) -> bool {
    metadata
        .title
        .as_deref()
        .is_some_and(|title| !title.trim().is_empty())
        || metadata
            .description
            .as_deref()
            .is_some_and(|description| !description.trim().is_empty())
}

fn text_content_lines(
    text: &str,
    content_width: u16,
    accent: Style,
    presentation: ConversationPresentation,
) -> Vec<Line<'static>> {
    match presentation {
        ConversationPresentation::Bubbles => text_bubble_lines(text, content_width, accent),
        ConversationPresentation::Flat => text_flat_lines(text, content_width, accent),
    }
}

fn text_flat_lines(text: &str, content_width: u16, _accent: Style) -> Vec<Line<'static>> {
    wrap_markdown_text(text, flat_text_width(content_width))
        .into_iter()
        .map(Line::from)
        .collect()
}

fn text_bubble_lines(text: &str, content_width: u16, accent: Style) -> Vec<Line<'static>> {
    let max_inner_width = bubble_inner_width(content_width);
    let wrapped = wrap_markdown_text(text, max_inner_width);
    let inner_width = wrapped
        .iter()
        .map(|spans| spans_width(spans))
        .max()
        .unwrap_or_default()
        .max(BUBBLE_MIN_WIDTH.saturating_sub(4))
        .min(max_inner_width.max(1));

    let mut lines = Vec::with_capacity(wrapped.len() + 2);
    lines.push(bubble_border_line('╭', '─', '╮', inner_width, accent));
    for spans in wrapped {
        lines.push(bubble_text_line(spans, inner_width, accent));
    }
    lines.push(bubble_border_line('╰', '─', '╯', inner_width, accent));
    lines
}

fn card_collection_lines(
    cards: &[chat_core::Card],
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut index = 0;
    while index < cards.len() {
        let run = inline_image_run_len(&cards[index..]);
        // Lay two or more consecutive image attachments out side by side (Flat
        // presentation only) to save vertical space.
        if run >= 2 && context.presentation == ConversationPresentation::Flat {
            lines.extend(inline_image_grid_lines(
                &cards[index..index + run],
                accent,
                context,
                start_line + lines.len(),
                is_from_me,
            ));
            index += run;
        } else {
            lines.extend(generic_card_lines(
                &cards[index],
                accent,
                context,
                start_line + lines.len(),
                is_from_me,
            ));
            index += 1;
        }
    }
    lines
}

/// True for an image attachment card (one we render as a bare thumbnail without
/// its filename/URL). Used to decide both suppression and side-by-side layout.
fn card_is_media_preview(card: &chat_core::Card) -> bool {
    matches!(card.kind, chat_core::CardKind::MediaPreview)
}

fn is_inline_image_card(card: &chat_core::Card) -> bool {
    card_is_media_preview(card) && (card.image.is_some() || card.thumbnail.is_some())
}

/// Number of consecutive inline image cards at the front of `cards`.
fn inline_image_run_len(cards: &[chat_core::Card]) -> usize {
    cards
        .iter()
        .take_while(|card| is_inline_image_card(card))
        .count()
}

/// How many fixed-width image thumbnails fit across `content_width`, clamped to
/// at least one. Pure function of the width so the layout and the line-count
/// pass agree without decoding any image.
fn inline_image_columns_per_row(content_width: u16) -> usize {
    let column = INLINE_IMAGE_COLUMN_WIDTH;
    let gap = INLINE_IMAGE_COLUMN_GAP;
    (content_width.saturating_add(gap) / column.saturating_add(gap)).max(1) as usize
}

/// Renders a run of image cards as a grid of fixed-width thumbnails followed by
/// any captions, instead of one full-width image per line.
fn inline_image_grid_lines(
    cards: &[chat_core::Card],
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let _ = is_from_me;
    let accent = card_accent_style(cards[0].accent_color.as_ref(), accent);
    let preview_color = accent.fg.unwrap_or(Color::DarkGray);
    let columns = inline_image_columns_per_row(context.content_width)
        .min(cards.len())
        .max(1);
    let column_width = INLINE_IMAGE_COLUMN_WIDTH;
    let gap = INLINE_IMAGE_COLUMN_GAP as usize;
    let rows = LINK_PREVIEW_THUMBNAIL_ROWS as usize;
    let mut lines = Vec::new();

    for chunk in cards.chunks(columns) {
        let row_base = start_line + lines.len();
        let mut column_rows: Vec<Vec<Vec<Span<'static>>>> = Vec::with_capacity(chunk.len());
        for (column_index, card) in chunk.iter().enumerate() {
            let image = card
                .image
                .as_ref()
                .or(card.thumbnail.as_ref())
                .expect("inline image card always has an image");
            let (preview_rows, source, error, _ready) = media_preview_rows(
                image,
                context,
                column_width,
                LINK_PREVIEW_THUMBNAIL_ROWS,
                preview_color,
            );
            if let Some((path, preview_path, retrieve)) = media_hit_paths(image, &source, &error) {
                let start_col = column_index * (column_width as usize + gap);
                context.media_hits.push(MediaHit {
                    start_line: row_base,
                    end_line: row_base + preview_rows.len().saturating_sub(1),
                    start_col: start_col as u16,
                    end_col: (start_col + column_width as usize) as u16,
                    preview_start_col: start_col as u16,
                    preview_end_col: (start_col + column_width as usize) as u16,
                    path,
                    preview_path,
                    title: card
                        .title
                        .as_deref()
                        .unwrap_or(image.file_name.as_ref())
                        .to_owned(),
                    caption: card.body.as_deref().map(str::to_owned),
                    retrieve,
                    play: None,
                    preview_skip_rows: 0,
                    play_badge: false,
                    open: None,
                });
            }
            column_rows.push(preview_rows);
        }

        for row in 0..rows {
            let mut spans = Vec::new();
            for (column_index, preview_rows) in column_rows.iter().enumerate() {
                if column_index > 0 {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
                let mut cells = preview_rows
                    .get(row)
                    .cloned()
                    .unwrap_or_else(|| empty_preview_row(column_width));
                pad_spans_to_width(&mut cells, column_width);
                spans.extend(cells);
            }
            lines.push(Line::from(spans));
        }
    }

    // Captions live below the thumbnail grid, wrapped to the full body width.
    let caption_width = flat_text_width(context.content_width).max(1);
    for card in cards {
        if let Some(body) = card.body.as_deref() {
            let body = sanitize_flat_provider_text(body);
            for row in wrap_markdown_text(&body, caption_width) {
                lines.push(flat_card_spans_line(accent, row));
            }
        }
    }

    lines
}

/// Right-pads a preview row with blanks so it occupies exactly `width` cells,
/// keeping adjacent grid columns aligned even when a thumbnail decodes narrow.
fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: u16) {
    let current = spans_width(spans);
    let target = width as usize;
    if current < target {
        spans.push(Span::raw(" ".repeat(target - current)));
    }
}

fn flat_card_lines(
    card: &chat_core::Card,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let card_width = flat_text_width(context.content_width).max(1) as u16;
    let accent = card_accent_style(card.accent_color.as_ref(), accent);
    // Image attachment cards are self-describing: the thumbnail and any caption
    // are enough, so the filename (`title`) and download `url` are noise in the
    // transcript. They remain on the card for the Details pane.
    let hide_filename_and_url = card_is_media_preview(card);
    let mut lines = Vec::new();

    if let Some(image) = card.image.as_ref().or(card.thumbnail.as_ref()) {
        let (preview_rows, source, error, _ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        if let Some((path, preview_path, retrieve)) = media_hit_paths(image, &source, &error) {
            context.media_hits.push(MediaHit {
                start_line: start_line + lines.len(),
                end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
                start_col: if is_from_me {
                    context
                        .content_width
                        .saturating_sub(card_width.saturating_add(4))
                } else {
                    0
                },
                end_col: context.content_width,
                preview_start_col: if is_from_me {
                    context
                        .content_width
                        .saturating_sub(card_width.saturating_add(4))
                } else {
                    0
                },
                preview_end_col: context.content_width,
                path,
                preview_path,
                title: card
                    .title
                    .as_deref()
                    .unwrap_or(image.file_name.as_ref())
                    .to_owned(),
                caption: card.body.as_deref().map(str::to_owned),
                retrieve,
                play: None,
                preview_skip_rows: 0,
                play_badge: false,
                open: None,
            });
        }
        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| flat_card_preview_line(accent, row)),
        );
    }

    if let Some(subtitle) = card.subtitle.as_deref() {
        lines.push(flat_card_text_line(
            accent,
            subtitle,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(title) = card.title.as_deref().filter(|_| !hide_filename_and_url) {
        lines.push(flat_card_text_line(
            accent,
            title,
            card_width,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(body) = card.body.as_deref() {
        let body = sanitize_flat_provider_text(body);
        for row in wrap_markdown_text(&body, card_width as usize) {
            lines.push(flat_card_spans_line(accent, row));
        }
    }
    for field in &card.fields {
        let text = field
            .title
            .as_deref()
            .map(|title| format!("{title}: {}", field.value))
            .unwrap_or_else(|| field.value.to_string());
        lines.push(flat_card_text_line(
            accent,
            &text,
            card_width,
            Style::default().fg(Color::Gray),
        ));
    }
    if !card.actions.is_empty() {
        lines.push(flat_card_spans_line(
            accent,
            card_action_spans(&card.actions),
        ));
    }
    if let Some(footer) = card.footer.as_deref() {
        lines.push(flat_card_text_line(
            accent,
            footer,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(url) = card.url.as_deref().filter(|_| !hide_filename_and_url) {
        lines.push(flat_card_text_line(
            accent,
            url,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }

    if lines.is_empty() {
        lines.push(flat_card_text_line(
            accent,
            "Card",
            card_width,
            Style::default(),
        ));
    }
    lines
}

fn generic_card_lines(
    card: &chat_core::Card,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    if context.presentation == ConversationPresentation::Flat {
        return flat_card_lines(card, accent, context, start_line, is_from_me);
    }

    let card_width = link_preview_card_width(context.content_width);
    let accent = card_accent_style(card.accent_color.as_ref(), media_card_accent(accent));
    let hide_filename_and_url = card_is_media_preview(card);
    let mut lines = vec![card_border_line('╭', '─', '╮', card_width, accent)];

    if let Some(image) = card.image.as_ref().or(card.thumbnail.as_ref()) {
        let (preview_rows, source, error, _ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        if let Some((path, preview_path, retrieve)) = media_hit_paths(image, &source, &error) {
            let hit_width = card_width.saturating_add(4).min(context.content_width);
            let start_col = if is_from_me {
                context.content_width.saturating_sub(hit_width)
            } else {
                0
            };
            context.media_hits.push(MediaHit {
                start_line: start_line + lines.len(),
                end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
                start_col,
                end_col: start_col.saturating_add(hit_width),
                preview_start_col: start_col.saturating_add(2),
                preview_end_col: start_col.saturating_add(2).saturating_add(card_width),
                path,
                preview_path,
                title: card
                    .title
                    .as_deref()
                    .unwrap_or(image.file_name.as_ref())
                    .to_owned(),
                caption: card.body.as_deref().map(str::to_owned),
                retrieve,
                play: None,
                preview_skip_rows: 0,
                play_badge: false,
                open: None,
            });
        }
        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| card_preview_line(accent, row, card_width)),
        );
    }

    if let Some(subtitle) = card.subtitle.as_deref() {
        lines.push(card_text_line(
            accent,
            subtitle,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(title) = card.title.as_deref().filter(|_| !hide_filename_and_url) {
        lines.push(card_text_line(
            accent,
            title,
            card_width,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(body) = card.body.as_deref() {
        for row in wrap_markdown_text(body, card_width as usize) {
            lines.push(card_spans_line(accent, row, card_width));
        }
    }
    for field in &card.fields {
        let text = field
            .title
            .as_deref()
            .map(|title| format!("{title}: {}", field.value))
            .unwrap_or_else(|| field.value.to_string());
        lines.push(card_text_line(
            accent,
            &text,
            card_width,
            Style::default().fg(Color::Gray),
        ));
    }
    if !card.actions.is_empty() {
        lines.push(card_spans_line(
            accent,
            card_action_spans(&card.actions),
            card_width,
        ));
    }
    if let Some(footer) = card.footer.as_deref() {
        lines.push(card_text_line(
            accent,
            footer,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(url) = card.url.as_deref().filter(|_| !hide_filename_and_url) {
        lines.push(card_text_line(
            accent,
            url,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }

    if lines.len() == 1 {
        lines.push(card_text_line(accent, "Card", card_width, Style::default()));
    }
    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    lines
}

/// Renders card actions (Block Kit buttons) as a row of button-styled pills,
/// e.g. `[ 🧰 Acknowledge ]  [ ✔ Close ]`. Actions carrying a URL are styled
/// like links (openable via the message "open link" action); interactive-only
/// buttons render as inert labels since Slack interactivity round-trips are
/// not supported.
fn card_action_spans(actions: &[chat_core::CardAction]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for action in actions {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let style = Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(if action.url.is_some() {
                Color::Cyan
            } else {
                Color::Gray
            });
        spans.push(Span::styled(format!("[ {} ]", action.label), style));
    }
    spans
}

fn card_accent_style(color: Option<&chat_core::CardColor>, fallback: Style) -> Style {
    let Some(color) = color else {
        return fallback;
    };
    fallback.fg(match color {
        chat_core::CardColor::Named(value) => match value.as_ref() {
            "good" => Color::Green,
            "warning" => Color::Yellow,
            "danger" => Color::Red,
            "primary" => Color::Blue,
            _ => fallback.fg.unwrap_or(Color::DarkGray),
        },
        chat_core::CardColor::Hex(value) => {
            terminal_color_from_hex(value).unwrap_or_else(|| fallback.fg.unwrap_or(Color::DarkGray))
        }
    })
}

fn terminal_color_from_hex(value: &str) -> Option<Color> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(red, green, blue))
}

fn card_collection_text(cards: &[chat_core::Card]) -> String {
    cards
        .iter()
        .map(card_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn card_text(card: &chat_core::Card) -> String {
    let mut parts = Vec::new();
    if let Some(title) = &card.title {
        parts.push(title.to_string());
    }
    if let Some(body) = &card.body {
        parts.push(body.to_string());
    }
    for field in &card.fields {
        parts.push(
            field
                .title
                .as_deref()
                .map(|title| format!("{title}: {}", field.value))
                .unwrap_or_else(|| field.value.to_string()),
        );
    }
    if let Some(footer) = &card.footer {
        parts.push(footer.to_string());
    }
    parts.join("\n")
}

fn link_preview_card_lines(
    link: &chat_core::LinkPreview,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    if context.presentation == ConversationPresentation::Flat {
        return flat_link_preview_card_lines(link, accent, context, start_line);
    }

    let card_width = link_preview_card_width(context.content_width);
    let accent = media_card_accent(accent);
    let mut lines = vec![card_border_line('╭', '─', '╮', card_width, accent)];

    if let Some(image) = &link.image {
        let (preview_rows, source, error, _ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );

        if let (Some(preview_path), None) = (&source, &error) {
            let hit_width = card_width.saturating_add(4).min(context.content_width);
            let start_col = if is_from_me {
                context.content_width.saturating_sub(hit_width)
            } else {
                0
            };
            context.media_hits.push(MediaHit {
                start_line: start_line + lines.len(),
                end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
                start_col,
                end_col: start_col.saturating_add(hit_width),
                preview_start_col: start_col.saturating_add(2),
                preview_end_col: start_col.saturating_add(2).saturating_add(card_width),
                path: media_open_source(image).unwrap_or_else(|| preview_path.clone()),
                preview_path: preview_path.clone(),
                title: clean_link_preview_text(link.title.as_deref())
                    .unwrap_or_else(|| "Link preview image".to_owned()),
                caption: clean_link_preview_text(link.description.as_deref()),
                retrieve: None,
                play: None,
                preview_skip_rows: 0,
                play_badge: false,
                open: None,
            });
        }

        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| card_preview_line(accent, row, card_width)),
        );
    }

    let title = clean_link_preview_text(link.title.as_deref()).unwrap_or_else(|| "Link".to_owned());
    let description = clean_link_preview_text(link.description.as_deref());
    let source = link_preview_source_label(link.url.as_ref());

    lines.push(card_text_line(
        accent,
        &title,
        card_width,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(description) = description.as_deref() {
        lines.push(card_text_line(
            accent,
            description,
            card_width,
            Style::default().fg(Color::Gray),
        ));
    }
    lines.push(card_text_line(
        accent,
        &source,
        card_width,
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    lines
}

fn flat_link_preview_card_lines(
    link: &chat_core::LinkPreview,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
) -> Vec<Line<'static>> {
    let card_width = link_preview_card_width(context.content_width);
    let accent = media_card_accent(accent);
    let mut lines = Vec::new();

    if let Some(image) = &link.image {
        let (preview_rows, source, error, _ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );

        if let (Some(preview_path), None) = (&source, &error) {
            context.media_hits.push(MediaHit {
                start_line: start_line + lines.len(),
                end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
                start_col: 0,
                end_col: card_width,
                preview_start_col: 0,
                preview_end_col: card_width,
                path: media_open_source(image).unwrap_or_else(|| preview_path.clone()),
                preview_path: preview_path.clone(),
                title: clean_link_preview_text(link.title.as_deref())
                    .unwrap_or_else(|| "Link preview image".to_owned()),
                caption: clean_link_preview_text(link.description.as_deref()),
                retrieve: None,
                play: None,
                preview_skip_rows: 0,
                play_badge: false,
                open: None,
            });
        }

        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| flat_card_preview_line(accent, row)),
        );
    }

    let title = clean_link_preview_text(link.title.as_deref()).unwrap_or_else(|| "Link".to_owned());
    let description = clean_link_preview_text(link.description.as_deref());
    let source = link_preview_source_label(link.url.as_ref());

    lines.push(flat_card_text_line(
        accent,
        &title,
        card_width,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(description) = description.as_deref() {
        let description = sanitize_flat_provider_text(description);
        for row in wrap_markdown_text(&description, card_width as usize) {
            lines.push(flat_card_spans_line(accent, row));
        }
    }
    lines.push(flat_card_text_line(
        accent,
        &source,
        card_width,
        Style::default().fg(Color::DarkGray),
    ));
    lines
}

fn link_preview_source_label(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let host = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme)
        .trim_start_matches("www.");

    if host.is_empty() {
        url.to_owned()
    } else {
        host.to_owned()
    }
}

fn clean_link_preview_text(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let text = strip_html_tags(&decode_html_entities(value))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() { None } else { Some(text) }
}

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

fn decode_html_entities(value: &str) -> String {
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

fn link_image_card_lines(
    label: &str,
    media: &chat_core::Media,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Option<Vec<Line<'static>>> {
    let card_width =
        media_card_width_for_media(media, context.content_width, MEDIA_PREVIEW_ROWS, label);
    let accent = media_card_accent(accent);
    let (preview_rows, source, error, ready) = media_preview_rows(
        media,
        context,
        card_width,
        MEDIA_PREVIEW_ROWS,
        accent.fg.unwrap_or(Color::DarkGray),
    );
    if !ready {
        return None;
    }
    let preview_path = source.filter(|_| error.is_none())?;
    let mut lines = vec![
        card_border_line('╭', '─', '╮', card_width, accent),
        card_text_line(
            accent,
            label,
            card_width,
            accent.add_modifier(Modifier::BOLD),
        ),
    ];

    let hit_width = card_width.saturating_add(4).min(context.content_width);
    let start_col = if is_from_me {
        context.content_width.saturating_sub(hit_width)
    } else {
        0
    };
    context.media_hits.push(MediaHit {
        start_line: start_line + lines.len(),
        end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
        start_col,
        end_col: start_col.saturating_add(hit_width),
        preview_start_col: start_col.saturating_add(2),
        preview_end_col: start_col.saturating_add(2).saturating_add(card_width),
        path: media_open_source(media).unwrap_or_else(|| preview_path.clone()),
        preview_path,
        title: media.file_name.to_string(),
        caption: media.caption.as_deref().map(str::to_owned),
        retrieve: None,
        play: None,
        preview_skip_rows: 0,
        play_badge: false,
        open: None,
    });

    lines.extend(
        preview_rows
            .into_iter()
            .map(|row| card_preview_line(accent, row, card_width)),
    );
    lines.push(card_text_line(
        accent,
        &format!("file: {}{}", media.file_name, format_media_size(media)),
        card_width,
        Style::default(),
    ));
    if let Some(caption) = visible_media_caption(media) {
        for row in wrap_markdown_text(caption, card_width as usize) {
            lines.push(card_spans_line(accent, row, card_width));
        }
    }
    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    Some(lines)
}

fn media_card_lines(
    label: &str,
    media: &chat_core::Media,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
    visual: bool,
) -> Vec<Line<'static>> {
    if context.presentation == ConversationPresentation::Flat {
        return flat_media_card_lines(label, media, accent, context, start_line);
    }

    let accent = media_card_accent(accent);
    let caption = visible_media_caption(media);

    // Visual media (photos, stickers) speak for themselves: skip the label and
    // filename rows and shrink the card so the border hugs the image's form
    // factor. Other media (files, voice notes) keep the descriptive rows.
    let (preview_rows, card_width, source, error, _ready) = if visual {
        let max_width = media_card_width(context.content_width);
        let (rows, source, error, _ready) = media_preview_rows(
            media,
            context,
            max_width,
            MEDIA_PREVIEW_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        let trimmed: Vec<Vec<Span<'static>>> =
            rows.into_iter().map(strip_preview_padding).collect();
        let image_width = trimmed
            .iter()
            .map(|row| spans_width(row))
            .max()
            .unwrap_or_default() as u16;
        let caption_width = caption
            .map(|caption| UnicodeWidthStr::width(caption) as u16)
            .unwrap_or_default();
        let card_width = image_width.max(caption_width).max(1).min(max_width);
        (trimmed, card_width, source, error, _ready)
    } else {
        let card_width =
            media_card_width_for_media(media, context.content_width, MEDIA_PREVIEW_ROWS, label);
        let (rows, source, error, _ready) = media_preview_rows(
            media,
            context,
            card_width,
            MEDIA_PREVIEW_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        (rows, card_width, source, error, _ready)
    };

    let mut lines = vec![card_border_line('╭', '─', '╮', card_width, accent)];
    if !visual {
        lines.push(card_text_line(
            accent,
            label,
            card_width,
            accent.add_modifier(Modifier::BOLD),
        ));
    }

    if let Some((path, preview_path, retrieve)) = media_hit_paths(media, &source, &error) {
        let hit_width = card_width.saturating_add(4).min(context.content_width);
        let start_col = if is_from_me {
            context.content_width.saturating_sub(hit_width)
        } else {
            0
        };
        context.media_hits.push(MediaHit {
            start_line: start_line + lines.len(),
            end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
            start_col,
            end_col: start_col.saturating_add(hit_width),
            preview_start_col: start_col.saturating_add(2),
            preview_end_col: start_col.saturating_add(2).saturating_add(card_width),
            path,
            preview_path,
            title: media.file_name.to_string(),
            caption: caption.map(str::to_owned),
            retrieve,
            play: None,
            preview_skip_rows: 0,
            play_badge: false,
            open: None,
        });
    }

    lines.extend(
        preview_rows
            .into_iter()
            .map(|row| card_preview_line(accent, center_preview_row(row, card_width), card_width)),
    );

    if !visual {
        lines.push(card_text_line(
            accent,
            &format!("file: {}{}", media.file_name, format_media_size(media)),
            card_width,
            Style::default(),
        ));
    }

    if error.is_some() || source.is_none() {
        if error.is_none() && media_awaits_retrieve(media) {
            lines.push(card_text_line(
                accent,
                &format!("Retrieve media{}", format_media_size(media)),
                card_width,
                accent.add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(card_text_line(
                accent,
                "Preview unavailable",
                card_width,
                Style::default().fg(if error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ));
        }
    }

    if let Some(caption) = caption {
        for row in wrap_markdown_text(caption, card_width as usize) {
            lines.push(card_spans_line(accent, row, card_width));
        }
    }

    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    lines
}

/// Documents and voice notes that are not images get a compact native-style
/// card (type badge, name, size, open action) instead of an empty preview box.
/// Image files sent as documents keep the preview card.
fn uses_document_card(media: &chat_core::Media, presentation: ConversationPresentation) -> bool {
    presentation == ConversationPresentation::Bubbles && !media.mime_type.starts_with("image/")
}

const DOCUMENT_CARD_MIN_WIDTH: u16 = 24;
const DOCUMENT_CARD_MAX_WIDTH: u16 = 48;
/// Badge column: up to five badge characters, padding, and a gap.
const DOCUMENT_BADGE_WIDTH: usize = 8;

/// Width of a document card. Depends only on the message data and the
/// content width so drawing and line counting always agree.
fn document_card_width(voice: bool, media: &chat_core::Media, content_width: u16) -> u16 {
    let text = UnicodeWidthStr::width(media.file_name.as_ref()).max(UnicodeWidthStr::width(
        document_detail(voice, media).as_str(),
    ));
    let wanted = (text + DOCUMENT_BADGE_WIDTH) as u16;
    let max = media_card_width(content_width).min(DOCUMENT_CARD_MAX_WIDTH);
    wanted.clamp(DOCUMENT_CARD_MIN_WIDTH.min(max), max).max(1)
}

fn document_badge(voice: bool, media: &chat_core::Media) -> String {
    if voice {
        "♪".to_owned()
    } else {
        crate::attach::file_type_badge(&media.file_name, &media.mime_type)
    }
}

fn document_detail(voice: bool, media: &chat_core::Media) -> String {
    let kind = if voice {
        "Voice note".to_owned()
    } else {
        crate::attach::file_type_badge(&media.file_name, &media.mime_type)
    };
    match media.size_bytes {
        Some(size) => format!("{kind} · {}", crate::attach::format_byte_size(size)),
        None => kind,
    }
}

fn truncate_to_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + char_width + 1 > width {
            break;
        }
        used += char_width;
        out.push(character);
    }
    out.push('…');
    out
}

/// Native-style document / voice note bubble:
///
/// ```text
/// ╭──────────────────────────╮
/// │  PDF   report-q3.pdf      │
/// │        PDF · 1.2 MB       │
/// │        Open ↗             │
/// ╰──────────────────────────╯
/// ```
///
/// Clicking opens the cached file in the system app, or retrieves it first
/// when it was too large to download automatically. Draw only stats the
/// cached path; nothing is read or decoded here.
fn document_card_lines(
    voice: bool,
    media: &chat_core::Media,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let accent = media_card_accent(accent);
    let card_width = document_card_width(voice, media, context.content_width);
    let text_width = (card_width as usize).saturating_sub(DOCUMENT_BADGE_WIDTH);
    let badge_style = accent.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let badge: String = document_badge(voice, media).chars().take(5).collect();
    let badge_pad = 5usize.saturating_sub(UnicodeWidthStr::width(badge.as_str()));
    let badge_cell = format!(
        " {}{badge}{} ",
        " ".repeat(badge_pad / 2),
        " ".repeat(badge_pad - badge_pad / 2)
    );
    let gutter = " ".repeat(DOCUMENT_BADGE_WIDTH);

    let local = media
        .local_path
        .as_ref()
        .filter(|path| path.exists())
        .cloned();
    let retrieve = local.is_none() && media_awaits_retrieve(media);
    let (action, action_style) = if local.is_some() {
        (
            if voice { "Play ↗" } else { "Open ↗" }.to_owned(),
            accent.add_modifier(Modifier::BOLD),
        )
    } else if retrieve {
        (
            format!("Retrieve{}", format_media_size(media)),
            accent.add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "Not downloaded".to_owned(),
            Style::default().fg(Color::DarkGray),
        )
    };

    let mut lines = vec![card_border_line('╭', '─', '╮', card_width, accent)];
    let first_row = start_line + lines.len();
    lines.push(card_spans_line(
        accent,
        vec![
            Span::styled(badge_cell, badge_style),
            Span::raw(" "),
            Span::styled(
                // badge cell (7) + gap (1) == DOCUMENT_BADGE_WIDTH
                truncate_to_width(&media.file_name, text_width),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ],
        card_width,
    ));
    lines.push(card_spans_line(
        accent,
        vec![
            Span::raw(gutter.clone()),
            Span::styled(
                truncate_to_width(&document_detail(voice, media), text_width),
                Style::default().fg(Color::DarkGray),
            ),
        ],
        card_width,
    ));
    lines.push(card_spans_line(
        accent,
        vec![Span::raw(gutter), Span::styled(action, action_style)],
        card_width,
    ));
    let last_row = start_line + lines.len() - 1;

    if local.is_some() || retrieve {
        let hit_width = card_width.saturating_add(4).min(context.content_width);
        let start_col = if is_from_me {
            context.content_width.saturating_sub(hit_width)
        } else {
            0
        };
        let path = local
            .clone()
            .or_else(|| media.local_path.clone())
            .unwrap_or_default();
        context.media_hits.push(MediaHit {
            start_line: first_row,
            end_line: last_row,
            start_col,
            end_col: start_col.saturating_add(hit_width),
            preview_start_col: start_col.saturating_add(2),
            preview_end_col: start_col.saturating_add(2).saturating_add(card_width),
            preview_path: path.clone(),
            path,
            title: media.file_name.to_string(),
            caption: visible_media_caption(media).map(str::to_owned),
            retrieve: retrieve.then(|| media.clone()),
            play: None,
            preview_skip_rows: 0,
            play_badge: false,
            open: local,
        });
    }

    if let Some(caption) = visible_media_caption(media) {
        for row in wrap_markdown_text(caption, card_width as usize) {
            lines.push(card_spans_line(accent, row, card_width));
        }
    }
    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    lines
}

fn document_card_line_count(voice: bool, media: &chat_core::Media, content_width: u16) -> usize {
    let caption = visible_media_caption(media)
        .map(|caption| {
            let width = document_card_width(voice, media, content_width);
            wrap_markdown_text(caption, width as usize).len()
        })
        .unwrap_or_default();
    5 + caption
}

/// Returns the media caption only when it carries real text, treating blank or
/// whitespace-only captions as absent.
fn visible_media_caption(media: &chat_core::Media) -> Option<&str> {
    media
        .caption
        .as_deref()
        .map(str::trim)
        .filter(|caption| !caption.is_empty())
        .filter(|caption| !caption.eq_ignore_ascii_case("[empty WhatsApp message]"))
}

/// Drops the surrounding blank padding spans from a decoded preview row so the
/// card border can be shrunk to the image's natural width.
fn strip_preview_padding(row: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let is_blank = |span: &Span<'static>| span.content.chars().all(|character| character == ' ');
    let Some(start) = row.iter().position(|span| !is_blank(span)) else {
        return Vec::new();
    };
    let end = row
        .iter()
        .rposition(|span| !is_blank(span))
        .unwrap_or(start);
    row[start..=end].to_vec()
}

/// Centers a (already trimmed) preview row inside `width`, padding both sides
/// equally so the image sits in the middle of the card.
fn center_preview_row(row: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let content_width = spans_width(&row);
    let total_padding = (width as usize).saturating_sub(content_width);
    if total_padding == 0 {
        return row;
    }
    let left = total_padding / 2;
    let mut centered = Vec::with_capacity(row.len() + 1);
    if left > 0 {
        centered.push(Span::raw(" ".repeat(left)));
    }
    centered.extend(row);
    centered
}

fn flat_media_card_lines(
    label: &str,
    media: &chat_core::Media,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
) -> Vec<Line<'static>> {
    let card_width =
        media_card_width_for_media(media, context.content_width, MEDIA_PREVIEW_ROWS, label);
    let accent = media_card_accent(accent);
    let (preview_rows, source, error, _ready) = media_preview_rows(
        media,
        context,
        card_width,
        MEDIA_PREVIEW_ROWS,
        accent.fg.unwrap_or(Color::DarkGray),
    );
    let mut lines = vec![flat_card_text_line(
        accent,
        &format!("{label}: {}{}", media.file_name, format_media_size(media)),
        card_width,
        Style::default().add_modifier(Modifier::BOLD),
    )];

    if let Some((path, preview_path, retrieve)) = media_hit_paths(media, &source, &error) {
        context.media_hits.push(MediaHit {
            start_line: start_line + lines.len(),
            end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
            start_col: 0,
            end_col: card_width,
            preview_start_col: 0,
            preview_end_col: card_width,
            path,
            preview_path,
            title: media.file_name.to_string(),
            caption: visible_media_caption(media).map(str::to_owned),
            retrieve,
            play: None,
            preview_skip_rows: 0,
            play_badge: false,
            open: None,
        });
    }

    lines.extend(
        preview_rows
            .into_iter()
            .map(|row| flat_card_preview_line(accent, row)),
    );

    if error.is_some() || source.is_none() {
        if error.is_none() && media_awaits_retrieve(media) {
            lines.push(flat_card_text_line(
                accent,
                &format!("Retrieve media{}", format_media_size(media)),
                card_width,
                accent.add_modifier(Modifier::BOLD),
            ));
        } else {
            lines.push(flat_card_text_line(
                accent,
                "Preview unavailable",
                card_width,
                Style::default().fg(if error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                }),
            ));
        }
    }

    if let Some(caption) = visible_media_caption(media) {
        for row in wrap_markdown_text(caption, card_width as usize) {
            lines.push(flat_card_spans_line(accent, row));
        }
    }

    lines
}

/// Human-facing name for a video: the real file name when the provider kept
/// one (Slack uploads, WhatsApp videos sent as documents), otherwise "Video"
/// rather than a content-hash cache key.
fn video_display_name(media: &chat_core::Media) -> &str {
    if video::is_meaningful_file_name(&media.file_name) {
        media.file_name.as_ref()
    } else {
        "Video"
    }
}

/// Local video file eligible for poster/metadata probing.
fn local_video_path(media: &chat_core::Media) -> Option<&PathBuf> {
    media.local_path.as_ref().filter(|path| path.exists())
}

/// Cache-only lookup of probed video info. Queues a background probe for
/// local videos that have not been probed yet; never runs ffprobe itself.
fn video_info_for_render(
    media: &chat_core::Media,
    context: &mut MessageRenderContext<'_>,
) -> Option<Arc<VideoInfo>> {
    let path = local_video_path(media)?;
    probed_info_for_render(path, context)
}

fn probed_info_for_render(
    path: &Path,
    context: &mut MessageRenderContext<'_>,
) -> Option<Arc<VideoInfo>> {
    match context.media_cache.video_info(path) {
        Some(Ok(info)) => Some(info.clone()),
        Some(Err(_)) => None,
        None => {
            context.media_cache.request_video_probe(path.to_path_buf());
            None
        }
    }
}

/// Picks the frame of a looping animation to draw at the shared animation
/// clock, for a `width`×`rows` preview. Draw-safe: reads caches only, queues
/// background decodes for frames not decoded yet, and falls back to the most
/// recent decoded frame so the loop never flashes a placeholder while frames
/// stream in. Returns `None` until at least one frame is decoded.
fn animated_frame_source(
    info: &VideoInfo,
    context: &mut MessageRenderContext<'_>,
    width: u16,
    rows: u16,
) -> Option<PathBuf> {
    let animation = info.animation.as_ref()?;
    let count = animation.frames.len();
    if count < 2 {
        return None;
    }
    let desired = animation.frame_at(context.media_cache.animation_clock_ms());
    let mut chosen = None;
    for offset in 0..count {
        let index = (desired + count - offset) % count;
        let key = MediaPreviewKey {
            path: animation.frames[index].clone(),
            width,
            rows,
        };
        match context.media_cache.get(&key) {
            Some(Ok(_)) => {
                if chosen.is_none() {
                    chosen = Some(index);
                }
            }
            Some(Err(_)) => {}
            None => context
                .media_preview_requests
                .push(MediaPreviewRequest { key }),
        }
    }
    let index = chosen?;
    context.media_cache.mark_animation_active();
    Some(animation.frames[index].clone())
}

/// Current animation frame for a local `.gif` image, queueing the one-off
/// frame extraction the first time the file is seen.
fn gif_image_frame_source(
    media: &chat_core::Media,
    context: &mut MessageRenderContext<'_>,
    width: u16,
    rows: u16,
) -> Option<PathBuf> {
    let path = media
        .local_path
        .as_ref()
        .filter(|path| video::is_gif_file(path) && path.exists())?;
    let info = probed_info_for_render(path, context)?;
    animated_frame_source(&info, context, width, rows)
}

/// Preview image for a video: the extracted ffmpeg poster when available,
/// else the provider's embedded thumbnail.
fn video_preview_source(media: &chat_core::Media, info: Option<&VideoInfo>) -> Option<PathBuf> {
    info.and_then(|info| info.poster.as_ref())
        .filter(|poster| poster.exists())
        .cloned()
        .or_else(|| {
            media
                .thumbnail
                .as_ref()
                .filter(|path| path.exists())
                .cloned()
        })
}

/// Single-line summary shown above the video preview, for example
/// `▶ Video · 0:42 · 1280×720 · 4.2 MB`, or `GIF · 200×200 · 121 KB` for
/// looping GIF-style clips (no play glyph or duration: they play inline).
fn video_title_text(media: &chat_core::Media, info: Option<&VideoInfo>) -> String {
    let gif = info.is_some_and(VideoInfo::is_gif_like);
    let mut parts = if gif {
        vec![if video::is_meaningful_file_name(&media.file_name) {
            format!("GIF · {}", media.file_name)
        } else {
            "GIF".to_owned()
        }]
    } else {
        vec![format!("▶ {}", video_display_name(media))]
    };
    if !gif && let Some(duration_ms) = info.and_then(|info| info.duration_ms) {
        parts.push(video::format_duration(duration_ms));
    }
    if let Some((width, height)) = info.and_then(|info| info.width.zip(info.height)) {
        parts.push(format!("{width}×{height}"));
    }
    if let Some(size) = media.size_bytes {
        parts.push(format_size(size).trim().trim_matches(['(', ')']).to_owned());
    }
    if media_awaits_retrieve(media) {
        parts.push("click to retrieve".to_owned());
    }
    parts.join(" · ")
}

/// Overlays a centered play badge on decoded preview rows. Rows hold one span
/// per terminal cell, so three center cells are swapped for ` ▶ `.
fn overlay_play_badge(rows: &mut [Vec<Span<'static>>]) {
    if rows.is_empty() {
        return;
    }
    let middle = rows.len() / 2;
    let row = &mut rows[middle];
    if row.len() < 5 {
        return;
    }
    let start = row.len() / 2 - 1;
    let badge = Span::styled(
        " ▶ ",
        Style::default()
            .fg(Color::White)
            .bg(Color::Black)
            .add_modifier(Modifier::BOLD),
    );
    row.splice(start..start + 3, [badge]);
}

/// Resolves the click target for a video card: play locally cached files,
/// retrieve large undownloaded ones, and otherwise fall back to viewing the
/// preview image. Returns `None` when there is nothing to activate.
fn video_hit_target(
    media: &chat_core::Media,
    preview: Option<&PathBuf>,
) -> Option<(PathBuf, Option<chat_core::Media>, Option<PathBuf>)> {
    let play = local_video_path(media).cloned();
    let retrieve = media_awaits_retrieve(media).then(|| media.clone());
    let anchor = preview
        .cloned()
        .or_else(|| play.clone())
        .or_else(|| media.local_path.clone());
    if play.is_none() && retrieve.is_none() && preview.is_none() {
        return None;
    }
    Some((anchor.unwrap_or_default(), retrieve, play))
}

/// Video card. The layout is fixed (title row, [`VIDEO_PREVIEW_ROWS`] preview
/// rows, wrapped caption) so late-arriving poster/metadata never changes the
/// line count computed by [`video_card_line_count`]. GIF-style clips loop
/// their frames inline instead of showing a play badge.
fn video_card_lines(
    media: &chat_core::Media,
    accent: Style,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let info = video_info_for_render(media, context);
    let gif = info.as_deref().is_some_and(VideoInfo::is_gif_like);
    let flat = context.presentation == ConversationPresentation::Flat;
    let accent = media_card_accent(accent);
    let card_width = media_card_width(context.content_width);
    let animated = match info.as_deref() {
        Some(info) if gif => animated_frame_source(info, context, card_width, VIDEO_PREVIEW_ROWS),
        _ => None,
    };
    let source = animated.or_else(|| video_preview_source(media, info.as_deref()));
    let (rows, source, error, ready) = preview_rows_for_source(
        source,
        media_awaits_retrieve(media),
        "no preview",
        context,
        card_width,
        VIDEO_PREVIEW_ROWS,
        accent.fg.unwrap_or(Color::DarkGray),
    );
    let has_image = ready && error.is_none() && source.is_some();
    let mut rows: Vec<Vec<Span<'static>>> = rows.into_iter().map(strip_preview_padding).collect();
    if has_image && !gif {
        overlay_play_badge(&mut rows);
    }

    let title = video_title_text(media, info.as_deref());
    let title_style = accent.add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    if flat {
        lines.push(flat_card_text_line(accent, &title, card_width, title_style));
    } else {
        lines.push(card_border_line('╭', '─', '╮', card_width, accent));
        lines.push(card_text_line(accent, &title, card_width, title_style));
    }

    let preview = source.as_ref().filter(|_| error.is_none());
    if let Some((path, retrieve, play)) = video_hit_target(media, preview) {
        let (start_col, hit_width, preview_offset) = if flat {
            (0, card_width, 0)
        } else {
            let hit_width = card_width.saturating_add(4).min(context.content_width);
            let start_col = if is_from_me {
                context.content_width.saturating_sub(hit_width)
            } else {
                0
            };
            (start_col, hit_width, 2)
        };
        // The title row is clickable too, so undownloaded videos without any
        // preview image still have a target.
        let first_line = start_line + lines.len() - 1;
        context.media_hits.push(MediaHit {
            start_line: first_line,
            end_line: first_line + rows.len(),
            start_col,
            end_col: start_col.saturating_add(hit_width),
            preview_start_col: start_col.saturating_add(preview_offset),
            preview_end_col: start_col
                .saturating_add(preview_offset)
                .saturating_add(card_width),
            preview_path: preview.cloned().unwrap_or_else(|| path.clone()),
            path,
            title: if gif {
                "GIF".to_owned()
            } else {
                video_display_name(media).to_owned()
            },
            caption: visible_media_caption(media).map(str::to_owned),
            retrieve,
            play,
            preview_skip_rows: 1,
            play_badge: has_image && !gif,
            open: None,
        });
    }

    for row in rows {
        let row = center_preview_row(row, card_width);
        lines.push(if flat {
            flat_card_preview_line(accent, row)
        } else {
            card_preview_line(accent, row, card_width)
        });
    }

    if let Some(caption) = visible_media_caption(media) {
        for row in wrap_markdown_text(caption, card_width as usize) {
            lines.push(if flat {
                flat_card_spans_line(accent, row)
            } else {
                card_spans_line(accent, row, card_width)
            });
        }
    }
    if !flat {
        lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    }
    lines
}

fn video_card_line_count(
    media: &chat_core::Media,
    content_width: u16,
    presentation: ConversationPresentation,
) -> usize {
    let caption = visible_media_caption(media)
        .map(|caption| wrap_markdown_text(caption, media_card_width(content_width) as usize).len())
        .unwrap_or_default();
    let chrome = match presentation {
        // Top border, title row, bottom border.
        ConversationPresentation::Bubbles => 3,
        // Title row only.
        ConversationPresentation::Flat => 1,
    };
    chrome + VIDEO_PREVIEW_ROWS as usize + caption
}

type PreviewRowsResult = (
    Vec<Vec<Span<'static>>>,
    Option<PathBuf>,
    Option<String>,
    bool,
);

fn media_preview_rows(
    media: &chat_core::Media,
    context: &mut MessageRenderContext<'_>,
    width: u16,
    rows: u16,
    accent: Color,
) -> PreviewRowsResult {
    let source =
        gif_image_frame_source(media, context, width, rows).or_else(|| media_preview_source(media));
    preview_rows_for_source(
        source,
        media_awaits_retrieve(media),
        "no local image",
        context,
        width,
        rows,
        accent,
    )
}

/// Cache-only preview lookup for an already resolved preview `source`. Queues
/// a background decode and returns placeholder rows while it is missing.
fn preview_rows_for_source(
    source: Option<PathBuf>,
    awaits_retrieve: bool,
    missing_label: &str,
    context: &mut MessageRenderContext<'_>,
    width: u16,
    rows: u16,
    accent: Color,
) -> PreviewRowsResult {
    let Some(source) = source else {
        let label = if awaits_retrieve {
            "not downloaded"
        } else {
            missing_label
        };
        return (
            fallback_preview_rows(width, rows, accent, label),
            None,
            None,
            false,
        );
    };

    let key = MediaPreviewKey {
        path: source.clone(),
        width,
        rows,
    };

    match context.media_cache.get(&key) {
        Some(Ok(rows)) => (rows.clone(), Some(source), None, true),
        Some(Err(error)) => (
            fallback_preview_rows(width, rows, accent, "image decode failed"),
            Some(source),
            Some(error.clone()),
            true,
        ),
        None => {
            context
                .media_preview_requests
                .push(MediaPreviewRequest { key });
            (
                fallback_preview_rows(width, rows, accent, "loading image"),
                Some(source),
                None,
                false,
            )
        }
    }
}

fn media_open_source(media: &chat_core::Media) -> Option<PathBuf> {
    media
        .local_path
        .as_ref()
        .filter(|path| is_supported_image(media, path))
        .filter(|path| path.exists())
        .cloned()
        .or_else(|| {
            media
                .thumbnail
                .as_ref()
                .filter(|path| path.exists())
                .cloned()
        })
}

fn media_preview_source(media: &chat_core::Media) -> Option<PathBuf> {
    media
        .thumbnail
        .as_ref()
        .filter(|path| path.exists())
        .cloned()
        .or_else(|| {
            media
                .local_path
                .as_ref()
                .filter(|path| is_supported_image(media, path))
                .filter(|path| path.exists())
                .cloned()
        })
}

fn is_supported_image(media: &chat_core::Media, path: &Path) -> bool {
    media.mime_type.starts_with("image/")
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["gif", "png", "jpg", "jpeg", "webp"]
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
}

fn decode_image_preview_rows(
    path: &Path,
    width: u16,
    rows: u16,
) -> Result<Vec<Vec<Span<'static>>>, String> {
    let reader = image::ImageReader::open(path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?
        .with_guessed_format()
        .map_err(|error| format!("detecting {}: {error}", path.display()))?;
    let image = reader
        .decode()
        .map_err(|error| format!("decoding {}: {error}", path.display()))?;
    let (fit_width, fit_rows) =
        fit_halfblock_cell_size(image.width(), image.height(), width.max(1), rows.max(1));
    let mut resized = image
        .resize_exact(
            u32::from(fit_width.max(1)),
            u32::from(fit_rows.max(1)) * 2,
            FilterType::Triangle,
        )
        .to_rgba8();
    apply_rounded_thumbnail_mask(&mut resized);
    Ok(preview_rows_from_resized_rgba(
        &resized, fit_width, fit_rows, width, rows,
    ))
}

pub fn apply_rounded_thumbnail_mask(image: &mut image::RgbaImage) {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        return;
    }

    let radius = rounded_thumbnail_corner_radius(width, height);
    if radius <= 0.0 {
        return;
    }

    for y in 0..height {
        for x in 0..width {
            let coverage = rounded_rect_pixel_coverage(x, y, width, height, radius);
            if coverage >= 1.0 {
                continue;
            }

            let pixel = image.get_pixel_mut(x, y);
            pixel.0[3] = (f32::from(pixel.0[3]) * coverage).round().clamp(0.0, 255.0) as u8;
        }
    }
}

fn rounded_thumbnail_corner_radius(width: u32, height: u32) -> f32 {
    let shortest_side = width.min(height) as f32;
    (shortest_side * ROUNDED_THUMBNAIL_CORNER_RADIUS_RATIO).clamp(1.0, shortest_side / 2.0)
}

fn rounded_rect_pixel_coverage(x: u32, y: u32, width: u32, height: u32, radius: f32) -> f32 {
    let samples = ROUNDED_THUMBNAIL_MASK_SAMPLES;
    let mut covered = 0_u32;
    let width = width as f32;
    let height = height as f32;

    for sample_y in 0..samples {
        for sample_x in 0..samples {
            let px = x as f32 + (sample_x as f32 + 0.5) / samples as f32;
            let py = y as f32 + (sample_y as f32 + 0.5) / samples as f32;
            if point_inside_rounded_rect(px, py, width, height, radius) {
                covered += 1;
            }
        }
    }

    covered as f32 / (samples * samples) as f32
}

fn point_inside_rounded_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> bool {
    let left = radius.min(width / 2.0);
    let right = (width - radius).max(left);
    let top = radius.min(height / 2.0);
    let bottom = (height - radius).max(top);
    let dx = if x < left {
        left - x
    } else if x > right {
        x - right
    } else {
        0.0
    };
    let dy = if y < top {
        top - y
    } else if y > bottom {
        y - bottom
    } else {
        0.0
    };
    dx.mul_add(dx, dy * dy) <= radius * radius
}

fn preview_rows_from_resized_rgba(
    resized: &image::RgbaImage,
    fit_width: u16,
    fit_rows: u16,
    width: u16,
    rows: u16,
) -> Vec<Vec<Span<'static>>> {
    let top_padding = rows.saturating_sub(fit_rows) / 2;
    let bottom_padding = rows.saturating_sub(fit_rows).saturating_sub(top_padding);

    let mut rendered_rows = Vec::with_capacity(rows as usize);
    for _ in 0..top_padding {
        rendered_rows.push(empty_preview_row(width));
    }
    for row in 0..fit_rows {
        let top_y = u32::from(row) * 2;
        let bottom_y = top_y + 1;
        let mut spans = Vec::with_capacity(fit_width as usize);
        for column in 0..fit_width {
            let x = u32::from(column);
            let top = resized.get_pixel(x, top_y);
            let bottom = resized.get_pixel(x, bottom_y);
            spans.push(halfblock_span(top.0, bottom.0));
        }
        rendered_rows.push(pad_preview_row(spans, fit_width, width));
    }
    for _ in 0..bottom_padding {
        rendered_rows.push(empty_preview_row(width));
    }

    rendered_rows
}

fn fit_halfblock_cell_size(
    image_width: u32,
    image_height: u32,
    max_width: u16,
    max_rows: u16,
) -> (u16, u16) {
    if image_width == 0 || image_height == 0 {
        return (max_width.max(1), max_rows.max(1));
    }

    let max_pixel_width = f64::from(max_width.max(1));
    let max_pixel_height = f64::from(max_rows.max(1)) * 2.0;
    let scale = (max_pixel_width / image_width as f64)
        .min(max_pixel_height / image_height as f64)
        .max(f64::MIN_POSITIVE);
    let fitted_width = ((image_width as f64 * scale).floor() as u16)
        .max(1)
        .min(max_width.max(1));
    let fitted_pixel_height = ((image_height as f64 * scale).floor() as u16)
        .max(1)
        .min(max_rows.max(1).saturating_mul(2));
    let fitted_rows = fitted_pixel_height.div_ceil(2).max(1).min(max_rows.max(1));

    (fitted_width, fitted_rows)
}

fn empty_preview_row(width: u16) -> Vec<Span<'static>> {
    vec![Span::raw(" ".repeat(width as usize))]
}

fn pad_preview_row(
    spans: Vec<Span<'static>>,
    content_width: u16,
    target_width: u16,
) -> Vec<Span<'static>> {
    let left_padding = target_width.saturating_sub(content_width) / 2;
    let right_padding = target_width
        .saturating_sub(content_width)
        .saturating_sub(left_padding);
    let mut padded = Vec::with_capacity(spans.len() + 2);
    if left_padding > 0 {
        padded.push(Span::raw(" ".repeat(left_padding as usize)));
    }
    padded.extend(spans);
    if right_padding > 0 {
        padded.push(Span::raw(" ".repeat(right_padding as usize)));
    }
    padded
}

fn flat_card_preview_line(_accent: Style, preview: Vec<Span<'static>>) -> Line<'static> {
    Line::from(preview)
}

fn flat_card_text_line(_accent: Style, text: &str, width: u16, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        fit_cell_text(&sanitize_flat_provider_text(text), width),
        style,
    ))
}

fn flat_card_spans_line(_accent: Style, spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from(sanitize_flat_provider_spans(spans))
}

fn sanitize_flat_provider_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let text = spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    if should_collapse_flat_provider_line(&text) {
        vec![Span::raw(sanitize_flat_provider_text(&text))]
    } else {
        spans
    }
}

fn sanitize_flat_provider_text(text: &str) -> String {
    text.replace('\u{00a0}', " ")
        .replace('\u{200b}', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn should_collapse_flat_provider_line(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let whitespace = trimmed
        .chars()
        .filter(|character| character.is_whitespace())
        .count();
    let non_whitespace = trimmed
        .chars()
        .filter(|character| !character.is_whitespace())
        .count();
    whitespace > non_whitespace.saturating_mul(2)
}

fn card_preview_line(accent: Style, preview: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let content_width = preview
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let mut spans = Vec::with_capacity(preview.len() + 3);
    spans.push(Span::styled("│ ", accent));
    spans.extend(preview);
    spans.push(Span::raw(
        " ".repeat((width as usize).saturating_sub(content_width)),
    ));
    spans.push(Span::styled(" │", accent));
    Line::from(spans)
}

fn card_text_line(accent: Style, text: &str, width: u16, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│ ", accent),
        Span::styled(fit_cell_text(text, width), style),
        Span::styled(" │", accent),
    ])
}

fn card_spans_line(accent: Style, spans: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let content_width = spans_width(&spans);
    let mut line_spans = Vec::with_capacity(spans.len() + 3);
    line_spans.push(Span::styled("│ ", accent));
    line_spans.extend(spans);
    line_spans.push(Span::raw(
        " ".repeat((width as usize).saturating_sub(content_width)),
    ));
    line_spans.push(Span::styled(" │", accent));
    Line::from(line_spans)
}

fn card_border_line(
    left: char,
    fill: char,
    right: char,
    width: u16,
    accent: Style,
) -> Line<'static> {
    let border = format!(
        "{left}{}{right}",
        std::iter::repeat_n(fill, width as usize + 2).collect::<String>()
    );
    Line::from(Span::styled(border, accent))
}

fn media_card_accent(accent: Style) -> Style {
    accent
}

fn media_card_width(content_width: u16) -> u16 {
    bubble_inner_width(content_width)
        .min(MEDIA_PREVIEW_MAX_WIDTH as usize)
        .max(1) as u16
}

fn media_card_width_for_media(
    media: &chat_core::Media,
    content_width: u16,
    _rows: u16,
    label: &str,
) -> u16 {
    let max_width = media_card_width(content_width);
    let file_line_width = UnicodeWidthStr::width(
        format!("file: {}{}", media.file_name, format_media_size(media)).as_str(),
    );
    let caption_line_width = visible_media_caption(media)
        .map(UnicodeWidthStr::width)
        .unwrap_or_default();
    let min_width = UnicodeWidthStr::width(label)
        .max(file_line_width)
        .max(caption_line_width)
        .max(1) as u16;

    // Keep message drawing non-blocking: do not inspect image files here.
    // Preview decoding/resizing runs through queued MediaPreviewRequest jobs;
    // synchronous dimension reads in this width helper can still stall the draw path.
    min_width.min(max_width).max(1)
}

fn link_preview_card_width(content_width: u16) -> u16 {
    bubble_inner_width(content_width)
        .min(LINK_PREVIEW_CARD_WIDTH as usize)
        .max(1) as u16
}

fn bubble_inner_width(content_width: u16) -> usize {
    let content_width = content_width as usize;
    let max_bubble_width = content_width
        .saturating_mul(BUBBLE_MAX_PERCENT as usize)
        .saturating_div(100)
        .max(BUBBLE_MIN_WIDTH)
        .min(content_width.saturating_sub(2).max(1));
    max_bubble_width.saturating_sub(4).max(1)
}

fn bubble_border_line(
    left: char,
    fill: char,
    right: char,
    inner_width: usize,
    accent: Style,
) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{left}{}{right}",
            std::iter::repeat_n(fill, inner_width + 2).collect::<String>()
        ),
        accent,
    ))
}

fn bubble_text_line(spans: Vec<Span<'static>>, inner_width: usize, accent: Style) -> Line<'static> {
    let content_width = spans_width(&spans);
    let mut line_spans = Vec::with_capacity(spans.len() + 3);
    line_spans.push(Span::styled("│ ", accent));
    line_spans.extend(fit_spans(spans, inner_width as u16));
    line_spans.push(Span::raw(
        " ".repeat(inner_width.saturating_sub(content_width.min(inner_width))),
    ));
    line_spans.push(Span::styled(" │", accent));
    Line::from(line_spans)
}

fn wrap_markdown_text(text: &str, width: usize) -> Vec<Vec<Span<'static>>> {
    wrap_styled_segments(&markdown_segments(text), width)
}

fn markdown_segments(text: &str) -> Vec<(String, Style)> {
    text.split('\n')
        .enumerate()
        .flat_map(|(index, paragraph)| {
            let mut segments = markdown_inline_segments(paragraph);
            if index > 0 {
                segments.insert(0, ("\n".to_owned(), Style::default()));
            }
            segments
        })
        .collect()
}

fn markdown_inline_segments(text: &str) -> Vec<(String, Style)> {
    let mut segments = Vec::new();
    let mut remaining = text;
    let mut style = Style::default();

    while !remaining.is_empty() {
        let Some((offset, marker)) = next_markdown_marker(remaining) else {
            if !remaining.is_empty() {
                push_mention_segments(&mut segments, remaining, style);
            }
            break;
        };

        if offset > 0 {
            push_mention_segments(&mut segments, &remaining[..offset], style);
            remaining = &remaining[offset..];
        }

        match marker {
            "```" => {
                style = toggle_modifier(style, Modifier::REVERSED);
                remaining = &remaining[3..];
            }
            "**" => {
                style = toggle_modifier(style, Modifier::BOLD);
                remaining = &remaining[2..];
            }
            "__" => {
                style = toggle_modifier(style, Modifier::BOLD);
                remaining = &remaining[2..];
            }
            "`" => {
                style = toggle_modifier(style, Modifier::REVERSED);
                remaining = &remaining[1..];
            }
            "*" => {
                style = toggle_modifier(style, Modifier::ITALIC);
                remaining = &remaining[1..];
            }
            "_" => {
                style = toggle_modifier(style, Modifier::ITALIC);
                remaining = &remaining[1..];
            }
            "~~" => {
                style = toggle_modifier(style, Modifier::CROSSED_OUT);
                remaining = &remaining[2..];
            }
            _ => unreachable!(),
        }
    }

    if segments.is_empty() {
        segments.push((String::new(), Style::default()));
    }
    segments
}

/// Push `text` into `segments`, splitting out `@token` mention runs and styling
/// them with an underline so mentions stand out in the transcript. A mention
/// starts with `@` at the beginning of the text or after whitespace, and runs to
/// the next whitespace; `user@host` is therefore left untouched.
fn push_mention_segments(segments: &mut Vec<(String, Style)>, text: &str, style: Style) {
    if !text.contains('@') {
        segments.push((text.to_owned(), style));
        return;
    }
    let mention_style = toggle_modifier(style, Modifier::UNDERLINED);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        let starts_mention = chars[index].1 == '@'
            && (index == 0 || chars[index - 1].1.is_whitespace())
            && chars
                .get(index + 1)
                .is_some_and(|(_, ch)| ch.is_alphanumeric() || *ch == '_');
        if starts_mention {
            let mut end = index + 1;
            while end < chars.len() && !chars[end].1.is_whitespace() {
                end += 1;
            }
            let token_start = chars[index].0;
            let token_end = chars.get(end).map_or(text.len(), |(byte, _)| *byte);
            if token_start > start {
                segments.push((text[start..token_start].to_owned(), style));
            }
            segments.push((text[token_start..token_end].to_owned(), mention_style));
            start = token_end;
            index = end;
            continue;
        }
        index += 1;
    }
    if start < text.len() {
        segments.push((text[start..].to_owned(), style));
    }
}

fn next_markdown_marker(text: &str) -> Option<(usize, &'static str)> {
    ["```", "**", "__", "~~", "`", "*", "_"]
        .into_iter()
        .filter_map(|marker| text.find(marker).map(|offset| (offset, marker)))
        .min_by_key(|(offset, _)| *offset)
}

fn toggle_modifier(mut style: Style, modifier: Modifier) -> Style {
    if style.add_modifier.contains(modifier) {
        style.add_modifier.remove(modifier);
    } else {
        style.add_modifier.insert(modifier);
    }
    style
}

fn wrap_styled_segments(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;

    for (text, style) in segments {
        if text == "\n" {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            continue;
        }
        for word in split_words_preserving_spaces(text) {
            let word_width = UnicodeWidthStr::width(word.as_str());
            if word.trim().is_empty() {
                if current_width > 0 && current_width.saturating_add(word_width) <= width {
                    current.push(Span::styled(word, *style));
                    current_width += word_width;
                }
                continue;
            }
            if current_width > 0 && current_width.saturating_add(word_width) > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if word_width <= width {
                current.push(Span::styled(word, *style));
                current_width += word_width;
            } else {
                push_broken_styled_word(
                    &word,
                    *style,
                    width,
                    &mut current,
                    &mut current_width,
                    &mut lines,
                );
            }
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn split_words_preserving_spaces(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_is_space = None;
    for character in text.chars() {
        let is_space = character.is_whitespace();
        if current_is_space.is_some_and(|space| space != is_space) {
            parts.push(std::mem::take(&mut current));
        }
        current.push(character);
        current_is_space = Some(is_space);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn push_broken_styled_word(
    word: &str,
    style: Style,
    width: usize,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    lines: &mut Vec<Vec<Span<'static>>>,
) {
    let mut chunk = String::new();
    let mut chunk_width = 0usize;
    for character in word.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if chunk_width.saturating_add(character_width) > width && !chunk.is_empty() {
            if !current.is_empty() {
                lines.push(std::mem::take(current));
                *current_width = 0;
            }
            lines.push(vec![Span::styled(std::mem::take(&mut chunk), style)]);
            chunk_width = 0;
        }
        chunk.push(character);
        chunk_width += character_width;
    }
    if !chunk.is_empty() {
        if *current_width > 0 && current_width.saturating_add(chunk_width) > width {
            lines.push(std::mem::take(current));
            *current_width = 0;
        }
        current.push(Span::styled(chunk, style));
        *current_width += chunk_width;
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn fit_spans(spans: Vec<Span<'static>>, width: u16) -> Vec<Span<'static>> {
    let mut fitted = Vec::new();
    let mut used = 0usize;
    let width = width as usize;
    for span in spans {
        let mut content = String::new();
        for character in span.content.chars() {
            let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
            if used.saturating_add(character_width) > width {
                break;
            }
            content.push(character);
            used += character_width;
        }
        if !content.is_empty() {
            fitted.push(Span::styled(content, span.style));
        }
        if used >= width {
            break;
        }
    }
    fitted
}

fn split_by_display_width(text: &str, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), text.to_owned());
    }

    let mut selected = String::new();
    let mut used = 0usize;
    for (offset, character) in text.char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > width {
            return (selected, text[offset..].to_owned());
        }
        selected.push(character);
        used += character_width;
        if used >= width {
            let rest_offset = offset + character.len_utf8();
            return (selected, text[rest_offset..].to_owned());
        }
    }

    (selected, String::new())
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for paragraph in text.split('\n') {
        wrap_paragraph(paragraph, width, &mut wrapped);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn wrap_paragraph(paragraph: &str, width: usize, wrapped: &mut Vec<String>) {
    if paragraph.is_empty() {
        wrapped.push(String::new());
        return;
    }

    let mut current = String::new();
    for word in paragraph.split_whitespace() {
        let word_len = word.chars().count();
        if current.is_empty() {
            if word_len <= width {
                current.push_str(word);
            } else {
                push_broken_word(word, width, wrapped);
            }
        } else if current.chars().count() + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            wrapped.push(std::mem::take(&mut current));
            if word_len <= width {
                current.push_str(word);
            } else {
                push_broken_word(word, width, wrapped);
            }
        }
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
}

fn push_broken_word(word: &str, width: usize, wrapped: &mut Vec<String>) {
    let mut current = String::new();
    for value in word.chars() {
        current.push(value);
        if current.chars().count() >= width {
            wrapped.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        wrapped.push(current);
    }
}

fn fit_cell_text(text: &str, width: u16) -> String {
    let width = width as usize;
    let mut value = String::new();
    let mut used: usize = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > width {
            break;
        }
        value.push(character);
        used += character_width;
    }
    value.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    value
}

fn halfblock_span(top: [u8; 4], bottom: [u8; 4]) -> Span<'static> {
    match (top[3] == 0, bottom[3] == 0) {
        (true, true) => Span::raw(" "),
        (true, false) => Span::styled(
            "▄",
            Style::default().fg(rgba_to_color(bottom)).bg(Color::Reset),
        ),
        (false, true) => Span::styled(
            "▀",
            Style::default().fg(rgba_to_color(top)).bg(Color::Reset),
        ),
        (false, false) => Span::styled(
            "▀",
            Style::default()
                .fg(rgba_to_color(top))
                .bg(rgba_to_color(bottom)),
        ),
    }
}

fn rgba_to_color([red, green, blue, alpha]: [u8; 4]) -> Color {
    match alpha {
        0 => Color::Reset,
        255 => Color::Rgb(red, green, blue),
        _ => {
            let alpha = u16::from(alpha);
            let blend = |channel: u8| {
                let channel = u16::from(channel);
                ((channel * alpha + 12 * (255 - alpha)) / 255) as u8
            };
            Color::Rgb(blend(red), blend(green), blend(blue))
        }
    }
}

fn format_media_size(media: &chat_core::Media) -> String {
    media.size_bytes.map(format_size).unwrap_or_default()
}

fn format_size(size: u64) -> String {
    if size >= 1_000_000 {
        format!(" ({:.1} MB)", size as f64 / 1_000_000.0)
    } else if size >= 1_000 {
        format!(" ({} KB)", size / 1_000)
    } else {
        format!(" ({size} B)")
    }
}

fn status_line(status: &str, style: Style) -> Line<'static> {
    Line::from(Span::styled(status.to_owned(), style))
}

fn reaction_pill_line(message: &Message, _theme: Theme) -> Line<'static> {
    let mut spans = Vec::new();
    if !message.is_from_me {
        spans.push(Span::raw("  "));
    }
    spans.extend(reaction_pill_spans(message));
    if message.is_from_me {
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

fn reaction_pill_spans(message: &Message) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, reaction) in message.reactions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::raw(format!(
            "{}{}",
            reaction_display_emoji(reaction.emoji.as_ref()),
            reaction.senders.len()
        )));
    }
    spans
}

pub fn reaction_display_emoji(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if emojis::get(trimmed).is_some() {
        return trimmed.to_owned();
    }

    let name = trimmed.trim_matches(':');
    if name.is_empty() {
        return trimmed.to_owned();
    }

    slack_emoji_lookup(name)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if trimmed.starts_with(':') && trimmed.ends_with(':') {
                trimmed.to_owned()
            } else {
                format!(":{name}:")
            }
        })
}

pub fn slack_emoji_shortcodes_to_display(text: &str) -> String {
    replace_colon_tokens(text, |name| slack_emoji_lookup(name).map(str::to_owned))
}

fn replace_colon_tokens<F>(text: &str, mut convert: F) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;

    while let Some(start) = remaining.find(':') {
        output.push_str(&remaining[..start]);
        let token_start = start + 1;
        let Some(end_offset) = remaining[token_start..].find(':') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let token_end = token_start + end_offset;
        let name = &remaining[token_start..token_end];
        if is_slack_emoji_name(name)
            && let Some(display) = convert(name)
        {
            output.push_str(&display);
        } else {
            output.push_str(&remaining[start..=token_end]);
        }
        remaining = &remaining[token_end + 1..];
    }

    output.push_str(remaining);
    output
}

fn is_slack_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+' | ':' | '\'')
        })
}

fn slack_emoji_lookup(name: &str) -> Option<&'static str> {
    slack_builtin_emoji_alias(name)
        .or_else(|| slack_skin_tone_emoji(name))
        .or_else(|| slack_composite_skin_tone_emoji(name))
        .or_else(|| emojis::get_by_shortcode(name).map(|emoji| emoji.as_str()))
        .or_else(|| slack_cldr_name_emoji(name))
}

fn slack_cldr_name_emoji(name: &str) -> Option<&'static str> {
    let normalized = name.replace('-', "_").to_ascii_lowercase();
    emojis::iter().find_map(|emoji| {
        let emoji_name = emoji.name().replace([' ', '-'], "_").to_ascii_lowercase();
        (emoji_name == normalized).then_some(emoji.as_str())
    })
}

fn slack_composite_skin_tone_emoji(name: &str) -> Option<&'static str> {
    let (base, tone) = name.split_once("::skin-tone-")?;
    let base = slack_emoji_lookup(base.trim_matches(':'))?;
    let tone = slack_skin_tone(tone.trim_matches(':'))?;
    emojis::get(base)?
        .with_skin_tone(tone)
        .map(|emoji| emoji.as_str())
}

fn slack_skin_tone_emoji(name: &str) -> Option<&'static str> {
    match name.trim_matches(':') {
        "skin-tone-2" => Some("🏻"),
        "skin-tone-3" => Some("🏼"),
        "skin-tone-4" => Some("🏽"),
        "skin-tone-5" => Some("🏾"),
        "skin-tone-6" => Some("🏿"),
        _ => None,
    }
}

fn slack_skin_tone(name: &str) -> Option<emojis::SkinTone> {
    match name {
        "2" | "skin-tone-2" => Some(emojis::SkinTone::Light),
        "3" | "skin-tone-3" => Some(emojis::SkinTone::MediumLight),
        "4" | "skin-tone-4" => Some(emojis::SkinTone::Medium),
        "5" | "skin-tone-5" => Some(emojis::SkinTone::MediumDark),
        "6" | "skin-tone-6" => Some(emojis::SkinTone::Dark),
        _ => None,
    }
}

fn slack_builtin_emoji_alias(name: &str) -> Option<&'static str> {
    match name {
        "+1" | "thumbsup" => Some("👍"),
        "-1" | "thumbsdown" => Some("👎"),
        "white_check_mark" => Some("✅"),
        "large_green_circle" => Some("🟢"),
        "large_yellow_circle" => Some("🟡"),
        "large_orange_circle" => Some("🟠"),
        "large_red_square" => Some("🟥"),
        "large_blue_square" => Some("🟦"),
        "large_green_square" => Some("🟩"),
        "large_yellow_square" => Some("🟨"),
        "large_orange_square" => Some("🟧"),
        "large_purple_square" => Some("🟪"),
        "large_brown_square" => Some("🟫"),
        "heavy_check_mark" => Some("✔️"),
        "heavy_multiplication_x" => Some("✖️"),
        "heavy_plus_sign" => Some("➕"),
        "heavy_minus_sign" => Some("➖"),
        "heavy_division_sign" => Some("➗"),
        _ => None,
    }
}

fn timeline_messages(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|message| !is_slack_thread_reply(message))
        .collect()
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

fn thread_summary_line(
    summary: &ThreadSummary,
    is_from_me: bool,
    unread_reply_count: usize,
    context: &mut MessageRenderContext<'_>,
) -> Line<'static> {
    let mut spans = Vec::new();
    if !is_from_me {
        spans.push(Span::raw("  "));
    }

    spans.extend(thread_summary_spans(summary, unread_reply_count, context));

    if is_from_me {
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

fn thread_summary_spans(
    summary: &ThreadSummary,
    unread_reply_count: usize,
    context: &mut MessageRenderContext<'_>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for participant in summary.participants.iter().take(5) {
        spans.extend(compact_reply_avatar_spans(participant, context));
    }

    spans.push(Span::styled(
        reply_count_text(summary.reply_count),
        context.theme.status_key().add_modifier(Modifier::BOLD),
    ));
    if unread_reply_count > 0 {
        spans.push(Span::styled(
            format!(" · {unread_reply_count} new"),
            context.theme.unread().add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        format!(
            " · Last reply {}",
            relative_reply_time(summary.last_reply_at)
        ),
        context.theme.muted(),
    ));
    spans
}

fn reply_count_text(count: usize) -> String {
    if count == 1 {
        "1 reply".to_owned()
    } else {
        format!("{count} replies")
    }
}

fn relative_reply_time(timestamp: Timestamp) -> String {
    let now = Local::now();
    let local = timestamp.with_timezone(&Local);
    let duration = now.signed_duration_since(local);
    if duration.num_days() >= 1 {
        format!("{}d ago", duration.num_days())
    } else if duration.num_hours() >= 1 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_minutes() >= 1 {
        format!("{}m ago", duration.num_minutes())
    } else {
        "just now".to_owned()
    }
}

fn thread_summaries(messages: &[Message]) -> HashMap<MessageId, ThreadSummary> {
    let message_ids = messages
        .iter()
        .map(|message| message.id.clone())
        .collect::<HashSet<_>>();
    let mut summaries: HashMap<MessageId, ThreadSummary> = HashMap::new();

    for reply in messages {
        let Some(root_id) = reply.reply_to.as_ref().or(reply.thread_id.as_ref()) else {
            continue;
        };
        if root_id.as_ref() == reply.id.as_ref() || !message_ids.contains(root_id) {
            continue;
        }

        let summary = summaries
            .entry(root_id.clone())
            .or_insert_with(|| ThreadSummary {
                reply_count: 0,
                participants: Vec::new(),
                last_reply_at: reply.timestamp,
            });
        summary.reply_count += 1;
        if summary
            .participants
            .iter()
            .all(|sender| sender.platform_id != reply.sender.platform_id)
        {
            summary.participants.push(reply.sender.clone());
        }
        if reply.timestamp > summary.last_reply_at {
            summary.last_reply_at = reply.timestamp;
        }
    }

    summaries
}

fn receipt_summary(message: &Message) -> String {
    let delivered = message
        .receipts
        .iter()
        .filter(|receipt| matches!(receipt.kind, ReceiptKind::Delivered))
        .count();
    let read = message
        .receipts
        .iter()
        .filter(|receipt| matches!(receipt.kind, ReceiptKind::Read))
        .count();

    match (delivered, read) {
        (0, 0) => String::new(),
        (_, read) if read > 0 => format!("✓✓ read by {read}"),
        (delivered, _) => format!("✓ delivered to {delivered}"),
    }
}

fn message_avatar_spans(
    sender: &Sender,
    context: &mut MessageRenderContext<'_>,
) -> Vec<Span<'static>> {
    sender
        .avatar
        .as_deref()
        .filter(|path| path.exists())
        .and_then(|path| {
            let key = MediaPreviewKey {
                path: path.to_path_buf(),
                width: MESSAGE_AVATAR_WIDTH,
                rows: MESSAGE_AVATAR_ROWS,
            };
            match context.media_cache.get(&key) {
                Some(Ok(rows)) => rows.iter().next().cloned(),
                Some(Err(_)) => None,
                None => {
                    context
                        .media_preview_requests
                        .push(MediaPreviewRequest { key });
                    None
                }
            }
        })
        .unwrap_or_else(|| vec![Span::styled(avatar_label(sender), compact_avatar_style())])
}

fn compact_avatar_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn avatar_label(sender: &Sender) -> String {
    let initials = sender
        .display_name
        .split_whitespace()
        .filter_map(|part| part.chars().find(|value| value.is_alphanumeric()))
        .take(2)
        .collect::<String>()
        .to_uppercase();

    if initials.is_empty() {
        "??".to_owned()
    } else {
        format!("{initials:<2}")
    }
}

fn compact_reply_avatar_spans(
    sender: &Sender,
    context: &mut MessageRenderContext<'_>,
) -> Vec<Span<'static>> {
    let mut spans = message_avatar_spans(sender, context);
    spans.push(Span::raw(" "));
    spans
}

/// Number of inner rows a reply quote box always occupies: the titled top
/// border, exactly one snippet line, and the bottom border. Fixed so the
/// line-count pass stays exact and independent of the (unbounded) snippet text.
const QUOTE_BOX_ROWS: usize = 3;
const QUOTE_MIN_BOX_WIDTH: usize = 8;

/// Stable per-participant colors for reply quote boxes, emulating WhatsApp's
/// distinct per-sender quote tint. Deterministic by sender so the same person
/// keeps the same color across the timeline.
const QUOTE_COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
];

fn quote_color(theme: Theme, seed: &str) -> Color {
    if seed.trim().is_empty() {
        return theme.accent;
    }
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    QUOTE_COLORS[(hasher.finish() as usize) % QUOTE_COLORS.len()]
}

fn reply_preview(message: &Message) -> ReplyPreview {
    let text = content_preview_text(&message.content);
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let snippet = if collapsed.is_empty() {
        Arc::<str>::from("attachment")
    } else {
        Arc::<str>::from(collapsed.as_str())
    };
    ReplyPreview {
        sender: message.sender.display_name.clone(),
        snippet,
    }
}

/// The text a reply's own body would render as a plain text bubble, when the
/// content is text-like (so the quote box can be nested inside the same outer
/// bubble). Returns `None` for media/cards/link content (rendered as their own
/// cards) and for text carrying a URL (which may expand into a link-preview
/// card), so those keep their existing rendering with a standalone quote box.
fn reply_inline_text(content: &Content) -> Option<String> {
    match content {
        Content::Text(text) if first_url_in_text(text).is_none() => Some(text.to_string()),
        Content::Poll(poll) => Some(poll_text(poll)),
        Content::Deleted => Some("[deleted]".to_owned()),
        Content::Unsupported(kind) => Some(format!("[unsupported: {kind}]")),
        _ => None,
    }
}

/// Truncates `text` to at most `max_width` display cells, appending `…` when it
/// had to drop characters.
fn truncate_display(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > budget {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
}

/// Builds the rows of a nested reply quote box: a titled top border carrying the
/// quoted sender's name, a single muted snippet line, and a bottom border. Each
/// returned row is exactly `box_width` display cells wide so it can be embedded
/// directly (bubbles via `bubble_text_line`, flat via `slack_indented_line`).
fn reply_quote_box_rows(
    preview: &ReplyPreview,
    box_width: usize,
    theme: Theme,
    color: Color,
) -> Vec<Vec<Span<'static>>> {
    let box_width = box_width.max(QUOTE_MIN_BOX_WIDTH);
    let border_style = Style::default().fg(color);
    let name_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let snippet_style = theme.muted();

    // Top border: "┌─ {name} {dashes}┐" padded to exactly `box_width`.
    let name_budget = box_width.saturating_sub(5).max(1);
    let name = truncate_display(&preview.sender, name_budget);
    let name_width = UnicodeWidthStr::width(name.as_str());
    let dashes = box_width.saturating_sub(5 + name_width).max(1);
    let top = vec![
        Span::styled("┌─ ".to_owned(), border_style),
        Span::styled(name, name_style),
        Span::styled(format!(" {}┐", "─".repeat(dashes)), border_style),
    ];

    // Single snippet line inside the box.
    let snippet_width = box_width.saturating_sub(4).max(1);
    let snippet = truncate_display(&preview.snippet, snippet_width);
    let snippet_pad = snippet_width.saturating_sub(UnicodeWidthStr::width(snippet.as_str()));
    let body = vec![
        Span::styled("│ ".to_owned(), border_style),
        Span::styled(snippet, snippet_style),
        Span::raw(" ".repeat(snippet_pad)),
        Span::styled(" │".to_owned(), border_style),
    ];

    let bottom = vec![Span::styled(
        format!("└{}┘", "─".repeat(box_width.saturating_sub(2))),
        border_style,
    )];

    vec![top, body, bottom]
}

/// Builds one bubble that nests the reply quote box above the reply's own text
/// body so both share a single outer border (the WhatsApp-style nested quote).
fn reply_text_bubble_lines(
    preview: &ReplyPreview,
    text: &str,
    content_width: u16,
    accent: Style,
    theme: Theme,
    color: Color,
) -> Vec<Line<'static>> {
    let inner_width = bubble_inner_width(content_width);
    let quote_rows = reply_quote_box_rows(preview, inner_width, theme, color);
    let wrapped = wrap_markdown_text(text, inner_width);

    let mut lines = Vec::with_capacity(quote_rows.len() + wrapped.len() + 2);
    lines.push(bubble_border_line('╭', '─', '╮', inner_width, accent));
    for row in quote_rows {
        lines.push(bubble_text_line(row, inner_width, accent));
    }
    for spans in wrapped {
        lines.push(bubble_text_line(spans, inner_width, accent));
    }
    lines.push(bubble_border_line('╰', '─', '╯', inner_width, accent));
    lines
}

/// Resolves the quote preview and per-sender color for a reply target, falling
/// back to a muted "not loaded" box when the quoted message is outside the
/// loaded window.
fn resolve_reply_preview(
    context: &MessageRenderContext<'_>,
    reply_to: &Arc<str>,
) -> (ReplyPreview, Color) {
    match context.reply_previews.get(reply_to) {
        Some(preview) => {
            let color = quote_color(context.theme, &preview.sender);
            (preview.clone(), color)
        }
        None => (
            ReplyPreview {
                sender: Arc::<str>::from("Reply"),
                snippet: Arc::<str>::from("[message not loaded]"),
            },
            context.theme.muted,
        ),
    }
}

fn content_preview_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.to_string(),
        // Video file names are usually provider cache hashes; never quote them.
        Content::Video(media) => visible_media_caption(media)
            .map(str::to_owned)
            .unwrap_or_else(|| video_display_name(media).to_owned()),
        Content::Image(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media
            .caption
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| media.file_name.to_string()),
        Content::LinkPreview(link) => {
            let title = link.title.as_deref().unwrap_or("Link");
            format!("{title}: {}", link.url)
        }
        Content::Cards(cards) => card_collection_text(cards),
        Content::Poll(poll) => format!("Poll: {}", poll.question),
        Content::Deleted => String::new(),
        Content::Unsupported(kind) => format!("Unsupported message: {kind}"),
    }
}

/// Extra timeline lines a reply quote box adds above/around the message body.
/// Fixed at [`QUOTE_BOX_ROWS`] whenever the message replies to another (a
/// titled top border, one snippet line, and a bottom border), and zero
/// otherwise. Both the nested-text bubble and the standalone box add exactly
/// these rows, so this single helper keeps the count pass aligned with the draw
/// pass in both presentations.
fn reply_quote_overhead_lines(message: &Message) -> usize {
    if message.reply_to.is_some() {
        QUOTE_BOX_ROWS
    } else {
        0
    }
}

fn message_lines_len(
    message: &Message,
    grouped: bool,
    has_previous_message: bool,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    thread_summary: Option<&ThreadSummary>,
    presentation: ConversationPresentation,
) -> usize {
    if presentation == ConversationPresentation::Flat {
        return slack_message_lines_len(
            message,
            grouped,
            has_previous_message,
            content_width,
            link_metadata,
            thread_summary,
        );
    }

    usize::from(!grouped)
        + reply_quote_overhead_lines(message)
        + content_lines_len(&message.content, content_width, link_metadata, presentation)
        + usize::from(
            message.is_from_me
                || (grouped && message.edited_at.is_some())
                || !receipt_summary(message).is_empty(),
        )
        + usize::from(!message.reactions.is_empty())
        + usize::from(thread_summary.is_some())
        + 1
}

fn slack_message_lines_len(
    message: &Message,
    grouped: bool,
    has_previous_message: bool,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    thread_summary: Option<&ThreadSummary>,
) -> usize {
    let body_width = slack_body_width(content_width);
    let content_count = content_lines_len(
        &message.content,
        body_width as u16,
        link_metadata,
        ConversationPresentation::Flat,
    )
    .max(1);
    let merged_content_line = usize::from(
        !grouped
            && message.reply_to.is_none()
            && slack_content_can_merge(&message.content)
            && content_count > 0,
    );

    slack_message_spacer_lines(grouped, has_previous_message)
        + usize::from(!grouped)
        + reply_quote_overhead_lines(message)
        + content_count.saturating_sub(merged_content_line)
        + usize::from(
            slack_edited_marker_placement(message, grouped, content_width, link_metadata)
                == FlatEditedMarker::OwnRow,
        )
        + usize::from(!message.reactions.is_empty())
        + usize::from(thread_summary.is_some())
}

fn content_lines_len(
    content: &Content,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    presentation: ConversationPresentation,
) -> usize {
    match content {
        Content::Text(text) => {
            text_with_link_preview_line_count(text, content_width, link_metadata, presentation)
        }
        Content::Deleted => text_content_line_count("[deleted]", content_width, presentation),
        Content::Poll(poll) => {
            text_content_line_count(&poll_text(poll), content_width, presentation)
        }
        Content::Image(media) | Content::Sticker(media) => {
            media_card_line_count(media, content_width, presentation, true)
        }
        Content::Video(media) => video_card_line_count(media, content_width, presentation),
        Content::Audio(media) if uses_document_card(media, presentation) => {
            document_card_line_count(true, media, content_width)
        }
        Content::File(media) if uses_document_card(media, presentation) => {
            document_card_line_count(false, media, content_width)
        }
        Content::Audio(media) | Content::File(media) => {
            media_card_line_count(media, content_width, presentation, false)
        }
        Content::LinkPreview(link) => link_preview_card_line_count(link, presentation),
        Content::Cards(cards) => card_collection_line_count(cards, content_width, presentation),
        Content::Unsupported(kind) => text_content_line_count(
            &format!("[unsupported: {kind}]"),
            content_width,
            presentation,
        ),
    }
}

fn text_content_line_count(
    text: &str,
    content_width: u16,
    presentation: ConversationPresentation,
) -> usize {
    match presentation {
        ConversationPresentation::Bubbles => text_bubble_line_count(text, content_width),
        ConversationPresentation::Flat => wrap_text(text, flat_text_width(content_width)).len(),
    }
}

fn text_bubble_line_count(text: &str, content_width: u16) -> usize {
    wrap_text(text, bubble_inner_width(content_width)).len() + 2
}

fn text_with_link_preview_line_count(
    text: &str,
    content_width: u16,
    link_metadata: &LinkMetadataCache,
    presentation: ConversationPresentation,
) -> usize {
    let Some(detected_url) = first_url_in_text(text) else {
        return text_content_line_count(text, content_width, presentation);
    };
    let Some(metadata) = link_metadata.get(detected_url.url) else {
        return text_content_line_count(text, content_width, presentation);
    };
    if !link_metadata_is_useful(metadata) {
        return text_content_line_count(text, content_width, presentation);
    }

    let text_without_url = remove_first_url_from_text(text);
    let text_lines = if text_without_url.is_empty() {
        0
    } else {
        text_content_line_count(&text_without_url, content_width, presentation)
    };

    if let Some(image) = &metadata.image {
        text_lines + media_card_line_count(image, content_width, presentation, false)
    } else {
        text_lines
            + link_preview_card_line_count_for_image(
                false,
                clean_link_preview_text(metadata.description.as_deref()).is_some(),
                presentation,
            )
    }
}

fn media_card_line_count(
    media: &chat_core::Media,
    content_width: u16,
    presentation: ConversationPresentation,
    visual: bool,
) -> usize {
    let caption = media_caption_line_count(media, content_width, presentation, visual);
    match presentation {
        ConversationPresentation::Bubbles => {
            // Two border rows plus, for non-visual media, a label and filename row.
            let chrome = if visual { 2 } else { 4 };
            chrome + MEDIA_PREVIEW_ROWS as usize + caption
        }
        ConversationPresentation::Flat => 1 + MEDIA_PREVIEW_ROWS as usize + 1 + caption,
    }
}

fn media_caption_line_count(
    media: &chat_core::Media,
    content_width: u16,
    presentation: ConversationPresentation,
    visual: bool,
) -> usize {
    let Some(caption) = visible_media_caption(media) else {
        return 0;
    };
    let card_width = match presentation {
        ConversationPresentation::Bubbles if visual => media_card_width(content_width),
        ConversationPresentation::Bubbles | ConversationPresentation::Flat => {
            media_card_width_for_media(media, content_width, MEDIA_PREVIEW_ROWS, "")
        }
    };
    wrap_markdown_text(caption, card_width as usize).len()
}

fn card_collection_line_count(
    cards: &[chat_core::Card],
    content_width: u16,
    presentation: ConversationPresentation,
) -> usize {
    let mut total = 0;
    let mut index = 0;
    while index < cards.len() {
        let run = inline_image_run_len(&cards[index..]);
        if run >= 2 && presentation == ConversationPresentation::Flat {
            total += inline_image_grid_line_count(&cards[index..index + run], content_width);
            index += run;
        } else {
            total += generic_card_line_count(&cards[index], content_width, presentation);
            index += 1;
        }
    }
    total
}

/// Mirror of [`inline_image_grid_lines`]: grid rows of fixed-height thumbnails
/// plus the wrapped captions rendered below them.
fn inline_image_grid_line_count(cards: &[chat_core::Card], content_width: u16) -> usize {
    let columns = inline_image_columns_per_row(content_width)
        .min(cards.len())
        .max(1);
    let grid_rows = cards.len().div_ceil(columns);
    let image_lines = grid_rows * LINK_PREVIEW_THUMBNAIL_ROWS as usize;
    let caption_width = flat_text_width(content_width).max(1);
    let caption_lines: usize = cards
        .iter()
        .filter_map(|card| card.body.as_deref())
        .map(|body| wrap_text(&sanitize_flat_provider_text(body), caption_width).len())
        .sum();
    image_lines + caption_lines
}

fn generic_card_line_count(
    card: &chat_core::Card,
    content_width: u16,
    presentation: ConversationPresentation,
) -> usize {
    let card_width = match presentation {
        ConversationPresentation::Bubbles => link_preview_card_width(content_width) as usize,
        ConversationPresentation::Flat => flat_text_width(content_width),
    };
    // Image attachment cards hide their filename (`title`) and `url` in the
    // transcript, so exclude those rows here to keep counts in sync.
    let hide_filename_and_url = card_is_media_preview(card);
    let counts_title = card.title.is_some() && !hide_filename_and_url;
    let counts_url = card.url.is_some() && !hide_filename_and_url;
    let body_lines = card
        .body
        .as_deref()
        .map(|body| wrap_text(&sanitize_flat_provider_text(body), card_width).len())
        .unwrap_or(0);
    let content_lines = usize::from(card.image.is_some() || card.thumbnail.is_some())
        * LINK_PREVIEW_THUMBNAIL_ROWS as usize
        + usize::from(card.subtitle.is_some())
        + usize::from(counts_title)
        + body_lines
        + card.fields.len()
        + usize::from(!card.actions.is_empty())
        + usize::from(card.footer.is_some())
        + usize::from(counts_url)
        + usize::from(
            !counts_title
                && card.subtitle.is_none()
                && card.body.is_none()
                && card.footer.is_none()
                && !counts_url
                && card.fields.is_empty()
                && card.actions.is_empty()
                && card.image.is_none()
                && card.thumbnail.is_none(),
        );

    match presentation {
        ConversationPresentation::Bubbles => content_lines + 2,
        ConversationPresentation::Flat => content_lines,
    }
}

fn link_preview_card_line_count(
    link: &chat_core::LinkPreview,
    presentation: ConversationPresentation,
) -> usize {
    link_preview_card_line_count_for_image(
        link.image.is_some(),
        clean_link_preview_text(link.description.as_deref()).is_some(),
        presentation,
    )
}

fn link_preview_card_line_count_for_image(
    has_image: bool,
    has_description: bool,
    presentation: ConversationPresentation,
) -> usize {
    match presentation {
        ConversationPresentation::Bubbles => {
            4 + usize::from(has_description)
                + usize::from(has_image) * LINK_PREVIEW_THUMBNAIL_ROWS as usize
        }
        ConversationPresentation::Flat => {
            2 + usize::from(has_description)
                + usize::from(has_image) * LINK_PREVIEW_THUMBNAIL_ROWS as usize
        }
    }
}

fn flat_text_width(content_width: u16) -> usize {
    (content_width as usize).max(1)
}

fn slack_body_width(content_width: u16) -> usize {
    (content_width as usize)
        .saturating_sub(SLACK_BODY_INDENT_WIDTH)
        .max(1)
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    area: Rect,
    content_len: usize,
    position: usize,
    theme: Theme,
) {
    let viewport = inner_area(area).height as usize;
    if area.width < 3 || area.height < 3 || content_len <= viewport.max(1) {
        return;
    }

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(theme.muted))
        .track_style(Style::default().fg(Color::Black))
        .begin_symbol(None)
        .end_symbol(None);
    let mut state = ScrollbarState::new(content_len)
        .position(position)
        .viewport_content_length(viewport);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

fn inner_area(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{Card, CardColor, CardField, CardKind, CardSource, PlatformData};
    use chrono::{TimeZone, Utc};
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn local_timestamp(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
    ) -> chat_core::Timestamp {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid local timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn message_timestamps_gain_dates_as_they_age() {
        // 2026-06-11 is a Thursday.
        let now = Local
            .with_ymd_and_hms(2026, 6, 11, 12, 0, 0)
            .single()
            .expect("valid now");

        assert_eq!(
            format_message_time_at(local_timestamp(2026, 6, 11, 9, 5), now),
            "09:05"
        );
        assert_eq!(
            format_message_time_at(local_timestamp(2026, 6, 10, 22, 48), now),
            "22:48"
        );
        assert_eq!(
            format_message_time_at(local_timestamp(2026, 6, 7, 8, 30), now),
            "07 Jun 08:30"
        );
        assert_eq!(
            format_message_time_at(local_timestamp(2025, 12, 31, 23, 59), now),
            "31 Dec 2025 23:59"
        );
    }

    #[test]
    fn compact_timestamps_fit_narrow_columns() {
        // 2026-06-11 is a Thursday, so 2026-06-08 is a Monday.
        let now = Local
            .with_ymd_and_hms(2026, 6, 11, 12, 0, 0)
            .single()
            .expect("valid now");
        let cases = [
            (local_timestamp(2026, 6, 11, 9, 5), "09:05"),
            (local_timestamp(2026, 6, 10, 22, 48), "22:48"),
            (local_timestamp(2026, 6, 8, 8, 0), "Mon"),
            (local_timestamp(2026, 6, 1, 8, 0), "01/06"),
            (local_timestamp(2025, 6, 1, 8, 0), "2025"),
        ];

        for (timestamp, expected) in cases {
            let stamp = format_timestamp_compact_at(timestamp, now);
            assert_eq!(stamp, expected);
            assert!(
                UnicodeWidthStr::width(stamp.as_str()) <= SLACK_TIMESTAMP_WIDTH,
                "compact stamp {stamp:?} exceeds the fixed gutter width"
            );
        }
    }

    #[test]
    fn halfblock_span_preserves_transparent_halves() {
        assert_eq!(halfblock_span([1, 2, 3, 0], [4, 5, 6, 0]).content, " ");
        assert_eq!(halfblock_span([1, 2, 3, 255], [4, 5, 6, 0]).content, "▀");
        assert_eq!(halfblock_span([1, 2, 3, 0], [4, 5, 6, 255]).content, "▄");
        assert_eq!(halfblock_span([1, 2, 3, 255], [4, 5, 6, 255]).content, "▀");
    }

    #[test]
    fn rounded_thumbnail_mask_clears_corners_and_keeps_center_opaque() {
        let mut image = image::RgbaImage::from_pixel(32, 32, image::Rgba([10, 20, 30, 255]));

        apply_rounded_thumbnail_mask(&mut image);

        assert_eq!(image.get_pixel(0, 0).0[3], 0);
        assert_eq!(image.get_pixel(31, 0).0[3], 0);
        assert_eq!(image.get_pixel(0, 31).0[3], 0);
        assert_eq!(image.get_pixel(31, 31).0[3], 0);
        assert_eq!(image.get_pixel(16, 16).0[3], 255);
        assert!(image.get_pixel(7, 0).0[3] > 0);
        assert!(image.get_pixel(7, 0).0[3] < 255);
    }

    #[test]
    fn first_url_in_text_accepts_slack_angle_links() {
        let detected = first_url_in_text("<https://example.com/path?x=1>")
            .expect("Slack-wrapped URL should be detected");

        assert_eq!(detected.url, "https://example.com/path?x=1");
        assert_eq!(detected.token, "<https://example.com/path?x=1>");
    }

    #[test]
    fn first_url_in_text_accepts_slack_labelled_links() {
        let detected = first_url_in_text("read <https://example.com/release|release notes>")
            .expect("Slack labelled URL should be detected");

        assert_eq!(detected.url, "https://example.com/release");
        assert_eq!(
            detected.token,
            "<https://example.com/release|release notes>"
        );
    }

    #[test]
    fn remove_first_url_from_text_removes_slack_wrapped_token() {
        assert_eq!(
            remove_first_url_from_text("cornel shared <https://example.com/image.png>"),
            "cornel shared"
        );
    }

    #[test]
    fn grouped_outgoing_messages_render_a_timestamp_per_bubble() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:alice");
        let sender = Sender {
            platform_id: Arc::<str>::from("me"),
            display_name: Arc::<str>::from("Me"),
            avatar: None,
        };
        let messages = vec![
            text_message(
                "msg-1",
                &chat_id,
                &account,
                sender.clone(),
                "First outgoing",
                4,
                11,
                true,
            ),
            text_message(
                "msg-2",
                &chat_id,
                &account,
                sender,
                "Grouped outgoing",
                4,
                12,
                true,
            ),
        ];
        let mut cache = MediaPreviewCache::default();

        let render = build_message_lines(
            &messages,
            80,
            0,
            200,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered = render
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let expected_first_time = format_message_time(messages[0].timestamp);
        let expected_second_time = format_message_time(messages[1].timestamp);
        assert!(rendered.contains("First outgoing"));
        assert!(rendered.contains("Grouped outgoing"));
        assert!(rendered.contains(&expected_first_time));
        assert!(rendered.contains(&expected_second_time));
        assert_eq!(rendered.matches("Me (me)").count(), 1);
    }

    #[test]
    fn selected_marker_aligns_with_message_side() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:alice");
        let incoming_sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let outgoing_sender = Sender {
            platform_id: Arc::<str>::from("me"),
            display_name: Arc::<str>::from("Me"),
            avatar: None,
        };
        let incoming = text_message(
            "incoming",
            &chat_id,
            &account,
            incoming_sender,
            "Incoming selected",
            4,
            10,
            false,
        );
        let outgoing = text_message(
            "outgoing",
            &chat_id,
            &account,
            outgoing_sender,
            "Outgoing selected",
            4,
            11,
            true,
        );

        let mut cache = MediaPreviewCache::default();
        let incoming_render = build_message_lines(
            &[incoming],
            80,
            0,
            40,
            Some("incoming"),
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let incoming_selected =
            rendered_line_containing(&incoming_render.lines, "Incoming selected");
        assert!(incoming_selected.starts_with("▏ "));
        assert!(!incoming_selected.ends_with(" ▕"));

        let mut cache = MediaPreviewCache::default();
        let outgoing_render = build_message_lines(
            &[outgoing],
            80,
            0,
            40,
            Some("outgoing"),
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let outgoing_selected =
            rendered_line_containing(&outgoing_render.lines, "Outgoing selected");
        assert!(outgoing_selected.ends_with(" ▕"));
        assert!(!outgoing_selected.starts_with("▏ "));
    }

    #[test]
    fn flat_selection_uses_timestamp_column_block_without_shifting_content() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:general");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let message = text_message(
            "selected-flat",
            &chat_id,
            &account,
            sender,
            "Selected flat message",
            9,
            30,
            false,
        );

        let mut cache = MediaPreviewCache::default();
        let unselected_render = build_message_lines_with_presentation(
            std::slice::from_ref(&message),
            80,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            ConversationPresentation::Flat,
        );
        let mut cache = MediaPreviewCache::default();
        let selected_render = build_message_lines_with_presentation(
            &[message],
            80,
            0,
            40,
            Some("selected-flat"),
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            ConversationPresentation::Flat,
        );
        let unselected =
            rendered_line_containing(&unselected_render.lines, "Selected flat message");
        let selected = rendered_line_containing(&selected_render.lines, "Selected flat message");

        assert_eq!(selected, unselected);
        assert!(!selected.starts_with("▏ "));
        assert!(!selected.ends_with(" ▕"));
        assert_eq!(selected.find("Alice"), unselected.find("Alice"));
    }

    #[test]
    fn reaction_pill_preserves_emoji_presentation_and_adds_margin() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:alice");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let mut message = text_message(
            "reaction",
            &chat_id,
            &account,
            sender,
            "Reacted message",
            4,
            10,
            false,
        );
        message.reactions = vec![chat_core::Reaction {
            emoji: Arc::<str>::from("❤️"),
            senders: vec![
                Arc::<str>::from("me"),
                Arc::<str>::from("mom"),
                Arc::<str>::from("leo"),
            ],
        }];

        let line = reaction_pill_line(&message, Theme::default());
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(rendered.starts_with("  "));
        assert!(rendered.contains("❤️3"));
        assert!(!rendered.contains("❤️ 3"));
        assert_eq!(rendered.matches('3').count(), 1);
        assert!(!line.spans.iter().any(|span| span.content.as_ref() == "3"));
    }

    #[test]
    fn reaction_pill_converts_stored_slack_names_to_icons() {
        assert_eq!(reaction_display_emoji("eyes"), "👀");
        assert_eq!(reaction_display_emoji("money_with_wings"), "💸");
        assert_eq!(reaction_display_emoji("white_check_mark"), "✅");
        assert_eq!(reaction_display_emoji("large_green_circle"), "🟢");
        assert_eq!(
            reaction_display_emoji("rolling_on_the_floor_laughing"),
            "🤣"
        );
        assert_eq!(reaction_display_emoji("+1::skin-tone-2"), "👍🏻");
        assert_eq!(reaction_display_emoji("party-parrot"), ":party-parrot:");
        assert_eq!(reaction_display_emoji(":party-parrot:"), ":party-parrot:");
    }

    #[test]
    fn slack_emoji_shortcodes_convert_in_stored_previews() {
        assert_eq!(
            slack_emoji_shortcodes_to_display("Done :white_check_mark: :large_green_circle:"),
            "Done ✅ 🟢"
        );
        assert_eq!(
            slack_emoji_shortcodes_to_display("Custom :party-parrot:"),
            "Custom :party-parrot:"
        );
    }

    #[test]
    fn markdown_styles_text_without_showing_markers() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:markdown");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let message = text_message(
            "markdown-message",
            &chat_id,
            &account,
            sender,
            "Hello **bold** _italic_ `code` ~~gone~~",
            10,
            0,
            false,
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            100,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(rendered_lines.iter().any(|line| line.contains("bold")));
        assert!(rendered_lines.iter().all(|line| !line.contains("**bold**")));
        assert!(render.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.as_ref().contains("bold")
                    && span.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(render.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.as_ref().contains("italic")
                    && span.style.add_modifier.contains(Modifier::ITALIC)
            })
        }));
        assert!(render.lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.as_ref().contains("code")
                    && span.style.add_modifier.contains(Modifier::REVERSED)
            })
        }));
    }

    #[test]
    fn media_messages_render_reactions_attached_to_the_card() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:media");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let mut message = text_message(
            "image-with-reaction",
            &chat_id,
            &account,
            sender,
            "",
            10,
            0,
            false,
        );
        message.content = Content::Image(chat_core::Media {
            id: Arc::<str>::from("media-1"),
            file_name: Arc::<str>::from("photo.jpg"),
            mime_type: Arc::<str>::from("image/jpeg"),
            size_bytes: Some(2048),
            caption: Some(Arc::<str>::from("last image")),
            local_path: Some(PathBuf::from("/tmp/missing-photo.jpg")),
            thumbnail: None,
        });
        message.reactions = vec![chat_core::Reaction {
            emoji: Arc::<str>::from("🔥"),
            senders: vec![Arc::<str>::from("bob")],
        }];

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            80,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = render
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        let media_line = rendered_lines
            .iter()
            .position(|line| line.contains("last image"))
            .expect("media card caption line");
        let reaction_line = rendered_lines
            .iter()
            .position(|line| line.contains("🔥1"))
            .expect("reaction line");
        assert!(reaction_line > media_line);
        assert_eq!(
            rendered_lines
                .iter()
                .filter(|line| line.contains("🔥1"))
                .count(),
            1
        );
    }

    #[test]
    fn text_message_with_url_waits_for_useful_metadata_before_preview_card() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let message = text_message(
            "plain-url",
            &chat_id,
            &account,
            sender,
            "Check this out https://www.example.com/story?id=42.",
            10,
            0,
            false,
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);
        let card_lines = rendered_lines
            .iter()
            .filter(|line| line.starts_with('╭') || line.starts_with('│') || line.starts_with('╰'))
            .collect::<Vec<_>>();

        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Check this out"))
        );
        assert!(!rendered_lines.iter().any(|line| line.contains("Link")));
        assert_eq!(render.link_preview_requests.len(), 1);
        assert_eq!(
            render.link_preview_requests[0].url.as_ref(),
            "https://www.example.com/story?id=42"
        );
        assert!(card_lines.len() < 7);
        assert_eq!(render.total_lines, 5);
    }

    #[test]
    fn text_message_with_url_uses_cached_metadata_for_preview_card() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let message = text_message(
            "plain-url-metadata",
            &chat_id,
            &account,
            sender,
            "Read https://example.com/story today",
            10,
            0,
            false,
        );
        let mut metadata = LinkMetadataCache::default();
        metadata.insert(
            Arc::<str>::from("https://example.com/story"),
            LinkMetadata {
                title: Some(Arc::<str>::from("&#xce;n drume&#x21b;ie")),
                description: Some(Arc::<str>::from(
                    "&lt;p&gt;Cele 7 Legi Universale &amp; mai mult&lt;/p&gt;",
                )),
                image_url: Some(Arc::<str>::from("https://example.com/preview.jpg")),
                image: None,
            },
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("În drumeție"))
        );
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Cele 7 Legi Universale & mai mult"))
        );
        assert!(!rendered_lines.iter().any(|line| line.contains("&#xce;")
            || line.contains("&#x21b;")
            || line.contains("<p>")));
        assert!(
            !rendered_lines
                .iter()
                .any(|line| line.contains("https://example.com/story"))
        );
        assert!(render.link_preview_requests.is_empty());
    }

    #[test]
    fn direct_image_url_replaces_url_with_image_card_when_ready() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let message = text_message(
            "plain-image-url",
            &chat_id,
            &account,
            sender,
            "https://cdn.example.com/photo.jpg",
            10,
            0,
            false,
        );
        let image_dir = tempfile::tempdir().expect("image tempdir");
        let image_path = image_dir.path().join("photo.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
            .save(&image_path)
            .expect("write test image");
        let mut metadata = LinkMetadataCache::default();
        metadata.insert(
            Arc::<str>::from("https://cdn.example.com/photo.jpg"),
            LinkMetadata {
                title: None,
                description: None,
                image_url: Some(Arc::<str>::from("https://cdn.example.com/photo.jpg")),
                image: Some(chat_core::Media {
                    id: Arc::<str>::from("link:https://cdn.example.com/photo.jpg"),
                    file_name: Arc::<str>::from("photo.jpg"),
                    mime_type: Arc::<str>::from("image/jpeg"),
                    size_bytes: Some(1024),
                    caption: None,
                    local_path: Some(image_path),
                    thumbnail: None,
                }),
            },
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            std::slice::from_ref(&message),
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let request = render
            .media_preview_requests
            .first()
            .expect("image preview should be queued")
            .clone();
        let decoded = decode_image_preview_rows_for_key(&request.key);
        cache.insert(request.key, decoded);
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(render.link_preview_requests.is_empty());
        assert!(rendered_lines.iter().any(|line| line.contains("Photo")));
        assert!(rendered_lines.iter().any(|line| line.contains("photo.jpg")));
        assert!(
            !rendered_lines
                .iter()
                .any(|line| line.contains("https://cdn.example.com/photo.jpg"))
        );
        assert_eq!(render.total_lines, 14);
    }

    #[test]
    fn unusable_long_url_message_stays_visible_after_metadata_fetch_fails() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let url = "https://cdn.example.com/this/path/is/far/too/long/to/be/a/useful/fallback/link/when/preview/metadata/fails.jpg";
        let message = text_message(
            "broken-long-url",
            &chat_id,
            &account,
            sender,
            &format!("Broken {url}"),
            10,
            0,
            false,
        );
        let mut metadata = LinkMetadataCache::default();
        metadata.insert(Arc::<str>::from(url), LinkMetadata::default());

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            60,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(rendered_lines.iter().any(|line| line.contains("Broken")));
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("cdn.example.com"))
        );
        assert!(render.link_preview_requests.is_empty());
    }

    #[test]
    fn unusable_single_row_url_message_stays_visible_after_metadata_fetch_fails() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let url = "https://x.co/a";
        let message = text_message(
            "broken-short-url",
            &chat_id,
            &account,
            sender,
            url,
            10,
            0,
            false,
        );
        let mut metadata = LinkMetadataCache::default();
        metadata.insert(Arc::<str>::from(url), LinkMetadata::default());

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(rendered_lines.iter().any(|line| line.contains(url)));
        assert!(render.link_preview_requests.is_empty());
    }

    #[test]
    fn undecodable_image_metadata_falls_back_to_original_message() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let url = "https://cdn.example.com/product/fono-hellberg-secure.jpg";
        let message = text_message(
            "bad-image-url",
            &chat_id,
            &account,
            sender,
            url,
            10,
            0,
            false,
        );
        let image_dir = tempfile::tempdir().expect("image tempdir");
        let image_path = image_dir.path().join("bad.jpg");
        std::fs::write(&image_path, b"not an image").expect("write bad image cache");
        let mut metadata = LinkMetadataCache::default();
        metadata.insert(
            Arc::<str>::from(url),
            LinkMetadata {
                title: None,
                description: None,
                image_url: Some(Arc::<str>::from(url)),
                image: Some(chat_core::Media {
                    id: Arc::<str>::from(
                        "link:https://cdn.example.com/product/fono-hellberg-secure.jpg",
                    ),
                    file_name: Arc::<str>::from("bad.jpg"),
                    mime_type: Arc::<str>::from("image/jpeg"),
                    size_bytes: Some(12),
                    caption: None,
                    local_path: Some(image_path),
                    thumbnail: None,
                }),
            },
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(rendered_lines.iter().any(|line| line.contains(url)));
        assert!(!rendered_lines.iter().any(|line| line.contains("Photo")));
        assert!(
            !rendered_lines
                .iter()
                .any(|line| line.contains("image decode failed"))
        );
        assert!(render.link_preview_requests.is_empty());
    }

    #[test]
    fn link_preview_renders_as_fixed_size_card() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let mut message =
            text_message("link-preview", &chat_id, &account, sender, "", 10, 0, false);
        message.content = Content::LinkPreview(chat_core::LinkPreview {
            url: Arc::<str>::from(
                "https://www.example.com/articles/a-very-long-path-that-should-not-expand-the-card",
            ),
            title: Some(Arc::<str>::from(
                "A very long article title that should be truncated instead of growing the preview card",
            )),
            description: Some(Arc::<str>::from(
                "A very long description that should stay on one fixed-width line so the preview never takes over the message pane.",
            )),
            image: None,
        });

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);
        let card_lines = rendered_lines
            .iter()
            .filter(|line| line.starts_with('╭') || line.starts_with('│') || line.starts_with('╰'))
            .collect::<Vec<_>>();

        assert_eq!(card_lines.len(), 5);
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("example.com"))
        );
        assert!(
            !rendered_lines
                .iter()
                .any(|line| line.contains("LINK PREVIEW"))
        );
        assert!(card_lines.iter().all(|line| line.chars().count() == 46));
        assert_eq!(
            render.total_lines,
            message_line_count(&[], 120, &LinkMetadataCache::default()) + 7
        );
    }

    #[test]
    fn link_preview_with_image_uses_compact_thumbnail_height() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:links");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let mut message = text_message(
            "link-preview-image",
            &chat_id,
            &account,
            sender,
            "",
            10,
            0,
            false,
        );
        message.content = Content::LinkPreview(chat_core::LinkPreview {
            url: Arc::<str>::from("https://example.com/story"),
            title: Some(Arc::<str>::from("Story")),
            description: Some(Arc::<str>::from("Short description")),
            image: Some(chat_core::Media {
                id: Arc::<str>::from("link-image"),
                file_name: Arc::<str>::from("preview.jpg"),
                mime_type: Arc::<str>::from("image/jpeg"),
                size_bytes: Some(1024),
                caption: None,
                local_path: Some(PathBuf::from("/tmp/missing-link-preview.jpg")),
                thumbnail: None,
            }),
        });

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);
        let preview_unavailable_rows = rendered_lines
            .iter()
            .filter(|line| line.contains("no local image"))
            .count();

        assert_eq!(preview_unavailable_rows, 1);
        assert_eq!(
            rendered_lines
                .iter()
                .filter(|line| line.starts_with('│'))
                .count(),
            7
        );
        assert_eq!(render.total_lines, 11);
    }

    #[test]
    fn message_list_clears_stale_cells_when_redrawing_shorter_content() {
        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let area = Rect::new(0, 0, 48, 8);
        let theme = Theme::default();

        terminal
            .draw(|frame| {
                render_message_list(
                    frame,
                    area,
                    MessageListProps {
                        title: "Messages",
                        lines: vec![Line::from("caption: Picnic photos are up 📸")],
                        total_lines: 1,
                        scroll: 0,
                        focused: true,
                        theme,
                    },
                );
            })
            .expect("first draw");
        assert!(buffer_text(terminal.backend()).contains("caption: Picnic photos"));

        terminal
            .draw(|frame| {
                render_message_list(
                    frame,
                    area,
                    MessageListProps {
                        title: "Messages",
                        lines: vec![reaction_pill_test_line("❤️", 3, theme)],
                        total_lines: 1,
                        scroll: 0,
                        focused: true,
                        theme,
                    },
                );
            })
            .expect("second draw");

        let rendered = buffer_text(terminal.backend());
        assert_eq!(rendered.matches('3').count(), 1);
        assert!(rendered.contains('❤'));
        assert!(!rendered.contains("caption:"));
        assert!(!rendered.contains("Picnic photos"));
    }

    #[test]
    fn threaded_root_message_renders_slack_style_reply_summary() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:bazaar");
        let dani = Sender {
            platform_id: Arc::<str>::from("U_dani"),
            display_name: Arc::<str>::from("dani"),
            avatar: None,
        };
        let alex = Sender {
            platform_id: Arc::<str>::from("U_alex"),
            display_name: Arc::<str>::from("Alex Geana"),
            avatar: None,
        };
        let violeta = Sender {
            platform_id: Arc::<str>::from("U_violeta"),
            display_name: Arc::<str>::from("Violeta"),
            avatar: None,
        };
        let mut root = text_message(
            "root-thread",
            &chat_id,
            &account,
            dani,
            "pff. iei banu de la gura copiilor.",
            14,
            24,
            false,
        );
        root.thread_id = Some(root.id.clone());
        root.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::<str>::from("1717597440.000100"),
            thread_ts: Some(Arc::<str>::from("1717597440.000100")),
            channel: chat_id.clone(),
        });
        root.reactions = vec![chat_core::Reaction {
            emoji: Arc::<str>::from("😆"),
            senders: vec![Arc::<str>::from("U_alex"), Arc::<str>::from("U_violeta")],
        }];
        let mut first_reply = text_message(
            "reply-one",
            &chat_id,
            &account,
            alex,
            "ce faci cu el?",
            14,
            27,
            false,
        );
        first_reply.reply_to = Some(root.id.clone());
        first_reply.thread_id = Some(root.id.clone());
        first_reply.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::<str>::from("1717597620.000200"),
            thread_ts: Some(Arc::<str>::from("1717597440.000100")),
            channel: chat_id.clone(),
        });
        let mut second_reply = text_message(
            "reply-two",
            &chat_id,
            &account,
            violeta,
            "confirm",
            14,
            29,
            false,
        );
        second_reply.reply_to = Some(root.id.clone());
        second_reply.thread_id = Some(root.id.clone());
        second_reply.platform_data.slack = Some(chat_core::SlackData {
            ts: Arc::<str>::from("1717597740.000300"),
            thread_ts: Some(Arc::<str>::from("1717597440.000100")),
            channel: chat_id.clone(),
        });

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[root.clone(), first_reply, second_reply],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered = rendered_lines(&render.lines).join("\n");

        assert!(rendered.contains("😆2"));
        assert!(rendered.contains("AG"));
        assert!(rendered.contains("V "));
        assert!(!rendered.contains("A "));
        assert!(!rendered.contains("[AG]"));
        assert!(!rendered.contains("[V]"));
        assert!(rendered.contains("2 replies"));
        assert!(rendered.contains("Last reply"));
        let root_hit = render
            .message_hits
            .iter()
            .find(|hit| hit.message_id == root.id)
            .expect("root message hit");
        assert!(
            root_hit
                .line_hits
                .iter()
                .any(|hit| hit.line > root_hit.start_line),
            "thread summary row should be clickable as part of the message"
        );
    }

    #[test]
    fn threaded_root_message_shows_new_badge_when_thread_has_unread_replies() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:design");
        let priya = Sender {
            platform_id: Arc::<str>::from("U_priya"),
            display_name: Arc::<str>::from("Priya"),
            avatar: None,
        };
        let sam = Sender {
            platform_id: Arc::<str>::from("U_sam"),
            display_name: Arc::<str>::from("Sam"),
            avatar: None,
        };
        let mut root = text_message(
            "root-unread",
            &chat_id,
            &account,
            priya,
            "Can we finalise the token names?",
            14,
            20,
            false,
        );
        root.thread_id = Some(root.id.clone());
        let mut reply = text_message(
            "reply-unread",
            &chat_id,
            &account,
            sam,
            "Shipping it.",
            14,
            31,
            false,
        );
        reply.reply_to = Some(root.id.clone());
        reply.thread_id = Some(root.id.clone());

        let mut thread_unread: HashMap<MessageId, u32> = HashMap::new();
        thread_unread.insert(root.id.clone(), 2);

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[root.clone(), reply],
            120,
            0,
            40,
            None,
            &HashSet::new(),
            &thread_unread,
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered = rendered_lines(&render.lines).join("\n");

        assert!(rendered.contains("1 reply"), "reply count still shown");
        assert!(
            rendered.contains("2 new"),
            "unread reply count should render the \"N new\" badge: {rendered}"
        );
    }

    #[test]
    fn build_message_lines_only_decodes_visible_media_previews() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:media");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let mut messages = Vec::new();
        for index in 0..40 {
            let mut message = text_message(
                &format!("message-{index}"),
                &chat_id,
                &account,
                sender.clone(),
                "hello",
                10,
                0,
                false,
            );
            message.content = Content::Image(chat_core::Media {
                id: Arc::<str>::from(format!("media-{index}")),
                file_name: Arc::<str>::from(format!("photo-{index}.jpg")),
                mime_type: Arc::<str>::from("image/jpeg"),
                size_bytes: Some(42),
                caption: Some(Arc::<str>::from("photo")),
                local_path: Some(PathBuf::from(format!("/tmp/missing-photo-{index}.jpg"))),
                thumbnail: None,
            });
            messages.push(message);
        }

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &messages,
            80,
            0,
            8,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );

        assert!(render.total_lines > render.lines.len());
        assert!(cache.previews.len() < messages.len());
        assert!(!render.lines.is_empty());
    }

    #[test]
    fn flat_presentation_renders_messages_left_aligned_without_bubbles() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:general");
        let incoming_sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let outgoing_sender = Sender {
            platform_id: Arc::<str>::from("me"),
            display_name: Arc::<str>::from("Me"),
            avatar: None,
        };
        let incoming = text_message(
            "incoming-flat",
            &chat_id,
            &account,
            incoming_sender,
            "Incoming flat message",
            9,
            30,
            false,
        );
        let outgoing = text_message(
            "outgoing-flat",
            &chat_id,
            &account,
            outgoing_sender,
            "Outgoing flat message",
            9,
            31,
            true,
        );

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines_with_presentation(
            &[incoming, outgoing],
            80,
            0,
            80,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            ConversationPresentation::Flat,
        );
        let rendered_lines = rendered_lines(&render.lines);
        let incoming_line = rendered_lines
            .iter()
            .find(|line| line.contains("Incoming flat message"))
            .expect("incoming Slack-style row");

        let expected_time = format_timestamp_compact(
            Utc.with_ymd_and_hms(2026, 6, 5, 9, 30, 0)
                .single()
                .expect("valid timestamp"),
        );
        assert!(incoming_line.starts_with(&expected_time));
        assert!(incoming_line.contains("Alice Incoming flat message"));
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Outgoing flat message"))
        );
        assert_eq!(render.total_lines, 3);
        assert!(
            rendered_lines.iter().any(|line| line.trim().is_empty()),
            "separate sender groups should have a spacer row: {rendered_lines:?}"
        );
        assert!(
            rendered_lines
                .iter()
                .all(|line| !line.contains('╭') && !line.contains('╰'))
        );
        assert!(
            render
                .lines
                .iter()
                .filter(|line| !line.spans.is_empty())
                .all(|line| line.alignment == Some(Alignment::Left))
        );
    }

    #[test]
    fn provider_cards_render_flat_attachment_without_extra_indent_bar() {
        let card = Card {
            kind: CardKind::ProviderAttachment,
            source: CardSource::Slack,
            title: Some(arc_str("Partition maintenance successful")),
            subtitle: None,
            body: Some(arc_str("Script: partition-maintenance")),
            footer: Some(arc_str("deploy")),
            url: None,
            accent_color: Some(CardColor::Named(arc_str("good"))),
            thumbnail: None,
            image: None,
            fields: vec![CardField {
                title: Some(arc_str("Status")),
                value: arc_str("successful"),
                short: true,
            }],
            actions: Vec::new(),
        };
        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: &mut cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Flat,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };

        let lines = card_collection_lines(&[card], Style::default(), &mut context, 0, false);
        let rendered = rendered_lines(&lines).join("\n");

        assert!(rendered.contains("Partition maintenance successful"));
        assert!(rendered.contains("Script:"));
        assert!(rendered.contains("partition-maintenance"));
        assert!(rendered.contains("Status: successful"));
        assert!(rendered.contains("deploy"));
        assert!(!rendered.contains('│'));
        assert!(!rendered.contains('╭'));
        assert!(!rendered.contains('╰'));
    }

    // Block Kit action buttons render as a row of button pills (not the
    // "Acknowledge button" fallback prose), and the layout line-count mirror
    // must agree with the rendered line count.
    #[test]
    fn card_actions_render_as_button_pills_with_matching_line_count() {
        let card = Card {
            kind: CardKind::BotMessage,
            source: CardSource::Slack,
            title: None,
            subtitle: None,
            body: Some(arc_str("🟥 **Critical priority issue is active**")),
            footer: Some(arc_str("This notification was sent via a workflow.")),
            url: None,
            accent_color: Some(CardColor::Named(arc_str("danger"))),
            thumbnail: None,
            image: None,
            fields: Vec::new(),
            actions: vec![
                chat_core::CardAction {
                    label: arc_str("🧰 Acknowledge"),
                    url: None,
                },
                chat_core::CardAction {
                    label: arc_str("✔️ Close"),
                    url: Some(arc_str("https://example.com/close")),
                },
            ],
        };
        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: &mut cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Flat,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };

        let cards = vec![card];
        let lines = card_collection_lines(&cards, Style::default(), &mut context, 0, false);
        let rendered = rendered_lines(&lines).join("\n");

        assert!(rendered.contains("[ 🧰 Acknowledge ]"));
        assert!(rendered.contains("[ ✔️ Close ]"));
        assert!(!rendered.contains("button"));
        assert_eq!(
            lines.len(),
            card_collection_line_count(&cards, 90, ConversationPresentation::Flat),
            "line-count mirror must include the actions row"
        );
    }

    // Media above the auto-download limit whose bytes are not cached must show
    // a "Retrieve media" action and register a clickable hit carrying the
    // media payload, instead of pretending a preview exists.
    #[test]
    fn oversized_media_without_local_bytes_offers_retrieve_hit() {
        let media = chat_core::Media {
            id: arc_str("https://files.slack.com/huge.png"),
            file_name: arc_str("huge.png"),
            mime_type: arc_str("image/png"),
            size_bytes: Some(chat_core::MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES + 1),
            caption: None,
            local_path: Some(PathBuf::from("/nonexistent/chat-cli-tests/huge.png")),
            thumbnail: None,
        };
        assert!(media_awaits_retrieve(&media));
        // Small media keeps the automatic pipeline; media without a reserved
        // download destination cannot advertise retrieval at all.
        assert!(!media_awaits_retrieve(&chat_core::Media {
            size_bytes: Some(1024),
            ..media.clone()
        }));
        assert!(!media_awaits_retrieve(&chat_core::Media {
            local_path: None,
            ..media.clone()
        }));

        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: &mut cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Bubbles,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };

        let lines = media_card_lines(
            "image",
            &media,
            Style::default(),
            &mut context,
            0,
            false,
            true,
        );
        let rendered = rendered_lines(&lines).join("\n");

        assert!(
            rendered.contains("Retrieve media (10.5 MB)"),
            "card should offer on-demand retrieval with the size: {rendered}"
        );
        assert!(!rendered.contains("Preview unavailable"));
        assert_eq!(media_hits.len(), 1);
        let retrieve = media_hits[0]
            .retrieve
            .as_ref()
            .expect("hit should carry the retrieve payload");
        assert_eq!(retrieve.id, media.id);
    }

    fn image_preview_card(title: &str, caption: Option<&str>) -> Card {
        Card {
            kind: CardKind::MediaPreview,
            source: CardSource::Slack,
            title: Some(arc_str(title)),
            subtitle: None,
            body: caption.map(arc_str),
            footer: None,
            url: Some(arc_str("https://erpk.slack.com/files/example.jpg")),
            accent_color: None,
            thumbnail: None,
            image: Some(chat_core::Media {
                id: arc_str("https://erpk.slack.com/files/example.jpg"),
                file_name: arc_str(title),
                mime_type: arc_str("image/jpeg"),
                size_bytes: None,
                caption: None,
                local_path: None,
                thumbnail: None,
            }),
            fields: Vec::new(),
            actions: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn flat_render_context<'a>(
        cache: &'a mut MediaPreviewCache,
        media_hits: &'a mut Vec<MediaHit>,
        link_preview_requests: &'a mut Vec<LinkPreviewRequest>,
        media_preview_requests: &'a mut Vec<MediaPreviewRequest>,
        link_metadata: &'a LinkMetadataCache,
        reply_previews: &'a HashMap<Arc<str>, ReplyPreview>,
        thread_summaries: &'a HashMap<MessageId, ThreadSummary>,
        thread_unread: &'a HashMap<MessageId, u32>,
    ) -> MessageRenderContext<'a> {
        MessageRenderContext {
            content_width: 90,
            media_cache: cache,
            media_hits,
            link_metadata,
            link_preview_requests,
            media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Flat,
            reply_previews,
            thread_summaries,
            thread_unread,
        }
    }

    #[test]
    fn image_card_line_count_excludes_filename_and_url() {
        // image rows + caption row only; filename (title) and url are hidden.
        let with_caption = image_preview_card("IMG-0001.jpg", Some("a caption"));
        assert_eq!(
            generic_card_line_count(&with_caption, 90, ConversationPresentation::Flat),
            LINK_PREVIEW_THUMBNAIL_ROWS as usize + 1
        );

        let without_caption = image_preview_card("IMG-0002.jpg", None);
        assert_eq!(
            generic_card_line_count(&without_caption, 90, ConversationPresentation::Flat),
            LINK_PREVIEW_THUMBNAIL_ROWS as usize
        );
    }

    #[test]
    fn consecutive_image_cards_render_side_by_side_without_filenames_or_urls() {
        let cards = vec![
            image_preview_card("IMG-20260608-WA0001.jpg", Some("Casa de vizavi de mine")),
            image_preview_card("IMG-20260608-WA0000.jpg", None),
        ];

        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = flat_render_context(
            &mut cache,
            &mut media_hits,
            &mut link_preview_requests,
            &mut media_preview_requests,
            &link_metadata,
            &reply_previews,
            &thread_summaries,
            &thread_unread,
        );

        let lines = card_collection_lines(&cards, Style::default(), &mut context, 0, false);
        let count = card_collection_line_count(&cards, 90, ConversationPresentation::Flat);

        // Rendered height and the cached line-count must agree.
        assert_eq!(lines.len(), count);
        // Both thumbnails share a single grid row (4 image lines) plus one caption.
        assert_eq!(lines.len(), LINK_PREVIEW_THUMBNAIL_ROWS as usize + 1);

        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("Casa de vizavi de mine"));
        assert!(!rendered.contains("IMG-20260608-WA0001.jpg"));
        assert!(!rendered.contains("IMG-20260608-WA0000.jpg"));
        assert!(!rendered.contains("https://"));
    }

    #[test]
    fn visual_media_captions_wrap_inside_bubble_cards() {
        let account = Arc::<str>::from("mock:local");
        let chat_id = Arc::<str>::from("mock:chat:media");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let caption = "Buna ziua, Comanda dvs. 6351726 a fost procesata insa din pacate produsele Math Without Numbers si The Golden Age Ovid s Metamorphoses nu s-au gasit fizic.";
        let mut message = text_message(
            "image-with-long-caption",
            &chat_id,
            &account,
            sender,
            "",
            10,
            0,
            false,
        );
        message.content = Content::Image(chat_core::Media {
            id: Arc::<str>::from("media-1"),
            file_name: Arc::<str>::from("photo.jpg"),
            mime_type: Arc::<str>::from("image/jpeg"),
            size_bytes: Some(2048),
            caption: Some(Arc::<str>::from(caption)),
            local_path: Some(PathBuf::from("/tmp/missing-photo.jpg")),
            thumbnail: None,
        });

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines(
            &[message],
            80,
            0,
            40,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);
        let caption_lines = rendered_lines
            .iter()
            .filter(|line| line.contains("Buna ziua") || line.contains("Math Without Numbers"))
            .count();

        assert!(
            caption_lines >= 2,
            "caption should wrap: {rendered_lines:?}"
        );
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Math Without Numbers")),
            "wrapped caption should keep text past the first row: {rendered_lines:?}"
        );
        assert_eq!(render.total_lines, rendered_lines.len());
    }

    #[test]
    fn flat_provider_card_artifact_text_is_sanitized() {
        let card = Card {
            kind: CardKind::ProviderAttachment,
            source: CardSource::Slack,
            title: Some(arc_str("Critical priority issue is active")),
            subtitle: None,
            body: Some(arc_str("t        ,        h        e        .")),
            footer: None,
            url: None,
            accent_color: Some(CardColor::Named(arc_str("danger"))),
            thumbnail: None,
            image: None,
            fields: Vec::new(),
            actions: Vec::new(),
        };
        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: &mut cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Flat,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };

        let lines = card_collection_lines(&[card], Style::default(), &mut context, 0, false);
        let rendered = rendered_lines(&lines).join("\n");

        assert!(!rendered.contains("t        ,        h        e"));
        assert!(rendered.contains("t , h e ."));
    }

    #[test]
    fn provider_cards_render_bubble_card_in_bubble_presentation() {
        let card = Card {
            kind: CardKind::ProviderAttachment,
            source: CardSource::Slack,
            title: Some(arc_str("Partition maintenance successful")),
            subtitle: None,
            body: Some(arc_str("Script: partition-maintenance")),
            footer: None,
            url: None,
            accent_color: Some(CardColor::Named(arc_str("good"))),
            thumbnail: None,
            image: None,
            fields: Vec::new(),
            actions: Vec::new(),
        };
        let mut cache = MediaPreviewCache::default();
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: &mut cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Bubbles,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };

        let lines = card_collection_lines(&[card], Style::default(), &mut context, 0, false);
        let rendered = rendered_lines(&lines).join("\n");

        assert!(rendered.contains("Partition maintenance successful"));
        assert!(rendered.contains("Script: partition-maintenance"));
        assert!(rendered.contains('╭'));
        assert!(rendered.contains('╰'));
    }

    #[test]
    fn flat_presentation_puts_grouped_timestamps_on_content_row_left_aligned() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:general");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let messages = vec![
            text_message(
                "first-flat",
                &chat_id,
                &account,
                sender.clone(),
                "First flat message",
                9,
                30,
                false,
            ),
            text_message(
                "second-flat",
                &chat_id,
                &account,
                sender,
                "Second flat message",
                9,
                31,
                false,
            ),
        ];

        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines_with_presentation(
            &messages,
            80,
            0,
            80,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            ConversationPresentation::Flat,
        );
        let rendered_lines = rendered_lines(&render.lines);
        let second_line = rendered_lines
            .iter()
            .find(|line| line.contains("Second flat message"))
            .expect("grouped message content line");

        assert!(second_line.contains("Second flat message"));
        assert!(second_line.starts_with(&format_timestamp_compact(messages[1].timestamp)));
        assert!(
            second_line.find(&format_timestamp_compact(messages[1].timestamp))
                < second_line.find("Second flat message"),
            "timestamp should be at the left of grouped Slack content: {second_line}"
        );
        assert_eq!(render.total_lines, 2);
    }

    #[test]
    fn flat_presentation_latest_scroll_ends_on_latest_message_without_trailing_gap() {
        let account = Arc::<str>::from("slack:workspace");
        let chat_id = Arc::<str>::from("slack:channel:general");
        let sender = Sender {
            platform_id: Arc::<str>::from("alice"),
            display_name: Arc::<str>::from("Alice"),
            avatar: None,
        };
        let messages = (0..12)
            .map(|index| {
                text_message(
                    &format!("flat-{index}"),
                    &chat_id,
                    &account,
                    sender.clone(),
                    &format!("flat message {index}"),
                    9,
                    index,
                    false,
                )
            })
            .collect::<Vec<_>>();

        let mut cache = MediaPreviewCache::default();
        let total_lines = message_line_count_with_presentation(
            &messages,
            80,
            &LinkMetadataCache::default(),
            ConversationPresentation::Flat,
        );
        let viewport_rows = 5;
        let latest_scroll = total_lines.saturating_sub(viewport_rows);
        let render = build_message_lines_with_presentation(
            &messages,
            80,
            latest_scroll,
            viewport_rows,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            ConversationPresentation::Flat,
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert_eq!(rendered_lines.len(), viewport_rows);
        assert!(
            rendered_lines
                .last()
                .is_some_and(|line| line.contains("flat message 11")),
            "latest flat message should occupy the bottom rendered row: {rendered_lines:?}"
        );
        assert!(rendered_lines.iter().any(|line| !line.trim().is_empty()));
    }

    #[test]
    fn message_list_bottom_aligns_short_history_at_latest_scroll() {
        let backend = TestBackend::new(48, 8);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let area = Rect::new(0, 0, 48, 8);
        let theme = Theme::default();

        terminal
            .draw(|frame| {
                render_message_list(
                    frame,
                    area,
                    MessageListProps {
                        title: "Messages",
                        lines: vec![Line::from("last slack message")],
                        total_lines: 20,
                        scroll: 15,
                        focused: true,
                        theme,
                    },
                );
            })
            .expect("draw");

        let rows = terminal_rows(terminal.backend(), 48);
        assert!(rows[6].contains("last slack message"));
        assert!(!rows[1].contains("last slack message"));
    }

    fn reaction_pill_test_line(emoji: &str, count: usize, _theme: Theme) -> Line<'static> {
        Line::from(vec![Span::raw("  "), Span::raw(format!("{emoji}{count}"))])
    }

    fn buffer_text(backend: &TestBackend) -> String {
        backend
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    fn terminal_rows(backend: &TestBackend, width: usize) -> Vec<String> {
        backend
            .buffer()
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    fn rendered_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn sender(platform_id: &str, display_name: &str) -> Sender {
        Sender {
            platform_id: arc_str(platform_id),
            display_name: arc_str(display_name),
            avatar: None,
        }
    }

    /// Renders a quoted original message and a reply to it, returning the
    /// rendered line strings plus the full render for count/draw parity checks.
    fn render_reply_fixture(
        original: Option<Message>,
        reply: Message,
        presentation: ConversationPresentation,
    ) -> (Vec<String>, MessageListRender) {
        let mut messages = Vec::new();
        if let Some(original) = original {
            messages.push(original);
        }
        messages.push(reply);
        let mut cache = MediaPreviewCache::default();
        let render = build_message_lines_with_presentation(
            &messages,
            60,
            0,
            400,
            None,
            &HashSet::new(),
            &HashMap::new(),
            &mut cache,
            &LinkMetadataCache::default(),
            Theme::default(),
            presentation,
        );
        (rendered_lines(&render.lines), render)
    }

    #[test]
    fn text_reply_renders_nested_titled_quote_box() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");
        let original = text_message(
            "orig",
            &chat_id,
            &account,
            sender("sorin", "Sorin"),
            "Facem 1-2 drumuri si aia e",
            22,
            14,
            false,
        );
        let mut reply = text_message(
            "reply",
            &chat_id,
            &account,
            sender("me", "Me"),
            "Loool :)",
            22,
            33,
            true,
        );
        reply.reply_to = Some(original.id.clone());

        let (rendered, _) =
            render_reply_fixture(Some(original), reply, ConversationPresentation::Bubbles);
        let joined = rendered.join("\n");

        assert!(
            rendered.iter().any(|line| line.contains("┌─ Sorin")),
            "quote box top border should carry the sender name:\n{joined}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("Facem 1-2 drumuri si aia e")),
            "quoted snippet should appear in the box:\n{joined}"
        );
        assert!(
            rendered.iter().any(|line| line.contains('└')),
            "quote box bottom border should close the box:\n{joined}"
        );
        assert!(joined.contains("Loool :)"), "reply body should render");
    }

    #[test]
    fn reply_quote_box_keeps_count_and_draw_in_sync() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");

        for presentation in [
            ConversationPresentation::Bubbles,
            ConversationPresentation::Flat,
        ] {
            let original = text_message(
                "orig",
                &chat_id,
                &account,
                sender("sorin", "Sorin"),
                "Facem 1-2 drumuri si aia e",
                22,
                14,
                false,
            );
            let mut reply = text_message(
                "reply",
                &chat_id,
                &account,
                sender("me", "Me"),
                "Loool :)",
                22,
                33,
                true,
            );
            reply.reply_to = Some(original.id.clone());

            let (rendered, render) = render_reply_fixture(Some(original), reply, presentation);
            assert_eq!(
                render.lines.len(),
                render.total_lines,
                "count pass and draw pass disagree for {presentation:?}:\n{}",
                rendered.join("\n")
            );
        }
    }

    #[test]
    fn edited_marker_renders_and_keeps_count_and_draw_in_sync() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");
        let long_text =
            "Fixed the typo in the deployment notes and linked the runbook for the rollback";

        for presentation in [
            ConversationPresentation::Bubbles,
            ConversationPresentation::Flat,
        ] {
            for width in [24_u16, 40, 60, 100] {
                let mut edited = text_message(
                    "edited",
                    &chat_id,
                    &account,
                    sender("me", "Me"),
                    long_text,
                    9,
                    30,
                    true,
                );
                edited.edited_at = Some(edited.timestamp + chrono::Duration::minutes(2));
                let plain = text_message(
                    "plain",
                    &chat_id,
                    &account,
                    sender("alice", "Alice"),
                    "Thanks!",
                    9,
                    35,
                    false,
                );
                let mut grouped_incoming = text_message(
                    "grouped-edited",
                    &chat_id,
                    &account,
                    sender("alice", "Alice"),
                    "Typo fixed",
                    9,
                    36,
                    false,
                );
                grouped_incoming.edited_at =
                    Some(grouped_incoming.timestamp + chrono::Duration::minutes(1));
                let messages = vec![edited, plain, grouped_incoming];
                let mut cache = MediaPreviewCache::default();
                let render = build_message_lines_with_presentation(
                    &messages,
                    width,
                    0,
                    400,
                    None,
                    &HashSet::new(),
                    &HashMap::new(),
                    &mut cache,
                    &LinkMetadataCache::default(),
                    Theme::default(),
                    presentation,
                );
                let rendered = rendered_lines(&render.lines);
                assert_eq!(
                    message_line_count_with_presentation(
                        &messages,
                        width,
                        &LinkMetadataCache::default(),
                        presentation,
                    ),
                    render.total_lines,
                    "measured height disagrees for {presentation:?} at width {width}:\n{}",
                    rendered.join("\n")
                );
                assert_eq!(
                    render.lines.len(),
                    render.total_lines,
                    "count pass and draw pass disagree for {presentation:?} at width {width}:\n{}",
                    rendered.join("\n")
                );
                let edited_rows = rendered
                    .iter()
                    .filter(|line| line.contains(EDITED_MARKER))
                    .count();
                // Plain text always shows the marker exactly once: inline when
                // it fits, otherwise on its own (measured) row.
                assert_eq!(
                    edited_rows,
                    2,
                    "one edited marker per edited message for {presentation:?} at width {width}:\n{}",
                    rendered.join("\n")
                );
            }
        }
    }

    #[test]
    fn reply_to_unloaded_message_renders_muted_placeholder_box() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");
        let mut reply = text_message(
            "reply",
            &chat_id,
            &account,
            sender("me", "Me"),
            "Loool :)",
            22,
            33,
            true,
        );
        reply.reply_to = Some(arc_str("missing-original"));

        let (rendered, _) = render_reply_fixture(None, reply, ConversationPresentation::Bubbles);
        let joined = rendered.join("\n");

        assert!(
            rendered
                .iter()
                .any(|line| line.contains("[message not loaded]")),
            "unloaded reply should render a placeholder snippet:\n{joined}"
        );
        assert!(
            rendered.iter().any(|line| line.contains("┌─ Reply")),
            "unloaded reply should still render a titled box:\n{joined}"
        );
    }

    #[test]
    fn long_quoted_text_is_bounded_to_a_single_snippet_line() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");
        let long = "This is a very long quoted message that should be truncated to a single \
                    snippet line inside the reply quote box so the layout stays bounded";
        let original = text_message(
            "orig",
            &chat_id,
            &account,
            sender("sorin", "Sorin"),
            long,
            22,
            14,
            false,
        );
        let mut reply = text_message(
            "reply",
            &chat_id,
            &account,
            sender("me", "Me"),
            "Loool :)",
            22,
            33,
            true,
        );
        reply.reply_to = Some(original.id.clone());

        let (rendered, _) =
            render_reply_fixture(Some(original), reply, ConversationPresentation::Bubbles);
        let joined = rendered.join("\n");

        // The quote box occupies exactly three rows: top border, one snippet
        // line (truncated with an ellipsis), and bottom border.
        let top = rendered
            .iter()
            .position(|line| line.contains("┌─ Sorin"))
            .expect("titled quote border present");
        let snippet_line = &rendered[top + 1];
        assert!(
            snippet_line.contains('…'),
            "long snippet should be truncated with an ellipsis: {snippet_line}"
        );
        assert!(
            rendered[top + 2].contains('└'),
            "snippet must be a single line so the bottom border follows it:\n{joined}"
        );
    }

    #[test]
    fn long_quoted_sender_name_is_truncated_inside_the_border() {
        let account = arc_str("mock:local");
        let chat_id = arc_str("mock:chat:alice");
        let long_name = "Bartholomew Aurelius Maximilian von Habsburg-Lothringen";
        let original = text_message(
            "orig",
            &chat_id,
            &account,
            sender("sorin", long_name),
            "short",
            22,
            14,
            false,
        );
        let mut reply = text_message(
            "reply",
            &chat_id,
            &account,
            sender("me", "Me"),
            "Loool :)",
            22,
            33,
            true,
        );
        reply.reply_to = Some(original.id.clone());

        let (rendered, _) =
            render_reply_fixture(Some(original), reply, ConversationPresentation::Bubbles);
        let border = rendered
            .iter()
            .find(|line| line.contains("┌─ "))
            .expect("titled quote border present");

        assert!(
            border.contains('…'),
            "an overlong sender name should be truncated in the border: {border}"
        );
        assert!(
            UnicodeWidthStr::width(border.as_str()) <= 60,
            "the quote border must not overflow the content width: {border}"
        );
    }

    fn rendered_line_containing(lines: &[Line<'static>], needle: &str) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .find(|line| line.contains(needle))
            .expect("rendered line containing message text")
    }

    #[allow(clippy::too_many_arguments)]
    fn text_message(
        id: &str,
        chat_id: &Arc<str>,
        account: &Arc<str>,
        sender: Sender,
        text: &str,
        hour: u32,
        minute: u32,
        is_from_me: bool,
    ) -> Message {
        Message {
            id: arc_str(id),
            chat_id: chat_id.clone(),
            account: account.clone(),
            sender,
            timestamp: Utc
                .with_ymd_and_hms(2026, 6, 5, hour, minute, 0)
                .single()
                .expect("valid timestamp"),
            edited_at: None,
            content: Content::Text(arc_str(text)),
            reply_to: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me,
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn arc_str(value: &str) -> Arc<str> {
        Arc::<str>::from(value)
    }

    fn write_test_jpeg(path: &Path, width: u32, height: u32) {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        image.save(path).expect("write test jpeg");
    }

    fn hashed_video_media(dir: &Path, caption: Option<&str>) -> chat_core::Media {
        let hash = "3f9a0c1b2d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8";
        let local = dir.join(format!("{hash}.mp4"));
        std::fs::write(&local, b"not really a video").expect("write video");
        let thumbnail = dir.join(format!("{hash}-thumb.jpg"));
        write_test_jpeg(&thumbnail, 32, 18);
        chat_core::Media {
            id: arc_str(hash),
            file_name: arc_str(&format!("{hash}.mp4")),
            mime_type: arc_str("video/mp4"),
            size_bytes: Some(4_200_000),
            caption: caption.map(arc_str),
            local_path: Some(local),
            thumbnail: Some(thumbnail),
        }
    }

    fn render_content(
        content: &Content,
        cache: &mut MediaPreviewCache,
    ) -> (Vec<Line<'static>>, Vec<MediaHit>, Vec<MediaPreviewRequest>) {
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation: ConversationPresentation::Bubbles,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };
        let lines = content_lines(content, &mut context, 0, false, Style::default(), None);
        (lines, media_hits, media_preview_requests)
    }

    fn document_media(
        local_path: Option<PathBuf>,
        size: u64,
        caption: Option<&str>,
    ) -> chat_core::Media {
        chat_core::Media {
            id: arc_str("doc-1"),
            file_name: arc_str("quarterly-report.pdf"),
            mime_type: arc_str("application/pdf"),
            size_bytes: Some(size),
            caption: caption.map(arc_str),
            local_path,
            thumbnail: None,
        }
    }

    #[test]
    fn document_card_shows_badge_name_size_and_opens_cached_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let local = dir.path().join("quarterly-report.pdf");
        std::fs::write(&local, b"%PDF-1.4").expect("write pdf");
        let media = document_media(Some(local.clone()), 1_200_000, Some("numbers attached"));
        let content = Content::File(media.clone());
        let mut cache = MediaPreviewCache::default();

        let (lines, hits, requests) = render_content(&content, &mut cache);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("PDF"), "{rendered}");
        assert!(rendered.contains("quarterly-report.pdf"), "{rendered}");
        assert!(rendered.contains("PDF · 1.2 MB"), "{rendered}");
        assert!(rendered.contains("Open ↗"), "{rendered}");
        assert!(rendered.contains("numbers attached"), "{rendered}");
        assert!(!rendered.contains("Preview unavailable"), "{rendered}");
        assert!(!rendered.contains("file: "), "{rendered}");
        assert!(requests.is_empty(), "documents never queue image decodes");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].open.as_ref(), Some(&local));
        assert!(hits[0].retrieve.is_none() && hits[0].play.is_none());
        assert_eq!(
            lines.len(),
            content_lines_len(
                &content,
                90,
                &LinkMetadataCache::default(),
                ConversationPresentation::Bubbles
            )
        );
    }

    #[test]
    fn document_card_offers_retrieve_for_large_uncached_file_and_voice_note_plays() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = document_media(
            Some(dir.path().join("missing.pdf")),
            chat_core::MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES + 1,
            None,
        );
        let content = Content::File(big);
        let mut cache = MediaPreviewCache::default();
        let (lines, hits, _) = render_content(&content, &mut cache);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("Retrieve"), "{rendered}");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].retrieve.is_some() && hits[0].open.is_none());

        let not_cached = Content::File(document_media(None, 10, None));
        let (lines, hits, _) = render_content(&not_cached, &mut cache);
        assert!(rendered_lines(&lines).join("\n").contains("Not downloaded"));
        assert!(hits.is_empty());

        let voice_path = dir.path().join("note.ogg");
        std::fs::write(&voice_path, b"OggS").expect("write voice");
        let voice = Content::Audio(chat_core::Media {
            id: arc_str("voice-1"),
            file_name: arc_str("note.ogg"),
            mime_type: arc_str("audio/ogg"),
            size_bytes: Some(34_000),
            caption: None,
            local_path: Some(voice_path.clone()),
            thumbnail: None,
        });
        let (lines, hits, _) = render_content(&voice, &mut cache);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("♪"), "{rendered}");
        assert!(rendered.contains("Voice note · 34 KB"), "{rendered}");
        assert!(rendered.contains("Play ↗"), "{rendered}");
        assert_eq!(hits[0].open.as_ref(), Some(&voice_path));
        assert_eq!(
            lines.len(),
            content_lines_len(
                &voice,
                90,
                &LinkMetadataCache::default(),
                ConversationPresentation::Bubbles
            )
        );
    }

    #[test]
    fn image_sent_as_document_keeps_preview_card() {
        let media = chat_core::Media {
            mime_type: arc_str("image/png"),
            ..document_media(None, 10, None)
        };
        assert!(!uses_document_card(
            &media,
            ConversationPresentation::Bubbles
        ));
        assert!(!uses_document_card(
            &document_media(None, 10, None),
            ConversationPresentation::Flat
        ));
    }

    fn render_video(
        media: &chat_core::Media,
        cache: &mut MediaPreviewCache,
        presentation: ConversationPresentation,
    ) -> (Vec<Line<'static>>, Vec<MediaHit>, Vec<MediaPreviewRequest>) {
        let mut media_hits = Vec::new();
        let mut link_preview_requests = Vec::new();
        let mut media_preview_requests = Vec::new();
        let reply_previews = HashMap::new();
        let thread_summaries = HashMap::new();
        let thread_unread = HashMap::new();
        let link_metadata = LinkMetadataCache::default();
        let mut context = MessageRenderContext {
            content_width: 90,
            media_cache: cache,
            media_hits: &mut media_hits,
            link_metadata: &link_metadata,
            link_preview_requests: &mut link_preview_requests,
            media_preview_requests: &mut media_preview_requests,
            theme: Theme::default(),
            previous_sender: None,
            presentation,
            reply_previews: &reply_previews,
            thread_summaries: &thread_summaries,
            thread_unread: &thread_unread,
        };
        let lines = video_card_lines(media, Style::default(), &mut context, 0, false);
        (lines, media_hits, media_preview_requests)
    }

    /// Resolves every queued preview decode synchronously, mirroring what the
    /// app's background workers do between draws.
    fn complete_preview_requests(
        cache: &mut MediaPreviewCache,
        requests: Vec<MediaPreviewRequest>,
    ) {
        for request in requests {
            let result = decode_image_preview_rows_for_key(&request.key);
            cache.insert(request.key, result);
        }
    }

    #[test]
    fn video_card_hides_hash_name_and_queues_probe_then_shows_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = hashed_video_media(dir.path(), Some("look at this"));
        let mut cache = MediaPreviewCache::default();

        // Phase 1: nothing cached yet — placeholder preview, probe queued, and
        // the hash never leaks into the card.
        let (lines, hits, requests) =
            render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(!rendered.contains("3f9a0c1b"), "hash leaked: {rendered}");
        assert!(rendered.contains("▶ Video · 4.2 MB"), "{rendered}");
        assert!(rendered.contains("loading image"), "{rendered}");
        assert_eq!(
            cache.take_video_probe_requests(),
            vec![media.local_path.clone().expect("local path")]
        );
        assert_eq!(requests.len(), 1, "embedded thumbnail decode queued");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].play.as_ref(), media.local_path.as_ref());
        assert_eq!(
            lines.len(),
            video_card_line_count(&media, 90, ConversationPresentation::Bubbles)
        );

        // Phase 2: probe finishes with a sharp poster; its decode completes.
        let poster = dir.path().join("poster.jpg");
        write_test_jpeg(&poster, 320, 180);
        cache.insert_video_info(
            media.local_path.clone().expect("local path"),
            Ok(VideoInfo {
                duration_ms: Some(42_480),
                width: Some(1280),
                height: Some(720),
                has_audio: Some(true),
                poster: Some(poster.clone()),
                animation: None,
            }),
        );
        let (_, _, requests) = render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].key.path, poster,
            "poster preferred over thumbnail"
        );
        complete_preview_requests(&mut cache, requests);

        let (lines, hits, requests) =
            render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(requests.is_empty());
        assert!(
            rendered.contains("▶ Video · 0:42 · 1280×720 · 4.2 MB"),
            "{rendered}"
        );
        assert!(rendered.contains(" ▶ "), "play badge overlay: {rendered}");
        assert!(rendered.contains("look at this"));
        assert!(!rendered.contains("loading image"));
        assert!(cache.take_video_probe_requests().is_empty());
        assert_eq!(hits[0].preview_path, poster);
        assert_eq!(
            lines.len(),
            video_card_line_count(&media, 90, ConversationPresentation::Bubbles),
            "late metadata must not change the line count"
        );
    }

    #[test]
    fn gif_style_video_loops_frames_inline_without_play_badge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = hashed_video_media(dir.path(), None);
        let local = media.local_path.clone().expect("local path");
        let frames_dir = dir.path().join("clip-frames");
        std::fs::create_dir_all(&frames_dir).expect("frames dir");
        let frames: Vec<PathBuf> = (0..2)
            .map(|index| {
                let frame = frames_dir.join(format!("frame-{index:03}.jpg"));
                write_test_jpeg(&frame, 64, 64);
                frame
            })
            .collect();
        let mut cache = MediaPreviewCache::default();
        cache.insert_video_info(
            local,
            Ok(VideoInfo {
                duration_ms: Some(2_000),
                width: Some(200),
                height: Some(200),
                has_audio: Some(false),
                poster: Some(frames[0].clone()),
                animation: Some(video::Animation {
                    frames: frames.clone(),
                    delays_ms: vec![100, 100],
                }),
            }),
        );

        // Phase 1: frames not decoded yet — every frame decode is queued and
        // the loop is not reported as running, so the app stays idle.
        cache.begin_animation_frame(0);
        let (lines, _, requests) =
            render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("GIF · 200×200"), "{rendered}");
        assert!(!rendered.contains("▶ Video"), "{rendered}");
        // The poster fallback is frame 0, so its decode shares that key.
        let queued: std::collections::HashSet<_> = requests
            .iter()
            .map(|request| request.key.path.clone())
            .collect();
        assert_eq!(
            queued,
            frames.iter().cloned().collect(),
            "all frames queued"
        );
        assert!(!cache.animation_active());
        complete_preview_requests(&mut cache, requests);

        // Phase 2: frames cached — the clock picks the frame, the loop keeps
        // the app ticking, and no play badge or HD badge is drawn.
        cache.begin_animation_frame(0);
        let (lines, hits, requests) =
            render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        assert!(requests.is_empty());
        assert!(cache.animation_active());
        assert_eq!(hits[0].path, frames[0]);
        assert!(!hits[0].play_badge);
        assert_eq!(hits[0].preview_skip_rows, 1, "HD image stays below title");
        assert!(video::is_animation_frame(&hits[0].path));
        assert!(!rendered_lines(&lines).join("\n").contains(" ▶ "));

        cache.begin_animation_frame(150);
        let (later, hits, _) = render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        assert_eq!(hits[0].path, frames[1], "clock advanced to the next frame");
        assert_eq!(later.len(), lines.len(), "frames never change the layout");
    }

    #[test]
    fn flat_video_card_line_count_matches_render() {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = hashed_video_media(dir.path(), Some("a caption"));
        let mut cache = MediaPreviewCache::default();
        let (lines, _, _) = render_video(&media, &mut cache, ConversationPresentation::Flat);
        assert_eq!(
            lines.len(),
            video_card_line_count(&media, 90, ConversationPresentation::Flat)
        );
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.starts_with("▶ Video"), "{rendered}");
    }

    #[test]
    fn video_card_keeps_meaningful_file_names_and_offers_retrieve() {
        let media = chat_core::Media {
            id: arc_str("slack-video"),
            file_name: arc_str("launch-demo.mp4"),
            mime_type: arc_str("video/mp4"),
            size_bytes: Some(chat_core::MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES + 1),
            caption: None,
            local_path: Some(PathBuf::from("/nonexistent/launch-demo.mp4")),
            thumbnail: None,
        };
        let mut cache = MediaPreviewCache::default();
        let (lines, hits, _) = render_video(&media, &mut cache, ConversationPresentation::Bubbles);
        let rendered = rendered_lines(&lines).join("\n");
        assert!(rendered.contains("▶ launch-demo.mp4"), "{rendered}");
        assert!(rendered.contains("click to retrieve"), "{rendered}");
        assert!(
            cache.take_video_probe_requests().is_empty(),
            "no local file to probe"
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].retrieve.is_some());
        assert!(hits[0].play.is_none());
        assert_eq!(
            content_preview_text(&Content::Video(media.clone())),
            "launch-demo.mp4"
        );
    }
}
