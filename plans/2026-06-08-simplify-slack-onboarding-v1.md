# Simplify Slack Onboarding While Keeping Advanced Modes

## Objective

Make Slack setup feel like a normal chat-account connection flow for regular users, while preserving the existing advanced modes for testing, debugging, restricted workspaces, and manual token workflows.

Current pain points:

- Users see Slack developer concepts too early: manifests, user OAuth, bot tokens, app tokens, webhooks, imported tokens, and manual apps.
- Real Slack behavior is per workspace install, but the UI exposes auth implementation details instead of the mental model: “connect another Slack workspace.”
- OAuth code exchange is not implemented yet, forcing users into copy/paste token flows.
- `team:read` is required for real workspace name/icon metadata, but adding scopes can trigger Slack workspace approval.
- Realtime support must remain first-class because this is a chat app.

## Current Code References

- Slack setup modes are currently all visible in `crates/tui/src/app.rs:894-976`.
- Current capability mapping includes realtime for the main Slack modes in `crates/tui/src/app.rs:978-1015`.
- OAuth code exchange currently errors out in `crates/providers/slack/src/lib.rs:1980-1985`.
- Provider setup option ordering currently exposes all setup modes in `crates/providers/slack/src/lib.rs:1990-2023`.
- OAuth URL generation falls back to manifest generation when client ID/redirect URI are absent in `crates/providers/slack/src/lib.rs:2040-2054`.
- Required Slack scopes, including `team:read`, are defined in `crates/providers/slack/src/lib.rs:2781-2799`.
- Slack manifest generation keeps Socket Mode and event subscriptions for realtime in `crates/providers/slack/src/lib.rs:4605-4651`.

## Product Direction

### Default user-facing model

Replace the current auth-mode-first setup with a workspace-first flow:

```text
Add account
  Slack
    Connect Slack workspace
```

After one workspace is connected, offer:

```text
Add another Slack workspace
```

Users should not need to understand Slack app creation, manifests, user tokens, bot tokens, app tokens, or webhooks for the happy path.

### Advanced/test model

Keep all existing modes, but move them behind an explicit advanced entry:

```text
Slack
  Connect Slack workspace
  Advanced setup
    User OAuth
    User OAuth read-only
    Workspace-approved bot/app tokens
    Existing approved token import
    Manual Slack app setup
    Incoming webhook
```

This preserves testability and fallback paths while preventing normal users from starting in the weeds.

## Proposed UX Flow

### Normal flow: connect workspace

1. User chooses **Connect Slack workspace**.
2. chat-cli opens the Slack OAuth URL in the browser.
3. User chooses a workspace in Slack.
4. Slack either:
   - approves immediately,
   - asks for workspace admin approval,
   - or returns an OAuth error.
5. chat-cli receives the callback/code.
6. chat-cli exchanges the code via `oauth.v2.access`.
7. chat-cli stores the returned workspace credentials.
8. UI shows:

```text
Connected: Slack · Workspace Name
```

9. UI offers:

```text
Add another Slack workspace
Done
Advanced setup
```

### Approval-required flow

If Slack blocks installation or requires approval, show a concise explanation:

```text
This workspace requires app approval.
Ask a Workspace Owner/Admin to approve chat-cli.

Needed for chat:
- read conversations you can access
- receive realtime message events
- send messages, if enabled
- read workspace name/icon via team:read
```

Include a retry action:

```text
Retry after approval
```

### Advanced setup flow

Advanced setup should preserve the current modes and capabilities, but label them as testing/fallback paths:

- **User OAuth**: full user-token flow; useful for testing send-as-user behavior.
- **User OAuth read-only**: restricted workspaces; read without write permissions.
- **Workspace-approved bot/app tokens**: admin-provisioned bot/app token deployment.
- **Existing approved token import**: paste token and infer capabilities.
- **Manual Slack app setup**: generate/import manifest, configure app directly.
- **Incoming webhook**: send-only fallback, no inbox/realtime.

## Implementation Plan

### Phase 1: UX simplification without changing auth internals

Goal: make the current setup less confusing immediately while preserving behavior.

Tasks:

1. Introduce a top-level Slack setup choice:
   - `Connect Slack workspace` as default/recommended.
   - `Advanced setup` as secondary.
2. Map `Connect Slack workspace` to the best current OAuth mode:
   - initially `UserOAuth` or `ReadOnlyOAuth`, depending on desired default.
   - full chat should prefer `UserOAuth` because it includes send/reaction/file scopes.
3. Move the six existing modes behind `Advanced setup`.
4. Update copy in `SlackSetupMode::label`, `description`, and `credential_hint` so normal users see workspace language, while advanced users still see precise token/mode language.
5. Add clearer approval messaging for `missing_scope`, app approval, and reinstall-required cases.
6. Keep existing realtime manifest behavior unchanged.

Validation:

- Add/update TUI tests for setup option ordering and labels.
- Ensure advanced modes remain selectable.
- Run `cargo fmt --check` and `cargo check`.

### Phase 2: Implement actual OAuth callback and token exchange

Goal: remove manual token copy/paste from the normal path.

Tasks:

1. Replace the current OAuth-code rejection in `crates/providers/slack/src/lib.rs:1980-1985` with real handling.
2. Add support for `oauth.v2.access` in the Slack API client abstraction.
3. Implement a local callback listener or a configured callback strategy:
   - local loopback callback is best for desktop/TUI use;
   - hosted callback can be added later if needed.
4. Open the OAuth URL in the browser when the user starts setup.
5. Receive and validate the OAuth `state` parameter.
6. Exchange the temporary `code` for Slack tokens.
7. Store returned bot/user tokens per workspace account.
8. Capture granted scopes from Slack’s response where available.
9. Show a clear success screen with workspace name and capabilities.

Validation:

- Unit-test OAuth URL generation, including `scope`, `user_scope`, `redirect_uri`, and state.
- Unit-test `oauth.v2.access` response parsing.
- Unit-test failed OAuth responses and approval/cancel states.
- Integration-test callback handling with a local fake callback request.
- Run `cargo fmt --check`, focused Slack tests, and `cargo check`.

### Phase 3: Official distributed Slack app support

Goal: users should not create their own Slack app for the normal path.

Tasks:

1. Define official chat-cli Slack app configuration:
   - app name/icon/description;
   - redirect URL;
   - scopes;
   - Socket Mode/realtime configuration;
   - event subscriptions.
2. Make default `Connect Slack workspace` use official app `client_id` and redirect URL when configured/bundled.
3. Keep manifest generation only in `Advanced setup > Manual Slack app setup`.
4. Add a settings/help page explaining:
   - each workspace install is separate;
   - admins may need to approve scopes;
   - `team:read` is only for workspace name/icon;
   - realtime uses Socket Mode/event subscriptions.
5. Add `Add another Slack workspace` action after successful connection.

Validation:

- Verify a new workspace install.
- Verify installing the same app to a second workspace creates a distinct account.
- Verify each account gets its own workspace name/icon.
- Verify realtime messages still arrive.
- Verify advanced modes still work independently.

### Phase 4: Optional scoped/fallback presets

Goal: reduce approval friction while still supporting full chat.

Tasks:

1. Offer two normal presets if needed:
   - **Full chat**: read, send, react, files, realtime, workspace icon.
   - **Read-only chat**: read, files, search, realtime, workspace icon.
2. Explain the tradeoff in one line:

```text
Full chat lets chat-cli send messages. Read-only can only receive/read.
```

3. Keep advanced mode for custom scope testing.
4. Consider optional scopes only if Slack’s approval behavior proves beneficial and the app handles missing scopes cleanly.

Validation:

- Verify missing write scopes degrade send UI correctly.
- Verify missing `team:read` degrades workspace icon/name gracefully.
- Verify missing realtime/app token is clearly reported.

## Error Handling Requirements

### `missing_scope`

When Slack returns `missing_scope`, show:

```text
Slack denied a required permission: <scope>.
To fix this, reinstall or request approval for chat-cli with that scope.
```

For `team:read`, explicitly say:

```text
team:read is used for workspace name and icon.
```

### App approval required

If Slack returns or redirects with an approval/cancel/admin-required signal, show:

```text
Your workspace requires an owner/admin to approve chat-cli.
After approval, retry this connection.
```

### Realtime unavailable

If app token or Socket Mode is absent/unusable, show:

```text
Connected, but realtime is not active.
Messages may only update during manual refresh/sync.
```

Do not silently degrade realtime in the normal full-chat path.

## Data Model Requirements

Each Slack workspace connection should be stored as a separate account:

```text
Slack · Workspace A
Slack · Workspace B
Slack · Workspace C
```

Each account should retain:

- Slack team/workspace ID;
- workspace display name;
- workspace icon URL/cache path;
- granted token types;
- granted scopes if available;
- realtime availability;
- connection/auth mode used;
- last validation error, if any.

## Testing Requirements

1. Existing advanced modes must remain accessible.
2. Existing token import/manual modes must continue to validate tokens.
3. Realtime manifest generation must remain covered by tests.
4. OAuth setup must have tests for:
   - URL generation;
   - callback state validation;
   - token exchange success;
   - token exchange failure;
   - missing scope;
   - app approval/cancel flow.
5. Multi-workspace behavior must verify separate accounts and no token overwrite.
6. Slack workspace icon retrieval must verify `team.info` is attempted with a token that has `team:read`.

## Non-Goals

- Do not remove advanced modes.
- Do not remove realtime support.
- Do not rely on webhooks for inbox/realtime chat.
- Do not hide Slack approval errors behind generic failures.
- Do not make users create a Slack app in the default path once official distributed app support exists.

## Recommended Milestone Order

1. **M1: UI reframe**
   - Add `Connect Slack workspace` default path.
   - Move current modes under `Advanced setup`.
   - Improve approval/scope copy.

2. **M2: OAuth implementation**
   - Implement callback/code exchange.
   - Store per-workspace tokens.
   - Remove normal-path copy/paste token requirement.

3. **M3: Official app distribution**
   - Configure/distribute the chat-cli Slack app.
   - Make default setup use the official app.
   - Keep manifest generation in advanced mode.

4. **M4: Multi-workspace polish**
   - Add explicit `Add another Slack workspace` action.
   - Improve connected workspace list and status.
   - Surface scope/realtime/icon diagnostics in account details.

## Success Criteria

A non-technical user can connect Slack by doing only this:

```text
Settings → Accounts → Add Slack → Connect workspace → approve in browser → done
```

A technical user/tester can still access every current mode:

```text
Advanced setup → User OAuth / Read-only OAuth / Bot token / Imported token / Manual app / Webhook
```

Realtime continues to work for full Slack chat setups, and workspace icons work when `team:read` is granted.
