# Quoted Replies: Inbound Fidelity and Reply/Thread Separation

## Objective

When someone replies to a specific WhatsApp message, on any device and in any direction, chat-cli should show it as a quoted reply: a nested box with the original sender and a snippet. This must work even when the original message isn't loaded, and it must work for live, offline-backlog and history-synced messages. WhatsApp quoted replies must not be shown as Slack-style threads.

## Findings (evidence)

**The screenshot.** The "Cristina Rădulescu" message is a plain bubble with no quote box. Its bridge event is in `tmp/debug.log` at line 32506. That event has `from_me:true` (sent from the phone) and carries **no reply fields at all**.

### Root cause

The Go bridge only *writes* `ContextInfo`, for outgoing replies (`crates/providers/whatsapp/go/bridge.go:1197-1222`, `crates/providers/whatsapp/go/bridge.go:1265-1284`). It never *reads* `ContextInfo.StanzaID`, `Participant` or `QuotedMessage` from incoming messages:
- `emitMessageEvent` builds the event without them (`crates/providers/whatsapp/go/bridge.go:1734-1748`).
- `bridgeEvent` has no reply fields (`crates/providers/whatsapp/go/bridge.go:162-225`).

The Rust side then hardcodes `reply_to: None` for every bridged message (`crates/providers/whatsapp/src/lib.rs:1628`).

### Blast radius

| Area | Effect | Evidence |
|---|---|---|
| WhatsApp live, offline and history messages | All replies lose their quote, whether sent by others or by you from the phone. All three paths go through `emitMessageEvent`. | `crates/providers/whatsapp/go/bridge.go:1474`, `crates/providers/whatsapp/go/bridge.go:1626` |
| Quote rendering when the original isn't loaded | Even with `reply_to` set, the TUI builds previews only from loaded messages. Anything older than the window or the sync scope shows "[message not loaded]". | `crates/tui/src/widgets/message_list.rs:540-543`, `crates/tui/src/widgets/message_list.rs:660-663`, `crates/tui/src/widgets/message_list.rs:5223-5239` |
| Reply vs. thread mix-up (all providers) | WhatsApp replies sent from chat-cli also set `thread_id`. Thread summaries treat any `reply_to`/`thread_id` as a thread, so "1 reply · Last reply…" can appear under WhatsApp originals. Once inbound replies are fixed, this would spread to every WhatsApp quote. | `crates/tui/src/app.rs:13825-13844`, `crates/tui/src/widgets/message_list.rs:4955-4975`, `crates/tui/src/widgets/message_list.rs:1550`, `crates/tui/src/app.rs:15380-15390` |
| Other `ContextInfo` data dropped | Mentions are read from only 6 message kinds; polls, locations and contacts are missed. The "forwarded" flag is never surfaced. | `crates/providers/whatsapp/go/bridge.go:2783-2807` |
| Storage overwrite | `reply_to_id = excluded.reply_to_id` lets a later snapshot without reply info (for example a local echo or an edit snapshot) erase a known reply link. | `crates/storage/src/lib.rs:2320-2321` |
| Slack / ClickUp | Their replies already populate `reply_to` (`crates/providers/slack/src/lib.rs:6213-6236`, `crates/providers/clickup/src/convert.rs:475`). They are only affected by the reply/thread separation. | — |

## Assumptions

- WhatsApp replies are **quotes**, never threads. Slack and ClickUp replies stay threads.
- A quoted snapshot (sender plus a short snippet) is persisted with the reply. The box can then render without loading the original and without database reads in the draw path (AGENTS.md).
- Existing stored messages are not backfilled. They get fixed when history sync replays them, because history goes through the same path.

## Implementation Plan

### Phase 1: Core model

- [x] 1.1 Add `reply_preview: Option<QuotedPreview>` to `Message` (`crates/core/src/types.rs:229-250`). `QuotedPreview` holds the quoted sender's platform id, display name, a one-line snippet and a content-kind label (Photo, Video, Voice message, Document, Sticker, Poll, Location, Contact).

  Rationale: lets quote boxes render even when the original isn't loaded. The type is provider-agnostic so Slack and ClickUp can use it later.

- [x] 1.2 Add `reply_kind: Option<ReplyKind>` with values `Quote | Thread`, set by the provider. Alternatively, derive it from the platform through one helper.

  Rationale: separates WhatsApp quotes from Slack and ClickUp threads explicitly, instead of the duplicated `is_slack_thread_reply` checks (`crates/tui/src/app.rs:21422-21431`, `crates/tui/src/widgets/message_list.rs:4872-4881`).

### Phase 2: Go bridge — read `ContextInfo` on every inbound message

- [x] 2.1 Add one `messageContextInfo(payload)` helper that covers every content kind carrying `ContextInfo`: extended text, image, video, audio, document, sticker, poll creation, location, live location, contact or contacts array, and template or button responses. Reuse it in `messageMentionedJID` (`crates/providers/whatsapp/go/bridge.go:2783-2807`).

  Rationale: one source of truth. It also fixes mentions missed in the extra message kinds.

- [x] 2.2 Add to `bridgeEvent` (`crates/providers/whatsapp/go/bridge.go:162-225`) and fill in `emitMessageEvent` (`crates/providers/whatsapp/go/bridge.go:1734-1767`):
  - `reply_to_id` (StanzaID);
  - `reply_to_sender_jid`: `Participant`, falling back to the chat JID for DMs, and mapped to its canonical form (PN vs. LID);
  - `reply_to_sender_name`, using the existing `participantName` / `contactDisplayName` resolvers, with "You" for your own JID;
  - `reply_to_text`: a snippet from `QuotedMessage` built with `messageText` (`crates/providers/whatsapp/go/bridge.go:2926`) plus mention resolution;
  - `reply_to_kind`: a media or poll label;
  - `is_forwarded`.

  This happens in the shared path, so live, offline and history messages all get it.

- [x] 2.3 Skip reply data that points at status broadcasts or has an empty StanzaID. Keep reply data on edits (`buildEditEvent`, `crates/providers/whatsapp/go/bridge.go:1863`) unchanged, so an edit never removes a quote.
- [x] 2.4 Extend Go unit tests (`crates/providers/whatsapp/go/bridge_test.go`):
  - a text reply in a DM;
  - a group reply that quotes another participant;
  - a reply that quotes your own message;
  - a reply carried in an image caption;
  - a quote whose original is a media message;
  - a history-sync (`ParseWebMessage`) reply.

### Phase 3: WhatsApp Rust provider

- [x] 3.1 Decode the new fields in `BridgeEvent` (`crates/providers/whatsapp/src/lib.rs:1159-1200`). In `forward_message_event` (`crates/providers/whatsapp/src/lib.rs:1620-1642`):
  - set `reply_to` to the quoted stanza id;
  - set `reply_preview` from the snippet fields;
  - set `reply_kind = Quote`;
  - leave `thread_id` as `None`.
- [x] 3.2 Make WhatsApp sends from the TUI stop setting `thread_id` (`crates/tui/src/app.rs:13830`). Also fill `reply_preview` on the local echo from the message being replied to.

  Rationale: stops chat-cli's own WhatsApp replies from showing up as threads.

### Phase 4: Storage

- [x] 4.1 Add a nullable `reply_preview_json` column to `messages` (`crates/storage/src/schema.rs:48`) with a migration, plus read and write support (`crates/storage/src/lib.rs:2283-2324`, `crates/storage/src/lib.rs:1922`).
- [x] 4.2 Change the upsert so `reply_to_id` and `reply_preview_json` use `COALESCE(excluded, existing)`. A snapshot without reply info must never erase a known reply link (`crates/storage/src/lib.rs:2320`).

### Phase 5: TUI rendering

- [x] 5.1 In `resolve_reply_preview` (`crates/tui/src/widgets/message_list.rs:5223-5239`), look things up in this order:
  1. the loaded original (current behaviour, freshest edits);
  2. the message's own `reply_preview`;
  3. only then "[message not loaded]".

  Include `reply_preview` in the layout-cache hash next to `reply_to` (`crates/tui/src/widgets/message_list.rs:831`).
- [x] 5.2 Build thread summaries, thread replies and thread unread bumps only from `reply_kind == Thread`:
  - `crates/tui/src/widgets/message_list.rs:4955-4975`
  - `crates/tui/src/app.rs:15380-15390`
  - `crates/tui/src/app.rs:14877-14890`

  Rationale: stops WhatsApp quotes from creating "N replies" lines, thread markers or `thread_reads` rows.
- [ ] (SKIPPED: optional, out of scope) 5.3 Optional follow-up: add a "jump to quoted message" action that scrolls to the original if loaded, or otherwise queues a background history load around it. Out of scope unless requested.

### Phase 6: Tests and validation

- [x] 6.1 Provider tests: a bridge event with reply fields becomes a `Message` with `reply_to`, `reply_preview` and `Quote`, and no `thread_id`.
- [x] 6.2 Storage tests: the reply preview survives a round trip. A later upsert without reply data keeps the stored link.
- [x] 6.3 Rendering tests:
  - a quoted box shows the stored snapshot when the original isn't loaded;
  - the loaded original is preferred when present;
  - a WhatsApp quote produces no thread summary line;
  - Slack thread summaries are unchanged.
- [x] 6.4 Run the CI checks from `.github/workflows/ci.yml` and `go test` for the bridge.

## Verification Criteria

- The Cristina Rădulescu reply, once re-synced or received again, shows the nested quote box with the original sender and snippet.
- Replies sent from the phone, from other contacts, in groups and in captions all show quotes, whether they arrived live, offline or via history sync.
- Quotes whose original is outside the loaded window show the stored snippet, not "[message not loaded]".
- No WhatsApp message shows "N replies" or thread markers. Slack and ClickUp thread behaviour is unchanged.
- Draw paths do no database reads for quote previews.

## Potential Risks and Mitigations

1. **Quoted sender JIDs are mixed PN and LID, so names don't resolve.**
   Mitigation: reuse the existing canonical-alias helpers (`crates/providers/whatsapp/go/bridge.go:1342-1344`), fall back to push name, then to the number.
2. **Snapshots go stale when the original is later edited or deleted.**
   Mitigation: prefer the loaded original when available. Show a "deleted" snippet when the original is known to be deleted.
3. **The schema migration on existing databases.**
   Mitigation: the column is nullable and old rows render exactly as today.
4. **Reply/thread separation could regress Slack threads.**
   Mitigation: Slack and ClickUp set `Thread` explicitly. Their existing thread tests must pass unchanged.

## Alternative Approaches

1. **Store `reply_to` only, no snapshot:** the smallest change, but most quotes of older messages would show "[message not loaded]" given the default sync scope.
2. **Keep the snapshot in WhatsApp `platform_json`:** avoids a migration, but it is WhatsApp-only and the TUI would need platform-specific parsing.
3. **Fetch the original on demand when it isn't loaded:** always accurate, but it adds background queries per quote and still fails for messages outside the synced history.
