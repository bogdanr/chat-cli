# Edit Sent Messages Across All Supported Providers

## Objective

Let users edit the text of messages they sent, from the TUI, on every real provider: WhatsApp, Slack and ClickUp. The Mock/Demo provider also gets editing so the feature can be demoed and tested. Edits made on other clients (phone, web, other devices) must also show up correctly, with a visible "(edited)" marker.

Expected outcomes:
- Selecting one of your own text messages and choosing **Edit** opens the composer with the original text and an "Editing · Esc cancels" banner. Enter saves the edit to the provider.
- Edits are sent off the event loop, applied to storage idempotently, and shown with an "(edited)" marker.
- Edits made elsewhere reach the timeline for WhatsApp (live and history sync), Slack (Socket Mode `message_changed` and history) and ClickUp (poll-detected updates).

## Current State (research findings)

| Layer | Status | Evidence |
|---|---|---|
| Model and schema | Ready. `Message.edited_at` exists and is persisted | `crates/core/src/types.rs:228-250`, `crates/storage/src/schema.rs:30-57`, `crates/storage/src/lib.rs:2214-2296` |
| Event | `ProviderEvent::MessageEdited { message }` exists, but it is a catch-all "message changed" event. WhatsApp reactions and poll votes also emit it | `crates/core/src/events.rs:25-97`, `crates/providers/whatsapp/src/lib.rs:716-718`, `crates/core/src/mock.rs:280-281` |
| TUI inbound | Handles `MessageEdited` (upsert, preview, reload) with perf labels | `crates/tui/src/app.rs:9825-9863` |
| Outbound trait | No edit method. The optional-capability pattern already exists (`vote_poll` default `bail!`) | `crates/core/src/provider.rs:386-394` |
| Capabilities | `OutboundCapabilities` gates UI features. There is no edit flag | `crates/core/src/provider.rs:46-130` |
| TUI actions | Action menu is built per message; Reply is the closest template, React is the template for applying results | `crates/tui/src/app.rs:20513-20544`, `crates/tui/src/app.rs:15165-15173`, `crates/tui/src/app.rs:5575-5606`, `crates/tui/src/app.rs:15591-15643` |
| Background pattern | Forward sends run in `tokio::spawn`, and results are drained in bounded batches with perf labels | `crates/tui/src/app.rs:15295-15313`, `crates/tui/src/app.rs:15379-15420` |
| Rendering | `edited_at` only feeds the layout cache hash. No marker is drawn | `crates/tui/src/widgets/message_list.rs:825-828` |
| WhatsApp outbound | Only `C_SendText`. The pinned whatsmeow has `BuildEdit` and `EditWindow = 20 * time.Minute` | `crates/providers/whatsapp/go/bridge.go:453-509`, whatsmeow `send.go:584-610` |
| WhatsApp inbound (live) | A live edit unwraps to a `ProtocolMessage{MESSAGE_EDIT}`, becomes "[unsupported WhatsApp message]", and is dropped | `crates/providers/whatsapp/go/bridge.go:1531-1555`, `crates/providers/whatsapp/go/bridge.go:1654-1656`, whatsmeow `events.go:407-410` |
| WhatsApp inbound (history) | `ParseWebMessage` rewrites the ID to the original message and the payload to the new content, so the edit is re-emitted as a normal message carrying the *edit's* timestamp and no `edited_at` | whatsmeow `client.go:997-1001`, `crates/providers/whatsapp/src/lib.rs:1532` |
| Slack | `message_changed` becomes `MessageEdited`, but `edited_at` is always `None`. There is no `chat.update` client method | `crates/providers/slack/src/lib.rs:5769-5795`, `crates/providers/slack/src/lib.rs:6048`, `crates/providers/slack/src/lib.rs:913-1019` |
| ClickUp | `edited_at` comes from `date_updated > date`. The poller ignores changes to messages it has already seen. The v3 API has `PATCH /api/v3/workspaces/{workspace_id}/chat/messages/{message_id}` (body: `content`, `content_format` of `text/md` or `text/plain`) | `crates/providers/clickup/src/convert.rs:470-473`, `crates/providers/clickup/src/lib.rs:1214-1228`, `crates/providers/clickup/src/http.rs:243` |

### Prioritised challenges

1. **WhatsApp inbound edits are silently lost.** This is the most user-visible gap, and it needs work in both Go and Rust.
2. **Keeping the event loop responsive** (AGENTS.md). Today's send path awaits network calls inside key handling. Edit must not copy that; it should use the forward-style background task instead.
3. **Mistaking reactions and polls for edits.** `MessageEdited` is overloaded, so the marker must come only from `edited_at`, and text edits need a precise signal.
4. **Stale or out-of-order edits** (multi-device, history replay). Applying edits must be idempotent and monotonic by `edited_at`.
5. **Per-provider rules:** WhatsApp's 20-minute window, Slack webhook identities that can't edit, and Slack bot tokens that can only edit the bot's own messages.

## Assumptions

- Scope is **text edits of your own messages** (`is_from_me == true`) where the content is `Content::Text`, or media captions where the provider supports them (see Alternatives). Deleting/revoking is out of scope, although `MessageDeleted` stays wired as-is.
- The edit is available in the main timeline and in the thread view composer, following the reply scoping rules.
- Mentions in edited text reuse `encode_outbound_mentions`, so editing keeps the same mention behaviour as sending.
- The action menu is the entry point. Plain characters in the Messages pane jump to the composer (`crates/tui/src/app.rs:10677-10687`), so no single-letter shortcut is added there.
- Discord is enum-only and unaffected.

## Implementation Plan

### Phase 1: Core contract

- [x] 1. **Add an edit capability to `OutboundCapabilities`** (`crates/core/src/provider.rs:46-101`): an `edit: bool` flag, default false and true in `all()`, plus an optional `edit_window` duration. Rationale: the UI already gates features on this struct, so the Edit action can be hidden cheaply without calling the provider.
- [x] 2. **Add `Provider::edit_message`** next to `vote_poll`. It takes the chat id, the full original `&Message` (providers need `platform_data` such as Slack `ts`/`channel`, ClickUp `workspace_id`/`message_id`, or the WhatsApp jid) and an `OutboundContent`. It returns the provider-confirmed edit timestamp. The default `bail!`s with "editing is not supported by this provider". Rationale: this matches the existing optional-capability pattern and doesn't break any other implementer.
- [x] 3. **Add a shared eligibility helper** in core, for example `can_edit(capabilities, message, now)`. It is true only when the capability is set, `is_from_me` is true, the content is editable text, the content isn't `Deleted`, and the message is within the edit window if there is one. Rationale: the action menu, the Up-arrow shortcut and provider-side validation all use one rule, and it is unit-testable in isolation.
- [x] 4. **Add a precise text-edit event**, `ProviderEvent::MessageContentEdited { chat_id, message_id, content, edited_at }`, next to `MessageEdited` in `crates/core/src/events.rs`. Also register its label in the TUI event-kind mapping (`crates/tui/src/app.rs:19346`). Rationale: WhatsApp and ClickUp edit notifications carry only the target id and new content, not a full `Message`. Rebuilding a full message inside providers risks clobbering reactions and receipts. It also separates real edits from reaction and poll changes.

### Phase 2: Storage

- [x] 5. **Add `Store::apply_message_edit(account, chat_id, message_id, content, edited_at)`** in `crates/storage/src/lib.rs`. It is a single targeted UPDATE of the content columns and `edited_at`. It applies only when the stored `edited_at` is null or older (monotonic), leaves reaction and receipt rows alone, and returns whether a row changed. Rationale: `upsert_message_on_conn` rewrites reactions and receipts (`crates/storage/src/lib.rs:2214-2296`). A narrow UPDATE is bounded, idempotent during replay, and resistant to out-of-order edits.
- [x] 6. **Storage tests:** the edit updates text and `edited_at`; a missing row is a no-op; an older edit doesn't overwrite a newer one; reactions and receipts survive; the latest-message preview query sees the new text.

### Phase 3: TUI

- [x] 7. **Edit state:** add an editing target (account, chat id, message id, original text, and the scope: main or thread) to the app state, next to `reply_to`. It is mutually exclusive with reply, so starting one clears the other. Rationale: this mirrors the proven reply lifecycle (`crates/tui/src/app.rs:15165-15173`, `crates/tui/src/app.rs:12790-12791`).
- [x] 8. **Add an `ActionMenuItem::Edit` item and its label** (`crates/tui/src/app.rs:1172-1205`). Include it in `action_menu_items_for_message` (`crates/tui/src/app.rs:20513-20544`) only when the eligibility helper passes for that message's provider. Dispatch it in `perform_action_menu_item` (`crates/tui/src/app.rs:10484-10543`) to a `start_edit` function that fills the composer with the original text (mentions shown in their display form), puts the cursor at the end and focuses the composer. Rationale: the action is discoverable and only appears where it can succeed.
- [x] 9. **Composer banner:** extend `draw_compose` (`crates/tui/src/app.rs:5575-5606`) to show "Editing · Esc cancels", styled differently from the reply banner. Esc cancels the edit and restores the draft that was in the composer before editing started, so the user doesn't lose unsent text. Draw stays cache-only.
- [x] 10. **Submit path:** when Enter is pressed with an edit target, branch before the normal send in `send_composed_message` (`crates/tui/src/app.rs:13344-13540`). If the text is unchanged, cancel without sending. If it is empty, show a status hint and don't send; deleting is out of scope. Otherwise encode mentions, re-check eligibility (the edit window may have expired) and spawn a background task. Model this on the forward sender (`crates/tui/src/app.rs:15295-15313`): a new `edit_send_tx/rx` channel and a result struct carrying the target, the outcome and the elapsed time. Immediately set the status to "saving edit…", clear the composer and edit state, and keep the selection and scroll position. Rationale: AGENTS.md forbids network calls on the input path.
- [x] 11. **Draining completions:** add `drain_edit_sends`, bounded by `MAX_COMPLETION_EVENTS_PER_DRAIN` and `COMPLETION_DRAIN_BUDGET` like `drain_forward_sends` (`crates/tui/src/app.rs:15379-15420`), and call it wherever the forward drain is called. On success, call `apply_message_edit` and refresh the chat preview only if this was the latest message. Invalidate the layout cache for that message, and reload the view only if the same account and chat are still selected; otherwise just persist. On failure, show a clear status, for example "edit failed: <reason>" or "edit window expired". Perf labels: `edit_send.complete` (account, chat, result) and `edit_send.drain` (count, errors, stale, budget_exhausted).
- [x] 12. **Handle `MessageContentEdited`** in `handle_provider_event` next to `MessageEdited` (`crates/tui/src/app.rs:9825-9863`). It calls the same store edit plus a conditional preview refresh and reload, and never reloads chats for unknown ids. Perf labels: `provider.message_content_edited.store`, `.preview` and `.reload_selected`, including a changed/no-op count. If you are editing a message and an external edit arrives for it, keep your draft and show a status note.
- [x] 13. **"(edited)" marker** in `crates/tui/src/widgets/message_list.rs`: render a dim "(edited)" beside the timestamp or metadata when `edited_at` is set. Update the draw pass and the line-count/measure pass together (the lesson from `plans/2026-06-15-whatsapp-reply-quote-block-v1.md`). The cache key already includes `edited_at` (`crates/tui/src/widgets/message_list.rs:825-828`). Optionally show the edit time in the Details pane.
- [-] 14. **(Dropped: Up on the composer's first line already moves focus to Messages.) Up-arrow shortcut:** with the composer focused, empty, and no reply, edit, mention autocomplete or emoji popup active, Up starts editing your most recent editable message in the current scope. It scans only the already-loaded message list (bounded, no database access). Rationale: this is the familiar Slack/Discord gesture. It is optional and can be dropped if Up already has a conflicting composer meaning (check `handle_compose_key` first).

### Phase 4: Providers

#### Mock (demo and test harness)
- [x] 15. **Implement edits in `crates/core/src/mock.rs`:** `edit_message` updates the in-memory message and emits `MessageContentEdited`, and the mock advertises `edit: true`. Rationale: this gives an end-to-end path for TUI tests and demo mode without network access.

#### Slack
- [x] 16. **Parse the `edited` object** (`{user, ts}`) on `SlackRealtimeInnerMessage` (`crates/providers/slack/src/lib.rs:1164-1177`) and on the history message structs. Pass it through `slack_message_from_parts` so `edited_at` is set from `edited.ts` instead of the hard-coded `None` (`crates/providers/slack/src/lib.rs:6048`). Rationale: other `message_changed` events such as unfurls have no `edited` field, so the marker stays accurate.
- [x] 17. **Add `update_message(credential, channel, ts, text, …)`** to the `SlackApiClient` trait (`crates/providers/slack/src/lib.rs:913-1019`). The real client calls `chat.update`, following the error handling and diagnostics of the existing `post_message` (`crates/providers/slack/src/lib.rs:1452-1506`). Also implement it on the test fake (`crates/providers/slack/src/lib.rs:8093`).
- [x] 18. **Slack provider `edit_message` and capabilities:** read the channel and `ts` from `PlatformData::Slack`, encode mentions exactly as `send` does, and return the edit timestamp. Set `edit: true` for user and bot Web API identities and false for incoming webhooks (`crates/providers/slack/src/lib.rs:4131`). Map Slack's `cant_update_message`, `edit_window_closed` and `message_not_found` errors to readable messages. Don't emit a local event: the TUI completion updates storage, and the realtime `message_changed` echo reconciles idempotently.

#### ClickUp
- [x] 19. **Add `update_message(workspace_id, message_id, content)`** to `ClickUpApiClient` (`crates/providers/clickup/src/api.rs:310-400`). It uses the existing `PATCH` support (`crates/providers/clickup/src/http.rs:243`) against `/api/v3/workspaces/{workspace_id}/chat/messages/{message_id}` with `content_format: text/md`. Also implement it on `FakeClient`.
- [x] 20. **ClickUp provider `edit_message`, capability and updates:** implement `edit_message` from `PlatformData::ClickUp` and set `edit: true`. In the poller (`crates/providers/clickup/src/lib.rs:1214-1228`), remember `date_updated` for messages already seen and emit `MessageContentEdited` when it advances. Keep this within the existing page bounds, and never cause extra history paging.

#### WhatsApp (Go bridge)
- [x] 21. **Outbound:** add a `C_EditMessage` export in `crates/providers/whatsapp/go/bridge.go`. It takes the chat JID, the original message id, the text and the mentioned JIDs, builds the new content with the existing `buildTextMessage` (`crates/providers/whatsapp/go/bridge.go:1145-1160`, which keeps mentions), wraps it with `Client.BuildEdit`, and calls `SendMessage`. It returns the same JSON result shape as `C_SendText`, including the server timestamp. Support the same `test:` fake path as `C_SendText` (`crates/providers/whatsapp/go/bridge.go:467-479`).
- [x] 22. **Inbound, live and history:** in `emitMessageEvent` (`crates/providers/whatsapp/go/bridge.go:1557-1675`), before the reaction branch, detect an edit. Either the payload is a `ProtocolMessage` of type `MESSAGE_EDIT` (live), where the target is `Key.ID` and the content is `EditedMessage`, or `message.IsEdit` is set on a payload that `ParseWebMessage` has already rewritten (history), where the target is `Info.ID` and the content is the payload itself. Emit a new `bridgeEvent` of type `edit` with the chat JID, target id, resolved text (mentions resolved as for normal messages), `edited_at` (from `TimestampMS`, falling back to `Info.Timestamp`) and `from_me`. Return early so the edit is never re-emitted as a new or reordered message. Rationale: this fixes both the dropped live edits and the history replay that moves messages to the edit's timestamp.
- [x] 23. **Go tests** in `crates/providers/whatsapp/go/bridge_test.go`: the live edit payload maps to an `edit` event with the correct target id; a history-parsed edit keeps the original id and doesn't emit a `message`/`history` event; a normal text message is unchanged. `go test` doesn't run in CI, so run it locally.

#### WhatsApp (Rust)
- [x] 24. **FFI and routing:** declare `C_EditMessage` and add a safe `bridge::edit_message` wrapper in `crates/providers/whatsapp/src/bridge.rs:21-82`. Add the `edit` type and the `edited_at` field to `BridgeEvent` (`crates/providers/whatsapp/src/lib.rs:1079-1143`), and route it in the event switch (`crates/providers/whatsapp/src/lib.rs:1265-1435`): update the in-memory message cache if present, then emit `MessageContentEdited`.
- [x] 25. **WhatsApp provider `edit_message` and capabilities:** set `edit: true` with `edit_window` = 20 minutes, matching whatsmeow `EditWindow`. Reject edits outside the window with a clear error before calling the bridge. Run the FFI call on a blocking worker, as the existing send does. Tests use the `test:` database path.

### Phase 5: Validation

- [x] 26. **Core tests:** the eligibility helper (not your message, non-text, deleted, outside the window, capability off) and the default `edit_message` error.
- [x] 27. **Two-phase TUI tests,** following the AGENTS.md async testing rule, against the mock provider:
  - (a) Choosing Edit fills the composer and shows the banner.
  - (b) Enter clears the composer and shows "saving edit…" *before* draining, then shows the new text and "(edited)" *after* draining.
  - (c) Esc restores the previous draft.
  - (d) A stale completion after switching chats persists but doesn't reload or scroll the new chat.
  - (e) The Edit item is absent for other people's messages and for providers without the capability.
  - (f) An inbound `MessageContentEdited` updates the preview only when it concerns the latest message.
  - (g) The Up-arrow shortcut picks the latest editable message.
- [x] 28. **Slack and ClickUp provider tests** with the fakes: the request contains the right channel/`ts` or workspace/message id and the encoded mentions; `edited.ts` is parsed into `edited_at`; `message_changed` without `edited` leaves the marker off; the ClickUp poller emits a content edit when `date_updated` advances.
- [x] 29. (Also fixed: bubble headers no longer duplicate the own-message marker; flat layout places "(edited)" on its own measured row when the last row is full.) **Message list rendering test:** the "(edited)" marker appears, and draw and measured line counts agree at several widths.
- [x] 30. **Mirror CI** (`.github/workflows/ci.yml`) locally: `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and a release build. Also run `go test ./...` in `crates/providers/whatsapp/go`.

## Verification Criteria

- On WhatsApp, Slack (user/bot token) and ClickUp, editing your own text message from the TUI changes it on the official client within one sync cycle. The TUI shows the new text with "(edited)" without switching chats.
- An edit made on the phone or web shows the new text and the marker in the TUI. On WhatsApp this works live and after history re-sync, and the message **keeps its original timeline position**.
- Reactions and poll votes never cause an "(edited)" marker.
- The Edit action is absent for other people's messages, non-text messages, Slack webhook accounts, and WhatsApp messages older than 20 minutes. Trying the edit after the window closes shows a readable error.
- There are no network or database calls on the key-handling or draw paths for editing, and perf logs show `edit_send.*` and `provider.message_content_edited.*` labels.
- Replaying the same or an older edit is a no-op: storage is unchanged and sidebar order doesn't regress.
- All CI-equivalent commands pass locally, and Go bridge tests pass.

## Potential Risks and Mitigations

1. **The WhatsApp history path re-emits edits as normal messages with the edit's timestamp, which reorders the timeline.**
   Mitigation: check `IsEdit` in history-parsed messages and convert them to `edit` events (Task 22), and add a test that the original timestamp and position are preserved.
2. **Edits arriving out of order across devices, or replayed during catch-up.**
   Mitigation: the monotonic `edited_at` guard in `apply_message_edit` (Task 5). Never go through `upsert_message` for text edits.
3. **The local optimistic message id (`sender.platform_id = "me"`) differs from the provider echo, so the edit targets a row the provider doesn't know.**
   Mitigation: edits key on the provider message id returned by `send` (already used as `Message.id`). Check that WhatsApp, Slack and ClickUp all return the real platform id, and hide Edit for messages whose `platform_data` lacks the required ids.
4. **Mention round-tripping:** stored text may contain display names while the provider needs native tokens (for example Slack `<@U…>`).
   Mitigation: re-encode with `encode_outbound_mentions` when submitting, reusing the send path, with tests per provider.
5. **Slack bot tokens can't edit messages the user posted with a different identity.**
   Mitigation: `is_from_me` is already based on `current_user_id` (`crates/providers/slack/src/lib.rs:6058`). Surface `cant_update_message` clearly instead of failing silently.
6. **ClickUp poll-based edit detection adds load.**
   Mitigation: compare only messages already fetched in each poll page; no extra requests.
7. **`app.rs` is very large (~29.7k lines), so merge conflicts and regressions are likely.**
   Mitigation: keep the edit logic in small dedicated functions next to the reply and forward code, and cover it with the two-phase tests.

## Alternative Approaches

1. **Reuse `MessageEdited { message }` instead of a new event:** each provider rebuilds the full `Message` (from its cache or the store) before emitting. This means fewer new types, but WhatsApp and ClickUp often lack the full message, and a full upsert risks overwriting reactions and receipts and keeps the reaction/edit ambiguity. Not recommended.
2. **Synchronous edit inside the key handler, like the current `send`:** simpler and immediately consistent, but it violates the responsiveness rules and blocks the UI on slow networks. Rejected.
3. **Optimistic local apply before the provider confirms:** feels instant but needs rollback on failure, which is messy with the edit window and permission errors. The recommended approach applies the edit on confirmation, using the same "save on success" model as React.
4. **Editing media captions as well as text:** WhatsApp (`BuildEdit` with a new caption) and Slack (`chat.update` on file-share messages) can do this, but it widens the test matrix. Suggested as a follow-up once the text path is stable.
5. **Ship Delete/Revoke together:** the infrastructure overlaps a lot (whatsmeow `BuildRevoke`, Slack `chat.delete`, ClickUp DELETE, and the unused `MessageDeleted` handler at `crates/tui/src/app.rs:10138-10140`), but it is a separate, destructive-UX feature. Keep it as its own follow-up plan.
