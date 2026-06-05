# Context Handoff Memory

Date: 2026-06-05
Project: chat-cli

## Standing Project Rules

1. Excellent UI/UX is the main project goal.
   - No Vim-style visible UX.
   - Mouse/touchpad interactions matter.
   - Use screenshots for critique and UI validation.

2. Low CPU and memory usage is important.
   - Avoid unnecessary polling/redraws.
   - Cache image/avatar decoding.
   - Keep hit-testing/state lightweight.

3. After every coding session, run:

```bash
cargo build --workspace
```

4. Avoid test loops.
   - Do not keep running tests without making relevant changes.
   - Inspect, patch, then validate.

## Current In-Progress Milestone

Finish message actions plus reaction UI polish.

Completed work:

- [x] Complete missing reply, reaction, copy, and reaction UI polish behavior.
- [x] Update or add focused tests for message selection and actions.
- [x] Run formatting, strict Clippy, workspace tests, final workspace build, and screenshot review.

## Main Files Involved

- `crates/tui/src/widgets/message_list.rs`
  - Message rendering
  - Message hit-testing
  - Media hit-testing
  - Text/media bubble rendering
  - Image/media preview handling

- `crates/tui/src/app.rs`
  - App-level interaction logic
  - Message selection state
  - Action menu state
  - Reply flow
  - Reaction picker flow
  - Copy behavior
  - Status bar hints
  - Mouse/touchpad handling

- `crates/core/src/mock.rs`
  - Mock provider send/reaction behavior
  - Mock message history updates

- `crates/storage/src/lib.rs`
  - Message persistence
  - Reaction persistence/hydration

## Specific UI Polish Still Needed

From the latest screenshot review:

1. [x] Attach reactions tightly to messages/cards.
   - Reactions should render as compact pills immediately under or attached to the bubble/card.
   - Avoid floating detached reaction lines.

2. [x] Improve reaction picker placement.
   - It should avoid covering the selected image/message when possible.
   - It should feel anchored to the selected message.
   - If not enough space, place above or below.

3. [x] Make reaction picker mouse/touchpad clickable.
   - Click emoji to react.
   - Click outside to cancel.
   - Keyboard selection should still work.

4. [x] Simplify modal status hints.
   - When reaction picker/action menu is open, status bar should show only modal-relevant hints.
   - Example: `Choose reaction · Arrow keys move · Enter selects · Esc cancels`

5. [x] Tone down intense selected-message/popup borders.
   - Make selected message treatment less debug-like.
   - Prefer subtle background/accent marker over loud full border.

## Functional Actions to Complete or Verify

### Message Selection

- [x] Click/tap message selects it.
- [x] Keyboard movement in message pane selects messages.
- [x] Selected message highlight is visible but subtle.

### Action Menu

Actions should include:

- [x] Reply
- [x] React
- [x] Copy text
- [x] Open image/save media if present
- [x] Cancel

### Reply Flow

- [x] Selecting Reply creates a compose reply preview.
- [x] Sending passes `reply_to` into provider send.
- [x] Sent reply persists and renders with quote/preview.

### Reaction Flow

- [x] Selecting/clicking an emoji calls provider reaction behavior.
- [x] Reaction persists locally or mock provider updates history.
- [x] UI updates visibly.

### Copy Flow

- [x] Copy text when clipboard is available.
- [x] Show friendly status fallback if clipboard is unavailable.

## Validation Required Before Finishing

After actual changes are complete, run:

```bash
cargo fmt --all --check        # [x] passed
cargo clippy --workspace -- -D warnings  # [x] passed
cargo test --workspace         # [x] passed
cargo build --workspace        # [x] passed
```

Then take a screenshot and review the UI if the app window is visible. [x] Screenshot captured; focused terminal was not the chat-cli app UI, so no app-specific visual critique was possible.

## Continuation Instruction

After context cleanup, read this file first and continue the in-progress message-actions/reaction-polish milestone from here.
