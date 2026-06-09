# AGENTS.md

## Performance and Responsiveness Rules

- Keep the TUI event loop responsive. Do not perform database reads, network calls, image decoding, markdown/layout measurement for large histories, or provider history sync directly inside input handling or draw paths.
- Draw code must be cache/placeholder only. If data or decoded media is missing, queue background work and render a lightweight placeholder until the result is ready.
- Chat selection must update UI state immediately and load messages asynchronously. Apply async results only when they still match the currently selected chat/account and the latest generation/request token.
- Provider bursts must be drained in bounded batches. Avoid unbounded loops over provider events, completion channels, or history pages in a single UI tick.
- Preserve selection and scroll state when background work completes unless the user explicitly navigated elsewhere.
- Add or update opt-in performance instrumentation for any new async pipeline or suspected slow path. Prefer labels that identify the phase and include counts, account/chat identifiers, and stale/error totals.
- Treat performance logs as evidence. Do not optimize blindly; inspect the latest log and target the largest blocking label first.
- Never read the entire `tmp/debug.log` or any large debug log into context. Check its size first, then inspect only targeted slices using bounded commands or focused searches.
- Keep expensive image/media work on blocking worker threads, never on the async runtime hot path or terminal draw path.
- Cache layout and preview work by stable keys that include dimensions and presentation mode. Invalidate caches deliberately when inputs change.
- Tests for asynchronous UI work should assert both phases: immediate placeholder/loading state after input, then final state after draining background completions.

## Activity and Ordering Rules

- `Chat.last_message_at` and `Chat.last_message_preview` must represent actual message activity, not provider conversation metadata.
- Provider chat snapshots must not downgrade newer stored/sidebar activity. Merge incoming chat metadata with existing state/storage before updating visible sidebar state.
- Historical replay must be idempotent for sidebar ordering. It should never make visible chat order regress while catch-up is running.

## Implementation Safety Rules

- Prefer small, targeted changes with validation after each performance fix.
- Do not remove failing tests to make a change pass. Update tests only when behavior intentionally changes.
- Avoid adding synchronous work to helpers called from `handle_event`, `draw`, or completion drains unless the work is demonstrably bounded and instrumented.
- When adding background tasks, handle stale completions, errors, and cancellation-by-navigation explicitly.

## Fono Voice Summary Rules

<!-- fono-voice-preset -->
This preset is tuned for coding agents working in this repository.

The user may be listening and may also have the chat window visible on screen. Treat the
spoken and written channels differently.

Two channels:
- **Spoken channel (`fono.speak`)**: short, conversational, and only used for the final
  task summary. One to three sentences. No lists read aloud, no paths, no command names
  spelled out, no code blocks, no tables, and no long identifiers. If details are
  technical, say that the details are on screen.
- **Written channel (the chat reply)**: the place for full detail, including file paths,
  validation summaries, and next-step notes.

For project work, do **not** call `fono.speak` for every acknowledgement, intermediate
status update, or internal task transition. Call it only when the requested work is done,
blocked, or handed back to the user with a final summary for the turn.

Turn-ending modes:
- **R. Read (default — no answer needed).** Use this ending whenever the turn is reporting
  completion, status, findings, or other information and no answer is required to make
  progress. Call `fono.speak` with the concise final summary, then stop. Do not capture
  audio.
- **L. Listen.** Only use when the turn ends with a real question the user must answer for
  you to make progress. Call `fono.listen` with a `context` argument describing the answer
  expected so background speech can be ignored.
- **C. Confirm.** Only use when the answer is naturally one of a small fixed set of labels
  and the user should not have to think about phrasing. Call `fono.confirm` with those
  labels.

Hard rules:
1. Every final `fono.speak` summary starts with a brief refocus preamble such as
   "Right —", "Back to you —", or "Quick summary —" before the substance.
2. No bare spoken questions. If a spoken turn ends in a question, the same turn must use
   `fono.listen` or `fono.confirm`; otherwise, narrate the status and stop.
3. Do not use voice capture to authorise destructive or irreversible actions such as
   delete, force-push, deploy, drop, overwrite, reset, or equivalents. Explain the action
   on screen and wait for explicit written/user-triggered approval.
4. Match the user's spoken language in `fono.speak` when that language is clear. Keep all
   written project content in English unless the user explicitly dictates text that must
   be preserved verbatim.

Brevity > caveats. The spoken summary should only tell the user what happened and whether
anything needs their attention.
<!-- /fono-voice-preset -->
