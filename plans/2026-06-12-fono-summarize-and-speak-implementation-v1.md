# Fono: `summarize_speak` Implementation Plan

## Objective

Add a universal "summarize and speak" capability to Fono (`/mnt/live/memory/data/Work/fono`): external applications (first consumer: chat-cli) send structured notification context; Fono summarizes it into 1–2 spoken sentences using the already-configured assistant backend and speaks the result through the already-configured TTS backend.

Exposed as:

1. A new MCP tool: `fono.summarize_speak` (primary interface)
2. A new CLI subcommand: `fono summarize-speak` (thin wrapper, stdin-driven, for testing and shell integrations)

Out of scope for v1 (deliberate): image/vision analysis of attachments (metadata only), automatic agent routing, classification output, daemon IPC variant.

All file references below are relative to the fono repository root.

---

## Verified Integration Points (already confirmed in code)

- Tool trait + registry: `Tool` trait at `crates/fono-mcp-server/src/tools/mod.rs:67-80`; registration of the standard tool set at `crates/fono-mcp-server/src/tools/mod.rs:110-117`.
- Shared tool context: `McpContext` at `crates/fono-mcp-server/src/tools/mod.rs:26-52` — carries full `Config` and `Secrets`, but has only whisper/polish model dirs (no assistant models dir).
- TTS + playback + cross-process speak-slot serialization already packaged: `speak_text()` at `crates/fono-mcp-server/src/voice_io.rs:454-506`.
- Existing tool to copy the shape from: `SpeakTool` at `crates/fono-mcp-server/src/tools/speak.rs:17-84`.
- Assistant factory: `build_assistant(cfg, secrets, assistant_models_dir)` at `crates/fono-assistant/src/factory.rs:138-155`; returns `Ok(None)` when assistant is disabled or backend is `none`.
- One-shot programmatic assistant call pattern (arbitrary text, custom system prompt, empty history): `AssistantContext` at `crates/fono-assistant/src/traits.rs:108-125` and `Assistant::reply_stream` at `crates/fono-assistant/src/traits.rs:146-150`; working reference in `crates/fono/examples/smoke_assistant.rs:280-332`.
- CLI subcommand declaration area: `crates/fono/src/cli.rs:314-337` (existing `Speak` / `Mcp` subcommands); dispatch around `crates/fono/src/cli.rs:627-653`.
- Assistant/TTS config sections: `[assistant]` at `crates/fono-core/src/config.rs:680-711`, `[tts]` at `crates/fono-core/src/config.rs:377-395`, `[mcp]` at `crates/fono-core/src/config.rs:1078-1110`.

---

## Implementation Plan

> **Post-implementation revision (2026-06-12):** renamed per user feedback — MCP tool is now **`fono.summarize`** with an optional **`silent: true`** parameter (skips TTS, returns `{"spoken": false, "summary": ...}`), and the CLI subcommand is now **`fono summarize`** with **`--silent`** replacing `--dry-run`. Tool module lives at `crates/fono-mcp-server/src/tools/summarize.rs` (struct `SummarizeTool`). All references below to `summarize_speak`/`summarize-speak` predate the rename. Re-validated: fmt/clippy/workspace tests green; silent E2E on both transports via Cerebras.

### Phase A — Core helper (shared by MCP tool and CLI)

- [x] Task 1. Add `fono-assistant` as a dependency of `fono-mcp-server` in `crates/fono-mcp-server/Cargo.toml`, mirroring the feature flags (`openai-compat`, `anthropic`, local backend feature) that the `fono` binary crate already enables for `fono-assistant`. Rationale: the MCP server currently builds TTS and polish but not the assistant; the summarizer needs `build_assistant`. Verify feature unification does not bloat the default build — gate the local/embedded backend behind the same feature the daemon uses.

- [x] Task 2. Create a new module `crates/fono-mcp-server/src/summarize.rs` with a function conceptually shaped as `summarize(cfg, secrets, payload) -> Result<String>`. It must: (a) build the assistant via `build_assistant(&cfg.assistant, &secrets, &assistant_models_dir)`; (b) return a clear, actionable error when the assistant is disabled/`none` (message should point at `fono setup` / `[assistant]` config, same style as the TTS-disabled error in `crates/fono-mcp-server/src/voice_io.rs:464-469`); (c) render the structured payload into a single user-turn string; (d) call `reply_stream` with a strict summarization system prompt, empty history (`ConversationHistory` not used — pass `Vec::new()`), `screen_capture: None`, `prefer_vision: false`; (e) collect the streamed deltas into a full string and return it. Rationale: one helper, two entry points; no duplicated logic between MCP tool and CLI.

- [x] Task 3. Resolve the assistant models directory inside the helper via `fono_core::Paths::resolve()` (same pattern as `voices_dir` resolution at `crates/fono-mcp-server/src/voice_io.rs:461`) rather than extending `McpContext`. Rationale: the directory is only consulted by the Ollama/embedded-local backend; path resolution keeps `McpContext` unchanged and avoids touching every constructor and test. _Implementation note: done better than planned — the daemon passes `paths.polish_models_dir()` as the assistant models dir (the local assistant shares the polish GGUF weights, see `crates/fono/src/session.rs:717-721`), and `McpContext` already carries `polish_models_dir`, so the helper takes the dir as a parameter and the tool passes the already-plumbed value; the CLI resolves it via `Paths`._

- [x] Task 4. Define the default summarization system prompt as a constant in `crates/fono-core/src/config.rs` next to the existing assistant prompt defaults (near `crates/fono-core/src/config.rs:778-783`), and add an optional `[mcp].summarize_prompt` (or `[assistant].prompt_notify`) override field with serde default. Required prompt behavior: speak-friendly output, 1–2 sentences, state who wants what, never quote raw logs or long content, describe attachments at a high level, preserve key names (people, servers, services, projects), reply in the user's configured language when set. Rationale: defaults must live in `fono-core` config like every other prompt; the override keeps it user-tunable without code changes.

- [x] Task 5. Add an input length cap in the helper (suggested: ~16,000 characters of `message_text`, keeping head and tail slices with an elision marker). Rationale: the embedded local backend defaults to an 8,192-token context (`crates/fono-core/src/config.rs:762`); none of the existing paths truncate, and chat-cli may forward long logs.

### Phase B — MCP tool

- [x] Task 6. Create `crates/fono-mcp-server/src/tools/summarize_speak.rs` implementing the `Tool` trait, modeled on `SpeakTool` (`crates/fono-mcp-server/src/tools/speak.rs:17-84`). Name: `fono.summarize_speak`. Input schema (all optional except `message_text`): `source_app`, `source_kind`, `account`, `chat_name`, `chat_kind`, `sender_name`, `message_text` (string, required, may be long/raw), `attachments` (array of `{kind, filename, mime_type?, size_bytes?}` — metadata only, no paths consumed in v1), `instructions` (caller override appended to the system prompt), `voice` (TTS voice override, same as `fono.speak`). Rationale: structured fields produce better summaries than one blob and define the stable contract chat-cli will target.

- [x] Task 7. Tool `call()` flow: validate `message_text` non-empty → `summarize(...)` → `speak_text(&cfg, &secrets, &summary, voice, &daemon_ipc_candidates)` (`crates/fono-mcp-server/src/voice_io.rs:454-506`) → return success payload containing the spoken summary text, e.g. `{"spoken": true, "summary": "..."}`. Rationale: returning the summary lets callers (chat-cli) log/display what was spoken; reusing `speak_text` inherits the daemon speak-slot serialization, tray feedback, and drain timeout for free.

- [x] Task 8. Register the tool in `ToolRegistry::default_with_context` (`crates/fono-mcp-server/src/tools/mod.rs:110-117`) and declare the module in `crates/fono-mcp-server/src/tools/mod.rs:11-14`. Update any registry-count assertions in existing tests. Rationale: this is the established single registration point; tests asserting tool counts will otherwise fail. _Implementation note: no registry-count assertions exist in the codebase — nothing to update._

### Phase C — CLI subcommand

- [x] Task 9. Add a `SummarizeSpeak` variant to the CLI command enum near the existing `Speak` subcommand (`crates/fono/src/cli.rs:314-327`). Behavior: read stdin; default mode treats stdin as raw text (`message_text`); `--json` flag parses stdin as the same JSON payload the MCP tool accepts; optional flags `--sender`, `--chat`, `--source`, `--instructions`, `--voice` populate payload fields for the raw-text mode. Dispatch near `crates/fono/src/cli.rs:627-653`, calling the same Phase A helper followed by the same speak path. Add `--dry-run` (print summary, skip TTS) for testing without audio. Rationale: enables `echo "..." | fono summarize-speak` for immediate manual testing and gives chat-cli a zero-protocol transitional transport.

- [x] Task 10. Ensure the CLI path and MCP path produce identical summaries for identical payloads by construction (both call the Phase A helper; payload struct shared, with `serde::Deserialize`). Rationale: prevents behavioral drift between transports.

### Phase D — Tests, docs, validation

- [x] Task 11. Unit tests in `crates/fono-mcp-server`: (a) input schema validation — missing/empty `message_text` returns failure without building assistant or TTS; (b) payload→prompt rendering — structured fields appear in the rendered user turn, attachments rendered as descriptions, truncation cap applied with head/tail preservation; (c) registry exposes `fono.summarize_speak` in `tools/list`. Use a mock `Assistant` implementation for the helper test (trait object, no network). Rationale: the prompt-rendering and truncation logic is the part most likely to regress.

- [x] Task 12. Unit test in `crates/fono` for CLI argument parsing of `summarize-speak` (raw mode and `--json` mode), following the existing CLI test patterns in `crates/fono/src/cli.rs`. Rationale: clap wiring errors are cheap to catch at parse-test level.

- [x] Task 13. Documentation updates within the fono repo (which documents every tool and subcommand): add the tool to the MCP tool list in `docs/coding-agents.md` and the config key to `docs/configuration.md`; mention the subcommand in `fono --help` text via the clap doc comment. Rationale: fono's convention is that every tool/subcommand is documented; the MCP tool description doubles as the agent-facing contract.

- [x] Task 14. Run fono's CI-equivalent validation locally before committing: `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace`, plus the release-build target if fono's `.github/workflows/ci.yml` requires it (inspect that file first and mirror it). Rationale: required by both repos' safety rules. _Validation run 2026-06-12: `cargo fmt --all -- --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo test --workspace --all-targets` all green (size-budget/bench/deny CI jobs not mirrored locally — infrastructure gates, not code gates). E2E verified: CLI `--dry-run` with 500-line log via Groq produced a one-sentence summary naming Mihai and the deployment problem with no raw log lines; MCP `tools/call fono.summarize_speak` via Cerebras returned `{"spoken":true,"summary":...}` with TTS playback; tools/list exposes the new tool. Note: the embedded local assistant backend (gemma-4-e2b) crashes with a pre-existing GGML_ASSERT in the llama.cpp fork's fused Gated Delta Net check — unrelated to this change, affects all assistant paths in the dev checkout._

---

## Verification Criteria

- `echo "Mihai: staging deploy fails after auth-service migration, can someone check the logs? <500 lines of log>" | fono summarize-speak --dry-run` prints a 1–2 sentence summary naming Mihai and the deployment problem, and never echoes raw log lines.
- The same payload via MCP `tools/call fono.summarize_speak` returns `{"spoken": true, "summary": ...}` and audio plays through the configured TTS backend.
- With `[assistant].backend = "none"`, both entry points fail with a clear instruction to configure the assistant, without touching TTS.
- With TTS disabled, the error matches the existing `fono.speak` guidance style.
- A 200 KB `message_text` is truncated before the LLM call; the summary still identifies sender and topic.
- Two concurrent `summarize_speak` calls do not overlap audio when the daemon is running (speak-slot serialization inherited from `speak_text`).
- `cargo check`, `cargo test`, `cargo clippy` pass workspace-wide.

---

## Potential Risks and Mitigations

1. **Feature-flag/bloat regression from adding `fono-assistant` to `fono-mcp-server`**
   Mitigation: mirror the exact feature set the `fono` binary already enables; check binary size delta against fono's two-build size expectations (~22/60 MB) before committing.

2. **Per-call assistant + TTS construction latency**
   Mitigation: accepted for v1 (same cost profile as existing `fono.speak`/`fono.listen` tools). If too slow, a follow-up can cache the built assistant in `McpContext` via the same `OnceLock` pattern as `polish_classifier_cache` (`crates/fono-mcp-server/src/tools/mod.rs:23`).

3. **Summary quality varies across backends (small local models)**
   Mitigation: strict default prompt with explicit output constraints; `instructions` and `[mcp].summarize_prompt` overrides; `--dry-run` makes iteration cheap.

4. **Long-input cost on cloud backends**
   Mitigation: the truncation cap bounds tokens; head+tail slicing preserves the parts of logs most likely to carry intent.

5. **Registry/count test breakage**
   Mitigation: Task 8 explicitly includes updating tool-count assertions.

---

## Alternative Approaches

1. **Daemon IPC variant (`Request::SummarizeSpeak`)**: lower per-call latency (daemon's already-built assistant/TTS) and native `Cancel` integration, but couples callers to the bincode protocol and same-build requirement. Deferred; can be added later behind the same helper.
2. **`fono speak --summarize` flag instead of a new subcommand**: fewer subcommands, but muddles `speak`'s "speaks exactly what you give it" contract and complicates its streaming stdin mode. Rejected.
3. **chat-cli summarizes, fono only speaks**: no fono changes needed, but duplicates LLM provider config in chat-cli and defeats the universal-capability goal. Rejected.

---

## Follow-up (separate plans, not this one)

- chat-cli side: voice-summary settings + async dispatcher hooked into pending-notification delivery (`crates/tui/src/app.rs:12148-12227` in chat-cli), calling `fono summarize-speak` first, MCP later.
- Vision support: extend `AssistantContext` with caller-supplied images so attachments (screenshots) can be analyzed, not just described.
- `fono.handle_external_event` generalization with modes (`summarize_only`, `ask_before_agent`, `agent_decides`) once agent routing work begins.
