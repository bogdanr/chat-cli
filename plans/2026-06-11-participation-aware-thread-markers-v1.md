# Participation-Aware Thread Markers and Direct Thread Navigation

## Objective

Make the sidebar `⤷N` thread marker reflect only threads the user is actually part of (authored the root, replied, or was mentioned), de-emphasize or hide unrelated thread chatter, and give the user a one-keystroke path from the sidebar marker straight into the relevant unread thread.

## Confirmed Problem

- `⤷N` counts unread replies for **all** threads in a chat. The live bump path (`crates/tui/src/app.rs:10305-10338`) only excludes the user's own replies and the currently open thread; the storage aggregation (`crates/storage/src/lib.rs:2020-2146`) has no participation concept.
- Participation can be derived locally from `messages.is_from_me` (`crates/storage/src/schema.rs:50`). Mention-based participation requires persisting `mentions_me`, which currently exists only on the in-memory `Message` (`crates/core/src/types.rs:210`).

## Implementation Plan

### Phase 1 — Persist the missing participation signal

- [x] Task 1. Add a `mentions_me INTEGER NOT NULL DEFAULT 0` column to the `messages` table via a schema migration, and write it in the message upsert path. Rationale: providers already compute `mentions_me` per message; persisting it makes "I was mentioned in this thread" answerable from storage for both live and historical messages.
- [x] Task 2. Backfill is not required; document that mention-based participation only applies to messages stored after the migration, while `is_from_me`-based participation works retroactively for all existing history.

### Phase 2 — Participation classification in storage

- [x] Task 3. Extend `ThreadSummary` (`crates/core/src/types.rs:26-49`) with a `participation` field, e.g. an enum `ThreadParticipation { Author, Replied, Mentioned, None }` (highest applicable wins). Rationale: a single authoritative classification lets every surface (sidebar, summary line, Threads inbox) apply consistent policy.
- [x] Task 4. Compute participation inside `thread_summaries_on_conn` (`crates/storage/src/lib.rs:2020-2146`) during the existing single-pass aggregation: root authored by me → `Author`; any reply with `is_from_me` → `Replied`; any message in the thread with `mentions_me` → `Mentioned`; otherwise `None`. No extra queries — it folds into the row handler already iterating thread messages.
- [~] Task 5. Add storage tests covering each classification and the "thread among other people only" case mapping to `None`.

### Phase 3 — Participation-aware counting policy (filter at read time, not write time)

- [ ] Task 6. Keep `bump_thread_unread` (`crates/storage/src/lib.rs:959-972`) writing counters for all threads, and instead filter at aggregation/display time. Rationale: filtering on read keeps raw data intact, makes the policy retroactive, and allows a future "show all" toggle without data loss.
- [ ] Task 7. Change `refresh_thread_unread_by_chat` (`crates/tui/src/app.rs:9620-9639`) to aggregate only summaries with `participation != None` into the bold `⤷N` map. Apply the same filter to the in-memory increment inside `mark_live_thread_reply_unread_if_needed` (`crates/tui/src/app.rs:10333-10337`) — the live bump must check participation (cheap: the arriving message's `mentions_me`/`is_from_me`, plus a cached per-thread participation set for the selected chat, falling back to a bounded async storage lookup off the draw path per the performance rules).
- [ ] Task 8. Render distinction in `chat_item` (`crates/tui/src/widgets/chat_list.rs:863-916`): participating threads keep the current bold/unread-styled `⤷N`; non-participating thread activity renders as a single dim `⤷` glyph with no count (muted theme color), so the channel still hints "threads are alive here" without implying the user owes attention. Provide a setting (`thread_marker_scope`: `participating` default | `all` | `none`) in the existing settings overlay for users who want the old behavior or total silence.

### Phase 4 — Direct navigation from sidebar to thread

- [ ] Task 9. Add a "jump to unread thread" action: with a chat selected in the chat list (or focused in Messages), a dedicated key (suggest `t`) opens the chat if needed and directly invokes `open_thread` (`crates/tui/src/app.rs:10952-10985`) on the most recent unread participating thread root; repeated presses cycle to the next unread thread in that chat. Selection/scroll must update immediately and message loading stays async per the responsiveness rules.
- [ ] Task 10. Make the `⤷N` marker itself clickable: extend the chat-list mouse hit-testing (same pattern as `account_badge_chat_at`, `crates/tui/src/widgets/chat_list.rs:657-696`) so a click on the marker region of a chat row triggers the same jump-to-thread action instead of a plain chat open.
- [ ] Task 11. Upgrade the Threads inbox (`crates/tui/src/app.rs:9644-9699`): default to participating threads only; add a toggle key (suggest `a`) to switch between "My threads" and "All threads", reflected in the overlay title; show a participation badge per entry (e.g. `you replied` / `mentioned`). Keep `Up`-at-top entry but also surface the inbox in the help/discoverable-controls panel so it is findable.
- [ ] Task 12. Status-line affordance: when a selected chat has unread participating threads, append a short hint (e.g. `t: open thread ⤷N`) so the new shortcut is discoverable in context.

### Phase 5 — Validation

- [ ] Task 13. App-level tests: live reply in a non-participating thread does not appear in `thread_unread_by_chat`; reply in a participating thread does; `t` jump opens the correct root; marker click hit-testing resolves the right chat and triggers thread open; both phases (immediate placeholder, post-drain final state) asserted for the async jump.
- [ ] Task 14. Run the CI-mirrored checks locally (`cargo check`, `cargo test`, `cargo clippy`, release build as configured in `.github/workflows/ci.yml`) before commit.

## Verification Criteria

- A thread where the user never wrote and was never mentioned produces no bold `⤷N` in the sidebar (dim glyph or nothing, per setting).
- A live reply to a thread the user authored/replied in/was mentioned in increments the bold `⤷N` immediately without a draw-path DB read.
- Pressing `t` (or clicking the marker) lands inside the most recent unread participating thread with its unread counter cleared via the existing `flush_pending_thread_read` path.
- Threads inbox defaults to participating threads and can toggle to all threads.
- All existing thread lifecycle tests still pass; new tests cover participation classification and navigation.

## Potential Risks and Mitigations

1. **Mention participation invisible for pre-migration history** — `mentions_me` was never stored.
   Mitigation: rely on `is_from_me` (fully retroactive) as the primary signal; treat mentions as additive enrichment going forward. Optionally re-derive mentions during provider history replay since replays rewrite message rows.
2. **Live bump needs participation knowledge without blocking the event loop.**
   Mitigation: maintain a small cached set of participating thread roots per chat (refreshed in the existing off-draw `refresh_thread_unread_*` passes); on cache miss, bump optimistically into a pending bucket and reconcile in the next async refresh.
3. **Users who relied on the old "all thread activity" signal lose information.**
   Mitigation: the `thread_marker_scope = all` setting restores current behavior; the dim glyph keeps an ambient hint by default.
4. **Slack's real subscription state (threads followed via the official client) is not exposed by documented APIs.**
   Mitigation: local participation heuristic (authored/replied/mentioned) matches Slack's own default auto-subscribe rules closely; document the divergence (manually followed threads in the Slack app won't be detected).

## Alternative Approaches

1. **Filter at write time (only bump counters for participating threads)**: simpler hot path, but destroys data needed for the `all` scope toggle and is not retroactive when policy changes. Rejected in favor of read-time filtering.
2. **Use Slack's undocumented `subscriptions.thread.*` endpoints for true follow state**: highest fidelity, but unofficial, brittle, and token-scope dependent. Could be layered in later behind the provider abstraction.
3. **Two separate counters in the sidebar (`⤷N` mine / `⤷M` others)**: maximal information, but consumes scarce row width and reintroduces the noise the user wants gone. The dim-glyph compromise conveys the same at lower cost.
