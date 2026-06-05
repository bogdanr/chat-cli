use crate::theme::Theme;
use chat_core::{Chat, Platform};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

const CHAT_ROW_HEIGHT: u16 = 2;
const CHAT_META_WIDTH: usize = 6;
const CHAT_RIGHT_PADDING: usize = 1;
const WHATSAPP_GREEN: Color = Color::Rgb(37, 211, 102);
const SELECTED_CHAT_BG: Color = Color::Rgb(0, 48, 48);
pub const CHAT_AVATAR_WIDTH: u16 = 4;
pub const CHAT_AVATAR_ROWS: u16 = CHAT_ROW_HEIGHT;
pub type AvatarRows = Vec<Vec<Span<'static>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatListRow {
    Chat { chat_index: usize },
    Separator,
}

pub struct ChatListProps<'a> {
    pub chats: &'a [Chat],
    pub visible_chat_indices: &'a [usize],
    pub selected_chat_index: usize,
    pub filter: &'a str,
    pub filter_mode: bool,
    pub account_filter: &'a str,
    pub focused: bool,
    pub avatar_rows: &'a HashMap<usize, AvatarRows>,
    pub theme: Theme,
}

pub fn render_chat_list(frame: &mut Frame<'_>, area: Rect, props: ChatListProps<'_>) {
    let rows = build_rows(props.chats, props.visible_chat_indices);
    let inner_width = inner_area(area).width as usize;
    let mut items = rows
        .iter()
        .map(|row| match row {
            ChatListRow::Chat { chat_index } => chat_item(
                &props.chats[*chat_index],
                props.avatar_rows.get(chat_index).map(Vec::as_slice),
                props.theme,
                inner_width,
                *chat_index == props.selected_chat_index,
            ),
            ChatListRow::Separator => separator_item(),
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        items.push(empty_state_item(props.filter, props.account_filter));
    }
    let mut state = ListState::default();
    state.select(selected_row_position(&rows, props.selected_chat_index));

    let list = List::new(items)
        .block(
            Block::default()
                .title(title(
                    props.filter,
                    props.filter_mode,
                    props.account_filter,
                    props.visible_chat_indices.len(),
                ))
                .borders(Borders::ALL)
                .border_style(props.theme.focus_border(props.focused)),
        )
        .highlight_style(Style::default())
        .highlight_symbol("");
    frame.render_stateful_widget(list, area, &mut state);
}

pub fn filter_chat_indices(chats: &[Chat], filter: &str) -> Vec<usize> {
    let terms = filter
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return (0..chats.len()).collect();
    }

    chats
        .iter()
        .enumerate()
        .filter_map(|(index, chat)| chat_matches_terms(chat, &terms).then_some(index))
        .collect()
}

pub fn build_rows(chats: &[Chat], visible_chat_indices: &[usize]) -> Vec<ChatListRow> {
    let mut rows = Vec::with_capacity(visible_chat_indices.len() + 1);
    let mut separator_inserted = false;
    let has_pinned = visible_chat_indices
        .iter()
        .any(|index| chats[*index].pinned);

    for &chat_index in visible_chat_indices {
        if has_pinned && !separator_inserted && !chats[chat_index].pinned {
            rows.push(ChatListRow::Separator);
            separator_inserted = true;
        }
        rows.push(ChatListRow::Chat { chat_index });
    }

    rows
}

pub fn row_at(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    column: u16,
    row: u16,
) -> Option<ChatListRow> {
    let inner = inner_area(list_area);
    if !contains(inner, column, row) {
        return None;
    }

    let rows = build_rows(chats, visible_chat_indices);
    let selected_row = selected_row_position(&rows, selected_chat_index);
    let offset = scroll_offset(&rows, selected_row, inner.height as usize);
    let relative_row = row.saturating_sub(inner.y);
    let mut y = 0;

    for chat_row in rows.into_iter().skip(offset) {
        let height = row_height(&chat_row);
        if relative_row < y + height {
            return Some(chat_row);
        }
        y += height;
        if y >= inner.height {
            break;
        }
    }

    None
}

pub fn chat_at(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    column: u16,
    row: u16,
) -> Option<usize> {
    match row_at(
        chats,
        visible_chat_indices,
        selected_chat_index,
        list_area,
        column,
        row,
    ) {
        Some(ChatListRow::Chat { chat_index }) => Some(chat_index),
        _ => None,
    }
}

pub fn avatar_column_bounds(list_area: Rect) -> (u16, u16) {
    let inner = inner_area(list_area);
    let start = inner.x;
    (start, start.saturating_add(CHAT_AVATAR_WIDTH))
}

pub fn rendered_chat_indices(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
) -> Vec<usize> {
    let inner = inner_area(list_area);
    if inner.height == 0 {
        return Vec::new();
    }

    let rows = build_rows(chats, visible_chat_indices);
    let selected_row = selected_row_position(&rows, selected_chat_index);
    let offset = scroll_offset(&rows, selected_row, inner.height as usize);
    let mut used_height = 0;

    rows.into_iter()
        .skip(offset)
        .take_while(|row| {
            if used_height >= inner.height {
                return false;
            }
            used_height += row_height(row);
            true
        })
        .filter_map(|row| match row {
            ChatListRow::Chat { chat_index } => Some(chat_index),
            ChatListRow::Separator => None,
        })
        .collect()
}

pub fn selected_visible_position(
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
) -> Option<usize> {
    visible_chat_indices
        .iter()
        .position(|index| *index == selected_chat_index)
}

pub fn selected_row_position(rows: &[ChatListRow], selected_chat_index: usize) -> Option<usize> {
    rows.iter().position(|row| {
        matches!(
            row,
            ChatListRow::Chat { chat_index } if *chat_index == selected_chat_index
        )
    })
}

fn chat_matches_terms(chat: &Chat, terms: &[String]) -> bool {
    let haystack = format!(
        "{} {} {} {}",
        chat.name,
        chat.last_message_preview.as_deref().unwrap_or_default(),
        platform_badge(&chat.platform),
        platform_name(&chat.platform),
    )
    .to_lowercase();
    terms.iter().all(|term| haystack.contains(term))
}

fn unread_marker(unread_count: u32) -> String {
    if unread_count > 0 {
        unread_count.min(99).to_string()
    } else {
        String::new()
    }
}

fn chat_item(
    chat: &Chat,
    avatar_rows: Option<&[Vec<Span<'static>>]>,
    theme: Theme,
    row_width: usize,
    selected: bool,
) -> ListItem<'static> {
    let unread_marker = unread_marker(chat.unread_count);
    let pinned_marker = if chat.pinned { " [P]" } else { "" };
    let muted_marker = if chat.muted { " [M]" } else { "" };
    let name_style = if chat.unread_count > 0 {
        theme.unread()
    } else {
        Style::default()
    };
    let fallback_avatar = avatar_placeholder(chat, theme);
    let first_avatar_line = avatar_rows
        .and_then(|rows| rows.first().cloned())
        .unwrap_or_else(|| fallback_avatar[0].clone());
    let second_avatar_line = avatar_rows
        .and_then(|rows| rows.get(1).cloned())
        .unwrap_or_else(|| fallback_avatar[1].clone());

    let selected_bg = selected.then_some(SELECTED_CHAT_BG);
    let selected_message_style = if selected {
        Style::default().fg(theme.accent).bg(SELECTED_CHAT_BG)
    } else {
        Style::default().fg(theme.muted)
    };
    let meta_style = style_with_optional_bg(Style::default().fg(theme.muted), selected_bg);
    let unread_style = style_with_optional_bg(unread_style(), selected_bg);
    let effective_width = row_width.saturating_sub(CHAT_RIGHT_PADDING);
    let meta_width = CHAT_META_WIDTH.min(effective_width.saturating_sub(1));
    let content_width = effective_width.saturating_sub(meta_width);
    let timestamp = formatted_time(chat);
    let timestamp_padding = meta_width.saturating_sub(UnicodeWidthStr::width(timestamp.as_str()));
    let unread_padding = meta_width.saturating_sub(UnicodeWidthStr::width(unread_marker.as_str()));

    let first_prefix_width = CHAT_AVATAR_WIDTH as usize
        + 1
        + platform_badge(&chat.platform).len()
        + pinned_marker.len()
        + muted_marker.len()
        + 1;
    let name_budget = content_width.saturating_sub(first_prefix_width);
    let name = truncate_to_width(&chat.name, name_budget);
    let used_first_width = first_prefix_width + UnicodeWidthStr::width(name.as_str());
    let first_gap = effective_width
        .saturating_sub(meta_width)
        .saturating_sub(used_first_width);

    let preview = chat
        .last_message_preview
        .as_deref()
        .unwrap_or("No messages yet");
    let second_prefix_width = CHAT_AVATAR_WIDTH as usize + 1;
    let preview_budget = content_width.saturating_sub(second_prefix_width);
    let preview = truncate_to_width(preview, preview_budget);
    let used_second_width = second_prefix_width + UnicodeWidthStr::width(preview.as_str());
    let second_gap = effective_width
        .saturating_sub(meta_width)
        .saturating_sub(used_second_width);

    ListItem::new(vec![
        Line::from({
            let mut spans = first_avatar_line;
            spans.extend([
                styled_raw(" ", selected_bg),
                Span::styled(
                    platform_badge(&chat.platform),
                    style_with_optional_bg(platform_style(&chat.platform), selected_bg),
                ),
                styled_raw(format!("{pinned_marker}{muted_marker} "), selected_bg),
                Span::styled(name, style_with_optional_bg(name_style, selected_bg)),
                styled_raw(" ".repeat(first_gap + timestamp_padding), selected_bg),
                Span::styled(timestamp, meta_style),
                styled_raw(" ".repeat(CHAT_RIGHT_PADDING), selected_bg),
            ]);
            spans
        }),
        Line::from({
            let mut spans = second_avatar_line;
            spans.extend([
                styled_raw(" ", selected_bg),
                Span::styled(preview, selected_message_style),
                styled_raw(" ".repeat(second_gap + unread_padding), selected_bg),
                Span::styled(unread_marker, unread_style),
                styled_raw(" ".repeat(CHAT_RIGHT_PADDING), selected_bg),
            ]);
            spans
        }),
    ])
}

fn separator_item() -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        "── Unpinned ──",
        Style::default().fg(Color::DarkGray),
    )))
}

fn empty_state_item(filter: &str, account_filter: &str) -> ListItem<'static> {
    let mut lines = match (filter.is_empty(), account_filter == "All accounts") {
        (true, true) => vec![Line::from(Span::styled(
            "No chats yet",
            Style::default().fg(Color::DarkGray),
        ))],
        (true, false) => vec![
            Line::from(Span::styled(
                "No chats for this account",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                format!("Account: {account_filter}"),
                Style::default().fg(Color::DarkGray),
            )),
        ],
        (false, true) => vec![Line::from(Span::styled(
            format!("No chats match '{filter}'"),
            Style::default().fg(Color::DarkGray),
        ))],
        (false, false) => vec![
            Line::from(Span::styled(
                format!("No chats match '{filter}'"),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                format!("Account: {account_filter}"),
                Style::default().fg(Color::DarkGray),
            )),
        ],
    };
    lines.push(Line::from(Span::styled(
        "Esc clears filter · Ctrl+A changes account",
        Style::default().fg(Color::DarkGray),
    )));
    ListItem::new(lines)
}

fn avatar_label(chat: &Chat) -> String {
    let initials = chat
        .name
        .split(|value: char| value.is_whitespace() || matches!(value, '#' | '-' | '_'))
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

fn avatar_placeholder(chat: &Chat, theme: Theme) -> [Vec<Span<'static>>; 2] {
    let label = avatar_label(chat);
    let tile_style = Style::default().fg(theme.foreground).bg(avatar_color(chat));

    [
        vec![Span::styled("    ", tile_style)],
        vec![Span::styled(format!("{label:^4}"), tile_style)],
    ]
}

fn avatar_color(chat: &Chat) -> Color {
    match chat.platform {
        Platform::WhatsApp => Color::Rgb(18, 140, 126),
        Platform::Slack => Color::Magenta,
        Platform::Discord => Color::Blue,
        Platform::Unknown(_) => Color::DarkGray,
    }
}

fn unread_style() -> Style {
    Style::default().fg(WHATSAPP_GREEN)
}

fn style_with_optional_bg(style: Style, bg: Option<Color>) -> Style {
    if let Some(bg) = bg {
        style.bg(bg)
    } else {
        style
    }
}

fn styled_raw(
    value: impl Into<std::borrow::Cow<'static, str>>,
    bg: Option<Color>,
) -> Span<'static> {
    Span::styled(value, style_with_optional_bg(Style::default(), bg))
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    const ELLIPSIS: &str = "…";

    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return ELLIPSIS.to_owned();
    }

    let mut output = String::new();
    let content_width = max_width - 1;
    for character in value.chars() {
        let next_width = UnicodeWidthStr::width(character.to_string().as_str());
        if UnicodeWidthStr::width(output.as_str()) + next_width > content_width {
            break;
        }
        output.push(character);
    }
    output.push_str(ELLIPSIS);
    output
}

fn title(filter: &str, filter_mode: bool, account_filter: &str, visible_count: usize) -> String {
    let account = if account_filter == "All accounts" {
        String::new()
    } else {
        format!(" · Account: {account_filter}")
    };
    if filter_mode {
        format!("Chats{account} · Filter: {filter}")
    } else if filter.is_empty() {
        format!("Chats{account}")
    } else {
        format!("Chats ({visible_count}){account} · Filter: {filter}")
    }
}

fn formatted_time(chat: &Chat) -> String {
    chat.last_message_at
        .map(|timestamp| timestamp.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_owned())
}

fn platform_badge(platform: &Platform) -> &'static str {
    match platform {
        Platform::WhatsApp => "[WA]",
        Platform::Slack => "[SL]",
        Platform::Discord => "[DI]",
        Platform::Unknown(_) => "[--]",
    }
}

fn platform_name(platform: &Platform) -> &str {
    match platform {
        Platform::WhatsApp => "whatsapp",
        Platform::Slack => "slack",
        Platform::Discord => "discord",
        Platform::Unknown(value) => value,
    }
}

fn scroll_offset(rows: &[ChatListRow], selected_row: Option<usize>, list_height: usize) -> usize {
    let Some(selected_row) = selected_row else {
        return 0;
    };
    if list_height == 0 {
        return 0;
    }

    let mut offset = 0;
    while offset < selected_row {
        let visible_height = rows[offset..=selected_row]
            .iter()
            .map(|row| row_height(row) as usize)
            .sum::<usize>();
        if visible_height <= list_height {
            break;
        }
        offset += 1;
    }
    offset
}

fn row_height(row: &ChatListRow) -> u16 {
    match row {
        ChatListRow::Chat { .. } => CHAT_ROW_HEIGHT,
        ChatListRow::Separator => 1,
    }
}

fn inner_area(area: Rect) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

pub fn content_height(chats: &[Chat], visible_chat_indices: &[usize]) -> usize {
    build_rows(chats, visible_chat_indices)
        .iter()
        .map(|row| row_height(row) as usize)
        .sum()
}

pub fn scroll_position(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
) -> usize {
    let inner = inner_area(list_area);
    if inner.height == 0 {
        return 0;
    }

    let rows = build_rows(chats, visible_chat_indices);
    let selected_row = selected_row_position(&rows, selected_chat_index);
    let offset = scroll_offset(&rows, selected_row, inner.height as usize);
    rows.iter()
        .take(offset)
        .map(|row| row_height(row) as usize)
        .sum()
}

fn platform_style(platform: &Platform) -> Style {
    match platform {
        Platform::WhatsApp => Style::default().fg(Color::Green),
        Platform::Slack => Style::default().fg(Color::Magenta),
        Platform::Discord => Style::default().fg(Color::Blue),
        Platform::Unknown(_) => Style::default().fg(Color::Cyan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use std::sync::Arc;

    #[test]
    fn filter_matches_chat_names_case_insensitively() {
        let chats = sample_chats();

        assert_eq!(filter_chat_indices(&chats, "PROJECT"), vec![1]);
        assert_eq!(filter_chat_indices(&chats, "media"), vec![2]);
        assert_eq!(filter_chat_indices(&chats, "missing"), Vec::<usize>::new());
    }

    #[test]
    fn filter_matches_preview_platform_and_multiple_terms() {
        let chats = sample_chats();

        assert_eq!(filter_chat_indices(&chats, "screenshot"), vec![2]);
        assert_eq!(filter_chat_indices(&chats, "slack build"), vec![1]);
        assert_eq!(filter_chat_indices(&chats, "wa alice"), vec![0]);
    }

    #[test]
    fn rows_insert_separator_between_pinned_and_unpinned_chats() {
        let chats = sample_chats();
        let rows = build_rows(&chats, &[0, 1, 2]);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Separator,
                ChatListRow::Chat { chat_index: 1 },
                ChatListRow::Chat { chat_index: 2 },
            ]
        );
        assert_eq!(selected_row_position(&rows, 2), Some(3));
    }

    #[test]
    fn unread_marker_shows_right_side_badge_text() {
        assert_eq!(unread_marker(0), "");
        assert_eq!(unread_marker(2), "2");
        assert_eq!(unread_marker(140), "99");
        assert!(!unread_marker(2).contains('•'));
        assert!(!unread_marker(2).contains('●'));
    }

    #[test]
    fn avatar_placeholder_uses_consistent_two_line_tile() {
        let chats = sample_chats();
        let placeholder = avatar_placeholder(&chats[0], Theme::default());

        assert_eq!(placeholder.len(), CHAT_AVATAR_ROWS as usize);
        assert_eq!(
            placeholder[0]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "    "
        );
        assert_eq!(
            placeholder[1]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            " AE "
        );
    }

    #[test]
    fn rendered_chat_indices_only_returns_rows_that_fit() {
        let chats = sample_chats();
        let area = Rect::new(0, 0, 40, 4);

        assert_eq!(rendered_chat_indices(&chats, &[0, 1, 2], 0, area), vec![0]);
        assert_eq!(rendered_chat_indices(&chats, &[0, 1, 2], 2, area), vec![2]);
    }

    #[test]
    fn row_at_maps_pointer_position_to_visible_chat() {
        let chats = sample_chats();
        let area = Rect::new(0, 0, 40, 8);

        assert_eq!(chat_at(&chats, &[0, 1, 2], 0, area, 2, 1), Some(0));
        assert_eq!(chat_at(&chats, &[0, 1, 2], 0, area, 2, 3), None);
        assert_eq!(chat_at(&chats, &[0, 1, 2], 0, area, 2, 4), Some(1));
        assert_eq!(chat_at(&chats, &[0, 1, 2], 0, area, 2, 6), Some(2));
    }

    fn sample_chats() -> Vec<Chat> {
        let account = arc_str("mock:local");
        let now = Utc::now();
        vec![
            Chat {
                id: arc_str("alice"),
                account: account.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Alice Example"),
                avatar: None,
                is_group: false,
                unread_count: 2,
                muted: false,
                pinned: true,
                last_message_at: Some(now),
                last_message_preview: Some(arc_str("Hello")),
                thread_id: None,
            },
            Chat {
                id: arc_str("project"),
                account: account.clone(),
                platform: Platform::Slack,
                name: arc_str("#project-chat-cli"),
                avatar: None,
                is_group: true,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(now - Duration::minutes(5)),
                last_message_preview: Some(arc_str("Build passed")),
                thread_id: None,
            },
            Chat {
                id: arc_str("media"),
                account,
                platform: Platform::WhatsApp,
                name: arc_str("Media Samples"),
                avatar: None,
                is_group: true,
                unread_count: 1,
                muted: true,
                pinned: false,
                last_message_at: Some(now - Duration::hours(1)),
                last_message_preview: Some(arc_str("Screenshot")),
                thread_id: None,
            },
        ]
    }

    fn arc_str(value: impl AsRef<str>) -> Arc<str> {
        Arc::from(value.as_ref())
    }
}
