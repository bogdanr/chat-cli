//! Member profile popup, presence labels, and activity statistics rendering
//! for the details pane.
//!
//! Everything here is pure: the app gathers cached state into a
//! [`ProfileView`] / [`ActivityView`] and this module turns it into styled
//! lines plus click targets. No IO, decoding, or storage access happens here,
//! so these helpers are safe to call from draw and measure paths.

use crate::{
    attach::format_byte_size,
    theme::Theme,
    widgets::message_list::{format_message_datetime, wrap_text},
};
use chat_core::{
    ChatId, ChatMember, ChatMemberRole, ContactProfile, Platform, PlatformId, Presence,
    PresenceState, PresenceSupport, ProviderId, Timestamp,
};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use storage::{
    ChatStats, ContactNote, LastMessageSeen, LinkReason, LinkedIdentity, SenderChatActivity,
    SharedItem,
};
use unicode_width::UnicodeWidthStr;

/// Avatar block shown at the top-left of the profile popup.
pub(crate) const PROFILE_AVATAR_WIDTH: u16 = 12;
pub(crate) const PROFILE_AVATAR_ROWS: u16 = 6;
/// Number of top posters listed in the Activity section.
pub(crate) const TOP_POSTERS: usize = 5;
/// Days covered by the activity sparkline.
pub(crate) const ACTIVITY_DAYS: u32 = 30;
/// Groups in common / shared items / linked identities listed in the popup.
pub(crate) const PROFILE_LIST_LIMIT: usize = 6;

/// Ordering of the Details member list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum MemberSort {
    /// Provider order (usually owners/admins first).
    #[default]
    Default,
    Name,
    Role,
    /// Most messages in local history first.
    Activity,
    /// Online first, then most recently seen.
    Presence,
}

impl MemberSort {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Default => Self::Name,
            Self::Name => Self::Role,
            Self::Role => Self::Activity,
            Self::Activity => Self::Presence,
            Self::Presence => Self::Default,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Name => "name",
            Self::Role => "role",
            Self::Activity => "activity",
            Self::Presence => "presence",
        }
    }
}

/// Which local note field is being edited in the popup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoteField {
    Nickname,
    Note,
}

impl NoteField {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Nickname => "nickname",
            Self::Note => "note",
        }
    }
}

/// In-progress edit of a local nickname/note.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NoteEdit {
    pub field: NoteField,
    pub buffer: String,
}

/// The open member profile popup. Identity fields are captured when opened
/// so the popup renders immediately, before any background load finishes.
#[derive(Clone, Debug)]
pub(crate) struct MemberProfilePopup {
    pub account: ProviderId,
    /// The chat the popup was opened from (scopes per-chat stats).
    pub chat_id: Option<ChatId>,
    pub chat_is_direct: bool,
    pub platform_id: PlatformId,
    pub display_name: Arc<str>,
    /// List thumbnail (the member's `Sender::avatar`).
    pub avatar: Option<PathBuf>,
    pub role: ChatMemberRole,
    pub is_self: bool,
    pub scroll: usize,
    pub editing: Option<NoteEdit>,
    pub opened_at: Instant,
}

/// Background-loaded extras for a member: groups in common, shared items,
/// linked identities, a direct chat, and local notes.
#[derive(Clone, Debug, Default)]
pub(crate) struct MemberExtras {
    pub groups: Vec<SenderChatActivity>,
    pub shared: Vec<SharedItem>,
    pub linked: Vec<LinkedIdentity>,
    pub note: ContactNote,
    pub direct_chat: Option<ChatId>,
}

/// What clicking a popup line does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PopupTarget {
    Avatar,
    OpenChat {
        account: ProviderId,
        chat_id: ChatId,
    },
    OpenFile(PathBuf),
    OpenUrl(Arc<str>),
    Copy(Arc<str>),
}

/// A clickable popup line (logical line index, before scrolling).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PopupHit {
    pub line: usize,
    pub target: PopupTarget,
}

/// Everything needed to render the profile popup, gathered from caches.
pub(crate) struct ProfileView<'a> {
    pub popup: &'a MemberProfilePopup,
    pub profile: Option<&'a ContactProfile>,
    pub profile_loading: bool,
    pub profile_error: Option<&'a str>,
    pub presence: Option<&'a Presence>,
    pub presence_support: PresenceSupport,
    pub show_activity: bool,
    pub stats: Option<&'a ChatStats>,
    pub stats_loading: bool,
    /// Newest local message from this person in any chat of the account.
    pub last_message: Option<&'a LastMessageSeen>,
    pub extras: Option<&'a MemberExtras>,
    pub extras_loading: bool,
    /// Half-block avatar rows (non-HD); `None` reserves blank space for an
    /// HD overlay or when nothing is cached yet.
    pub avatar_rows: Option<Vec<Vec<Span<'static>>>>,
    /// Display names for chats, used to label groups in common.
    pub chat_names: &'a HashMap<ChatId, Arc<str>>,
    pub now: Timestamp,
}

/// Key for per-member extras: the account, the member, and the chat the popup
/// was opened from (shared items are scoped to that chat).
pub(crate) type ExtrasKey = (ProviderId, PlatformId, Option<ChatId>);

/// Screen-independent position of one clickable member row in the Details
/// overview paragraph, recorded while building the lines.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DetailsMemberHit {
    /// First logical line of the row within the details paragraph.
    pub line_index: usize,
    pub rows: usize,
    pub platform_id: PlatformId,
    /// Whether columns `2..2+CHAT_AVATAR_WIDTH` show this member's avatar
    /// (clicking there enlarges it instead of opening the profile).
    pub has_avatar: bool,
    /// Member-list rows participate in the keyboard cursor; activity-bar
    /// rows only respond to clicks.
    pub is_member_row: bool,
}

/// Details-pane member, presence and activity state (all cache-only for the
/// draw path; filled by background completions).
#[derive(Default)]
pub(crate) struct MemberUiState {
    /// Clickable member rows from the last overview build.
    pub hits: Vec<DetailsMemberHit>,
    /// Keyboard cursor: the highlighted member (kept by id so re-sorting or
    /// filtering never moves it to a different person).
    pub cursor: Option<PlatformId>,
    pub sort: MemberSort,
    /// Member-list search query (`Some` while the search line is shown).
    pub search: Option<String>,
    pub search_editing: bool,
    pub popup: Option<MemberProfilePopup>,
    pub popup_hits: Vec<PopupHit>,
    /// Popup outer rect, inner text rect, and avatar rect from the last draw.
    pub popup_area: Rect,
    pub popup_content_area: Rect,
    pub popup_avatar_area: Rect,
    pub popup_line_count: usize,
    pub profile_errors: HashMap<(ProviderId, PlatformId), String>,
    pub presence: HashMap<(ProviderId, PlatformId), Presence>,
    pub loading_presence: HashSet<(ProviderId, PlatformId)>,
    pub presence_checked_at: HashMap<(ProviderId, PlatformId), Instant>,
    pub stats: HashMap<(ProviderId, ChatId), ChatStats>,
    pub stats_errors: HashMap<(ProviderId, ChatId), String>,
    pub loading_stats: HashSet<(ProviderId, ChatId)>,
    /// Chats whose stats are stale because new messages arrived.
    pub stats_dirty_at: HashMap<(ProviderId, ChatId), Instant>,
    pub extras: HashMap<ExtrasKey, MemberExtras>,
    pub loading_extras: HashSet<ExtrasKey>,
    /// Each person's newest local message across all chats of the account
    /// (the "last message X ago" fallback when presence hides last seen).
    pub last_message: HashMap<(ProviderId, PlatformId), LastMessageSeen>,
    pub loading_last_message: HashSet<(ProviderId, PlatformId)>,
    pub last_message_checked_at: HashMap<(ProviderId, PlatformId), Instant>,
}

impl std::fmt::Debug for MemberUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemberUiState")
            .field("hits", &self.hits.len())
            .field("cursor", &self.cursor)
            .field("sort", &self.sort)
            .field("search", &self.search)
            .field(
                "popup",
                &self.popup.as_ref().map(|popup| &popup.platform_id),
            )
            .field("presence", &self.presence.len())
            .field("stats", &self.stats.len())
            .finish_non_exhaustive()
    }
}

impl MemberUiState {
    /// Platform ids of the member rows (cursor order).
    pub(crate) fn member_rows(&self) -> impl Iterator<Item = &DetailsMemberHit> {
        self.hits.iter().filter(|hit| hit.is_member_row)
    }

    pub(crate) fn cursor_hit(&self) -> Option<&DetailsMemberHit> {
        let cursor = self.cursor.as_ref()?;
        self.member_rows().find(|hit| &hit.platform_id == cursor)
    }

    /// Moves the cursor by `delta` member rows (wrapping), starting at the
    /// first/last row when no cursor is set. Returns the new cursor row.
    pub(crate) fn move_cursor(&mut self, delta: isize) -> Option<DetailsMemberHit> {
        let rows: Vec<&DetailsMemberHit> = self.member_rows().collect();
        if rows.is_empty() {
            self.cursor = None;
            return None;
        }
        let len = rows.len() as isize;
        let current = self
            .cursor
            .as_ref()
            .and_then(|cursor| rows.iter().position(|hit| &hit.platform_id == cursor));
        let next = match current {
            Some(index) => (index as isize + delta).rem_euclid(len),
            None if delta >= 0 => 0,
            None => len - 1,
        } as usize;
        let hit = rows[next].clone();
        self.cursor = Some(hit.platform_id.clone());
        Some(hit)
    }
}

#[derive(Debug)]
pub(crate) struct ChatStatsFetchResult {
    pub account: ProviderId,
    pub chat_id: ChatId,
    pub result: Result<ChatStats, String>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub(crate) struct PresenceFetchResult {
    pub account: ProviderId,
    pub platform_id: PlatformId,
    pub result: Result<Option<Presence>, String>,
    pub elapsed: Duration,
}

/// Completion of one batched cross-chat "last message" lookup.
#[derive(Debug)]
pub(crate) struct LastMessageFetchResult {
    pub account: ProviderId,
    /// Every person requested in the batch (found or not).
    pub requested: Vec<PlatformId>,
    pub result: Result<HashMap<PlatformId, LastMessageSeen>, String>,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub(crate) struct MemberExtrasFetchResult {
    pub key: ExtrasKey,
    pub result: Result<MemberExtras, String>,
    pub elapsed: Duration,
}

/// "5m ago", "3h ago", "2d ago", or a date for anything older than a week.
pub(crate) fn format_relative(timestamp: Timestamp, now: Timestamp) -> String {
    let seconds = (now - timestamp).num_seconds();
    if seconds < 0 {
        return "just now".to_owned();
    }
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=604_799 => format!("{}d ago", seconds / 86_400),
        _ => format_message_datetime(timestamp),
    }
}

/// Compact duration such as "45s", "12m", "3h", "2d".
pub(crate) fn format_duration_short(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Human presence label, or `None` when there is nothing meaningful to say.
///
/// `last_message` is the caller-formatted "last message X ago" label. It
/// replaces "Last seen hidden" / a bare "Offline" when the provider gives no
/// last-seen time, so hidden presence still says when the person was active.
pub(crate) fn presence_label(
    presence: Option<&Presence>,
    support: PresenceSupport,
    now: Timestamp,
    last_message: Option<String>,
) -> Option<String> {
    match support {
        PresenceSupport::Unsupported => return None,
        PresenceSupport::Disabled => return Some("Presence disabled".to_owned()),
        PresenceSupport::Pushed | PresenceSupport::Polled => {}
    }
    let presence = presence?;
    Some(match presence.state {
        PresenceState::Online => "Online".to_owned(),
        PresenceState::Away => "Away".to_owned(),
        PresenceState::Offline => match (presence.last_seen, last_message) {
            (Some(seen), _) => format!("Last seen {}", format_relative(seen, now)),
            (None, Some(last_message)) => last_message,
            (None, None) if presence.last_seen_hidden => "Last seen hidden".to_owned(),
            (None, None) => "Offline".to_owned(),
        },
        PresenceState::Unknown => return None,
    })
}

/// The newer of a person's cross-chat last message (`anywhere`) and their
/// last message in the current chat (`here`, from the chat's stats, which is
/// usually cached first). Returns the time and, when it came from the
/// cross-chat lookup, the chat it was sent in.
pub(crate) fn newest_last_message(
    anywhere: Option<&LastMessageSeen>,
    here: Option<Timestamp>,
) -> Option<(Timestamp, Option<&LastMessageSeen>)> {
    match (anywhere, here) {
        (Some(seen), Some(here)) if here > seen.at => Some((here, None)),
        (Some(seen), _) => Some((seen.at, Some(seen))),
        (None, Some(here)) => Some((here, None)),
        (None, None) => None,
    }
}

/// Ids a person's messages may be stored under: the member id plus, for a
/// WhatsApp privacy `@lid` member whose phone number is known, the phone JID
/// used in their direct chat.
pub(crate) fn last_message_alias_ids(
    platform_id: &PlatformId,
    profile: Option<&ContactProfile>,
) -> Vec<PlatformId> {
    let mut ids = vec![platform_id.clone()];
    if platform_id.ends_with("@lid")
        && let Some(phone) = profile.and_then(|profile| profile.phone.as_deref())
    {
        let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            ids.push(Arc::from(format!("{digits}@s.whatsapp.net")));
        }
    }
    ids
}

/// Color for a presence dot.
pub(crate) fn presence_dot(presence: Option<&Presence>) -> Option<(char, Color)> {
    match presence?.state {
        PresenceState::Online => Some(('●', Color::Green)),
        PresenceState::Away => Some(('◐', Color::Yellow)),
        PresenceState::Offline => Some(('○', Color::DarkGray)),
        PresenceState::Unknown => None,
    }
}

/// `████░░░░` bar for a 0-100 percentage.
pub(crate) fn share_bar(percent: f64, width: usize) -> String {
    let width = width.max(1);
    let filled = ((percent.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Unicode sparkline (`▁▂▃▄▅▆▇█`) of the counts, scaled to the maximum.
pub(crate) fn sparkline(counts: &[u32]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = counts.iter().copied().max().unwrap_or(0);
    counts
        .iter()
        .map(|&count| {
            if max == 0 || count == 0 {
                ' '
            } else {
                let index = ((count as f64 / max as f64) * (BARS.len() - 1) as f64).round();
                BARS[(index as usize).min(BARS.len() - 1)]
            }
        })
        .collect()
}

pub(crate) fn hour_range_label(hour: u8) -> String {
    format!("{:02}:00–{:02}:00", hour, (hour + 1) % 24)
}

pub(crate) fn weekday_label(day: u8) -> &'static str {
    match day {
        0 => "Mon",
        1 => "Tue",
        2 => "Wed",
        3 => "Thu",
        4 => "Fri",
        5 => "Sat",
        _ => "Sun",
    }
}

fn platform_label(platform: &Platform) -> &str {
    match platform {
        Platform::WhatsApp => "WhatsApp",
        Platform::Slack => "Slack",
        Platform::ClickUp => "ClickUp",
        Platform::Discord => "Discord",
        Platform::Unknown(name) => name.as_str(),
    }
}

fn truncate(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w + 1 > max_width {
            break;
        }
        width += w;
        out.push(ch);
    }
    out.push('…');
    out
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Returns the indices of `members` that match `query` (case-insensitive
/// substring of the display name or platform id), in the requested order.
pub(crate) fn ordered_member_indices(
    members: &[ChatMember],
    query: &str,
    sort: MemberSort,
    stats: Option<&ChatStats>,
    presence: &dyn Fn(&PlatformId) -> Option<Presence>,
) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut indices: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, member)| {
            query.is_empty()
                || member.sender.display_name.to_lowercase().contains(&query)
                || member.sender.platform_id.to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect();
    let name = |index: usize| members[index].sender.display_name.to_lowercase();
    match sort {
        MemberSort::Default => {}
        MemberSort::Name => indices.sort_by_key(|&index| name(index)),
        MemberSort::Role => indices.sort_by_key(|&index| {
            let rank = match members[index].role {
                ChatMemberRole::Owner => 0,
                ChatMemberRole::Admin => 1,
                ChatMemberRole::Member => 2,
            };
            (rank, name(index))
        }),
        MemberSort::Activity => indices.sort_by_key(|&index| {
            let member = &members[index];
            let count = stats
                .and_then(|stats| stats.member(&member.sender.platform_id, member.is_self))
                .map(|(_, sender)| sender.activities)
                .unwrap_or(0);
            (std::cmp::Reverse(count), name(index))
        }),
        MemberSort::Presence => indices.sort_by_key(|&index| {
            let snapshot = presence(&members[index].sender.platform_id);
            let (rank, seen) = match snapshot.as_ref() {
                Some(p) if p.state == PresenceState::Online => (0, 0),
                Some(p) if p.state == PresenceState::Away => (1, 0),
                Some(p) => (
                    2,
                    -p.last_seen
                        .map(|seen| seen.timestamp())
                        .unwrap_or(i64::MIN / 2),
                ),
                None => (3, 0),
            };
            (rank, seen, name(index))
        }),
    }
    indices
}

/// Inputs for [`activity_lines`].
pub(crate) struct ActivityView<'a> {
    pub stats: Option<&'a ChatStats>,
    pub loading: bool,
    pub error: Option<&'a str>,
    /// Loaded member list (for the silent-members count), when any.
    pub members: Option<&'a [ChatMember]>,
    pub is_direct: bool,
    pub width: u16,
    pub now: Timestamp,
}

/// Builds the Details "Activity" section. Returns lines plus clickable rows
/// (line offset within the returned lines, sender id) for the top posters.
pub(crate) fn activity_lines(
    view: &ActivityView<'_>,
    theme: Theme,
) -> (Vec<Line<'static>>, Vec<(usize, PlatformId)>) {
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled("Activity", theme.pane_title())),
    ];
    let mut hits = Vec::new();
    let width = view.width as usize;
    let Some(stats) = view.stats else {
        let text = if let Some(error) = view.error {
            format!("  Stats unavailable: {error}")
        } else if view.loading {
            "  Computing activity…".to_owned()
        } else {
            "  No activity yet".to_owned()
        };
        lines.push(Line::from(Span::styled(
            truncate(&text, width),
            theme.muted(),
        )));
        return (lines, hits);
    };
    if stats.total_messages == 0 {
        lines.push(Line::from(Span::styled(
            "  No synced messages yet",
            theme.muted(),
        )));
        return (lines, hits);
    }
    let since = stats
        .oldest_at
        .map(|oldest| format!(" since {}", format_message_datetime(oldest)))
        .unwrap_or_default();
    lines.push(Line::from(Span::styled(
        truncate(
            &format!(
                "  Based on {} synced messages{since}",
                thousands(stats.total_messages)
            ),
            width,
        ),
        theme.muted(),
    )));
    if let Some(newest) = stats.newest_at {
        lines.push(Line::from(truncate(
            &format!("  Last message: {}", format_relative(newest, view.now)),
            width,
        )));
    }
    if !stats.daily_counts.is_empty() {
        let spark_width = width.saturating_sub(7).min(stats.daily_counts.len());
        let start = stats.daily_counts.len() - spark_width;
        lines.push(Line::from(vec![
            Span::raw(format!("  {:>2}d ", stats.daily_counts.len())),
            Span::styled(
                sparkline(&stats.daily_counts[start..]),
                Style::default().fg(theme.accent),
            ),
        ]));
    }
    let mut busiest = Vec::new();
    if let Some(day) = stats.busiest_weekday() {
        busiest.push(weekday_label(day).to_owned());
    }
    if let Some(hour) = stats.busiest_hour() {
        busiest.push(hour_range_label(hour));
    }
    if !busiest.is_empty() {
        lines.push(Line::from(truncate(
            &format!("  Busiest: {}", busiest.join(" · ")),
            width,
        )));
    }

    if view.is_direct {
        let mine = stats.senders.iter().find(|sender| sender.is_me);
        let theirs = stats.senders.iter().find(|sender| !sender.is_me);
        let label = |value: Option<i64>| {
            value
                .map(format_duration_short)
                .unwrap_or_else(|| "–".to_owned())
        };
        let me = label(mine.and_then(|s| s.median_reply_seconds));
        let them = label(theirs.and_then(|s| s.median_reply_seconds));
        if me != "–" || them != "–" {
            lines.push(Line::from(truncate(
                &format!("  Typical reply: you {me} · them {them}"),
                width,
            )));
        }
    }

    let posters: Vec<_> = stats
        .senders
        .iter()
        .filter(|sender| sender.messages > 0)
        .take(TOP_POSTERS)
        .collect();
    if !posters.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate("  Top posters (1-min bursts = 1)", width),
            theme.muted(),
        )));
        // "  " + name + " " + bar + " " + "100%"
        let bar_width = (width / 4).clamp(4, 10);
        let name_width = width.saturating_sub(2 + 1 + bar_width + 5).max(4);
        for sender in posters {
            let percent = stats.share_percent(sender.activities);
            let name = if sender.is_me {
                "You".to_owned()
            } else {
                sender.display_name.to_string()
            };
            let name = truncate(&name, name_width);
            let pad = name_width.saturating_sub(UnicodeWidthStr::width(name.as_str()));
            // The merged "me" entry has a synthetic key; click through to a
            // real id (our member record, else an id our messages used).
            let target = if sender.is_me {
                view.members
                    .and_then(|members| members.iter().find(|member| member.is_self))
                    .map(|member| member.sender.platform_id.clone())
                    .or_else(|| stats.my_sender_ids.first().cloned())
            } else {
                Some(sender.sender_id.clone())
            };
            if let Some(target) = target {
                hits.push((lines.len(), target));
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{name}{}", " ".repeat(pad)),
                    Style::default().fg(theme.foreground),
                ),
                Span::raw(" "),
                Span::styled(
                    share_bar(percent, bar_width),
                    Style::default().fg(theme.accent),
                ),
                Span::styled(format!(" {:>3.0}%", percent), theme.muted()),
            ]));
        }
    }
    if stats.my_messages > 0 && !view.is_direct {
        lines.push(Line::from(truncate(
            &format!(
                "  You: {:.0}% ({} messages)",
                stats.share_percent(stats.my_activities),
                thousands(stats.my_messages)
            ),
            width,
        )));
    }
    let media = &stats.media;
    let mut parts = Vec::new();
    for (count, label) in [
        (media.images, "photos"),
        (media.videos, "videos"),
        (media.audio, "audio"),
        (media.files, "files"),
        (media.stickers, "stickers"),
        (media.links, "links"),
        (media.polls, "polls"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    if !parts.is_empty() {
        let mut text = format!("  Media: {}", parts.join(" · "));
        if media.total_bytes > 0 {
            text.push_str(&format!(" ({})", format_byte_size(media.total_bytes)));
        }
        for line in wrap_text(&text, width.max(8)) {
            lines.push(Line::from(line));
        }
    }
    if !view.is_direct
        && let Some(members) = view.members
        && !members.is_empty()
    {
        let silent = members
            .iter()
            .filter(|member| {
                stats
                    .member(&member.sender.platform_id, member.is_self)
                    .is_none_or(|(_, sender)| sender.messages == 0)
            })
            .count();
        if silent > 0 {
            lines.push(Line::from(Span::styled(
                format!("  {silent} of {} members haven't posted", members.len()),
                theme.muted(),
            )));
        }
    }
    (lines, hits)
}

fn kv(label: &str, value: &str, width: usize) -> Vec<Line<'static>> {
    let prefix = format!("  {label}: ");
    let wrap = width.saturating_sub(prefix.len()).max(8);
    wrap_text(value, wrap)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                Line::from(format!("{prefix}{line}"))
            } else {
                Line::from(format!("{}{line}", " ".repeat(prefix.len())))
            }
        })
        .collect()
}

fn section(lines: &mut Vec<Line<'static>>, title: &str, theme: Theme) {
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        title.to_owned(),
        theme.pane_title(),
    )));
}

/// Builds the profile popup body. The first [`PROFILE_AVATAR_ROWS`] lines
/// start with the avatar block (or blank space reserved for an HD overlay)
/// followed by the header text.
pub(crate) fn profile_lines(
    view: &ProfileView<'_>,
    theme: Theme,
    width: u16,
) -> (Vec<Line<'static>>, Vec<PopupHit>) {
    let width = width as usize;
    let popup = view.popup;
    let mut hits = Vec::new();
    let note = view.extras.map(|extras| &extras.note);

    // Header beside the avatar.
    let avatar_width = PROFILE_AVATAR_WIDTH as usize;
    let text_width = width.saturating_sub(avatar_width + 2).max(8);
    let mut header: Vec<Line<'static>> = Vec::new();
    let name = view
        .profile
        .and_then(|profile| profile.display_name.clone())
        .unwrap_or_else(|| popup.display_name.clone());
    header.push(Line::from(Span::styled(
        truncate(&name, text_width),
        Style::default()
            .fg(theme.foreground)
            .add_modifier(Modifier::BOLD),
    )));
    if let Some(nickname) = note.and_then(|note| note.nickname.as_ref()) {
        header.push(Line::from(Span::styled(
            truncate(&format!("“{nickname}” (your nickname)"), text_width),
            Style::default().fg(theme.accent),
        )));
    }
    let mut subtitle = Vec::new();
    if let Some(handle) = view.profile.and_then(|p| p.handle.as_ref()) {
        subtitle.push(handle.to_string());
    }
    if let Some(title) = view.profile.and_then(|p| p.title.as_ref()) {
        subtitle.push(title.to_string());
    }
    if !popup.role.label().is_empty() {
        subtitle.push(popup.role.label().to_owned());
    }
    if popup.is_self {
        subtitle.push("you".to_owned());
    }
    if !subtitle.is_empty() {
        header.push(Line::from(Span::styled(
            truncate(&subtitle.join(" · "), text_width),
            theme.muted(),
        )));
    }
    let last_message = newest_last_message(
        view.last_message,
        view.stats
            .and_then(|stats| stats.member(&popup.platform_id, popup.is_self))
            .and_then(|(_, sender)| sender.last_at),
    )
    .map(|(at, seen)| {
        let elsewhere = seen
            .filter(|seen| Some(&seen.chat_id) != popup.chat_id.as_ref())
            .and_then(|seen| seen.chat_name.as_ref())
            .map(|name| format!(" in {name}"))
            .unwrap_or_default();
        format!("Last message {}{elsewhere}", format_relative(at, view.now))
    });
    if view.show_activity
        && let Some(label) =
            presence_label(view.presence, view.presence_support, view.now, last_message)
    {
        let mut spans = Vec::new();
        if let Some((dot, color)) = presence_dot(view.presence) {
            spans.push(Span::styled(format!("{dot} "), Style::default().fg(color)));
        }
        spans.push(Span::raw(truncate(&label, text_width.saturating_sub(2))));
        header.push(Line::from(spans));
    } else if view.show_activity && view.presence_support == PresenceSupport::Polled {
        header.push(Line::from(Span::styled(
            "Checking presence…",
            theme.muted(),
        )));
    }
    if let Some(status) = view.profile.and_then(|p| p.status.as_ref()) {
        header.push(Line::from(truncate(status, text_width)));
    }
    header.push(Line::from(Span::styled(
        "click avatar or press v to enlarge",
        theme.muted(),
    )));

    let mut lines: Vec<Line<'static>> = Vec::new();
    let header_rows = (PROFILE_AVATAR_ROWS as usize).max(header.len());
    for row in 0..header_rows {
        let mut spans = Vec::new();
        match view.avatar_rows.as_ref().and_then(|rows| rows.get(row)) {
            Some(avatar) => {
                let used: usize = avatar
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum();
                spans.extend(avatar.iter().cloned());
                spans.push(Span::raw(" ".repeat(avatar_width.saturating_sub(used))));
            }
            None => spans.push(Span::raw(" ".repeat(avatar_width))),
        }
        spans.push(Span::raw("  "));
        if let Some(line) = header.get(row) {
            spans.extend(line.spans.iter().cloned());
        }
        if row < PROFILE_AVATAR_ROWS as usize {
            hits.push(PopupHit {
                line: lines.len(),
                target: PopupTarget::Avatar,
            });
        }
        lines.push(Line::from(spans));
    }

    if let Some(edit) = popup.editing.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(format!("Edit {}: ", edit.field.label()), theme.status_key()),
            Span::raw(edit.buffer.clone()),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]));
        lines.push(Line::from(Span::styled(
            "Enter saves · Esc cancels · empty clears",
            theme.muted(),
        )));
    }

    // Contact details.
    section(&mut lines, "Contact", theme);
    match view.profile {
        Some(profile) => {
            let before = lines.len();
            if let Some(phone) = profile.phone.as_ref() {
                hits.push(PopupHit {
                    line: lines.len(),
                    target: PopupTarget::Copy(phone.clone()),
                });
                lines.extend(kv("Phone", phone, width));
            } else if profile.phone_hidden {
                lines.push(Line::from(Span::styled(
                    "  Phone: hidden by privacy settings",
                    theme.muted(),
                )));
            }
            if let Some(email) = profile.email.as_ref() {
                hits.push(PopupHit {
                    line: lines.len(),
                    target: PopupTarget::Copy(email.clone()),
                });
                lines.extend(kv("Email", email, width));
            }
            if let Some(local_time) = profile.local_time.as_ref() {
                let value = match profile.timezone.as_ref() {
                    Some(tz) => format!("{local_time} ({tz})"),
                    None => local_time.to_string(),
                };
                lines.extend(kv("Local time", &value, width));
            } else if let Some(tz) = profile.timezone.as_ref() {
                lines.extend(kv("Timezone", tz, width));
            }
            if let Some(about) = profile.about.as_ref() {
                lines.extend(kv("About", about, width));
            }
            let mut flags = Vec::new();
            if profile.is_bot {
                flags.push("Bot / app");
            }
            if profile.is_business {
                flags.push("Business account");
            }
            if profile.is_deactivated {
                flags.push("Deactivated");
            }
            if !flags.is_empty() {
                lines.push(Line::from(format!("  {}", flags.join(" · "))));
            }
            for (label, value) in &profile.facts {
                lines.extend(kv(label, value, width));
            }
            if lines.len() == before {
                lines.push(Line::from(Span::styled(
                    "  No additional details available",
                    theme.muted(),
                )));
            }
        }
        None if view.profile_loading => {
            lines.push(Line::from(Span::styled(
                "  Loading profile…",
                theme.muted(),
            )));
        }
        None => {
            let text = match view.profile_error {
                Some(error) => format!("  Profile unavailable: {error}"),
                None => "  No additional details available".to_owned(),
            };
            for line in wrap_text(&text, width.max(8)) {
                lines.push(Line::from(Span::styled(line, theme.muted())));
            }
        }
    }
    lines.extend(kv("ID", &popup.platform_id, width));

    // Per-member activity in this chat.
    if view.show_activity {
        section(&mut lines, "Activity here", theme);
        match view.stats {
            Some(stats) => match stats.member(&popup.platform_id, popup.is_self) {
                Some((rank, sender)) => {
                    if sender.messages > 0 {
                        let rank = rank
                            .map(|rank| format!(" · #{rank} of {}", stats.active_senders()))
                            .unwrap_or_default();
                        lines.push(Line::from(truncate(
                            &format!(
                                "  Activity: {:.0}%{rank}",
                                stats.share_percent(sender.activities)
                            ),
                            width,
                        )));
                        lines.push(Line::from(truncate(
                            &format!(
                                "  {} messages in {} bursts",
                                thousands(sender.messages),
                                thousands(sender.activities)
                            ),
                            width,
                        )));
                    } else {
                        lines.push(Line::from(Span::styled(
                            "  No messages in synced history",
                            theme.muted(),
                        )));
                    }
                    if let Some(last) = sender.last_at {
                        lines.push(Line::from(format!(
                            "  Last message here: {}",
                            format_relative(last, view.now)
                        )));
                    }
                    if let Some(first) = sender.first_at {
                        lines.push(Line::from(format!(
                            "  First message: {}",
                            format_message_datetime(first)
                        )));
                    }
                    if let Some(read) = sender.last_read_at {
                        lines.push(Line::from(format!(
                            "  Last read: {}",
                            format_relative(read, view.now)
                        )));
                    }
                    let hours = sender.top_hours(2);
                    if !hours.is_empty() && sender.messages > 0 {
                        let hours: Vec<String> = hours.into_iter().map(hour_range_label).collect();
                        lines.push(Line::from(truncate(
                            &format!("  Most active: {}", hours.join(", ")),
                            width,
                        )));
                    }
                    let mut counts = Vec::new();
                    if sender.media > 0 {
                        counts.push(format!("{} media", sender.media));
                    }
                    if sender.threads_started > 0 {
                        counts.push(format!("{} threads", sender.threads_started));
                    }
                    if sender.replies > 0 {
                        counts.push(format!("{} replies", sender.replies));
                    }
                    if !counts.is_empty() {
                        lines.push(Line::from(truncate(
                            &format!("  {}", counts.join(" · ")),
                            width,
                        )));
                    }
                    if sender.reactions_given > 0 || sender.reactions_received > 0 {
                        let top = sender
                            .top_reaction
                            .as_ref()
                            .map(|emoji| format!(" · favourite {emoji}"))
                            .unwrap_or_default();
                        lines.push(Line::from(truncate(
                            &format!(
                                "  Reactions: {} given · {} received{top}",
                                sender.reactions_given, sender.reactions_received
                            ),
                            width,
                        )));
                    }
                    if sender.mentions_me > 0 && !sender.is_me {
                        lines.push(Line::from(format!(
                            "  Mentioned you {} times",
                            sender.mentions_me
                        )));
                    }
                    if let Some(avg) = sender.avg_text_len {
                        lines.push(Line::from(format!("  Avg message: {avg} chars")));
                    }
                    if let Some(reply) = sender.median_reply_seconds {
                        lines.push(Line::from(format!(
                            "  Typical reply time: {}",
                            format_duration_short(reply)
                        )));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "  No messages in synced history",
                    theme.muted(),
                ))),
            },
            None if view.stats_loading => {
                lines.push(Line::from(Span::styled(
                    "  Computing activity…",
                    theme.muted(),
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "  No local history for this chat",
                theme.muted(),
            ))),
        }
        if let Some(stats) = view.stats
            && stats.total_messages > 0
        {
            lines.push(Line::from(Span::styled(
                format!(
                    "  Based on {} synced messages",
                    thousands(stats.total_messages)
                ),
                theme.muted(),
            )));
        }
    }

    // Extras.
    match view.extras {
        Some(extras) => {
            let groups: Vec<_> = extras
                .groups
                .iter()
                .filter(|group| Some(&group.chat_id) != popup.chat_id.as_ref())
                .take(PROFILE_LIST_LIMIT)
                .collect();
            if !groups.is_empty() {
                section(&mut lines, "Chats in common", theme);
                for (index, group) in groups.into_iter().enumerate() {
                    let name = view
                        .chat_names
                        .get(&group.chat_id)
                        .cloned()
                        .or_else(|| group.chat_name.clone())
                        .unwrap_or_else(|| group.chat_id.clone());
                    let last = group
                        .last_at
                        .map(|at| format!(" · {}", format_relative(at, view.now)))
                        .unwrap_or_default();
                    hits.push(PopupHit {
                        line: lines.len(),
                        target: PopupTarget::OpenChat {
                            account: popup.account.clone(),
                            chat_id: group.chat_id.clone(),
                        },
                    });
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", index + 1), theme.status_key()),
                        Span::raw(truncate(
                            &format!("{name} · {} msgs{last}", group.messages),
                            width.saturating_sub(4),
                        )),
                    ]));
                }
            }
            if !extras.shared.is_empty() {
                section(&mut lines, "Shared media & links", theme);
                for item in extras.shared.iter().take(PROFILE_LIST_LIMIT) {
                    let when = item
                        .timestamp
                        .map(|at| format!(" · {}", format_relative(at, view.now)))
                        .unwrap_or_default();
                    let size = item
                        .size
                        .map(|size| format!(" · {}", format_byte_size(size)))
                        .unwrap_or_default();
                    let target = match (&item.local_path, &item.url) {
                        (Some(path), _) if path.exists() => {
                            Some(PopupTarget::OpenFile(path.clone()))
                        }
                        (_, Some(url)) => Some(PopupTarget::OpenUrl(url.clone())),
                        _ => None,
                    };
                    if let Some(target) = target {
                        hits.push(PopupHit {
                            line: lines.len(),
                            target,
                        });
                    }
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {} ", item.kind.label()), theme.muted()),
                        Span::raw(truncate(
                            &format!("{}{size}{when}", item.label),
                            width.saturating_sub(item.kind.label().len() + 3),
                        )),
                    ]));
                }
            }
            if !extras.linked.is_empty() {
                section(&mut lines, "Also on", theme);
                for identity in extras.linked.iter().take(PROFILE_LIST_LIMIT) {
                    let platform = identity
                        .platform
                        .as_ref()
                        .map(platform_label)
                        .unwrap_or("Other");
                    let reason = match identity.reason {
                        LinkReason::SamePerson => "linked",
                        LinkReason::SameName => "same name",
                    };
                    if let Some(chat_id) = identity.direct_chat_id.as_ref() {
                        hits.push(PopupHit {
                            line: lines.len(),
                            target: PopupTarget::OpenChat {
                                account: identity.account.clone(),
                                chat_id: chat_id.clone(),
                            },
                        });
                    }
                    lines.push(Line::from(truncate(
                        &format!(
                            "  {platform} · {} ({reason}){}",
                            identity.display_name,
                            if identity.direct_chat_id.is_some() {
                                " · open"
                            } else {
                                ""
                            }
                        ),
                        width,
                    )));
                }
            }
            section(&mut lines, "Private note", theme);
            match extras.note.note.as_ref() {
                Some(text) => {
                    for line in wrap_text(text, width.saturating_sub(2).max(8)) {
                        lines.push(Line::from(format!("  {line}")));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "  None · press e to add (stored only on this device)",
                    theme.muted(),
                ))),
            }
        }
        None if view.extras_loading => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Loading chats in common, media and notes…",
                theme.muted(),
            )));
        }
        None => {}
    }

    lines.push(Line::from(""));
    for hint in wrap_text(
        "v avatar · m message · @ mention · s search · c copy · 1-6 open chat · n nickname · e note · Esc close",
        width.max(8),
    ) {
        lines.push(Line::from(Span::styled(hint, theme.muted())));
    }
    (lines, hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::Sender;
    use chrono::{Duration, TimeZone, Utc};

    fn member(id: &str, name: &str, role: ChatMemberRole) -> ChatMember {
        ChatMember {
            sender: Sender {
                platform_id: Arc::from(id),
                display_name: Arc::from(name),
                avatar: None,
            },
            role,
            is_self: false,
        }
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
        assert_eq!(format_relative(now - Duration::seconds(5), now), "just now");
        assert_eq!(format_relative(now - Duration::minutes(5), now), "5m ago");
        assert_eq!(format_relative(now - Duration::hours(3), now), "3h ago");
        assert_eq!(format_relative(now - Duration::days(2), now), "2d ago");
    }

    #[test]
    fn presence_labels_cover_states() {
        let now = Utc::now();
        let last_msg = || Some("Last message 2h ago".to_owned());
        let label = |presence: &Presence, last: Option<String>| {
            presence_label(Some(presence), PresenceSupport::Pushed, now, last)
        };
        let mut presence = Presence::new(PresenceState::Online, now);
        assert_eq!(label(&presence, last_msg()).as_deref(), Some("Online"));
        presence.state = PresenceState::Offline;
        presence.last_seen = Some(now - Duration::minutes(7));
        assert_eq!(
            label(&presence, last_msg()).as_deref(),
            Some("Last seen 7m ago"),
            "a real last seen beats the local fallback"
        );
        presence.last_seen = None;
        presence.last_seen_hidden = true;
        assert_eq!(
            label(&presence, last_msg()).as_deref(),
            Some("Last message 2h ago"),
            "hidden last seen falls back to the last local message"
        );
        assert_eq!(label(&presence, None).as_deref(), Some("Last seen hidden"));
        presence.last_seen_hidden = false;
        assert_eq!(
            label(&presence, last_msg()).as_deref(),
            Some("Last message 2h ago")
        );
        assert_eq!(label(&presence, None).as_deref(), Some("Offline"));
        assert_eq!(
            presence_label(
                Some(&presence),
                PresenceSupport::Unsupported,
                now,
                last_msg()
            ),
            None
        );
    }

    #[test]
    fn newest_last_message_prefers_the_most_recent_source() {
        let now = Utc::now();
        let seen = LastMessageSeen {
            at: now - Duration::hours(1),
            chat_id: Arc::from("other"),
            chat_name: Some(Arc::from("Climbing")),
        };
        let (at, from) = newest_last_message(Some(&seen), Some(now - Duration::hours(3))).unwrap();
        assert_eq!(
            (at, from.map(|s| s.chat_id.as_ref())),
            (seen.at, Some("other"))
        );
        let here = now - Duration::minutes(5);
        assert_eq!(
            newest_last_message(Some(&seen), Some(here)),
            Some((here, None))
        );
        assert_eq!(newest_last_message(None, None), None);
    }

    #[test]
    fn whatsapp_lid_members_also_search_their_phone_jid() {
        let profile = ContactProfile {
            phone: Some(Arc::from("+44 7700 900123")),
            ..ContactProfile::default()
        };
        let lid: PlatformId = Arc::from("123@lid");
        let ids = last_message_alias_ids(&lid, Some(&profile));
        assert_eq!(
            ids.iter().map(|id| id.as_ref()).collect::<Vec<_>>(),
            vec!["123@lid", "447700900123@s.whatsapp.net"]
        );
        let slack: PlatformId = Arc::from("U123");
        assert_eq!(last_message_alias_ids(&slack, Some(&profile)).len(), 1);
        assert_eq!(last_message_alias_ids(&lid, None).len(), 1);
    }

    #[test]
    fn bars_and_sparklines_scale() {
        assert_eq!(share_bar(50.0, 10), "█████░░░░░");
        assert_eq!(share_bar(0.0, 4), "░░░░");
        assert_eq!(share_bar(150.0, 4), "████");
        assert_eq!(sparkline(&[0, 1, 8]), " ▂█");
        assert_eq!(thousands(4312), "4,312");
        assert_eq!(thousands(12), "12");
    }

    #[test]
    fn member_ordering_filters_and_sorts() {
        let members = vec![
            member("c", "Carol", ChatMemberRole::Member),
            member("a", "alice", ChatMemberRole::Admin),
            member("b", "Bob", ChatMemberRole::Owner),
        ];
        let none = |_: &PlatformId| None;
        assert_eq!(
            ordered_member_indices(&members, "", MemberSort::Name, None, &none),
            vec![1, 2, 0]
        );
        assert_eq!(
            ordered_member_indices(&members, "", MemberSort::Role, None, &none),
            vec![2, 1, 0]
        );
        assert_eq!(
            ordered_member_indices(&members, "BO", MemberSort::Default, None, &none),
            vec![2]
        );
        let now = Utc::now();
        let online = |id: &PlatformId| {
            (id.as_ref() == "c").then(|| Presence::new(PresenceState::Online, now))
        };
        assert_eq!(
            ordered_member_indices(&members, "", MemberSort::Presence, None, &online)[0],
            0
        );
    }

    #[test]
    fn member_sort_cycles_back_to_default() {
        let mut sort = MemberSort::Default;
        for _ in 0..5 {
            sort = sort.next();
        }
        assert_eq!(sort, MemberSort::Default);
    }

    #[test]
    fn activity_lists_merged_self_once_and_ranks_by_activity() {
        use storage::{ME_SENDER_KEY, SenderStats};
        let sender =
            |id: &str, name: &str, is_me: bool, messages: u64, activities: u64| SenderStats {
                sender_id: Arc::from(id),
                display_name: Arc::from(name),
                is_me,
                messages,
                activities,
                ..SenderStats::default()
            };
        let stats = ChatStats {
            total_messages: 30,
            my_messages: 10,
            total_activities: 10,
            my_activities: 2,
            // Alice sent fewer messages but in more separate bursts.
            senders: vec![
                sender("alice", "Alice", false, 10, 6),
                sender(ME_SENDER_KEY, "Me", true, 10, 2),
                sender("bob", "Bob", false, 10, 2),
            ],
            my_sender_ids: vec![Arc::from("123@lid"), Arc::from("me")],
            daily_counts: vec![0; 7],
            ..ChatStats::default()
        };
        let mut me = member("4477@s.whatsapp.net", "Me", ChatMemberRole::Member);
        me.is_self = true;
        let members = vec![member("alice", "Alice", ChatMemberRole::Member), me];
        let view = ActivityView {
            stats: Some(&stats),
            loading: false,
            error: None,
            members: Some(&members),
            is_direct: false,
            width: 40,
            now: Utc::now(),
        };
        let (lines, hits) = activity_lines(&view, Theme::default());
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let you_rows = text.iter().filter(|line| line.contains("You ")).count();
        assert_eq!(you_rows, 1, "self listed once: {text:#?}");
        let alice_row = text.iter().position(|l| l.contains("Alice")).unwrap();
        assert!(text[alice_row].contains("60%"), "{}", text[alice_row]);
        // Clicking "You" opens our real member id, never the synthetic key.
        let ids: Vec<&str> = hits.iter().map(|(_, id)| id.as_ref()).collect();
        assert_eq!(ids, vec!["alice", "4477@s.whatsapp.net", "bob"]);
        // Members lookups resolve self through the merged entry.
        assert_eq!(
            stats
                .member("4477@s.whatsapp.net", true)
                .unwrap()
                .1
                .activities,
            2
        );
        assert!(stats.member("4477@s.whatsapp.net", false).is_none());
    }
}
