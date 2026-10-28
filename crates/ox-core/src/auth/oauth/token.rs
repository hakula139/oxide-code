//! Requests to the OAuth token endpoint: the authorization-code exchange and the refresh.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::{OAUTH_CLIENT_ID, OAUTH_SCOPES, Token, now_millis};
use crate::util::response::{self, condense, http_status};
use crate::util::tls::Trust;

pub(super) const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Tokens the token endpoint issues for a code exchange or a refresh.
#[cfg_attr(test, derive(Debug))]
#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub(super) access_token: String,
    pub(super) refresh_token: String,
    pub(super) expires_in: u64,
    pub(super) scope: Option<String>,
}

impl TokenResponse {
    /// The issued access token, expiring `expires_in` seconds from now. A clock set before 1970
    /// records it as expired, as [`is_near_expiry`](super::is_near_expiry) reads such a clock.
    pub(super) fn token(&self) -> Token {
        Token {
            access_token: self.access_token.clone(),
            expires_at_ms: now_millis().map_or(0, |now| {
                now.saturating_add(self.expires_in.saturating_mul(1000))
            }),
            unsaved: None,
        }
    }
}

/// The authorization-code grant `/login` sends once the browser returns or the user pastes a code.
#[derive(Serialize)]
pub(super) struct CodeGrant<'a> {
    pub(super) code: &'a str,
    pub(super) redirect_uri: &'a str,
    pub(super) code_verifier: &'a str,
    pub(super) state: &'a str,
}

#[derive(Serialize)]
struct CodeExchangeRequest<'a> {
    grant_type: &'a str,
    client_id: &'a str,
    #[serde(flatten)]
    grant: &'a CodeGrant<'a>,
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: &'a str,
}

/// Trades an authorization code for tokens.
pub(super) async fn exchange_code(
    url: &str,
    grant: &CodeGrant<'_>,
    trust: &dyn Trust,
) -> Result<TokenResponse> {
    let request = CodeExchangeRequest {
        grant_type: "authorization_code",
        client_id: OAUTH_CLIENT_ID,
        grant,
    };
    request_tokens(url, "token exchange", &request, trust)
        .await
        .map_err(|e| e.error)
}

pub(super) async fn refresh_oauth_token(
    url: &str,
    refresh_token: &str,
    trust: &dyn Trust,
) -> Result<TokenResponse, TokenRequestError> {
    let scope = OAUTH_SCOPES.join(" ");
    let request = RefreshRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: OAUTH_CLIENT_ID,
        scope: &scope,
    };
    request_tokens(url, "token refresh", &request, trust).await
}

/// A failed token request.
#[cfg_attr(test, derive(Debug))]
pub(super) struct TokenRequestError {
    pub(super) error: anyhow::Error,
    /// The endpoint sent no response or a server error, so the request may succeed when sent again.
    pub(super) transient: bool,
}

impl From<anyhow::Error> for TokenRequestError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            transient: false,
        }
    }
}

/// Posts `body` to the token endpoint. `what` names the request in errors.
async fn request_tokens(
    url: &str,
    what: &str,
    body: &impl Serialize,
    trust: &dyn Trust,
) -> Result<TokenResponse, TokenRequestError> {
    let client = trust
        .apply(reqwest::Client::builder().timeout(TOKEN_REQUEST_TIMEOUT))?
        .build()
        .context("failed to build OAuth HTTP client")?;

    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("failed to send {what} request"))
        .map_err(|error| TokenRequestError {
            error,
            transient: true,
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        debug!(
            status = status.as_u16(),
            body = body.as_str(),
            "OAuth token endpoint error"
        );
        let cause = http_status(status);
        let error = match token_error_details(&body) {
            Some(details) => anyhow!("{what} failed with {cause}: {details}"),
            None => anyhow!("{what} failed with {cause}"),
        };
        return Err(TokenRequestError {
            error,
            transient: status.is_server_error(),
        });
    }

    Ok(response
        .json()
        .await
        .with_context(|| format!("failed to parse {what} response"))?)
}

/// What a token endpoint's error `body` says, [condensed](condense): RFC 6749's
/// `error_description`, or else its `error` code, or else [`response::body_details`].
fn token_error_details(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct TokenError {
        error: String,
        error_description: Option<String>,
    }

    match serde_json::from_str::<TokenError>(body) {
        Ok(TokenError {
            error,
            error_description,
        }) => error_description
            .as_deref()
            .and_then(condense)
            .or_else(|| condense(&error)),
        Err(_) => response::body_details(body),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::oauth::testing::ok_refresh_body;
    use crate::util::tls::PlatformTrust;
    use crate::util::tls::testing::{NoRoots, TEST_CA_PATH};

    mod refresh_oauth_token {
        use super::*;

        #[tokio::test]
        async fn sends_grant_and_client_id_and_produces_parsed_response() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                    "new-access",
                    "new-refresh",
                    3600,
                )))
                .mount(&server)
                .await;

            let response = refresh_oauth_token(&server.uri(), "old-refresh", &NoRoots)
                .await
                .unwrap();

            assert_eq!(response.access_token, "new-access");
            assert_eq!(response.refresh_token, "new-refresh");
            assert_eq!(response.expires_in, 3600);
            assert_eq!(
                response.scope.as_deref(),
                Some("user:profile user:inference")
            );

            let received = server.received_requests().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
            assert_eq!(
                body,
                serde_json::json!({
                    "grant_type": "refresh_token",
                    "refresh_token": "old-refresh",
                    "client_id": OAUTH_CLIENT_ID,
                    "scope": OAUTH_SCOPES.join(" "),
                }),
            );
        }

        #[tokio::test]
        async fn propagates_http_error_with_status_and_condensed_body() {
            for (status, body, expected, transient) in [
                (
                    401,
                    r#"{"error":"invalid_grant","error_description":"Refresh token\nnot found"}"#,
                    "token refresh failed with HTTP 401 Unauthorized: Refresh token not found",
                    false,
                ),
                (
                    522,
                    "<html><title>example.com | 522: Connection timed out</title></html>",
                    "token refresh failed with HTTP 522: example.com | 522: Connection timed out",
                    true,
                ),
            ] {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(wm_path("/"))
                    .respond_with(ResponseTemplate::new(status).set_body_string(body))
                    .mount(&server)
                    .await;

                let err = refresh_oauth_token(&server.uri(), "bad", &NoRoots)
                    .await
                    .expect_err("expected HTTP error");

                assert_eq!(format!("{:#}", err.error), expected);
                assert_eq!(err.transient, transient, "{status}");
            }
        }

        #[tokio::test]
        async fn malformed_json_errors_with_context() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_string("<not json>"))
                .mount(&server)
                .await;

            let err = refresh_oauth_token(&server.uri(), "tok", &NoRoots)
                .await
                .expect_err("expected parse error");
            assert_eq!(
                format!("{:#}", err.error),
                format!(
                    "failed to parse token refresh response: \
                     error decoding response body for url ({}/): \
                     expected value at line 1 column 1",
                    server.uri(),
                ),
            );
            assert!(!err.transient, "a malformed reply is not retried");
        }

        #[tokio::test]
        async fn honors_extra_ca_certs() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                    "new-access",
                    "new-refresh",
                    3600,
                )))
                .mount(&server)
                .await;

            let response = refresh_oauth_token(
                &server.uri(),
                "refresh",
                &PlatformTrust(Some(TEST_CA_PATH.into())),
            )
            .await
            .expect("valid PEM must build a client and produce a response");
            assert_eq!(response.access_token, "new-access");
        }

        #[tokio::test]
        async fn surfaces_extra_ca_certs_error_with_path() {
            let missing = Path::new("/definitely/does/not/exist.pem");
            let err = refresh_oauth_token(
                "http://unused.invalid/",
                "refresh",
                &PlatformTrust(Some(missing.to_path_buf())),
            )
            .await
            .expect_err("missing CA bundle must error");
            let msg = format!("{:#}", err.error);
            assert!(msg.contains("failed to read extra CA bundle"), "{msg}");
            assert!(msg.contains("/definitely/does/not/exist.pem"), "{msg}");
        }
    }

    mod token_error_details {
        use super::*;

        #[test]
        fn prefers_the_description_then_the_code_then_the_page_title() {
            for (body, expected) in [
                (
                    r#"{"error":"invalid_grant","error_description":"Token revoked."}"#,
                    Some("Token revoked."),
                ),
                (r#"{"error":"invalid_grant"}"#, Some("invalid_grant")),
                (
                    r#"{"error":"invalid_grant","error_description":" "}"#,
                    Some("invalid_grant"),
                ),
                (
                    "<!DOCTYPE html>\n<html><head><title>503 Service Unavailable</title>\n\
                     <style>body { margin: 0 }</style></head><body>...</body></html>",
                    Some("503 Service Unavailable"),
                ),
                ("", None),
            ] {
                assert_eq!(token_error_details(body).as_deref(), expected, "{body:?}");
            }
        }
    }
}
