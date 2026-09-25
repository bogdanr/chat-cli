# Outbound Mentions Across All Chat Vendors

## Objective

Let a user mention a person from the compose box in any connected account — type `@`, pick a
name from an autocomplete (e.g. `@Bogdan`), and have the message actually notify that person
on the receiving platform. Today mentions are an **inbound-only** concept: providers resolve
`<@U123>` / `@<jid>` / `@Name` to display text on the way in and compute a `mentions_me`
boolean, but the outbound path sends the raw composed string with no mention assistance and no
provider-native encoding (`crates/core/src/provider.rs:203-208`,
`crates/tui/src/app.rs:11725-11794`).

The feature has three cooperating parts:

1. **Compose autocomplete** (TUI) — a `@`-triggered picker mirroring the existing emoji picker,
   sourced from the chat's already-cached member roster.
2. **Provider-native encoding** (per provider) — convert the chosen display token into the form
   each platform understands: Slack `<@U123>`, ClickUp `@Display Name`, WhatsApp `@<phone>`
   **plus** a `MentionedJID` list carried out-of-band.
3. **Outbound mention plumbing** (core) — a way to pass resolved mention identities from the TUI
   through `Provider::send` to providers that need them.

## Initial Assessment

### Project structure summary

Workspace (`Cargo.toml:3-13`): `core` (domain + `Provider` trait), `tui` (Ratatui front-end),
`storage` (SQLite), `chat-cli` (composition root), `providers/{slack,clickup,whatsapp}`, plus
`notify` and a stub `mcp`. Dependency direction is clean: providers depend on `core`; `tui`
depends on `core`; only `chat-cli` constructs concrete providers. The domain model is
transport-agnostic, and the single `Provider::send(chat_id, content, reply_to)` signature is the
one outbound choke point.

### Relevant files examined

- **Outbound contract**: `crates/core/src/provider.rs:46-57` (`OutboundCapabilities`),
  `crates/core/src/provider.rs:203-208` (`send`), `crates/core/src/provider.rs:286-289`
  (`chat_members`).
- **Domain types**: `crates/core/src/types.rs:228-250` (`Message`), `:252-257` (`Sender`),
  `:289-310` (`ChatMember`/`ChatMemberRole`), `:413-425` (`Content::Text`).
- **Compose + emoji picker (the analog to copy)**: `crates/tui/src/app.rs:1131-1156`
  (`ComposeEmoticonPicker`), `:11542-11615` (completion + insertion),
  `:16932-16949` (`compose_emoticon_query`), `:9402-9434` (picker key handling),
  `:6844-6912` (picker draw), `:15320-15344` (picker rect).
- **Compose send path**: `crates/tui/src/app.rs:11725-11794` (`send_composed_message`),
  `:11664-11723` (thread compose).
- **Member roster (candidate source)**: `crates/tui/src/app.rs:2436-2437`
  (`chat_members`/`loading_chat_members`), `:12588-12621` (`request_selected_chat_members`),
  `:3851-3893` (`drain_chat_member_fetches`), `:17583-17589` (`chat_supports_member_listing`).
- **Per-provider send + inbound mention handling**: Slack `send_text_message`
  `crates/providers/slack/src/lib.rs:3351-3406` and inbound `replace_slack_user_mentions`
  `:6998-7020`; WhatsApp `send` `crates/providers/whatsapp/src/lib.rs:432-470`, bridge
  `send_text` `crates/providers/whatsapp/src/bridge.rs:120-143`, Go `resolveMentions`
  `crates/providers/whatsapp/go/bridge.go:2597-2645`; ClickUp `body_mentions_user`
  `crates/providers/clickup/src/convert.rs:493-520`.
- **Rendering**: `crates/tui/src/widgets/message_list.rs:3531-3593` (markdown segmenter — no `@`
  handling).
- **Dormant directory**: `crates/storage/src/schema.rs:76-89` (`persons`/`handles`, unused by
  providers/TUI).

### Prioritized challenges (highest first)

1. **Carrying mention identity out-of-band (WhatsApp).** Slack and ClickUp can encode mentions
   inside the text itself, but WhatsApp requires a `MentionedJID` list alongside the text. This
   forces a change to the outbound contract and to the Go/FFI bridge — the highest-risk,
   highest-effort item.
2. **Resolving a display token back to an identity.** The composer holds human text (`@Bogdan`);
   the provider needs an ID. Matching by display name is fragile (spaces, duplicates, edits) and
   must be defined precisely.
3. **Candidate sourcing without violating the responsiveness rules.** The roster may be large
   (Slack channels) and may not be loaded yet. Autocomplete must run in the input path, so it
   must be bounded, cached, and show a loading placeholder rather than fetching synchronously
   (`AGENTS.md`).
4. **`send` signature ripple.** Extending the outbound contract touches all providers, the mock,
   and every TUI call site/test.
5. **Identity/capability limits.** Slack webhook/bot identities and WhatsApp DMs may not support
   mentions; the UI should not offer what the account cannot do.

## Clarity Assessment (Assumptions)

- **Trigger**: `@` at a token boundary (start of text or preceded by whitespace) opens the
  picker; `@` inside a word (e.g. `user@host`) does **not**. This mirrors the whitespace-token
  logic in `compose_emoticon_query` (`crates/tui/src/app.rs:16932-16949`).
- **Insertion form**: the composer keeps a readable display token `@DisplayName` (like Slack's
  own client); conversion to native syntax happens at send time, not at pick time.
- **Resolution strategy**: on send, the provider scans the final text for `@DisplayName` tokens
  and matches them against the chat roster, longest-name-first, case-insensitive; unresolved or
  ambiguous tokens are sent as plain text. (Chosen over span-tracking because it survives edits.)
- **Candidate source**: the selected chat's `chat_members` roster. For `ChatKind::Direct` chats
  without a roster (e.g. WhatsApp DMs), fall back to the single counterpart derived from the
  chat name / `sender_cache`.
- **Broadcast mentions**: offer `@here`/`@channel`/`@everyone` for Slack channels and `@all` for
  ClickUp; WhatsApp broadcast mention is deferred (not exposed by the send API in use).
- **Scope**: group/channel chats are the primary target; DM mentions are best-effort.
- **Inbound highlighting** (styling `@Name` in the message body) is a nice-to-have, scoped as an
  optional final phase.

## Implementation Plan

### Phase 1 — Core model and outbound contract

- [x] Task 1. Add a `Mention` type to `crates/core/src/types.rs` (near `Sender`/`ChatMember`,
  `crates/core/src/types.rs:252-310`) carrying the resolved identity needed to encode a mention:
  `platform_id: PlatformId` and `display_name: Arc<str>`. Rationale: one small, transport-neutral
  struct lets the TUI hand resolved identities to providers without leaking provider syntax.
- [x] Task 2. Add an `OutboundMentions` result type and a `Provider::encode_outbound_mentions`
  method with a pass-through default (`text` unchanged, empty mention list) in
  `crates/core/src/provider.rs` (near `send`, `:198-208`). Signature: takes the composed text and
  the chat roster (`&[ChatMember]`), returns the provider-native text plus the `Vec<PlatformId>`
  of mentioned users. Rationale: the default keeps every existing provider compiling and makes
  "no mention support" the safe baseline; providers override to add encoding.
- [x] Task 3. Extend the outbound send contract so mentioned IDs reach providers that need them
  out-of-band. Preferred: bundle into a small `OutboundContent { content: Content, mentions:
  Vec<PlatformId> }` and change `Provider::send` to take it (`crates/core/src/provider.rs:203-208`).
  Rationale: keeps `send` at a sane arity and makes the mention list explicit rather than a loose
  trailing parameter. (See Alternative 1 for the trade-off vs. a bare extra argument.)
- [x] Task 4. Add a `mentions: bool` capability to `OutboundCapabilities`
  (`crates/core/src/provider.rs:46-57`), defaulting to `false`, and include it in `all()`.
  Rationale: lets the UI hide the picker for identities that cannot mention (e.g. Slack webhook),
  consistent with the existing capability-negotiation pattern (`:97-124`).
- [x] Task 5. Update `MockProvider` (`crates/core/src/mock.rs`) to implement the new `send`
  signature and `encode_outbound_mentions`, recording mentions so tests and `--mock-provider`
  exercise the path. Rationale: the mock is the reference implementation and the demo surface.

### Phase 2 — TUI compose mention autocomplete

- [x] Task 6. Add a `ComposeMentionPicker` struct mirroring `ComposeEmoticonPicker`
  (`crates/tui/src/app.rs:1131-1156`): `selected`, `scroll_offset`, `query`, `matches:
  Vec<usize>`, `token_char_len`, plus a `keep_selected_visible` helper. Store it on `AppState`
  next to `compose_emoticon_picker` (`crates/tui/src/app.rs:2378`). Rationale: reusing the proven
  picker shape minimizes new UI risk.
- [x] Task 7. Add `compose_mention_query(text, cursor)` mirroring `compose_emoticon_query`
  (`crates/tui/src/app.rs:16932-16949`): require a leading `@` at a token boundary, reject empty
  queries and queries containing whitespace/`@`, and return `(query, token_char_len)`. Rationale:
  a boundary-anchored trigger avoids firing on email addresses and keeps parity with the emoji
  detector.
- [x] Task 8. Add a cached, bounded candidate builder for the selected chat: derive the match
  list from `state.chat_members[(account, chat)]` (plus broadcast entries for channels), filtered
  by prefix/substring against the query and capped (reuse the emoji cap constant style). Cache
  the built candidate list by `(account, chat)` and rebuild only when the roster changes.
  Rationale: `AGENTS.md` forbids heavy work in the input path, so filtering must run over a
  precomputed, bounded list, not the raw roster or storage.
- [x] Task 9. Add `update_compose_mention_completion` and call it from `apply_compose_edit_input`
  alongside `update_compose_emoticon_completion` (`crates/tui/src/app.rs:11623-11627`). When the
  roster is not yet loaded, show a lightweight "loading members…" placeholder in the picker and
  trigger `request_selected_chat_members` (`crates/tui/src/app.rs:12588-12621`) rather than
  blocking. Rationale: satisfies the placeholder-then-final requirement for async UI work.
- [x] Task 10. Add `handle_compose_mention_picker_key` mirroring
  `handle_compose_emoticon_picker_key` (`crates/tui/src/app.rs:9402-9434`): `Esc` closes,
  `Up`/`Down` move selection with scroll-on-edge, `Enter`/`Tab` inserts. Invoke it in
  `handle_compose_key` before the emoji picker (the two are mutually exclusive by trigger char,
  `@` vs `:`). Rationale: keeps key precedence deterministic and prevents double-handling.
- [x] Task 11. Add `insert_selected_compose_mention` mirroring `insert_selected_compose_emoticon`
  (`crates/tui/src/app.rs:11590-11606`): backspace `token_char_len` chars, insert `@DisplayName`
  followed by a trailing space, then close the picker. Rationale: the trailing space terminates
  the token so subsequent typing does not extend the mention.
- [x] Task 12. Add `draw_compose_mention_picker` and its rect helper mirroring
  `draw_compose_emoticon_picker` (`crates/tui/src/app.rs:6844-6912`) and
  `compose_emoticon_picker_rect` (`:15320-15344`); render it in the draw path next to the emoji
  picker (`crates/tui/src/app.rs:4387-4388`), self-guarding on `Some`. Include a loading state and
  `↑/↓ N more` affordances. Rationale: consistent look and behavior with the emoji picker.
- [x] Task 13. Add the DM fallback candidate source: for `ChatKind::Direct` chats without a
  roster, derive the single counterpart from the chat name / `sender_cache`
  (`crates/tui/src/app.rs:2442`). Rationale: makes `@` useful in 1:1 chats where member listing
  is unavailable (e.g. WhatsApp DMs, `chat_supports_member_listing`, `:17583-17589`).
- [x] Task 14. Gate picker availability on `outbound_capabilities().mentions` for the active
  account; when unsupported, suppress the picker and surface a one-line status. Rationale: avoids
  offering mentions an identity cannot deliver (e.g. Slack webhook).

### Phase 3 — Per-provider mention encoding

- [x] Task 15. **Slack**: implement `encode_outbound_mentions` to rewrite `@DisplayName` tokens to
  `<@platform_id>` using the roster (longest-name-first, case-insensitive), and map broadcast
  entries to `<!here>`/`<!channel>`/`<!everyone>`. `send` ignores the out-of-band list (the ID is
  already in the text). Set `mentions: true` in `outbound_capabilities` for user/bot identities
  (`crates/providers/slack/src/lib.rs:3972-3990`), `false` for webhook. Rationale: Slack's API
  mentions by user ID in mrkdwn; the inbound counterpart already exists
  (`replace_slack_user_mentions`, `:6998-7020`) and this is its inverse.
- [x] Task 16. **ClickUp**: implement `encode_outbound_mentions` to keep/emit `@Display Name`
  (ClickUp resolves mentions server-side from the rendered markdown name; see
  `body_mentions_user`, `crates/providers/clickup/src/convert.rs:493-520`). Map `@all` broadcast.
  Set `mentions: true`. Rationale: matches ClickUp's documented rendered-mention form, so no ID
  encoding is required.
- [x] Task 17. **WhatsApp (Rust)**: implement `encode_outbound_mentions` to emit `@<phone>`
  tokens in the text and return the mentioned JIDs, and thread the JID list from the new
  `send` payload into the bridge call (`crates/providers/whatsapp/src/lib.rs:432-470`). Normalize
  JIDs (strip device/agent suffixes) as the existing send path already does (`:442-447`).
  Rationale: whatsmeow requires `MentionedJID` alongside the `@<phone>` text token.
- [x] Task 18. **WhatsApp (bridge)**: extend `bridge::send_text` (`crates/providers/whatsapp/src/bridge.rs:120-143`)
  and the Go `C_SendText`/`sendText` to accept a list of mentioned JIDs and set `MentionedJID` on
  the outgoing message; mirror the inbound `resolveMentions`/`rewriteMentionTokens` logic in
  reverse (`crates/providers/whatsapp/go/bridge.go:2597-2645`). Keep the FFI change
  backward-compatible (empty list = current behavior) and NUL-safe via `CString`. Rationale: the
  Go bridge is the only layer that can set protocol mention metadata.

### Phase 4 — Inbound mention rendering (optional polish)

- [x] Task 19. Extend the markdown segmenter (`crates/tui/src/widgets/message_list.rs:3531-3593`)
  to detect `@Name`/`@here`-style tokens and render them with a distinct style (e.g. accent
  color). Rationale: makes mentions visually obvious in the transcript, matching the compose-side
  affordance; optional and independently shippable.

### Phase 5 — Tests

- [x] Task 20. Core tests: default `encode_outbound_mentions` is pass-through; `OutboundContent`
  round-trips the mention list; `OutboundCapabilities::mentions` defaults false.
- [x] Task 21. Slack tests: `@Name` → `<@U123>` with a roster (including a spaced name like
  "Ada Lovelace" and a duplicate-name ambiguity case), broadcast tokens, and unresolved tokens
  left as plain text.
- [x] Task 22. WhatsApp tests: text token + JID list produced; Rust-side mapping of the new `send`
  payload into the bridge call; Go bridge test that a mention list sets `MentionedJID` and an
  empty list preserves current behavior (`crates/providers/whatsapp/go/bridge_test.go`).
- [x] Task 23. ClickUp tests: `@Name` passthrough and `@all` mapping.
- [x] Task 24. TUI tests: `compose_mention_query` boundary rules (fires at start/after space,
  not inside `user@host`); picker opens/filters/inserts; the two-phase async case — immediate
  loading placeholder when the roster is absent, then populated matches after draining
  `drain_chat_member_fetches` (`crates/tui/src/app.rs:3851-3893`), per `AGENTS.md`.
- [x] Task 25. Mock tests: mentions recorded on send for the `--mock-provider` path.

### Phase 6 — Validation and docs

- [x] Task 26. Run the CI-mirrored checks locally per `AGENTS.md`: `cargo check`, `cargo test`,
  `cargo clippy -- -D warnings`, and the release build, matching `.github/workflows/ci.yml`; run
  `go test ./...` in `crates/providers/whatsapp/go` for the bridge change.
- [x] Task 27. Update user-facing docs where mentions are described: the shortcut/help table
  (compose hint near `crates/tui/src/app.rs:7609`) and the README "Getting around" table
  (`README.md:144-155`) to note the `@` mention autocomplete. Rationale: discoverability.

## Verification Criteria

- Typing `@` at a token boundary in the compose box opens a suggestion list of the chat's members;
  typing further filters it; `Up`/`Down` navigate; `Enter`/`Tab` inserts `@DisplayName `.
- Typing `@` inside a word (e.g. `user@host`) does **not** open the picker.
- When the roster is not yet loaded, the picker shows a loading placeholder and populates after
  background completion, without blocking the event loop.
- Sending `@Bogdan` in a Slack channel posts a message whose text contains `<@U…>` and actually
  notifies Bogdan.
- Sending `@Bogdan` in a ClickUp chat posts `@Bogdan` (rendered as a mention by ClickUp).
- Sending `@Bogdan` in a WhatsApp group posts `@<phone>` text **and** carries the JID in
  `MentionedJID`, so Bogdan is notified.
- Unresolved or ambiguous `@tokens` are sent as plain text (no dropped content, no panic).
- Accounts/identities without mention support (e.g. Slack webhook) do not offer the picker.
- Existing emoji autocomplete, compose wrapping, reply, and attachment flows are unchanged.
- `cargo check`, `cargo test`, `cargo clippy -- -D warnings`, the release build, and
  `go test ./...` (WhatsApp bridge) all pass.

## Potential Risks and Mitigations

1. **Display-name matching is ambiguous or fragile** (duplicate names, names with spaces, edits
   after insertion).
   Mitigation: match longest-name-first and case-insensitively against the roster; on ambiguity or
   no match, send the token as plain text rather than guessing; document the behavior. Offer the
   span-tracking alternative (Alternative 3) if ambiguity proves common in practice.
2. **WhatsApp bridge/FFI change is cross-language and protocol-sensitive.**
   Mitigation: keep the FFI addition backward-compatible (empty list = today's behavior), use
   `CString` for NUL safety, mirror the existing inbound mention logic, and cover it with a Go
   bridge test plus a Rust-side mapping test.
3. **Autocomplete cost in the input path** (large Slack rosters).
   Mitigation: build and cache a bounded candidate list per `(account, chat)`, cap matches, and
   never read storage/network during typing — per `AGENTS.md`. Add opt-in perf instrumentation for
   the candidate build if it shows up in logs.
4. **`send` signature change ripples across providers, mock, and tests.**
   Mitigation: bundle the payload into `OutboundContent` (Task 3) so the change is one type; let
   the compiler enumerate every call site; update the mock first as the reference.
5. **Picker key precedence between `@` and `:` triggers.**
   Mitigation: the triggers are mutually exclusive by first character; handle the mention picker
   before the emoji picker in `handle_compose_key` and keep each self-guarding on `Some`.
6. **Slack webhook/bot and WhatsApp DM mention limits.**
   Mitigation: gate the picker on `OutboundCapabilities::mentions`; set the flag per identity;
   fall back to plain text where unsupported.
7. **ClickUp server-side mention resolution uncertainty.**
   Mitigation: emit the documented `@Display Name` form and verify against a live workspace
   during implementation; if name resolution is unreliable, fall back to `@<user_id>` (already
   recognized by `body_mentions_user`).

## Alternative Approaches

1. **Bare extra parameter vs. bundled `OutboundContent`.** Adding `mentions: &[PlatformId]` to
   `send` is the smallest diff but grows the arity; a bundled struct is cleaner and extensible
   (future: formatting, silent flag). Trade-off: one new type and a wider call-site update vs. a
   simpler signature. Recommended: bundled struct.
2. **Insert provider-native tokens at pick time.** Have the picker insert `<@U123>` (Slack)
   directly so no send-time encoding is needed. Trade-off: simplest send path, but the composer
   becomes unreadable and WhatsApp still needs the out-of-band JID list, so it does not remove the
   hard part. Rejected as the primary approach.
3. **Span-tracked pending mentions instead of name scanning.** Record `(span, member)` as the user
   picks, and substitute by span at send. Trade-off: robust to duplicate names, but fragile to
   edits and deletion; name scanning survives edits. Recommended: name scanning, with span
   tracking as a fallback if ambiguity is common.
4. **Wire the dormant `Person`/`Handle` directory** (`crates/storage/src/schema.rs:76-89`) into
   mention lookup for cross-account identity. Trade-off: enables unified mentions across accounts,
   but requires populating and maintaining a directory no provider currently writes. Deferred as a
   future enhancement.

## Handoff Note

This is a strategic plan only; no source files were modified. Implementation requires an
implementation agent (e.g. Forge). Highest-risk items to watch: the WhatsApp bridge/FFI mention
plumbing (Tasks 17–18), the display-name resolution policy (Task 15 and the Clarity Assessment),
and the input-path performance of the candidate builder (Tasks 8–9). Suggested implementation
order: Phase 1 → Phase 2 (with the mock) → Slack → ClickUp → WhatsApp → optional Phase 4 → tests
and validation.
