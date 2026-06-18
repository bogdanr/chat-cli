# Improve NewRelic Alert Card Rendering (Slack Block Kit Support)

## Objective

Make Slack Block Kit bot messages (e.g. NewRelic alert cards in `#newrelic-alerts`) render as structured cards in the TUI — status line, bold linked title, action buttons, alert detail sections, inline chart image, and context footer — instead of the current flattened plain-text fallback ("Critical priority issue is active 🧰 Acknowledge button ✔ Close button …").

### Current vs. target

| Slack renders | chat-cli renders today | Root cause |
| --- | --- | --- |
| 🟥 Critical priority line + bold blue linked title | One run-on paragraph | `blocks` never deserialized (`crates/providers/slack/src/lib.rs:481-499`); Slack's plain-text fallback in `text` is used verbatim |
| Acknowledge / Close buttons | Literal text "Acknowledge button ✔ Close button" | Fallback text; `Card.actions` (`crates/core/src/types.rs:286`) exists but is never populated or rendered |
| Inline chart image | Nothing | `image` blocks unmodeled; attachment `image_url` path sets `local_path: None` (`crates/providers/slack/src/lib.rs:5524-5539`) so no preview/retrieve |
| "Edit workflow" link label | Raw `<https://radar-api…\|⚙ Edit workflow>` token + stray link-preview card | mrkdwn `<url\|label>` tokens never rewritten to labels |
| Bold section labels ("1 alert event", "1 policy") | Italic or unstyled | Slack mrkdwn (`*bold*`, `~strike~`) mismatches the renderer's markdown dialect (`crates/tui/src/widgets/message_list.rs:3432-3487`) |

## Implementation Plan

### Phase 1 — Parse Block Kit payloads (provider)

- [ ] Task 1. Add an optional `blocks: Option<Vec<serde_json::Value>>` field to `SlackHistoryMessageResponse` (`crates/providers/slack/src/lib.rs:481-499`) and to the realtime structs `SlackRealtimeEvent` / `SlackRealtimeInnerMessage` (`crates/providers/slack/src/lib.rs:966-1004`), then decode each entry leniently into a new `SlackBlockResponse` enum, skipping unknown block types with a `slack_diagnostic_log` entry. Rationale: per-value lenient decoding mirrors the existing `deserialize_lenient_slack_messages` resilience pattern (`crates/providers/slack/src/lib.rs:525-542`) so novel block types can never break message ingestion.
- [ ] Task 2. Model the block variants needed by the NewRelic card and common bot messages: `section` (mrkdwn/plain text, optional `fields`, optional `accessory` image/button), `header`, `image` (with `image_url`, `alt_text`, `title`), `actions` (buttons with `text`, optional `url`, `style`), `context` (mixed text + image elements), `divider`, and `rich_text` (flatten elements to text as a degradation path). Rationale: these seven cover the full NewRelic layout and the overwhelming majority of bot/app messages.
- [ ] Task 3. Write a `slack_block_cards(&[SlackBlockResponse]) -> Vec<Card>` converter that groups consecutive blocks into cards: `header` → card `title`; `section` text → card `body` (with section `fields` → `CardField`s); `image` block → `Card { kind: MediaPreview, image: Some(media) }` or attach to the preceding card's `image` when it directly follows a section; `actions` buttons → `CardAction { label, url }` on the preceding card (synthesize a card if none); `context` → card `footer`; `divider` → card boundary. Set `kind: CardKind::BotMessage`, `source: CardSource::Slack`. Rationale: keeps the provider-neutral `Card` model unchanged while reproducing Slack's visual grouping.
- [ ] Task 4. Wire blocks into `slack_message_from_parts` (`crates/providers/slack/src/lib.rs:5232-5322`): when parsed block cards are non-empty, use them instead of the top-level fallback `text` for content (Slack populates `text` only as a notification fallback when `blocks` exist), while still appending attachment/file cards. Derive the plain-text used for `mentions_me` detection and sidebar `last_message_preview` from the block text (first header/section line) so notification scope and chat ordering rules keep working. Rationale: prevents the duplicate wall-of-text card while preserving the activity/ordering guarantees in the project guidelines.

### Phase 2 — mrkdwn fidelity

- [ ] Task 5. Add a `slack_mrkdwn_to_markdown` translation applied to block text (and to `slack_message_own_text`, `crates/providers/slack/src/lib.rs:5324-5328`): `*bold*` → `**bold**`, `~strike~` → `~~strike~~`, `<url|label>` → `label` (keeping the URL available for link hit regions where the renderer supports it; bare `<url>` stays as the URL). Reuse the existing `replace_slack_emoji_codes` pass. Rationale: the provider already emits the renderer's dialect for attachment titles (`crates/providers/slack/src/lib.rs:5369`); this makes the NewRelic title render bold and "⚙ Edit workflow" show its label instead of the raw token, which also stops the spurious link-preview card from triggering on the angle-bracket URL.

### Phase 3 — Image pipeline for block/attachment images

- [ ] Task 6. Route block `image_url`s (and fix the existing attachment `image_url` dead-end in `slack_card_media`, `crates/providers/slack/src/lib.rs:5524-5539`) through `slack_cached_media_path` (`crates/providers/slack/src/lib.rs:6138-6195`) with `auth_token: None`, since these are public URLs. Keep the download on the existing background worker with the failure-cooldown dedupe. Rationale: gives the NewRelic chart a deterministic cache path so the existing `media_preview_rows` placeholder-then-decode pipeline (`crates/tui/src/widgets/message_list.rs:2958-3009`) renders it with zero renderer changes, honoring the cache-only-draw rule.
- [ ] Task 7. Persist card `image`/`thumbnail` media in storage: `StoredCard::into_card` currently discards them (`crates/storage/src/lib.rs:1323-1324`), so previews would vanish after restart. Store at least `local_path`, `url`, `mime`, and size so rehydrated cards keep working previews/retrieve affordances. Rationale: without this, Phase 3 only works for messages still in memory.

### Phase 4 — Render actions and polish the card layout (TUI)

- [ ] Task 8. Render `Card.actions` in `flat_card_lines` (`crates/tui/src/widgets/message_list.rs:2081-2203`) and `generic_card_lines` (`:2205-2317`) as a single row of button-styled pills (e.g. `[ 🧰 Acknowledge ] [ ✔ Close ]`) using a distinct style; register a clickable hit region (reusing the existing link-hit machinery) for actions that carry a URL, and render URL-less interactive buttons as inert labels. Rationale: Slack interactivity (`response_url` round-trips) is out of scope, but visually distinct buttons remove the confusing "Acknowledge button ✔ Close button" prose.
- [ ] Task 9. Apply card accent styling for alert severity: when the first block/card title begins with a colored-square emoji (🟥/🟧/🟨) map it to `accent_color` via the existing `card_accent_style` path (`crates/tui/src/widgets/message_list.rs:2319`). Rationale: cheap visual parity with Slack's red attachment bar; purely additive.

### Phase 5 — Tests and validation

- [ ] Task 10. Add provider tests in the existing `mod tests` of `crates/providers/slack/src/lib.rs` using a captured NewRelic-style blocks payload: blocks decode leniently (unknown block type skipped, message kept); block cards carry title/body/footer/actions/image; fallback `text` suppressed when blocks parse; mrkdwn translation cases (`*b*`, `~s~`, `<url|label>`); preview text derived from blocks.
- [ ] Task 11. Add renderer tests in `crates/tui/src/widgets/message_list.rs` `mod tests`: actions row renders as pills without the word "button"; URL action exposes a clickable hit; image card shows the loading placeholder immediately and the decoded preview after draining background completions (both phases, per project guidelines).
- [ ] Task 12. Mirror CI locally before commit per `.github/workflows/ci.yml`: `cargo check`, `cargo test`, `cargo clippy`, and the release build as applicable.

## Verification Criteria

- A NewRelic alert message renders as: accent-marked title line, bold linked issue title, `[ Acknowledge ] [ Close ]` pills, "1 alert event · …" section, chart image (placeholder first, decoded preview after background fetch), "1 impacted entity / 1 condition / 1 policy" lines, and a footer with "⚙ Edit workflow" as a label — no raw `<url|label>` tokens, no "button" prose, no duplicate fallback paragraph.
- Unknown block types in any message never cause the message (or the history page) to be dropped; a diagnostic log line is emitted instead.
- Sidebar preview and ordering for `#newrelic-alerts` still reflect actual message activity (first meaningful block line as preview).
- Card image previews survive an app restart (storage round-trip keeps media paths).
- All existing provider/renderer tests pass unchanged; new tests cover both placeholder and final render phases.

## Potential Risks and Mitigations

1. **Blocks vs. fallback divergence** — some apps put richer text in `text` than in `blocks`.
   Mitigation: only suppress fallback `text` when block parsing yields at least one card with non-empty content; otherwise fall back to today's behavior.
2. **Unbounded image downloads from arbitrary block URLs.**
   Mitigation: reuse the existing auto-download size limit, dedupe, and failure-cooldown in `slack_cached_media_path`; downloads stay on the background worker, never in draw/input paths.
3. **Storage schema change for card media breaks existing rows.**
   Mitigation: make new `StoredCard` fields optional with serde defaults so old JSON blobs rehydrate as before (image-less), no migration needed.
4. **Layout-cache staleness when a card's image finishes downloading.**
   Mitigation: the preview cache key `(path, width, rows)` already drives invalidation; ensure block-image cards reserve the cache path up front so the key is stable from first render.
5. **mrkdwn translation corrupting code spans or literal asterisks.**
   Mitigation: skip translation inside backtick spans; add regression tests with mixed literals.

## Alternative Approaches

1. **Render-side cleanup only** (regex the fallback text: strip "button" suffixes, unwrap `<url|label>`): tiny effort, but no buttons, no chart, no structure — treats symptoms and breaks on each bot's fallback phrasing.
2. **Full Block Kit mirror model** (dedicated `Block` content variant rendered natively instead of mapping to `Card`): highest fidelity (rich_text trees, overflow menus), but adds a parallel rendering path, storage variant, and per-provider divergence the `Card` abstraction was built to avoid. Revisit only if card mapping proves too lossy.
3. **Interactive buttons** (POST to Slack `response_url`/actions API on click): genuinely actionable Acknowledge/Close, but requires interactivity payload plumbing and app-level auth scopes; deferred as a follow-up once buttons are visible.

## Assumptions

- Block Kit chart/image URLs from NewRelic are publicly fetchable without the workspace token (standard for `image` blocks).
- Slack interactivity (executing Acknowledge/Close) is out of scope for this iteration; buttons are visual, URL buttons open links.
- The `Card` model and storage `StoredCardAction` (`crates/storage/src/lib.rs:1264-1268`) need no shape changes — only population and rendering.
