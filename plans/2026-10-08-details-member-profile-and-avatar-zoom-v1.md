# Clickable Members in Channel Details: Profile Popup and Enlarged Avatar

## Objective

In the right-side Details pane (chat overview), make each member row clickable:

- **Click the member's name or row**: open a centered **Member Profile popup**. It shows everything the provider can supply: display name, handle, title, status, about/bio, phone, email, timezone/local time, role (admin/owner), bot/business/deactivated flags, and extra `facts`. It also shows a larger avatar.
- **Click the member's avatar** (in the Details pane or inside the popup): open the existing full-screen **image viewer** with that avatar.
- Expand the provider profile data so the popup is useful on WhatsApp too. Today WhatsApp only returns name and phone.

## Current State (from codebase analysis)

**Details pane**
- The member list is drawn by `append_member_detail_lines` (`crates/tui/src/app.rs:6458-6559`).
- Each member takes 2 lines: a 4-column avatar, then name and role badge.
- HD avatar positions are recorded as `MemberAvatarOverlay { line_index, key }` (`crates/tui/src/app.rs:313-317`) but are only used for drawing.

**Clicks**
- A click in the Details pane only sets the status text "details focused" (`crates/tui/src/app.rs:12813-12816`).

**Reusable pieces that already exist**
- `ContactProfile` type (`crates/core/src/types.rs:456-502`) and `Provider::contact_profile(platform_id)` (`crates/core/src/provider.rs:570-585`).
- `request_contact_profile` (`crates/tui/src/app.rs:14929-14957`), which is cached, de-duplicated and safe to call from draw. Its drain is at `crates/tui/src/app.rs:4534-4558`.
- `append_contact_profile_lines` (`crates/tui/src/app.rs:6654-6731`), which already turns a `ContactProfile` into display lines.
- `ImageViewer` and `draw_image_viewer` (`crates/tui/src/app.rs:1203-1206`, `9036-9078`), already used by `open_chat_avatar_at` and `open_message_avatar_at` (`crates/tui/src/app.rs:15251-15283`, `15485-15541`).
- Help-overlay pattern: centered rect, dimmed backdrop, scroll, click-outside-to-close (`crates/tui/src/app.rs:8527-8568`, `11916-11936`, `18130-18144`).
- `render_details_hd_avatar` (12x6 HD avatar) at `crates/tui/src/app.rs:9371-9401`.

**What each provider returns for a profile**
- **Slack:** rich data from `users.info` (`crates/providers/slack/src/lib.rs:914-942`, `4994-5006`). Avatars come from `image_72` only (`crates/providers/slack/src/lib.rs:869-871`).
- **WhatsApp:** name plus phone parsed from the JID (`crates/providers/whatsapp/src/lib.rs:1114-1133`). The Go bridge has no user-info export.
- **ClickUp:** uses the trait default (name only).

**Confirmed bug**
- `downloadProfilePicture` uses the same cache key `"full:"+jid+":"+pictureID` (`crates/providers/whatsapp/go/bridge.go:2436-2438`) for preview-size member avatars (`crates/providers/whatsapp/go/bridge.go:1059`) and full-size chat avatars (`crates/providers/whatsapp/go/bridge.go:2414`).
- Whichever is downloaded first wins, so an enlarged avatar can end up as the low-resolution preview.

## Assumptions

- The popup is a modal overlay, not a replacement for the Details pane contents. It reuses the help-overlay interaction model, so it works in Wide and Compact layouts alike.
- Clicking the 4-column avatar opens the image viewer directly. Clicking anywhere else on the 2-line member row opens the profile popup.
- Only data the provider APIs already expose (or can cheaply expose) is shown. There is no new persistent storage of profiles beyond the existing in-memory `contact_profiles` cache.
- The DM "Contact" section and the message-details sender are also good click targets. They are a phase-2 nicety, not required.

## Implementation Plan

### Phase 1: Hit-testing member rows in the Details pane

- [ ] **Task 1. Record member hit regions during draw.**
  - Extend the member-line builder (`append_member_detail_lines`) to output a `DetailsMemberHit { line_index, account/provider id, platform_id, has_avatar }` for **every** rendered member, not only HD-overlay ones. Store them in a new `state.details_member_hits`, cleared and rebuilt each `draw_details` pass, mirroring how `message_hits` works.
  - Rationale: draw already knows the exact line positions, so storing them avoids duplicating the layout logic at click time. Recording a few dozen entries keeps the work bounded and cheap, as AGENTS.md requires.
  - Also record the inner rect of the details paragraph and the `details_scroll` value that were used for this draw, so clicks map to the same layout that is on screen.

- [ ] **Task 2. Add a `details_member_at(column, row)` resolver.**
  - Compute `line = details_scroll + (row - inner.y)`, then match a hit where `line ∈ {line_index, line_index + 1}`.
  - Classify the click as `Avatar` when the column falls in `[inner.x + 2, inner.x + 2 + 4)` (the same offset `render_member_hd_avatars` uses at `crates/tui/src/app.rs:6295-6335`), and as `Row` otherwise.
  - Return nothing when a thread view or message-details view is active (`draw_details` branches at `crates/tui/src/app.rs:6242-6257`), because member rows aren't shown then.

- [ ] **Task 3. Wire the `FocusPane::Details` branch of `handle_left_click`** (`crates/tui/src/app.rs:12813-12816`).
  - **Avatar hit:** if the member's `sender.avatar` is set, set `state.image_viewer = Some(ImageViewer { path })`. Otherwise show the status message "no avatar available" and open the profile popup instead.
  - **Row hit:** call `open_member_profile(...)` (Task 5).
  - **No hit:** keep the current "details focused" behaviour.

- [ ] **Task 4. Keyboard parity (accessibility; not strictly requested).**
  - When Details is focused, let Up/Down (or j/k) move a highlighted member cursor, Enter open the profile and `v` open the avatar.
  - Update the "Discoverable controls" text (`crates/tui/src/app.rs:6431-6450`) to mention clicking members and avatars.
  - Rationale: the app is keyboard-first, and a mouse-only feature is less discoverable.

### Phase 2: Member Profile popup

- [ ] **Task 5. Add popup state and an `open_member_profile` action.**
  - New `state.profile_overlay: Option<ProfileOverlay { provider_id, chat_id, platform_id, sender snapshot, role, is_self, scroll }>`.
  - Opening it fills in the `Sender` and role already available from `chat_members`, so the popup appears immediately with name, role and avatar. It then calls the existing `request_contact_profile` to load the full profile in the background.
  - Rationale: this follows the immediate-placeholder-then-async-result rule in AGENTS.md.

- [ ] **Task 6. Draw `draw_profile_overlay`.**
  - Register it in the overlay draw order block (`crates/tui/src/app.rs:5003-5022`).
  - **Layout:** a centered rect via `centered_fixed_rect`, about 60x22, clamped to the terminal; dimmed backdrop, border and title "Member profile".
  - **Left column:** a 12x6 avatar using the same path as `render_details_hd_avatar` (HD when supported, cached half-block thumbnail or initials otherwise). This only reads from the cache; missing decodes are queued in the background.
  - **Right side / below:** text lines produced by refactoring `append_contact_profile_lines` into a shared helper, so the DM Contact section and the popup format identically. Add role, "you" marker and platform/account.
  - While `loading_contact_profiles` contains the key, show "Loading profile…". When there's no detail (`!has_detail()`), show "No additional details available from <provider>".
  - Copy/action hints in the footer: Esc close, `v`/click avatar to enlarge, `y` copy phone/email (optional), `m` message user (optional, phase 3).

- [ ] **Task 7. Popup input handling.**
  - In `handle_mouse` (`crates/tui/src/app.rs:11885-12150`), add a branch with the same priority as the other popups:
    - Clicking the popup's avatar rect opens the image viewer, stacked above the popup.
    - Clicking outside closes the popup.
    - The scroll wheel scrolls its content.
  - In the key handler: Esc/q close, arrows/PgUp/PgDn scroll, `v` open avatar.
  - Make sure closing the image viewer returns to the popup rather than closing both. The image viewer is checked before popups in `handle_mouse` (`crates/tui/src/app.rs:11909-11914`), so this ordering works naturally.

- [ ] **Task 8. Stale/navigation handling.**
  - Close the popup when the selected chat or account changes.
  - The contact-profile drain stores results by `(ProviderId, PlatformId)`. The popup reads that cache by key at draw time, so a result for a different member never shows in the wrong popup. No generation token is needed, but document this choice.
  - Show provider errors in the popup ("Couldn't load profile: …") instead of silently showing an empty one. This may need the drain to record errors per key; check what the current drain does with an `Err`.

### Phase 3: Richer provider data

- [ ] **Task 9. WhatsApp: add a user-info bridge export.**
  - Add `C_UserInfo(jid)` to `crates/providers/whatsapp/go/bridge.go`, wrapping whatsmeow `GetUserInfo`. It returns JSON with status/about text and its set-time, verified business name, picture ID, and the LID/phone mapping.
  - Optionally add `GetBusinessProfile` for business accounts (address, email, website, category, description) and put those in `ContactProfile.facts`.
  - Declare it in `crates/providers/whatsapp/src/bridge.rs:22-88` and use it in the WhatsApp `contact_profile` (`crates/providers/whatsapp/src/lib.rs:1114-1133`). Keep the phone value parsed from the JID.
  - Run the call on a blocking task with a timeout, and cache it per JID with a TTL so repeated popup opens don't hit the network.
  - Handle `@lid` privacy JIDs: try the alternate phone JID the same way `memberAvatarPath` does (`crates/providers/whatsapp/go/bridge.go:1048-1068`). If the phone number really is hidden, show "Phone hidden (privacy)" rather than nothing.

- [ ] **Task 10. WhatsApp: separate preview and full-size avatar caches and offer a full-resolution avatar.**
  - Fix the cache key in `downloadProfilePicture` (`crates/providers/whatsapp/go/bridge.go:2436-2438`) by passing a size tag (`preview` / `full`), so the two downloads no longer overwrite each other.
  - Add an on-demand "full-size avatar for JID" path (part of `C_UserInfo`, or a separate export). Expose it as `ContactProfile.avatar_full: Option<PathBuf>` (a new optional field in `crates/core/src/types.rs`).
  - **The enlarge action** uses `avatar_full` when present and otherwise falls back to `sender.avatar`. When the full-size file is still loading, open the viewer with the preview immediately and swap it in once the file arrives. If that's too complex, show a "loading HD…" status instead.

- [ ] **Task 11. Slack: higher-resolution avatars and complete profile fields.**
  - Have Slack's `contact_profile` fill in `avatar_full` from `profile.image_512` / `image_original` (the user struct is at `crates/providers/slack/src/lib.rs:740-762`). Download it through the existing Slack file-download/auth path into the profile cache directory.
  - Add custom profile fields (`profile.fields`) and pronouns, if returned, to `facts`.
  - Keep the 72px avatar for list thumbnails.

- [ ] **Task 12. ClickUp: override `contact_profile`.**
  - Return the email, username, initials/color and role available from ClickUp's member data (`crates/providers/clickup/src/lib.rs:993-1016`). Otherwise fall back to the default.

- [ ] **Task 13. Core type additions.**
  - Add `avatar_full: Option<PathBuf>` (and possibly `pronouns`, `status_since`) to `ContactProfile`, using `#[serde(default)]`-style compatibility wherever it's serialized.
  - Update `has_detail()` and `MockProvider` (`crates/core/src/mock.rs:391-399`) so tests can return a full profile.

### Phase 4: Instrumentation, tests and validation

- [ ] **Task 14. Instrumentation.**
  - Add perf labels: `details.member_hit` (click resolution), `profile_overlay.draw`, `contact_profile.fetch` (with provider, platform_id, ok/error, duration) and `whatsapp.user_info` (bridge latency) through `log_slow_perf_duration`.

- [ ] **Task 15. Tests** (`crates/tui/src/app.rs` test module; mouse helper near `crates/tui/src/app.rs:31531`):
  - Clicking a member row opens the popup **immediately**, showing name, role and a loading state. After the completions drain, phone, email and about are shown. This is the two-phase assertion AGENTS.md asks for.
  - Clicking a member avatar sets `image_viewer` to that member's avatar path. Clicking a member without an avatar opens the popup instead.
  - Hit-testing respects `details_scroll`: after scrolling, a click lands on the correct member, including the second line of a 2-line row.
  - Clicks in thread or message-details mode don't open a member popup.
  - Changing the selected chat closes the popup. A late profile result for another member doesn't change the open popup's content.
  - The image viewer opened from the popup closes back to the popup.
  - Go: a unit test that preview and full cache keys differ. If feasible, `C_UserInfo` JSON shaping in test mode (`dbPath` with a `test:` prefix).

- [ ] **Task 16. Validation before commit.**
  - Inspect `.github/workflows/ci.yml` and run the same steps locally: `cargo check`, `cargo test`, `cargo clippy` (with the CI's flags) and the release build. Also run `go test` for the bridge if CI does.

## Verification Criteria

- In Wide layout, clicking any member name in a Slack channel or WhatsApp group opens the Member Profile popup within one frame. Details fill in after the background fetch, and the UI stays responsive.
- Slack popups show handle, title, status, phone, email and local time when the workspace exposes them.
- WhatsApp popups show phone (or a privacy note), about/status text and the business name when applicable.
- Clicking a member's avatar in the Details pane or in the popup opens the image viewer. For Slack and WhatsApp the image is the full-resolution version, not the 32px or 72px thumbnail.
- After scrolling the Details pane, clicks still resolve to the member shown under the cursor.
- Esc or clicking outside closes the popup. Closing the image viewer returns to the popup.
- No database, network or image decode work runs in `handle_event` or draw. Perf logs show the new labels with bounded durations.
- All CI-equivalent checks pass locally.

## Potential Risks and Mitigations

1. **Hit-testing drifts from the rendered layout** (wrapping, header length changes, scroll clamping).
   Mitigation: record hits during the same draw pass that produces the lines, and store the scroll offset and inner rect actually used. Never recompute the layout at click time.
2. **WhatsApp privacy (LID) hides phone numbers and avatars.**
   Mitigation: try the alternate JID; show explicit "hidden by privacy" labels instead of blanks.
3. **WhatsApp rate limits and anti-abuse if profiles are fetched too eagerly.**
   Mitigation: fetch only when a member is clicked, never for the whole member list; cache with a TTL; de-duplicate in-flight requests (the existing `loading_contact_profiles` set already does this).
4. **Low-quality enlarged avatars from the shared cache key.**
   Mitigation: Task 10's size-tagged keys. Optionally clear or migrate old `full:` entries; they're only a cache, so they will be re-downloaded.
5. **Overlay stacking bugs** (image viewer over popup, popup over other popups).
   Mitigation: allow only one popup family at a time; give the image viewer the highest priority, as it already has; add tests for close ordering.
6. **`app.rs` is very large (about 32k lines), so merge conflicts and review are hard.**
   Mitigation: put the popup state, drawing and hit-testing in a new module (for example `crates/tui/src/widgets/profile_overlay.rs`), leaving only thin wiring in `app.rs`.
7. **Sensitive data on screen** (phone and email in shared-screen situations).
   Mitigation: an optional setting to mask phone and email until revealed (low priority).

## Alternative Approaches

1. **Inline expansion inside the Details pane, with no popup.** Clicking a member replaces the overview with a "Member details" view (back with Esc), similar to the existing message-details mode.
   - Pros: no new overlay; reuses the scroll and avatar overlay code.
   - Cons: unavailable in Medium layout, where Details is hidden; it loses the member-list context; and the pane is narrow (25% width), which cramps a large avatar.
2. **Enlarged avatar inside the popup only, with no full-screen viewer.** Render a 24x12 HD avatar inside the popup.
   - Pros: one surface.
   - Cons: still limited in size; it duplicates viewer logic. Reusing `ImageViewer` costs almost nothing.
3. **Prefetch profiles for all visible members.**
   - Pros: instant popups.
   - Cons: many network calls per channel open, and a risk of WhatsApp or Slack rate limits. On-click lazy loading with caching is preferred.
4. **Ship Phase 1 and 2 only** (UI plus existing provider data).
   - Pros: delivers clickable members and avatar zoom quickly; Slack is already rich.
   - Cons: WhatsApp popups stay sparse (name and phone) and enlarged WhatsApp member avatars stay low-resolution until Phases 3 and 10 land.
