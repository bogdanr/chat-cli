mod schema;

use anyhow::{Context, Result, anyhow};
use chat_core::*;
use chrono::{TimeZone, Utc};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatInboxStyle {
    #[default]
    ActivityFirst,
    RecentFlat,
    PeopleFirst,
    GroupsFirst,
    AccountSeparated,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiThemePreset {
    #[default]
    DefaultDark,
    Light,
    HighContrast,
    WhatsApp,
    Slack,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationPresentationSetting {
    ProviderNative,
    #[serde(alias = "unified")]
    #[default]
    WhatsApp,
    Slack,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImagePreviewMode {
    #[default]
    Matrix,
    Hd,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkActivityDisplay {
    Hidden,
    #[default]
    CombinedLights,
    RecentCounts,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMode {
    Off,
    #[default]
    Desktop,
    InApp,
}

/// Which incoming messages are eligible for notifications. Applies on top of
/// [`NotificationMode`]: it never enables notifications that the mode disables,
/// it only further restricts which messages qualify.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationScope {
    /// Notify for every eligible message, including group and channel traffic.
    #[default]
    All,
    /// Notify only for direct (1:1) messages and messages that mention the
    /// authenticated user. Group and channel messages without a mention are
    /// suppressed.
    DirectAndMentions,
}

/// Which thread-activity markers the sidebar shows. Applies to the `⤷N`
/// marker rendered under a chat's name; the underlying per-thread unread
/// counters are always tracked so changing this setting is retroactive.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadMarkerScope {
    /// Count only threads the user participates in (authored the root,
    /// replied, or was mentioned). Other thread activity renders as a dim
    /// glyph without a count.
    #[default]
    Participating,
    /// Count every unread thread reply, regardless of participation.
    All,
    /// Hide thread markers entirely.
    None,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppSettings {
    pub notifications: NotificationMode,
    pub notification_scope: NotificationScope,
    /// When enabled, messages the authenticated user sent themselves (e.g. a
    /// Slack/WhatsApp "note to self" or a message echoed from another device)
    /// are eligible for notifications. Defaults to `true` so users can verify
    /// notifications end-to-end by sending themselves a message from web clients.
    pub notify_self_messages: bool,
    pub chat_inbox_style: ChatInboxStyle,
    pub ui_theme: UiThemePreset,
    pub conversation_presentation: ConversationPresentationSetting,
    pub image_preview_mode: ImagePreviewMode,
    pub network_activity: NetworkActivityDisplay,
    pub show_muted_chats: bool,
    pub show_browse_channels: bool,
    pub show_empty_chats: bool,
    /// Scope of the sidebar `⤷N` unread-thread marker.
    pub thread_marker_scope: ThreadMarkerScope,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct NotificationPauseState {
    pub paused_until: Option<chrono::DateTime<Utc>>,
}

impl NotificationPauseState {
    pub fn is_paused_at(&self, now: chrono::DateTime<Utc>) -> bool {
        self.paused_until
            .is_some_and(|paused_until| paused_until > now)
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatAvatarHint {
    pub account_id: ProviderId,
    pub chat_id: ChatId,
    pub avatar: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvatarThumbnailCacheRecord {
    pub cache_key: String,
    pub source_kind: String,
    pub source_path: PathBuf,
    pub source_mtime: Option<i64>,
    pub source_size: Option<i64>,
    pub image_format: String,
    pub image_blob: Vec<u8>,
    pub cache_version: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvatarThumbnailCacheUpsert {
    pub cache_key: String,
    pub source_kind: String,
    pub source_path: PathBuf,
    pub source_mtime: Option<i64>,
    pub source_size: Option<i64>,
    pub image_format: String,
    pub image_blob: Vec<u8>,
    pub cache_version: i64,
}

impl AvatarThumbnailCacheUpsert {
    pub fn blob_bytes(&self) -> i64 {
        self.image_blob.len() as i64
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            notifications: NotificationMode::Desktop,
            notification_scope: NotificationScope::All,
            notify_self_messages: true,
            chat_inbox_style: ChatInboxStyle::ActivityFirst,
            ui_theme: UiThemePreset::DefaultDark,
            conversation_presentation: ConversationPresentationSetting::WhatsApp,
            image_preview_mode: ImagePreviewMode::Matrix,
            network_activity: NetworkActivityDisplay::CombinedLights,
            show_muted_chats: true,
            show_browse_channels: false,
            show_empty_chats: true,
            thread_marker_scope: ThreadMarkerScope::Participating,
        }
    }
}

impl<'de> Deserialize<'de> for AppSettings {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(default)]
        struct AppSettingsCompat {
            notifications: Option<NotificationMode>,
            desktop_notifications: Option<bool>,
            in_app_notifications: Option<bool>,
            notification_scope: NotificationScope,
            notify_self_messages: bool,
            chat_inbox_style: ChatInboxStyle,
            ui_theme: UiThemePreset,
            conversation_presentation: ConversationPresentationSetting,
            image_preview_mode: ImagePreviewMode,
            network_activity: NetworkActivityDisplay,
            show_muted_chats: bool,
            show_browse_channels: bool,
            show_empty_chats: bool,
            thread_marker_scope: ThreadMarkerScope,
        }

        impl Default for AppSettingsCompat {
            fn default() -> Self {
                let defaults = AppSettings::default();
                Self {
                    notifications: None,
                    desktop_notifications: None,
                    in_app_notifications: None,
                    notification_scope: defaults.notification_scope,
                    notify_self_messages: defaults.notify_self_messages,
                    chat_inbox_style: defaults.chat_inbox_style,
                    ui_theme: defaults.ui_theme,
                    conversation_presentation: defaults.conversation_presentation,
                    image_preview_mode: defaults.image_preview_mode,
                    network_activity: defaults.network_activity,
                    show_muted_chats: defaults.show_muted_chats,
                    show_browse_channels: defaults.show_browse_channels,
                    show_empty_chats: defaults.show_empty_chats,
                    thread_marker_scope: defaults.thread_marker_scope,
                }
            }
        }

        let compat = AppSettingsCompat::deserialize(deserializer)?;
        let notifications = compat.notifications.unwrap_or_else(|| {
            if compat.desktop_notifications.unwrap_or(false) {
                NotificationMode::Desktop
            } else if compat.in_app_notifications.unwrap_or(false) {
                NotificationMode::InApp
            } else {
                NotificationMode::Off
            }
        });

        Ok(Self {
            notifications,
            notification_scope: compat.notification_scope,
            notify_self_messages: compat.notify_self_messages,
            chat_inbox_style: compat.chat_inbox_style,
            ui_theme: compat.ui_theme,
            conversation_presentation: compat.conversation_presentation,
            image_preview_mode: compat.image_preview_mode,
            network_activity: compat.network_activity,
            show_muted_chats: compat.show_muted_chats,
            show_browse_channels: compat.show_browse_channels,
            show_empty_chats: compat.show_empty_chats,
            thread_marker_scope: compat.thread_marker_scope,
        })
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
        ensure_message_metadata_columns(&conn).context("adding message metadata columns")?;

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

    pub async fn notification_pause_state(&self) -> Result<NotificationPauseState> {
        self.get_setting("notifications.pause")
            .await
            .map(|state| state.unwrap_or_default())
    }

    pub async fn save_notification_pause_state(
        &self,
        state: &NotificationPauseState,
    ) -> Result<()> {
        self.set_setting("notifications.pause", state).await
    }

    pub async fn pause_notifications_until(
        &self,
        paused_until: chrono::DateTime<Utc>,
    ) -> Result<()> {
        self.save_notification_pause_state(&NotificationPauseState {
            paused_until: Some(paused_until),
        })
        .await
    }

    pub async fn avatar_thumbnail_cache_entries(
        &self,
        cache_keys: &[String],
    ) -> Result<Vec<AvatarThumbnailCacheRecord>> {
        if cache_keys.is_empty() {
            return Ok(Vec::new());
        }

        let now = Utc::now().timestamp_millis();
        let mut conn = self.conn.lock().await;
        let placeholders = std::iter::repeat_n("?", cache_keys.len())
            .collect::<Vec<_>>()
            .join(",");
        let query = format!(
            "SELECT cache_key, source_kind, source_path, source_mtime, source_size,
                    image_format, image_blob, cache_version, updated_at
             FROM avatar_thumbnail_cache
             WHERE cache_key IN ({placeholders})"
        );
        let records = {
            let mut stmt = conn.prepare(&query)?;
            let rows = stmt.query_map(params_from_iter(cache_keys.iter()), |row| {
                Ok(AvatarThumbnailCacheRecord {
                    cache_key: row.get(0)?,
                    source_kind: row.get(1)?,
                    source_path: PathBuf::from(row.get::<_, String>(2)?),
                    source_mtime: row.get(3)?,
                    source_size: row.get(4)?,
                    image_format: row.get(5)?,
                    image_blob: row.get(6)?,
                    cache_version: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let tx = conn.transaction()?;
        for cache_key in records.iter().map(|record| &record.cache_key) {
            tx.execute(
                "UPDATE avatar_thumbnail_cache SET last_accessed_at = ?2 WHERE cache_key = ?1",
                params![cache_key, now],
            )?;
        }
        tx.commit()?;

        Ok(records)
    }

    pub async fn upsert_avatar_thumbnail_cache(
        &self,
        entry: &AvatarThumbnailCacheUpsert,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO avatar_thumbnail_cache (
                cache_key, source_kind, source_path, source_mtime, source_size,
                image_format, image_blob, blob_bytes, cache_version,
                created_at, updated_at, last_accessed_at, error_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?10, NULL)
             ON CONFLICT(cache_key) DO UPDATE SET
                source_kind = excluded.source_kind,
                source_path = excluded.source_path,
                source_mtime = excluded.source_mtime,
                source_size = excluded.source_size,
                image_format = excluded.image_format,
                image_blob = excluded.image_blob,
                blob_bytes = excluded.blob_bytes,
                cache_version = excluded.cache_version,
                updated_at = excluded.updated_at,
                last_accessed_at = excluded.last_accessed_at,
                error_json = NULL",
            params![
                entry.cache_key,
                entry.source_kind,
                entry.source_path.to_string_lossy().as_ref(),
                entry.source_mtime,
                entry.source_size,
                entry.image_format,
                entry.image_blob,
                entry.blob_bytes(),
                entry.cache_version,
                now,
            ],
        )?;
        Ok(())
    }

    pub async fn resume_notifications(&self) -> Result<()> {
        self.save_notification_pause_state(&NotificationPauseState { paused_until: None })
            .await
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
                    m.media_thumbnail, m.reply_to_id, m.thread_id, m.is_from_me, m.platform_json,
                    m.mentions_me
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

    pub async fn latest_sender_avatar_for_each_chat(&self) -> Result<Vec<ChatAvatarHint>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT m.account_id, m.chat_id, m.sender_avatar
             FROM messages m
             JOIN chats c
               ON c.account_id = m.account_id
              AND c.id = m.chat_id
             WHERE c.platform = ?1
               AND c.kind = ?2
               AND c.avatar_path IS NULL
               AND c.id != 'whatsapp:status@broadcast'
               AND NOT (LOWER(c.name) = 'status' AND instr(c.id, 'status') > 0)
               AND m.sender_avatar IS NOT NULL
               AND m.sender_avatar != ''
               AND m.timestamp = (
                   SELECT MAX(m2.timestamp)
                   FROM messages m2
                   WHERE m2.account_id = m.account_id
                     AND m2.chat_id = m.chat_id
                     AND m2.sender_avatar IS NOT NULL
                     AND m2.sender_avatar != ''
               )
               AND m.id = (
                   SELECT m3.id
                   FROM messages m3
                   WHERE m3.account_id = m.account_id
                     AND m3.chat_id = m.chat_id
                     AND m3.sender_avatar IS NOT NULL
                     AND m3.sender_avatar != ''
                     AND m3.timestamp = m.timestamp
                   ORDER BY m3.id DESC
                   LIMIT 1
               )",
        )?;
        let hints = stmt
            .query_map(
                params![
                    platform_to_str(&Platform::WhatsApp),
                    chat_kind_to_str(ChatKind::Direct)
                ],
                |row| {
                    Ok(ChatAvatarHint {
                        account_id: arc_str(row.get::<_, String>(0)?),
                        chat_id: arc_str(row.get::<_, String>(1)?),
                        avatar: PathBuf::from(row.get::<_, String>(2)?),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hints)
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
                        media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json, mentions_me
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
                        media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json, mentions_me
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
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json, mentions_me
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
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json, mentions_me
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

    /// Return all messages belonging to a single thread (root + replies),
    /// oldest first. Uses the `idx_messages_thread` index.
    pub async fn get_messages_for_thread(
        &self,
        account_id: &ProviderId,
        thread_id: &ThreadId,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let conn = self.conn.lock().await;
        let limit = usize_to_i64(limit)?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, account_id, sender_id, sender_name, sender_avatar, timestamp, edited_at,
                    content_type, content_text, content_caption, media_id, media_filename, media_mime,
                    media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json, mentions_me
             FROM messages WHERE account_id = ?1 AND thread_id = ?2 ORDER BY timestamp ASC LIMIT ?3",
        )?;
        let mut messages = stmt
            .query_map(
                params![account_id.as_ref(), thread_id.as_ref(), limit],
                message_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for msg in &mut messages {
            hydrate_reactions_and_receipts(&conn, msg)?;
        }
        Ok(messages)
    }

    /// Aggregate thread summaries for a single chat (reply counts, participants,
    /// last reply time, unread replies), ordered by most recent reply first.
    pub async fn thread_summaries_for_chat(
        &self,
        account_id: &ProviderId,
        chat_id: &ChatId,
    ) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().await;
        thread_summaries_on_conn(&conn, account_id, Some(chat_id))
    }

    /// Aggregate thread summaries across all chats for an account that currently
    /// have unread replies, ordered by most recent reply first. Backs the
    /// Threads inbox view.
    pub async fn unread_thread_summaries(
        &self,
        account_id: &ProviderId,
    ) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().await;
        let mut summaries = thread_summaries_on_conn(&conn, account_id, None)?;
        summaries.retain(|summary| summary.unread_reply_count > 0);
        Ok(summaries)
    }

    /// Current unread reply count for a single thread (0 when untracked).
    pub async fn thread_unread_count(
        &self,
        account_id: &ProviderId,
        thread_id: &ThreadId,
    ) -> Result<u32> {
        let conn = self.conn.lock().await;
        thread_unread_count_on_conn(&conn, account_id, thread_id)
    }

    /// How the authenticated user relates to a single thread, derived from
    /// stored messages (authored root / replied / mentioned). Bounded by the
    /// `idx_messages_thread` index, so it is safe to call from live event
    /// drains off the draw path.
    pub async fn thread_participation(
        &self,
        account_id: &ProviderId,
        thread_id: &ThreadId,
    ) -> Result<ThreadParticipation> {
        let conn = self.conn.lock().await;
        let (authored_root, replied, mentioned): (bool, bool, bool) = conn.query_row(
            "SELECT
                COALESCE(MAX(CASE WHEN id = thread_id AND is_from_me THEN 1 ELSE 0 END), 0),
                COALESCE(MAX(CASE WHEN id != thread_id AND is_from_me THEN 1 ELSE 0 END), 0),
                COALESCE(MAX(CASE WHEN mentions_me THEN 1 ELSE 0 END), 0)
             FROM messages WHERE account_id = ?1 AND thread_id = ?2",
            params![account_id.as_ref(), thread_id.as_ref()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? != 0,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)? != 0,
                ))
            },
        )?;
        Ok(if authored_root {
            ThreadParticipation::Author
        } else if replied {
            ThreadParticipation::Replied
        } else if mentioned {
            ThreadParticipation::Mentioned
        } else {
            ThreadParticipation::None
        })
    }

    /// Increment the unread reply counter for a thread by one. Intended for the
    /// live (non-historical) arrival path only, mirroring how chat-level unread
    /// is bumped, so historical replay never inflates unread counts.
    pub async fn bump_thread_unread(
        &self,
        account_id: &ProviderId,
        thread_id: &ThreadId,
    ) -> Result<u32> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO thread_reads (account_id, thread_id, unread_count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(account_id, thread_id) DO UPDATE SET unread_count = unread_count + 1",
            params![account_id.as_ref(), thread_id.as_ref()],
        )?;
        thread_unread_count_on_conn(&conn, account_id, thread_id)
    }

    /// Mark a thread as read, clearing its unread reply counter and recording
    /// the read watermark. Safe to call repeatedly.
    pub async fn mark_thread_read(
        &self,
        account_id: &ProviderId,
        thread_id: &ThreadId,
        last_read_message_id: Option<&MessageId>,
        at: Option<Timestamp>,
    ) -> Result<()> {
        let conn = self.conn.lock().await;
        let at_millis = at.map(|t| t.timestamp_millis());
        let last_read = last_read_message_id.map(|id| id.as_ref());
        conn.execute(
            "INSERT INTO thread_reads (account_id, thread_id, unread_count, last_read_at, last_read_message_id)
             VALUES (?1, ?2, 0, ?3, ?4)
             ON CONFLICT(account_id, thread_id) DO UPDATE SET
                unread_count = 0,
                last_read_at = excluded.last_read_at,
                last_read_message_id = excluded.last_read_message_id",
            params![account_id.as_ref(), thread_id.as_ref(), at_millis, last_read],
        )?;
        Ok(())
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
           avatar_path = COALESCE(excluded.avatar_path, chats.avatar_path),
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

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredCards {
    cards: Vec<StoredCard>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredCard {
    kind: String,
    source: String,
    title: Option<String>,
    subtitle: Option<String>,
    body: Option<String>,
    footer: Option<String>,
    url: Option<String>,
    accent_color: Option<String>,
    fields: Vec<StoredCardField>,
    actions: Vec<StoredCardAction>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredCardField {
    title: Option<String>,
    value: String,
    short: bool,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredCardAction {
    label: String,
    url: Option<String>,
}

impl StoredCards {
    fn from_cards(cards: &[Card]) -> Self {
        Self {
            cards: cards.iter().map(StoredCard::from_card).collect(),
        }
    }

    fn into_cards(self) -> Vec<Card> {
        self.cards.into_iter().map(StoredCard::into_card).collect()
    }
}

impl StoredCard {
    fn from_card(card: &Card) -> Self {
        Self {
            kind: card_kind_to_str(card.kind).to_owned(),
            source: card_source_to_str(&card.source),
            title: card.title.as_ref().map(ToString::to_string),
            subtitle: card.subtitle.as_ref().map(ToString::to_string),
            body: card.body.as_ref().map(ToString::to_string),
            footer: card.footer.as_ref().map(ToString::to_string),
            url: card.url.as_ref().map(ToString::to_string),
            accent_color: card.accent_color.as_ref().map(card_color_to_str),
            fields: card
                .fields
                .iter()
                .map(|field| StoredCardField {
                    title: field.title.as_ref().map(ToString::to_string),
                    value: field.value.to_string(),
                    short: field.short,
                })
                .collect(),
            actions: card
                .actions
                .iter()
                .map(|action| StoredCardAction {
                    label: action.label.to_string(),
                    url: action.url.as_ref().map(ToString::to_string),
                })
                .collect(),
        }
    }

    fn into_card(self) -> Card {
        Card {
            kind: card_kind_from_str(&self.kind),
            source: card_source_from_str(&self.source),
            title: self.title.map(arc_str),
            subtitle: self.subtitle.map(arc_str),
            body: self.body.map(arc_str),
            footer: self.footer.map(arc_str),
            url: self.url.map(arc_str),
            accent_color: self.accent_color.map(card_color_from_str),
            thumbnail: None,
            image: None,
            fields: self
                .fields
                .into_iter()
                .map(|field| CardField {
                    title: field.title.map(arc_str),
                    value: arc_str(field.value),
                    short: field.short,
                })
                .collect(),
            actions: self
                .actions
                .into_iter()
                .map(|action| CardAction {
                    label: arc_str(action.label),
                    url: action.url.map(arc_str),
                })
                .collect(),
        }
    }
}

fn card_kind_to_str(kind: CardKind) -> &'static str {
    match kind {
        CardKind::LinkPreview => "link_preview",
        CardKind::ProviderAttachment => "provider_attachment",
        CardKind::BotMessage => "bot_message",
        CardKind::MediaPreview => "media_preview",
        CardKind::SocialPreview => "social_preview",
        CardKind::Unknown => "unknown",
    }
}

fn card_kind_from_str(value: &str) -> CardKind {
    match value {
        "link_preview" => CardKind::LinkPreview,
        "provider_attachment" => CardKind::ProviderAttachment,
        "bot_message" => CardKind::BotMessage,
        "media_preview" => CardKind::MediaPreview,
        "social_preview" => CardKind::SocialPreview,
        _ => CardKind::Unknown,
    }
}

fn card_source_to_str(source: &CardSource) -> String {
    match source {
        CardSource::Slack => "slack".to_owned(),
        CardSource::WhatsApp => "whatsapp".to_owned(),
        CardSource::OpenGraph => "open_graph".to_owned(),
        CardSource::YouTube => "youtube".to_owned(),
        CardSource::Instagram => "instagram".to_owned(),
        CardSource::Facebook => "facebook".to_owned(),
        CardSource::GenericUrl => "generic_url".to_owned(),
        CardSource::Unknown(value) => value.to_string(),
    }
}

fn card_source_from_str(value: &str) -> CardSource {
    match value {
        "slack" => CardSource::Slack,
        "whatsapp" => CardSource::WhatsApp,
        "open_graph" => CardSource::OpenGraph,
        "youtube" => CardSource::YouTube,
        "instagram" => CardSource::Instagram,
        "facebook" => CardSource::Facebook,
        "generic_url" => CardSource::GenericUrl,
        other => CardSource::Unknown(arc_str(other.to_owned())),
    }
}

fn card_color_to_str(color: &CardColor) -> String {
    match color {
        CardColor::Named(value) => value.to_string(),
        CardColor::Hex(value) => format!("#{value}"),
    }
}

fn card_color_from_str(value: String) -> CardColor {
    let trimmed = value.trim().trim_start_matches('#');
    if trimmed.len() == 6 && trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        CardColor::Hex(arc_str(trimmed.to_owned()))
    } else {
        CardColor::Named(arc_str(value))
    }
}

fn cards_from_text(text: Option<String>) -> Content {
    let text = text.unwrap_or_default();
    serde_json::from_str::<StoredCards>(&text)
        .map(|stored| Content::Cards(stored.into_cards()))
        .unwrap_or_else(|_| Content::Unsupported(arc_str("cards".to_owned())))
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
            Content::Cards(cards) => Self::text(
                "cards",
                Some(serde_json::to_string(&StoredCards::from_cards(cards)).unwrap_or_default()),
            ),
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

/// Adds message columns introduced after the initial schema. `mentions_me`
/// backs mention-based thread participation; pre-migration rows default to 0,
/// so mention participation only applies to messages stored after the
/// migration (authored-by-me participation works retroactively via
/// `is_from_me`).
fn ensure_message_metadata_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;

    if !columns.contains("mentions_me") {
        conn.execute(
            "ALTER TABLE messages ADD COLUMN mentions_me INTEGER NOT NULL DEFAULT 0",
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
        mentions_me: row.get::<_, Option<bool>>(21)?.unwrap_or(false),
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
        "cards" => cards_from_text(text),
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
        "cards": StoredCards::from_cards(&data.cards),
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

    let cards = value
        .get("cards")
        .cloned()
        .and_then(|value| serde_json::from_value::<StoredCards>(value).ok())
        .map(StoredCards::into_cards)
        .unwrap_or_default();

    PlatformData {
        whatsapp,
        slack,
        cards,
    }
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
                media_size, media_local, media_thumbnail, reply_to_id, thread_id, is_from_me, platform_json,
                mentions_me
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
             ON CONFLICT(id, account_id) DO UPDATE SET
                chat_id = excluded.chat_id,
                sender_id = excluded.sender_id,
                sender_name = excluded.sender_name,
                sender_avatar = COALESCE(excluded.sender_avatar, messages.sender_avatar),
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
                platform_json = excluded.platform_json,
                mentions_me = excluded.mentions_me",
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
            msg.mentions_me,
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

fn thread_unread_count_on_conn(
    conn: &Connection,
    account_id: &ProviderId,
    thread_id: &ThreadId,
) -> Result<u32> {
    let count: Option<i64> = conn
        .query_row(
            "SELECT unread_count FROM thread_reads WHERE account_id = ?1 AND thread_id = ?2",
            params![account_id.as_ref(), thread_id.as_ref()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(u32::try_from(count.unwrap_or(0).max(0)).unwrap_or(0))
}

/// Short preview text for a message row given its stored content columns.
fn row_preview(kind: &str, text: Option<String>, caption: Option<String>) -> Option<Arc<str>> {
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        return Some(arc_str(text));
    }
    if let Some(caption) = caption.filter(|c| !c.is_empty()) {
        return Some(arc_str(caption));
    }
    match kind {
        "text" | "" => None,
        "image" => Some(arc_str("Photo".to_owned())),
        "video" => Some(arc_str("Video".to_owned())),
        "audio" => Some(arc_str("Audio".to_owned())),
        "file" => Some(arc_str("File".to_owned())),
        "sticker" => Some(arc_str("Sticker".to_owned())),
        "poll" => Some(arc_str("Poll".to_owned())),
        other => Some(arc_str(other.to_owned())),
    }
}

struct ThreadAgg {
    chat_id: Arc<str>,
    root_preview: Option<Arc<str>>,
    reply_count: u32,
    last_reply_at: Option<i64>,
    participants: Vec<Arc<str>>,
    authored_root: bool,
    replied_by_me: bool,
    mentioned_me: bool,
}

impl ThreadAgg {
    fn participation(&self) -> ThreadParticipation {
        if self.authored_root {
            ThreadParticipation::Author
        } else if self.replied_by_me {
            ThreadParticipation::Replied
        } else if self.mentioned_me {
            ThreadParticipation::Mentioned
        } else {
            ThreadParticipation::None
        }
    }
}

/// Build [`ThreadSummary`] aggregates for an account, optionally scoped to a
/// single chat. Threads with no replies (root-only) are omitted. Results are
/// ordered by most recent reply first.
fn thread_summaries_on_conn(
    conn: &Connection,
    account_id: &ProviderId,
    chat_id: Option<&ChatId>,
) -> Result<Vec<ThreadSummary>> {
    let mut unread_map: HashMap<Arc<str>, u32> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT thread_id, unread_count FROM thread_reads WHERE account_id = ?1")?;
        let rows = stmt.query_map(params![account_id.as_ref()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (thread_id, count) = row?;
            unread_map.insert(arc_str(thread_id), u32::try_from(count.max(0)).unwrap_or(0));
        }
    }

    let mut order: Vec<Arc<str>> = Vec::new();
    let mut aggs: HashMap<Arc<str>, ThreadAgg> = HashMap::new();

    let select = "SELECT id, chat_id, thread_id, sender_name, timestamp, is_from_me, content_type, content_text, content_caption, mentions_me
                  FROM messages
                  WHERE account_id = ?1 AND thread_id IS NOT NULL AND thread_id != ''";
    let mut handle_row = |id: String,
                          row_chat: String,
                          thread_id: String,
                          sender: String,
                          timestamp: i64,
                          is_from_me: bool,
                          content_type: String,
                          content_text: Option<String>,
                          content_caption: Option<String>,
                          mentions_me: bool| {
        let thread_id = arc_str(thread_id);
        let agg = aggs.entry(thread_id.clone()).or_insert_with(|| {
            order.push(thread_id.clone());
            ThreadAgg {
                chat_id: arc_str(row_chat),
                root_preview: None,
                reply_count: 0,
                last_reply_at: None,
                participants: Vec::new(),
                authored_root: false,
                replied_by_me: false,
                mentioned_me: false,
            }
        });
        if mentions_me {
            agg.mentioned_me = true;
        }
        let is_root = id == thread_id.as_ref();
        if is_root {
            agg.root_preview = row_preview(&content_type, content_text, content_caption);
            if is_from_me {
                agg.authored_root = true;
            }
        } else {
            agg.reply_count = agg.reply_count.saturating_add(1);
            if is_from_me {
                agg.replied_by_me = true;
            }
            agg.last_reply_at = Some(match agg.last_reply_at {
                Some(existing) => existing.max(timestamp),
                None => timestamp,
            });
            let sender = arc_str(sender);
            if !agg.participants.contains(&sender) {
                agg.participants.push(sender);
            }
        }
    };

    if let Some(chat_id) = chat_id {
        let mut stmt =
            conn.prepare(&format!("{select} AND chat_id = ?2 ORDER BY timestamp ASC"))?;
        let rows = stmt.query_map(params![account_id.as_ref(), chat_id.as_ref()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<bool>>(9)?.unwrap_or(false),
            ))
        })?;
        for row in rows {
            let (id, c, t, s, ts, mine, ct, txt, cap, mentioned) = row?;
            handle_row(id, c, t, s, ts, mine, ct, txt, cap, mentioned);
        }
    } else {
        let mut stmt = conn.prepare(&format!("{select} ORDER BY timestamp ASC"))?;
        let rows = stmt.query_map(params![account_id.as_ref()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<bool>>(9)?.unwrap_or(false),
            ))
        })?;
        for row in rows {
            let (id, c, t, s, ts, mine, ct, txt, cap, mentioned) = row?;
            handle_row(id, c, t, s, ts, mine, ct, txt, cap, mentioned);
        }
    }

    let mut summaries: Vec<ThreadSummary> = order
        .into_iter()
        .filter_map(|thread_id| {
            let agg = aggs.remove(&thread_id)?;
            if agg.reply_count == 0 {
                return None;
            }
            let unread = unread_map
                .get(&thread_id)
                .copied()
                .unwrap_or(0)
                .min(agg.reply_count);
            let participation = agg.participation();
            Some(ThreadSummary {
                account: account_id.clone(),
                chat_id: agg.chat_id,
                root_id: thread_id.clone(),
                thread_id,
                root_preview: agg.root_preview,
                reply_count: agg.reply_count,
                unread_reply_count: unread,
                last_reply_at: millis_to_timestamp(agg.last_reply_at),
                participants: agg.participants,
                participation,
            })
        })
        .collect();

    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_reply_at));
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn avatar_thumbnail_cache_roundtrips_blob_metadata() -> Result<()> {
        let store = Store::open_memory().await?;
        let entry = AvatarThumbnailCacheUpsert {
            cache_key: "avatar:v1:path:4x2".to_owned(),
            source_kind: "chat_avatar".to_owned(),
            source_path: PathBuf::from("/tmp/avatar.png"),
            source_mtime: Some(123),
            source_size: Some(456),
            image_format: "png".to_owned(),
            image_blob: vec![1, 2, 3, 4],
            cache_version: 1,
        };

        store.upsert_avatar_thumbnail_cache(&entry).await?;
        let loaded = store
            .avatar_thumbnail_cache_entries(&[entry.cache_key.clone(), "missing".to_owned()])
            .await?;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].cache_key, entry.cache_key);
        assert_eq!(loaded[0].source_kind, entry.source_kind);
        assert_eq!(loaded[0].source_path, entry.source_path);
        assert_eq!(loaded[0].source_mtime, entry.source_mtime);
        assert_eq!(loaded[0].source_size, entry.source_size);
        assert_eq!(loaded[0].image_format, entry.image_format);
        assert_eq!(loaded[0].image_blob, entry.image_blob);
        assert_eq!(loaded[0].cache_version, entry.cache_version);
        Ok(())
    }

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

    #[test]
    fn legacy_notification_booleans_migrate_to_notification_mode() -> Result<()> {
        let desktop = serde_json::from_str::<AppSettings>(
            r#"{
                "desktop_notifications": true,
                "in_app_notifications": true
            }"#,
        )?;
        assert_eq!(desktop.notifications, NotificationMode::Desktop);

        let in_app = serde_json::from_str::<AppSettings>(
            r#"{
                "desktop_notifications": false,
                "in_app_notifications": true
            }"#,
        )?;
        assert_eq!(in_app.notifications, NotificationMode::InApp);

        let off = serde_json::from_str::<AppSettings>(
            r#"{
                "desktop_notifications": false,
                "in_app_notifications": false
            }"#,
        )?;
        assert_eq!(off.notifications, NotificationMode::Off);
        Ok(())
    }

    #[test]
    fn notification_scope_defaults_to_all_and_roundtrips() -> Result<()> {
        // Legacy settings without the field migrate to the permissive default.
        let legacy = serde_json::from_str::<AppSettings>(
            r#"{
                "notifications": "desktop"
            }"#,
        )?;
        assert_eq!(legacy.notification_scope, NotificationScope::All);

        // An explicitly stored restricted scope survives a serialize/deserialize
        // round-trip.
        let settings = AppSettings {
            notification_scope: NotificationScope::DirectAndMentions,
            ..Default::default()
        };
        let json = serde_json::to_string(&settings)?;
        let restored = serde_json::from_str::<AppSettings>(&json)?;
        assert_eq!(
            restored.notification_scope,
            NotificationScope::DirectAndMentions
        );
        Ok(())
    }

    #[test]
    fn notify_self_messages_defaults_on_and_roundtrips() -> Result<()> {
        // Legacy settings without the field migrate to the enabled default, so
        // sending yourself a message from a web client can exercise the
        // notification pipeline without another settings toggle.
        let legacy = serde_json::from_str::<AppSettings>(
            r#"{
                "notifications": "desktop"
            }"#,
        )?;
        assert!(legacy.notify_self_messages);

        // An explicit stored value still survives a serialize/deserialize
        // round-trip for compatibility with existing config files.
        let settings = AppSettings {
            notify_self_messages: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&settings)?;
        let restored = serde_json::from_str::<AppSettings>(&json)?;
        assert!(!restored.notify_self_messages);
        Ok(())
    }

    #[tokio::test]
    async fn notification_pause_state_roundtrips_and_resumes() -> Result<()> {
        let store = Store::open_memory().await?;
        let paused_until = Utc::now() + chrono::Duration::minutes(25);

        store.pause_notifications_until(paused_until).await?;
        let paused = store.notification_pause_state().await?;
        assert_eq!(paused.paused_until, Some(paused_until));
        assert!(paused.is_paused_at(Utc::now()));

        store.resume_notifications().await?;
        let resumed = store.notification_pause_state().await?;
        assert_eq!(resumed.paused_until, None);
        assert!(!resumed.is_paused_at(Utc::now()));
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
            mentions_me: false,
            platform_data: PlatformData {
                whatsapp: Some(WhatsAppData {
                    jid: arc_str("alice@s.whatsapp.net".to_owned()),
                }),
                slack: None,
                cards: Vec::new(),
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
    async fn chat_upsert_preserves_cached_avatar_when_provider_snapshot_omits_it() -> Result<()> {
        let store = Store::open_memory().await?;
        let account_id = arc_str("whatsapp:avatar-preserve".to_owned());
        let chat_id = arc_str("whatsapp:123@s.whatsapp.net".to_owned());
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

        let mut chat = Chat {
            id: chat_id.clone(),
            account: account_id.clone(),
            platform: Platform::WhatsApp,
            name: arc_str("Ada Lovelace".to_owned()),
            avatar: Some(PathBuf::from("/tmp/ada-avatar.jpg")),
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
        };
        store.upsert_chat(&chat).await?;

        chat.avatar = None;
        chat.last_message_preview = Some(arc_str("fresh provider metadata".to_owned()));
        store.upsert_chat(&chat).await?;

        let chat = store.get_chats(&account_id).await?.remove(0);
        assert_eq!(
            chat.avatar.as_deref(),
            Some(Path::new("/tmp/ada-avatar.jpg"))
        );
        assert_eq!(
            chat.last_message_preview.as_deref(),
            Some("fresh provider metadata")
        );
        Ok(())
    }

    #[tokio::test]
    async fn latest_sender_avatar_hints_return_whatsapp_direct_chats_missing_avatars() -> Result<()>
    {
        let store = Store::open_memory().await?;
        let account_id = arc_str("whatsapp:avatar-hints".to_owned());
        let direct_chat_id = arc_str("whatsapp:123@s.whatsapp.net".to_owned());
        let group_chat_id = arc_str("whatsapp:group@g.us".to_owned());
        let status_chat_id = arc_str("whatsapp:status@broadcast".to_owned());
        let slack_chat_id = arc_str("C123".to_owned());
        let base = Utc
            .with_ymd_and_hms(2026, 6, 7, 9, 0, 0)
            .single()
            .expect("valid timestamp");
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
        store
            .upsert_account(
                &Account {
                    id: arc_str("slack:avatar-hints".to_owned()),
                    platform: Platform::Slack,
                    display_name: arc_str("Slack".to_owned()),
                    avatar: None,
                },
                "{}",
            )
            .await?;

        for chat in [
            Chat {
                id: direct_chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Ada Lovelace".to_owned()),
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
            },
            Chat {
                id: group_chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Family".to_owned()),
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
            },
            Chat {
                id: status_chat_id.clone(),
                account: account_id.clone(),
                platform: Platform::WhatsApp,
                name: arc_str("Status".to_owned()),
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
            },
            Chat {
                id: slack_chat_id.clone(),
                account: arc_str("slack:avatar-hints".to_owned()),
                platform: Platform::Slack,
                name: arc_str("#general".to_owned()),
                avatar: None,
                is_group: true,
                kind: ChatKind::PublicChannel,
                membership: ChatMembership::Joined,
                is_shared: false,
                unread_count: 0,
                muted: false,
                pinned: false,
                last_message_at: None,
                last_message_preview: None,
                thread_id: None,
            },
        ] {
            store.upsert_chat(&chat).await?;
        }

        for (chat_id, sender_avatar, minute) in [
            (
                direct_chat_id.clone(),
                Some(PathBuf::from("/tmp/ada-old.jpg")),
                1,
            ),
            (
                direct_chat_id.clone(),
                Some(PathBuf::from("/tmp/ada-new.jpg")),
                2,
            ),
            (
                group_chat_id.clone(),
                Some(PathBuf::from("/tmp/group-sender.jpg")),
                3,
            ),
            (
                status_chat_id.clone(),
                Some(PathBuf::from("/tmp/status-sender.jpg")),
                4,
            ),
            (
                slack_chat_id.clone(),
                Some(PathBuf::from("/tmp/slack-sender.jpg")),
                5,
            ),
        ] {
            store
                .upsert_message(&Message {
                    id: arc_str(format!("msg-{minute}")),
                    chat_id,
                    account: if minute == 5 {
                        arc_str("slack:avatar-hints".to_owned())
                    } else {
                        account_id.clone()
                    },
                    sender: Sender {
                        platform_id: arc_str("sender".to_owned()),
                        display_name: arc_str("Sender".to_owned()),
                        avatar: sender_avatar,
                    },
                    timestamp: base + chrono::Duration::minutes(minute),
                    edited_at: None,
                    content: Content::Text(arc_str("hello".to_owned())),
                    reply_to: None,
                    thread_id: None,
                    reactions: Vec::new(),
                    receipts: Vec::new(),
                    is_from_me: false,
                    mentions_me: false,
                    platform_data: PlatformData::default(),
                })
                .await?;
        }

        let hints = store.latest_sender_avatar_for_each_chat().await?;
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].account_id, account_id);
        assert_eq!(hints[0].chat_id, direct_chat_id);
        assert_eq!(hints[0].avatar, PathBuf::from("/tmp/ada-new.jpg"));
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
                    mentions_me: false,
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
                mentions_me: false,
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
                mentions_me: false,
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

    #[allow(clippy::too_many_arguments)]
    fn thread_message_with_flags(
        id: &str,
        chat_id: &ChatId,
        account: &ProviderId,
        sender: &str,
        text: &str,
        thread_id: &str,
        reply_to: Option<&str>,
        ts: Timestamp,
        is_from_me: bool,
        mentions_me: bool,
    ) -> Message {
        Message {
            id: arc_str(id.to_owned()),
            chat_id: chat_id.clone(),
            account: account.clone(),
            sender: Sender {
                platform_id: arc_str(sender.to_owned()),
                display_name: arc_str(sender.to_owned()),
                avatar: None,
            },
            timestamp: ts,
            edited_at: None,
            content: Content::Text(arc_str(text.to_owned())),
            reply_to: reply_to.map(|r| arc_str(r.to_owned())),
            thread_id: Some(arc_str(thread_id.to_owned())),
            reactions: Vec::new(),
            receipts: Vec::new(),
            is_from_me,
            mentions_me,
            platform_data: PlatformData::default(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn thread_message(
        id: &str,
        chat_id: &ChatId,
        account: &ProviderId,
        sender: &str,
        text: &str,
        thread_id: &str,
        reply_to: Option<&str>,
        ts: Timestamp,
    ) -> Message {
        thread_message_with_flags(
            id, chat_id, account, sender, text, thread_id, reply_to, ts, false, false,
        )
    }

    #[tokio::test]
    async fn thread_participation_classifies_author_reply_mention_and_none() -> Result<()> {
        let store = Store::open_memory().await?;
        let account = arc_str("acct-participation".to_owned());
        let chat_id = arc_str("chat-participation".to_owned());
        let base = Utc.timestamp_millis_opt(2_000_000).single().unwrap();

        for (thread, root_from_me, reply_from_me, mention_me, expected) in [
            (
                "thread-author",
                true,
                false,
                true,
                ThreadParticipation::Author,
            ),
            (
                "thread-replied",
                false,
                true,
                true,
                ThreadParticipation::Replied,
            ),
            (
                "thread-mentioned",
                false,
                false,
                true,
                ThreadParticipation::Mentioned,
            ),
            (
                "thread-other",
                false,
                false,
                false,
                ThreadParticipation::None,
            ),
        ] {
            store
                .upsert_message(&thread_message_with_flags(
                    thread,
                    &chat_id,
                    &account,
                    "Root",
                    "root message",
                    thread,
                    None,
                    base,
                    root_from_me,
                    false,
                ))
                .await?;
            store
                .upsert_message(&thread_message_with_flags(
                    &format!("{thread}-reply"),
                    &chat_id,
                    &account,
                    "Reply",
                    "reply message",
                    thread,
                    Some(thread),
                    base + chrono::Duration::seconds(1),
                    reply_from_me,
                    mention_me,
                ))
                .await?;

            assert_eq!(
                store
                    .thread_participation(&account, &arc_str(thread.to_owned()))
                    .await?,
                expected
            );
        }

        let summaries = store.thread_summaries_for_chat(&account, &chat_id).await?;
        let participation_by_thread = summaries
            .into_iter()
            .map(|summary| (summary.thread_id.to_string(), summary.participation))
            .collect::<HashMap<_, _>>();

        assert_eq!(
            participation_by_thread.get("thread-author"),
            Some(&ThreadParticipation::Author)
        );
        assert_eq!(
            participation_by_thread.get("thread-replied"),
            Some(&ThreadParticipation::Replied)
        );
        assert_eq!(
            participation_by_thread.get("thread-mentioned"),
            Some(&ThreadParticipation::Mentioned)
        );
        assert_eq!(
            participation_by_thread.get("thread-other"),
            Some(&ThreadParticipation::None)
        );
        Ok(())
    }

    #[tokio::test]
    async fn thread_summaries_and_unread_lifecycle() -> Result<()> {
        let store = Store::open_memory().await?;
        let account = arc_str("acct".to_owned());
        let chat_id = arc_str("chat".to_owned());
        let thread = "root-1";
        let base = Utc.timestamp_millis_opt(1_000_000).single().unwrap();

        // Root message + two replies from different senders.
        store
            .upsert_message(&thread_message(
                thread,
                &chat_id,
                &account,
                "Priya",
                "token names?",
                thread,
                None,
                base,
            ))
            .await?;
        store
            .upsert_message(&thread_message(
                "r1",
                &chat_id,
                &account,
                "Lee",
                "semantic names",
                thread,
                Some(thread),
                base + chrono::Duration::seconds(10),
            ))
            .await?;
        store
            .upsert_message(&thread_message(
                "r2",
                &chat_id,
                &account,
                "Sam",
                "shipping it",
                thread,
                Some(thread),
                base + chrono::Duration::seconds(20),
            ))
            .await?;

        let summaries = store.thread_summaries_for_chat(&account, &chat_id).await?;
        assert_eq!(summaries.len(), 1);
        let summary = &summaries[0];
        assert_eq!(summary.thread_id.as_ref(), thread);
        assert_eq!(summary.reply_count, 2);
        assert_eq!(
            summary.unread_reply_count, 0,
            "untracked thread starts read"
        );
        assert_eq!(summary.root_preview.as_deref(), Some("token names?"));
        assert_eq!(summary.participants.len(), 2);

        // get_messages_for_thread returns root + replies oldest first.
        let thread_msgs = store
            .get_messages_for_thread(&account, &arc_str(thread.to_owned()), 50)
            .await?;
        assert_eq!(thread_msgs.len(), 3);
        assert_eq!(thread_msgs[0].id.as_ref(), thread);

        // A live reply bumps unread; it then appears in the unread inbox.
        store
            .bump_thread_unread(&account, &arc_str(thread.to_owned()))
            .await?;
        let unread = store.unread_thread_summaries(&account).await?;
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].unread_reply_count, 1);

        // Marking read clears unread and removes it from the inbox.
        store
            .mark_thread_read(
                &account,
                &arc_str(thread.to_owned()),
                Some(&arc_str("r2".to_owned())),
                Some(base + chrono::Duration::seconds(20)),
            )
            .await?;
        assert_eq!(
            store
                .thread_unread_count(&account, &arc_str(thread.to_owned()))
                .await?,
            0
        );
        assert!(store.unread_thread_summaries(&account).await?.is_empty());
        Ok(())
    }
}
