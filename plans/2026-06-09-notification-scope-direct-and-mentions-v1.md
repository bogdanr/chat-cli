# Notification Scope: Direct Messages and Mentions Only

## Objective

Add a user-facing notification setting that restricts notifications to **direct messages and mentions** across both Slack and WhatsApp. When enabled, messages arriving in groups or channels must be suppressed unless the current user is actually mentioned in the message. Direct (1:1) conversations always notify. The existing "notify for everything" behavior must remain available and stay the default so current users see no surprise change.

The core gap: the notification decision in `crates/tui/src/app.rs:9804-9910` (`maybe_queue_notification`) currently has no concept of "mention" and does not branch on `ChatKind`. Neither provider surfaces mention data today — Slack leaves raw `<@U123>` tokens in text without resolving "is this me", and WhatsApp discards `contextInfo.mentionedJid` entirely. This plan plumbs a single `mentions_me` signal from each provider up to the notification filter, then gates delivery on a new scope setting.

## Key Findings From Research

- `Message` (`crates/core/src/types.rs:152-166`) has no mention field; `SlackData`/`WhatsAppData` (`crates/core/src/types.rs:313-323`) carry no mention metadata.
- `ChatKind` (`crates/core/src/types.rs:31-38`) already distinguishes `Direct`, `Group`, `PublicChannel`, `PrivateChannel`, `GroupDirectMessage`.
- Slack self identity is `connection.user_id` (fallback `bot_id`), resolved at `crates/providers/slack/src/lib.rs:3175`; the single message builder is `slack_message_from_parts` (`crates/providers/slack/src/lib.rs:4662-4727`); raw `<@U123>` tokens are still present at build time and an extractor `slack_user_ids_in_text` exists (`crates/providers/slack/src/lib.rs:5131-5147`); chat kind is assigned in `conversation_chat_kind` (`crates/providers/slack/src/lib.rs:4068-4080`).
- WhatsApp own JID exists on the Go side via `ownJID()` (`crates/providers/whatsapp/go/bridge.go:697-702`) and is emitted on connect (`crates/providers/whatsapp/go/bridge.go:206-207`), but the Rust side only retains a mangled `display_name` (`crates/providers/whatsapp/src/lib.rs:920-930`). Mentions are never extracted: `emitMessageEvent` (`crates/providers/whatsapp/go/bridge.go:895-1002`) and `messageText` (`crates/providers/whatsapp/go/bridge.go:1784-1813`) never read `ContextInfo.GetMentionedJID()`. Inbound messages are built in `forward_message_event` (`crates/providers/whatsapp/src/lib.rs:1053-1157`, struct at `:1103-1123`); `BridgeEvent` decode struct is at `crates/providers/whatsapp/src/lib.rs:795-845`.
- Settings live in `AppSettings` (`crates/storage/src/lib.rs:67-77`) with a hand-written backward-compatible `Deserialize` (`crates/storage/src/lib.rs:130-189`) and a `Default` (`crates/storage/src/lib.rs:115-128`). The settings overlay enum/rows are in `crates/tui/src/app.rs:1370-1529`.

## Clarity Assessment (Assumptions)

- **Default value**: New scope defaults to `All` (notify for everything), preserving existing behavior. The "direct and mentions only" mode is opt-in.
- **Direct classification**: Only `ChatKind::Direct` counts as a direct message that always notifies. `ChatKind::GroupDirectMessage` (Slack multi-person DMs / "mpim") is treated as a group and therefore requires a mention. This is flagged as an assumption that can be flipped if undesired.
- **Slack broadcast mentions**: `@here`, `@channel`, `@everyone` (`<!here>`, `<!channel>`, `<!everyone>`) count as mentions of the user, since they directly target the user's attention. This is an assumption that can be made configurable later.
- **Single global setting**: One scope applies to all accounts/platforms (matching the existing single-mode notification model). Per-account scope is out of scope for V1.
- **Edits**: Message-edited events do not currently queue notifications, so no mention gating is needed there; the `mentions_me` field is still populated for consistency.

## Implementation Plan

### Phase 1 — Core data model

- [ ] Task 1. Add a `mentions_me: bool` field to `Message` in `crates/core/src/types.rs:152-166`. Rationale: a single boolean computed by each provider (which alone knows the authenticated identity and raw mention tokens) is the cleanest cross-platform signal for the notification filter and avoids leaking provider-specific mention encodings into the TUI layer. Update all `Message` construction sites and any struct-literal initializers to set the new field (default `false`).

### Phase 2 — Storage setting and migration

- [ ] Task 2. Add a `NotificationScope` enum (`All`, `DirectAndMentions`) near `NotificationMode` in `crates/storage/src/lib.rs:58-65`, deriving the same traits and using `#[serde(rename_all = "snake_case")]` with `All` as `#[default]`. Rationale: mirrors the existing `NotificationMode` pattern for serialization and defaulting.
- [ ] Task 3. Add `notification_scope: NotificationScope` to `AppSettings` (`crates/storage/src/lib.rs:67-77`), set it in the `Default` impl (`crates/storage/src/lib.rs:115-128`) to `NotificationScope::All`, and add the field to the hand-written `AppSettingsCompat` deserializer (`crates/storage/src/lib.rs:130-189`) so older persisted settings without the field migrate cleanly to the default. Rationale: the bespoke `Deserialize` impl must be updated explicitly or older settings will fail to load.

### Phase 3 — Slack mention detection

- [ ] Task 4. In `slack_message_from_parts` (`crates/providers/slack/src/lib.rs:4662-4727`), compute `mentions_me` from the raw message text/attachment text (still un-substituted at this point) by reusing/extending `slack_user_ids_in_text` (`crates/providers/slack/src/lib.rs:5131-5147`) and comparing extracted IDs against the passed-in current user id (the same `current_user_id` already used for `is_from_me` at `crates/providers/slack/src/lib.rs:4716`). Rationale: the builder already receives both the raw text and the self identity, making it the correct single choke point.
- [ ] Task 5. Extend the mention computation to also treat Slack broadcast tokens `<!here>`, `<!channel>`, `<!everyone>` as `mentions_me = true` (per assumption). Add a small helper (e.g. `slack_text_mentions_user`) so both realtime and history paths share identical logic. Rationale: broadcast pings are user-directed attention and keeping the logic in one helper prevents drift between pipelines.
- [ ] Task 6. Ensure both call paths feed the computed value: realtime via `emit_realtime_message` (`crates/providers/slack/src/lib.rs:4508-4603`) and history via `slack_history_message` (`crates/providers/slack/src/lib.rs:4627-4660`), since both funnel through the builder. Verify that any later mention substitution in `apply_cached_user_to_message` (`crates/providers/slack/src/lib.rs:5020-5040`) does not need to re-run detection (the boolean is already fixed at build time).

### Phase 4 — WhatsApp mention detection (Go bridge + Rust)

- [ ] Task 7. On the Go side, persist the authenticated own JID where mention comparison can use it, and extract mentioned JIDs in `emitMessageEvent` (`crates/providers/whatsapp/go/bridge.go:895-1002`) by reading `ContextInfo.GetMentionedJID()` from the relevant message variants (extended text and media context info). Add a `MentionedJIDs []string` (or a pre-computed `MentionsMe bool`) field to the `bridgeEvent` struct (`crates/providers/whatsapp/go/bridge.go:64-105`) and populate it. Rationale: the Go bridge is the only place that decodes the whatsmeow protocol where mention data exists.
- [ ] Task 8. Decide and implement the comparison point. Preferred: compute `mentions_me` in Go by comparing each mentioned JID's user-part against `ownJID()` (`crates/providers/whatsapp/go/bridge.go:697-702`) and emit a single `mentions_me` boolean, avoiding the lossy identity problem on the Rust side. Rationale: the Go side already has the canonical own JID; emitting a boolean keeps the Rust adapter thin and sidesteps the fact that the Rust side only retains a mangled `display_name` (`crates/providers/whatsapp/src/lib.rs:920-930`).
- [ ] Task 9. Add the matching field to the Rust `BridgeEvent` decode struct (`crates/providers/whatsapp/src/lib.rs:795-845`) and set `Message.mentions_me` in `forward_message_event` (`crates/providers/whatsapp/src/lib.rs:1103-1123`). For locally sent/echo and status/system messages, set `mentions_me = false`. Rationale: completes the plumbing so the boolean reaches core `Message`.

### Phase 5 — Notification filter

- [ ] Task 10. In `maybe_queue_notification` (`crates/tui/src/app.rs:9804-9910`), after the existing chat-resolution and muted checks, add a scope gate: when `settings.notification_scope == DirectAndMentions`, suppress (with a `notification.suppress reason=scope_group_no_mention` perf marker consistent with existing markers) unless the resolved chat's `ChatKind` is `Direct` **or** `message.mentions_me` is true. Rationale: placing the gate after mute/pause/off checks keeps the existing precedence and reuses the established perf-marker logging style mandated by the project guidelines.
- [ ] Task 11. Confirm the gate uses the resolved `Chat` from `notification_chat_for` (`crates/tui/src/app.rs:9912-9923`) for `ChatKind`, not the message, so classification is consistent with sidebar state.

### Phase 6 — Settings overlay UI

- [ ] Task 12. Add a `NotificationScope` row to the `SettingsItem` enum and its `ALL` array (`crates/tui/src/app.rs:1370-1394`), with `label`, `description`, `value_text`, `checkbox` (None), and `apply` (cycle) entries (`crates/tui/src/app.rs:1396-1512`). Add `notification_scope_label` and `next_notification_scope` helpers alongside the existing `notification_mode_label`/`next_notification_mode` (`crates/tui/src/app.rs:1515-1529`). Rationale: follows the established cycling-row pattern exactly.
- [ ] Task 13. Make the new row's relevance clear in its description (it only affects delivery when notifications are not `off`). Optionally consider visually de-emphasizing or skipping the row when mode is `off`, but keep behavior simple for V1 by always showing it. Rationale: avoids confusing interaction between two related settings.

### Phase 7 — Tests

- [ ] Task 14. Add storage tests asserting that legacy settings JSON without `notification_scope` deserializes to `NotificationScope::All`, and that round-trip serialization preserves an explicitly set `DirectAndMentions`, mirroring `legacy_notification_booleans_migrate_to_notification_mode` (`crates/storage/src/lib.rs:1688-1711`).
- [ ] Task 15. Add Slack provider unit tests for mention detection: a self `<@U_ME>` token yields `mentions_me = true`, an unrelated `<@U_OTHER>` yields `false`, and `<!here>`/`<!channel>`/`<!everyone>` yield `true`. Reuse the test style near `slack_user_mentions_prefer_cache_label_then_id_fallback` (`crates/providers/slack/src/lib.rs:7391-7406`).
- [ ] Task 16. Add a WhatsApp Rust-side test that a `BridgeEvent` carrying a self mention maps to `Message.mentions_me = true` and a non-self mention maps to `false` (driving `forward_message_event`).
- [ ] Task 17. Add TUI notification tests asserting the scope gate: with `DirectAndMentions`, a group message without mention is suppressed (no pending notification queued), a group message with `mentions_me` is queued, and a direct-chat message is queued regardless; with `All`, all are queued. Follow the existing two-phase async assertion guidance (immediate queue state, then drain). Rationale: the project guidelines require asserting both placeholder/queued and drained phases for async UI work.

### Phase 8 — Validation

- [ ] Task 18. Run targeted tests for the touched crates (storage, slack, whatsapp, tui) and fix failures without deleting existing tests. Rationale: project rules require validation after each performance/behavior fix and prohibit removing failing tests to force a pass.

## Verification Criteria

- The settings menu exposes a new notification scope option with values "all" and "direct and mentions" (or equivalent labels).
- Default for new/legacy installs is "all"; existing persisted settings load without error and retain prior notification behavior.
- With scope "direct and mentions": a Slack channel message that does not mention the user produces no notification; the same channel message containing `<@self>`, `@here`, `@channel`, or `@everyone` produces a notification.
- With scope "direct and mentions": a WhatsApp group message without a mention produces no notification; a WhatsApp group message whose `mentionedJid` includes the user's own JID produces a notification.
- Direct (1:1) Slack and WhatsApp messages always produce notifications regardless of scope.
- With scope "all", behavior is unchanged from today (group/channel messages still notify).
- Mention gating composes correctly with existing suppression: historical, from-me, off-mode, paused, and muted chats remain suppressed; the scope gate only further restricts group/channel non-mentions.
- Suppression due to scope emits a perf marker identifying the reason, account, chat, and message, consistent with existing markers.

## Potential Risks and Mitigations

1. **WhatsApp own-JID comparison is unreliable on the Rust side (only a mangled display name is retained).**
   Mitigation: compute `mentions_me` in the Go bridge using `ownJID()` and emit a boolean, so the Rust adapter never needs the canonical JID. Normalize JID user-parts (strip device/agent suffixes) before comparison.
2. **Slack mention tokens could be missed if detection runs after substitution.**
   Mitigation: compute `mentions_me` inside `slack_message_from_parts` while raw `<@...>`/`<!...>` tokens are still present, and cover both realtime and history paths via a single shared helper.
3. **Group DM (mpim) classification surprises users who expect notifications.**
   Mitigation: documented assumption that `GroupDirectMessage` requires a mention; isolate the direct-vs-grouped decision in one helper so the policy can be changed in one place.
4. **Adding a field to `Message` touches many construction sites and could miss one.**
   Mitigation: rely on the compiler to flag every struct-literal site; add the field with a sensible default at each provider/build location.
5. **Backward-compatible settings deserialization is hand-written and easy to break.**
   Mitigation: update `AppSettingsCompat` explicitly and add a dedicated legacy-migration test before relying on the new field.
6. **WhatsApp media messages carry mentions in a different context-info location than text.**
   Mitigation: extract `MentionedJID` from all relevant message variants (extended text and media context info) in `emitMessageEvent`, not just `Conversation`.

## Alternative Approaches

1. **Carry a `Vec<PlatformId> mentions` on `Message` plus self-resolution in the TUI**: more reusable for future features (mention highlighting), but requires the TUI to know each account's authenticated identity, which it does not today — higher complexity for V1. Trade-off: flexibility vs. significant new plumbing.
2. **Compute mention status from raw text entirely in the TUI layer**: avoids core/provider changes for Slack, but is impossible for WhatsApp (mentions are protocol metadata, not text) and still needs self identity in the TUI — rejected for inconsistency across platforms.
3. **Reuse the existing per-chat `muted` flag instead of a global scope**: lets users silence specific noisy groups, but does not satisfy the requirement of a single global "direct and mentions only" toggle and offers no mention exception — complementary, not a replacement.
4. **Make broadcast mentions (`@here`/`@channel`/`@everyone`) a separate sub-setting**: more granular control, but adds UI/storage complexity beyond the stated requirement; deferred as a future enhancement.
