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

const CHAT_ROW_HEIGHT: u16 = 2;
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
    let mut items = rows
        .iter()
        .map(|row| match row {
            ChatListRow::Chat { chat_index } => chat_item(
                &props.chats[*chat_index],
                props.avatar_rows.get(chat_index).map(Vec::as_slice),
                props.theme,
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
        .highlight_style(Style::default().bg(Color::DarkGray))
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

fn chat_item(
    chat: &Chat,
    avatar_rows: Option<&[Vec<Span<'static>>]>,
    theme: Theme,
) -> ListItem<'static> {
    let unread_marker = if chat.unread_count > 0 {
        format!("●{} ", chat.unread_count)
    } else {
        "   ".to_owned()
    };
    let pinned_marker = if chat.pinned { " [P]" } else { "" };
    let muted_marker = if chat.muted { " [M]" } else { "" };
    let name_style = if chat.unread_count > 0 {
        theme.unread()
    } else {
        Style::default()
    };
    let avatar_fallback = avatar_label(chat);
    let first_avatar_line = avatar_rows
        .and_then(|rows| rows.first().cloned())
        .unwrap_or_else(|| {
            vec![Span::styled(
                avatar_fallback.clone(),
                Style::default().fg(Color::Cyan),
            )]
        });
    let second_avatar_line = avatar_rows
        .and_then(|rows| rows.get(1).cloned())
        .unwrap_or_else(|| vec![Span::styled("    ", Style::default().fg(Color::Cyan))]);

    ListItem::new(vec![
        Line::from({
            let mut spans = vec![Span::styled(unread_marker, theme.unread())];
            spans.extend(first_avatar_line);
            spans.extend([
                Span::raw(" "),
                Span::styled(
                    platform_badge(&chat.platform),
                    platform_style(&chat.platform),
                ),
                Span::raw(format!("{pinned_marker}{muted_marker} ")),
                Span::styled(chat.name.to_string(), name_style),
                Span::raw(format!(" {}", formatted_time(chat))),
            ]);
            spans
        }),
        Line::from({
            let mut spans = vec![Span::raw("   ")];
            spans.extend(second_avatar_line);
            spans.extend([
                Span::raw(" "),
                Span::styled(
                    chat.last_message_preview
                        .as_deref()
                        .unwrap_or("No messages yet")
                        .to_owned(),
                    Style::default().fg(Color::DarkGray),
                ),
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
        " ?? ".to_owned()
    } else {
        format!(" {initials:<2} ")
    }
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
