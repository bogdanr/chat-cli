use crate::{theme::Theme, widgets::message_list};
use chat_core::{
    Account, Chat, ChatId, ChatKind, ChatMembership, DiscoveryAction, DiscoveryResult,
    DiscoveryResultKind, Platform, ProviderId,
};
use chrono::{Local, NaiveDateTime};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
};
use std::collections::{HashMap, HashSet};
use storage::ChatInboxStyle;
use unicode_width::UnicodeWidthStr;

const CHAT_ROW_HEIGHT: u16 = 2;
const CHAT_META_WIDTH: usize = 6;
const CHAT_RIGHT_PADDING: usize = 1;
const SELECTED_CHAT_BG: Color = Color::Rgb(0, 48, 48);
pub const CHAT_AVATAR_WIDTH: u16 = 4;
pub const CHAT_AVATAR_ROWS: u16 = CHAT_ROW_HEIGHT;
pub const ACCOUNT_BADGE_WIDTH: u16 = 2;
pub const ACCOUNT_BADGE_ROWS: u16 = 1;
pub type AvatarRows = Vec<Vec<Span<'static>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatListRow {
    Chat { chat_index: usize },
    Section { title: String },
}

pub struct ChatListProps<'a> {
    pub chats: &'a [Chat],
    pub visible_chat_indices: &'a [usize],
    pub selected_chat_index: usize,
    pub filter: &'a str,
    pub filter_mode: bool,
    pub discovery_results: &'a [DiscoveryResult],
    pub account_filter: &'a str,
    pub inbox_style: ChatInboxStyle,
    pub focused: bool,
    pub layout: &'a ChatListLayout,
    pub avatar_rows: &'a HashMap<usize, AvatarRows>,
    pub account_badge_rows: &'a HashMap<ProviderId, AvatarRows>,
    pub typing_previews: &'a HashMap<usize, String>,
    /// Aggregated unread thread-reply counts per chat id, used to render the
    /// `⤷N` thread-activity marker distinct from the channel unread badge.
    pub thread_unread_by_chat: &'a HashMap<ChatId, u32>,
    pub theme: Theme,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatListLayout {
    pub rows: Vec<ChatListRow>,
    pub visible_rows: Vec<ChatListRow>,
    pub content_height: usize,
    pub scroll_position: usize,
}

impl ChatListLayout {
    pub fn rendered_chat_indices(&self) -> Vec<usize> {
        self.visible_rows
            .iter()
            .filter_map(|row| match row {
                ChatListRow::Chat { chat_index } => Some(*chat_index),
                ChatListRow::Section { .. } => None,
            })
            .collect()
    }
}

pub fn render_chat_list(frame: &mut Frame<'_>, area: Rect, props: ChatListProps<'_>) {
    let rows = &props.layout.visible_rows;
    let inner_width = inner_area(area).width as usize;
    let mut items = rows
        .iter()
        .map(|row| match row {
            ChatListRow::Chat { chat_index } => chat_item(
                &props.chats[*chat_index],
                props.avatar_rows.get(chat_index).map(Vec::as_slice),
                props
                    .account_badge_rows
                    .get(&props.chats[*chat_index].account)
                    .map(Vec::as_slice),
                props.typing_previews.get(chat_index).map(String::as_str),
                props
                    .thread_unread_by_chat
                    .get(&props.chats[*chat_index].id)
                    .copied()
                    .unwrap_or(0),
                props.theme,
                inner_width,
                *chat_index == props.selected_chat_index,
            ),
            ChatListRow::Section { title } => section_item(title),
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        if props.filter.is_empty() || props.discovery_results.is_empty() {
            items.push(empty_state_item(props.filter, props.account_filter));
        } else {
            items.extend(
                props
                    .discovery_results
                    .iter()
                    .map(|result| discovery_item(result, props.theme, inner_width)),
            );
        }
    }
    let mut state = ListState::default();
    state.select(
        selected_row_position(rows, props.selected_chat_index)
            .or_else(|| (!items.is_empty()).then_some(0)),
    );

    let list = List::new(items)
        .block(
            Block::default()
                .title(title(
                    props.filter,
                    props.filter_mode,
                    props.account_filter,
                    props.visible_chat_indices.len(),
                    props.inbox_style,
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

pub fn build_layout(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    inbox_style: ChatInboxStyle,
) -> ChatListLayout {
    let rows = build_rows(chats, visible_chat_indices, inbox_style);
    let content_height = rows.iter().map(|row| row_height(row) as usize).sum();
    let inner = inner_area(list_area);
    if inner.height == 0 {
        return ChatListLayout {
            rows,
            visible_rows: Vec::new(),
            content_height,
            scroll_position: 0,
        };
    }

    let selected_row = selected_row_position(&rows, selected_chat_index);
    let offset = scroll_offset(&rows, selected_row, inner.height as usize);
    let scroll_position = rows
        .iter()
        .take(offset)
        .map(|row| row_height(row) as usize)
        .sum();
    let visible_rows = visible_rows_from_offset(&rows, offset, inner.height as usize);

    ChatListLayout {
        rows,
        visible_rows,
        content_height,
        scroll_position,
    }
}

#[cfg(test)]
fn visible_rows(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    inbox_style: ChatInboxStyle,
) -> Vec<ChatListRow> {
    build_layout(
        chats,
        visible_chat_indices,
        selected_chat_index,
        list_area,
        inbox_style,
    )
    .visible_rows
}

pub fn build_rows(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
) -> Vec<ChatListRow> {
    if inbox_style == ChatInboxStyle::RecentFlat {
        return visible_chat_indices
            .iter()
            .copied()
            .map(|chat_index| ChatListRow::Chat { chat_index })
            .collect();
    }

    let use_recent_fallback =
        use_recent_activity_fallback(chats, visible_chat_indices, inbox_style);
    let ordered_indices = ordered_chat_indices(chats, visible_chat_indices, inbox_style);
    let mut seen_sections = HashSet::new();
    let mut rows = Vec::with_capacity(ordered_indices.len() + 8);

    for chat_index in ordered_indices {
        let section = chat_section(&chats[chat_index], inbox_style, use_recent_fallback);
        if seen_sections.insert(section.to_owned()) {
            rows.push(ChatListRow::Section {
                title: section.to_owned(),
            });
        }
        rows.push(ChatListRow::Chat { chat_index });
    }

    rows
}

pub fn ordered_chat_indices(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
) -> Vec<usize> {
    if inbox_style == ChatInboxStyle::RecentFlat {
        return visible_chat_indices.to_vec();
    }

    let mut ordered_indices = Vec::with_capacity(visible_chat_indices.len());

    for section in section_titles(chats, visible_chat_indices, inbox_style) {
        ordered_indices.extend(section_chat_indices(
            chats,
            visible_chat_indices,
            inbox_style,
            &section,
        ));
    }

    ordered_indices
}

fn section_chat_indices(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
    section: &str,
) -> Vec<usize> {
    let use_recent_fallback =
        use_recent_activity_fallback(chats, visible_chat_indices, inbox_style);
    visible_chat_indices
        .iter()
        .copied()
        .filter(|chat_index| {
            chat_section(&chats[*chat_index], inbox_style, use_recent_fallback) == section
        })
        .collect()
}

fn visible_rows_from_offset(
    rows: &[ChatListRow],
    offset: usize,
    list_height: usize,
) -> Vec<ChatListRow> {
    if list_height == 0 {
        return Vec::new();
    }
    let mut used_height = 0;

    rows.iter()
        .skip(offset)
        .take_while(|row| {
            if used_height >= list_height {
                return false;
            }
            used_height += row_height(row) as usize;
            true
        })
        .cloned()
        .collect()
}

fn first_visible_row_offset(
    rows: &[ChatListRow],
    selected_chat_index: usize,
    list_height: usize,
) -> usize {
    let selected_row = selected_row_position(rows, selected_chat_index);
    scroll_offset(rows, selected_row, list_height)
}

fn visible_rows_for_rows(
    rows: &[ChatListRow],
    selected_chat_index: usize,
    list_height: usize,
) -> Vec<ChatListRow> {
    let offset = first_visible_row_offset(rows, selected_chat_index, list_height);
    visible_rows_from_offset(rows, offset, list_height)
}

pub fn row_at(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    column: u16,
    row: u16,
    inbox_style: ChatInboxStyle,
) -> Option<ChatListRow> {
    let inner = inner_area(list_area);
    if !contains(inner, column, row) {
        return None;
    }

    let rows = build_rows(chats, visible_chat_indices, inbox_style);
    let offset = first_visible_row_offset(&rows, selected_chat_index, inner.height as usize);
    let relative_row = row.saturating_sub(inner.y);
    let mut y = 0;

    for chat_row in rows.iter().skip(offset) {
        let height = row_height(chat_row);
        if relative_row < y + height {
            return Some(chat_row.clone());
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
    inbox_style: ChatInboxStyle,
) -> Option<usize> {
    match row_at(
        chats,
        visible_chat_indices,
        selected_chat_index,
        list_area,
        column,
        row,
        inbox_style,
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

pub fn account_badge_column_bounds(list_area: Rect) -> (u16, u16) {
    let inner = inner_area(list_area);
    let start = inner.x.saturating_add(CHAT_AVATAR_WIDTH).saturating_add(1);
    (start, start.saturating_add(ACCOUNT_BADGE_WIDTH))
}

pub fn account_badge_chat_at(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    column: u16,
    row: u16,
    inbox_style: ChatInboxStyle,
) -> Option<usize> {
    let (badge_start, badge_end) = account_badge_column_bounds(list_area);
    if column < badge_start || column >= badge_end {
        return None;
    }

    let inner = inner_area(list_area);
    if !contains(inner, column, row) {
        return None;
    }

    let rows = build_rows(chats, visible_chat_indices, inbox_style);
    let offset = first_visible_row_offset(&rows, selected_chat_index, inner.height as usize);
    let relative_row = row.saturating_sub(inner.y);
    let mut y = 0;

    for chat_row in rows.iter().skip(offset) {
        let height = row_height(chat_row);
        if relative_row < y + height {
            return match chat_row {
                ChatListRow::Chat { chat_index } => Some(*chat_index),
                _ => None,
            };
        }
        y += height;
        if y >= inner.height {
            break;
        }
    }

    None
}

pub fn rendered_chat_indices(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    inbox_style: ChatInboxStyle,
) -> Vec<usize> {
    let inner = inner_area(list_area);
    if inner.height == 0 {
        return Vec::new();
    }

    let rows = build_rows(chats, visible_chat_indices, inbox_style);
    visible_rows_for_rows(&rows, selected_chat_index, inner.height as usize)
        .into_iter()
        .filter_map(|row| match row {
            ChatListRow::Chat { chat_index } => Some(chat_index),
            ChatListRow::Section { .. } => None,
        })
        .collect()
}

pub fn selected_visible_position(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    inbox_style: ChatInboxStyle,
) -> Option<usize> {
    ordered_chat_indices(chats, visible_chat_indices, inbox_style)
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

#[allow(clippy::too_many_arguments)]
fn chat_item(
    chat: &Chat,
    avatar_rows: Option<&[Vec<Span<'static>>]>,
    account_badge_rows: Option<&[Vec<Span<'static>>]>,
    typing_preview: Option<&str>,
    thread_unread: u32,
    theme: Theme,
    row_width: usize,
    selected: bool,
) -> ListItem<'static> {
    let has_unread = chat.unread_count > 0;
    let unread_marker = unread_marker(chat.unread_count);
    let pinned_marker = if chat.pinned { " [P]" } else { "" };
    let muted_marker = if chat.muted { " [M]" } else { "" };
    let name_style = if has_unread {
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
    } else if has_unread {
        theme.unread()
    } else {
        Style::default().fg(theme.muted)
    };
    let meta_style = style_with_optional_bg(Style::default().fg(theme.muted), selected_bg);
    let unread_style = style_with_optional_bg(theme.unread(), selected_bg);
    let effective_width = row_width.saturating_sub(CHAT_RIGHT_PADDING);
    let meta_width = CHAT_META_WIDTH.min(effective_width.saturating_sub(1));
    let content_width = effective_width.saturating_sub(meta_width);
    let timestamp = formatted_time(chat);
    let timestamp_padding = meta_width.saturating_sub(UnicodeWidthStr::width(timestamp.as_str()));
    let unread_padding = meta_width.saturating_sub(UnicodeWidthStr::width(unread_marker.as_str()));

    let first_badge_line = account_badge_rows.and_then(|rows| rows.first()).cloned();
    let badge_width = first_badge_line
        .as_ref()
        .map(|line| spans_width(line))
        .unwrap_or_else(|| platform_badge(&chat.platform).len());
    let first_prefix_width =
        CHAT_AVATAR_WIDTH as usize + 1 + badge_width + pinned_marker.len() + muted_marker.len() + 1;
    let name_budget = content_width.saturating_sub(first_prefix_width);
    let name = truncate_to_width(&chat.name, name_budget);
    let used_first_width = first_prefix_width + UnicodeWidthStr::width(name.as_str());
    let first_gap = effective_width
        .saturating_sub(meta_width)
        .saturating_sub(used_first_width);

    let preview = typing_preview
        .map(str::to_owned)
        .or_else(|| {
            chat.last_message_preview
                .as_deref()
                .map(message_list::slack_emoji_shortcodes_to_display)
        })
        .unwrap_or_else(|| "No messages yet".to_owned());
    let second_prefix_width = CHAT_AVATAR_WIDTH as usize + 1;
    let thread_marker = if thread_unread > 0 {
        format!("\u{2937}{} ", thread_unread.min(99))
    } else {
        String::new()
    };
    let thread_marker_width = UnicodeWidthStr::width(thread_marker.as_str());
    let preview_budget = content_width
        .saturating_sub(second_prefix_width)
        .saturating_sub(thread_marker_width);
    let preview = truncate_to_width(&preview, preview_budget);
    let used_second_width =
        second_prefix_width + thread_marker_width + UnicodeWidthStr::width(preview.as_str());
    let second_gap = effective_width
        .saturating_sub(meta_width)
        .saturating_sub(used_second_width);

    ListItem::new(vec![
        Line::from({
            let mut spans = first_avatar_line;
            spans.extend([styled_raw(" ", selected_bg)]);
            if let Some(badge_line) = first_badge_line.clone() {
                spans.extend(badge_line);
            } else {
                spans.extend([Span::styled(
                    platform_badge(&chat.platform),
                    style_with_optional_bg(platform_style(&chat.platform), selected_bg),
                )]);
            }
            spans.extend([
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
            spans.extend([styled_raw(" ", selected_bg)]);
            if !thread_marker.is_empty() {
                spans.push(Span::styled(
                    thread_marker,
                    style_with_optional_bg(theme.unread(), selected_bg),
                ));
            }
            spans.extend([
                Span::styled(preview, selected_message_style),
                styled_raw(" ".repeat(second_gap + unread_padding), selected_bg),
                Span::styled(unread_marker, unread_style),
                styled_raw(" ".repeat(CHAT_RIGHT_PADDING), selected_bg),
            ]);
            spans
        }),
    ])
}

fn section_item(title: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("── {title} ──"),
        Style::default().fg(Color::DarkGray),
    )))
}

const ACTIVITY_FIRST_SECTION_ORDER: [&str; 9] = [
    "Today",
    "Yesterday",
    "Recent",
    "Earlier This Week",
    "Groups & Channels",
    "People",
    "Muted",
    "Browse Channels",
    "Other Chats",
];
const PEOPLE_FIRST_SECTION_ORDER: [&str; 7] = [
    "Today",
    "Yesterday",
    "People",
    "Groups & Channels",
    "Muted",
    "Browse Channels",
    "Other Chats",
];
const GROUPS_FIRST_SECTION_ORDER: [&str; 7] = [
    "Today",
    "Yesterday",
    "Groups & Channels",
    "People",
    "Muted",
    "Browse Channels",
    "Other Chats",
];

fn section_titles(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
) -> Vec<String> {
    let use_recent_fallback =
        use_recent_activity_fallback(chats, visible_chat_indices, inbox_style);
    match inbox_style {
        ChatInboxStyle::ActivityFirst => ACTIVITY_FIRST_SECTION_ORDER
            .iter()
            .filter(|section| use_recent_fallback || **section != "Recent")
            .filter(|section| !use_recent_fallback || **section != "Earlier This Week")
            .map(|section| (*section).to_owned())
            .collect(),
        ChatInboxStyle::PeopleFirst => PEOPLE_FIRST_SECTION_ORDER
            .iter()
            .map(|section| (*section).to_owned())
            .collect(),
        ChatInboxStyle::GroupsFirst => GROUPS_FIRST_SECTION_ORDER
            .iter()
            .map(|section| (*section).to_owned())
            .collect(),
        ChatInboxStyle::AccountSeparated => {
            let mut sections = Vec::new();
            for chat_index in visible_chat_indices {
                let section = chat_section(&chats[*chat_index], inbox_style, use_recent_fallback);
                if !sections.iter().any(|existing| existing == &section) {
                    sections.push(section);
                }
            }
            sections
        }
        ChatInboxStyle::RecentFlat => Vec::new(),
    }
}

fn use_recent_activity_fallback(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
) -> bool {
    inbox_style == ChatInboxStyle::ActivityFirst
        && !visible_chat_indices.iter().any(|chat_index| {
            let chat = &chats[*chat_index];
            chat.membership != ChatMembership::NotJoined
                && !(chat.muted && chat.unread_count == 0)
                && matches!(
                    activity_date_section(chat, false, false),
                    Some("Today" | "Yesterday")
                )
        })
}

fn chat_section(chat: &Chat, inbox_style: ChatInboxStyle, use_recent_fallback: bool) -> String {
    match inbox_style {
        ChatInboxStyle::ActivityFirst => {
            activity_first_chat_section(chat, use_recent_fallback).to_owned()
        }
        ChatInboxStyle::PeopleFirst => people_first_chat_section(chat).to_owned(),
        ChatInboxStyle::GroupsFirst => groups_first_chat_section(chat).to_owned(),
        ChatInboxStyle::AccountSeparated => format!("Account: {}", chat.account),
        ChatInboxStyle::RecentFlat => String::new(),
    }
}

fn activity_first_chat_section(chat: &Chat, use_recent_fallback: bool) -> &'static str {
    if chat.membership == ChatMembership::NotJoined {
        return "Browse Channels";
    }
    if chat.muted && chat.unread_count == 0 {
        return "Muted";
    }
    if let Some(section) = activity_date_section(chat, true, use_recent_fallback) {
        return section;
    }
    type_browse_section(chat)
}

fn people_first_chat_section(chat: &Chat) -> &'static str {
    if chat.membership == ChatMembership::NotJoined {
        return "Browse Channels";
    }
    if chat.muted && chat.unread_count == 0 {
        return "Muted";
    }
    if let Some(section) = activity_date_section(chat, false, false) {
        return section;
    }
    if is_direct_chat(chat) {
        "People"
    } else {
        type_browse_section(chat)
    }
}

fn groups_first_chat_section(chat: &Chat) -> &'static str {
    if chat.membership == ChatMembership::NotJoined {
        return "Browse Channels";
    }
    if chat.muted && chat.unread_count == 0 {
        return "Muted";
    }
    if let Some(section) = activity_date_section(chat, false, false) {
        return section;
    }
    if is_shared_space(chat) {
        "Groups & Channels"
    } else {
        type_browse_section(chat)
    }
}

fn activity_date_section(
    chat: &Chat,
    include_this_week: bool,
    use_recent_fallback: bool,
) -> Option<&'static str> {
    let message_at = chat.last_message_at?.with_timezone(&Local).naive_local();
    activity_date_section_from_datetimes(
        message_at,
        Local::now().naive_local(),
        include_this_week,
        use_recent_fallback,
    )
}

fn activity_date_section_from_datetimes(
    message_at: NaiveDateTime,
    now: NaiveDateTime,
    include_this_week: bool,
    use_recent_fallback: bool,
) -> Option<&'static str> {
    let days_ago = now
        .date()
        .signed_duration_since(message_at.date())
        .num_days();
    match days_ago {
        i64::MIN..=0 => Some("Today"),
        1 => Some("Yesterday"),
        2..=6 if include_this_week && use_recent_fallback => Some("Recent"),
        2..=6 if include_this_week => Some("Earlier This Week"),
        _ => None,
    }
}

fn type_browse_section(chat: &Chat) -> &'static str {
    if is_shared_space(chat) {
        "Groups & Channels"
    } else if is_direct_chat(chat) {
        "People"
    } else {
        "Other Chats"
    }
}

fn is_shared_space(chat: &Chat) -> bool {
    matches!(
        chat.kind,
        ChatKind::Group
            | ChatKind::PublicChannel
            | ChatKind::PrivateChannel
            | ChatKind::GroupDirectMessage
    ) || chat.is_group
}

fn is_direct_chat(chat: &Chat) -> bool {
    matches!(chat.kind, ChatKind::Direct) && !chat.is_group
}

fn discovery_item(result: &DiscoveryResult, theme: Theme, row_width: usize) -> ListItem<'static> {
    let effective_width = row_width.saturating_sub(CHAT_RIGHT_PADDING);
    let kind = discovery_kind_label(result);
    let action = discovery_action_label(result.action);
    let label_prefix_width = platform_badge(&result.platform).len() + kind.len() + 3;
    let action_width = UnicodeWidthStr::width(action);
    let label_budget = effective_width
        .saturating_sub(label_prefix_width)
        .saturating_sub(action_width)
        .saturating_sub(1);
    let label = truncate_to_width(result.label.as_ref(), label_budget);
    let used_first_width =
        label_prefix_width + UnicodeWidthStr::width(label.as_str()) + action_width;
    let first_gap = effective_width.saturating_sub(used_first_width);

    let subtitle = result
        .subtitle
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| discovery_default_subtitle(result).to_owned());
    let subtitle = truncate_to_width(&subtitle, effective_width.saturating_sub(2));

    ListItem::new(vec![
        Line::from(vec![
            Span::styled(
                platform_badge(&result.platform),
                platform_style(&result.platform),
            ),
            Span::raw(" "),
            Span::styled(kind, Style::default().fg(theme.accent)),
            Span::raw(" "),
            Span::styled(label, Style::default()),
            Span::raw(" ".repeat(first_gap)),
            Span::styled(action.to_owned(), Style::default().fg(theme.muted)),
            Span::raw(" ".repeat(CHAT_RIGHT_PADDING)),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(subtitle, Style::default().fg(theme.muted)),
            Span::raw(" ".repeat(CHAT_RIGHT_PADDING)),
        ]),
    ])
}

fn discovery_kind_label(result: &DiscoveryResult) -> &'static str {
    match result.kind {
        DiscoveryResultKind::ExistingChat => "chat",
        DiscoveryResultKind::Contact => "contact",
        DiscoveryResultKind::User => "user",
        DiscoveryResultKind::DirectMessage => "dm",
        DiscoveryResultKind::PublicChannel => "channel",
        DiscoveryResultKind::PrivateChannel => "private",
        DiscoveryResultKind::Group => "group",
        DiscoveryResultKind::ManualDestination => "manual",
    }
}

fn discovery_action_label(action: DiscoveryAction) -> &'static str {
    match action {
        DiscoveryAction::Open => "open",
        DiscoveryAction::CreateChat => "start",
        DiscoveryAction::OpenDm => "dm",
        DiscoveryAction::JoinRequired => "join required",
        DiscoveryAction::Unsupported => "unavailable",
    }
}

fn discovery_default_subtitle(result: &DiscoveryResult) -> &'static str {
    match result.action {
        DiscoveryAction::JoinRequired => "Discoverable public channel; join before opening",
        DiscoveryAction::CreateChat => "Not in the chat list yet; open to start a chat",
        DiscoveryAction::OpenDm => "Open or create a direct message",
        DiscoveryAction::Unsupported => "This destination cannot be opened yet",
        DiscoveryAction::Open => match result.membership {
            ChatMembership::NotJoined => "Not joined",
            ChatMembership::Joined => "Existing destination",
            ChatMembership::Unknown => "Destination",
        },
    }
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

pub fn account_badge_placeholder(account: &Account, _theme: Theme) -> AvatarRows {
    match account.platform {
        Platform::WhatsApp => two_cell_icon(
            Color::White,
            Color::Rgb(37, 211, 102),
            Color::Rgb(37, 211, 102),
            Color::White,
        ),
        Platform::Slack => two_cell_icon(
            Color::Rgb(46, 182, 125),
            Color::Rgb(54, 197, 240),
            Color::Rgb(236, 178, 46),
            Color::Rgb(224, 30, 90),
        ),
        Platform::Discord => two_cell_icon(
            Color::Rgb(88, 101, 242),
            Color::Rgb(88, 101, 242),
            Color::White,
            Color::Rgb(88, 101, 242),
        ),
        Platform::Unknown(_) => {
            let color = stable_badge_color(&account.id, &account.display_name);
            two_cell_icon(color, color, color, color)
        }
    }
}

fn two_cell_icon(
    top_left: Color,
    bottom_left: Color,
    top_right: Color,
    bottom_right: Color,
) -> AvatarRows {
    vec![vec![
        Span::styled("▀", Style::default().fg(top_left).bg(bottom_left)),
        Span::styled("▀", Style::default().fg(top_right).bg(bottom_right)),
    ]]
}

fn stable_badge_color(id: &str, display_name: &str) -> Color {
    const PALETTE: [Color; 12] = [
        Color::Rgb(97, 31, 105),
        Color::Rgb(54, 88, 153),
        Color::Rgb(18, 140, 126),
        Color::Rgb(203, 75, 22),
        Color::Rgb(133, 92, 197),
        Color::Rgb(176, 48, 96),
        Color::Rgb(42, 124, 111),
        Color::Rgb(189, 95, 27),
        Color::Rgb(80, 112, 60),
        Color::Rgb(122, 85, 46),
        Color::Rgb(48, 105, 152),
        Color::Rgb(154, 68, 82),
    ];
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in id.bytes().chain(display_name.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PALETTE[(hash as usize) % PALETTE.len()]
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

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
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

fn title(
    filter: &str,
    filter_mode: bool,
    account_filter: &str,
    visible_count: usize,
    inbox_style: ChatInboxStyle,
) -> String {
    let style = format!(" · {}", inbox_style_title(inbox_style));
    let account = if account_filter == "All accounts" {
        String::new()
    } else {
        format!(" · Account: {account_filter}")
    };
    if filter_mode {
        format!("Chats{style}{account} · Filter: {filter}")
    } else if filter.is_empty() {
        format!("Chats{style}{account}")
    } else {
        format!("Chats ({visible_count}){style}{account} · Filter: {filter}")
    }
}

fn inbox_style_title(inbox_style: ChatInboxStyle) -> &'static str {
    match inbox_style {
        ChatInboxStyle::ActivityFirst => "Activity first",
        ChatInboxStyle::RecentFlat => "Recent flat",
        ChatInboxStyle::PeopleFirst => "People first",
        ChatInboxStyle::GroupsFirst => "Groups & channels first",
        ChatInboxStyle::AccountSeparated => "Account separated",
    }
}

fn formatted_time(chat: &Chat) -> String {
    chat.last_message_at
        .map(|timestamp| timestamp.with_timezone(&Local).format("%H:%M").to_string())
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
        ChatListRow::Section { .. } => 1,
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

pub fn content_height(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    inbox_style: ChatInboxStyle,
) -> usize {
    build_rows(chats, visible_chat_indices, inbox_style)
        .iter()
        .map(|row| row_height(row) as usize)
        .sum()
}

pub fn scroll_position(
    chats: &[Chat],
    visible_chat_indices: &[usize],
    selected_chat_index: usize,
    list_area: Rect,
    inbox_style: ChatInboxStyle,
) -> usize {
    let inner = inner_area(list_area);
    if inner.height == 0 {
        return 0;
    }

    let rows = build_rows(chats, visible_chat_indices, inbox_style);
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
    use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
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
    fn activity_date_sections_use_calendar_day_buckets() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 7).expect("valid date");
        let now = noon(today);

        assert_eq!(
            activity_date_section_from_datetimes(noon(today), now, true, false),
            Some("Today")
        );
        assert_eq!(
            activity_date_section_from_datetimes(
                noon(NaiveDate::from_ymd_opt(2026, 6, 6).expect("valid date")),
                now,
                true,
                false,
            ),
            Some("Yesterday")
        );
        assert_eq!(
            activity_date_section_from_datetimes(
                noon(NaiveDate::from_ymd_opt(2026, 6, 5).expect("valid date")),
                now,
                true,
                false,
            ),
            Some("Earlier This Week")
        );
        assert_eq!(
            activity_date_section_from_datetimes(
                noon(NaiveDate::from_ymd_opt(2026, 6, 5).expect("valid date")),
                now,
                false,
                false,
            ),
            None
        );
    }

    #[test]
    fn activity_date_sections_do_not_treat_two_calendar_days_ago_as_yesterday() {
        let now = NaiveDate::from_ymd_opt(2026, 6, 7)
            .expect("valid date")
            .and_hms_opt(5, 45, 0)
            .expect("valid time");
        let late_two_local_days_ago = NaiveDate::from_ymd_opt(2026, 6, 5)
            .expect("valid date")
            .and_hms_opt(22, 48, 0)
            .expect("valid time");

        assert_eq!(
            activity_date_section_from_datetimes(late_two_local_days_ago, now, true, false),
            Some("Earlier This Week")
        );
        assert_eq!(
            activity_date_section_from_datetimes(late_two_local_days_ago, now, false, false),
            None
        );
    }

    #[test]
    fn activity_first_uses_recent_fallback_when_today_and_yesterday_are_empty() {
        let mut chats = sample_chats();
        let now = Utc::now();
        chats[0].last_message_at = Some(now - Duration::days(3));
        chats[1].last_message_at = Some(now - Duration::days(4));

        let rows = build_rows(&chats, &[0, 1], ChatInboxStyle::ActivityFirst);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Section {
                    title: "Recent".to_owned()
                },
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Chat { chat_index: 1 },
            ]
        );
    }

    #[test]
    fn activity_first_uses_earlier_this_week_when_today_or_yesterday_exist() {
        let mut chats = sample_chats();
        let now = Utc::now();
        chats[0].last_message_at = Some(now - Duration::days(1));
        chats[1].last_message_at = Some(now - Duration::days(3));

        let rows = build_rows(&chats, &[0, 1], ChatInboxStyle::ActivityFirst);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Section {
                    title: "Yesterday".to_owned()
                },
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Section {
                    title: "Earlier This Week".to_owned()
                },
                ChatListRow::Chat { chat_index: 1 },
            ]
        );
    }

    #[test]
    fn rows_group_chats_activity_first_by_default() {
        let mut chats = sample_chats();
        let now = Utc::now();
        chats[0].last_message_at = Some(now);
        chats[1].last_message_at = Some(now - Duration::days(1));
        chats[2].muted = false;
        chats[2].unread_count = 0;
        chats[2].last_message_at = Some(now - Duration::days(8));
        chats[3].last_message_at = Some(now - Duration::days(9));
        chats[4].last_message_at = Some(now - Duration::days(3));

        let rows = build_rows(&chats, &[0, 1, 4, 2, 3], ChatInboxStyle::ActivityFirst);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Section {
                    title: "Today".to_owned()
                },
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Section {
                    title: "Yesterday".to_owned()
                },
                ChatListRow::Chat { chat_index: 1 },
                ChatListRow::Section {
                    title: "Earlier This Week".to_owned()
                },
                ChatListRow::Chat { chat_index: 4 },
                ChatListRow::Section {
                    title: "Groups & Channels".to_owned()
                },
                ChatListRow::Chat { chat_index: 2 },
                ChatListRow::Section {
                    title: "People".to_owned()
                },
                ChatListRow::Chat { chat_index: 3 },
            ]
        );
        assert_eq!(selected_row_position(&rows, 2), Some(7));
    }

    #[test]
    fn rows_can_prioritize_people_after_recent_activity() {
        let mut chats = sample_chats();
        let now = Utc::now();
        chats[0].last_message_at = Some(now);
        chats[1].last_message_at = Some(now - Duration::days(8));
        chats[2].muted = false;
        chats[2].unread_count = 0;
        chats[2].last_message_at = Some(now - Duration::days(9));
        chats[3].last_message_at = Some(now - Duration::days(10));

        let rows = build_rows(&chats, &[0, 1, 2, 3], ChatInboxStyle::PeopleFirst);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Section {
                    title: "Today".to_owned()
                },
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Section {
                    title: "People".to_owned()
                },
                ChatListRow::Chat { chat_index: 3 },
                ChatListRow::Section {
                    title: "Groups & Channels".to_owned()
                },
                ChatListRow::Chat { chat_index: 1 },
                ChatListRow::Chat { chat_index: 2 },
            ]
        );
    }

    #[test]
    fn recent_flat_omits_section_headers() {
        let chats = sample_chats();
        let rows = build_rows(&chats, &[1, 4, 3], ChatInboxStyle::RecentFlat);

        assert_eq!(
            rows,
            vec![
                ChatListRow::Chat { chat_index: 1 },
                ChatListRow::Chat { chat_index: 4 },
                ChatListRow::Chat { chat_index: 3 },
            ]
        );
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
    fn unread_chat_item_keeps_standard_alignment_and_badge_text() {
        assert_eq!(unread_marker(2), "2");
    }

    #[test]
    fn read_chat_item_has_no_unread_badge_text() {
        assert_eq!(unread_marker(0), "");
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
    fn visible_rows_are_limited_to_viewport_height() {
        let chats = sample_chats();
        let visible = [0, 1, 2, 3, 4];
        let area = Rect::new(0, 0, 40, 6);

        let rows = visible_rows(&chats, &visible, 0, area, ChatInboxStyle::RecentFlat);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows,
            vec![
                ChatListRow::Chat { chat_index: 0 },
                ChatListRow::Chat { chat_index: 1 },
            ]
        );
    }

    #[test]
    fn visible_rows_scroll_to_keep_selected_chat_visible() {
        let chats = sample_chats();
        let visible = [0, 1, 2, 3, 4];
        let area = Rect::new(0, 0, 40, 6);

        let rows = visible_rows(&chats, &visible, 4, area, ChatInboxStyle::RecentFlat);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows,
            vec![
                ChatListRow::Chat { chat_index: 3 },
                ChatListRow::Chat { chat_index: 4 },
            ]
        );
    }

    #[test]
    fn rendered_chat_indices_only_returns_rows_that_fit() {
        let chats = sample_chats();
        let area = Rect::new(0, 0, 40, 4);

        assert_eq!(
            rendered_chat_indices(&chats, &[0, 1, 2], 0, area, ChatInboxStyle::ActivityFirst),
            vec![0]
        );
        assert_eq!(
            rendered_chat_indices(&chats, &[0, 1, 2], 2, area, ChatInboxStyle::ActivityFirst),
            vec![2]
        );
    }

    #[test]
    fn row_at_maps_pointer_position_to_visible_chat() {
        let mut chats = sample_chats();
        let base = Utc::now();
        for (index, chat) in chats.iter_mut().enumerate() {
            chat.last_message_at = Some(base - Duration::minutes(index as i64));
            chat.muted = false;
        }
        let area = Rect::new(0, 0, 40, 8);

        assert_eq!(
            chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                2,
                1,
                ChatInboxStyle::ActivityFirst
            ),
            None
        );
        assert_eq!(
            chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                2,
                2,
                ChatInboxStyle::ActivityFirst
            ),
            Some(0)
        );
        assert_eq!(
            chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                2,
                4,
                ChatInboxStyle::ActivityFirst
            ),
            Some(1)
        );
        assert_eq!(
            chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                2,
                6,
                ChatInboxStyle::ActivityFirst
            ),
            Some(2)
        );
    }

    #[test]
    fn account_badge_hit_test_accepts_full_visible_chat_item() {
        let mut chats = sample_chats();
        let base = Utc::now();
        for (index, chat) in chats.iter_mut().enumerate() {
            chat.last_message_at = Some(base - Duration::minutes(index as i64));
            chat.muted = false;
        }
        let area = Rect::new(0, 0, 40, 8);
        let (badge_start, _) = account_badge_column_bounds(area);

        assert_eq!(
            account_badge_chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                badge_start,
                2,
                ChatInboxStyle::ActivityFirst
            ),
            Some(0)
        );
        assert_eq!(
            account_badge_chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                badge_start,
                3,
                ChatInboxStyle::ActivityFirst
            ),
            Some(0)
        );
        assert_eq!(
            account_badge_chat_at(
                &chats,
                &[0, 1, 2],
                0,
                area,
                badge_start.saturating_sub(1),
                2,
                ChatInboxStyle::ActivityFirst
            ),
            None
        );
    }

    #[test]
    fn discovery_result_labels_make_destination_actions_clear() {
        let result = DiscoveryResult {
            account: arc_str("slack:test"),
            platform: Platform::Slack,
            kind: DiscoveryResultKind::PublicChannel,
            action: DiscoveryAction::JoinRequired,
            id: arc_str("slack:destination:slack:test:Cnew"),
            platform_id: arc_str("Cnew"),
            chat_id: Some(arc_str("Cnew")),
            label: arc_str("#new-public-channel"),
            subtitle: None,
            avatar: None,
            chat_kind: Some(ChatKind::PublicChannel),
            membership: ChatMembership::NotJoined,
            metadata: Default::default(),
        };

        assert_eq!(discovery_kind_label(&result), "channel");
        assert_eq!(discovery_action_label(result.action), "join required");
        assert_eq!(
            discovery_default_subtitle(&result),
            "Discoverable public channel; join before opening"
        );
    }

    #[test]
    fn account_badge_placeholder_generates_whatsapp_icon_tile() {
        let account = Account {
            id: arc_str("whatsapp:bridge"),
            platform: Platform::WhatsApp,
            display_name: arc_str("WhatsApp"),
            avatar: None,
        };

        let rows = account_badge_placeholder(&account, Theme::default());

        assert_eq!(rows.len(), ACCOUNT_BADGE_ROWS as usize);
        assert_eq!(rows[0].len(), ACCOUNT_BADGE_WIDTH as usize);
        assert_eq!(
            rows[0]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "▀▀"
        );
    }

    #[test]
    fn account_badge_placeholder_uses_slack_icon_colors() {
        let account = Account {
            id: arc_str("slack:workspace-1"),
            platform: Platform::Slack,
            display_name: arc_str("Slack (erepublik.com)"),
            avatar: None,
        };

        let rows = account_badge_placeholder(&account, Theme::default());

        assert_eq!(rows.len(), ACCOUNT_BADGE_ROWS as usize);
        assert_eq!(rows[0].len(), ACCOUNT_BADGE_WIDTH as usize);
        assert_eq!(
            rows[0]
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "▀▀"
        );
        assert_eq!(rows[0][0].style.fg, Some(Color::Rgb(46, 182, 125)));
        assert_eq!(rows[0][0].style.bg, Some(Color::Rgb(54, 197, 240)));
        assert_eq!(rows[0][1].style.fg, Some(Color::Rgb(236, 178, 46)));
        assert_eq!(rows[0][1].style.bg, Some(Color::Rgb(224, 30, 90)));
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
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
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
                kind: ChatKind::PublicChannel,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(now - Duration::minutes(5)),
                last_message_preview: Some(arc_str("Build passed")),
                thread_id: None,
            },
            Chat {
                id: arc_str("media"),
                account: account.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Media Samples"),
                avatar: None,
                is_group: true,
                kind: ChatKind::Group,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 1,
                muted: true,
                pinned: false,
                last_message_at: Some(now - Duration::hours(1)),
                last_message_preview: Some(arc_str("Screenshot")),
                thread_id: None,
            },
            Chat {
                id: arc_str("direct"),
                account: account.clone(),
                platform: Platform::Slack,
                name: arc_str("Dana Example"),
                avatar: None,
                is_group: false,
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(now - Duration::minutes(10)),
                last_message_preview: Some(arc_str("DM preview")),
                thread_id: None,
            },
            Chat {
                id: arc_str("design"),
                account,
                platform: Platform::Slack,
                name: arc_str("#design"),
                avatar: None,
                is_group: true,
                kind: ChatKind::PublicChannel,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(now - Duration::minutes(15)),
                last_message_preview: Some(arc_str("Design notes")),
                thread_id: None,
            },
        ]
    }

    fn noon(date: NaiveDate) -> NaiveDateTime {
        date.and_hms_opt(12, 0, 0).expect("valid noon timestamp")
    }

    fn arc_str(value: impl AsRef<str>) -> Arc<str> {
        Arc::from(value.as_ref())
    }
}
