use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::{Auth, Config};
use crate::message::Message;
use crate::tool::ToolDefinition;

const API_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_BETA_HEADER: &str = "claude-code-20250219";
const INTERLEAVED_THINKING_BETA_HEADER: &str = "interleaved-thinking-2025-05-14";
const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Matches the installed Claude Code version.
const CLAUDE_CLI_VERSION: &str = "2.1.87";

/// System prompt prefix that identifies the client to the Anthropic API. Required
/// for OAuth tokens — without it, non-Haiku models return 429. Always sent
/// regardless of auth method for simplicity.
const SYSTEM_PROMPT_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// ── Request types ──

#[derive(Serialize)]
struct CreateMessageRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: &'a [Message],
    system: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ToolDefinition]>,
}

// ── SSE response types ──

#[expect(
    dead_code,
    reason = "fields are populated by serde and used in downstream matching"
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
    Text { text: String },
    ToolUse { id: String, name: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[expect(
    dead_code,
    reason = "fields populated by serde, defined for full SSE protocol coverage"
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

pub struct Client {
    http: reqwest::Client,
    config: Config,
}

impl Client {
    pub fn new(config: Config) -> Result<Self> {
        let mut headers = HeaderMap::new();

        let mut betas = vec![
            CLAUDE_CODE_BETA_HEADER,
            INTERLEAVED_THINKING_BETA_HEADER,
            CONTEXT_1M_BETA_HEADER,
        ];

        match &config.auth {
            Auth::ApiKey(key) => {
                headers.insert("x-api-key", HeaderValue::from_str(key)?);
            }
            Auth::OAuth(token) => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
                betas.push(OAUTH_BETA_HEADER);
            }
        }

        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        headers.insert("anthropic-beta", HeaderValue::from_str(&betas.join(","))?);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("claude-cli/{CLAUDE_CLI_VERSION} (external, cli)"))?,
        );
        headers.insert("x-app", HeaderValue::from_static("cli"));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self { http, config })
    }

    /// Stream a message response from the Anthropic API.
    ///
    /// Returns a channel receiver yielding [`StreamEvent`]s. The caller
    /// should recv events as they arrive for real-time output.
    pub fn stream_message(
        &self,
        messages: &[Message],
        system: Option<&str>,
        tools: &[ToolDefinition],
    ) -> Result<mpsc::Receiver<Result<StreamEvent>>> {
        let system_prompt = match system {
            Some(s) => format!("{SYSTEM_PROMPT_PREFIX}\n{s}"),
            None => SYSTEM_PROMPT_PREFIX.to_owned(),
        };

        let url = format!("{}/v1/messages", self.config.base_url);
        let body = serde_json::to_value(CreateMessageRequest {
            model: &self.config.model,
            max_tokens: self.config.max_tokens,
            messages,
            system: &system_prompt,
            stream: true,
            tools: (!tools.is_empty()).then_some(tools),
        })
        .context("failed to serialize request")?;

        let (tx, rx) = mpsc::channel(64);
        let http = self.http.clone();

        tokio::spawn(async move {
            let result = stream_sse(&http, &url, &body, &tx).await;
            if let Err(e) = result {
                let _ = tx.send(Err(e)).await;
            }
        });

        Ok(rx)
    }
}

async fn stream_sse(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let response = http.post(url).json(body).send().await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("API error (HTTP {status}): {body}");
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("error reading response stream")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // SSE frames are terminated by a blank line (\n\n).
        while let Some(end) = buf.find("\n\n") {
            let frame = buf[..end].to_owned();
            buf.drain(..end + 2);

            if let Some(event) = parse_sse_frame(&frame)?
                && tx.send(Ok(event)).await.is_err()
            {
                return Ok(());
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
fn parse_sse_frame(frame: &str) -> Result<Option<StreamEvent>> {
    let mut data = None;

    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("data: ") {
            data = Some(value);
        }
    }

    let Some(data) = data else {
        return Ok(None);
    };

    let event: StreamEvent =
        serde_json::from_str(data).with_context(|| format!("failed to parse SSE data: {data}"))?;

    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

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
}
