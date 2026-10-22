//! The engine's `/login` and `/logout`, which swap the client's credentials.

use anyhow::anyhow;
use tokio::sync::mpsc;

use super::abort::{Inbox, TurnAbort};
use super::{Engine, TurnEnd, abort, report_abort};
use crate::config::auth::{self, Auth, AuthSource};
use crate::config::login::PendingLogin;
use crate::config::oauth;
use crate::protocol::{Event, EventSink, Op};
use crate::util::text::join_sentences;

impl Engine {
    /// Fails while signed out.
    /// A session that a transient failure signed out first loads the stored login again,
    /// and keeps doing so on later prompts and compactions while the failure stays transient.
    /// An interrupt or a closed channel stops only the wait.
    pub(super) async fn ensure_signed_in(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
        inbox: &mut Inbox,
    ) -> Result<(), TurnAbort> {
        if self.client.auth_source().is_some() {
            return Ok(());
        }
        if !self.retry_login {
            return Err(signed_out());
        }
        let retry = self.client.retry_stored_login(self.claude_code_keychain);
        match abort::await_unless_aborted(retry, ops, inbox, sink).await? {
            Ok(auth) => {
                let source = auth.source();
                self.set_stored_login(Some(auth), false);
                sink.emit(Event::LoginRestored { auth: source }, "login-restored");
                Ok(())
            }
            Err(unavailable) => {
                self.retry_login = unavailable.retry;
                Err(TurnAbort::Failed(anyhow!(unavailable.notice)))
            }
        }
    }

    /// `retry` says whether the next prompt or compaction loads the stored login again.
    fn set_stored_login(&mut self, auth: Option<Auth>, retry: bool) {
        self.client.set_auth(auth);
        self.retry_login = retry;
    }

    /// Signs in through the browser and stores the new oxide-code login.
    pub(super) async fn login(
        &mut self,
        sink: &dyn EventSink,
        ops: &mut mpsc::Receiver<Op>,
    ) -> TurnEnd {
        let pending = match PendingLogin::start().await {
            Ok(pending) => pending,
            Err(e) => return report_abort(sink, sign_in_failed(&e), "login-failed"),
        };
        sink.emit(
            Event::LoginStarted {
                url: pending.url().to_owned(),
            },
            "login-started",
        );

        let mut inbox = Inbox::default();
        let finish = pending.finish(&self.oauth_token_url, self.client.trust());
        let signed_in = abort::await_unless_aborted(finish, ops, &mut inbox, sink).await;
        self.adopt_mode(&mut inbox);
        match signed_in {
            Ok(Ok(auth)) => {
                self.adopt_login(sink, auth);
                TurnEnd::Completed
            }
            Ok(Err(e)) => report_abort(sink, sign_in_failed(&e), "login-failed"),
            Err(abort) => report_abort(sink, abort, "login-failed"),
        }
    }

    /// Uses a new oxide-code login for requests unless an env var or `client.api_key` outranks it.
    fn adopt_login(&mut self, sink: &dyn EventSink, auth: Auth) {
        let active = match self.client.auth_source() {
            Some(source) if !source.is_stored_login() => source,
            _ => {
                self.set_stored_login(Some(auth), false);
                AuthSource::OxLogin
            }
        };
        sink.emit(Event::SignedIn { auth: active }, "signed-in");
    }

    /// Removes the oxide-code login. A session using it, or one signed out, falls back to the
    /// Claude Code login, and stays signed out without a usable one.
    pub(super) async fn logout(&mut self, sink: &dyn EventSink) {
        let removed = match oauth::delete_login().await {
            Ok(removed) => removed,
            Err(e) => {
                sink.emit(
                    Event::TurnFailed(format!("Sign-out failed: {e:#}")),
                    "logout-failed",
                );
                return;
            }
        };
        let mut fallback_error = None;
        if self
            .client
            .auth_source()
            .is_none_or(|source| source == AuthSource::OxLogin)
        {
            let mut retry = false;
            let fallback = auth::claude_code_login(self.claude_code_keychain, self.client.trust())
                .await
                .unwrap_or_else(|e| {
                    retry = auth::is_transient(&e);
                    fallback_error = Some(format!("{e:#}"));
                    None
                });
            self.set_stored_login(fallback, retry);
        }
        sink.emit(
            Event::SignedOut {
                removed,
                auth: self.client.auth_source(),
                fallback_error,
            },
            "signed-out",
        );
    }
}

fn signed_out() -> TurnAbort {
    TurnAbort::Failed(anyhow!(auth::SIGNED_OUT))
}

fn sign_in_failed(e: &anyhow::Error) -> TurnAbort {
    let message = join_sentences(
        &format!("Sign-in failed: {e:#}"),
        "Run `/login` to try again.",
    );
    TurnAbort::Failed(anyhow!(message))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use indoc::formatdoc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::oauth::testing::write_creds;
    use crate::engine::local_testing::{inert_ops, login_url, submit, tool_heavy_transcript};
    use crate::engine::testing::{engine, mount_reply, with_pinned_home};
    use crate::permission::Mode;
    use crate::protocol::testing::CapturingSink;
    use crate::session::store::testing::test_store;

    /// Stores an oxide-code login under `data`, standing in for `$XDG_DATA_HOME`.
    fn store_ox_login(data: &Path) -> PathBuf {
        let file = data.join("ox").join("credentials.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_creds(&file, "ox-token", None, 9_999_999_999_999);
        file
    }

    mod ensure_signed_in {
        use super::*;

        /// The authorization header of the first Messages request `server` received.
        async fn sent_authorization(server: &MockServer) -> String {
            let requests = server.received_requests().await.unwrap();
            let request = requests
                .iter()
                .find(|request| request.url.path() == "/v1/messages")
                .expect("a request reaches the API");
            request.headers["authorization"]
                .to_str()
                .unwrap()
                .to_owned()
        }

        /// Stores an expired, refreshable oxide-code login under `data` and holds its lock,
        /// so loading it fails transiently after the lock wait.
        fn hold_expired_ox_login(data: &Path) {
            let file = store_ox_login(data);
            write_creds(&file, "dead", Some("refresh"), 0);
            std::fs::create_dir(data.join("ox").join("credentials.lock")).unwrap();
        }

        #[tokio::test]
        async fn restores_the_stored_login_and_goes_on_with_the_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                store_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine.client.set_auth(None);
                    engine.retry_login = true;
                    let sink = CapturingSink::default();
                    let mut ops = inert_ops();

                    let end = engine.submit_prompt("hi", &sink, &mut ops).await;

                    assert_eq!(end, TurnEnd::Completed);
                    assert!(!engine.retry_login);
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.first(),
                            Some(Event::LoginRestored {
                                auth: AuthSource::OxLogin
                            })
                        ),
                        "{events:?}"
                    );
                    assert!(
                        matches!(events.last(), Some(Event::TurnComplete)),
                        "{events:?}"
                    );
                    assert_eq!(sent_authorization(&server).await, "Bearer ox-token");
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn restores_the_stored_login_and_goes_on_with_the_compaction() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                mount_reply(&server, "summary").await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                store_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine.messages = tool_heavy_transcript();
                    engine.client.set_auth(None);
                    engine.retry_login = true;
                    let sink = CapturingSink::default();
                    let mut ops = inert_ops();

                    let end = engine.compact(&sink, &mut ops, None).await;

                    let events = sink.events();
                    assert_eq!(end, TurnEnd::Completed, "{events:?}");
                    assert!(
                        matches!(
                            events.first(),
                            Some(Event::LoginRestored {
                                auth: AuthSource::OxLogin
                            })
                        ),
                        "{events:?}"
                    );
                    assert!(
                        events
                            .iter()
                            .any(|event| matches!(event, Event::SessionCompacted { .. })),
                        "{events:?}"
                    );
                    assert_eq!(sent_authorization(&server).await, "Bearer ox-token");
                })
                .await;
            })
            .await;
        }

        #[tokio::test(start_paused = true)]
        async fn keeps_retrying_while_the_failure_stays_transient() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                hold_expired_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine = engine(
                        "http://127.0.0.1:9".to_owned(),
                        &test_store(dir.path()),
                        dir.path(),
                    )
                    .await;
                    engine.client.set_auth(None);
                    engine.retry_login = true;
                    let sink = CapturingSink::default();
                    let mut ops = inert_ops();

                    let end = engine.submit_prompt("hi", &sink, &mut ops).await;

                    assert_eq!(end, TurnEnd::Failed);
                    assert!(engine.retry_login);
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.as_slice(),
                            [Event::TurnFailed(notice)]
                                if notice.ends_with("The next prompt tries the stored login again.")
                        ),
                        "{events:?}"
                    );
                })
                .await;
            })
            .await;
        }

        #[tokio::test(start_paused = true)]
        async fn an_interrupt_ends_the_wait_and_the_reload_still_finishes() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                hold_expired_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine = engine(
                        "http://127.0.0.1:9".to_owned(),
                        &test_store(dir.path()),
                        dir.path(),
                    )
                    .await;
                    engine.client.set_auth(None);
                    engine.retry_login = true;
                    let sink = CapturingSink::default();
                    let (tx, mut ops) = mpsc::channel(1);
                    tx.try_send(Op::Interrupt).unwrap();
                    let start = tokio::time::Instant::now();

                    let end = engine.submit_prompt("hi", &sink, &mut ops).await;
                    let waited = start.elapsed();
                    engine.client.finish_reloads().await;

                    assert_eq!(end, TurnEnd::Cancelled);
                    assert!(waited < Duration::from_secs(1), "{waited:?}");
                    assert!(engine.retry_login);
                    assert_eq!(engine.client.auth_source(), None);
                    assert!(matches!(sink.events().as_slice(), [Event::Cancelled]));
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn stops_retrying_once_the_failure_is_not_transient() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine = engine(
                        "http://127.0.0.1:9".to_owned(),
                        &test_store(dir.path()),
                        dir.path(),
                    )
                    .await;
                    engine.client.set_auth(None);
                    engine.retry_login = true;
                    let sink = CapturingSink::default();
                    let mut ops = inert_ops();

                    let end = engine.submit_prompt("hi", &sink, &mut ops).await;

                    assert_eq!(end, TurnEnd::Failed);
                    assert!(!engine.retry_login);
                    assert_eq!(engine.client.auth_source(), None);
                    assert!(
                        matches!(
                            sink.events().as_slice(),
                            [Event::TurnFailed(notice)]
                                if notice == "Not signed in. Run `/login` to sign in."
                        ),
                        "{:?}",
                        sink.events()
                    );
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn reads_no_store_without_a_transient_sign_out() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                store_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine = engine(
                        "http://127.0.0.1:9".to_owned(),
                        &test_store(dir.path()),
                        dir.path(),
                    )
                    .await;
                    engine.client.set_auth(None);
                    let sink = CapturingSink::default();
                    let mut ops = inert_ops();

                    let end = engine.submit_prompt("hi", &sink, &mut ops).await;

                    assert_eq!(end, TurnEnd::Failed);
                    assert_eq!(engine.client.auth_source(), None);
                    assert!(
                        matches!(
                            sink.events().as_slice(),
                            [Event::TurnFailed(notice)]
                                if notice == "Not signed in. Run `/login` to sign in."
                        ),
                        "{:?}",
                        sink.events()
                    );
                })
                .await;
            })
            .await;
        }
    }

    mod login {
        use super::*;

        /// Returns to the sign-in listener behind `url` with `query` and its `state`, as the
        /// browser does, and reads the reply.
        async fn return_from_browser(url: &str, query: &str) -> String {
            let url = reqwest::Url::parse(url).unwrap();
            let param = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
                    .unwrap()
            };
            let redirect = reqwest::Url::parse(&param("redirect_uri")).unwrap();
            let mut stream =
                tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
                    .await
                    .unwrap();
            let state = param("state");
            let request = formatdoc! {"
                GET /callback?{query}&state={state} HTTP/1.1\r
                Host: localhost\r
                \r
            "};
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            reply
        }

        #[tokio::test]
        async fn stores_the_login_from_the_browser_and_sends_it_with_the_next_prompt() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(path("/token"))
                    .and(body_partial_json(
                        serde_json::json!({ "code": "auth-code" }),
                    ))
                    .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "access_token": "fresh-access",
                        "refresh_token": "fresh-refresh",
                        "expires_in": 3600,
                        "scope": "user:inference",
                    })))
                    .expect(1)
                    .mount(&server)
                    .await;
                mount_reply(&server, "reply").await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine.client.set_auth(None);
                    engine.oauth_token_url = format!("{}/token", server.uri());
                    let mut handle = engine.spawn();

                    handle.ops.send(Op::Login).await.unwrap();
                    let url = login_url(&mut handle.events).await;
                    let reply = return_from_browser(&url, "code=auth-code").await;
                    let signed_in = handle.events.recv().await;
                    let turn = submit(&mut handle, "hi").await;
                    handle.shutdown().await.unwrap();

                    assert!(reply.starts_with("HTTP/1.1 302 Found\r\n"), "{reply}");
                    assert!(
                        matches!(
                            signed_in,
                            Some(Event::SignedIn {
                                auth: AuthSource::OxLogin
                            })
                        ),
                        "{signed_in:?}"
                    );
                    assert!(matches!(turn.last(), Some(Event::TurnComplete)), "{turn:?}");
                    let requests = server.received_requests().await.unwrap();
                    let prompt = requests
                        .iter()
                        .find(|request| request.url.path() == "/v1/messages")
                        .expect("the prompt reaches the API");
                    assert_eq!(prompt.headers["authorization"], "Bearer fresh-access");
                    assert!(data.path().join("ox").join("credentials.json").exists());
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn fails_the_sign_in_when_the_browser_returns_a_refusal() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let mut handle = engine.spawn();

                handle.ops.send(Op::Login).await.unwrap();
                let url = login_url(&mut handle.events).await;
                let reply = return_from_browser(&url, "error=access_denied").await;
                let failed = handle.events.recv().await;
                handle.shutdown().await.unwrap();

                assert!(reply.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{reply}");
                assert!(
                    matches!(
                        &failed,
                        Some(Event::TurnFailed(msg))
                            if msg == "Sign-in failed: authorization was refused (access_denied). \
                                       Run `/login` to try again."
                    ),
                    "{failed:?}"
                );
            })
            .await;
        }

        #[tokio::test]
        async fn keeps_a_mode_switch_sent_before_a_cancel() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let (ops_tx, mut ops) = mpsc::channel(2);
                ops_tx.try_send(Op::SetMode(Mode::Plan)).unwrap();
                ops_tx.try_send(Op::Interrupt).unwrap();

                let end = engine.login(&sink, &mut ops).await;

                assert_eq!(end, TurnEnd::Cancelled);
                assert_eq!(engine.policy.mode(), Mode::Plan);
            })
            .await;
        }
    }

    mod adopt_login {
        use super::*;

        #[tokio::test]
        async fn replaces_a_stored_or_missing_login_but_not_an_explicit_credential() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let mut engine = engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                let sink = CapturingSink::default();
                let ox_login = || Auth::new(AuthSource::OxLogin, "ox-token");

                engine.adopt_login(&sink, ox_login());
                assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
                for before in [
                    None,
                    Some(Auth::new(AuthSource::ClaudeCodeLogin, "claude-code-token")),
                ] {
                    engine.client.set_auth(before);
                    engine.retry_login = true;
                    engine.adopt_login(&sink, ox_login());
                    assert_eq!(engine.client.auth().unwrap().secret(), "ox-token");
                    assert!(!engine.retry_login);
                }

                let events = sink.events();
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            Event::SignedIn {
                                auth: AuthSource::ApiKeyEnv
                            },
                            Event::SignedIn {
                                auth: AuthSource::OxLogin
                            },
                            Event::SignedIn {
                                auth: AuthSource::OxLogin
                            },
                        ]
                    ),
                    "{events:?}"
                );
            })
            .await;
        }
    }

    mod logout {
        use super::*;

        #[tokio::test]
        async fn removes_the_oxide_code_login_and_keeps_an_explicit_credential() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                let file = store_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    let sink = CapturingSink::default();

                    engine.logout(&sink).await;

                    assert!(!file.exists());
                    assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.as_slice(),
                            [Event::SignedOut {
                                removed: true,
                                auth: Some(AuthSource::ApiKeyEnv),
                                fallback_error: None,
                            }]
                        ),
                        "{events:?}"
                    );
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn falls_back_to_the_claude_code_login_or_stays_signed_out() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                store_ox_login(data.path());
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    engine
                        .client
                        .set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
                    let sink = CapturingSink::default();
                    let claude_code_file = std::env::home_dir()
                        .unwrap()
                        .join(".claude")
                        .join(".credentials.json");

                    engine.logout(&sink).await;
                    std::fs::create_dir(claude_code_file.parent().unwrap()).unwrap();
                    std::fs::write(&claude_code_file, "not json").unwrap();
                    engine.logout(&sink).await;
                    write_creds(
                        &claude_code_file,
                        "claude-code-token",
                        None,
                        9_999_999_999_999,
                    );
                    engine.logout(&sink).await;

                    assert_eq!(engine.client.auth().unwrap().secret(), "claude-code-token");
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.as_slice(),
                            [
                                Event::SignedOut {
                                    removed: true,
                                    auth: None,
                                    fallback_error: None,
                                },
                                Event::SignedOut {
                                    removed: false,
                                    auth: None,
                                    fallback_error: Some(unreadable),
                                },
                                Event::SignedOut {
                                    removed: false,
                                    auth: Some(AuthSource::ClaudeCodeLogin),
                                    fallback_error: None,
                                },
                            ] if unreadable.ends_with("Run `claude` to sign in again.")
                        ),
                        "{events:?}"
                    );
                })
                .await;
            })
            .await;
        }

        #[tokio::test]
        async fn reports_a_login_it_cannot_remove() {
            with_pinned_home(async {
                let server = MockServer::start().await;
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                std::fs::create_dir_all(data.path().join("ox").join("credentials.json")).unwrap();
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine =
                        engine(server.uri(), &test_store(dir.path()), dir.path()).await;
                    let sink = CapturingSink::default();

                    engine.logout(&sink).await;

                    assert_eq!(engine.client.auth_source(), Some(AuthSource::ApiKeyEnv));
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.as_slice(),
                            [Event::TurnFailed(msg)]
                                if msg.starts_with("Sign-out failed: failed to remove ")
                        ),
                        "{events:?}"
                    );
                })
                .await;
            })
            .await;
        }

        #[tokio::test(start_paused = true)]
        async fn retries_a_claude_code_login_a_transient_failure_kept_from_taking_over() {
            with_pinned_home(async {
                let dir = tempfile::tempdir().unwrap();
                let data = tempfile::tempdir().unwrap();
                store_ox_login(data.path());
                let home = std::env::home_dir().unwrap();
                let claude_code_file = home.join(".claude").join(".credentials.json");
                std::fs::create_dir(home.join(".claude")).unwrap();
                write_creds(&claude_code_file, "dead", Some("refresh"), 0);
                let lock = home.join(".claude.lock");
                std::fs::create_dir(&lock).unwrap();
                temp_env::async_with_vars([("XDG_DATA_HOME", Some(data.path()))], async {
                    let mut engine = engine(
                        "http://127.0.0.1:9".to_owned(),
                        &test_store(dir.path()),
                        dir.path(),
                    )
                    .await;
                    engine
                        .client
                        .set_auth(Some(Auth::new(AuthSource::OxLogin, "ox-token")));
                    let sink = CapturingSink::default();

                    engine.logout(&sink).await;
                    let retrying = engine.retry_login;
                    std::fs::remove_dir(&lock).unwrap();
                    write_creds(
                        &claude_code_file,
                        "claude-code-token",
                        None,
                        9_999_999_999_999,
                    );
                    let restored = engine
                        .ensure_signed_in(&sink, &mut inert_ops(), &mut Inbox::default())
                        .await;

                    assert!(retrying);
                    assert!(restored.is_ok(), "the next prompt loads the login again");
                    assert_eq!(engine.client.auth().unwrap().secret(), "claude-code-token");
                    let events = sink.events();
                    assert!(
                        matches!(
                            events.as_slice(),
                            [
                                Event::SignedOut {
                                    removed: true,
                                    auth: None,
                                    fallback_error: Some(error),
                                },
                                Event::LoginRestored {
                                    auth: AuthSource::ClaudeCodeLogin
                                },
                            ] if error.ends_with("Try again in a moment.")
                        ),
                        "{events:?}"
                    );
                })
                .await;
            })
            .await;
        }
    }

    mod sign_in_failed {
        use super::*;

        #[test]
        fn keeps_a_cause_that_ends_a_sentence_verbatim() {
            let cause = anyhow!("token exchange failed with HTTP 400 Bad Request: Code expired.");

            let abort = sign_in_failed(&cause);

            assert_eq!(
                abort.into_failure().to_string(),
                "Sign-in failed: token exchange failed with HTTP 400 Bad Request: Code expired. \
                 Run `/login` to try again.",
            );
        }
    }
}
