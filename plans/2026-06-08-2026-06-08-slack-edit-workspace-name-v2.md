# Edit Slack Workspace Name (v2 — screenshot-informed)

## Objective

Let a user rename a Slack workspace account from inside the TUI in a simple, intuitive way: select the account, type a new clean name, confirm, and see it update immediately across every surface that shows the account — without orphaning stored chats/messages and without re-introducing the double-wrapping bug currently visible in the running app.

## Screenshot Evidence (must-fix context)

A live screenshot of the running TUI shows the account label as **"Slack (Slack (Workspace 1))"** — a double-wrapped name. The same wrong value renders in four places simultaneously:

- Details pane `Account:` line — `crates/tui/src/app.rs:3504` (via `account_status_summary`, `crates/tui/src/app.rs:1856-1858` → `crates/tui/src/app.rs:11486-11500`).
- Details pane `Account filter:` line — `crates/tui/src/app.rs:3505` (via `account_filter_label`, `crates/tui/src/app.rs:9159`).
- Chats pane header ("Account: Slack (…") — top-left title.
- Status bar ("filtered to Slack (Slack (Workspace 1))").

**Root cause.** The only existing rename path is re-running Slack setup, which seeds the editor's `workspace_label` with the already-formatted `account.display_name` rather than the raw workspace: `open_slack_setup_for_account` (`crates/tui/src/app.rs:8806-8814`) and `open_slack_setup_for_provider` (`crates/tui/src/app.rs:8823-8826`). On submit, `validate_submission` re-wraps it as `Slack ({workspace})` (`crates/providers/slack/src/lib.rs:1704-1716`), so "Workspace 1" became "Slack (Workspace 1)" became "Slack (Slack (Workspace 1))", and each re-run wraps again. The new rename feature must both avoid this and clean it up.

## Initial Assessment

### What "workspace name" means today

- The Slack workspace label lives in `SlackProviderOptions.workspace` (`crates/providers/slack/src/lib.rs:58-69`).
- The user-facing label is derived as `Slack (<workspace>)` at construction (`crates/providers/slack/src/lib.rs:1648-1663`) and after auth (`crates/providers/slack/src/lib.rs:1704-1716`).

### The critical constraint (drives the design)

The provider ID is **derived from the workspace name**: `provider_id_for_options` → `slack:<sanitized-workspace>` (`crates/providers/slack/src/lib.rs:1492-1526`). All durable data is keyed by that ID: the `accounts` row, `chats.account_id`, `messages.account_id`, reactions, receipts (`crates/storage/src/schema.rs:2-73`), plus in-memory `account_statuses: HashMap<ProviderId, AccountStatus>` (`crates/tui/src/app.rs:1629`).

`validate_submission` keeps the old ID in-session (`crates/providers/slack/src/lib.rs:1710-1715`), but on restart the ID is **re-derived from the persisted workspace** (`crates/chat-cli/src/main.rs:386-388`), so mutating `workspace` produces a new ID next launch and orphans stored chats/messages. There is no re-key migration today (`stored_id_matches` bookkeeping at `crates/chat-cli/src/main.rs:389-399`).

**Conclusion:** rename a decoupled *display label*; keep `workspace` (and therefore the provider ID and all stored data) stable.

### Reusable UI building blocks

- Modals are `Option<...>` fields on `AppState` (`crates/tui/src/app.rs:1616-1628`), dispatched in an ordered chain in `handle_key` (`crates/tui/src/app.rs:5854-5900`) and drawn in one batch (`crates/tui/src/app.rs:2873-2888`).
- The account switcher lists accounts and already hosts a per-account destructive action (Delete to remove): state `AccountSwitcher` (`crates/tui/src/app.rs:1253-1257`), handler `handle_account_switcher_key` (`crates/tui/src/app.rs:6394-6466`), draw + footer (`crates/tui/src/app.rs:4969-5036`). Natural launch point for rename.
- Single-field text editing already exists as `String` push/pop in the Slack setup handler (`crates/tui/src/app.rs:6142-6240`).
- The post-auth refresh sequence shows how to persist + refresh after a metadata change (`crates/tui/src/app.rs:8758-8766`); because the Details pane, header, status bar, and account-filter all read from `account_statuses`/`account_info`, refreshing those updates every surface at once.

### Assumptions

- "Workspace name" = the user-facing label shown in the Details pane, Chats header, status bar, and account switcher — not the Slack team identity or credentials.
- Renaming must require no network call and must work whether or not the account is currently connected.
- Only Slack needs rename initially; the mechanism stays generic so other providers are unaffected.

## Implementation Plan

### Provider/core: decouple display label from the ID-deriving workspace, and de-wrap

- [ ] Task 1. Add `display_label: Option<String>` to `SlackProviderOptions` (`crates/providers/slack/src/lib.rs:58-69`). It serializes automatically via `config_json()` (`crates/providers/slack/src/lib.rs:2606-2608`) and is restored on startup, so a renamed label persists without touching `workspace` or the provider ID.
- [ ] Task 2. Introduce one display-name helper that prefers a non-empty `display_label`, then falls back to `Slack (<workspace>)`, then `Slack (<auth-mode label>)`. Use it at construction (`crates/providers/slack/src/lib.rs:1648-1663`) and after auth (`crates/providers/slack/src/lib.rs:1704-1716`). Crucially, derive only from the raw `workspace`/`display_label` — never from an already-formatted display name — so the `Slack (Slack (...))` wrapping cannot recur.
- [ ] Task 3. Add a focused rename method to the `Provider` trait, e.g. `async fn set_display_label(&self, label: &str) -> anyhow::Result<()>`, with a default implementation that returns an unsupported result (mirror the optional-capability pattern at `crates/core/src/provider.rs:216-229`). Lets the TUI rename generically through `Arc<dyn Provider>` without downcasting.
- [ ] Task 4. Implement `set_display_label` on `SlackProvider`: trim/validate the label, store it in `options.display_label`, rebuild the cached `Account` display name via the Task 2 helper while preserving `self.id`, and emit a lightweight refresh event (reuse an existing event such as `SyncComplete`/account-updated). No network I/O; the provider ID stays stable so stored data remains associated.
- [ ] Task 5. Fix the existing double-wrap seeding so it cannot persist a formatted name back into the workspace/label: change `open_slack_setup_for_account` (`crates/tui/src/app.rs:8806-8814`) and `open_slack_setup_for_provider` (`crates/tui/src/app.rs:8823-8826`) to seed the editor from the raw workspace/clean label rather than `account.display_name`. This removes the source of "Slack (Slack (Workspace 1))" and aligns the setup flow with the new label rule.

### TUI: rename modal launched from the account switcher

- [ ] Task 6. Add a `rename_account` overlay to `AppState` (new `Option<RenameAccountOverlay { provider_id: ProviderId, value: String }>` near `crates/tui/src/app.rs:1616-1628`) and initialize it in `Default` (`crates/tui/src/app.rs:1656-1723`).
- [ ] Task 7. Add a draw function modeled on `draw_settings_overlay` (`crates/tui/src/app.rs:4843-4901`): a small centered box showing the edited name with a cursor marker and a footer hint (Enter to save, Esc to cancel). Register it in the overlay draw batch (`crates/tui/src/app.rs:2873-2888`).
- [ ] Task 8. Add `handle_rename_account_key` modeled on the Slack setup string editor (`crates/tui/src/app.rs:6142-6240`): printable `Char` appends, `Backspace` pops, `Esc` cancels, `Enter` submits. Insert an early-return branch in `handle_key` (`crates/tui/src/app.rs:5854-5900`) with priority while open.
- [ ] Task 9. In `handle_account_switcher_key` (`crates/tui/src/app.rs:6394-6466`), add a rename trigger key (for example `r` or `F2`) that resolves the selected account's `provider_id` from `account_options()` and opens the rename overlay pre-filled with the current clean name (the `display_label` if set, else the raw `workspace`, NOT the wrapped display name). Ignore the key for the "All accounts"/"Add account" pseudo-entries. Update the switcher footer (`crates/tui/src/app.rs:5015-5018`) to advertise the rename key.

### TUI: submit, persist, and refresh every surface

- [ ] Task 10. On submit, look up the provider via `provider_for_id`/`provider_index_for_id`, call `provider.set_display_label(new_label).await`, then mirror the post-auth refresh: re-read `account_info()` + `config_json()`, `store.upsert_account(...)`, refresh the `account_statuses` entry, and update cached sidebar/account display (model on `crates/tui/src/app.rs:8758-8766`). Because the Details pane (`crates/tui/src/app.rs:3504-3505`), Chats header, status bar, and account filter all read from these, the new name appears everywhere immediately. Set a status confirmation and close the overlay.
- [ ] Task 11. Handle edge cases: reject empty/whitespace-only input (keep old name, show a hint); re-validate the captured `provider_id` still exists before applying (account may have changed while the modal was open); and if a provider returns the unsupported-rename default, surface a clear status rather than an error.

### Validation

- [ ] Task 12. Add/extend tests: (a) provider-level — `set_display_label` updates `account_info().display_name`, keeps `id()` unchanged, round-trips through `config_json()` → `with_options()`, and never produces a `Slack (Slack (...))` double-wrap; (b) TUI — the rename overlay opens from the switcher, accepts typed characters, and on Enter updates the Details pane `Account:`/`Account filter:` lines and the status bar (follow the two-phase async UI test guidance in `AGENTS.md`; reuse patterns near `crates/tui/src/app.rs:12412`, `14025`, `14212`).
- [ ] Task 13. Run `test.sh` plus the standard build/lint to confirm no regressions in other providers or the startup reconstruction path (`crates/chat-cli/src/main.rs:378-402`).

## Verification Criteria

- Selecting a Slack account in the switcher and pressing the rename key opens a single-field editor pre-filled with the current clean name (no `Slack (...)` wrapping shown).
- Entering a new name and pressing Enter immediately updates all four surfaces: Details pane `Account:`, Details pane `Account filter:`, Chats header, and status bar. Esc cancels with no change.
- The resulting label is exactly what the user typed (e.g. "Workspace 1"), never re-wrapped to "Slack (Slack (…))".
- After quit + relaunch, the renamed label persists.
- The renamed account retains all stored chats/messages (`id()` unchanged in-session and across restart).
- Empty/whitespace-only names are rejected without crashing or clearing the existing label.
- Non-Slack providers are unaffected; their default `set_display_label` is a no-op/unsupported status.

## Potential Risks and Mitigations

1. **Orphaned history from ID change.** Renaming the ID-deriving `workspace` would re-derive a new provider ID on restart and orphan stored data.
   Mitigation: rename a decoupled `display_label` only; never mutate `workspace`.
2. **Recurring double-wrap.** Seeding editors from the formatted display name reintroduces "Slack (Slack (…))".
   Mitigation: Task 2 derives the label only from raw `workspace`/`display_label`; Task 5 fixes the buggy seeding in the setup-open paths.
3. **Provider trait expansion affecting all providers.** A new trait method could force changes across every implementation.
   Mitigation: ship with a default implementation returning unsupported; only Slack overrides it.
4. **Stale modal target after account switching.** The selected account could change while the modal is open.
   Mitigation: capture `provider_id` at open time and re-validate before applying.
5. **Persistence/refresh divergence.** Updating memory without persisting (or vice versa) makes the rename appear to revert.
   Mitigation: reuse the single post-auth sequence — `set_display_label` → re-read `account_info`/`config_json` → `upsert_account` → refresh `account_statuses`.
6. **Hot-path/responsiveness violations.** Per `AGENTS.md`, no blocking work on input/draw paths.
   Mitigation: rename does no network I/O; the only async work is the bounded `upsert_account`, performed in the existing async key-handler path like `submit_current_slack_setup`.

## Alternative Approaches

1. **Rename the real workspace identity with a storage migration.** Let the edit change `workspace` (and the ID) and add a migration re-keying `chats`/`messages`/`reactions`/`receipts` and the `accounts` row from old to new `account_id` (analogous to `Store::merge_chat`, `crates/storage/src/lib.rs:376-404`). Trade-off: matches a literal "change the workspace" reading but is heavier and riskier (migration correctness, duplicate-ID handling at `crates/chat-cli/src/main.rs:357-366`); not recommended for a "simple and intuitive" feature.
2. **Reuse the existing Slack setup flow for an existing account.** Open the setup overlay to edit the workspace label. Trade-off: multi-step, re-validates credentials over the network (`validate_submission`), is neither simple nor offline-capable, and is exactly the path that produced the current double-wrap bug.
3. **Stable team-id-based provider ID.** Derive the provider ID from the Slack `team_id` in `SlackValidatedCredential` (`crates/providers/slack/src/lib.rs:118-125`) so `workspace` becomes a pure label. Trade-off: cleanest long term but a larger architectural change with its own migration concerns; a good follow-up to the recommended approach.
