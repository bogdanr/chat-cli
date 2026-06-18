# Compose / Emoji Picker / Window Title UI-UX Improvements

## Objective

Bring three TUI surfaces closer to a polished, WhatsApp-like experience:

1. **Compose window** — long messages must wrap onto multiple visible rows automatically as you type, instead of staying on one horizontally-scrolling line until you press Alt/Shift+Enter. The box should grow with the wrapped content.
2. **Emoji suggestion picker** — currently capped at 8 suggestions (`COMPOSE_EMOTICON_MAX_SUGGESTIONS = 8`). Allow more matches and let the user scroll past the first 8 with the arrow keys inside a fixed-height, scrolling viewport.
3. **Window/pane titles** — titles currently begin flush against the top-left border corner (`╭Messages…`). Add a small left/right margin so they read like a label, matching the spaced style already used for the Thread panes (`" Thread "`) and the reply quote name (`┌─ Sorin ─┐`).

### Root causes (evidence)

- **Compose single-line**: The compose editor is a `ratatui-textarea` `TextArea` (`crates/tui/src/app.rs:51`, rendered at `crates/tui/src/app.rs:4861-4864`). `ratatui-textarea` 0.8.0 exposes **no soft-wrap API** — each logical line is rendered on exactly one display row and long lines scroll horizontally. The height formula at `crates/tui/src/app.rs:4774-4793` already *estimates* wrapped rows (using a coarse `width/2` divisor), but the widget itself never wraps, so the extra rows show empty while the text scrolls sideways on row one. A new visible line only appears when an explicit newline is inserted via Alt/Shift+Enter (`crates/tui/src/app.rs:10471-10477`).
- **Emoji picker 8-cap**: `update_compose_emoticon_completion` truncates matches with `.take(COMPOSE_EMOTICON_MAX_SUGGESTIONS)` (`crates/tui/src/app.rs:10611`, constant at `crates/tui/src/app.rs:247`). The draw loop renders every match (`crates/tui/src/app.rs:6368-6381`) and the rect height clamps to 12 (`crates/tui/src/app.rs:14205`), so at most ~8 rows are ever produced and there is no scroll offset to move beyond them.
- **Flush titles**: Blocks set titles with bare strings, e.g. `.title(title)` / `.title("Emoji")` (`crates/tui/src/app.rs:4807`, `:6389`; `crates/tui/src/widgets/message_list.rs:300`; `crates/tui/src/widgets/chat_list.rs:157`). Ratatui anchors a top-left title immediately after the corner glyph, so there is no breathing room. The Thread panes already pad with spaces (`crates/tui/src/app.rs:5662`, `:5804`), confirming the intended look.

## Implementation Plan

### Part A — Compose auto-wrap (Issues #1)

- [ ] Task A1. **Add a render-time soft-wrap helper for compose content.** Create a helper that takes the compose `TextArea`'s logical lines (`compose.lines()`) and the target inner width, and produces wrapped display rows plus a mapping from the logical cursor `(row, col)` (`compose.cursor()`) to a display `(row, col)`. Wrap on word boundaries with character-level fallback for long unbroken tokens, mirroring the wrapping already used elsewhere in `message_list.rs`. Rationale: `ratatui-textarea` cannot wrap, so wrapping must happen in our render path while the `TextArea` stays the single source of truth for editing.
- [ ] Task A2. **Render the compose editor as a wrapped view with a manual cursor.** In `draw_compose` (`crates/tui/src/app.rs:4795-4865`) and `draw_thread_compose` (`crates/tui/src/app.rs:5879+`), replace direct `frame.render_widget(&compose, editor_area)` with rendering of the wrapped display rows (as a `Paragraph`/styled lines) and place the terminal cursor via `frame.set_cursor_position` at the mapped display position when the pane is focused. Preserve the placeholder text/style behavior currently provided by `compose_textarea_for_render` (`crates/tui/src/app.rs:4867-4888`) for the empty state. Rationale: wrapping must be reflected visually and the cursor must track the wrapped position so editing stays intuitive.
- [ ] Task A3. **Make height growth use the real wrapped row count.** Update `compose_height` (`crates/tui/src/app.rs:4774-4793`) and `thread_compose_height` (`crates/tui/src/app.rs:4755-4772`) to compute extra rows from the same wrap helper at the actual inner width (replacing the `width/2` / `width-4` heuristics), keeping the existing `+ reply_extra + attachment_extra` terms and the `clamp(3, 8)` bound. Rationale: the box should grow to match what is actually rendered, and the count must agree with the draw to avoid clipping or empty rows.
- [ ] Task A4. **Keep editing/navigation semantics intact.** Verify Up/Down/Home/End and edge-focus-switching (`crates/tui/src/app.rs:10488-10542`) still feel correct against wrapped rows — at minimum, document that arrow keys continue to operate on logical lines (acceptable) or, if pursued, map vertical movement to visual rows. Enter still sends, Alt/Shift+Enter still inserts a hard newline (`crates/tui/src/app.rs:10471-10478`). Rationale: wrapping is a display concern and must not regress the established key contract or the `compose_text()` value used by send.
- [ ] Task A5. **Confirm sent payload is unchanged.** Ensure soft-wrapping never injects newline characters into `state.compose_text`/`compose_text()` — wraps are visual only; only explicit Alt/Shift+Enter produces `\n`. Cross-check `send_composed_message` (`crates/tui/src/app.rs:10767`) and `send_thread_composed_message` (`crates/tui/src/app.rs:10707`). Rationale: visual wrapping must not corrupt outgoing message content.

### Part B — Emoji picker scrolling (Issue #2)

- [ ] Task B1. **Raise/remove the suggestion cap.** Replace the hard `COMPOSE_EMOTICON_MAX_SUGGESTIONS = 8` truncation in `update_compose_emoticon_completion` (`crates/tui/src/app.rs:10599-10612`) so `matches` can hold all relevant results (optionally keep a generous safety cap, e.g. 50). Rationale: scrolling is meaningless if matches are discarded at 8.
- [ ] Task B2. **Introduce a viewport/scroll offset on the picker.** Add a `scroll_offset: usize` (or derive a visible window from `selected`) to `ComposeEmoticonPicker` (`crates/tui/src/app.rs:1025-1030`) and define a fixed visible-rows constant (e.g. 8). Rationale: a stable viewport with an offset is the standard way to scroll a longer list in a bounded modal.
- [ ] Task B3. **Keep the selection in view on arrow navigation.** In `handle_compose_emoticon_picker_key` (`crates/tui/src/app.rs:8677-8689`), after moving `selected` Up/Down, adjust `scroll_offset` so `selected` stays within `[offset, offset + visible_rows)` (scroll-on-edge). Rationale: arrow keys must reveal items beyond the first 8, which is the explicit request.
- [ ] Task B4. **Render only the visible window with scroll affordances.** Update the draw loop in `draw_compose_emoticon_picker` (`crates/tui/src/app.rs:6368-6381`) to render the `scroll_offset..offset+visible_rows` slice, mapping the selected highlight to its windowed row, and add subtle more-above/more-below indicators (e.g. `↑`/`↓` or a count) when items are hidden. Rationale: the modal must show a coherent window and hint that more exist.
- [ ] Task B5. **Fix the picker rect height to the viewport, not the match count.** Update `compose_emoticon_picker_rect` (`crates/tui/src/app.rs:14198-14212`) so height is based on `min(matches.len(), visible_rows) + chrome`, not the full match count, keeping the existing clamps and on-screen placement. Rationale: a long match list must not blow up the modal; the box stays fixed and scrolls internally.
- [ ] Task B6. **Reset scroll on query change.** Ensure `scroll_offset` (and clamped `selected`) reset sensibly when the query narrows/changes in `update_compose_emoticon_completion` (`crates/tui/src/app.rs:10619-10630`). Rationale: a stale offset after filtering would hide the top matches.

### Part C — Title margins (Issue #3)

- [ ] Task C1. **Add a single title-padding helper and apply it consistently.** Introduce one small helper (e.g. `padded_title(s) -> " {s} "`) and use it for every pane/overlay `Block` title so the start/end gain one space. Apply to the main panes — message list (`crates/tui/src/widgets/message_list.rs:300`) and chat list (`crates/tui/src/widgets/chat_list.rs:157`, optionally padding inside `title()` at `chat_list.rs:1620`) — and the app overlays/panes at `crates/tui/src/app.rs:729, 4807, 4924, 4965, 5604, 5885, 6199, 6281, 6346, 6389, 6453, 6490, 6761, 6865, 6897, 7031, 7059, 7195, 7264, 7365`. Rationale: a single helper guarantees a uniform margin and avoids drift.
- [ ] Task C2. **Leave already-padded titles consistent.** The Thread panes (`crates/tui/src/app.rs:5662`, `:5804`) already use `" Thread "`; align them to the same helper so spacing is identical everywhere (avoid double spaces). Rationale: consistency and no regressions on the panes that already look right.
- [ ] Task C3. **Verify no title is truncated by the new padding.** Confirm padded titles still fit narrow panes and that truncation logic (e.g. `truncate_chars` callers) accounts for the 2 extra cells where titles are length-sensitive. Rationale: padding must not push long titles past the border or clip badges/counters.

### Part D — Tests & validation

- [ ] Task D1. **Compose wrap tests.** Add widget/unit tests asserting: (a) a long single-line message renders across multiple display rows after a draw, (b) `compose_height` grows to match the wrapped row count (bounded by 8), and (c) `compose_text()` contains no inserted `\n` for soft-wrapped content while Alt/Shift+Enter still yields explicit newlines (extend the pattern in `app_compose_supports_multiline_textarea_input` at `crates/tui/src/app.rs:23308`). Rationale: lock in the visual-only nature of wrapping and prevent payload regressions.
- [ ] Task D2. **Emoji scroll tests.** Add tests asserting: matches can exceed 8, Down past the 8th visible item advances `scroll_offset` and reveals later matches, Up scrolls back, and the rendered modal height stays fixed. Rationale: guard the core requested behavior.
- [ ] Task D3. **Title margin test.** Add a render test asserting a representative pane title is surrounded by spaces (no glyph flush against the corner). Rationale: prevent future regressions to flush titles.
- [ ] Task D4. **Run CI-mirrored validation locally** per `AGENTS.md`: `cargo check`, `cargo test`, `cargo clippy -- -D warnings`, and the release build, mirroring `.github/workflows/ci.yml`. Rationale: required by repo rules before commit.

## Verification Criteria

- Typing a message longer than the compose width wraps onto additional visible rows automatically (no horizontal scroll, no need for Alt/Shift+Enter), and the compose box grows up to its max height then scrolls internally.
- The cursor renders at the correct wrapped position while editing, and the sent message text contains no newlines unless the user explicitly inserted them.
- The emoji suggestion picker can present more than 8 matches; pressing Down past the 8th reveals further matches within a fixed-height modal, and Up scrolls back; selected item is always visible.
- Every pane and overlay title has a one-cell margin on both sides, visually consistent with the Thread panes and the reply quote name.
- `cargo check`, `cargo test`, `cargo clippy -- -D warnings`, and the release build all pass.

## Potential Risks and Mitigations

1. **Cursor mapping drift in the wrapped compose view.**
   Mitigation: centralize the logical→display cursor mapping in the same helper that produces wrapped rows (Task A1) and cover it with tests (Task D1); fall back to end-of-text positioning if mapping is out of range.
2. **Height count vs. draw mismatch (clipping / empty rows / scroll jumps).**
   Mitigation: compute `compose_height` from the exact same wrap helper used for drawing (Task A3), and assert parity in tests.
3. **Performance of wrapping on the draw path.**
   Mitigation: compose text is small and bounded; wrapping is O(n) over a short buffer. Per `AGENTS.md`, keep it bounded and avoid any per-keystroke heavy work; cache by `(width, content)` if profiling shows cost.
4. **Loss of TextArea built-in features (selection highlight, placeholder).**
   Mitigation: preserve placeholder rendering explicitly (Task A2); if selection highlighting is in use, replicate it in the wrapped render or scope this plan to non-selection editing and note the limitation.
5. **Emoji scroll offset desync after filtering.**
   Mitigation: reset/clamp `scroll_offset` and `selected` on every query update (Task B6) and test the narrow-then-scroll path.
6. **Title padding causing truncation or double spaces.**
   Mitigation: single shared helper (Task C1), reconcile already-padded titles (Task C2), and verify narrow-pane fit (Task C3).

## Alternative Approaches

1. **Compose wrapping — auto-insert hard newlines at width.** Insert real `\n` into the buffer as the user types past the edge. *Rejected*: corrupts the outgoing message and breaks editing/undo.
2. **Compose wrapping — fork/patch `ratatui-textarea` to add soft-wrap.** Vendor the crate and add wrapping at its render layer. *Heavier*: more faithful cursor/selection handling but adds a maintained fork; reconsider if render-time wrapping proves too limited for selection.
3. **Emoji scrolling — paginate instead of continuous scroll.** Show pages of 8 with PageUp/PageDown. *Trade-off*: simpler offset math but less fluid than the requested arrow-key scroll; continuous scroll is preferred.
4. **Titles — bordered title block / centered titles.** Use ratatui title alignment or a decorative `─ X ─` style like the reply box. *Trade-off*: more decorative but higher risk of inconsistency; a simple symmetric space margin is the minimal, uniform fix the user asked for.

## Handoff Note

This is a strategic plan only; no source files were modified. Implementation (editing `crates/tui/src/app.rs`, `crates/tui/src/widgets/message_list.rs`, `crates/tui/src/widgets/chat_list.rs`, and tests) requires an implementation agent (e.g. Forge). The highest-risk items to watch during implementation are the compose cursor mapping and the `compose_height`-vs-draw parity (Tasks A1–A3, D1).
