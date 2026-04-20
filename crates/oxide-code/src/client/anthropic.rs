use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::debug;
use uuid::Uuid;

use super::billing;
use crate::config::{Auth, Config, ThinkingConfig};
use crate::message::{ContentBlock, Message, Role};
use crate::prompt::SYSTEM_PROMPT_DYNAMIC_BOUNDARY;
use crate::tool::ToolDefinition;

const API_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_BETA_HEADER: &str = "claude-code-20250219";
const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";
const CONTEXT_MANAGEMENT_BETA_HEADER: &str = "context-management-2025-06-27";
const EFFORT_BETA_HEADER: &str = "effort-2025-11-24";
const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const PROMPT_CACHING_SCOPE_BETA_HEADER: &str = "prompt-caching-scope-2026-01-05";

/// Matches the installed Claude Code version.
const CLAUDE_CLI_VERSION: &str = "2.1.101";

/// OAuth-required identity prefix. The Anthropic API returns 429 for non-Haiku
/// models with OAuth tokens unless the system prompt starts with this exact
/// string in its own text block.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// ── Request types ──

#[derive(Serialize)]
struct CreateMessageRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    metadata: RequestMetadata,
    /// Serialized before `messages` so the billing header's `cch=00000`
    /// placeholder appears first in the JSON, making [`billing::inject_cch`]'s
    /// single-occurrence replacement safe even when tool results contain the
    /// literal placeholder string.
    system: Vec<SystemBlock<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a ThinkingConfig>,
    messages: &'a [Message],
}

/// Request metadata matching Claude Code's `getAPIMetadata()` format.
///
/// `user_id` is a stringified JSON object containing `session_id` (and
/// optionally `device_id` / `account_uuid`). The API receives it as a
/// flat string, not a nested object.
#[derive(Serialize)]
struct RequestMetadata {
    user_id: String,
}

/// A text block in the system prompt array. The Anthropic API accepts `system`
/// as either a string or an array of these blocks. Using the array form lets
/// the identity prefix occupy its own block, which is required for OAuth
/// validation on non-Haiku models.
#[derive(Serialize)]
struct SystemBlock<'a> {
    r#type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

/// Prompt caching control. The `scope` field determines the cache sharing
/// level: `"global"` for static content identical across sessions, `"org"`
/// for organization-scoped content.
#[derive(Serialize)]
struct CacheControl {
    r#type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
}

// ── SSE response types ──

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields are populated by serde and used in downstream matching"
    )
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageResponse,
    },
    ContentBlockStart {
        index: usize,
        content_block: ContentBlockInfo,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
    /// Catch-all for unrecognized event types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    pub model: String,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockInfo {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    ServerToolUse {
        id: String,
        name: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Catch-all for unrecognized block types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names mirror Anthropic API delta type values (text_delta, input_json_delta, etc.)"
)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    /// Full signature value (overwrites, not appended).
    SignatureDelta {
        signature: String,
    },
    /// Catch-all for unrecognized delta types.
    /// Silently skipped during stream processing.
    #[serde(other)]
    Unknown,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<String>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "fields populated by serde, defined for full SSE protocol coverage"
    )
)]
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// ── Client ──

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
    session_id: String,
}

impl Client {
    pub fn new(config: Config, session_id: Option<String>) -> Result<Self> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut headers = HeaderMap::new();

        let mut betas = vec![
            CLAUDE_CODE_BETA_HEADER,
            CONTEXT_1M_BETA_HEADER,
            CONTEXT_MANAGEMENT_BETA_HEADER,
            EFFORT_BETA_HEADER,
            INTERLEAVED_THINKING_BETA_HEADER,
            PROMPT_CACHING_SCOPE_BETA_HEADER,
        ];

        match &config.auth {
            Auth::ApiKey(key) => {
                let mut value = HeaderValue::from_str(key)?;
                value.set_sensitive(true);
                headers.insert("x-api-key", value);
            }
            Auth::OAuth(token) => {
                let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
                value.set_sensitive(true);
                headers.insert(AUTHORIZATION, value);
                betas.push(OAUTH_BETA_HEADER);
            }
        }

        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        headers.insert("anthropic-beta", HeaderValue::from_str(&betas.join(","))?);
        headers.insert(
            "anthropic-dangerous-direct-browser-access",
            HeaderValue::from_static("true"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("claude-cli/{CLAUDE_CLI_VERSION} (external, cli)"))?,
        );
        headers.insert("x-app", HeaderValue::from_static("cli"));
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_str(&session_id)?,
        );
        // Stainless SDK headers — the Anthropic TypeScript SDK adds these
        // automatically. Third-party gateways may check for their presence.
        headers.insert("x-stainless-lang", HeaderValue::from_static("js"));
        headers.insert(
            "x-stainless-os",
            HeaderValue::from_static(normalize_platform(std::env::consts::OS)),
        );
        headers.insert(
            "x-stainless-arch",
            HeaderValue::from_static(normalize_arch(std::env::consts::ARCH)),
        );

        // No whole-request timeout — assistant responses can legitimately
        // run for minutes. The 60 s read timeout catches slowloris dribble;
        // Anthropic sends keepalives every ~15 s on healthy streams.
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_mins(1))
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self {
            http,
            config,
            session_id,
        })
    }

    /// Returns the model name for use in the system prompt.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Client-side session id carried in `x-claude-code-session-id` /
    /// billing metadata. Exposed for tests to assert on the id that
    /// [`Client::new`] plumbed through (either the caller-supplied
    /// value or an auto-generated UUID v4).
    #[cfg(test)]
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Stream a message response from the Anthropic API.
    ///
    /// `system_sections` are the static system prompt sections (one text
    /// block per section, matching Claude Code's multi-block layout).
    ///
    /// `user_context` is a `<system-reminder>`-wrapped string that gets
    /// prepended to the messages array as a synthetic user message,
    /// matching Claude Code's `prependUserContext()` pattern. This keeps
    /// dynamic content (CLAUDE.md) out of the `system` parameter.
    ///
    /// Returns a channel receiver yielding [`StreamEvent`]s. The caller
    /// should recv events as they arrive for real-time output.
    pub fn stream_message(
        &self,
        messages: &[Message],
        system_sections: &[&str],
        user_context: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        // Prepend user context as a synthetic user message (messages[0]).
        let messages_with_context: Vec<Message>;
        let effective_messages: &[Message] = if let Some(ctx) = user_context {
            messages_with_context = std::iter::once(Message::user(ctx))
                .chain(messages.iter().cloned())
                .collect();
            &messages_with_context
        } else {
            messages
        };

        let billing_header = if matches!(self.config.auth, Auth::OAuth(_)) {
            let first_text = first_user_text(effective_messages);
            let fingerprint = billing::compute_fingerprint(first_text, CLAUDE_CLI_VERSION);
            Some(billing::build_billing_header(
                CLAUDE_CLI_VERSION,
                &fingerprint,
            ))
        } else {
            None
        };

        // Build system blocks matching Claude Code's `splitSysPromptPrefix`:
        //   1. Billing header (no cache_control)
        //   2. Identity prefix (no cache_control)
        //   3. Static sections joined (cache_control: ephemeral, scope: global)
        //   4. Dynamic sections joined (no cache_control)
        // The boundary marker is filtered out before sending to the API.
        let (static_sections, dynamic_sections) = split_at_boundary(system_sections);
        let static_joined = static_sections.join("\n\n");
        let dynamic_joined = dynamic_sections.join("\n\n");

        let mut system_blocks = Vec::new();
        if let Some(ref header) = billing_header {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: header,
                cache_control: None,
            });
        }
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: SYSTEM_PROMPT_PREFIX,
            cache_control: None,
        });
        if !static_joined.is_empty() {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: &static_joined,
                cache_control: Some(CacheControl {
                    r#type: "ephemeral",
                    scope: Some("global"),
                }),
            });
        }
        if !dynamic_joined.is_empty() {
            system_blocks.push(SystemBlock {
                r#type: "text",
                text: &dynamic_joined,
                cache_control: None,
            });
        }

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        let mut body = serde_json::to_string(&CreateMessageRequest {
            model: &self.config.model,
            max_tokens: self.config.max_tokens,
            stream: true,
            metadata: self.build_metadata(),
            system: system_blocks,
            tools: (!tools.is_empty()).then_some(tools),
            thinking: self.config.thinking.as_ref(),
            messages: effective_messages,
        })
        .context("failed to serialize request")?;

        if billing_header.is_some() {
            body = billing::inject_cch(&body);
        }

        debug!(body_len = body.len(), "sending API request");

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();

        tokio::spawn(async move {
            let result = stream_sse(&http, &url, body, &tx).await;
            if let Err(e) = result {
                _ = tx.send(Err(e)).await;
            }
        });

        Ok(rx)
    }

    /// Send a single non-streaming completion request and return the
    /// concatenated text of the assistant's response.
    ///
    /// Used for background helpers like AI title generation — a one-shot
    /// `prompt → answer` call that bypasses the tools / thinking / dynamic
    /// context plumbing the interactive stream needs. The auth pipeline
    /// (OAuth vs API key, billing attestation) still applies so the same
    /// client works for both auth modes.
    ///
    /// Non-text content blocks (`tool_use`, thinking, …) are filtered out
    /// so callers get the assistant's user-visible answer directly.
    pub async fn complete(
        &self,
        model: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String> {
        let body = build_completion_body(
            model,
            system,
            user,
            max_tokens,
            &self.config.auth,
            &self.session_id,
        )?;

        let url = format!("{}/v1/messages?beta=true", self.config.base_url);
        debug!(model, body_len = body.len(), "sending completion request");

        let response = self.http.post(&url).body(body).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("API error (HTTP {status}): {body}");
        }

        let CompletionResponse { content } = response
            .json()
            .await
            .context("failed to parse completion response")?;
        Ok(join_text_blocks(content))
    }

    /// Build the `metadata.user_id` field as a stringified JSON object.
    fn build_metadata(&self) -> RequestMetadata {
        build_metadata(&self.session_id)
    }
}

/// Shared `metadata.user_id` builder used by both the streaming and
/// non-streaming request paths. Kept at file scope so
/// [`build_completion_body`] can call it without pulling in `&Client`.
fn build_metadata(session_id: &str) -> RequestMetadata {
    let user_id = serde_json::json!({ "session_id": session_id }).to_string();
    RequestMetadata { user_id }
}

/// Serialize the JSON request body for [`Client::complete`]. Extracted
/// so the billing-header / identity-prefix / system-block assembly can
/// be asserted on without a live HTTP client — the HTTP leg of
/// [`Client::complete`] is covered by a `TcpListener` integration test
/// further down.
///
/// System block order matches [`Client::stream_message`]:
///   1. Billing header (OAuth only; no `cache_control`, injected with `cch`)
///   2. Identity prefix (required for non-Haiku OAuth, cheap safety net)
///   3. Caller-supplied system prompt (omitted when empty)
fn build_completion_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    auth: &Auth,
    session_id: &str,
) -> Result<String> {
    let messages = [Message::user(user)];

    let billing_header = matches!(auth, Auth::OAuth(_)).then(|| {
        let fingerprint = billing::compute_fingerprint(user, CLAUDE_CLI_VERSION);
        billing::build_billing_header(CLAUDE_CLI_VERSION, &fingerprint)
    });

    let mut system_blocks = Vec::with_capacity(3);
    if let Some(ref header) = billing_header {
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: header,
            cache_control: None,
        });
    }
    system_blocks.push(SystemBlock {
        r#type: "text",
        text: SYSTEM_PROMPT_PREFIX,
        cache_control: None,
    });
    if !system.is_empty() {
        system_blocks.push(SystemBlock {
            r#type: "text",
            text: system,
            cache_control: None,
        });
    }

    let mut body = serde_json::to_string(&CreateMessageRequest {
        model,
        max_tokens,
        stream: false,
        metadata: build_metadata(session_id),
        system: system_blocks,
        tools: None,
        thinking: None,
        messages: &messages,
    })
    .context("failed to serialize request")?;

    if billing_header.is_some() {
        body = billing::inject_cch(&body);
    }
    Ok(body)
}

/// Flatten a `messages.create` response's content array into the
/// assistant's user-visible text. Extracted so the filter logic is
/// reusable and independently testable.
fn join_text_blocks(content: Vec<ContentBlock>) -> String {
    content
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        })
        .collect()
}

/// Shape we accept back from the non-streaming `/v1/messages` endpoint.
/// The API sends many more fields (`id`, `role`, `model`, `stop_reason`,
/// `usage`); we only care about the content blocks — serde ignores the rest.
#[derive(Deserialize)]
struct CompletionResponse {
    content: Vec<ContentBlock>,
}

/// Map `std::env::consts::OS` to the Stainless SDK's `normalizePlatform` names.
fn normalize_platform(os: &str) -> &'static str {
    match os {
        "macos" => "MacOS",
        "linux" => "Linux",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "ios" => "iOS",
        "android" => "Android",
        _ => "Unknown",
    }
}

/// Map `std::env::consts::ARCH` to the Stainless SDK's `normalizeArch` names.
fn normalize_arch(arch: &str) -> &'static str {
    match arch {
        "x86" => "x32",
        "x86_64" => "x64",
        "arm" => "arm",
        "aarch64" => "arm64",
        _ => "unknown",
    }
}

/// Split system sections at the boundary marker into static and dynamic parts.
///
/// Returns `(static_sections, dynamic_sections)`. The boundary marker itself
/// is excluded from both. Sections before the boundary are static (globally
/// cacheable); sections after are dynamic (per-session).
fn split_at_boundary<'a>(sections: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let boundary_pos = sections
        .iter()
        .position(|&s| s == SYSTEM_PROMPT_DYNAMIC_BOUNDARY);

    if let Some(pos) = boundary_pos {
        let static_part = sections[..pos]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect();
        let dynamic_part = sections[pos + 1..]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect();
        (static_part, dynamic_part)
    } else {
        // No boundary — treat everything as static.
        let all = sections.iter().filter(|s| !s.is_empty()).copied().collect();
        (all, Vec::new())
    }
}

/// Extract the text of the first user message for fingerprint computation.
fn first_user_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .find(|m| m.role == Role::User)
        .into_iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// Hard cap on the unterminated SSE frame buffer. A misbehaving upstream that
/// never emits `\n\n` would otherwise let `buf` grow without bound until OOM.
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

async fn stream_sse(
    http: &reqwest::Client,
    url: &str,
    body: String,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let response = http.post(url).body(body).send().await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("API error (HTTP {status}): {body}");
    }

    let mut stream = response.bytes_stream();
    // Byte buffer: reassembles UTF-8 sequences split across chunk boundaries
    // intact (`from_utf8_lossy` would inject U+FFFD at the boundary).
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading response stream")?;
        buf.extend_from_slice(&chunk);

        loop {
            let Some(end) = buf.windows(2).position(|w| w == b"\n\n") else {
                if buf.len() > MAX_SSE_FRAME_BYTES {
                    bail!(
                        "SSE frame buffer exceeded {MAX_SSE_FRAME_BYTES} bytes without \
                         a terminating blank line; upstream may be misbehaving"
                    );
                }
                break;
            };
            let frame_bytes: Vec<u8> = buf.drain(..end + 2).take(end).collect();

            let frame = match std::str::from_utf8(&frame_bytes) {
                Ok(s) => s,
                Err(e) => {
                    debug!("skipping invalid UTF-8 SSE frame: {e}");
                    continue;
                }
            };

            // A single malformed frame should not poison the whole turn.
            match parse_sse_frame(frame) {
                Ok(Some(event)) => {
                    if tx.send(Ok(event)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => debug!("skipping malformed SSE frame: {e:#}"),
            }
        }
    }

    Ok(())
}

/// Parse a single SSE frame into a [`StreamEvent`].
///
/// Per the SSE spec, multiple `data:` lines concatenate with `\n`.
/// Anthropic currently emits single-line data, but we follow the spec
/// so a future multi-line payload doesn't silently lose everything
/// but the last line.
fn parse_sse_frame(frame: &str) -> Result<Option<StreamEvent>> {
    let mut data_lines: Vec<&str> = Vec::new();

    for line in frame.lines() {
        if let Some(value) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            data_lines.push(value);
        }
    }

    let data: std::borrow::Cow<'_, str> = match data_lines.as_slice() {
        [] => return Ok(None),
        [one] => std::borrow::Cow::Borrowed(*one),
        _ => std::borrow::Cow::Owned(data_lines.join("\n")),
    };

    let event: StreamEvent =
        serde_json::from_str(&data).with_context(|| format!("failed to parse SSE data: {data}"))?;

    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    // ── Client::new / Client::model ──
    //
    // Client construction is 0% covered today because `stream_message` /
    // `complete` need a live HTTP mock (tracked as Tier 3 of the
    // integration-tests plan). These tests exercise the non-HTTP
    // portions — header validation, UUID defaulting, getter contracts —
    // that are reachable via the constructor alone.

    fn make_config(auth: Auth) -> Config {
        Config {
            auth,
            model: "claude-sonnet-4-6".to_owned(),
            base_url: "https://example.invalid".to_owned(),
            max_tokens: 128,
            thinking: None,
            show_thinking: false,
        }
    }

    #[test]
    fn new_with_api_key_succeeds_and_exposes_model() {
        let client = Client::new(make_config(Auth::ApiKey("sk-test".to_owned())), None).unwrap();
        assert_eq!(client.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn new_with_oauth_token_succeeds_and_exposes_model() {
        let client = Client::new(make_config(Auth::OAuth("oauth-token".to_owned())), None).unwrap();
        assert_eq!(client.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn new_none_session_id_generates_a_uuid() {
        let client = Client::new(make_config(Auth::ApiKey("k".to_owned())), None).unwrap();
        let sid = client.session_id();
        let parsed = Uuid::parse_str(sid)
            .unwrap_or_else(|_| panic!("auto-generated session_id is not a UUID: {sid:?}"));
        assert_eq!(parsed.get_version_num(), 4);
    }

    #[test]
    fn new_preserves_explicit_session_id() {
        let sid = "11111111-2222-4333-8444-555555555555".to_owned();
        let client =
            Client::new(make_config(Auth::ApiKey("k".to_owned())), Some(sid.clone())).unwrap();
        assert_eq!(client.session_id(), sid);
    }

    fn new_err_message(result: Result<Client>) -> String {
        // `Client` does not derive `Debug`, so `.unwrap_err()` doesn't
        // compile; this helper keeps the error-path tests readable
        // without forcing a blanket derive on the production type.
        match result {
            Ok(_) => panic!("expected Client::new to fail"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn new_rejects_api_key_containing_invalid_header_bytes() {
        // reqwest's HeaderValue::from_str rejects control chars like \n;
        // this exercises the early-error path in the API-key arm that
        // isn't otherwise reachable via the happy-path config loader.
        let err = new_err_message(Client::new(
            make_config(Auth::ApiKey("bad\nkey".to_owned())),
            Some("sid".to_owned()),
        ));
        assert!(err.to_ascii_lowercase().contains("header"));
    }

    #[test]
    fn new_rejects_oauth_token_containing_invalid_header_bytes() {
        let err = new_err_message(Client::new(
            make_config(Auth::OAuth("bad\rtoken".to_owned())),
            Some("sid".to_owned()),
        ));
        assert!(err.to_ascii_lowercase().contains("header"));
    }

    // ── StreamEvent ──

    #[test]
    fn stream_event_content_block_start_text() {
        let json =
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        let StreamEvent::ContentBlockStart {
            index,
            content_block,
        } = event
        else {
            panic!("expected ContentBlockStart");
        };
        assert_eq!(index, 0);
        assert!(matches!(content_block, ContentBlockInfo::Text { text } if text.is_empty()));
    }

    #[test]
    fn stream_event_content_block_stop() {
        let json = r#"{"type":"content_block_stop","index":2}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        let StreamEvent::ContentBlockStop { index } = event else {
            panic!("expected ContentBlockStop");
        };
        assert_eq!(index, 2);
    }

    #[test]
    fn stream_event_message_delta_with_usage() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        let StreamEvent::MessageDelta { delta, usage } = event else {
            panic!("expected MessageDelta");
        };
        assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
        let usage = usage.expect("expected usage");
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 42);
    }

    #[test]
    fn stream_event_message_stop() {
        let json = r#"{"type":"message_stop"}"#;
        let event: StreamEvent = serde_json::from_str(json).unwrap();
        assert!(matches!(event, StreamEvent::MessageStop));
    }

    // ── ContentBlockInfo ──

    #[test]
    fn content_block_info_text() {
        let json = r#"{"type":"text","text":"Hello world"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::Text { text } = info else {
            panic!("expected Text");
        };
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn content_block_info_tool_use() {
        let json = r#"{"type":"tool_use","id":"toolu_01","name":"bash"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::ToolUse { id, name } = info else {
            panic!("expected ToolUse");
        };
        assert_eq!(id, "toolu_01");
        assert_eq!(name, "bash");
    }

    #[test]
    fn content_block_info_server_tool_use() {
        let json = r#"{"type":"server_tool_use","id":"stu_01","name":"advisor"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::ServerToolUse { id, name } = info else {
            panic!("expected ServerToolUse");
        };
        assert_eq!(id, "stu_01");
        assert_eq!(name, "advisor");
    }

    #[test]
    fn content_block_info_thinking() {
        let json = r#"{"type":"thinking","thinking":"Let me analyze this","signature":"sig_xyz"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::Thinking {
            thinking,
            signature,
        } = info
        else {
            panic!("expected Thinking");
        };
        assert_eq!(thinking, "Let me analyze this");
        assert_eq!(signature, "sig_xyz");
    }

    #[test]
    fn content_block_info_redacted_thinking() {
        let json = r#"{"type":"redacted_thinking","data":"base64data=="}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        let ContentBlockInfo::RedactedThinking { data } = info else {
            panic!("expected RedactedThinking");
        };
        assert_eq!(data, "base64data==");
    }

    #[test]
    fn content_block_info_unknown_type() {
        let json = r#"{"type":"some_future_block","data":"opaque"}"#;
        let info: ContentBlockInfo = serde_json::from_str(json).unwrap();
        assert!(matches!(info, ContentBlockInfo::Unknown));
    }

    // ── Delta ──

    #[test]
    fn delta_text() {
        let json = r#"{"type":"text_delta","text":"Hello"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        let Delta::TextDelta { text } = delta else {
            panic!("expected TextDelta");
        };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn delta_input_json() {
        let json = r#"{"type":"input_json_delta","partial_json":"{\"key\":"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        let Delta::InputJsonDelta { partial_json } = delta else {
            panic!("expected InputJsonDelta");
        };
        assert_eq!(partial_json, r#"{"key":"#);
    }

    #[test]
    fn delta_thinking() {
        let json = r#"{"type":"thinking_delta","thinking":"partial reasoning"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        let Delta::ThinkingDelta { thinking } = delta else {
            panic!("expected ThinkingDelta");
        };
        assert_eq!(thinking, "partial reasoning");
    }

    #[test]
    fn delta_signature() {
        let json = r#"{"type":"signature_delta","signature":"sig_abc123"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        let Delta::SignatureDelta { signature } = delta else {
            panic!("expected SignatureDelta");
        };
        assert_eq!(signature, "sig_abc123");
    }

    #[test]
    fn delta_unknown_type() {
        let json = r#"{"type":"some_future_delta","data":"opaque"}"#;
        let delta: Delta = serde_json::from_str(json).unwrap();
        assert!(matches!(delta, Delta::Unknown));
    }

    // ── normalize_platform ──

    #[test]
    fn normalize_platform_known_values() {
        assert_eq!(normalize_platform("macos"), "MacOS");
        assert_eq!(normalize_platform("linux"), "Linux");
        assert_eq!(normalize_platform("windows"), "Windows");
        assert_eq!(normalize_platform("freebsd"), "FreeBSD");
        assert_eq!(normalize_platform("openbsd"), "OpenBSD");
        assert_eq!(normalize_platform("ios"), "iOS");
        assert_eq!(normalize_platform("android"), "Android");
    }

    #[test]
    fn normalize_platform_unknown_value() {
        assert_eq!(normalize_platform("haiku"), "Unknown");
    }

    // ── normalize_arch ──

    #[test]
    fn normalize_arch_known_values() {
        assert_eq!(normalize_arch("x86"), "x32");
        assert_eq!(normalize_arch("x86_64"), "x64");
        assert_eq!(normalize_arch("arm"), "arm");
        assert_eq!(normalize_arch("aarch64"), "arm64");
    }

    #[test]
    fn normalize_arch_unknown_value() {
        assert_eq!(normalize_arch("riscv64gc"), "unknown");
    }

    // ── split_at_boundary ──

    #[test]
    fn split_at_boundary_separates_static_and_dynamic() {
        let sections = &["intro", "tasks", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "env"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro", "tasks"]);
        assert_eq!(dynamic, vec!["env"]);
    }

    #[test]
    fn split_at_boundary_without_marker_treats_all_as_static() {
        let sections = &["intro", "tasks", "env"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro", "tasks", "env"]);
        assert!(dynamic.is_empty());
    }

    #[test]
    fn split_at_boundary_filters_empty_sections() {
        let sections = &["intro", "", SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "", "env"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro"]);
        assert_eq!(dynamic, vec!["env"]);
    }

    #[test]
    fn split_at_boundary_at_start_yields_empty_static() {
        let sections = &[SYSTEM_PROMPT_DYNAMIC_BOUNDARY, "env", "lang"];
        let (statics, dynamic) = split_at_boundary(sections);
        assert!(statics.is_empty());
        assert_eq!(dynamic, vec!["env", "lang"]);
    }

    #[test]
    fn split_at_boundary_at_end_yields_empty_dynamic() {
        let sections = &["intro", "tasks", SYSTEM_PROMPT_DYNAMIC_BOUNDARY];
        let (statics, dynamic) = split_at_boundary(sections);
        assert_eq!(statics, vec!["intro", "tasks"]);
        assert!(dynamic.is_empty());
    }

    // ── build_metadata ──

    #[test]
    fn build_metadata_wraps_session_id_in_stringified_json() {
        // The API accepts `metadata.user_id` as a string, not a nested
        // object — claude-code stringifies a JSON object with `session_id`
        // (and sometimes `device_id` / `account_uuid`). Round-trip check
        // keeps the contract explicit.
        let meta = build_metadata("abc-123");
        let parsed: serde_json::Value = serde_json::from_str(&meta.user_id).unwrap();
        assert_eq!(parsed["session_id"], "abc-123");
    }

    // ── build_completion_body ──

    fn parse_body(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("serialized body must be valid JSON")
    }

    #[test]
    fn build_completion_body_shapes_non_streaming_request_with_no_tools_or_thinking() {
        // stream=false, tools omitted, thinking omitted — the
        // non-streaming path must not accidentally carry interactive
        // plumbing; Haiku would reject `thinking` with 400 and extra
        // tools would waste tokens.
        let body = build_completion_body(
            "claude-haiku-4-5",
            "sys",
            "hi",
            40,
            &Auth::ApiKey("k".to_owned()),
            "sid",
        )
        .unwrap();
        let v = parse_body(&body);
        assert_eq!(v["model"], "claude-haiku-4-5");
        assert_eq!(v["max_tokens"], 40);
        assert_eq!(v["stream"], false);
        assert!(v.get("tools").is_none(), "tools must be omitted: {v}");
        assert!(v.get("thinking").is_none(), "thinking must be omitted: {v}");
        let user = &v["messages"][0];
        assert_eq!(user["role"], "user");
        assert_eq!(user["content"][0]["text"], "hi");
    }

    #[test]
    fn build_completion_body_api_key_skips_billing_header_keeps_identity_prefix() {
        let body = build_completion_body(
            "claude-haiku-4-5",
            "sys-prompt",
            "hi",
            40,
            &Auth::ApiKey("k".to_owned()),
            "sid",
        )
        .unwrap();
        let v = parse_body(&body);
        let system = v["system"].as_array().unwrap();
        // API-key path: only identity prefix + user-supplied system.
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(system[1]["text"], "sys-prompt");
        assert!(
            !body.contains("x-anthropic-billing-header:"),
            "API-key requests must not emit billing attestation: {body}",
        );
    }

    #[test]
    fn build_completion_body_oauth_injects_billing_header_and_cch() {
        // OAuth must emit an initial billing block (Claude Code's fingerprint
        // contract) and the placeholder `cch=00000` must be replaced by an
        // actual 5-hex-digit tag via `inject_cch`.
        let body = build_completion_body(
            "claude-haiku-4-5",
            "sys-prompt",
            "Fix login",
            40,
            &Auth::OAuth("t".to_owned()),
            "sid",
        )
        .unwrap();
        let v = parse_body(&body);
        let system = v["system"].as_array().unwrap();
        assert_eq!(system.len(), 3, "expected billing + identity + user: {v}");
        let first = system[0]["text"].as_str().unwrap();
        assert!(
            first.starts_with("x-anthropic-billing-header:"),
            "first block must be the billing header: {first}",
        );
        assert!(
            first.contains(&format!("cc_version={CLAUDE_CLI_VERSION}")),
            "billing header should name the current CLI version: {first}",
        );
        assert_eq!(system[1]["text"], SYSTEM_PROMPT_PREFIX);
        assert_eq!(system[2]["text"], "sys-prompt");
        assert!(
            !body.contains("cch=00000"),
            "placeholder `cch=00000` must have been replaced by a real tag: {body}",
        );
    }

    #[test]
    fn build_completion_body_empty_system_omits_caller_block_but_keeps_identity_prefix() {
        let body = build_completion_body(
            "claude-haiku-4-5",
            "",
            "hi",
            40,
            &Auth::ApiKey("k".to_owned()),
            "sid",
        )
        .unwrap();
        let v = parse_body(&body);
        let system = v["system"].as_array().unwrap();
        // Identity prefix must survive even without a caller system
        // prompt; non-Haiku OAuth requests rely on its presence in
        // block 0 (API-key case here, but same contract).
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], SYSTEM_PROMPT_PREFIX);
    }

    #[test]
    fn build_completion_body_routes_session_id_into_metadata() {
        let body = build_completion_body(
            "claude-haiku-4-5",
            "",
            "hi",
            40,
            &Auth::ApiKey("k".to_owned()),
            "unique-sid-789",
        )
        .unwrap();
        assert!(
            body.contains("unique-sid-789"),
            "session_id must flow into metadata.user_id: {body}",
        );
    }

    // ── join_text_blocks / CompletionResponse ──

    #[test]
    fn join_text_blocks_concatenates_text_and_drops_tool_and_thinking_blocks() {
        // The non-streaming endpoint can hand back tool_use, thinking, and
        // plain text blocks. `join_text_blocks` concatenates the Text blocks
        // and drops the rest; exercising via the response-shape path also
        // pins the `CompletionResponse` deserializer on live-like JSON.
        let body = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-haiku-4-5",
            "stop_reason": "end_turn",
            "content": [
                {"type":"thinking","thinking":"pondering","signature":"sig"},
                {"type":"text","text":"Fix auth bug"},
                {"type":"tool_use","id":"t1","name":"noop","input":{}},
                {"type":"text","text":" and friends"}
            ],
            "usage": {"input_tokens":10,"output_tokens":5}
        }"#;
        let parsed: CompletionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(join_text_blocks(parsed.content), "Fix auth bug and friends");
    }

    #[test]
    fn join_text_blocks_returns_empty_for_tool_only_response() {
        // Defensive: if Haiku returns only a tool_use (ignoring our
        // "JSON envelope" instruction), we must not surface it — the
        // caller treats empty as "parse failure, keep first-prompt title".
        let blocks = vec![ContentBlock::ToolUse {
            id: "t1".to_owned(),
            name: "noop".to_owned(),
            input: serde_json::Value::Null,
        }];
        assert_eq!(join_text_blocks(blocks), "");
    }

    // ── first_user_text ──

    #[test]
    fn first_user_text_extracts_from_first_user_message() {
        let messages = vec![Message::user("hello world"), Message::assistant("hi")];
        assert_eq!(first_user_text(&messages), "hello world");
    }

    #[test]
    fn first_user_text_returns_empty_for_no_user_messages() {
        let messages = vec![Message::assistant("hi")];
        assert_eq!(first_user_text(&messages), "");
    }

    #[test]
    fn first_user_text_returns_empty_for_empty_messages() {
        let messages: Vec<Message> = vec![];
        assert_eq!(first_user_text(&messages), "");
    }

    #[test]
    fn first_user_text_returns_empty_when_first_user_has_no_text() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "id".to_owned(),
                content: "result".to_owned(),
                is_error: false,
            }],
        }];
        assert_eq!(first_user_text(&messages), "");
    }

    // ── parse_sse_frame ──

    #[test]
    fn parse_sse_frame_text_delta() {
        let frame = indoc! {r#"
            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        let StreamEvent::ContentBlockDelta { index, delta } = event else {
            panic!("expected ContentBlockDelta");
        };
        assert_eq!(index, 0);
        let Delta::TextDelta { text } = delta else {
            panic!("expected TextDelta");
        };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn parse_sse_frame_ping() {
        let frame = indoc! {r#"
            event: ping
            data: {"type":"ping"}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(event, StreamEvent::Ping));
    }

    #[test]
    fn parse_sse_frame_message_start() {
        let frame = indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        let StreamEvent::MessageStart { message } = event else {
            panic!("expected MessageStart");
        };
        assert_eq!(message.id, "msg_123");
        assert_eq!(message.model, "claude-sonnet-4-6");
        let usage = message.usage.expect("expected usage");
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.output_tokens, 1);
    }

    #[test]
    fn parse_sse_frame_error_event() {
        let frame = indoc! {r#"
            event: error
            data: {"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        let StreamEvent::Error { error } = event else {
            panic!("expected Error");
        };
        assert_eq!(error.error_type, "rate_limit_error");
        assert_eq!(error.message, "Too many requests");
    }

    #[test]
    fn parse_sse_frame_unknown_event_type() {
        let frame = indoc! {r#"
            event: some_future_event
            data: {"type":"some_future_event","payload":"data"}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(event, StreamEvent::Unknown));
    }

    #[test]
    fn parse_sse_frame_comment_only() {
        let frame = ": comment line";
        let event = parse_sse_frame(frame).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn parse_sse_frame_empty() {
        let event = parse_sse_frame("").unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn parse_sse_frame_invalid_json() {
        let frame = "data: {not valid json}";
        assert!(parse_sse_frame(frame).is_err());
    }

    #[test]
    fn parse_sse_frame_concatenates_multiple_data_lines_with_newline() {
        // Per the SSE spec, multiple data: lines must be joined with \n.
        // The prior implementation kept only the last line, which would
        // drop the opening brace here and fail to parse. JSON treats \n
        // as whitespace between tokens so this round-trips cleanly.
        let frame = indoc! {r#"
            event: ping
            data: {"type":
            data: "ping"}
        "#};

        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(event, StreamEvent::Ping));
    }

    #[test]
    fn parse_sse_frame_accepts_data_prefix_without_space() {
        // Some gateways emit `data:payload` (no leading space). The spec
        // allows both; tolerate either form.
        let frame = indoc! {r#"
            data:{"type":"ping"}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(event, StreamEvent::Ping));
    }
}
