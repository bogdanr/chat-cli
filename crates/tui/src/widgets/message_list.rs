use crate::theme::Theme;
use chat_core::{Content, Message, ReceiptKind, Sender};
use image::imageops::FilterType;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MEDIA_PREVIEW_MAX_WIDTH: u16 = 48;
const MEDIA_PREVIEW_ROWS: u16 = 8;
const MESSAGE_AVATAR_WIDTH: u16 = 2;
const MESSAGE_AVATAR_ROWS: u16 = 1;
const BUBBLE_MAX_PERCENT: u16 = 72;
const BUBBLE_MIN_WIDTH: usize = 10;

pub struct MessageListProps<'a> {
    pub title: &'a str,
    pub lines: Vec<Line<'static>>,
    pub total_lines: usize,
    pub scroll: usize,
    pub focused: bool,
    pub theme: Theme,
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
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MediaPreviewKey {
    path: PathBuf,
    width: u16,
    rows: u16,
}

#[derive(Debug, Default)]
pub struct MediaPreviewCache {
    previews: HashMap<MediaPreviewKey, Result<Vec<Vec<Span<'static>>>, String>>,
}

pub struct MessageListRender {
    pub lines: Vec<Line<'static>>,
    pub media_hits: Vec<MediaHit>,
    pub message_hits: Vec<MessageHit>,
    pub total_lines: usize,
}

pub fn render_message_list(frame: &mut Frame<'_>, area: Rect, props: MessageListProps<'_>) {
    frame.render_widget(Clear, area);
    let paragraph = Paragraph::new(props.lines).block(
        Block::default()
            .title(props.title)
            .borders(Borders::ALL)
            .border_style(props.theme.focus_border(props.focused)),
    );
    frame.render_widget(paragraph, area);
    render_scrollbar(frame, area, props.total_lines, props.scroll, props.theme);
}

pub fn build_message_lines(
    messages: &[Message],
    content_width: u16,
    scroll: usize,
    viewport_rows: usize,
    selected_message_id: Option<&str>,
    media_cache: &mut MediaPreviewCache,
    theme: Theme,
) -> MessageListRender {
    let total_lines = message_line_count(messages, content_width);
    let render_start = scroll.saturating_sub(1);
    let render_end = scroll
        .saturating_add(viewport_rows.max(1))
        .saturating_add(1)
        .min(total_lines);
    let mut all_lines = Vec::new();
    let mut media_hits = Vec::new();
    let mut message_hits = Vec::new();
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
        reply_previews,
    };

    let mut line_cursor: usize = 0;
    for message in messages {
        let grouped = context
            .previous_sender
            .as_ref()
            .is_some_and(|previous| previous == &message.sender.platform_id);
        let line_count = message_lines_len(message, grouped, content_width);
        let message_start = line_cursor;
        let message_end_exclusive = message_start.saturating_add(line_count);
        context.previous_sender = Some(message.sender.platform_id.clone());
        line_cursor = message_end_exclusive;

        if message_end_exclusive <= render_start || message_start >= render_end {
            continue;
        }

        let selected = selected_message_id == Some(message.id.as_ref());
        let message_lines = message_lines(message, &mut context, message_start, selected, grouped);
        let line_hits = message_line_hits(&message_lines, message_start, content_width, grouped);
        let avatar_hit = message_avatar_hit(&message_lines, message_start, content_width, grouped);
        let end_line = message_start + message_lines.len().saturating_sub(1);
        if end_line >= message_start {
            message_hits.push(MessageHit {
                start_line: message_start,
                end_line,
                message_id: message.id.clone(),
                line_hits,
                avatar_hit,
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
    }
}

fn message_line_hits(
    lines: &[Line<'static>],
    start_line: usize,
    content_width: u16,
    grouped: bool,
) -> Vec<MessageLineHit> {
    lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            (*index == 0 && !grouped) || is_clickable_message_content_line(line)
        })
        .filter_map(|(index, line)| {
            let width = line_width(line).min(content_width as usize) as u16;
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
) -> Option<MessageLineHit> {
    if grouped {
        return None;
    }
    let header = lines.first()?;
    let width = line_width(header).min(content_width as usize) as u16;
    if width == 0 {
        return None;
    }
    let start_col = aligned_line_start_col(header, width, content_width);
    let selected_prefix = header
        .spans
        .first()
        .filter(|span| span.content.as_ref() == "▏ ")
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
        .unwrap_or_default();
    let avatar_start = start_col.saturating_add(selected_prefix);
    Some(MessageLineHit {
        line: start_line,
        start_col: avatar_start,
        end_col: avatar_start.saturating_add(MESSAGE_AVATAR_WIDTH),
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
    content.starts_with('╭') || content.starts_with('│') || content.starts_with('╰')
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

pub fn message_line_count(messages: &[Message], content_width: u16) -> usize {
    let mut previous_sender: Option<&Arc<str>> = None;
    messages
        .iter()
        .map(|message| {
            let grouped =
                previous_sender.is_some_and(|previous| previous == &message.sender.platform_id);
            previous_sender = Some(&message.sender.platform_id);
            message_lines_len(message, grouped, content_width)
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

pub fn bubble_accent(theme: Theme, is_from_me: bool) -> Style {
    if is_from_me {
        Style::default().fg(theme.outgoing)
    } else {
        Style::default().fg(theme.incoming)
    }
}

struct MessageRenderContext<'a> {
    media_cache: &'a mut MediaPreviewCache,
    content_width: u16,
    media_hits: &'a mut Vec<MediaHit>,
    theme: Theme,
    previous_sender: Option<Arc<str>>,
    reply_previews: HashMap<Arc<str>, String>,
}

fn message_lines(
    message: &Message,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    selected: bool,
    grouped: bool,
) -> Vec<Line<'static>> {
    let accent_style = bubble_accent(context.theme, message.is_from_me);
    let mut lines = Vec::new();

    if !grouped {
        let mut header_spans = Vec::new();
        header_spans.extend(message_avatar_spans(&message.sender, context.media_cache));
        header_spans.extend([
            Span::raw(" "),
            Span::styled(
                message.sender.display_name.to_string(),
                sender_style(context.theme, message.is_from_me),
            ),
            Span::raw(if message.is_from_me { " (me)" } else { "" }),
        ]);
        if !message.is_from_me {
            header_spans.push(Span::styled(
                format!("  {}", message.timestamp.format("%H:%M")),
                context.theme.muted(),
            ));
        }
        lines.push(Line::from(header_spans));
    }

    if let Some(reply_to) = &message.reply_to {
        let preview = context
            .reply_previews
            .get(reply_to)
            .cloned()
            .unwrap_or_else(|| format!("message {}", short_id(reply_to)));
        lines.push(Line::from(Span::styled(
            format!("↪ {preview}"),
            context.theme.muted(),
        )));
    }

    lines.extend(content_lines(
        &message.content,
        context,
        start_line + lines.len(),
        message.is_from_me,
    ));

    let receipts = receipt_summary(message);
    if message.is_from_me {
        let status = if receipts.is_empty() {
            message.timestamp.format("%H:%M").to_string()
        } else {
            format!("{} · {receipts}", message.timestamp.format("%H:%M"))
        };
        lines.push(status_line(&status, context.theme.muted()));
    } else if !receipts.is_empty() {
        lines.push(status_line(&receipts, context.theme.muted()));
    }

    if !message.reactions.is_empty() {
        lines.push(reaction_pill_line(message, context.theme));
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

fn content_lines(
    content: &Content,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    match content {
        Content::Text(text) => {
            text_bubble_lines(text, context.content_width, context.theme, is_from_me)
        }
        Content::Image(media) => media_card_lines(
            "Photo",
            media,
            Color::DarkGray,
            context,
            start_line,
            is_from_me,
        ),
        Content::Video(media) => media_card_lines(
            "Video",
            media,
            Color::LightBlue,
            context,
            start_line,
            is_from_me,
        ),
        Content::Audio(media) => media_card_lines(
            "Voice note",
            media,
            Color::LightCyan,
            context,
            start_line,
            is_from_me,
        ),
        Content::File(media) => media_card_lines(
            "File",
            media,
            Color::Yellow,
            context,
            start_line,
            is_from_me,
        ),
        Content::Sticker(media) => media_card_lines(
            "Sticker",
            media,
            Color::Magenta,
            context,
            start_line,
            is_from_me,
        ),
        Content::LinkPreview(link) => {
            let title = link.title.as_deref().unwrap_or("Link preview");
            let description = link
                .description
                .as_deref()
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            let mut lines = text_bubble_lines(
                &format!("LINK PREVIEW: {title} — {}{description}", link.url),
                context.content_width,
                context.theme,
                is_from_me,
            );
            if let Some(image) = &link.image {
                lines.extend(media_card_lines(
                    "Link image",
                    image,
                    Color::LightBlue,
                    context,
                    start_line + lines.len(),
                    is_from_me,
                ));
            }
            lines
        }
        Content::Poll(poll) => {
            poll_bubble_lines(poll, context.content_width, context.theme, is_from_me)
        }
        Content::Deleted => text_bubble_lines(
            "[deleted]",
            context.content_width,
            context.theme,
            is_from_me,
        ),
        Content::Unsupported(kind) => text_bubble_lines(
            &format!("[unsupported: {kind}]"),
            context.content_width,
            context.theme,
            is_from_me,
        ),
    }
}

fn poll_bubble_lines(
    poll: &chat_core::Poll,
    content_width: u16,
    theme: Theme,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    text_bubble_lines(&poll_text(poll), content_width, theme, is_from_me)
}

fn poll_text(poll: &chat_core::Poll) -> String {
    let mut text = format!("POLL: {}", poll.question);
    for (index, option) in poll.options.iter().enumerate() {
        text.push('\n');
        text.push_str(&format!("{}. {}", index + 1, option));
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

fn text_bubble_lines(
    text: &str,
    content_width: u16,
    theme: Theme,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let accent = bubble_accent(theme, is_from_me);
    let max_inner_width = bubble_inner_width(content_width);
    let wrapped = wrap_text(text, max_inner_width);
    let inner_width = wrapped
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or_default()
        .max(BUBBLE_MIN_WIDTH.saturating_sub(4))
        .min(max_inner_width.max(1));

    let mut lines = Vec::with_capacity(wrapped.len() + 2);
    lines.push(bubble_border_line('╭', '─', '╮', inner_width, accent));
    for line in wrapped {
        lines.push(bubble_text_line(&line, inner_width, accent));
    }
    lines.push(bubble_border_line('╰', '─', '╯', inner_width, accent));
    lines
}

fn media_card_lines(
    label: &str,
    media: &chat_core::Media,
    accent: Color,
    context: &mut MessageRenderContext<'_>,
    start_line: usize,
    is_from_me: bool,
) -> Vec<Line<'static>> {
    let card_width = media_card_width(context.content_width);
    let accent = media_card_accent(accent);
    let (preview_rows, source, error) = media_preview_rows(
        media,
        context.media_cache,
        card_width,
        MEDIA_PREVIEW_ROWS,
        accent,
    );
    let mut lines = vec![
        card_border_line('╭', '─', '╮', card_width, accent),
        card_text_line(
            accent,
            label,
            card_width,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
    ];

    if let (Some(path), None) = (&source, &error) {
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

fn media_preview_rows(
    media: &chat_core::Media,
    media_cache: &mut MediaPreviewCache,
    width: u16,
    rows: u16,
    accent: Color,
) -> (Vec<Vec<Span<'static>>>, Option<PathBuf>, Option<String>) {
    let Some(source) = media_preview_source(media) else {
        return (
            fallback_preview_rows(width, rows, accent, "no local image"),
            None,
            None,
        );
    };

    let key = MediaPreviewKey {
        path: source.clone(),
        width,
        rows,
    };
    let decode_path = source.clone();
    let cached = media_cache
        .previews
        .entry(key)
        .or_insert_with(|| decode_image_preview_rows(&decode_path, width, rows));

    match cached {
        Ok(rows) => (rows.clone(), Some(source), None),
        Err(error) => (
            fallback_preview_rows(width, rows, accent, "image decode failed"),
            Some(source),
            Some(error.clone()),
        ),
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

fn card_preview_line(accent: Color, preview: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let content_width = preview
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let mut spans = Vec::with_capacity(preview.len() + 3);
    spans.push(Span::styled("│ ", Style::default().fg(accent)));
    spans.extend(preview);
    spans.push(Span::raw(
        " ".repeat((width as usize).saturating_sub(content_width)),
    ));
    spans.push(Span::styled(" │", Style::default().fg(accent)));
    Line::from(spans)
}

fn card_text_line(accent: Color, text: &str, width: u16, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│ ", Style::default().fg(accent)),
        Span::styled(fit_cell_text(text, width), style),
        Span::styled(" │", Style::default().fg(accent)),
    ])
}

fn card_border_line(
    left: char,
    fill: char,
    right: char,
    width: u16,
    accent: Color,
) -> Line<'static> {
    let border = format!(
        "{left}{}{right}",
        std::iter::repeat_n(fill, width as usize + 2).collect::<String>()
    );
    Line::from(Span::styled(border, Style::default().fg(accent)))
}

fn media_card_accent(_accent: Color) -> Color {
    Color::DarkGray
}

fn media_card_width(content_width: u16) -> u16 {
    bubble_inner_width(content_width)
        .min(MEDIA_PREVIEW_MAX_WIDTH as usize)
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

fn bubble_text_line(text: &str, inner_width: usize, accent: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled("│ ", accent),
        Span::raw(fit_cell_text(text, inner_width as u16)),
        Span::styled(" │", accent),
    ])
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
    for (index, reaction) in message.reactions.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::raw(format!(
            "{}{}",
            reaction.emoji,
            reaction.senders.len()
        )));
    }
    if message.is_from_me {
        spans.push(Span::raw("  "));
    }
    Line::from(spans)
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
    media_cache: &mut MediaPreviewCache,
) -> Vec<Span<'static>> {
    sender
        .avatar
        .as_deref()
        .filter(|path| path.exists())
        .and_then(|path| {
            cached_image_preview_rows(path, media_cache, MESSAGE_AVATAR_WIDTH, MESSAGE_AVATAR_ROWS)
                .ok()
                .and_then(|rows| rows.into_iter().next())
        })
        .unwrap_or_else(|| {
            vec![Span::styled(
                avatar_label(sender),
                Style::default().fg(Color::Cyan),
            )]
        })
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

fn message_lines_len(message: &Message, grouped: bool, content_width: u16) -> usize {
    usize::from(!grouped)
        + usize::from(message.reply_to.is_some())
        + content_lines_len(&message.content, content_width)
        + usize::from(message.is_from_me || !receipt_summary(message).is_empty())
        + usize::from(!message.reactions.is_empty())
        + 1
}

fn content_lines_len(content: &Content, content_width: u16) -> usize {
    match content {
        Content::Text(text) => text_bubble_line_count(text, content_width),
        Content::Deleted => text_bubble_line_count("[deleted]", content_width),
        Content::Poll(poll) => text_bubble_line_count(&poll_text(poll), content_width),
        Content::Image(media)
        | Content::Video(media)
        | Content::Audio(media)
        | Content::File(media)
        | Content::Sticker(media) => media_card_line_count(media),
        Content::LinkPreview(link) => {
            let title = link.title.as_deref().unwrap_or("Link preview");
            let description = link
                .description
                .as_deref()
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            text_bubble_line_count(
                &format!("LINK PREVIEW: {title} — {}{description}", link.url),
                content_width,
            ) + link
                .image
                .as_ref()
                .map(media_card_line_count)
                .unwrap_or_default()
        }
        Content::Unsupported(kind) => {
            text_bubble_line_count(&format!("[unsupported: {kind}]"), content_width)
        }
    }
}

fn text_bubble_line_count(text: &str, content_width: u16) -> usize {
    wrap_text(text, bubble_inner_width(content_width)).len() + 2
}

fn media_card_line_count(media: &chat_core::Media) -> usize {
    4 + MEDIA_PREVIEW_ROWS as usize + usize::from(media.caption.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::PlatformData;
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

        let render = build_message_lines(&messages, 80, 0, 200, None, &mut cache, Theme::default());
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

        assert!(rendered.contains("First outgoing"));
        assert!(rendered.contains("Grouped outgoing"));
        assert!(rendered.contains("04:11"));
        assert!(rendered.contains("04:12"));
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
            &mut cache,
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
            &mut cache,
            Theme::default(),
        );
        let outgoing_selected =
            rendered_line_containing(&outgoing_render.lines, "Outgoing selected");
        assert!(outgoing_selected.ends_with(" ▕"));
        assert!(!outgoing_selected.starts_with("▏ "));
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
        let render = build_message_lines(&[message], 80, 0, 40, None, &mut cache, Theme::default());
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
        let render = build_message_lines(&messages, 80, 0, 8, None, &mut cache, Theme::default());

        assert!(render.total_lines > render.lines.len());
        assert!(cache.previews.len() < messages.len());
        assert!(!render.lines.is_empty());
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
