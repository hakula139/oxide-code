//! SSE streaming pump and frame parser for the Anthropic Messages API.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::debug;

use super::accumulator::ResponseAccumulator;
use super::transport::{AttemptError, MessagesRequest, post_messages, rate_limits, with_retry};
use super::wire::{ApiError, StreamEvent};
use crate::client::ResponseEvent;

/// Hard cap. A misbehaving upstream that never ends a frame would otherwise grow until OOM.
#[cfg(not(test))]
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Tests use a smaller cap so the overflow path exercises without allocating 8 MiB per run.
#[cfg(test)]
const MAX_SSE_FRAME_BYTES: usize = 4 * 1024;

/// Drives the streaming `/v1/messages` request and forwards its [`ResponseEvent`]s on `tx`,
/// retrying an attempt that fails before any content block arrives. Each retry forwards a
/// [`ResponseEvent::Retrying`] notice first. `model` is the id the request names, which the
/// reply's reasoning blocks record.
///
/// Returns `Ok(())` once the completed reply is forwarded or the receiver drops. Returns `Err`
/// for transport / HTTP failures, an in-band `error` event, a stream that ends before
/// `message_stop`, and the frame-size guard. Single malformed frames are logged and skipped so
/// they do not poison the rest of the turn.
pub(super) async fn stream_sse(
    http: &reqwest::Client,
    request: &MessagesRequest,
    model: &str,
    tx: &mpsc::Sender<Result<ResponseEvent>>,
) -> Result<()> {
    with_retry(
        |retry| async move {
            let response = post_messages(http, request, retry).await?;
            pump_events(response, model, tx).await
        },
        |notice| {
            _ = tx.try_send(Ok(ResponseEvent::Retrying(notice.to_string())));
        },
    )
    .await
}

/// Forwards one attempt's events, led by the account's rate limits when the response reports them.
/// Failures before the first content block stay retryable because the consumer has seen nothing it
/// would have to retract.
async fn pump_events(
    response: reqwest::Response,
    model: &str,
    tx: &mpsc::Sender<Result<ResponseEvent>>,
) -> Result<(), AttemptError> {
    if let Some(limits) = rate_limits(response.headers())
        && tx
            .send(Ok(ResponseEvent::RateLimits(limits)))
            .await
            .is_err()
    {
        return Ok(());
    }
    let mut stream = response.bytes_stream();
    let mut frames = FrameDecoder::default();
    let mut reply = ResponseAccumulator::new(model);
    let fail = |cause: &str, error: anyhow::Error, retryable: bool| {
        if retryable {
            AttemptError::retryable(cause, error)
        } else {
            AttemptError::Fatal(error)
        }
    };

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .context("error reading response stream")
            .map_err(|e| fail("Response stream failed", e, !reply.content_started()))?;
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

            let event = match reply.apply(event) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(error) => {
                    return Err(fail(
                        &format!("Stream {}", error.error_type),
                        anyhow!("API error ({}): {}", error.error_type, error.message),
                        !reply.content_started() && is_transient(&error),
                    ));
                }
            };
            let completed = matches!(event, ResponseEvent::Completed(_));
            if tx.send(Ok(event)).await.is_err() || completed {
                return Ok(());
            }
        }
    }

    Err(fail(
        "Response stream ended early",
        anyhow!("response stream ended before message_stop"),
        !reply.content_started(),
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
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use indoc::{formatdoc, indoc};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::testing::messages_request;
    use crate::client::anthropic::wire::Delta;
    use crate::client::{RateLimitWindow, RateLimits, Response, TokenUsage};
    use crate::message::ContentBlock;

    // ── Fixtures ──

    const MESSAGE_START: &str = indoc! {r#"
        event: message_start
        data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"output_tokens":0}}}

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

    const START_USAGE: ResponseEvent = ResponseEvent::Usage(TokenUsage::new(5, 0));

    fn sse_response(body: impl Into<Vec<u8>>) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_bytes(body)
    }

    /// `response` with a subscription's five-hour usage window.
    fn rate_limited(response: ResponseTemplate) -> ResponseTemplate {
        response
            .insert_header("anthropic-ratelimit-unified-5h-utilization", "0.42")
            .insert_header("anthropic-ratelimit-unified-5h-reset", "1760000000")
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

    /// HTTP/1.1 response streaming `body` under a declared length of `declared` bytes. Declaring
    /// more than `body` holds fails the client's body read once the connection closes.
    fn raw_sse_response(body: &str, declared: usize) -> String {
        formatdoc! {"
            HTTP/1.1 200 OK\r
            content-type: text/event-stream\r
            content-length: {declared}\r
            connection: close\r
            \r
            {body}"
        }
    }

    /// Answers one connection per response, in order, closing each once its response is sent.
    /// Returns the base URL.
    fn serve_raw(responses: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for response in responses {
                let (mut socket, _) = listener.accept().unwrap();
                read_request(&mut socket);
                socket.write_all(response.as_bytes()).unwrap();
            }
        });
        base_url
    }

    /// Reads a request through its `{}` body, so closing the socket after the reply sends no reset.
    fn read_request(socket: &mut TcpStream) {
        let mut request = Vec::new();
        let mut buf = [0; 1024];
        while !request.ends_with(b"\r\n\r\n{}") {
            let read = socket.read(&mut buf).unwrap();
            assert_ne!(read, 0, "the client closed before sending its request");
            request.extend_from_slice(&buf[..read]);
        }
    }

    /// Streams one request, returning its result and every forwarded event.
    async fn run_stream(base_url: &str) -> (Result<()>, Vec<ResponseEvent>) {
        let request = messages_request(format!("{base_url}/v1/messages"));
        let (tx, mut rx) = mpsc::channel(64);
        let result = stream_sse(&reqwest::Client::new(), &request, "claude-opus-5-5", &tx).await;
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

    fn texts(events: &[ResponseEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ResponseEvent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn hi_reply() -> ResponseEvent {
        ResponseEvent::Completed(Response {
            content: vec![ContentBlock::Text {
                text: "Hi".to_owned(),
            }],
            ..Response::default()
        })
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

        let (result, events) = run_stream(&server.uri()).await;

        result.expect("CR / CRLF framed stream completes");
        assert_eq!(
            events,
            [
                START_USAGE,
                ResponseEvent::Text("Hi".to_owned()),
                hi_reply()
            ],
        );
    }

    #[tokio::test]
    async fn stream_sse_retries_failures_before_any_content() {
        // An in-band overload and a stream cut before content both retry. The consumer sees only
        // the pre-content usage from the failed attempts, each replaced by the next attempt's.
        let server = serve(vec![
            sse_response([MESSAGE_START, OVERLOADED].concat()),
            sse_response(MESSAGE_START),
            sse_response([MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat()),
        ])
        .await;

        let (result, events) = run_stream(&server.uri()).await;

        result.expect("third attempt succeeds");
        assert_eq!(request_count(&server).await, 3);
        assert_eq!(
            events,
            [
                START_USAGE,
                ResponseEvent::Retrying(
                    "Stream overloaded_error. Retrying in 1s (retry 1 of 5).".to_owned()
                ),
                START_USAGE,
                ResponseEvent::Retrying(
                    "Response stream ended early. Retrying in 1s (retry 2 of 5).".to_owned()
                ),
                START_USAGE,
                ResponseEvent::Text("Hi".to_owned()),
                hi_reply(),
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
        for (body, expected) in [
            (
                [MESSAGE_START, TEXT_DELTA, OVERLOADED].concat(),
                "API error (overloaded_error): Overloaded",
            ),
            (
                [MESSAGE_START, TEXT_DELTA].concat(),
                "ended before message_stop",
            ),
        ] {
            let server = serve(vec![sse_response(body)]).await;

            let (result, events) = run_stream(&server.uri()).await;

            assert_eq!(request_count(&server).await, 1, "no retry");
            assert_eq!(texts(&events), ["Hi"]);
            let err = result.expect_err("the attempt fails");
            assert!(format!("{err:#}").contains(expected), "{err:#}");
        }
    }

    #[tokio::test]
    async fn stream_sse_failed_body_read_retries_only_before_content() {
        let cut = |body: &str| raw_sse_response(body, body.len() + 64);
        let full = [MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat();
        let base_url = serve_raw(vec![
            cut(MESSAGE_START),
            raw_sse_response(&full, full.len()),
        ]);

        let (result, events) = run_stream(&base_url).await;

        result.expect("the retry succeeds");
        assert_eq!(
            events,
            [
                START_USAGE,
                ResponseEvent::Retrying(
                    "Response stream failed. Retrying in 1s (retry 1 of 5).".to_owned()
                ),
                START_USAGE,
                ResponseEvent::Text("Hi".to_owned()),
                hi_reply(),
            ],
        );

        let base_url = serve_raw(vec![cut(&[MESSAGE_START, TEXT_DELTA].concat())]);

        let (result, events) = run_stream(&base_url).await;

        let err = result.expect_err("a failed read after content ends the call");
        assert!(
            format!("{err:#}").contains("error reading response stream"),
            "{err:#}",
        );
        assert_eq!(events, [START_USAGE, ResponseEvent::Text("Hi".to_owned())]);
    }

    #[tokio::test]
    async fn stream_sse_gives_up_on_a_stream_that_never_finishes() {
        let server = serve(vec![sse_response(MESSAGE_START)]).await;

        let (result, _) = run_stream(&server.uri()).await;

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

        let (result, _) = run_stream(&server.uri()).await;

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
        let mut body = indoc! {b"
            event: ping
            data: \xc3\x28

            : heartbeat

        "}
        .to_vec();
        body.extend_from_slice(MESSAGE_START.as_bytes());
        body.extend_from_slice(MESSAGE_STOP.as_bytes());
        let server = serve(vec![sse_response(body)]).await;

        let (result, events) = run_stream(&server.uri()).await;

        result.unwrap();
        assert_eq!(
            events,
            [START_USAGE, ResponseEvent::Completed(Response::default())],
            "only the valid frames surfaced",
        );
    }

    #[tokio::test]
    async fn stream_sse_leads_with_the_rate_limits_the_response_reports() {
        let server = serve(vec![rate_limited(sse_response(
            [MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat(),
        ))])
        .await;

        let (result, events) = run_stream(&server.uri()).await;

        result.expect("stream completes");
        assert_eq!(
            events,
            [
                ResponseEvent::RateLimits(RateLimits {
                    five_hour: Some(RateLimitWindow {
                        utilization: 0.42,
                        resets_at: 1_760_000_000,
                    }),
                    seven_day: None,
                }),
                START_USAGE,
                ResponseEvent::Text("Hi".to_owned()),
                hi_reply(),
            ],
        );
    }

    #[tokio::test]
    async fn stream_sse_succeeds_when_receiver_drops_before_send() {
        // Consumer cancellation must end the stream with a graceful Ok(()).
        for response in [
            sse_response(MESSAGE_START.repeat(8)),
            rate_limited(sse_response(MESSAGE_START)),
        ] {
            let server = serve(vec![response]).await;
            let request = messages_request(format!("{}/v1/messages", server.uri()));
            let (tx, rx) = mpsc::channel::<Result<ResponseEvent>>(1);
            drop(rx);

            let result =
                stream_sse(&reqwest::Client::new(), &request, "claude-opus-5-5", &tx).await;

            assert!(matches!(result, Ok(())), "graceful shutdown: {result:?}");
        }
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
        // `data:` lines join with `\n`, which JSON reads as whitespace, so this round-trips.
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
