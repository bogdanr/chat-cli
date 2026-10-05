# Composer selection toolbar, chat-search exit, and duplicate-name mentions

This plan replaces `plans/2026-09-30-composer-select-undo-and-formatting-v1.md`.

## Objective

1. **Composer:** add only two shortcuts, Ctrl+A (select all) and Ctrl+B (bold). While text is selected, a small toolbar of buttons appears: **B**, *I*, ~~S~~ and `code`. Formatting arrives correctly on WhatsApp, Slack and ClickUp.
2. **Chat search (Ctrl+F):** leaving the chat list ends the search. The highlighted chat stays selected and opens, and the Messages pane is never left in search mode by accident.
3. **Mentions:** `@Bogdan` in a WhatsApp group with two people named Bogdan (you and someone else) must mention the other Bogdan, never you.

## Findings (evidence)

### Composer
- Every Ctrl/Alt character key is dropped except Ctrl+U (`crates/tui/src/app.rs:12860-12863`, `crates/tui/src/app.rs:12964-12974`). Editing goes through `input_without_shortcuts` (`crates/tui/src/app.rs:13087-13091`).
- Ctrl+A is free inside the composer, because the global Ctrl+A only applies outside it (`crates/tui/src/app.rs:10720`). Ctrl+B is unused.
- A paste is inserted one character at a time, with a cache sync after each character (`crates/tui/src/app.rs:12451-12460`).
- `ratatui-textarea` 0.8 already provides `select_all`, `selection_range`, `cancel_selection`, `cut`, `insert_str` and `set_selection_style`.
- Received messages are rendered with `**bold**`, `_italic_`/`*italic*`, `~~strike~~` and `` `code` `` (`crates/tui/src/widgets/message_list.rs:4326-4356`).
- Slack inbound text is already translated into that syntax (`crates/providers/slack/src/lib.rs:6817-6870`). WhatsApp inbound is not. ClickUp is markdown in both directions (`crates/providers/clickup/src/api.rs:635`). Outbound text is never converted.

### Chat search
- In search mode, Left/Right move focus and then call `sync_filter_scope_to_focus`, which keeps search mode on and switches to searching messages (`crates/tui/src/app.rs:12826-12835`, `crates/tui/src/app.rs:12589-12604`).
- Clicking another pane does the same (`crates/tui/src/app.rs:11813-11816`).
- The chat is only confirmed on Enter (`crates/tui/src/app.rs:12672-12685`). That is why letters typed in the right pane keep searching instead of starting a message.

### Mentions
- The picker removes duplicate names, ignoring case, so only one "Bogdan" is ever offered (`crates/tui/src/app.rs:13176-13183`).
- Picking inserts plain `@Bogdan` text without remembering which person was chosen (`crates/tui/src/app.rs:13317`).
- At send time, `resolve_mention_tokens` looks the name up again in the member list and takes the first member whose name matches (`crates/core/src/provider.rs:246`). The sort by name length keeps the member-list order for equal names, so whichever Bogdan comes first wins, and here that is you.
- There is no "this member is me" flag. `ChatMember` has only `sender` and `role` (`crates/core/src/types.rs:292-295`), and `Account` has no own user ID (`crates/core/src/types.rs:98-103`).
- The WhatsApp bridge knows your JID (`crates/providers/whatsapp/go/bridge.go:1159-1160`). Group members are built in `groupMembers` (`crates/providers/whatsapp/go/bridge.go:951-990`), where your entry could be flagged.

## Decisions and assumptions

- **New shortcuts:** only Ctrl+A (select all) and Ctrl+B (bold).
  - No undo, and no italic/strike/code shortcuts; those are toolbar buttons only.
  - Shift+Arrow/Shift+Home/Shift+End selection also works. It is standard text-box behaviour rather than a new shortcut, and without it the toolbar would appear only after Ctrl+A.
  - Selecting by dragging the mouse in the composer is optional.
- **Selection behaviour:**
  - Backspace/Delete removes the selection, and typing or pasting replaces it.
  - Plain arrows, Esc and focus changes cancel the selection. Esc cancels the selection first, before its usual actions.
- **The toolbar:**
  - It shows `[B] [I] [S] [</>]` on the composer's top border, and only while a selection exists.
  - It is mouse-clickable. The buttons toggle: they wrap the selection, or remove the markers if the selection is already wrapped.
  - Ctrl+B without a selection inserts `****` with the cursor in the middle.
- **One composer syntax** (`**b**`, `_i_`, `~~s~~`, `` `c` ``), converted on the wire only:
  - Slack and WhatsApp: `**`→`*` and `~~`→`~`. Nothing inside code is converted.
  - ClickUp: sent unchanged.
  - WhatsApp inbound `*b*`/`~s~` is converted back to the composer syntax.
- **Leaving chat search:**
  - Moving focus out of the chat list while searching (Left/Right or a mouse click) clears the chat search and exits search mode. The highlighted chat stays selected, is scrolled into view in the full list, and its messages load in the background.
  - If no chat matches but a discovery result exists, it opens that result, as Enter does.
  - Esc keeps its current meaning: clear and stay.
- **Mentions:**
  - You are never offered in the picker.
  - Members who share a name appear as separate entries with a short hint (for example, the last 4 digits of the phone number).
  - The person you pick is remembered for that token. Typed names that match several people prefer someone other than you.

## Implementation Plan

### Part A: Composer selection and formatting
- [x] 1. **Ctrl+A select all.** In `handle_compose_key`, before the catch-all `Char` arm, call `select_all`, sync the compose cache, and set the status "text selected — Backspace deletes, use the buttons to format". Rationale: this is the main way to remove a large paste.
- [x] 2. **Shift-selection and selection-aware editing.**
  - Pass Shift+Arrow/Home/End to the textarea so it starts or extends a selection.
  - With a selection active, Backspace/Delete removes it (restore the textarea's yank buffer afterwards so no hidden clipboard state is left), and typed characters or a paste replace it.
  - Plain navigation and Esc cancel it. Existing edge-of-pane focus moves are kept, but a pane change cancels the selection first.
- [x] 3. **Paste as one insert.** Replace the per-character paste loop with one `insert_str`. Normalise line endings first, remove any active selection, then run a single cache sync and completion refresh. Add the opt-in perf label `compose_paste` with the character count. Rationale: large pastes become fast, and paste-over-selection works.
- [x] 4. **Pure formatting helper.** Add a function that takes the text, the selection or cursor, and a format kind (Bold, Italic, Strike, Code) and returns the new text and cursor. It handles wrap, insert-pair and toggle-off. Keep it free of App state so it can be unit tested. Apply the result to the textarea in one operation.
- [x] 5. **Ctrl+B.** Bind it to the helper with Bold.
- [x] 6. **Selection toolbar.**
  - Draw: while `compose.is_selecting()` is true, draw the four buttons right-aligned on the composer's top border (on narrow terminals the row may shrink to single-letter buttons). Store their rectangles in draw-time state, as the attachment tray does. The draw path stays cache-only.
  - Clicks: check them in the mouse handler before pane focus changes (next to `handle_compose_attachment_click`, `crates/tui/src/app.rs:11802-11807`). A click applies the format and keeps the result selected so buttons can be combined.
  - Selection style: theme-derived.
- [-] 7. (Skipped: Ctrl+A and Shift+Arrows cover selection.) **(Optional) Mouse drag selection** in the composer. Map mouse down/drag/up inside the composer to textarea cursor positions. Skip it if the textarea can't map screen positions reliably after wrapping. Shift+Arrows and Ctrl+A are enough.
- [x] 8. **Shared converter in `chat_core`.**
  - Add `markdown_to_chat_markup` (Slack/WhatsApp outbound) and `whatsapp_markup_to_markdown` (WhatsApp inbound).
  - Both use word-boundary delimiter rules and skip code spans, fenced blocks, URLs and `<...>` tokens.
  - Reuse Slack's existing code-span-aware helpers instead of duplicating them.
- [x] 9. **Wire conversion.**
  - Slack and WhatsApp: convert in `send` and `edit_message`, before mention encoding.
  - WhatsApp: convert incoming live and history messages, and incoming edits, back to the composer syntax.
  - ClickUp: unchanged, but add a test.
  - Stored local messages keep the composer syntax, so starting an edit shows the same markers.
- [x] 10. **Help text.** Add Ctrl+A, Ctrl+B, Shift+Arrows and the toolbar to the Compose section (`crates/tui/src/app.rs:8419-8431`).

### Part B: Chat search ends when leaving the chat list
- [x] 11. **`leave_chat_search` helper.** It runs only when `filter_mode` is on and `filter_scope == Chats`. It must never touch the database or network directly.
  - (a) If no chat is visible and a discovery result exists, open it through the existing `open_discovery_result` path.
  - (b) Otherwise, remember the highlighted chat's key (account and chat ID), clear the chat query, turn off `filter_mode`, and re-apply the filter.
  - (c) Put the selection back on the remembered chat by key, since its row index changes when the list is no longer filtered, and keep it in view.
  - (d) Report "selection changed", so the existing async message load runs with its generation-token check.
  - (e) Set the status to "opened <chat name>".
- [x] 12. **Call it on every way out of the chat list:**
  - Left/Right in `handle_filter_key`, replacing the `sync_filter_scope_to_focus` hand-off for the Chats scope.
  - A mouse click on another pane, before `sync_filter_scope_to_focus` (`crates/tui/src/app.rs:11813-11816`).
  - Any other focus-change helper used while in search mode.
  - Searching messages is still possible by pressing Ctrl+F while the Messages pane is focused.
- [x] 13. **Check how selection behaves when the filter is cleared.** Confirm that `apply_filter`/`clear_filter` don't reset the selection to the top when the query becomes empty. If they do, the helper restores it (step 11c). Selection and scroll must not jump when the background load finishes (AGENTS.md).

### Part C: Mentions with duplicate names
- [x] 14. **Self flag on members (core).** Add `is_self: bool` to `ChatMember`, defaulting to false in `new`/`with_role`, plus an `as_self()` builder. Update any struct literals.
- [x] 15. **WhatsApp marks you.** In `groupMembers`, set `IsSelf` when the participant's user part matches `Store.ID` or `Store.LID` (so both phone-number and LID groups are covered). Map it in the Rust member conversion. Where Slack and ClickUp already know the signed-in user ID, set the flag in their member lists too; otherwise leave it false.
- [x] 16. **Picker keeps people separate.** Change `ComposeMentionCandidates.names` into a list of entries, each with the display name, `PlatformId` and an optional disambiguation hint.
  - Remove duplicate *members* by platform ID, not by name.
  - Leave out `is_self` members.
  - When two entries share a name, add a hint: the last 4 digits of the phone number for WhatsApp, or the handle/ID tail elsewhere.
  - Keep the cache keyed on roster changes, so there is no per-keystroke cost.
- [x] 17. **Remember the pick.**
  - On insert, add the picked `Mention` (name and platform ID) to a new `compose_mention_picks` list. The inserted text stays `@Bogdan `, without the hint.
  - Clear the list whenever the composer is reset: send, edit start or cancel, reply cancel, or chat switch.
  - Do the same for the thread composer.
- [x] 18. **Prefer the right person when resolving.**
  - Add a core function (or an optional `preferred` argument on `resolve_mention_tokens`): when a token's name matches several members, use the first unused pick with that name, then any member who is not you, and only then you.
  - Use it in `encode_outbound_mentions` for WhatsApp, Slack, ClickUp and the mock provider. Add a default trait method so the four implementations change only slightly.
  - Pass the picks from all three send paths: main composer, thread composer (`crates/tui/src/app.rs:13381-13382`) and edit.
- [x] 19. **Refresh the roster when needed.** If an existing cached roster has no `is_self` member for a WhatsApp group, run the usual bounded background member refresh once, so old caches pick up the new flag without blocking input.

### Part D: Tests and validation
- [x] 20. **Composer tests:**
  - Ctrl+A then Backspace empties the draft.
  - A paste replaces a selection.
  - Shift+Right selects, and the toolbar appears. The toolbar is hidden when nothing is selected.
  - Clicking each button wraps the selection, and clicking again removes the markers.
  - Ctrl+B wraps a selection, or inserts the marker pair without one.
  - A large paste is one insert.
  - Ctrl+A outside the composer still opens the account switcher.
- [x] 21. **Converter tests:** nesting, code spans and fences, URLs with `_`/`*`, `2*3*4`, mentions, multi-line text, round-trip stability, and a check of the wire text for Slack, WhatsApp and ClickUp.
- [x] 22. **Chat search tests:**
  - Ctrl+F, type, press Right: search mode is off, the chat query is empty, and the highlighted chat is selected. Messages show the loading state immediately, then the final messages after background work finishes.
  - A letter typed in Messages now starts a reply.
  - The same holds for a mouse click on the Messages pane.
  - With no matches, the discovery result is opened.
  - Esc still only clears.
  - Ctrl+F in the Messages pane still searches messages.
- [x] 23. **Mention tests:**
  - Core resolver: two members named "Bogdan", one of them you. A typed `@Bogdan` resolves to the other. A pick resolves to the picked person. Two different same-name picks in one message resolve in order.
  - TUI: the picker never lists you, and shows hints for same-name members. After picking and sending, the mock provider's `mentioned` is the other Bogdan.
  - The thread composer and edit paths are covered.
  - A Go bridge test for `IsSelf` covers both the phone-number and LID cases.
- [x] 24. **CI-equivalent run:** inspect `.github/workflows/ci.yml`, then run `cargo check --workspace --all-targets`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --release -p chat-cli`, plus `go vet`/`go test` for the bridge.

## Verification Criteria

- Only Ctrl+A and Ctrl+B are new key bindings. The help overlay lists them and the toolbar.
- After selecting text, four buttons appear on the composer border. Clicking **B** on "hello" gives `**hello**`, which arrives bold on WhatsApp and Slack (sent as `*hello*`) and on ClickUp (sent as `**hello**`).
- Ctrl+A then Backspace clears a 10,000-character paste in two keypresses.
- After Ctrl+F, typing a name and pressing Right (or clicking the messages), search mode is off, that chat is open, and typing starts a message.
- In the WhatsApp "Vin" group, `@Bogdan` (picked or typed) sends `MentionedJID` for the other Bogdan, and the picker never offers you.
- All existing tests and CI steps pass.

## Potential Risks and Mitigations

1. **The terminal may not report Shift+Arrow** (some terminals send the same code as a plain arrow).
   Mitigation: Ctrl+A always works, so the toolbar is still reachable. Mouse drag is available if step 7 is built.
2. **The toolbar overlaps the composer border title or the attachment tray.**
   Mitigation: draw it only while a selection exists, right-aligned, with a single-letter fallback on narrow terminals. Draw and hit-test use the same stored rectangles.
3. **Clearing the chat filter moves the selection or the list scroll.**
   Mitigation: restore the selection by chat key (not by index), and check this in a test after the background completions are drained.
4. **Your own JID has several forms** (phone number, LID, with or without device suffix).
   Mitigation: compare only the user part, without the device suffix, against both `Store.ID` and `Store.LID`, with Go tests for each form.
5. **Stale picks** (the user edits the text after picking).
   Mitigation: a pick only changes the result for a token with the same name. Otherwise name resolution with the you-last rule applies, so the worst case is today's behaviour minus mentioning yourself.
6. **Wrong formatting conversion of ordinary text.**
   Mitigation: delimiter rules that need word boundaries, skipping code and URLs, and a broad set of test cases.

## Alternative Approaches

1. **Insert a unique token for duplicate names** (such as `@Bogdan·1234`): the choice is always clear, but the recipient sees odd text unless the provider rewrites it. Picks plus you-last give clean text.
2. **Remove yourself from the member list entirely** (TUI-only, no core flag): quicker, but it needs the own JID in the TUI anyway, and it also removes the ability to mention yourself on purpose.
3. **Keep search mode but just stop it following focus:** smaller change, but the user asked to cancel the search and open the chat, which step 11 does directly.
4. **Keyboard access to the toolbar** (such as Tab to cycle buttons while selecting): more keys, which the user explicitly declined. The toolbar stays mouse-only apart from Ctrl+B.
