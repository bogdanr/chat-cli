# Fono Summarize: Backend-Aware Timeouts + One Retry + One Fallback (Simplified)

## Objective

Make `fono summarize` (CLI and `fono.summarize` MCP tool) resilient to transient cloud
failures with the **simplest policy that covers the observed failure**, and make the LLM
timeouts match the backend: cloud requests should give up fast (a healthy cloud responds
in 1–5 s; the observed Cerebras stalls burned the full 30 s), while the embedded local
backend legitimately needs a long budget (model load + prompt eval of a capped 16 k-char
input).

Supersedes `plans/2026-06-12-fono-summarize-retry-and-backend-fallback-v1.md` (too
complicated: multi-fallback loop with attempt accounting).

## Policy (3 attempts maximum)

```text
attempt 1: configured backend
attempt 2: same backend, one immediate retry
attempt 3: first *available* fallback backend (single attempt)
then: fail with an error naming what was tried
```

"Available" = `build_assistant` succeeds for the candidate (it already errors on a
missing API key or missing local model file — `crates/fono-assistant/src/factory.rs:138-155`).

## Backend-aware timeouts

Selected by a tiny helper from `cfg.assistant.backend`
(`AssistantBackend::Ollama` = local/self-hosted; everything else = cloud):

| Constant | Cloud | Local (Ollama/embedded) |
|---|---|---|
| open (connect + first byte) | 10 s | 60 s |
| drain (full reply stream) | 30 s | 120 s |

Rationale: summaries are 1–2 sentences, so a cloud provider that hasn't produced a first
byte in 10 s is shedding load — failing fast makes room for the retry/fallback inside the
caller budget (chat-cli kills the process at 180 s, `crates/tui/src/voice_summary.rs:34`
in chat-cli). The local backend's first byte arrives only after prompt evaluation of up
to ~4 k tokens, which on modest hardware genuinely takes tens of seconds.

Worst case wall time: 10 + 10 + 60 = 80 s (cloud primary, local fallback) — comfortably
inside the 180 s budget.

## Implementation Plan

All edits in the fono repo, `crates/fono-mcp-server/src/summarize.rs` unless noted.

- [x] Task 1. Replace the fixed `LLM_OPEN_TIMEOUT`/`LLM_DRAIN_TIMEOUT` constants
      (`crates/fono-mcp-server/src/summarize.rs:35-37`) with two pairs
      (`CLOUD_OPEN_TIMEOUT = 10 s`, `CLOUD_DRAIN_TIMEOUT = 30 s`,
      `LOCAL_OPEN_TIMEOUT = 60 s`, `LOCAL_DRAIN_TIMEOUT = 120 s`) and a helper
      `fn llm_timeouts(backend: &AssistantBackend) -> (Duration, Duration)` that returns
      the local pair for `AssistantBackend::Ollama` and the cloud pair otherwise.
      `summarize_with` already receives `cfg`, so it picks the pair from
      `cfg.assistant.backend` with no signature change.

- [x] Task 2. Add `pub async fn summarize_with_retry(assistant, cfg, payload)`:
      call `summarize_with`; on error, `warn!` (target
      `fono_mcp_server::summarize`, include `assistant.name()` and the `{:#}` chain) and
      retry exactly once. Second failure returns the error with context
      `"retry on the same backend also failed"`.

- [x] Task 3. In `summarize()` (`crates/fono-mcp-server/src/summarize.rs:173-194`):
      keep all fail-fast behavior unchanged (empty `message_text`, primary build error,
      disabled assistant — existing tests must pass as-is). On
      `summarize_with_retry` failure, pick **one** fallback: iterate the preference order
      `[Cerebras, Groq, OpenAI, OpenRouter, Anthropic, Ollama]` minus the configured
      backend, take the first candidate whose `build_assistant` succeeds (clone
      `cfg.assistant`, set `backend`, set `cloud = None` so the provider-specific
      override block from `crates/fono-assistant/src/factory.rs:54-65` never leaks
      across providers), make a single `summarize_with` attempt on it, `warn!` before and
      `info!` on success. Caveat: the fallback attempt must use the *fallback* backend's
      timeout pair, not the primary's — derive the pair from the candidate, which means
      `summarize_with` should take the timeout pair (or the backend) as a parameter
      internally; keep the public signature stable by adding a private
      `summarize_with_timeouts` and making `summarize_with` a thin wrapper that derives
      the pair from `cfg`.

- [x] Task 4. Final error when everything failed: wrap the primary error with context
      naming the fallback backend tried (or "no fallback backend available"), so
      chat-cli's status line stays informative (it surfaces the last stderr line).

- [x] Task 5. Tests (extend the existing `mod tests`):
      - `llm_timeouts_local_vs_cloud`: Ollama → (60, 120); Cerebras/OpenAI → (10, 30).
      - `FlakyAssistant` mock with an `AtomicUsize` call counter:
        `retry_succeeds_on_second_attempt` (fails once, then succeeds → 2 calls, Ok) and
        `retry_gives_up_after_two_attempts` (always fails → exactly 2 calls, error
        mentions the retry context).
      - Fallback-order helper test: first candidate for primary Cerebras is Groq; the
        configured backend never appears.

- [x] Task 6. One-sentence doc updates in `docs/coding-agents.md` and
      `docs/configuration.md`: failed requests are retried once, then one alternate
      configured backend is tried; cloud timeouts are short, local timeouts long.

- [x] Task 7. Validation mirroring fono CI: `cargo fmt --all -- --check`,
      `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo test --workspace --all-targets`, `cargo build -p fono` (the `/usr/bin/fono`
      symlink picks it up immediately; no daemon restart needed — summarize runs in the
      short-lived CLI process). E2E smoke: pipe a payload into
      `fono summarize --json --silent` and confirm a summary.

## Verification Criteria

- A transient first-attempt failure is invisible to the caller when the retry succeeds.
- Exactly 2 attempts on the configured backend, at most 1 fallback attempt, never more.
- Cloud attempts give up within ~10 s of a stalled connection; local attempts keep the
  long budget.
- Permanent config errors fail fast with unchanged messages; existing tests green.
- fmt/clippy/workspace tests green.

## Potential Risks and Mitigations

1. **10 s cloud open-timeout too aggressive for slow networks** — Mitigation: constants
   are trivially tunable in one place; the retry + fallback absorb occasional misses.
2. **Fallback hides a degraded primary provider** — Mitigation: warn-level log on every
   failed attempt and on fallback use.
3. **Local fallback produces weaker summaries** — Mitigation: local is last in the
   preference order; only reached when no cloud candidate is available or the primary
   *is* cloud and the first available fallback happens to be local.

## Alternative Approaches

1. Multi-fallback chain (plan v1) — more resilient, more code; rejected as
   over-engineered for a notification summary.
2. Retry in chat-cli by re-spawning the process — duplicates policy per caller.
3. Config-exposed timeout/fallback settings — defer until the defaults prove wrong.
