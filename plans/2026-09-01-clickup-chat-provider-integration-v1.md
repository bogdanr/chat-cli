# ClickUp Chat Provider Integration

## Objective

Add ClickUp Chat as a third first-class provider in chat-cli, alongside Slack and WhatsApp, so users can browse ClickUp Channels/DMs, read history, send messages, reply in threads, and react — using the same sidebar, conversation pane, notification, and threading surfaces that already exist.

Expected outcomes:

- A new `crates/providers/clickup` crate implementing `chat_core::Provider` (`crates/core/src/provider.rs:143-313`).
- A new `Platform::ClickUp` variant wired through storage, rendering, and labels.
- A new `AccountProviderKind::ClickUp` wired through the account setup overlay and the CLI provider factory.
- Rate-limit-safe polling-based liveness (ClickUp has no realtime chat transport).
- Test coverage mirroring the Slack crate's fake-client pattern, plus two-phase async UI tests.

---

## Assessment

### Project structure summary

The workspace is a Cargo workspace declared at `Cargo.toml:1-15`, with a deliberate two-layer provider identity:

- **Domain layer** — `chat_core::Platform` (`crates/core/src/types.rs:88-94`) describes what a message came from and is persisted.
- **Setup layer** — `AccountProviderKind` (`crates/tui/src/app.rs:657-696`) describes what the user can add from the setup UI.

Providers implement one trait, `Provider` (`crates/core/src/provider.rs:143-313`), of which only ~12 methods are required; everything platform-specific (`vote_poll`, `submit_auth`, `chat_members`, `chat_details`, `discover_destinations`) has a defaulted or `bail!` implementation at `crates/core/src/provider.rs:220-312`. Capability negotiation happens at runtime through `OutboundCapabilities` (`crates/core/src/provider.rs:46-124`) and `DiscoveryCapabilities` (`crates/core/src/types.rs:183-192`), not through trait-level feature flags.

Providers push into a lossy 512-slot broadcast bus (`crates/core/src/events.rs:119-143`); the TUI pulls at most `MAX_PROVIDER_EVENTS_PER_DRAIN = 16` per tick (`crates/tui/src/app.rs:99`, drain loop `crates/tui/src/app.rs:4100-4178`). This is the entire responsiveness contract a new provider inherits.

### Relevant files examined

The Slack provider is the correct template — it is HTTP/REST-based, has no FFI, and already solves every problem ClickUp poses:

| Concern | Slack reference |
| --- | --- |
| Options struct + redacting `Debug` | `crates/providers/slack/src/lib.rs:188-209`, `crates/providers/slack/src/lib.rs:3629-3644` |
| Deterministic provider id from options | `crates/providers/slack/src/lib.rs:2445-2479` |
| `config_json()` round-trip | `crates/providers/slack/src/lib.rs:3966-3968` |
| Pooled blocking HTTP agent | `crates/providers/slack/src/lib.rs:7479-7497` |
| Global request pacing | `crates/providers/slack/src/lib.rs:7499-7514` |
| 429 retry honoring `Retry-After` | `crates/providers/slack/src/lib.rs:7516-7554` |
| Testable network seam (`SlackApiClient`) | `crates/providers/slack/src/lib.rs:911-1016` |
| Poll loop with three suppression layers | `crates/providers/slack/src/lib.rs:4983-5119` |
| `chats()` returning `last_message_at: None` | `crates/providers/slack/src/lib.rs:4718-4748` |
| Media cache + in-flight registry | `crates/providers/slack/src/lib.rs:7188-7375` |
| Fake client + fixtures | `crates/providers/slack/src/lib.rs:7783-8101` |

The persisted-account restore path is currently Slack-specific (`crates/chat-cli/src/main.rs:503-527`, id-prefix filter at `crates/chat-cli/src/main.rs:275`); the runtime factory match arms are at `crates/chat-cli/src/main.rs:279-320`.

### ClickUp Chat API findings (external research)

Verified against ClickUp's own documentation:

- **Chat lives in Public API v3** under `https://api.clickup.com/api/v3/workspaces/{workspace_id}/chat/...`. Endpoints cover channels (list/get/create/update/delete/members/followers), DMs, messages (list/send/patch/delete), reactions, and replies. ClickUp explicitly marks these endpoints **experimental and subject to change at any time**.
- **Pagination is cursor-based** with `limit` 1–100 (default 50), plus highly useful server-side filters on the channel list: `with_message_since` (only channels with a message after a timestamp), `is_follower`, `include_closed`, and `channel_types`.
- **Content format is negotiable** — `content_format` / `description_format` accept `text/md` (default) or `text/plain`. Markdown maps directly onto `Content::Text` with no custom parser, unlike Slack mrkdwn.
- **Authentication is either a personal token (`pk_...`, sent as a bare `Authorization: {token}` header, never expires) or OAuth 2.0 authorization-code** (`Authorization: Bearer {access_token}`, authorization URL `https://app.clickup.com/api`, token URL `https://api.clickup.com/api/v2/oauth/token`, currently non-expiring). Workspace enumeration uses the v2 Get Authorized Workspaces endpoint.
- **Rate limits are per token and plan-dependent: 100 req/min on Free/Unlimited/Business**, 1,000 on Business Plus, 10,000 on Enterprise. `429` responses carry `X-RateLimit-Limit`, `X-RateLimit-Remaining`, and `X-RateLimit-Reset`.
- **There is no realtime transport for Chat.** ClickUp webhooks cover tasks, lists, folders, spaces, and goals — not chat messages — and would require an inbound public HTTP endpoint the TUI cannot offer. Polling is the only viable liveness mechanism.

### Prioritized challenges and risks

Ranked by likelihood × blast radius:

1. **The 100 req/min rate limit is the dominant design constraint.** Slack's pacing is a fixed 1,100 ms gap (`crates/providers/slack/src/lib.rs:44-49`) ≈ 55 req/min, which already assumes one provider. ClickUp's budget is shared across the entire token, is plan-dependent, and a naive "list channels, then fetch history per channel" pass over a large workspace will exhaust it in seconds. This is ranked first because it silently degrades into a permanent 429 loop rather than a visible failure.
2. **No realtime means perceived latency and no delivery guarantee.** Users comparing ClickUp to the Slack Socket Mode path will see multi-second delays. Ranked second because it is a permanent product limitation, not a bug, and must be communicated in the UI.
3. **Experimental API surface.** ClickUp reserves the right to change these endpoints without notice, so response decoding must be lenient and failures must be non-fatal — mirroring `deserialize_lenient_slack_messages` (`crates/providers/slack/src/lib.rs:558-581`).
4. **`Platform` is matched exhaustively in ten places.** Adding a variant breaks compilation at `crates/tui/src/app.rs:4669-4672`, `crates/tui/src/app.rs:17521-17528`, `crates/tui/src/app.rs:18413-18420`, `crates/tui/src/app.rs:18719-18724`, `crates/tui/src/widgets/chat_list.rs:1408-1420`, `crates/tui/src/widgets/chat_list.rs:1581-1587`, `crates/tui/src/widgets/chat_list.rs:1689-1704`, `crates/tui/src/widgets/chat_list.rs:1783-1789`, and `crates/storage/src/lib.rs:1957-1973`. Two further sites use `_ =>` wildcards and will compile while silently misbehaving: `crates/tui/src/app.rs:4828-4834` (conversation presentation defaults to WhatsApp layout) and `crates/tui/src/app.rs:17258-17263` (member listing defaults to `false`).
5. **`AccountProviderKind::ALL` is a hardcoded `[Self; 3]`** at `crates/tui/src/app.rs:665`, and the test factory at `crates/tui/src/app.rs:22128-22138` is exhaustive while two other test factories use `_ => bail!` (`crates/tui/src/app.rs:21625-21628`, `crates/tui/src/app.rs:21932-21938`) and will fail silently.
6. **Persisted-account restore is Slack-only.** `crates/chat-cli/src/main.rs:503-527` and the id-prefix filter at `crates/chat-cli/src/main.rs:275` need a per-platform dispatch before a third provider can survive a restart.
7. **Personal tokens are workspace-wide, long-lived, and never expire.** Storing them in `config_json` (`crates/storage/src/lib.rs:367-447`) is consistent with existing Slack behavior but raises the stakes on the redaction discipline at `crates/providers/slack/src/lib.rs:7573-7617`.

---

## Implementation Plan

### Phase 1 — Core model and scaffolding

- [ ] Task 1. Add `ClickUp` to `chat_core::Platform` at `crates/core/src/types.rs:88-94`. Rationale: the domain layer must be able to represent and persist ClickUp messages before any provider code compiles, and this variant is what drives storage round-trip and all rendering dispatch.
- [ ] Task 2. Add the `clickup` string mapping to `platform_to_str` / `platform_from_str` at `crates/storage/src/lib.rs:1957-1973`. Rationale: without this, accounts and messages will not survive a restart; this is the narrowest possible persistence change and should land before UI work.
- [ ] Task 3. Resolve every exhaustive `Platform` match site listed in risk 4 above, adding a ClickUp arm with a distinct badge glyph, accent color, and label. Rationale: these are compile errors, so completing them early keeps the tree buildable throughout the rest of the work.
- [ ] Task 4. Audit and explicitly handle the two wildcard `Platform` sites at `crates/tui/src/app.rs:4828-4834` and `crates/tui/src/app.rs:17258-17263`. Rationale: these compile silently; ClickUp Channels are channel-shaped (Slack-like presentation, member lists available), so inheriting the WhatsApp default would produce wrong layout and hide the members pane.
- [ ] Task 5. Add ClickUp brand assets under `assets/` following the existing naming used by `static_account_icon_path` at `crates/tui/src/app.rs:18719-18724`. Rationale: the account badge and sidebar icon paths are derived by prefix convention, so missing assets surface as blank badges rather than errors.
- [ ] Task 6. Create the `crates/providers/clickup` crate, register it in the workspace members list at `Cargo.toml:1-15`, and depend on `chat-core`, `ureq`, `serde`, `serde_json`, `anyhow`, `async-trait`, `chrono`, and `tokio`, mirroring `crates/providers/slack/Cargo.toml:1-22`. Rationale: matching Slack's dependency set means no new transitive dependencies and no new CI toolchain requirements.

### Phase 2 — Options, identity, and configuration

- [ ] Task 7. Define `ClickUpProviderOptions` with an all-`Option<String>` flat shape plus a `ClickUpAuthMode` enum covering `PersonalToken` and `OAuth`, deriving `Clone`/`Serialize`/`Deserialize`, mirroring `crates/providers/slack/src/lib.rs:188-209`. Rationale: the flat serde shape is what makes `config_json` round-tripping trivial and keeps CLI flag mapping mechanical.
- [ ] Task 8. Implement a hand-written `Debug` for the options and credential types that redacts every token, secret, and workspace token, mirroring `crates/providers/slack/src/lib.rs:3629-3644` and the `redacted_option` helper at `crates/providers/slack/src/lib.rs:4491-4496`. Rationale: personal tokens never expire, so a single leaked log line is a permanent credential compromise.
- [ ] Task 9. Implement `Display` / `FromStr` for `ClickUpAuthMode` and `TryFrom<AuthSubmissionMode>`, mirroring `crates/providers/slack/src/lib.rs:3910-3951`. Rationale: the setup overlay speaks `AuthSubmissionMode` (`crates/core/src/provider.rs:7-30`) while the CLI and TOML config speak strings; both conversions are required.
- [ ] Task 10. Implement a pure `provider_id_for_options` deriving a stable id from auth mode plus workspace identifier, with a sanitizer, mirroring `crates/providers/slack/src/lib.rs:2445-2479`. Rationale: a deterministic id is what allows multiple ClickUp workspaces to coexist and lets stored account rows re-associate on restart without a migration.
- [ ] Task 11. Implement `config_json()` as a plain serialization of the options, mirroring `crates/providers/slack/src/lib.rs:3966-3968`. Rationale: the store treats this as an opaque blob (`crates/storage/src/lib.rs:161-167`), so the provider owns its own config shape.
- [ ] Task 12. Add CLI flags with env fallbacks (`--clickup`, `--clickup-auth-mode`, `--clickup-token`, `--clickup-workspace`, `--clickup-client-id`, `--clickup-client-secret`, `--clickup-redirect-uri`) to `Args` at `crates/chat-cli/src/main.rs:22-109`, and a `single_clickup_options` mapper mirroring `crates/chat-cli/src/main.rs:432-444`. Rationale: env-var fallback is how `test.sh:4-19` drives local development and must exist for the provider to be testable end-to-end.
- [ ] Task 13. Refactor the Slack-only persisted-account restore at `crates/chat-cli/src/main.rs:503-527` and the id-prefix filter at `crates/chat-cli/src/main.rs:275` into a per-platform dispatch, then add the ClickUp branch. Rationale: risk 6 — without this, a configured ClickUp account is silently dropped on the next launch; refactoring now avoids a third copy of the same logic.
- [ ] Task 14. Extend `build_providers_with_persisted` (`crates/chat-cli/src/main.rs:328-360`) and `build_account_provider_factory` (`crates/chat-cli/src/main.rs:263-321`) with ClickUp arms, including the duplicate-id `HashSet` guard used by the Slack arm. Rationale: these are the two registration points — startup (static) and runtime (dynamic) — and both must agree on id derivation.

### Phase 3 — HTTP client layer

- [ ] Task 15. Define an async `ClickUpApiClient` trait in domain terms (list workspaces, current user, list channels, get channel, channel members, list messages, send message, send reply, list replies, add/remove reaction, list attachments, download bytes), with a real `ClickUpHttpClient` implementation delegating to free functions, mirroring `crates/providers/slack/src/lib.rs:911-1016` and `crates/providers/slack/src/lib.rs:1243-1383`. Rationale: this seam is what lets the Slack crate run 91 tests with zero network access, and it must exist before any provider logic is written or the tests will be untestable retrofits.
- [ ] Task 16. Implement a process-wide pooled `ureq::Agent` in a `OnceLock` with an explicit timeout and `http_status_as_error(false)`, mirroring `crates/providers/slack/src/lib.rs:7479-7497`. Rationale: `429` and `4xx` must arrive as inspectable responses so headers can be read, not as opaque transport errors.
- [ ] Task 17. Implement an adaptive rate limiter driven by `X-RateLimit-Remaining` and `X-RateLimit-Reset` rather than a fixed sleep, defaulting to a conservative fixed gap until the first response reveals the plan's limit. Rationale: risk 1 — the budget is plan-dependent (100 vs 1,000 vs 10,000 req/min), so a hardcoded constant is either needlessly slow on Enterprise or fatally aggressive on Free; reading the headers makes the limiter self-tuning.
- [ ] Task 18. Implement `429` retry with `Retry-After` / `X-RateLimit-Reset` backoff, a bounded attempt count, and a clamped sleep window, mirroring `crates/providers/slack/src/lib.rs:7516-7554`. Rationale: bounded retries prevent a rate-limited workspace from turning into an unbounded request storm.
- [ ] Task 19. Wrap every blocking `ureq` call in `tokio::task::spawn_blocking`, mirroring the discipline at `crates/providers/slack/src/lib.rs:1388-1417`. Rationale: mandated by the repository performance rules — no network I/O on the async runtime hot path.
- [ ] Task 20. Implement two-layer error handling: `anyhow::Context` for transport plus explicit decoding of ClickUp's error envelope into a readable message, with `#[serde(default)]` on all optional collections and a lenient per-message decoder that skips and logs unparsable entries, mirroring `crates/providers/slack/src/lib.rs:558-581`. Rationale: risk 3 — an experimental API will add and rename fields, and one unexpected field must not blank an entire channel.
- [ ] Task 21. Implement an error sanitizer that scrubs `pk_` tokens and bearer tokens from every error string crossing into the UI, mirroring `crates/providers/slack/src/lib.rs:7573-7617`. Rationale: error text is rendered in the sidebar and account status line, and tokens must never reach the terminal or the debug log.
- [ ] Task 22. Route all provider-level API calls through a `call_api` wrapper that emits `ProviderEvent::NetworkActivity` Tx before and Rx after, mirroring `crates/providers/slack/src/lib.rs:2600-2619`. Rationale: the network indicator is the user's only feedback that a polling provider is alive, and `provider_event_requests_draw` (`crates/tui/src/app.rs:17027-17039`) already suppresses repaints when it is hidden.

### Phase 4 — Authentication and setup UX

- [ ] Task 23. Add `AccountProviderKind::ClickUp` at `crates/tui/src/app.rs:657-662`, widen `ALL` from `[Self; 3]` to `[Self; 4]` at `crates/tui/src/app.rs:665`, and add `label()`, `summary()`, and `setup_hint()` arms at `crates/tui/src/app.rs:667-695`. Rationale: this is the launcher entry the user selects; the hint text is the right place to state the polling limitation up front.
- [ ] Task 24. Update the exhaustive test factory at `crates/tui/src/app.rs:22128-22138` and explicitly review the two `_ => bail!` test factories at `crates/tui/src/app.rs:21625-21628` and `crates/tui/src/app.rs:21932-21938`. Rationale: risk 5 — only one of these three sites is compile-checked.
- [ ] Task 25. Review the length- and index-dependent account-setup call sites at `crates/tui/src/app.rs:2126`, `crates/tui/src/app.rs:7609`, `crates/tui/src/app.rs:10070`, `crates/tui/src/app.rs:10082`, `crates/tui/src/app.rs:10556`, `crates/tui/src/app.rs:14923`, and `crates/tui/src/app.rs:15040`. Rationale: the overlay geometry and hit-testing derive from `ALL.len()`, so a fourth entry changes modal height and row mapping.
- [ ] Task 26. Implement `Provider::submit_auth` accepting an `AuthSubmissionMode::ProviderSpecific` personal-token submission, validating the token by calling the authorized-workspaces endpoint, and emitting `AuthSucceeded` or a sanitized failure. Rationale: validation belongs in the provider, matching the deliberate design at `crates/tui/src/app.rs:13797-13894` where the UI only trims values and renders errors.
- [ ] Task 27. Implement `connect()` to emit `AuthRequired` when no credential is configured, and `AuthSucceeded` followed by `SyncComplete` when validation passes, mirroring `crates/providers/slack/src/lib.rs:4040-4067`. Rationale: `AuthRequired` is what triggers the setup overlay via the platform branch at `crates/tui/src/app.rs:8823-8853`.
- [ ] Task 28. For Phase 1 scope, route ClickUp through the generic `AuthOverlay` path at `crates/tui/src/app.rs:8842-8851` with a single token-entry challenge rather than building a Slack-style seven-mode wizard. Rationale: ClickUp Phase 1 needs exactly one field (a `pk_` token); a bespoke overlay mirroring `crates/tui/src/app.rs:1447-1541` and `crates/tui/src/app.rs:9644-9872` is ~500 lines of UI that only becomes justified once OAuth and multi-workspace selection land.
- [ ] Task 29. Implement workspace selection: when the token authorizes multiple workspaces, surface them as a choice and persist the chosen `workspace_id` into the options. Rationale: every v3 chat endpoint is workspace-scoped, so a workspace id is a hard prerequisite for `chats()` and cannot be deferred.
- [ ] Task 30. Defer the OAuth authorization-code flow to a follow-up phase, but keep `ClickUpAuthMode::OAuth` and the client-id/secret/redirect option fields reserved and unused. Rationale: reserving the config shape now avoids a breaking `config_json` migration later, while keeping Phase 1 shippable.

### Phase 5 — Reading: chats, history, and threads

- [ ] Task 31. Implement `chats()` by paginating the channel-list endpoint with `limit=100` and cursor following, then mapping each channel to `chat_core::Chat`. Rationale: cursor pagination with the maximum page size minimizes requests against the rate budget.
- [ ] Task 32. Map ClickUp channel types to `ChatKind` and `ChatMembership` (`crates/core/src/types.rs:104-124`), treating DMs as `Direct`, group DMs as `GroupDirectMessage`, and location/standalone channels as public or private channels according to their privacy flag. Rationale: `ChatKind` drives sidebar bucketing via `sort_chats` (`crates/providers/slack/src/lib.rs:7456-7477`) and the inbox-style grouping in the chat list widget.
- [ ] Task 33. Set `last_message_at: None` and `last_message_preview: None` on every returned `Chat`, mirroring `crates/providers/slack/src/lib.rs:4744-4745`. Rationale: this is a hard repository rule — these fields must represent actual message activity, not provider conversation metadata; activity is filled downstream by `refresh_chat_preview` (`crates/tui/src/app.rs:16005-16040`) and protected by `preserve_sidebar_activity_metadata` (`crates/tui/src/app.rs:17189-17216`).
- [ ] Task 34. Implement `history()` and `history_before_message()` using the channel-messages endpoint with `content_format=text/md` and cursor pagination, converting message bodies directly into `Content::Text`. Rationale: ClickUp serving Markdown natively removes the need for a mrkdwn-style translator, which is roughly 400 lines of the Slack crate.
- [ ] Task 35. Map ClickUp message authors to `Sender` (`crates/core/src/types.rs:251-256`), computing `is_from_me` from the authenticated user id and `mentions_me` from the tagged-users field, per the contract documented at `crates/core/src/types.rs:241-247`. Rationale: `mentions_me` is provider-computed and drives the direct-and-mentions notification scope.
- [ ] Task 36. Map ClickUp replies onto `Message.thread_id` and `Message.reply_to`, and implement `chat_members()` using the channel-members endpoint. Rationale: ClickUp replies are a first-class threading concept that maps cleanly onto the existing thread inbox model (`ThreadSummary`, `crates/core/src/types.rs:19-86`), so threading is nearly free.
- [ ] Task 37. Add a `ClickUpData` variant to `PlatformData` at `crates/core/src/types.rs:542-559` carrying the workspace id, channel id, and message id. Rationale: `react` and `mark_read` need the raw platform identifiers, exactly as Slack's `react` requires `SlackData` (`crates/providers/slack/src/lib.rs:4236-4295`).
- [ ] Task 38. Implement `chat_details()` and `contact_profile()` from the channel and user endpoints, or leave the trait defaults where data is unavailable. Rationale: the details pane degrades gracefully via `ChatDetails::is_empty` (`crates/core/src/types.rs:345-358`), so partial implementation is acceptable and preferable to fabricated data.

### Phase 6 — Polling and liveness

- [ ] Task 39. Implement a single polling loop that first calls the channel-list endpoint with `with_message_since` set to the previous pass timestamp, and only then fetches messages for the channels it returns. Rationale: this is the single most important design decision in the plan — it converts an O(number of channels) pass into O(number of active channels), which is what makes the 100 req/min budget survivable; Slack has no equivalent server-side filter and had to invent a client-side snapshot heuristic instead (`crates/providers/slack/src/lib.rs:5020-5035`).
- [ ] Task 40. Bound each pass with a maximum number of channels fetched and a maximum message page size, deferring overflow to the next pass. Rationale: mandated by the repository rule on bounded batches; an unbounded pass over a busy workspace would both exhaust the rate budget and flood the 512-slot broadcast bus.
- [ ] Task 41. Implement per-pass message deduplication keyed `"{chat_id}:{message_id}"` combined with a `started_at` liveness anchor, mirroring `crates/providers/slack/src/lib.rs:5064-5081` and `crates/providers/slack/src/lib.rs:5175-5190`. Rationale: without the liveness anchor, the first poll after launch emits the entire visible window as live messages and generates a burst of spurious notifications.
- [ ] Task 42. Implement a permanently-inaccessible channel set for `404`, archived, and forbidden responses, mirroring `crates/providers/slack/src/lib.rs:4871-4886` and `crates/providers/slack/src/lib.rs:5083-5109`. Rationale: retrying a deleted channel every pass wastes a scarce request slot and produces recurring error noise.
- [ ] Task 43. Emit `ProviderEvent::ChatUpdated` per polled channel and `ProviderEvent::Message { is_historical: false }` only for messages that pass both the dedup and liveness gates, mirroring `crates/providers/slack/src/lib.rs:5046` and `crates/providers/slack/src/lib.rs:5076-5079`. Rationale: `is_historical` selects between in-place append and a selected-message reload at `crates/tui/src/app.rs:8681-8683`, and getting it wrong causes visible scroll jumps.
- [ ] Task 44. Make the poll interval adaptive — shorter when the user has a ClickUp chat selected, longer when idle or when the remaining rate budget is low. Rationale: risk 2 — the interactive latency that matters is the selected conversation, and spending the budget there rather than uniformly is the best available mitigation for the absence of realtime.
- [ ] Task 45. Emit a one-time `ProviderEvent::AccountNotice` on connect explaining that ClickUp Chat has no realtime transport and updates arrive on an interval. Rationale: risk 2 — setting the expectation once is far better than users reporting missing messages; the notice severity enum already exists at `crates/core/src/events.rs:99-109`.
- [ ] Task 46. Add opt-in performance instrumentation to the poll pass with labels identifying the phase and including channel counts, request counts, remaining rate budget, and stale/error totals. Rationale: mandated by the repository rule that every new async pipeline carries instrumentation, and the rate budget is the metric most likely to need field diagnosis.

### Phase 7 — Writing: send, react, read state

- [ ] Task 47. Implement `send()` dispatching on `Content`, routing `Content::Text` to the send-message endpoint and `reply_to` to the reply endpoint, returning the ClickUp message id as the `MessageId`. Rationale: mirrors `crates/providers/slack/src/lib.rs:4147-4170`; returning the real id is what lets subsequent reactions and edits target the message.
- [ ] Task 48. Implement `outbound_capabilities()` reporting text and reply support, and — for Phase 1 — no media support, with a `media_note` explaining the limitation. Rationale: `supports_content` and `unsupported_reason` (`crates/core/src/provider.rs:97-123`) gate the compose bar at runtime, so an honest capability report produces a clear message instead of a failed send.
- [ ] Task 49. Implement `react()` as a toggle that reads existing reactions to decide add versus remove, then emits `ProviderEvent::ReactionChanged`, mirroring `crates/providers/slack/src/lib.rs:4236-4295`. Rationale: ClickUp's reaction endpoint requires lower-case emoji names, so an emoji-glyph-to-name mapping table is needed, analogous to `crates/providers/slack/src/lib.rs:7160-7182`.
- [ ] Task 50. Implement `mark_read()` as a documented no-op returning `Ok(())`, with unread counts maintained client-side. Rationale: matches the existing Slack behavior at `crates/providers/slack/src/lib.rs:4228-4234`; ClickUp's public Chat API exposes no read-state write, so local tracking via `mark_selected_chat_read` (`crates/tui/src/app.rs:12421-12450`) is the only option. This is a known, shared limitation rather than a ClickUp-specific gap.
- [ ] Task 51. Implement `search()` and `discover_destinations()`, or accept the trait default. Rationale: the default `discover_destinations` at `crates/core/src/provider.rs:257-281` scans `self.chats()` in-process, which for a large ClickUp workspace is an unbounded scan on an interactive path — prefer a cached channel list over a live fetch.

### Phase 8 — Attachments and media (follow-up phase)

- [ ] Task 52. Map ClickUp message attachments to `Media` (`crates/core/src/types.rs:504-513`) and to `Card` where the attachment is a link preview or task reference. Rationale: ClickUp chat messages routinely embed task references, which render far better as cards than as raw URLs.
- [ ] Task 53. Implement `download_media()` with a deterministic cache path, a size-tiered eager-versus-on-demand policy respecting `MEDIA_AUTO_DOWNLOAD_LIMIT_BYTES` (`crates/core/src/types.rs:13-17`), an in-flight and failure registry with a cooldown, and atomic `.part`-plus-rename writes, mirroring `crates/providers/slack/src/lib.rs:7188-7375`. Rationale: the registry is what prevents retry storms, and atomic writes are what prevent `path.exists()` from observing a partial file during a draw.
- [ ] Task 54. Keep all image decoding and file writing on blocking worker threads, never on the runtime hot path or the draw path. Rationale: explicit repository rule on media work.
- [ ] Task 55. Implement outbound attachment upload via the v3 attachments endpoint and widen `outbound_capabilities()` accordingly. Rationale: sending files is the natural completion of the media story but is independent of reading, so it belongs last.

### Phase 9 — Testing and validation

- [ ] Task 56. Build a `FakeClickUpApiClient` as a `Default`-constructed struct of `Mutex`-wrapped call logs and canned responses, with substring-based failure injection, mirroring `crates/providers/slack/src/lib.rs:7783-8073`. Rationale: this pattern is what allows the Slack crate's 91 tests to run with zero network and no HTTP fixtures.
- [ ] Task 57. Add a `with_api_client` constructor for test injection, mirroring `crates/providers/slack/src/lib.rs:2621-2656`. Rationale: without an injection point the fake client is unreachable.
- [ ] Task 58. Add unit tests for the pure decision functions: provider-id derivation, options redaction, rate-limit header parsing, emoji-name mapping, channel-kind mapping, and the poll liveness predicate. Rationale: these are the functions most likely to regress and cheapest to test, mirroring `crates/providers/slack/src/lib.rs:8324-8372`.
- [ ] Task 59. Add poll-pass tests asserting dedup, the `with_message_since` narrowing, the skip of inaccessible channels, per-pass bounds, and rate-limit backoff, mirroring `crates/providers/slack/src/lib.rs:8424-8785`. Rationale: the polling loop is where every serious failure mode lives, and each suppression layer needs an independent test.
- [ ] Task 60. Add two-phase async TUI tests asserting the immediate placeholder or loading state after selecting a ClickUp chat, then the final state after draining background completions, mirroring `crates/tui/src/app.rs:22126-22145`. Rationale: explicit repository rule for asynchronous UI work.
- [ ] Task 61. Add a storage round-trip test for `Platform::ClickUp` and for ClickUp account config persistence and restore. Rationale: risk 6 — the restore path is the most likely place for a silent regression, and it has no UI symptom until the next launch.
- [ ] Task 62. Run the full CI mirror locally before committing: `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo build --release -p chat-cli`, per `.github/workflows/ci.yml:13-86` and the repository safety rules. Rationale: `clippy` runs with `-D warnings`, so any lint in new code fails CI.

### Phase 10 — Documentation

- [ ] Task 63. Update `README.md` — the tagline at `README.md:7`, first-run text at `README.md:92-99`, the connecting-accounts section and provider bullets at `README.md:111-125`, the theme list at `README.md:157-163`, the advanced-config flags at `README.md:166-215`, and the workspace layout at `README.md:239-251`. Rationale: the README enumerates providers in seven separate places, and a partial update is worse than none.
- [ ] Task 64. Extend `test.sh` with ClickUp env-var pass-through and a non-fatal warning when the token is unset, mirroring `test.sh:14-19`. Rationale: this is the documented local development entry point and is how the provider will actually be exercised by hand.

---

## Verification Criteria

- `cargo check --workspace`, `cargo test --workspace`, and `cargo clippy --workspace -- -D warnings` all pass, matching `.github/workflows/ci.yml:13-86`.
- Selecting ClickUp in the account overlay prompts for a personal token; an invalid token produces a sanitized error containing no fragment of the token, and a valid token transitions the account to connected.
- After connecting, the sidebar lists ClickUp Channels, DMs, and group DMs with correct `ChatKind` bucketing and correct platform badge and accent color.
- Every `Chat` returned by `chats()` has `last_message_at == None` and `last_message_preview == None`; sidebar ordering is driven exclusively by observed messages and never regresses during catch-up.
- Selecting a ClickUp chat renders a placeholder within one frame and populates history from a background load, with results applied only when they still match the selected chat and the latest generation token.
- A message sent from another ClickUp client appears within one poll interval, exactly once, with no duplicate notification.
- Messages sent from chat-cli appear in ClickUp with correct Markdown rendering; replies land in the correct thread.
- Reacting toggles correctly and is reflected in ClickUp.
- Steady-state request rate stays below 50% of the detected plan rate limit with an idle workspace; a forced `429` produces a bounded backoff and recovery with no request storm and no UI stall.
- Restarting chat-cli restores the ClickUp account, its workspace selection, and its chats without re-prompting for the token.
- The performance log shows bounded per-pass channel and request counts, and no ClickUp label appears among the top blocking phases.
- No token, secret, or bearer value appears anywhere in `tmp/debug.log` or the performance log.

---

## Potential Risks and Mitigations

1. **Rate limit exhaustion on Free/Unlimited/Business plans (100 req/min per token), shared with any other tool using the same token.**
   Mitigation: use `with_message_since` on the channel list so message fetches scale with active channels rather than total channels (Task 39); drive pacing adaptively from `X-RateLimit-Remaining` and `X-RateLimit-Reset` rather than a fixed constant (Task 17); bound channels and pages per pass (Task 40); lengthen the interval when idle or when the remaining budget is low (Task 44); surface remaining budget in the performance log (Task 46).

2. **No realtime transport, so message latency equals the poll interval and users may perceive dropped messages.**
   Mitigation: emit a one-time account notice stating the limitation (Task 45); poll the selected conversation more aggressively than the background (Task 44); state the constraint in the setup hint text (Task 23) and the README (Task 63). Webhooks are not a viable alternative — ClickUp webhooks do not emit chat message events and would require an inbound public endpoint.

3. **ClickUp explicitly documents the Chat endpoints as experimental and subject to change without notice.**
   Mitigation: lenient per-message decoding that skips and logs unparsable entries rather than failing a page (Task 20); `#[serde(default)]` on all optional collections; confine all wire types behind the `ClickUpApiClient` trait (Task 15) so a breaking change is a localized edit; keep a dedicated schema-drift test fixture set.

4. **Adding a `Platform` variant breaks ten exhaustive match sites and silently changes behavior at two wildcard sites.**
   Mitigation: complete all exhaustive sites in Phase 1 Task 3 to keep the tree buildable, then explicitly audit the two wildcard sites in Task 4; treat `crates/tui/src/app.rs:4828-4834` and `crates/tui/src/app.rs:17258-17263` as required review, not optional.

5. **`AccountProviderKind::ALL` is a hardcoded array length and two test factories use `_ => bail!` rather than exhaustive matches.**
   Mitigation: Tasks 23–25 widen the array, update the one compile-checked test factory, and explicitly review the two silent ones alongside the seven length- and index-dependent overlay call sites.

6. **Persisted-account restore is Slack-specific, so a ClickUp account could vanish on restart.**
   Mitigation: refactor to a per-platform dispatch before adding the ClickUp branch (Task 13), and cover it with an explicit storage round-trip test (Task 61).

7. **Long-lived, never-expiring personal tokens stored in `config_json` and potentially echoed into logs.**
   Mitigation: hand-written redacting `Debug` on all credential-bearing types (Task 8); a dedicated error sanitizer on every path into the UI (Task 21); a test asserting no token substring appears in formatted debug output or sanitized errors (Task 58); document that the store is only as protected as the user's filesystem.

8. **HTTP pacing, retry, and media-registry logic will be duplicated between the Slack and ClickUp crates.**
   Mitigation: accept the duplication for the initial implementation to keep the diff reviewable and avoid destabilizing Slack, then extract a shared `providers/http-common` crate as a dedicated follow-up once both call sites are proven. Premature extraction risks a regression in the more mature provider.

9. **Emoji name mapping mismatch — ClickUp requires lower-case emoji names while the picker yields glyphs.**
   Mitigation: build an explicit mapping table analogous to `crates/providers/slack/src/lib.rs:7160-7182`, and treat an unmapped glyph as a clean, user-visible unsupported-reaction error rather than a silent failure.

10. **Scope creep — the full Slack feature surface is roughly 7,700 lines of production code.**
    Mitigation: the phase boundaries are deliberately shippable. Phases 1–7 deliver read, send, reply, and react, which is a genuinely useful provider. Phase 8 (media) and OAuth (Task 30) are explicitly deferred, with config shape reserved so no migration is needed.

---

## Alternative Approaches

1. **Webhook-based liveness instead of polling.** Register a ClickUp webhook and receive push events. Trade-offs: rejected — ClickUp webhooks cover tasks, lists, folders, spaces, and goals, not chat messages, and would additionally require a publicly reachable inbound HTTP endpoint, which a local TUI cannot provide without a tunnel. Not viable today; worth revisiting if ClickUp adds chat webhook events.

2. **Reverse-engineer ClickUp's internal WebSocket (as used by the web app) for realtime.** Trade-offs: would deliver true realtime and eliminate the rate-limit pressure, but relies on an undocumented, unsupported, unversioned protocol that can break without notice, and likely conflicts with ClickUp's terms of service. Rejected for the primary path; the `WhatsAppProvider` bridge precedent (`crates/providers/whatsapp/src/lib.rs:42-57`) shows the project is willing to take on unofficial transports, so this could be an opt-in experimental mode much later.

3. **Model ClickUp Chat as read-only first.** Ship `chats()` and `history()` with no `send()`, then add writes. Trade-offs: smaller and safer initial diff with no risk of posting wrong content to a real workspace, but a read-only chat client has low practical value and the send path is the least risky part of the API. Rejected as a shipping boundary, though it is a reasonable intermediate review checkpoint.

4. **Extract a shared HTTP-provider crate first, then build ClickUp on top of it.** Trade-offs: avoids duplicating pacing, retry, and media-registry logic, and yields a cleaner third provider. However, it requires refactoring the 11,014-line Slack crate before any ClickUp value ships, and risks regressing the most-used provider. Rejected as a prerequisite; recommended as an immediate follow-up (risk 8).

5. **Use ClickUp's v2 Chat View comment endpoints instead of the v3 Chat API.** Trade-offs: v2 comment endpoints are stable and non-experimental, but they model the legacy Chat *view* rather than modern ClickUp Chat Channels and DMs, lack channel enumeration, and would not match what users see in the ClickUp app. Rejected — wrong product surface.

6. **Build a bespoke multi-mode setup wizard for ClickUp mirroring the Slack overlay.** Trade-offs: a polished onboarding experience with per-mode field sets and capability preview, but roughly 500 lines of UI (`crates/tui/src/app.rs:1447-1541`, `crates/tui/src/app.rs:9644-9872`, `crates/tui/src/app.rs:6882`) to collect a single token field. Deferred (Task 28) until OAuth and multi-workspace selection make multiple modes real.
