# Universal Reachability Search

## Objective

Add a unified discovery capability that lets users find and reach destinations that are not shown in the default chat sidebar, including WhatsApp contacts, Slack users/DMs, and Slack public channels. The sidebar should remain a focused inbox rather than a full address book or workspace directory.

## Initial Assessment

### Project Structure Summary

- The app already uses a provider-based architecture for WhatsApp, Slack, and demo data, with common core types and a shared TUI.
- WhatsApp currently exposes chats from loaded bridge events and in-memory maps, so old contacts may be missing unless they have appeared in loaded history or events.
- Slack already models conversations, membership state, public/private channels, MPIMs, DMs, and users.
- The current TUI chat filter searches only existing local chat rows, so it cannot discover provider-side destinations that are not represented as chats.
- Storage already separates people/handles from chats, which supports persisting discovered contacts without polluting the chat list.

### Relevant Files and Implications

- `crates/core/src/provider.rs:143-237`: The provider trait owns chats, history, sends, message search, contact lookup, and chat member listing. A provider-neutral discovery method belongs here.
- `crates/core/src/types.rs:50-67`: The `Chat` model already supports direct chats, public channels, private channels, membership, unread state, and metadata needed to materialize discovered destinations.
- `crates/tui/src/widgets/chat_list.rs:84-98`: Current chat filtering only filters existing chats; it should be extended or complemented by destination discovery.
- `crates/providers/whatsapp/src/lib.rs:37-44`: WhatsApp keeps chats, messages, and profiles in memory, so contacts absent from loaded events are not naturally visible.
- `crates/providers/whatsapp/src/lib.rs:367-379`: WhatsApp chat listing returns only the in-memory chats map.
- `crates/providers/whatsapp/go/bridge.go:1100-1122`: The WhatsApp Go bridge can resolve contact display names from the whatsmeow contact store, which can support contact discovery.
- `crates/providers/slack/src/lib.rs:219-238`: Slack conversation metadata includes channel/user identity, membership, privacy, topic, purpose, member counts, unread counts, and archive state.
- `crates/providers/slack/src/lib.rs:2063-2090`: Slack conversations can already be converted into core `Chat` records.
- `crates/providers/slack/src/lib.rs:2894-2906`: Slack sidebar inclusion already excludes non-member public channels by default, matching the desired UX.
- `crates/providers/slack/src/lib.rs:2932-2945`: Slack already maps `is_member == false` into `ChatMembership::NotJoined`, which can drive “join required” discovery results.
- `crates/storage/src/schema.rs:75-88`: Storage already has `persons` and `handles`, suitable for discovered contacts independent of visible chats.

### Prioritized Challenges and Risks

1. **Provider-neutral discovery model design**
   - Reasoning: This is the architectural foundation. A WhatsApp-only or Slack-only design would create duplicated UI logic and make future providers harder.
2. **TUI behavior and result semantics**
   - Reasoning: Mixing existing chats, contacts, DMs, and channels can confuse users unless labels and actions are explicit.
3. **Slack permissions and action handling**
   - Reasoning: Public channel discovery, joining, opening DMs, and posting can require different scopes and token types.
4. **WhatsApp contact identity normalization**
   - Reasoning: WhatsApp phone JIDs, LID identities, canonical JIDs, and alternate JIDs must not create duplicate chats.
5. **Performance and scale**
   - Reasoning: Slack workspaces and WhatsApp address books can be large; discovery must be bounded, cached, and debounce-friendly.

## Implementation Plan

- [x] **Task 1. Define a provider-neutral discovery result model.**  
  Add a core type such as `DiscoveryResult` or `ReachableDestination` with fields for provider account, platform, result kind, stable destination ID, display label, subtitle/topic, avatar, membership, and action requirement. This is necessary so the TUI can render WhatsApp contacts, Slack users, Slack DMs, and Slack channels through one flow.

- [x] **Task 2. Add discovery result kinds and action semantics.**  
  Model kinds such as existing chat, contact/person, Slack user, Slack public channel, Slack private channel, Slack MPIM, WhatsApp contact, and manual destination. Include action semantics such as `Open`, `CreateChat`, `OpenDm`, `JoinRequired`, or `Unsupported`. This prevents accidental Slack joins or unintended chat creation.

- [x] **Task 3. Extend the provider trait with destination discovery.**  
  Add an optional async provider method such as `discover_destinations(query, limit)` with a default empty result. This keeps discovery provider-owned while allowing providers without directory support to remain unchanged.

- [x] **Task 4. Preserve the existing sidebar as an inbox.**  
  Keep the default chat list restricted to active, visible, joined, or explicitly materialized chats. Non-member Slack public channels and dormant WhatsApp contacts should not appear unless selected/opened by the user.

- [x] **Task 5. Implement local existing-chat discovery first.**  
  Reuse current chat filtering logic to return existing chats as discovery results. This preserves today’s behavior while creating a migration path from simple filtering to richer search.

- [x] **Task 6. Implement Slack public channel discovery.**  
  Search Slack conversations from `conversations.list`, including non-archived public channels that are not members. Match by channel name, topic, purpose, and potentially member count metadata. Return non-member public channels as discoverable results with `ChatMembership::NotJoined` and a join-required action.

- [x] **Task 7. Keep Slack non-member public channels out of the default sidebar.**  
  Preserve the existing sidebar inclusion behavior that only includes public channels when `is_member` is true, while exposing non-member channels through discovery results.

- [x] **Task 8. Add Slack user and DM destination discovery.**  
  Search cached Slack users first and add API-backed user discovery if needed. Selecting a Slack user should open or create a DM where provider capabilities and Slack API permissions allow it.

- [x] **Task 9. Add Slack channel join support as a separate action.**  
  Add `conversations.join` support only for explicit join actions. Discovery should be possible without automatically joining. If the token cannot join, the result should display an explanatory unsupported or read-only state.

- [x] **Task 10. Implement WhatsApp bridge contact search.**  
  Add a Go bridge function that searches whatsmeow contacts by display name and phone/JID, returns JSON results, and exposes it through the Rust bridge wrapper. Normalize canonical and alternate JIDs to avoid duplicate chats.

- [x] **Task 11. Implement WhatsApp provider discovery.**  
  Combine loaded chats, in-memory profiles, and bridge contact search results into provider-neutral discovery results. Selecting a WhatsApp contact should create or select a direct chat only on demand.

- [x] **Task 12. Add TUI support for mixed discovery results.**  
  Update or complement the current chat filter UI so it can display grouped and labeled results, such as existing chats, contacts, Slack users, Slack channels, and WhatsApp contacts. Labels should clearly indicate membership and required actions.

- [x] **Task 13. Decide shortcut and mode behavior.**  
  Preserve `Ctrl+F` as local chat filtering or migrate it carefully to unified discovery. If keeping both, use a separate shortcut such as `Ctrl+K` for global discovery. The plan should avoid surprising users who rely on the existing filter.

- [x] **Task 14. Materialize destinations only after explicit user action.**  
  Existing chats should be selected directly. WhatsApp contacts should become direct chats only when opened or messaged. Slack public channels should require explicit join/open behavior. Slack users should create/open DMs only when selected.

- [x] **Task 15. Persist discovered people independently from visible chats.**  
  Store discovered people through existing `persons` and `handles` tables. Avoid persisting every discovered Slack channel as a chat unless the user joins, opens, or otherwise explicitly uses it.

- [x] **Task 16. Add discovery capability reporting.**  
  Add provider capability indicators for contact discovery, user discovery, public channel discovery, DM opening, and public channel joining. The TUI should use these to display meaningful result actions and error states.

- [x] **Task 17. Add focused automated tests.**  
  Cover existing-chat discovery, WhatsApp contact discovery, Slack non-member public channel discovery, Slack sidebar exclusion, Slack join-required behavior, mixed result ordering, and destination materialization.

- [x] **Task 18. Add integration-level verification paths.**  
  Verify that searches remain responsive on large result sets, provider failures degrade gracefully, and unsupported token modes display actionable status instead of silently hiding results.

## Verification Criteria

- [x] WhatsApp contacts absent from the chat list can be found by search.
- [x] Selecting a WhatsApp contact creates or opens a direct chat on demand.
- [x] Slack public channels that are not joined are discoverable by search.
- [x] Slack public channels that are not joined do not appear in the default sidebar.
- [x] Slack channel results clearly show joined versus not-joined state.
- [x] Selecting a non-joined Slack public channel does not silently join it.
- [x] Slack users/DM targets are discoverable where permissions allow.
- [x] Existing chat filtering/search behavior remains available.
- [x] Provider-specific permission limitations are visible to the user.
- [x] Large workspaces/contact lists do not make the TUI sluggish.
- [x] Tests cover all provider result types and major user actions.

## Potential Risks and Mitigations

1. **Slack permissions vary by token type**  
   Mitigation: Add discovery-specific capability flags and show unsupported actions clearly instead of failing silently.

2. **Accidental Slack channel joins**  
   Mitigation: Require explicit join/open confirmation for `ChatMembership::NotJoined` public channels.

3. **Search result ambiguity**  
   Mitigation: Label result kinds and actions clearly, group results by type, and show provider/account context.

4. **WhatsApp duplicate chat creation**  
   Mitigation: Normalize canonical and alternate JIDs before matching or creating chat records.

5. **Large directory performance**  
   Mitigation: Debounce searches, cap result counts, cache provider directory responses, and prefer local results before network refresh.

6. **Confusing message search versus destination search**  
   Mitigation: Keep destination discovery visually and semantically separate from message-content search.

## Alternative Approaches

1. **Single unified search box**: Best long-term UX, but requires careful grouping and action labeling to avoid confusion.
2. **Separate local filter and global discovery modes**: Safer incremental path; preserves current `Ctrl+F` and adds a new discovery shortcut such as `Ctrl+K`.
3. **Dedicated directory pane**: Clear and scalable, but adds UI complexity and can be implemented later if search results become too dense.
4. **Show all contacts/channels behind settings**: Simple but not recommended as the default because it turns the sidebar into a large directory.

## Clarity Assessment

- [x] Assumption: The default chat sidebar should remain an inbox-like list rather than a complete provider directory.
- [x] Assumption: Discovery should be provider-neutral and should include both people and spaces/channels.
- [x] Assumption: Slack non-member public channels should require explicit join/open behavior before becoming normal chat rows.
- [x] Assumption: WhatsApp contact discovery should use the linked-device contact store when available and manual phone/JID entry can be a later fallback.

## Recommended Path

- [x] Start with the provider-neutral discovery model and local existing-chat results.
- [x] Add Slack public channel discovery next because Slack already has strong conversation metadata and sidebar exclusion semantics.
- [x] Add WhatsApp bridge contact search after the shared UI and model are in place.
- [x] Add Slack user/DM opening and public-channel joining as explicit follow-up capabilities, since they have more permission-dependent behavior.
