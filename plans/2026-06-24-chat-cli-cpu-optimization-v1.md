# chat-cli CPU / Battery Optimization

## Objective

Reduce chat-cli's CPU consumption (improving battery life) **without** removing any
functionality and **without** degrading TUI responsiveness. All changes must preserve the
`AGENTS.md` performance rules: keep the event loop responsive, keep draw code cache/placeholder
only, keep provider drains bounded, and preserve selection/scroll state.

## Initial Assessment

### Project structure summary
- TUI lives in `crates/tui` (driving loop + all rendering in `crates/tui/src/app.rs`, message
  rendering in `crates/tui/src/widgets/message_list.rs`).
- Providers (`crates/providers/slack`, `crates/providers/whatsapp`) run background network tasks.
- Runtime is multi-threaded Tokio (`crates/chat-cli/src/main.rs:134`).

### How CPU is actually spent (evidence-based)
The event loop is **not** a busy loop: when idle it blocks in `event::poll(idle_timeout)` for up
to 250 ms and only redraws when `needs_draw` is set (`crates/tui/src/app.rs:16710-16715`,
`crates/tui/src/app.rs:16750-16768`). Therefore idle CPU is already low, and the real cost is in:

1. **Per-draw whole-history revalidation (largest avoidable cost).** Every redraw recomputes
   `messages_layout_hash` over the *entire* history (up to `SELECTED_CHAT_MESSAGE_LIMIT = 5000`
   messages, hashing full text + reactions + receipts per message) — and it runs **3-4 times per
   single `draw()`** via scroll-clamp, scroll-to-bottom, the line-count pass, and the build pass
   (`crates/tui/src/widgets/message_list.rs:721-754`; call sites traced to
   `crates/tui/src/app.rs:4184`, `crates/tui/src/app.rs:4186`, `crates/tui/src/app.rs:4877`,
   `crates/tui/src/app.rs:4902`). During LLM streaming or provider bursts, every token/event
   triggers a redraw, so this O(history) hashing dominates active-use CPU.

2. **Per-frame allocations during active redraws.** Visible markdown bodies are re-parsed and
   re-wrapped every draw with a `String` allocated per segment and per word
   (`crates/tui/src/widgets/message_list.rs:3481-3496`,
   `crates/tui/src/widgets/message_list.rs:3571-3634`); a fresh `HashSet` of unread ids is rebuilt
   every draw (`crates/tui/src/app.rs:4900`, `crates/tui/src/app.rs:11447-11451`); and HD overlay
   hit vectors are fully cloned every draw (`crates/tui/src/app.rs:7869`,
   `crates/tui/src/app.rs:8022`).

3. **Synchronous filesystem stats in the draw path.** `path.exists()` / `is_supported_image`
   calls run while building visible lines and hits
   (`crates/tui/src/widgets/message_list.rs:3078-3108`,
   `crates/tui/src/widgets/message_list.rs:95-125`, avatar gating at
   `crates/tui/src/widgets/message_list.rs:514` and `:684`). Bounded to visible messages but still
   blocking syscalls per frame, violating "draw must be cache/placeholder only."

4. **Steady idle wakeups.** The 250 ms idle poll fires a `Tick` ~4x/sec
   (`IDLE_POLL_TIMEOUT`, `crates/tui/src/app.rs:74`), and `handle_tick` performs an async DB read
   (`notification_pause_state`) every ~1s even when nothing is happening
   (`crates/tui/src/app.rs:14513-14525`). Each wake is cheap, but constant wakeups prevent the CPU
   from staying in low-power idle states — directly relevant to battery.

5. **Always-on provider polling.** Slack DM poll runs every 8s unconditionally
   (`crates/providers/slack/src/lib.rs:4862-4877`). Network-bound and modest, but it is a constant
   periodic wake that can be made adaptive.

### Prioritization rationale
Ranked by expected CPU saving per unit of risk: (1) eliminates the single biggest repeated
computation and is purely internal; (2) and (3) cut active-redraw cost; (4) and (5) cut idle
wakeups for battery. Items 1-3 give the largest wins during active use; 4-5 help sustained idle
battery drain.

## Implementation Plan

### Phase 1 — Eliminate per-draw whole-history hashing (highest impact)

- [ ] Task 1. Replace the content-hash cache key with a monotonic revision counter. Introduce a
  `messages_revision: u64` on the app/chat state that is incremented at every existing
  `clear_message_layout_cache()` call site and at every message mutation (append, edit, delete,
  reaction/receipt update, history merge). Swap `MessageLayoutKey.messages_hash` for this revision
  in `crates/tui/src/widgets/message_list.rs:184-219`. Rationale: proving the cache is still valid
  currently costs O(total history) every frame; a revision counter makes validity checks O(1) while
  remaining content-correct, because the cache is already explicitly cleared on every mutation.

- [ ] Task 2. Collapse the redundant per-draw revalidation passes into one. Audit the 3-4
  invocations within a single `draw()` (scroll clamp at `crates/tui/src/app.rs:4184`,
  scroll-to-bottom at `crates/tui/src/app.rs:4186`, line-count pass at `crates/tui/src/app.rs:4877`,
  build pass at `crates/tui/src/app.rs:4902`) and ensure the layout cache is validated/rebuilt at
  most once per frame, with subsequent reads hitting the warm cache. Rationale: even with an O(1)
  key, avoiding repeated cache lookups and rebuild branches per frame removes duplicated work and
  simplifies reasoning.

- [ ] Task 3. Keep the existing `messages_layout_hash` path available behind the revision system as
  a debug/verification aid only (not on the hot path), or remove it if fully superseded. Rationale:
  preserve a correctness fallback during rollout without paying its per-frame cost.

### Phase 2 — Reduce per-frame allocation during active redraws

- [ ] Task 4. Memoize styled markdown wrapping per visible message. Cache `wrap_markdown_text`
  output keyed by `(message_id, content_width, style/presentation revision)` so scrolling and
  streaming redraws over unchanged bodies reuse parsed/wrapped spans instead of re-tokenizing and
  re-allocating every frame (`crates/tui/src/widgets/message_list.rs:3481-3496`). Rationale: removes
  repeated markdown parsing + per-word `String` allocation from the steady redraw path; invalidate
  by the same revision used in Phase 1.

- [ ] Task 5. Cache the unread-id set and invalidate on change. Replace the per-draw rebuild of
  `unread_message_ids()` (`crates/tui/src/app.rs:4900`, `crates/tui/src/app.rs:11447-11451`) with a
  cached set recomputed only when read state or message set changes. Rationale: avoids a full
  reverse-iteration + `HashSet<Arc<str>>` allocation on every frame.

- [ ] Task 6. Avoid cloning HD overlay hit vectors each draw. Replace the full clones at
  `crates/tui/src/app.rs:7869` and `crates/tui/src/app.rs:8022` with a borrow-friendly approach
  (drain/swap into a scratch buffer, index-based access, or splitting state borrows) so
  `media_hits`/`message_hits` are not deep-cloned per frame. Rationale: removes per-frame
  `PathBuf`/`String`/`Vec` allocations while keeping identical behavior.

### Phase 3 — Move filesystem work off the draw path

- [ ] Task 7. Hoist `path.exists()` / `is_supported_image` decisions into the async
  load/merge pipeline. Store boolean flags (e.g. `local_present`, `is_previewable`) on the media/
  message state when messages are loaded or media is retrieved, and have the draw build pass read
  those flags instead of calling the filesystem
  (`crates/tui/src/widgets/message_list.rs:3078-3108`, `:95-125`, `:514`, `:684`). Rationale:
  removes blocking syscalls from the draw path (AGENTS.md compliance) and prevents frame stalls on
  slow/networked filesystems; recompute flags on the existing media-fetch completion events.

### Phase 4 — Reduce idle wakeups for battery

- [ ] Task 8. Make the idle poll timeout adaptive. When nothing time-based is pending (no visible
  notification countdown, no network-activity pulse, no deferred navigation, no active streaming),
  extend the blocking `event::poll` timeout well beyond 250 ms (e.g. up to ~1s) and shrink it back
  to the responsive value when any time-based UI element is active
  (`crates/tui/src/app.rs:16746-16768`, `crates/tui/src/app.rs:74`). Rationale: `event::poll`
  returns immediately on real input regardless of timeout, so input responsiveness is unchanged; a
  longer idle timeout simply reduces idle `Tick` wakeups (~4x/sec → ~1x/sec), letting the CPU stay
  in low-power states longer. Document the chosen bounds as named constants.

- [ ] Task 9. Decouple periodic background bookkeeping from the poll cadence. Drive notification-
  pause reload, due-notification/voice drains, and TTL countdowns from elapsed wall-clock checks
  inside `handle_tick` rather than a fixed tick count, so that lengthening the idle timeout (Task 8)
  does not change their effective real-time cadence
  (`crates/tui/src/app.rs:14510-14546`). Rationale: preserves existing functional behavior/timing
  while allowing fewer wakeups.

- [ ] Task 10. Gate or lengthen the idle notification-pause DB read. Skip the
  `notification_pause_state()` DB read when the pause state cannot have changed, or back its cadence
  off when the app is idle/unfocused (`crates/tui/src/app.rs:14513-14525`). Rationale: removes a
  recurring async DB hit during pure idle, a direct battery saver.

### Phase 5 — Adaptive provider polling (optional, lower impact)

- [ ] Task 11. Add adaptive backoff to the Slack DM poll. When the workspace has been quiet for a
  while, lengthen the 8s `SLACK_DM_POLL_INTERVAL` toward a higher ceiling, resetting to the fast
  interval on any detected activity (`crates/providers/slack/src/lib.rs:73`,
  `crates/providers/slack/src/lib.rs:4862-4877`). Rationale: reduces always-on network + wake
  frequency during idle periods without losing DM delivery responsiveness when conversations are
  active.

- [ ] Task 12. Consider coalescing redraws during high-frequency streaming. Introduce a minimum
  inter-frame interval (frame budget) so that a burst of provider/streaming events produces at most
  one redraw per small time window instead of one redraw per drained event
  (`crates/tui/src/app.rs:16735-16741`). Rationale: bounds redraw frequency under token streaming
  while keeping the existing bounded-drain semantics; must preserve "apply async results only when
  still current" and not delay user-visible input echo.

### Phase 6 — Validation and instrumentation

- [ ] Task 13. Add/extend opt-in perf instrumentation for the changed paths. Ensure labels exist
  for layout-cache validation, markdown-wrap cache hit/miss, and idle wakeup cadence, with counts
  and chat/account identifiers, per AGENTS.md. Rationale: provides before/after evidence and guards
  against regressions; targets the largest blocking label first
  (`draw.messages.line_count`, `terminal.draw`).

- [ ] Task 14. Run the repository's required CI checks locally before any commit. Mirror
  `.github/workflows/ci.yml` (`cargo check`, `cargo test`, `cargo clippy`, release build as
  applicable). Rationale: AGENTS.md mandates matching CI locally to avoid preventable failures.

## Verification Criteria

- Measured idle CPU usage drops (fewer wakeups/sec) with no change to input latency, verified by
  perf logs showing reduced `Tick` cadence when idle and unchanged input-event handling time.
- During LLM streaming / provider bursts in a large chat (thousands of messages), `terminal.draw`
  and `draw.messages.line_count` durations drop substantially and no longer scale with total
  history size — only with visible window size.
- No `path.exists()` / filesystem syscalls occur in the draw build pass (verified by code audit and
  perf instrumentation); media previews and avatars still render correctly via async flags.
- Scrolling within an unchanged history shows markdown-wrap cache hits (no re-parse) in
  instrumentation, with identical rendered output.
- All existing tests pass; async UI tests still assert both phases (immediate placeholder, then
  final state after draining background completions).
- Selection and scroll state are preserved across background completions and adaptive timeout
  changes.
- `cargo check`, `cargo test`, `cargo clippy`, and the release build all pass locally.

## Potential Risks and Mitigations

1. **Revision counter misses a mutation site, causing stale rendering.**
   Mitigation: enumerate all current `clear_message_layout_cache()` call sites and message-mutation
   paths as the authoritative list; add a debug-only assertion (Task 3) that periodically compares
   the revision-based key against the legacy content hash during testing.
2. **Markdown-wrap cache grows unbounded in long chats.**
   Mitigation: bound the cache (e.g. LRU keyed by message id + width) and invalidate on the shared
   revision; only cache visible/recently-visible messages.
3. **Longer idle poll timeout delays time-based UI (notification countdown, pulses).**
   Mitigation: keep the timeout short whenever any time-based element is active (Task 8) and drive
   bookkeeping by wall-clock elapsed (Task 9) so functional timing is preserved regardless of poll
   cadence.
4. **Adaptive Slack backoff delays DM visibility.**
   Mitigation: reset to the fast interval immediately on any realtime/activity signal; cap the
   maximum backoff conservatively.
5. **Redraw coalescing introduces perceptible lag or drops a final frame.**
   Mitigation: always force a redraw on input and on stream completion; keep the frame budget small;
   make Task 12 optional and gate it behind measurement showing redraw frequency is a real cost.
6. **Borrow-checker refactor for hit vectors (Task 6) changes behavior subtly.**
   Mitigation: keep the data identical; only change ownership/lifetime handling, covered by existing
   overlay/interaction tests.

## Alternative Approaches

1. **Cache the layout hash for the duration of one frame only** (compute once per `draw`, reuse
   across the 3-4 internal call sites) instead of a full revision-counter rewrite. Lower risk and
   smaller change, but still O(history) once per frame — a partial win versus Task 1's O(1).
2. **Incremental/append-only layout cache**: update only the tail of the layout cache when messages
   are appended rather than rebuilding. Larger win for very long chats during streaming, but more
   complex and higher regression risk; consider as a follow-up after Phase 1.
3. **Pre-render messages into cached `Line` buffers on mutation** (fully move markdown parsing out of
   draw entirely) rather than memoizing per-frame. Strongest draw-path reduction but a larger
   architectural change; Task 4's memoization is the lower-risk first step toward it.
4. **Event-driven Slack DMs via a user-token path** instead of polling, eliminating the 8s wake
   entirely. Largest idle win for Slack but depends on auth scope availability and is a bigger
   feature change than adaptive backoff (Task 11).
