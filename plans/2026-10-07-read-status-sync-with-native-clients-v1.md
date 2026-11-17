# Read-Status Sync With Native Clients (WhatsApp, Slack, ClickUp)

## Objective

Reading a chat in chat-cli should clear its unread badge in the native app (phone, Slack app), and reading it in the native app should clear the badge in chat-cli. This should hold in steady state, after a restart, and after an offline period, without slowing the TUI event loop.

## Current Status (evidence)

| Provider | Native → chat-cli | chat-cli → native | Verdict |
|---|---|---|---|
| WhatsApp | Works live for self-read receipts and "mark read" app-state (`crates/providers/whatsapp/go/bridge.go:1407-1433`). Unread counts from history sync can override in both directions (`crates/tui/src/app.rs:10294-10327`). | Partly works. Receipts are sent (`crates/providers/whatsapp/go/bridge.go:1055-1117`), but often for the wrong messages, too late, or not at all. | Buggy |
| Slack | Nothing. Realtime handles only messages and reactions (`crates/providers/slack/src/lib.rs:5897-5914`). | Nothing. `mark_read` is a no-op (`crates/providers/slack/src/lib.rs:4535-4541`). | Missing |
| ClickUp | Only through channel re-listing (`crates/providers/clickup/src/convert.rs:233-240`). | Local only. The API has no mark-read endpoint (`crates/providers/clickup/src/lib.rs:895-904`). | API-limited |

What the WhatsApp debug log (`tmp/debug.log`) shows:
- **Phone → chat-cli works while connected.** Group self-reads arrive as `type="read"` from the user's own LID and are forwarded as `"type":"read"`. Examples: log lines 1389-1395, 4325-4330, 4570-4575.
- **Receipts are sent as `read-self`.** The account's privacy setting `readreceipts=none` (line 1165) makes whatsmeow downgrade them. This is expected and matches WhatsApp Web, so it is not the bug.
- **Receipts target the wrong message.** Message `ACF53CD6…` arrived at 14:20:57 (line 2064). At 14:21:06 chat-cli re-sent a receipt for the older `AC72169A…` (line 2079). `ACF53CD6…` and three later messages were only acknowledged at 16:29:43 (line 4848), about two hours later. Meanwhile the phone's own count for that chat rose to 2, 3 and 4 (lines 2110, 2166, 2208, 2363).
- **Stale phone counts bring the badge back.** An on-demand history sync 1 second after a mark-read reported `unread_count:1` for the just-read chat (lines 2015 and 2044) and briefly restored the badge.

Root causes in code, ranked by impact:
1. **Live messages in the open chat are never marked read.** The live-message path only reloads the pane (`crates/tui/src/app.rs:9947-9953`). The provider still increments its cached unread (`crates/providers/whatsapp/src/lib.rs:1660-1662`), and the WhatsApp `max()` merge pushes that badge into the open chat (`crates/tui/src/app.rs:20209-20213`).
2. **Receipts are chosen by a fragile rule.** The provider takes the last `max(unread,1)` messages from its in-memory list, in insertion order (`crates/providers/whatsapp/src/lib.rs:677-697`). After a restart that list is empty, so nothing is sent while the TUI still clears the badge locally (`crates/tui/src/app.rs:14816-14825`). chat-cli and the phone then disagree permanently.
3. **The local count decides whether to sync.** `mark_selected_chat_read` returns early when the local `unread_count == 0` (`crates/tui/src/app.rs:14802-14804`), even if the phone still has unread messages.
4. **There is no read watermark per chat.** Storage has only an `unread_count` integer (`crates/storage/src/schema.rs:21`). That makes it impossible to tell a stale phone snapshot from a real new unread.
5. **Two WhatsApp signals are ignored.** "Mark as unread" from the phone is dropped (`crates/providers/whatsapp/go/bridge.go:1425`). chat-cli also never clears the phone's manual marked-unread flag; the `BuildMarkChatAsRead` app-state patch exists in whatsmeow but is unused.
6. **No catch-up after time offline.** Nothing reconciles reads made on the phone while chat-cli was closed, beyond whatever receipts the server replays.
7. **The event loop waits on the provider.** `provider.mark_read(...).await` runs inline in the TUI (`crates/tui/src/app.rs:14813`). `ChatMarkedRead` and `ChatUnreadSynced` also write to storage inline (`crates/tui/src/app.rs:10288`, `crates/tui/src/app.rs:10322`), which breaks the AGENTS.md rules.

## Assumptions

- Reading counts as **the chat is selected, the Messages pane is focused, and the message has been loaded into the visible timeline.** Background or unfocused chats never send receipts.
- Read sync stays on by default. A per-account opt-out (privacy) is added because receipts become more frequent.
- Slack Events API (Socket Mode) does not deliver `channel_marked`/`im_marked`, which are RTM-only. Slack inbound sync therefore has to poll. This must be checked against Slack docs before implementation.
- ClickUp's public v3 Chat API still has no mark-read endpoint. Only listing-based reconciliation is possible there.

## Implementation Plan

### Phase 0: Instrumentation (required by AGENTS.md before optimizing)

- [x] 0.1 Add opt-in labels for the read-sync pipeline:
  - `read_sync.local_mark` (account, chat, inbound target count, reason: open, live_arrival or history_loaded)
  - `read_sync.provider_send` (success or error, receipt count, sender groups)
  - `read_sync.remote_read` (source: self_receipt, app_state or history_snapshot)
  - `read_sync.snapshot_rejected_stale`

  Rationale: the log above was the only way to tell wrong-target receipts from missing ones. These labels make regressions measurable.

### Phase 1: Shared core model — a read watermark per chat

- [x] 1.1 Add `last_read_at` (timestamp) and `last_read_message_id` to `Chat` (`crates/core/src/types.rs:138`). Add matching nullable columns to `chats`, with a migration in `crates/storage/src/schema.rs`. (DONE with deviation: only `last_read_at` is stored; the message id is carried per request in `MarkReadRequest.up_to` instead of persisted)

  Persist them with "never go backwards" semantics in `upsert_chat_on_conn` (`crates/storage/src/lib.rs:1308-1355`), mirroring how `last_msg_at` is protected.

  Rationale: this is the foundation for rejecting stale snapshots (cause 4) and for deciding what to acknowledge without trusting `unread_count` (cause 3).

- [x] 1.2 Change the `Provider::mark_read` contract (`crates/core/src/provider.rs:467-468`). It should receive an explicit read request: the newest read message (id and timestamp) plus the bounded list of inbound messages being acknowledged (id, sender platform id, timestamp). The TUI builds the list from loaded and stored messages newer than the watermark.

  Update the mock provider (`crates/core/src/mock.rs:237-239`) and all three providers.

  Rationale: this removes the provider's dependence on its in-memory message list, which is wrong after a restart (cause 2).

- [x] 1.3 Make `ChatMarkedRead` carry an optional `read_up_to` timestamp. Make `ChatUnreadSynced` carry an optional `as_of` timestamp: the newest message time of the snapshot (`crates/core/src/events.rs:65-84`).

  Rationale: lets the app ignore snapshots older than the local watermark.

### Phase 2: TUI read-trigger and reconciliation logic

- [x] 2.1 Rewrite `mark_selected_chat_read` (`crates/tui/src/app.rs:14798-14827`):
  - Decide what to acknowledge from "inbound messages newer than `last_read_at`" instead of `unread_count == 0`.
  - Update UI state and the watermark immediately.
  - Hand the provider call to a background task, so the event loop never awaits it.
  - Record completion and errors through the existing completion-drain pattern, with stale-generation checks.

  Rationale: fixes causes 3 and 7.

- [x] 2.2 Mark read when a live message arrives in the attended chat. If an inbound live message lands in the selected chat while Messages is focused, schedule a debounced mark-read (about 1–2 s, merging bursts into one request). Also reset that chat's provider-side cached unread so the next `ChatUpdated` cannot raise it.

  Rationale: fixes cause 1, the main bug seen in the log.

- [x] 2.3 Narrow the WhatsApp `max()` merge in `preserve_sidebar_activity_metadata` (`crates/tui/src/app.rs:20204-20213`):
  - Never raise the unread count of the currently attended chat.
  - Never raise it above the count of inbound messages newer than `last_read_at`.

  Rationale: stops `ChatUpdated` snapshots from bringing back badges the user has already cleared.

- [x] 2.4 Make `ChatUnreadSynced` aware of the watermark (`crates/tui/src/app.rs:10294-10327`):
  - Lowering always applies.
  - Raising applies only when `as_of` is newer than `last_read_at`, or when no watermark exists.
  - Otherwise, log `read_sync.snapshot_rejected_stale`.

  Make `ChatMarkedRead` advance the watermark. Move both storage writes to `queue_chat_persist`.

  Rationale: fixes the stale-snapshot flicker (log lines 2015 and 2044) and removes inline database writes.

- [x] 2.5 Make sure that opening a chat, finishing a history load (`crates/tui/src/app.rs:4147-4185`, `crates/tui/src/app.rs:4295-4297`) and the new live-arrival trigger all go through one debounced entry point. That keeps receipt bursts bounded.

### Phase 3: WhatsApp provider and Go bridge

- [x] 3.1 Rewrite `WhatsAppProvider::mark_read` (`crates/providers/whatsapp/src/lib.rs:652-736`) to use the explicit message list from the new contract:
  - Keep the 100-message cap (`crates/providers/whatsapp/src/lib.rs:33`).
  - Keep the detached blocking worker.
  - On bridge failure, emit a status or diagnostic event, but don't roll back the local state.

  Rationale: fixes the wrong-target receipts proven at log lines 2064, 2079 and 4848.

- [x] 3.2 Extend `C_MarkRead` (`crates/providers/whatsapp/go/bridge.go:1055-1117`) with an optional "clear manual unread flag" step. When requested, send `appstate.BuildMarkChatAsRead(chat, true, lastTs, lastKey)` through the app-state send API after the receipts. The Rust side requests it only when the chat is known to be manually marked unread (see 3.3).

  Rationale: receipts alone don't clear a "mark as unread" flag set on the phone (cause 5).

- [x] 3.3 Track the manual-unread flag:
  - Forward `MarkChatAsRead{Read:false}` (`crates/providers/whatsapp/go/bridge.go:1424-1429`) as a `chat_unread` event with `marked_unread=true`, and a count of at least 1.
  - Forward the history-sync `MarkedAsUnread` bit alongside `conversationUnreadCount` (`crates/providers/whatsapp/go/bridge.go:1441-1462`).
  - Keep the flag in provider state.

  Rationale: syncs manual mark-as-unread in both directions.

- [x] 3.4 Add the conversation's newest-message timestamp to `chat_unread` events as `as_of`. Add the receipt timestamp to `read` events as `read_up_to`.

  Rationale: feeds the watermark logic from 1.3 and 2.4.

- [x] 3.5 Reconcile after reconnect. After `OfflineSyncCompleted` (`crates/providers/whatsapp/go/bridge.go:1480-1484`), let the Rust side queue a bounded, rate-limited batch of small on-demand history requests, for example the 20 most recent chats with local unread > 0. That reuses the existing `C_RequestHistory` path (`crates/providers/whatsapp/go/bridge.go:372-434`) to get the phone's authoritative `UnreadCount` for each.

  Rationale: covers reads made on the phone while chat-cli was offline (cause 6).

  Caveats: this depends on the phone being online. It must be drained in bounded batches.

- [x] 3.6 Explicitly fetch the `regular_low` app-state collection on connect, next to the existing `regular_high` mute fetch (`crates/providers/whatsapp/go/bridge.go:1358-1365`).

  Rationale: mark-read and mark-unread actions live in `regular_low`, and today they arrive only if the server happens to push a `server_sync` notification (log line 604).

### Phase 4: Slack

- [x] 4.1 Implement `mark_read` using `conversations.mark` (channel plus the `ts` of the newest read message), run as a background request.
  - Add `channels:write`, `groups:write`, `im:write` and `mpim:write` user scopes to the generated manifest and OAuth scope list (`crates/providers/slack/src/lib.rs:8049-8052`) and update the manifest tests.
  - On `missing_scope`, degrade gracefully: local-only clear, plus a one-time status hint to re-authorize.

  Rationale: covers the chat-cli → Slack direction. This was planned in `plans/2026-06-03-chat-cli-v1.md:974` but never built.

- [x] 4.2 Add read-state polling. For Slack chats with local unread > 0, call `conversations.info` (`crates/providers/slack/src/lib.rs:2034-2055`) on a bounded, rate-limit-aware cadence. Compare the returned `last_read` with the newest stored message `ts`, and emit `ChatMarkedRead` / `ChatUnreadSynced` with timestamps.

  Run this inside the existing polling scheduler and respect its stop signal (`crates/providers/slack/src/lib.rs:5375-5400`).

  Rationale: covers the Slack → chat-cli direction without RTM.

- [x] 4.3 Make `mark_slack_live_message_unread_if_needed` (`crates/tui/src/app.rs:14829-14863`) respect the new watermark, so polling and live counting don't double count.

### Phase 5: ClickUp

- [x] 5.1 Keep `mark_read` local-only and document the limitation in code. Make sure channel re-listing (`crates/providers/clickup/src/convert.rs:233-240`) emits `ChatUnreadSynced` with an `as_of` timestamp, so ClickUp-side reads still lower the badge through the same watermark logic. (DONE with deviation: ClickUp re-listing still emits `ChatUpdated`; the watermark guard in `preserve_sidebar_activity_metadata` keeps locally-read chats at 0 unless newer activity arrives)

### Phase 6: Settings and tests

- [x] 6.1 Add a per-account setting `send_read_receipts` (default on). When off, the app clears unread locally only. It does not affect the WhatsApp privacy downgrade to `read-self`, which whatsmeow handles itself. (DONE with deviation: implemented as the global `send_read_status` setting, "Sync read status" in Settings, default on)
- [x] 6.2 TUI tests, each asserting both phases (immediate state, then the state after draining completions):
  - Opening a chat after a restart sends receipts for stored inbound messages.
  - A live message in the attended chat triggers one debounced mark-read and no badge.
  - A stale `ChatUnreadSynced` older than the watermark is rejected; a newer one is applied.
  - `ChatMarkedRead` advances the watermark and cancels notifications.
- [x] 6.3 Provider tests:
  - WhatsApp builds receipts from the explicit list, grouped by sender.
  - Go bridge unit tests for forwarding mark-unread and for the `MarkedAsUnread` flag (extend `crates/providers/whatsapp/go/bridge_test.go:143`).
  - Slack `conversations.mark` request shape and missing-scope degradation, using the existing mock Web API client (`crates/providers/slack/src/lib.rs:8145`).
- [x] 6.4 Run the CI checks from `.github/workflows/ci.yml` (cargo check, test, clippy, release build, plus `go test` for the bridge) before committing.

## Verification Criteria

- An inbound WhatsApp message read in chat-cli (chat open and focused) produces a receipt naming that exact message id within about 2 s. The phone's badge for that chat drops to 0.
- After restarting chat-cli, opening a chat with unread messages from before the restart still sends receipts. There are no more "messages=0" or silent no-op cases in `read_sync.provider_send`.
- Reading a chat on the phone clears the chat-cli badge within a few seconds while connected. After a restart, it clears once the reconnect reconciliation finishes.
- A phone snapshot whose `as_of` is older than the local watermark never brings a badge back. `read_sync.snapshot_rejected_stale` shows these events.
- "Mark as unread" on the phone shows a badge in chat-cli. Opening the chat in chat-cli clears it on the phone.
- Slack: reading in chat-cli clears the Slack app badge (with the new scopes). Reading in the Slack app clears chat-cli within one polling interval.
- No `mark_read` network call or storage write is awaited on the TUI event loop (checked by review and perf labels).

## Potential Risks and Mitigations

1. **More receipts reveal reading activity to contacts.**
   Mitigation: receipts are only sent for attended chats, `readreceipts=none` is honored automatically through `read-self`, and the `send_read_receipts` opt-out is available.
2. **Reconnect reconciliation floods the phone with on-demand history requests.**
   Mitigation: limit it to recent chats with unread > 0, rate-limit, drain in bounded batches, and skip it when the phone hasn't answered the previous batch.
3. **The schema migration runs on existing databases.**
   Mitigation: the new columns are nullable. A missing watermark keeps today's behavior ("no watermark → accept snapshot").
4. **Existing Slack installs lack the `*:write` scopes.**
   Mitigation: detect `missing_scope`, fall back to local-only, and show a one-time re-authorize hint.
5. **Sending app-state patches (`BuildMarkChatAsRead`) could conflict with phone state if the encoding is wrong.**
   Mitigation: send only when the chat is known to be manually marked unread, log the patch outcome, and test against a secondary account first.
6. **Changing the `mark_read` signature touches every provider and the mock.**
   Mitigation: change it in one step in Phase 1, behind compile-checked types, with tests updated together.

## Alternative Approaches

1. **Minimal WhatsApp-only fix:** do 2.2, plus 3.1 with the TUI passing message ids, and skip the watermark. This is fast and fixes most of what the log shows, but stale-snapshot flicker and offline drift stay unsolved.
2. **Provider-side storage access:** let providers read messages from storage in `mark_read` instead of changing the trait. This means a smaller API change, but it couples providers to storage and puts database reads on provider paths.
3. **Use phone counts only:** drop local unread counting for WhatsApp and rely on `chat_unread` snapshots. This is always consistent with the phone, but badges lag (they need history-sync pushes) and nothing works while the phone is offline.
4. **Slack via RTM (`rtm.connect`) to receive `*_marked` events:** this gives realtime inbound read sync, but RTM is legacy and unavailable to newer Slack apps. Polling `conversations.info` is the safer path.
