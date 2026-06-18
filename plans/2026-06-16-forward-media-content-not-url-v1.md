# Forward Actual Media Content Instead of URLs Across Slack/WhatsApp

## Objective

When forwarding a message between Slack and WhatsApp (either direction), forward the **actual media bytes** (image, video, audio, file, sticker) rather than a provider-specific URL or permalink. The current behaviour sends a Slack link when forwarding a Slack image to WhatsApp; recipients outside the Slack workspace cannot open it. The goal is for the destination provider to re-upload the real file so it appears as a native attachment in the target chat.

## Initial Assessment

### Project Structure Summary

- Forwarding is orchestrated entirely in the TUI layer: `crates/tui/src/app.rs`.
- Content portability/capability gating lives in free functions in `crates/tui/src/app.rs:17845-17896`.
- The provider abstraction (`Content`, `Media`, `send`, `download_media`, `OutboundCapabilities`) lives in `crates/core/src/types.rs` and `crates/core/src/provider.rs`.
- Slack provider: `crates/providers/slack/src/lib.rs`. WhatsApp provider: `crates/providers/whatsapp/src/lib.rs`.

### Relevant Files Examination

- `crates/tui/src/app.rs:12951-13023` — `forward_message_to_target`: resolves destination provider, calls `forward_content_for_capabilities`, then `provider.send(...)`. Only the **destination** provider is resolved today; the **source** provider is never used to fetch bytes.
- `crates/tui/src/app.rs:13025-13047` — `forward_targets_for_message`: builds the destination list, filtering by `forward_content_for_capabilities`.
- `crates/tui/src/app.rs:17845-17887` — `forward_content_for_capabilities` → `portable_forward_content` → `card_forward_text`. **Root cause:** for `Content::Cards`, `card_forward_text` returns the card URL or title as plain text (app.rs:17875-17887). Media never survives.
- `crates/providers/slack/src/lib.rs:6335-6461` — Slack image/file uploads are mapped to `Content::Cards` with `CardKind::MediaPreview` (image, `card.image` = `Media` whose `id` is the private URL) or `CardKind::ProviderAttachment` (non-image, only a download `url`, `image: None`). **Slack media is therefore never `Content::Image`/`Content::File`.**
- `crates/providers/whatsapp/src/lib.rs:1747-1758` — WhatsApp incoming media maps to `Content::Image/Video/Audio/File/Sticker(media)` with `local_path` populated by the bridge.
- `crates/providers/whatsapp/src/lib.rs:211-243, 430-527` — WhatsApp `send_media_to_bridge` **requires `media.local_path`** to point to a real local file.
- `crates/providers/slack/src/lib.rs:3373-3436, 4068-4090` — Slack `send_file_message` **requires `media.local_path`** to be a real local file (uploads via `files.getUploadURLExternal`/`completeUploadExternal`).
- `crates/providers/slack/src/lib.rs:4092-4145` — Slack `download_media` fetches authenticated bytes to the deterministic cache path using `media.id` (the URL).
- `crates/providers/slack/src/lib.rs:6442-6461` — Slack `Media.local_path` for large files is only a **reserved** cache path (bytes absent until `download_media` runs); small files are auto-downloaded.
- `crates/providers/whatsapp/src/lib.rs:529-531` — WhatsApp `download_media` is **not wired** (bails).
- `crates/tui/src/app.rs:12468-12502` — `start_media_download`: existing async, non-blocking pattern that calls `provider.download_media` and reports via a channel; the canonical template for fetching bytes off the UI hot path.
- `crates/tui/src/app.rs:18133-18163` — `message_image_preview`/`media_image_preview`: existing logic that extracts a card image `Media` and checks `local_path.exists()`. Good reference for "is the file actually present".

### Prioritised Problems

1. **(Highest) Card-borne media is flattened to a URL on forward.** This is the literal bug the user reports for Slack→WhatsApp images. Fixing `portable_forward_content`/`forward_content_for_capabilities` to surface the card's `Media` as `Content::Image`/`Content::File` is the core change.
2. **(High) Destination `send` requires a real local file.** Even after we surface the `Media`, large Slack files have only a reserved cache path (bytes absent). Forwarding must ensure bytes exist locally (via the source provider's `download_media`) before calling the destination `send`, otherwise the send fails.
3. **(Medium) Forward must keep the UI responsive.** Per `AGENTS.md`, network download + upload must not block the input/draw path; the fetch-then-send sequence should run as a background task with status/placeholder feedback and stale-completion handling.
4. **(Medium) WhatsApp→Slack already surfaces `Content::Image`, but relies on `local_path` bytes existing.** WhatsApp incoming media is bridge-downloaded, so this generally works; needs verification and a graceful failure path when the file is missing (WhatsApp `download_media` is unimplemented).
5. **(Low) Non-image Slack attachments (`ProviderAttachment`).** These cards carry only a `url`, no `Media`. Forwarding them as real files requires synthesising a `Media` (id = download URL) so `download_media` can fetch them. Treat as an optional extension.

## Implementation Plan

- [ ] Task 1. Introduce a "forwardable media" extraction step that surfaces card-borne media as real content. In `crates/tui/src/app.rs` near `portable_forward_content` (app.rs:17853-17868), add a helper that, for `Content::Cards`, finds the first card carrying a usable `Media` (prefer `CardKind::MediaPreview` with `card.image`; fall back to `card.thumbnail`) and returns it as `Content::Image` (or the appropriate media-kind variant inferred from `media.mime_type`), carrying the card `title` into the media `caption` when present. Rationale: this is the precise point where Slack images currently degrade to a URL; converting to a media `Content` lets the destination provider upload real bytes.

- [ ] Task 2. Define the media-kind inference rule for converted card media. Add a helper that maps a `Media.mime_type` to the correct `Content` variant (`image/*` and `image/gif` → `Content::Image`; `video/*` → `Content::Video`; `audio/*` → `Content::Audio`; otherwise `Content::File`). Rationale: Slack `MediaPreview` cards are images today, but using the mime type keeps the conversion correct if other media-bearing cards appear, and aligns with `OutboundCapabilities::supports_content` gating (provider.rs:97-111).

- [ ] Task 3. Preserve the existing text fallback ordering. Keep `card_forward_text` (app.rs:17875-17887) as the fallback for cards that carry **no** usable `Media` (e.g. link-preview cards, `ProviderAttachment` without a synthesised media). Rationale: forwarding a pure link card as its URL/text remains the correct behaviour; only media-bearing cards should be upgraded.

- [ ] Task 4. Ensure media bytes exist locally before the destination send. In `forward_message_to_target` (app.rs:12951-13023), after computing the forward `Content`, detect when the content is a media variant whose `Media.local_path` is `None` or points to a non-existent file (reuse the `local_path.exists()` check pattern from `media_image_preview`, app.rs:18151-18157). When bytes are missing, resolve the **source** provider via `self.provider_for_id(&source.account)` and call its `download_media(&media)` to fetch the bytes to the cache path before sending. Rationale: both destination providers reject media `send` without a real local file (whatsapp lib.rs:217-220, slack lib.rs:3381-3384); large Slack files only have a reserved path until downloaded.

- [ ] Task 5. Run the fetch-then-send sequence as a background task to keep the event loop responsive. Model the flow on `start_media_download` (app.rs:12468-12502): spawn the download (if needed) and the destination `send` on a Tokio task, communicate completion over an mpsc channel, and apply results in a drain step (mirroring `drain_media_downloads`, app.rs:12505+). Set an immediate "forwarding …" status on dispatch and a terminal "forwarded to {label}" / failure status on completion. Guard the result application against navigation (apply only if state still allows) per `AGENTS.md`. Rationale: forwarding now performs a download + an upload; doing this inline in the key handler would stall the TUI and violate the performance rules.

- [ ] Task 6. Keep the persisted/echoed message consistent with what was actually sent. When the background send completes, perform the existing post-send bookkeeping currently in `forward_message_to_target` (build the echoed `Message` with the resolved media `Content`, `store.upsert_message`, `update_chat_after_send`, `reload_chats`, conditional `reload_selected_messages` + `scroll_messages_to_bottom`; app.rs:12980-13021). Ensure the echoed `Content` is the **converted media content** (with a valid `local_path`) so the forwarded image renders natively in the source-side history view too. Rationale: preserves the existing sidebar/activity behaviour and avoids a regression where the echoed message shows a URL.

- [ ] Task 7. Make destination filtering recognise card-borne media. Update `forward_targets_for_message` (app.rs:13025-13047) so that the capability check (`forward_content_for_capabilities`) operates on the **converted** content from Task 1. This ensures, for example, that a Slack image offers image-capable WhatsApp destinations (and that text-only destinations are filtered out for media). Rationale: today a Slack image card is treated as text, so every text destination appears; after conversion the picker must reflect true media capability.

- [ ] Task 8. Add a graceful, explicit failure path when bytes cannot be obtained. If `download_media` fails or is unsupported (WhatsApp `download_media` bails today, whatsapp lib.rs:529-531) and no local file exists, abort the forward with a clear status message (e.g. "cannot forward {file}: media not available") instead of sending a broken/empty attachment or silently falling back to a URL. Rationale: prevents sending a non-functional message and gives the user actionable feedback; respects the explicit error-handling requirement in `AGENTS.md`.

- [ ] Task 9. (Optional extension) Support non-image Slack `ProviderAttachment` forwards. For `CardKind::ProviderAttachment` cards that carry only a download `url` (slack lib.rs:6385-6402), synthesise a `Media` with `id` set to that URL (so the source provider's `download_media` can fetch it) and a `file_name`/`mime_type` derived from the card subtitle/title, then forward as `Content::File`. Rationale: extends the fix beyond images to documents; kept optional because it touches Slack card→media synthesis and is broader than the reported image case.

- [ ] Task 10. Update and extend tests. Adjust `forward_content_uses_portable_form_and_provider_capabilities` (app.rs:24745-24778) and add cases asserting: (a) a `Content::Cards` MediaPreview card with an image `Media` converts to `Content::Image` carrying the card title as caption; (b) a link-only card still forwards as text; (c) capability gating still rejects media for text-only destinations. Add an async app-level test mirroring the existing media-download tests (app.rs:25037-25075) that asserts the two-phase forward: immediate "forwarding …" status, then a terminal "forwarded" state after the background download+send drains, including the stale/missing-file failure path. Rationale: `AGENTS.md` requires asserting both the placeholder and final phases for async UI work, and the existing forward tests must be kept green.

- [ ] Task 11. Mirror CI validation locally. Per `AGENTS.md`, inspect `.github/workflows/ci.yml` and run the same required `cargo check`, `cargo test`, `cargo clippy`, and release-build commands for the touched crates (`tui`, and any `core`/provider changes) before considering the work complete. Rationale: avoids preventable CI failures.

## Verification Criteria

- Forwarding a Slack image (rendered as a `MediaPreview` card) to a WhatsApp chat results in the actual image being uploaded and appearing as a native WhatsApp image, not a Slack permalink/text.
- Forwarding a WhatsApp image to a Slack channel continues to upload the actual file (no regression).
- When the source media has not yet been downloaded (large Slack file with a reserved-only cache path), the forward triggers a source-provider download first, then uploads the bytes; the picker and status reflect a media forward, not a text forward.
- The forward picker lists only destinations whose `OutboundCapabilities` actually support the converted media kind.
- The UI shows an immediate in-progress status on dispatch and a terminal success/failure status after the background task completes; the input/draw path is not blocked during download/upload.
- When bytes cannot be obtained (download fails/unsupported and no local file), the forward aborts with a clear status and sends nothing.
- All existing forwarding tests pass and the new two-phase async forwarding tests pass; `cargo check`/`test`/`clippy` and the release build (as required by CI) succeed.

## Potential Risks and Mitigations

1. **Large media download + upload latency and memory pressure.**
   Mitigation: run the entire fetch-then-send sequence on a background task (Task 5) using the established `start_media_download` pattern; rely on each provider's existing on-demand size limits and blocking-thread download paths; surface progress via status only.
2. **WhatsApp `download_media` is unimplemented, so forwarding an un-cached WhatsApp media item out to Slack would fail.**
   Mitigation: detect missing bytes and abort with a clear message (Task 8); document that WhatsApp-sourced forwards rely on bridge-downloaded local files; treat wiring WhatsApp `download_media` as out-of-scope follow-up.
3. **Stale completion after the user navigates away during the background forward.**
   Mitigation: apply results only when still valid (selection/account checks) exactly as existing drains do; never regress sidebar ordering (Task 5/6, per `AGENTS.md`).
4. **Mime-type-based variant inference could misclassify edge cases (e.g. `image/*` generic).**
   Mitigation: default unknown/ambiguous types to `Content::File`, which every media-capable destination supports; keep `image/gif` mapped to `Content::Image` so the GIF capability branch (provider.rs:100-102) applies.
5. **Echoed/persisted forwarded message could still show a URL if the converted content is not used for bookkeeping.**
   Mitigation: thread the converted media `Content` (with a valid `local_path`) through `upsert_message` and chat-preview updates (Task 6).

## Alternative Approaches

1. **Per-provider conversion in the destination `send`:** Have each destination provider accept `Content::Cards` and internally extract/upload media. Trade-off: spreads forwarding logic across providers, duplicates extraction, and couples providers to Slack's card representation — rejected in favour of a single conversion point in the TUI forward path.
2. **Re-upload by URL handoff (destination fetches the URL):** Pass the Slack URL to the destination provider and let it download+upload. Trade-off: destination providers would need cross-provider authenticated access to Slack URLs (impossible for WhatsApp), so it cannot work for private workspace files — rejected.
3. **Eagerly download all media at receive time:** Always fetch full bytes so `local_path` is always present. Trade-off: violates the lazy/auto-download size limits and adds bandwidth/storage cost for media never forwarded — rejected in favour of on-demand download during forward (Task 4).
