# Edit Slack Workspace Name

## Objective

Let a user rename a Slack workspace account from inside the TUI in a simple, intuitive way: select the account, type a new name, confirm, and see it update immediately and survive restarts — without orphaning the account's stored chats and messages.

## Initial Assessment

### What "workspace name" means today

- The Slack workspace label lives in `SlackProviderOptions.workspace` (`crates/providers/slack/src/lib.rs:58-69`).
- The user-facing account label is derived as `Slack (<workspace>)` in two places: at construction (`crates/providers/slack/src/lib.rs:1648-1663`) and after auth (`crates/providers/slack/src/lib.rs:1704-1716`).
- During first-time setup, the workspace label is typed in the `ChooseWorkspace` phase of the Slack setup overlay (`crates/tui/src/app.rs:772-790`, editing logic at `crates/tui/src/app.rs:6142-6240`, submission at `crates/tui/src/app.rs:8726-8742`).

### The critical constraint (must drive the design)

The provider ID is **derived from the workspace name**: `provider_id_for_options` → `slack:<sanitized-workspace>` (`crates/providers/slack/src/lib.rs:1492-1526`). Everything durable is keyed by that ID: the `accounts` row, `chats.account_id`, `messages.account_id`, reactions, and receipts (`crates/storage/src/schema.rs:2-73`), plus in-memory `account_statuses: HashMap<ProviderId, AccountStatus>` (`crates/tui/src/app.rs:1629`).

`validate_submission` deliberately keeps the old ID in-session (`id: self.id.clone()` at `crates/providers/slack/src/lib.rs:1710-1715`), so a runtime rename does not break continuity while the app is open. But on restart the ID is **re-derived from the persisted workspace** (`crates/chat-cli/src/main.rs:386-388`, `provider_id_for_options`), so changing `workspace` produces a different ID next launch and orphans the previously stored chats/messages under the old ID. The existing `stored_id_matches` bookkeeping (`crates/chat-cli/src/main.rs:389-399`) confirms the authors already know stored IDs and workspace-derived IDs can diverge, and there is no re-key migration today.

**Conclusion:** to rename safely, the display label must be decoupled from the ID-deriving `workspace` field. The recommended approach renames a *display label* while keeping `workspace` (and therefore the provider ID and all stored data) stable.

### Reusable UI building blocks

- All modals are `Option<...>` fields on `AppState` (`crates/tui/src/app.rs:1616-1628`), dispatched by an ordered early-return chain in `handle_key` (`crates/tui/src/app.rs:5854-5900`) and drawn in a single batch (`crates/tui/src/app.rs:2873-2888`).
- The account switcher already lists accounts and supports a destructive action (Delete to remove): state `AccountSwitcher` (`crates/tui/src/app.rs:1253-1257`), handler `handle_account_switcher_key` (`crates/tui/src/app.rs:6394-6466`), draw + footer (`crates/tui/src/app.rs:4969-5036`). This is the natural launch point for a rename.
- Single-field text editing already exists as a hand-rolled `String` push/pop pattern in the Slack setup handler (`crates/tui/src/app.rs:6142-6240`); a small rename modal can mirror it.
- The post-auth refresh sequence shows exactly how to persist and refresh after a metadata change: re-read `account_info()`/`config_json()`, `upsert_account`, update `account_statuses` (`crates/tui/src/app.rs:8758-8766`).

### Assumptions

- "Workspace name" means the user-facing label shown in the account switcher and status bar, not the Slack team identity. The rename changes what the user sees, not the underlying credentials or Slack team.
- Only Slack accounts need rename initially; the mechanism should be generic enough not to break other providers.
- No network call should be required to rename; renaming must work whether or not the account is currently connected.

## Implementation Plan

### Provider/core: decouple display label from the ID-deriving workspace

- [ ] Task 1. Add an optional `display_label: Option<String>` field to `SlackProviderOptions` (`crates/providers/slack/src/lib.rs:58-69`). Rationale: it serializes automatically via the existing `config_json()` (`crates/providers/slack/src/lib.rs:2606-2608`) and is restored on startup, so a renamed label persists without touching `workspace` or the provider ID.
- [ ] Task 2. Introduce a single display-name helper that prefers `display_label` (when non-empty), then falls back to `Slack (<workspace>)`, then `Slack (<auth-mode label>)`. Use it at construction (`crates/providers/slack/src/lib.rs:1648-1663`) and after auth (`crates/providers/slack/src/lib.rs:1704-1716`). Rationale: centralizes the label rule so a custom name always wins consistently, including across restarts.
- [ ] Task 3. Add a focused rename method on the `Provider` trait, e.g. `async fn set_display_label(&self, label: &str) -> anyhow::Result<()>`, with a default implementation that bails as unsupported (mirror the existing optional-capability pattern at `crates/core/src/provider.rs:216-229`). Rationale: lets the TUI rename generically through `Arc<dyn Provider>` without downcasting, and other providers can opt in later.
- [ ] Task 4. Implement `set_display_label` on `SlackProvider`: trim/validate the new label, update `options.display_label`, rebuild the cached `Account` display name via the Task 2 helper while preserving `self.id`, and emit a lightweight provider event so the UI can refresh (reuse an existing event such as `SyncComplete`/account-updated rather than inventing a new pipeline). Rationale: keeps the provider ID stable, so all stored chats/messages stay associated; no network I/O is performed.

### TUI: rename modal launched from the account switcher

- [ ] Task 5. Add a `rename_account` overlay state to `AppState` (new `Option<RenameAccountOverlay { provider_id: ProviderId, value: String }>` near the other overlay fields at `crates/tui/src/app.rs:1616-1628`) and initialize it in `Default` (`crates/tui/src/app.rs:1656-1723`). Rationale: follows the established one-`Option`-per-modal convention.
- [ ] Task 6. Add a draw function for the rename overlay modeled on `draw_settings_overlay` (`crates/tui/src/app.rs:4843-4901`): a small centered box showing the current/edited name with a cursor marker and a footer hint (Enter to save, Esc to cancel). Register it in the overlay draw batch (`crates/tui/src/app.rs:2873-2888`). Rationale: consistent look-and-feel with existing modals.
- [ ] Task 7. Add `handle_rename_account_key` modeled on the Slack setup string editor (`crates/tui/src/app.rs:6142-6240`): printable `Char` appends, `Backspace` pops, `Esc` cancels, `Enter` submits. Insert an early-return branch for it in `handle_key` (`crates/tui/src/app.rs:5854-5900`), placed so it has priority while open. Rationale: reuses the proven single-field text-entry idiom.
- [ ] Task 8. In `handle_account_switcher_key` (`crates/tui/src/app.rs:6394-6466`), add a rename trigger key (for example `r` or `F2`) that resolves the selected account's `provider_id` from `account_options()` and opens the rename overlay pre-filled with the current display name; ignore the key for the "All accounts"/"Add account" pseudo-entries. Update the switcher footer text (`crates/tui/src/app.rs:5015-5018`) to advertise the rename key. Rationale: the switcher is the discoverable, intuitive home for per-account actions and already hosts Delete.

### TUI: submit, persist, and refresh

- [ ] Task 9. On rename submit, look up the provider via `provider_for_id`/`provider_index_for_id`, call `provider.set_display_label(new_label).await`, then mirror the post-auth refresh: re-read `account_info()` + `config_json()`, call `store.upsert_account(...)`, refresh the `account_statuses` entry, and update any cached sidebar/account display so the new name appears immediately (model on `crates/tui/src/app.rs:8758-8766`). Set a status-line confirmation and close the overlay. Rationale: guarantees the new name is both visible instantly and durably persisted under the unchanged provider ID.
- [ ] Task 10. Handle edge cases explicitly: empty/whitespace-only input is rejected (keep the old name and show a status hint), the selected account may have changed while the modal was open (re-validate `provider_id` before applying), and a provider that returns the unsupported-rename default surfaces a clear status message rather than an error. Rationale: matches the repository's stale-completion and graceful-degradation expectations.

### Validation

- [ ] Task 11. Add/extend tests: (a) a provider-level test that `set_display_label` updates `account_info().display_name`, keeps `id()` unchanged, and round-trips through `config_json()` → `with_options()`; (b) a TUI test asserting the rename overlay opens from the switcher, accepts typed characters, and on Enter updates the visible account label (follow the existing two-phase async UI test guidance in `AGENTS.md` and patterns near `crates/tui/src/app.rs:14025`, `14212`). Rationale: locks in both the persistence contract and the interactive behavior.
- [ ] Task 12. Run the workspace test script (`test.sh`) and the standard build/lint to confirm no regressions in other providers or the startup reconstruction path (`crates/chat-cli/src/main.rs:378-402`). Rationale: the trait change and label-helper touch shared code paths that must remain green.

## Verification Criteria

- Selecting a Slack account in the account switcher and pressing the rename key opens a single-field editor pre-filled with the current name.
- Typing a new name and pressing Enter immediately updates the account label shown in the switcher and status bar; Esc cancels with no change.
- After quitting and relaunching the app, the renamed label persists.
- The renamed account retains all its previously stored chats and messages (the provider `id()` is unchanged before and after rename, in-session and across restart).
- Empty or whitespace-only names are rejected without crashing or clearing the existing label.
- Non-Slack providers are unaffected; their default `set_display_label` is a no-op/clear unsupported status, and existing tests remain green.

## Potential Risks and Mitigations

1. **Orphaned history from ID change.** Renaming the ID-deriving `workspace` field would re-derive a new provider ID on restart and orphan stored chats/messages.
   Mitigation: rename a decoupled `display_label` only; never mutate `workspace`, so the provider ID and all storage keys stay stable.
2. **Provider trait expansion affecting all providers.** Adding a trait method could force changes across every provider implementation.
   Mitigation: ship it with a default implementation that returns an unsupported result, so only Slack overrides it and other providers are untouched.
3. **Stale modal target after account switching.** The selected account could change while the rename modal is open.
   Mitigation: capture the `provider_id` at open time and re-validate it still exists before applying; otherwise abort with a status message.
4. **Persistence/refresh divergence.** Updating in-memory state without persisting (or vice versa) would make the rename appear to revert.
   Mitigation: reuse the established post-auth sequence — `set_display_label` → re-read `account_info`/`config_json` → `upsert_account` → refresh `account_statuses`/sidebar — in one path.
5. **Hot-path/responsiveness violations.** Per `AGENTS.md`, no blocking work belongs on input/draw paths.
   Mitigation: the rename does no network I/O; the only async work is the bounded `upsert_account`, performed in the existing async key-handler path exactly like `submit_current_slack_setup`.

## Alternative Approaches

1. **Rename the real workspace identity with a storage migration.** Let the edit change `workspace` (and thus the ID), and add a startup/runtime migration that re-keys `chats`/`messages`/`reactions`/`receipts` and the `accounts` row from the old `account_id` to the new derived ID (analogous to `Store::merge_chat` at `crates/storage/src/lib.rs:376-404`). Trade-off: matches a literal "change the workspace" interpretation but is significantly heavier and riskier (data-migration correctness, duplicate-ID handling at `crates/chat-cli/src/main.rs:357-366`), so it is not recommended for a "simple and intuitive" feature.
2. **Reuse the existing Slack setup `ChooseWorkspace` flow for an existing account.** Open the setup overlay (`open_slack_setup_for_provider`, `crates/tui/src/app.rs:8816-8854`) to edit the workspace label. Trade-off: it is a multi-step flow that re-validates credentials over the network (`validate_submission`), so it is neither simple nor offline-capable, and it still changes the ID on restart unless combined with Alternative 1.
3. **Stable team-id-based provider ID.** Derive the provider ID from the Slack `team_id` captured in `SlackValidatedCredential` (`crates/providers/slack/src/lib.rs:118-125`) so the `workspace` field becomes a pure label. Trade-off: the cleanest long-term fix but a larger architectural change with its own migration concerns for existing accounts; can be a follow-up to the recommended approach.
