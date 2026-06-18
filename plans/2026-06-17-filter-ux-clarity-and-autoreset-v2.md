# Filter UX Clarity, Honest Active-State, Enter-Optional Open, and Inactivity Auto-Reset

## Objective

Make filtering in the TUI predictable and unmistakable. Eliminate the confusion where a
filter typed in one pane silently re-applies in another, make it always obvious when a
filter is active (and in which scope), give filtering a single coherent lifecycle, remove
the unnecessary mandatory Enter to open a filtered result, and add an opt-in inactivity
auto-reset as a safety net. All changes must respect the responsiveness, ordering, and
safety rules in `AGENTS.md` (no DB/network/layout work on the input or draw hot path;
timeout checks belong in the existing tick handler; history loads must stay debounced).

## Root-Cause Summary (evidence)

- Single shared query + scope, not per-pane filters: `crates/tui/src/app.rs:2195-2196`,
  `crates/tui/src/app.rs:756-779`.
- Focus change re-targets the same query to a new scope:
  `crates/tui/src/app.rs:10597-10609`.
- "Active" predicates ignore `filter_mode`, so Esc leaves the filter silently applied:
  `crates/tui/src/app.rs:10611-10623`, `crates/tui/src/app.rs:10770-10774`.
- Full reset only via a later Esc at chat list -> `clear_filter`:
  `crates/tui/src/app.rs:11740`, `crates/tui/src/app.rs:11818-11832`.
- Inconsistent/weak title indicators: `crates/tui/src/widgets/chat_list.rs:1656-1662`,
  `crates/tui/src/app.rs:4764-4779`.
- Tick hook available for timeout (~250ms cadence): `crates/tui/src/app.rs:74`,
  `crates/tui/src/app.rs:14347`.

### Why Enter is currently required (and why it should not be, just to open)

- Filtering is already live (applied per keystroke): `crates/tui/src/app.rs:10803-10812`.
- Navigation loads are suppressed while in filter mode, so highlighting a result does NOT
  load it: `should_schedule_navigation_load` returns false during filter mode,
  `crates/tui/src/app.rs:2908-2909`.
- Enter exists purely to commit/open: it exits filter mode and fires the deferred chat load
  (`load_selected_after_filter`, `crates/tui/src/app.rs:10660-10663`) or opens the message
  action menu (`crates/tui/src/app.rs:10666-10668`).
- A debounce mechanism already exists to load on navigation without per-keystroke DB hits:
  `NAVIGATION_LOAD_DEBOUNCE` = 120ms, `crates/tui/src/app.rs:87`.

## Implementation Plan

### Phase 1 - Decouple the query from scope (stop cross-pane leakage)

- [ ] Task 1. Decide and document the ownership model for the active filter. Recommended:
  a filter is owned by exactly one scope at a time (the scope it was entered in), and
  changing focus does NOT transplant the typed text into a different scope. Rationale: the
  current shared-buffer model in `crates/tui/src/app.rs:2195-2196` is the direct cause of
  the "also active in messages" symptom.
- [ ] Task 2. Change `sync_filter_scope_to_focus` (`crates/tui/src/app.rs:10597-10609`) so
  that moving focus while in filter mode either (a) keeps the filter bound to its original
  scope and stops applying it to the newly focused pane, or (b) swaps in a separate
  per-scope query buffer. Recommended approach: introduce per-scope query buffers (e.g., a
  small struct holding `chats`, `messages`, `thread` strings) so each pane remembers its
  own filter independently and none bleeds across panes.
- [ ] Task 3. Update all read sites of `self.state.filter` to read the active scope's buffer
  via a single accessor (e.g., `active_filter_query()`), so the draw path and predicates
  never reference a cross-scope value. Audit every `self.state.filter` reference found in
  `crates/tui/src/app.rs` (status, titles, thread matching, discovery) and route through the
  accessor.

### Phase 2 - Make the active state honest and the lifecycle coherent

- [ ] Task 4. Introduce an explicit, unambiguous notion of "filter applied" that is
  independent of whether the user is currently typing. Replace the implicit
  "non-empty string == active" logic in `chat_filter_active` / `message_filter_active` /
  `thread_filter_active` (`crates/tui/src/app.rs:10611-10623`) with state that distinguishes
  three conditions per scope: (i) no filter, (ii) filter being edited (`filter_mode`),
  (iii) filter applied but not being edited.
- [ ] Task 5. Redefine the Esc lifecycle so it is single-stage and predictable. When Esc is
  pressed while editing a filter (`handle_filter_key`, `crates/tui/src/app.rs:10770-10774`),
  it should fully clear that scope's filter and exit filter mode in one action, rather than
  leaving the query silently applied. Confirm the `handle_escape` cascade
  (`crates/tui/src/app.rs:11684-11741`) no longer relies on a second Esc to reach
  `clear_filter`.
- [ ] Task 6. Ensure `clear_filter` (`crates/tui/src/app.rs:11818-11832`) clears only the
  focused/active scope's buffer (and its derived `filtered_messages`/`discovery_results`),
  preserving selection and scroll for that pane per `AGENTS.md`.

### Phase 3 - Make Enter optional for opening (remove mandatory commit step)

- [ ] Task 7. Allow debounced navigation loads to run while in filter mode by relaxing
  `should_schedule_navigation_load` (`crates/tui/src/app.rs:2908-2909`) so that selecting a
  chat result during filtering schedules a debounced load via the existing
  `NAVIGATION_LOAD_DEBOUNCE` path (`crates/tui/src/app.rs:87`). This keeps loads off the
  per-keystroke hot path (load only after the user pauses on a selection) while letting the
  highlighted chat open without an explicit Enter.
- [ ] Task 8. Redefine Enter as an optional, scope-specific "act on selection" command rather
  than a mandatory open step: in Chats scope, Enter simply confirms/finishes filter mode
  (the chat is already loading from Task 7); in Messages scope, Enter still opens the action
  menu (`crates/tui/src/app.rs:10666-10668`); in Thread scope, Enter remains a no-op/status.
  Update `confirm_filter_selection` (`crates/tui/src/app.rs:10651-10678`) accordingly and
  preserve the discovery-result open path when there are zero local matches.
- [ ] Task 9. Verify and tune the debounce interaction with rapid scrubbing through many
  filtered chats: ensure only the settled selection triggers a load, stale loads are
  discarded by the existing generation/request-token logic, and selection/scroll are
  preserved per `AGENTS.md`. Add instrumentation labels for filter-mode navigation loads.
- [ ] Task 10. Update in-app help and status hints so the new model is clear: typing filters
  live, highlighting opens (after a brief pause), Enter acts on a message, Esc clears.
  Reconcile the hint strings in `enter_filter_mode` (`crates/tui/src/app.rs:11650-11653`).

### Phase 4 - Make active filters unmistakable in the UI

- [ ] Task 11. Design a single, consistent "filter chip" indicator used across panes:
  show the scope label, the query text, and the live match count, plus an explicit
  "Esc to clear" affordance. Use a visually distinct style (e.g., reversed/highlighted)
  reserved for active filters so it cannot be confused with normal titles.
- [ ] Task 12. Apply the chip to the chat-list title, replacing the inconsistent always-on
  "· Filter:" branch in `crates/tui/src/widgets/chat_list.rs:1656-1662`. Only render the
  chip when that pane actually has an applied/editing filter.
- [ ] Task 13. Apply the same chip to the message-list title, replacing the ad-hoc
  formatting in `crates/tui/src/app.rs:4764-4779`, and to the thread/details filter header
  (`crates/tui/src/app.rs:5112-5113`, `crates/tui/src/app.rs:6062-6063`).
- [ ] Task 14. Add a global status-bar indicator present whenever ANY filter is applied
  (even when not in filter mode), naming the scope and query. Reconcile with the existing
  status composition around `crates/tui/src/app.rs:5219-5258`.
- [ ] Task 15. Differentiate the three states visually: editing, applied-but-idle, and none,
  so the user always understands whether the list is still filtered.

### Phase 5 - Inactivity auto-reset (opt-in safety net)

- [ ] Task 16. Add a `filter_last_interaction: Instant` timestamp to app state, updated on
  every filter keystroke, scope change, and filter-driven navigation
  (`crates/tui/src/app.rs:10767-10815`).
- [ ] Task 17. In `handle_tick` (`crates/tui/src/app.rs:14347`), compare elapsed time
  against a configurable timeout (default 60s) and, when exceeded with a filter applied,
  clear the filter via the same path as manual clear. Do this only in the tick handler per
  `AGENTS.md`. Preserve selection and scroll on reset unless the cleared filter forces a
  selection change.
- [ ] Task 18. Make the timeout configurable in settings (including disable), mirroring other
  toggles in the settings overlay. Recommended default: enabled at 60s. Show a brief status
  message when an auto-reset occurs so the change is not silent.
- [ ] Task 19. Decide scope coverage for auto-reset. Recommended: auto-reset message and
  thread filters and optionally chat-list filters; ensure resetting a chat filter does not
  regress sidebar ordering (`AGENTS.md` activity/ordering rules).

### Phase 6 - Validation and instrumentation

- [ ] Task 20. Add/extend tests asserting: typing a chat filter then moving focus does NOT
  filter messages; a single Esc fully clears the focused filter; highlighting a filtered
  chat triggers a debounced load without Enter (assert immediate placeholder/loading state,
  then final loaded state after draining); Enter in Messages scope opens the action menu;
  and auto-reset fires after the configured idle window. Follow the two-phase async test
  guidance in `AGENTS.md`.
- [ ] Task 21. Add opt-in performance/diagnostic instrumentation around the new filter
  lifecycle and filter-mode navigation loads (scope, applied/editing flags, match counts,
  debounce hits, auto-reset events), consistent with `AGENTS.md`.
- [ ] Task 22. Before any commit, mirror CI locally per `AGENTS.md`: inspect
  `.github/workflows/ci.yml` and run the required `cargo check`, `cargo test`,
  `cargo clippy`, and release-build commands.

## Verification Criteria

- Typing a filter in the chat list and then moving focus to Messages leaves the message list
  unfiltered; the chat filter remains bound to and indicated only on the chat list.
- Highlighting a chat while filtering opens it (its messages load) after a brief debounce,
  with no Enter required; Enter is no longer mandatory just to view a result.
- Enter retains a clear purpose: confirming/finishing in Chats scope and opening the action
  menu in Messages scope; the discovery-open path still works when there are no local
  matches.
- At all times the UI shows an unmistakable, consistently styled indicator (pane chip plus
  status bar) whenever a filter is applied, including when the user is not actively typing.
- A single Esc while editing a filter fully clears that scope's filter and exits filter mode.
- With auto-reset enabled, an applied filter clears after the configured idle window
  (default 60s) via the tick handler, with selection/scroll preserved and a brief status
  notice; disabling the setting prevents auto-reset.
- No filtering or history-load work is added to the input or draw hot paths; filter-mode
  loads go through the existing debounce, and the timeout check lives only in `handle_tick`.
- All required CI checks pass locally.

## Potential Risks and Mitigations

1. **Allowing loads during filter mode could reintroduce per-keystroke DB/history work.**
   Mitigation: route exclusively through the existing `NAVIGATION_LOAD_DEBOUNCE` so only a
   settled selection loads; rely on generation/request tokens to discard stale loads; add
   instrumentation (Task 21) to confirm load frequency stays bounded.
2. **Per-scope buffers increase state surface and could desync from derived collections.**
   Mitigation: centralize reads through one accessor; recompute derived collections only from
   the active scope's buffer; cover with tests in Task 20.
3. **Changing Enter/Esc semantics may surprise existing muscle memory.**
   Mitigation: keep the `handle_escape` cascade order intact; document the new model in help
   and status hints (Task 10).
4. **Auto-reset could clear a filter the user still wants.**
   Mitigation: opt-in/configurable; reset the idle timer on any filter interaction and on
   scrolling within the filtered pane; surface a status notice on reset.
5. **Auto-reset of a chat-list filter might disturb sidebar ordering or selection.**
   Mitigation: route through the selection-preserving clear path and honor activity/ordering
   rules; consider excluding chat-list filters from auto-reset.
6. **Indicator styling may clash with themes or overflow narrow titles.**
   Mitigation: reuse existing title/status primitives, truncate long queries, verify narrow
   layouts.

## Alternative Approaches

1. **Keep mandatory Enter but make it discoverable.** Leave the commit step as-is and only
   add a strong "Press Enter to open" hint plus the honest indicators. Trade-off: simplest,
   preserves current performance guarantees exactly, but keeps the extra step the user is
   questioning.
2. **Single shared query, block cross-scope carry-over only.** Smallest leakage fix without
   per-scope buffers. Trade-off: simpler, but the chat filter is lost when moving to messages.
3. **Explicit confirm-to-apply model.** Filters apply only when confirmed and show a
   persistent dismissible chip. Trade-off: most unambiguous but a larger redesign and slower
   for power users who like live filtering.
4. **No auto-reset; rely on honest indicators and one-Esc clear.** Trade-off: avoids surprise
   clears but drops the requested safety net for forgotten filters.

## Assumptions

- "Reset the filter after 1 minute of inactivity" means clearing the applied filter (and
  exiting filter mode) after ~60s with no filter-related interaction; configurable, 60s
  default.
- Removing mandatory Enter-to-open is desirable as long as history loads remain debounced;
  the user's question implies the extra commit step feels unnecessary.
- The primary pains are cross-pane leakage, the silent residual filter after Esc, and the
  mandatory Enter; indicator/lifecycle/Enter fixes are prioritized above the timeout.
- Existing tick cadence (~250ms) is acceptable granularity for a minute-scale timeout.
