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

        // Anthropic sends keepalive events at least every ~15 s during streaming,
        // so a 60 s read timeout catches slowloris-style dribble without false
        // positives on healthy streams. The connect timeout is separate and
        // tighter; the whole-request `timeout` is omitted because a single
        // assistant response can legitimately take several minutes.
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

    /// Build the `metadata.user_id` field as a stringified JSON object.
    fn build_metadata(&self) -> RequestMetadata {
        let user_id = serde_json::json!({ "session_id": self.session_id }).to_string();
        RequestMetadata { user_id }
    }
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
    // Byte buffer (not String) so a UTF-8 multibyte sequence split across
    // network chunks is reassembled intact. `String::from_utf8_lossy` on raw
    // chunks would inject U+FFFD at the boundary instead.
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading response stream")?;
        buf.extend_from_slice(&chunk);

        loop {
            if buf.len() > MAX_SSE_FRAME_BYTES {
                bail!(
                    "SSE frame buffer exceeded {MAX_SSE_FRAME_BYTES} bytes without \
                     a terminating blank line; upstream may be misbehaving"
                );
            }

            // SSE frames are terminated by a blank line (\n\n).
            let Some(end) = buf.windows(2).position(|w| w == b"\n\n") else {
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

            match parse_sse_frame(frame) {
                Ok(Some(event)) => {
                    if tx.send(Ok(event)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    // A single malformed frame should not poison the whole turn.
                    debug!("skipping malformed SSE frame: {e:#}");
                }
            }
        }
    }

    Ok(())
}

/// Parse a single SSE frame into a [`StreamEvent`].
///
/// SSE format:
///
/// ```text
/// event: content_block_delta
/// data: {"type":"content_block_delta", ...}
/// ```
///
/// Per the SSE spec, multiple `data:` lines in one frame are concatenated
/// with `\n`. Anthropic's protocol currently uses single-line data fields,
/// but supporting the spec defensively costs nothing and avoids silently
/// dropping all-but-the-last line if the format evolves.
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
