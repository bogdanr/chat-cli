# chat-cli

A unified, keyboard-and-mouse friendly terminal chat client for people who want fast conversations without leaving the command line.

`chat-cli` is being built as a polished terminal app for WhatsApp and Slack, with a shared message model, local storage, rich message rendering, and a future MCP interface for voice and assistant integrations.

## Project goals

- **Excellent terminal UX** — a clean, discoverable interface that feels like a modern app, not a debug console.
- **Unified inbox** — bring chats from multiple accounts and providers into one responsive view.
- **Rich conversations** — support replies, reactions, threads, media previews, links, read states, and message actions.
- **Low resource usage** — avoid wasteful redraw loops, keep rendering lightweight, and cache expensive media work.
- **Local-first state** — persist accounts, chats, messages, reactions, and identity data in SQLite.
- **Cross-platform binaries** — ship a self-contained app for Linux, macOS, and Windows.
- **Extensible providers** — keep WhatsApp, Slack, and future integrations behind a common provider interface.
- **Assistant-ready workflows** — expose useful chat actions through MCP so tools like voice assistants can summarize, search, and reply.

## Current status

The foundation is in place:

- Rust workspace with separate crates for the app, core domain model, TUI, storage, providers, notifications, and MCP.
- SQLite-backed local storage for accounts, chats, messages, reactions, receipts, and people.
- Mock provider for local development and UI testing.
- Terminal UI with chat list, message list, compose box, status bar, account filter, notifications, message actions, replies, reactions, image/media previews, and thread view.
- WhatsApp bridge prototype crate for the cgo-to-async integration path.

Slack, production WhatsApp provider behavior, MCP tools, and system notifications are planned next-stage integrations.

## Try the mock UI

```bash
cargo run -p chat-cli -- --mock-provider
```

Use the mock provider to explore the interface without connecting a real account.

## Build

```bash
cargo build --workspace
```

## Test

```bash
cargo test --workspace
```

## Workspace layout

```text
crates/
  chat-cli/              CLI entry point
  core/                  Shared domain types, provider trait, mock provider
  tui/                   Terminal UI and interaction model
  storage/               SQLite persistence layer
  providers/whatsapp/    WhatsApp bridge and provider work
  providers/slack/       Slack provider work
  mcp/                   MCP server integration
  notify/                System notification integration
plans/                   Implementation plans and handoff notes
```

## Development philosophy

The project prioritizes a refined user experience and efficient runtime behavior. Every feature should be easy to discover, pleasant to use with both keyboard and pointer input, and validated with focused tests plus a full workspace build before a session is considered complete.
