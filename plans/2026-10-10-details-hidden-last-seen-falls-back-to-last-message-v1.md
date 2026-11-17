# Hidden "last seen" falls back to "Last message X ago"

## Objective
When a contact hides their last-seen time (WhatsApp sets `last_seen_hidden`), stop showing "Last seen hidden". Show "Last message 3h ago" instead, worked out from locally synced messages in the current chat. Use this in both the Details member list and the member profile popup. Only show "Last seen hidden" when there is no local message to fall back to.

## Findings
- `crates/tui/src/member_profile.rs:327-348`: `presence_label` returns `Some("Last seen hidden")` for hidden offline presence. It has no way to know the person's last message time.
- `crates/tui/src/app.rs:6690-6700`: the member list already has a "last msg …" fallback, but it runs only when `presence_label` returns `None`. A hidden last-seen returns `Some(...)`, so the fallback never runs. This is the user's bug.
- `crates/tui/src/member_profile.rs:794-808`: the popup header uses `presence_label` too, with no fallback. The person's `last_at` is already available through `view.stats.member(&popup.platform_id, popup.is_self)`.
- `crates/tui/src/member_profile.rs:1234-1257`: an existing test asserts `"Last seen hidden"`. It must change, because the behaviour is changing on purpose.

## Implementation Plan
- [ ] Task 1. Add an optional "last local message" timestamp parameter to `presence_label` in `crates/tui/src/member_profile.rs`. In the `Offline` branch, when `last_seen` is `None` and `last_seen_hidden` is set: return `"Last message {relative}"` if the timestamp is present, otherwise keep `"Last seen hidden"`. Rationale: one place decides the wording, so the list and popup can't drift apart.
- [ ] Task 2. Apply the same fallback to plain `Offline` with no last-seen and not hidden (e.g. a Slack-like provider or an incomplete WhatsApp snapshot): prefer "Last message X ago" over a bare "Offline". Leave `Online`, `Away`, a real `Last seen …`, `Unsupported` and `Disabled` unchanged. Rationale: same user value, same data.
- [ ] Task 3. Member list (`crates/tui/src/app.rs:6689-6700`): look up the member's `last_at` first, using `stats.member(platform_id, is_self)` so the merged "You" entry still works. Pass it into `presence_label`. Keep the current "last msg …" fallback for when there's no presence at all. Make the capitalisation consistent ("Last message" in both places, or the shorter "last msg" in the narrow list; keep the list short to save width).
- [ ] Task 4. Popup header (`crates/tui/src/member_profile.rs:794-808`): get `last_at` from `view.stats` the same way and pass it in. Keep the offline `○` dot, so it's clear the person is not online. The "Last message here" line in the Activity section stays as it is.
- [ ] Task 5. Draw paths must stay cache-only: the timestamp comes from the already-computed `ChatStats` cache, with no new queries. If stats are still loading, show "Last seen hidden" until the stats arrive. The next redraw then switches the label, with no extra trigger needed.
- [ ] Task 6. Tests: update `presence_labels_cover_states` so that hidden plus a last message gives `"Last message 2h ago"`, hidden with no message gives `"Last seen hidden"`, and offline with no last-seen plus a message gives `"Last message …"`. Add a TUI test using the mock provider (its presence already sets `last_seen_hidden: true` at `crates/core/src/mock.rs:453`). Assert the member row shows "Last seen hidden" before stats finish, then "Last message …" after the background stats are drained (both phases, per AGENTS.md).
- [ ] Task 7. CI parity: `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --release -p chat-cli`.

## Verification Criteria
- In a WhatsApp chat with a contact who hides last seen, the member row and popup header show "Last message Xm/h/d ago" once stats load.
- Contacts with a visible last seen still show "Last seen …"; online and away people are unchanged.
- Your own row never shows a presence label (self presence isn't requested), and its last-message time comes from the merged "You" entry.
- All CI commands pass.

## Potential Risks and Mitigations
1. **"Last message" read as "last online"**
   Mitigation: always use the words "Last message", never "Last seen", and keep the offline dot.
2. **The last message is outside the synced window, so the time looks too old**
   Mitigation: the Activity section already shows a coverage line saying how much history the stats are based on. No extra wording needed.
3. **The label flickers from "hidden" to "last message" when stats arrive**
   Mitigation: acceptable and expected. It's a single change, covered by the two-phase test.

## Alternative Approaches
1. Combine both: "Last seen hidden · last msg 3h ago". This keeps more information but often gets cut off in the narrow Details column.
2. Search for the person's last message across all chats (via `sender_chats`) instead of only this chat. This gives a fresher time but needs a new background query for each visible member; better as a popup-only addition later.
