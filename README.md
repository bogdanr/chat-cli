# chat-cli

<div align="center">

# A polished terminal inbox for WhatsApp, Slack, and fast local workflows

**chat-cli** brings modern messaging into the terminal: real providers, local-first history, rich conversations, guided account setup, keyboard-first navigation, and mouse-friendly controls in one responsive TUI.

</div>

---

## Why chat-cli?

Most chat apps are heavy, noisy, and split across browser tabs. `chat-cli` is built for people who live in the command line and still want a refined messaging experience:

- **One terminal inbox** for Slack workspaces, WhatsApp chats, and demo data.
- **Local-first state** backed by SQLite so chats, messages, settings, accounts, reactions, receipts, and identity data stay available.
- **Rich conversation support** with replies, reactions, threads, polls, link previews, media-aware rendering, image previews, read handling, and message actions.
- **A real app-like TUI** with responsive layouts, overlays, settings, notifications, help, account setup, keyboard shortcuts, and mouse/touchpad support.
- **Extensible provider architecture** so new chat services can plug into the same domain model and UI.

---

## Current capabilities

### Unified terminal experience

- Responsive compact, medium, and wide layouts for different terminal sizes.
- Separate panes for chats, messages, compose, and details/thread context.
- Keyboard navigation across panes plus mouse click and scroll support.
- Built-in help overlay with current context and shortcut reference.
- Account switcher and account filter for narrowing the inbox.
- Runtime account setup for Slack, WhatsApp, and demo accounts.
- Settings overlay for inbox style, theme, conversation presentation, network activity display, notification behavior, and visibility filters.
- Themes for default dark, light, high contrast, WhatsApp-inspired, and Slack-inspired palettes.
- Status bar hints, connection state, sync progress, and RX/TX network activity indicators.

### Conversations that feel complete

- Local rendering for text, replies, reactions, receipts, edits/deletes, link previews, polls, and unsupported-content fallbacks.
- Thread view in the details pane with threaded compose support.
- Message action menu for replying, viewing threads, reacting, voting in polls, copying text, and opening image previews.
- Compose box with emoji suggestions such as `:joy` and `:heart`.
- Attachment flow that can send local image, GIF, video, audio, file, or sticker paths when the active provider supports them.
- Inline and desktop notification support with configurable previews and mute behavior.

### Slack provider

- Multiple workspace support from CLI flags, inline profiles, TOML workspace files, and persisted account configs.
- Guided Slack setup with auth-mode choices ordered by robustness:
  - User OAuth guidance
  - Read-only OAuth/token import
  - Bot token setup
  - Imported token setup
  - Manual Slack app setup
  - Incoming webhook fallback
- Web API integration for conversations, users, members, history, posting, reactions, file upload/download, and search-capable workflows.
- Socket Mode plumbing for realtime events when an app-level token is available.
- Send identity selection for user, bot, or webhook modes depending on credentials and workspace approval.
- Slack channel, private channel, MPIM, and DM modeling.

### WhatsApp provider

- WhatsApp bridge provider powered by a Go/whatsmeow bridge behind the Rust provider interface.
- QR-code pairing flow for WhatsApp linked devices.
- Configurable history sync scope: `all`, `today`, or `none`.
- Text and local media sends for images, GIFs, videos, audio, files, and stickers.
- Reaction sending and poll-vote support.
- Incoming bridge event handling for messages, reactions, poll votes, chat updates, auth, disconnects, and sync progress.
- Canonical JID handling for phone-number and LID identities.
- Optional debug logging for bridge/media activity.

### Storage and reliability

- SQLite-backed persistence for:
  - accounts and provider configs
  - chats and metadata
  - messages and platform-specific payloads
  - reactions and read receipts
  - contacts/people
  - app settings
- Default database location via platform data directories, plus `--db` override for tests or portable runs.
- Provider config de-duplication for persisted Slack accounts.
- Test cleanup mode for integration-style runs.
- Event bus for provider updates including messages, edits, deletes, reactions, receipts, typing, sync, auth, reconnecting, and network activity.

---

## Quick start

### Run with real providers

By default, `chat-cli` starts the Slack setup provider and the WhatsApp bridge provider:

```bash
cargo run -p chat-cli
```

### Try the demo UI

Explore the interface without connecting accounts:

```bash
cargo run -p chat-cli -- --mock-provider
```

### Enable specific providers

```bash
# Slack only
cargo run -p chat-cli -- --slack

# WhatsApp only
cargo run -p chat-cli -- --whatsapp

# Demo + Slack + WhatsApp together
cargo run -p chat-cli -- --mock-provider --slack --whatsapp
```

---

## Configuration examples

### Slack single workspace

```bash
cargo run -p chat-cli -- \
  --slack \
  --slack-workspace engineering \
  --slack-auth-mode user-oauth \
  --slack-user-token xoxp-...
```

### Multiple Slack workspaces inline

```bash
cargo run -p chat-cli -- \
  --slack-workspace-profile 'label=Team Alpha,auth=user-oauth,user_token=xoxp-alpha' \
  --slack-workspace-profile 'label=Team Beta,auth=bot-token,bot_token=xoxb-beta'
```

### Multiple Slack workspaces from TOML

```toml
[[workspaces]]
workspace = "Engineering"
auth_mode = "read-only-oauth"
user_token = "xoxp-engineering"

[[workspaces]]
workspace = "Ops"
auth_mode = "bot-token"
bot_token = "xoxb-ops"
```

```bash
cargo run -p chat-cli -- --slack-workspaces-file slack-workspaces.toml
```

### WhatsApp bridge options

```bash
cargo run -p chat-cli -- \
  --whatsapp \
  --whatsapp-db chat-cli-whatsapp.db \
  --whatsapp-sync today
```

Use `--whatsapp-sync all`, `today`, or `none` depending on how much history you want to load.

---

## Keyboard and mouse highlights

| Action | Shortcut |
| --- | --- |
| Open help | `F1` or `?` outside compose |
| Quit | `Ctrl+Q` |
| Move between panes | `Left` / `Right` |
| Filter chats | `Ctrl+F` |
| Filter by account | `Ctrl+A` |
| Add account | `Ctrl+N` |
| Open settings | `Ctrl+,` |
| Open selected chat/message action | `Enter` |
| Browse chats/messages | `Up` / `Down` / `PageUp` / `PageDown` |
| Send compose text | `Enter` in compose |
| Insert emoji suggestion | Type `:name` and select suggestion |

Mouse and touchpad interactions are supported for focusing panes, opening chats/messages, selecting popup options, and scrolling lists or help content.

---

## Build and test

```bash
cargo build --workspace
cargo test --workspace
```

The WhatsApp bridge also includes Go-side tests:

```bash
go test ./...
```

Run the Go command from `crates/providers/whatsapp/go`.

---

## Workspace layout

```text
crates/
  chat-cli/              CLI entry point and provider wiring
  core/                  Shared domain model, events, provider trait, mock provider
  tui/                   Terminal UI, interaction model, overlays, rendering
  storage/               SQLite persistence and settings
  providers/whatsapp/    WhatsApp bridge provider and Go bridge
  providers/slack/       Slack provider, auth modes, Web API, Socket Mode plumbing
  mcp/                   Future MCP integration crate
  notify/                Desktop notification integration
plans/                   Implementation plans and handoff notes
```

---

## Project philosophy

`chat-cli` prioritizes a refined terminal UX, local-first data, and provider-agnostic design. Features should be discoverable, pleasant with keyboard and pointer input, efficient at runtime, and validated with focused tests plus a full workspace build before being considered complete.
