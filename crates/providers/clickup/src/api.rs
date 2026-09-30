//! ClickUp API surface: wire types, the [`ClickUpApiClient`] seam, and the real
//! HTTP-backed implementation.
//!
//! The trait is modelled in domain-ish terms (channels, messages, users) rather
//! than in HTTP terms, so tests can substitute a fake without constructing any
//! HTTP fixtures. Every `ureq` call lives below the trait in
//! [`ClickUpHttpClient`].
//!
//! Chat lives in ClickUp's **Public API v3** and is documented as experimental
//! and subject to change without notice, so every response type marks all
//! non-identity fields optional and tolerates unknown fields.

use crate::http::{self, Method};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Base URL for every ClickUp API call.
pub const API_BASE: &str = "https://api.clickup.com";

/// Decoders for fields whose JSON type ClickUp is not consistent about.
///
/// Two concrete mismatches motivate this, both observed against the live API
/// and neither visible in the published schema:
///
/// * `GET /api/v2/user` returns a **numeric** user id (`"id": 26132370`) while
///   every v3 chat endpoint returns the same kind of id as a **string**
///   (`"id": "14748565"`). Decoding the v2 shape into `String` fails outright,
///   which made token validation reject perfectly valid tokens.
/// * `latest_comment_at` and `created_at` on a channel are documented as
///   strings but arrive as **numbers** (`1788284175064`).
///
/// Since the Chat API is explicitly experimental and subject to change, every
/// id- and timestamp-like field is decoded through here rather than trusting
/// one representation. Numbers are rendered to their plain digit string, so
/// downstream parsing (which already accepts epoch millis as text) is
/// unaffected.
mod lenient {
    use serde::{Deserialize, Deserializer};

    /// A JSON scalar ClickUp may render either as text or as a number.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        Text(String),
        Integer(i64),
        Float(f64),
    }

    impl Scalar {
        fn into_string(self) -> String {
            match self {
                Self::Text(text) => text,
                Self::Integer(value) => value.to_string(),
                // Ids and epoch-millisecond timestamps are integral, so they
                // must not pick up a `.0` suffix that would then fail to parse.
                Self::Float(value) if value.is_finite() && value.fract() == 0.0 => {
                    format!("{value:.0}")
                }
                Self::Float(value) => value.to_string(),
            }
        }
    }

    /// Decodes a required string field from either a string or a number,
    /// treating an explicit `null` as absent.
    pub fn string<'de, D>(deserializer: D) -> Result<String, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<Scalar>::deserialize(deserializer)?
            .map(Scalar::into_string)
            .unwrap_or_default())
    }

    /// Decodes an optional string field from either a string or a number.
    pub fn optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<Scalar>::deserialize(deserializer)?.map(Scalar::into_string))
    }
}

/// Maximum page size accepted by the v3 cursor-paginated endpoints.
pub const MAX_PAGE_LIMIT: u32 = 100;

/// Hard ceiling on pages followed in a single logical listing operation, so an
/// unexpected cursor loop cannot spin forever against the rate budget.
pub const MAX_PAGES_PER_LISTING: u32 = 20;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Envelope shared by every v3 cursor-paginated response.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Page<T> {
    #[serde(default)]
    pub data: Vec<T>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub next_cursor: Option<String>,
}

/// A ClickUp Chat Channel, DM, or group DM.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireChannel {
    #[serde(default, deserialize_with = "lenient::string")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    /// `CHANNEL`, `DM`, or `GROUP_DM`.
    #[serde(default, rename = "type")]
    pub channel_kind: Option<String>,
    /// `PUBLIC` or `PRIVATE`.
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub workspace_id: Option<String>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub creator: Option<String>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub archived: Option<bool>,
    /// Timestamp of the most recent message in the channel. Unlike Slack's
    /// `updated`, this *is* genuine message activity, so it is safe to use for
    /// sidebar ordering.
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub latest_comment_at: Option<String>,
    /// Whether the user hid this DM/group DM from their sidebar.
    #[serde(default)]
    pub is_hidden: Option<bool>,
    #[serde(default)]
    pub counts: Option<WireChannelCounts>,
}

/// Read-state and unread counters scoped to the requesting user.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireChannelCounts {
    #[serde(default)]
    pub has_unread: Option<bool>,
    #[serde(default)]
    pub num_unread: Option<u32>,
    #[serde(default)]
    pub mention_count: Option<u32>,
    #[serde(default)]
    pub latest_comment_at: Option<f64>,
}

/// A ClickUp Chat message or post.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireMessage {
    #[serde(default, deserialize_with = "lenient::string")]
    pub id: String,
    #[serde(default)]
    pub content: Option<String>,
    /// Creation time as Unix epoch milliseconds.
    #[serde(default)]
    pub date: Option<f64>,
    #[serde(default)]
    pub date_updated: Option<f64>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub user_id: Option<String>,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub parent_channel: Option<String>,
    /// Set when this message is a reply, naming the message it replies to.
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub parent_message: Option<String>,
    #[serde(default)]
    pub replies_count: Option<u32>,
    /// `message` or `post`.
    #[serde(default, rename = "type")]
    pub message_kind: Option<String>,
    #[serde(default)]
    pub resolved: Option<bool>,
    #[serde(default)]
    pub reactions: Vec<WireReaction>,
}

/// A single reaction record. ClickUp reports one row per user per emoji.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WireReaction {
    #[serde(default)]
    pub reaction: String,
    #[serde(default, deserialize_with = "lenient::optional_string")]
    pub user_id: Option<String>,
    #[serde(default)]
    pub date: Option<f64>,
}

/// A ClickUp user as returned by chat member/follower listings.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireUser {
    #[serde(default, deserialize_with = "lenient::string")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub initials: Option<String>,
    /// ClickUp spells this `profilePicture` on the v2 endpoints and
    /// `profile_picture` on some v3 payloads. Without the alias the field
    /// silently deserializes to `None` and every avatar falls back to
    /// initials, so both spellings are accepted.
    #[serde(default, alias = "profilePicture")]
    pub profile_picture: Option<String>,
}

impl WireUser {
    /// Best available human label for this user.
    pub fn best_name(&self) -> String {
        for candidate in [
            self.name.as_deref(),
            self.username.as_deref(),
            self.email.as_deref(),
        ] {
            if let Some(value) = candidate.map(str::trim).filter(|value| !value.is_empty()) {
                return value.to_owned();
            }
        }
        if self.id.is_empty() {
            "Unknown user".to_owned()
        } else {
            format!("User {}", self.id)
        }
    }
}

/// A workspace ("team" in v2 terminology) the credential can access.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireWorkspace {
    #[serde(default, deserialize_with = "lenient::string")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    /// Every member of the workspace, each wrapped in a `user` object.
    ///
    /// This is the only ClickUp response that carries profile pictures for
    /// more than one person: the v3 chat member listing returns names and
    /// initials but no picture at all. Seeding a directory from here is what
    /// lets message senders and DM rows show real avatars.
    #[serde(default)]
    pub members: Vec<WireWorkspaceMember>,
}

/// One entry of a workspace's member list.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireWorkspaceMember {
    #[serde(default)]
    pub user: WireUser,
}

/// Response shape of the v2 authorized-workspaces endpoint.
#[derive(Clone, Debug, Default, Deserialize)]
struct WireWorkspacesResponse {
    #[serde(default)]
    teams: Vec<WireWorkspace>,
}

/// Response shape of the v2 authorized-user endpoint.
#[derive(Clone, Debug, Default, Deserialize)]
struct WireUserResponse {
    #[serde(default)]
    user: WireUser,
}

/// Identity of the credential holder plus the workspaces it can reach. This is
/// what token validation produces.
#[derive(Clone, Debug, Default)]
pub struct ClickUpIdentity {
    pub user: WireUser,
    pub workspaces: Vec<WireWorkspace>,
}

/// Filters accepted by the channel-listing endpoint.
///
/// `with_message_since` is the single most valuable parameter for this
/// provider: it makes the server return only channels with a message after the
/// given instant, which is what keeps polling within ClickUp's rate budget.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelQuery {
    /// Only channels with at least one message after this Unix-millisecond
    /// timestamp.
    pub with_message_since: Option<i64>,
    /// Include DMs and group DMs the user explicitly closed.
    pub include_closed: bool,
}

// ---------------------------------------------------------------------------
// Client seam
// ---------------------------------------------------------------------------

/// Every network operation the ClickUp provider performs.
///
/// Tests substitute a fake implementation, which is why the methods speak in
/// wire types rather than in HTTP requests and responses.
#[async_trait]
pub trait ClickUpApiClient: Send + Sync {
    /// Validates a credential and reports the identity and reachable
    /// workspaces. Used both by setup and by `connect`.
    async fn identity(&self, authorization: &str) -> Result<ClickUpIdentity>;

    /// Lists channels in a workspace, following cursors up to
    /// [`MAX_PAGES_PER_LISTING`].
    async fn list_channels(
        &self,
        authorization: &str,
        workspace_id: &str,
        query: ChannelQuery,
    ) -> Result<Vec<WireChannel>>;

    /// Fetches a single channel.
    async fn channel(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<WireChannel>;

    /// Lists members of a channel.
    async fn channel_members(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<Vec<WireUser>>;

    /// Fetches one page of a channel's top-level messages, most recent first.
    async fn messages(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<WireMessage>>;

    /// Fetches one page of replies to a message, most recent first.
    async fn replies(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<WireMessage>>;

    /// Posts a new top-level message to a channel.
    async fn send_message(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
        content: &str,
    ) -> Result<WireMessage>;

    /// Posts a reply to an existing message.
    async fn send_reply(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<WireMessage>;

    /// Replaces the text content of an existing message the caller authored.
    async fn update_message(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<()>;

    /// Lists reactions on a message.
    async fn message_reactions(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
    ) -> Result<Vec<WireReaction>>;

    /// Adds a reaction, named with a lower-case emoji shortcode.
    async fn add_reaction(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        reaction: &str,
    ) -> Result<()>;

    /// Removes a reaction previously added by the authenticated user.
    async fn remove_reaction(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        reaction: &str,
    ) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Real HTTP client
// ---------------------------------------------------------------------------

/// Zero-sized real implementation. All blocking work is moved onto
/// `spawn_blocking` worker threads so the async runtime is never occupied by
/// network I/O.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClickUpHttpClient;

/// Percent-encodes a path segment so ids containing unexpected characters
/// cannot alter the request path.
fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Runs a blocking closure on a worker thread and flattens the join error.
async fn blocking<T, F>(label: &'static str, task: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .with_context(|| format!("joining ClickUp {label} task"))?
}

/// Follows cursors for a paginated listing, bounded by
/// [`MAX_PAGES_PER_LISTING`].
fn collect_pages<T, F>(mut fetch: F) -> Result<Vec<T>>
where
    F: FnMut(Option<&str>) -> Result<Page<T>>,
{
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES_PER_LISTING {
        let page = fetch(cursor.as_deref())?;
        items.extend(page.data);
        match page.next_cursor.filter(|cursor| !cursor.trim().is_empty()) {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(items)
}

#[async_trait]
impl ClickUpApiClient for ClickUpHttpClient {
    async fn identity(&self, authorization: &str) -> Result<ClickUpIdentity> {
        let authorization = authorization.to_owned();
        blocking("identity", move || {
            let user: WireUserResponse = http::request_json(
                Method::Get,
                &format!("{API_BASE}/api/v2/user"),
                &authorization,
                None,
            )
            .context("validating ClickUp credential")?;

            let workspaces: WireWorkspacesResponse = http::request_json(
                Method::Get,
                &format!("{API_BASE}/api/v2/team"),
                &authorization,
                None,
            )
            .context("listing authorized ClickUp workspaces")?;

            Ok(ClickUpIdentity {
                user: user.user,
                workspaces: workspaces.teams,
            })
        })
        .await
    }

    async fn list_channels(
        &self,
        authorization: &str,
        workspace_id: &str,
        query: ChannelQuery,
    ) -> Result<Vec<WireChannel>> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        blocking("list_channels", move || {
            collect_pages(|cursor| {
                let mut url = format!(
                    "{API_BASE}/api/v3/workspaces/{workspace}/chat/channels?limit={MAX_PAGE_LIMIT}&description_format=text/md"
                );
                if let Some(since) = query.with_message_since {
                    url.push_str(&format!("&with_message_since={since}"));
                }
                if query.include_closed {
                    url.push_str("&include_closed=true");
                }
                if let Some(cursor) = cursor {
                    url.push_str(&format!("&cursor={}", encode_segment(cursor)));
                }
                http::request_json(Method::Get, &url, &authorization, None)
            })
        })
        .await
    }

    async fn channel(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<WireChannel> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let channel = encode_segment(channel_id);
        blocking("channel", move || {
            #[derive(Deserialize)]
            struct Envelope {
                #[serde(default)]
                data: Option<WireChannel>,
            }
            let url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/channels/{channel}?description_format=text/md"
            );
            let envelope: Envelope = http::request_json(Method::Get, &url, &authorization, None)?;
            Ok(envelope.data.unwrap_or_default())
        })
        .await
    }

    async fn channel_members(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
    ) -> Result<Vec<WireUser>> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let channel = encode_segment(channel_id);
        blocking("channel_members", move || {
            collect_pages(|cursor| {
                let mut url = format!(
                    "{API_BASE}/api/v3/workspaces/{workspace}/chat/channels/{channel}/members?limit={MAX_PAGE_LIMIT}"
                );
                if let Some(cursor) = cursor {
                    url.push_str(&format!("&cursor={}", encode_segment(cursor)));
                }
                http::request_json(Method::Get, &url, &authorization, None)
            })
        })
        .await
    }

    async fn messages(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<WireMessage>> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let channel = encode_segment(channel_id);
        let cursor = cursor.map(str::to_owned);
        let limit = limit.clamp(1, MAX_PAGE_LIMIT);
        blocking("messages", move || {
            let mut url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/channels/{channel}/messages?limit={limit}&content_format=text/md"
            );
            if let Some(cursor) = cursor.as_deref() {
                url.push_str(&format!("&cursor={}", encode_segment(cursor)));
            }
            http::request_json(Method::Get, &url, &authorization, None)
        })
        .await
    }

    async fn replies(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<WireMessage>> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        let cursor = cursor.map(str::to_owned);
        let limit = limit.clamp(1, MAX_PAGE_LIMIT);
        blocking("replies", move || {
            let mut url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}/replies?limit={limit}&content_format=text/md"
            );
            if let Some(cursor) = cursor.as_deref() {
                url.push_str(&format!("&cursor={}", encode_segment(cursor)));
            }
            http::request_json(Method::Get, &url, &authorization, None)
        })
        .await
    }

    async fn send_message(
        &self,
        authorization: &str,
        workspace_id: &str,
        channel_id: &str,
        content: &str,
    ) -> Result<WireMessage> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let channel = encode_segment(channel_id);
        let body = serde_json::json!({
            "type": "message",
            "content": content,
            "content_format": "text/md",
        });
        blocking("send_message", move || {
            let url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/channels/{channel}/messages"
            );
            http::request_json(Method::Post, &url, &authorization, Some(&body))
        })
        .await
    }

    async fn send_reply(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<WireMessage> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        let body = serde_json::json!({
            "type": "message",
            "content": content,
            "content_format": "text/md",
        });
        blocking("send_reply", move || {
            let url =
                format!("{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}/replies");
            http::request_json(Method::Post, &url, &authorization, Some(&body))
        })
        .await
    }

    async fn update_message(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<()> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        let body = serde_json::json!({
            "content": content,
            "content_format": "text/md",
        });
        // The response body shape is not relied upon: the provider applies the
        // confirmed text locally and polling reconciles any server-side drift.
        blocking("update_message", move || {
            let url = format!("{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}");
            http::request_empty(Method::Patch, &url, &authorization, Some(&body))
        })
        .await
    }

    async fn message_reactions(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
    ) -> Result<Vec<WireReaction>> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        blocking("message_reactions", move || {
            collect_pages(|cursor| {
                let mut url = format!(
                    "{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}/reactions?limit={MAX_PAGE_LIMIT}"
                );
                if let Some(cursor) = cursor {
                    url.push_str(&format!("&cursor={}", encode_segment(cursor)));
                }
                http::request_json(Method::Get, &url, &authorization, None)
            })
        })
        .await
    }

    async fn add_reaction(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        reaction: &str,
    ) -> Result<()> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        let body = serde_json::json!({ "reaction": reaction });
        blocking("add_reaction", move || {
            let url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}/reactions"
            );
            http::request_empty(Method::Post, &url, &authorization, Some(&body))
        })
        .await
    }

    async fn remove_reaction(
        &self,
        authorization: &str,
        workspace_id: &str,
        message_id: &str,
        reaction: &str,
    ) -> Result<()> {
        let authorization = authorization.to_owned();
        let workspace = encode_segment(workspace_id);
        let message = encode_segment(message_id);
        let reaction = encode_segment(reaction);
        blocking("remove_reaction", move || {
            let url = format!(
                "{API_BASE}/api/v3/workspaces/{workspace}/chat/messages/{message}/reactions/{reaction}"
            );
            http::request_empty(Method::Delete, &url, &authorization, None)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke test against the real ClickUp API, skipped unless
    /// `CHAT_CLI_CLICKUP_TOKEN` is exported.
    ///
    /// This exists because ClickUp's published schema has already disagreed
    /// with the live API twice (numeric user ids, numeric channel timestamps),
    /// and the Chat endpoints are documented as subject to change. Run it with
    /// `cargo test -p clickup -- --ignored` after any wire-type edit.
    #[tokio::test]
    #[ignore = "requires a live ClickUp token in CHAT_CLI_CLICKUP_TOKEN"]
    async fn live_api_decodes_into_the_wire_types() -> Result<()> {
        let Ok(token) = std::env::var("CHAT_CLI_CLICKUP_TOKEN") else {
            return Ok(());
        };
        let client = ClickUpHttpClient;

        let identity = client.identity(&token).await?;
        assert!(
            !identity.user.id.is_empty(),
            "authorized user must decode an id"
        );
        assert!(
            !identity.workspaces.is_empty(),
            "token must reach a workspace"
        );

        let workspace = &identity.workspaces[0].id;
        let channels = client
            .list_channels(&token, workspace, ChannelQuery::default())
            .await?;
        assert!(
            channels.iter().all(|channel| !channel.id.is_empty()),
            "every channel must decode an id"
        );

        // Reading one channel exercises message, author, and reaction decoding.
        if let Some(channel) = channels.iter().find(|channel| {
            channel.channel_kind.as_deref() == Some("CHANNEL") && !channel.id.is_empty()
        }) {
            let page = client
                .messages(&token, workspace, &channel.id, None, 10)
                .await?;
            assert!(page.data.iter().all(|message| !message.id.is_empty()));
            client
                .channel_members(&token, workspace, &channel.id)
                .await?;
        }
        Ok(())
    }

    #[test]
    fn encodes_unsafe_path_segments() {
        assert_eq!(encode_segment("90010000000"), "90010000000");
        assert_eq!(encode_segment("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("a b"), "a%20b");
        assert_eq!(encode_segment("../etc"), "..%2Fetc");
    }

    #[test]
    fn best_name_prefers_name_then_username_then_email() {
        let user = WireUser {
            name: Some("Ada Lovelace".to_owned()),
            username: Some("ada".to_owned()),
            email: Some("ada@example.com".to_owned()),
            ..WireUser::default()
        };
        assert_eq!(user.best_name(), "Ada Lovelace");

        let user = WireUser {
            username: Some("ada".to_owned()),
            email: Some("ada@example.com".to_owned()),
            ..WireUser::default()
        };
        assert_eq!(user.best_name(), "ada");

        let user = WireUser {
            email: Some("ada@example.com".to_owned()),
            ..WireUser::default()
        };
        assert_eq!(user.best_name(), "ada@example.com");
    }

    #[test]
    fn best_name_ignores_blank_values_and_falls_back_to_id() {
        let user = WireUser {
            id: "42".to_owned(),
            name: Some("   ".to_owned()),
            ..WireUser::default()
        };
        assert_eq!(user.best_name(), "User 42");

        assert_eq!(WireUser::default().best_name(), "Unknown user");
    }

    #[test]
    fn channel_decodes_with_only_required_fields() {
        // The Chat API is experimental; a response carrying only an id must
        // still decode rather than blanking the channel.
        let channel: WireChannel = serde_json::from_str(r#"{"id":"abc"}"#).expect("decodes");
        assert_eq!(channel.id, "abc");
        assert!(channel.name.is_none());
        assert!(channel.counts.is_none());
    }

    #[test]
    fn channel_ignores_unknown_fields() {
        let channel: WireChannel =
            serde_json::from_str(r#"{"id":"abc","brand_new_field":{"nested":1}}"#)
                .expect("tolerates unknown fields");
        assert_eq!(channel.id, "abc");
    }

    #[test]
    fn authorized_user_decodes_a_numeric_id() {
        // Verbatim shape of `GET /api/v2/user`. ClickUp returns this id as a
        // number even though every v3 chat endpoint returns it as a string;
        // rejecting it here rejected valid tokens at setup time.
        let response: WireUserResponse = serde_json::from_str(
            r#"{"user":{"id":26132370,"username":"Bogdan","email":"b@example.org",
                "profilePicture":null,"initials":"BR","week_start_day":1}}"#,
        )
        .expect("decodes the live v2 user shape");
        assert_eq!(response.user.id, "26132370");
        assert_eq!(response.user.best_name(), "Bogdan");
    }

    #[test]
    fn user_decodes_the_camel_case_profile_picture() {
        // ClickUp spells the field `profilePicture` on v2. Before the alias
        // existed this silently produced `None` and every avatar in the app
        // fell back to initials.
        let camel: WireUser = serde_json::from_str(
            r#"{"id":"1001","username":"Vlad",
                "profilePicture":"https://attachments.clickup.com/p/vlad.jpg"}"#,
        )
        .expect("decodes the live v2 user shape");
        assert_eq!(
            camel.profile_picture.as_deref(),
            Some("https://attachments.clickup.com/p/vlad.jpg")
        );

        // The snake_case spelling seen on some v3 payloads still decodes.
        let snake: WireUser =
            serde_json::from_str(r#"{"id":"1001","profile_picture":"https://x/y.jpg"}"#)
                .expect("decodes the snake_case spelling");
        assert_eq!(snake.profile_picture.as_deref(), Some("https://x/y.jpg"));
    }

    #[test]
    fn workspace_decodes_its_avatar_and_member_pictures() {
        // Verbatim shape of `GET /api/v2/team`: the workspace logo plus the
        // only listing that carries other people's profile pictures.
        let response: WireWorkspacesResponse = serde_json::from_str(
            r#"{"teams":[{"id":"10536607","name":"TitleCapture",
                "avatar":"https://attachments.clickup.com/p/team.png",
                "members":[
                  {"user":{"id":14764623,"username":"Vlad Bolota",
                           "profilePicture":"https://attachments.clickup.com/p/vlad.jpg"}},
                  {"user":{"id":26132370,"username":"Bogdan","profilePicture":null}}]}]}"#,
        )
        .expect("decodes the live v2 team shape");

        let workspace = &response.teams[0];
        assert_eq!(
            workspace.avatar.as_deref(),
            Some("https://attachments.clickup.com/p/team.png")
        );
        assert_eq!(workspace.members.len(), 2);
        assert_eq!(workspace.members[0].user.id, "14764623");
        assert_eq!(
            workspace.members[0].user.profile_picture.as_deref(),
            Some("https://attachments.clickup.com/p/vlad.jpg")
        );
        assert!(workspace.members[1].user.profile_picture.is_none());
    }

    #[test]
    fn workspace_ids_decode_from_either_json_type() {
        // v2 reports team ids as strings today, but the same id arrives as a
        // number elsewhere in the same API, so both must decode identically.
        let as_text: WireWorkspacesResponse =
            serde_json::from_str(r#"{"teams":[{"id":"10536607","name":"TitleCapture"}]}"#)
                .expect("decodes string ids");
        let as_number: WireWorkspacesResponse =
            serde_json::from_str(r#"{"teams":[{"id":10536607,"name":"TitleCapture"}]}"#)
                .expect("decodes numeric ids");
        assert_eq!(as_text.teams[0].id, "10536607");
        assert_eq!(as_number.teams[0].id, as_text.teams[0].id);
    }

    #[test]
    fn channel_decodes_numeric_timestamps_as_epoch_millis() {
        // Verbatim shape of a live v3 channel row: `created_at` and
        // `latest_comment_at` are documented as strings but arrive as numbers,
        // and must not gain a `.0` that would break timestamp parsing.
        let channel: WireChannel = serde_json::from_str(
            r#"{"creator":"14764623","archived":false,"id":"a1hmz-11954",
                "name":"Data (new)","type":"CHANNEL","visibility":"PUBLIC",
                "parent":{"id":"90147141663","type":4},
                "created_at":1787150988352,"updated_at":1787150988352,
                "latest_comment_at":1788284175064,"room_updated_at":1787244144031,
                "is_canonical_channel":true,"workspace_id":"10536607"}"#,
        )
        .expect("decodes the live v3 channel shape");
        assert_eq!(channel.id, "a1hmz-11954");
        assert_eq!(channel.created_at.as_deref(), Some("1787150988352"));
        assert_eq!(channel.latest_comment_at.as_deref(), Some("1788284175064"));
        assert_eq!(channel.workspace_id.as_deref(), Some("10536607"));
    }

    #[test]
    fn direct_message_channel_decodes_without_a_name_or_archived_flag() {
        // Verbatim shape of a live DM row: `name` is absent entirely and
        // `archived` is explicitly null rather than false.
        let channel: WireChannel = serde_json::from_str(
            r#"{"creator":"49810918","archived":null,"chat_room_category":null,
                "id":"a1hmz-12794","type":"DM","visibility":"PRIVATE",
                "parent":{"id":"10536607","type":12},"created_at":1787214268475,
                "latest_comment_at":1788342843740,"workspace_id":"10536607"}"#,
        )
        .expect("decodes the live v3 DM shape");
        assert_eq!(channel.id, "a1hmz-12794");
        assert!(channel.name.is_none());
        assert!(channel.archived.is_none());
        assert_eq!(channel.channel_kind.as_deref(), Some("DM"));
    }

    #[test]
    fn message_decodes_numeric_ids_from_either_json_type() {
        // The message, author, and parent ids all arrive as strings today, but
        // the Chat API is experimental, so a flip to numbers must not break
        // reading a channel.
        let message: WireMessage = serde_json::from_str(
            r#"{"id":80140050873511,"content":"ready","date":1788284175064,
                "user_id":14748565,"parent_channel":"a1hmz-11954","parent_message":null,
                "resolved":false,"replies_count":0,"type":"message"}"#,
        )
        .expect("decodes numeric message ids");
        assert_eq!(message.id, "80140050873511");
        assert_eq!(message.user_id.as_deref(), Some("14748565"));
        assert!(message.parent_message.is_none());
    }

    #[test]
    fn message_decodes_reply_and_reaction_shape() {
        let message: WireMessage = serde_json::from_str(
            r#"{"id":"m1","content":"hi","date":1735689600000,"user_id":"u1",
                "parent_channel":"c1","parent_message":"m0","replies_count":2,
                "type":"message","reactions":[{"reaction":"thumbsup","user_id":"u2"}]}"#,
        )
        .expect("decodes");
        assert_eq!(message.id, "m1");
        assert_eq!(message.parent_message.as_deref(), Some("m0"));
        assert_eq!(message.replies_count, Some(2));
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(message.reactions[0].reaction, "thumbsup");
    }

    #[test]
    fn page_decodes_missing_cursor_as_end_of_listing() {
        let page: Page<WireMessage> = serde_json::from_str(r#"{"data":[]}"#).expect("decodes");
        assert!(page.next_cursor.is_none());
        assert!(page.data.is_empty());
    }

    #[test]
    fn collect_pages_follows_cursors_until_exhausted() {
        let mut calls = Vec::new();
        let items = collect_pages(|cursor| {
            calls.push(cursor.map(str::to_owned));
            Ok(match cursor {
                None => Page {
                    data: vec![1],
                    next_cursor: Some("p2".to_owned()),
                },
                Some("p2") => Page {
                    data: vec![2],
                    next_cursor: Some("p3".to_owned()),
                },
                _ => Page {
                    data: vec![3],
                    next_cursor: None,
                },
            })
        })
        .expect("collects");
        assert_eq!(items, vec![1, 2, 3]);
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn collect_pages_treats_blank_cursor_as_end() {
        let items = collect_pages(|_| {
            Ok(Page {
                data: vec![1],
                next_cursor: Some("   ".to_owned()),
            })
        })
        .expect("collects");
        assert_eq!(items, vec![1]);
    }

    #[test]
    fn collect_pages_is_bounded_against_cursor_loops() {
        let mut calls = 0_u32;
        let items = collect_pages(|_| {
            calls += 1;
            Ok(Page {
                data: vec![0],
                next_cursor: Some("same-cursor-forever".to_owned()),
            })
        })
        .expect("collects");
        assert_eq!(calls, MAX_PAGES_PER_LISTING);
        assert_eq!(items.len() as u32, MAX_PAGES_PER_LISTING);
    }
}
