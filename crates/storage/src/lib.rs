mod schema;

use anyhow::{Context, Result, anyhow};
use chat_core::*;
use chrono::{TimeZone, Utc};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub async fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory database")?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(schema::V1)
            .context("running storage migrations")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn open_default() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "chat-cli")
            .ok_or_else(|| anyhow!("could not determine project data directory"))?;
        Self::open(&dirs.data_dir().join("chat-cli.db")).await
    }

    pub async fn upsert_account(&self, account: &Account, config: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO accounts (id, platform, display_name, avatar_path, config_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                platform = excluded.platform,
                display_name = excluded.display_name,
                avatar_path = excluded.avatar_path,
                config_json = excluded.config_json",
            params![
                account.id.as_ref(),
                platform_to_str(&account.platform),
                account.display_name.as_ref(),
                path_to_string(account.avatar.as_ref()),
                config,
                Utc::now().timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub async fn get_accounts(&self) -> Result<Vec<Account>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, platform, display_name, avatar_path FROM accounts ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Account {
                id: arc_str(row.get::<_, String>(0)?),
                platform: platform_from_str(&row.get::<_, String>(1)?),
                display_name: arc_str(row.get::<_, String>(2)?),
                avatar: string_to_path(row.get::<_, Option<String>>(3)?),
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub async fn remove_account(&self, id: &ProviderId) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM accounts WHERE id = ?1", params![id.as_ref()])?;
        Ok(())
    }

    pub async fn upsert_chat(&self, chat: &Chat) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO chats (
                id, account_id, platform, name, avatar_path, is_group, unread_count, muted, pinned,
                last_msg_at, last_preview, thread_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id, account_id) DO UPDATE SET
                platform = excluded.platform,
                name = excluded.name,
                avatar_path = excluded.avatar_path,
                is_group = excluded.is_group,
                unread_count = excluded.unread_count,
                muted = excluded.muted,
                pinned = excluded.pinned,
                last_msg_at = excluded.last_msg_at,
                last_preview = excluded.last_preview,
                thread_id = excluded.thread_id",
            params![
                chat.id.as_ref(),
                chat.account.as_ref(),
                platform_to_str(&chat.platform),
                chat.name.as_ref(),
                path_to_string(chat.avatar.as_ref()),
                chat.is_group,
                chat.unread_count,
                chat.muted,
                chat.pinned,
                chat.last_message_at.map(|t| t.timestamp_millis()),
                chat.last_message_preview.as_deref(),
                chat.thread_id.as_deref(),
            ],
        )?;
        Ok(())
    }

    pub async fn get_chats(&self, account_id: &ProviderId) -> Result<Vec<Chat>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, platform, name, avatar_path, is_group, unread_count, muted, pinned,
                    last_msg_at, last_preview, thread_id
             FROM chats WHERE account_id = ?1 ORDER BY pinned DESC, last_msg_at DESC NULLS LAST, name",
        )?;
        let rows = stmt.query_map(params![account_id.as_ref()], chat_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub async fn get_all_chats(&self) -> Result<Vec<Chat>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, account_id, platform, name, avatar_path, is_group, unread_count, muted, pinned,
                    last_msg_at, last_preview, thread_id
             FROM chats ORDER BY pinned DESC, last_msg_at DESC NULLS LAST, name",
        )?;
        let rows = stmt.query_map([], chat_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub async fn upsert_message(&self, msg: &Message) -> Result<()> {
        let conn = self.conn.lock().await;
        let content = StoredContent::from_content(&msg.content);
        let platform_json = platform_data_to_json(&msg.platform_data)?;

        conn.execute(
            "INSERT INTO messages (
                id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                content_type, content_text, content_caption, media_id, media_filename, media_mime,
                media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
             ON CONFLICT(id, account_id) DO UPDATE SET
                chat_id = excluded.chat_id,
                sender_id = excluded.sender_id,
                sender_name = excluded.sender_name,
                sender_avatar = excluded.sender_avatar,
                edited_at = excluded.edited_at,
                content_type = excluded.content_type,
                content_text = excluded.content_text,
                content_caption = excluded.content_caption,
                media_id = excluded.media_id,
                media_filename = excluded.media_filename,
                media_mime = excluded.media_mime,
                media_size = excluded.media_size,
                media_local = excluded.media_local,
                media_thumbnail = excluded.media_thumbnail,
                reply_to_id = excluded.reply_to_id,
                thread_id = excluded.thread_id,
                is_from_me = excluded.is_from_me,
                platform_json = excluded.platform_json",
            params![
                msg.id.as_ref(),
                msg.chat_id.as_ref(),
                msg.account.as_ref(),
                msg.sender.platform_id.as_ref(),
                msg.sender.display_name.as_ref(),
                path_to_string(msg.sender.avatar.as_ref()),
                msg.timestamp.timestamp_millis(),
                msg.edited_at.map(|t| t.timestamp_millis()),
                content.kind,
                content.text,
                content.caption,
                content.media_id,
                content.media_filename,
                content.media_mime,
                content.media_size,
                content.media_local,
                content.media_thumbnail,
                msg.reply_to.as_deref(),
                msg.thread_id.as_deref(),
                msg.is_from_me,
                platform_json,
            ],
        )?;

        conn.execute(
            "DELETE FROM reactions WHERE message_id = ?1 AND account_id = ?2",
            params![msg.id.as_ref(), msg.account.as_ref()],
        )?;
        for reaction in &msg.reactions {
            for sender in &reaction.senders {
                conn.execute(
                    "INSERT OR IGNORE INTO reactions (message_id, account_id, emoji, sender_id) VALUES (?1, ?2, ?3, ?4)",
                    params![msg.id.as_ref(), msg.account.as_ref(), reaction.emoji.as_ref(), sender.as_ref()],
                )?;
            }
        }

        conn.execute(
            "DELETE FROM receipts WHERE message_id = ?1 AND account_id = ?2",
            params![msg.id.as_ref(), msg.account.as_ref()],
        )?;
        for receipt in &msg.receipts {
            conn.execute(
                "INSERT OR REPLACE INTO receipts (message_id, account_id, sender_id, kind, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![msg.id.as_ref(), msg.account.as_ref(), receipt.platform_id.as_ref(), receipt_kind_to_str(&receipt.kind), receipt.at.map(|t| t.timestamp_millis())],
            )?;
        }

        Ok(())
    }

    pub async fn get_messages(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock().await;
        let limit = usize_to_i64(limit)?;
        let before = before.map(|t| t.timestamp_millis()).unwrap_or(i64::MAX);
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                    content_type, content_text, content_caption, media_id, media_filename, media_mime,
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
             FROM messages WHERE chat_id = ?1 AND timestamp < ?2 ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let mut messages = stmt
            .query_map(params![chat_id.as_ref(), before, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for msg in &mut messages {
            hydrate_reactions_and_receipts(&conn, msg)?;
        }
        messages.reverse();
        Ok(messages)
    }

    pub async fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        let conn = self.conn.lock().await;
        let pattern = format!("%{}%", query);
        let limit = usize_to_i64(limit)?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                    content_type, content_text, content_caption, media_id, media_filename, media_mime,
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
             FROM messages WHERE content_text LIKE ?1 ORDER BY timestamp DESC LIMIT ?2",
        )?;
        let mut messages = stmt
            .query_map(params![pattern, limit], message_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for msg in &mut messages {
            hydrate_reactions_and_receipts(&conn, msg)?;
        }
        Ok(messages)
    }

    pub async fn upsert_person(&self, person: &Person) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO persons (id, display_name, avatar_path) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name, avatar_path = excluded.avatar_path",
            params![person.id.to_string(), person.display_name.as_ref(), path_to_string(person.avatar.as_ref())],
        )?;
        conn.execute(
            "DELETE FROM handles WHERE person_id = ?1",
            params![person.id.to_string()],
        )?;
        for handle in &person.handles {
            conn.execute(
                "INSERT OR REPLACE INTO handles (person_id, platform, account_id, platform_id, display_name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![person.id.to_string(), platform_to_str(&handle.platform), handle.account.as_ref(), handle.platform_id.as_ref(), handle.display_name.as_ref()],
            )?;
        }
        Ok(())
    }

    pub async fn find_person_by_handle(
        &self,
        platform: &Platform,
        platform_id: &PlatformId,
    ) -> Result<Option<Person>> {
        let conn = self.conn.lock().await;
        let person_id = conn
            .query_row(
                "SELECT person_id FROM handles WHERE platform = ?1 AND platform_id = ?2 LIMIT 1",
                params![platform_to_str(platform), platform_id.as_ref()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        person_id.map(|id| get_person_by_id(&conn, &id)).transpose()
    }

    pub async fn merge_persons(&self, a: &PersonId, b: &PersonId) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE handles SET person_id = ?1 WHERE person_id = ?2",
            params![a.to_string(), b.to_string()],
        )?;
        conn.execute("DELETE FROM persons WHERE id = ?1", params![b.to_string()])?;
        Ok(())
    }
}

struct StoredContent {
    kind: &'static str,
    text: Option<String>,
    caption: Option<String>,
    media_id: Option<String>,
    media_filename: Option<String>,
    media_mime: Option<String>,
    media_size: Option<u64>,
    media_local: Option<String>,
    media_thumbnail: Option<String>,
}

impl StoredContent {
    fn from_content(content: &Content) -> Self {
        match content {
            Content::Text(text) => Self::text("text", Some(text.to_string())),
            Content::Unsupported(text) => Self::text("unsupported", Some(text.to_string())),
            Content::Deleted => Self::text("deleted", None),
            Content::LinkPreview(link) => Self {
                kind: "link",
                text: Some(link.url.to_string()),
                caption: link.title.as_ref().map(ToString::to_string),
                media_id: None,
                media_filename: None,
                media_mime: None,
                media_size: None,
                media_local: None,
                media_thumbnail: None,
            },
            Content::Image(media) => Self::media("image", media),
            Content::Video(media) => Self::media("video", media),
            Content::Audio(media) => Self::media("audio", media),
            Content::File(media) => Self::media("file", media),
            Content::Sticker(media) => Self::media("sticker", media),
        }
    }

    fn text(kind: &'static str, text: Option<String>) -> Self {
        Self {
            kind,
            text,
            caption: None,
            media_id: None,
            media_filename: None,
            media_mime: None,
            media_size: None,
            media_local: None,
            media_thumbnail: None,
        }
    }

    fn media(kind: &'static str, media: &Media) -> Self {
        Self {
            kind,
            text: None,
            caption: media.caption.as_ref().map(ToString::to_string),
            media_id: Some(media.id.to_string()),
            media_filename: Some(media.file_name.to_string()),
            media_mime: Some(media.mime_type.to_string()),
            media_size: media.size_bytes,
            media_local: path_to_string(media.local_path.as_ref()),
            media_thumbnail: path_to_string(media.thumbnail.as_ref()),
        }
    }
}

fn chat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chat> {
    Ok(Chat {
        id: arc_str(row.get::<_, String>(0)?),
        account: arc_str(row.get::<_, String>(1)?),
        platform: platform_from_str(&row.get::<_, String>(2)?),
        name: arc_str(row.get::<_, String>(3)?),
        avatar: string_to_path(row.get::<_, Option<String>>(4)?),
        is_group: row.get(5)?,
        unread_count: row.get(6)?,
        muted: row.get(7)?,
        pinned: row.get(8)?,
        last_message_at: millis_to_timestamp(row.get(9)?),
        last_message_preview: row.get::<_, Option<String>>(10)?.map(arc_str),
        thread_id: row.get::<_, Option<String>>(11)?.map(arc_str),
    })
}

fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let kind: String = row.get(8)?;
    let content = content_from_parts(ContentParts {
        kind: &kind,
        text: row.get(9)?,
        caption: row.get(10)?,
        media_id: row.get(11)?,
        media_filename: row.get(12)?,
        media_mime: row.get(13)?,
        media_size: row.get(14)?,
        media_local: row.get(15)?,
        media_thumbnail: row.get(16)?,
    });
    let platform_json: Option<String> = row.get(20)?;

    Ok(Message {
        id: arc_str(row.get::<_, String>(0)?),
        chat_id: arc_str(row.get::<_, String>(1)?),
        account: arc_str(row.get::<_, String>(2)?),
        sender: Sender {
            platform_id: arc_str(row.get::<_, String>(3)?),
            display_name: arc_str(row.get::<_, String>(4)?),
            avatar: string_to_path(row.get::<_, Option<String>>(5)?),
        },
        timestamp: millis_to_timestamp(Some(row.get(6)?)).unwrap_or_else(Utc::now),
        edited_at: millis_to_timestamp(row.get(7)?),
        content,
        reply_to: row.get::<_, Option<String>>(17)?.map(arc_str),
        thread_id: row.get::<_, Option<String>>(18)?.map(arc_str),
        reactions: Vec::new(),
        receipts: Vec::new(),
        is_from_me: row.get(19)?,
        platform_data: platform_data_from_json(platform_json.as_deref()),
    })
}

struct ContentParts<'a> {
    kind: &'a str,
    text: Option<String>,
    caption: Option<String>,
    media_id: Option<String>,
    media_filename: Option<String>,
    media_mime: Option<String>,
    media_size: Option<u64>,
    media_local: Option<String>,
    media_thumbnail: Option<String>,
}

fn content_from_parts(parts: ContentParts<'_>) -> Content {
    let ContentParts {
        kind,
        text,
        caption,
        media_id,
        media_filename,
        media_mime,
        media_size,
        media_local,
        media_thumbnail,
    } = parts;

    match kind {
        "text" => Content::Text(arc_str(text.unwrap_or_default())),
        "unsupported" => Content::Unsupported(arc_str(text.unwrap_or_default())),
        "deleted" => Content::Deleted,
        "link" => Content::LinkPreview(LinkPreview {
            url: arc_str(text.unwrap_or_default()),
            title: caption.map(arc_str),
            description: None,
            image: None,
        }),
        "image" => Content::Image(media_from_parts(
            media_id,
            media_filename,
            media_mime,
            media_size,
            caption,
            media_local,
            media_thumbnail,
        )),
        "video" => Content::Video(media_from_parts(
            media_id,
            media_filename,
            media_mime,
            media_size,
            caption,
            media_local,
            media_thumbnail,
        )),
        "audio" => Content::Audio(media_from_parts(
            media_id,
            media_filename,
            media_mime,
            media_size,
            caption,
            media_local,
            media_thumbnail,
        )),
        "file" => Content::File(media_from_parts(
            media_id,
            media_filename,
            media_mime,
            media_size,
            caption,
            media_local,
            media_thumbnail,
        )),
        "sticker" => Content::Sticker(media_from_parts(
            media_id,
            media_filename,
            media_mime,
            media_size,
            caption,
            media_local,
            media_thumbnail,
        )),
        other => Content::Unsupported(arc_str(other.to_owned())),
    }
}

fn media_from_parts(
    id: Option<String>,
    file_name: Option<String>,
    mime_type: Option<String>,
    size_bytes: Option<u64>,
    caption: Option<String>,
    local_path: Option<String>,
    thumbnail: Option<String>,
) -> Media {
    Media {
        id: arc_str(id.unwrap_or_default()),
        file_name: arc_str(file_name.unwrap_or_default()),
        mime_type: arc_str(mime_type.unwrap_or_default()),
        size_bytes,
        caption: caption.map(arc_str),
        local_path: string_to_path(local_path),
        thumbnail: string_to_path(thumbnail),
    }
}

fn hydrate_reactions_and_receipts(conn: &Connection, msg: &mut Message) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT emoji, sender_id FROM reactions WHERE message_id = ?1 AND account_id = ?2 ORDER BY emoji, sender_id",
    )?;
    let pairs = stmt
        .query_map(params![msg.id.as_ref(), msg.account.as_ref()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (emoji, sender) in pairs {
        if let Some(reaction) = msg.reactions.iter_mut().find(|r| r.emoji.as_ref() == emoji) {
            reaction.senders.push(arc_str(sender));
        } else {
            msg.reactions.push(Reaction {
                emoji: arc_str(emoji),
                senders: vec![arc_str(sender)],
            });
        }
    }

    let mut stmt = conn.prepare(
        "SELECT sender_id, kind, at FROM receipts WHERE message_id = ?1 AND account_id = ?2 ORDER BY sender_id, kind",
    )?;
    msg.receipts = stmt
        .query_map(params![msg.id.as_ref(), msg.account.as_ref()], |row| {
            Ok(Receipt {
                platform_id: arc_str(row.get::<_, String>(0)?),
                kind: receipt_kind_from_str(&row.get::<_, String>(1)?),
                at: millis_to_timestamp(row.get(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(())
}

fn get_person_by_id(conn: &Connection, id: &str) -> Result<Person> {
    let mut person = conn.query_row(
        "SELECT id, display_name, avatar_path FROM persons WHERE id = ?1",
        params![id],
        |row| {
            Ok(Person {
                id: Uuid::parse_str(&row.get::<_, String>(0)?).map_err(|err| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(err),
                    )
                })?,
                display_name: arc_str(row.get::<_, String>(1)?),
                avatar: string_to_path(row.get::<_, Option<String>>(2)?),
                handles: Vec::new(),
            })
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT platform, account_id, platform_id, display_name FROM handles WHERE person_id = ?1 ORDER BY platform, platform_id",
    )?;
    person.handles = stmt
        .query_map(params![id], |row| {
            Ok(Handle {
                platform: platform_from_str(&row.get::<_, String>(0)?),
                account: arc_str(row.get::<_, String>(1)?),
                platform_id: arc_str(row.get::<_, String>(2)?),
                display_name: arc_str(row.get::<_, String>(3)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(person)
}

fn platform_to_str(platform: &Platform) -> String {
    match platform {
        Platform::WhatsApp => "WhatsApp".to_owned(),
        Platform::Slack => "Slack".to_owned(),
        Platform::Discord => "Discord".to_owned(),
        Platform::Unknown(value) => format!("Unknown:{value}"),
    }
}

fn platform_from_str(value: &str) -> Platform {
    match value {
        "WhatsApp" => Platform::WhatsApp,
        "Slack" => Platform::Slack,
        "Discord" => Platform::Discord,
        other => Platform::Unknown(other.strip_prefix("Unknown:").unwrap_or(other).to_owned()),
    }
}

fn receipt_kind_to_str(kind: &ReceiptKind) -> &'static str {
    match kind {
        ReceiptKind::Delivered => "delivered",
        ReceiptKind::Read => "read",
    }
}

fn receipt_kind_from_str(value: &str) -> ReceiptKind {
    match value {
        "read" => ReceiptKind::Read,
        _ => ReceiptKind::Delivered,
    }
}

fn platform_data_to_json(data: &PlatformData) -> Result<String> {
    let value = serde_json::json!({
        "whatsapp": data.whatsapp.as_ref().map(|wa| serde_json::json!({ "jid": wa.jid.as_ref() })),
        "slack": data.slack.as_ref().map(|slack| serde_json::json!({
            "ts": slack.ts.as_ref(),
            "thread_ts": slack.thread_ts.as_deref(),
            "channel": slack.channel.as_ref(),
        })),
    });
    Ok(serde_json::to_string(&value)?)
}

fn platform_data_from_json(json: Option<&str>) -> PlatformData {
    let Some(json) = json else {
        return PlatformData::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return PlatformData::default();
    };

    let whatsapp = value.get("whatsapp").and_then(|wa| {
        Some(WhatsAppData {
            jid: arc_str(wa.get("jid")?.as_str()?.to_owned()),
        })
    });
    let slack = value.get("slack").and_then(|slack| {
        Some(SlackData {
            ts: arc_str(slack.get("ts")?.as_str()?.to_owned()),
            thread_ts: slack
                .get("thread_ts")
                .and_then(|v| v.as_str())
                .map(|s| arc_str(s.to_owned())),
            channel: arc_str(slack.get("channel")?.as_str()?.to_owned()),
        })
    });

    PlatformData { whatsapp, slack }
}

fn path_to_string(path: Option<&PathBuf>) -> Option<String> {
    path.map(|path| path.to_string_lossy().into_owned())
}

fn string_to_path(path: Option<String>) -> Option<PathBuf> {
    path.map(PathBuf::from)
}

fn millis_to_timestamp(millis: Option<i64>) -> Option<Timestamp> {
    millis.and_then(|millis| Utc.timestamp_millis_opt(millis).single())
}

fn arc_str(value: String) -> Arc<str> {
    Arc::<str>::from(value)
}

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).context("limit does not fit into i64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn account_chat_message_and_person_roundtrip() -> Result<()> {
        let dir = tempdir()?;
        let store = Store::open(&dir.path().join("test.db")).await?;
        let account_id = arc_str("whatsapp:+1234".to_owned());

        let account = Account {
            id: account_id.clone(),
            platform: Platform::WhatsApp,
            display_name: arc_str("Personal".to_owned()),
            avatar: None,
        };
        store.upsert_account(&account, "{}").await?;
        assert_eq!(store.get_accounts().await?.len(), 1);

        let chat = Chat {
            id: arc_str("chat-1".to_owned()),
            account: account_id.clone(),
            platform: Platform::WhatsApp,
            name: arc_str("Alice".to_owned()),
            avatar: None,
            is_group: false,
            unread_count: 1,
            muted: false,
            pinned: true,
            last_message_at: Some(Utc::now()),
            last_message_preview: Some(arc_str("hello".to_owned())),
            thread_id: None,
        };
        store.upsert_chat(&chat).await?;
        assert_eq!(
            store.get_chats(&account_id).await?[0].name.as_ref(),
            "Alice"
        );
        assert_eq!(store.get_all_chats().await?.len(), 1);

        let message = Message {
            id: arc_str("msg-1".to_owned()),
            chat_id: chat.id.clone(),
            account: account_id.clone(),
            sender: Sender {
                platform_id: arc_str("alice".to_owned()),
                display_name: arc_str("Alice".to_owned()),
                avatar: None,
            },
            timestamp: Utc::now(),
            edited_at: None,
            content: Content::Text(arc_str("hello from sqlite".to_owned())),
            reply_to: None,
            thread_id: None,
            reactions: vec![Reaction {
                emoji: arc_str("👍".to_owned()),
                senders: vec![arc_str("me".to_owned())],
            }],
            receipts: vec![Receipt {
                platform_id: arc_str("me".to_owned()),
                kind: ReceiptKind::Read,
                at: Some(Utc::now()),
            }],
            is_from_me: false,
            platform_data: PlatformData {
                whatsapp: Some(WhatsAppData {
                    jid: arc_str("alice@s.whatsapp.net".to_owned()),
                }),
                slack: None,
            },
        };
        store.upsert_message(&message).await?;

        let mut refreshed_message = message.clone();
        let original_timestamp = message.timestamp;
        refreshed_message.timestamp = original_timestamp + chrono::Duration::hours(2);
        refreshed_message.content = Content::Text(arc_str("hello from refreshed sync".to_owned()));
        store.upsert_message(&refreshed_message).await?;

        let messages = store.get_messages(&chat.id, None, 10).await?;
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].timestamp.timestamp_millis(),
            original_timestamp.timestamp_millis()
        );
        assert!(matches!(
            &messages[0].content,
            Content::Text(text) if text.as_ref() == "hello from refreshed sync"
        ));
        assert_eq!(messages[0].reactions.len(), 1);
        assert_eq!(messages[0].receipts.len(), 1);
        assert_eq!(store.search_messages("refreshed", 10).await?.len(), 1);

        let person = Person {
            id: Uuid::new_v4(),
            display_name: arc_str("Alice".to_owned()),
            avatar: None,
            handles: vec![Handle {
                platform: Platform::WhatsApp,
                account: account_id,
                platform_id: arc_str("alice".to_owned()),
                display_name: arc_str("Alice".to_owned()),
            }],
        };
        store.upsert_person(&person).await?;
        let found = store
            .find_person_by_handle(&Platform::WhatsApp, &arc_str("alice".to_owned()))
            .await?;
        assert_eq!(found.expect("person should exist").id, person.id);

        Ok(())
    }

    #[tokio::test]
    async fn messages_are_returned_chronologically_after_out_of_order_upserts() -> Result<()> {
        let store = Store::open_memory().await?;
        let account_id = arc_str("mock:local".to_owned());
        let chat_id = arc_str("mock:chat:alice".to_owned());
        let base = Utc
            .with_ymd_and_hms(2026, 6, 5, 4, 0, 0)
            .single()
            .expect("valid timestamp");

        let account = Account {
            id: account_id.clone(),
            platform: Platform::Unknown("mock".to_owned()),
            display_name: arc_str("Mock Account".to_owned()),
            avatar: None,
        };
        store.upsert_account(&account, "{}").await?;
        store
            .upsert_chat(&Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::Unknown("mock".to_owned()),
                name: arc_str("Alice".to_owned()),
                avatar: None,
                is_group: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(base + chrono::Duration::minutes(3)),
                last_message_preview: Some(arc_str("third".to_owned())),
                thread_id: None,
            })
            .await?;

        for (id, text, minute) in [
            ("msg-3", "third", 3),
            ("msg-1", "first", 1),
            ("msg-2", "second", 2),
        ] {
            store
                .upsert_message(&Message {
                    id: arc_str(id.to_owned()),
                    chat_id: chat_id.clone(),
                    account: account_id.clone(),
                    sender: Sender {
                        platform_id: arc_str("alice".to_owned()),
                        display_name: arc_str("Alice".to_owned()),
                        avatar: None,
                    },
                    timestamp: base + chrono::Duration::minutes(minute),
                    edited_at: None,
                    content: Content::Text(arc_str(text.to_owned())),
                    reply_to: None,
                    thread_id: None,
                    reactions: Vec::new(),
                    receipts: Vec::new(),
                    is_from_me: false,
                    platform_data: PlatformData::default(),
                })
                .await?;
        }

        let messages = store.get_messages(&chat_id, None, 10).await?;
        let ids = messages
            .iter()
            .map(|message| message.id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["msg-1", "msg-2", "msg-3"]);

        Ok(())
    }
}
