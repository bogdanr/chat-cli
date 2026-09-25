<div align="center">

# chat-cli

**Your chats, in the terminal. Fast, quiet, and keyboard-first.**

WhatsApp, Slack and ClickUp in one tidy inbox — no browser tabs, no clutter, no waiting.

<br>

![License](https://img.shields.io/badge/license-GPL--3.0-blue)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)
[![Crates.io](https://img.shields.io/crates/v/chat-cli)](https://crates.io/crates/chat-cli)
![Platform](https://img.shields.io/badge/platform-Linux%20%C2%B7%20macOS%20%C2%B7%20Windows-lightgrey)

<br>
<br>

![chat-cli showing a WhatsApp group conversation with inline photos and reactions](assets/inbox-conversation.png)

<sub>One inbox, three panes: chats on the left, the conversation in the middle, details on the right.</sub>

</div>

---

## What it is

`chat-cli` is a small, responsive terminal app for reading and sending messages.
It keeps your conversations local, opens instantly, and gets out of your way.

If you live in the terminal, it should feel like it belongs there.

- **Fast.** Opens quickly and stays responsive while history and media load in the background.
- **Intuitive.** Sensible defaults, discoverable shortcuts, and a built-in help overlay.
- **Tidy.** One inbox for every account, with panes for chats, the conversation, and details.
- **Yours.** History lives locally in SQLite — no cloud round-trip to read your own messages.

> It does a lot more than this page lets on. We'd rather you find that out by using it.

---

## See it in action

**Everyday conversations, with the whole thread in view.**

The screenshot above is a WhatsApp group ("Family Weekend") open in the center pane.
The **Chats** pane on the left is sorted activity-first, with avatars, unread badges,
and the accounts they belong to. The **Messages** pane shows the live conversation —
inline photos rendered right in the terminal, emoji reactions under a message, and a
poll at the top of the thread. The **Details** pane on the right summarizes the chat:
type, membership, last activity, description, member list with roles, and even the
disappearing-message timer. The compose box and a context-aware shortcut bar sit along
the bottom.

**Work chat, threads, and rich app messages too.**

![Slack channel in chat-cli showing a thread reply and a Deploy Bot deployment card with action buttons](assets/channel-thread.png)

This is a Slack-style public channel (`#project-chat-cli`). You can see a **threaded
reply** at the top and a **rich app message** from Deploy Bot below — a formatted
deployment card with fields (environment, duration, commit), action buttons like
`View run` and `Rollback`, and a linked GitHub Actions run. The Details pane reflects
the channel context: public channel, workspace, topic, and members. Same three-pane
layout, same keyboard flow — whether it's family photos or a production deploy.

---

## Install

Grab a prebuilt binary from the [releases page](https://github.com/bogdanr/chat-cli/releases),
make it executable, and run it. No toolchain, no build step.

```bash
# Move the downloaded binary onto your PATH
chmod +x chat-cli
sudo mv chat-cli /usr/local/bin/

# Start it
chat-cli
```

Already have the Rust toolchain? [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall)
fetches the same prebuilt binary for you (still no compiling):

```bash
cargo binstall chat-cli
```

---

## First run

Launching with no arguments starts the Slack and WhatsApp setup flows so you can
connect an account from inside the app:

```bash
chat-cli
```

ClickUp is opt-in: because it has no realtime feed and must poll, it only starts
once you add it from the account screen or pass a token.

Want to look around before connecting anything? Use the demo data:

```bash
chat-cli --mock-provider
```

Press `F1` or `?` at any time for the help overlay.

---

## Connecting accounts

You can add and configure accounts from within the app (`Ctrl+A`), or pass flags
to enable a single provider:

```bash
chat-cli --slack       # Slack only
chat-cli --whatsapp    # WhatsApp only
chat-cli --clickup     # ClickUp Chat only
```

- **WhatsApp** pairs by scanning a QR code, just like WhatsApp Web.
- **Slack** offers a guided setup with several auth options, from user OAuth to a
  simple incoming webhook.
- **ClickUp** asks for a personal API token (the `pk_...` value from ClickUp's
  *Settings → Apps*). ClickUp has no realtime feed for Chat, so new messages are
  picked up by periodic checks rather than pushed instantly. Channels, direct
  messages and group DMs all show up, named after the people in them.

Most people never need a flag — the in-app account screen handles setup.

The quickest way to authorize Slack is the official app — click below to install
it into your workspace, then finish signing in from the account screen:

<a href="https://slack.com/oauth/v2/authorize?client_id=4614087544.11325148183808&scope=&user_scope=team:read,channels:history,channels:read,chat:write,files:read,files:write,groups:history,groups:read,im:history,im:read,mpim:history,mpim:read,reactions:read,reactions:write,search:read,users.profile:read,users:read"><img alt="Add to Slack" height="40" width="139" src="https://platform.slack-edge.com/img/add_to_slack.png" srcSet="https://platform.slack-edge.com/img/add_to_slack.png 1x, https://platform.slack-edge.com/img/add_to_slack@2x.png 2x" /></a>

---

## Getting around

| Action | Shortcut |
| --- | --- |
| Open help | `F1` or `?` |
| Move between panes | `Left` / `Right` |
| Browse chats and messages | `Up` / `Down` / `PageUp` / `PageDown` |
| Open chat / message action | `Enter` |
| Filter chats | `Ctrl+F` |
| Accounts (add / filter) | `Ctrl+A` |
| Settings | `Ctrl+S` |
| Send a message | `Enter` in the compose box |
| Insert emoji | Type `:name` and pick a suggestion |
| Mention someone | Type `@name` and pick a suggestion |
| Quit | `Ctrl+Q` |

Mouse and touchpad work too — click to focus a pane, open a chat, or scroll a list.

---

## Make it yours

Open settings with `Ctrl+S` to tune the inbox style, conversation layout,
notifications, and visibility filters. Pick a theme that suits your terminal:

- Default dark
- Light
- High contrast
- WhatsApp-inspired
- Slack-inspired

---

<details>
<summary><strong>Advanced configuration</strong></summary>

<br>

Most options have an in-app equivalent, but flags and environment variables are
available for scripting and multi-workspace setups.

**Multiple Slack workspaces inline:**

```bash
chat-cli \
  --slack-workspace-profile 'label=Team Alpha,auth=user-oauth,user_token=xoxp-alpha' \
  --slack-workspace-profile 'label=Team Beta,auth=bot-token,bot_token=xoxb-beta'
```

**Multiple Slack workspaces from a file:**

```toml
# slack-workspaces.toml
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
chat-cli --slack-workspaces-file slack-workspaces.toml
```

**WhatsApp history scope:**

```bash
chat-cli --whatsapp --whatsapp-sync today   # all | today | none
```

**ClickUp with an explicit token and workspace:**

```bash
chat-cli \
  --clickup-token pk_12345_ABCDEF \
  --clickup-workspace-id 9013000000 \
  --clickup-workspace 'Acme'
```

The workspace id is only required when the token can reach more than one
workspace; with a single workspace it is detected automatically.

**Other useful flags:**

| Flag | Purpose |
| --- | --- |
| `--db <path>` | Use a custom SQLite database location |
| `--log-file <path>` | Write a debug log (logging is off by default) |
| `--mock-provider` | Run with synthetic demo data |

</details>

---

<details>
<summary><strong>Building from source (contributors)</strong></summary>

<br>

Requires a recent Rust toolchain. The WhatsApp bridge also has a Go component.

```bash
# Build and test the whole workspace
cargo build --workspace
cargo test --workspace

# Run from source
cargo run -p chat-cli

# Go-side tests for the WhatsApp bridge
cd crates/providers/whatsapp/go
go test ./...
```

**Workspace layout:**

```text
crates/
  chat-cli/              CLI entry point and provider wiring
  core/                  Domain model, events, provider trait, mock provider
  tui/                   Terminal UI, interaction model, rendering
  storage/               SQLite persistence and settings
  providers/slack/       Slack provider, auth modes, Web API, Socket Mode
  providers/clickup/     ClickUp Chat provider (Public API v3, polling)
  providers/whatsapp/    WhatsApp bridge provider and Go bridge
  notify/                Desktop notifications
  mcp/                   MCP integration crate
```

See `AGENTS.md` for the performance, ordering, and safety rules the codebase
follows.

</details>

---

<div align="center">

Built in Rust · GPL-3.0 licensed

</div>
