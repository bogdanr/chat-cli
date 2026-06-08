use crate::theme::Theme;
use chat_core::{Content, Message, MessageId, ReceiptKind, Sender, Timestamp};
use chrono::Local;
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

const MEDIA_PREVIEW_MAX_WIDTH: u16 = 48;
const MEDIA_PREVIEW_ROWS: u16 = 8;
const LINK_PREVIEW_CARD_WIDTH: u16 = 42;
const LINK_PREVIEW_THUMBNAIL_ROWS: u16 = 4;
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
    pub path: PathBuf,
    pub title: String,
    pub caption: Option<String>,
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
}

impl MediaPreviewCache {
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

#[derive(Debug, Default)]
pub struct MessageLayoutCache {
    key: Option<MessageLayoutKey>,
    entries: Vec<MessageLayoutEntry>,
    total_lines: usize,
    reply_previews: HashMap<Arc<str>, String>,
    thread_summaries: HashMap<MessageId, ThreadSummary>,
}

impl MessageLayoutCache {
    pub fn clear(&mut self) {
        self.key = None;
        self.entries.clear();
        self.total_lines = 0;
        self.reply_previews.clear();
        self.thread_summaries.clear();
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

pub fn render_message_list(frame: &mut Frame<'_>, area: Rect, props: MessageListProps<'_>) {
    frame.render_widget(Clear, area);
    let viewport_rows = inner_area(area).height as usize;
    let lines =
        bottom_aligned_message_lines(props.lines, props.total_lines, props.scroll, viewport_rows);
    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(props.title)
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
    media_cache: &mut MediaPreviewCache,
    link_metadata: &LinkMetadataCache,
    link_metadata_revision: u64,
    cache: &mut MessageLayoutCache,
    theme: Theme,
    presentation: ConversationPresentation,
) -> MessageListRender {
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

    build_message_lines_from_layout_cache(
        messages,
        content_width,
        scroll,
        viewport_rows,
        selected_message_id,
        unread_message_ids,
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
        .map(|message| (message.id.clone(), compact_message_preview(message)))
        .collect::<HashMap<_, _>>();
    let mut context = MessageRenderContext {
        media_cache,
        content_width,
        media_hits: &mut media_hits,
        theme,
        previous_sender: None,
        reply_previews: &reply_previews,
        thread_summaries: &thread_summaries,
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
        let thread_summary_hit = message_thread_summary_hit(
            &message_lines,
            message_start,
            content_width,
            context.thread_summaries.get(&message.id).is_some(),
        );
        let end_line = message_start + message_lines.len().saturating_sub(1);
        if end_line >= message_start {
            message_hits.push(MessageHit {
                start_line: message_start,
                end_line,
                message_id: message.id.clone(),
                line_hits,
                avatar_hit,
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
        .map(|message| (message.id.clone(), compact_message_preview(message)))
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
        let thread_summary_hit = message_thread_summary_hit(
            &message_lines,
            message_start,
            content_width,
            context.thread_summaries.get(&message.id).is_some(),
        );
        let end_line = message_start + message_lines.len().saturating_sub(1);
        if end_line >= message_start {
            message_hits.push(MessageHit {
                start_line: message_start,
                end_line,
                message_id: message.id.clone(),
                line_hits,
                avatar_hit,
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
    cache.total_lines
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

pub fn format_message_time(timestamp: chat_core::Timestamp) -> String {
    timestamp.with_timezone(&Local).format("%H:%M").to_string()
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
    reply_previews: &'a HashMap<Arc<str>, String>,
    thread_summaries: &'a HashMap<MessageId, ThreadSummary>,
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
        let timestamp = format_message_time(message.timestamp);
        push_right_aligned_spans(
            &mut header_spans,
            context.content_width,
            &[Span::styled(timestamp, context.theme.muted())],
        );
        lines.push(Line::from(header_spans));
    }

    if let Some(reply_to) = &message.reply_to {
        let preview = context
            .reply_previews
            .get(reply_to)
            .cloned()
            .unwrap_or_else(|| format!("message {}", short_id(reply_to)));
        lines.push(Line::from(Span::styled(
            format!("  ↪ {preview}"),
            context.theme.muted(),
        )));
    }

    lines.extend(content_lines(
        &message.content,
        context,
        start_line + lines.len(),
        message.is_from_me,
        accent_style,
        Some(&message.id),
    ));

    let receipts = receipt_summary(message);
    if message.is_from_me {
        let status = if receipts.is_empty() {
            format_message_time(message.timestamp)
        } else {
            format!("{} · {receipts}", format_message_time(message.timestamp))
        };
        lines.push(status_line(&status, context.theme.muted()));
    } else if !receipts.is_empty() {
        lines.push(status_line(&receipts, context.theme.muted()));
    }

    if !message.reactions.is_empty() {
        lines.push(reaction_pill_line(message, context.theme));
    }

    if let Some(summary) = context.thread_summaries.get(&message.id) {
        lines.push(thread_summary_line(summary, message.is_from_me, context));
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
    let body_start_line =
        start_line + spacer_lines + usize::from(!grouped) + usize::from(message.reply_to.is_some());
    let mut body_lines = slack_content_lines(message, context, body_start_line, accent_style);
    if body_lines.is_empty() {
        body_lines.push(Line::from(""));
    }
    let merge_first_body_line =
        !grouped && message.reply_to.is_none() && slack_content_can_merge(&message.content);

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
        let preview = context
            .reply_previews
            .get(reply_to)
            .cloned()
            .unwrap_or_else(|| format!("message {}", short_id(reply_to)));
        lines.push(slack_indented_line(vec![Span::styled(
            format!("↪ {preview}"),
            context.theme.muted(),
        )]));
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
        let summary_spans = thread_summary_spans(summary, context);
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

fn slack_message_spacer_lines(grouped: bool, has_previous_message: bool) -> usize {
    usize::from(has_previous_message && !grouped) * SLACK_MESSAGE_SPACER_LINES
}

fn apply_slack_timestamp_selection(line: &mut Line<'static>, theme: Theme) {
    let selection_style = Style::default().fg(theme.foreground).bg(Color::DarkGray);
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
        hit.start_col = hit.start_col.saturating_add(SLACK_BODY_INDENT_WIDTH as u16);
        hit.end_col = hit.end_col.saturating_add(SLACK_BODY_INDENT_WIDTH as u16);
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
    vec![
        Span::styled(format_message_time(message.timestamp), theme.muted()),
        Span::raw(" ".repeat(SLACK_GUTTER_GAP)),
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
        Content::Image(media) => {
            media_card_lines("Photo", media, accent, context, start_line, is_from_me)
        }
        Content::Video(media) => {
            media_card_lines("Video", media, accent, context, start_line, is_from_me)
        }
        Content::Audio(media) => {
            media_card_lines("Voice note", media, accent, context, start_line, is_from_me)
        }
        Content::File(media) => {
            media_card_lines("File", media, accent, context, start_line, is_from_me)
        }
        Content::Sticker(media) => {
            media_card_lines("Sticker", media, accent, context, start_line, is_from_me)
        }
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
    let Some(url) = first_url_in_text(text) else {
        return text_content_lines(text, context.content_width, accent, context.presentation);
    };

    let url = Arc::<str>::from(url);
    let metadata = context.link_metadata.get(&url);
    if metadata.is_none()
        && let Some(message_id) = message_id
    {
        context.link_preview_requests.push(LinkPreviewRequest {
            message_id: message_id.clone(),
            url,
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
            url,
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

fn first_url_in_text(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|part| part.starts_with("https://") || part.starts_with("http://"))
        .map(|part| part.trim_end_matches(['.', ',', ')', ']', '}']))
        .filter(|url| !url.is_empty())
}

fn remove_first_url_from_text(text: &str) -> String {
    let Some(url) = first_url_in_text(text) else {
        return text.trim().to_owned();
    };
    text.replacen(url, "", 1)
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
    for card in cards {
        lines.extend(generic_card_lines(
            card,
            accent,
            context,
            start_line + lines.len(),
            is_from_me,
        ));
    }
    lines
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
    let mut lines = Vec::new();

    if let Some(image) = card.image.as_ref().or(card.thumbnail.as_ref()) {
        let (preview_rows, source, error, ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        if let (Some(path), None) = (&source, &error)
            && ready
        {
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
                path: path.clone(),
                title: card
                    .title
                    .as_deref()
                    .unwrap_or(image.file_name.as_ref())
                    .to_owned(),
                caption: card.body.as_deref().map(str::to_owned),
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
    if let Some(title) = card.title.as_deref() {
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
    if let Some(footer) = card.footer.as_deref() {
        lines.push(flat_card_text_line(
            accent,
            footer,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(url) = card.url.as_deref() {
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
    let mut lines = vec![card_border_line('╭', '─', '╮', card_width, accent)];

    if let Some(image) = card.image.as_ref().or(card.thumbnail.as_ref()) {
        let (preview_rows, source, error, ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );
        if let (Some(path), None) = (&source, &error)
            && ready
        {
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
                path: path.clone(),
                title: card
                    .title
                    .as_deref()
                    .unwrap_or(image.file_name.as_ref())
                    .to_owned(),
                caption: card.body.as_deref().map(str::to_owned),
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
    if let Some(title) = card.title.as_deref() {
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
    if let Some(footer) = card.footer.as_deref() {
        lines.push(card_text_line(
            accent,
            footer,
            card_width,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(url) = card.url.as_deref() {
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
        let (preview_rows, source, error, ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );

        if let (Some(path), None) = (&source, &error)
            && ready
        {
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
                path: path.clone(),
                title: link
                    .title
                    .as_deref()
                    .unwrap_or("Link preview image")
                    .to_owned(),
                caption: link.description.as_deref().map(str::to_owned),
            });
        }

        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| card_preview_line(accent, row, card_width)),
        );
    }

    let title = link.title.as_deref().unwrap_or("Link");
    let source = link_preview_source_label(link.url.as_ref());

    lines.push(card_text_line(
        accent,
        title,
        card_width,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(description) = link.description.as_deref() {
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
        let (preview_rows, source, error, ready) = media_preview_rows(
            image,
            context,
            card_width,
            LINK_PREVIEW_THUMBNAIL_ROWS,
            accent.fg.unwrap_or(Color::DarkGray),
        );

        if let (Some(path), None) = (&source, &error)
            && ready
        {
            context.media_hits.push(MediaHit {
                start_line: start_line + lines.len(),
                end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
                start_col: 0,
                end_col: card_width,
                path: path.clone(),
                title: link
                    .title
                    .as_deref()
                    .unwrap_or("Link preview image")
                    .to_owned(),
                caption: link.description.as_deref().map(str::to_owned),
            });
        }

        lines.extend(
            preview_rows
                .into_iter()
                .map(|row| flat_card_preview_line(accent, row)),
        );
    }

    let title = link.title.as_deref().unwrap_or("Link");
    let source = link_preview_source_label(link.url.as_ref());

    lines.push(flat_card_text_line(
        accent,
        title,
        card_width,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if let Some(description) = link.description.as_deref() {
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
    let path = source.filter(|_| error.is_none() && ready)?;
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
        path,
        title: media.file_name.to_string(),
        caption: media.caption.as_deref().map(str::to_owned),
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
    if let Some(caption) = &media.caption {
        lines.push(card_text_line(
            accent,
            &format!("caption: {caption}"),
            card_width,
            Style::default(),
        ));
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
) -> Vec<Line<'static>> {
    if context.presentation == ConversationPresentation::Flat {
        return flat_media_card_lines(label, media, accent, context, start_line);
    }

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
    let mut lines = vec![
        card_border_line('╭', '─', '╮', card_width, accent),
        card_text_line(
            accent,
            label,
            card_width,
            accent.add_modifier(Modifier::BOLD),
        ),
    ];

    if let (Some(path), None) = (&source, &error)
        && ready
    {
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
            path: path.clone(),
            title: media.file_name.to_string(),
            caption: media.caption.as_deref().map(str::to_owned),
        });
    }

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

    if error.is_some() || source.is_none() {
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

    if let Some(caption) = &media.caption {
        lines.push(card_text_line(
            accent,
            &format!("caption: {caption}"),
            card_width,
            Style::default(),
        ));
    }

    lines.push(card_border_line('╰', '─', '╯', card_width, accent));
    lines
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
    let (preview_rows, source, error, ready) = media_preview_rows(
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

    if let (Some(path), None) = (&source, &error)
        && ready
    {
        context.media_hits.push(MediaHit {
            start_line: start_line + lines.len(),
            end_line: start_line + lines.len() + preview_rows.len().saturating_sub(1),
            start_col: 0,
            end_col: card_width,
            path: path.clone(),
            title: media.file_name.to_string(),
            caption: media.caption.as_deref().map(str::to_owned),
        });
    }

    lines.extend(
        preview_rows
            .into_iter()
            .map(|row| flat_card_preview_line(accent, row)),
    );

    if error.is_some() || source.is_none() {
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

    if let Some(caption) = &media.caption {
        lines.push(flat_card_text_line(
            accent,
            caption,
            card_width,
            Style::default(),
        ));
    }

    lines
}

fn media_preview_rows(
    media: &chat_core::Media,
    context: &mut MessageRenderContext<'_>,
    width: u16,
    rows: u16,
    accent: Color,
) -> (
    Vec<Vec<Span<'static>>>,
    Option<PathBuf>,
    Option<String>,
    bool,
) {
    let Some(source) = media_preview_source(media) else {
        return (
            fallback_preview_rows(width, rows, accent, "no local image"),
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

fn media_preview_source(media: &chat_core::Media) -> Option<PathBuf> {
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
    let resized = image
        .resize_exact(
            u32::from(fit_width.max(1)),
            u32::from(fit_rows.max(1)) * 2,
            FilterType::Triangle,
        )
        .to_rgba8();
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
            spans.push(Span::styled(
                "▀",
                Style::default()
                    .fg(rgba_to_color(top.0))
                    .bg(rgba_to_color(bottom.0)),
            ));
        }
        rendered_rows.push(pad_preview_row(spans, fit_width, width));
    }
    for _ in 0..bottom_padding {
        rendered_rows.push(empty_preview_row(width));
    }

    Ok(rendered_rows)
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
    let caption_line_width = media
        .caption
        .as_deref()
        .map(|caption| UnicodeWidthStr::width(format!("caption: {caption}").as_str()))
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
                segments.push((remaining.to_owned(), style));
            }
            break;
        };

        if offset > 0 {
            segments.push((remaining[..offset].to_owned(), style));
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

fn wrap_text(text: &str, width: usize) -> Vec<String> {
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
    context: &mut MessageRenderContext<'_>,
) -> Line<'static> {
    let mut spans = Vec::new();
    if !is_from_me {
        spans.push(Span::raw("  "));
    }

    spans.extend(thread_summary_spans(summary, context));

    if is_from_me {
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
}

fn thread_summary_spans(
    summary: &ThreadSummary,
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

fn compact_message_preview(message: &Message) -> String {
    let text = content_preview_text(&message.content);
    let preview = if text.trim().is_empty() {
        "attachment".to_owned()
    } else {
        truncate_chars(text.trim(), 38)
    };
    format!("{}: {preview}", message.sender.display_name)
}

fn content_preview_text(content: &Content) -> String {
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
            format!("{title}: {}", link.url)
        }
        Content::Cards(cards) => card_collection_text(cards),
        Content::Poll(poll) => format!("Poll: {}", poll.question),
        Content::Deleted => String::new(),
        Content::Unsupported(kind) => format!("Unsupported message: {kind}"),
    }
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

fn short_id(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .chars()
        .take(10)
        .collect()
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
        + usize::from(message.reply_to.is_some())
        + content_lines_len(&message.content, content_width, link_metadata, presentation)
        + usize::from(message.is_from_me || !receipt_summary(message).is_empty())
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
        + usize::from(message.reply_to.is_some())
        + content_count.saturating_sub(merged_content_line)
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
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media_card_line_count(media, presentation),
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
    let Some(url) = first_url_in_text(text) else {
        return text_content_line_count(text, content_width, presentation);
    };
    let Some(metadata) = link_metadata.get(url) else {
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
        text_lines + media_card_line_count(image, presentation)
    } else {
        text_lines
            + link_preview_card_line_count_for_image(
                false,
                metadata.description.is_some(),
                presentation,
            )
    }
}

fn media_card_line_count(
    media: &chat_core::Media,
    presentation: ConversationPresentation,
) -> usize {
    match presentation {
        ConversationPresentation::Bubbles => {
            4 + MEDIA_PREVIEW_ROWS as usize + usize::from(media.caption.is_some())
        }
        ConversationPresentation::Flat => {
            1 + MEDIA_PREVIEW_ROWS as usize + 1 + usize::from(media.caption.is_some())
        }
    }
}

fn card_collection_line_count(
    cards: &[chat_core::Card],
    content_width: u16,
    presentation: ConversationPresentation,
) -> usize {
    cards
        .iter()
        .map(|card| generic_card_line_count(card, content_width, presentation))
        .sum()
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
    let body_lines = card
        .body
        .as_deref()
        .map(|body| wrap_text(&sanitize_flat_provider_text(body), card_width).len())
        .unwrap_or(0);
    let content_lines = usize::from(card.image.is_some() || card.thumbnail.is_some())
        * LINK_PREVIEW_THUMBNAIL_ROWS as usize
        + usize::from(card.subtitle.is_some())
        + usize::from(card.title.is_some())
        + body_lines
        + card.fields.len()
        + usize::from(card.footer.is_some())
        + usize::from(card.url.is_some())
        + usize::from(
            card.title.is_none()
                && card.subtitle.is_none()
                && card.body.is_none()
                && card.footer.is_none()
                && card.url.is_none()
                && card.fields.is_empty(),
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
        link.description.is_some(),
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
            .position(|line| line.contains("photo.jpg"))
            .expect("media card file line");
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
                title: Some(Arc::<str>::from("Actual article title")),
                description: Some(Arc::<str>::from("Fetched Open Graph description")),
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
            &mut cache,
            &metadata,
            Theme::default(),
        );
        let rendered_lines = rendered_lines(&render.lines);

        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Actual article title"))
        );
        assert!(
            rendered_lines
                .iter()
                .any(|line| line.contains("Fetched Open Graph description"))
        );
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

        let expected_time = format_message_time(
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
        assert!(second_line.starts_with(&format_message_time(messages[1].timestamp)));
        assert!(
            second_line.find(&format_message_time(messages[1].timestamp))
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
            platform_data: PlatformData::default(),
        }
    }

    fn arc_str(value: &str) -> Arc<str> {
        Arc::<str>::from(value)
    }
}
