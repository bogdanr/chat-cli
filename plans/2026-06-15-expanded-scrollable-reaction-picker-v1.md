# Expanded, Scrollable Reaction Picker (with Kaomoji Awareness)

## Objective

Replace the current six-emoji single-row reaction picker with a richer,
scrollable, searchable reaction chooser that delivers excellent keyboard and
mouse UX, while correctly respecting each provider's reaction limitations
(including whether kaomoji / arbitrary text reactions are acceptable).

## Current State Assessment

### Reaction picker (the thing to improve)
- The picker offers only six fixed emoji defined in
  `crates/tui/src/app.rs:176` (`REACTION_OPTIONS`).
- It renders as a single horizontal row, navigated with Left/Right/Up/Down,
  with no scrolling and no search (`crates/tui/src/app.rs:6354-6416` draw,
  `crates/tui/src/app.rs:8727-8755` keys).
- Modal sizing is one fixed-height row: width = options x 6 + 4, height 5
  (`crates/tui/src/app.rs:14276-14282`).
- Mouse hit-testing assumes a single row of equal-width cells
  (`crates/tui/src/app.rs:10018-10028`, `crates/tui/src/app.rs:14496-14512`).
- Selected index is seeded from `local_reaction_option`
  (`crates/tui/src/app.rs:18342-18346`), which only searches the six options.

### A proven pattern already exists in the codebase
- The compose emoticon picker is already scrollable, searchable, and includes
  kaomoji. It is the model to follow:
  - Data set ~60 entries incl. kaomoji: `crates/tui/src/app.rs:177-246`
    (`COMPOSE_EMOTICON_OPTIONS`).
  - State with `scroll_offset`, `query`, `matches`, `keep_selected_visible`:
    `crates/tui/src/app.rs:1025-1040`.
  - Scroll-window drawing with "N more" affordances and footer hints:
    `crates/tui/src/app.rs:6418-6486`.
  - Incremental fuzzy/prefix filtering: `crates/tui/src/app.rs:10684-10730`.
  - Visible-row constants: `COMPOSE_EMOTICON_VISIBLE_ROWS` /
    `COMPOSE_EMOTICON_MAX_MATCHES` at `crates/tui/src/app.rs:247-248`.

### Provider limitations (must be respected)
- WhatsApp (`crates/providers/whatsapp/src/lib.rs:619-686`): sends the literal
  emoji string to the bridge. WhatsApp allows only ONE reaction per user per
  message (a new emoji replaces the old one). The current toggle logic only
  compares the same emoji, so switching emoji locally can create duplicate
  "me" reactions that diverge from server state.
- Slack (`crates/providers/slack/src/lib.rs:4156-4215`): reactions are gated by
  `capabilities().can_react`, and the emoji is normalized to a Slack shortcode
  via `normalize_slack_reaction_name` (`crates/providers/slack/src/lib.rs:6769`).
  Slack's `reactions.add` API only accepts valid emoji shortcodes / workspace
  custom emoji names. Arbitrary text or kaomoji will be rejected by the Slack
  API.
- The reaction action is itself gated by `can_react` in the action menu
  (`crates/tui/src/app.rs:1208-1265`).

### Kaomoji answer (assessment)
- Display: kaomoji render fine; `reaction_display_emoji`
  (`crates/tui/src/widgets/message_list.rs:3873-3897`) passes unknown tokens
  through unchanged.
- WhatsApp: the reaction field is a free string, so kaomoji can technically be
  transmitted, but it is non-standard and may not render for recipients.
- Slack: kaomoji will fail server-side normalization/validation.
- Conclusion: kaomoji must be treated as an opt-in / provider-aware option, not
  a universal reaction. The picker should surface only what the active provider
  can accept (or clearly flag unsupported entries).

## Implementation Plan

- [ ] Task 1. Introduce a richer reaction catalog. Define a curated, ordered
  reaction list (with searchable alias labels) instead of the six-item
  `REACTION_OPTIONS`. Rationale: "more reactions" is the core request; an
  alias-tagged list enables search and keeps the data declarative. Reuse the
  shape of `COMPOSE_EMOTICON_OPTIONS` so the existing filter logic transfers.

- [ ] Task 2. Tag each catalog entry with provider compatibility. Add metadata
  marking whether an entry is a standard Unicode emoji (universally safe) vs. a
  kaomoji / non-standard token (WhatsApp-only / display-only). Rationale: the
  picker must respect Slack's shortcode requirement and avoid offering
  reactions that will be rejected.

- [ ] Task 3. Upgrade `ReactionPicker` state to support scrolling and search.
  Extend `crates/tui/src/app.rs:1020-1023` with `scroll_offset`, `query`,
  `matches`, and a `keep_selected_visible`-style helper mirroring
  `ComposeEmoticonPicker` (`crates/tui/src/app.rs:1025-1075`). Rationale: a long
  list needs a bounded visible window and incremental narrowing for good UX.

- [ ] Task 4. Redesign the picker rendering as a vertical scrollable list.
  Replace the single-row draw (`crates/tui/src/app.rs:6354-6416`) with a
  windowed list: header showing current query, "up N more"/"down N more"
  affordances, per-row "reacted-by-me" highlight, and a footer hint
  ("Enter toggles - Esc cancels - up/down scrolls - type to filter").
  Rationale: consistency with the compose picker and clear discoverability.

- [ ] Task 5. Recompute modal geometry for the new layout. Update
  `reaction_picker_rect` (`crates/tui/src/app.rs:14276-14282`) to size by a
  fixed visible-row count plus chrome, clamped to the available area via
  `anchored_message_popup_rect`. Rationale: the old width/height math assumed a
  single horizontal row.

- [ ] Task 6. Implement keyboard navigation and incremental search. Extend
  `handle_reaction_picker_key` (`crates/tui/src/app.rs:8727-8755`): Up/Down move
  selection with scroll-on-edge, PageUp/PageDown jump a window, typed
  characters append to the query and re-filter, Backspace narrows the query,
  Enter toggles, Esc cancels. Rationale: this is the heart of "scroll through
  them with excellent UX" and matches the compose picker's interaction model.

- [ ] Task 7. Rework mouse interaction for a vertical list. Update
  `handle_reaction_picker_click` (`crates/tui/src/app.rs:10018-10028`) and the
  hit-test helper (`crates/tui/src/app.rs:14496-14512`) to map a clicked row to
  a match index, and wire scroll-wheel events to move the scroll window.
  Rationale: mouse users must scroll and click rows, not horizontal cells.

- [ ] Task 8. Seed initial selection from any of the user's existing reactions.
  Generalize `local_reaction_option` (`crates/tui/src/app.rs:18342-18346`) to
  search the full catalog so reopening the picker lands on an already-applied
  reaction. Rationale: preserves the existing "toggle off what I picked"
  behavior across the larger set.

- [ ] Task 9. Enforce provider-aware availability at selection time. Before
  calling `apply_reaction` (`crates/tui/src/app.rs:12920-12958`), confirm the
  chosen entry is acceptable for the active provider; for unsupported entries
  (e.g., kaomoji on Slack) either hide them from that provider's catalog or
  surface a clear status message and skip the network call. Rationale: prevents
  silent API failures and confusing divergence between local and server state.

- [ ] Task 10. Fix single-reaction-per-user semantics for WhatsApp. When the
  user picks a new emoji while a different "me" reaction already exists, mirror
  WhatsApp's replace behavior in local state so the optimistic UI matches the
  server (remove the prior "me" reaction before adding the new one). Reference
  the toggle in `apply_reaction` (`crates/tui/src/app.rs:12944-12948`) and the
  provider replace semantics (`crates/providers/whatsapp/src/lib.rs:639-684`).
  Rationale: avoids duplicate local reactions that never reconcile with the
  bridge.

- [ ] Task 11. Update overlay/keymap bookkeeping. Ensure the new typing-driven
  picker is registered in active-overlay tracking
  (`crates/tui/src/app.rs:16269-16270`), input-capture guards
  (`crates/tui/src/app.rs:10107-10109`), and the help text describing reaction
  controls (`crates/tui/src/app.rs:7073`, `crates/tui/src/app.rs:7087`).
  Rationale: the picker now consumes character keys and scroll, so global
  shortcuts and help must reflect that.

- [ ] Task 12. Update and extend tests. Adapt the existing reaction-picker test
  (`crates/tui/src/app.rs:21970-22030`) and add coverage for: scrolling beyond
  the visible window, search narrowing then toggling, reopening onto an existing
  reaction, WhatsApp emoji replacement, and Slack rejection/hiding of kaomoji.
  Follow the two-phase async assertion guidance in `AGENTS.md`. Rationale:
  behavior changes must be locked in and the action menu test must keep passing.

## Verification Criteria

- The reaction picker presents substantially more than six reactions and never
  draws outside its modal regardless of catalog size.
- Up/Down (and PageUp/PageDown) scroll through all entries with a stable
  visible window and accurate "N more" indicators.
- Typing filters the list incrementally; Backspace restores broader matches;
  Enter toggles the highlighted reaction; Esc cancels.
- Mouse wheel scrolls the list and clicking a row toggles that reaction.
- Reopening the picker on a message the user already reacted to highlights the
  existing reaction.
- On WhatsApp, switching from one reaction to another results in exactly one
  local "me" reaction matching server replace semantics.
- On Slack, only API-acceptable reactions are offered (or unsupported entries
  are clearly flagged and never sent), and no reaction triggers a server-side
  validation error.
- `cargo check`, `cargo test`, and `cargo clippy` pass (mirroring
  `.github/workflows/ci.yml` per `AGENTS.md`).

## Potential Risks and Mitigations

1. **Slack API rejects kaomoji / non-shortcode reactions.**
   Mitigation: gate the catalog by provider; only expose standard Unicode emoji
   to Slack and validate against the shortcode normalizer before sending.
2. **WhatsApp duplicate "me" reactions from missing replace semantics.**
   Mitigation: implement Task 10 to clear the prior "me" reaction before adding
   a new emoji optimistically.
3. **Performance: filtering/measuring a large list inside the draw or input
   path.** Mitigation: keep filtering bounded (cap matches like
   `COMPOSE_EMOTICON_MAX_MATCHES`) and recompute only on query change, never in
   the draw path, per the `AGENTS.md` responsiveness rules.
4. **Wide emoji / kaomoji cause column misalignment in the list.**
   Mitigation: render one entry per row (vertical list) so variable display
   width does not break a horizontal grid.
5. **Modal overflow on small terminals.** Mitigation: clamp height/width via
   `anchored_message_popup_rect` and reduce the visible-row count when space is
   limited.
6. **Regressions in existing reaction tests / overlay handling.**
   Mitigation: update overlay bookkeeping (Task 11) and the existing test
   (Task 12) deliberately; do not delete failing tests.

## Alternative Approaches

1. Reuse the compose emoticon picker components directly: extract the
   scroll/search widget shared by `ComposeEmoticonPicker` and the new reaction
   picker into one reusable list component. Trade-off: cleaner long-term, but a
   larger refactor touching compose code paths.
2. Categorized/tabbed reaction grid (Smileys / Gestures / Objects / Kaomoji):
   richer browsing, but more UI surface and navigation complexity than a single
   searchable scroll list.
3. Minimal expansion only: grow `REACTION_OPTIONS` to two or three static rows
   with arrow navigation and no search. Trade-off: least effort, but scales
   poorly and does not deliver the "excellent UX" or search the request implies.
