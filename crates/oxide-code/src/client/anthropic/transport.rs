//! Shared `POST /v1/messages` plumbing for the streaming and one-shot paths: per-request headers,
//! non-2xx classification, and bounded retry with exponential backoff.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use reqwest::StatusCode;
use tracing::warn;

use super::Client;
use super::wire::ErrorResponse;

// ── Retry Policy ──

/// Retries after the first attempt, so one request makes at most `MAX_RETRIES + 1` attempts.
const MAX_RETRIES: u32 = 5;
/// Doubles per retry: 1, 2, 4, 8, 16 s before jitter, about half a minute in total.
#[cfg(not(test))]
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Tests exercise the retry loop without sleeping for real.
#[cfg(test)]
const INITIAL_BACKOFF: Duration = Duration::from_millis(1);
/// Upper bound of the random extra delay, as a fraction of the backoff.
const MAX_JITTER: f64 = 0.25;
/// Longest server-requested `retry-after` worth waiting out. A longer hint, such as an exhausted
/// quota, fails fast so the user sees the error promptly.
const MAX_RETRY_AFTER: Duration = Duration::from_mins(1);

// ── Request ──

/// One `/v1/messages` call, replayable across retry attempts.
pub(super) struct MessagesRequest {
    pub(super) url: String,
    pub(super) betas: String,
    pub(super) session_id: String,
    pub(super) body: String,
}

impl Client {
    pub(super) fn messages_request(&self, betas: String, body: String) -> MessagesRequest {
        MessagesRequest {
            url: format!("{}/v1/messages?beta=true", self.config.base_url),
            betas,
            session_id: self.session_id.clone(),
            body,
        }
    }
}

/// A failed attempt, classified by whether sending the same request again may succeed.
#[derive(Debug)]
pub(super) enum AttemptError {
    /// Transient failure. `cause` names it in a few words for the retry notice, and `retry_after`
    /// carries the server's delay hint when it sent one.
    Retryable {
        error: anyhow::Error,
        cause: String,
        retry_after: Option<Duration>,
    },
    Fatal(anyhow::Error),
}

impl AttemptError {
    pub(super) fn retryable(cause: impl Into<String>, error: anyhow::Error) -> Self {
        Self::Retryable {
            error,
            cause: cause.into(),
            retry_after: None,
        }
    }
}

impl From<anyhow::Error> for AttemptError {
    fn from(error: anyhow::Error) -> Self {
        Self::Fatal(error)
    }
}

/// Runs `attempt` until it succeeds, fails fatally, or exhausts [`MAX_RETRIES`]. `attempt`
/// receives the zero-based retry count for `x-stainless-retry-count`, and `on_retry` hears of each
/// retry before its delay starts.
pub(super) async fn with_retry<T, F, Fut, R>(mut attempt: F, mut on_retry: R) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
    R: FnMut(&RetryNotice),
{
    let mut retry = 0;
    loop {
        let (error, cause, retry_after) = match attempt(retry).await {
            Ok(value) => return Ok(value),
            Err(AttemptError::Fatal(error)) => return Err(error),
            Err(AttemptError::Retryable {
                error,
                cause,
                retry_after,
            }) => (error, cause, retry_after),
        };
        let Some(delay) = retry_delay(retry, retry_after) else {
            return Err(error);
        };
        retry += 1;
        warn!("retrying Anthropic request in {delay:?} ({retry}/{MAX_RETRIES}): {error:#}");
        on_retry(&RetryNotice {
            delay,
            retry,
            cause,
        });
        tokio::time::sleep(delay).await;
    }
}

/// A retry about to wait out its delay, surfaced so a slow recovery never looks like a hang. The
/// full error, with its advice and response body, surfaces only if every retry fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetryNotice {
    delay: Duration,
    /// One-based, out of [`MAX_RETRIES`].
    retry: u32,
    cause: String,
}

impl fmt::Display for RetryNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.delay.as_secs() + u64::from(self.delay.subsec_nanos() > 0);
        write!(
            f,
            "{}. Retrying in {secs}s (retry {} of {MAX_RETRIES}).",
            self.cause, self.retry,
        )
    }
}

/// Delay before the next attempt, or `None` when the budget is spent or the server asked for a
/// longer wait than [`MAX_RETRY_AFTER`].
fn retry_delay(retry: u32, retry_after: Option<Duration>) -> Option<Duration> {
    if retry >= MAX_RETRIES {
        return None;
    }
    if let Some(hint) = retry_after {
        return (hint <= MAX_RETRY_AFTER).then_some(hint);
    }
    let backoff = INITIAL_BACKOFF.saturating_mul(1 << retry);
    Some(backoff.mul_f64(1.0 + MAX_JITTER * fastrand::f64()))
}

/// Sends one attempt and classifies a transport failure or a non-2xx status.
pub(super) async fn post_messages(
    http: &reqwest::Client,
    request: &MessagesRequest,
    retry: u32,
) -> Result<reqwest::Response, AttemptError> {
    let response = http
        .post(&request.url)
        .header("anthropic-beta", &request.betas)
        .header("x-claude-code-session-id", &request.session_id)
        .header("x-stainless-retry-count", retry)
        .body(request.body.clone())
        .send()
        .await
        .map_err(|e| {
            if e.is_builder() {
                AttemptError::Fatal(e.into())
            } else {
                AttemptError::retryable("Connection failed", e.into())
            }
        })?;
    error_for_status(response).await
}

/// Passes a 2xx response through. Otherwise reads the body into a user-facing error. As in the
/// Anthropic SDKs, `x-should-retry: true` or `false` decides whether to retry, and without it 408,
/// 409, 429, and 5xx retry.
async fn error_for_status(response: reqwest::Response) -> Result<reqwest::Response, AttemptError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let header = |name| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let retry_after = header("retry-after").as_deref().and_then(parse_retry_after);
    let should_retry = header("x-should-retry");
    let body = response.text().await.unwrap_or_default();
    let error = anyhow!("{}", format_api_error(status, retry_after, &body));

    let retryable = match should_retry.as_deref() {
        Some("true") => true,
        Some("false") => false,
        _ => matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error(),
    };
    if !retryable {
        return Err(AttemptError::Fatal(error));
    }
    Err(AttemptError::Retryable {
        error,
        cause: status_cause(status, &body),
        retry_after,
    })
}

/// Names a failed status in a few words, such as `HTTP 529 overloaded_error`, preferring the API's
/// error type to the status reason.
fn status_cause(status: StatusCode, body: &str) -> String {
    let error_type = serde_json::from_str::<ErrorResponse>(body)
        .ok()
        .map(|response| response.error.error_type);
    match error_type.as_deref().or(status.canonical_reason()) {
        Some(detail) => format!("HTTP {} {detail}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    }
}

/// Accepts the delay-seconds form. An HTTP-date falls back to the computed backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
}

/// Renders a user-facing diagnostic for a non-2xx Anthropic response, with a per-status hint and
/// the response body appended for debugging.
fn format_api_error(status: StatusCode, retry_after: Option<Duration>, body: &str) -> String {
    let prefix = match status.as_u16() {
        401 => "Anthropic API rejected credentials (HTTP 401). Check ANTHROPIC_API_KEY, or run `claude` to refresh OAuth.".to_owned(),
        429 => match retry_after {
            Some(after) => format!(
                "Anthropic API rate limited (HTTP 429). Retry after {} s.",
                after.as_secs(),
            ),
            None => "Anthropic API rate limited (HTTP 429). Retry after a short delay.".to_owned(),
        },
        529 => "Anthropic API overloaded (HTTP 529). This is usually transient, so retry in a few seconds.".to_owned(),
        s if (500..600).contains(&s) => {
            format!("Anthropic API server error (HTTP {status}). This is usually transient, so retry.")
        }
        _ => format!("API error (HTTP {status})."),
    };
    format!("{prefix} Details: {body}")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn request(server: &MockServer) -> MessagesRequest {
        MessagesRequest {
            url: format!("{}/v1/messages", server.uri()),
            betas: String::new(),
            session_id: "sid".to_owned(),
            body: "{}".to_owned(),
        }
    }

    async fn post_once(server: &MockServer) -> Result<reqwest::Response, AttemptError> {
        post_messages(&reqwest::Client::new(), &request(server), 0).await
    }

    async fn mount(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    // ── with_retry ──

    #[tokio::test]
    async fn with_retry_retries_transient_failures_until_success() {
        let calls = &AtomicU32::new(0);
        let mut notices = Vec::new();
        let value = with_retry(
            |retry| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if retry < 2 {
                    Err(AttemptError::Retryable {
                        error: anyhow!("transient {retry}"),
                        cause: format!("Cause {retry}"),
                        retry_after: Some(Duration::from_millis(1500)),
                    })
                } else {
                    Ok(retry)
                }
            },
            |notice| notices.push(notice.clone()),
        )
        .await
        .unwrap();

        assert_eq!(value, 2, "the attempt sees its retry count");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let expected: Vec<RetryNotice> = (1..=2)
            .map(|retry| RetryNotice {
                delay: Duration::from_millis(1500),
                retry,
                cause: format!("Cause {}", retry - 1),
            })
            .collect();
        assert_eq!(notices, expected, "one notice per retry, before its delay");
    }

    #[tokio::test]
    async fn with_retry_gives_up_after_the_retry_budget() {
        let calls = &AtomicU32::new(0);
        let err = with_retry(
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(AttemptError::retryable(
                    "Overloaded",
                    anyhow!("still overloaded"),
                ))
            },
            |_| {},
        )
        .await
        .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
        assert_eq!(err.to_string(), "still overloaded", "last error surfaces");
    }

    #[tokio::test]
    async fn with_retry_stops_on_a_fatal_failure() {
        let calls = &AtomicU32::new(0);
        let err = with_retry(
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(AttemptError::Fatal(anyhow!("bad request")))
            },
            |_| {},
        )
        .await
        .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(err.to_string(), "bad request");
    }

    #[tokio::test]
    async fn with_retry_fails_fast_when_retry_after_exceeds_the_cap() {
        let calls = &AtomicU32::new(0);
        let err = with_retry(
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(AttemptError::Retryable {
                    error: anyhow!("quota exhausted"),
                    cause: "HTTP 429 rate_limit_error".to_owned(),
                    retry_after: Some(MAX_RETRY_AFTER + Duration::from_secs(1)),
                })
            },
            |_| {},
        )
        .await
        .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(err.to_string(), "quota exhausted");
    }

    // ── RetryNotice ──

    #[test]
    fn retry_notice_display_rounds_the_delay_up_to_whole_seconds() {
        let notice = |delay| RetryNotice {
            delay,
            retry: 2,
            cause: "HTTP 529 overloaded_error".to_owned(),
        };
        assert_eq!(
            notice(Duration::from_millis(3200)).to_string(),
            "HTTP 529 overloaded_error. Retrying in 4s (retry 2 of 5).",
        );
        assert_eq!(
            notice(Duration::from_secs(8)).to_string(),
            "HTTP 529 overloaded_error. Retrying in 8s (retry 2 of 5).",
        );
    }

    // ── retry_delay ──

    #[test]
    fn retry_delay_doubles_the_backoff_with_bounded_jitter() {
        for retry in 0..MAX_RETRIES {
            let base = INITIAL_BACKOFF * 2_u32.pow(retry);
            let delay = retry_delay(retry, None).expect("within budget");
            assert!(
                delay >= base && delay <= base.mul_f64(1.0 + MAX_JITTER),
                "retry {retry}: {delay:?} outside [{base:?}, +{MAX_JITTER}]",
            );
        }
    }

    #[test]
    fn retry_delay_honors_a_server_hint_within_the_cap() {
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(7))),
            Some(Duration::from_secs(7)),
        );
        assert_eq!(retry_delay(0, Some(MAX_RETRY_AFTER)), Some(MAX_RETRY_AFTER));
        assert_eq!(
            retry_delay(0, Some(MAX_RETRY_AFTER + Duration::from_secs(1))),
            None,
        );
    }

    #[test]
    fn retry_delay_is_none_once_the_budget_is_spent() {
        assert!(retry_delay(MAX_RETRIES, None).is_none());
        assert!(retry_delay(MAX_RETRIES, Some(Duration::ZERO)).is_none());
    }

    // ── post_messages ──

    #[tokio::test]
    async fn post_messages_sends_per_request_headers() {
        let server = MockServer::start().await;
        mount(&server, ResponseTemplate::new(200)).await;
        let mut req = request(&server);
        req.betas = "beta-a,beta-b".to_owned();

        post_messages(&reqwest::Client::new(), &req, 3)
            .await
            .expect("2xx passes through");

        let received = server.received_requests().await.unwrap();
        let headers = &received[0].headers;
        assert_eq!(headers["anthropic-beta"], "beta-a,beta-b");
        assert_eq!(headers["x-claude-code-session-id"], "sid");
        assert_eq!(headers["x-stainless-retry-count"], "3");
    }

    #[tokio::test]
    async fn post_messages_connection_failure_is_retryable() {
        // Port 9 (discard) is closed on test hosts, so the connect fails fast.
        let req = MessagesRequest {
            url: "http://127.0.0.1:9/v1/messages".to_owned(),
            betas: String::new(),
            session_id: "sid".to_owned(),
            body: "{}".to_owned(),
        };
        let err = post_messages(&reqwest::Client::new(), &req, 0)
            .await
            .expect_err("nothing listens on the discard port");
        assert!(matches!(err, AttemptError::Retryable { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn post_messages_malformed_url_is_fatal() {
        let req = MessagesRequest {
            url: "not a url".to_owned(),
            betas: String::new(),
            session_id: "sid".to_owned(),
            body: "{}".to_owned(),
        };
        let err = post_messages(&reqwest::Client::new(), &req, 0)
            .await
            .expect_err("the request cannot be built");
        assert!(matches!(err, AttemptError::Fatal(_)), "{err:?}");
    }

    #[tokio::test]
    async fn post_messages_classifies_status_codes() {
        for (status, retryable) in [
            (408_u16, true),
            (409, true),
            (429, true),
            (500, true),
            (502, true),
            (529, true),
            (400, false),
            (401, false),
            (404, false),
        ] {
            let server = MockServer::start().await;
            mount(
                &server,
                ResponseTemplate::new(status).set_body_string("boom"),
            )
            .await;

            let err = post_once(&server).await.expect_err("non-2xx fails");
            let (AttemptError::Retryable { error, .. } | AttemptError::Fatal(error)) = &err;
            assert_eq!(
                matches!(err, AttemptError::Retryable { .. }),
                retryable,
                "status {status}",
            );
            let msg = format!("{error:#}");
            assert!(msg.contains(&status.to_string()), "{msg}");
            assert!(msg.contains("Details: boom"), "{msg}");
        }
    }

    #[tokio::test]
    async fn post_messages_threads_retry_after_into_the_hint_and_message() {
        let server = MockServer::start().await;
        mount(
            &server,
            ResponseTemplate::new(429).insert_header("retry-after", "12"),
        )
        .await;

        let Err(AttemptError::Retryable {
            error, retry_after, ..
        }) = post_once(&server).await
        else {
            panic!("429 must be retryable");
        };
        assert_eq!(retry_after, Some(Duration::from_secs(12)));
        assert!(
            format!("{error:#}").contains("Retry after 12 s."),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn post_messages_names_the_api_error_type_in_the_retry_cause() {
        for (body, cause) in [
            (
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                "HTTP 529 overloaded_error",
            ),
            ("<html>busy</html>", "HTTP 529"),
        ] {
            let server = MockServer::start().await;
            mount(&server, ResponseTemplate::new(529).set_body_string(body)).await;

            let Err(AttemptError::Retryable { cause: got, .. }) = post_once(&server).await else {
                panic!("529 must be retryable");
            };
            assert_eq!(got, cause);
        }
    }

    #[tokio::test]
    async fn post_messages_x_should_retry_overrides_the_status_rule() {
        for (status, header, retryable) in [(529, "false", false), (400, "true", true)] {
            let server = MockServer::start().await;
            mount(
                &server,
                ResponseTemplate::new(status).insert_header("x-should-retry", header),
            )
            .await;

            let err = post_once(&server).await.expect_err("non-2xx fails");
            assert_eq!(
                matches!(err, AttemptError::Retryable { .. }),
                retryable,
                "HTTP {status} with x-should-retry: {header}",
            );
        }
    }

    #[tokio::test]
    async fn post_messages_with_retry_recovers_from_a_transient_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount(&server, ResponseTemplate::new(200).set_body_string("ok")).await;
        let http = &reqwest::Client::new();
        let req = &request(&server);

        let body = with_retry(
            |retry| async move {
                let response = post_messages(http, req, retry).await?;
                Ok(response.text().await.map_err(anyhow::Error::from)?)
            },
            |_| {},
        )
        .await
        .unwrap();

        assert_eq!(body, "ok");
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
        assert_eq!(counts, ["0", "1"]);
    }

    // ── parse_retry_after ──

    #[test]
    fn parse_retry_after_accepts_seconds_and_rejects_dates() {
        assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    // ── format_api_error ──

    #[test]
    fn format_api_error_renders_a_hint_per_status_and_appends_the_body() {
        let secs = Some(Duration::from_secs(42));
        for (status, retry_after, prefix) in [
            (
                401,
                None,
                "Anthropic API rejected credentials (HTTP 401). Check ANTHROPIC_API_KEY, or run `claude` to refresh OAuth.",
            ),
            (
                429,
                secs,
                "Anthropic API rate limited (HTTP 429). Retry after 42 s.",
            ),
            (
                429,
                None,
                "Anthropic API rate limited (HTTP 429). Retry after a short delay.",
            ),
            (
                529,
                None,
                "Anthropic API overloaded (HTTP 529). This is usually transient, so retry in a few seconds.",
            ),
            (
                502,
                None,
                "Anthropic API server error (HTTP 502 Bad Gateway). This is usually transient, so retry.",
            ),
            (400, secs, "API error (HTTP 400 Bad Request)."),
        ] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                format_api_error(status, retry_after, "body"),
                format!("{prefix} Details: body"),
            );
        }
    }
}
