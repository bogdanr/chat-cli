# WhatsApp-Style Reply Quote Block in the Message Timeline

## Objective

Make in-conversation replies render like WhatsApp: a nested quote block drawn
*inside* the reply's own bubble, directly above the reply text. The quote is a
smaller "bubble-in-bubble" box whose top border carries the quoted sender's
name as a title (`┌─ Sorin ──────────┐`), with the quoted snippet on the line(s)
below. This replaces the current single muted caption line (`  ↪ Sender: text`)
that makes replies look like loose new messages rather than quoted replies.

**Chosen visual (agreed with the user): nested titled box, no vertical bar.**

```
  Bogdan Munteanu                                    22:33
  ╭──────────────────────────╮
  │ ┌─ Sorin ──────────────┐ │   <- quoted sender name in inner top border
  │ │ Facem 1-2 drumuri si │ │   <- quoted snippet (wrapped, bounded)
  │ │ aia e                │ │
  │ └──────────────────────┘ │
  │ Loool :)                 │   <- reply text
  ╰──────────────────────────╯
```

Common single-line (truncated) case:

```
  ╭──────────────────────────╮
  │ ┌─ Sorin ──────────────┐ │
  │ │ Facem 1-2 drumuri si…│ │
  │ └──────────────────────┘ │
  │ Loool :)                 │
  ╰──────────────────────────╯
```

The reference is `/root/Downloads/Screenshot 2026-06-15 at 06-45-31 WhatsApp Business.png`:
- WhatsApp uses a colored left rule + sender name + snippet. We translate that
  to the app's box-drawing aesthetic as an inner titled box (the inner border,
  colored per sender, plays the role of WhatsApp's colored rule).
- The quoted sender's display name is the inner box's title, bold and colored.
- The quoted message text (truncated/wrapped to 1–2 lines) sits inside the inner
  box, in a muted tone.
- The whole inner box is contained by the reply bubble; the reply text sits
  below it.

## Current Behaviour (verified)

- Reply rendering (Bubbles): `crates/tui/src/widgets/message_list.rs:1346-1356`
  pushes one muted `Line` `  ↪ {preview}` *before* the content bubble, so it is
  outside the bubble entirely.
- Reply rendering (Flat/Slack): `crates/tui/src/widgets/message_list.rs:1458-1468`
  pushes a single indented muted `↪ {preview}` line.
- Preview text is a flat string `"{sender}: {snippet}"` from
  `compact_message_preview` (`crates/tui/src/widgets/message_list.rs:4188-4196`),
  stored in `reply_previews: HashMap<Arc<str>, String>`
  (`crates/tui/src/widgets/message_list.rs:197`, populated at `429-432` and
  `549-552`). There is no structured sender/snippet separation and no styling
  per participant.
- Line-count accounting assumes the reply occupies exactly one line:
  `usize::from(message.reply_to.is_some())` in `message_lines_len`
  (`crates/tui/src/widgets/message_list.rs:4261`) and
  `slack_message_lines_len` (`crates/tui/src/widgets/message_list.rs:4294`).
  The Slack body-start offset uses the same single-line assumption
  (`crates/tui/src/widgets/message_list.rs:1438`).
- The layout cache hashes `message.reply_to`
  (`crates/tui/src/widgets/message_list.rs:720`) but the cached
  `reply_previews` map only stores the flat string
  (`crates/tui/src/widgets/message_list.rs:197, 549-552`).
- Theme provides `accent`, `incoming`, `outgoing`, `muted`, `foreground`
  (`crates/tui/src/theme.rs:4-16`); there is no per-participant color palette
  today.

## Key Design Decisions / Assumptions

- **Scope:** Pure rendering/layout change in the TUI message-list widget plus
  the structured preview it consumes. No provider, storage, or `chat_core`
  schema changes — `reply_to` and `compact_message_preview` source data already
  exist.
- **Quote block shape:** A nested titled box (no vertical bar). The sender name
  is embedded in the inner top border line (`┌─ Sorin ──┐`); the snippet wraps
  inside the inner box; a bottom border (`└──┘`) closes it.
- **Quote block height:** Fixed and bounded to keep the line-count pass exact
  and cheap (per the performance rules: draw and count paths must agree without
  measuring large histories). Inner top border (1) + up to two wrapped snippet
  lines + inner bottom border (1) = a maximum of four quote lines. The snippet
  is truncated/wrapped to that bound.
- **Containment & width math:** In Bubbles mode the inner box renders inside the
  bubble border, between the top `╭─╮` and the body text, emitted through the
  existing `bubble_text_line` (`crates/tui/src/widgets/message_list.rs:3439-3449`)
  so the outer bubble border/alignment stays correct. The inner box width is
  `outer_inner_width − 2` (the two leading content cells of `bubble_text_line`
  are already consumed; the inner box draws its own `┌ … ┐`/`│ … │`/`└ … ┘`),
  and the snippet wraps to `inner_box_width − 4` (two inner border cells + two
  inner pad cells). Both the draw pass and the count pass must use the same
  width helper.
- **Name-in-border truncation:** The title segment `┌─ {name} ─┐` is truncated
  so the inner top border never exceeds the inner box width; long names become
  `┌─ Sorin… ─┐`. Fill dashes pad the remainder of the border.
- **Per-participant color (assumption):** WhatsApp colors each quoted sender
  distinctly. Initial implementation will color the inner box border + title
  with a stable hash-derived color chosen from a small fixed palette
  (deterministic per `sender.platform_id`), falling back to `theme.accent`. The
  whole inner box is tinted to the quoted speaker, replacing WhatsApp's colored
  left rule. This is additive and isolated to a helper so it can be tuned later.
- **Missing quoted message:** When `reply_to` resolves to a message not in the
  loaded history (no entry in the preview map), render the inner box with a
  muted border and a `[message not loaded]`/`message {short_id}` snippet,
  matching the existing fallback intent at
  `crates/tui/src/widgets/message_list.rs:1349-1351`.
- **Both presentations:** Apply the nested titled-box treatment to Bubbles and a
  visually consistent indented titled box to Flat/Slack, so reply semantics read
  the same in both modes.
- **No behaviour change to compose/reply-send flow** — only the inbound/render
  representation of an existing reply changes.

## Implementation Plan

- [x] Task 1. Introduce a structured reply-preview type (e.g. `ReplyPreview {
  sender: Arc<str>, snippet: Arc<str>, is_from_me: bool, kind: <text/media> }`)
  to replace the flat `String` preview. Rationale: rendering a WhatsApp-style
  block requires the sender name and the snippet separated so they can be styled
  independently (bold colored name vs. muted snippet). Update the
  `reply_previews` field type on `MessageLayoutCache`
  (`crates/tui/src/widgets/message_list.rs:197`) and the `MessageRenderContext`
  reference (`crates/tui/src/widgets/message_list.rs:1291`).

- [x] Task 2. Replace `compact_message_preview`
  (`crates/tui/src/widgets/message_list.rs:4188-4196`) with a builder that
  returns the structured preview: sender display name, a snippet derived from
  `content_preview_text` (`crates/tui/src/widgets/message_list.rs:4198-4219`)
  truncated to the bounded width, and a content-kind marker so media replies can
  show a leading glyph/label (e.g. "Photo", "Voice note") like WhatsApp.
  Rationale: keeps snippet derivation centralized and reused by both the cache
  build (`crates/tui/src/widgets/message_list.rs:429-432, 549-552`) and any
  fallback path.

- [x] Task 3. Add a `quote_block_lines(...)` helper that produces the nested
  titled-box `Line`s for Bubbles mode: (a) an inner top-border line embedding the
  quoted sender name as a title (`┌─ {name} ─{dashes}─┐`, name bold + per-sender
  color, dashes padded to the inner box width); (b) up to two inner body lines
  (`│ {snippet} │`, muted snippet, wrapped to `inner_box_width − 4`); (c) an
  inner bottom-border line (`└{dashes}┘`). Compute the inner box width from
  `bubble_inner_width` (`crates/tui/src/widgets/message_list.rs:3413-3421`) and
  emit every quote line through `bubble_text_line`
  (`crates/tui/src/widgets/message_list.rs:3439-3449`) so they sit inside the
  outer bubble border. Rationale: reuse the established bubble width/border
  contract instead of inventing parallel layout that could drift from the count
  pass; centralize the name-in-border truncation here.

- [x] Task 4. Rework the Bubbles reply branch
  (`crates/tui/src/widgets/message_list.rs:1346-1356`) to: (a) render the outer
  bubble top border, (b) emit the inner titled-box quote lines from Task 3, (c)
  then the existing content/body, all within one outer bubble. This likely means
  folding the reply quote into the content-bubble construction (`text_bubble_lines`
  at `crates/tui/src/widgets/message_list.rs:1898-1916`) or wrapping content so
  the quote and body share a single bordered bubble. Rationale: the design shows
  the inner titled box and reply text inside the *same* outer bubble; today the
  quote is a separate pre-bubble line.

- [x] Task 5. Add the per-participant quote color helper (stable hash of
  `sender.platform_id` mapped to a small fixed palette, fallback
  `theme.accent`). Apply it to the inner box border characters and the title
  name. Rationale: matches WhatsApp's distinct per-sender quote colors while
  staying deterministic and theme-aware; the colored inner border replaces the
  vertical bar as the per-sender cue.

- [x] Task 6. Implement the Flat/Slack variant in `slack_message_lines`
  (`crates/tui/src/widgets/message_list.rs:1458-1468`): replace the single
  `↪ {preview}` line with an indented titled box (inner top border carrying the
  bold colored sender name, up to two muted snippet lines, inner bottom border),
  consistent with the bubble version but using `slack_indented_line`
  (`crates/tui/src/widgets/message_list.rs:1615-1619`) and the Slack body width
  for inner-box sizing. Update the Slack body-start offset calculation at
  `crates/tui/src/widgets/message_list.rs:1438` to account for the new
  multi-line quote height (no longer a single line). Rationale: keep reply
  semantics identical across presentations and keep `body_start_line` aligned
  with actual rendered lines.

- [x] Task 7. Update the line-count functions so the count pass exactly matches
  the new rendered height. Replace `usize::from(message.reply_to.is_some())`
  with a `quote_block_line_count(...)` helper in both `message_lines_len`
  (`crates/tui/src/widgets/message_list.rs:4260-4267`) and
  `slack_message_lines_len` (`crates/tui/src/widgets/message_list.rs:4292-4298`).
  The helper must compute the same bounded height (inner top border + 1–2 snippet
  lines + inner bottom border, i.e. 3–4 lines) from the same wrapping logic and
  width math used in Task 3. Rationale: the layout cache, scrolling, hit-testing,
  and anchor logic all rely on the count pass agreeing with the draw pass; any
  mismatch corrupts scroll/selection.

- [x] Task 8. Ensure the structured preview is part of the layout cache and its
  invalidation key. Confirm `messages_layout_hash`
  (`crates/tui/src/widgets/message_list.rs:708-741`) already covers the inputs
  (it hashes `reply_to`, sender, and content) and that
  `rebuild_message_layout_cache`
  (`crates/tui/src/widgets/message_list.rs:549-553`) populates the new
  structured map. Rationale: cache correctness — a changed quoted message or
  changed reply target must rebuild the layout.

- [x] Task 9. Verify and, if needed, adjust hit-testing/clickability so the
  quote block lines are treated as part of the message and (optionally) as a
  clickable "jump to quoted message" affordance. Inspect
  `is_clickable_message_content_line`
  (`crates/tui/src/widgets/message_list.rs:1011-1023`) and `message_line_hits`
  (`crates/tui/src/widgets/message_list.rs:901-936`). Rationale: the inner box
  introduces new leading characters (`┌`, `│`, `└` already partly handled by the
  heuristic) nested inside the outer `│ … │`; confirm no false
  negatives/positives and preserve the existing message-level hit region.

- [x] Task 10. Handle the not-loaded quoted-message case explicitly in the
  preview builder/fallback so the inner box still renders with a muted border
  and a muted snippet (`[message not loaded]` or `message {short_id}` via
  `crates/tui/src/widgets/message_list.rs:4231-4238`). Rationale: replies
  frequently reference messages outside the loaded window; the block must
  degrade gracefully, never panic or collapse the layout count.

- [x] Task 11. Add/extend widget unit tests next to the existing message-list
  tests to assert: (a) a reply renders the inner titled box with the quoted
  sender name in the top border and the snippet inside, nested within the outer
  bubble; (b) the rendered line count equals
  `message_lines_len`/`slack_message_lines_len` for the same message
  (count-vs-draw parity); (c) the not-loaded fallback renders the muted
  placeholder box; (d) long quoted text is bounded to the max quote height; (e)
  a long quoted sender name is truncated within the top border without
  overflowing the inner box width. Rationale: count/draw parity is the
  highest-risk regression and must be locked by tests.

- [x] Task 12. Run the CI-mirrored validation locally before completion: per
  `AGENTS.md`, inspect `.github/workflows/ci.yml` and run the same required
  `cargo check`, `cargo test`, `cargo clippy`, and release-build commands.
  Rationale: avoid preventable CI failures; this change touches a hot,
  test-covered rendering path.

## Verification Criteria

- A reply message renders a nested titled box with: an inner top border carrying
  the quoted sender's name (bold, per-sender color), 1–2 muted snippet lines, and
  an inner bottom border — all contained within the reply's outer bubble (Bubbles
  mode) and as an indented titled box (Flat mode). No vertical bar is used.
- The inner box top border never overflows the inner box width; long sender
  names are truncated within the border.
- The quote block visually distinguishes replies from standalone messages,
  matching the agreed nested titled-box design.
- `message_lines_len` and `slack_message_lines_len` return exactly the number of
  lines produced by `message_lines`/`slack_message_lines` for reply messages
  (asserted by a parity test), so scrolling, selection, and anchoring remain
  correct.
- A reply whose quoted message is outside loaded history renders the muted
  `[message not loaded]` placeholder without breaking layout counts.
- Long quoted text is truncated/wrapped to the bounded maximum quote height (no
  unbounded growth).
- `cargo check`, `cargo test`, `cargo clippy`, and the release build all pass.

## Potential Risks and Mitigations

1. **Count/draw mismatch corrupts scrolling and hit-testing.**
   Mitigation: derive both the draw and count paths from one shared
   `quote_block_lines`/`quote_block_line_count` helper sharing the same wrapping
   logic and bound; add an explicit parity test (Task 11b).
2. **Quote-block glyphs trip the clickable-line heuristic** in
   `is_clickable_message_content_line` (`crates/tui/src/widgets/message_list.rs:1011-1023`),
   creating spurious or missing hit regions.
   Mitigation: review and adjust the heuristic (Task 9) and assert message hit
   ranges in tests.
3. **Bubble/inner-box width drift** if the quote is rendered with parallel
   layout instead of the existing `bubble_inner_width`/`bubble_text_line`
   contract, or if the inner box width math diverges between draw and count.
   Mitigation: reuse those functions (Tasks 3–4) for the outer bubble and share
   one inner-box-width helper between draw and count (Tasks 3, 7).
4. **Performance on large histories** if preview building becomes expensive.
   Mitigation: keep structured-preview construction O(messages) and cached in
   `MessageLayoutCache` exactly as the current flat preview is (Task 8); bound
   snippet wrapping to ≤2 lines so no per-message full-text layout occurs.
5. **Per-participant color instability/legibility** across themes.
   Mitigation: deterministic hash→fixed-palette mapping with `theme.accent`
   fallback; verify against existing theme presets (`crates/tui/src/theme.rs`).

## Alternative Approaches

1. **Single styled caption upgrade (minimal):** Keep the one-line pre-bubble
   reply but restyle it (colored sender + bar glyph) without nesting inside the
   bubble. Lower risk and no count changes, but only partially matches WhatsApp
   and still reads as a separate line above the bubble — does not fully solve the
   "looks like a new message" complaint.
2. **Nested titled box (chosen):** Render the quote inside the reply bubble as a
   smaller box whose top border carries the quoted sender's name as a colored
   title, with a bounded muted snippet inside. No vertical bar. Leans into the
   app's box-drawing aesthetic; requires the count-pass updates (Tasks 4, 7) but
   yields the agreed look.
4. **Vertical-bar block (rejected):** A left accent glyph (`▎`) prefixing the
   quote header/snippet lines inside the bubble. Closer to WhatsApp's literal
   rule but the user preferred the titled-box framing over a bar.
3. **Resolve and render full quoted content recursively:** Render the quoted
   message via the same content renderer (media thumbnails, etc.) inside the
   quote. Most faithful for media replies but materially more complex,
   unbounded in height, and at odds with the performance rules; deferred as a
   possible later enhancement built on the structured preview from Task 1.
