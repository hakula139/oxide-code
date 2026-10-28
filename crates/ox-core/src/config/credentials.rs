//! Credential resolution: which explicit credential wins, the `[client.auth]` file a session reads
//! at startup, and the fallback to a stored login.

use std::process::Command;

use anyhow::{Context as _, Result, ensure};

use super::BaseUrl;
use super::gateway::CUSTOM_HEADERS_VAR;
use super::load::user_config_hint;
use super::schema::{AuthValue, ClientAuth};
use crate::auth::oauth::{LoginError, login_stores};
use crate::auth::{Auth, AuthSource, LoginUnavailable};
use crate::util::env;

// ── AuthState ──

/// Where a config stands in loading its credential.
#[derive(Debug, Clone)]
pub(crate) enum AuthState {
    /// Waiting for [`Config::resolve_auth`](crate::config::Config::resolve_auth) to read the
    /// selected credential file, or to fall back to a stored login when there is none.
    Unresolved(Option<PendingAuth>),
    Ready(Auth),
    /// No stored login could be loaded, which only a session that can run `/login` starts in.
    SignedOut(LoginUnavailable),
}

impl AuthState {
    /// Where the credential came from, `None` until one is loaded.
    pub(super) fn source(&self) -> Option<AuthSource> {
        match self {
            Self::Ready(auth) => Some(auth.source()),
            Self::Unresolved(_) | Self::SignedOut(_) => None,
        }
    }
}

/// Selected file credential, whose contents are read once at startup after declaration checks.
#[derive(Debug, Clone)]
pub(crate) struct PendingAuth {
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
        Auth::new(self.source, secret)
            .with_context(|| format!("invalid client.auth.file at {}", self.path.display()))
    }
}

// ── Explicit Credentials ──

/// One slot in [`EXPLICIT`].
#[derive(Debug, Clone, Copy)]
enum Explicit {
    /// A credential environment variable.
    Env(AuthSource),
    /// The `[client.auth]` credential, whichever type it selects.
    Config,
}

/// The explicit credential slots in the order they win over each other and over the stored
/// logins: the environment variables around the one config slot.
const EXPLICIT: [Explicit; 4] = [
    Explicit::Env(AuthSource::AuthTokenEnv),
    Explicit::Env(AuthSource::ApiKeyEnv),
    Explicit::Config,
    Explicit::Env(AuthSource::OAuthTokenEnv),
];

pub(super) fn configured_auth(auth: ClientAuth) -> Result<Option<ExplicitAuth>> {
    let (source, value) = match auth {
        ClientAuth::Bearer(value) => (AuthSource::ClientBearer, value),
        ClientAuth::ApiKey(value) => (AuthSource::ClientApiKey, value),
        ClientAuth::Oauth(value) => (AuthSource::ClientOAuth, value),
        ClientAuth::Login(_) => return Ok(None),
    };
    match value {
        AuthValue::Value(secret) => Ok(Some(ExplicitAuth::Inline(Auth::new(source, secret)?))),
        AuthValue::File(path) => Ok(Some(ExplicitAuth::File(PendingAuth { source, path }))),
    }
}

/// An explicit credential: one ready to send, or a `[client.auth]` file whose secret loads in
/// [`Config::resolve_auth`](crate::config::Config::resolve_auth).
#[derive(Debug)]
pub(super) enum ExplicitAuth {
    Inline(Auth),
    File(PendingAuth),
}

impl ExplicitAuth {
    pub(super) fn source(&self) -> AuthSource {
        match self {
            Self::Inline(auth) => auth.source(),
            Self::File(pending) => pending.source,
        }
    }
}

/// The first explicit credential in [`EXPLICIT`] order. `config` occupies the config slot, where a
/// file credential wins over later env sources even though its secret loads during
/// [`Config::resolve_auth`](crate::config::Config::resolve_auth). Fails when the winning secret
/// cannot travel in a header.
pub(super) fn first_explicit(config: Option<ExplicitAuth>) -> Result<Option<ExplicitAuth>> {
    for slot in EXPLICIT {
        match slot {
            Explicit::Env(source) => {
                if let Some(secret) = source.env_var().and_then(env::string) {
                    return Auth::new(source, secret).map(|auth| Some(ExplicitAuth::Inline(auth)));
                }
            }
            Explicit::Config => {
                if config.is_some() {
                    return Ok(config);
                }
            }
        }
    }
    Ok(None)
}

// ── Stored Login Fallback ──

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
    fn gateway(base_url: &BaseUrl) -> Self {
        let message = format!(
            "no credentials for {base_url}. \
             Set ANTHROPIC_AUTH_TOKEN, ANTHROPIC_API_KEY, or CLAUDE_CODE_OAUTH_TOKEN, \
             or add a credential under `[client.auth]` in {config}. \
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
                 add a credential under `[client.auth]` in {config}, \
                 or sign in with `/login` in the `ox` TUI.",
                stores = login_stores(),
                config = user_config_hint(),
            ),
            cause => cause.to_string(),
        };
        Self::SignedOut { cause, message }
    }
}

/// The stored login a session falls back on without an explicit credential. `stored` is awaited
/// only when `base_url` is first-party, so a personal subscription token never reaches a gateway
/// the user did not hand it to.
pub(super) async fn fallback(
    base_url: &BaseUrl,
    stored: impl Future<Output = Result<Auth, LoginError>>,
) -> Result<Auth, CredentialError> {
    if !base_url.is_first_party() {
        return Err(CredentialError::gateway(base_url));
    }
    stored.await.map_err(CredentialError::signed_out)
}

// ── Child Processes ──

/// The env vars that can carry a secret ox authenticates with: each credential variable, and
/// [`CUSTOM_HEADERS_VAR`], which gateways use for their keys.
pub(crate) fn secret_env_vars() -> impl Iterator<Item = &'static str> {
    EXPLICIT
        .into_iter()
        .filter_map(|slot| match slot {
            Explicit::Env(source) => source.env_var(),
            Explicit::Config => None,
        })
        .chain([CUSTOM_HEADERS_VAR])
}

/// Keeps [`secret_env_vars`] out of the environment `cmd` starts its process with.
pub(crate) fn withhold_secrets(cmd: &mut Command) {
    for var in secret_env_vars() {
        cmd.env_remove(var);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::auth::oauth::Login;
    use crate::config::testing::base_url;
    use crate::util::path::local_testing::abs;

    mod first_explicit {
        use super::*;

        const VARS: [&str; 3] = [
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ];

        fn resolve(
            set: &[(&'static str, &'static str)],
            config: Option<ExplicitAuth>,
        ) -> Result<Option<ExplicitAuth>> {
            let vars = VARS.map(|var| {
                let value = set.iter().find(|(k, _)| *k == var).map(|(_, v)| *v);
                (var, value)
            });
            temp_env::with_vars(vars, || first_explicit(config))
        }

        fn inline() -> ExplicitAuth {
            ExplicitAuth::Inline(Auth::test(AuthSource::ClientApiKey, "config-secret"))
        }

        fn pending() -> ExplicitAuth {
            ExplicitAuth::File(PendingAuth {
                source: AuthSource::ClientApiKey,
                path: PathBuf::from("/token"),
            })
        }

        #[test]
        fn a_higher_env_source_beats_the_config_slot() {
            for (var, winner) in [
                ("ANTHROPIC_AUTH_TOKEN", AuthSource::AuthTokenEnv),
                ("ANTHROPIC_API_KEY", AuthSource::ApiKeyEnv),
            ] {
                for config in [Some(inline()), Some(pending())] {
                    let picked = resolve(&[(var, "env-secret")], config).unwrap().unwrap();

                    assert!(
                        matches!(&picked, ExplicitAuth::Inline(auth)
                            if auth.source() == winner && auth.secret() == "env-secret"),
                        "{var}: {picked:?}",
                    );
                }
            }
        }

        #[test]
        fn the_config_slot_wins_over_the_oauth_token_variable() {
            let picked = resolve(&[("CLAUDE_CODE_OAUTH_TOKEN", "oauth")], Some(inline()))
                .unwrap()
                .unwrap();
            assert!(
                matches!(&picked, ExplicitAuth::Inline(auth) if auth.source() == AuthSource::ClientApiKey),
                "{picked:?}",
            );

            let oauth_set = [("CLAUDE_CODE_OAUTH_TOKEN", "oauth")];
            let picked = resolve(&oauth_set, Some(pending())).unwrap().unwrap();
            assert!(
                matches!(picked, ExplicitAuth::File(_)),
                "a file credential stops the search: {picked:?}",
            );
        }

        #[test]
        fn finds_none_when_nothing_is_set() {
            assert!(resolve(&[], None).unwrap().is_none());
        }

        #[test]
        fn rejects_a_winning_secret_no_header_can_carry() {
            for (var, config, expected) in [
                (
                    "ANTHROPIC_API_KEY",
                    Some(inline()),
                    "API key (ANTHROPIC_API_KEY) is not a valid HTTP header value",
                ),
                (
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    None,
                    "OAuth (CLAUDE_CODE_OAUTH_TOKEN) is not a valid HTTP header value",
                ),
            ] {
                let err = resolve(&[(var, "bad\nkey")], config).unwrap_err();

                assert_eq!(err.to_string(), expected, "{var}");
            }
        }
    }

    mod fallback {
        use super::*;

        const FIRST_PARTY: &str = "https://api.anthropic.com";
        const GATEWAY: &str = "https://gateway.example.com/anthropic";

        fn unreachable_store() -> impl Future<Output = Result<Auth, LoginError>> {
            std::future::poll_fn(|_| panic!("stored credentials must not be read"))
        }

        fn stored_ox_login() -> std::future::Ready<Result<Auth, LoginError>> {
            std::future::ready(Ok(Auth::test(AuthSource::OxLogin, "stored-token")))
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
                     or add a credential under `[client.auth]` in {file}. \
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
                     add a credential under `[client.auth]` in {config_file}, \
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

    mod withhold_secrets {
        use super::*;

        #[test]
        fn removes_each_secret_env_var() {
            let mut cmd = Command::new("true");

            withhold_secrets(&mut cmd);

            let removed: BTreeMap<_, _> = cmd.get_envs().collect();
            let secrets = secret_env_vars().map(|var| (OsStr::new(var), None));
            assert_eq!(removed, secrets.collect());
        }
    }
}
