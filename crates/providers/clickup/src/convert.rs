//! Mapping between ClickUp wire types and the `chat-core` domain model.
//!
//! ClickUp serves message bodies as Markdown (`content_format=text/md`), which
//! the renderer already understands, so there is no platform-specific markup
//! translator here — only structural mapping, timestamp parsing, and emoji
//! name/glyph conversion.

use crate::api::{WireChannel, WireMessage, WireReaction, WireUser};
use chat_core::{
    Chat, ChatKind, ChatMember, ChatMembership, ClickUpData, Content, Message, Platform,
    PlatformData, PlatformId, ProviderId, Reaction, Sender, Timestamp,
};
use chrono::{DateTime, TimeZone, Utc};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// Interns a `String` into the shared `Arc<str>` representation used throughout
/// the domain model.
pub fn arc_str(value: impl Into<String>) -> Arc<str> {
    Arc::from(value.into())
}

/// Parses a ClickUp timestamp.
///
/// ClickUp is inconsistent here: `date` fields on messages are JSON numbers of
/// Unix epoch **milliseconds**, while `latest_comment_at` and `created_at` on a
/// channel are strings. The strings have historically held Unix milliseconds
/// too, but an ISO-8601 value would also be plausible for an experimental API,
/// so both are accepted rather than dropping the field.
pub fn parse_timestamp_str(value: &str) -> Option<Timestamp> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(millis) = trimmed.parse::<i64>() {
        return timestamp_from_millis(millis as f64);
    }

    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

/// Converts Unix epoch milliseconds into a domain timestamp.
pub fn timestamp_from_millis(millis: f64) -> Option<Timestamp> {
    if !millis.is_finite() {
        return None;
    }
    let millis = millis as i64;
    Utc.timestamp_millis_opt(millis).single()
}

/// Renders a domain timestamp as the Unix-millisecond value ClickUp query
/// parameters expect.
pub fn millis_from_timestamp(timestamp: Timestamp) -> i64 {
    timestamp.timestamp_millis()
}

/// Whether a channel should appear in the sidebar.
///
/// Archived channels are always excluded. DMs and group DMs the user has hidden
/// are excluded too, matching what the ClickUp app shows.
pub fn include_channel_in_sidebar(channel: &WireChannel) -> bool {
    if channel.id.trim().is_empty() {
        return false;
    }
    if channel.archived.unwrap_or(false) {
        return false;
    }
    if channel.is_hidden.unwrap_or(false) {
        return false;
    }
    true
}

/// Maps ClickUp's room type and visibility onto the domain [`ChatKind`].
pub fn channel_chat_kind(channel: &WireChannel) -> ChatKind {
    let kind = channel
        .channel_kind
        .as_deref()
        .unwrap_or("CHANNEL")
        .trim()
        .to_ascii_uppercase();
    match kind.as_str() {
        "DM" => ChatKind::Direct,
        "GROUP_DM" => ChatKind::GroupDirectMessage,
        _ => {
            let private = channel
                .visibility
                .as_deref()
                .map(|value| value.trim().eq_ignore_ascii_case("PRIVATE"))
                .unwrap_or(false);
            if private {
                ChatKind::PrivateChannel
            } else {
                ChatKind::PublicChannel
            }
        }
    }
}

/// Human-facing name for a channel.
///
/// Channels are prefixed with `#` to match the ClickUp app and the existing
/// Slack presentation; DMs are left bare. An unnamed channel falls back to its
/// id so the row is still addressable.
pub fn channel_display_name(channel: &WireChannel, kind: ChatKind) -> Arc<str> {
    let raw = channel
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    let Some(name) = raw else {
        return match kind {
            ChatKind::Direct => arc_str("Direct message"),
            ChatKind::GroupDirectMessage => arc_str("Group message"),
            _ => arc_str(format!("#{}", channel.id)),
        };
    };

    match kind {
        ChatKind::PublicChannel | ChatKind::PrivateChannel => {
            if name.starts_with('#') {
                arc_str(name)
            } else {
                arc_str(format!("#{name}"))
            }
        }
        _ => arc_str(name),
    }
}

/// Local cache path for a user's ClickUp profile picture, queueing the
/// download in the background when the bytes are not cached yet.
///
/// Returns `None` when ClickUp has no picture for the user, so the UI falls
/// back to initials instead of waiting for a file that will never appear.
pub fn avatar_for_user(user: &WireUser) -> Option<PathBuf> {
    let url = user
        .profile_picture
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())?;
    crate::http::cached_avatar_path(url)
}

/// Human-facing name for a DM or group DM, built from its members.
///
/// ClickUp leaves `name` empty on DMs and group DMs — the app composes the
/// title from the participants — so the counterpart names have to be joined
/// here. The authenticated user is excluded, since a conversation is named
/// after the *other* people in it. A DM with oneself keeps the self name
/// rather than collapsing to an empty label.
pub fn direct_chat_display_name(
    members: &[WireUser],
    self_user_id: Option<&str>,
) -> Option<String> {
    let others: Vec<String> = members
        .iter()
        .filter(|member| {
            self_user_id
                .map(|self_id| !self_id.is_empty() && member.id.trim() != self_id)
                .unwrap_or(true)
        })
        .map(WireUser::best_name)
        .collect();

    let names = if others.is_empty() {
        members.iter().map(WireUser::best_name).collect::<Vec<_>>()
    } else {
        others
    };

    if names.is_empty() {
        return None;
    }
    Some(names.join(", "))
}

/// Overwrites a DM/group-DM chat's name and avatar from its member list.
///
/// A named channel is left alone: only DMs and group DMs lack a server-side
/// title. The avatar is taken from the single counterpart of a one-to-one DM;
/// a group DM has no one representative picture, so it keeps its fallback.
pub fn apply_direct_chat_identity(
    chat: &mut Chat,
    members: &[WireUser],
    self_user_id: Option<&str>,
) -> bool {
    if !matches!(chat.kind, ChatKind::Direct | ChatKind::GroupDirectMessage) {
        return false;
    }

    let mut changed = false;
    if let Some(name) = direct_chat_display_name(members, self_user_id)
        && chat.name.as_ref() != name
    {
        chat.name = arc_str(name);
        changed = true;
    }

    let counterparts: Vec<&WireUser> = members
        .iter()
        .filter(|member| {
            self_user_id
                .map(|self_id| !self_id.is_empty() && member.id.trim() != self_id)
                .unwrap_or(true)
        })
        .collect();
    if let [only] = counterparts.as_slice() {
        let avatar = avatar_for_user(only);
        if avatar.is_some() && chat.avatar != avatar {
            chat.avatar = avatar;
            changed = true;
        }
    }

    changed
}

/// Builds a domain [`Chat`] from a ClickUp channel.
///
/// `last_message_at` is populated from `latest_comment_at`, which — unlike
/// Slack's `updated` field — is genuine message activity rather than
/// conversation metadata, so it is a legitimate sidebar ordering key.
/// `last_message_preview` stays `None` because ClickUp's channel listing does
/// not carry message text; it is filled downstream from real messages.
pub fn chat_from_channel(account: &ProviderId, channel: &WireChannel) -> Chat {
    let kind = channel_chat_kind(channel);
    let counts = channel.counts.as_ref();

    let unread_count = counts
        .and_then(|counts| counts.num_unread)
        .or_else(|| {
            counts
                .and_then(|counts| counts.has_unread)
                .and_then(|has_unread| has_unread.then_some(1))
        })
        .unwrap_or(0);

    Chat {
        id: arc_str(channel.id.clone()),
        account: account.clone(),
        platform: Platform::ClickUp,
        name: channel_display_name(channel, kind),
        avatar: None,
        is_group: !matches!(kind, ChatKind::Direct),
        kind,
        membership: ChatMembership::Joined,
        is_shared: false,
        unread_count,
        muted: false,
        pinned: false,
        // Repository rule: sidebar activity must reflect real messages, not
        // provider channel metadata. `latest_comment_at` is deliberately used
        // only to narrow polling, never to order the sidebar.
        last_message_at: None,
        last_message_preview: None,
        thread_id: None,
    }
}

/// Groups ClickUp's per-user reaction rows into the domain's per-emoji shape,
/// preserving first-seen emoji order.
pub fn reactions_from_wire(reactions: &[WireReaction]) -> Vec<Reaction> {
    let mut order: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<String, Vec<PlatformId>> = BTreeMap::new();

    for reaction in reactions {
        let name = reaction.reaction.trim();
        if name.is_empty() {
            continue;
        }
        let display = emoji_display(name);
        if !grouped.contains_key(&display) {
            order.push(display.clone());
        }
        let senders = grouped.entry(display).or_default();
        if let Some(user_id) = reaction
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let user_id: PlatformId = arc_str(user_id);
            if !senders.contains(&user_id) {
                senders.push(user_id);
            }
        }
    }

    order
        .into_iter()
        .filter_map(|display| {
            grouped.remove(&display).map(|senders| Reaction {
                emoji: arc_str(display),
                senders,
            })
        })
        .collect()
}

/// Converts a ClickUp emoji shortcode into a display glyph, falling back to a
/// `:name:` rendering for custom emoji the local table cannot resolve.
pub fn emoji_display(name: &str) -> String {
    let trimmed = name.trim().trim_matches(':').trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(emoji) = emojis::get_by_shortcode(trimmed) {
        return emoji.as_str().to_owned();
    }
    // ClickUp shortcodes occasionally use `-` where the table uses `_`.
    let normalized = trimmed.replace('-', "_");
    if let Some(emoji) = emojis::get_by_shortcode(&normalized) {
        return emoji.as_str().to_owned();
    }
    // Already a literal glyph.
    if emojis::get(trimmed).is_some() {
        return trimmed.to_owned();
    }
    format!(":{trimmed}:")
}

/// Converts a glyph or shortcode into the lower-case emoji **name** ClickUp's
/// reaction endpoint requires.
///
/// Returns `None` when no name can be determined, so the caller can surface a
/// clean "unsupported reaction" error instead of posting a value ClickUp will
/// reject.
pub fn emoji_reaction_name(emoji: &str) -> Option<String> {
    let trimmed = emoji.trim().trim_matches(':').trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(shortcode) = emojis::get(trimmed).and_then(|emoji| emoji.shortcode()) {
        return Some(shortcode.to_ascii_lowercase());
    }

    // Already a shortcode the emoji table recognises.
    if emojis::get_by_shortcode(trimmed).is_some() {
        return Some(trimmed.to_ascii_lowercase());
    }

    let normalized = trimmed.replace('-', "_").to_ascii_lowercase();
    if emojis::get_by_shortcode(&normalized).is_some() {
        return Some(normalized);
    }

    None
}

/// Builds a [`Sender`] for a ClickUp user id, using a resolved display name
/// when one is available.
pub fn sender_from_user_id(user_id: &str, display_name: Option<&str>) -> Sender {
    let platform_id = arc_str(user_id);
    let display_name = display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(arc_str)
        .unwrap_or_else(|| {
            if user_id.trim().is_empty() {
                arc_str("Unknown user")
            } else {
                arc_str(format!("User {user_id}"))
            }
        });
    Sender {
        platform_id,
        display_name,
        avatar: None,
    }
}

/// Builds a [`Sender`] from a fully resolved ClickUp user, including their
/// cached profile picture.
pub fn sender_from_user(user: &WireUser) -> Sender {
    Sender {
        avatar: avatar_for_user(user),
        ..sender_from_user_id(&user.id, Some(&user.best_name()))
    }
}

/// Builds a [`ChatMember`] from a ClickUp user.
///
/// ClickUp's chat member listing does not expose a per-member role, so every
/// member is reported as an ordinary participant.
pub fn chat_member_from_user(user: &WireUser) -> ChatMember {
    ChatMember::new(sender_from_user(user))
}

/// Context needed to convert a wire message into a domain message.
pub struct MessageContext<'a> {
    pub account: &'a ProviderId,
    pub workspace_id: &'a str,
    pub channel_id: &'a str,
    /// The authenticated user's ClickUp id, used to compute `is_from_me`.
    pub self_user_id: Option<&'a str>,
    /// Resolved users keyed by ClickUp user id, supplying both the display
    /// name and the avatar for a message's sender.
    /// `Send + Sync` because the polling task holds a [`MessageContext`]
    /// across `.await` points inside a spawned task.
    pub users: &'a (dyn Fn(&str) -> Option<WireUser> + Send + Sync),
}

/// Converts a ClickUp message into the domain model.
///
/// Returns `None` only when the message carries no usable id, since every
/// downstream operation is keyed by it.
pub fn message_from_wire(wire: &WireMessage, context: &MessageContext<'_>) -> Option<Message> {
    if wire.id.trim().is_empty() {
        return None;
    }

    let user_id = wire.user_id.as_deref().unwrap_or("").trim().to_owned();
    let resolved = (context.users)(&user_id);
    let display_name = resolved.as_ref().map(WireUser::best_name);
    let sender = match resolved.as_ref() {
        Some(user) => Sender {
            platform_id: arc_str(user_id.clone()),
            ..sender_from_user(user)
        },
        None => sender_from_user_id(&user_id, None),
    };

    let timestamp = wire
        .date
        .and_then(timestamp_from_millis)
        .unwrap_or_else(Utc::now);

    let body = wire.content.as_deref().unwrap_or("").trim();
    let content = if body.is_empty() {
        Content::Unsupported(arc_str("(empty ClickUp message)"))
    } else {
        Content::Text(arc_str(body))
    };

    let is_from_me = context
        .self_user_id
        .map(|self_id| !self_id.is_empty() && self_id == user_id)
        .unwrap_or(false);

    let mentions_me = context
        .self_user_id
        .map(|self_id| body_mentions_user(body, self_id, display_name.as_deref()))
        .unwrap_or(false);

    // A ClickUp reply names its parent; the thread is identified by that
    // parent, which is also the thread root. A message with replies but no
    // parent is itself a thread root.
    let parent = wire
        .parent_message
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let thread_id = match parent {
        Some(parent) => Some(arc_str(parent)),
        None if wire.replies_count.unwrap_or(0) > 0 => Some(arc_str(wire.id.clone())),
        None => None,
    };

    Some(Message {
        id: arc_str(wire.id.clone()),
        chat_id: arc_str(context.channel_id),
        account: context.account.clone(),
        sender,
        timestamp,
        edited_at: wire
            .date_updated
            .filter(|updated| wire.date.map(|created| *updated > created).unwrap_or(false))
            .and_then(timestamp_from_millis),
        content,
        reply_to: parent.map(arc_str),
        thread_id,
        reactions: reactions_from_wire(&wire.reactions),
        receipts: Vec::new(),
        is_from_me,
        mentions_me,
        platform_data: PlatformData {
            clickup: Some(ClickUpData {
                workspace_id: arc_str(context.workspace_id),
                channel_id: arc_str(context.channel_id),
                message_id: arc_str(wire.id.clone()),
                parent_message_id: parent.map(arc_str),
            }),
            ..PlatformData::default()
        },
    })
}

/// Whether a message body @-mentions the authenticated user, or contains a
/// broadcast ping.
///
/// ClickUp renders mentions in Markdown as `@Display Name`, and the public API
/// exposes tagged users only through a separate per-message endpoint that would
/// cost one request per message. Matching the rendered mention text keeps this
/// free, at the cost of missing mentions when the display name is unknown.
fn body_mentions_user(body: &str, self_user_id: &str, self_display_name: Option<&str>) -> bool {
    if body.is_empty() {
        return false;
    }
    let lowered = body.to_lowercase();

    for broadcast in ["@all", "@here", "@channel", "@everyone"] {
        if lowered.contains(broadcast) {
            return true;
        }
    }

    if !self_user_id.is_empty() && lowered.contains(&format!("@{}", self_user_id.to_lowercase())) {
        return true;
    }

    self_display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_some_and(|name| lowered.contains(&format!("@{}", name.to_lowercase())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_names() -> impl Fn(&str) -> Option<WireUser> {
        |_| None
    }

    fn user(id: &str, name: &str) -> WireUser {
        WireUser {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            ..WireUser::default()
        }
    }

    fn channel(id: &str) -> WireChannel {
        WireChannel {
            id: id.to_owned(),
            ..WireChannel::default()
        }
    }

    #[test]
    fn parses_unix_millisecond_string_timestamps() {
        let parsed = parse_timestamp_str("1735689600000").expect("parses");
        assert_eq!(parsed.timestamp(), 1_735_689_600);
    }

    #[test]
    fn parses_rfc3339_timestamps() {
        let parsed = parse_timestamp_str("2025-01-01T00:00:00Z").expect("parses");
        assert_eq!(parsed.timestamp(), 1_735_689_600);
    }

    #[test]
    fn rejects_blank_and_garbage_timestamps() {
        assert!(parse_timestamp_str("").is_none());
        assert!(parse_timestamp_str("   ").is_none());
        assert!(parse_timestamp_str("not-a-date").is_none());
    }

    #[test]
    fn timestamp_millis_round_trip() {
        let value = timestamp_from_millis(1_735_689_600_000.0).expect("parses");
        assert_eq!(millis_from_timestamp(value), 1_735_689_600_000);
        assert!(timestamp_from_millis(f64::NAN).is_none());
        assert!(timestamp_from_millis(f64::INFINITY).is_none());
    }

    #[test]
    fn maps_room_type_to_chat_kind() {
        let mut dm = channel("c");
        dm.channel_kind = Some("DM".to_owned());
        assert_eq!(channel_chat_kind(&dm), ChatKind::Direct);

        let mut group = channel("c");
        group.channel_kind = Some("GROUP_DM".to_owned());
        assert_eq!(channel_chat_kind(&group), ChatKind::GroupDirectMessage);

        let mut public = channel("c");
        public.channel_kind = Some("CHANNEL".to_owned());
        public.visibility = Some("PUBLIC".to_owned());
        assert_eq!(channel_chat_kind(&public), ChatKind::PublicChannel);

        let mut private = channel("c");
        private.channel_kind = Some("CHANNEL".to_owned());
        private.visibility = Some("PRIVATE".to_owned());
        assert_eq!(channel_chat_kind(&private), ChatKind::PrivateChannel);
    }

    #[test]
    fn unknown_room_type_defaults_to_public_channel() {
        let mut unknown = channel("c");
        unknown.channel_kind = Some("SOMETHING_NEW".to_owned());
        assert_eq!(channel_chat_kind(&unknown), ChatKind::PublicChannel);
    }

    #[test]
    fn channel_names_are_hash_prefixed_exactly_once() {
        let mut named = channel("c");
        named.name = Some("general".to_owned());
        assert_eq!(
            channel_display_name(&named, ChatKind::PublicChannel).as_ref(),
            "#general"
        );

        named.name = Some("#general".to_owned());
        assert_eq!(
            channel_display_name(&named, ChatKind::PublicChannel).as_ref(),
            "#general"
        );
    }

    #[test]
    fn dm_names_are_not_prefixed() {
        let mut dm = channel("c");
        dm.name = Some("Ada Lovelace".to_owned());
        assert_eq!(
            channel_display_name(&dm, ChatKind::Direct).as_ref(),
            "Ada Lovelace"
        );
    }

    #[test]
    fn unnamed_channels_fall_back_by_kind() {
        let bare = channel("90010");
        assert_eq!(
            channel_display_name(&bare, ChatKind::PublicChannel).as_ref(),
            "#90010"
        );
        assert_eq!(
            channel_display_name(&bare, ChatKind::Direct).as_ref(),
            "Direct message"
        );
        assert_eq!(
            channel_display_name(&bare, ChatKind::GroupDirectMessage).as_ref(),
            "Group message"
        );
    }

    #[test]
    fn excludes_archived_and_hidden_channels() {
        let mut archived = channel("c");
        archived.archived = Some(true);
        assert!(!include_channel_in_sidebar(&archived));

        let mut hidden = channel("c");
        hidden.is_hidden = Some(true);
        assert!(!include_channel_in_sidebar(&hidden));

        assert!(!include_channel_in_sidebar(&channel("  ")));
        assert!(include_channel_in_sidebar(&channel("c")));
    }

    #[test]
    fn chat_never_reports_channel_metadata_as_message_activity() {
        let account: ProviderId = arc_str("clickup:acct");
        let mut wire = channel("c1");
        wire.name = Some("general".to_owned());
        wire.latest_comment_at = Some("1735689600000".to_owned());

        let chat = chat_from_channel(&account, &wire);
        assert_eq!(chat.platform, Platform::ClickUp);
        // Repository rule: activity and preview must come from real stored
        // messages. `latest_comment_at` only narrows polling.
        assert!(chat.last_message_at.is_none());
        assert!(chat.last_message_preview.is_none());
    }

    #[test]
    fn chat_reads_unread_counts_from_counts_block() {
        let account: ProviderId = arc_str("clickup:acct");
        let mut wire = channel("c1");
        wire.counts = Some(crate::api::WireChannelCounts {
            num_unread: Some(7),
            ..Default::default()
        });
        assert_eq!(chat_from_channel(&account, &wire).unread_count, 7);
    }

    #[test]
    fn chat_falls_back_to_has_unread_flag() {
        let account: ProviderId = arc_str("clickup:acct");
        let mut wire = channel("c1");
        wire.counts = Some(crate::api::WireChannelCounts {
            has_unread: Some(true),
            ..Default::default()
        });
        assert_eq!(chat_from_channel(&account, &wire).unread_count, 1);

        wire.counts = Some(crate::api::WireChannelCounts {
            has_unread: Some(false),
            ..Default::default()
        });
        assert_eq!(chat_from_channel(&account, &wire).unread_count, 0);
    }

    #[test]
    fn dm_chats_are_not_marked_as_groups() {
        let account: ProviderId = arc_str("clickup:acct");
        let mut dm = channel("c1");
        dm.channel_kind = Some("DM".to_owned());
        assert!(!chat_from_channel(&account, &dm).is_group);

        let mut group = channel("c2");
        group.channel_kind = Some("GROUP_DM".to_owned());
        assert!(chat_from_channel(&account, &group).is_group);
    }

    #[test]
    fn groups_reactions_by_emoji_preserving_order() {
        let wire = vec![
            WireReaction {
                reaction: "thumbsup".to_owned(),
                user_id: Some("u1".to_owned()),
                date: None,
            },
            WireReaction {
                reaction: "tada".to_owned(),
                user_id: Some("u2".to_owned()),
                date: None,
            },
            WireReaction {
                reaction: "thumbsup".to_owned(),
                user_id: Some("u3".to_owned()),
                date: None,
            },
        ];
        let reactions = reactions_from_wire(&wire);
        assert_eq!(reactions.len(), 2);
        assert_eq!(reactions[0].emoji.as_ref(), "👍");
        assert_eq!(reactions[0].senders.len(), 2);
        assert_eq!(reactions[1].emoji.as_ref(), "🎉");
    }

    #[test]
    fn reaction_grouping_deduplicates_repeat_senders_and_skips_blanks() {
        let wire = vec![
            WireReaction {
                reaction: "thumbsup".to_owned(),
                user_id: Some("u1".to_owned()),
                date: None,
            },
            WireReaction {
                reaction: "thumbsup".to_owned(),
                user_id: Some("u1".to_owned()),
                date: None,
            },
            WireReaction {
                reaction: "   ".to_owned(),
                user_id: Some("u2".to_owned()),
                date: None,
            },
        ];
        let reactions = reactions_from_wire(&wire);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].senders.len(), 1);
    }

    #[test]
    fn renders_known_shortcodes_as_glyphs() {
        assert_eq!(emoji_display("thumbsup"), "👍");
        assert_eq!(emoji_display(":thumbsup:"), "👍");
        assert_eq!(emoji_display("👍"), "👍");
    }

    #[test]
    fn renders_unknown_custom_emoji_as_colon_name() {
        assert_eq!(emoji_display("clickup_party"), ":clickup_party:");
        assert_eq!(emoji_display(""), "");
    }

    #[test]
    fn converts_glyphs_to_lowercase_reaction_names() {
        assert_eq!(emoji_reaction_name("🎉").as_deref(), Some("tada"));

        // The emoji table's primary shortcode for a glyph is not always the
        // most familiar alias (👍 maps to `+1`, not `thumbsup`), so assert the
        // property that actually matters: whatever name is produced must round
        // trip back to the same glyph, since that name is what the UI renders
        // after ClickUp echoes the reaction back.
        for glyph in ["👍", "🎉", "❤️", "😀"] {
            let name = emoji_reaction_name(glyph).expect("glyph maps to a name");
            assert_eq!(name, name.to_ascii_lowercase());
            assert_eq!(emoji_display(&name), glyph);
        }
    }

    #[test]
    fn accepts_shortcodes_as_reaction_names() {
        assert_eq!(emoji_reaction_name("thumbsup").as_deref(), Some("thumbsup"));
        assert_eq!(
            emoji_reaction_name(":thumbsup:").as_deref(),
            Some("thumbsup")
        );
    }

    #[test]
    fn rejects_unmappable_reaction_names() {
        // Callers turn this into a clean unsupported-reaction error rather than
        // posting a value ClickUp would reject.
        assert!(emoji_reaction_name("definitely_not_an_emoji").is_none());
        assert!(emoji_reaction_name("").is_none());
    }

    #[test]
    fn message_conversion_maps_text_thread_and_platform_data() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: Some("u-self"),
            users: &names,
        };
        let wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("hello **world**".to_owned()),
            date: Some(1_735_689_600_000.0),
            user_id: Some("u1".to_owned()),
            parent_message: Some("m0".to_owned()),
            ..WireMessage::default()
        };

        let message = message_from_wire(&wire, &context).expect("converts");
        assert_eq!(message.id.as_ref(), "m1");
        assert_eq!(message.chat_id.as_ref(), "c1");
        assert!(
            matches!(message.content, Content::Text(ref text) if text.as_ref() == "hello **world**")
        );
        assert_eq!(message.reply_to.as_deref(), Some("m0"));
        assert_eq!(message.thread_id.as_deref(), Some("m0"));
        assert!(!message.is_from_me);

        let data = message.platform_data.clickup.expect("clickup data");
        assert_eq!(data.workspace_id.as_ref(), "ws1");
        assert_eq!(data.channel_id.as_ref(), "c1");
        assert_eq!(data.message_id.as_ref(), "m1");
        assert_eq!(data.parent_message_id.as_deref(), Some("m0"));
    }

    #[test]
    fn message_without_id_is_skipped() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: None,
            users: &names,
        };
        let wire = WireMessage {
            id: "  ".to_owned(),
            ..WireMessage::default()
        };
        assert!(message_from_wire(&wire, &context).is_none());
    }

    #[test]
    fn own_messages_are_flagged_from_me() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: Some("u1"),
            users: &names,
        };
        let wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("mine".to_owned()),
            user_id: Some("u1".to_owned()),
            ..WireMessage::default()
        };
        assert!(
            message_from_wire(&wire, &context)
                .expect("converts")
                .is_from_me
        );
    }

    #[test]
    fn thread_root_is_self_when_message_has_replies() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: None,
            users: &names,
        };
        let wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("root".to_owned()),
            replies_count: Some(3),
            ..WireMessage::default()
        };
        let message = message_from_wire(&wire, &context).expect("converts");
        assert_eq!(message.thread_id.as_deref(), Some("m1"));
        assert!(message.reply_to.is_none());
    }

    #[test]
    fn standalone_message_has_no_thread() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: None,
            users: &names,
        };
        let wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("alone".to_owned()),
            replies_count: Some(0),
            ..WireMessage::default()
        };
        let message = message_from_wire(&wire, &context).expect("converts");
        assert!(message.thread_id.is_none());
    }

    #[test]
    fn empty_body_becomes_unsupported_content() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: None,
            users: &names,
        };
        let wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("   ".to_owned()),
            ..WireMessage::default()
        };
        let message = message_from_wire(&wire, &context).expect("converts");
        assert!(matches!(message.content, Content::Unsupported(_)));
    }

    #[test]
    fn edited_at_only_set_when_update_is_after_creation() {
        let account: ProviderId = arc_str("clickup:acct");
        let names = no_names();
        let context = MessageContext {
            account: &account,
            workspace_id: "ws1",
            channel_id: "c1",
            self_user_id: None,
            users: &names,
        };
        let mut wire = WireMessage {
            id: "m1".to_owned(),
            content: Some("x".to_owned()),
            date: Some(1_000_000.0),
            date_updated: Some(1_000_000.0),
            ..WireMessage::default()
        };
        assert!(
            message_from_wire(&wire, &context)
                .expect("converts")
                .edited_at
                .is_none()
        );

        wire.date_updated = Some(2_000_000.0);
        assert!(
            message_from_wire(&wire, &context)
                .expect("converts")
                .edited_at
                .is_some()
        );
    }

    #[test]
    fn detects_broadcast_and_named_mentions() {
        assert!(body_mentions_user("hey @all please look", "u1", None));
        assert!(body_mentions_user("cc @here", "u1", None));
        assert!(body_mentions_user(
            "ping @Ada Lovelace",
            "u1",
            Some("Ada Lovelace")
        ));
        assert!(body_mentions_user("ping @u1", "u1", None));
    }

    #[test]
    fn ignores_unrelated_mentions() {
        assert!(!body_mentions_user("hello team", "u1", Some("Ada")));
        assert!(!body_mentions_user("ping @Bob", "u1", Some("Ada")));
        assert!(!body_mentions_user("", "u1", Some("Ada")));
    }

    #[test]
    fn chat_member_uses_best_available_name() {
        let user = WireUser {
            id: "u1".to_owned(),
            username: Some("ada".to_owned()),
            ..WireUser::default()
        };
        let member = chat_member_from_user(&user);
        assert_eq!(member.sender.display_name.as_ref(), "ada");
        assert_eq!(member.sender.platform_id.as_ref(), "u1");
    }

    #[test]
    fn direct_chat_name_excludes_the_authenticated_user() {
        let members = vec![user("u-self", "Bogdan"), user("u1", "Kethe")];
        assert_eq!(
            direct_chat_display_name(&members, Some("u-self")).as_deref(),
            Some("Kethe")
        );
    }

    #[test]
    fn group_direct_chat_name_joins_every_counterpart() {
        let members = vec![
            user("u-self", "Bogdan"),
            user("u1", "Kethe"),
            user("u2", "Vlad"),
        ];
        assert_eq!(
            direct_chat_display_name(&members, Some("u-self")).as_deref(),
            Some("Kethe, Vlad")
        );
    }

    #[test]
    fn self_dm_keeps_the_self_name_and_blank_members_yield_nothing() {
        let members = vec![user("u-self", "Bogdan")];
        assert_eq!(
            direct_chat_display_name(&members, Some("u-self")).as_deref(),
            Some("Bogdan")
        );
        assert!(direct_chat_display_name(&[], Some("u-self")).is_none());
    }

    #[test]
    fn member_identity_renames_direct_chats_but_not_channels() {
        let account: ProviderId = arc_str("clickup:acct");
        let members = vec![user("u-self", "Bogdan"), user("u1", "Kethe")];

        let mut dm = channel("c1");
        dm.channel_kind = Some("DM".to_owned());
        let mut dm_chat = chat_from_channel(&account, &dm);
        assert_eq!(dm_chat.name.as_ref(), "Direct message");
        assert!(apply_direct_chat_identity(
            &mut dm_chat,
            &members,
            Some("u-self")
        ));
        assert_eq!(dm_chat.name.as_ref(), "Kethe");
        // Idempotent: a second pass with the same members changes nothing.
        assert!(!apply_direct_chat_identity(
            &mut dm_chat,
            &members,
            Some("u-self")
        ));

        let mut named = channel("c2");
        named.name = Some("general".to_owned());
        let mut channel_chat = chat_from_channel(&account, &named);
        assert!(!apply_direct_chat_identity(
            &mut channel_chat,
            &members,
            Some("u-self")
        ));
        assert_eq!(channel_chat.name.as_ref(), "#general");
    }

    #[test]
    fn sender_carries_the_resolved_avatar() {
        let mut resolved = user("u1", "Kethe");
        resolved.profile_picture = Some("https://cdn.example.com/kethe.jpg".to_owned());
        let sender = sender_from_user(&resolved);
        assert_eq!(sender.display_name.as_ref(), "Kethe");
        assert!(sender.avatar.is_some());

        // No profile picture must stay `None` so the UI renders initials.
        assert!(sender_from_user(&user("u2", "Vlad")).avatar.is_none());
    }

    #[test]
    fn sender_falls_back_to_user_id_label() {
        let sender = sender_from_user_id("u9", None);
        assert_eq!(sender.display_name.as_ref(), "User u9");
        let unknown = sender_from_user_id("", None);
        assert_eq!(unknown.display_name.as_ref(), "Unknown user");
    }
}
