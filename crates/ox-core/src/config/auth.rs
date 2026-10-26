//! Credential resolution: which secret authenticates requests, where it came from, and how it
//! travels on the wire.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_util::task::TaskTracker;
use tracing::warn;

use super::gateway::CUSTOM_HEADERS_VAR;
use super::load::user_config_hint;
use super::oauth::{ClaudeCodeKeychain, Login, LoginError, SessionLogin, login_stores};
use super::schema::{AuthValue, ClientAuth};
use crate::util::env;
use crate::util::text::join_sentences;
use crate::util::tls::Trust;

// ── AuthKind ──

/// How a credential authenticates on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// `x-api-key` header.
    ApiKey,
    /// `Authorization: Bearer` with the API-key request shape, as LLM gateways expect.
    AuthToken,
    /// `Authorization: Bearer` plus the OAuth beta that Claude subscription tokens require.
    OAuth,
}

impl AuthKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ApiKey => "API key",
            Self::AuthToken => "Auth token",
            Self::OAuth => "OAuth",
        }
    }

    /// The request header the credential travels in.
    pub(super) fn header_name(self) -> HeaderName {
        match self {
            Self::ApiKey => HeaderName::from_static("x-api-key"),
            Self::AuthToken | Self::OAuth => AUTHORIZATION,
        }
    }
}

// ── AuthSource ──

/// Where the active credential came from, listed in resolution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(strum::VariantArray))]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AuthSource {
    AuthTokenEnv,
    ApiKeyEnv,
    ApiKeyFile,
    ClientBearer,
    ClientApiKey,
    ClientOAuth,
    OAuthTokenEnv,
    /// oxide-code's stored login from `/login` (`$XDG_DATA_HOME/ox/credentials.json`), refreshed
    /// before a request near its expiry and reloaded after a 401.
    OxLogin,
    /// Claude Code's stored login (macOS Keychain or `.credentials.json` in `~/.claude` or
    /// `$CLAUDE_CONFIG_DIR`), refreshed before a request near its expiry and reloaded after a 401.
    ClaudeCodeLogin,
}

impl AuthSource {
    /// The env and config sources, in the precedence order they win over each other and over the
    /// stored logins.
    const EXPLICIT: [Self; 4] = [
        Self::AuthTokenEnv,
        Self::ApiKeyEnv,
        Self::ApiKeyFile,
        Self::OAuthTokenEnv,
    ];

    pub const fn kind(self) -> AuthKind {
        match self {
            Self::AuthTokenEnv | Self::ClientBearer => AuthKind::AuthToken,
            Self::ApiKeyEnv | Self::ApiKeyFile | Self::ClientApiKey => AuthKind::ApiKey,
            Self::OAuthTokenEnv | Self::ClientOAuth | Self::OxLogin | Self::ClaudeCodeLogin => {
                AuthKind::OAuth
            }
        }
    }

    /// Returns whether the credential is a stored login, which `/login` and `/logout` can replace.
    pub(crate) const fn is_stored_login(self) -> bool {
        self.login().is_some()
    }

    const fn login(self) -> Option<Login> {
        match self {
            Self::OxLogin => Some(Login::Ox),
            Self::ClaudeCodeLogin => Some(Login::ClaudeCode),
            Self::AuthTokenEnv
            | Self::ApiKeyEnv
            | Self::ApiKeyFile
            | Self::OAuthTokenEnv
            | Self::ClientBearer
            | Self::ClientApiKey
            | Self::ClientOAuth => None,
        }
    }

    /// The env var the credential was read from, `None` for the config file or a stored login.
    pub(super) const fn env_var(self) -> Option<&'static str> {
        match self {
            Self::AuthTokenEnv => Some("ANTHROPIC_AUTH_TOKEN"),
            Self::ApiKeyEnv => Some("ANTHROPIC_API_KEY"),
            Self::OAuthTokenEnv => Some("CLAUDE_CODE_OAUTH_TOKEN"),
            Self::ApiKeyFile
            | Self::ClientBearer
            | Self::ClientApiKey
            | Self::ClientOAuth
            | Self::OxLogin
            | Self::ClaudeCodeLogin => None,
        }
    }

    /// The env var, config key, or store the credential was read from.
    fn origin(self) -> &'static str {
        if matches!(
            self,
            Self::ClientBearer | Self::ClientApiKey | Self::ClientOAuth
        ) {
            return "client.auth";
        }
        self.env_var()
            .or(self.login().map(Login::name))
            .unwrap_or("client.api_key")
    }

    /// Recovery step shown when the API rejects the credential with HTTP 401.
    pub(crate) const fn rejection_hint(self) -> &'static str {
        match self {
            Self::AuthTokenEnv => "Check ANTHROPIC_AUTH_TOKEN.",
            Self::ApiKeyEnv => "Check ANTHROPIC_API_KEY.",
            Self::ApiKeyFile => {
                "Check client.api_key in your user config. `/config` shows which file sets it."
            }
            Self::ClientBearer | Self::ClientApiKey | Self::ClientOAuth => {
                "Check client.auth in your user config. `/config` shows which file sets it."
            }
            Self::OAuthTokenEnv => {
                "CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. \
                 Generate a new one with `claude setup-token`."
            }
            Self::OxLogin => Login::Ox.sign_in_step(),
            Self::ClaudeCodeLogin => Login::ClaudeCode.sign_in_step(),
        }
    }
}

/// Kind and origin, as `/status` and `/config` show it.
impl fmt::Display for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.kind().label(), self.origin())
    }
}

impl From<Login> for AuthSource {
    fn from(login: Login) -> Self {
        match login {
            Login::Ox => Self::OxLogin,
            Login::ClaudeCode => Self::ClaudeCodeLogin,
        }
    }
}

// ── Auth ──

/// Resolved credential. Clones share a stored login's token, so a refresh made for one request
/// serves them all. `Debug` omits the secret.
#[derive(Clone)]
pub(crate) struct Auth {
    source: AuthSource,
    secret: Secret,
}

#[derive(Clone)]
enum Secret {
    Fixed(String),
    Stored(Arc<SessionLogin>),
}

impl Auth {
    /// A credential sent unchanged for the whole session, as env vars and the config file supply.
    pub(crate) fn new(source: AuthSource, secret: impl Into<String>) -> Self {
        Self {
            source,
            secret: Secret::Fixed(secret.into()),
        }
    }

    pub(super) fn stored(login: SessionLogin) -> Self {
        Self {
            source: login.login().into(),
            secret: Secret::Stored(Arc::new(login)),
        }
    }

    pub(crate) const fn source(&self) -> AuthSource {
        self.source
    }

    pub(crate) const fn kind(&self) -> AuthKind {
        self.source.kind()
    }

    #[cfg(test)]
    pub(crate) fn secret(&self) -> String {
        match &self.secret {
            Secret::Fixed(secret) => secret.clone(),
            Secret::Stored(login) => login.cached_token(),
        }
    }

    /// The credential the next request sends, refreshing a stored login near expiry first. That
    /// reload runs on a task in `reloads`, which outlives a dropped request. When the refresh fails
    /// for a reason a later attempt may clear, [`is_transient`] says so.
    pub(crate) async fn credential(
        &self,
        trust: &Arc<dyn Trust>,
        reloads: &TaskTracker,
    ) -> Result<Credential> {
        let secret = match &self.secret {
            Secret::Fixed(secret) => secret.clone(),
            Secret::Stored(login) => login.access_token(trust, reloads).await?,
        };
        Credential::new(self.source, secret)
    }

    /// The credential to resend a request with after the API rejected `rejected` with HTTP 401: a
    /// stored login's token that another process stored or a refresh issued. `None` for other
    /// credentials and for a login that cannot be refreshed. The reload runs in `reloads`, as in
    /// [`Self::credential`]. When its refresh fails for a reason a later attempt may clear,
    /// [`is_transient`] says so.
    pub(crate) async fn reload(
        &self,
        rejected: &Credential,
        trust: &Arc<dyn Trust>,
        reloads: &TaskTracker,
    ) -> Result<Option<Credential>> {
        let Secret::Stored(login) = &self.secret else {
            return Ok(None);
        };
        match login.reload(&rejected.secret, trust, reloads).await {
            Ok(secret) => Credential::new(self.source, secret).map(Some),
            Err(e) if e.is_transient() => Err(e.into()),
            Err(e) => {
                warn!(
                    error = %format_args!("{e:#}"),
                    "failed to reload the stored login after HTTP 401"
                );
                Ok(None)
            }
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Whether `error`, from [`Auth::credential`] or [`Auth::reload`], is a stored login's refresh that
/// a later attempt may complete.
pub(crate) fn is_transient(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<LoginError>()
        .is_some_and(LoginError::is_transient)
}

// ── Credential ──

/// A credential as one request sends it.
pub(crate) struct Credential {
    secret: String,
    name: HeaderName,
    value: HeaderValue,
}

impl Credential {
    /// Fails when `secret` cannot travel in a header.
    fn new(source: AuthSource, secret: String) -> Result<Self> {
        let kind = source.kind();
        let value = match kind {
            AuthKind::ApiKey => secret.clone(),
            AuthKind::AuthToken | AuthKind::OAuth => format!("Bearer {secret}"),
        };
        let mut value = HeaderValue::from_str(&value)
            .with_context(|| format!("{source} is not a valid HTTP header value"))?;
        value.set_sensitive(true);
        Ok(Self {
            secret,
            name: kind.header_name(),
            value,
        })
    }

    /// The header the credential travels in, with its value marked sensitive.
    pub(crate) const fn header(&self) -> (&HeaderName, &HeaderValue) {
        (&self.name, &self.value)
    }
}

// ── Resolution ──

/// Selected file credential, whose contents are read once at startup after declaration checks.
#[derive(Debug, Clone)]
pub(super) struct PendingAuth {
    pub(super) source: AuthSource,
    path: std::path::PathBuf,
}

impl PendingAuth {
    pub(super) fn load(&self) -> Result<Auth> {
        let raw = std::fs::read_to_string(&self.path).with_context(|| {
            format!("failed to read client.auth.file at {}", self.path.display())
        })?;
        let secret = raw.trim_end_matches(['\r', '\n']);
        ensure!(
            !secret.is_empty(),
            "client.auth.file at {} is empty",
            self.path.display()
        );
        Credential::new(self.source, secret.to_owned())
            .with_context(|| format!("invalid client.auth.file at {}", self.path.display()))?;
        Ok(Auth::new(self.source, secret))
    }
}

pub(super) fn configured_auth(auth: ClientAuth) -> Result<(Option<Auth>, Option<PendingAuth>)> {
    let (source, AuthValue { value, file }) = match auth {
        ClientAuth::Bearer(value) => (AuthSource::ClientBearer, value),
        ClientAuth::ApiKey(value) => (AuthSource::ClientApiKey, value),
        ClientAuth::Oauth(value) => (AuthSource::ClientOAuth, value),
        ClientAuth::Login {} => return Ok((None, None)),
    };
    if let Some(secret) = value {
        Credential::new(source, secret.clone())?;
        return Ok((Some(Auth::new(source, secret)), None));
    }
    let path = crate::util::path::expand_user(&file.expect("validated auth has a source"))?;
    Ok((None, Some(PendingAuth { source, path })))
}

/// Credential resolution failure. Messages capture config paths when the error is constructed
/// because those paths depend on the current environment.
#[derive(Debug, thiserror::Error)]
pub(super) enum CredentialError {
    /// The base URL is a gateway, which never receives a stored login.
    #[error("{message}")]
    Gateway { message: String },
    /// No stored login supplied a token. A TUI session starts signed out, since `/login` can sign
    /// it in.
    #[error("{message}")]
    SignedOut { cause: LoginError, message: String },
}

impl CredentialError {
    fn gateway(base_url: &super::BaseUrl) -> Self {
        let message = format!(
            "no credentials for {base_url}. \
             Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
             or add `api_key` to {config}. \
             Stored logins are sent only to the Anthropic API.",
            config = user_config_hint(),
        );
        Self::Gateway { message }
    }

    fn signed_out(cause: LoginError) -> Self {
        let message = match &cause {
            LoginError::Missing => format!(
                "no credentials found: no API key or token is set, no oxide-code login is stored, \
                 and no Claude Code login is in {stores}. \
                 Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, \
                 add `api_key` to {config}, or sign in with `/login` in the `ox` TUI.",
                stores = login_stores(),
                config = user_config_hint(),
            ),
            cause => cause.to_string(),
        };
        Self::SignedOut { cause, message }
    }
}

/// What a session shows on a request while signed out, and at startup when no login is stored.
pub(crate) const SIGNED_OUT: &str = "Not signed in. Run `/login` to sign in.";

const RETRY_STEP: &str = "The next prompt tries the stored login again.";

/// Why a TUI session has no stored login to send.
#[derive(Debug, Clone)]
pub(crate) struct LoginUnavailable {
    /// What the session shows at startup or when a retry fails.
    pub(crate) notice: String,
    /// Whether a later attempt may load the login, so the next prompt or compaction tries it.
    pub(crate) retry: bool,
}

impl LoginUnavailable {
    pub(super) fn new(e: &LoginError) -> Self {
        let notice = match e {
            LoginError::Missing => SIGNED_OUT.to_owned(),
            e if e.is_transient() => join_sentences(&format!("Not signed in: {e}"), RETRY_STEP),
            e => format!("Not signed in: {e}"),
        };
        Self {
            notice,
            retry: e.is_transient(),
        }
    }
}

/// Loads the first stored login again for a session that a transient failure signed out.
pub(crate) async fn retry_stored_login(
    keychain: ClaudeCodeKeychain,
    trust: &dyn Trust,
) -> Result<Auth, LoginUnavailable> {
    stored_login(keychain, trust)
        .await
        .map_err(|e| LoginUnavailable::new(&e))
}

/// Explicit credentials in precedence order: env and config values, before the stored logins.
pub(super) fn explicit_credentials(
    mut file_api_key: Option<String>,
) -> [(AuthSource, Option<String>); 4] {
    AuthSource::EXPLICIT.map(|source| {
        let secret = match source.env_var() {
            Some(var) => env::string(var),
            None => file_api_key.take(),
        };
        (source, secret)
    })
}

/// The env vars that can carry a secret ox authenticates with: each credential variable, and
/// [`CUSTOM_HEADERS_VAR`], which gateways use for their keys. Commands run for the model and the
/// git probes do not inherit them.
pub(crate) fn secret_env_vars() -> impl Iterator<Item = &'static str> {
    AuthSource::EXPLICIT
        .into_iter()
        .filter_map(AuthSource::env_var)
        .chain([CUSTOM_HEADERS_VAR])
}

/// The first credential set in `explicit`, which [`explicit_credentials`] lists by precedence.
pub(super) fn first_explicit(
    explicit: impl IntoIterator<Item = (AuthSource, Option<String>)>,
) -> Option<Auth> {
    explicit
        .into_iter()
        .find_map(|(source, value)| value.map(|secret| Auth::new(source, secret)))
}

/// The stored login a session falls back on without an explicit credential. `stored` is awaited
/// only when `base_url` is first-party, so a personal subscription token never reaches a gateway
/// the user did not hand it to.
pub(super) async fn fallback(
    base_url: &super::BaseUrl,
    stored: impl Future<Output = Result<Auth, LoginError>>,
) -> Result<Auth, CredentialError> {
    if !base_url.is_first_party() {
        return Err(CredentialError::gateway(base_url));
    }
    stored.await.map_err(CredentialError::signed_out)
}

/// Loads the stored login to fall back on, trying oxide-code's own before Claude Code's. A login
/// that exists but fails ends the search, so a broken oxide-code login never quietly yields to
/// another account.
pub(super) async fn stored_login(
    keychain: ClaudeCodeKeychain,
    trust: &dyn Trust,
) -> Result<Auth, LoginError> {
    first_stored_login(|login| load_login(login, keychain, trust)).await
}

async fn first_stored_login<T, F, Fut>(load: F) -> Result<T, LoginError>
where
    F: Fn(Login) -> Fut,
    Fut: Future<Output = Result<T, LoginError>>,
{
    for login in [Login::Ox, Login::ClaudeCode] {
        match load(login).await {
            Ok(found) => return Ok(found),
            Err(LoginError::Missing) => {}
            Err(e) => return Err(e),
        }
    }
    Err(LoginError::Missing)
}

/// Loads the Claude Code login, which takes over once `/logout` removes the oxide-code login.
/// Returns `None` when Claude Code stores no login, and an error when its login exists but fails.
pub(crate) async fn claude_code_login(
    keychain: ClaudeCodeKeychain,
    trust: &dyn Trust,
) -> Result<Option<Auth>> {
    match load_login(Login::ClaudeCode, keychain, trust).await {
        Ok(auth) => Ok(Some(auth)),
        Err(LoginError::Missing) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

async fn load_login(
    login: Login,
    keychain: ClaudeCodeKeychain,
    trust: &dyn Trust,
) -> Result<Auth, LoginError> {
    SessionLogin::load(login, keychain, trust)
        .await
        .map(Auth::stored)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use strum::VariantArray as _;
    use tracing::instrument::WithSubscriber as _;

    use super::*;
    use crate::config::oauth::testing::{ox_login_in, write_creds};
    use crate::util::log::testing::LogCapture;
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::local_testing::no_roots;
    use crate::util::tls::testing::NoRoots;

    #[test]
    fn debug_omits_secret() {
        for source in AuthSource::VARIANTS {
            let debug = format!("{:?}", Auth::new(*source, "sk-must-not-leak"));

            assert!(!debug.contains("sk-must-not-leak"), "{debug}");
            assert!(debug.contains(&format!("{source:?}")), "{debug}");
        }
    }

    mod auth_source {
        use super::*;

        /// `f` of every source, in resolution order.
        fn each_source<T>(f: impl Fn(AuthSource) -> T) -> Vec<T> {
            AuthSource::VARIANTS.iter().copied().map(f).collect()
        }

        #[test]
        fn display_names_kind_and_origin_for_each_source() {
            let labels = each_source(|source| source.to_string());
            assert_eq!(
                labels,
                [
                    "Auth token (ANTHROPIC_AUTH_TOKEN)",
                    "API key (ANTHROPIC_API_KEY)",
                    "API key (client.api_key)",
                    "Auth token (client.auth)",
                    "API key (client.auth)",
                    "OAuth (client.auth)",
                    "OAuth (CLAUDE_CODE_OAUTH_TOKEN)",
                    "OAuth (oxide-code login)",
                    "OAuth (Claude Code login)",
                ],
            );
        }

        mod kind {
            use super::*;

            #[test]
            fn maps_each_source_to_its_wire_scheme() {
                let kinds = each_source(AuthSource::kind);
                assert_eq!(
                    kinds,
                    [
                        AuthKind::AuthToken,
                        AuthKind::ApiKey,
                        AuthKind::ApiKey,
                        AuthKind::AuthToken,
                        AuthKind::ApiKey,
                        AuthKind::OAuth,
                        AuthKind::OAuth,
                        AuthKind::OAuth,
                        AuthKind::OAuth,
                    ],
                );
            }
        }

        mod is_stored_login {
            use super::*;

            #[test]
            fn holds_only_for_the_two_logins() {
                assert_eq!(
                    each_source(AuthSource::is_stored_login),
                    [false, false, false, false, false, false, false, true, true],
                );
            }
        }

        mod env_var {
            use super::*;

            #[test]
            fn names_only_environment_sources() {
                assert_eq!(
                    each_source(AuthSource::env_var),
                    [
                        Some("ANTHROPIC_AUTH_TOKEN"),
                        Some("ANTHROPIC_API_KEY"),
                        None,
                        None,
                        None,
                        None,
                        Some("CLAUDE_CODE_OAUTH_TOKEN"),
                        None,
                        None,
                    ],
                );
            }
        }

        mod rejection_hint {
            use super::*;

            #[test]
            fn names_where_to_fix_each_source() {
                assert_eq!(
                    each_source(AuthSource::rejection_hint),
                    [
                        "Check ANTHROPIC_AUTH_TOKEN.",
                        "Check ANTHROPIC_API_KEY.",
                        "Check client.api_key in your user config. \
                         `/config` shows which file sets it.",
                        "Check client.auth in your user config. \
                         `/config` shows which file sets it.",
                        "Check client.auth in your user config. \
                         `/config` shows which file sets it.",
                        "Check client.auth in your user config. \
                         `/config` shows which file sets it.",
                        "CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. \
                         Generate a new one with `claude setup-token`.",
                        "Run `/login` to sign in again.",
                        "Run `claude` to sign in again.",
                    ],
                );
            }
        }
    }

    mod credential {
        use super::*;

        #[tokio::test]
        async fn sends_api_keys_raw_and_tokens_as_bearer() {
            for (source, name, value) in [
                (AuthSource::ApiKeyEnv, "x-api-key", "secret-value"),
                (
                    AuthSource::AuthTokenEnv,
                    "authorization",
                    "Bearer secret-value",
                ),
                (
                    AuthSource::OAuthTokenEnv,
                    "authorization",
                    "Bearer secret-value",
                ),
                (AuthSource::ClientApiKey, "x-api-key", "secret-value"),
                (
                    AuthSource::ClientBearer,
                    "authorization",
                    "Bearer secret-value",
                ),
                (
                    AuthSource::ClientOAuth,
                    "authorization",
                    "Bearer secret-value",
                ),
            ] {
                let credential = Auth::new(source, "secret-value")
                    .credential(&no_roots(), &TaskTracker::new())
                    .await
                    .unwrap();

                let (got_name, got_value) = credential.header();
                assert_eq!(got_name, name, "{source:?}");
                assert_eq!(got_value, value, "{source:?}");
                assert!(got_value.is_sensitive(), "{source:?}");
            }
        }
    }

    mod reload {
        use super::*;

        #[tokio::test]
        async fn gives_up_on_env_credentials_and_unreadable_logins() {
            let dir = tempfile::tempdir().unwrap();
            for auth in [
                Auth::new(AuthSource::OAuthTokenEnv, "env-token"),
                ox_login_in(
                    &dir.path().join("missing.json"),
                    "http://unused",
                    "rejected",
                    9_999_999_999_999,
                ),
            ] {
                let (trust, reloads) = (no_roots(), TaskTracker::new());
                let rejected = auth.credential(&trust, &reloads).await.unwrap();
                let logs = LogCapture::default();

                let reloaded = auth
                    .reload(&rejected, &trust, &reloads)
                    .with_subscriber(logs.dispatch())
                    .await
                    .unwrap();

                assert!(reloaded.is_none(), "{auth:?}");
                let expected = if auth.source == AuthSource::OxLogin {
                    vec![BTreeMap::from([
                        ("error".to_owned(), "no stored login found".to_owned()),
                        (
                            "message".to_owned(),
                            "failed to reload the stored login after HTTP 401".to_owned(),
                        ),
                    ])]
                } else {
                    Vec::new()
                };
                assert_eq!(logs.events(), expected, "{auth:?}");
            }
        }

        #[tokio::test]
        async fn fails_transiently_when_the_token_endpoint_sends_no_response() {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("credentials.json");
            write_creds(&file, "rejected", Some("ref"), 9_999_999_999_999);
            let auth = ox_login_in(
                &file,
                "http://127.0.0.1:9/token",
                "rejected",
                9_999_999_999_999,
            );
            let (trust, reloads) = (no_roots(), TaskTracker::new());
            let rejected = auth.credential(&trust, &reloads).await.unwrap();

            let err = auth
                .reload(&rejected, &trust, &reloads)
                .await
                .err()
                .expect("the refresh of the rejected token fails");

            assert!(is_transient(&err), "{err:#}");
        }
    }

    mod login_unavailable {
        use super::*;

        #[test]
        fn names_the_step_that_signs_in_and_retries_no_lasting_failure() {
            for (e, notice) in [
                (
                    LoginError::Missing,
                    "Not signed in. Run `/login` to sign in.",
                ),
                (
                    LoginError::Expired(Login::Ox),
                    "Not signed in: oxide-code login has expired. Run `/login` to sign in again.",
                ),
                (
                    LoginError::Unreadable(
                        Login::ClaudeCode,
                        anyhow::anyhow!("failed to parse the credentials in the macOS Keychain"),
                    ),
                    "Not signed in: failed to load the Claude Code login: \
                     failed to parse the credentials in the macOS Keychain. \
                     Run `claude` to sign in again.",
                ),
            ] {
                let unavailable = LoginUnavailable::new(&e);

                assert_eq!(unavailable.notice, notice);
                assert!(!unavailable.retry, "{notice}");
            }
        }

        #[test]
        fn retries_a_transient_failure_and_says_so() {
            let unavailable = LoginUnavailable::new(&LoginError::RefreshFailed {
                login: Login::Ox,
                error: anyhow::anyhow!("token endpoint returned 503"),
                transient: true,
            });

            assert_eq!(
                unavailable.notice,
                "Not signed in: failed to refresh the expired oxide-code login: \
                 token endpoint returned 503. \
                 Run `/login` to sign in again. \
                 The next prompt tries the stored login again.",
            );
            assert!(unavailable.retry);
        }
    }

    mod explicit_credentials {
        use super::*;

        #[test]
        fn reads_env_in_precedence_order_with_file_key_third() {
            let vars = [
                ("ANTHROPIC_AUTH_TOKEN", Some("bearer")),
                ("ANTHROPIC_API_KEY", None),
                ("CLAUDE_CODE_OAUTH_TOKEN", Some("oauth")),
            ];
            let got =
                temp_env::with_vars(vars, || explicit_credentials(Some("file-key".to_owned())));
            assert_eq!(
                got,
                [
                    (AuthSource::AuthTokenEnv, Some("bearer".to_owned())),
                    (AuthSource::ApiKeyEnv, None),
                    (AuthSource::ApiKeyFile, Some("file-key".to_owned())),
                    (AuthSource::OAuthTokenEnv, Some("oauth".to_owned())),
                ],
            );
        }
    }

    mod secret_env_vars {
        use super::*;

        #[test]
        fn lists_each_credential_variable_and_the_custom_headers() {
            assert_eq!(
                secret_env_vars().collect::<Vec<_>>(),
                [
                    "ANTHROPIC_AUTH_TOKEN",
                    "ANTHROPIC_API_KEY",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    "ANTHROPIC_CUSTOM_HEADERS",
                ],
            );
        }
    }

    mod first_explicit {
        use super::*;

        fn explicit(present: &[AuthSource]) -> Vec<(AuthSource, Option<String>)> {
            AuthSource::EXPLICIT
                .iter()
                .map(|&source| {
                    let value = present
                        .contains(&source)
                        .then(|| format!("{source:?}-secret"));
                    (source, value)
                })
                .collect()
        }

        #[test]
        fn picks_the_highest_precedence_credential() {
            // Each row drops the previous winner, so every adjacent precedence pair is exercised.
            for (present, winner) in [
                (&AuthSource::EXPLICIT[..], AuthSource::AuthTokenEnv),
                (&AuthSource::EXPLICIT[1..], AuthSource::ApiKeyEnv),
                (&AuthSource::EXPLICIT[2..], AuthSource::ApiKeyFile),
                (&AuthSource::EXPLICIT[3..], AuthSource::OAuthTokenEnv),
            ] {
                let auth = first_explicit(explicit(present)).unwrap();

                assert_eq!(auth.source(), winner);
                assert_eq!(auth.secret(), format!("{winner:?}-secret"));
            }
        }

        #[test]
        fn finds_none_when_nothing_is_set() {
            assert!(first_explicit(explicit(&[])).is_none());
        }
    }

    mod fallback {
        use super::*;

        const FIRST_PARTY: &str = "https://api.anthropic.com";
        const GATEWAY: &str = "https://gateway.example.com/anthropic";

        fn base_url(raw: &str) -> crate::config::BaseUrl {
            crate::config::BaseUrl::parse(raw.to_owned()).unwrap()
        }

        fn unreachable_store() -> impl Future<Output = Result<Auth, LoginError>> {
            std::future::poll_fn(|_| panic!("stored credentials must not be read"))
        }

        fn stored_ox_login() -> std::future::Ready<Result<Auth, LoginError>> {
            std::future::ready(Ok(Auth::new(AuthSource::OxLogin, "stored-token")))
        }

        #[tokio::test]
        async fn falls_back_to_stored_login_on_first_party_host() {
            let auth = fallback(&base_url(FIRST_PARTY), stored_ox_login())
                .await
                .unwrap();
            assert_eq!(auth.source(), AuthSource::OxLogin);
            assert_eq!(auth.secret(), "stored-token");
        }

        #[tokio::test]
        async fn refuses_stored_login_for_third_party_host_without_reading_it() {
            let config = abs("/srv/cfg");

            let err = temp_env::async_with_vars(
                [("XDG_CONFIG_HOME", Some(config.as_str()))],
                fallback(&base_url(GATEWAY), unreachable_store()),
            )
            .await
            .expect_err("stored login must not go to a gateway");

            assert!(matches!(err, CredentialError::Gateway { .. }), "{err:?}");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "no credentials for {GATEWAY}. \
                     Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
                     or add `api_key` to {file}. \
                     Stored logins are sent only to the Anthropic API.",
                    file = Path::new(&config).join("ox").join("config.toml").display(),
                ),
            );
        }

        #[tokio::test]
        async fn missing_login_lists_every_credential_option() {
            let claude_dir = abs("/srv/claude");
            let config = abs("/srv/cfg");

            let err = temp_env::async_with_vars(
                [
                    ("CLAUDE_CONFIG_DIR", Some(claude_dir.as_str())),
                    ("XDG_CONFIG_HOME", Some(config.as_str())),
                ],
                fallback(&base_url(FIRST_PARTY), async { Err(LoginError::Missing) }),
            )
            .await
            .expect_err("no credential anywhere");

            assert!(
                matches!(
                    err,
                    CredentialError::SignedOut {
                        cause: LoginError::Missing,
                        ..
                    }
                ),
                "{err:?}"
            );
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "no credentials found: \
                     no API key or token is set, no oxide-code login is stored, \
                     and no Claude Code login is in {keychain}{file}. \
                     Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, \
                     add `api_key` to {config_file}, \
                     or sign in with `/login` in the `ox` TUI.",
                    keychain = if cfg!(target_os = "macos") {
                        "the macOS Keychain or "
                    } else {
                        ""
                    },
                    file = std::path::absolute(&claude_dir)
                        .unwrap()
                        .join(".credentials.json")
                        .display(),
                    config_file = Path::new(&config).join("ox").join("config.toml").display(),
                ),
            );
        }

        #[tokio::test]
        async fn other_login_failures_keep_their_own_recovery_step() {
            let err = fallback(&base_url(FIRST_PARTY), async {
                Err(LoginError::Expired(Login::ClaudeCode))
            })
            .await
            .expect_err("an expired login must fail");
            assert_eq!(
                format!("{err:#}"),
                "Claude Code login has expired. Run `claude` to sign in again.",
            );
        }
    }

    mod stored_login {
        use super::*;

        #[tokio::test]
        async fn reads_the_oxide_code_login_from_the_data_dir() {
            let data = tempfile::tempdir().unwrap();
            std::fs::create_dir(data.path().join("ox")).unwrap();
            write_creds(
                &data.path().join("ox").join("credentials.json"),
                "ox-token",
                None,
                9_999_999_999_999,
            );

            let auth = temp_env::async_with_vars(
                [("XDG_DATA_HOME", Some(data.path()))],
                stored_login(ClaudeCodeKeychain::NONE, &NoRoots),
            )
            .await
            .unwrap();
            assert_eq!(auth.source(), AuthSource::OxLogin);
            assert_eq!(auth.secret(), "ox-token");
        }
    }

    mod first_stored_login {
        use super::*;

        #[tokio::test]
        async fn prefers_the_oxide_code_login() {
            let found = first_stored_login(|login| async move {
                match login {
                    Login::Ox => Ok("ox-token"),
                    Login::ClaudeCode => panic!("the Claude Code login must not be read"),
                }
            })
            .await
            .unwrap();
            assert_eq!(found, "ox-token");
        }

        #[tokio::test]
        async fn falls_back_to_claude_code_without_an_oxide_code_login() {
            let found = first_stored_login(|login| async move {
                match login {
                    Login::Ox => Err(LoginError::Missing),
                    Login::ClaudeCode => Ok("claude-code-token"),
                }
            })
            .await
            .unwrap();
            assert_eq!(found, "claude-code-token");
        }

        #[tokio::test]
        async fn stops_at_a_failing_oxide_code_login() {
            let err = first_stored_login(|login| async move {
                match login {
                    Login::Ox => Err::<&str, _>(LoginError::Expired(Login::Ox)),
                    Login::ClaudeCode => panic!("a failing oxide-code login must not yield"),
                }
            })
            .await
            .unwrap_err();
            assert!(matches!(err, LoginError::Expired(Login::Ox)), "{err:?}");
        }

        #[tokio::test]
        async fn is_missing_without_either_login() {
            let err = first_stored_login(|_| async { Err::<&str, _>(LoginError::Missing) })
                .await
                .unwrap_err();
            assert!(matches!(err, LoginError::Missing), "{err:?}");
        }
    }

    mod claude_code_login {
        use super::*;

        #[tokio::test]
        async fn reads_the_claude_code_credentials_file() {
            let home = tempfile::tempdir().unwrap();
            let file = home.path().join(".claude").join(".credentials.json");

            let (missing, unreadable, auth) = temp_env::async_with_vars(
                [(HOME_VAR, Some(home.path())), ("CLAUDE_CONFIG_DIR", None)],
                async {
                    let missing = claude_code_login(ClaudeCodeKeychain::NONE, &NoRoots).await;
                    std::fs::create_dir(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, "not json").unwrap();
                    let unreadable = claude_code_login(ClaudeCodeKeychain::NONE, &NoRoots).await;
                    write_creds(&file, "claude-code-token", None, 9_999_999_999_999);
                    (
                        missing,
                        unreadable,
                        claude_code_login(ClaudeCodeKeychain::NONE, &NoRoots).await,
                    )
                },
            )
            .await;

            assert!(missing.unwrap().is_none());
            let unreadable = unreadable.unwrap_err().to_string();
            assert!(
                unreadable.ends_with("Run `claude` to sign in again."),
                "{unreadable}"
            );
            let auth = auth.unwrap().unwrap();
            assert_eq!(auth.source(), AuthSource::ClaudeCodeLogin);
            assert_eq!(auth.secret(), "claude-code-token");
        }
    }
}
