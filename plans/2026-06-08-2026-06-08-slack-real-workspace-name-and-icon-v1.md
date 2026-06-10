# Use Real Slack Workspace Name and Icon

## Objective

Make the app show each Slack account's **real workspace identity** automatically: (A) use the actual Slack workspace/team name (e.g. the real name behind "erepublik.com") as the account label instead of the manually-typed "Workspace 1", eliminating the current "Slack (Slack (Workspace 1))" double-wrap; and (B) retrieve the workspace icon and render it in place of the generic `[SL]` text badge. No manual rename UI is required — the correct values are derived from Slack on connect.

> Supersedes the earlier "manual rename" plan (v1/v2). The display-label decoupling insight from those versions is retained where useful, but the primary mechanism is now automatic derivation from Slack, not user editing.

## Screenshot Evidence

A live screenshot of the running TUI shows:
- Account label rendered as **"Slack (Slack (Workspace 1))"** in the Chats header ("Account: Slack (Slack (Workspa…"), the Details pane `Account:` line, the Details `Account filter:` line, and the status bar ("filtered to Slack (Slack (Workspace 1))").
- Every chat row prefixed with the text badge **`[SL]`** (e.g. `[SL] #bob-updates`, `[SL] #infrastructure`).
- An avatar column on the left showing magenta initials tiles for channels (BU, I, A, R, B, WD) and real grayscale photo thumbnails for DMs — proving the chat list already renders decoded images, not just initials.

## Initial Assessment

### (A) The real workspace name is already available, just unused

- `auth.test` returns the workspace name in its `team` field (`SlackAuthTestResponse`, `crates/providers/slack/src/lib.rs:664-673`), captured into `SlackValidatedCredential.team_name` (`crates/providers/slack/src/lib.rs:877`) and persisted into `SlackConnectionState.team_name`/`team_id` (`crates/providers/slack/src/lib.rs:173-176`, `1595-1598`).
- The Account `display_name` is instead built from the manual `workspace` option: at construction (`crates/providers/slack/src/lib.rs:1648-1663`) and after auth (`crates/providers/slack/src/lib.rs:1704-1716`). Because the setup flow seeds `workspace_label` from the already-formatted display name (see earlier analysis of `open_slack_setup_for_*`), the wrapper compounds into "Slack (Slack (…))".
- `team.info` (a Web API method not yet called) returns a richer identity: `name`, `domain`, `email_domain`, and `icon.image_34/44/68/88/102/132/230/default`. This is the authoritative source for both a human name and the icon.

### (B) The icon plumbing exists; only the fetch and a render site are missing

- A new `team.info` API method is needed on the `SlackApiClient` trait (`crates/providers/slack/src/lib.rs:517-588`) and `SlackWebApiClient` impl (`crates/providers/slack/src/lib.rs:737-851`); it should follow the existing GET pattern of `get_web_api_user_info` (`crates/providers/slack/src/lib.rs:1145-1181`).
- `slack_avatar_path(url)` already turns an image URL into a cached `PathBuf`, downloading bytes on a background thread (`crates/providers/slack/src/lib.rs:4073-4128`). It can produce the icon path the same way it does for user avatars.
- `Account.avatar` exists (`crates/core/src/types.rs:25`) but is currently set to `None` (`crates/providers/slack/src/lib.rs:1659`, `1714`) and is only consumed when building outgoing sender identity in the TUI (`send` paths) — it has **no display render site** today.
- The chat list `[SL]` badge is plain text from `platform_badge` (`crates/tui/src/widgets/chat_list.rs:968-975`), applied inline on line 1 of each two-line item (`crates/tui/src/widgets/chat_list.rs:504-506`); the avatar column reserves it width at `:469-474`.
- The avatar image pipeline is fully reusable and AGENTS.md-compliant: draw is cache/placeholder-only (`chat_avatar_rows`), decode runs on `spawn_blocking` (`queue_avatar_preview`), and results apply via a bounded drain (`drain_avatar_preview_fetches`) into `avatar_preview_cache`. Half-block decode lives in `decode_image_preview_rows` (`crates/tui/src/widgets/message_list.rs:2432-2482`) keyed by path+size.

### Critical constraint carried over from prior analysis

The provider ID is derived from the `workspace` option (`provider_id_for_options`, `crates/providers/slack/src/lib.rs:1492-1526`) and all stored data is keyed by it. Therefore this feature must change only the **display name and avatar**, never the `workspace` option, so the provider ID (and stored chats/messages) stay stable across restarts.

### Assumptions

- "Correct workspace name" means the human workspace name from Slack (`team.info.name`, with `auth.test.team` as a no-extra-call fallback). If the literal domain "erepublik.com" is preferred over a human name, the same code path can select `email_domain`/`domain`; this is a one-line selection choice documented in the plan and easily flipped.
- The connected token grants the `team:read` scope needed for `team.info`. If it does not, the feature degrades gracefully to the existing behavior (manual workspace label, `[SL]` text badge).
- The workspace icon should appear where the workspace identity is shown — primarily replacing the `[SL]` chat badge, and reused in the account switcher and Details pane.
- No manual rename UI is in scope.

## Implementation Plan

### Provider/core: fetch the real name + icon and set the Account

- [ ] Task 1. Add a `team.info` method to the `SlackApiClient` trait (`crates/providers/slack/src/lib.rs:517-588`) returning a small `SlackTeamInfo { id, name, domain, email_domain, icon_url }`, plus the matching response structs (`team.name`, `team.domain`, `team.email_domain`, `team.icon.image_*`). Rationale: this is the authoritative source for both the name and the icon.
- [ ] Task 2. Implement `team.info` on `SlackWebApiClient` modeled on `get_web_api_user_info` (`crates/providers/slack/src/lib.rs:1145-1181`): a `GET https://slack.com/api/team.info` with the bearer credential, decoded into the response struct, choosing the largest reasonable `icon.image_*` URL (and ignoring `image_default` placeholders). Rationale: keeps the new call consistent with existing API plumbing and off the hot path (it already runs via `spawn_blocking`).
- [ ] Task 3. Extend `SlackConnectionState` (`crates/providers/slack/src/lib.rs:167-177`) to also hold the resolved `team_icon_url`/cached icon path. Wire the `team.info` call into the post-validation flow (after credentials validate in `validate_options_with_client`, `crates/providers/slack/src/lib.rs:1854-1923`, or immediately after) so both `connect` and `submit_auth` obtain it. Treat failure as non-fatal (log + continue). Rationale: one fetch point feeds both startup and interactive setup.
- [ ] Task 4. Add a single `account_identity()` helper on `SlackProvider` that builds the Account `display_name` by preferring, in order: the resolved `team.info.name`, then `auth.test` `team_name`, then the manual `workspace` option, then the auth-mode label — and derives the value **only from raw names** so the `Slack (Slack (…))` wrapping cannot recur. Decide and document the final presentation (recommended: `Slack (<real name>)`, or plain `<real name>` if the icon already conveys the platform). Rationale: centralizes the label rule and fixes the double-wrap at its source.
- [ ] Task 5. Set `Account.avatar` from the cached workspace-icon path via `slack_avatar_path(icon_url)` (`crates/providers/slack/src/lib.rs:4073-4128`). Apply both the new display name and the avatar by rebuilding the cached `Account`: in `validate_submission` (replace the `avatar: None` + workspace-only name at `crates/providers/slack/src/lib.rs:1704-1716`) AND in `connect` success (which today does not rebuild the Account at all — `crates/providers/slack/src/lib.rs:2659-2670`). Rationale: ensures the real name + icon appear on every launch, not just during interactive setup; keeps `self.id` unchanged so stored data stays associated.
- [ ] Task 6. Ensure an account-refresh signal reaches the TUI after `connect` (not only `submit_auth`). Reuse the existing `AuthSucceeded`/`SyncComplete` events (`crates/providers/slack/src/lib.rs:2665-2666`) and have the TUI re-read `account_info()` on them (see Task 9). Rationale: the connect path currently leaves the cached account label/icon stale in the sidebar.

### TUI: refresh the displayed name everywhere

- [ ] Task 7. Confirm/adjust that the name surfaces flow from `account_info()`/`account_statuses` so the real name appears in the Chats header, Details `Account:` line, Details `Account filter:` line, and the status bar. These all read from `account_status_summary`/`account_filter_label`, so refreshing the stored `AccountStatus.display_name` updates all four at once. Rationale: no per-surface wiring needed once the Account label is correct.
- [ ] Task 8. Fix the setup-overlay seeding so it never feeds a formatted display name back into the editable workspace/label (the `open_slack_setup_for_account`/`open_slack_setup_for_provider` paths). Rationale: removes the residual double-wrap source if a user re-opens setup.
- [ ] Task 9. On `AuthSucceeded`/`SyncComplete` for a known provider, have the TUI re-read `account_info()` + `config_json()`, `upsert_account`, and refresh the `account_statuses` entry (mirror the post-submit refresh already used after interactive setup). Rationale: propagates the freshly derived name + icon into persisted state and the live sidebar on startup connect.

### TUI: render the workspace icon instead of `[SL]`

- [ ] Task 10. Introduce an "account/workspace badge" rendering helper that prefers the decoded workspace icon (from `Account.avatar`) and falls back to the existing `[SL]` text when the icon is unavailable or not yet decoded. Render it in the chat list where `platform_badge` is used today (`crates/tui/src/widgets/chat_list.rs:504-506`), reusing the half-block decode/cache/queue machinery (`avatar_preview_cache`, `queue_avatar_preview`, `drain_avatar_preview_fetches`, `decode_image_preview_rows`). Keep draw cache/placeholder-only and decode on `spawn_blocking`, per AGENTS.md. Rationale: directly satisfies "use the workspace icon instead of [SL]" while honoring the responsiveness rules.
- [ ] Task 11. Reserve/measure the badge column width for the icon variant and keep the `[SL]` text fallback width identical, so name truncation budgets (`first_prefix_width`, `crates/tui/src/widgets/chat_list.rs:469-474`) stay correct in both states. Rationale: avoids layout shift between placeholder and decoded states.
- [ ] Task 12. Also surface the workspace icon in the account switcher rows (`draw_account_switcher`) and the Details pane `Account:` line, falling back to text when undecoded. Rationale: consistent workspace identity across the surfaces that name the account; reuses the same badge helper.

### Validation

- [ ] Task 13. Provider tests: a mock `team.info` drives `account_info().display_name` to the real team name (no `Slack (Slack (…))`), sets `account_info().avatar` to a cached path, keeps `id()` unchanged, and round-trips through `config_json()`/`with_options()`; `team.info` failure degrades to the prior label and a `None`/text badge without erroring connect.
- [ ] Task 14. TUI tests (two-phase async per AGENTS.md): after a connect/sync with an icon, the chat badge shows a placeholder/text first then the decoded icon after draining the avatar-preview channel; and the header/Details/status surfaces show the real name. Reuse existing patterns (`account_status_summary` assertions, the avatar-preview drain helper).
- [ ] Task 15. Run `test.sh` plus build/lint to confirm no regressions in other providers, the chat-list layout, or the startup reconstruction path.

## Verification Criteria

- On connect, the account label across the Chats header, Details `Account:`, Details `Account filter:`, and the status bar shows the real Slack workspace name (no "Slack (Slack (…))" wrapping), without any manual rename action.
- Each Slack chat row shows the workspace icon in place of `[SL]`; while the icon is still downloading/decoding, the `[SL]` text appears as a placeholder, then is replaced once decoded (no flicker or layout shift).
- The workspace icon also appears in the account switcher and Details pane.
- The provider `id()` is unchanged before/after the refresh, in-session and across restart; stored chats/messages remain associated.
- If `team.info` is unavailable (missing scope/permission), behavior degrades cleanly to the manual workspace label and the `[SL]` text badge, with no errors that break connect.
- Non-Slack providers are unaffected (their badge stays text; `Account.avatar` remains optional).

## Potential Risks and Mitigations

1. **Missing `team:read` scope.** The token may not be allowed to call `team.info`.
   Mitigation: treat the call as best-effort; on failure keep the existing label and `[SL]` text, and surface a non-fatal status. The name fallback chain (auth.test `team` → manual workspace) still improves on today.
2. **Icon rendering in a tiny badge slot.** A 4-cell text badge is small; an icon there may be hard to make legible.
   Mitigation: reuse the proven 4×2 half-block tile sizing from the avatar column; if the inline-badge size proves too small in practice, fall back to the documented alternative of rendering the icon in the avatar column for chats lacking a per-chat image (see Alternatives), keeping `[SL]` text otherwise.
3. **Provider ID instability.** Touching the `workspace` option would re-derive the ID on restart and orphan stored data.
   Mitigation: change only `display_name`/`avatar`; never write the derived name back into `workspace`; keep `self.id`.
4. **Responsiveness / hot-path violations.** Icon download/decode must not block input or draw (AGENTS.md).
   Mitigation: download via `slack_avatar_path`'s background thread; decode via the existing `spawn_blocking` avatar pipeline; draw consults cache only; apply via bounded drain.
5. **Persistence/refresh divergence on startup connect.** The connect path historically did not refresh the Account, so the icon/name could appear only after interactive setup.
   Mitigation: Task 5 + Task 9 rebuild the Account in `connect` success and have the TUI re-read `account_info()` on `AuthSucceeded`/`SyncComplete`.
6. **Name choice ambiguity (human name vs domain "erepublik.com").** Slack exposes `name`, `domain`, and `email_domain`.
   Mitigation: implement the selection in one helper (Task 4) defaulting to the human `name` with documented one-line switches to `domain`/`email_domain`; confirm preference with the user if needed.

## Alternative Approaches

1. **Icon in the avatar column instead of the badge.** Use the workspace icon as the avatar-tile fallback for chats that have no per-chat image (channels), and drop the `[SL]` text. Trade-off: simplest to render (the avatar column already shows images) and very legible, but every channel tile looks identical (loses the per-channel initials BU/I/A/…). Good fallback if the inline badge proves too small.
2. **Name-only, defer the icon.** Ship Task 1–9 (real name + double-wrap fix) first and add the icon rendering (Task 10–12) as a follow-up. Trade-off: delivers the higher-value, lower-risk half immediately; the icon work (terminal image rendering) is the riskier part and can land separately.
3. **No `team.info` call; use `auth.test` only.** Use the `team` name already captured at validation and skip the icon. Trade-off: zero new API surface and no scope concern, but provides no icon and a less complete name than `team.info`.
