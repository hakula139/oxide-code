//! SSE streaming pump and frame parser for the Anthropic Messages API.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use super::transport::{AttemptError, MessagesRequest, post_messages, with_retry};
use super::wire::{ApiError, StreamEvent};

/// Hard cap. A misbehaving upstream that never ends a frame would otherwise grow until OOM.
#[cfg(not(test))]
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Tests use a smaller cap so the overflow path exercises without allocating 8 MiB per run.
#[cfg(test)]
const MAX_SSE_FRAME_BYTES: usize = 4 * 1024;

/// Drives the streaming `/v1/messages` request and forwards parsed [`StreamEvent`]s on `tx`,
/// retrying an attempt that fails before any content block arrives. Each retry forwards a
/// [`StreamEvent::Retrying`] notice first.
///
/// Returns `Ok(())` once `message_stop` or an in-band `error` event is forwarded, or when the
/// receiver drops (consumer cancellation is not an error). Returns `Err` for transport / HTTP
/// failures, a stream that ends before `message_stop`, and the frame-size guard. Single malformed
/// frames are logged and skipped so they do not poison the rest of the turn.
pub(super) async fn stream_sse(
    http: &reqwest::Client,
    request: &MessagesRequest,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    with_retry(
        |retry| async move {
            let response = post_messages(http, request, retry).await?;
            pump_events(response, tx).await
        },
        |notice| {
            _ = tx.try_send(Ok(StreamEvent::Retrying {
                notice: notice.clone(),
            }));
        },
    )
    .await
}

/// Forwards one attempt's events. Failures before the first content block stay retryable
/// because the consumer has seen nothing it would have to retract.
async fn pump_events(
    response: reqwest::Response,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<(), AttemptError> {
    let mut stream = response.bytes_stream();
    let mut frames = FrameDecoder::default();
    let mut content_started = false;
    let fail = |cause: &str, error: anyhow::Error, content_started: bool| {
        if content_started {
            AttemptError::Fatal(error)
        } else {
            AttemptError::retryable(cause, error)
        }
    };

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .context("error reading response stream")
            .map_err(|e| fail("Response stream failed", e, content_started))?;
        frames.push(&chunk);

        while let Some(frame) = frames.next_frame()? {
            let frame = match std::str::from_utf8(&frame) {
                Ok(s) => s,
                Err(e) => {
                    debug!("skipping invalid UTF-8 SSE frame: {e}");
                    continue;
                }
            };
            let event = match parse_sse_frame(frame) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(e) => {
                    debug!("skipping malformed SSE frame: {e:#}");
                    continue;
                }
            };

            match &event {
                StreamEvent::Error { error } if !content_started && is_transient(error) => {
                    return Err(AttemptError::retryable(
                        format!("Stream {}", error.error_type),
                        anyhow!("API error ({}): {}", error.error_type, error.message),
                    ));
                }
                StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockDelta { .. } => {
                    content_started = true;
                }
                _ => {}
            }
            let terminal = matches!(event, StreamEvent::MessageStop | StreamEvent::Error { .. });
            if tx.send(Ok(event)).await.is_err() || terminal {
                return Ok(());
            }
        }
    }

    Err(fail(
        "Response stream ended early",
        anyhow!("response stream ended before message_stop"),
        content_started,
    ))
}

/// In-band equivalents of HTTP 529 and 500.
fn is_transient(error: &ApiError) -> bool {
    matches!(error.error_type.as_str(), "overloaded_error" | "api_error")
}

/// Incremental SSE frame splitter. CRLF and lone CR line endings are normalized to LF as bytes
/// arrive, so a frame always ends at `\n\n`, and each search resumes where the last one stopped.
/// Works on bytes so a UTF-8 sequence split across chunks reassembles intact.
#[derive(Default)]
struct FrameDecoder {
    buf: Vec<u8>,
    /// Bytes already searched without finding a frame boundary.
    scanned: usize,
    /// The previous byte was a CR, so an immediately following LF belongs to the same line ending.
    after_cr: bool,
}

impl FrameDecoder {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.reserve(chunk.len());
        for &byte in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
            match byte {
                b'\n' if after_cr => {}
                b'\r' => self.buf.push(b'\n'),
                _ => self.buf.push(byte),
            }
        }
    }

    /// Returns the next complete frame without its terminating blank line.
    fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        // Back up one byte so a boundary straddling the previous search end is still found.
        let from = self.scanned.saturating_sub(1);
        let Some(offset) = self.buf[from..].windows(2).position(|w| w == b"\n\n") else {
            self.scanned = self.buf.len();
            if self.buf.len() > MAX_SSE_FRAME_BYTES {
                bail!(
                    "SSE frame buffer exceeded {MAX_SSE_FRAME_BYTES} bytes without a terminating \
                     blank line, so upstream may be misbehaving"
                );
            }
            return Ok(None);
        };
        let end = from + offset;
        self.scanned = 0;
        Ok(Some(self.buf.drain(..end + 2).take(end).collect()))
    }
}

/// Parses one SSE frame (lines up to a blank-line separator). Returns `Ok(None)` for frames with
/// no `data:` line (comments, heartbeats). Per the SSE spec, multiple `data:` lines concatenate
/// with `\n`. Both `data: payload` and `data:payload` (no leading space) are accepted because some
/// gateways drop the space.
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::wire::Delta;
    use crate::config::auth::AuthSource;

    // ── Fixtures ──

    const MESSAGE_START: &str = indoc! {r#"
        event: message_start
        data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5"}}

    "#};

    const TEXT_DELTA: &str = indoc! {r#"
        event: content_block_start
        data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

        event: content_block_delta
        data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}

    "#};

    const OVERLOADED: &str = indoc! {r#"
        event: error
        data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

    "#};

    const MESSAGE_STOP: &str = indoc! {r#"
        event: message_stop
        data: {"type":"message_stop"}

    "#};

    fn sse_response(body: impl Into<Vec<u8>>) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_bytes(body)
    }

    /// Serves `responses` in order, one request each, repeating the last one after that.
    async fn serve(responses: Vec<ResponseTemplate>) -> MockServer {
        let server = MockServer::start().await;
        let last = responses.len() - 1;
        for (i, response) in responses.into_iter().enumerate() {
            let mock = Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .respond_with(response);
            let mock = if i == last {
                mock
            } else {
                mock.up_to_n_times(1)
            };
            mock.mount(&server).await;
        }
        server
    }

    /// Streams one request, returning its result and every forwarded event.
    async fn run_stream(server: &MockServer) -> (Result<()>, Vec<StreamEvent>) {
        let request = MessagesRequest {
            url: format!("{}/v1/messages", server.uri()),
            betas: String::new(),
            session_id: "sid".to_owned(),
            body: "{}".to_owned(),
            auth: AuthSource::ApiKeyEnv,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let result = stream_sse(&reqwest::Client::new(), &request, &tx).await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event.expect("stream_sse forwards only Ok events"));
        }
        (result, events)
    }

    async fn request_count(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().len()
    }

    fn texts(events: &[StreamEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: Delta::TextDelta { text },
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    // ── stream_sse ──

    #[tokio::test]
    async fn stream_sse_splits_crlf_and_cr_framed_events() {
        // A plain `\n\n` splitter finds no frame in this body and surfaces nothing.
        let body = [MESSAGE_START, TEXT_DELTA, MESSAGE_STOP]
            .concat()
            .replace('\n', "\r\n")
            .replacen("\r\n\r\n", "\r\r", 1);
        let server = serve(vec![sse_response(body)]).await;

        let (result, events) = run_stream(&server).await;

        result.expect("CR / CRLF framed stream completes");
        assert!(
            matches!(events[0], StreamEvent::MessageStart { .. }),
            "{events:?}"
        );
        assert_eq!(texts(&events), ["Hi"]);
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn stream_sse_retries_failures_before_any_content() {
        // An in-band overload and a stream cut before content both retry. The consumer sees only
        // the pre-content `message_start` frames from the failed attempts.
        let server = serve(vec![
            sse_response([MESSAGE_START, OVERLOADED].concat()),
            sse_response(MESSAGE_START),
            sse_response([MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat()),
        ])
        .await;

        let (result, events) = run_stream(&server).await;

        result.expect("third attempt succeeds");
        assert_eq!(request_count(&server).await, 3);
        assert_eq!(texts(&events), ["Hi"]);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. })),
            "a retried overload is not forwarded: {events:?}",
        );
        let notices: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Retrying { notice } => Some(notice.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notices,
            [
                "Stream overloaded_error. Retrying in 1s (retry 1 of 5).",
                "Response stream ended early. Retrying in 1s (retry 2 of 5).",
            ],
        );
        let counts: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                r.headers["x-stainless-retry-count"]
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(counts, ["0", "1", "2"]);
    }

    #[tokio::test]
    async fn stream_sse_never_retries_after_content() {
        // Streamed text already reached the user, so an overload or a cut stream ends the call.
        for (body, expect_error_event) in [
            ([MESSAGE_START, TEXT_DELTA, OVERLOADED].concat(), true),
            ([MESSAGE_START, TEXT_DELTA].concat(), false),
        ] {
            let server = serve(vec![sse_response(body)]).await;

            let (result, events) = run_stream(&server).await;

            assert_eq!(request_count(&server).await, 1, "no retry");
            assert_eq!(texts(&events), ["Hi"]);
            let forwarded_error = events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { error } if error.error_type == "overloaded_error"));
            assert_eq!(forwarded_error, expect_error_event, "{events:?}");
            if expect_error_event {
                result.expect("an in-band error is forwarded for the consumer to report");
            } else {
                let err = result.expect_err("a cut stream fails");
                assert!(
                    format!("{err:#}").contains("ended before message_stop"),
                    "{err:#}",
                );
            }
        }
    }

    #[tokio::test]
    async fn stream_sse_gives_up_on_a_stream_that_never_finishes() {
        let server = serve(vec![sse_response(MESSAGE_START)]).await;

        let (result, _) = run_stream(&server).await;

        let err = result.expect_err("every attempt is cut");
        assert!(
            format!("{err:#}").contains("ended before message_stop"),
            "{err:#}"
        );
        assert!(request_count(&server).await > 1, "pre-content cuts retry");
    }

    #[tokio::test]
    async fn stream_sse_buffer_overflow_bails_without_retry() {
        // No frame terminator would grow the buffer unbounded, so the cap turns this into an
        // error. Retrying the same upstream would only repeat it.
        let server = serve(vec![sse_response("a".repeat(MAX_SSE_FRAME_BYTES + 1024))]).await;

        let (result, _) = run_stream(&server).await;

        let err = result.expect_err("expected overflow error");
        assert!(
            format!("{err:#}").contains("exceeded"),
            "overflow message: {err:#}"
        );
        assert_eq!(request_count(&server).await, 1);
    }

    #[tokio::test]
    async fn stream_sse_skips_unusable_frames_then_keeps_streaming() {
        // An invalid UTF-8 frame (0xC3 0x28 is a lone start byte) and a comment heartbeat with no
        // `data:` line are both skipped without poisoning the rest of the stream.
        let mut body: Vec<u8> = b"event: ping\ndata: \xc3\x28\n\n: heartbeat\n\n".to_vec();
        body.extend_from_slice(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        body.extend_from_slice(MESSAGE_STOP.as_bytes());
        let server = serve(vec![sse_response(body)]).await;

        let (result, events) = run_stream(&server).await;

        result.unwrap();
        assert!(
            matches!(
                events.as_slice(),
                [StreamEvent::Ping, StreamEvent::MessageStop]
            ),
            "only the valid frames surfaced: {events:?}",
        );
    }

    #[tokio::test]
    async fn stream_sse_succeeds_when_receiver_drops_before_send() {
        // Consumer cancellation must surface as graceful Ok(()), not an Err on the stream.
        let server = serve(vec![sse_response(MESSAGE_START.repeat(8))]).await;
        let request = MessagesRequest {
            url: format!("{}/v1/messages", server.uri()),
            betas: String::new(),
            session_id: "sid".to_owned(),
            body: "{}".to_owned(),
            auth: AuthSource::ApiKeyEnv,
        };
        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(1);
        drop(rx);

        let result = stream_sse(&reqwest::Client::new(), &request, &tx).await;

        assert!(matches!(result, Ok(())), "graceful shutdown: {result:?}");
    }

    // ── FrameDecoder ──

    fn drain_frames(decoder: &mut FrameDecoder) -> Vec<String> {
        std::iter::from_fn(|| decoder.next_frame().unwrap())
            .map(|f| String::from_utf8(f).unwrap())
            .collect()
    }

    #[test]
    fn frame_decoder_normalizes_line_endings_split_across_chunks() {
        let mut decoder = FrameDecoder::default();
        for chunk in ["data: a\r", "\n\r", "\ndata: b\r", "\r", "data: c\n\n"] {
            decoder.push(chunk.as_bytes());
        }
        assert_eq!(
            drain_frames(&mut decoder),
            ["data: a", "data: b", "data: c"]
        );
    }

    #[test]
    fn frame_decoder_finds_a_boundary_straddling_two_searches() {
        let mut decoder = FrameDecoder::default();
        decoder.push(b"data: a\n");
        assert_eq!(drain_frames(&mut decoder), Vec::<String>::new());
        decoder.push(b"\ndata: b");
        assert_eq!(drain_frames(&mut decoder), ["data: a"]);
        decoder.push(b"\n\n");
        assert_eq!(drain_frames(&mut decoder), ["data: b"]);
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
            data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}
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
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens: 1,
                    }),
                },
            } if id == "msg_123" && model == "claude-sonnet-5-5",
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
        // Multiple `data:` lines join with `\n`; JSON treats it as whitespace, so this round-trips.
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
