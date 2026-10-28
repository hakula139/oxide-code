//! The credential requests send: where it came from, how it travels on the wire, and the stored
//! logins a session falls back on.

pub(crate) mod oauth;

use std::fmt;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use tokio_util::task::TaskTracker;
use tracing::warn;

use self::oauth::{ClaudeCodeKeychain, Login, LoginError, SessionLogin};
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
    pub(crate) fn header_name(self) -> HeaderName {
        match self {
            Self::ApiKey => HeaderName::from_static("x-api-key"),
            Self::AuthToken | Self::OAuth => AUTHORIZATION,
        }
    }
}

// ── AuthSource ──

/// Where the active credential came from, listed in resolution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, derive(strum::VariantArray))]
pub enum AuthSource {
    AuthTokenEnv,
    ApiKeyEnv,
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
    pub const fn kind(self) -> AuthKind {
        match self {
            Self::AuthTokenEnv | Self::ClientBearer => AuthKind::AuthToken,
            Self::ApiKeyEnv | Self::ClientApiKey => AuthKind::ApiKey,
            Self::OAuthTokenEnv | Self::ClientOAuth | Self::OxLogin | Self::ClaudeCodeLogin => {
                AuthKind::OAuth
            }
        }
    }

    /// Returns whether the credential is a stored login, which `/login` and `/logout` can replace.
    pub(crate) const fn is_stored_login(self) -> bool {
        matches!(self, Self::OxLogin | Self::ClaudeCodeLogin)
    }

    /// The env var the credential was read from, `None` for the config file or a stored login.
    pub(crate) const fn env_var(self) -> Option<&'static str> {
        match self {
            Self::AuthTokenEnv => Some("ANTHROPIC_AUTH_TOKEN"),
            Self::ApiKeyEnv => Some("ANTHROPIC_API_KEY"),
            Self::OAuthTokenEnv => Some("CLAUDE_CODE_OAUTH_TOKEN"),
            Self::ClientBearer
            | Self::ClientApiKey
            | Self::ClientOAuth
            | Self::OxLogin
            | Self::ClaudeCodeLogin => None,
        }
    }

    /// The env var, config key, or store the credential was read from.
    fn origin(self) -> &'static str {
        match self {
            Self::AuthTokenEnv => "ANTHROPIC_AUTH_TOKEN",
            Self::ApiKeyEnv => "ANTHROPIC_API_KEY",
            Self::OAuthTokenEnv => "CLAUDE_CODE_OAUTH_TOKEN",
            Self::ClientBearer | Self::ClientApiKey | Self::ClientOAuth => "client.auth",
            Self::OxLogin => Login::Ox.name(),
            Self::ClaudeCodeLogin => Login::ClaudeCode.name(),
        }
    }

    /// Recovery step shown when the API rejects the credential with HTTP 401.
    pub(crate) const fn rejection_hint(self) -> &'static str {
        match self {
            Self::AuthTokenEnv => "Check ANTHROPIC_AUTH_TOKEN.",
            Self::ApiKeyEnv => "Check ANTHROPIC_API_KEY.",
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
    Fixed(Credential),
    Stored(Arc<SessionLogin>),
}

impl Auth {
    /// A credential sent unchanged for the whole session, as env vars and the config file supply.
    /// Fails when `secret` cannot travel in an HTTP header.
    pub(crate) fn new(source: AuthSource, secret: impl Into<String>) -> Result<Self> {
        Ok(Self {
            source,
            secret: Secret::Fixed(Credential::new(source, secret.into())?),
        })
    }

    /// [`Self::new`] for a fixture secret that is known to be header-safe.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn test(source: AuthSource, secret: &str) -> Self {
        Self::new(source, secret).expect("test secret is a header-safe value")
    }

    fn stored(login: SessionLogin) -> Self {
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
            Secret::Fixed(credential) => credential.secret.clone(),
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
        match &self.secret {
            Secret::Fixed(credential) => Ok(credential.clone()),
            Secret::Stored(login) => {
                let secret = login.access_token(trust, reloads).await?;
                Credential::new(self.source, secret)
            }
        }
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
#[derive(Clone)]
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

// ── Stored Logins ──

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
    pub(crate) fn new(e: &LoginError) -> Self {
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

/// Loads the stored login to fall back on, trying oxide-code's own before Claude Code's. A login
/// that exists but fails ends the search, so a broken oxide-code login never quietly yields to
/// another account.
pub(crate) async fn stored_login(
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

    use strum::VariantArray as _;
    use tracing::instrument::WithSubscriber as _;

    use super::*;
    use crate::auth::oauth::testing::{ox_login_in, write_creds};
    use crate::util::log::testing::LogCapture;
    use crate::util::path::testing::HOME_VAR;
    use crate::util::tls::local_testing::no_roots;
    use crate::util::tls::testing::NoRoots;

    #[test]
    fn debug_omits_secret() {
        for source in AuthSource::VARIANTS {
            let debug = format!("{:?}", Auth::test(*source, "sk-must-not-leak"));

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
                    [false, false, false, false, false, false, true, true],
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
                let credential = Auth::test(source, "secret-value")
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
                Auth::test(AuthSource::OAuthTokenEnv, "env-token"),
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
