//! OAuth logins: the browser sign-in behind `/login`, and the stored logins, oxide-code's own and
//! Claude Code's. Loads a login's token, refreshes it near expiry, and writes rotated tokens back
//! to the store they came from.

mod keychain;
mod lock;
pub(crate) mod login;
mod store;
mod token;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::task::TaskTracker;

use self::keychain::Keychain;
#[cfg(target_os = "macos")]
use self::keychain::SystemKeychain;
use self::lock::LOCK_WAIT;
use self::store::{Store, load_token_from};
use self::token::{Token, TokenResponse, is_near_expiry};
use crate::util::path::{CLAUDE_CREDENTIALS, claude_dir, tildify};
use crate::util::text::join_sentences;
use crate::util::tls::Trust;

pub(crate) const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const OAUTH_SCOPES: &[&str] = &[
    "user:file_upload",
    "user:inference",
    "user:mcp_servers",
    "user:profile",
    "user:sessions:claude_code",
];

// ── Login ──

/// A stored login that requests fall back to when no explicit credential is set.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Login {
    /// oxide-code's own login, which `/login` writes and `/logout` removes.
    Ox,
    /// Claude Code's login, which oxide-code reads and refreshes but never removes.
    ClaudeCode,
}

impl Login {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Ox => "oxide-code login",
            Self::ClaudeCode => "Claude Code login",
        }
    }

    /// Recovery step for a login that stopped working.
    pub(super) const fn sign_in_step(self) -> &'static str {
        match self {
            Self::Ox => "Run `/login` to sign in again.",
            Self::ClaudeCode => "Run `claude` to sign in again.",
        }
    }
}

impl fmt::Display for Login {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ── LoginError ──

/// Why a stored login supplied no token. Callers match on [`Self::Missing`] to list other
/// credential options, and every other message carries its own recovery step.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoginError {
    #[error("no stored login found")]
    Missing,
    /// The Keychain refused access, and no credentials file exists to fall back on. Only the macOS
    /// Keychain and its test fakes deny access.
    #[cfg(any(target_os = "macos", test))]
    #[error(
        "macOS Keychain denied access to the Claude Code login. \
         Allow access when the Keychain prompt appears, \
         or set CLAUDE_CODE_OAUTH_TOKEN (from `claude setup-token`) where no prompt can appear."
    )]
    KeychainDenied,
    /// The token expired, and no refresh token is stored.
    #[error("{0} has expired. {step}", step = .0.sign_in_step())]
    Expired(Login),
    /// The token expired while another process held the store's lock for all of [`LOCK_WAIT`], so
    /// that process may still be refreshing it.
    #[error(
        "failed to refresh the expired {login}: another process held {} for over {} s. \
         Try again in a moment.",
        .lock.display(),
        LOCK_WAIT.as_secs()
    )]
    LockHeld { login: Login, lock: PathBuf },
    /// `transient` when the token endpoint sent no response or a server error, so a later attempt
    /// may succeed.
    #[error(
        "{}",
        join_sentences(
            &format!("failed to refresh the expired {}: {:#}", .login, .error),
            .login.sign_in_step(),
        )
    )]
    RefreshFailed {
        login: Login,
        error: anyhow::Error,
        transient: bool,
    },
    /// The stored login exists but cannot be read or parsed.
    #[error(
        "{}",
        join_sentences(&format!("failed to load the {}: {:#}", .0, .1), .0.sign_in_step())
    )]
    Unreadable(Login, anyhow::Error),
}

impl LoginError {
    /// Whether a later attempt may load the token without the user signing in again.
    pub(super) const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::RefreshFailed {
                transient: true,
                ..
            } | Self::LockHeld { .. }
        )
    }
}

// ── ClaudeCodeKeychain ──

#[cfg(target_os = "macos")]
const SYSTEM_KEYCHAIN: Option<&dyn Keychain> = Some(&SystemKeychain);
#[cfg(not(target_os = "macos"))]
const SYSTEM_KEYCHAIN: Option<&dyn Keychain> = None;

/// Where [`SessionLogin::load`] looks for the Claude Code login before its credentials file.
#[derive(Clone, Copy)]
pub(crate) struct ClaudeCodeKeychain(Option<&'static dyn Keychain>);

impl ClaudeCodeKeychain {
    /// The macOS login Keychain, and none on other platforms.
    pub(crate) const SYSTEM: Self = Self(SYSTEM_KEYCHAIN);
    /// No Keychain, so only the credentials file holds the login.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) const NONE: Self = Self(None);
}

// ── Stores ──

/// Stores tokens from `/login` as the oxide-code login, which refreshes at `token_url`.
async fn save_login(tokens: &TokenResponse, token_url: &str) -> Result<SessionLogin> {
    let store = Store::locate(Login::Ox, None)?;
    let token = store.save(tokens).await?;
    Ok(SessionLogin::new(store, token_url, token))
}

/// Removes the oxide-code login, returning whether one was stored. Claude Code's store stays.
pub(crate) async fn delete_login() -> Result<bool> {
    Store::locate(Login::Ox, None)?.delete().await
}

/// Where [`SessionLogin::load`] looks for the Claude Code login, as error messages name it.
pub(crate) fn login_stores() -> String {
    let file = claude_dir().map_or_else(
        || format!("~/.claude/{CLAUDE_CREDENTIALS}"),
        |dir| tildify(&dir.join(CLAUDE_CREDENTIALS)),
    );
    if cfg!(target_os = "macos") {
        format!("the macOS Keychain or {file}")
    } else {
        file
    }
}

// ── SessionLogin ──

/// A stored login as a session sends it: the store it lives in, the endpoint that refreshes it, and
/// its current access token. The store is read again only near expiry or after a 401.
pub(super) struct SessionLogin {
    store: Store<'static>,
    refresh_url: String,
    /// Held across each reload, so concurrent requests wait for a single refresh.
    token: Arc<Mutex<Token>>,
}

impl SessionLogin {
    /// Loads the login, refreshing it near expiry. The Claude Code login reads `keychain` first,
    /// then `.credentials.json` in [`claude_dir`]. The oxide-code login reads
    /// `$XDG_DATA_HOME/ox/credentials.json`.
    pub(super) async fn load(
        login: Login,
        keychain: ClaudeCodeKeychain,
        trust: &dyn Trust,
    ) -> Result<Self, LoginError> {
        let store =
            Store::locate(login, keychain.0).map_err(|e| LoginError::Unreadable(login, e))?;
        let token = load_token_from(&store, OAUTH_TOKEN_URL, trust, None, None).await?;
        Ok(Self::new(store, OAUTH_TOKEN_URL, token))
    }

    fn new(store: Store<'static>, refresh_url: &str, token: Token) -> Self {
        Self {
            store,
            refresh_url: refresh_url.to_owned(),
            token: Arc::new(Mutex::new(token)),
        }
    }

    pub(super) const fn login(&self) -> Login {
        self.store.login
    }

    #[cfg(test)]
    pub(super) fn cached_token(&self) -> String {
        self.token
            .try_lock()
            .expect("no reload is in flight")
            .access_token
            .clone()
    }

    /// The access token for the next request, reloaded from the store first when it nears expiry.
    pub(super) async fn access_token(
        &self,
        trust: &Arc<dyn Trust>,
        reloads: &TaskTracker,
    ) -> Result<String, LoginError> {
        let token = Arc::clone(&self.token).lock_owned().await;
        if !is_near_expiry(token.expires_at_ms) {
            return Ok(token.access_token.clone());
        }
        self.reload_detached(token, trust, reloads, None).await
    }

    /// A token to send in place of `rejected`, which the API rejected with HTTP 401: one another
    /// process stored, or else one refreshed whatever the stored expiry. Once another request has
    /// replaced `rejected`, returns its token unread.
    pub(super) async fn reload(
        &self,
        rejected: &str,
        trust: &Arc<dyn Trust>,
        reloads: &TaskTracker,
    ) -> Result<String, LoginError> {
        let token = Arc::clone(&self.token).lock_owned().await;
        if token.access_token != rejected {
            return Ok(token.access_token.clone());
        }
        self.reload_detached(token, trust, reloads, Some(rejected))
            .await
    }

    /// Reloads under `token`'s lock in a task tracked by `reloads`. Refresh-token rotation must
    /// update both the store and `token` even if the waiting request is dropped, and the session
    /// waits for the task at exit. The task is never aborted, so a join failure resumes its panic.
    async fn reload_detached(
        &self,
        mut token: OwnedMutexGuard<Token>,
        trust: &Arc<dyn Trust>,
        reloads: &TaskTracker,
        rejected: Option<&str>,
    ) -> Result<String, LoginError> {
        let store = self.store.clone();
        let refresh_url = self.refresh_url.clone();
        let trust = Arc::clone(trust);
        let rejected = rejected.map(str::to_owned);
        reloads
            .spawn(async move {
                *token = load_token_from(
                    &store,
                    &refresh_url,
                    trust.as_ref(),
                    Some(&token),
                    rejected.as_deref(),
                )
                .await?;
                Ok(token.access_token.clone())
            })
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    use anyhow::{Result, bail};
    use tokio::sync::Notify;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::keychain::Keychain;
    use super::store::Store;
    use super::token::Token;
    use super::{LoginError, SessionLogin};
    use crate::auth::Auth;

    /// A stored `claudeAiOauth` document, with no refresh token when `refresh` is `None`.
    pub(crate) fn creds_json(
        access: &str,
        refresh: Option<&str>,
        expires_at: u64,
    ) -> serde_json::Value {
        let mut oauth = serde_json::json!({
            "accessToken": access,
            "expiresAt": expires_at,
        });
        if let Some(r) = refresh {
            oauth["refreshToken"] = r.into();
        }
        serde_json::json!({ "claudeAiOauth": oauth })
    }

    pub(crate) fn write_creds(path: &Path, access: &str, refresh: Option<&str>, expires_at: u64) {
        std::fs::write(path, creds_json(access, refresh, expires_at).to_string()).unwrap();
    }

    /// The token endpoint's reply to a successful refresh.
    pub(crate) fn ok_refresh_body(
        access: &str,
        refresh: &str,
        expires_in: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": expires_in,
            "scope": "user:profile user:inference",
        })
    }

    /// A token endpoint that holds its reply to a refresh until [`Self::release`], so a test can
    /// drop the request awaiting the refresh while it is in flight.
    pub(crate) struct HeldRefresh {
        arrived: Arc<Notify>,
        release: mpsc::Sender<()>,
    }

    impl HeldRefresh {
        /// Mounts the endpoint at `/` on `server`, expecting exactly one refresh, which it answers
        /// with `fresh-access` and `fresh-refresh`.
        pub(crate) async fn mount(server: &MockServer) -> Self {
            let arrived = Arc::new(Notify::new());
            let (release, gate) = mpsc::channel();
            let (notify, gate) = (Arc::clone(&arrived), Mutex::new(gate));
            Mock::given(method("POST"))
                .and(path("/"))
                .respond_with(move |_: &Request| {
                    notify.notify_one();
                    // Blocks only the mock server's own thread, until `release` drops.
                    _ = gate.lock().unwrap().recv();
                    ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                        "fresh-access",
                        "fresh-refresh",
                        3600,
                    ))
                })
                .expect(1)
                .mount(server)
                .await;
            Self { arrived, release }
        }

        /// Spawns `request`, then aborts it once its refresh reaches the endpoint.
        pub(crate) async fn abort_mid_refresh<F>(&self, request: F)
        where
            F: Future<Output: Send + 'static> + Send + 'static,
        {
            let request = tokio::spawn(request);
            self.arrived.notified().await;
            request.abort();
            assert!(request.await.is_err_and(|e| e.is_cancelled()));
        }

        /// Lets the endpoint answer the held refresh.
        pub(crate) fn release(self) {
            drop(self.release);
        }
    }

    /// The oxide-code login stored in `file`, as a session holds it: sending `access_token` until
    /// `expires_at_ms`, then refreshing at `refresh_url`.
    pub(crate) fn ox_login_in(
        file: &Path,
        refresh_url: &str,
        access_token: &str,
        expires_at_ms: u64,
    ) -> Auth {
        let token = Token {
            access_token: access_token.to_owned(),
            expires_at_ms,
            unsaved: None,
        };
        Auth::stored(SessionLogin::new(Store::ox_at(file), refresh_url, token))
    }

    pub(super) fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    pub(super) struct FakeKeychain {
        pub(super) entry: Mutex<Option<String>>,
        denies: bool,
        panics: bool,
        pub(super) writable: AtomicBool,
    }

    impl FakeKeychain {
        pub(super) fn holding(entry: impl Into<String>) -> Self {
            Self {
                entry: Mutex::new(Some(entry.into())),
                ..Self::empty()
            }
        }

        pub(super) fn empty() -> Self {
            Self {
                entry: Mutex::new(None),
                denies: false,
                panics: false,
                writable: AtomicBool::new(true),
            }
        }

        /// Holds `entry` and refuses writes until `writable` is set.
        pub(super) fn unwritable(entry: impl Into<String>) -> Self {
            Self {
                writable: AtomicBool::new(false),
                ..Self::holding(entry)
            }
        }

        pub(super) fn denying() -> Self {
            Self {
                denies: true,
                ..Self::empty()
            }
        }

        pub(super) fn panicking() -> Self {
            Self {
                panics: true,
                ..Self::empty()
            }
        }

        pub(super) fn entry(&self) -> Option<String> {
            self.entry.lock().unwrap().clone()
        }

        pub(super) fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.entry().unwrap()).unwrap()
        }
    }

    impl Keychain for FakeKeychain {
        fn read(&self) -> Result<Option<String>, LoginError> {
            assert!(!self.panics, "Keychain read panicked");
            if self.denies {
                return Err(LoginError::KeychainDenied);
            }
            Ok(self.entry())
        }

        fn write(&self, json: &str) -> Result<()> {
            if !self.writable.load(Ordering::Relaxed) {
                bail!("Keychain write denied");
            }
            *self.entry.lock().unwrap() = Some(json.to_owned());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use wiremock::matchers::{body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::{
        FakeKeychain, HeldRefresh, creds_json, ok_refresh_body, read_json, write_creds,
    };
    use super::token::now_millis;
    use super::*;
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::local_testing::no_roots;
    use crate::util::tls::testing::NoRoots;

    mod delete_login {
        use super::*;

        #[tokio::test]
        async fn removes_only_the_oxide_code_login() {
            let data = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            let ox_file = data.path().join("ox").join("credentials.json");
            let claude_code_file = home.path().join(".claude").join(".credentials.json");
            for file in [&ox_file, &claude_code_file] {
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, "{}").unwrap();
            }
            let delete_in = |data: &Path| {
                temp_env::async_with_vars(
                    [
                        ("XDG_DATA_HOME", Some(data.to_owned())),
                        (HOME_VAR, Some(home.path().to_owned())),
                    ],
                    async { delete_login().await.unwrap() },
                )
            };

            let removed = (delete_in(data.path()).await, delete_in(data.path()).await);
            let without_data_dir = delete_in(&data.path().join("missing")).await;

            assert_eq!(removed, (true, false));
            assert!(!without_data_dir);
            assert!(!ox_file.exists());
            assert!(!data.path().join("ox").join("credentials.lock").exists());
            assert_eq!(std::fs::read_to_string(&claude_code_file).unwrap(), "{}");
        }
    }

    mod session_login {
        use super::*;

        fn session_login(
            store: Store<'static>,
            refresh_url: &str,
            access_token: &str,
            expires_at_ms: u64,
        ) -> SessionLogin {
            let token = Token {
                access_token: access_token.to_owned(),
                expires_at_ms,
                unsaved: None,
            };
            SessionLogin::new(store, refresh_url, token)
        }

        /// A session holding a login that sends `access` until `expires_at_ms`, then refreshes with
        /// `refresh` at `refresh_url`. A Keychain that refuses writes holds the login, leaked so
        /// the session's store can borrow it, and the store locks within `dir`.
        fn unwritable_login(
            dir: &Path,
            refresh_url: &str,
            access: &str,
            refresh: &str,
            expires_at_ms: u64,
        ) -> (SessionLogin, &'static FakeKeychain) {
            let json = creds_json(access, Some(refresh), expires_at_ms).to_string();
            let keychain: &'static FakeKeychain =
                Box::leak(Box::new(FakeKeychain::unwritable(json)));
            let store =
                Store::claude_code(Some(keychain), &dir.join("creds.json"), &dir.join("lock"));
            let login = session_login(store, refresh_url, access, expires_at_ms);
            (login, keychain)
        }

        /// Answers the one refresh that sends `sent` with `access` and `refresh`, which expire in
        /// `expires_in` seconds.
        async fn mount_refresh(
            server: &MockServer,
            sent: &str,
            access: &str,
            refresh: &str,
            expires_in: u64,
        ) {
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .and(body_partial_json(
                    serde_json::json!({ "refresh_token": sent }),
                ))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(ok_refresh_body(access, refresh, expires_in)),
                )
                .expect(1)
                .mount(server)
                .await;
        }

        mod load {
            use super::*;

            #[tokio::test]
            async fn resolves_credentials_relative_to_home() {
                let home = tempfile::tempdir().unwrap();
                let claude_dir = home.path().join(".claude");
                std::fs::create_dir_all(&claude_dir).unwrap();
                write_creds(
                    &claude_dir.join(".credentials.json"),
                    "token-from-home",
                    None,
                    9_999_999_999_999,
                );

                let login = temp_env::async_with_vars(
                    [
                        (HOME_VAR, Some(home.path().to_string_lossy().into_owned())),
                        ("CLAUDE_CONFIG_DIR", None),
                    ],
                    async {
                        SessionLogin::load(Login::ClaudeCode, ClaudeCodeKeychain::NONE, &NoRoots)
                            .await
                            .unwrap()
                    },
                )
                .await;
                assert_eq!(login.cached_token(), "token-from-home");
            }
        }

        mod access_token {
            use super::*;

            #[tokio::test]
            async fn far_from_expiry_leaves_the_store_unread() {
                let dir = tempfile::tempdir().unwrap();
                let missing = dir.path().join("creds.json");
                let login = session_login(
                    Store::claude_code(None, &missing, &dir.path().join("lock")),
                    "http://should-not-be-called",
                    "held",
                    9_999_999_999_999,
                );

                let token = login
                    .access_token(&no_roots(), &TaskTracker::new())
                    .await
                    .unwrap();

                assert_eq!(token, "held");
            }

            #[tokio::test]
            async fn near_expiry_refreshes_once_for_concurrent_requests() {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .and(wm_path("/"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                        "fresh-access",
                        "fresh-refresh",
                        3600,
                    )))
                    .expect(1)
                    .mount(&server)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let near_expiry = now_millis().unwrap() + 60_000;
                // The refused write-back leaves the store stale, so only the shared token stops a
                // second refresh.
                let (login, _) = unwritable_login(
                    dir.path(),
                    &server.uri(),
                    "stale",
                    "old-refresh",
                    near_expiry,
                );
                let (trust, reloads) = (no_roots(), TaskTracker::new());

                let (first, second) = tokio::join!(
                    login.access_token(&trust, &reloads),
                    login.access_token(&trust, &reloads)
                );

                assert_eq!(first.unwrap(), "fresh-access");
                assert_eq!(second.unwrap(), "fresh-access");
                server.verify().await;
            }

            #[tokio::test]
            async fn keeps_sending_the_rotated_refresh_token_until_a_write_back_succeeds() {
                let server = MockServer::start().await;
                // Tokens that expire within the 5-minute buffer make each request refresh.
                mount_refresh(&server, "old-refresh", "first-access", "first-refresh", 60).await;
                mount_refresh(
                    &server,
                    "first-refresh",
                    "second-access",
                    "second-refresh",
                    60,
                )
                .await;
                mount_refresh(
                    &server,
                    "second-refresh",
                    "third-access",
                    "third-refresh",
                    3600,
                )
                .await;
                let dir = tempfile::tempdir().unwrap();
                let near_expiry = now_millis().unwrap() + 60_000;
                let (login, keychain) = unwritable_login(
                    dir.path(),
                    &server.uri(),
                    "old-access",
                    "old-refresh",
                    near_expiry,
                );
                let (trust, reloads) = (no_roots(), TaskTracker::new());

                let first = login.access_token(&trust, &reloads).await.unwrap();
                let second = login.access_token(&trust, &reloads).await.unwrap();
                keychain.writable.store(true, Ordering::Relaxed);
                let third = login.access_token(&trust, &reloads).await.unwrap();

                assert_eq!(
                    [first, second, third],
                    ["first-access", "second-access", "third-access"]
                );
                let stored = &keychain.json()["claudeAiOauth"];
                assert_eq!(stored["accessToken"], "third-access");
                assert_eq!(stored["refreshToken"], "third-refresh");
                server.verify().await;
            }

            #[tokio::test]
            async fn a_refresh_token_another_process_stored_wins_over_the_unsaved_one() {
                let server = MockServer::start().await;
                mount_refresh(&server, "old-refresh", "first-access", "first-refresh", 60).await;
                mount_refresh(
                    &server,
                    "sibling-refresh",
                    "second-access",
                    "second-refresh",
                    60,
                )
                .await;
                let dir = tempfile::tempdir().unwrap();
                let near_expiry = now_millis().unwrap() + 60_000;
                let (login, keychain) = unwritable_login(
                    dir.path(),
                    &server.uri(),
                    "old-access",
                    "old-refresh",
                    near_expiry,
                );
                let (trust, reloads) = (no_roots(), TaskTracker::new());

                let first = login.access_token(&trust, &reloads).await.unwrap();
                *keychain.entry.lock().unwrap() = Some(
                    creds_json("sibling-access", Some("sibling-refresh"), near_expiry).to_string(),
                );
                let second = login.access_token(&trust, &reloads).await.unwrap();

                assert_eq!([first, second], ["first-access", "second-access"]);
                server.verify().await;
            }

            #[tokio::test]
            async fn dropped_mid_refresh_still_stores_and_holds_the_rotated_tokens() {
                let server = MockServer::start().await;
                let refresh = HeldRefresh::mount(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let creds = dir.path().join("creds.json");
                write_creds(&creds, "expired", Some("old-refresh"), 0);
                let login = Arc::new(session_login(
                    Store::claude_code(None, &creds, &dir.path().join("lock")),
                    &server.uri(),
                    "expired",
                    0,
                ));
                let reloads = TaskTracker::new();

                refresh
                    .abort_mid_refresh({
                        let (login, reloads) = (Arc::clone(&login), reloads.clone());
                        async move { login.access_token(&no_roots(), &reloads).await }
                    })
                    .await;
                refresh.release();
                reloads.close();
                reloads.wait().await;

                assert_eq!(login.cached_token(), "fresh-access");
                let stored = &read_json(&creds)["claudeAiOauth"];
                assert_eq!(stored["accessToken"], "fresh-access");
                assert_eq!(stored["refreshToken"], "fresh-refresh");
                server.verify().await;
            }

            #[tokio::test]
            async fn a_panicking_reload_fails_the_request_with_its_panic_and_frees_the_token() {
                let dir = tempfile::tempdir().unwrap();
                let keychain: &'static FakeKeychain =
                    Box::leak(Box::new(FakeKeychain::panicking()));
                let login = Arc::new(session_login(
                    Store::claude_code(
                        Some(keychain),
                        &dir.path().join("creds.json"),
                        &dir.path().join("lock"),
                    ),
                    "http://should-not-be-called",
                    "expired",
                    0,
                ));

                let request = tokio::spawn({
                    let login = Arc::clone(&login);
                    async move { login.access_token(&no_roots(), &TaskTracker::new()).await }
                });
                let panic = request.await.unwrap_err().into_panic();

                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"Keychain read panicked")
                );
                assert_eq!(login.cached_token(), "expired");
            }
        }

        mod reload {
            use super::*;

            #[tokio::test]
            async fn adopts_the_stored_token_once_per_rejected_token() {
                let dir = tempfile::tempdir().unwrap();
                let creds = dir.path().join("creds.json");
                write_creds(&creds, "rotated", Some("ref"), 9_999_999_999_999);
                let login = session_login(
                    Store::claude_code(None, &creds, &dir.path().join("lock")),
                    "http://should-not-be-called",
                    "rejected",
                    9_999_999_999_999,
                );
                let (trust, reloads) = (no_roots(), TaskTracker::new());

                let reloaded = login.reload("rejected", &trust, &reloads).await.unwrap();
                write_creds(&creds, "rotated-again", Some("ref"), 9_999_999_999_999);
                let late = login.reload("rejected", &trust, &reloads).await.unwrap();

                assert_eq!(reloaded, "rotated");
                assert_eq!(late, "rotated", "a replaced token is not reloaded again");
            }

            #[tokio::test]
            async fn refreshes_with_the_rotated_refresh_token_after_a_failed_write_back() {
                let server = MockServer::start().await;
                mount_refresh(
                    &server,
                    "old-refresh",
                    "first-access",
                    "first-refresh",
                    3600,
                )
                .await;
                mount_refresh(
                    &server,
                    "first-refresh",
                    "second-access",
                    "second-refresh",
                    3600,
                )
                .await;
                let dir = tempfile::tempdir().unwrap();
                let (login, _) = unwritable_login(
                    dir.path(),
                    &server.uri(),
                    "rejected",
                    "old-refresh",
                    9_999_999_999_999,
                );
                let (trust, reloads) = (no_roots(), TaskTracker::new());

                let first = login.reload("rejected", &trust, &reloads).await.unwrap();
                let second = login
                    .reload("first-access", &trust, &reloads)
                    .await
                    .unwrap();

                assert_eq!([first, second], ["first-access", "second-access"]);
                server.verify().await;
            }

            #[tokio::test]
            async fn dropped_mid_refresh_still_stores_and_holds_the_rotated_tokens() {
                let server = MockServer::start().await;
                let refresh = HeldRefresh::mount(&server).await;
                let dir = tempfile::tempdir().unwrap();
                let creds = dir.path().join("creds.json");
                write_creds(&creds, "rejected", Some("old-refresh"), 9_999_999_999_999);
                let login = Arc::new(session_login(
                    Store::claude_code(None, &creds, &dir.path().join("lock")),
                    &server.uri(),
                    "rejected",
                    9_999_999_999_999,
                ));
                let reloads = TaskTracker::new();

                refresh
                    .abort_mid_refresh({
                        let (login, reloads) = (Arc::clone(&login), reloads.clone());
                        async move { login.reload("rejected", &no_roots(), &reloads).await }
                    })
                    .await;
                refresh.release();
                reloads.close();
                reloads.wait().await;

                assert_eq!(login.cached_token(), "fresh-access");
                let stored = &read_json(&creds)["claudeAiOauth"];
                assert_eq!(stored["accessToken"], "fresh-access");
                assert_eq!(stored["refreshToken"], "fresh-refresh");
                server.verify().await;
            }
        }
    }
}
