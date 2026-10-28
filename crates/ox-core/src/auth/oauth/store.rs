//! Where each stored login lives, the credentials JSON it holds, and loading its token with a
//! refresh near expiry that writes rotated tokens back.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use tracing::warn;

use super::keychain::Keychain;
use super::lock::{Unlocked, acquire_lock, sibling_lock, wait_for_lock};
use super::token::{TokenRequestError, TokenResponse, refresh_oauth_token};
use super::{Login, LoginError, Token, UnsavedRefresh, is_expired, is_near_expiry};
use crate::util::fs::{atomic_write_private, create_private_dir_all};
use crate::util::path::{CLAUDE_CREDENTIALS, XdgBase, app_dir, claude_dir};
use crate::util::tls::Trust;

// ── Credential Types ──

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OAuthCredential,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthCredential {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
}

impl OAuthCredential {
    fn expires_at_ms(&self) -> u64 {
        u64::try_from(self.expires_at).unwrap_or(0)
    }

    fn into_token(self) -> Token {
        Token {
            expires_at_ms: self.expires_at_ms(),
            access_token: self.access_token,
            unsaved: None,
        }
    }
}

/// Credentials parsed from one store, keeping the raw JSON so write-back preserves unknown fields.
/// `oauth` may hold the session's tokens in place of the stored ones while the store lacks them.
struct StoredCredentials<'a> {
    backend: Backend<'a>,
    json: String,
    oauth: OAuthCredential,
    /// Set when `oauth` holds the session's unsaved tokens.
    unsaved: Option<UnsavedRefresh>,
}

impl<'a> StoredCredentials<'a> {
    fn parse(backend: Backend<'a>, json: String) -> Result<Self> {
        let file: CredentialsFile = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse the credentials in {backend}"))?;
        Ok(Self {
            backend,
            json,
            oauth: file.claude_ai_oauth,
            unsaved: None,
        })
    }

    /// Puts the session's `held` tokens in place of the stored ones while the store still holds the
    /// refresh token they replaced.
    fn prefer_unsaved(&mut self, held: Option<&Token>) {
        if let Some(held) = held
            && let Some(unsaved) = &held.unsaved
            && self.oauth.refresh_token.as_deref() == Some(unsaved.replaced.as_str())
        {
            self.oauth = OAuthCredential {
                access_token: held.access_token.clone(),
                refresh_token: Some(unsaved.refresh_token.clone()),
                expires_at: i64::try_from(held.expires_at_ms).unwrap_or(i64::MAX),
            };
            self.unsaved = Some(unsaved.clone());
        }
    }

    fn into_token(self) -> Token {
        Token {
            unsaved: self.unsaved,
            ..self.oauth.into_token()
        }
    }
}

/// The backend credentials were read from, and therefore the only one their refresh updates.
enum Backend<'a> {
    Keychain(&'a dyn Keychain),
    File(&'a Path),
}

impl fmt::Display for Backend<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keychain(_) => f.write_str("the macOS Keychain"),
            Self::File(path) => write!(f, "{}", path.display()),
        }
    }
}

// ── Store ──

/// Where one login lives: an optional Keychain entry read first, then a file holding Claude Code's
/// credentials JSON, plus the lock directory that serializes writes across processes.
#[derive(Clone)]
pub(super) struct Store<'a> {
    pub(super) login: Login,
    pub(super) keychain: Option<&'a dyn Keychain>,
    pub(super) file: PathBuf,
    pub(super) lock: PathBuf,
}

impl<'a> Store<'a> {
    /// `keychain` holds the Claude Code login ahead of its file. The oxide-code login ignores it.
    pub(super) fn locate(login: Login, keychain: Option<&'a dyn Keychain>) -> Result<Self> {
        match login {
            Login::Ox => {
                let dir = app_dir(XdgBase::Data).context(
                    "failed to determine the data directory that holds the oxide-code login",
                )?;
                Ok(Self {
                    login,
                    keychain: None,
                    file: dir.join("credentials.json"),
                    lock: dir.join("credentials.lock"),
                })
            }
            Login::ClaudeCode => {
                let dir = claude_dir().context(
                    "failed to determine the directory that holds the Claude Code login",
                )?;
                Ok(Self {
                    login,
                    keychain,
                    file: dir.join(CLAUDE_CREDENTIALS),
                    lock: sibling_lock(&dir),
                })
            }
        }
    }
}

impl Store<'_> {
    /// Replaces the stored login with newly issued tokens.
    pub(super) async fn save(&self, tokens: &TokenResponse) -> Result<Token> {
        if let Some(dir) = self.file.parent() {
            create_private_dir_all(dir)?;
        }
        let _lock = acquire_lock(&self.lock).await?;
        let token = tokens.token();
        let mut json = serde_json::json!({ "claudeAiOauth": {} });
        apply_tokens(&mut json["claudeAiOauth"], tokens, token.expires_at_ms);
        atomic_write_private(&self.file, serde_json::to_string_pretty(&json)?.as_bytes())?;
        Ok(token)
    }

    /// Removes the login file, returning whether one existed.
    pub(super) async fn delete(&self) -> Result<bool> {
        if !self.file.parent().is_some_and(Path::exists) {
            return Ok(false);
        }
        let _lock = acquire_lock(&self.lock).await?;
        match std::fs::remove_file(&self.file) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", self.file.display())),
        }
    }
}

// ── Token Loading ──

/// Loads the stored token, refreshing it near expiry. The session's `held` token stands in for the
/// stored one while the store still holds the refresh token it replaced. A token the API rejected,
/// `rejected`, counts as expired, so it is refreshed whatever its expiry and never returned.
pub(super) async fn load_token_from(
    store: &Store<'_>,
    refresh_url: &str,
    trust: &dyn Trust,
    held: Option<&Token>,
    rejected: Option<&str>,
) -> Result<Token, LoginError> {
    let login = store.login;
    let load = || {
        load_credentials(store).map(|mut creds| {
            creds.prefer_unsaved(held);
            if rejected == Some(creds.oauth.access_token.as_str()) {
                creds.oauth.expires_at = 0;
            }
            creds
        })
    };
    let creds = load()?;
    if !is_near_expiry(creds.oauth.expires_at_ms()) {
        return Ok(creds.into_token());
    }
    if creds.oauth.refresh_token.is_none() {
        return usable_until_expiry(login, creds.oauth);
    }

    let _lock = match wait_for_lock(&store.lock).await {
        Ok(lock) => lock,
        Err(unlocked) => {
            // The lock holder may have written a fresh token before releasing.
            let creds = load()?;
            if !is_expired(creds.oauth.expires_at_ms()) {
                match &unlocked {
                    Unlocked::Held => {
                        warn!("skipping OAuth token refresh: another process holds the lock");
                    }
                    Unlocked::Failed(e) => {
                        warn!(
                            error = %format_args!("{e:#}"),
                            "skipping OAuth token refresh, using existing"
                        );
                    }
                }
                return Ok(creds.into_token());
            }
            return Err(match unlocked {
                Unlocked::Held => LoginError::LockHeld {
                    login,
                    lock: store.lock.clone(),
                },
                Unlocked::Failed(error) => LoginError::RefreshFailed {
                    login,
                    error,
                    transient: false,
                },
            });
        }
    };

    // A sibling process may have refreshed while this one waited on the lock.
    let creds = load()?;
    let expires_at_ms = creds.oauth.expires_at_ms();
    if !is_near_expiry(expires_at_ms) {
        return Ok(creds.into_token());
    }

    let Some(refresh_token) = creds.oauth.refresh_token.as_deref() else {
        return usable_until_expiry(login, creds.oauth);
    };

    match refresh_oauth_token(refresh_url, refresh_token, trust).await {
        Ok(response) => {
            let mut token = response.token();
            if let Err(e) = write_refreshed_credentials(&creds, &response, token.expires_at_ms) {
                warn!(
                    error = %format_args!("{e:#}"),
                    "failed to save refreshed OAuth credentials, keeping them in memory"
                );
                token.unsaved = Some(UnsavedRefresh {
                    refresh_token: response.refresh_token,
                    // A refresh sending an unsaved token leaves the store with the one it replaced.
                    replaced: creds
                        .unsaved
                        .map_or_else(|| refresh_token.to_owned(), |unsaved| unsaved.replaced),
                });
            }
            Ok(token)
        }
        Err(TokenRequestError { error, transient }) if is_expired(expires_at_ms) => {
            Err(LoginError::RefreshFailed {
                login,
                error,
                transient,
            })
        }
        Err(TokenRequestError { error, .. }) => {
            warn!(
                error = %format_args!("{error:#}"),
                "failed to refresh OAuth token, using existing"
            );
            Ok(creds.into_token())
        }
    }
}

/// A near-expiry token with no refresh token stays usable until it expires.
fn usable_until_expiry(login: Login, oauth: OAuthCredential) -> Result<Token, LoginError> {
    if is_expired(oauth.expires_at_ms()) {
        return Err(LoginError::Expired(login));
    }
    warn!("OAuth token expires soon but no refresh token available");
    Ok(oauth.into_token())
}

/// Reads the Keychain first, then the file. When the file is missing, a denied or unparsable
/// Keychain entry explains why no login was found.
fn load_credentials<'a>(store: &'a Store<'a>) -> Result<StoredCredentials<'a>, LoginError> {
    let login = store.login;
    let mut keychain_failure = None;
    if let Some(keychain) = store.keychain {
        keychain_failure = match keychain.read() {
            Ok(Some(json)) => match StoredCredentials::parse(Backend::Keychain(keychain), json) {
                Ok(creds) => return Ok(creds),
                Err(e) => Some(LoginError::Unreadable(login, e)),
            },
            Ok(None) => None,
            Err(denied) => Some(denied),
        };
    }

    match read_credentials(&store.file) {
        Ok(Some(creds)) => Ok(creds),
        Ok(None) => Err(keychain_failure.unwrap_or(LoginError::Missing)),
        Err(e) => Err(LoginError::Unreadable(login, e)),
    }
}

/// `Ok(None)` when the file does not exist.
fn read_credentials(path: &Path) -> Result<Option<StoredCredentials<'_>>> {
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    enforce_private_mode(path);
    StoredCredentials::parse(Backend::File(path), json).map(Some)
}

#[cfg(unix)]
fn enforce_private_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    use tracing::debug;

    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        debug!(path = %path.display(), error = %e, "failed to reassert 0o600 on credentials");
    }
}

#[cfg(not(unix))]
fn enforce_private_mode(_path: &Path) {}

// ── Write-back ──

fn write_refreshed_credentials(
    creds: &StoredCredentials<'_>,
    response: &TokenResponse,
    expires_at_ms: u64,
) -> Result<()> {
    let mut json: serde_json::Value =
        serde_json::from_str(&creds.json).context("failed to parse credentials for update")?;

    let oauth = json
        .get_mut("claudeAiOauth")
        .context("missing claudeAiOauth in credentials")?;
    apply_tokens(oauth, response, expires_at_ms);

    let serialized = serde_json::to_string_pretty(&json)?;
    match creds.backend {
        Backend::Keychain(keychain) => keychain.write(&serialized),
        Backend::File(path) => atomic_write_private(path, serialized.as_bytes()),
    }
}

/// Writes `tokens`, which expire at `expires_at_ms`, into a `claudeAiOauth` object, keeping its
/// other fields.
fn apply_tokens(oauth: &mut serde_json::Value, tokens: &TokenResponse, expires_at_ms: u64) {
    oauth["accessToken"] = serde_json::Value::String(tokens.access_token.clone());
    oauth["refreshToken"] = serde_json::Value::String(tokens.refresh_token.clone());
    oauth["expiresAt"] = serde_json::json!(expires_at_ms);

    if let Some(scope) = &tokens.scope {
        let scopes: Vec<&str> = scope.split_whitespace().collect();
        oauth["scopes"] = serde_json::json!(scopes);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tracing::instrument::WithSubscriber as _;
    use wiremock::matchers::{body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::oauth::lock::LOCK_RETRY_INTERVAL;
    use crate::auth::oauth::now_millis;
    use crate::auth::oauth::testing::{
        FakeKeychain, claude_code_store, creds_json, ok_refresh_body, read_json, write_creds,
    };
    use crate::auth::oauth::token::TOKEN_REQUEST_TIMEOUT;
    use crate::util::fs::testing::symlink_dir;
    use crate::util::log::testing::LogCapture;
    use crate::util::path::local_testing::{abs, canonical_tempdir};
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::testing::NoRoots;

    mod oauth_credential {
        use super::*;

        mod expires_at_ms {
            use super::*;

            #[test]
            fn keeps_nonnegative_expiry_and_clamps_negative_expiry() {
                for (case, expires_at, expected) in [
                    ("positive", 1_700_000_000_000, 1_700_000_000_000),
                    ("zero", 0, 0),
                    ("negative", -1, 0),
                ] {
                    let cred = OAuthCredential {
                        access_token: String::new(),
                        refresh_token: None,
                        expires_at,
                    };

                    assert_eq!(cred.expires_at_ms(), expected, "{case}");
                }
            }
        }
    }

    mod store {
        use super::*;

        mod locate {
            use super::*;

            #[test]
            fn keeps_each_login_in_its_own_tools_directory() {
                let (home, data) = (abs("/home/user"), abs("/data"));
                let keychain = FakeKeychain::empty();
                let (ox, default_ox, claude_code) = temp_env::with_vars(
                    [
                        (HOME_VAR, Some(home.as_str())),
                        ("XDG_DATA_HOME", Some(data.as_str())),
                        ("CLAUDE_CONFIG_DIR", None),
                    ],
                    || {
                        let ox = Store::locate(Login::Ox, Some(&keychain)).unwrap();
                        let default_ox = temp_env::with_var_unset("XDG_DATA_HOME", || {
                            Store::locate(Login::Ox, None).unwrap()
                        });
                        let claude_code =
                            Store::locate(Login::ClaudeCode, Some(&keychain)).unwrap();
                        (ox, default_ox, claude_code)
                    },
                );

                let data_dir = Path::new(&data).join("ox");
                assert_eq!(ox.file, data_dir.join("credentials.json"));
                assert_eq!(ox.lock, data_dir.join("credentials.lock"));
                assert!(ox.keychain.is_none());
                assert_eq!(
                    default_ox.file,
                    Path::new(&home).join(".local/share/ox/credentials.json")
                );
                assert_eq!(
                    claude_code.file,
                    Path::new(&home).join(".claude").join(".credentials.json")
                );
                assert_eq!(claude_code.lock, Path::new(&home).join(".claude.lock"));
                assert!(claude_code.keychain.is_some());
            }

            #[test]
            fn keeps_the_claude_code_login_in_claude_config_dir_and_locks_beside_its_real_path() {
                let root = canonical_tempdir();
                let root = root.path();
                std::fs::create_dir(root.join("other")).unwrap();
                std::fs::create_dir(root.join("claude-work")).unwrap();
                let spelled = root.join("other").join("..").join("claude-work");

                let store = temp_env::with_var("CLAUDE_CONFIG_DIR", Some(&spelled), || {
                    Store::locate(Login::ClaudeCode, None).unwrap()
                });

                assert_eq!(
                    store.file,
                    std::path::absolute(&spelled)
                        .unwrap()
                        .join(".credentials.json")
                );
                assert_eq!(store.lock, root.join("claude-work.lock"));
            }

            #[test]
            fn locks_where_a_symlinked_claude_directory_resolves() {
                let home = canonical_tempdir();
                let home = home.path();
                std::fs::create_dir(home.join("dotfiles")).unwrap();
                symlink_dir(&home.join("dotfiles"), &home.join(".claude"));

                let store = temp_env::with_vars(
                    [(HOME_VAR, Some(home)), ("CLAUDE_CONFIG_DIR", None)],
                    || Store::locate(Login::ClaudeCode, None).unwrap(),
                );

                assert_eq!(store.file, home.join(".claude/.credentials.json"));
                assert_eq!(store.lock, home.join("dotfiles.lock"));
            }
        }
    }

    mod load_token_from {
        use super::*;

        /// Loads the token in `store` as a session does at startup, with no token held.
        async fn load(store: &Store<'_>, refresh_url: &str) -> Result<Token, LoginError> {
            load_token_from(store, refresh_url, &NoRoots, None, None).await
        }

        async fn mock_refresh_server() -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(ok_refresh_body(
                    "fresh-access",
                    "fresh-refresh",
                    3600,
                )))
                .mount(&server)
                .await;
            server
        }

        #[tokio::test]
        async fn keeps_existing_when_far_from_expiry() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", Some("ref"), 9_999_999_999_999);

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "tok");
        }

        #[tokio::test]
        async fn near_expiry_refreshes_and_persists_new_credentials() {
            let server = mock_refresh_server().await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(
                &creds,
                "old",
                Some("old-refresh"),
                now_millis().unwrap() + 1_000,
            );

            let token = load(&claude_code_store(None, &creds, &lock), &server.uri())
                .await
                .unwrap();
            assert_eq!(token.access_token, "fresh-access");

            let json = read_json(&creds);
            assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
            assert_eq!(json["claudeAiOauth"]["refreshToken"], "fresh-refresh");
            let expires_at = json["claudeAiOauth"]["expiresAt"].as_u64().unwrap();
            let now = now_millis().unwrap();
            assert!(
                expires_at >= now + 3_500_000 && expires_at <= now + 3_700_000,
                "expiresAt out of band: {expires_at}",
            );
            assert_eq!(token.expires_at_ms, expires_at);
        }

        #[tokio::test]
        async fn keychain_refresh_writes_back_to_keychain_without_creating_file() {
            let server = mock_refresh_server().await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            let keychain = FakeKeychain::holding(
                creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000).to_string(),
            );

            let token = load(
                &claude_code_store(Some(&keychain), &creds, &lock),
                &server.uri(),
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "fresh-access");

            let json = keychain.json();
            assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
            assert_eq!(json["claudeAiOauth"]["refreshToken"], "fresh-refresh");
            assert!(!creds.exists(), "file must not be created");
        }

        #[tokio::test]
        async fn keychain_refresh_leaves_existing_file_untouched() {
            let server = mock_refresh_server().await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            let mut file_json = creds_json("file-access", Some("file-refresh"), 0);
            file_json["claudeAiOauth"]["subscriptionType"] = "pro".into();
            std::fs::write(&creds, file_json.to_string()).unwrap();
            let mut keychain_json =
                creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000);
            keychain_json["claudeAiOauth"]["subscriptionType"] = "max".into();
            let keychain = FakeKeychain::holding(keychain_json.to_string());

            let token = load(
                &claude_code_store(Some(&keychain), &creds, &lock),
                &server.uri(),
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "fresh-access");

            let json = keychain.json();
            assert_eq!(json["claudeAiOauth"]["accessToken"], "fresh-access");
            assert_eq!(json["claudeAiOauth"]["subscriptionType"], "max");
            assert_eq!(read_json(&creds), file_json);
        }

        #[tokio::test]
        async fn unparsable_keychain_falls_back_to_file_and_refreshes_only_the_file() {
            let server = mock_refresh_server().await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(
                &creds,
                "old",
                Some("old-refresh"),
                now_millis().unwrap() + 1_000,
            );
            let keychain = FakeKeychain::holding("not json");

            let token = load(
                &claude_code_store(Some(&keychain), &creds, &lock),
                &server.uri(),
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "fresh-access");

            assert_eq!(
                read_json(&creds)["claudeAiOauth"]["accessToken"],
                "fresh-access"
            );
            assert_eq!(keychain.entry().as_deref(), Some("not json"));
        }

        #[tokio::test]
        async fn denied_keychain_falls_back_to_the_file() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "from-file", None, 9_999_999_999_999);

            let keychain = FakeKeychain::denying();
            let token = load(
                &claude_code_store(Some(&keychain), &creds, &lock),
                "http://unused",
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "from-file");
        }

        #[tokio::test]
        async fn write_back_failure_holds_the_refreshed_tokens() {
            let server = mock_refresh_server().await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            let keychain = FakeKeychain::unwritable(
                creds_json("old", Some("old-refresh"), now_millis().unwrap() + 1_000).to_string(),
            );
            let logs = LogCapture::default();

            let token = load(
                &claude_code_store(Some(&keychain), &creds, &lock),
                &server.uri(),
            )
            .with_subscriber(logs.dispatch())
            .await
            .unwrap();

            assert_eq!(token.access_token, "fresh-access");
            let unsaved = token.unsaved.expect("the rotated refresh token is held");
            assert_eq!(unsaved.refresh_token, "fresh-refresh");
            assert_eq!(unsaved.replaced, "old-refresh");
            assert_eq!(
                logs.events(),
                [BTreeMap::from([
                    ("error".to_owned(), "Keychain write denied".to_owned()),
                    (
                        "message".to_owned(),
                        "failed to save refreshed OAuth credentials, keeping them in memory"
                            .to_owned(),
                    ),
                ])]
            );
        }

        #[tokio::test]
        async fn without_refresh_token_keeps_nonexpired_as_is() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", None, now_millis().unwrap() + 60_000);

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "tok");
        }

        #[tokio::test]
        async fn refresh_endpoint_down_keeps_existing_token_if_unexpired() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(
                &creds,
                "stale",
                Some("old-refresh"),
                now_millis().unwrap() + 60_000,
            );

            let token = load(&claude_code_store(None, &creds, &lock), &server.uri())
                .await
                .unwrap();
            assert_eq!(token.access_token, "stale");
        }

        #[tokio::test]
        async fn refresh_endpoint_down_keeps_the_unsaved_refresh_token() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .and(body_partial_json(
                    serde_json::json!({ "refresh_token": "held-refresh" }),
                ))
                .respond_with(ResponseTemplate::new(503))
                .expect(1)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "old-access", Some("old-refresh"), 0);
            let held = Token {
                access_token: "held-access".to_owned(),
                expires_at_ms: now_millis().unwrap() + 60_000,
                unsaved: Some(UnsavedRefresh {
                    refresh_token: "held-refresh".to_owned(),
                    replaced: "old-refresh".to_owned(),
                }),
            };
            let logs = LogCapture::default();

            let token = load_token_from(
                &claude_code_store(None, &creds, &lock),
                &server.uri(),
                &NoRoots,
                Some(&held),
                None,
            )
            .with_subscriber(logs.dispatch())
            .await
            .unwrap();

            assert_eq!(token.access_token, "held-access");
            let unsaved = token.unsaved.expect("the unsaved refresh token is kept");
            assert_eq!(unsaved.refresh_token, "held-refresh");
            assert_eq!(unsaved.replaced, "old-refresh");
            server.verify().await;
            assert_eq!(
                logs.events(),
                [
                    BTreeMap::from([
                        ("status".to_owned(), "503".to_owned()),
                        ("body".to_owned(), String::new()),
                        (
                            "message".to_owned(),
                            "OAuth token endpoint error".to_owned()
                        ),
                    ]),
                    BTreeMap::from([
                        (
                            "error".to_owned(),
                            "token refresh failed with HTTP 503 Service Unavailable".to_owned(),
                        ),
                        (
                            "message".to_owned(),
                            "failed to refresh OAuth token, using existing".to_owned(),
                        ),
                    ]),
                ]
            );
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_keeps_existing_token_if_unexpired() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", Some("ref"), now_millis().unwrap() + 60_000);
            std::fs::create_dir(&lock).unwrap();

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            assert_eq!(token.access_token, "tok");
        }

        #[tokio::test]
        async fn without_refresh_token_bails_when_expired() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", None, 0);

            let err = load(&claude_code_store(None, &creds, &lock), "http://unused")
                .await
                .expect_err("expired without refresh must bail");
            assert!(
                matches!(err, LoginError::Expired(Login::ClaudeCode)),
                "{err:?}"
            );
        }

        #[tokio::test]
        async fn refresh_endpoint_down_bails_if_expired() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&server)
                .await;

            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("old"), 0);

            let err = load(&claude_code_store(None, &creds, &lock), &server.uri())
                .await
                .expect_err("expired + refresh down must bail");
            assert!(
                matches!(
                    err,
                    LoginError::RefreshFailed {
                        login: Login::ClaudeCode,
                        transient: true,
                        ..
                    }
                ),
                "{err:?}"
            );
            assert_eq!(
                format!("{err:#}"),
                "failed to refresh the expired Claude Code login: \
                 token refresh failed with HTTP 503 Service Unavailable. \
                 Run `claude` to sign in again.",
            );
        }

        #[tokio::test]
        async fn refresh_rejection_keeps_a_body_that_ends_a_sentence_verbatim() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(wm_path("/"))
                .respond_with(ResponseTemplate::new(400).set_body_string("Refresh token revoked."))
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("old"), 0);

            let err = load(&claude_code_store(None, &creds, &lock), &server.uri())
                .await
                .expect_err("expired + refresh rejected must bail");

            assert!(!err.is_transient(), "{err:?}");
            assert_eq!(
                format!("{err:#}"),
                "failed to refresh the expired Claude Code login: \
                 token refresh failed with HTTP 400 Bad Request: Refresh token revoked. \
                 Run `claude` to sign in again.",
            );
        }

        #[tokio::test]
        async fn lock_that_cannot_be_created_fails_if_expired_and_keeps_an_unexpired_token() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("missing").join("lock");
            let store = claude_code_store(None, &creds, &lock);

            write_creds(&creds, "dead", Some("ref"), 0);
            let err = load(&store, "http://should-not-be-called")
                .await
                .expect_err("expired + lock failure must bail");
            write_creds(&creds, "tok", Some("ref"), now_millis().unwrap() + 60_000);
            let logs = LogCapture::default();

            let token = load(&store, "http://should-not-be-called")
                .with_subscriber(logs.dispatch())
                .await
                .unwrap();

            assert!(!err.is_transient(), "{err:?}");
            let message = format!("{err:#}");
            let headline = format!(
                "failed to refresh the expired Claude Code login: failed to create lock at {}: ",
                lock.display()
            );
            assert!(message.starts_with(&headline), "{message}");
            assert!(
                message.ends_with(". Run `claude` to sign in again."),
                "{message}"
            );
            assert_eq!(token.access_token, "tok");
            let cause = std::fs::create_dir(&lock).unwrap_err();
            assert_eq!(
                logs.events(),
                [BTreeMap::from([
                    (
                        "error".to_owned(),
                        format!("failed to create lock at {}: {cause}", lock.display()),
                    ),
                    (
                        "message".to_owned(),
                        "skipping OAuth token refresh, using existing".to_owned(),
                    ),
                ])]
            );
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_past_the_wait_fails_transiently_if_expired() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();

            let err = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .expect_err("expired + lock held must bail");

            assert!(err.is_transient(), "{err:?}");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "failed to refresh the expired Claude Code login: \
                     another process held {} for over 20 s. \
                     Try again in a moment.",
                    lock.display()
                ),
            );
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_waits_for_a_holder_as_long_as_its_token_request_may_take() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();
            let (sibling_creds, sibling_lock) = (creds.clone(), lock.clone());
            let sibling = tokio::spawn(async move {
                tokio::time::sleep(TOKEN_REQUEST_TIMEOUT).await;
                write_creds(
                    &sibling_creds,
                    "sibling",
                    Some("sibling-refresh"),
                    now_millis().unwrap() + 60 * 60 * 1000,
                );
                std::fs::remove_dir(&sibling_lock).unwrap();
            });

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            sibling.await.unwrap();

            assert_eq!(token.access_token, "sibling");
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_picks_up_sibling_refresh() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();

            let sibling_creds = creds.clone();
            let sibling = tokio::spawn(async move {
                tokio::time::sleep(LOCK_RETRY_INTERVAL * 2).await;
                write_creds(
                    &sibling_creds,
                    "sibling",
                    Some("ref"),
                    now_millis().unwrap() + 60 * 60 * 1000,
                );
            });

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            sibling.await.unwrap();
            assert_eq!(token.access_token, "sibling");
        }

        #[tokio::test(start_paused = true)]
        async fn lock_released_after_a_sibling_refresh_keeps_the_sibling_token() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();
            let (sibling_creds, sibling_lock) = (creds.clone(), lock.clone());
            let sibling = tokio::spawn(async move {
                tokio::time::sleep(LOCK_RETRY_INTERVAL * 2).await;
                write_creds(
                    &sibling_creds,
                    "sibling",
                    Some("sibling-refresh"),
                    now_millis().unwrap() + 60 * 60 * 1000,
                );
                std::fs::remove_dir(&sibling_lock).unwrap();
            });

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            sibling.await.unwrap();

            assert_eq!(token.access_token, "sibling");
        }

        #[tokio::test(start_paused = true)]
        async fn lock_released_without_a_refresh_token_keeps_the_sibling_login() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();

            let (sibling_creds, sibling_lock) = (creds.clone(), lock.clone());
            let sibling = tokio::spawn(async move {
                tokio::time::sleep(LOCK_RETRY_INTERVAL * 2).await;
                write_creds(
                    &sibling_creds,
                    "sibling",
                    None,
                    now_millis().unwrap() + 60 * 1000,
                );
                std::fs::remove_dir(&sibling_lock).unwrap();
            });

            let token = load(
                &claude_code_store(None, &creds, &lock),
                "http://should-not-be-called",
            )
            .await
            .unwrap();
            sibling.await.unwrap();
            assert_eq!(token.access_token, "sibling");
        }

        #[tokio::test]
        async fn reports_why_no_login_was_found() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            let (empty, denying, garbled) = (
                FakeKeychain::empty(),
                FakeKeychain::denying(),
                FakeKeychain::holding("not json"),
            );

            for keychain in [None, Some(&empty as &dyn Keychain)] {
                let err = load(
                    &claude_code_store(keychain, &missing, &lock),
                    "http://unused",
                )
                .await
                .expect_err("no login anywhere");
                assert!(matches!(err, LoginError::Missing), "{err:?}");
            }

            let err = load(
                &claude_code_store(Some(&denying), &missing, &lock),
                "http://unused",
            )
            .await
            .expect_err("a denied Keychain and no file");
            assert!(matches!(err, LoginError::KeychainDenied), "{err:?}");

            let err = load(
                &claude_code_store(Some(&garbled), &missing, &lock),
                "http://unused",
            )
            .await
            .expect_err("an unparsable Keychain entry and no file");
            assert_eq!(
                format!("{err:#}"),
                "failed to load the Claude Code login: \
                 failed to parse the credentials in the macOS Keychain: \
                 expected ident at line 1 column 2. \
                 Run `claude` to sign in again.",
            );

            let unreadable = dir.path().join("directory.json");
            std::fs::create_dir(&unreadable).unwrap();
            let err = load(
                &claude_code_store(Some(&empty), &unreadable, &lock),
                "http://unused",
            )
            .await
            .expect_err("an unreadable credentials file");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with(&format!(
                    "failed to load the Claude Code login: failed to read {}: ",
                    unreadable.display()
                )),
                "{msg}"
            );
            assert!(msg.ends_with(". Run `claude` to sign in again."), "{msg}");
        }
    }

    mod read_credentials {
        use super::*;

        #[test]
        fn parses_tokens_and_expiry() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("creds.json");
            write_creds(&path, "tok", Some("ref"), 9_999_999_999_999);

            let creds = read_credentials(&path).unwrap().unwrap();
            assert_eq!(creds.oauth.access_token, "tok");
            assert_eq!(creds.oauth.refresh_token.as_deref(), Some("ref"));
            assert_eq!(creds.oauth.expires_at, 9_999_999_999_999);
        }

        #[test]
        fn without_refresh_token_keeps_access_token() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("creds.json");
            write_creds(&path, "tok", None, 9_999_999_999_999);

            let creds = read_credentials(&path).unwrap().unwrap();
            assert_eq!(creds.oauth.access_token, "tok");
            assert!(creds.oauth.refresh_token.is_none());
        }

        #[test]
        fn missing_file_is_absent() {
            assert!(
                read_credentials(Path::new("/nonexistent/creds.json"))
                    .unwrap()
                    .is_none()
            );
        }

        #[test]
        fn invalid_json_fails_naming_the_path() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("creds.json");
            std::fs::write(&path, "not json").unwrap();
            let err = read_credentials(&path)
                .err()
                .expect("invalid JSON must fail");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "failed to parse the credentials in {}: expected ident at line 1 column 2",
                    path.display()
                ),
            );
        }

        #[test]
        fn without_oauth_key_fails_naming_the_missing_field() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("creds.json");
            std::fs::write(&path, r#"{"other": "data"}"#).unwrap();
            let err = read_credentials(&path)
                .err()
                .expect("credentials without claudeAiOauth must fail");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "failed to parse the credentials in {}: \
                     missing field `claudeAiOauth` at line 1 column 17",
                    path.display()
                ),
            );
        }
    }

    mod write_refreshed_credentials {
        use super::*;

        #[test]
        fn preserves_unknown_fields() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("credentials.json");

            let mut creds = creds_json("old-access", Some("old-refresh"), 1000);
            creds["claudeAiOauth"]["scopes"] = serde_json::json!(["user:profile"]);
            creds["claudeAiOauth"]["subscriptionType"] = "pro".into();
            creds["claudeAiOauth"]["rateLimitTier"] = "default".into();
            std::fs::write(&path, creds.to_string()).unwrap();

            let response = TokenResponse {
                access_token: "new-access".to_owned(),
                refresh_token: "new-refresh".to_owned(),
                expires_in: 3600,
                scope: Some("user:profile user:inference".to_owned()),
            };

            write_refreshed_credentials(
                &read_credentials(&path).unwrap().unwrap(),
                &response,
                9_000,
            )
            .unwrap();

            let json = read_json(&path);
            let oauth = &json["claudeAiOauth"];

            assert_eq!(oauth["accessToken"], "new-access");
            assert_eq!(oauth["refreshToken"], "new-refresh");
            assert_eq!(oauth["expiresAt"], 9_000);
            assert_eq!(
                oauth["scopes"],
                serde_json::json!(["user:profile", "user:inference"])
            );
            assert_eq!(oauth["subscriptionType"], "pro");
            assert_eq!(oauth["rateLimitTier"], "default");
        }

        #[test]
        fn tolerates_whitespace_in_scope_field() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("credentials.json");

            let mut creds = creds_json("t", Some("r"), 1000);
            creds["claudeAiOauth"]["scopes"] = serde_json::json!([]);
            std::fs::write(&path, creds.to_string()).unwrap();

            let response = TokenResponse {
                access_token: "t2".to_owned(),
                refresh_token: "r2".to_owned(),
                expires_in: 3600,
                scope: Some("  user:profile   user:inference  ".to_owned()),
            };

            write_refreshed_credentials(
                &read_credentials(&path).unwrap().unwrap(),
                &response,
                9_000,
            )
            .unwrap();

            let json = read_json(&path);
            assert_eq!(
                json["claudeAiOauth"]["scopes"],
                serde_json::json!(["user:profile", "user:inference"]),
            );
        }

        #[test]
        fn skips_scopes_when_absent() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("credentials.json");

            let mut creds = creds_json("old", Some("old"), 1000);
            creds["claudeAiOauth"]["scopes"] = serde_json::json!(["user:profile"]);
            std::fs::write(&path, creds.to_string()).unwrap();

            let response = TokenResponse {
                access_token: "new".to_owned(),
                refresh_token: "new".to_owned(),
                expires_in: 3600,
                scope: None,
            };

            write_refreshed_credentials(
                &read_credentials(&path).unwrap().unwrap(),
                &response,
                9_000,
            )
            .unwrap();

            let json = read_json(&path);
            let oauth = &json["claudeAiOauth"];

            assert_eq!(oauth["scopes"], serde_json::json!(["user:profile"]));
        }
    }
}
