mod schema;

use anyhow::{Context, Result, anyhow};
use chat_core::*;
use chrono::{TimeZone, Utc};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, ToSql, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatInboxStyle {
    ActivityFirst,
    RecentFlat,
    PeopleFirst,
    GroupsFirst,
    AccountSeparated,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiThemePreset {
    DefaultDark,
    Light,
    HighContrast,
    WhatsApp,
    Slack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPresentationSetting {
    ProviderNative,
    #[serde(alias = "unified")]
    WhatsApp,
    Slack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkActivityDisplay {
    Hidden,
    CombinedLights,
    RecentCounts,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AppSettings {
    pub desktop_notifications: bool,
    pub in_app_notifications: bool,
    pub notification_previews: bool,
    pub notify_selected_chat: bool,
    pub notify_muted_chats: bool,
    pub chat_inbox_style: ChatInboxStyle,
    pub ui_theme: UiThemePreset,
    pub conversation_presentation: ConversationPresentationSetting,
    pub network_activity: NetworkActivityDisplay,
    pub show_muted_chats: bool,
    pub show_browse_channels: bool,
    pub show_empty_chats: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredAccountConfig {
    pub id: ProviderId,
    pub platform: Platform,
    pub display_name: Arc<str>,
    pub config_json: String,
}

#[derive(Clone, Debug)]
pub struct ChatActivityUpdate {
    pub account_id: ProviderId,
    pub chat_id: ChatId,
    pub timestamp: Option<Timestamp>,
    pub preview: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub struct ChatLatestMessage {
    pub account_id: ProviderId,
    pub chat_id: ChatId,
    pub message: Message,
}

impl Default for ChatInboxStyle {
    fn default() -> Self {
        Self::ActivityFirst
    }
}

impl Default for UiThemePreset {
    fn default() -> Self {
        Self::DefaultDark
    }
}

impl Default for ConversationPresentationSetting {
    fn default() -> Self {
        Self::WhatsApp
    }
}

impl Default for NetworkActivityDisplay {
    fn default() -> Self {
        Self::CombinedLights
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            desktop_notifications: true,
            in_app_notifications: true,
            notification_previews: true,
            notify_selected_chat: false,
            notify_muted_chats: false,
            chat_inbox_style: ChatInboxStyle::ActivityFirst,
            ui_theme: UiThemePreset::DefaultDark,
            conversation_presentation: ConversationPresentationSetting::WhatsApp,
            network_activity: NetworkActivityDisplay::CombinedLights,
            show_muted_chats: true,
            show_browse_channels: false,
            show_empty_chats: true,
        }
    }
}

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
        ensure_chat_metadata_columns(&conn).context("adding chat metadata columns")?;

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

    pub async fn get_account_configs(&self) -> Result<Vec<StoredAccountConfig>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, platform, display_name, config_json FROM accounts ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StoredAccountConfig {
                id: arc_str(row.get::<_, String>(0)?),
                platform: platform_from_str(&row.get::<_, String>(1)?),
                display_name: arc_str(row.get::<_, String>(2)?),
                config_json: row.get(3)?,
            })
        })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub async fn remove_account(&self, id: &ProviderId) -> Result<()> {
        let conn = self.conn.lock().await;
        let transaction = conn.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM reactions WHERE account_id = ?1",
            params![id.as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM receipts WHERE account_id = ?1",
            params![id.as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM messages WHERE account_id = ?1",
            params![id.as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM chats WHERE account_id = ?1",
            params![id.as_ref()],
        )?;
        transaction.execute("DELETE FROM accounts WHERE id = ?1", params![id.as_ref()])?;
        transaction.commit()?;
        Ok(())
    }

    pub async fn app_settings(&self) -> Result<AppSettings> {
        self.get_setting("app.settings")
            .await
            .map(|settings| settings.unwrap_or_default())
    }

    pub async fn save_app_settings(&self, settings: &AppSettings) -> Result<()> {
        self.set_setting("app.settings", settings).await
    }

    async fn get_setting<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let conn = self.conn.lock().await;
        let value = conn
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|value| {
                serde_json::from_str(&value).with_context(|| format!("parsing setting {key}"))
            })
            .transpose()
    }

    async fn set_setting<T>(&self, key: &str, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        let value_json =
            serde_json::to_string(value).with_context(|| format!("encoding setting {key}"))?;
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO settings (key, value_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value_json = excluded.value_json,
                updated_at = excluded.updated_at",
            params![key, value_json, Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub async fn upsert_chat(&self, chat: &Chat) -> Result<()> {
        let conn = self.conn.lock().await;
        upsert_chat_on_conn(&conn, chat)
    }

    pub async fn set_chat_activities(&self, activities: &[ChatActivityUpdate]) -> Result<()> {
        if activities.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        for activity in activities {
            tx.execute(
                "UPDATE chats
                 SET last_msg_at = ?3,
                     last_preview = ?4
                 WHERE account_id = ?1 AND id = ?2",
                params![
                    activity.account_id.as_ref(),
                    activity.chat_id.as_ref(),
                    activity.timestamp.map(|t| t.timestamp_millis()),
                    activity.preview.as_deref(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn latest_message_for_each_chat(&self) -> Result<Vec<ChatLatestMessage>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.chat_id, m.account_id, m.sender_id, m.sender_name, m.sender_avatar,
                    m.timestamp, m.edited_at, m.content_type, m.content_text, m.content_caption,
                    m.media_id, m.media_filename, m.media_mime, m.media_size, m.media_local,
                    m.media_thumbnail, m.reply_to_id, m.thread_id, m.is_from_me, m.platform_json
             FROM messages m
             JOIN (
                 SELECT account_id, chat_id, MAX(timestamp) AS timestamp
                 FROM messages
                 GROUP BY account_id, chat_id
             ) latest
               ON latest.account_id = m.account_id
              AND latest.chat_id = m.chat_id
              AND latest.timestamp = m.timestamp
             WHERE m.id = (
                 SELECT m2.id
                 FROM messages m2
                 WHERE m2.account_id = m.account_id
                   AND m2.chat_id = m.chat_id
                   AND m2.timestamp = m.timestamp
                 ORDER BY m2.id DESC
                 LIMIT 1
             )",
        )?;
        let mut rows = stmt.query([])?;
        let mut latest_messages = Vec::new();
        while let Some(row) = rows.next()? {
            let message = message_from_row(row)?;
            latest_messages.push(ChatLatestMessage {
                account_id: message.account.clone(),
                chat_id: message.chat_id.clone(),
                message,
            });
        }
        Ok(latest_messages)
    }

    pub async fn set_chat_activity(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
        timestamp: Option<Timestamp>,
        preview: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE chats
             SET last_msg_at = ?3,
                 last_preview = ?4
             WHERE account_id = ?1 AND id = ?2",
            params![
                account_id.as_ref(),
                chat_id.as_ref(),
                timestamp.map(|t| t.timestamp_millis()),
                preview,
            ],
        )?;
        Ok(())
    }

    pub async fn merge_chat(
        &self,
        account_id: &ProviderId,
        from_chat_id: &ChatId,
        to_chat: &Chat,
    ) -> Result<()> {
        if from_chat_id == &to_chat.id {
            self.upsert_chat(to_chat).await?;
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        upsert_chat_on_conn(&tx, to_chat)?;
        tx.execute(
            "UPDATE messages SET chat_id = ?1 WHERE account_id = ?2 AND chat_id = ?3",
            params![
                to_chat.id.as_ref(),
                account_id.as_ref(),
                from_chat_id.as_ref()
            ],
        )?;
        tx.execute(
            "DELETE FROM chats WHERE account_id = ?1 AND id = ?2",
            params![account_id.as_ref(), from_chat_id.as_ref()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub async fn get_chats(&self, account_id: &ProviderId) -> Result<Vec<Chat>> {
        let conn = self.conn.lock().await;
        load_chats(
            &conn,
            "WHERE account_id = ?1",
            &[&account_id.as_ref() as &dyn ToSql],
        )
    }

    pub async fn get_all_chats(&self) -> Result<Vec<Chat>> {
        let conn = self.conn.lock().await;
        load_chats(&conn, "", &[])
    }

    pub async fn delete_chats_not_in(
        &self,
        account_id: &ProviderId,
        chat_ids: &[ChatId],
    ) -> Result<usize> {
        let conn = self.conn.lock().await;
        if chat_ids.is_empty() {
            let deleted = conn.execute(
                "DELETE FROM chats WHERE account_id = ?1",
                params![account_id.as_ref()],
            )?;
            return Ok(deleted);
        }

        let placeholders = std::iter::repeat_n("?", chat_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("DELETE FROM chats WHERE account_id = ? AND id NOT IN ({placeholders})");
        let account_id_ref = account_id.as_ref();
        let chat_id_refs = chat_ids.iter().map(|id| id.as_ref()).collect::<Vec<_>>();
        let mut params = Vec::with_capacity(chat_id_refs.len() + 1);
        params.push(&account_id_ref as &dyn ToSql);
        params.extend(chat_id_refs.iter().map(|id| id as &dyn ToSql));
        let deleted = conn.execute(&sql, params.as_slice())?;
        Ok(deleted)
    }

    pub async fn upsert_message(&self, msg: &Message) -> Result<()> {
        let conn = self.conn.lock().await;
        upsert_message_on_conn(&conn, msg)
    }

    pub async fn upsert_messages(&self, messages: &[Message]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        for message in messages {
            upsert_message_on_conn(&tx, message)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub async fn get_messages_for_chat(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.get_messages_for_chat_internal(Some(account_id), chat_id, before, limit)
            .await
    }

    pub async fn get_messages(
        &self,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.get_messages_for_chat_internal(None, chat_id, before, limit)
            .await
    }

    async fn get_messages_for_chat_internal(
        &self,
        account_id: Option<&ProviderId>,
        chat_id: &ChatId,
        before: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock().await;
        let limit = usize_to_i64(limit)?;
        let before = before.map(|t| t.timestamp_millis()).unwrap_or(i64::MAX);
        let mut messages = if let Some(account_id) = account_id {
            let mut stmt = conn.prepare(
                "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                        content_type, content_text, content_caption, media_id, media_filename, media_mime,
                        media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
                 FROM messages WHERE account_id = ?1 AND chat_id = ?2 AND timestamp < ?3 ORDER BY timestamp DESC LIMIT ?4",
            )?;
            stmt.query_map(
                params![account_id.as_ref(), chat_id.as_ref(), before, limit],
                message_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                        content_type, content_text, content_caption, media_id, media_filename, media_mime,
                        media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
                 FROM messages WHERE chat_id = ?1 AND timestamp < ?2 ORDER BY timestamp DESC LIMIT ?3",
            )?;
            stmt.query_map(params![chat_id.as_ref(), before, limit], message_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for msg in &mut messages {
            hydrate_reactions_and_receipts(&conn, msg)?;
        }
        messages.reverse();
        Ok(messages)
    }

    pub async fn oldest_message_for_chat(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
    ) -> Result<Option<Message>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                    content_type, content_text, content_caption, media_id, media_filename, media_mime,
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json
             FROM messages WHERE account_id = ?1 AND chat_id = ?2 ORDER BY timestamp ASC LIMIT 1",
        )?;
        let mut message = stmt
            .query_row(
                params![account_id.as_ref(), chat_id.as_ref()],
                message_from_row,
            )
            .optional()?;
        if let Some(message) = &mut message {
            hydrate_reactions_and_receipts(&conn, message)?;
        }
        Ok(message)
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

fn upsert_chat_on_conn(conn: &Connection, chat: &Chat) -> Result<()> {
    conn.execute(
        "INSERT INTO chats (
            id, account_id, platform, name, avatar_path, is_group, kind, membership, is_shared,
            unread_count, muted, pinned, last_msg_at, last_preview, thread_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id, account_id) DO UPDATE SET
           platform = excluded.platform,
           name = excluded.name,
           avatar_path = excluded.avatar_path,
           is_group = excluded.is_group,
           kind = excluded.kind,
           membership = excluded.membership,
           is_shared = excluded.is_shared,
           unread_count = excluded.unread_count,
           muted = excluded.muted,
           pinned = excluded.pinned,
           last_msg_at = CASE
               WHEN excluded.last_msg_at IS NULL THEN chats.last_msg_at
               WHEN chats.last_msg_at IS NULL OR excluded.last_msg_at >= chats.last_msg_at THEN excluded.last_msg_at
               ELSE chats.last_msg_at
           END,
           last_preview = CASE
               WHEN excluded.last_msg_at IS NULL THEN COALESCE(chats.last_preview, excluded.last_preview)
               WHEN chats.last_msg_at IS NULL OR excluded.last_msg_at >= chats.last_msg_at THEN COALESCE(excluded.last_preview, chats.last_preview)
               ELSE chats.last_preview
           END,
           thread_id = excluded.thread_id",
        params![
            chat.id.as_ref(),
            chat.account.as_ref(),
            platform_to_str(&chat.platform),
            chat.name.as_ref(),
            path_to_string(chat.avatar.as_ref()),
            chat.is_group,
            chat_kind_to_str(chat.kind),
            chat_membership_to_str(chat.membership),
            chat.is_shared,
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
#[derive(serde::Deserialize, serde::Serialize)]
struct StoredPoll {
    question: String,
    options: Vec<StoredPollOption>,
    selectable_options_count: Option<u32>,
    #[serde(default)]
    votes: Vec<StoredPollVote>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
enum StoredPollOption {
    Full { id: String, label: String },
    Legacy(String),
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredPollVote {
    sender: String,
    options: Vec<String>,
    timestamp: Option<i64>,
}

impl StoredPoll {
    fn from_poll(poll: &Poll) -> Self {
        Self {
            question: poll.question.to_string(),
            options: poll
                .options
                .iter()
                .map(|option| StoredPollOption::Full {
                    id: option.id.to_string(),
                    label: option.label.to_string(),
                })
                .collect(),
            selectable_options_count: poll.selectable_options_count,
            votes: poll
                .votes
                .iter()
                .map(|vote| StoredPollVote {
                    sender: vote.sender.to_string(),
                    options: vote.options.iter().map(ToString::to_string).collect(),
                    timestamp: vote.timestamp.map(|value| value.timestamp_millis()),
                })
                .collect(),
        }
    }

    fn into_poll(self) -> Poll {
        Poll {
            question: arc_str(self.question),
            options: self
                .options
                .into_iter()
                .map(|option| match option {
                    StoredPollOption::Full { id, label } => PollOption {
                        id: arc_str(id),
                        label: arc_str(label),
                    },
                    StoredPollOption::Legacy(label) => PollOption {
                        id: arc_str(label.clone()),
                        label: arc_str(label),
                    },
                })
                .collect(),
            selectable_options_count: self.selectable_options_count,
            votes: self
                .votes
                .into_iter()
                .filter(|vote| !vote.sender.is_empty())
                .map(|vote| PollVote {
                    sender: arc_str(vote.sender),
                    options: vote.options.into_iter().map(arc_str).collect(),
                    timestamp: vote
                        .timestamp
                        .and_then(|timestamp| Utc.timestamp_millis_opt(timestamp).single()),
                })
                .collect(),
        }
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
            Content::Poll(poll) => Self::text(
                "poll",
                Some(
                    serde_json::to_string(&StoredPoll::from_poll(poll))
                        .unwrap_or_else(|_| poll.question.to_string()),
                ),
            ),
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

fn load_chats(conn: &Connection, where_clause: &str, params: &[&dyn ToSql]) -> Result<Vec<Chat>> {
    let query = format!(
        "SELECT id, account_id, platform, name, avatar_path, is_group, kind, membership, is_shared,
                unread_count, muted, pinned, last_msg_at, last_preview, thread_id
         FROM chats {where_clause}"
    );
    let mut chats = conn
        .prepare(&query)?
        .query_map(params, chat_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    sort_chats_for_sidebar(&mut chats);
    Ok(chats)
}

fn sort_chats_for_sidebar(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| chat_sort_bucket(a).cmp(&chat_sort_bucket(b)))
            .then_with(|| b.unread_count.cmp(&a.unread_count))
            .then_with(|| b.last_message_at.cmp(&a.last_message_at))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn chat_sort_bucket(chat: &Chat) -> u8 {
    if chat.membership == ChatMembership::NotJoined {
        return 6;
    }
    if chat.muted {
        return 5;
    }
    match (&chat.platform, chat.kind) {
        (Platform::Slack, ChatKind::PublicChannel | ChatKind::PrivateChannel) => 0,
        (Platform::Slack, ChatKind::Direct) => 1,
        (Platform::Slack, ChatKind::GroupDirectMessage) => 2,
        _ if chat.is_group => 3,
        _ => 4,
    }
}

fn ensure_chat_metadata_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(chats)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;

    if !columns.contains("kind") {
        conn.execute(
            "ALTER TABLE chats ADD COLUMN kind TEXT NOT NULL DEFAULT 'direct'",
            [],
        )?;
    }
    if !columns.contains("membership") {
        conn.execute(
            "ALTER TABLE chats ADD COLUMN membership TEXT NOT NULL DEFAULT 'joined'",
            [],
        )?;
    }
    if !columns.contains("is_shared") {
        conn.execute(
            "ALTER TABLE chats ADD COLUMN is_shared INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    Ok(())
}

fn chat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chat> {
    Ok(Chat {
        id: arc_str(row.get::<_, String>(0)?),
        account: arc_str(row.get::<_, String>(1)?),
        platform: platform_from_str(&row.get::<_, String>(2)?),
        name: arc_str(row.get::<_, String>(3)?),
        avatar: string_to_path(row.get::<_, Option<String>>(4)?),
        is_group: row.get(5)?,
        kind: chat_kind_from_str(&row.get::<_, String>(6)?),
        membership: chat_membership_from_str(&row.get::<_, String>(7)?),
        is_shared: row.get(8)?,
        unread_count: row.get(9)?,
        muted: row.get(10)?,
        pinned: row.get(11)?,
        last_message_at: millis_to_timestamp(row.get(12)?),
        last_message_preview: row.get::<_, Option<String>>(13)?.map(arc_str),
        thread_id: row.get::<_, Option<String>>(14)?.map(arc_str),
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
        "poll" => poll_from_text(text),
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

fn poll_from_text(text: Option<String>) -> Content {
    let text = text.unwrap_or_default();
    serde_json::from_str::<StoredPoll>(&text)
        .map(StoredPoll::into_poll)
        .map(Content::Poll)
        .unwrap_or_else(|_| {
            Content::Poll(Poll {
                question: arc_str(text),
                options: Vec::new(),
                selectable_options_count: None,
                votes: Vec::new(),
            })
        })
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

fn chat_kind_to_str(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Direct => "direct",
        ChatKind::Group => "group",
        ChatKind::PublicChannel => "public_channel",
        ChatKind::PrivateChannel => "private_channel",
        ChatKind::GroupDirectMessage => "group_direct_message",
    }
}

fn chat_kind_from_str(value: &str) -> ChatKind {
    match value {
        "group" => ChatKind::Group,
        "public_channel" => ChatKind::PublicChannel,
        "private_channel" => ChatKind::PrivateChannel,
        "group_direct_message" => ChatKind::GroupDirectMessage,
        _ => ChatKind::Direct,
    }
}

fn chat_membership_to_str(membership: ChatMembership) -> &'static str {
    match membership {
        ChatMembership::Joined => "joined",
        ChatMembership::NotJoined => "not_joined",
        ChatMembership::Unknown => "unknown",
    }
}

fn chat_membership_from_str(value: &str) -> ChatMembership {
    match value {
        "not_joined" => ChatMembership::NotJoined,
        "unknown" => ChatMembership::Unknown,
        _ => ChatMembership::Joined,
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

fn upsert_message_on_conn(conn: &Connection, msg: &Message) -> Result<()> {
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

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).context("limit does not fit into i64")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn app_settings_roundtrip_includes_ui_preferences() -> Result<()> {
        let store = Store::open_memory().await?;
        let settings = AppSettings {
            ui_theme: UiThemePreset::Slack,
            conversation_presentation: ConversationPresentationSetting::ProviderNative,
            ..AppSettings::default()
        };

        store.save_app_settings(&settings).await?;
        let loaded = store.app_settings().await?;

        assert_eq!(loaded.ui_theme, UiThemePreset::Slack);
        assert_eq!(
            loaded.conversation_presentation,
            ConversationPresentationSetting::ProviderNative
        );
        Ok(())
    }

    #[test]
    fn legacy_unified_conversation_presentation_maps_to_whatsapp_layout() -> Result<()> {
        let settings = serde_json::from_str::<AppSettings>(
            r#"{
                "conversation_presentation": "unified"
            }"#,
        )?;

        assert_eq!(
            settings.conversation_presentation,
            ConversationPresentationSetting::WhatsApp
        );
        Ok(())
    }

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
            kind: ChatKind::Direct,
            membership: ChatMembership::Joined,
            is_shared: false,
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

        let messages = store
            .get_messages_for_chat(&chat.account, &chat.id, None, 10)
            .await?;
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
    async fn chat_upsert_keeps_newer_preview_when_provider_metadata_is_stale() -> Result<()> {
        let store = Store::open_memory().await?;
        let account_id = arc_str("whatsapp:stale".to_owned());
        let chat_id = arc_str("whatsapp:chat:vlad".to_owned());
        store
            .upsert_account(
                &Account {
                    id: account_id.clone(),
                    platform: Platform::WhatsApp,
                    display_name: arc_str("WhatsApp".to_owned()),
                    avatar: None,
                },
                "{}",
            )
            .await?;
        let newer = Utc
            .with_ymd_and_hms(2026, 6, 6, 22, 0, 0)
            .single()
            .expect("valid timestamp");
        let older = newer - chrono::Duration::days(1);

        store
            .upsert_chat(&Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Vlad Ghenu".to_owned()),
                avatar: None,
                is_group: false,
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(newer),
                last_message_preview: Some(arc_str("yesterday's message".to_owned())),
                thread_id: None,
            })
            .await?;
        store
            .upsert_chat(&Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Vlad Ghenu".to_owned()),
                avatar: None,
                is_group: false,
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: None,
                last_message_preview: None,
                thread_id: None,
            })
            .await?;
        store
            .upsert_chat(&Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Vlad Ghenu".to_owned()),
                avatar: None,
                is_group: false,
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(older),
                last_message_preview: Some(arc_str("older provider preview".to_owned())),
                thread_id: None,
            })
            .await?;

        let chat = store.get_chats(&account_id).await?.remove(0);
        assert_eq!(chat.last_message_at, Some(newer));
        assert_eq!(
            chat.last_message_preview.as_deref(),
            Some("yesterday's message")
        );
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
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
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

        let messages = store
            .get_messages_for_chat(&account_id, &chat_id, None, 10)
            .await?;
        let ids = messages
            .iter()
            .map(|message| message.id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["msg-1", "msg-2", "msg-3"]);

        Ok(())
    }

    #[tokio::test]
    async fn batch_upsert_messages_persists_messages_chronologically() -> Result<()> {
        let store = Store::open_memory().await?;
        let account_id = arc_str("mock:batch".to_owned());
        let chat_id = arc_str("mock:chat:batch".to_owned());
        let base = Utc
            .with_ymd_and_hms(2026, 6, 5, 4, 0, 0)
            .single()
            .expect("valid timestamp");

        store
            .upsert_account(
                &Account {
                    id: account_id.clone(),
                    platform: Platform::Unknown("mock".to_owned()),
                    display_name: arc_str("Mock Account".to_owned()),
                    avatar: None,
                },
                "{}",
            )
            .await?;
        store
            .upsert_chat(&Chat {
                id: chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::Unknown("mock".to_owned()),
                name: arc_str("Batch Chat".to_owned()),
                avatar: None,
                is_group: false,
                kind: ChatKind::Direct,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: Some(base + chrono::Duration::minutes(2)),
                last_message_preview: Some(arc_str("second".to_owned())),
                thread_id: None,
            })
            .await?;

        let sender = Sender {
            platform_id: arc_str("alice".to_owned()),
            display_name: arc_str("Alice".to_owned()),
            avatar: None,
        };
        let messages = [
            Message {
                id: arc_str("batch-2".to_owned()),
                chat_id: chat_id.clone(),
                account: account_id.clone(),
                sender: sender.clone(),
                timestamp: base + chrono::Duration::minutes(2),
                edited_at: None,
                content: Content::Text(arc_str("second".to_owned())),
                reply_to: None,
                thread_id: None,
                reactions: Vec::new(),
                receipts: Vec::new(),
                is_from_me: false,
                platform_data: PlatformData::default(),
            },
            Message {
                id: arc_str("batch-1".to_owned()),
                chat_id: chat_id.clone(),
                account: account_id.clone(),
                sender,
                timestamp: base + chrono::Duration::minutes(1),
                edited_at: None,
                content: Content::Text(arc_str("first".to_owned())),
                reply_to: None,
                thread_id: None,
                reactions: Vec::new(),
                receipts: Vec::new(),
                is_from_me: false,
                platform_data: PlatformData::default(),
            },
        ];

        store.upsert_messages(&messages).await?;

        let persisted = store
            .get_messages_for_chat(&account_id, &chat_id, None, 10)
            .await?;
        let ids = persisted
            .iter()
            .map(|message| message.id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["batch-1", "batch-2"]);
        Ok(())
    }

    #[tokio::test]
    async fn delete_chats_not_in_prunes_stale_account_chats() -> Result<()> {
        let store = Store::open_memory().await?;
        let account_id = arc_str("slack:test".to_owned());
        let account = Account {
            id: account_id.clone(),
            platform: Platform::Slack,
            display_name: arc_str("Slack Test".to_owned()),
            avatar: None,
        };
        store.upsert_account(&account, "{}").await?;

        for (id, membership) in [
            ("CJOINED", ChatMembership::Joined),
            ("CSTALE", ChatMembership::NotJoined),
        ] {
            store
                .upsert_chat(&Chat {
                    id: arc_str(id.to_owned()),
                    account: account_id.clone(),
                    platform: Platform::Slack,
                    name: arc_str(id.to_owned()),
                    avatar: None,
                    is_group: true,
                    kind: ChatKind::PublicChannel,
                    membership,
                    is_shared: false,
                    unread_count: 0,
                    muted: false,
                    pinned: false,
                    last_message_at: None,
                    last_message_preview: None,
                    thread_id: None,
                })
                .await?;
        }

        let deleted = store
            .delete_chats_not_in(&account_id, &[arc_str("CJOINED".to_owned())])
            .await?;
        let chats = store.get_chats(&account_id).await?;

        assert_eq!(deleted, 1);
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].id.as_ref(), "CJOINED");
        Ok(())
    }
}
