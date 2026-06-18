# ChatCLI → Fono Voice Summaries

## Objective

Have ChatCLI send incoming message context to Fono so Fono can analyze the message and speak a short human-friendly summary instead of reading the raw message content aloud.

The intended spoken result is something like:

> “Mihai is reporting a production bug in the billing service and wants help investigating the failing deployment.”

or:

> “Alex is asking for someone to be granted access to a server.”

This should be implemented as a general-purpose Fono capability that can also be reused by Forge, Chess CLI, agents, scripts, and other open-source projects.

---

## Recommended Architecture

Build this in two layers:

1. Fono gets a universal `summarize_and_speak` capability.
2. ChatCLI calls that capability from its notification pipeline.

This avoids making ChatCLI responsible for LLM/TTS configuration and avoids making Fono ChatCLI-specific.

---

## Current Architecture Findings

### ChatCLI

ChatCLI already has a good hook point: the notification pipeline.

Incoming messages are evaluated for notification eligibility in the TUI notification path around:

`crates/tui/src/app.rs:11959-12083`

The best integration point is near pending notification delivery:

`crates/tui/src/app.rs:12148-12227`

That location is useful because ChatCLI has already handled:

- muted chats
- notification settings
- historical message suppression
- self-message suppression
- debounce/coalescing
- active-chat cancellation

The lower-level desktop notification crate is too late in the pipeline because it only receives a small preview object:

`crates/notify/src/lib.rs:7-11`

The desktop notification sender itself is implemented at:

`crates/notify/src/lib.rs:52-76`

with a `notify-send` fallback at:

`crates/notify/src/lib.rs:101-111`

The important conclusion is:

**Do not integrate Fono inside the `chat_notify` crate.**

The Fono integration should happen earlier, while ChatCLI still has the full message, chat, account, sender, and attachment metadata.

Relevant full message data is available in the core message model around:

`crates/core/src/types.rs:228-249`

Media attachment paths and thumbnails are represented around:

`crates/core/src/types.rs:352-360`

### Fono

Fono already owns the right responsibilities:

- assistant/LLM provider configuration
- TTS provider configuration
- MCP tools
- voice interaction behavior
- assistant prompts
- speech serialization/interruption behavior

Fono already has a `speak` capability, but that only speaks text. The missing universal capability is:

```text
summarize arbitrary external context → produce short spoken summary → speak it
```

The recommended addition is an MCP tool such as:

```text
fono.summarize_and_speak
```

Optionally, Fono can also expose a CLI command such as:

```text
fono summarize-speak
```

---

## Proposed Flow

```text
Incoming Slack/WhatsApp message
        ↓
ChatCLI notification pipeline decides it is worth notifying
        ↓
ChatCLI builds structured context
        ↓
ChatCLI sends context to Fono
        ↓
Fono summarizes with its configured assistant
        ↓
Fono speaks the summary with its configured TTS
```

---

## Example Payload from ChatCLI to Fono

```json
{
  "source_app": "chat-cli",
  "source_kind": "incoming_message",
  "account": "Slack / Engineering",
  "chat_name": "Backend Alerts",
  "chat_kind": "channel",
  "sender_name": "Mihai",
  "message_text": "long raw message, log, or normal chat text",
  "attachments": [
    {
      "kind": "image",
      "filename": "screenshot.png",
      "local_path": "/path/to/screenshot.png"
    }
  ],
  "instructions": "Summarize what this person likely wants in one or two spoken sentences. Do not read raw logs or long content aloud."
}
```

Fono might speak:

> “Mihai appears to be asking for help debugging a backend alert, likely related to the attached screenshot and service logs.”

---

## Implementation Plan

### Phase 1: Add the universal Fono feature

- [ ] Add a reusable Fono helper that accepts structured external context.
  - Rationale: The summarization behavior should be owned by Fono because Fono already owns assistant provider configuration, prompts, speech behavior, and TTS.

- [ ] Use Fono’s configured assistant model to summarize the context.
  - Rationale: This avoids duplicating model configuration in ChatCLI and keeps user-selected Fono providers authoritative.

- [ ] Use Fono’s configured TTS backend to speak the summary.
  - Rationale: Fono already owns voice selection and speech output behavior.

- [ ] Add an MCP tool named `fono.summarize_and_speak`.
  - Rationale: MCP is the best universal integration surface for ChatCLI, Forge, Chess CLI, and other tools.

- [ ] Optionally add a CLI command named `fono summarize-speak`.
  - Rationale: A CLI command is useful for scripts, manual testing, and a simple first ChatCLI integration path.

- [ ] Add a default summarization prompt.
  - Rationale: The prompt should ensure Fono says who wants what, avoids raw logs, avoids long quotes, and keeps the spoken output to one or two sentences.

### Phase 2: Add ChatCLI integration

- [ ] Add ChatCLI settings for voice summaries.
  - Rationale: Users need control over whether this feature is enabled and which messages are eligible.

- [ ] Add an async voice-summary dispatcher.
  - Rationale: ChatCLI’s TUI must remain responsive; Fono calls, LLM work, and TTS work must not block event handling or drawing.

- [ ] Trigger the dispatcher from the pending notification delivery path.
  - Rationale: This reuses existing notification eligibility, debounce, muted-chat behavior, paused-notification behavior, and active-chat cancellation.

- [ ] Build a structured payload from the full message, chat, sender, account, and attachments.
  - Rationale: Structured context lets Fono summarize intent instead of reading content verbatim.

- [ ] Send that payload to Fono.
  - Rationale: ChatCLI should be a caller of Fono’s universal capability, not a duplicate summarizer/TTS system.

- [ ] Do not block the TUI while Fono summarizes or speaks.
  - Rationale: The existing project guidelines require background work for expensive operations.

- [ ] Respect existing notification rules and active-chat cancellation.
  - Rationale: Voice summaries should not create a second notification system with different semantics.

### Phase 3: Improve behavior over time

- [ ] Add coalescing for message bursts.
  - Rationale: A flood of messages should not become a flood of stale spoken summaries.

- [ ] Add queue limits so old messages are not spoken late.
  - Rationale: Spoken notifications are time-sensitive and become annoying if delayed too long.

- [ ] Add attachment metadata.
  - Rationale: Fono should be able to say “there is an attached screenshot” or “this includes a log file” without reading or decoding everything immediately.

- [ ] Add image/vision support for screenshots.
  - Rationale: A future version can allow Fono to summarize screenshots when the configured assistant supports vision.

- [ ] Later, expose ChatCLI itself as an MCP server.
  - Rationale: This would let Fono ask for unread messages, search history, open chats, mark messages read, or help the user reply.

---

## Suggested ChatCLI Settings

- [ ] `voice_summaries_enabled`
- [ ] `voice_summary_scope`
  - direct messages only
  - mentions only
  - all notifications
- [ ] `voice_summary_include_muted`
- [ ] `voice_summary_active_chat_behavior`
  - suppress
  - delay
  - allow
- [ ] `voice_summary_transport`
  - MCP
  - CLI command
  - daemon IPC later

Settings infrastructure already exists in ChatCLI’s app settings:

`crates/storage/src/lib.rs:91-107`

The settings overlay is driven from:

`crates/tui/src/app.rs:1567-1732`

---

## Transport Options

### Option 1: MCP Tool

Recommended long-term option.

```text
fono.summarize_and_speak
```

Pros:

- universal
- works for other projects too
- aligned with Fono’s agent direction
- can later support human-in-the-loop workflows

Cons:

- ChatCLI needs a small MCP client or subprocess bridge.

### Option 2: CLI Command

Good first step.

```text
fono summarize-speak
```

Pros:

- simple
- easy to test
- easy for ChatCLI to call
- useful for shell scripts too

Cons:

- less elegant than MCP
- process startup overhead
- weaker for future interactive workflows

### Option 3: Fono Daemon IPC

Efficient later option.

Pros:

- fast
- daemon-native
- good cancellation/interruption behavior

Cons:

- tighter coupling
- less universal than MCP
- version compatibility concerns

---

## Verification Criteria

- [ ] Incoming eligible messages can trigger a spoken summary through Fono.
- [ ] Raw long messages/logs are not spoken verbatim.
- [ ] Summary is one or two sentences.
- [ ] Desktop notifications continue to work unchanged.
- [ ] Muted chats, paused notifications, historical messages, and self messages do not trigger voice summaries unless explicitly configured.
- [ ] The TUI remains responsive during summarization and speech.
- [ ] Multiple quick messages are coalesced or bounded so the user does not hear stale summaries.
- [ ] The same Fono capability can be used outside ChatCLI by another project.

---

## Potential Risks and Mitigations

1. **Fono speaks too much**
   Mitigation: Summarize only; never speak raw message text by default; cap summaries to one or two sentences.

2. **Message bursts become annoying**
   Mitigation: Coalesce per chat, add cooldowns, limit queue size, and drop stale summaries.

3. **Long logs exceed model context**
   Mitigation: Truncate intelligently, send excerpts plus metadata, and tell the model to identify topic and intent rather than analyze every line.

4. **ChatCLI becomes slow**
   Mitigation: Call Fono asynchronously and never run LLM, TTS, file reads, or network calls in the TUI event loop.

5. **Screenshots need vision support**
   Mitigation: First pass attachment metadata only; later pass image paths to Fono vision-capable providers.

6. **Too much coupling between ChatCLI and Fono**
   Mitigation: Put the durable abstraction in Fono as MCP/CLI rather than creating ChatCLI-specific Fono code.

---

## Alternative Approaches

1. **ChatCLI summarizes, Fono only speaks**
   - ChatCLI would call its own LLM provider, then pipe text to `fono speak`.
   - Trade-off: duplicates Fono’s assistant configuration and weakens the universal Fono integration.

2. **Fono polls ChatCLI through MCP**
   - ChatCLI exposes unread messages as MCP tools, and Fono decides what to speak.
   - Trade-off: powerful long-term, but more work because ChatCLI’s MCP crate is currently only a stub.

3. **Simple desktop notification mirroring**
   - Fono listens to desktop notifications and summarizes those.
   - Trade-off: easy conceptually, but loses full message context and attachment metadata.

4. **Direct daemon IPC only**
   - ChatCLI sends a Fono IPC request directly.
   - Trade-off: efficient, but less portable and less suitable for other projects.

---

## Final Recommendation

Start with:

- [ ] Add `fono.summarize_and_speak` as a universal MCP tool.
- [ ] Add `fono summarize-speak` for easy testing and shell integrations.
- [ ] Add a ChatCLI voice summaries on/off setting.
- [ ] Trigger only for messages that would already produce a notification.
- [ ] Speak one or two sentence summaries only.

This gives ChatCLI and Fono the synergy you want without making either project too dependent on the other.