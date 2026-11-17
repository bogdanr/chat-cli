//! Read-only aggregate queries over locally synced history that back the
//! details pane: chat activity stats, per-member stats and recency, shared
//! media/links, groups in common, linked identities, and local contact notes.
//!
//! Every query is scoped to a single account (and usually a single chat) and
//! runs as a handful of grouped passes over the `idx_messages_chat` range, so
//! callers can run them on a background worker without touching the draw path.
//! Stats only describe *synced* messages; [`ChatStats::total_messages`] and
//! [`ChatStats::oldest_at`] let the UI say how much history they cover.

use crate::{Store, arc_str, millis_to_timestamp, platform_from_str, string_to_path};
use anyhow::Result;
use chat_core::{ChatId, Platform, PlatformId, ProviderId, Timestamp};
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::HashMap, path::PathBuf, sync::Arc};

/// Content types counted as media attachments.
const MEDIA_TYPES_SQL: &str = "('image','video','audio','file','sticker')";
/// Gaps longer than this are not treated as a "reply" to the other side.
const RESPONSE_TIME_MAX_GAP_SECONDS: i64 = 7 * 24 * 60 * 60;
const SECONDS_PER_DAY: i64 = 86_400;
/// Messages from the same sender at most this far apart (chained) count as a
/// single activity, so a burst of short messages is not over-counted.
pub const ACTIVITY_MERGE_SECONDS: i64 = 60;
/// Stable key all of the signed-in user's sender ids are merged under. A
/// platform can store our own messages under several ids (WhatsApp phone JID,
/// privacy @lid, synthetic "me"), which must count as one person.
pub const ME_SENDER_KEY: &str = "\u{1}me";

/// Parameters for [`Store::chat_stats`].
#[derive(Clone, Copy, Debug)]
pub struct ChatStatsOptions {
    /// Only count messages at or after this time (`None` = all synced history).
    pub since: Option<Timestamp>,
    /// Local UTC offset used for day/hour bucketing.
    pub utc_offset_seconds: i32,
    /// Number of local days in [`ChatStats::daily_counts`] (ending at `now`).
    pub days: u32,
    /// Reference "now" for the daily histogram.
    pub now: Timestamp,
}

/// Count of attachments and links, plus the size of the attachments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaBreakdown {
    pub images: u64,
    pub videos: u64,
    pub audio: u64,
    pub files: u64,
    pub stickers: u64,
    pub links: u64,
    pub polls: u64,
    /// Sum of known attachment sizes in bytes.
    pub total_bytes: u64,
}

impl MediaBreakdown {
    /// Number of attachments (excludes links and polls).
    pub fn attachments(&self) -> u64 {
        self.images + self.videos + self.audio + self.files + self.stickers
    }
}

/// Activity of one sender within one chat.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SenderStats {
    pub sender_id: PlatformId,
    /// Display name from the sender's most recent message.
    pub display_name: Arc<str>,
    pub is_me: bool,
    pub messages: u64,
    /// Bursts of messages (see [`ACTIVITY_MERGE_SECONDS`]); what shares,
    /// ranks and histograms are based on.
    pub activities: u64,
    pub first_at: Option<Timestamp>,
    pub last_at: Option<Timestamp>,
    pub media: u64,
    /// Messages that are the root of a thread.
    pub threads_started: u64,
    /// Quoted replies and thread replies.
    pub replies: u64,
    /// Average length (characters) of text messages, when any.
    pub avg_text_len: Option<u32>,
    /// Messages by this sender that mention the signed-in user.
    pub mentions_me: u64,
    pub reactions_given: u64,
    pub reactions_received: u64,
    /// The emoji this sender reacts with most often.
    pub top_reaction: Option<Arc<str>>,
    /// Newest read receipt from this sender in this chat (not windowed).
    pub last_read_at: Option<Timestamp>,
    /// Activities per local hour of day.
    pub hourly_counts: [u32; 24],
    /// Median seconds this sender took to answer someone else's message.
    pub median_reply_seconds: Option<i64>,
}

impl SenderStats {
    /// Local hours with the most messages, busiest first (at most `limit`).
    pub fn top_hours(&self, limit: usize) -> Vec<u8> {
        top_indices(&self.hourly_counts, limit)
            .into_iter()
            .map(|hour| hour as u8)
            .collect()
    }
}

/// Aggregated activity for a chat over the synced (and optionally windowed)
/// history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatStats {
    pub total_messages: u64,
    pub my_messages: u64,
    /// Total activities (message bursts) across all senders.
    pub total_activities: u64,
    pub my_activities: u64,
    pub oldest_at: Option<Timestamp>,
    pub newest_at: Option<Timestamp>,
    /// Senders ordered by activity count (desc), then messages, then name.
    /// The signed-in user appears once (all own ids merged under
    /// [`ME_SENDER_KEY`]). People who only reacted or read (with
    /// `messages == 0`) come last.
    pub senders: Vec<SenderStats>,
    /// Activities per local day, oldest first, the last entry being "today".
    pub daily_counts: Vec<u32>,
    pub hourly_counts: [u32; 24],
    /// Activities per local weekday, Monday first.
    pub weekday_counts: [u32; 7],
    pub media: MediaBreakdown,
    /// Every sender id the signed-in user's own messages were stored under.
    pub my_sender_ids: Vec<PlatformId>,
}

impl ChatStats {
    /// Percentage (0-100) of all activities that `activities` represents.
    pub fn share_percent(&self, activities: u64) -> f64 {
        if self.total_activities == 0 {
            0.0
        } else {
            activities as f64 * 100.0 / self.total_activities as f64
        }
    }

    /// Number of senders with at least one message.
    pub fn active_senders(&self) -> usize {
        self.senders.iter().filter(|s| s.messages > 0).count()
    }

    /// A sender's stats plus their 1-based rank by activity (rank is `None`
    /// when they have no messages). Any of the signed-in user's ids, or
    /// [`ME_SENDER_KEY`], resolves to the merged "me" entry.
    pub fn sender(&self, sender_id: &str) -> Option<(Option<usize>, &SenderStats)> {
        let index = self
            .senders
            .iter()
            .position(|s| s.sender_id.as_ref() == sender_id)
            .or_else(|| {
                let is_mine = sender_id == ME_SENDER_KEY
                    || self.my_sender_ids.iter().any(|id| id.as_ref() == sender_id);
                is_mine
                    .then(|| self.senders.iter().position(|s| s.is_me))
                    .flatten()
            })?;
        let stats = &self.senders[index];
        let rank = (stats.messages > 0).then_some(index + 1);
        Some((rank, stats))
    }

    /// Like [`Self::sender`], but for a chat member: the signed-in user
    /// (`is_self`) always resolves to the merged "me" entry, even when the
    /// member id differs from every id our messages were stored under.
    pub fn member(
        &self,
        platform_id: &str,
        is_self: bool,
    ) -> Option<(Option<usize>, &SenderStats)> {
        if is_self {
            self.sender(ME_SENDER_KEY)
        } else {
            self.sender(platform_id)
        }
    }

    /// The local hour of day with the most activities.
    pub fn busiest_hour(&self) -> Option<u8> {
        top_indices(&self.hourly_counts, 1)
            .first()
            .map(|hour| *hour as u8)
    }

    /// The weekday (0 = Monday) with the most messages.
    pub fn busiest_weekday(&self) -> Option<u8> {
        top_indices(&self.weekday_counts, 1)
            .first()
            .map(|day| *day as u8)
    }
}

/// A chat a person has posted in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderChatActivity {
    pub chat_id: ChatId,
    pub chat_name: Option<Arc<str>>,
    pub messages: u64,
    pub last_at: Option<Timestamp>,
}

/// What a [`SharedItem`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedItemKind {
    Image,
    Video,
    Audio,
    File,
    Sticker,
    Link,
}

impl SharedItemKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "Photo",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::File => "File",
            Self::Sticker => "Sticker",
            Self::Link => "Link",
        }
    }
}

/// An attachment or link shared in a chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedItem {
    pub message_id: Arc<str>,
    pub sender_id: PlatformId,
    pub sender_name: Arc<str>,
    pub timestamp: Option<Timestamp>,
    pub kind: SharedItemKind,
    /// File name, caption, or URL.
    pub label: Arc<str>,
    pub size: Option<u64>,
    /// Downloaded file, when available locally.
    pub local_path: Option<PathBuf>,
    pub url: Option<Arc<str>>,
}

/// Why two identities are considered related.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkReason {
    /// Explicitly linked in the persons/handles directory.
    SamePerson,
    /// A different account has a sender with the exact same display name.
    SameName,
}

/// The same (or probably the same) person on another account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedIdentity {
    pub account: ProviderId,
    pub platform: Option<Platform>,
    pub platform_id: PlatformId,
    pub display_name: Arc<str>,
    pub reason: LinkReason,
    /// A direct chat with this identity, when one is stored.
    pub direct_chat_id: Option<ChatId>,
}

/// A person's newest locally synced message across every chat of an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastMessageSeen {
    pub at: Timestamp,
    pub chat_id: ChatId,
    pub chat_name: Option<Arc<str>>,
}

/// One person to look up in [`Store::last_messages_from`]: the result is
/// reported under `key`, and every id in `ids` (the key plus any known
/// aliases, e.g. a WhatsApp phone JID for an `@lid` member) is searched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastMessageQuery {
    pub key: PlatformId,
    pub ids: Vec<PlatformId>,
}

/// Private, local-only nickname and note for a contact.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContactNote {
    pub nickname: Option<Arc<str>>,
    pub note: Option<Arc<str>>,
}

impl ContactNote {
    pub fn is_empty(&self) -> bool {
        self.nickname.is_none() && self.note.is_none()
    }
}

impl Store {
    /// Aggregated activity statistics for one chat. See [`ChatStats`].
    pub async fn chat_stats(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
        options: ChatStatsOptions,
    ) -> Result<ChatStats> {
        let conn = self.conn.lock().await;
        chat_stats_on_conn(&conn, account_id, chat_id, options)
    }

    /// Chats (in this account) where `sender_id` has posted, most recent
    /// first. Scans the account's messages; run off the UI thread.
    pub async fn sender_chats(
        &self,
        account_id: &ProviderId,
        sender_id: &PlatformId,
        limit: usize,
    ) -> Result<Vec<SenderChatActivity>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT m.chat_id, c.name, COUNT(*), MAX(m.timestamp)
             FROM messages m
             LEFT JOIN chats c ON c.id = m.chat_id AND c.account_id = m.account_id
             WHERE m.account_id = ?1 AND m.sender_id = ?2 AND m.content_type != 'deleted'
             GROUP BY m.chat_id
             ORDER BY MAX(m.timestamp) DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![account_id.as_ref(), sender_id.as_ref(), limit as i64],
            |row| {
                Ok(SenderChatActivity {
                    chat_id: arc_str(row.get::<_, String>(0)?),
                    chat_name: row.get::<_, Option<String>>(1)?.map(arc_str),
                    messages: row.get::<_, i64>(2)?.max(0) as u64,
                    last_at: millis_to_timestamp(row.get::<_, Option<i64>>(3)?),
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Attachments and links shared in a chat (optionally by one sender),
    /// newest first.
    pub async fn shared_items(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
        sender_id: Option<&PlatformId>,
        limit: usize,
    ) -> Result<Vec<SharedItem>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, sender_id, sender_name, timestamp, content_type, content_text,
                    content_caption, media_filename, media_size, media_local
             FROM messages
             WHERE account_id = ?1 AND chat_id = ?2 AND (?3 IS NULL OR sender_id = ?3)
               AND (content_type IN {MEDIA_TYPES_SQL} OR content_type = 'link'
                    OR content_text LIKE '%http://%' OR content_text LIKE '%https://%')
             ORDER BY timestamp DESC
             LIMIT ?4"
        ))?;
        let rows = stmt.query_map(
            params![
                account_id.as_ref(),
                chat_id.as_ref(),
                sender_id.map(|id| id.as_ref()),
                limit as i64
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            },
        )?;
        let mut items = Vec::new();
        for row in rows {
            let (id, sender_id, sender_name, ts, kind, text, caption, filename, size, local) = row?;
            let kind = match kind.as_str() {
                "image" => SharedItemKind::Image,
                "video" => SharedItemKind::Video,
                "audio" => SharedItemKind::Audio,
                "file" => SharedItemKind::File,
                "sticker" => SharedItemKind::Sticker,
                _ => SharedItemKind::Link,
            };
            let url = if kind == SharedItemKind::Link {
                text.as_deref().and_then(first_url).map(arc_str)
            } else {
                None
            };
            if kind == SharedItemKind::Link && url.is_none() {
                continue;
            }
            let label = match kind {
                SharedItemKind::Link => url.clone().unwrap_or_else(|| arc_str(String::new())),
                _ => filename
                    .filter(|name| !name.trim().is_empty())
                    .or(caption.filter(|caption| !caption.trim().is_empty()))
                    .map(arc_str)
                    .unwrap_or_else(|| arc_str(kind.label().to_owned())),
            };
            items.push(SharedItem {
                message_id: arc_str(id),
                sender_id: arc_str(sender_id),
                sender_name: arc_str(sender_name),
                timestamp: millis_to_timestamp(Some(ts)),
                kind,
                label,
                size: size.and_then(|size| u64::try_from(size).ok()),
                local_path: string_to_path(local),
                url,
            });
        }
        Ok(items)
    }

    /// The same person on other accounts: explicit persons/handles links
    /// first, then senders on *other* accounts with the exact same display
    /// name (a weaker, clearly labelled heuristic).
    pub async fn linked_identities(
        &self,
        account_id: &ProviderId,
        platform_id: &PlatformId,
        display_name: &str,
        limit: usize,
    ) -> Result<Vec<LinkedIdentity>> {
        let conn = self.conn.lock().await;
        let mut identities: Vec<LinkedIdentity> = Vec::new();

        let person_id = conn
            .query_row(
                "SELECT person_id FROM handles WHERE account_id = ?1 AND platform_id = ?2 LIMIT 1",
                params![account_id.as_ref(), platform_id.as_ref()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(person_id) = person_id {
            let mut stmt = conn.prepare(
                "SELECT account_id, platform, platform_id, display_name FROM handles
                 WHERE person_id = ?1 AND NOT (account_id = ?2 AND platform_id = ?3)",
            )?;
            let rows = stmt.query_map(
                params![person_id, account_id.as_ref(), platform_id.as_ref()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?;
            for row in rows {
                let (account, platform, id, name) = row?;
                identities.push(LinkedIdentity {
                    account: arc_str(account),
                    platform: Some(platform_from_str(&platform)),
                    platform_id: arc_str(id),
                    display_name: arc_str(name),
                    reason: LinkReason::SamePerson,
                    direct_chat_id: None,
                });
            }
        }

        let name = display_name.trim();
        // Very short names ("Me", "Al") match too many unrelated people.
        if name.chars().count() >= 3 && identities.len() < limit {
            let mut stmt = conn.prepare(
                "SELECT m.account_id, a.platform, m.sender_id, m.sender_name, MAX(m.timestamp)
                 FROM messages m
                 LEFT JOIN accounts a ON a.id = m.account_id
                 WHERE m.sender_name = ?1 COLLATE NOCASE AND m.account_id != ?2
                   AND m.is_from_me = 0
                 GROUP BY m.account_id, m.sender_id
                 ORDER BY MAX(m.timestamp) DESC
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![name, account_id.as_ref(), limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (account, platform, id, sender_name) = row?;
                if identities.iter().any(|known| {
                    known.account.as_ref() == account && known.platform_id.as_ref() == id
                }) {
                    continue;
                }
                identities.push(LinkedIdentity {
                    account: arc_str(account),
                    platform: platform.as_deref().map(platform_from_str),
                    platform_id: arc_str(id),
                    display_name: arc_str(sender_name),
                    reason: LinkReason::SameName,
                    direct_chat_id: None,
                });
            }
        }
        identities.truncate(limit);
        for identity in &mut identities {
            identity.direct_chat_id =
                direct_chat_on_conn(&conn, &identity.account, &identity.platform_id)?;
        }
        Ok(identities)
    }

    /// The stored direct chat with a user, if any: a direct chat whose id is
    /// the user id, or in which the user has posted.
    pub async fn direct_chat_with(
        &self,
        account_id: &ProviderId,
        platform_id: &PlatformId,
    ) -> Result<Option<ChatId>> {
        let conn = self.conn.lock().await;
        direct_chat_on_conn(&conn, account_id, platform_id)
    }

    /// Newest non-deleted message sent by each queried person in *any* chat
    /// of the account. Each id costs one seek on `idx_messages_sender` plus
    /// one on `idx_messages_chat` (their direct chat, whose id is the user id
    /// on platforms like WhatsApp), so a bounded batch stays cheap on large
    /// histories. People with no local messages are absent from the result.
    pub async fn last_messages_from(
        &self,
        account_id: &ProviderId,
        queries: &[LastMessageQuery],
    ) -> Result<HashMap<PlatformId, LastMessageSeen>> {
        let conn = self.conn.lock().await;
        let mut by_sender = conn.prepare_cached(
            "SELECT timestamp, chat_id FROM messages
             WHERE account_id = ?1 AND sender_id = ?2 AND is_from_me = 0
               AND content_type != 'deleted'
             ORDER BY timestamp DESC LIMIT 1",
        )?;
        let mut in_direct_chat = conn.prepare_cached(
            "SELECT timestamp, chat_id FROM messages
             WHERE chat_id = ?2 AND account_id = ?1 AND is_from_me = 0
               AND content_type != 'deleted'
             ORDER BY timestamp DESC LIMIT 1",
        )?;
        let mut chat_name =
            conn.prepare_cached("SELECT name FROM chats WHERE account_id = ?1 AND id = ?2")?;
        let account = account_id.as_ref();
        let mut found = HashMap::new();
        for query in queries {
            let mut newest: Option<(i64, String)> = None;
            for id in &query.ids {
                for stmt in [&mut by_sender, &mut in_direct_chat] {
                    let row = stmt
                        .query_row(params![account, id.as_ref()], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })
                        .optional()?;
                    if let Some(row) = row
                        && newest.as_ref().is_none_or(|best| row.0 > best.0)
                    {
                        newest = Some(row);
                    }
                }
            }
            let Some((millis, chat_id)) = newest else {
                continue;
            };
            let Some(at) = millis_to_timestamp(Some(millis)) else {
                continue;
            };
            let name = chat_name
                .query_row(params![account, chat_id], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .optional()?
                .flatten()
                .filter(|name| !name.trim().is_empty())
                .map(arc_str);
            found.insert(
                query.key.clone(),
                LastMessageSeen {
                    at,
                    chat_id: arc_str(chat_id),
                    chat_name: name,
                },
            );
        }
        Ok(found)
    }

    /// Local nickname/note for a contact.
    pub async fn contact_note(
        &self,
        account_id: &ProviderId,
        platform_id: &PlatformId,
    ) -> Result<ContactNote> {
        let conn = self.conn.lock().await;
        let note = conn
            .query_row(
                "SELECT nickname, note FROM contact_notes WHERE account_id = ?1 AND platform_id = ?2",
                params![account_id.as_ref(), platform_id.as_ref()],
                |row| {
                    Ok(ContactNote {
                        nickname: row.get::<_, Option<String>>(0)?.map(arc_str),
                        note: row.get::<_, Option<String>>(1)?.map(arc_str),
                    })
                },
            )
            .optional()?;
        Ok(note.unwrap_or_default())
    }

    /// Saves (or clears, when empty) a contact's local nickname/note.
    pub async fn set_contact_note(
        &self,
        account_id: &ProviderId,
        platform_id: &PlatformId,
        note: &ContactNote,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let clean = |value: &Option<Arc<str>>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let nickname = clean(&note.nickname);
        let text = clean(&note.note);
        if nickname.is_none() && text.is_none() {
            conn.execute(
                "DELETE FROM contact_notes WHERE account_id = ?1 AND platform_id = ?2",
                params![account_id.as_ref(), platform_id.as_ref()],
            )?;
            return Ok(());
        }
        conn.execute(
            "INSERT INTO contact_notes (account_id, platform_id, nickname, note, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, platform_id) DO UPDATE SET
                nickname = excluded.nickname, note = excluded.note, updated_at = excluded.updated_at",
            params![
                account_id.as_ref(),
                platform_id.as_ref(),
                nickname,
                text,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(())
    }

    /// All local nicknames for an account, keyed by platform id.
    pub async fn contact_nicknames(
        &self,
        account_id: &ProviderId,
    ) -> Result<HashMap<PlatformId, Arc<str>>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT platform_id, nickname FROM contact_notes
             WHERE account_id = ?1 AND nickname IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![account_id.as_ref()], |row| {
            Ok((
                arc_str(row.get::<_, String>(0)?),
                arc_str(row.get::<_, String>(1)?),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }
}

fn direct_chat_on_conn(
    conn: &Connection,
    account_id: &ProviderId,
    platform_id: &PlatformId,
) -> Result<Option<ChatId>> {
    let id = conn
        .query_row(
            "SELECT c.id FROM chats c
             WHERE c.account_id = ?1 AND c.kind = 'direct'
               AND (c.id = ?2 OR EXISTS (
                    SELECT 1 FROM messages m
                    WHERE m.account_id = c.account_id AND m.chat_id = c.id
                      AND m.sender_id = ?2 AND m.is_from_me = 0))
             ORDER BY c.id = ?2 DESC
             LIMIT 1",
            params![account_id.as_ref(), platform_id.as_ref()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(id.map(arc_str))
}

/// Local day number and hour for a millisecond timestamp.
fn local_day_hour(millis: i64, offset: i64) -> (i64, usize) {
    let seconds = millis.div_euclid(1000) + offset;
    let day = seconds.div_euclid(SECONDS_PER_DAY);
    let hour = (seconds.rem_euclid(SECONDS_PER_DAY) / 3600) as usize;
    (day, hour)
}

/// Weekday (0 = Monday) of a local day number (1970-01-01 was a Thursday).
fn weekday_of_day(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

fn top_indices(counts: &[u32], limit: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
    indices.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then(a.cmp(&b)));
    indices.truncate(limit);
    indices
}

fn first_url(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let url: String = text[start..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !matches!(c, '>' | '<' | '"' | '|'))
        .collect();
    let url = url.trim_end_matches(['.', ',', ')', ';', ':', '!', '?']);
    (url.len() > "https://".len()).then(|| url.to_owned())
}

fn median(values: &mut [i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2
    } else {
        values[mid]
    })
}

pub(crate) fn chat_stats_on_conn(
    conn: &Connection,
    account_id: &ProviderId,
    chat_id: &ChatId,
    options: ChatStatsOptions,
) -> Result<ChatStats> {
    let account = account_id.as_ref();
    let chat = chat_id.as_ref();
    let since = options
        .since
        .map(|since| since.timestamp_millis())
        .unwrap_or(i64::MIN);
    let offset = i64::from(options.utc_offset_seconds);
    let scope =
        "account_id = ?1 AND chat_id = ?2 AND timestamp >= ?3 AND content_type != 'deleted'";

    let mut stats = ChatStats::default();
    // Keyed by merged sender key: every own id collapses into ME_SENDER_KEY.
    let mut by_sender: HashMap<String, SenderStats> = HashMap::new();
    let mut my_ids: Vec<String> = Vec::new();

    // 1. Per-sender totals, grouped by (sender_id, is_from_me) and merged so
    //    the signed-in user's several ids count as one person.
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT sender_id, is_from_me, COUNT(*), MIN(timestamp), MAX(timestamp),
                    SUM(CASE WHEN content_type IN {MEDIA_TYPES_SQL} THEN 1 ELSE 0 END),
                    SUM(CASE WHEN thread_id IS NOT NULL AND thread_id = id THEN 1 ELSE 0 END),
                    SUM(CASE WHEN reply_to_id IS NOT NULL
                              OR (thread_id IS NOT NULL AND thread_id != id) THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'text' AND content_text IS NOT NULL
                             THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'text' AND content_text IS NOT NULL
                             THEN LENGTH(content_text) ELSE 0 END),
                    SUM(mentions_me)
             FROM messages WHERE {scope}
             GROUP BY sender_id, is_from_me"
        ))?;
        let rows = stmt.query_map(params![account, chat, since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
            ))
        })?;
        // Text message count and total length per key, for the weighted average.
        let mut text_lengths: HashMap<String, (i64, i64)> = HashMap::new();
        for row in rows {
            let (id, is_me, count, first, last, media, threads, replies, texts, text_len, mentions) =
                row?;
            let count = count.max(0) as u64;
            stats.total_messages += count;
            if is_me {
                stats.my_messages += count;
                if !my_ids.contains(&id) {
                    my_ids.push(id.clone());
                }
            }
            let key = sender_key(&id, is_me);
            let entry = by_sender.entry(key.clone()).or_insert_with(|| SenderStats {
                sender_id: arc_str(key.clone()),
                display_name: arc_str(id.clone()),
                is_me,
                ..SenderStats::default()
            });
            entry.messages += count;
            entry.first_at = min_timestamp(entry.first_at, millis_to_timestamp(first));
            entry.last_at = max_timestamp(entry.last_at, millis_to_timestamp(last));
            entry.media += media.unwrap_or(0).max(0) as u64;
            entry.threads_started += threads.unwrap_or(0).max(0) as u64;
            entry.replies += replies.unwrap_or(0).max(0) as u64;
            entry.mentions_me += mentions.unwrap_or(0).max(0) as u64;
            let lengths = text_lengths.entry(key).or_default();
            lengths.0 += texts.unwrap_or(0).max(0);
            lengths.1 += text_len.unwrap_or(0).max(0);
        }
        for (key, (texts, total_len)) in text_lengths {
            if texts > 0
                && let Some(entry) = by_sender.get_mut(&key)
            {
                entry.avg_text_len = Some((total_len as f64 / texts as f64).round() as u32);
            }
        }
    }
    my_ids.sort();
    let resolve = |id: &str| -> String {
        if my_ids.iter().any(|mine| mine == id) {
            ME_SENDER_KEY.to_owned()
        } else {
            id.to_owned()
        }
    };

    // 2. Latest display name per (sender_id, is_from_me); the newest wins
    //    after merging. A single MAX aggregate makes SQLite take the bare
    //    `sender_name` from the newest row.
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT sender_id, is_from_me, sender_name, MAX(timestamp) FROM messages WHERE {scope}
             GROUP BY sender_id, is_from_me"
        ))?;
        let rows = stmt.query_map(params![account, chat, since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut newest_name: HashMap<String, i64> = HashMap::new();
        for row in rows {
            let (id, is_me, name, at) = row?;
            if name.trim().is_empty() {
                continue;
            }
            let key = sender_key(&id, is_me);
            let newest = newest_name.entry(key.clone()).or_insert(i64::MIN);
            if at >= *newest
                && let Some(sender) = by_sender.get_mut(&key)
            {
                *newest = at;
                sender.display_name = arc_str(name);
            }
        }
    }

    // 3. One ordered pass over the chat range: activity bursts (messages
    //    from the same sender chained within ACTIVITY_MERGE_SECONDS count
    //    once), the day/hour/weekday histograms of those activities,
    //    coverage, and reply gaps (whenever the author changes, the gap is
    //    attributed to the new author). Streams rows; nothing is collected.
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT sender_id, is_from_me, timestamp FROM messages WHERE {scope}
             ORDER BY timestamp"
        ))?;
        let mut rows = stmt.query(params![account, chat, since])?;
        let days = options.days as i64;
        let (today, _) = local_day_hour(options.now.timestamp_millis(), offset);
        stats.daily_counts = vec![0; options.days as usize];
        let merge_millis = ACTIVITY_MERGE_SECONDS * 1000;
        let mut last_by_key: HashMap<String, i64> = HashMap::new();
        let mut previous: Option<(String, i64)> = None;
        let mut gaps: HashMap<String, Vec<i64>> = HashMap::new();
        let mut oldest: Option<i64> = None;
        let mut newest: Option<i64> = None;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let is_me = row.get::<_, i64>(1)? != 0;
            let ts: i64 = row.get(2)?;
            let key = sender_key(&id, is_me);
            oldest = Some(oldest.map_or(ts, |o| o.min(ts)));
            newest = Some(newest.map_or(ts, |n| n.max(ts)));

            let starts_activity = last_by_key
                .get(&key)
                .is_none_or(|last| ts - last > merge_millis);
            last_by_key.insert(key.clone(), ts);
            if starts_activity {
                let (day, hour) = local_day_hour(ts, offset);
                stats.total_activities += 1;
                if is_me {
                    stats.my_activities += 1;
                }
                stats.hourly_counts[hour] += 1;
                stats.weekday_counts[weekday_of_day(day)] += 1;
                let age = today - day;
                if (0..days).contains(&age) {
                    stats.daily_counts[(days - 1 - age) as usize] += 1;
                }
                if let Some(sender) = by_sender.get_mut(&key) {
                    sender.activities += 1;
                    sender.hourly_counts[hour] += 1;
                }
            }

            if let Some((previous_key, previous_ts)) = &previous
                && *previous_key != key
            {
                let gap = (ts - previous_ts) / 1000;
                if (0..=RESPONSE_TIME_MAX_GAP_SECONDS).contains(&gap) {
                    gaps.entry(key.clone()).or_default().push(gap);
                }
            }
            previous = Some((key, ts));
        }
        for (key, mut values) in gaps {
            if let Some(entry) = by_sender.get_mut(&key) {
                entry.median_reply_seconds = median(&mut values);
            }
        }
        stats.oldest_at = millis_to_timestamp(oldest);
        stats.newest_at = millis_to_timestamp(newest);
    }

    // 4. Media breakdown.
    {
        stats.media = conn.query_row(
            &format!(
                "SELECT
                    SUM(CASE WHEN content_type = 'image' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'video' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'audio' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'file' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'sticker' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'link'
                              OR content_text LIKE '%http://%'
                              OR content_text LIKE '%https://%' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type = 'poll' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN content_type IN {MEDIA_TYPES_SQL} THEN COALESCE(media_size, 0) ELSE 0 END)
                 FROM messages WHERE {scope}"
            ),
            params![account, chat, since],
            |row| {
                let get = |index: usize| -> rusqlite::Result<u64> {
                    Ok(row.get::<_, Option<i64>>(index)?.unwrap_or(0).max(0) as u64)
                };
                Ok(MediaBreakdown {
                    images: get(0)?,
                    videos: get(1)?,
                    audio: get(2)?,
                    files: get(3)?,
                    stickers: get(4)?,
                    links: get(5)?,
                    polls: get(6)?,
                    total_bytes: get(7)?,
                })
            },
        )?;
    }

    // 5. Reactions given (with emoji, for the favourite) and received.
    {
        let mut stmt = conn.prepare(
            "SELECT r.sender_id, r.emoji, COUNT(*)
             FROM messages m
             JOIN reactions r ON r.message_id = m.id AND r.account_id = m.account_id
             WHERE m.account_id = ?1 AND m.chat_id = ?2 AND m.timestamp >= ?3
             GROUP BY r.sender_id, r.emoji",
        )?;
        let rows = stmt.query_map(params![account, chat, since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut per_emoji: HashMap<String, HashMap<String, i64>> = HashMap::new();
        for row in rows {
            let (sender, emoji, count) = row?;
            let key = resolve(&sender);
            let entry = by_sender
                .entry(key.clone())
                .or_insert_with(|| empty_sender(&key));
            entry.reactions_given += count.max(0) as u64;
            *per_emoji.entry(key).or_default().entry(emoji).or_insert(0) += count;
        }
        for (key, emojis) in per_emoji {
            let favourite = emojis
                .into_iter()
                .max_by(|(a_emoji, a), (b_emoji, b)| a.cmp(b).then_with(|| b_emoji.cmp(a_emoji)));
            if let (Some((emoji, _)), Some(entry)) = (favourite, by_sender.get_mut(&key)) {
                entry.top_reaction = Some(arc_str(emoji));
            }
        }

        let mut stmt = conn.prepare(
            "SELECT m.sender_id, m.is_from_me, COUNT(*)
             FROM messages m
             JOIN reactions r ON r.message_id = m.id AND r.account_id = m.account_id
             WHERE m.account_id = ?1 AND m.chat_id = ?2 AND m.timestamp >= ?3
             GROUP BY m.sender_id, m.is_from_me",
        )?;
        let rows = stmt.query_map(params![account, chat, since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? != 0,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (sender, is_me, count) = row?;
            if let Some(entry) = by_sender.get_mut(&sender_key(&sender, is_me)) {
                entry.reactions_received += count.max(0) as u64;
            }
        }
    }

    // 6. Last read receipt per reader (recency, so not windowed).
    {
        let mut stmt = conn.prepare(
            "SELECT rc.sender_id, MAX(rc.at)
             FROM messages m
             JOIN receipts rc ON rc.message_id = m.id AND rc.account_id = m.account_id
             WHERE m.account_id = ?1 AND m.chat_id = ?2 AND rc.kind = 'read' AND rc.at IS NOT NULL
             GROUP BY rc.sender_id",
        )?;
        let rows = stmt.query_map(params![account, chat], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })?;
        for row in rows {
            let (reader, at) = row?;
            let key = resolve(&reader);
            let entry = by_sender
                .entry(key.clone())
                .or_insert_with(|| empty_sender(&key));
            entry.last_read_at = max_timestamp(entry.last_read_at, millis_to_timestamp(at));
        }
    }

    if let Some(me) = by_sender.get_mut(ME_SENDER_KEY) {
        me.is_me = true;
    }
    let mut senders: Vec<SenderStats> = by_sender.into_values().collect();
    senders.sort_by(|a, b| {
        b.activities
            .cmp(&a.activities)
            .then_with(|| b.messages.cmp(&a.messages))
            .then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
            .then_with(|| a.sender_id.cmp(&b.sender_id))
    });
    stats.senders = senders;
    stats.my_sender_ids = my_ids.into_iter().map(arc_str).collect();
    Ok(stats)
}

/// Merged stats key: all of the signed-in user's ids share one key.
fn sender_key(id: &str, is_me: bool) -> String {
    if is_me {
        ME_SENDER_KEY.to_owned()
    } else {
        id.to_owned()
    }
}

fn min_timestamp(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_timestamp(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn empty_sender(id: &str) -> SenderStats {
    SenderStats {
        sender_id: arc_str(id.to_owned()),
        display_name: arc_str(id.to_owned()),
        ..SenderStats::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_core::{
        Chat, ChatKind, ChatMembership, Content, Media, Message, PlatformData, Reaction, Receipt,
        ReceiptKind, Sender,
    };
    use chrono::{TimeZone, Utc};

    fn ts(seconds: i64) -> Timestamp {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    fn message(id: &str, chat: &str, sender: &str, at: i64, content: Content) -> Message {
        Message {
            id: Arc::from(id),
            chat_id: Arc::from(chat),
            account: Arc::from("acct"),
            sender: Sender {
                platform_id: Arc::from(sender),
                display_name: Arc::from(sender.to_uppercase()),
                avatar: None,
            },
            timestamp: ts(at),
            edited_at: None,
            content,
            reply_to: None,
            reply_preview: None,
            thread_id: None,
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me: sender == "me",
            mentions_me: false,
            platform_data: PlatformData::default(),
        }
    }

    fn text(value: &str) -> Content {
        Content::Text(Arc::from(value))
    }

    fn media(size: u64) -> Media {
        Media {
            id: Arc::from("m"),
            mime_type: Arc::from("image/png"),
            file_name: Arc::from("cat.png"),
            size_bytes: Some(size),
            caption: None,
            local_path: None,
            thumbnail: None,
        }
    }

    fn options(now: i64, offset: i32) -> ChatStatsOptions {
        ChatStatsOptions {
            since: None,
            utc_offset_seconds: offset,
            days: 7,
            now: ts(now),
        }
    }

    const DAY: i64 = 86_400;
    // 2024-01-01T00:00:00Z, a Monday.
    const MONDAY: i64 = 1_704_067_200;

    #[tokio::test]
    async fn sender_counts_and_shares_add_up() -> Result<()> {
        let store = Store::open_memory().await?;
        let mut messages = vec![
            message("1", "c", "alice", MONDAY + 10 * 3600, text("hello there")),
            message("2", "c", "alice", MONDAY + 11 * 3600, text("hi")),
            message("3", "c", "bob", MONDAY + 12 * 3600, text("yo")),
            message(
                "4",
                "c",
                "me",
                MONDAY + 13 * 3600,
                Content::Image(media(1000)),
            ),
            message("5", "other", "alice", MONDAY, text("elsewhere")),
            message("6", "c", "alice", MONDAY + 14 * 3600, Content::Deleted),
        ];
        messages[2].reply_to = Some(Arc::from("1"));
        store.upsert_messages(&messages).await?;

        let stats = store
            .chat_stats(
                &Arc::from("acct"),
                &Arc::from("c"),
                options(MONDAY + DAY, 0),
            )
            .await?;
        assert_eq!(stats.total_messages, 4, "deleted and other chats excluded");
        assert_eq!(stats.my_messages, 1);
        assert_eq!(stats.active_senders(), 3);
        let (rank, alice) = stats.sender("alice").expect("alice stats");
        assert_eq!(rank, Some(1));
        assert_eq!(alice.messages, 2);
        assert_eq!(alice.display_name.as_ref(), "ALICE");
        assert_eq!(alice.avg_text_len, Some(7));
        assert_eq!(alice.activities, 2);
        assert_eq!(stats.share_percent(alice.activities), 50.0);
        let total: f64 = stats
            .senders
            .iter()
            .map(|s| stats.share_percent(s.activities))
            .sum();
        assert!((total - 100.0).abs() < 0.001);
        assert_eq!(stats.sender("bob").unwrap().1.replies, 1);
        assert_eq!(stats.media.images, 1);
        assert_eq!(stats.media.total_bytes, 1000);
        assert_eq!(stats.oldest_at, Some(ts(MONDAY + 10 * 3600)));
        assert_eq!(stats.newest_at, Some(ts(MONDAY + 13 * 3600)));
        Ok(())
    }

    #[tokio::test]
    async fn time_window_excludes_older_messages() -> Result<()> {
        let store = Store::open_memory().await?;
        store
            .upsert_messages(&[
                message("1", "c", "alice", MONDAY - 10 * DAY, text("old")),
                message("2", "c", "bob", MONDAY, text("new")),
            ])
            .await?;
        let stats = store
            .chat_stats(
                &Arc::from("acct"),
                &Arc::from("c"),
                ChatStatsOptions {
                    since: Some(ts(MONDAY - DAY)),
                    ..options(MONDAY, 0)
                },
            )
            .await?;
        assert_eq!(stats.total_messages, 1);
        assert!(stats.sender("alice").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn histograms_bucket_by_local_time() -> Result<()> {
        let store = Store::open_memory().await?;
        // 23:30 UTC on Monday is 01:30 Tuesday at UTC+2.
        store
            .upsert_messages(&[
                message("1", "c", "alice", MONDAY + 23 * 3600 + 1800, text("late")),
                message("2", "c", "alice", MONDAY + 2 * DAY, text("wed")),
            ])
            .await?;
        let utc = store
            .chat_stats(
                &Arc::from("acct"),
                &Arc::from("c"),
                options(MONDAY + 2 * DAY, 0),
            )
            .await?;
        assert_eq!(utc.hourly_counts[23], 1);
        assert_eq!(utc.weekday_counts[0], 1, "Monday in UTC");
        assert_eq!(utc.daily_counts, vec![0, 0, 0, 0, 1, 0, 1]);

        let plus_two = store
            .chat_stats(
                &Arc::from("acct"),
                &Arc::from("c"),
                options(MONDAY + 2 * DAY, 2 * 3600),
            )
            .await?;
        assert_eq!(plus_two.hourly_counts[1], 1);
        assert_eq!(plus_two.weekday_counts[1], 1, "Tuesday at UTC+2");
        assert_eq!(plus_two.daily_counts, vec![0, 0, 0, 0, 0, 1, 1]);
        assert_eq!(plus_two.sender("alice").unwrap().1.top_hours(1), vec![1]);
        Ok(())
    }

    #[tokio::test]
    async fn reactions_and_read_receipts_join_per_sender() -> Result<()> {
        let store = Store::open_memory().await?;
        let mut first = message("1", "c", "alice", MONDAY, text("hi"));
        first.reactions = vec![
            Reaction {
                emoji: Arc::from("👍"),
                senders: vec![Arc::from("bob"), Arc::from("carol")],
            },
            Reaction {
                emoji: Arc::from("😂"),
                senders: vec![Arc::from("bob")],
            },
        ];
        let mut second = message("2", "c", "me", MONDAY + 60, text("hey"));
        second.reactions = vec![Reaction {
            emoji: Arc::from("😂"),
            senders: vec![Arc::from("bob")],
        }];
        second.receipts = vec![Receipt {
            platform_id: Arc::from("alice"),
            kind: ReceiptKind::Read,
            at: Some(ts(MONDAY + 120)),
        }];
        store.upsert_messages(&[first, second]).await?;
        let stats = store
            .chat_stats(&Arc::from("acct"), &Arc::from("c"), options(MONDAY, 0))
            .await?;
        let bob = stats.sender("bob").expect("reactor is listed").1;
        assert_eq!(bob.messages, 0);
        assert_eq!(bob.reactions_given, 3);
        assert_eq!(bob.top_reaction.as_deref(), Some("😂"));
        assert_eq!(
            stats.sender("bob").unwrap().0,
            None,
            "no rank without messages"
        );
        let alice = stats.sender("alice").unwrap().1;
        assert_eq!(alice.reactions_received, 3);
        assert_eq!(alice.last_read_at, Some(ts(MONDAY + 120)));
        assert_eq!(alice.median_reply_seconds, None);
        assert_eq!(stats.sender("me").unwrap().1.median_reply_seconds, Some(60));
        Ok(())
    }

    #[tokio::test]
    async fn shared_items_groups_in_common_and_notes() -> Result<()> {
        let store = Store::open_memory().await?;
        store
            .upsert_account(
                &chat_core::Account {
                    id: Arc::from("acct"),
                    platform: Platform::WhatsApp,
                    display_name: Arc::from("Me"),
                    avatar: None,
                },
                "{}",
            )
            .await?;
        store
            .upsert_chat(&Chat {
                id: Arc::from("c"),
                account: Arc::from("acct"),
                platform: Platform::WhatsApp,
                name: Arc::from("Climbing"),
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
                last_read_at: None,
            })
            .await?;
        store
            .upsert_messages(&[
                message(
                    "1",
                    "c",
                    "alice",
                    MONDAY,
                    text("see https://example.com/x."),
                ),
                message("2", "c", "alice", MONDAY + 1, Content::Image(media(5))),
                message("3", "d", "alice", MONDAY + 2, text("plain")),
                message("4", "c", "bob", MONDAY + 3, text("no link")),
            ])
            .await?;
        let items = store
            .shared_items(&Arc::from("acct"), &Arc::from("c"), None, 10)
            .await?;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, SharedItemKind::Image);
        assert_eq!(items[0].label.as_ref(), "cat.png");
        assert_eq!(items[1].url.as_deref(), Some("https://example.com/x"));

        let chats = store
            .sender_chats(&Arc::from("acct"), &Arc::from("alice"), 10)
            .await?;
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0].chat_id.as_ref(), "d");
        assert_eq!(chats[1].chat_name.as_deref(), Some("Climbing"));
        assert_eq!(chats[1].messages, 2);

        let account: ProviderId = Arc::from("acct");
        let alice: PlatformId = Arc::from("alice");
        assert!(store.contact_note(&account, &alice).await?.is_empty());
        let note = ContactNote {
            nickname: Some(Arc::from(" Al ")),
            note: Some(Arc::from("Met at the climbing gym")),
        };
        store.set_contact_note(&account, &alice, &note).await?;
        let saved = store.contact_note(&account, &alice).await?;
        assert_eq!(saved.nickname.as_deref(), Some("Al"));
        assert_eq!(
            store
                .contact_nicknames(&account)
                .await?
                .get(&alice)
                .map(|n| n.as_ref()),
            Some("Al")
        );
        store
            .set_contact_note(&account, &alice, &ContactNote::default())
            .await?;
        assert!(store.contact_note(&account, &alice).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn linked_identities_match_same_name_on_other_accounts() -> Result<()> {
        let store = Store::open_memory().await?;
        let mut elsewhere = message("1", "dm", "U123", MONDAY, text("hi"));
        elsewhere.account = Arc::from("slack");
        elsewhere.sender.display_name = Arc::from("Alice Chen");
        let mut here = message("2", "c", "alice", MONDAY, text("hi"));
        here.sender.display_name = Arc::from("Alice Chen");
        store.upsert_messages(&[elsewhere, here]).await?;
        let linked = store
            .linked_identities(&Arc::from("acct"), &Arc::from("alice"), "alice chen", 5)
            .await?;
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].account.as_ref(), "slack");
        assert_eq!(linked[0].platform_id.as_ref(), "U123");
        assert_eq!(linked[0].reason, LinkReason::SameName);
        let short = store
            .linked_identities(&Arc::from("acct"), &Arc::from("alice"), "Al", 5)
            .await?;
        assert!(short.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn own_messages_under_several_ids_merge_into_one_sender() -> Result<()> {
        let store = Store::open_memory().await?;
        // WhatsApp can store our own messages under the phone JID, the
        // privacy @lid and a synthetic "me" id.
        let mut ids = Vec::new();
        for (index, sender) in ["447700900000@s.whatsapp.net", "123@lid", "me"]
            .into_iter()
            .enumerate()
        {
            let mut own = message(
                &format!("m{index}"),
                "c",
                sender,
                MONDAY + index as i64 * 3600,
                text("mine"),
            );
            own.is_from_me = true;
            own.sender.display_name = Arc::from("Me");
            ids.push(own);
        }
        ids.push(message("a", "c", "alice", MONDAY + 5 * 3600, text("hi")));
        store.upsert_messages(&ids).await?;

        let stats = store
            .chat_stats(
                &Arc::from("acct"),
                &Arc::from("c"),
                options(MONDAY + DAY, 0),
            )
            .await?;
        let mine: Vec<_> = stats.senders.iter().filter(|s| s.is_me).collect();
        assert_eq!(
            mine.len(),
            1,
            "own ids collapse into one entry: {:?}",
            stats.senders
        );
        assert_eq!(mine[0].messages, 3);
        assert_eq!(mine[0].activities, 3);
        assert_eq!(stats.senders.len(), 2);
        assert_eq!(stats.my_sender_ids.len(), 3);
        for id in ["123@lid", "me", ME_SENDER_KEY] {
            assert!(stats.sender(id).unwrap().1.is_me, "{id} resolves to me");
        }
        assert_eq!(stats.sender("123@lid").unwrap().0, Some(1));
        assert_eq!(stats.my_activities, 3);
        assert_eq!(stats.share_percent(stats.my_activities), 75.0);
        Ok(())
    }

    #[tokio::test]
    async fn message_bursts_count_as_one_activity() -> Result<()> {
        let store = Store::open_memory().await?;
        store
            .upsert_messages(&[
                // Alice: a chained burst (each gap <= 60s) = 1 activity,
                // then a separate message 2 minutes later = 1 more.
                message("1", "c", "alice", MONDAY, text("a")),
                message("2", "c", "alice", MONDAY + 30, text("b")),
                message("3", "c", "alice", MONDAY + 85, text("c")),
                message("4", "c", "alice", MONDAY + 205, text("d")),
                // Bob interleaves but does not split Alice's burst.
                message("5", "c", "bob", MONDAY + 40, text("x")),
                message("6", "c", "bob", MONDAY + 100, text("y")),
            ])
            .await?;
        let stats = store
            .chat_stats(&Arc::from("acct"), &Arc::from("c"), options(MONDAY, 0))
            .await?;
        assert_eq!(stats.total_messages, 6);
        assert_eq!(stats.total_activities, 3);
        let (rank, alice) = stats.sender("alice").unwrap();
        assert_eq!((rank, alice.messages, alice.activities), (Some(1), 4, 2));
        let bob = stats.sender("bob").unwrap().1;
        assert_eq!((bob.messages, bob.activities), (2, 1));
        assert_eq!(stats.hourly_counts[0], 3, "histograms count activities");
        assert_eq!(stats.daily_counts.last(), Some(&3));
        Ok(())
    }

    #[tokio::test]
    async fn last_messages_from_spans_chats_aliases_and_direct_chats() -> Result<()> {
        let store = Store::open_memory().await?;
        let mut other_account = message("x", "c", "alice", MONDAY + 900, text("elsewhere"));
        other_account.account = Arc::from("other");
        let mut own = message("o", "c", "alice", MONDAY + 800, text("mine"));
        own.is_from_me = true;
        store
            .upsert_messages(&[
                message("1", "group-a", "alice", MONDAY, text("old")),
                message("2", "group-b", "alice", MONDAY + 300, text("newest")),
                message("3", "group-b", "alice", MONDAY + 600, Content::Deleted),
                other_account,
                own,
                // Bob is stored as an @lid in groups but his phone JID is
                // the id of the direct chat.
                message("4", "group-a", "123@lid", MONDAY + 100, text("lid")),
                message(
                    "5",
                    "4477@s.whatsapp.net",
                    "4477@s.whatsapp.net",
                    MONDAY + 200,
                    text("dm"),
                ),
            ])
            .await?;
        let query = |key: &str, ids: &[&str]| LastMessageQuery {
            key: Arc::from(key),
            ids: ids.iter().map(|id| Arc::from(*id)).collect(),
        };
        let found = store
            .last_messages_from(
                &Arc::from("acct"),
                &[
                    query("alice", &["alice"]),
                    query("123@lid", &["123@lid", "4477@s.whatsapp.net"]),
                    query("nobody", &["nobody"]),
                ],
            )
            .await?;
        let alice = &found[&Arc::<str>::from("alice")];
        assert_eq!(
            alice.at,
            ts(MONDAY + 300),
            "deleted/own/other-account ignored"
        );
        assert_eq!(alice.chat_id.as_ref(), "group-b");
        let bob = &found[&Arc::<str>::from("123@lid")];
        assert_eq!(bob.at, ts(MONDAY + 200), "alias in direct chat wins");
        assert!(!found.contains_key(&Arc::<str>::from("nobody")));

        let conn = store.conn.lock().await;
        let plan: String = conn.query_row(
            "EXPLAIN QUERY PLAN SELECT timestamp, chat_id FROM messages
             WHERE account_id = 'acct' AND sender_id = 'alice' AND is_from_me = 0
               AND content_type != 'deleted' ORDER BY timestamp DESC LIMIT 1",
            [],
            |row| row.get(3),
        )?;
        assert!(plan.contains("idx_messages_sender"), "{plan}");
        Ok(())
    }

    #[test]
    fn first_url_trims_trailing_punctuation() {
        assert_eq!(
            first_url("go to https://a.b/c?d=1)."),
            Some("https://a.b/c?d=1".to_owned())
        );
        assert_eq!(first_url("no links"), None);
        assert_eq!(
            first_url("<https://x.io|x>"),
            Some("https://x.io".to_owned())
        );
    }
}
