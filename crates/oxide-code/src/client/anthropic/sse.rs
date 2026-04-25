//! SSE streaming pump and frame parser for the Anthropic Messages API.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use super::wire::StreamEvent;

/// Hard cap on the unterminated SSE frame buffer. A misbehaving upstream that
/// never emits `\n\n` would otherwise let `buf` grow without bound until OOM.
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn stream_sse(
    http: &reqwest::Client,
    url: &str,
    betas: String,
    body: String,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let response = http
        .post(url)
        .header("anthropic-beta", betas)
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await.unwrap_or_default();
        bail!(
            "{}",
            format_api_error(status, retry_after.as_deref(), &body)
        );
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

/// Builds an actionable error message for a non-2xx Anthropic API
/// response. The raw body is always appended as `details: {body}` so
/// debug context is preserved on every branch.
pub(super) fn format_api_error(
    status: reqwest::StatusCode,
    retry_after: Option<&str>,
    body: &str,
) -> String {
    let prefix = match status.as_u16() {
        401 => "Anthropic API rejected credentials (HTTP 401). Check ANTHROPIC_API_KEY, or run `claude` to refresh OAuth.".to_owned(),
        429 => match retry_after {
            Some(after) => format!("Anthropic API rate limited (HTTP 429); retry after {after}."),
            None => "Anthropic API rate limited (HTTP 429); retry after a short delay.".to_owned(),
        },
        529 => "Anthropic API overloaded (HTTP 529); this is transient — retry in a few seconds.".to_owned(),
        s if (500..600).contains(&s) => {
            format!("Anthropic API server error (HTTP {status}). Usually transient; retry.")
        }
        _ => format!("API error (HTTP {status})"),
    };
    format!("{prefix} details: {body}")
}

/// Parses a single SSE frame into a [`StreamEvent`].
///
/// Per the SSE spec, multiple `data:` lines concatenate with `\n`.
/// Anthropic currently emits single-line data, but we follow the spec
/// so a future multi-line payload doesn't silently lose everything
/// but the last line.
pub(super) fn parse_sse_frame(frame: &str) -> Result<Option<StreamEvent>> {
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
    use crate::client::anthropic::wire::Delta;

    // ── format_api_error ──

    #[test]
    fn format_api_error_401_names_both_auth_paths() {
        let msg = format_api_error(
            reqwest::StatusCode::UNAUTHORIZED,
            None,
            r#"{"error":"invalid_api_key"}"#,
        );
        assert!(
            msg.starts_with(
                "Anthropic API rejected credentials (HTTP 401). Check ANTHROPIC_API_KEY, or run `claude` to refresh OAuth."
            ),
            "401 prefix: {msg}",
        );
        assert!(msg.contains(r#"details: {"error":"invalid_api_key"}"#));
    }

    #[test]
    fn format_api_error_429_mentions_retry_after_when_present() {
        let with = format_api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, Some("42"), "rl");
        assert!(
            with.starts_with("Anthropic API rate limited (HTTP 429); retry after 42."),
            "429 with retry-after: {with}",
        );
        let without = format_api_error(reqwest::StatusCode::TOO_MANY_REQUESTS, None, "rl");
        assert!(
            without
                .starts_with("Anthropic API rate limited (HTTP 429); retry after a short delay."),
            "429 without retry-after: {without}",
        );
        assert!(with.contains("details: rl"));
        assert!(without.contains("details: rl"));
    }

    #[test]
    fn format_api_error_529_flags_overload_as_transient() {
        let status = reqwest::StatusCode::from_u16(529).unwrap();
        let msg = format_api_error(status, None, "overloaded");
        assert!(
            msg.starts_with(
                "Anthropic API overloaded (HTTP 529); this is transient — retry in a few seconds."
            ),
            "529 prefix: {msg}",
        );
        assert!(msg.contains("details: overloaded"));
    }

    #[test]
    fn format_api_error_5xx_uses_generic_server_branch() {
        let msg = format_api_error(reqwest::StatusCode::BAD_GATEWAY, None, "bad gw");
        assert!(
            msg.starts_with(
                "Anthropic API server error (HTTP 502 Bad Gateway). Usually transient; retry."
            ),
            "5xx prefix: {msg}",
        );
        assert!(msg.contains("details: bad gw"));
    }

    #[test]
    fn format_api_error_other_falls_back_to_generic_shape() {
        let msg = format_api_error(reqwest::StatusCode::BAD_REQUEST, None, "invalid");
        assert!(
            msg.starts_with("API error (HTTP 400 Bad Request)"),
            "generic prefix: {msg}",
        );
        assert!(msg.contains("details: invalid"));
    }

    // ── parse_sse_frame ──

    #[test]
    fn parse_sse_frame_text_delta() {
        let frame = indoc! {r#"
            event: content_block_delta
            data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
        "#};
        let StreamEvent::ContentBlockDelta { index, delta } =
            parse_sse_frame(frame).unwrap().unwrap()
        else {
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
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }

    #[test]
    fn parse_sse_frame_message_start() {
        let frame = indoc! {r#"
            event: message_start
            data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}
        "#};
        let StreamEvent::MessageStart { message } = parse_sse_frame(frame).unwrap().unwrap() else {
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
        let StreamEvent::Error { error } = parse_sse_frame(frame).unwrap().unwrap() else {
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
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Unknown,
        ));
    }

    #[test]
    fn parse_sse_frame_without_data_line_yields_none() {
        assert!(parse_sse_frame("").unwrap().is_none());
        assert!(parse_sse_frame(": comment line").unwrap().is_none());
    }

    #[test]
    fn parse_sse_frame_invalid_json_errors() {
        assert!(parse_sse_frame("data: {not valid json}").is_err());
    }

    #[test]
    fn parse_sse_frame_concatenates_multiple_data_lines_with_newline() {
        // Per the SSE spec, multiple data: lines join with \n. JSON
        // treats \n as token whitespace, so this round-trips cleanly.
        let frame = indoc! {r#"
            event: ping
            data: {"type":
            data: "ping"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }

    #[test]
    fn parse_sse_frame_accepts_data_prefix_without_space() {
        // Some gateways emit `data:payload` (no leading space).
        let frame = indoc! {r#"
            data:{"type":"ping"}
        "#};
        assert!(matches!(
            parse_sse_frame(frame).unwrap().unwrap(),
            StreamEvent::Ping,
        ));
    }
}
