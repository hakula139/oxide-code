//! Shared `POST /v1/messages` plumbing for the streaming and one-shot paths: per-request headers,
//! non-2xx classification, and bounded retry with exponential backoff.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use regex::Regex;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use tracing::{debug, warn};

use super::Client;
use super::wire::{ApiError, ErrorResponse};
use crate::client::PromptTooLong;
use crate::client::{RateLimitWindow, RateLimits};
use crate::config::auth::{self, Auth, AuthSource, Credential};
use crate::util::text::{join_sentences, truncate_to_width};
use crate::util::tls::Trust;

// ── Retry Policy ──

/// Retries after the first attempt, so one request makes at most `MAX_RETRIES + 1` attempts.
const MAX_RETRIES: u32 = 5;
/// Doubles per retry: 1, 2, 4, 8, 16 s before jitter, about half a minute in total.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
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
    pub(super) body: Bytes,
    pub(super) auth: Auth,
    /// Verifies the token endpoint when a stored login refreshes.
    pub(super) trust: Arc<dyn Trust>,
}

impl Client {
    pub(super) fn messages_request(
        &self,
        betas: String,
        body: String,
        auth: &Auth,
    ) -> MessagesRequest {
        MessagesRequest {
            url: format!("{}/v1/messages?beta=true", self.base_url),
            betas,
            session_id: self.session_id.clone(),
            body: body.into(),
            auth: auth.clone(),
            trust: Arc::clone(&self.trust),
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
/// full error, with its details and advice, surfaces only if every retry fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RetryNotice {
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
            "{}. Retrying in {secs} s (retry {} of {MAX_RETRIES}).",
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

/// Sends one attempt and classifies a transport failure or a non-2xx status. A stored login is
/// refreshed first when near expiry. When the API rejects it with HTTP 401, it is reloaded, or
/// refreshed when its store still holds the rejected token, and sent once more.
pub(super) async fn post_messages(
    http: &reqwest::Client,
    request: &MessagesRequest,
    retry: u32,
) -> Result<reqwest::Response, AttemptError> {
    let trust = request.trust.as_ref();
    let credential = request
        .auth
        .credential(trust)
        .await
        .map_err(credential_failure)?;
    let mut response = send(http, request, &credential, retry).await?;
    if response.status() == StatusCode::UNAUTHORIZED
        && let Some(reloaded) = request
            .auth
            .reload(&credential, trust)
            .await
            .map_err(credential_failure)?
    {
        response = send(http, request, &reloaded, retry).await?;
    }
    error_for_status(response, request.auth.source()).await
}

/// Retries a stored login's refresh that a later attempt may complete. Any other failure to
/// produce a credential is fatal.
fn credential_failure(error: anyhow::Error) -> AttemptError {
    if auth::is_transient(&error) {
        AttemptError::retryable("Token refresh failed", error)
    } else {
        AttemptError::Fatal(error)
    }
}

async fn send(
    http: &reqwest::Client,
    request: &MessagesRequest,
    credential: &Credential,
    retry: u32,
) -> Result<reqwest::Response, AttemptError> {
    let (name, value) = credential.header();
    http.post(&request.url)
        .header("anthropic-beta", &request.betas)
        .header("x-claude-code-session-id", &request.session_id)
        .header("x-stainless-retry-count", retry)
        .header(name, value)
        .body(request.body.clone())
        .send()
        .await
        .map_err(|e| {
            if e.is_builder() {
                AttemptError::Fatal(e.into())
            } else {
                AttemptError::retryable(
                    "API connection failed",
                    anyhow::Error::new(e).context("API connection failed"),
                )
            }
        })
}

/// Passes a 2xx response through. Otherwise reads the body into a user-facing error. As in the
/// Anthropic SDKs, `x-should-retry: true` or `false` decides whether to retry, and without it 408,
/// 409, 429, and 5xx retry.
async fn error_for_status(
    response: reqwest::Response,
    auth: AuthSource,
) -> Result<reqwest::Response, AttemptError> {
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
    let location = header("location");
    let should_retry = header("x-should-retry");
    let body = response.text().await.unwrap_or_default();
    debug!(status = status.as_u16(), body, "API error response");
    let api_error = serde_json::from_str::<ErrorResponse>(&body)
        .ok()
        .map(|response| response.error);
    let details = body_details(&body, api_error.as_ref());
    let diagnostic = format_api_error(
        auth,
        status,
        retry_after,
        location.as_deref(),
        details.as_deref(),
    );
    let error = if status == StatusCode::BAD_REQUEST
        && let Some(message) = api_error.as_ref().and_then(prompt_too_long_message)
    {
        anyhow::Error::new(PromptTooLong {
            overflow: prompt_overflow(message),
            diagnostic,
        })
    } else {
        anyhow!(diagnostic)
    };

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
        cause: status_cause(status, api_error.as_ref()),
        retry_after,
    })
}

/// Reads the unified rate-limit headers that subscription sign-ins receive. `None` when the
/// response reports neither window, as for API keys.
pub(super) fn rate_limits(headers: &HeaderMap) -> Option<RateLimits> {
    let limits = RateLimits {
        five_hour: rate_limit_window(headers, "5h"),
        seven_day: rate_limit_window(headers, "7d"),
    };
    (limits.five_hour.is_some() || limits.seven_day.is_some()).then_some(limits)
}

fn rate_limit_window(headers: &HeaderMap, window: &str) -> Option<RateLimitWindow> {
    let field = |name: &str| {
        headers
            .get(format!("anthropic-ratelimit-unified-{window}-{name}"))
            .and_then(|value| value.to_str().ok())
    };
    Some(RateLimitWindow {
        utilization: field("utilization")?
            .parse::<f64>()
            .ok()
            .filter(|utilization| utilization.is_finite())?,
        resets_at: field("reset")?.parse().ok()?,
    })
}

/// The message of an `invalid_request_error` that says the prompt is too long.
fn prompt_too_long_message(error: &ApiError) -> Option<&str> {
    (error.error_type == "invalid_request_error"
        && error
            .message
            .to_ascii_lowercase()
            .starts_with("prompt is too long"))
    .then_some(&error.message)
}

/// Reads the overflow from `prompt is too long: N tokens > M maximum`.
fn prompt_overflow(message: &str) -> Option<u32> {
    let non_digit = |c: char| !c.is_ascii_digit();
    let (actual, limit) = message.split_once('>')?;
    let actual: u32 = actual
        .split(non_digit)
        .rfind(|run| !run.is_empty())?
        .parse()
        .ok()?;
    let limit: u32 = limit
        .split(non_digit)
        .find(|run| !run.is_empty())?
        .parse()
        .ok()?;
    actual.checked_sub(limit).filter(|&overflow| overflow > 0)
}

/// Names a failed status in a few words, such as `HTTP 529 overloaded_error`, with the API's error
/// type when the body carries one and the [`http_status`] reason otherwise.
fn status_cause(status: StatusCode, error: Option<&ApiError>) -> String {
    match error {
        Some(error) => format!("HTTP {} {}", status.as_u16(), error.error_type),
        None => http_status(status),
    }
}

/// `HTTP 502 Bad Gateway`, or `HTTP 529` for a code without a standard reason phrase.
fn http_status(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("HTTP {} {reason}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    }
}

/// Accepts the delay-seconds form. An HTTP-date falls back to the computed backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
}

/// Headline of an overload, reported as HTTP 529 or an in-band `overloaded_error`.
const OVERLOADED: &str = "Anthropic API overloaded";
/// Headline of a server failure, reported as a 5xx status or an in-band `api_error`.
const SERVER_ERROR: &str = "Anthropic API server error";
/// Advice for an overload or a server failure, both of which usually pass.
const TRANSIENT_ADVICE: &str = "Retry in a few seconds.";

/// Renders a user-facing diagnostic for a non-2xx Anthropic response: a per-status headline, the
/// body's `details` after a colon, then a per-status hint.
fn format_api_error(
    auth: AuthSource,
    status: StatusCode,
    retry_after: Option<Duration>,
    location: Option<&str>,
    details: Option<&str>,
) -> String {
    let (headline, advice) = match status.as_u16() {
        300..=399 => {
            let target = location
                .and_then(|location| reqwest::Url::parse(location).ok())
                .and_then(|url| url.host_str().map(|host| format!(" to {host}")))
                .unwrap_or_default();
            (
                format!(
                    "Anthropic API endpoint redirected ({}){target}",
                    http_status(status),
                ),
                Some(
                    "Redirects are not followed, so point the base URL at the final endpoint."
                        .to_owned(),
                ),
            )
        }
        401 => (
            "Anthropic API rejected credentials (HTTP 401)".to_owned(),
            Some(auth.rejection_hint().to_owned()),
        ),
        429 => (
            "Anthropic API rate limited (HTTP 429)".to_owned(),
            Some(match retry_after {
                Some(after) => format!("Retry after {} s.", after.as_secs()),
                None => "Retry after a short delay.".to_owned(),
            }),
        ),
        529 => (
            format!("{OVERLOADED} (HTTP 529)"),
            Some(TRANSIENT_ADVICE.to_owned()),
        ),
        s if (500..600).contains(&s) => (
            format!("{SERVER_ERROR} ({})", http_status(status)),
            Some(TRANSIENT_ADVICE.to_owned()),
        ),
        _ => (format!("API error ({})", http_status(status)), None),
    };
    with_details(&headline, details, advice.as_deref())
}

/// Renders an in-band SSE `error` event in the shape of the HTTP diagnostic for the same failure,
/// so an `overloaded_error` reads like HTTP 529 and an `api_error` like HTTP 500.
pub(super) fn format_stream_error(error_type: &str, message: &str) -> String {
    let (headline, advice) = match error_type {
        "overloaded_error" => (OVERLOADED, Some(TRANSIENT_ADVICE)),
        "api_error" => (SERVER_ERROR, Some(TRANSIENT_ADVICE)),
        _ => ("API error", None),
    };
    let details = condense(message);
    with_details(
        &format!("{headline} ({error_type})"),
        details.as_deref(),
        advice,
    )
}

/// Joins a diagnostic as `headline: details. advice`, leaving out whichever part is absent.
/// [`join_sentences`] picks the separator between the details and the advice.
fn with_details(headline: &str, details: Option<&str>, advice: Option<&str>) -> String {
    match (details, advice) {
        (None, None) => headline.to_owned(),
        (None, Some(advice)) => format!("{headline}. {advice}"),
        (Some(details), None) => format!("{headline}: {details}"),
        (Some(details), Some(advice)) => format!("{headline}: {}", join_sentences(details, advice)),
    }
}

/// Most terminal columns [`condense`] keeps, so a verbose body cannot bury the advice.
const MAX_DETAILS_WIDTH: usize = 300;

static HTML_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("the title pattern is a valid regex")
});

/// What a non-2xx `body` says, [condensed](condense): the message of an API `error`, the title of
/// a markup page such as a gateway's HTML error, or else the text itself. `None` when that is
/// blank or a page has no title.
fn body_details(body: &str, error: Option<&ApiError>) -> Option<String> {
    let text = match error {
        Some(error) => &error.message,
        None if body.trim_start().starts_with('<') => HTML_TITLE.captures(body)?.get(1)?.as_str(),
        None => body,
    };
    condense(text)
}

/// Server text on one line of at most [`MAX_DETAILS_WIDTH`] columns, with its punctuation as sent.
/// `None` when the text is blank.
fn condense(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then(|| truncate_to_width(&text, MAX_DETAILS_WIDTH))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use indoc::indoc;
    use reqwest::header::{HeaderName, HeaderValue};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::client::anthropic::testing::{messages_request, plain_http};
    use crate::config::oauth::testing::{ok_refresh_body, ox_login_in, write_creds};

    fn request(server: &MockServer) -> MessagesRequest {
        messages_request(format!("{}/v1/messages", server.uri()))
    }

    async fn post_once(server: &MockServer) -> Result<reqwest::Response, AttemptError> {
        post_messages(&plain_http(), &request(server), 0).await
    }

    async fn mount(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    // ── with_retry ──

    #[tokio::test(start_paused = true)]
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

    #[tokio::test(start_paused = true)]
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
            "HTTP 529 overloaded_error. Retrying in 4 s (retry 2 of 5).",
        );
        assert_eq!(
            notice(Duration::from_secs(8)).to_string(),
            "HTTP 529 overloaded_error. Retrying in 8 s (retry 2 of 5).",
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

        post_messages(&plain_http(), &req, 3)
            .await
            .expect("2xx passes through");

        let received = server.received_requests().await.unwrap();
        let headers = &received[0].headers;
        assert_eq!(headers["anthropic-beta"], "beta-a,beta-b");
        assert_eq!(headers["x-claude-code-session-id"], "sid");
        assert_eq!(headers["x-stainless-retry-count"], "3");
        assert_eq!(headers["x-api-key"], "k");
    }

    #[tokio::test]
    async fn post_messages_connection_failure_is_retryable() {
        // Port 9 (discard) is closed on test hosts, so the connect fails fast.
        let req = messages_request("http://127.0.0.1:9/v1/messages");
        let err = post_messages(&plain_http(), &req, 0)
            .await
            .expect_err("nothing listens on the discard port");
        let AttemptError::Retryable { error, cause, .. } = &err else {
            panic!("expected a retryable error, got {err:?}");
        };
        assert_eq!(cause, "API connection failed");
        assert!(
            format!("{error:#}").starts_with("API connection failed: "),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn post_messages_malformed_url_is_fatal() {
        let req = messages_request("not a url");
        let err = post_messages(&plain_http(), &req, 0)
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
            (307, false),
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
            assert_eq!(
                matches!(err, AttemptError::Retryable { .. }),
                retryable,
                "status {status}",
            );
        }
    }

    #[tokio::test]
    async fn post_messages_resends_a_rejected_stored_login_with_the_token_another_process_stored() {
        let server = MockServer::start().await;
        for (token, status) in [("old", 401), ("new", 200)] {
            Mock::given(method("POST"))
                .and(path("/v1/messages"))
                .and(header("authorization", format!("Bearer {token}")))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        write_creds(&file, "new", Some("ref"), 9_999_999_999_999);
        let mut req = request(&server);
        req.auth = ox_login_in(&file, "http://unused", "old", 9_999_999_999_999);

        post_messages(&plain_http(), &req, 0)
            .await
            .expect("the resend succeeds");

        server.verify().await;
    }

    /// Answers `/v1/messages` sent with `token` with `status`, expecting `hits` such requests.
    async fn mount_for_token(server: &MockServer, token: &str, status: u16, hits: u64) {
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(status).set_body_string("token revoked"))
            .expect(hits)
            .mount(server)
            .await;
    }

    /// Answers the token endpoint at `/token` with `response`, expecting `hits` refreshes.
    async fn mount_refresh(server: &MockServer, response: ResponseTemplate, hits: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(response)
            .expect(hits)
            .mount(server)
            .await;
    }

    /// A request sending the oxide-code login `token`, which its store at `file` also holds with a
    /// refresh token and `server` refreshes at `/token`.
    fn stored_login_request(
        server: &MockServer,
        file: &Path,
        token: &str,
        expires_at_ms: u64,
    ) -> MessagesRequest {
        write_creds(file, token, Some("ref"), expires_at_ms);
        let mut req = request(server);
        let refresh_url = format!("{}/token", server.uri());
        req.auth = ox_login_in(file, &refresh_url, token, expires_at_ms);
        req
    }

    fn refreshed() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(ok_refresh_body("fresh-access", "fresh-ref", 3600))
    }

    #[tokio::test]
    async fn post_messages_refreshes_a_rejected_stored_login_its_store_still_holds() {
        let server = MockServer::start().await;
        mount_refresh(&server, refreshed(), 1).await;
        mount_for_token(&server, "revoked", 401, 1).await;
        mount_for_token(&server, "fresh-access", 200, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        let req = stored_login_request(&server, &file, "revoked", 9_999_999_999_999);

        post_messages(&plain_http(), &req, 0)
            .await
            .expect("the resend with the refreshed token succeeds");

        server.verify().await;
    }

    #[tokio::test]
    async fn post_messages_reports_a_401_the_refreshed_token_also_gets_with_the_sign_in_step() {
        let server = MockServer::start().await;
        mount_refresh(&server, refreshed(), 1).await;
        mount_for_token(&server, "revoked", 401, 1).await;
        mount_for_token(&server, "fresh-access", 401, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        let req = stored_login_request(&server, &file, "revoked", 9_999_999_999_999);

        let err = post_messages(&plain_http(), &req, 0)
            .await
            .expect_err("the second 401 fails the attempt");

        let AttemptError::Fatal(error) = &err else {
            panic!("expected a fatal error, got {err:?}");
        };
        assert_eq!(
            format!("{error:#}"),
            "Anthropic API rejected credentials (HTTP 401): token revoked. \
             Run `/login` to sign in again.",
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn post_messages_reports_a_401_without_resending_when_the_refresh_is_rejected() {
        let server = MockServer::start().await;
        let invalid_grant =
            ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid_grant"}"#);
        mount_refresh(&server, invalid_grant, 1).await;
        mount_for_token(&server, "revoked", 401, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        let req = stored_login_request(&server, &file, "revoked", 9_999_999_999_999);

        let err = post_messages(&plain_http(), &req, 0)
            .await
            .expect_err("a login the refresh cannot restore fails the attempt");

        let AttemptError::Fatal(error) = &err else {
            panic!("expected a fatal error, got {err:?}");
        };
        assert_eq!(
            format!("{error:#}"),
            "Anthropic API rejected credentials (HTTP 401): token revoked. \
             Run `/login` to sign in again.",
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn post_messages_retries_a_refresh_the_token_endpoint_failed_with_a_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        mount_refresh(&server, refreshed(), 1).await;
        mount_for_token(&server, "fresh-access", 200, 1).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credentials.json");
        let req = stored_login_request(&server, &file, "expired", 0);
        let http = plain_http();

        let first = post_messages(&http, &req, 0).await;
        let retry = post_messages(&http, &req, 1).await;

        let Err(AttemptError::Retryable { cause, .. }) = &first else {
            panic!("expected a retryable error, got {first:?}");
        };
        assert_eq!(cause, "Token refresh failed");
        retry.expect("the retry refreshes and sends the request");
        server.verify().await;
    }

    #[tokio::test]
    async fn post_messages_condenses_a_gateway_page_to_its_title() {
        let server = MockServer::start().await;
        let page = indoc! {r#"
            <!DOCTYPE html>
            <html>
            <head><TITLE lang="en">
              example.com | 522: Connection timed out
            </TITLE><style>body { margin: 0 }</style></head>
            <body><h1>Connection timed out</h1></body>
            </html>
        "#};
        mount(&server, ResponseTemplate::new(522).set_body_string(page)).await;

        let Err(AttemptError::Retryable { error, cause, .. }) = post_once(&server).await else {
            panic!("HTTP 522 is retryable");
        };

        assert_eq!(
            error.to_string(),
            "Anthropic API server error (HTTP 522): example.com | 522: Connection timed out. \
             Retry in a few seconds.",
        );
        assert_eq!(cause, "HTTP 522");
    }

    #[tokio::test]
    async fn post_messages_types_a_prompt_too_long_rejection() {
        for (kind, message, expected) in [
            (
                "invalid_request_error",
                "prompt is too long: 215000 tokens > 200000 maximum",
                Some(Some(15_000)),
            ),
            ("invalid_request_error", "Prompt is too long", Some(None)),
            ("invalid_request_error", "max_tokens: too large", None),
            (
                "api_error",
                "prompt is too long: 3 tokens > 2 maximum",
                None,
            ),
        ] {
            let server = MockServer::start().await;
            let body =
                format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"{message}"}}}}"#);
            mount(&server, ResponseTemplate::new(400).set_body_string(&body)).await;

            let Err(AttemptError::Fatal(error)) = post_once(&server).await else {
                panic!("HTTP 400 is fatal");
            };

            let too_long = error.downcast_ref::<PromptTooLong>();
            assert_eq!(too_long.map(|e| e.overflow), expected, "{body}");
            assert_eq!(
                error.to_string(),
                format!("API error (HTTP 400 Bad Request): {message}"),
            );
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
        assert_eq!(
            error.to_string(),
            "Anthropic API rate limited (HTTP 429). Retry after 12 s.",
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

    #[tokio::test(start_paused = true)]
    async fn post_messages_with_retry_recovers_from_a_transient_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount(&server, ResponseTemplate::new(200).set_body_string("ok")).await;
        let http = &plain_http();
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

    // ── rate_limits ──

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|&(name, value)| {
                (
                    HeaderName::from_static(name),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    fn window(utilization: f64, resets_at: i64) -> RateLimitWindow {
        RateLimitWindow {
            utilization,
            resets_at,
        }
    }

    #[test]
    fn rate_limits_reads_each_window_the_response_reports() {
        assert_eq!(
            rate_limits(&headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("anthropic-ratelimit-unified-5h-reset", "1760000000"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
                ("anthropic-ratelimit-unified-7d-reset", "1760500000"),
            ])),
            Some(RateLimits {
                five_hour: Some(window(0.42, 1_760_000_000)),
                seven_day: Some(window(0.13, 1_760_500_000)),
            }),
        );
        assert_eq!(
            rate_limits(&headers(&[
                ("anthropic-ratelimit-unified-7d-utilization", "1"),
                ("anthropic-ratelimit-unified-7d-reset", "1760500000"),
            ])),
            Some(RateLimits {
                five_hour: None,
                seven_day: Some(window(1.0, 1_760_500_000)),
            }),
        );
    }

    #[test]
    fn rate_limits_skips_an_incomplete_or_malformed_window() {
        assert_eq!(rate_limits(&HeaderMap::new()), None, "API keys get none");
        for (utilization, reset) in [("0.42", ""), ("NaN", "1760000000"), ("0.42", "soon")] {
            let mut pairs = vec![("anthropic-ratelimit-unified-5h-utilization", utilization)];
            if !reset.is_empty() {
                pairs.push(("anthropic-ratelimit-unified-5h-reset", reset));
            }
            assert_eq!(
                rate_limits(&headers(&pairs)),
                None,
                "{utilization:?} / {reset:?}",
            );
        }
    }

    // ── parse_retry_after ──

    #[test]
    fn parse_retry_after_accepts_seconds_and_rejects_dates() {
        assert_eq!(parse_retry_after(" 30 "), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    // ── format_api_error ──

    #[test]
    fn format_api_error_puts_the_details_after_the_headline_and_the_hint_last() {
        let secs = Some(Duration::from_secs(42));
        for (status, retry_after, expected) in [
            (
                401,
                None,
                "Anthropic API rejected credentials (HTTP 401): boom. \
                 CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. \
                 Generate a new one with `claude setup-token`.",
            ),
            (
                429,
                secs,
                "Anthropic API rate limited (HTTP 429): boom. Retry after 42 s.",
            ),
            (
                429,
                None,
                "Anthropic API rate limited (HTTP 429): boom. Retry after a short delay.",
            ),
            (
                529,
                None,
                "Anthropic API overloaded (HTTP 529): boom. Retry in a few seconds.",
            ),
            (
                502,
                None,
                "Anthropic API server error (HTTP 502 Bad Gateway): boom. Retry in a few seconds.",
            ),
            (
                522,
                None,
                "Anthropic API server error (HTTP 522): boom. Retry in a few seconds.",
            ),
            (400, secs, "API error (HTTP 400 Bad Request): boom"),
        ] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(
                format_api_error(
                    AuthSource::OAuthTokenEnv,
                    status,
                    retry_after,
                    None,
                    Some("boom"),
                ),
                expected,
            );
        }
    }

    #[test]
    fn format_api_error_without_details_puts_the_hint_after_the_headline() {
        for (status, expected) in [
            (StatusCode::BAD_REQUEST, "API error (HTTP 400 Bad Request)"),
            (
                StatusCode::BAD_GATEWAY,
                "Anthropic API server error (HTTP 502 Bad Gateway). Retry in a few seconds.",
            ),
        ] {
            assert_eq!(
                format_api_error(AuthSource::ApiKeyEnv, status, None, None, None),
                expected,
            );
        }
    }

    #[test]
    fn format_api_error_keeps_the_details_as_sent_and_ends_each_sentence_once() {
        let truncated = body_details(&"x".repeat(MAX_DETAILS_WIDTH + 1), None).unwrap();
        for (status, details, expected) in [
            (
                StatusCode::BAD_GATEWAY,
                "Upstream is down.",
                "Anthropic API server error (HTTP 502 Bad Gateway): Upstream is down. \
                 Retry in a few seconds."
                    .to_owned(),
            ),
            (
                StatusCode::BAD_GATEWAY,
                truncated.as_str(),
                format!(
                    "Anthropic API server error (HTTP 502 Bad Gateway): {}... \
                     Retry in a few seconds.",
                    "x".repeat(MAX_DETAILS_WIDTH - 3),
                ),
            ),
            (
                StatusCode::BAD_GATEWAY,
                "upstream connect error",
                "Anthropic API server error (HTTP 502 Bad Gateway): upstream connect error. \
                 Retry in a few seconds."
                    .to_owned(),
            ),
            (
                StatusCode::BAD_REQUEST,
                "messages.0: The block is empty. Remove it.",
                "API error (HTTP 400 Bad Request): messages.0: The block is empty. Remove it."
                    .to_owned(),
            ),
        ] {
            assert_eq!(
                format_api_error(AuthSource::ApiKeyEnv, status, None, None, Some(details)),
                expected,
                "{details:?}",
            );
        }
    }

    #[test]
    fn format_api_error_3xx_names_the_redirect_host() {
        let redirected = "Anthropic API endpoint redirected (HTTP 307 Temporary Redirect)";
        let advice = "Redirects are not followed, so point the base URL at the final endpoint.";
        for (location, target) in [
            (
                Some("https://other.example.com/v1/messages"),
                " to other.example.com",
            ),
            (Some("/v2/messages"), ""),
            (None, ""),
        ] {
            assert_eq!(
                format_api_error(
                    AuthSource::ApiKeyEnv,
                    StatusCode::TEMPORARY_REDIRECT,
                    None,
                    location,
                    Some("moved"),
                ),
                format!("{redirected}{target}: moved. {advice}"),
                "{location:?}",
            );
        }
    }

    // ── body_details ──

    #[test]
    fn body_details_shows_the_api_error_message() {
        let error = ApiError {
            error_type: "invalid_request_error".to_owned(),
            message: "max_tokens:\n  too large".to_owned(),
        };

        let details = body_details(r#"{"type":"error"}"#, Some(&error));

        assert_eq!(details.as_deref(), Some("max_tokens: too large"));
    }

    #[test]
    fn body_details_shows_the_title_of_a_markup_page() {
        let nginx = indoc! {"
            <html>
            <head><title>502 Bad Gateway</title></head>
            <body><center><h1>502 Bad Gateway</h1></center></body>
            </html>
        "};
        assert_eq!(
            body_details(nginx, None).as_deref(),
            Some("502 Bad Gateway")
        );
        assert_eq!(body_details("<html><body>busy</body></html>", None), None);
    }

    #[test]
    fn body_details_shows_plain_text_on_one_capped_line() {
        let long = "x".repeat(MAX_DETAILS_WIDTH + 1);
        for (body, expected) in [
            (
                "\n  upstream connect error\r\n\treset reason: overflow\n",
                Some("upstream connect error reset reason: overflow".to_owned()),
            ),
            (
                long.as_str(),
                Some(format!("{}...", "x".repeat(MAX_DETAILS_WIDTH - 3))),
            ),
            (" \r\n", None),
        ] {
            assert_eq!(body_details(body, None), expected, "{body:?}");
        }
    }

    // ── format_stream_error ──

    #[test]
    fn format_stream_error_matches_the_http_diagnostic_for_the_same_failure() {
        for (error_type, message, expected) in [
            (
                "overloaded_error",
                "Overloaded",
                "Anthropic API overloaded (overloaded_error): Overloaded. Retry in a few seconds.",
            ),
            (
                "api_error",
                "Internal server error.",
                "Anthropic API server error (api_error): Internal server error. \
                 Retry in a few seconds.",
            ),
            (
                "invalid_request_error",
                "messages:\n  bad block",
                "API error (invalid_request_error): messages: bad block",
            ),
        ] {
            assert_eq!(
                format_stream_error(error_type, message),
                expected,
                "{error_type}",
            );
        }
    }
}
