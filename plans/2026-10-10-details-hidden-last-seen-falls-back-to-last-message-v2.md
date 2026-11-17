# Hidden "last seen" falls back to "Last message X ago" (across all chats)

Supersedes v1, which only looked at the current chat.

## Objective
When a contact hides their last-seen time, or no last-seen is known, show **"Last message X ago"**. The time is taken from that person's newest locally synced message in **any chat on the same account**, not just the chat currently open. Use this in the Details member list and in the profile popup header. Only fall back to "Last seen hidden" when no local message exists anywhere.

## Findings
- `crates/tui/src/member_profile.rs:327-348`: `presence_label` returns `"Last seen hidden"` and has no access to a last-message time.
- `crates/tui/src/app.rs:6690-6700`: the member list's "last msg" fallback runs only when `presence_label` returns `None`. It also uses per-chat `ChatStats.last_at`, so it covers this chat only.
- `crates/tui/src/member_profile.rs:794-808`: the popup header has no fallback.
- `crates/storage/src/schema.rs:55-57`: no index covers `messages(account_id, sender_id)`. A lookup across all chats would scan every message of the account, so it needs a new index.
- `crates/storage/src/stats.rs:283-311`: `sender_chats` already groups by chat for one person. That's useful for the popup, but it's a full scan without the index and handles only one person at a time.
- `crates/tui/src/app_member_ui.rs:196-238`: `request_visible_member_presence` already gathers the right bounded set of people (popup member, DM peer, visible rows, excluding self). The new lookup can reuse it.

## Implementation Plan
- [ ] Task 1. **Index.** Add `idx_messages_sender ON messages (account_id, sender_id, timestamp DESC)` to `crates/storage/src/schema.rs`. Rationale: each person's latest message becomes a single index seek instead of a scan of the whole account. Building it on an existing large database is a one-time cost at startup; record it in the existing migration/perf logging.
- [ ] Task 2. **Batch storage query.** Add a `Store` method to `crates/storage/src/stats.rs` that takes an account and a bounded list of sender ids. For each id it returns the newest non-deleted message time and its chat id (and chat name, for the popup). It should do one indexed lookup per id within one lock, and must exclude `is_from_me` rows. Add storage tests: newest message across two chats wins; deleted messages and other accounts are ignored; unknown ids are absent.
- [ ] Task 3. **TUI cache and pipeline** (in `crates/tui/src/app_member_ui.rs`, next to presence). Add a cache keyed by `(account, platform_id)` holding `{at, chat_id, chat_name, checked_at}`, plus an in-flight set and a completion channel. Requests reuse the presence wanted-set, capped by an existing limit. One background task handles each batch. Drain results in a bounded batch per UI tick. Re-check only after a TTL (about 60s), and drop results for an account the user has navigated away from.
- [ ] Task 4. **Live updates without queries.** When an incoming message (provider event or stored upsert) is from a cached person, raise the cached time to the newer value directly. Rationale: the label stays fresh as messages arrive, with no database work in event handling.
- [ ] Task 5. **Label logic.** Extend `presence_label` with an optional "last message anywhere" time. Offline and hidden, or offline with no last-seen: show `"Last message {relative}"`. With nothing local, keep `"Last seen hidden"` or `"Offline"`. Leave Online, Away, a real "Last seen …", Unsupported and Disabled unchanged. The source order is: cross-chat cache → this chat's `ChatStats.last_at` (already loaded, so it's an instant placeholder) → none.
- [ ] Task 6. **Member list** (`crates/tui/src/app.rs:6689-6700`): pass the resolved time into the label. Keep the wording short ("last msg 3h ago") for the narrow column. Rows with no presence at all also use the cross-chat time instead of this chat only.
- [ ] Task 7. **Popup header** (`crates/tui/src/member_profile.rs:794-808`): show `"Last message 3h ago in Climbing"` when the newest message is in another chat, and drop the chat name when it's this chat. Keep the grey offline dot. The Activity section keeps its separate "Last message here" line, so both facts stay visible.
- [ ] Task 8. **Instrumentation:** add a perf label (e.g. `member_last_message.fetch`) with account, number of requested and found ids, elapsed time, and stale/error counts.
- [ ] Task 9. **Tests.** Update `presence_labels_cover_states` (hidden + time → "Last message …"; hidden without one → "Last seen hidden"). Add a two-phase TUI test with the mock provider (its presence is `last_seen_hidden`, see `crates/core/src/mock.rs:453`). Store the member's newest message in a *different* chat. Assert that right after selecting the chat the row shows the placeholder (this chat's time or "Last seen hidden"), and that after draining it shows the newer time from the other chat. Add a test that an incoming message bumps the cached time with no new fetch.
- [ ] Task 10. **CI parity:** `cargo fmt --check`, `cargo check --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `cargo build --release -p chat-cli`.

## Assumptions
- "All chats" means all chats on the **same account**. Sender ids are per account. Matching people across WhatsApp, Slack and ClickUp would rely on guessing from names (linked identities), which would be misleading in a "last active" signal.
- Only the person's own messages count; reactions and read receipts don't. Read receipts are already shown separately as "Last read".

## Verification Criteria
- A WhatsApp contact who hides last seen, and who posted 10 minutes ago in a different group, shows "last msg 10m ago" in this chat's member list. The popup shows "Last message 10m ago in <group>".
- With no local messages from them anywhere, it still shows "Last seen hidden".
- New messages from that person update the label without the chat being reopened.
- Large accounts: the perf log shows the batch lookup in milliseconds, with no full-table scan (confirmed by the index being used).
- All CI commands pass.

## Potential Risks and Mitigations
1. **WhatsApp stores one person under two ids** (phone JID in DMs, private `@lid` in groups), so their last message in the other form is missed.
   Mitigation: include the alternate id in the lookup whenever the profile or bridge already reports it (the user-info call returns the phone/LID). Otherwise accept a slightly older time. It's never wrong, only less fresh.
2. **Index build time on large databases.**
   Mitigation: `CREATE INDEX IF NOT EXISTS` runs once. Log how long it takes; the storage worker is already off the UI thread.
3. **The label is mistaken for real presence.**
   Mitigation: always "Last message", never "Last seen"; keep the offline dot.
4. **Only synced history counts.**
   Mitigation: no change. Older messages that were never synced can't make the time look newer than it is, only older.

## Alternative Approaches
1. **This chat only (v1):** no index or new pipeline needed, but it misses activity elsewhere. This is what the user rejected.
2. **Reuse `sender_chats` for each person:** no new query, but it would scan once per member without the index. Fine for the popup alone, too slow for a list of visible members.
3. **Across all accounts via linked identities:** broader, but based on guessing from names; better offered later as an explicitly labelled popup line.
