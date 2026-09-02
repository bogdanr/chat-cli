//! Shared HTTP plumbing for the ClickUp provider: a single pooled blocking
//! agent, an adaptive per-token rate limiter, `429` retry, and secret
//! redaction.
//!
//! ClickUp's rate limit is **per token and plan-dependent** — 100 requests per
//! minute on Free/Unlimited/Business, 1,000 on Business Plus, and 10,000 on
//! Enterprise. A hardcoded pacing constant would therefore be either needlessly
//! slow on Enterprise or fatally aggressive on Free, so the limiter starts
//! conservative and re-tunes itself from the `X-RateLimit-*` response headers.

use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

/// Wall-clock ceiling for a single ClickUp HTTP request.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Rate limit assumed before any response has revealed the real one. This is
/// the Free/Unlimited/Business figure, i.e. the most restrictive plan, so the
/// first few requests can never overrun a low-tier workspace.
pub const DEFAULT_RATE_LIMIT_PER_MINUTE: u32 = 100;

/// Fraction of the detected rate limit the provider is willing to consume in
/// steady state, leaving the remainder for other tools sharing the same token.
const RATE_LIMIT_BUDGET_NUMERATOR: u32 = 1;
const RATE_LIMIT_BUDGET_DENOMINATOR: u32 = 2;

/// Floor on the inter-request gap, so even an Enterprise limit cannot turn into
/// an unbounded request storm.
const MIN_REQUEST_GAP: Duration = Duration::from_millis(25);

/// Ceiling on the inter-request gap, so a pathological header value cannot
/// stall the provider indefinitely.
const MAX_REQUEST_GAP: Duration = Duration::from_secs(10);

/// Attempts (including the first) for a request that keeps returning `429`.
pub const MAX_RATE_LIMIT_ATTEMPTS: u32 = 4;

/// Fallback backoff when a `429` carries no usable reset hint.
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(5);

/// Clamp for any server-provided backoff, so a bogus header cannot hang a poll
/// pass for minutes.
const MAX_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

/// Process-wide pooled agent. `http_status_as_error(false)` is essential: it
/// makes `429` and other error statuses arrive as inspectable responses whose
/// headers can be read, rather than as opaque transport errors.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// Rate-limit bookkeeping for one credential.
#[derive(Clone, Copy, Debug)]
struct TokenRateState {
    /// Earliest instant at which the next request may be issued.
    next_allowed: Option<Instant>,
    /// Requests-per-minute ceiling, learned from `X-RateLimit-Limit`.
    limit_per_minute: u32,
    /// Remaining requests in the current window, from `X-RateLimit-Remaining`.
    remaining: Option<u32>,
}

impl Default for TokenRateState {
    fn default() -> Self {
        Self {
            next_allowed: None,
            limit_per_minute: DEFAULT_RATE_LIMIT_PER_MINUTE,
            remaining: None,
        }
    }
}

impl TokenRateState {
    /// Target gap between requests: the detected limit scaled down by the
    /// budget fraction, then clamped.
    fn request_gap(&self) -> Duration {
        let permitted_per_minute = (self.limit_per_minute * RATE_LIMIT_BUDGET_NUMERATOR)
            / RATE_LIMIT_BUDGET_DENOMINATOR.max(1);
        let permitted_per_minute = permitted_per_minute.max(1);
        let gap = Duration::from_millis(60_000 / u64::from(permitted_per_minute));
        gap.clamp(MIN_REQUEST_GAP, MAX_REQUEST_GAP)
    }

    /// Extra caution when the window is nearly exhausted: once fewer than a
    /// tenth of the window remains, widen the gap so the tail of the window is
    /// spread out instead of being burned immediately.
    fn effective_gap(&self) -> Duration {
        let gap = self.request_gap();
        match self.remaining {
            Some(remaining) if remaining <= self.limit_per_minute / 10 => {
                (gap * 4).clamp(MIN_REQUEST_GAP, MAX_REQUEST_GAP)
            }
            _ => gap,
        }
    }
}

fn rate_states() -> &'static Mutex<HashMap<String, TokenRateState>> {
    static STATES: OnceLock<Mutex<HashMap<String, TokenRateState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Short, non-reversible fingerprint of a credential, used as the rate-limiter
/// key so the map never stores a usable token.
pub fn token_fingerprint(token: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in token.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Reserves the next request slot for `fingerprint` and sleeps until it is due.
///
/// Runs on a blocking worker thread (every caller is already inside
/// `spawn_blocking`), so the sleep never occupies an async runtime thread.
pub fn throttle(fingerprint: &str) {
    let wait = {
        let mut states = match rate_states().lock() {
            Ok(states) => states,
            Err(poisoned) => poisoned.into_inner(),
        };
        let state = states.entry(fingerprint.to_owned()).or_default();
        let now = Instant::now();
        let gap = state.effective_gap();
        let slot = match state.next_allowed {
            Some(next) if next > now => next,
            _ => now,
        };
        state.next_allowed = Some(slot + gap);
        slot.saturating_duration_since(now)
    };
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
}

/// Folds observed `X-RateLimit-*` headers back into the limiter so subsequent
/// pacing matches the workspace's actual plan.
pub fn observe_rate_limit_headers(fingerprint: &str, limit: Option<u32>, remaining: Option<u32>) {
    if limit.is_none() && remaining.is_none() {
        return;
    }
    let mut states = match rate_states().lock() {
        Ok(states) => states,
        Err(poisoned) => poisoned.into_inner(),
    };
    let state = states.entry(fingerprint.to_owned()).or_default();
    if let Some(limit) = limit.filter(|limit| *limit > 0) {
        state.limit_per_minute = limit;
    }
    state.remaining = remaining;
}

/// Current view of a token's rate budget, for performance instrumentation.
pub fn rate_budget_snapshot(fingerprint: &str) -> (u32, Option<u32>) {
    let states = match rate_states().lock() {
        Ok(states) => states,
        Err(poisoned) => poisoned.into_inner(),
    };
    states
        .get(fingerprint)
        .map(|state| (state.limit_per_minute, state.remaining))
        .unwrap_or((DEFAULT_RATE_LIMIT_PER_MINUTE, None))
}

#[cfg(test)]
pub fn reset_rate_limiter() {
    let mut states = match rate_states().lock() {
        Ok(states) => states,
        Err(poisoned) => poisoned.into_inner(),
    };
    states.clear();
}

/// A single header value parsed as an unsigned count.
fn header_u32(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<u32> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u32>().ok())
}

/// Backoff for a `429`, derived from `Retry-After` (seconds) or
/// `X-RateLimit-Reset` (absolute Unix seconds), clamped to a sane window.
fn rate_limit_backoff(response: &ureq::http::Response<ureq::Body>) -> Duration {
    if let Some(seconds) = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        return Duration::from_secs(seconds).clamp(Duration::from_secs(1), MAX_RATE_LIMIT_BACKOFF);
    }

    if let Some(reset_at) = response
        .headers()
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
    {
        let now = chrono::Utc::now().timestamp();
        let delta = reset_at.saturating_sub(now);
        if delta > 0 {
            return Duration::from_secs(delta as u64)
                .clamp(Duration::from_secs(1), MAX_RATE_LIMIT_BACKOFF);
        }
        // A reset timestamp already in the past means the window has rolled
        // over; a short nudge is enough.
        return Duration::from_secs(1);
    }

    DEFAULT_RATE_LIMIT_BACKOFF
}

/// HTTP methods the provider issues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Outcome of a successful (non-retryable) request.
pub struct RawResponse {
    pub status: u16,
    pub body: String,
}

impl RawResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Issues one paced, retrying ClickUp request.
///
/// `authorization` is the full header value (a bare `pk_...` personal token or
/// `Bearer ...` for OAuth). The request is throttled before every attempt and
/// retried with backoff on `429`.
pub fn request(
    method: Method,
    url: &str,
    authorization: &str,
    body: Option<&serde_json::Value>,
) -> Result<RawResponse> {
    let fingerprint = token_fingerprint(authorization);
    let mut attempt = 0_u32;

    loop {
        attempt += 1;
        throttle(&fingerprint);

        let agent = agent();
        let mut response = match method {
            Method::Get => agent
                .get(url)
                .header("Authorization", authorization)
                .header("Accept", "application/json")
                .call(),
            Method::Delete => agent
                .delete(url)
                .header("Authorization", authorization)
                .header("Accept", "application/json")
                .call(),
            Method::Post | Method::Patch => {
                let payload = body.cloned().unwrap_or(serde_json::json!({}));
                let builder = if method == Method::Post {
                    agent.post(url)
                } else {
                    agent.patch(url)
                };
                builder
                    .header("Authorization", authorization)
                    .header("Accept", "application/json")
                    .send_json(&payload)
            }
        }
        .with_context(|| {
            format!(
                "sending ClickUp {} request to {}",
                method.as_str(),
                redact_url(url)
            )
        })?;

        let status = response.status().as_u16();
        observe_rate_limit_headers(
            &fingerprint,
            header_u32(&response, "x-ratelimit-limit"),
            header_u32(&response, "x-ratelimit-remaining"),
        );

        if status == 429 && attempt < MAX_RATE_LIMIT_ATTEMPTS {
            std::thread::sleep(rate_limit_backoff(&response));
            continue;
        }

        let body = response.body_mut().read_to_string().with_context(|| {
            format!(
                "reading ClickUp {} response from {}",
                method.as_str(),
                redact_url(url)
            )
        })?;

        return Ok(RawResponse { status, body });
    }
}

/// Issues a request and decodes a successful JSON body, converting ClickUp's
/// error envelope into a readable, secret-free error.
pub fn request_json<T: serde::de::DeserializeOwned>(
    method: Method,
    url: &str,
    authorization: &str,
    body: Option<&serde_json::Value>,
) -> Result<T> {
    let response = request(method, url, authorization, body)?;
    if !response.is_success() {
        bail!(
            "ClickUp request failed ({}): {}",
            response.status,
            describe_error_body(&response.body)
        );
    }
    // `204 No Content` bodies are empty; give serde a valid JSON object so
    // unit-like response types still decode.
    let body = if response.body.trim().is_empty() {
        "{}"
    } else {
        response.body.as_str()
    };
    serde_json::from_str(body).with_context(|| {
        format!(
            "decoding ClickUp response from {} ({} bytes)",
            redact_url(url),
            response.body.len()
        )
    })
}

/// Issues a request that is expected to return no meaningful body.
pub fn request_empty(
    method: Method,
    url: &str,
    authorization: &str,
    body: Option<&serde_json::Value>,
) -> Result<()> {
    let response = request(method, url, authorization, body)?;
    if !response.is_success() {
        bail!(
            "ClickUp request failed ({}): {}",
            response.status,
            describe_error_body(&response.body)
        );
    }
    Ok(())
}

/// Extracts the most useful message from a ClickUp error body.
///
/// ClickUp returns `{"err": "...", "ECODE": "..."}` on v2 and a variety of
/// shapes on v3. Anything unrecognised falls back to a truncated raw body so
/// an unexpected schema still produces a diagnosable message.
pub fn describe_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "empty response body".to_owned();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for key in ["err", "error", "message", "detail"] {
            if let Some(text) = value.get(key).and_then(|value| value.as_str()) {
                let code = value
                    .get("ECODE")
                    .and_then(|value| value.as_str())
                    .map(|code| format!(" [{code}]"))
                    .unwrap_or_default();
                return redact_secrets(&format!("{text}{code}"));
            }
        }
    }

    let truncated: String = trimmed.chars().take(300).collect();
    redact_secrets(&truncated)
}

/// Strips the query string from a URL so nothing sensitive leaks through the
/// error path. ClickUp does not put credentials in query parameters today, but
/// error strings are rendered in the sidebar and written to the debug log.
pub fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_owned(),
    }
}

/// Replaces anything that looks like a ClickUp credential with a placeholder.
///
/// Personal tokens never expire, so a single leaked log line is a permanent
/// credential compromise. This runs on every string that can reach the UI or
/// the debug log.
pub fn redact_secrets(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    loop {
        // Find the earliest occurrence of any credential-ish prefix.
        let next = ["pk_", "Bearer ", "bearer "]
            .iter()
            .filter_map(|needle| rest.find(needle).map(|index| (index, *needle)))
            .min_by_key(|(index, _)| *index);

        let Some((index, needle)) = next else {
            out.push_str(rest);
            return out;
        };

        out.push_str(&rest[..index]);
        out.push_str("<redacted>");

        let after = &rest[index + needle.len()..];
        // Consume the credential body: token characters for `pk_`, the opaque
        // token for `Bearer `.
        let consumed = after
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
            .unwrap_or(after.len());
        rest = &after[consumed..];
    }
}

/// Redacts a credential-bearing option for `Debug` output.
pub fn redacted_option(value: &Option<String>) -> &'static str {
    match value {
        Some(_) => "<redacted>",
        None => "None",
    }
}

// ---------------------------------------------------------------------------
// Avatar cache
// ---------------------------------------------------------------------------

/// Ceiling on a single cached avatar body.
const AVATAR_DOWNLOAD_LIMIT_BYTES: u64 = 4 * 1024 * 1024;

/// How long a failed avatar fetch is remembered before it may be retried.
const AVATAR_FAILURE_RETRY_COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Default)]
struct AvatarDownloadRegistry {
    in_flight: std::collections::HashSet<String>,
    failed_at: HashMap<String, Instant>,
}

fn avatar_download_registry() -> &'static Mutex<AvatarDownloadRegistry> {
    static REGISTRY: OnceLock<Mutex<AvatarDownloadRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Registers `url` as downloading, or reports that it must not be queued
/// because it is already in flight or failed within the cooldown.
fn begin_avatar_download(url: &str) -> bool {
    let mut registry = avatar_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.in_flight.contains(url) {
        return false;
    }
    if registry
        .failed_at
        .get(url)
        .is_some_and(|failed_at| failed_at.elapsed() < AVATAR_FAILURE_RETRY_COOLDOWN)
    {
        return false;
    }
    registry.in_flight.insert(url.to_owned());
    true
}

fn finish_avatar_download(url: &str, success: bool) {
    let mut registry = avatar_download_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.in_flight.remove(url);
    if success {
        registry.failed_at.remove(url);
    } else {
        registry.failed_at.insert(url.to_owned(), Instant::now());
    }
}

/// Deterministic on-disk location for a ClickUp avatar URL. Pure: performs no
/// I/O, so it is safe to call from conversion code on any thread.
pub fn avatar_cache_file_path(url: &str) -> Option<std::path::PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("chat-cli")
        .join("clickup")
        .join("avatars");

    let extension = url
        .split('?')
        .next()
        .and_then(|path| path.rsplit('.').next())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 5
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or("img");

    Some(cache_dir.join(format!("{}.{extension}", token_fingerprint(url))))
}

/// Resolves a ClickUp avatar URL to a local cache path, downloading the bytes
/// on a blocking worker thread when they are not cached yet.
///
/// The reserved path is returned immediately so callers never block; the image
/// simply renders once the file appears. ClickUp serves profile pictures from a
/// public CDN, so no credential is attached.
pub fn cached_avatar_path(url: &str) -> Option<std::path::PathBuf> {
    let url = url.trim();
    let path = avatar_cache_file_path(url)?;
    if path.exists() {
        return Some(path);
    }
    if !begin_avatar_download(url) {
        return Some(path);
    }

    let url = url.to_owned();
    let destination = path.clone();
    std::thread::spawn(move || {
        let result = download_avatar_to_path(&url, &destination);
        finish_avatar_download(&url, result.is_ok());
    });

    Some(path)
}

/// Streams an avatar to a temporary sibling file and renames it into place, so
/// `path.exists()` never observes a partially written cache entry.
fn download_avatar_to_path(url: &str, path: &std::path::Path) -> Result<()> {
    let parent = path
        .parent()
        .context("ClickUp avatar cache path has no parent directory")?;
    std::fs::create_dir_all(parent).context("creating ClickUp avatar cache directory")?;

    let mut response = agent()
        .get(url)
        .call()
        .context("downloading ClickUp avatar")?;
    if !(200..300).contains(&response.status().as_u16()) {
        bail!(
            "ClickUp avatar request failed ({})",
            response.status().as_u16()
        );
    }
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(AVATAR_DOWNLOAD_LIMIT_BYTES)
        .reader();

    let temp_path = path.with_extension("part");
    let result = (|| -> Result<()> {
        let mut file =
            std::fs::File::create(&temp_path).context("creating ClickUp avatar cache file")?;
        std::io::copy(&mut reader, &mut file).context("reading ClickUp avatar body")?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    std::fs::rename(&temp_path, path).context("storing ClickUp avatar cache file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_personal_tokens_anywhere_in_a_string() {
        let redacted = redact_secrets("auth failed for pk_12345_ABCDEF while listing channels");
        assert!(!redacted.contains("pk_"));
        assert!(!redacted.contains("12345"));
        assert!(redacted.contains("<redacted>"));
        assert!(redacted.contains("while listing channels"));
    }

    #[test]
    fn redacts_bearer_tokens() {
        let redacted = redact_secrets("Authorization: Bearer abc123.def456 rejected");
        assert!(!redacted.contains("abc123"));
        assert!(redacted.contains("<redacted>"));
        assert!(redacted.contains("rejected"));
    }

    #[test]
    fn redacts_multiple_secrets_in_one_string() {
        let redacted = redact_secrets("pk_one and pk_two");
        assert_eq!(redacted, "<redacted> and <redacted>");
    }

    #[test]
    fn leaves_secret_free_strings_untouched() {
        let input = "channel_not_found for 90010000000";
        assert_eq!(redact_secrets(input), input);
    }

    #[test]
    fn redacts_query_strings_from_urls() {
        let redacted = redact_url("https://api.clickup.com/api/v3/x/chat/channels?cursor=abc");
        assert_eq!(
            redacted,
            "https://api.clickup.com/api/v3/x/chat/channels?<redacted>"
        );
        assert_eq!(redact_url("https://example.com/a"), "https://example.com/a");
    }

    #[test]
    fn describes_clickup_error_envelope_with_code() {
        let described = describe_error_body(r#"{"err":"Team not authorized","ECODE":"OAUTH_023"}"#);
        assert_eq!(described, "Team not authorized [OAUTH_023]");
    }

    #[test]
    fn describes_unknown_error_shape_by_truncating() {
        let described = describe_error_body("<html>gateway timeout</html>");
        assert!(described.contains("gateway timeout"));
    }

    #[test]
    fn describes_empty_error_body() {
        assert_eq!(describe_error_body("   "), "empty response body");
    }

    #[test]
    fn default_rate_gap_respects_the_most_restrictive_plan() {
        let state = TokenRateState::default();
        // 100/min budgeted at 50% => 50/min => 1200ms.
        assert_eq!(state.request_gap(), Duration::from_millis(1200));
    }

    #[test]
    fn learned_higher_limit_shrinks_the_gap() {
        let state = TokenRateState {
            limit_per_minute: 10_000,
            ..TokenRateState::default()
        };
        assert_eq!(state.request_gap(), MIN_REQUEST_GAP);

        let business_plus = TokenRateState {
            limit_per_minute: 1_000,
            ..TokenRateState::default()
        };
        assert_eq!(business_plus.request_gap(), Duration::from_millis(120));
    }

    #[test]
    fn near_exhaustion_widens_the_gap() {
        let state = TokenRateState {
            limit_per_minute: 100,
            remaining: Some(3),
            ..TokenRateState::default()
        };
        assert!(state.effective_gap() > state.request_gap());
    }

    #[test]
    fn healthy_remaining_does_not_widen_the_gap() {
        let state = TokenRateState {
            limit_per_minute: 100,
            remaining: Some(80),
            ..TokenRateState::default()
        };
        assert_eq!(state.effective_gap(), state.request_gap());
    }

    #[test]
    fn observing_headers_retunes_the_limiter() {
        let fingerprint = token_fingerprint("observe-headers-test-token");
        observe_rate_limit_headers(&fingerprint, Some(1_000), Some(950));
        let (limit, remaining) = rate_budget_snapshot(&fingerprint);
        assert_eq!(limit, 1_000);
        assert_eq!(remaining, Some(950));
    }

    #[test]
    fn fingerprint_never_contains_the_token() {
        let fingerprint = token_fingerprint("pk_secret_value");
        assert!(!fingerprint.contains("pk_"));
        assert!(!fingerprint.contains("secret"));
        assert_eq!(fingerprint.len(), 16);
    }

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        assert_eq!(token_fingerprint("a"), token_fingerprint("a"));
        assert_ne!(token_fingerprint("a"), token_fingerprint("b"));
    }

    #[test]
    fn redacted_option_hides_present_values() {
        assert_eq!(redacted_option(&Some("pk_abc".to_owned())), "<redacted>");
        assert_eq!(redacted_option(&None), "None");
    }
}
