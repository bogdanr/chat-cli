# Filter UX Clarity, Honest Active-State, and Inactivity Auto-Reset

## Objective

Make filtering in the TUI predictable and unmistakable. Eliminate the confusion where a
filter typed in one pane silently re-applies in another, make it always obvious when a
filter is active (and in which scope), give filtering a single coherent lifecycle, and add
an opt-in inactivity auto-reset as a safety net. All changes must respect the
responsiveness, ordering, and safety rules in `AGENTS.md` (no DB/network/layout work on the
input or draw hot path; timeout checks belong in the existing tick handler).

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
  preserving selection and scroll for that pane per `AGENTS.md`. Verify it no longer assumes
  a single global string.
- [ ] Task 7. Keep Enter behavior intentional: Enter "confirms" a selection/action without
  silently leaving an invisible residual filter; if a filter stays applied after Enter, it
  must remain visibly indicated (see Phase 3).

### Phase 3 - Make active filters unmistakable in the UI

- [ ] Task 8. Design a single, consistent "filter chip" indicator used across panes:
  show the scope label, the query text, and the live match count, plus an explicit
  "Esc to clear" affordance. Use a visually distinct style (e.g., reversed/highlighted)
  reserved for active filters so it cannot be confused with normal titles.
- [ ] Task 9. Apply the chip to the chat-list title, replacing the inconsistent always-on
  "· Filter:" branch in `crates/tui/src/widgets/chat_list.rs:1656-1662`. Only render the
  chip when that pane actually has an applied/editing filter.
- [ ] Task 10. Apply the same chip to the message-list title, replacing the ad-hoc
  formatting in `crates/tui/src/app.rs:4764-4779`, and to the thread/details filter header
  (`crates/tui/src/app.rs:5112-5113`, `crates/tui/src/app.rs:6062-6063`).
- [ ] Task 11. Add a global status-bar indicator that is present whenever ANY filter is
  applied (even when the user is not in filter mode), naming the scope and query, so the
  user can never lose track of a lingering filter. Reconcile with the existing status
  composition around `crates/tui/src/app.rs:5219-5258`.
- [ ] Task 12. Differentiate the three states visually: editing (cursor/active prompt),
  applied-but-idle (static chip), and none (no chip). Make the "applied-but-idle" state
  clearly readable so the user understands the list is still filtered.

### Phase 4 - Inactivity auto-reset (opt-in safety net)

- [ ] Task 13. Add a `filter_last_interaction: Instant` timestamp to app state, updated on
  every filter keystroke, scope change, and filter-driven navigation (the handlers in
  `crates/tui/src/app.rs:10767-10815` and the apply paths).
- [ ] Task 14. In `handle_tick` (`crates/tui/src/app.rs:14347`), compare elapsed time
  against a configurable timeout (default 60s) and, when exceeded with a filter applied,
  clear the filter via the same path as manual clear. Do this only in the tick handler to
  keep it off the input/draw hot path per `AGENTS.md`. Preserve current selection and scroll
  on reset unless the cleared filter forces a selection change.
- [ ] Task 15. Make the timeout configurable in settings (including an option to disable it),
  mirroring how other toggles are surfaced in the settings overlay. Recommended default:
  enabled at 60s. Show a brief, non-intrusive status message when an auto-reset occurs so
  the change is not silent.
- [ ] Task 16. Decide scope coverage for auto-reset. Recommended: auto-reset message and
  thread filters (transient, in-chat) and optionally chat-list filters; document the chosen
  behavior. Ensure resetting a chat filter does not regress sidebar ordering
  (`AGENTS.md` activity/ordering rules).

### Phase 5 - Validation and instrumentation

- [ ] Task 17. Add/extend tests asserting the corrected lifecycle: typing a chat filter then
  moving focus must NOT filter messages; a single Esc fully clears the focused filter; the
  applied-but-idle indicator state is reported correctly; and auto-reset fires after the
  configured idle window. Follow the two-phase async test guidance in `AGENTS.md`
  (immediate state, then state after draining ticks/completions).
- [ ] Task 18. Add opt-in performance/diagnostic instrumentation around the new filter
  lifecycle (scope, applied/editing flags, match counts, auto-reset events) consistent with
  the instrumentation guidance in `AGENTS.md`.
- [ ] Task 19. Before any commit, mirror CI locally per `AGENTS.md`: inspect
  `.github/workflows/ci.yml` and run the required `cargo check`, `cargo test`,
  `cargo clippy`, and release-build commands.

## Verification Criteria

- Typing a filter in the chat list and then moving focus to Messages leaves the message list
  unfiltered; the chat filter remains bound to and indicated only on the chat list.
- At all times the UI shows an unmistakable, consistently styled indicator (pane chip plus
  status bar) whenever a filter is applied, naming the scope and query, including when the
  user is not actively typing.
- A single Esc while editing a filter fully clears that scope's filter and exits filter
  mode; no hidden residual filter remains.
- With auto-reset enabled, an applied filter clears after the configured idle window
  (default 60s) via the tick handler, with selection/scroll preserved and a brief status
  notice; disabling the setting prevents auto-reset.
- No filtering work (string matching beyond bounded current data, layout, DB) is added to
  input or draw hot paths; the timeout check lives only in `handle_tick`.
- All required CI checks pass locally.

## Potential Risks and Mitigations

1. **Per-scope buffers increase state surface and could desync from derived collections
   (`filtered_messages`, `discovery_results`).**
   Mitigation: centralize all reads through one accessor and recompute derived collections
   only from the active scope's buffer; cover with tests in Task 17.
2. **Changing Esc semantics may surprise existing muscle memory or other Esc consumers.**
   Mitigation: keep the global `handle_escape` cascade order intact; only change the
   filter-clearing stage, and document the new single-stage behavior in the in-app help.
3. **Auto-reset could clear a filter the user still wants (e.g., reading a long filtered
   thread).**
   Mitigation: make it opt-in/configurable, reset the idle timer on any filter-related
   interaction and scrolling within the filtered pane, and surface a status notice on reset.
4. **Auto-reset of a chat-list filter might disturb sidebar ordering or selection.**
   Mitigation: route through the same selection-preserving clear path and honor the
   activity/ordering rules; consider excluding chat-list filters from auto-reset.
5. **Indicator styling may clash with themes or overflow narrow titles.**
   Mitigation: reuse existing title/status styling primitives, truncate long queries with an
   ellipsis, and verify in narrow-terminal layouts.

## Alternative Approaches

1. **Keep a single shared query but block cross-scope carry-over only.** Smallest change:
   stop `sync_filter_scope_to_focus` from re-applying the query to a new scope, and tie
   active-state to `filter_mode`. Trade-off: less flexible than per-scope buffers (user
   loses the chat filter when moving to messages), but far simpler and still fixes the core
   confusion.
2. **Explicit confirm-to-apply model.** Filters only apply once explicitly confirmed and
   show a persistent dismissible chip; no implicit live filtering. Trade-off: most
   discoverable and unambiguous, but a larger interaction redesign and slower for power
   users who like live filtering.
3. **No auto-reset; rely solely on the honest indicator and one-Esc clear.** Trade-off:
   simpler and avoids surprise clears, but loses the safety net the user explicitly asked
   about for forgotten filters.

## Assumptions

- "Reset the filter after 1 minute of inactivity" means clearing the applied filter (and
  exiting filter mode) after ~60s with no filter-related interaction; made configurable with
  a 60s default.
- The primary user pain is cross-pane leakage and the silent residual filter after Esc; the
  indicator and lifecycle fixes are prioritized above the timeout.
- Existing tick cadence (~250ms idle poll) is acceptable granularity for a minute-scale
  timeout, so no new timer thread is introduced.
