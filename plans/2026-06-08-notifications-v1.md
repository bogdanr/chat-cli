# Notification V1 Simplification and Delayed Delivery Plan

## Objective

Implement a simpler and safer notification system for chat-cli:

- One user-facing notification mode: `off`, `desktop`, or `in-app`.
- Eligible notifications are delayed by 5 seconds before delivery.
- Pending notifications are cancelled only when the user gives high-confidence attention to that chat during the delay.
- Self-authored messages are notification-eligible to support self-chat testing.
- Historical sync messages and muted chats remain suppressed.
- Temporary notification pause is controlled outside the settings menu via CLI now and MCP later.
- Desktop notification failures are surfaced instead of silently ignored.

## Product Decisions

### Notification mode

Use a single durable preference:

```text
notifications = off | desktop | in-app
```

This replaces the current split booleans for desktop notifications, in-app notifications, active-chat notifications, muted-chat notifications, and preview toggles in the settings overlay.

### Active chat behavior

A selected chat is not automatically treated as attended. chat-cli often runs in terminal tabs, panes, hidden workspaces, or other desktops.

V1 behavior:

```text
selected chat alone does not suppress notifications
```

Instead:

```text
queue notification for 5 seconds
cancel only if the user attends that specific chat during the delay
```

### Self messages

Do not suppress solely because `message.is_from_me` is true. Self chats need to produce notifications for testing.

### Pause mode

Add temporary notification pause state independent of notification mode:

```text
notifications_paused_until = optional timestamp
```

V1 scope is global. Future scopes can include account and chat.

Pause is controlled externally through CLI now and MCP later. It is not shown in the settings menu.

## Notification Decision Flow

### On incoming message

1. Suppress immediately if the message is historical.
2. Resolve the chat; suppress if unresolved.
3. Suppress immediately if the chat is muted.
4. Suppress immediately if notifications are paused.
5. Suppress immediately if notification mode is `off`.
6. Otherwise enqueue or coalesce a pending notification for the chat with `deliver_at = now + 5 seconds`.

### During the 5-second delay

Cancel pending notifications for a chat if the user provides high-confidence attention to that chat:

- Explicitly navigates/selects that chat after the notification was queued.
- Types in compose while that chat is selected.
- Sends a message in that chat.
- Scrolls or navigates the message pane while that chat is selected.
- Clicks in the selected chat message pane.

Do not cancel for:

- Generic settings/help/account overlay input.
- Terminal resize.
- Tick/draw activity.
- Chat being selected before the message arrived.

### At delivery deadline

When the pending notification expires:

1. Re-check notification pause.
2. Re-check current notification mode.
3. Re-check muted chat state.
4. Deliver according to current mode.
5. Surface desktop delivery failure in app status/perf diagnostics.

## Implementation Tasks

- [x] 1. Add `NotificationMode` to storage and migrate old settings.
- [x] 2. Replace notification setting rows with one `Notifications` row in the settings overlay.
- [x] 3. Add temporary notification pause storage with pause/resume/status operations.
- [x] 4. Add CLI pause/resume/status commands or arguments that update pause state without entering the TUI.
- [x] 5. Add pending notification state to the TUI app.
- [x] 6. Change incoming message handling to enqueue notifications instead of delivering immediately.
- [x] 7. Drain pending notifications from the tick path in bounded batches.
- [x] 8. Add high-confidence chat-attention cancellation hooks.
- [x] 9. Remove self-message suppression from notification eligibility.
- [x] 10. Keep historical and muted-chat suppression.
- [x] 11. Handle desktop notification errors visibly.
- [x] 12. Add/update tests for notification mode migration, delayed delivery, cancellation, pause, and self-message eligibility.
- [x] 13. Run targeted tests and fix failures.

## Verification Criteria

- The settings menu exposes exactly one notification option.
- `off` mode suppresses notification delivery.
- `desktop` mode sends desktop notifications after the 5-second delay.
- `in-app` mode shows in-app notification cards after the 5-second delay.
- Eligible notifications are not delivered immediately.
- Pending notifications are cancelled by high-confidence attention to the same chat.
- Generic input and resize do not cancel pending notifications.
- Self-authored messages are eligible for notification delivery.
- Historical messages are never queued.
- Muted chats are never queued.
- Pause suppresses otherwise eligible notifications.
- Expired pause no longer suppresses notifications.
- Desktop notification failures are visible or diagnostically logged.

## Risks and Mitigations

- Delayed notifications may feel late: keep V1 delay fixed at 5 seconds.
- Cancellation may suppress too much: only cancel on chat-specific attention.
- Pending bursts may grow: coalesce to one pending notification per account/chat.
- CLI pause may not affect running TUI instantly: persist pause state and reload periodically on tick.
- Existing settings compatibility: implement serde/default migration from old boolean fields to the new mode.
