# Surface, View, and Respond to Thread Messages

## Objective

Make thread activity a first-class, recognisable experience in chat-cli, modelled on native Slack-style apps. Specifically:

1. **Surface** — clearly signal when a new message arrives *inside a thread* (notification wording, sidebar indicator, and an unread-thread badge on the root message's summary line), distinct from ordinary channel messages.
2. **View** — make threads with new activity trivially discoverable and jumpable: a dedicated "Threads" inbox view, keyboard navigation to the next unread thread, and visually highlighted unread thread summaries.
3. **Respond** — keep the existing thread compose box but smooth the open → read → reply flow with predictable keybindings and automatic mark-as-read on view.

The expected outcome is that a user can tell at a glance that someone replied to a thread, reach that thread in one or two keystrokes, see exactly which replies are new, and respond inline — matching the affordances of native desktop apps.

## Background Findings (grounding for the plan)

- Threads are storable but not queryable: `messages.thread_id` / `idx_messages_thread` exist (`crates/storage/src/schema.rs:48-55`) but no API reads by thread; no `Thread` type in `crates/core/src/types.rs`.
- Unread is chat-level only (`Chat.unread_count`, `crates/core/src/types.rs:61`); there is no per-thread unread tracking.
- Thread replies are hidden from the timeline and aggregated into a summary line (`crates/tui/src/widgets/message_list.rs:3456-3536`); the line is already hit-tested for clicks (`message_list.rs:776-799`).
- A working Thread pane and thread compose already exist (`crates/tui/src/app.rs:4015-4123`, `:4224`, `open_thread` at `:9190-9201`, `send_thread_composed_message` at `:8055-8113`).
- Incoming messages bump chat-level unread (`app.rs:8648-8672`) and queue a generic notification (`app.rs:9875-9960`) with no thread awareness.
- Slack round-trips threads fully and exposes transient `reply_count` (`crates/providers/slack/src/lib.rs:478-479`, `:4834-4872`); WhatsApp currently has no thread concept.

## Assumptions

- "Thread" semantics are Slack-first; the generic `reply_to`/`thread_id` fallback is the cross-provider mechanism. WhatsApp threading remains out of scope here (it has no inbound thread data today).
- Per-thread unread is derived/tracked in the app/storage layer; providers are not expected to deliver per-thread read state.
- Native-app inspiration target is Slack desktop: a top-of-sidebar "Threads" entry, bold/blue unread thread rows, "N new replies" badges, and "replied to a thread" notification wording.
- No code is produced in this plan; it defines the strategic what/why for an implementation agent.

## Implementation Plan

### Phase 1 — Data model: make threads queryable and unread-aware

- [ ] Task 1. Introduce a thread identity and summary concept in `crates/core/src/types.rs`. Rationale: there is no `Thread`/`ThreadId` today, so the UI cannot reason about a thread as an entity. Add a `ThreadId` alias and a `ThreadSummary`-style core type capturing root message id, `reply_count`, `unread_reply_count`, `last_reply_at`, and `participants`, so all layers share one vocabulary.
- [ ] Task 2. Add per-thread read tracking to storage. Rationale: unread is chat-level only, so a thread reply is indistinguishable from a channel message. Add a `thread_reads` table (keyed by `account_id`, `thread_id`) storing `last_read_at` / `last_read_message_id`, plus a schema migration. Keep `chats.unread_count` semantics intact to avoid regressing sidebar ordering.
- [ ] Task 3. Add thread-scoped query methods in `crates/storage/src/lib.rs`. Rationale: `idx_messages_thread` exists but nothing uses it. Provide `get_messages_for_thread`, `thread_summaries_for_chat` (reply counts, last reply, participants), and an `unread_thread_summaries` query joining against `thread_reads`. These are the data feeds for surfacing and the Threads view.
- [ ] Task 4. Add a thread mark-read API. Rationale: viewing a thread must clear its unread state without touching unrelated chat unread. Provide `mark_thread_read(account, thread_id, up_to_message_id)` that updates `thread_reads` and recomputes derived unread counts.

### Phase 2 — Surface: signal thread arrivals distinctly

- [ ] Task 5. Detect "is a thread reply" on the live incoming path in `crates/tui/src/app.rs` (around the `ProviderEvent::Message` handler at `app.rs:5865-5937`). Rationale: today thread replies are handled identically to top-level messages. Reuse the existing `is_slack_thread_reply` logic (mirroring `message_list.rs:3456-3472`) to branch behaviour for threads.
- [ ] Task 6. Make thread arrivals increment per-thread unread (Phase 1 state) in addition to / instead of blanket chat unread, integrating with `mark_slack_live_message_unread_if_needed` (`app.rs:8648-8672`). Rationale: this is what lets the UI show "which thread" has new replies and how many, rather than only a chat-level number.
- [ ] Task 7. Differentiate thread notifications in `maybe_queue_notification` (`app.rs:9875-9960`) and the `chat_notify` payload (`MessageNotification`). Rationale: native apps say "replied to a thread" rather than a plain message. Adjust the notification title/body to indicate a thread reply and include the thread/root context, while honouring existing mute/scope/pause suppression rules.
- [ ] Task 8. Add an unread badge to the thread summary line in `crates/tui/src/widgets/message_list.rs` (`thread_summary_spans`, `:3492-3536`). Rationale: the summary currently shows total "N replies"; native apps show new-reply emphasis. Render an "N new" badge / bold styling when `unread_reply_count > 0`, derived from Phase 1 data.
- [ ] Task 9. Reflect thread activity in the sidebar in `crates/tui/src/widgets/chat_list.rs` (`unread_marker`/`chat_item`, `:474-588`). Rationale: a user scanning the sidebar should know a chat has unread *thread* activity, not just channel messages. Add a small thread indicator (e.g. a distinct glyph or secondary count) without breaking the existing numeric-badge tests (`chat_list.rs:1419-1426`).

### Phase 3 — View: make unread threads discoverable and reachable

- [ ] Task 10. Add a "Threads" inbox view that aggregates unread/active threads across the selected account (Slack-style top-of-sidebar entry or a dedicated overlay/pane). Rationale: native apps centralise thread activity so users do not have to scan every chat. Back it with `unread_thread_summaries` (Task 3), showing root preview, participant avatars, "N new replies", and last-reply time. Integrate with `FilterScope::Thread` which already exists (`app.rs:569-593`).
- [ ] Task 11. Add keyboard navigation to threads in `handle_key` / `handle_message_key` (`app.rs:6459-6582`). Rationale: ease of view is keystroke-driven in TUIs. Provide a shortcut to open the Threads view and a "jump to next/previous unread thread" action that selects the relevant chat and opens its thread pane.
- [ ] Task 12. Highlight and auto-scroll to the thread root with unread replies when navigating from the Threads view or a notification. Rationale: viewing should land the user precisely on the relevant thread, consistent with the responsiveness rules (immediate selection, async load via the existing generation-token mechanism at `app.rs:272-295`).

### Phase 4 — Respond: smooth the open → read → reply loop

- [ ] Task 13. Mark a thread read on open in `open_thread` (`app.rs:9190-9201`) / `draw_thread_details` (`app.rs:4015-4123`) by calling `mark_thread_read` (Task 4). Rationale: viewing a thread should clear its unread state and badges, matching native behaviour, while preserving chat selection/scroll per `AGENTS.md`.
- [ ] Task 14. Surface unread reply separators inside the thread pane (a "new replies" divider above the first unread reply in `thread_replies`, `app.rs:9034-9044`). Rationale: native apps mark where unread begins so users orient quickly within a long thread.
- [ ] Task 15. Confirm and document the reply send path (`send_thread_composed_message`, `app.rs:8055-8113`) correctly sets `reply_to`/`thread_id` for Slack and degrades gracefully where threads are unsupported (WhatsApp). Rationale: responding must be reliable and predictable; verify `outbound_capabilities` gating and that the Slack webhook-identity path's lack of `thread_ts` (`slack/src/lib.rs:3077-3086`) is handled or surfaced.
- [ ] Task 16. Add a one-key "reply in thread" affordance from the timeline (e.g. on a root message with replies) that opens the thread pane focused on its compose box. Rationale: minimise steps from "I see a thread" to "I'm typing a reply", mirroring native quick-reply.

### Phase 5 — Validation and instrumentation

- [ ] Task 17. Add opt-in performance instrumentation for the new thread query/unread pipeline (labels with account/chat/thread ids, counts, stale/error totals), per `AGENTS.md`. Rationale: any new async pipeline must be measurable; thread aggregation queries are a candidate slow path.
- [ ] Task 18. Add two-phase async UI tests: assert immediate placeholder/selection state after opening a thread or the Threads view, then final state after draining background completions. Rationale: required by `AGENTS.md` for async UI work; protects against blocking the draw/event path.
- [ ] Task 19. Add tests for thread-unread lifecycle: a live thread reply increments per-thread unread and shows the badge; opening the thread clears it; historical replay is idempotent and does not regress sidebar order. Rationale: locks in the surfacing behaviour and protects the activity-ordering invariants.

## Verification Criteria

- A live thread reply produces a notification whose wording identifies it as a thread reply, subject to existing mute/scope/pause rules.
- The thread summary line on the root message shows an "N new" indicator that clears after the thread is viewed.
- The sidebar distinguishes a chat with unread thread activity from one with only channel activity, without breaking existing badge tests.
- A dedicated Threads view lists all threads with unread replies for the account, ordered by most recent reply, each reachable in one selection action.
- A keyboard shortcut jumps to the next unread thread and opens its thread pane focused for reply.
- Opening a thread marks it read (per-thread state) and shows a "new replies" divider above the first previously-unread reply.
- Replying in a thread sends with the correct `reply_to`/`thread_id` for Slack and degrades gracefully on providers without thread support.
- New async pipelines are instrumented; async UI tests assert both placeholder and final states; historical replay does not regress sidebar order or thread unread counts.

## Potential Risks and Mitigations

1. **Double-counting unread (chat-level vs thread-level).**
   Mitigation: define a single source of truth for derived counts; keep `chats.unread_count` semantics unchanged and compute thread unread separately, reconciling in one place (Tasks 2, 6) with tests (Task 19).
2. **Schema migration risk for `thread_reads`.**
   Mitigation: additive, versioned migration with a backfill default of "all read at install time" to avoid a flood of false unread on first launch; no destructive changes to existing tables.
3. **Performance regression from thread aggregation in the draw/event path.**
   Mitigation: run thread summary/unread queries off the hot path with bounded batches and generation tokens; cache by stable keys; instrument per `AGENTS.md` (Tasks 3, 12, 17).
4. **Historical replay regressing sidebar order or inflating thread unread.**
   Mitigation: suppress notifications and unread increments for `is_historical` messages (consistent with `app.rs:9876-9884`); make replay idempotent and assert it in tests (Task 19).
5. **Provider asymmetry (WhatsApp lacks threads).**
   Mitigation: gate thread UI/affordances on provider capability and the presence of `thread_id`; degrade to plain reply where threads are unsupported, surfacing capability clearly (Task 15).
6. **Sidebar/notification visual changes breaking existing tests/expectations.**
   Mitigation: respect the existing numeric-badge contract (`chat_list.rs:1419-1426`); add new indicators as additive styling/glyphs and update tests only where behaviour intentionally changes.

## Alternative Approaches

1. **Thread-as-chat modelling**: represent each thread as its own `Chat` row (leveraging the existing `Chat.thread_id`, `crates/core/src/types.rs:66`) so threads reuse the entire chat-list/unread/notification machinery. Trade-off: maximal reuse and a very native "threads in the sidebar" feel, but risks sidebar clutter and complicates merge/ordering logic against parent chats.
2. **Minimal surfacing only (no Threads view)**: implement just notification wording, the summary-line "N new" badge, and mark-read-on-open. Trade-off: much smaller change and quick UX win, but lacks centralised discoverability and next-unread navigation, so it is less recognisable versus native apps.
3. **Provider-driven thread unread**: rely on provider-reported thread read state instead of local `thread_reads`. Trade-off: more accurate cross-device read state where supported, but Slack's API surface here is limited and WhatsApp offers nothing, so coverage would be inconsistent and harder to test.
