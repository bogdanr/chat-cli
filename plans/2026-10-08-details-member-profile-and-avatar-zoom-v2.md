# Channel Details: Member Profiles, Avatar Zoom, Last Seen and Activity Stats

## Objective

Make the right-side Details pane an informative view of the chat and its people:

1. **Clickable members.** A click opens a Member Profile popup with every detail the provider exposes (phone, email, about, title, timezone, and so on). Clicking an avatar opens it full-size. (Carried over from v1.)
2. **Last seen / activity recency.** For each member, show real presence where the platform provides it (online, last seen) and always show what the app can work out locally ("last message here", "last read").
3. **Basic stats.** Message counts and share (%) per member, top posters, activity over time, media breakdown. Everything is computed from locally synced history, and the UI says how much history that covers.
4. **Extras.** Groups in common, quick actions, member sorting/filtering, linked identities across platforms.

## Current State (from codebase analysis)

**Details pane, profiles and avatars** (unchanged since v1)
- Member list: `crates/tui/src/app.rs:6458-6559`.
- Details-pane click handler (a no-op today): `crates/tui/src/app.rs:6813-6816`.
- `ContactProfile`: `crates/core/src/types.rs:456-502`.
- `request_contact_profile`: `crates/tui/src/app.rs:14929-14957`.
- Image viewer: `crates/tui/src/app.rs:9036-9078`.
- WhatsApp avatar cache-key collision (preview and full-size share one key): `crates/providers/whatsapp/go/bridge.go:2436-2438`, used at `:1059` and `:2414`.

**Presence**
- Nothing exists yet. Searching the repo for presence, last_seen and getPresence finds only:
  - the WhatsApp bridge announcing *our own* availability (`crates/providers/whatsapp/go/bridge.go:1635-1648`, which can be turned off with `CHATCLI_WHATSAPP_NO_PRESENCE`);
  - mock seed data (`crates/core/src/mock.rs:422`).
- `ProviderEvent` (`crates/core/src/events.rs:26`) already has a `Typing` variant (`crates/core/src/events.rs:111`). The TUI keeps short-lived per-chat indicators for it (`crates/tui/src/app.rs:2018`, `2679`, `16680`). A new `Presence` event can follow the same pattern.
- Slack connects in Socket Mode (`crates/providers/slack/src/lib.rs:2710-2728`, `5842+`). Socket Mode (Events API) does not deliver `presence_change` events, so Slack presence has to be fetched with `users.getPresence`.

**Data available for stats**
- The `messages` table (`crates/storage/src/schema.rs:30-54`) already has `chat_id`, `account_id`, `sender_id`, `sender_name`, `timestamp`, `content_type`, `media_size`, `reply_to_id`, `thread_id`, `is_from_me` and `mentions_me`.
- The index `idx_messages_chat (chat_id, account_id, timestamp DESC)` (`crates/storage/src/schema.rs:55`) supports per-chat range scans.
- Related tables: `reactions` (`crates/storage/src/schema.rs:59-65`), `receipts` with `kind` and `at` (`crates/storage/src/schema.rs:67-74`, which gives "last read" for WhatsApp), and `persons`/`handles` for cross-platform identity (`crates/storage/src/schema.rs:76-89`).

## Assumptions

- **Two kinds of recency, labelled differently.** Real presence comes from the platform and is labelled "Online" / "Last seen 5m ago". Locally derived recency is labelled "Last message here 2h ago" / "Last read 10m ago". The two are never mixed, because they mean different things.
- **Stats are honest about coverage.** They are computed only from locally stored messages, and every stats block says so, e.g. "Based on 4,312 synced messages since 12 Mar". There is no server-side backfill just for stats.
- **Presence is fetched lazily.** Only for the DM peer, members visible in the Details pane, and the member whose popup is open. Never for whole large channels at once, to avoid rate limits and WhatsApp anti-abuse heuristics.
- **No new persistent tables in the first iteration.** Presence is cached in memory with a TTL. Stats are computed with on-demand SQL and kept in an in-memory cache.

## Implementation Plan

### Phase 1: Clickable members and the profile popup (from v1, condensed)

- [x] **Task 1. Record member hit regions during draw.** For every rendered member, store `DetailsMemberHit { line_index, provider/account, platform_id }`, plus the inner rect and the `details_scroll` value used for that draw. Rationale: hit-testing uses exactly the layout that is on screen.
- [x] **Task 2. Add a `details_member_at` resolver and wire up `FocusPane::Details` clicks.**
  - Avatar column → open the image viewer.
  - Rest of the row → open the profile popup.
  - Otherwise → keep the existing behaviour.
  - Disabled in thread and message-details modes.
- [x] **Task 3. Profile popup** (a new module such as `crates/tui/src/widgets/profile_overlay.rs`, with thin wiring in `app.rs`).
  - Appears immediately with the cached name, role and avatar, then loads the full profile through `request_contact_profile`.
  - Uses the help-overlay pattern: Esc or click outside closes it, it scrolls, and clicking its avatar opens the image viewer stacked on top.
  - Closes when the chat or account changes.
- [x] **Task 4. Keyboard parity.** A member cursor in Details; Enter opens the profile, `v` opens the avatar. Update the "Discoverable controls" text.
- [x] **Task 5. Richer provider profiles.**
  - **WhatsApp:** add a `C_UserInfo` bridge export (about text, verified business name, picture ID, LID/phone mapping), and optionally the business profile.
  - **WhatsApp:** fix the preview/full avatar cache key and add `ContactProfile.avatar_full`.
  - **Slack:** use `image_512`/`image_original` and custom profile fields.
  - **ClickUp:** override `contact_profile`.

### Phase 2: Presence ("last seen")

- [x] **Task 6. Core presence model.**
  - Add `PresenceState { Online, Away, Offline, Unknown }` and `Presence { state, last_seen: Option<DateTime>, source, fetched_at }` to `crates/core/src/types.rs`.
  - Add a `ProviderEvent::Presence { account, platform_id, presence }` variant next to `Typing` (`crates/core/src/events.rs:111`).
  - Add a trait method `Provider::presence(platform_id) -> Option<Presence>`, defaulting to `None`, and a capability flag `supports_presence`.
  - Rationale: providers differ — WhatsApp pushes presence, Slack has to be polled, ClickUp has none — so the model needs both a pull method and a push event.

- [x] **Task 7. WhatsApp presence.**
  - In `crates/providers/whatsapp/go/bridge.go`, add a `C_SubscribePresence(jid)` export that calls whatsmeow's presence subscription.
  - Handle the `events.Presence` callback (it carries `From`, `Unavailable` and `LastSeen`) and emit it as a new JSON event kind through the existing message callback.
  - Map that event to `ProviderEvent::Presence` in `crates/providers/whatsapp/src/lib.rs`.
  - Use the alternate phone JID for `@lid` members, as `memberAvatarPath` already does (`crates/providers/whatsapp/go/bridge.go:1048-1068`).
  - Constraints to handle:
    - Presence updates only arrive while our client announces itself available, which it does unless `CHATCLI_WHATSAPP_NO_PRESENCE` is set. When that variable is set, show "Presence disabled".
    - Many users hide last seen. Treat a missing `LastSeen` as "hidden", not "never".
    - Subscriptions are per contact. Subscribe only for the DM peer, the open popup's member, and a bounded number of visible members (for example 20). De-duplicate, and re-subscribe after a reconnect.

- [x] **Task 8. Slack presence.**
  - Implement `presence` by calling `users.getPresence` (`presence: active|away`; `last_activity` only for the authenticated user). Check that the existing token's scope (`users:read`) is enough, and update the manifest generator (`crates/providers/slack/src/lib.rs:8267`) if it isn't.
  - Slack provides no last-seen timestamp for other users, so show "Active" / "Away" only.
  - Rate limits (Tier 3): only for the popup member and the DM peer. Cache with a TTL of about 60s, and coalesce requests that are already in flight.

- [x] **Task 9. TUI presence cache and pipeline.**
  - Add `presence: HashMap<(ProviderId, PlatformId), Presence>` and a `loading_presence` set.
  - Add a request function that is safe to call from draw: it only queues work, and only when the cached entry is missing or older than its TTL.
  - Add a bounded drain (respecting `MAX_COMPLETION_EVENTS_PER_DRAIN`) and handle pushed `ProviderEvent::Presence` events.
  - Rendering:
    - A coloured dot (online, away, offline, unknown) next to each member's name in Details.
    - A "Last seen" row in the popup and the DM Contact section.
    - Relative times ("5m ago") are re-formatted at draw time from the stored timestamp, so no extra work runs on each tick.
  - Visible-member subscription:
    - Work out which members are visible from the current draw.
    - Queue their subscriptions in the background with a debounce, so scrolling doesn't send a burst.
    - Mark entries stale after the TTL.

- [x] **Task 10. Recency every provider can show.**
  - **Last message here:** `MAX(timestamp)` per sender for the chat.
  - **Last read:** for WhatsApp, the newest `receipts.at` where `kind` is read, for that sender in this chat.
  - Shown in the popup and as a faint suffix on member rows (e.g. "· 3d"). Computed together with the stats in Phase 3.
  - Rationale: this works on Slack, ClickUp and in offline mode, where real presence is unavailable or hidden.

### Phase 3: Activity stats (local, from SQLite)

- [x] **Task 11. Storage aggregate queries** (`crates/storage`). Add read-only query functions, each scoped to `(chat_id, account_id)` and an optional time window:
  - `chat_sender_stats`: per `sender_id` — message count, share of total, first and last message time, media count (by `content_type`), threads started (`thread_id` equal to the message id) or replies (`reply_to_id`), and average text length.
  - `chat_activity_histogram`: message counts per day for the last N days, and per hour of the day (in local time).
  - `chat_reaction_stats`: reactions given per sender and received per sender, via a join with `reactions`.
  - `chat_last_read`: newest read receipt per sender (feeds Task 10).
  - `chat_stats_coverage`: total synced messages and the oldest synced timestamp.
  - Each query is a single grouped pass over the chat's rows, using `idx_messages_chat`. Before adding an index such as `(chat_id, account_id, sender_id)`, measure timings on a large chat with the perf labels from Task 16.

- [x] **Task 12. Stats pipeline in the TUI.**
  - Run `request_chat_stats(account, chat)` on a blocking worker, with a cache keyed by `(ProviderId, ChatId)`.
  - **Invalidation:** store the chat's message count and newest timestamp with the cached result. Recompute only when the chat gains new messages, at most every few seconds per chat (debounced), so busy channels don't cause constant recomputation.
  - Discard a result if the selected chat changed while it ran, using a generation token, following the pattern from AGENTS.md.
  - Render "Computing stats…" until the first result arrives.

- [x] **Task 13. Stats in the Details overview.** A new collapsible "Activity" section between the Chat and Members sections:
  - A coverage line: "Based on 4,312 synced messages since 12 Mar 2026".
  - A 30-day activity sparkline (ratatui `Sparkline`, or Unicode block characters inside the paragraph, so the 1:1 line mapping that hit-testing relies on is preserved).
  - The busiest hour and day of the week.
  - The top 5 posters, each with a share bar, percentage and count (e.g. `Alice ████████░░ 38% (1,640)`). The bars are clickable and open that member's profile, using the Task 1 hit mechanism.
  - Your own share (from `is_from_me`).
  - A media breakdown (images, videos, files, voice, links if detectable) and the total size of media.
  - "Quiet members": the number of members with no messages in the synced window. Only shown when the member list is available.

- [x] **Task 14. Per-member stats in the popup.** An "In this chat" block:
  - Messages and share percentage, with their rank ("#2 of 37").
  - First and last message.
  - Most active hours.
  - Media sent, threads started, replies, average message length.
  - Reactions given and received, and their most-used reaction.
  - How many times they mentioned you (using `mentions_me` for messages they sent).
  - Each line is shown only when it has a value, so the popup doesn't fill up with zeros.

- [x] **Task 15. Sort the member list.** Add a sort toggle in Details (a key such as `s`, plus a clickable header label) that cycles through name, role, activity (message share) and presence. It uses the cached stats and presence and never queries during draw.

### Phase 4: Further ideas (optional, in priority order)

- [x] **Task 16a. Groups in common.** In the popup, list the other chats where this person has posted (`SELECT DISTINCT chat_id FROM messages WHERE sender_id = ? AND account_id = ?`) or is in the `chat_members` cache. Clicking one jumps to that chat. This is very useful in WhatsApp.
- [x] **Task 16b. Quick actions in the popup.**
  - Message directly (open the DM, or create it if the provider supports that).
  - Mention them in the composer.
  - Search their messages in this chat (pre-filled message filter).
  - Copy phone, email or ID to the clipboard.
  - Open their shared media.
- [x] **Task 16c. Shared media and links gallery.** For the chat or a single member, a list of attachments and links from `messages` (`media_*` columns and URLs in `content_text`). Thumbnails reuse the existing media pipeline.
- [x] **Task 16d. Linked identities.** Using the existing `persons`/`handles` tables (`crates/storage/src/schema.rs:76-89`), show "Also known as" — for example, the same person on Slack and WhatsApp — and offer to switch to that conversation.
- [x] **Task 16e. Local notes and nickname.** A per-contact note or nickname stored in `settings` or a small new table. Shown in the popup and, optionally, used as the display name.
- [x] **Task 16f. Member search box** in Details, for large channels where the 40-member render cap (`crates/tui/src/app.rs:6466`) hides most people.
- [x] **Task 16g. Response-time stats** (DMs): median time for each side to reply to the other. The data is already available, but treat this as a nice-to-have because the computation is more complex.

### Phase 5: Instrumentation, tests and validation

- [x] **Task 16. Instrumentation.** Add perf labels:
  - `details.member_hit`, `profile_overlay.draw`, `contact_profile.fetch`;
  - `presence.request`, `presence.drain`, `presence.subscribe` (with counts and stale totals);
  - `chat_stats.query` (with row counts and duration), `chat_stats.drain` (with stale and error totals);
  - `whatsapp.user_info`, `slack.users_get_presence`.
- [x] **Task 17. Tests.**
  - **Two-phase UI tests** (asserting the immediate state, then the state after draining completions):
    - Clicking a member shows the popup straight away with a loading state, then the profile data.
    - The Details pane shows "Computing stats…", then the Activity section.
    - Presence shows the unknown dot, then online/away after a mocked presence event.
  - **Storage tests:** the per-sender count and percentage maths; coverage line; the time-window filter; the histogram buckets around local-time boundaries; reaction joins; last read from receipts.
  - **Stale handling:**
    - A stats result for a previously selected chat is discarded.
    - A presence event for an unrelated member doesn't change the open popup.
    - A new message invalidates stats, and recomputation is debounced.
  - **Presence semantics:**
    - Hidden last seen shows "hidden" rather than "never".
    - Slack shows Active/Away without a timestamp.
    - With `CHATCLI_WHATSAPP_NO_PRESENCE` set, the UI says presence is disabled.
  - **Mock and Go:** extend `MockProvider` (`crates/core/src/mock.rs`) with presence and profile data; Go tests for the presence event JSON and the avatar cache keys.
- [x] **Task 18. CI parity.** Check `.github/workflows/ci.yml` and run the same `cargo check`, `cargo test`, `cargo clippy` and release-build commands, plus `go test` if CI runs it.

## Verification Criteria

- **Profiles and avatars.** Clicking a member opens the popup within one frame; details fill in asynchronously. Clicking an avatar opens a full-resolution image on Slack and WhatsApp.
- **WhatsApp presence.** WhatsApp DM peers and popup members show Online or "Last seen X ago" when their privacy settings allow, otherwise "Last seen hidden". Updates arrive live without polling.
- **Slack presence.** Slack members show Active or Away, refreshed at most once per TTL per user. No rate-limit errors in logs during normal use.
- **Local recency.** Every provider shows "Last message here" for members who have posted. WhatsApp also shows "Last read" where receipts exist.
- **Activity section.**
  - Shows a coverage line, a 30-day sparkline, the top posters with percentages, your share, and a media breakdown.
  - Percentages add up to 100% (±1% from rounding) over the covered messages.
  - The `chat_stats.query` perf label stays under the agreed budget (for example 50ms) for a chat with 50k messages, and never runs on the draw or input path.
- **Navigation.** Switching chats quickly never shows the wrong chat's stats or the wrong member's presence.
- **CI.** All CI-equivalent checks pass locally.

## Potential Risks and Mitigations

1. **Users misread partial-history stats as complete.**
   Mitigation: always show the coverage line. Optionally add a "Load more history" action that reuses the existing history paging.
2. **WhatsApp presence subscriptions look like automation or trigger anti-abuse measures.**
   Mitigation: subscribe only on demand and for a bounded set of members, de-duplicate, and never subscribe to every member of a large group.
3. **Presence is commonly hidden, so the feature looks broken.**
   Mitigation: explicit "hidden" and "unknown" states, and always show the locally derived recency next to it.
4. **Slack `users.getPresence` rate limits.**
   Mitigation: a TTL cache, coalescing requests already in flight, and only fetching for the popup member and the DM peer.
5. **Aggregate queries become slow on very large chats.**
   Mitigation: run them on a blocking worker, limit them to a time window, debounce recomputation, measure with perf labels, and add a sender index only if measurements justify it.
6. **Privacy when sharing a screen.** Stats and last seen reveal behaviour.
   Mitigation: a setting to hide the Activity section and presence (default on), alongside the optional phone/email masking from v1.
7. **`app.rs` keeps growing.**
   Mitigation: put the presence, stats and profile-popup rendering in new widget modules, keeping only event wiring in `app.rs`.

## Alternative Approaches

1. **Persist stats incrementally** (a `chat_sender_stats` table updated on every message insert).
   - Pros: O(1) reads and instant display.
   - Cons: a schema migration, double counting when history is replayed (AGENTS.md requires replay to be idempotent), and edit/delete handling.
   - Worth doing only if on-demand queries prove too slow.
2. **A dedicated "Stats" view** that takes over the Details pane (or a full-screen overlay) instead of an inline section.
   - Pros: room for bigger charts and heatmaps.
   - Cons: one more navigation step.
   - A reasonable compromise: a compact inline summary plus an "Open full stats" action.
3. **Local recency only** (skip real presence).
   - Pros: no new network calls or bridge exports, and it works the same everywhere.
   - Cons: no "Online now" and no true last seen, which was the specific request.
4. **Poll presence instead of subscribing** (WhatsApp).
   - Simpler state handling, but more network traffic and staler data. Subscribing is the native whatsmeow model.
