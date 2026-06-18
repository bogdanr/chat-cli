# Enrich the Details Pane for Slack & WhatsApp (Users and Groups)

## Objective

Expand the right-hand Details pane so that, for the selected conversation, it shows
substantially richer, provider-sourced context for both **groups/channels** and
**users/contacts** on WhatsApp and Slack — while obeying the repository's async/draw
performance rules (no blocking work in `draw`/`handle_event`, async load with
stale-completion guards, cache-only draw paths).

Today the pane renders only:
- Overview "Chat" block: `Type`, `Membership`, `Last activity` (`crates/tui/src/app.rs` `overview_detail_lines`).
- "Members" roster: count + per-member avatar/name/role (`append_member_detail_lines`).
- Message details: sender display name, avatar, time, ids, reactions (`draw_message_details`).

The providers already fetch — but the core model discards — data such as Slack topic/purpose/
member count and WhatsApp group admin flags, and neither provider surfaces group descriptions,
creation metadata, group settings, or rich user profiles to the UI.

## Inventory: What Information We Can Add (and its source)

### Groups / Channels

**WhatsApp groups** (source: `whatsmeow` `types.GroupInfo` via `bridge.go GetGroupInfo`; today only `Name` + participants + admin flags are used at `crates/providers/whatsapp/go/bridge.go:700,715`):
- Group description / topic (`GroupInfo.Topic`).
- Creation date and creator/owner (`GroupCreated`, `OwnerJID` → resolved name).
- Participant count and admin/owner counts (derivable from participants already fetched).
- Group settings flags: "Only admins can send" (`IsAnnounce`), "Only admins can edit info" (`IsLocked`), disappearing-messages timer (`IsEphemeral` + duration).
- Community/parent linkage where present.

**Slack channels** (source: `conversations.info`; `SlackConversation` already parses `topic`, `purpose`, `num_members`, `is_archived`, `is_private`, `is_ext_shared`, `is_muted`, `is_pinned`, `updated` at `crates/providers/slack/src/lib.rs:332-351`, but these are dropped when building the core `Chat`):
- Topic and purpose/description.
- Member count (`num_members`).
- Visibility: public vs private channel, archived, externally shared.
- Created date and creator (requires parsing `created` + `creator` from `SlackConversationResponse`, not currently parsed).
- Workspace/team name (already available via `SlackTeamInfo`).

### Users / Contacts (DM overview, member rows, message sender)

**WhatsApp contacts** (source: bridge contact store + `whatsmeow` user info; today only name resolution uses `FullName`/`FirstName`/`BusinessName`/`PushName` at `crates/providers/whatsapp/go/bridge.go:1547,1659`):
- Phone number (from JID).
- About / status text.
- Business account name + verified-business indication.
- Saved contact name vs push name distinction.

**Slack users** (source: `users.info`; `SlackUserProfileResponse` currently parses only `real_name`, `display_name`, `image_72` at `crates/providers/slack/src/lib.rs:678-682`):
- Real name, display name, handle (`@name`).
- Title / role (`profile.title`).
- Current status (`status_emoji` + `status_text`).
- Timezone and computed local time (`tz`, `tz_label`, `tz_offset`).
- Email / phone (`profile.email`, `profile.phone` — scope-gated).
- Account class: human vs bot/app, deactivated (`deleted`), workspace admin/owner.

## Design Decisions (assumptions)

- **Hybrid data shape.** Add a small number of typed fields for values that need formatting
  (e.g. `created_at: Option<Timestamp>`, `member_count: Option<u32>`, `description: Option<Arc<str>>`),
  plus an ordered list of labeled facts `Vec<(Arc<str>, Arc<str>)>` (or `BTreeMap`) for
  platform-specific extras. This mirrors the existing `DiscoveryResult.metadata` pattern
  (`crates/core/src/types.rs:159`) and minimizes churn as new facts are added.
- **Pull, not push.** Enrichment loads on chat selection (like `queue_selected_chat_member_avatars`),
  not via new realtime events, keeping the change additive and the draw path cache-only.
- **Graceful degradation.** Every field is optional; missing scopes (Slack) or unavailable
  bridge data (WhatsApp) render nothing rather than placeholders, except where a short
  "loading"/"unavailable" line already exists for parity with the Members section.
- **Persistence deferred.** v1 caches enrichment in TUI state keyed by `(account, chat_id)` /
  `(account, platform_id)`; durable storage columns are a later, optional phase.
- **Privacy.** Email/phone are shown only when already returned by the provider for the
  authenticated user's scopes; no new always-on scope is required to ship the group/topic work.

## Implementation Plan

### Phase 1 — Core data model

- [ ] Add a `ChatDetails` struct in `crates/core/src/types.rs` with typed fields
  (`description`, `created_at`, `creator: Option<PlatformId>`, `member_count`, `admin_count`,
  visibility/archived/announce/locked booleans, disappearing-timer) plus an ordered
  `facts: Vec<(Arc<str>, Arc<str>)>` for platform-specific extras. Rationale: typed fields
  format correctly; the facts list keeps the model open for per-platform additions.
- [ ] Add a `ContactProfile` struct (or extend the value returned by `contact_info`) carrying
  `display_name`, `handle`, `title`, `status`, `timezone/local_time`, `phone`, `email`,
  `about`, `is_bot`, `is_deactivated`, and a `facts` list. Rationale: `Sender` is intentionally
  minimal and shared across hot paths; profile detail belongs in a dedicated type.
- [ ] Keep all new fields optional and `Default`-friendly so providers populate only what they have.

### Phase 2 — Provider trait surface

- [ ] Add `async fn chat_details(&self, chat_id: &ChatId) -> Result<ChatDetails>` to the
  `Provider` trait (`crates/core/src/provider.rs:144`) with a default impl that derives a
  minimal `ChatDetails` from the cached `Chat` (so providers without support still work).
- [ ] Add `async fn contact_profile(&self, platform_id: &PlatformId) -> Result<Option<ContactProfile>>`
  with a default impl delegating to `contact_info` and wrapping the `Sender`. Rationale:
  preserves the existing `contact_info` callers while giving an enrichment entry point.
- [ ] Update the mock provider (`crates/core/src/mock.rs`) to return representative
  `ChatDetails`/`ContactProfile` so TUI tests can exercise both phases.

### Phase 3 — Slack provider implementation

- [ ] Extend `SlackConversationResponse` to parse `created` and `creator`
  (`crates/providers/slack/src/lib.rs:438`) and carry them onto `SlackConversation`.
- [ ] Implement `chat_details` by mapping the cached/fetched `conversations.info` data
  (topic, purpose, num_members, created, creator, is_private, is_archived, is_ext_shared)
  into `ChatDetails`, resolving `creator` to a display name via existing user lookup.
- [ ] Extend `SlackUserProfileResponse` to parse `title`, `status_text`, `status_emoji`,
  `tz`, `tz_label`, `tz_offset`, `email`, `phone`; map into `ContactProfile` in
  `contact_profile`, computing local time from `tz_offset`. Handle missing scopes by
  leaving fields `None`.
- [ ] Surface workspace/team name as a `ChatDetails` fact for channels.

### Phase 4 — WhatsApp provider implementation

- [ ] Extend the Go bridge group payload (`bridge.go` around `GetGroupInfo`/`fetchAndEmitGroupInfo`,
  `:700,1688`) to include `Topic`, `OwnerJID`, `GroupCreated`, `IsAnnounce`, `IsLocked`,
  disappearing-timer, and admin/participant counts; add matching JSON tags on the bridge structs.
- [ ] Add the Rust-side parsing for the new bridge fields and implement `chat_details`
  in `crates/providers/whatsapp/src/lib.rs`, resolving `OwnerJID` to a name via the existing
  contact-name resolution path.
- [ ] Extend the bridge contact payload to include About/status text and business-profile
  info; implement `contact_profile` to expose phone (from JID), about, and business name.
- [ ] Keep the embedded mock bridge data (`bridge.go:684`) updated so headless/dev runs render
  the new fields.

### Phase 5 — TUI state & async loading

- [ ] Add `chat_details: HashMap<(ProviderId, ChatId), ChatDetails>` and
  `contact_profiles: HashMap<(ProviderId, PlatformId), ContactProfile>` caches plus
  `loading_*` sets to TUI state, mirroring `chat_members`/`loading_chat_members`.
- [ ] Add `queue_selected_chat_details` and `queue_selected_contact_profile` helpers invoked
  from the details draw path (cache-only read; spawn background fetch on miss), analogous to
  `queue_selected_chat_member_avatars` (`crates/tui/src/app.rs`).
- [ ] Apply async results only when they still match the currently selected chat/account and
  the latest request token; preserve selection/scroll on completion (per AGENTS async rules).
- [ ] Add opt-in perf instrumentation labels for the new fetch/apply pipeline (counts,
  account/chat identifiers, stale/error totals).

### Phase 6 — Details pane rendering

- [ ] Extend `overview_detail_lines` Chat section to render available `ChatDetails`:
  description/topic, created + creator, member/admin counts, visibility/archived/shared,
  group settings flags, and a generic facts list. Keep lines short to avoid wrap drift in
  the non-wrapping overview paragraph.
- [ ] For Direct chats, render a "Contact" block in the overview from `ContactProfile`
  (about/status, phone, business, local time) since DMs have no Members roster.
- [ ] Extend `draw_message_details` sender block with `ContactProfile` facts (title, status,
  local time, handle) beneath the existing sender/avatar lines.
- [ ] Update the corresponding `*_line_count` helpers (`message_details_line_count`,
  `overview_details_line_count`) so scroll bounds account for the new fixed lines.

### Phase 7 — Tests & validation

- [ ] Add unit tests for Slack/WhatsApp `chat_details`/`contact_profile` mapping (including
  missing-field/missing-scope cases).
- [ ] Add TUI tests asserting both phases for the new async pipeline: immediate
  placeholder/absence on selection, then enriched lines after draining background completions
  (per AGENTS testing rule).
- [ ] Run the CI-mirrored checks locally: `cargo check`, `cargo test`, `cargo clippy`, and the
  release build, per `.github/workflows/ci.yml` and AGENTS implementation-safety rules.

### Phase 8 — Optional persistence (follow-up)

- [ ] If enrichment should survive restarts/offline, add nullable columns to the `chats`
  table (`description`, `created_at`, `creator`, `member_count`) or a `chat_metadata` table,
  plus a contact-facts table, with a forward-only migration. Defer unless required.

## Verification Criteria

- Selecting a WhatsApp group shows description, creation date + creator, participant/admin
  counts, and any active group settings (announce/locked/disappearing) when the bridge reports them.
- Selecting a Slack channel shows topic, purpose, member count, visibility/archived state,
  created + creator, and workspace name when the token's scopes allow.
- Selecting a Slack DM or a member/sender shows title, status, local time, and handle (and
  email/phone only when the provider returns them).
- Missing data renders nothing (or the existing loading/unavailable line) — never a crash or
  blank-field clutter.
- No blocking I/O is introduced on `draw`/`handle_event`; enrichment loads asynchronously and
  applies only for the still-selected chat/account with stale-completion guards.
- All CI-required checks pass locally.

## Potential Risks and Mitigations

1. **Slack scope gaps (title/status/email/phone, users.info).**
   Mitigation: treat every profile field as optional; never request a new always-on scope to
   ship group/topic enrichment; document which fields depend on which scopes and hide absent ones.
2. **WhatsApp bridge changes span Go + Rust + protocol.**
   Mitigation: land the bridge JSON additions behind optional fields, keep Rust parsing
   tolerant of missing keys, and update the embedded mock data so dev/headless runs stay green.
3. **Performance regressions from per-selection fetches.**
   Mitigation: follow the existing `chat_members` async pattern exactly — cache-only draw,
   bounded background fetch, request-token/stale guards, perf instrumentation on the new pipeline.
4. **Overview paragraph wrap drift inflating scroll.**
   Mitigation: keep enrichment lines concise/truncated; update the `*_line_count` helpers in
   lockstep so scroll bounds remain correct.
5. **Privacy exposure of phone/email.**
   Mitigation: show contact PII only when already returned for the account's scopes; consider a
   settings toggle to hide PII before enabling any new scope.

## Alternative Approaches

1. **Pure metadata map (no typed fields).** Simpler to thread through providers/storage, but
   loses correct formatting (dates, counts) and ordering control. Rejected in favor of the
   hybrid typed + facts-list shape.
2. **Push enrichment via new `ProviderEvent`s** (e.g. `ChatDetailsUpdated`, `ContactUpdated`)
   instead of pull-on-selection. Better for always-fresh data but a larger, riskier change to
   the event bus and replay ordering; can be layered on later if staleness becomes an issue.
3. **Reuse/extend `contact_info` return type** rather than adding `contact_profile`. Fewer
   methods, but it would touch every existing `contact_info` caller and risk the message hot
   path; keeping a separate enrichment method is safer.
4. **Persist immediately (Phase 8 up front).** Improves offline UX but adds schema migrations
   and merge rules now; deferred so the visible feature ships first against in-memory caches.
