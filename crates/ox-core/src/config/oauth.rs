//! Stored OAuth logins: oxide-code's own from `/login`, and Claude Code's. Loads a login's token,
//! refreshes it near expiry, and writes rotated tokens back to the store they came from.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::task::TaskTracker;
use tracing::{debug, warn};

#[cfg(target_os = "macos")]
use crate::util::env;
use crate::util::fs::{atomic_write_private, create_private_dir_all};
#[cfg(any(target_os = "macos", test))]
use crate::util::path::claude_config_var;
use crate::util::path::{CLAUDE_CREDENTIALS, XdgBase, app_dir, claude_dir, tildify};
use crate::util::response::{self, condense, http_status};
use crate::util::sync::lock_or_recover;
use crate::util::text::join_sentences;
use crate::util::tls::Trust;

pub(crate) const OAUTH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(super) const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(super) const OAUTH_SCOPES: &[&str] = &[
    "user:file_upload",
    "user:inference",
    "user:mcp_servers",
    "user:profile",
    "user:sessions:claude_code",
];
const TOKEN_EXPIRY_BUFFER_MS: u64 = 5 * 60 * 1000;
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

// Match the `stale` and `update` defaults of `proper-lockfile`, which Claude Code locks its config
// directory with.
const LOCK_STALE_THRESHOLD: Duration = Duration::from_secs(10);
const LOCK_UPDATE_INTERVAL: Duration = Duration::from_secs(5);
/// How long a process waits for another to finish a refresh, which covers the holder's token
/// request plus one mtime update.
const LOCK_WAIT: Duration = TOKEN_REQUEST_TIMEOUT.saturating_add(LOCK_UPDATE_INTERVAL);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

// ── Login ──

/// A stored login that requests fall back to when no explicit credential is set.
#[derive(Debug, Clone, Copy)]
pub(super) enum Login {
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
pub(super) enum LoginError {
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
    #[serde(default)]
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

/// An access token and when it expires, in Unix milliseconds.
#[expect(
    clippy::struct_field_names,
    reason = "`access_token` matches the field in `OAuthCredential` and `TokenResponse`, \
              which this token is built from"
)]
#[cfg_attr(test, derive(Debug))]
struct Token {
    access_token: String,
    expires_at_ms: u64,
    /// The refresh token issued with `access_token`, held only when the store failed to save it.
    unsaved: Option<UnsavedRefresh>,
}

/// A rotated refresh token that the store failed to save, which reloads send in place of the stored
/// one it replaced.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone)]
struct UnsavedRefresh {
    refresh_token: String,
    /// The stored refresh token this one superseded. A store holding any other was written by
    /// another process, and its login wins.
    replaced: String,
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
struct Store<'a> {
    login: Login,
    keychain: Option<&'a dyn Keychain>,
    file: PathBuf,
    lock: PathBuf,
}

impl<'a> Store<'a> {
    /// `keychain` holds the Claude Code login ahead of its file. The oxide-code login ignores it.
    fn locate(login: Login, keychain: Option<&'a dyn Keychain>) -> Result<Self> {
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
    async fn save(&self, tokens: &TokenResponse) -> Result<Token> {
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
    async fn delete(&self) -> Result<bool> {
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

/// Stores tokens from `/login` as the oxide-code login, which refreshes at `token_url`.
pub(super) async fn save_login(tokens: &TokenResponse, token_url: &str) -> Result<SessionLogin> {
    let store = Store::locate(Login::Ox, None)?;
    let token = store.save(tokens).await?;
    Ok(SessionLogin::new(store, token_url, token))
}

/// Removes the oxide-code login, returning whether one was stored. Claude Code's store stays.
pub(crate) async fn delete_login() -> Result<bool> {
    Store::locate(Login::Ox, None)?.delete().await
}

/// Where [`SessionLogin::load`] looks for the Claude Code login, as error messages name it.
pub(super) fn login_stores() -> String {
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

// ── Token Loading ──

/// Loads the stored token, refreshing it near expiry. The session's `held` token stands in for the
/// stored one while the store still holds the refresh token it replaced. A token the API rejected,
/// `rejected`, counts as expired, so it is refreshed whatever its expiry and never returned.
async fn load_token_from(
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
                        warn!("skipping OAuth token refresh, using existing: {e:#}");
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
                warn!("failed to save refreshed OAuth credentials, keeping them in memory: {e:#}");
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
            warn!("failed to refresh OAuth token, using existing: {error:#}");
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

    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        debug!("failed to reassert 0o600 on {}: {e}", path.display());
    }
}

#[cfg(not(unix))]
fn enforce_private_mode(_path: &Path) {}

fn is_near_expiry(expires_at_ms: u64) -> bool {
    now_millis().is_none_or(|now| now.saturating_add(TOKEN_EXPIRY_BUFFER_MS) >= expires_at_ms)
}

fn is_expired(expires_at_ms: u64) -> bool {
    now_millis().is_none_or(|now| now >= expires_at_ms)
}

// ── Keychain ──

/// Claude Code's macOS Keychain entry, holding the same JSON document as the credentials file.
trait Keychain: Sync {
    /// The stored JSON, `None` when the entry does not exist or cannot be read for a reason other
    /// than `LoginError::KeychainDenied`, the only error it returns.
    fn read(&self) -> Result<Option<String>, LoginError>;
    fn write(&self, json: &str) -> Result<()>;
}

/// `OSStatus` codes from the Security framework's `SecBase.h`.
#[cfg(target_os = "macos")]
mod os_status {
    pub(super) const ITEM_NOT_FOUND: i32 = -25_300;
    pub(super) const USER_CANCELED: i32 = -128;
    pub(super) const AUTH_FAILED: i32 = -25_293;
    pub(super) const INTERACTION_NOT_ALLOWED: i32 = -25_308;
}

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

#[cfg(target_os = "macos")]
struct SystemKeychain;

#[cfg(target_os = "macos")]
impl Keychain for SystemKeychain {
    fn read(&self) -> Result<Option<String>, LoginError> {
        use security_framework::passwords::{PasswordOptions, generic_password};

        let account = match keychain_account() {
            Ok(account) => account,
            Err(e) => {
                debug!("skipping the Keychain: {e:#}");
                return Ok(None);
            }
        };
        let service = keychain_service();
        let options = PasswordOptions::new_generic_password(&service, &account);
        match generic_password(options) {
            Ok(bytes) => Ok(String::from_utf8(bytes)
                .inspect_err(|_| debug!("Keychain data is not valid UTF-8"))
                .ok()),
            Err(e) => match e.code() {
                os_status::ITEM_NOT_FOUND => Ok(None),
                os_status::USER_CANCELED
                | os_status::AUTH_FAILED
                | os_status::INTERACTION_NOT_ALLOWED => Err(LoginError::KeychainDenied),
                _ => {
                    debug!("failed to read the Claude Code login from the Keychain: {e}");
                    Ok(None)
                }
            },
        }
    }

    fn write(&self, json: &str) -> Result<()> {
        use security_framework::passwords::set_generic_password;

        let account = keychain_account()?;
        set_generic_password(&keychain_service(), &account, json.as_bytes())
            .context("failed to write to the macOS Keychain")
    }
}

#[cfg(target_os = "macos")]
fn keychain_account() -> Result<String> {
    env::string("USER").context("failed to determine the OS username")
}

/// The Keychain service Claude Code keeps its login under. A `CLAUDE_CONFIG_DIR` adds the first 8
/// hex digits of the SHA-256 of the variable's value, so each config directory keeps its own login.
#[cfg(any(target_os = "macos", test))]
fn keychain_service() -> String {
    use sha2::{Digest as _, Sha256};

    claude_config_var().map_or_else(
        || KEYCHAIN_SERVICE.to_owned(),
        |dir| {
            let hash = Sha256::digest(dir.as_bytes());
            let prefix = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
            format!("{KEYCHAIN_SERVICE}-{prefix:08x}")
        },
    )
}

// ── Token Requests ──

/// Tokens the token endpoint issues for a code exchange or a refresh.
#[cfg_attr(test, derive(Debug))]
#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

impl TokenResponse {
    /// The issued access token, expiring `expires_in` seconds from now. A clock set before 1970
    /// records it as expired, as [`is_near_expiry`] reads such a clock.
    fn token(&self) -> Token {
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

async fn refresh_oauth_token(
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
struct TokenRequestError {
    error: anyhow::Error,
    /// The endpoint sent no response or a server error, so the request may succeed when sent again.
    transient: bool,
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

fn now_millis() -> Option<u64> {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis(),
    )
    .ok()
}

// ── File Locking ──

/// Credential lock kept fresh by a background heartbeat. Drop stops the heartbeat and removes the
/// lock while still owned. Like `proper-lockfile`, a changed mtime signals takeover, so the
/// heartbeat stops and leaves the new owner's lock intact.
struct LockGuard {
    path: PathBuf,
    /// The mtime this process last gave the lock, `None` once another process took it over.
    mtime: Arc<std::sync::Mutex<Option<SystemTime>>>,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl LockGuard {
    fn hold(path: &Path) -> std::io::Result<Self> {
        let mtime = Arc::new(std::sync::Mutex::new(Some(lock_mtime(path)?)));
        let heartbeat = tokio::spawn({
            let path = path.to_owned();
            let mtime = Arc::clone(&mtime);
            async move {
                loop {
                    tokio::time::sleep(LOCK_UPDATE_INTERVAL).await;
                    let mut held = lock_or_recover(&mtime, "credentials lock");
                    if !held.is_some_and(|set| owns_lock(&path, set)) {
                        warn!("another process took over the lock at {}", path.display());
                        *held = None;
                        return;
                    }
                    match touch_dir(&path).and_then(|()| lock_mtime(&path)) {
                        Ok(set) => *held = Some(set),
                        Err(e) => debug!("failed to refresh lock {}: {e}", path.display()),
                    }
                }
            }
        });
        Ok(Self {
            path: path.to_owned(),
            mtime,
            heartbeat,
        })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        let held = *lock_or_recover(&self.mtime, "credentials lock");
        if !held.is_some_and(|set| owns_lock(&self.path, set)) {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!("failed to release lock {}: {e}", self.path.display());
        }
    }
}

/// Why [`wait_for_lock`] returned without the lock.
enum Unlocked {
    /// Another process held the lock for all of [`LOCK_WAIT`].
    Held,
    Failed(anyhow::Error),
}

/// Directory-based advisory lock around credential writes. `mkdir` is the cross-platform atomic
/// primitive, and the guard `rmdir`s on drop.
async fn acquire_lock(path: &Path) -> Result<LockGuard> {
    match wait_for_lock(path).await {
        Ok(lock) => Ok(lock),
        Err(Unlocked::Held) => bail!(
            "another process held {} for over {} s",
            path.display(),
            LOCK_WAIT.as_secs()
        ),
        Err(Unlocked::Failed(e)) => Err(e),
    }
}

/// Acquires `path`, waiting up to [`LOCK_WAIT`] and clearing locks whose mtime is older than
/// [`LOCK_STALE_THRESHOLD`].
async fn wait_for_lock(path: &Path) -> Result<LockGuard, Unlocked> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match std::fs::create_dir(path) {
            Ok(()) => {
                return LockGuard::hold(path).map_err(|e| {
                    Unlocked::Failed(
                        anyhow::Error::new(e)
                            .context(format!("failed to read lock {}", path.display())),
                    )
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(path)
                    && let Err(e) = std::fs::remove_dir_all(path)
                {
                    debug!("failed to clear stale lock {}: {e}", path.display());
                }
            }
            Err(e) => {
                return Err(Unlocked::Failed(
                    anyhow::Error::new(e)
                        .context(format!("failed to create lock at {}", path.display())),
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Unlocked::Held);
        }
        tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
    }
}

fn is_stale_lock(path: &Path) -> bool {
    lock_mtime(path).is_ok_and(|t| t.elapsed().unwrap_or_default() > LOCK_STALE_THRESHOLD)
}

fn lock_mtime(path: &Path) -> std::io::Result<SystemTime> {
    std::fs::metadata(path)?.modified()
}

/// Returns whether the lock at `path` still has the mtime this process last gave it, `set`.
fn owns_lock(path: &Path, set: SystemTime) -> bool {
    lock_mtime(path).is_ok_and(|mtime| mtime == set)
}

/// Sets the lock directory's mtime to now, as `proper-lockfile` refreshes a held lock.
fn touch_dir(path: &Path) -> std::io::Result<()> {
    open_dir(path)?.set_modified(SystemTime::now())
}

/// Opens a directory with enough access to set its mtime.
fn open_dir(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
        };

        // Windows opens a directory only with `FILE_FLAG_BACKUP_SEMANTICS`.
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    std::fs::File::open(path)
}

/// Where Claude Code's `proper-lockfile` locks the directory `dir`: beside its real path, with
/// `.lock` appended, so a symlinked `~/.claude` locks where its target does.
fn sibling_lock(dir: &Path) -> PathBuf {
    let mut lock = dunce::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_owned())
        .into_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use std::path::Path;
    use std::sync::{Arc, Mutex, mpsc};

    use tokio::sync::Notify;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::{Login, SessionLogin, Store, Token};
    use crate::config::auth::Auth;

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
        let store = Store {
            login: Login::Ox,
            keychain: None,
            file: file.to_owned(),
            lock: file.with_extension("lock"),
        };
        let token = Token {
            access_token: access_token.to_owned(),
            expires_at_ms,
            unsaved: None,
        };
        Auth::stored(SessionLogin::new(store, refresh_url, token))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::testing::{HeldRefresh, creds_json, ok_refresh_body, write_creds};
    use super::*;
    use crate::util::fs::testing::symlink_dir;
    use crate::util::path::local_testing::{abs, canonical_tempdir};
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::PlatformTrust;
    use crate::util::tls::local_testing::no_roots;
    use crate::util::tls::testing::{NoRoots, TEST_CA_PATH};

    fn store<'a>(keychain: Option<&'a dyn Keychain>, file: &Path, lock: &Path) -> Store<'a> {
        Store {
            login: Login::ClaudeCode,
            keychain,
            file: file.to_owned(),
            lock: lock.to_owned(),
        }
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    struct FakeKeychain {
        entry: Mutex<Option<String>>,
        denies: bool,
        panics: bool,
        writable: AtomicBool,
    }

    impl FakeKeychain {
        fn holding(entry: impl Into<String>) -> Self {
            Self {
                entry: Mutex::new(Some(entry.into())),
                ..Self::empty()
            }
        }

        fn empty() -> Self {
            Self {
                entry: Mutex::new(None),
                denies: false,
                panics: false,
                writable: AtomicBool::new(true),
            }
        }

        /// Holds `entry` and refuses writes until `writable` is set.
        fn unwritable(entry: impl Into<String>) -> Self {
            Self {
                writable: AtomicBool::new(false),
                ..Self::holding(entry)
            }
        }

        fn denying() -> Self {
            Self {
                denies: true,
                ..Self::empty()
            }
        }

        fn panicking() -> Self {
            Self {
                panics: true,
                ..Self::empty()
            }
        }

        fn entry(&self) -> Option<String> {
            self.entry.lock().unwrap().clone()
        }

        fn json(&self) -> serde_json::Value {
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

    mod oauth_credential {
        use super::*;

        mod expires_at_ms {
            use super::*;

            #[test]
            fn keeps_a_positive_value() {
                let cred = OAuthCredential {
                    access_token: String::new(),
                    refresh_token: None,
                    expires_at: 1_700_000_000_000,
                };
                assert_eq!(cred.expires_at_ms(), 1_700_000_000_000);
            }

            #[test]
            fn negative_clamps_to_zero() {
                let cred = OAuthCredential {
                    access_token: String::new(),
                    refresh_token: None,
                    expires_at: -1,
                };
                assert_eq!(cred.expires_at_ms(), 0);
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
            let store = store(Some(keychain), &dir.join("creds.json"), &dir.join("lock"));
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
                    store(None, &missing, &dir.path().join("lock")),
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
                    store(None, &creds, &dir.path().join("lock")),
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
                    store(
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
                    store(None, &creds, &dir.path().join("lock")),
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
                    store(None, &creds, &dir.path().join("lock")),
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

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), &server.uri())
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

            let token = load(&store(Some(&keychain), &creds, &lock), &server.uri())
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

            let token = load(&store(Some(&keychain), &creds, &lock), &server.uri())
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

            let token = load(&store(Some(&keychain), &creds, &lock), &server.uri())
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
            let token = load(&store(Some(&keychain), &creds, &lock), "http://unused")
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

            let token = load(&store(Some(&keychain), &creds, &lock), &server.uri())
                .await
                .unwrap();

            assert_eq!(token.access_token, "fresh-access");
            let unsaved = token.unsaved.expect("the rotated refresh token is held");
            assert_eq!(unsaved.refresh_token, "fresh-refresh");
            assert_eq!(unsaved.replaced, "old-refresh");
        }

        #[tokio::test]
        async fn without_refresh_token_keeps_nonexpired_as_is() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", None, now_millis().unwrap() + 60_000);

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), &server.uri())
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

            let token = load_token_from(
                &store(None, &creds, &lock),
                &server.uri(),
                &NoRoots,
                Some(&held),
                None,
            )
            .await
            .unwrap();

            assert_eq!(token.access_token, "held-access");
            let unsaved = token.unsaved.expect("the unsaved refresh token is kept");
            assert_eq!(unsaved.refresh_token, "held-refresh");
            assert_eq!(unsaved.replaced, "old-refresh");
            server.verify().await;
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_keeps_existing_token_if_unexpired() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "tok", Some("ref"), now_millis().unwrap() + 60_000);
            std::fs::create_dir(&lock).unwrap();

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let err = load(&store(None, &creds, &lock), "http://unused")
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

            let err = load(&store(None, &creds, &lock), &server.uri())
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

            let err = load(&store(None, &creds, &lock), &server.uri())
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
            let store = store(None, &creds, &lock);

            write_creds(&creds, "dead", Some("ref"), 0);
            let err = load(&store, "http://should-not-be-called")
                .await
                .expect_err("expired + lock failure must bail");
            write_creds(&creds, "tok", Some("ref"), now_millis().unwrap() + 60_000);
            let token = load(&store, "http://should-not-be-called").await.unwrap();

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
        }

        #[tokio::test(start_paused = true)]
        async fn lock_held_past_the_wait_fails_transiently_if_expired() {
            let dir = tempfile::tempdir().unwrap();
            let creds = dir.path().join("creds.json");
            let lock = dir.path().join("lock");
            write_creds(&creds, "dead", Some("ref"), 0);
            std::fs::create_dir(&lock).unwrap();

            let err = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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

            let token = load(&store(None, &creds, &lock), "http://should-not-be-called")
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
                let err = load(&store(keychain, &missing, &lock), "http://unused")
                    .await
                    .expect_err("no login anywhere");
                assert!(matches!(err, LoginError::Missing), "{err:?}");
            }

            let err = load(&store(Some(&denying), &missing, &lock), "http://unused")
                .await
                .expect_err("a denied Keychain and no file");
            assert!(matches!(err, LoginError::KeychainDenied), "{err:?}");

            let err = load(&store(Some(&garbled), &missing, &lock), "http://unused")
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
            let err = load(&store(Some(&empty), &unreadable, &lock), "http://unused")
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

    mod is_near_expiry {
        use super::*;

        #[test]
        fn counts_a_far_future_expiry_as_not_near() {
            assert!(!is_near_expiry(u64::MAX));
        }

        #[test]
        fn counts_an_expiry_in_the_past_as_near() {
            assert!(is_near_expiry(0));
        }
    }

    mod is_expired {
        use super::*;

        #[test]
        fn counts_a_far_future_expiry_as_not_expired() {
            assert!(!is_expired(u64::MAX));
        }

        #[test]
        fn counts_an_expiry_in_the_past_as_expired() {
            assert!(is_expired(0));
        }
    }

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

    mod keychain_service {
        use super::*;

        #[test]
        fn suffixes_a_claude_config_dir_with_its_hash() {
            let service = |dir| temp_env::with_var("CLAUDE_CONFIG_DIR", dir, keychain_service);

            assert_eq!(service(None), "Claude Code-credentials");
            assert_eq!(
                service(Some("/srv/claude")),
                "Claude Code-credentials-bc29611f"
            );
            assert_eq!(
                service(Some("/srv/cafe\u{301}")),
                service(Some("/srv/caf\u{e9}"))
            );
        }
    }

    mod acquire_lock {
        use super::*;

        #[tokio::test]
        async fn creates_and_drop_removes() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");

            let guard = acquire_lock(&lock_path).await.unwrap();
            assert!(lock_path.exists());

            drop(guard);
            assert!(!lock_path.exists());
        }

        #[tokio::test(start_paused = true)]
        async fn clears_stale_lock() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            std::fs::create_dir(&lock_path).unwrap();
            set_lock_mtime(
                &lock_path,
                SystemTime::now() - LOCK_STALE_THRESHOLD - Duration::from_secs(1),
            );

            let _guard = acquire_lock(&lock_path).await.unwrap();
            assert!(!is_stale_lock(&lock_path));
        }

        /// Sets the lock's mtime to `at`, as its holder or a process taking it over would.
        fn set_lock_mtime(lock_path: &Path, at: SystemTime) -> SystemTime {
            open_dir(lock_path).unwrap().set_modified(at).unwrap();
            lock_mtime(lock_path).unwrap()
        }

        #[tokio::test(start_paused = true)]
        async fn keeps_a_held_lock_fresh_until_dropped() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            let long_ago = SystemTime::now() - LOCK_STALE_THRESHOLD * 2;
            *guard.mtime.lock().unwrap() = Some(set_lock_mtime(&lock_path, long_ago));

            tokio::time::sleep(LOCK_UPDATE_INTERVAL + Duration::from_millis(1)).await;

            assert!(!is_stale_lock(&lock_path));
            drop(guard);
            assert!(!lock_path.exists());
        }

        #[tokio::test(start_paused = true)]
        async fn leaves_a_lock_another_process_took_over_alone() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            let other = set_lock_mtime(&lock_path, SystemTime::now() - Duration::from_secs(3));

            tokio::time::sleep(LOCK_UPDATE_INTERVAL * 2).await;

            assert_eq!(lock_mtime(&lock_path).unwrap(), other);
            assert_eq!(*guard.mtime.lock().unwrap(), None);
            drop(guard);
            assert!(lock_path.exists());
        }

        #[tokio::test]
        async fn leaves_a_lock_taken_over_before_the_next_refresh_alone() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            let guard = acquire_lock(&lock_path).await.unwrap();
            set_lock_mtime(&lock_path, SystemTime::now() - Duration::from_secs(3));

            drop(guard);

            assert!(lock_path.exists());
        }

        #[test]
        fn matches_proper_lockfiles_defaults() {
            assert_eq!(LOCK_STALE_THRESHOLD, Duration::from_secs(10));
            assert_eq!(LOCK_UPDATE_INTERVAL, Duration::from_secs(5));
        }

        #[tokio::test(start_paused = true)]
        async fn reports_a_lock_held_past_the_wait() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("test.lock");
            std::fs::create_dir(&lock_path).unwrap();

            let err = acquire_lock(&lock_path).await.err().expect("held lock");

            assert_eq!(
                format!("{err:#}"),
                format!("another process held {} for over 20 s", lock_path.display())
            );
        }

        #[tokio::test]
        async fn surfaces_io_error() {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("missing").join("test.lock");

            let err = acquire_lock(&lock_path)
                .await
                .err()
                .expect("missing parent must fail");
            let msg = format!("{err:#}");
            assert!(msg.contains("failed to create lock"), "{msg}");
        }
    }
}
