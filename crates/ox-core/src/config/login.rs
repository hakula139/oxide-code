//! OAuth sign-in with PKCE, a loopback callback or pasted code, and credential storage.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

use super::auth::Auth;
use super::oauth::{self, CodeGrant, OAUTH_CLIENT_ID, OAUTH_SCOPES};
use crate::util::tls::Trust;

const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// Claude Code's sign-in asks for this Console scope beside the refresh scopes, and its consent
/// page expects it.
const CONSOLE_SCOPE: &str = "org:create_api_key";
const SUCCESS_URL: &str = "https://platform.claude.com/oauth/code/success?app=claude-code";
/// Claude Code's manual redirect, a page that shows the code as `code#state` for the user to paste.
const MANUAL_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const INCOMPLETE_CODE: &str = "That is not a complete sign-in code. \
                               Copy the whole code from the sign-in page and paste it again.";
const MAX_REQUEST_BYTES: usize = 8 * 1024;

// ── PendingLogin ──

/// A sign-in waiting for the browser to return to the loopback listener, or for the code the manual
/// redirect page shows.
pub(crate) struct PendingLogin {
    listener: TcpListener,
    url: String,
    manual_url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
}

impl PendingLogin {
    /// Binds the listener on a free loopback port and builds the URLs to sign in at, which share
    /// the verifier and `state`.
    pub(crate) async fn start() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to start the sign-in listener")?;
        let port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
        let verifier = random_token()?;
        let state = random_token()?;
        let challenge = code_challenge(&verifier);
        let url = authorize_url(&redirect_uri, &challenge, &state);
        let manual_url = authorize_url(MANUAL_REDIRECT_URI, &challenge, &state);
        Ok(Self {
            listener,
            url,
            manual_url,
            redirect_uri,
            verifier,
            state,
        })
    }

    /// The URL whose redirect returns to the listener.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The URL whose redirect shows the code to paste, which a browser on any machine can use.
    pub(crate) fn manual_url(&self) -> &str {
        &self.manual_url
    }

    /// Waits for the browser to return to the listener. Fails if the browser refuses the sign-in,
    /// or if nothing returns in time, which also ends the wait for a pasted code. Cancel-safe.
    pub(crate) async fn await_redirect(&self) -> Result<Grant> {
        let Ok(callback) = tokio::time::timeout(LOGIN_TIMEOUT, self.await_code()).await else {
            bail!(
                "the sign-in did not finish within {} minutes",
                LOGIN_TIMEOUT.as_secs() / 60
            );
        };
        let (browser, code) = callback?;
        Ok(Grant::Browser(browser, code))
    }

    /// Takes the `code#state` the manual redirect page shows, as the user pasted it. The error is a
    /// complete message with its recovery step.
    pub(crate) fn paste(&self, input: &str) -> Result<Grant> {
        let Some((code, state)) = input
            .trim()
            .split_once('#')
            .filter(|(code, state)| !code.is_empty() && !state.is_empty())
        else {
            bail!(INCOMPLETE_CODE);
        };
        if state != self.state {
            if self.state.starts_with(state) {
                bail!(INCOMPLETE_CODE);
            }
            bail!(
                "That code is not from this sign-in. \
                 Sign in at the URL above and paste the whole code it shows."
            );
        }
        Ok(Grant::Pasted(code.to_owned()))
    }

    /// Exchanges the code of `grant` for tokens at `token_url` and stores them as the oxide-code
    /// login, then sends a browser that returned to the listener to the result page.
    pub(crate) async fn finish(
        self,
        grant: Grant,
        token_url: &str,
        trust: &dyn Trust,
    ) -> Result<Auth> {
        let (mut browser, code, redirect_uri) = match grant {
            Grant::Browser(browser, code) => (Some(browser), code, self.redirect_uri.as_str()),
            Grant::Pasted(code) => (None, code, MANUAL_REDIRECT_URI),
        };
        let grant = CodeGrant {
            code: &code,
            redirect_uri,
            code_verifier: &self.verifier,
            state: &self.state,
        };
        let result = async {
            let tokens = oauth::exchange_code(token_url, &grant, trust).await?;
            Ok(Auth::stored(oauth::save_login(&tokens, token_url).await?))
        }
        .await;
        if let Some(browser) = &mut browser {
            let reply = match &result {
                Ok(_) => redirect(SUCCESS_URL),
                Err(_) => plain_text(
                    "500 Internal Server Error",
                    "Sign-in failed. Return to the terminal for details.",
                ),
            };
            respond(browser, &reply).await;
        }
        result
    }

    /// Serves the listener until a request reaches the callback, keeping that connection open for
    /// the reply. Connections are read concurrently, so an idle one never blocks the redirect.
    async fn await_code(&self) -> Result<(TcpStream, String)> {
        let mut reads = FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.context("failed to accept the browser redirect")?;
                    reads.push(read_request_target(stream));
                }
                Some((mut stream, target)) = reads.next() => {
                    let target = match target {
                        Ok(target) => target,
                        Err(e) => {
                            debug!("ignoring a sign-in listener request: {e:#}");
                            continue;
                        }
                    };
                    match parse_callback(&target, &self.state) {
                        Callback::Elsewhere => {
                            respond(&mut stream, &plain_text("404 Not Found", "Not found")).await;
                        }
                        Callback::Foreign => {
                            debug!("refusing a callback from another sign-in attempt");
                            let reply = plain_text("400 Bad Request", "Unknown sign-in attempt");
                            respond(&mut stream, &reply).await;
                        }
                        Callback::Code(code) => return Ok((stream, code)),
                        Callback::Rejected(reason) => {
                            let reply = plain_text(
                                "400 Bad Request",
                                "Sign-in failed. Return to the terminal for details.",
                            );
                            respond(&mut stream, &reply).await;
                            return Err(reason);
                        }
                    }
                }
            }
        }
    }
}

// ── Grant ──

/// The authorization code a sign-in received, which the exchange sends with the redirect URI that
/// carried it.
pub(crate) enum Grant {
    /// The browser returned to the listener, whose connection stays open for the reply.
    Browser(TcpStream, String),
    /// The user pasted the code from the manual redirect page.
    Pasted(String),
}

// ── PKCE ──

/// 32 bytes from the OS random source, base64url-encoded, as Claude Code makes its verifier and
/// `state`.
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("failed to read the OS random source")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The `S256` challenge for `verifier`.
fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_url(redirect_uri: &str, challenge: &str, state: &str) -> String {
    let scope = [[CONSOLE_SCOPE].as_slice(), OAUTH_SCOPES]
        .concat()
        .join(" ");
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid URL");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", OAUTH_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.into()
}

// ── Callback ──

/// What a request to the listener carried.
#[derive(Debug)]
enum Callback {
    /// A request for some other path, such as a favicon.
    Elsewhere,
    /// A callback without this sign-in's `state`, which the listener refuses and keeps waiting.
    Foreign,
    Code(String),
    /// The callback ended the sign-in: denied, or missing its code.
    Rejected(anyhow::Error),
}

/// Reads a request up to the end of its headers and returns its target, such as
/// `/callback?code=...`.
async fn read_request_target(mut stream: TcpStream) -> (TcpStream, Result<String>) {
    let mut buf = Vec::new();
    let result = async {
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            if buf.len() > MAX_REQUEST_BYTES {
                bail!("request headers exceed {MAX_REQUEST_BYTES} bytes");
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                bail!("connection closed before the request ended");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let line = buf.split(|&b| b == b'\r').next().unwrap_or_default();
        let line = std::str::from_utf8(line).context("request line is not UTF-8")?;
        match line.split(' ').collect::<Vec<_>>()[..] {
            ["GET", target, _version] => Ok(target.to_owned()),
            _ => bail!("unexpected request line {line:?}"),
        }
    }
    .await;
    (stream, result)
}

fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
        return Callback::Elsewhere;
    };
    if url.path() != CALLBACK_PATH {
        return Callback::Elsewhere;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if param("state").as_deref() != Some(expected_state) {
        return Callback::Foreign;
    }
    if let Some(error) = param("error") {
        let detail = match param("error_description") {
            Some(description) => format!("{error}: {description}"),
            None => error,
        };
        return Callback::Rejected(anyhow!("authorization was refused ({detail})"));
    }
    match param("code") {
        Some(code) => Callback::Code(code),
        None => Callback::Rejected(anyhow!(
            "the browser returned without an authorization code"
        )),
    }
}

// ── Responses ──

fn redirect(location: &str) -> String {
    response("302 Found", ("Location", location), "")
}

fn plain_text(status: &str, body: &str) -> String {
    response(status, ("Content-Type", "text/plain; charset=utf-8"), body)
}

fn response(status: &str, (name, value): (&str, &str), body: &str) -> String {
    let head = [
        format!("HTTP/1.1 {status}"),
        format!("{name}: {value}"),
        format!("Content-Length: {}", body.len()),
        "Connection: close".to_owned(),
    ];
    format!("{}\r\n\r\n{body}", head.join("\r\n"))
}

/// Writes `reply` and closes the connection. The browser reply is a courtesy, so a failed write
/// only logs.
async fn respond(stream: &mut TcpStream, reply: &str) {
    if let Err(e) = stream.write_all(reply.as_bytes()).await {
        debug!("failed to answer the browser: {e}");
    }
    _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use indoc::{formatdoc, indoc};
    use serde_json::json;
    use wiremock::matchers::{body_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::auth::AuthSource;
    use crate::config::oauth::{ClaudeCodeKeychain, Login, SessionLogin};
    use crate::util::tls::testing::NoRoots;

    fn port(pending: &PendingLogin) -> u16 {
        pending.listener.local_addr().unwrap().port()
    }

    /// Sends `request` to the listener on `port` and reads the reply until the listener closes.
    async fn send(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply
    }

    const FAILED_PAGE: &str = "Sign-in failed. Return to the terminal for details.";

    async fn browse(port: u16, target: &str) -> String {
        let request = formatdoc! {"
            GET {target} HTTP/1.1\r
            Host: localhost\r
            \r
        "};
        send(port, &request).await
    }

    mod start {
        use super::*;

        /// The value of the query parameter `name` in `url`.
        fn query_param(url: &str, name: &str) -> String {
            reqwest::Url::parse(url)
                .unwrap()
                .query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap()
        }

        #[tokio::test]
        async fn points_the_redirect_at_its_own_listener_and_the_manual_page() {
            let pending = PendingLogin::start().await.unwrap();

            let redirect_uri = format!("http://localhost:{}/callback", port(&pending));
            assert_eq!(query_param(pending.url(), "redirect_uri"), redirect_uri);
            assert_eq!(pending.redirect_uri, redirect_uri);
            assert_eq!(
                query_param(pending.manual_url(), "redirect_uri"),
                MANUAL_REDIRECT_URI
            );
            for url in [pending.url(), pending.manual_url()] {
                assert_eq!(query_param(url, "state"), pending.state);
                assert_eq!(
                    query_param(url, "code_challenge"),
                    code_challenge(&pending.verifier)
                );
            }
            assert_ne!(pending.state, pending.verifier);
        }
    }

    mod paste {
        use super::*;

        #[tokio::test]
        async fn takes_the_code_of_a_matching_state() {
            let pending = PendingLogin::start().await.unwrap();

            let grant = pending.paste(&format!("  auth-code#{}\n", pending.state));

            assert!(
                matches!(&grant, Ok(Grant::Pasted(code)) if code == "auth-code"),
                "{:?}",
                grant.err()
            );
        }

        #[tokio::test]
        async fn rejects_an_incomplete_code_or_one_from_another_sign_in() {
            let pending = PendingLogin::start().await.unwrap();
            let incomplete = "That is not a complete sign-in code. \
                              Copy the whole code from the sign-in page and paste it again.";
            let foreign = "That code is not from this sign-in. \
                           Sign in at the URL above and paste the whole code it shows.";
            let state = pending.state.clone();
            let truncated = &state[..state.len() - 1];

            for (input, expected) in [
                ("auth-code".to_owned(), incomplete),
                (format!("#{state}"), incomplete),
                ("auth-code#".to_owned(), incomplete),
                (String::new(), incomplete),
                (format!("auth-code#{truncated}"), incomplete),
                ("auth-code#other-state".to_owned(), foreign),
                (format!("auth-code#{state}x"), foreign),
            ] {
                let Err(e) = pending.paste(&input) else {
                    panic!("{input:?} must be rejected");
                };
                assert_eq!(e.to_string(), expected, "{input:?}");
            }
        }
    }

    mod finish {
        use super::*;

        /// Waits for the browser redirect, then finishes the sign-in with it.
        async fn sign_in_from_browser(pending: PendingLogin, token_url: &str) -> Result<Auth> {
            let grant = pending.await_redirect().await?;
            pending.finish(grant, token_url, &NoRoots).await
        }

        #[tokio::test]
        async fn exchanges_the_code_and_stores_the_oxide_code_login() {
            let data = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            let pending = PendingLogin::start().await.unwrap();
            Mock::given(method("POST"))
                .and(body_json(json!({
                    "grant_type": "authorization_code",
                    "client_id": OAUTH_CLIENT_ID,
                    "code": "auth-code",
                    "redirect_uri": pending.redirect_uri,
                    "code_verifier": pending.verifier,
                    "state": pending.state,
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "fresh-access",
                    "refresh_token": "fresh-refresh",
                    "expires_in": 3600,
                    "scope": "user:inference user:profile",
                })))
                .expect(1)
                .mount(&server)
                .await;
            let port = port(&pending);
            let token_url = server.uri();
            let target = format!("/callback?code=auth-code&state={}", pending.state);

            let (auth, reply, reloaded) =
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let (auth, reply) = tokio::join!(
                        sign_in_from_browser(pending, &token_url),
                        browse(port, &target)
                    );
                    (
                        auth,
                        reply,
                        SessionLogin::load(Login::Ox, ClaudeCodeKeychain::NONE, &NoRoots).await,
                    )
                })
                .await;

            let auth = auth.unwrap();
            assert_eq!(auth.source(), AuthSource::OxLogin);
            assert_eq!(auth.secret(), "fresh-access");
            assert!(
                reply.starts_with(&format!(
                    "HTTP/1.1 302 Found\r\nLocation: {SUCCESS_URL}\r\n"
                )),
                "{reply}"
            );
            assert_eq!(reloaded.unwrap().cached_token(), "fresh-access");
            let stored: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(data.path().join("ox").join("credentials.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(stored["claudeAiOauth"]["refreshToken"], "fresh-refresh");
            assert_eq!(
                stored["claudeAiOauth"]["scopes"],
                json!(["user:inference", "user:profile"])
            );
        }

        #[tokio::test]
        async fn reports_a_failed_exchange_to_the_browser_and_stores_nothing() {
            let data = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(400).set_body_string("invalid_grant"))
                .mount(&server)
                .await;
            let pending = PendingLogin::start().await.unwrap();
            let port = port(&pending);
            let token_url = server.uri();
            let target = format!("/callback?code=auth-code&state={}", pending.state);

            let (auth, reply) =
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    tokio::join!(
                        sign_in_from_browser(pending, &token_url),
                        browse(port, &target)
                    )
                })
                .await;

            assert_eq!(
                format!("{:#}", auth.unwrap_err()),
                "token exchange failed with HTTP 400 Bad Request: invalid_grant",
            );
            assert!(
                reply.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
                "{reply}"
            );
            assert!(reply.ends_with(FAILED_PAGE), "{reply}");
            assert!(!data.path().join("ox").exists());
        }

        #[tokio::test]
        async fn exchanges_a_pasted_code_with_the_manual_redirect() {
            let data = tempfile::tempdir().unwrap();
            let server = MockServer::start().await;
            let pending = PendingLogin::start().await.unwrap();
            Mock::given(method("POST"))
                .and(body_json(json!({
                    "grant_type": "authorization_code",
                    "client_id": OAUTH_CLIENT_ID,
                    "code": "pasted-code",
                    "redirect_uri": MANUAL_REDIRECT_URI,
                    "code_verifier": pending.verifier,
                    "state": pending.state,
                })))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "pasted-access",
                    "refresh_token": "pasted-refresh",
                    "expires_in": 3600,
                    "scope": "user:inference",
                })))
                .expect(1)
                .mount(&server)
                .await;
            let grant = pending
                .paste(&format!("pasted-code#{}", pending.state))
                .unwrap();
            let token_url = server.uri();

            let auth = temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                pending.finish(grant, &token_url, &NoRoots).await
            })
            .await;

            assert_eq!(auth.unwrap().secret(), "pasted-access");
            assert!(data.path().join("ox").join("credentials.json").exists());
        }
    }

    mod await_redirect {
        use super::*;

        #[tokio::test]
        async fn ends_when_the_browser_returns_a_refusal() {
            let pending = PendingLogin::start().await.unwrap();
            let port = port(&pending);
            let target = format!("/callback?error=access_denied&state={}", pending.state);

            let (grant, reply) = tokio::join!(pending.await_redirect(), browse(port, &target));

            let Err(e) = grant else {
                panic!("a refusal must end the sign-in");
            };
            assert_eq!(e.to_string(), "authorization was refused (access_denied)");
            assert!(reply.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{reply}");
            assert!(reply.ends_with(FAILED_PAGE), "{reply}");
        }

        #[tokio::test(start_paused = true)]
        async fn gives_up_when_nothing_returns() {
            let pending = PendingLogin::start().await.unwrap();

            let Err(e) = pending.await_redirect().await else {
                panic!("the wait must time out");
            };

            assert_eq!(e.to_string(), "the sign-in did not finish within 5 minutes");
        }
    }

    mod await_code {
        use super::*;

        #[tokio::test]
        async fn answers_other_requests_until_the_callback_arrives() {
            let pending = PendingLogin::start().await.unwrap();
            let port = port(&pending);
            let callback = format!(
                "GET /callback?code=auth-code&state={} HTTP/1.1\r\n\r\n",
                pending.state
            );
            let browser = async {
                let idle = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap();
                let not_found = browse(port, "/favicon.ico").await;
                let foreign = browse(port, "/callback?error=access_denied&state=other").await;
                let malformed = send(port, "POST /callback HTTP/1.1\r\n\r\n").await;
                let mut redirect = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                    .await
                    .unwrap();
                redirect.write_all(callback.as_bytes()).await.unwrap();
                (idle, not_found, foreign, malformed, redirect)
            };

            let (code, (_idle, not_found, foreign, malformed, _redirect)) =
                tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(pending.await_code(), browser)
                })
                .await
                .expect("an idle connection must not hold up the others");

            assert_eq!(code.unwrap().1, "auth-code");
            assert!(
                not_found.starts_with("HTTP/1.1 404 Not Found\r\n"),
                "{not_found}"
            );
            assert!(not_found.ends_with("\r\n\r\nNot found"), "{not_found}");
            assert!(
                foreign.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                "{foreign}"
            );
            assert_eq!(malformed, "");
        }
    }

    mod random_token {
        use super::*;

        #[test]
        fn encodes_32_fresh_bytes() {
            let token = random_token().unwrap();
            assert_eq!(URL_SAFE_NO_PAD.decode(&token).unwrap().len(), 32);
            assert_ne!(token, random_token().unwrap());
        }
    }

    mod code_challenge {
        use super::*;

        #[test]
        fn matches_the_rfc_7636_example() {
            assert_eq!(
                code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            );
        }
    }

    mod authorize_url {
        use super::*;

        #[test]
        fn asks_claude_ai_for_a_pkce_code_sent_to_the_loopback_redirect() {
            let url = reqwest::Url::parse(&authorize_url(
                "http://localhost:1234/callback",
                "challenge-1",
                "state-1",
            ))
            .unwrap();

            assert_eq!(url.host_str(), Some("claude.ai"));
            assert_eq!(url.path(), "/oauth/authorize");
            let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
            let expected = [
                ("code", "true"),
                ("client_id", "9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
                ("response_type", "code"),
                ("redirect_uri", "http://localhost:1234/callback"),
                (
                    "scope",
                    "org:create_api_key user:file_upload user:inference user:mcp_servers \
                     user:profile user:sessions:claude_code",
                ),
                ("code_challenge", "challenge-1"),
                ("code_challenge_method", "S256"),
                ("state", "state-1"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()));
            assert_eq!(pairs, expected);
        }
    }

    mod read_request_target {
        use super::*;

        /// Writes `request` from a client that stops sending, and reads its target server-side.
        async fn request_target(request: &[u8]) -> Result<String> {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (server, _) = listener.accept().await.unwrap();
            client.write_all(request).await.unwrap();
            client.shutdown().await.unwrap();
            read_request_target(server).await.1
        }

        #[tokio::test]
        async fn returns_the_target_of_a_get() {
            let request = indoc! {b"
                GET /callback?code=abc HTTP/1.1\r
                Host: localhost\r
                \r
            "};

            let target = request_target(request).await.unwrap();

            assert_eq!(target, "/callback?code=abc");
        }

        #[tokio::test]
        async fn rejects_other_methods_oversized_and_truncated_requests() {
            let oversized = format!("GET /{} HTTP/1.1", "a".repeat(MAX_REQUEST_BYTES + 1024));
            for (request, expected) in [
                (
                    "POST /callback HTTP/1.1\r\n\r\n",
                    r#"unexpected request line "POST /callback HTTP/1.1""#,
                ),
                (oversized.as_str(), "request headers exceed 8192 bytes"),
                (
                    "GET /callback HTTP/1.1\r\n",
                    "connection closed before the request ended",
                ),
            ] {
                let err = request_target(request.as_bytes()).await.unwrap_err();
                assert_eq!(err.to_string(), expected);
            }
        }
    }

    mod parse_callback {
        use super::*;

        #[test]
        fn takes_the_code_of_a_matching_state() {
            assert!(matches!(
                parse_callback("/callback?code=abc&state=s1", "s1"),
                Callback::Code(code) if code == "abc"
            ));
        }

        #[test]
        fn ignores_other_paths() {
            for target in [
                "/favicon.ico",
                "/",
                "/callback/extra?code=abc&state=s1",
                ":99999",
            ] {
                assert!(
                    matches!(parse_callback(target, "s1"), Callback::Elsewhere),
                    "{target}"
                );
            }
        }

        #[test]
        fn refuses_a_callback_from_another_attempt() {
            for target in [
                "/callback?code=abc&state=s2",
                "/callback?code=abc",
                "/callback?error=access_denied&state=s2",
                "/callback?error=access_denied",
            ] {
                assert!(
                    matches!(parse_callback(target, "s1"), Callback::Foreign),
                    "{target}"
                );
            }
        }

        #[test]
        fn rejects_refusals_and_missing_codes() {
            for (target, expected) in [
                (
                    "/callback?error=access_denied&error_description=User%20declined&state=s1",
                    "authorization was refused (access_denied: User declined)",
                ),
                (
                    "/callback?error=access_denied&state=s1",
                    "authorization was refused (access_denied)",
                ),
                (
                    "/callback?state=s1",
                    "the browser returned without an authorization code",
                ),
            ] {
                match parse_callback(target, "s1") {
                    Callback::Rejected(e) => assert_eq!(e.to_string(), expected, "{target}"),
                    other => panic!("{target}: expected a rejection, got {other:?}"),
                }
            }
        }
    }
}
