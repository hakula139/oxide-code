//! SSE streaming pump and frame parser for the Anthropic Messages API.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use super::wire::StreamEvent;

/// Hard cap — a misbehaving upstream that never emits `\n\n` would otherwise grow until OOM.
#[cfg(not(test))]
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Tests use a smaller cap so the overflow path exercises without allocating 8 MiB per run.
#[cfg(test)]
const MAX_SSE_FRAME_BYTES: usize = 4 * 1024;

pub(super) async fn stream_sse(
    http: &reqwest::Client,
    url: &str,
    betas: String,
    session_id: String,
    body: String,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let response = http
        .post(url)
        .header("anthropic-beta", betas)
        .header("x-claude-code-session-id", session_id)
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
    // Byte buffer reassembles UTF-8 split across chunk boundaries (lossy would inject U+FFFD).
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

/// Builds an actionable error message for a non-2xx Anthropic API response.
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
/// Per the SSE spec, multiple `data:` lines concatenate with `\n`. Anthropic currently emits
/// single-line data, but we follow the spec so multi-line payloads round-trip correctly.
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

    // ── stream_sse ──

    #[tokio::test]
    async fn stream_sse_buffer_overflow_bails_when_frame_lacks_terminator() {
        // An upstream that emits a long byte string with no `\n\n`
        // separator would let the buffer grow unbounded; the cap turns
        // it into an actionable error instead of an OOM.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = "a".repeat(MAX_SSE_FRAME_BYTES + 1024);
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let (tx, mut rx) = mpsc::channel::<Result<StreamEvent>>(1);
        let url = format!("{}/messages", server.uri());
        let err = stream_sse(
            &http,
            &url,
            String::new(),
            "sid-test".to_owned(),
            "{}".to_owned(),
            &tx,
        )
        .await
        .expect_err("expected overflow error");
        assert!(
            format!("{err:#}").contains("exceeded"),
            "overflow message: {err:#}",
        );
        // Drain pending events to silence any unused-receiver warnings.
        rx.close();
    }

    #[tokio::test]
    async fn stream_sse_skips_invalid_utf8_frame_then_keeps_streaming() {
        // A chunk with non-UTF-8 bytes between SSE separators must be
        // skipped (not surfaced) so a single corrupted frame can't poison
        // the stream — every other frame after it must still deliver.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // 0xC3 0x28 is an invalid UTF-8 sequence (lone start byte).
        // The valid follow-up frame must still parse to Ping.
        let mut body: Vec<u8> = b"event: ping\ndata: \xc3\x28\n\n".to_vec();
        body.extend_from_slice(
            indoc! {r#"
                event: ping
                data: {"type":"ping"}

            "#}
            .as_bytes(),
        );
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let (tx, mut rx) = mpsc::channel::<Result<StreamEvent>>(8);
        let url = format!("{}/messages", server.uri());
        stream_sse(
            &http,
            &url,
            String::new(),
            "sid-test".to_owned(),
            "{}".to_owned(),
            &tx,
        )
        .await
        .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        assert_eq!(events.len(), 1, "only the valid frame surfaced: {events:?}");
        assert!(matches!(events[0], StreamEvent::Ping));
    }

    #[tokio::test]
    async fn stream_sse_handles_data_less_frames_without_emitting_event() {
        // A frame with only an `event:` (or comment) line and no `data:`
        // line parses to `Ok(None)`; the loop must consume it silently
        // and continue — Anthropic occasionally emits comment heartbeats.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = indoc! {r#"
            : heartbeat comment

            event: ping
            data: {"type":"ping"}

        "#};
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let (tx, mut rx) = mpsc::channel::<Result<StreamEvent>>(8);
        let url = format!("{}/messages", server.uri());
        stream_sse(
            &http,
            &url,
            String::new(),
            "sid-test".to_owned(),
            "{}".to_owned(),
            &tx,
        )
        .await
        .unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.unwrap());
        }
        assert_eq!(events.len(), 1, "comment skipped, ping kept: {events:?}");
        assert!(matches!(events[0], StreamEvent::Ping));
    }

    #[tokio::test]
    async fn stream_sse_succeeds_when_receiver_drops_before_send() {
        // Consumer cancellation closes the channel; the next tx.send
        // call surfaces an Err which stream_sse must treat as graceful
        // shutdown (Ok(())), not propagate as a stream failure.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = indoc! {r#"
            event: ping
            data: {"type":"ping"}

        "#}
        .repeat(8);
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(1);
        drop(rx);
        let url = format!("{}/messages", server.uri());
        let result = stream_sse(
            &http,
            &url,
            String::new(),
            "sid-test".to_owned(),
            "{}".to_owned(),
            &tx,
        )
        .await;
        assert!(matches!(result, Ok(())), "graceful shutdown: {result:?}");
    }

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
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::TextDelta { text },
            } if text == "Hello",
        ));
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
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::MessageStart {
                message: super::super::wire::MessageResponse {
                    ref id,
                    ref model,
                    usage: Some(super::super::wire::Usage {
                        input_tokens: 25,
                        output_tokens: 1,
                    }),
                },
            } if id == "msg_123" && model == "claude-sonnet-4-6",
        ));
    }

    #[test]
    fn parse_sse_frame_error_event() {
        let frame = indoc! {r#"
            event: error
            data: {"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}
        "#};
        let event = parse_sse_frame(frame).unwrap().unwrap();
        assert!(matches!(
            event,
            StreamEvent::Error {
                error: super::super::wire::ApiError {
                    ref error_type,
                    ref message,
                },
            } if error_type == "rate_limit_error" && message == "Too many requests",
        ));
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
