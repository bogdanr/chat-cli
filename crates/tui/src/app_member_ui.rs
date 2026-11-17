//! `App` wiring for the Details member list, the member profile popup,
//! presence, and local activity statistics.
//!
//! Pure rendering lives in [`crate::member_profile`]; this module owns the
//! state transitions, background pipelines (stats, presence, extras), and
//! input handling. Every request is cache/loading-guarded so it is safe to
//! call from the draw path, and every drain is bounded.

use super::*;
use crate::member_profile::{
    ACTIVITY_DAYS, ExtrasKey, MemberSort, PROFILE_AVATAR_ROWS, PROFILE_AVATAR_WIDTH,
    PROFILE_LIST_LIMIT, PopupHit, ProfileView,
};
use chat_core::ChatMemberRole;
use storage::{LastMessageQuery, LastMessageSeen};

/// Delay before re-computing stats after new messages arrive, so busy chats
/// don't recompute on every message.
pub(super) const STATS_REFRESH_DEBOUNCE: Duration = Duration::from_secs(3);
/// Polled presence (Slack) is re-checked at most this often per user.
const PRESENCE_POLL_TTL: Duration = Duration::from_secs(60);
/// Pushed presence (WhatsApp) only needs re-subscribing occasionally; live
/// updates arrive as provider events in between.
const PRESENCE_PUSH_TTL: Duration = Duration::from_secs(10 * 60);
/// Visible member rows whose presence is requested.
const PRESENCE_VISIBLE_LIMIT: usize = 20;
/// Concurrent presence requests across all accounts.
const PRESENCE_MAX_IN_FLIGHT: usize = 8;
/// A person's cross-chat last message is re-read at most this often; new
/// messages raise the cached value in between without a query.
const LAST_MESSAGE_TTL: Duration = Duration::from_secs(60);
/// People looked up per batch (popup member, DM peer, visible rows).
const LAST_MESSAGE_BATCH_LIMIT: usize = 24;
/// Chats in common fetched for the popup.
const SENDER_CHATS_LIMIT: usize = 20;
/// Popup size bounds.
const PROFILE_POPUP_MAX_WIDTH: u16 = 64;
const PROFILE_POPUP_MAX_HEIGHT: u16 = 34;

impl App {
    fn log_perf_elapsed(&mut self, label: &str, elapsed: Duration, details: impl AsRef<str>) {
        if let Some(perf_log) = &mut self.perf_log {
            perf_log.log(label, Some(elapsed), details);
        }
    }

    fn provider_for(&self, account: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|provider| provider.id().as_ref() == account.as_ref())
            .cloned()
    }

    /// How the account delivers presence; `Unsupported` when activity display
    /// is turned off so nothing is requested.
    pub(super) fn presence_support_for(&self, account: &ProviderId) -> PresenceSupport {
        if !self.settings.show_member_activity {
            return PresenceSupport::Unsupported;
        }
        self.provider_for(account)
            .map(|provider| provider.presence_support())
            .unwrap_or_default()
    }

    // ----- Stats pipeline -------------------------------------------------

    /// Marks a chat's cached stats stale after a new message. Only chats whose
    /// stats were already computed are tracked; the refresh is debounced.
    pub(super) fn mark_chat_stats_dirty(&mut self, account: &ProviderId, chat_id: &ChatId) {
        let key = (account.clone(), chat_id.clone());
        if self.state.members_ui.stats.contains_key(&key)
            || self.state.members_ui.stats_errors.contains_key(&key)
        {
            self.state
                .members_ui
                .stats_dirty_at
                .entry(key)
                .or_insert_with(Instant::now);
        }
    }

    /// Computes activity stats for the selected chat in the background.
    /// Cache/loading-guarded and debounced; safe to call from the draw path.
    pub(super) fn request_selected_chat_stats(&mut self) {
        if !self.settings.show_member_activity {
            return;
        }
        let Some(chat) = self.state.selected_chat() else {
            return;
        };
        let key = (chat.account.clone(), chat.id.clone());
        self.request_chat_stats(key);
    }

    fn request_chat_stats(&mut self, key: (ProviderId, ChatId)) {
        let ui = &mut self.state.members_ui;
        if ui.loading_stats.contains(&key) {
            return;
        }
        let cached = ui.stats.contains_key(&key) || ui.stats_errors.contains_key(&key);
        if cached {
            match ui.stats_dirty_at.get(&key) {
                Some(dirty) if dirty.elapsed() >= STATS_REFRESH_DEBOUNCE => {}
                _ => return,
            }
        }
        ui.stats_dirty_at.remove(&key);
        ui.loading_stats.insert(key.clone());
        let options = ChatStatsOptions {
            since: None,
            utc_offset_seconds: chrono::Local::now().offset().local_minus_utc(),
            days: ACTIVITY_DAYS,
            now: Utc::now(),
        };
        self.log_perf_marker(
            "member_stats.request",
            format!("account={} chat={} refresh={cached}", key.0, key.1),
        );
        let store = Arc::clone(&self.store);
        let tx = self.chat_stats_tx.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = store
                .chat_stats(&key.0, &key.1, options)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(ChatStatsFetchResult {
                account: key.0,
                chat_id: key.1,
                result,
                elapsed: started.elapsed(),
            });
        });
    }

    /// Re-requests stats for the selected chat once its debounce has passed,
    /// even when nothing else triggers a redraw. Called from the tick.
    pub(super) fn refresh_due_chat_stats(&mut self) {
        let Some(chat) = self.state.selected_chat() else {
            return;
        };
        let key = (chat.account.clone(), chat.id.clone());
        if self
            .state
            .members_ui
            .stats_dirty_at
            .get(&key)
            .is_some_and(|dirty| dirty.elapsed() >= STATS_REFRESH_DEBOUNCE)
        {
            self.request_selected_chat_stats();
        }
    }

    pub(super) fn drain_chat_stats(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut drained = 0;
        let mut errors = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.chat_stats_rx.try_recv()
        {
            drained += 1;
            let key = (result.account.clone(), result.chat_id.clone());
            self.state.members_ui.loading_stats.remove(&key);
            let details = match &result.result {
                Ok(stats) => format!(
                    "account={} chat={} messages={} senders={}",
                    key.0,
                    key.1,
                    stats.total_messages,
                    stats.senders.len()
                ),
                Err(error) => format!("account={} chat={} error={error}", key.0, key.1),
            };
            self.log_perf_elapsed("member_stats.load", result.elapsed, details);
            match result.result {
                Ok(stats) => {
                    self.state.members_ui.stats_errors.remove(&key);
                    self.state.members_ui.stats.insert(key, stats);
                }
                Err(error) => {
                    errors += 1;
                    self.state.members_ui.stats_errors.insert(key, error);
                }
            }
        }
        if drained > 0 {
            self.log_slow_perf_duration(
                "member_stats.drain",
                drain_started,
                format!("count={drained} errors={errors}"),
            );
        }
        drained > 0
    }

    // ----- Presence pipeline ----------------------------------------------

    /// Requests presence for the open popup member, the direct-message peer,
    /// and members visible in the Details pane. Bounded and TTL-guarded; safe
    /// to call from the draw path.
    pub(super) fn request_visible_member_presence(&mut self) {
        if !self.settings.show_member_activity {
            return;
        }
        let mut wanted: Vec<(ProviderId, PlatformId)> = Vec::new();
        if let Some(popup) = self.state.members_ui.popup.as_ref()
            && !popup.is_self
        {
            wanted.push((popup.account.clone(), popup.platform_id.clone()));
        }
        if let Some(chat) = self.state.selected_chat().cloned() {
            if let Some(peer) = self.dm_peer_platform_id(&chat) {
                wanted.push((chat.account.clone(), peer));
            }
            let top = self.state.details_scroll;
            let height = inner_area(self.state.pane_areas.details).height as usize;
            let bottom = top + height.max(1);
            let self_ids: HashSet<PlatformId> = self
                .state
                .chat_members
                .get(&(chat.account.clone(), chat.id.clone()))
                .map(|members| {
                    members
                        .iter()
                        .filter(|member| member.is_self)
                        .map(|member| member.sender.platform_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            wanted.extend(
                self.state
                    .members_ui
                    .member_rows()
                    .filter(|hit| hit.line_index + hit.rows > top && hit.line_index < bottom)
                    .filter(|hit| !self_ids.contains(&hit.platform_id))
                    .take(PRESENCE_VISIBLE_LIMIT)
                    .map(|hit| (chat.account.clone(), hit.platform_id.clone())),
            );
        }
        for (account, platform_id) in &wanted {
            self.request_presence(account, platform_id);
        }
        self.request_member_last_messages(&wanted);
    }

    fn request_presence(&mut self, account: &ProviderId, platform_id: &PlatformId) {
        let support = self.presence_support_for(account);
        let ttl = match support {
            PresenceSupport::Polled => PRESENCE_POLL_TTL,
            PresenceSupport::Pushed => PRESENCE_PUSH_TTL,
            PresenceSupport::Unsupported | PresenceSupport::Disabled => return,
        };
        let key = (account.clone(), platform_id.clone());
        let ui = &mut self.state.members_ui;
        if ui.loading_presence.contains(&key)
            || ui.loading_presence.len() >= PRESENCE_MAX_IN_FLIGHT
            || ui
                .presence_checked_at
                .get(&key)
                .is_some_and(|checked| checked.elapsed() < ttl)
        {
            return;
        }
        let Some(provider) = self.provider_for(account) else {
            return;
        };
        let ui = &mut self.state.members_ui;
        ui.loading_presence.insert(key.clone());
        ui.presence_checked_at.insert(key.clone(), Instant::now());
        let tx = self.presence_tx.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = provider
                .presence(&key.1)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(PresenceFetchResult {
                account: key.0,
                platform_id: key.1,
                result,
                elapsed: started.elapsed(),
            });
        });
    }

    /// Stores a presence snapshot unless a newer one (e.g. a pushed event)
    /// is already cached.
    pub(super) fn apply_presence(
        &mut self,
        account: ProviderId,
        platform_id: PlatformId,
        presence: Presence,
    ) {
        let key = (account, platform_id);
        let newer_cached = self
            .state
            .members_ui
            .presence
            .get(&key)
            .is_some_and(|cached| cached.fetched_at > presence.fetched_at);
        if !newer_cached {
            self.state.members_ui.presence.insert(key, presence);
        }
    }

    pub(super) fn drain_presence_fetches(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut drained = 0;
        let mut errors = 0;
        let mut changed = false;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.presence_rx.try_recv()
        {
            drained += 1;
            let key = (result.account.clone(), result.platform_id.clone());
            self.state.members_ui.loading_presence.remove(&key);
            match result.result {
                Ok(Some(presence)) => {
                    self.log_perf_elapsed(
                        "presence.fetch",
                        result.elapsed,
                        format!("account={} state={:?}", key.0, presence.state),
                    );
                    self.apply_presence(key.0, key.1, presence);
                    changed = true;
                }
                Ok(None) => {
                    self.log_perf_elapsed(
                        "presence.fetch",
                        result.elapsed,
                        format!("account={} state=none", key.0),
                    );
                }
                Err(error) => {
                    errors += 1;
                    self.log_perf_elapsed(
                        "presence.fetch",
                        result.elapsed,
                        format!("account={} error={error}", key.0),
                    );
                }
            }
        }
        if drained > 0 {
            self.log_slow_perf_duration(
                "presence.drain",
                drain_started,
                format!("count={drained} errors={errors}"),
            );
        }
        changed
    }

    // ----- Cross-chat "last message" pipeline -------------------------------

    /// Looks up each person's newest local message across every chat of the
    /// account (one background batch per account). TTL- and in-flight
    /// guarded and bounded, so it is safe to call from the draw path.
    pub(super) fn request_member_last_messages(&mut self, wanted: &[(ProviderId, PlatformId)]) {
        let mut batches: HashMap<ProviderId, Vec<LastMessageQuery>> = HashMap::new();
        let mut queued = 0;
        for (account, platform_id) in wanted {
            if queued >= LAST_MESSAGE_BATCH_LIMIT {
                break;
            }
            let key = (account.clone(), platform_id.clone());
            let ui = &self.state.members_ui;
            if ui.loading_last_message.contains(&key)
                || ui
                    .last_message_checked_at
                    .get(&key)
                    .is_some_and(|checked| checked.elapsed() < LAST_MESSAGE_TTL)
                || batches
                    .get(account)
                    .is_some_and(|batch| batch.iter().any(|query| &query.key == platform_id))
            {
                continue;
            }
            let ids = member_profile::last_message_alias_ids(
                platform_id,
                self.state.contact_profiles.get(&key),
            );
            batches
                .entry(account.clone())
                .or_default()
                .push(LastMessageQuery {
                    key: platform_id.clone(),
                    ids,
                });
            queued += 1;
        }
        for (account, queries) in batches {
            let requested: Vec<PlatformId> =
                queries.iter().map(|query| query.key.clone()).collect();
            let ui = &mut self.state.members_ui;
            let now = Instant::now();
            for platform_id in &requested {
                let key = (account.clone(), platform_id.clone());
                ui.loading_last_message.insert(key.clone());
                ui.last_message_checked_at.insert(key, now);
            }
            let store = Arc::clone(&self.store);
            let tx = self.last_message_tx.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let result = store
                    .last_messages_from(&account, &queries)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send(LastMessageFetchResult {
                    account,
                    requested,
                    result,
                    elapsed: started.elapsed(),
                });
            });
        }
    }

    pub(super) fn drain_member_last_messages(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut drained = 0;
        let mut errors = 0;
        let mut changed = false;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.last_message_rx.try_recv()
        {
            drained += 1;
            let ui = &mut self.state.members_ui;
            for platform_id in &result.requested {
                ui.loading_last_message
                    .remove(&(result.account.clone(), platform_id.clone()));
            }
            match result.result {
                Ok(found) => {
                    self.log_perf_elapsed(
                        "member_last_message.fetch",
                        result.elapsed,
                        format!(
                            "account={} requested={} found={}",
                            result.account,
                            result.requested.len(),
                            found.len()
                        ),
                    );
                    let ui = &mut self.state.members_ui;
                    for (platform_id, seen) in found {
                        let key = (result.account.clone(), platform_id);
                        // A live message may already have raised the value.
                        let newer_cached = ui
                            .last_message
                            .get(&key)
                            .is_some_and(|cached| cached.at >= seen.at);
                        if !newer_cached {
                            ui.last_message.insert(key, seen);
                            changed = true;
                        }
                    }
                }
                Err(error) => {
                    errors += 1;
                    self.log_perf_elapsed(
                        "member_last_message.fetch",
                        result.elapsed,
                        format!("account={} error={error}", result.account),
                    );
                }
            }
        }
        if drained > 0 {
            self.log_slow_perf_duration(
                "member_last_message.drain",
                drain_started,
                format!("count={drained} errors={errors}"),
            );
        }
        changed
    }

    /// Raises a sender's cached cross-chat last message when a newer message
    /// arrives, without touching storage, so labels stay live between fetches.
    pub(super) fn bump_member_last_message(&mut self, message: &Message) {
        if message.is_from_me || matches!(message.content, Content::Deleted) {
            return;
        }
        let key = (message.account.clone(), message.sender.platform_id.clone());
        if self
            .state
            .members_ui
            .last_message
            .get(&key)
            .is_some_and(|cached| cached.at >= message.timestamp)
        {
            return;
        }
        let chat_name = self
            .state
            .chats
            .iter()
            .find(|chat| chat.account == message.account && chat.id == message.chat_id)
            .map(|chat| chat.name.clone())
            .filter(|name| !name.trim().is_empty());
        self.state.members_ui.last_message.insert(
            key,
            LastMessageSeen {
                at: message.timestamp,
                chat_id: message.chat_id.clone(),
                chat_name,
            },
        );
    }

    // ----- Extras pipeline (chats in common, media, links, notes) ----------

    fn request_member_extras(&mut self, key: ExtrasKey, display_name: Arc<str>) {
        let ui = &mut self.state.members_ui;
        if ui.extras.contains_key(&key) || !ui.loading_extras.insert(key.clone()) {
            return;
        }
        let store = Arc::clone(&self.store);
        let tx = self.member_extras_tx.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let (account, platform_id, chat_id) = &key;
            let result: Result<MemberExtras> = async {
                let groups = store
                    .sender_chats(account, platform_id, SENDER_CHATS_LIMIT)
                    .await?;
                let shared = match chat_id {
                    Some(chat_id) => {
                        store
                            .shared_items(account, chat_id, Some(platform_id), PROFILE_LIST_LIMIT)
                            .await?
                    }
                    None => Vec::new(),
                };
                let linked = store
                    .linked_identities(account, platform_id, &display_name, PROFILE_LIST_LIMIT)
                    .await?;
                let note = store.contact_note(account, platform_id).await?;
                let direct_chat = store.direct_chat_with(account, platform_id).await?;
                Ok(MemberExtras {
                    groups,
                    shared,
                    linked,
                    note,
                    direct_chat,
                })
            }
            .await;
            let _ = tx.send(MemberExtrasFetchResult {
                key,
                result: result.map_err(|error| error.to_string()),
                elapsed: started.elapsed(),
            });
        });
    }

    pub(super) fn drain_member_extras(&mut self) -> bool {
        let drain_started = Instant::now();
        let mut drained = 0;
        while drained < MAX_COMPLETION_EVENTS_PER_DRAIN
            && (drained == 0 || drain_started.elapsed() < COMPLETION_DRAIN_BUDGET)
            && let Ok(result) = self.member_extras_rx.try_recv()
        {
            drained += 1;
            self.state.members_ui.loading_extras.remove(&result.key);
            match result.result {
                Ok(extras) => {
                    self.log_perf_elapsed(
                        "member_extras.load",
                        result.elapsed,
                        format!(
                            "account={} groups={} shared={} linked={}",
                            result.key.0,
                            extras.groups.len(),
                            extras.shared.len(),
                            extras.linked.len()
                        ),
                    );
                    self.state.members_ui.extras.insert(result.key, extras);
                }
                Err(error) => {
                    self.log_perf_elapsed(
                        "member_extras.load",
                        result.elapsed,
                        format!("account={} error={error}", result.key.0),
                    );
                    // Cache an empty result so the popup stops showing a
                    // loading line; the error goes to the status bar.
                    self.state
                        .members_ui
                        .extras
                        .insert(result.key, MemberExtras::default());
                    self.state.status = format!("member details unavailable: {error}");
                }
            }
        }
        drained > 0
    }

    // ----- Details pane hit testing and actions -----------------------------

    /// Whether the Details pane currently shows the chat overview (where the
    /// member list lives) rather than thread or message details.
    pub(super) fn details_shows_overview(&self) -> bool {
        !self.thread_filter_active()
            && self.state.thread_root.is_none()
            && self
                .state
                .selected_message_id
                .as_ref()
                .is_none_or(|id| self.message_by_id(id).is_none())
    }

    /// Resolves a screen position in the Details pane to a member row,
    /// accounting for the details scroll. The bool is `true` when the click
    /// landed on that member's avatar.
    pub(super) fn details_member_at(
        &self,
        column: u16,
        row: u16,
    ) -> Option<(DetailsMemberHit, bool)> {
        if !self.details_shows_overview() {
            return None;
        }
        let inner = inner_area(self.state.pane_areas.details);
        if !rect_contains(inner, column, row) {
            return None;
        }
        let line = self.state.details_scroll + (row - inner.y) as usize;
        let hit = self
            .state
            .members_ui
            .hits
            .iter()
            .find(|hit| line >= hit.line_index && line < hit.line_index + hit.rows)?
            .clone();
        let offset = column - inner.x;
        let on_avatar = hit.has_avatar && (2..2 + chat_list::CHAT_AVATAR_WIDTH).contains(&offset);
        Some((hit, on_avatar))
    }

    /// Handles a left click in the Details pane: avatar → enlarge, member
    /// row or activity bar → profile popup.
    pub(super) fn handle_details_click(&mut self, column: u16, row: u16) -> bool {
        let Some((hit, on_avatar)) = self.details_member_at(column, row) else {
            return false;
        };
        if hit.is_member_row {
            self.state.members_ui.cursor = Some(hit.platform_id.clone());
        }
        if on_avatar {
            self.open_member_avatar(&hit.platform_id);
        } else {
            self.open_member_profile(&hit.platform_id);
        }
        true
    }

    /// The member record for `platform_id` in the selected chat, if loaded.
    fn selected_chat_member(&self, platform_id: &PlatformId) -> Option<(Chat, Option<ChatMember>)> {
        let chat = self.state.selected_chat()?.clone();
        let member = self
            .state
            .chat_members
            .get(&(chat.account.clone(), chat.id.clone()))
            .and_then(|members| {
                members
                    .iter()
                    .find(|member| &member.sender.platform_id == platform_id)
                    .cloned()
            });
        Some((chat, member))
    }

    /// Opens the profile popup for a member of the selected chat. The popup
    /// renders immediately from cached identity; profile, presence, stats and
    /// extras load in the background.
    pub(super) fn open_member_profile(&mut self, platform_id: &PlatformId) {
        let Some((chat, member)) = self.selected_chat_member(platform_id) else {
            return;
        };
        let key = (chat.account.clone(), chat.id.clone());
        let stats_name = self
            .state
            .members_ui
            .stats
            .get(&key)
            .and_then(|stats| stats.sender(platform_id))
            .map(|(_, sender)| (sender.display_name.clone(), sender.is_me));
        let (display_name, avatar, role, is_self) = match member {
            Some(member) => (
                member.sender.display_name.clone(),
                member.sender.avatar.clone(),
                member.role,
                member.is_self,
            ),
            None => {
                let (name, is_me) = stats_name.unwrap_or_else(|| (platform_id.clone(), false));
                (name, None, ChatMemberRole::Member, is_me)
            }
        };
        let chat_is_direct = chat.kind == ChatKind::Direct && !chat.is_group;
        self.state.members_ui.popup = Some(MemberProfilePopup {
            account: chat.account.clone(),
            chat_id: Some(chat.id.clone()),
            chat_is_direct,
            platform_id: platform_id.clone(),
            display_name: display_name.clone(),
            avatar,
            role,
            is_self,
            scroll: 0,
            editing: None,
            opened_at: Instant::now(),
        });
        self.state.action_menu = None;
        self.state.reaction_picker = None;
        self.log_perf_marker(
            "member_profile.open",
            format!(
                "account={} chat={} member={platform_id}",
                chat.account, chat.id
            ),
        );
        self.request_contact_profile(&chat.account, platform_id);
        if !is_self {
            self.request_presence(&chat.account, platform_id);
        }
        self.request_selected_chat_stats();
        self.request_member_extras(
            (
                chat.account.clone(),
                platform_id.clone(),
                Some(chat.id.clone()),
            ),
            display_name.clone(),
        );
        self.state.status = format!("profile: {display_name} · Esc closes");
    }

    pub(super) fn close_member_profile(&mut self) {
        if let Some(popup) = self.state.members_ui.popup.take() {
            self.log_perf_duration(
                "member_profile.close",
                popup.opened_at,
                format!("member={}", popup.platform_id),
            );
            self.state.status = "profile closed".to_owned();
        }
        self.state.members_ui.popup_hits.clear();
    }

    /// Best avatar file for a member: the full-size profile picture when the
    /// provider has cached one, else the list thumbnail.
    fn best_member_avatar(
        &self,
        account: &ProviderId,
        platform_id: &PlatformId,
    ) -> Option<PathBuf> {
        let full = self
            .state
            .contact_profiles
            .get(&(account.clone(), platform_id.clone()))
            .and_then(|profile| profile.avatar_full.clone())
            .filter(|path| path.exists());
        if full.is_some() {
            return full;
        }
        if let Some(popup) = self.state.members_ui.popup.as_ref()
            && &popup.account == account
            && &popup.platform_id == platform_id
            && let Some(path) = popup.avatar.as_ref().filter(|path| path.exists())
        {
            return Some(path.clone());
        }
        self.selected_chat_member(platform_id)
            .and_then(|(chat, member)| (&chat.account == account).then_some(member).flatten())
            .and_then(|member| member.sender.avatar)
            .filter(|path| path.exists())
    }

    /// Opens a member's avatar in the full-screen image viewer. Also kicks
    /// off a profile load so a full-size picture can replace the thumbnail
    /// next time.
    pub(super) fn open_member_avatar(&mut self, platform_id: &PlatformId) {
        let account = match self.state.members_ui.popup.as_ref() {
            Some(popup) if &popup.platform_id == platform_id => popup.account.clone(),
            _ => match self.state.selected_chat() {
                Some(chat) => chat.account.clone(),
                None => return,
            },
        };
        self.request_contact_profile(&account, platform_id);
        match self.best_member_avatar(&account, platform_id) {
            Some(path) => {
                self.state.status = format!("viewing avatar {}", path.display());
                self.state.image_viewer = Some(ImageViewer { path });
            }
            None => {
                self.state.status = "no avatar available for this member".to_owned();
            }
        }
    }

    /// Scrolls the Details pane so the member row `hit` is fully visible.
    fn reveal_details_line(&mut self, hit: &DetailsMemberHit) {
        let height = inner_area(self.state.pane_areas.details).height as usize;
        if height == 0 {
            return;
        }
        if hit.line_index < self.state.details_scroll {
            self.state.details_scroll = hit.line_index;
        } else if hit.line_index + hit.rows > self.state.details_scroll + height {
            self.state.details_scroll = (hit.line_index + hit.rows).saturating_sub(height);
        }
    }

    /// Details-pane keys for the member list. Returns `Some(changed)` when
    /// the key was consumed.
    pub(super) fn handle_details_member_key(&mut self, key: KeyEvent) -> Option<bool> {
        if !self.details_shows_overview() {
            return None;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if self.state.members_ui.search_editing {
            let ui = &mut self.state.members_ui;
            match key.code {
                KeyCode::Esc => {
                    ui.search = None;
                    ui.search_editing = false;
                    self.state.status = "member search cleared".to_owned();
                }
                KeyCode::Enter => {
                    ui.search_editing = false;
                    self.state.status = "member search applied · / edits · Esc clears".to_owned();
                }
                KeyCode::Backspace => {
                    let search = ui.search.get_or_insert_with(String::new);
                    if search.pop().is_none() {
                        ui.search = None;
                        ui.search_editing = false;
                    }
                }
                KeyCode::Char(value) if plain => {
                    ui.search.get_or_insert_with(String::new).push(value);
                    self.state.details_scroll = 0;
                }
                _ => {}
            }
            return Some(false);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                let delta = if key.code == KeyCode::BackTab
                    || key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    -1
                } else {
                    1
                };
                match self.state.members_ui.move_cursor(delta) {
                    Some(hit) => {
                        self.reveal_details_line(&hit);
                        self.state.status = "member selected · Enter profile · v avatar".to_owned();
                    }
                    None => self.state.status = "no members listed".to_owned(),
                }
                Some(false)
            }
            KeyCode::Enter => {
                let id = self.state.members_ui.cursor_hit()?.platform_id.clone();
                self.open_member_profile(&id);
                Some(false)
            }
            KeyCode::Char('v') if plain => {
                let id = self.state.members_ui.cursor_hit()?.platform_id.clone();
                self.open_member_avatar(&id);
                Some(false)
            }
            KeyCode::Char('o') if plain => {
                let ui = &mut self.state.members_ui;
                ui.sort = ui.sort.next();
                if ui.sort == MemberSort::Activity && !self.settings.show_member_activity {
                    ui.sort = ui.sort.next();
                }
                self.state.status = format!("members sorted by {}", ui.sort.label());
                Some(false)
            }
            KeyCode::Char('/') if plain => {
                let ui = &mut self.state.members_ui;
                ui.search.get_or_insert_with(String::new);
                ui.search_editing = true;
                self.state.status =
                    "find member: type a name · Enter keeps · Esc clears".to_owned();
                Some(false)
            }
            KeyCode::Esc if self.state.members_ui.search.is_some() => {
                self.state.members_ui.search = None;
                self.state.status = "member search cleared".to_owned();
                Some(false)
            }
            KeyCode::Esc if self.state.members_ui.cursor.is_some() => {
                self.state.members_ui.cursor = None;
                Some(false)
            }
            _ => None,
        }
    }

    // ----- Popup rendering ------------------------------------------------

    fn member_profile_popup_rect(&self, area: Rect) -> Rect {
        let width = area.width.saturating_sub(4).min(PROFILE_POPUP_MAX_WIDTH);
        let height = area.height.saturating_sub(2).min(PROFILE_POPUP_MAX_HEIGHT);
        centered_fixed_rect(area, width, height)
    }

    /// Avatar preview key for the popup's large avatar block.
    fn popup_avatar_key(&self) -> Option<AvatarPreviewKey> {
        let popup = self.state.members_ui.popup.as_ref()?;
        let path = self.best_member_avatar(&popup.account, &popup.platform_id)?;
        Some(AvatarPreviewKey {
            path,
            width: PROFILE_AVATAR_WIDTH,
            rows: PROFILE_AVATAR_ROWS,
            source: AvatarPreviewSource::Avatar,
        })
    }

    /// Builds the popup body from caches (no IO).
    pub(super) fn member_profile_lines(
        &self,
        width: u16,
        hd: bool,
    ) -> (Vec<Line<'static>>, Vec<PopupHit>) {
        let Some(popup) = self.state.members_ui.popup.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        let ui = &self.state.members_ui;
        let profile_key = (popup.account.clone(), popup.platform_id.clone());
        let chat_key = popup
            .chat_id
            .as_ref()
            .map(|chat_id| (popup.account.clone(), chat_id.clone()));
        let extras_key: ExtrasKey = (
            popup.account.clone(),
            popup.platform_id.clone(),
            popup.chat_id.clone(),
        );
        let avatar_key = self.popup_avatar_key();
        let avatar_rows = match avatar_key.as_ref() {
            Some(_) if hd => None,
            Some(key) => match self.avatar_preview_cache.get(key) {
                Some(Ok(avatar)) => Some(avatar.rows.clone()),
                _ => None,
            },
            None => {
                let sender = Sender {
                    platform_id: popup.platform_id.clone(),
                    display_name: popup.display_name.clone(),
                    avatar: None,
                };
                Some(member_avatar_placeholder_rows(&sender, self.theme))
            }
        };
        let chat_names: HashMap<ChatId, Arc<str>> = self
            .state
            .members_ui
            .extras
            .get(&extras_key)
            .map(|extras| {
                extras
                    .groups
                    .iter()
                    .filter_map(|group| {
                        self.state
                            .chats
                            .iter()
                            .find(|chat| chat.account == popup.account && chat.id == group.chat_id)
                            .map(|chat| (chat.id.clone(), chat.name.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let profile_error = ui
            .profile_errors
            .get(&profile_key)
            .map(String::as_str)
            .filter(|error| !error.is_empty());
        let view = ProfileView {
            popup,
            profile: self.state.contact_profiles.get(&profile_key),
            profile_loading: self.state.loading_contact_profiles.contains(&profile_key),
            profile_error,
            presence: ui.presence.get(&profile_key),
            presence_support: if popup.is_self {
                PresenceSupport::Unsupported
            } else {
                self.presence_support_for(&popup.account)
            },
            show_activity: self.settings.show_member_activity,
            stats: chat_key.as_ref().and_then(|key| ui.stats.get(key)),
            stats_loading: chat_key
                .as_ref()
                .is_some_and(|key| ui.loading_stats.contains(key)),
            last_message: ui.last_message.get(&profile_key),
            extras: ui.extras.get(&extras_key),
            extras_loading: ui.loading_extras.contains(&extras_key),
            avatar_rows,
            chat_names: &chat_names,
            now: Utc::now(),
        };
        member_profile::profile_lines(&view, self.theme, width)
    }

    pub(super) fn draw_member_profile_popup(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let Some(popup) = self.state.members_ui.popup.clone() else {
            return;
        };
        let rect = self.member_profile_popup_rect(area);
        if rect.width < 20 || rect.height < 8 {
            return;
        }
        let block = Block::default()
            .title(padded_title("Profile"))
            .borders(Borders::ALL)
            .border_style(self.theme.overlay_border());
        let content = block.inner(rect);
        let hd = self.settings.image_preview_mode == ImagePreviewMode::Hd
            && self.terminal_hd_images_supported();
        let avatar_key = self.popup_avatar_key();
        if let Some(key) = avatar_key.clone()
            && !hd
        {
            self.queue_avatar_preview_loads(vec![key]);
        }
        let (lines, hits) = self.member_profile_lines(content.width, hd);
        let max_scroll = lines.len().saturating_sub(content.height as usize);
        let scroll = popup.scroll.min(max_scroll);
        if let Some(open) = self.state.members_ui.popup.as_mut() {
            open.scroll = scroll;
        }
        let ui = &mut self.state.members_ui;
        ui.popup_area = rect;
        ui.popup_content_area = content;
        ui.popup_line_count = lines.len();
        ui.popup_hits = hits;
        let avatar_visible_rows = (PROFILE_AVATAR_ROWS as usize).saturating_sub(scroll) as u16;
        ui.popup_avatar_area = if avatar_visible_rows == 0 {
            Rect::default()
        } else {
            Rect::new(
                content.x,
                content.y,
                PROFILE_AVATAR_WIDTH.min(content.width),
                avatar_visible_rows.min(content.height),
            )
        };

        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            rect,
        );
        // HD avatar overlay only while the whole avatar block is visible, so
        // a clipped terminal image never bleeds over scrolled text.
        if hd
            && scroll == 0
            && let Some(key) = avatar_key
        {
            let avatar_area = self.state.members_ui.popup_avatar_area;
            self.render_hd_avatar_for_key(frame, avatar_area, &key);
        }
        // Drawn without recording a scrollbar hit: the popup owns all mouse
        // input while open, and a Details hit here would scroll the pane
        // underneath instead of the popup.
        let line_count = self.state.members_ui.popup_line_count;
        if line_count > content.height as usize {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(self.theme.muted))
                .track_style(Style::default().fg(Color::Black))
                .begin_symbol(None)
                .end_symbol(None);
            let mut state = ScrollbarState::new(line_count)
                .position(scroll)
                .viewport_content_length(content.height as usize);
            frame.render_stateful_widget(scrollbar, rect, &mut state);
        }
    }

    fn scroll_member_profile(&mut self, delta: isize) {
        let height = self.state.members_ui.popup_content_area.height as usize;
        let max = self
            .state
            .members_ui
            .popup_line_count
            .saturating_sub(height);
        if let Some(popup) = self.state.members_ui.popup.as_mut() {
            popup.scroll = popup.scroll.saturating_add_signed(delta).min(max);
        }
    }

    // ----- Popup input ----------------------------------------------------

    fn popup_extras(&self) -> Option<&MemberExtras> {
        let popup = self.state.members_ui.popup.as_ref()?;
        self.state.members_ui.extras.get(&(
            popup.account.clone(),
            popup.platform_id.clone(),
            popup.chat_id.clone(),
        ))
    }

    fn copy_to_clipboard(&mut self, label: &str, text: &str) {
        match Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_owned())) {
            Ok(()) => self.state.status = format!("copied {label}"),
            Err(error) => {
                self.state.status = format!("clipboard unavailable; {label}: {text} ({error})");
            }
        }
    }

    /// Opens a stored chat by id, closing the popup. Returns whether the
    /// selection changed.
    fn open_chat_from_profile(&mut self, account: &ProviderId, chat_id: &ChatId) -> bool {
        let Some(index) = self
            .state
            .chats
            .iter()
            .position(|chat| &chat.account == account && &chat.id == chat_id)
        else {
            self.state.status = "that chat is not loaded in the sidebar".to_owned();
            return false;
        };
        self.close_member_profile();
        self.activate_chat_index(index)
    }

    fn popup_copy_target(&self) -> Option<(&'static str, Arc<str>)> {
        let popup = self.state.members_ui.popup.as_ref()?;
        let profile = self
            .state
            .contact_profiles
            .get(&(popup.account.clone(), popup.platform_id.clone()));
        if let Some(profile) = profile {
            if let Some(phone) = profile.phone.clone() {
                return Some(("phone", phone));
            }
            if let Some(email) = profile.email.clone() {
                return Some(("email", email));
            }
            if let Some(handle) = profile.handle.clone() {
                return Some(("handle", handle));
            }
        }
        Some(("id", popup.platform_id.clone()))
    }

    fn start_note_edit(&mut self, field: NoteField) {
        let current = self
            .popup_extras()
            .and_then(|extras| match field {
                NoteField::Nickname => extras.note.nickname.clone(),
                NoteField::Note => extras.note.note.clone(),
            })
            .map(|value| value.to_string())
            .unwrap_or_default();
        if let Some(popup) = self.state.members_ui.popup.as_mut() {
            popup.editing = Some(NoteEdit {
                field,
                buffer: current,
            });
            self.state.status = format!("editing {} · Enter saves · Esc cancels", field.label());
        }
    }

    /// Saves the edited note locally: the cached extras update immediately
    /// and the store write happens in the background.
    fn save_note_edit(&mut self) {
        let Some(popup) = self.state.members_ui.popup.as_mut() else {
            return;
        };
        let Some(edit) = popup.editing.take() else {
            return;
        };
        let account = popup.account.clone();
        let platform_id = popup.platform_id.clone();
        let extras_key: ExtrasKey = (account.clone(), platform_id.clone(), popup.chat_id.clone());
        let value = edit.buffer.trim();
        let value: Option<Arc<str>> = (!value.is_empty()).then(|| Arc::from(value));
        let mut note = self
            .state
            .members_ui
            .extras
            .get(&extras_key)
            .map(|extras| extras.note.clone())
            .unwrap_or_default();
        match edit.field {
            NoteField::Nickname => note.nickname = value,
            NoteField::Note => note.note = value,
        }
        // Every popup key for this person shares the note.
        for (key, extras) in self.state.members_ui.extras.iter_mut() {
            if key.0 == account && key.1 == platform_id {
                extras.note = note.clone();
            }
        }
        self.state
            .members_ui
            .extras
            .entry(extras_key)
            .or_default()
            .note = note.clone();
        let store = Arc::clone(&self.store);
        let tx = self.member_extras_tx.clone();
        let label = edit.field.label();
        self.state.status = format!("{label} saved on this device");
        tokio::spawn(async move {
            let started = Instant::now();
            if let Err(error) = store.set_contact_note(&account, &platform_id, &note).await {
                // Surface failures through the extras channel so the UI
                // thread reports them; the optimistic value stays visible.
                let _ = tx.send(MemberExtrasFetchResult {
                    key: (account, platform_id, None),
                    result: Err(format!("saving {label} failed: {error}")),
                    elapsed: started.elapsed(),
                });
            }
        });
    }

    /// Mentions the popup member in the composer of the selected chat.
    fn mention_popup_member(&mut self) {
        let Some(popup) = self.state.members_ui.popup.clone() else {
            return;
        };
        if popup.is_self {
            self.state.status = "you can't mention yourself".to_owned();
            return;
        }
        let in_selected_chat = self.state.selected_chat().is_some_and(|chat| {
            chat.account == popup.account && Some(&chat.id) == popup.chat_id.as_ref()
        });
        if !in_selected_chat {
            self.state.status = "open the chat to mention this member".to_owned();
            return;
        }
        self.close_member_profile();
        self.state.compose_mention_picks.push(Mention {
            platform_id: popup.platform_id.clone(),
            display_name: popup.display_name.clone(),
        });
        self.state.focus = FocusPane::Compose;
        self.insert_compose_text(&format!("@{} ", popup.display_name));
        self.state.compose_mention_picker = None;
        self.state.status = format!("mentioned @{}", popup.display_name);
    }

    /// Filters the selected chat's messages to this member.
    async fn search_popup_member_messages(&mut self) -> Result<bool> {
        let Some(popup) = self.state.members_ui.popup.clone() else {
            return Ok(false);
        };
        self.close_member_profile();
        self.state.focus = FocusPane::Messages;
        *self.state.filters.get_mut(FilterScope::Messages) =
            format!("{MESSAGE_FILTER_FROM_PREFIX}{}", popup.platform_id);
        let changed = self.enter_filter_mode().await?;
        self.state.status = format!(
            "showing loaded messages from {} · type to narrow · Esc clears",
            popup.display_name
        );
        Ok(changed)
    }

    /// Opens (or reports the absence of) a direct chat with the member.
    fn message_popup_member(&mut self) -> bool {
        let Some(popup) = self.state.members_ui.popup.clone() else {
            return false;
        };
        if popup.chat_is_direct
            && let Some(chat_id) = popup.chat_id.clone()
        {
            return self.open_chat_from_profile(&popup.account, &chat_id);
        }
        let direct = self
            .popup_extras()
            .and_then(|extras| extras.direct_chat.clone());
        let direct = direct.or_else(|| {
            // Many providers use the user id as the direct chat id.
            self.state
                .chats
                .iter()
                .find(|chat| {
                    chat.account == popup.account
                        && chat.kind == ChatKind::Direct
                        && chat.id.as_ref() == popup.platform_id.as_ref()
                })
                .map(|chat| chat.id.clone())
        });
        match direct {
            Some(chat_id) => self.open_chat_from_profile(&popup.account, &chat_id),
            None => {
                self.state.status = if self.popup_extras().is_none() {
                    "still looking up a direct chat…".to_owned()
                } else {
                    "no direct chat with this member yet".to_owned()
                };
                false
            }
        }
    }

    /// Keys while the popup is open. Always consumes the key.
    pub(super) async fn handle_member_profile_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.state.image_viewer.is_some() {
            self.close_image_viewer();
            return Ok(false);
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let editing = self
            .state
            .members_ui
            .popup
            .as_ref()
            .is_some_and(|popup| popup.editing.is_some());
        if editing {
            match key.code {
                KeyCode::Esc => {
                    if let Some(popup) = self.state.members_ui.popup.as_mut() {
                        popup.editing = None;
                    }
                    self.state.status = "edit cancelled".to_owned();
                }
                KeyCode::Enter => self.save_note_edit(),
                KeyCode::Backspace => {
                    if let Some(edit) = self
                        .state
                        .members_ui
                        .popup
                        .as_mut()
                        .and_then(|popup| popup.editing.as_mut())
                    {
                        edit.buffer.pop();
                    }
                }
                KeyCode::Char(value) if plain => {
                    if let Some(edit) = self
                        .state
                        .members_ui
                        .popup
                        .as_mut()
                        .and_then(|popup| popup.editing.as_mut())
                        && edit.buffer.chars().count() < 2_000
                    {
                        edit.buffer.push(value);
                    }
                }
                _ => {}
            }
            return Ok(false);
        }
        let page = self
            .state
            .members_ui
            .popup_content_area
            .height
            .saturating_sub(1)
            .max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_member_profile(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_member_profile(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_member_profile(-1),
            KeyCode::PageDown => self.scroll_member_profile(page),
            KeyCode::PageUp => self.scroll_member_profile(-page),
            KeyCode::Home => self.scroll_member_profile(isize::MIN / 2),
            KeyCode::End => self.scroll_member_profile(isize::MAX / 2),
            KeyCode::Char('v') if plain => {
                if let Some(id) = self
                    .state
                    .members_ui
                    .popup
                    .as_ref()
                    .map(|popup| popup.platform_id.clone())
                {
                    self.open_member_avatar(&id);
                }
            }
            KeyCode::Char('m') if plain => return Ok(self.message_popup_member()),
            KeyCode::Char('@') => self.mention_popup_member(),
            KeyCode::Char('s') if plain => return self.search_popup_member_messages().await,
            KeyCode::Char('c') if plain => {
                if let Some((label, value)) = self.popup_copy_target() {
                    self.copy_to_clipboard(label, &value);
                }
            }
            KeyCode::Char('n') if plain => self.start_note_edit(NoteField::Nickname),
            KeyCode::Char('e') if plain => self.start_note_edit(NoteField::Note),
            KeyCode::Char(digit @ '1'..='9') if plain => {
                let index = digit as usize - '1' as usize;
                let target = self
                    .state
                    .members_ui
                    .popup_hits
                    .iter()
                    .filter_map(|hit| match &hit.target {
                        PopupTarget::OpenChat { account, chat_id } => {
                            Some((account.clone(), chat_id.clone()))
                        }
                        _ => None,
                    })
                    .nth(index);
                if let Some((account, chat_id)) = target {
                    return Ok(self.open_chat_from_profile(&account, &chat_id));
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Mouse input while the popup is open. Always consumes the event.
    pub(super) fn handle_member_profile_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.scroll_member_profile(MOUSE_SCROLL_STEP as isize);
                false
            }
            MouseEventKind::ScrollUp => {
                self.scroll_member_profile(-(MOUSE_SCROLL_STEP as isize));
                false
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let ui = &self.state.members_ui;
                if !rect_contains(ui.popup_area, mouse.column, mouse.row) {
                    self.close_member_profile();
                    return false;
                }
                if rect_contains(ui.popup_avatar_area, mouse.column, mouse.row) {
                    if let Some(id) = ui.popup.as_ref().map(|popup| popup.platform_id.clone()) {
                        self.open_member_avatar(&id);
                    }
                    return false;
                }
                let content = ui.popup_content_area;
                if !rect_contains(content, mouse.column, mouse.row) {
                    return false;
                }
                let scroll = ui.popup.as_ref().map_or(0, |popup| popup.scroll);
                let line = scroll + (mouse.row - content.y) as usize;
                let Some(target) = ui
                    .popup_hits
                    .iter()
                    .find(|hit| hit.line == line && hit.target != PopupTarget::Avatar)
                    .map(|hit| hit.target.clone())
                else {
                    return false;
                };
                match target {
                    PopupTarget::Avatar => false,
                    PopupTarget::Copy(value) => {
                        self.copy_to_clipboard("value", &value);
                        false
                    }
                    PopupTarget::OpenChat { account, chat_id } => {
                        self.open_chat_from_profile(&account, &chat_id)
                    }
                    PopupTarget::OpenFile(path) => {
                        self.open_with_system_app(&path);
                        false
                    }
                    PopupTarget::OpenUrl(url) => {
                        open_url(&url);
                        self.state.status = format!("opening {url}");
                        false
                    }
                }
            }
            _ => false,
        }
    }

    /// Whether a text field owned by the member UI is receiving keystrokes.
    pub(super) fn member_ui_text_input_active(&self) -> bool {
        let ui = &self.state.members_ui;
        ui.popup
            .as_ref()
            .is_some_and(|popup| popup.editing.is_some())
            || (self.state.focus == FocusPane::Details && ui.search_editing)
    }

    /// Queues avatar thumbnails for the members actually rendered (honouring
    /// sort and search), falling back to provider order before the first draw.
    pub(super) fn rendered_member_avatar_keys(
        &self,
        members: &[ChatMember],
    ) -> Vec<AvatarPreviewKey> {
        let rendered: Vec<&PlatformId> = self
            .state
            .members_ui
            .member_rows()
            .map(|hit| &hit.platform_id)
            .collect();
        if rendered.is_empty() {
            return members
                .iter()
                .take(40)
                .filter_map(|member| self.member_avatar_preview_key(member))
                .collect();
        }
        rendered
            .into_iter()
            .filter_map(|id| {
                members
                    .iter()
                    .find(|member| &member.sender.platform_id == id)
            })
            .filter_map(|member| self.member_avatar_preview_key(member))
            .collect()
    }
}
