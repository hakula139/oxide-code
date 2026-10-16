//! Credential resolution: which secret authenticates requests, where it came from, and how it
//! travels on the wire.

use std::fmt;

use anyhow::{Context, Result, bail};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

use super::oauth::{LOGIN_STORES, LoginError};
use crate::util::env;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthSource {
    AuthTokenEnv,
    ApiKeyEnv,
    ApiKeyFile,
    OAuthTokenEnv,
    /// Claude Code's stored login (macOS Keychain or `~/.claude/.credentials.json`), refreshed
    /// near expiry.
    ClaudeCodeLogin,
}

impl AuthSource {
    pub(crate) const fn kind(self) -> AuthKind {
        match self {
            Self::AuthTokenEnv => AuthKind::AuthToken,
            Self::ApiKeyEnv | Self::ApiKeyFile => AuthKind::ApiKey,
            Self::OAuthTokenEnv | Self::ClaudeCodeLogin => AuthKind::OAuth,
        }
    }

    /// The env var the credential was read from, `None` for the config file or stored login.
    pub(super) const fn env_var(self) -> Option<&'static str> {
        match self {
            Self::AuthTokenEnv => Some("ANTHROPIC_AUTH_TOKEN"),
            Self::ApiKeyEnv => Some("ANTHROPIC_API_KEY"),
            Self::OAuthTokenEnv => Some("CLAUDE_CODE_OAUTH_TOKEN"),
            Self::ApiKeyFile | Self::ClaudeCodeLogin => None,
        }
    }

    /// The env var, config key, or store the credential was read from.
    fn origin(self) -> &'static str {
        self.env_var().unwrap_or(match self {
            Self::ApiKeyFile => "client.api_key",
            _ => "Claude Code login",
        })
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
                "CLAUDE_CODE_OAUTH_TOKEN may be expired or revoked. Generate a new one with \
                 `claude setup-token`."
            }
            Self::ClaudeCodeLogin => "Run `claude` to sign in again.",
        }
    }
}

/// Kind and origin, as `/status` and `/config` show it.
impl fmt::Display for AuthSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.kind().label(), self.origin())
    }
}

// ── Auth ──

/// Resolved credential. `Debug` omits the secret.
#[derive(Clone)]
pub(crate) struct Auth {
    source: AuthSource,
    secret: String,
}

impl Auth {
    pub(crate) fn new(source: AuthSource, secret: impl Into<String>) -> Self {
        Self {
            source,
            secret: secret.into(),
        }
    }

    pub(crate) const fn source(&self) -> AuthSource {
        self.source
    }

    pub(crate) const fn kind(&self) -> AuthKind {
        self.source.kind()
    }

    #[cfg(test)]
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }

    /// The credential header, with its value marked sensitive.
    pub(crate) fn header(&self) -> Result<(HeaderName, HeaderValue)> {
        let kind = self.kind();
        let value = match kind {
            AuthKind::ApiKey => self.secret.clone(),
            AuthKind::AuthToken | AuthKind::OAuth => format!("Bearer {}", self.secret),
        };
        let mut value = HeaderValue::from_str(&value)
            .with_context(|| format!("{} is not a valid HTTP header value", self.source))?;
        value.set_sensitive(true);
        Ok((kind.header_name(), value))
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

// ── Resolution ──

/// Explicit credentials in precedence order: env and config values, before the stored login.
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

/// Picks the first explicit credential, falling back to the stored Claude Code login. `stored`
/// is awaited only on fallback, and only when `base_url` is first-party, so a personal
/// subscription token never reaches a gateway the user did not hand it to.
pub(super) async fn resolve(
    explicit: impl IntoIterator<Item = (AuthSource, Option<String>)>,
    base_url: &str,
    stored: impl Future<Output = Result<String, LoginError>>,
) -> Result<Auth> {
    if let Some((source, secret)) = explicit
        .into_iter()
        .find_map(|(source, value)| value.map(|secret| (source, secret)))
    {
        return Ok(Auth::new(source, secret));
    }

    if !super::is_first_party_base_url(base_url) {
        bail!(
            "no credentials for {base_url}. Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or \
             CLAUDE_CODE_OAUTH_TOKEN, or add `api_key` to ~/.config/ox/config.toml. The Claude \
             Code login is sent only to the Anthropic API."
        );
    }
    match stored.await {
        Ok(token) => Ok(Auth::new(AuthSource::ClaudeCodeLogin, token)),
        Err(LoginError::Missing) => bail!(
            "no credentials found: no API key or token is set, and no Claude Code login is in \
             {LOGIN_STORES}. Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, add `api_key` to \
             ~/.config/ox/config.toml, or run `claude` to sign in."
        ),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_SOURCES: [AuthSource; 5] = [
        AuthSource::AuthTokenEnv,
        AuthSource::ApiKeyEnv,
        AuthSource::ApiKeyFile,
        AuthSource::OAuthTokenEnv,
        AuthSource::ClaudeCodeLogin,
    ];

    const FIRST_PARTY: &str = "https://api.anthropic.com";
    const GATEWAY: &str = "https://gateway.example.com/anthropic";

    fn unreachable_store() -> impl Future<Output = Result<String, LoginError>> {
        std::future::poll_fn(|_| panic!("stored credentials must not be read"))
    }

    fn stored_token() -> std::future::Ready<Result<String, LoginError>> {
        std::future::ready(Ok("stored-token".to_owned()))
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

    // ── AuthSource::kind ──

    #[test]
    fn kind_maps_each_source_to_its_wire_scheme() {
        let kinds = ALL_SOURCES.map(AuthSource::kind);
        assert_eq!(
            kinds,
            [
                AuthKind::AuthToken,
                AuthKind::ApiKey,
                AuthKind::ApiKey,
                AuthKind::OAuth,
                AuthKind::OAuth,
            ],
        );
    }

    // ── AuthSource::env_var ──

    #[test]
    fn env_var_names_only_environment_sources() {
        assert_eq!(
            ALL_SOURCES.map(AuthSource::env_var),
            [
                Some("ANTHROPIC_AUTH_TOKEN"),
                Some("ANTHROPIC_API_KEY"),
                None,
                Some("CLAUDE_CODE_OAUTH_TOKEN"),
                None,
            ],
        );
    }

    // ── AuthSource::rejection_hint ──

    #[test]
    fn rejection_hint_names_where_to_fix_each_source() {
        for (source, expected) in [
            (AuthSource::AuthTokenEnv, "ANTHROPIC_AUTH_TOKEN"),
            (AuthSource::ApiKeyEnv, "ANTHROPIC_API_KEY"),
            (AuthSource::ApiKeyFile, "client.api_key in your user config"),
            (AuthSource::OAuthTokenEnv, "claude setup-token"),
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

    // ── AuthSource::fmt ──

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
                "OAuth (Claude Code login)",
            ],
        );
    }

    // ── Auth::header ──

    #[test]
    fn header_sends_api_keys_raw_and_tokens_as_bearer() {
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
            let (got_name, got_value) = Auth::new(source, "secret-value").header().unwrap();
            assert_eq!(got_name, name, "{source:?}");
            assert_eq!(got_value, value, "{source:?}");
            assert!(got_value.is_sensitive(), "{source:?}");
        }
    }

    // ── Auth::fmt ──

    #[test]
    fn debug_omits_secret() {
        let debug = format!("{:?}", Auth::new(AuthSource::ApiKeyEnv, "sk-must-not-leak"));
        assert!(!debug.contains("sk-must-not-leak"), "{debug}");
        assert!(debug.contains("ApiKeyEnv"), "{debug}");
    }

    // ── explicit_credentials ──

    #[test]
    fn explicit_credentials_reads_env_in_precedence_order_with_file_key_third() {
        let vars = [
            ("ANTHROPIC_AUTH_TOKEN", Some("bearer")),
            ("ANTHROPIC_API_KEY", None),
            ("CLAUDE_CODE_OAUTH_TOKEN", Some("oauth")),
        ];
        let got = temp_env::with_vars(vars, || explicit_credentials(Some("file-key".to_owned())));
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

    // ── resolve ──

    #[tokio::test]
    async fn resolve_picks_highest_precedence_explicit_credential() {
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
    async fn resolve_falls_back_to_stored_login_on_first_party_host() {
        let auth = resolve(explicit(&[]), FIRST_PARTY, stored_token())
            .await
            .unwrap();
        assert_eq!(auth.source(), AuthSource::ClaudeCodeLogin);
        assert_eq!(auth.secret(), "stored-token");
    }

    #[tokio::test]
    async fn resolve_refuses_stored_login_for_third_party_host_without_reading_it() {
        let err = resolve(explicit(&[]), GATEWAY, unreachable_store())
            .await
            .expect_err("stored login must not go to a gateway");
        let msg = format!("{err:#}");
        for needle in [
            GATEWAY,
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "api_key",
        ] {
            assert!(msg.contains(needle), "{needle}: {msg}");
        }
    }

    #[tokio::test]
    async fn resolve_missing_login_lists_every_credential_option() {
        let err = resolve(explicit(&[]), FIRST_PARTY, async {
            Err(LoginError::Missing)
        })
        .await
        .expect_err("no credential anywhere");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "no credentials found: no API key or token is set, and no Claude Code login is \
                 in {LOGIN_STORES}. Set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN, add \
                 `api_key` to ~/.config/ox/config.toml, or run `claude` to sign in."
            ),
        );
    }

    #[tokio::test]
    async fn resolve_other_login_failures_keep_their_own_recovery_step() {
        let err = resolve(explicit(&[]), FIRST_PARTY, async {
            Err(LoginError::Expired)
        })
        .await
        .expect_err("an expired login must fail");
        assert_eq!(
            format!("{err:#}"),
            "Claude Code login has expired. Run `claude` to sign in again.",
        );
    }
}
