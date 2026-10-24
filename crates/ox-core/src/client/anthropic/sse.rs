//! SSE streaming pump and frame parser for the Anthropic Messages API.

use std::borrow::Cow;

use anyhow::{Context as _, Result, anyhow, bail};
use futures::StreamExt as _;
use tokio::sync::mpsc;

use super::accumulator::{ReplySpec, ResponseAccumulator};
use super::transport::{
    AttemptError, MessagesRequest, format_stream_error, post_messages, rate_limits, with_retry,
};
use super::wire::{ApiError, StreamEvent};
use crate::client::ResponseEvent;

/// Events a stream may queue ahead of its consumer. The SSE pump waits while the queue is full, but
/// drops a retry notice so the retry never waits on the consumer.
const STREAM_BUFFER: usize = 64;

/// A misbehaving upstream that never ends a frame would otherwise grow until OOM.
const MAX_SSE_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Sends `request` on a background task and returns the receiver for its [`ResponseEvent`]s. A
/// failure arrives as the last item, and dropping the receiver cancels the request.
pub(super) fn spawn_stream(
    http: reqwest::Client,
    request: MessagesRequest,
    spec: ReplySpec,
) -> mpsc::Receiver<Result<ResponseEvent>> {
    let (tx, rx) = mpsc::channel(STREAM_BUFFER);
    tokio::spawn(forward_sse(http, request, spec, tx));
    rx
}

/// Runs [`stream_sse`] until it ends or the receiver drops, then forwards any failure on `tx`.
/// Racing `closed` stops an in-flight request or retry backoff once the consumer is gone.
async fn forward_sse(
    http: reqwest::Client,
    request: MessagesRequest,
    spec: ReplySpec,
    tx: mpsc::Sender<Result<ResponseEvent>>,
) {
    tokio::select! {
        result = stream_sse(&http, &request, &spec, &tx) => {
            if let Err(e) = result {
                _ = tx.send(Err(e)).await;
            }
        }
        () = tx.closed() => {}
    }
}

/// Drives the streaming `/v1/messages` request and forwards its [`ResponseEvent`]s on `tx`,
/// retrying an attempt that fails before any content block arrives. Each retry forwards a
/// [`ResponseEvent::Retrying`] notice first.
///
/// Returns `Ok(())` once the completed reply is forwarded or the receiver drops. Returns `Err` for
/// transport / HTTP failures, an in-band `error` event, a stream that ends before `message_stop`,
/// invalid stream data, and the frame-size guard.
async fn stream_sse(
    http: &reqwest::Client,
    request: &MessagesRequest,
    spec: &ReplySpec,
    tx: &mpsc::Sender<Result<ResponseEvent>>,
) -> Result<()> {
    with_retry(
        |retry| async move {
            let response = post_messages(http, request, retry).await?;
            pump_events(response, spec, tx).await
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
    spec: &ReplySpec,
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
    let mut frames = FrameDecoder::new(MAX_SSE_FRAME_BYTES);
    let mut reply = ResponseAccumulator::new(spec);
    let fail = |cause: &str, error: anyhow::Error, retryable: bool| {
        if retryable {
            AttemptError::retryable(cause, error)
        } else {
            AttemptError::Fatal(error)
        }
    };

    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .context("Anthropic API stream failed")
            .map_err(|e| fail("Anthropic API stream failed", e, !reply.content_started()))?;
        frames.push(&chunk);

        while let Some(frame) = frames.next_frame().context("Anthropic API stream failed")? {
            let Some(event) = parse_sse_frame(&frame)
                .context("Anthropic API stream failed")
                .map_err(|e| fail("Anthropic API stream failed", e, !reply.content_started()))?
            else {
                continue;
            };

            let event = match reply.apply(event) {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(error) => {
                    return Err(fail(
                        &format!("Anthropic API stream {}", error.error_type),
                        anyhow!(format_stream_error(&error.error_type, &error.message)),
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
        "Anthropic API stream ended early",
        anyhow!("Anthropic API stream ended before message_stop"),
        !reply.content_started(),
    ))
}

/// Returns whether an in-band error is the equivalent of HTTP 529 or 500.
fn is_transient(error: &ApiError) -> bool {
    matches!(error.error_type.as_str(), "overloaded_error" | "api_error")
}

/// Incremental SSE frame splitter. CRLF and lone CR line endings are normalized to LF as bytes
/// arrive, so a frame always ends at `\n\n`, and each search resumes where the last one stopped.
/// Works on bytes so a UTF-8 sequence split across chunks reassembles intact.
struct FrameDecoder {
    buf: Vec<u8>,
    /// Bytes already searched without finding a frame boundary.
    scanned: usize,
    /// Whether the previous byte was a CR, which makes an immediately following LF part of its line
    /// ending.
    after_cr: bool,
    /// Longest unterminated frame to buffer before failing.
    max_frame_bytes: usize,
}

impl FrameDecoder {
    fn new(max_frame_bytes: usize) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            after_cr: false,
            max_frame_bytes,
        }
    }

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
            if self.buf.len() > self.max_frame_bytes {
                bail!(
                    "unterminated SSE frame exceeded {} bytes",
                    self.max_frame_bytes
                );
            }
            return Ok(None);
        };
        let end = from + offset;
        self.scanned = 0;
        Ok(Some(self.buf.drain(..end + 2).take(end).collect()))
    }
}

/// Parses one UTF-8 SSE frame (lines up to a blank-line separator). Frames without a `data:` line
/// are ignored unless named `error`. Multiple `data:` lines concatenate with `\n` per the SSE spec.
/// Both `data: payload` and `data:payload` are accepted because some gateways drop the space.
fn parse_sse_frame(frame: &[u8]) -> Result<Option<StreamEvent>> {
    let frame = std::str::from_utf8(frame).context("invalid UTF-8 SSE frame")?;
    let mut event_name = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in frame.lines() {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => event_name = Some(value),
            "data" => data_lines.push(value),
            _ => {}
        }
    }

    let data: Cow<'_, str> = match data_lines.as_slice() {
        [] if event_name == Some("error") => bail!("SSE error event has no data"),
        [] => return Ok(None),
        [one] => Cow::Borrowed(*one),
        _ => Cow::Owned(data_lines.join("\n")),
    };

    let event: StreamEvent =
        serde_json::from_str(&data).with_context(|| format!("failed to parse SSE data: {data}"))?;

    if let Some(name) = event_name.filter(|name| !matches!(*name, "" | "message")) {
        let payload: serde_json::Value = serde_json::from_str(&data)?;
        if payload["type"] != name {
            bail!(
                "SSE event {name:?} does not match data type {}",
                payload["type"]
            );
        }
    }

    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::time::Duration;

    use indoc::{formatdoc, indoc};
    use wiremock::ResponseTemplate;

    use super::*;
    use crate::client::anthropic::local_testing::{
        messages_request, plain_http, request_count, retry_counts, serve,
    };
    use crate::client::anthropic::testing::{sse_body, sse_response};
    use crate::client::anthropic::wire::{Delta, MessageResponse};
    use crate::client::{RateLimitWindow, RateLimits, Response};
    use crate::message::ContentBlock;
    use crate::model::TokenUsage;

    const MESSAGE_START: &str = indoc! {r#"
        event: message_start
        data: {"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5-5","usage":{"input_tokens":5,"output_tokens":0}}}

    "#};

    const START_USAGE: ResponseEvent = ResponseEvent::Usage(TokenUsage::new(5, 0));

    fn opus_spec() -> ReplySpec {
        ReplySpec {
            model: "claude-opus-5-5".to_owned(),
            thinking_text: true,
        }
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

    mod forward_sse {
        use super::*;

        /// Answers one connection with `body` under a longer declared length, then holds the
        /// connection open until the client hangs up, like a stream stalled mid-reply. Returns the
        /// base URL.
        fn serve_stalled(body: &str) -> String {
            let response = raw_sse_response(body, body.len() + 1);
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                read_request(&mut socket);
                socket.write_all(response.as_bytes()).unwrap();
                while socket.read(&mut [0; 64]).is_ok_and(|read| read > 0) {}
            });
            base_url
        }

        #[tokio::test(start_paused = true)]
        async fn ends_a_stalled_stream_once_the_receiver_drops() {
            let request = messages_request(format!("{}/v1/messages", serve_stalled(MESSAGE_START)));
            let (tx, mut rx) = mpsc::channel(STREAM_BUFFER);
            let forwarding = tokio::spawn(forward_sse(plain_http(), request, opus_spec(), tx));
            let first = rx.recv().await.map(Result::unwrap);

            drop(rx);
            let ended = tokio::time::timeout(Duration::from_secs(1), forwarding).await;

            assert_eq!(first, Some(START_USAGE));
            ended
                .expect("forward_sse ends once the receiver drops")
                .unwrap();
        }
    }

    mod stream_sse {
        use super::*;

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

        /// `response` with a subscription's five-hour usage window.
        fn rate_limited(response: ResponseTemplate) -> ResponseTemplate {
            response
                .insert_header("anthropic-ratelimit-unified-5h-utilization", "0.42")
                .insert_header("anthropic-ratelimit-unified-5h-reset", "1760000000")
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

        /// Streams one request, returning its result and every forwarded event.
        async fn run_stream(base_url: &str) -> (Result<()>, Vec<ResponseEvent>) {
            let request = messages_request(format!("{base_url}/v1/messages"));
            let (tx, mut rx) = mpsc::channel(STREAM_BUFFER);
            let result = stream_sse(&plain_http(), &request, &opus_spec(), &tx).await;
            drop(tx);
            let mut events = Vec::new();
            while let Some(event) = rx.recv().await {
                events.push(event.expect("stream_sse forwards only Ok events"));
            }
            (result, events)
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
                content: vec![ContentBlock::text("Hi")],
                ..Response::default()
            })
        }

        #[tokio::test]
        async fn splits_crlf_and_cr_framed_events() {
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

        #[tokio::test(start_paused = true)]
        async fn retries_failures_before_any_content() {
            // An in-band overload and a stream cut before content both retry. The consumer sees
            // only the pre-content usage of each failed attempt, replaced by the next attempt's.
            let server = serve(vec![
                sse_response([MESSAGE_START, OVERLOADED].concat()),
                sse_response(MESSAGE_START),
                sse_response([MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat()),
            ])
            .await;

            let (result, events) = run_stream(&server.uri()).await;

            result.expect("third attempt succeeds");
            assert_eq!(
                events,
                [
                    START_USAGE,
                    ResponseEvent::Retrying(
                        "Anthropic API stream overloaded_error. Retrying in 2 s (retry 1 of 5)."
                            .to_owned()
                    ),
                    START_USAGE,
                    ResponseEvent::Retrying(
                        "Anthropic API stream ended early. Retrying in 3 s (retry 2 of 5)."
                            .to_owned()
                    ),
                    START_USAGE,
                    ResponseEvent::Text("Hi".to_owned()),
                    hi_reply(),
                ],
            );
            let counts = retry_counts(&server).await;
            assert_eq!(counts, ["0", "1", "2"]);
        }

        #[tokio::test]
        async fn never_retries_after_content() {
            // Streamed text already reached the user, so an overload or a cut stream ends the call.
            for (body, expected) in [
                (
                    [MESSAGE_START, TEXT_DELTA, OVERLOADED].concat(),
                    "Anthropic API overloaded (overloaded_error): Overloaded. \
                     Retry in a few seconds.",
                ),
                (
                    [MESSAGE_START, TEXT_DELTA].concat(),
                    "Anthropic API stream ended before message_stop",
                ),
            ] {
                let server = serve(vec![sse_response(body)]).await;

                let (result, events) = run_stream(&server.uri()).await;

                assert_eq!(request_count(&server).await, 1, "no retry");
                assert_eq!(texts(&events), ["Hi"]);
                let err = result.expect_err("the attempt fails");
                assert_eq!(format!("{err:#}"), expected);
            }
        }

        #[tokio::test(start_paused = true)]
        async fn failed_body_read_retries_only_before_content() {
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
                        "Anthropic API stream failed. Retrying in 2 s (retry 1 of 5).".to_owned()
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
                format!("{err:#}").starts_with("Anthropic API stream failed: "),
                "{err:#}"
            );
            assert_eq!(events, [START_USAGE, ResponseEvent::Text("Hi".to_owned())]);
        }

        #[tokio::test(start_paused = true)]
        async fn gives_up_on_a_stream_that_never_finishes() {
            let server = serve(vec![sse_response(MESSAGE_START)]).await;

            let (result, _) = run_stream(&server.uri()).await;

            let err = result.expect_err("every attempt is cut");
            assert_eq!(
                format!("{err:#}"),
                "Anthropic API stream ended before message_stop"
            );
            assert_eq!(retry_counts(&server).await, ["0", "1", "2", "3", "4", "5"]);
        }

        #[tokio::test]
        async fn buffer_overflow_bails_without_retry() {
            // No frame terminator would grow the buffer unbounded, so the cap turns this into an
            // error. Retrying the same upstream would only repeat it.
            let server = serve(vec![sse_response("a".repeat(MAX_SSE_FRAME_BYTES + 1024))]).await;

            let (result, _) = run_stream(&server.uri()).await;

            let err = result.expect_err("expected overflow error");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "Anthropic API stream failed: \
                     unterminated SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes"
                ),
            );
            assert_eq!(request_count(&server).await, 1);
        }

        #[tokio::test]
        async fn skips_comments_heartbeats_and_unknown_event_types() {
            let ignored = indoc! {r#"
                event: ping
                data: {"type":"ping"}

                : heartbeat

                event: future_event
                data: {"type":"future_event","payload":"opaque"}

            "#};
            let body = [MESSAGE_START, TEXT_DELTA, ignored, MESSAGE_STOP].concat();
            let server = serve(vec![sse_response(body)]).await;

            let (result, events) = run_stream(&server.uri()).await;

            result.unwrap();
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
        async fn corrupt_frames_fail_without_retry_or_completion() {
            for (frame, expected) in [
                (b"data: \xc3\x28\n\n".to_vec(), "invalid UTF-8 SSE frame"),
                (b"data: {broken}\n\n".to_vec(), "failed to parse SSE data"),
                (
                    sse_body(&[serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": 42}})]).into_bytes(),
                    "failed to parse SSE data",
                ),
                (
                    sse_body(&[serde_json::json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1"}})]).into_bytes(),
                    "failed to parse SSE data",
                ),
                (
                    indoc! {r#"
                        event: error
                        data: {"type":"overloaded_error","message":"Overloaded"}

                    "#}.as_bytes().to_vec(),
                    "SSE event \"error\" does not match data type",
                ),
                (
                    indoc! {r#"
                        event: error
                        data: {"type":"message_stop"}

                    "#}.as_bytes().to_vec(),
                    "SSE event \"error\" does not match data type",
                ),
                (
                    indoc! {r#"
                        event: content_block_delta
                        data: {"type":"future_event","index":0,"delta":{"type":"input_json_delta","partial_json":42}}

                    "#}.as_bytes().to_vec(),
                    "SSE event \"content_block_delta\" does not match data type",
                ),
                (b"event: error\n\n".to_vec(), "SSE error event has no data"),
            ] {
                let body = [MESSAGE_START.as_bytes(), TEXT_DELTA.as_bytes(), &frame, MESSAGE_STOP.as_bytes()].concat();
                let server = serve(vec![sse_response(body)]).await;

                let (result, events) = run_stream(&server.uri()).await;

                let error = result.expect_err("corrupt data fails the attempt");
                assert!(format!("{error:#}").contains(expected), "{error:#}");
                assert_eq!(events, [START_USAGE, ResponseEvent::Text("Hi".to_owned())]);
                assert_eq!(request_count(&server).await, 1, "content prevents retry");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn retries_corrupt_data_before_content() {
            for frame in [
                &b"data: \xc3\x28\n\n"[..],
                &b"data: {broken}\n\n"[..],
                indoc! {r#"
                    event: content_block_delta
                    data: {"type":"future_event"}

                "#}
                .as_bytes(),
            ] {
                let body = [MESSAGE_START.as_bytes(), frame, MESSAGE_STOP.as_bytes()].concat();
                let server = serve(vec![
                    sse_response(body),
                    sse_response([MESSAGE_START, TEXT_DELTA, MESSAGE_STOP].concat()),
                ])
                .await;

                let (result, events) = run_stream(&server.uri()).await;
                result.expect("the fresh attempt succeeds");

                assert_eq!(
                    events,
                    [
                        START_USAGE,
                        ResponseEvent::Retrying(
                            "Anthropic API stream failed. Retrying in 2 s (retry 1 of 5)."
                                .to_owned()
                        ),
                        START_USAGE,
                        ResponseEvent::Text("Hi".to_owned()),
                        hi_reply(),
                    ],
                );
                assert_eq!(retry_counts(&server).await, ["0", "1"]);
            }
        }

        #[tokio::test]
        async fn leads_with_the_rate_limits_the_response_reports() {
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
        async fn succeeds_when_receiver_drops_before_send() {
            // Consumer cancellation must end the stream with a graceful Ok(()).
            for response in [
                sse_response(MESSAGE_START.repeat(8)),
                rate_limited(sse_response(MESSAGE_START)),
            ] {
                let server = serve(vec![response]).await;
                let request = messages_request(format!("{}/v1/messages", server.uri()));
                let (tx, rx) = mpsc::channel::<Result<ResponseEvent>>(1);
                drop(rx);

                let result = stream_sse(&plain_http(), &request, &opus_spec(), &tx).await;

                assert!(matches!(result, Ok(())), "graceful shutdown: {result:?}");
            }
        }
    }

    mod frame_decoder {
        use super::*;

        fn drain_frames(decoder: &mut FrameDecoder) -> Vec<String> {
            std::iter::from_fn(|| decoder.next_frame().unwrap())
                .map(|f| String::from_utf8(f).unwrap())
                .collect()
        }

        #[test]
        fn normalizes_line_endings_split_across_chunks() {
            let mut decoder = FrameDecoder::new(MAX_SSE_FRAME_BYTES);
            for chunk in ["data: a\r", "\n\r", "\ndata: b\r", "\r", "data: c\n\n"] {
                decoder.push(chunk.as_bytes());
            }
            assert_eq!(
                drain_frames(&mut decoder),
                ["data: a", "data: b", "data: c"]
            );
        }

        #[test]
        fn finds_a_boundary_straddling_two_searches() {
            let mut decoder = FrameDecoder::new(MAX_SSE_FRAME_BYTES);
            decoder.push(b"data: a\n");
            assert_eq!(drain_frames(&mut decoder), Vec::<String>::new());
            decoder.push(b"\ndata: b");
            assert_eq!(drain_frames(&mut decoder), ["data: a"]);
            decoder.push(b"\n\n");
            assert_eq!(drain_frames(&mut decoder), ["data: b"]);
        }

        #[test]
        fn fails_once_an_unterminated_frame_outgrows_its_cap() {
            let mut decoder = FrameDecoder::new(8);
            decoder.push(b"data: ab");
            assert_eq!(drain_frames(&mut decoder), Vec::<String>::new());

            decoder.push(b"c");
            let err = decoder
                .next_frame()
                .expect_err("9 bytes outgrow the 8-byte cap");

            assert_eq!(err.to_string(), "unterminated SSE frame exceeded 8 bytes");
        }
    }

    mod parse_sse_frame {
        use super::*;

        #[test]
        fn text_delta_keeps_its_block_index_and_text() {
            let frame = indoc! {r#"
                event: content_block_delta
                data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(
                event,
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: Delta::TextDelta {
                        text: "Hello".to_owned(),
                    },
                }
            );
        }

        #[test]
        fn ping_parses_from_its_data_line() {
            let frame = indoc! {r#"
                event: ping
                data: {"type":"ping"}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(event, StreamEvent::Ping);
        }

        #[test]
        fn message_start_carries_the_initial_usage() {
            let frame = indoc! {r#"
                event: message_start
                data: {"type":"message_start","message":{"id":"msg_123","type":"message","role":"assistant","content":[],"model":"claude-sonnet-5-5","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(
                event,
                StreamEvent::MessageStart {
                    message: MessageResponse {
                        usage: Some(TokenUsage::new(25, 1)),
                    },
                }
            );
        }

        #[test]
        fn error_event_carries_the_api_error() {
            let frame = indoc! {r#"
                event: error
                data: {"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(
                event,
                StreamEvent::Error {
                    error: ApiError {
                        error_type: "rate_limit_error".to_owned(),
                        message: "Too many requests".to_owned(),
                    },
                }
            );
        }

        #[test]
        fn unknown_event_type_parses_for_forward_compat() {
            let frame = indoc! {r#"
                event: some_future_event
                data: {"type":"some_future_event","payload":"data"}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(event, StreamEvent::Unknown);
        }

        #[test]
        fn concatenates_multiple_data_lines_with_newline() {
            // `data:` lines join with `\n`, which JSON reads as whitespace, so this round-trips.
            let frame = indoc! {r#"
                event: ping
                data: {"type":
                data: "ping"}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(event, StreamEvent::Ping);
        }

        #[test]
        fn accepts_data_prefix_without_space() {
            // Some gateways emit `data:payload` (no leading space).
            let frame = indoc! {r#"
                data:{"type":"ping"}
            "#};

            let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

            assert_eq!(event, StreamEvent::Ping);
        }

        #[test]
        fn accepts_default_event_envelopes() {
            for prefix in [
                "",
                "event:\n",
                "event\n",
                "event: message\n",
                "event: stale\nevent:\n",
            ] {
                let frame = format!("{prefix}data: {{\"type\":\"ping\"}}");

                let event = parse_sse_frame(frame.as_bytes()).unwrap().unwrap();

                assert_eq!(event, StreamEvent::Ping, "{prefix:?}");
            }
        }

        #[test]
        fn skips_a_frame_without_a_data_line() {
            assert!(parse_sse_frame(b"").unwrap().is_none());
            assert!(parse_sse_frame(b": comment line").unwrap().is_none());
        }

        #[test]
        fn invalid_json_errors() {
            let err = parse_sse_frame(b"data: {not valid json}").unwrap_err();

            assert_eq!(
                err.to_string(),
                "failed to parse SSE data: {not valid json}"
            );
        }
    }
}
