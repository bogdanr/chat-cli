# Composer: select all, undo paste, and easy text formatting

## Objective

Give the message composer three things it lacks today:

1. **Select all and clear:** select the whole draft, then delete or replace it.
2. **Undo:** undo and redo, where a whole paste is a single undo step, so one keypress removes a long paste.
3. **Easy formatting:** bold, italic, strikethrough, inline code and code blocks, using keyboard shortcuts and a clickable Format menu. The text must arrive correctly formatted on WhatsApp, Slack and ClickUp.

## Current state (evidence)

- The composer ignores every Ctrl/Alt character key except Ctrl+U (attach) (`crates/tui/src/app.rs:12860-12863`, `crates/tui/src/app.rs:12964-12974`). Edits go through `input_without_shortcuts` (`crates/tui/src/app.rs:13087-13091`), so the text area's built-in undo and selection are never reached. Today the only way to remove a paste is to hold Backspace.
- A paste is inserted one character at a time, and the compose cache is re-synced after each character (`crates/tui/src/app.rs:12451-12460`). This is slow for large pastes (up to 64 KiB), and each character becomes its own undo entry.
- Global keys handled before the composer: Ctrl+Q/Ctrl+C quit, Ctrl+X toggles image mode, Ctrl+S settings, Ctrl+F filter (`crates/tui/src/app.rs:10639-10732`). Ctrl+A goes to the account switcher only outside the composer (`crates/tui/src/app.rs:10720`), so Ctrl+A is free inside it.
- `ratatui-textarea` 0.8 already provides `select_all`, `start_selection`/`cancel_selection`, `selection_range`, `cut`, `undo`, `redo`, `insert_str`, `set_selection_style` and `set_max_histories`.
- Received messages are rendered with a markdown dialect: `**bold**`/`__bold__`, `*italic*`/`_italic_`, `~~strike~~`, `` `code` ``, fenced code (`crates/tui/src/widgets/message_list.rs:4326-4356`).
- Slack inbound mrkdwn is already translated into that dialect (`crates/providers/slack/src/lib.rs:6817-6870`). WhatsApp inbound is not translated, so WhatsApp `*bold*` shows as italic and `~strike~` is not styled. ClickUp already sends and receives `text/md` (`crates/providers/clickup/src/api.rs:635`).
- Outbound text is sent exactly as typed to every provider. No provider converts formatting.

## Decisions and assumptions

- **Key bindings** (chosen because they work reliably in terminals):
  - Ctrl+A: select all (composer only).
  - Ctrl+Z: undo. Ctrl+Y: redo.
  - Ctrl+B: bold. Alt+I: italic (Ctrl+I is the same key as Tab in terminals). Alt+S: strikethrough. Alt+C: inline code. Alt+Shift+C: code block (optional).
  - Ctrl+X stays the image toggle and Ctrl+C stays quit. No cut or copy shortcuts in this plan.
- **Selection behaviour:** Backspace/Delete removes the selection. Typing or pasting replaces it. Arrow keys cancel it. The selection is highlighted with a theme style.
- **One formatting syntax in the composer:** the renderer's markdown (`**bold**`, `_italic_`, `~~strike~~`, `` `code` ``, fenced blocks). The text is stored and shown locally in that syntax, and each provider converts it only on the wire:
  - Slack and WhatsApp: `**x**` → `*x*`, `~~x~~` → `~x~`, `_x_` unchanged. Nothing is converted inside code spans or blocks.
  - ClickUp: sent unchanged (markdown).
- **Format actions:**
  - With a selection, the markers wrap the selection.
  - Without a selection, a marker pair is inserted with the cursor between.
  - If the selection is already wrapped in the same markers, they are removed.
- Esc keeps its current meaning. It does not clear the draft.

## Implementation Plan

### Phase 1: Select all, undo, paste as one step
- [ ] 1. **Route composer editing keys.** In `handle_compose_key`, before the catch-all `KeyCode::Char` arm, handle Ctrl+A (select all), Ctrl+Z (undo) and Ctrl+Y (redo). Each re-syncs the compose cache, refreshes mention/emoji completion and sets a short status such as "all text selected — Backspace deletes, Ctrl+Z undoes". Rationale: the text area already supports these; they are just never reached.
- [ ] 2. **Selection-aware editing.** When a selection is active: Backspace/Delete removes it (`cut`, after which the yank buffer is restored so no hidden clipboard state leaks); a typed character or Enter-with-modifier replaces it; arrow/Home/End cancel it before moving. Rationale: select all is only useful if the next key acts on the selection.
- [ ] 3. **Paste as one undoable insert.** Replace the per-character paste loop in `handle_paste` with a single `insert_str` (after normalising `\r\n`/`\r`, and deleting any active selection first). Then run one cache sync and one completion update. Add an opt-in perf label `compose_paste` with the character count. Rationale: one Ctrl+Z removes the whole paste, and large pastes no longer cause a cache sync per character.
- [ ] 4. **Undo history bounds.** Set a deliberate `set_max_histories` in `new_compose_textarea`. Make sure the textarea replacement paths (send, start/cancel edit, attach-from-path, reply) reset history on purpose, so Ctrl+Z never brings back a message that was already sent.
- [ ] 5. **Selection highlight.** Apply a theme-derived selection style to the compose textarea. Keep the draw path cache-only.

### Phase 2: Formatting in the composer
- [ ] 6. **Pure formatting helper.** Add a small TUI-side function that, given the text, cursor and optional selection range plus a format kind (Bold, Italic, Strike, Code, CodeBlock), returns the new text and the new cursor/selection. It handles wrap, insert-pair and toggle-off. Keep it free of App state so it can be unit tested. The textarea is updated in one operation, so the formatting is a single undo step.
- [ ] 7. **Shortcuts.** Bind Ctrl+B, Alt+I, Alt+S, Alt+C (and optionally Alt+Shift+C) in `handle_compose_key` to the helper. Set a status line naming the format that was applied.
- [ ] 8. **Format menu.** Add a clickable "Aa Format" control next to "+ Attach" that opens a small popup listing the five formats with their shortcuts. Follow the existing `compose_attach_menu` pattern for state, key handling, mouse hit-testing, and closing on outside click. Add it to the overlay checks in `handle_paste` and the `handle_key` dispatch order. Rationale: shortcuts alone are hard to discover, and the user asked for an easy way.
- [ ] 9. **Help and hints.** Add the new composer keys and the Format menu to the help overlay's Compose section (`crates/tui/src/app.rs:8419-8431`) and to any composer footer hints.

### Phase 3: Correct formatting on each provider
- [ ] 10. **Shared converter in core.** Add a `markup` module to `chat_core` with:
  - (a) `markdown_to_chat_markup` for Slack/WhatsApp: `**`→`*`, `~~`→`~`, single-`*` italic→`_`. It uses word-boundary delimiter rules like Slack's existing converter and never converts inside code spans, fenced blocks, URLs or `<...>` tokens.
  - (b) `whatsapp_markup_to_markdown` for inbound WhatsApp text: `*`→`**`, `~`→`~~`.
  Move or reuse the Slack code-span-aware helpers instead of duplicating them.
- [ ] 11. **Slack outbound.** Apply the converter to the wire text in both `send` and `edit_message`, before mention encoding. The stored local message keeps the composer syntax.
- [ ] 12. **WhatsApp outbound and inbound.** Apply the converter on send and edit. Apply `whatsapp_markup_to_markdown` to incoming text, both live and history, including incoming edits, so bold and strikethrough display correctly and `editable_text` returns composer syntax when an edit starts.
- [ ] 13. **ClickUp.** No conversion. Add a test that proves the composer syntax is sent unchanged as `text/md`.
- [ ] 14. **Edit round trip.** Starting an edit fills the composer with composer-syntax text for all three providers, and saving it converts again, without doubled or lost markers.

### Phase 4: Tests and validation
- [ ] 15. **TUI tests:**
  - Ctrl+A then Backspace empties the composer.
  - A typed character replaces the selection.
  - A multi-line paste followed by one Ctrl+Z restores the previous draft exactly; Ctrl+Y restores the paste.
  - A paste over a selection replaces it.
  - Ctrl+Z after send does not bring back the sent text.
  - Ctrl+B wraps, inserts a pair, and toggles off.
  - The Format menu opens, applies a format, and closes.
  - Ctrl+A outside the composer still opens the account switcher.
- [ ] 16. **Converter unit tests:** nested and adjacent markers, code spans and fences left untouched, URLs with underscores/asterisks, `2*3*4`, mentions, multi-line text, and round-trip stability (`to` then `from` is identity for composer syntax).
- [ ] 17. **Provider tests:**
  - Slack fake client and WhatsApp FFI send/edit tests check the converted wire text.
  - WhatsApp inbound (live and history) checks the translated stored text.
  - ClickUp checks that text is sent unchanged.
- [ ] 18. **CI-equivalent validation:** inspect `.github/workflows/ci.yml`, then run `cargo check --workspace --all-targets`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --release -p chat-cli`, plus `go vet`/`go test` for the bridge if it changes.

## Verification Criteria

- Pasting 10,000 characters and pressing Ctrl+Z once restores the previous draft. The paste causes one cache sync, and the perf label shows one entry.
- Ctrl+A followed by Backspace leaves an empty composer, and the status bar explains both keys.
- Selecting "hello" and pressing Ctrl+B gives `**hello**` in the composer. It renders bold locally and arrives as `*hello*` (bold) on Slack and WhatsApp and `**hello**` on ClickUp.
- A WhatsApp message received as `*hi* ~old~` renders as bold "hi" and struck "old".
- Code spans containing `**` are sent unchanged on every provider.
- Every existing test and CI step passes.

## Potential Risks and Mitigations

1. **Terminal key conflicts:** Ctrl+I is Tab, some terminals swallow Alt, and macOS needs Option-as-Meta.
   Mitigation: Ctrl+B for the most common format, Alt for the rest, and the clickable Format menu as a fallback that always works. The help text documents this.
2. **Wrong conversion of ordinary text** (such as `a*b*c` or URLs).
   Mitigation: delimiter rules that need word boundaries, skipping code spans, URLs and `<...>` tokens, and a broad set of unit test cases.
3. **Users pressing Ctrl+C to copy quit the app while composing.**
   Mitigation: out of scope here, and this plan doesn't change it. It is listed as a follow-up (for example, Ctrl+C copies when there is a selection).
4. **Undo bringing back sent or replaced drafts.**
   Mitigation: history is reset on purpose wherever the textarea is replaced, and a test covers it.
5. **Double conversion on echoes** (a sent message coming back through realtime or history).
   Mitigation: inbound translation always outputs composer syntax, and a round-trip test checks that it is stable.

## Alternative Approaches

1. **Send provider-native markers directly** (the composer writes `*bold*` for Slack and WhatsApp): no converter is needed, but local rendering shows bold as italic, and the syntax changes per chat.
2. **Live-styled composer** (render bold/italic inside the composer as you type): more polished, but it needs custom textarea rendering and draw/measure parity work. It is a good follow-up once the markers work.
3. **"Clear draft" key instead of select all** (such as Ctrl+L): simpler, but less flexible than select plus undo, and it doesn't help with removing just the paste.
