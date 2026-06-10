<div align="center">

# chat-cli

**Your chats, in the terminal. Fast, quiet, and keyboard-first.**

WhatsApp and Slack in one tidy inbox — no browser tabs, no clutter, no waiting.

<br>

![License](https://img.shields.io/badge/license-GPL--3.0-blue)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)
![Platform](https://img.shields.io/badge/platform-Linux%20%C2%B7%20macOS-lightgrey)

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

## Install

Grab a prebuilt binary, make it executable, and run it.

```bash
# Move the downloaded binary onto your PATH
chmod +x chat-cli
sudo mv chat-cli /usr/local/bin/

# Start it
chat-cli
```

That's it. No toolchain, no build step.

---

## First run

Launching with no arguments starts the Slack and WhatsApp setup flows so you can
connect an account from inside the app:

```bash
chat-cli
```

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
```

- **WhatsApp** pairs by scanning a QR code, just like WhatsApp Web.
- **Slack** offers a guided setup with several auth options, from user OAuth to a
  simple incoming webhook.

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
