//! Credential resolution: which secret authenticates requests, where it came from, and how it
//! travels on the wire.

use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio_util::task::TaskTracker;
use tracing::warn;

use super::oauth::{ClaudeCodeKeychain, LOGIN_STORES, Login, LoginError, SessionLogin};
use crate::util::env;
use crate::util::tls::Trust;

// ── AuthKind ──

/// How a credential authenticates on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthKind {
    /// `x-api-key` header.
    ApiKey,
    /// `Authorization: Bearer` with the API-key request shape, as LLM gateways expect.
    AuthToken,
    /// `Authorization: Bearer` plus the OAuth beta that Claude subscription tokens require.
    OAuth,
}

impl AuthKind {
    pub(crate) const fn label(self) -> &'static str {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum AuthSource {
    AuthTokenEnv,
    ApiKeyEnv,
    ApiKeyFile,
    OAuthTokenEnv,
    /// oxide-code's stored login from `/login` (`$XDG_DATA_HOME/ox/credentials.json`), refreshed
    /// before a request near its expiry and reloaded after a 401.
    OxLogin,
    /// Claude Code's stored login (macOS Keychain or `~/.claude/.credentials.json`), refreshed
    /// before a request near its expiry and reloaded after a 401.
    ClaudeCodeLogin,
}

impl AuthSource {
    pub(crate) const fn kind(self) -> AuthKind {
        match self {
            Self::AuthTokenEnv => AuthKind::AuthToken,
            Self::ApiKeyEnv | Self::ApiKeyFile => AuthKind::ApiKey,
            Self::OAuthTokenEnv | Self::OxLogin | Self::ClaudeCodeLogin => AuthKind::OAuth,
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
            Self::AuthTokenEnv | Self::ApiKeyEnv | Self::ApiKeyFile | Self::OAuthTokenEnv => None,
        }
    }

    /// The env var the credential was read from, `None` for the config file or a stored login.
    pub(super) const fn env_var(self) -> Option<&'static str> {
        match self {
            Self::AuthTokenEnv => Some("ANTHROPIC_AUTH_TOKEN"),
            Self::ApiKeyEnv => Some("ANTHROPIC_API_KEY"),
            Self::OAuthTokenEnv => Some("CLAUDE_CODE_OAUTH_TOKEN"),
            Self::ApiKeyFile | Self::OxLogin | Self::ClaudeCodeLogin => None,
        }
    }

    /// The env var, config key, or store the credential was read from.
    fn origin(self) -> &'static str {
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
                warn!("failed to reload the stored login after HTTP 401: {e:#}");
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

/// Whether `error`, from [`Auth::credential`] or [`Auth::reload`], is a stored login's refresh
/// that a later attempt may complete.
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

/// Why no credential resolved.
#[derive(Debug, thiserror::Error)]
pub(super) enum ResolveError {
    /// `base_url` is a gateway, which never receives a stored login.
    #[error(
        "no credentials for {0}. \
         Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
         or add `api_key` to ~/.config/ox/config.toml. \
         Stored logins are sent only to the Anthropic API."
    )]
    Gateway(String),
    /// No stored login supplied a token. A TUI session starts signed out, since `/login` can sign
    /// it in.
    #[error("{}", signed_out_error(.0))]
    SignedOut(LoginError),
}

fn signed_out_error(e: &LoginError) -> String {
    match e {
        LoginError::Missing => format!(
            "no credentials found: no API key or token is set, no oxide-code login is stored, \
             and no Claude Code login is in {LOGIN_STORES}. \
             Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, \
             add `api_key` to ~/.config/ox/config.toml, or sign in with `/login` in the `ox` TUI."
        ),
        e => e.to_string(),
    }
}

/// What a session shows on a request while signed out, and at startup when no login is stored.
pub(crate) const SIGNED_OUT: &str = "Not signed in. Run `/login` to sign in.";

/// The startup notice for a TUI session that starts signed out because of `e`.
pub(super) fn signed_out_notice(e: &LoginError) -> String {
    match e {
        LoginError::Missing => SIGNED_OUT.to_owned(),
        e => format!("Not signed in: {e}"),
    }
}

/// Explicit credentials in precedence order: env and config values, before the stored logins.
pub(super) fn explicit_credentials(
    file_api_key: Option<String>,
) -> [(AuthSource, Option<String>); 4] {
    let from_env = |source: AuthSource| (source, env::string(source.origin()));
    [
        from_env(AuthSource::AuthTokenEnv),
        from_env(AuthSource::ApiKeyEnv),
        (AuthSource::ApiKeyFile, file_api_key),
        from_env(AuthSource::OAuthTokenEnv),
    ]
}

/// Picks the first explicit credential, falling back to a stored login. `stored` is awaited only
/// on fallback, and only when `base_url` is first-party, so a personal subscription token never
/// reaches a gateway the user did not hand it to.
pub(super) async fn resolve(
    explicit: impl IntoIterator<Item = (AuthSource, Option<String>)>,
    base_url: &str,
    stored: impl Future<Output = Result<Auth, LoginError>>,
) -> Result<Auth, ResolveError> {
    if let Some((source, secret)) = explicit
        .into_iter()
        .find_map(|(source, value)| value.map(|secret| (source, secret)))
    {
        return Ok(Auth::new(source, secret));
    }

    if !super::is_first_party_base_url(base_url) {
        return Err(ResolveError::Gateway(base_url.to_owned()));
    }
    stored.await.map_err(ResolveError::SignedOut)
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
    use super::*;
    use crate::config::oauth::testing::{ox_login_in, write_creds};
    use crate::util::tls::testing::{NoRoots, no_roots};

    const ALL_SOURCES: [AuthSource; 6] = [
        AuthSource::AuthTokenEnv,
        AuthSource::ApiKeyEnv,
        AuthSource::ApiKeyFile,
        AuthSource::OAuthTokenEnv,
        AuthSource::OxLogin,
        AuthSource::ClaudeCodeLogin,
    ];

    fn credentials_json(access_token: &str) -> String {
        serde_json::json!({
            "claudeAiOauth": { "accessToken": access_token, "expiresAt": 9_999_999_999_999_u64 },
        })
        .to_string()
    }

    #[test]
    fn debug_omits_secret() {
        let debug = format!("{:?}", Auth::new(AuthSource::ApiKeyEnv, "sk-must-not-leak"));
        assert!(!debug.contains("sk-must-not-leak"), "{debug}");
        assert!(debug.contains("ApiKeyEnv"), "{debug}");
    }

    mod auth_source {
        use super::*;

        #[test]
        fn display_names_kind_and_origin_for_each_source() {
            let labels = ALL_SOURCES.map(|source| source.to_string());
            assert_eq!(
                labels,
                [
                    "Auth token (ANTHROPIC_AUTH_TOKEN)",
                    "API key (ANTHROPIC_API_KEY)",
                    "API key (client.api_key)",
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
                let kinds = ALL_SOURCES.map(AuthSource::kind);
                assert_eq!(
                    kinds,
                    [
                        AuthKind::AuthToken,
                        AuthKind::ApiKey,
                        AuthKind::ApiKey,
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
                    ALL_SOURCES.map(AuthSource::is_stored_login),
                    [false, false, false, false, true, true],
                );
            }
        }

        mod env_var {
            use super::*;

            #[test]
            fn names_only_environment_sources() {
                assert_eq!(
                    ALL_SOURCES.map(AuthSource::env_var),
                    [
                        Some("ANTHROPIC_AUTH_TOKEN"),
                        Some("ANTHROPIC_API_KEY"),
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
                for (source, expected) in [
                    (AuthSource::AuthTokenEnv, "ANTHROPIC_AUTH_TOKEN"),
                    (AuthSource::ApiKeyEnv, "ANTHROPIC_API_KEY"),
                    (AuthSource::ApiKeyFile, "client.api_key in your user config"),
                    (AuthSource::OAuthTokenEnv, "claude setup-token"),
                    (AuthSource::OxLogin, "Run `/login`"),
                    (AuthSource::ClaudeCodeLogin, "Run `claude`"),
                ] {
                    let hint = source.rejection_hint();
                    assert!(hint.contains(expected), "{source:?}: {hint}");
                }
                assert!(
                    AuthSource::OAuthTokenEnv
                        .rejection_hint()
                        .starts_with("CLAUDE_CODE_OAUTH_TOKEN")
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

                let reloaded = auth.reload(&rejected, &trust, &reloads).await.unwrap();

                assert!(reloaded.is_none(), "{auth:?}");
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

    mod signed_out_notice {
        use super::*;

        #[test]
        fn names_the_step_that_signs_in() {
            assert_eq!(
                signed_out_notice(&LoginError::Missing),
                "Not signed in. Run `/login` to sign in.",
            );
            assert_eq!(
                signed_out_notice(&LoginError::Expired(Login::Ox)),
                "Not signed in: oxide-code login has expired. Run `/login` to sign in again.",
            );
            assert_eq!(
                signed_out_notice(&LoginError::Unreadable(
                    Login::ClaudeCode,
                    anyhow::anyhow!("failed to parse the credentials in the macOS Keychain"),
                )),
                "Not signed in: failed to load the Claude Code login: \
                 failed to parse the credentials in the macOS Keychain. \
                 Run `claude` to sign in again.",
            );
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

    mod resolve {
        use super::*;

        const FIRST_PARTY: &str = "https://api.anthropic.com";
        const GATEWAY: &str = "https://gateway.example.com/anthropic";

        fn unreachable_store() -> impl Future<Output = Result<Auth, LoginError>> {
            std::future::poll_fn(|_| panic!("stored credentials must not be read"))
        }

        fn stored_ox_login() -> std::future::Ready<Result<Auth, LoginError>> {
            std::future::ready(Ok(Auth::new(AuthSource::OxLogin, "stored-token")))
        }

        fn explicit(present: &[AuthSource]) -> Vec<(AuthSource, Option<String>)> {
            ALL_SOURCES[..4]
                .iter()
                .map(|&source| {
                    let value = present
                        .contains(&source)
                        .then(|| format!("{source:?}-secret"));
                    (source, value)
                })
                .collect()
        }

        #[tokio::test]
        async fn picks_highest_precedence_explicit_credential() {
            // Each row drops the previous winner, so every adjacent precedence pair is exercised.
            for (present, winner) in [
                (&ALL_SOURCES[..4], AuthSource::AuthTokenEnv),
                (&ALL_SOURCES[1..4], AuthSource::ApiKeyEnv),
                (&ALL_SOURCES[2..4], AuthSource::ApiKeyFile),
                (&ALL_SOURCES[3..4], AuthSource::OAuthTokenEnv),
            ] {
                let auth = resolve(explicit(present), GATEWAY, unreachable_store())
                    .await
                    .unwrap();
                assert_eq!(auth.source(), winner);
                assert_eq!(auth.secret(), format!("{winner:?}-secret"));
            }
        }

        #[tokio::test]
        async fn falls_back_to_stored_login_on_first_party_host() {
            let auth = resolve(explicit(&[]), FIRST_PARTY, stored_ox_login())
                .await
                .unwrap();
            assert_eq!(auth.source(), AuthSource::OxLogin);
            assert_eq!(auth.secret(), "stored-token");
        }

        #[tokio::test]
        async fn refuses_stored_login_for_third_party_host_without_reading_it() {
            let err = resolve(explicit(&[]), GATEWAY, unreachable_store())
                .await
                .expect_err("stored login must not go to a gateway");
            assert!(matches!(err, ResolveError::Gateway(_)), "{err:?}");
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "no credentials for {GATEWAY}. \
                     Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
                     or add `api_key` to ~/.config/ox/config.toml. \
                     Stored logins are sent only to the Anthropic API."
                ),
            );
        }

        #[tokio::test]
        async fn missing_login_lists_every_credential_option() {
            let err = resolve(explicit(&[]), FIRST_PARTY, async {
                Err(LoginError::Missing)
            })
            .await
            .expect_err("no credential anywhere");
            assert!(
                matches!(err, ResolveError::SignedOut(LoginError::Missing)),
                "{err:?}"
            );
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "no credentials found: \
                     no API key or token is set, no oxide-code login is stored, \
                     and no Claude Code login is in {LOGIN_STORES}. \
                     Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, \
                     add `api_key` to ~/.config/ox/config.toml, \
                     or sign in with `/login` in the `ox` TUI."
                ),
            );
        }

        #[tokio::test]
        async fn other_login_failures_keep_their_own_recovery_step() {
            let err = resolve(explicit(&[]), FIRST_PARTY, async {
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
            std::fs::write(
                data.path().join("ox").join("credentials.json"),
                credentials_json("ox-token"),
            )
            .unwrap();

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
                [(crate::util::path::testing::HOME_VAR, Some(home.path()))],
                async {
                    let missing = claude_code_login(ClaudeCodeKeychain::NONE, &NoRoots).await;
                    std::fs::create_dir(file.parent().unwrap()).unwrap();
                    std::fs::write(&file, "not json").unwrap();
                    let unreadable = claude_code_login(ClaudeCodeKeychain::NONE, &NoRoots).await;
                    std::fs::write(&file, credentials_json("claude-code-token")).unwrap();
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
