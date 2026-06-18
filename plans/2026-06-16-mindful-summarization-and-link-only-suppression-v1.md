# Mindful Summarization: Link-Only Suppression & Mute Verification

## Objective

Make chat-cli more mindful about voice summaries and notifications by:
1. Not dispatching a voice summary when an incoming message is "just a link" (a bare URL or a pure link-preview card with no human-authored message body).
2. Optionally suppressing the visual/desktop notification for link-only messages, controlled by a user setting with a sensible default.
3. Verifying and locking in (via tests) the already-correct behavior that muted chats produce neither notifications nor voice summaries.

The result should reduce low-value noise (links narrated awkwardly by the voice assistant) while preserving notifications/summaries for messages that carry real intent, including links that come with a caption.

## Context From Current Implementation

- Notification + summary gating lives in `maybe_queue_notification` at `crates/tui/src/app.rs:13547-13694`. Gates run in order: historical, from-me, notifications-off, paused, chat-missing, muted, scope. The voice payload is built only after all those gates pass at `crates/tui/src/app.rs:13638-13652`.
- Muted chats already return early at `crates/tui/src/app.rs:13604` (before the payload is built), and there is a second mute re-check at dispatch time at `crates/tui/src/app.rs:13891-13906`. Muting is therefore already fully covered for both paths.
- Voice payloads are built by `payload_for_message` at `crates/tui/src/voice_summary.rs:139-165` and queued/grouped by `queue_voice_summary` at `crates/tui/src/app.rs:13701-13754`.
- Content model: `Content` enum at `crates/core/src/types.rs:411-424`; `LinkPreview` at `crates/core/src/types.rs:515-521`; `Card`/`CardKind` at `crates/core/src/types.rs:426-450`.
- URL helpers already exist: `first_url_in_text` at `crates/tui/src/app.rs:17879-17891` and `is_openable_url` at `crates/tui/src/app.rs:17893-17897`. A Slack-aware variant `remove_first_url_from_text` exists in `crates/tui/src/widgets/message_list.rs:1873`.
- Settings struct `AppSettings` at `crates/storage/src/lib.rs:107-130`, with serde back-compat shim around `crates/storage/src/lib.rs:240-292` and defaults at `crates/storage/src/lib.rs:213-215`.
- Settings UI toggles handled around `crates/tui/src/app.rs:1815-1919` (label, value, and cycling for `VoiceSummaries`, `NotificationScope`, etc.).

## Implementation Plan

### A. Define the "link-only" classifier (shared, pure, testable)

- [ ] Task 1. Add a pure helper (e.g. `is_link_only_content(content: &Content) -> bool`) in `crates/tui/src/app.rs` near the existing URL helpers (`crates/tui/src/app.rs:17879-17897`). Rationale: keeping it beside `first_url_in_text`/`is_openable_url` lets it reuse the established URL-detection logic and keeps all "what is this content" predicates together.
- [ ] Task 2. Define link-only precisely so it never suppresses messages with real intent:
  - `Content::LinkPreview(_)` → link-only (a pure link card with no separate body).
  - `Content::Text(t)` → link-only only when the trimmed text contains exactly one URL token and removing that token leaves no remaining word characters (handle Slack `<url>` / `<url|label>` wrapping; a label means human intent and must NOT be treated as link-only).
  - `Content::Cards(cards)` → link-only only when every card is `CardKind::LinkPreview` and there is no accompanying text body.
  - All other variants (`Image/Video/Audio/File/Sticker/Poll/Deleted/Unsupported`) → not link-only.
  Rationale: captions like "check this out <url>" carry intent and must remain eligible; only the zero-intent bare-link case is suppressed.
- [ ] Task 3. Decide and document the Slack-label handling explicitly in a code comment: `<url|release notes>` is treated as carrying a body (label text) and is therefore NOT link-only. Rationale: prevents over-suppression of deliberately labeled shares.

### B. Suppress the voice summary for link-only messages

- [ ] Task 4. In `maybe_queue_notification` at `crates/tui/src/app.rs:13638-13652`, extend the voice-payload guard so the payload is only built when the message is not link-only (in addition to the existing `voice_summaries && !is_from_me` conditions). Rationale: prevents the bare URL from ever entering the grouping queue or being spoken.
- [ ] Task 5. Emit an instrumentation marker (e.g. `voice_summary.suppress` with `reason=link_only`) at the suppression point, mirroring the existing suppress markers (e.g. `crates/tui/src/app.rs:13873-13905`). Rationale: AGENTS.md requires opt-in performance/decision instrumentation with identifying labels; this makes suppression observable in the debug log.

### C. Optionally suppress the notification for link-only messages (settings-driven)

- [ ] Task 6. Add a setting (e.g. `suppress_link_only_notifications: bool`) to `AppSettings` at `crates/storage/src/lib.rs:107-130`, with a doc comment describing scope and default. Rationale: fully silencing link-only notifications can hide a meaningful shared link, so the behavior must be user-controllable.
- [ ] Task 7. Wire the new field through the serde back-compat shim and defaults at `crates/storage/src/lib.rs:213-215` and `crates/storage/src/lib.rs:240-292`, defaulting to the user's requested behavior (suppress on) while round-tripping cleanly for older config files. Rationale: existing stored settings must continue to load without error and must adopt a deterministic default.
- [ ] Task 8. Add an early-return notification gate in `maybe_queue_notification` (placed after the muted/scope gates, near `crates/tui/src/app.rs:13629`, and before building the notification overlay) that suppresses the visual/desktop notification for link-only content when the setting is enabled. Emit a `notification.suppress` marker with `reason=link_only`. Rationale: matches the established gating pattern and keeps suppression decisions logged.
- [ ] Task 9. Decide ordering/interaction between Task 4 and Task 8: when the notification is suppressed for a link-only message, the summary must also be suppressed (a summary without a notification would be inconsistent). Ensure the link-only summary suppression (Task 4) is independent of the notification setting, so summaries are always skipped for link-only even if the user keeps link-only notifications on. Rationale: the voice channel is the noisiest surface for bare links; keep it always-off for link-only regardless of the visual-notification preference.

### D. Surface the new setting in the Settings UI

- [ ] Task 10. Add the new toggle to the settings enum/label/value/cycle logic near `crates/tui/src/app.rs:1815-1919`, following the existing on/off pattern used for `VoiceSummaries`. Rationale: keeps the option discoverable and consistent with current settings affordances.
- [ ] Task 11. If the README documents notification behavior, add a short line under the "Make it yours" / settings section (`README.md:120-131`). Rationale: only touch docs if an existing relevant section exists; do not create new doc files.

### E. Verify and lock in muted-chat behavior (no new gating expected)

- [ ] Task 12. Add an app-level test asserting that an incoming message in a muted chat queues neither a pending notification nor a pending voice summary, exercising the gate at `crates/tui/src/app.rs:13604`. Rationale: confirms the user-reported concern is already handled and prevents regressions.
- [ ] Task 13. Add a test for the post-queue mute race: a voice summary queued while unmuted is dropped at dispatch time when the chat becomes muted, exercising `crates/tui/src/app.rs:13891-13906`. Rationale: locks in the second-layer protection.

### F. Tests for the new link-only behavior

- [ ] Task 14. Unit-test `is_link_only_content` across: bare URL text, Slack `<url>` wrapped, Slack `<url|label>` (NOT link-only), text-with-caption-plus-url (NOT link-only), `Content::LinkPreview` (link-only), `Content::Cards` all-link-preview (link-only), and media/poll/deleted (NOT link-only). Rationale: the classifier is the correctness core; edge cases must be pinned.
- [ ] Task 15. Add an async app test (mirroring `voice_summary_*` tests around `crates/tui/src/app.rs:19766-20148`) asserting that a link-only message produces no pending voice summary even with `voice_summaries = true`. Rationale: AGENTS.md requires asserting async UI behavior; this validates the queue path, not just the classifier.
- [ ] Task 16. Add a test that with `suppress_link_only_notifications` enabled, a link-only message queues no pending notification, and with it disabled the notification still queues (and summary still suppressed). Rationale: verifies both setting branches and the Task 9 independence rule.
- [ ] Task 17. Add a storage round-trip test for the new setting (default value + explicit override), following `crates/storage/src/lib.rs:2545-2564`. Rationale: guards serde back-compat.

### G. Validation (mirror CI before committing)

- [ ] Task 18. Run the same required checks CI enforces — inspect `.github/workflows/ci.yml` and run the matching `cargo check`, `cargo test`, `cargo clippy`, and release-build commands across the workspace. Rationale: AGENTS.md mandates mirroring CI locally to avoid preventable failures.

## Verification Criteria

- A bare-URL message (plain, Slack-wrapped `<url>`, and `Content::LinkPreview`) never results in a dispatched voice summary, regardless of grouping, and a `voice_summary.suppress reason=link_only` marker is logged.
- A URL accompanied by a caption or a Slack `<url|label>` link still produces a voice summary as before.
- With `suppress_link_only_notifications` enabled, a link-only message raises no notification; with it disabled, the notification still appears but no summary is spoken.
- A message in a muted chat produces neither a notification nor a voice summary (queue-time and dispatch-time), proven by tests.
- New setting round-trips through storage and loads cleanly from a config file written before the field existed.
- `cargo check`, `cargo test`, `cargo clippy`, and the release build all pass for the workspace.

## Potential Risks and Mitigations

1. **Over-suppression hides meaningful links.**
   Mitigation: Restrict link-only to zero-intent bare links; treat any caption or Slack label as intent. Make the notification suppression a setting; keep only the voice summary unconditionally suppressed for link-only.
2. **Provider sends a bare URL as `Text` first, then upgrades to a `LinkPreview` card later (two events).**
   Mitigation: The classifier covers both `Text`-only-URL and `LinkPreview`, so either representation is caught at arrival. Confirm with a test for the text-then-card sequence; ensure the later card event does not resurrect a summary.
3. **Slack angle-bracket / labeled link parsing differences between `app.rs` and `message_list.rs` helpers.**
   Mitigation: Reuse/align with the existing Slack-aware URL handling (`crates/tui/src/widgets/message_list.rs:1825-1873`) and cover wrapped/labeled cases explicitly in unit tests.
4. **Settings serde back-compat break for existing users.**
   Mitigation: Add the field through the existing compat shim with a default, and add a round-trip test loading a pre-field config.
5. **Performance: classification on the input/notification path.**
   Mitigation: The classifier is O(token count) string work on a single message and runs only on the already-gated notification path, not in draw or completion drains — consistent with AGENTS.md bounded-work rules.

## Alternative Approaches

1. **Push the decision into the payload builder (`voice_summary::payload_for_message`) instead of the app gate.** Trade-off: keeps voice logic in one module, but the app still needs the predicate for the notification gate, so centralizing the predicate in the app and reusing it in both places avoids duplication. Preferred: shared predicate in the app, gate at the call sites.
2. **Let `fono` decide via an instruction flag (e.g. pass an "is link-only" hint and let the assistant choose silence).** Trade-off: avoids local heuristics but still spawns a process, incurs latency, and depends on external behavior; contradicts "be more mindful" by doing the work anyway. Preferred only if local classification proves unreliable.
3. **Single combined setting ("mindful summaries") that governs link-only plus future cases (reaction-only, deleted, very short acks).** Trade-off: simpler UI now, but couples unrelated behaviors and is harder to reason about; better to ship the link-only/mute scope first and add granular options if real noise patterns emerge.
4. **Three-state notification preference (Summarize / Notify-only / Silent) for link-only.** Trade-off: most expressive and matches the real middle ground, but more UI surface than a boolean; can be layered on later if the boolean proves too coarse.
