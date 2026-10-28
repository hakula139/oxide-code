//! Request-shape settings for LLM gateways, from user configuration and Claude Code env vars.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize as _;
use serde::de::{self, Deserializer};

use super::merge::Origins;
use super::provenance::{Source, Sourced};
use super::schema::ClientTable;
use super::{env_bool, env_string, resolve_flag};
use crate::auth::AuthSource;

/// The env var holding extra request headers, which gateways also use to carry their keys.
pub(super) const CUSTOM_HEADERS_VAR: &str = "ANTHROPIC_CUSTOM_HEADERS";

// ── GatewayCompat ──

/// How requests adapt to a gateway between ox and the Anthropic API.
#[derive(Debug, Clone)]
pub(crate) struct GatewayCompat {
    /// Sent on every API request after the built-in headers.
    pub(crate) custom_headers: HeaderMap,
    /// Whether requests carry the `x-anthropic-billing-header` attribution block.
    pub(crate) attribution: bool,
    /// Whether one-shots may request `output_config.format`. Off for gateways whose upstream
    /// rejects or mistranslates it.
    pub(crate) structured_outputs: bool,
}

/// [`GatewayCompat`] with the source of each setting.
#[derive(Debug)]
pub(super) struct ResolvedGateway {
    pub(super) custom_headers: Sourced<HeaderMap>,
    pub(super) attribution: Sourced<bool>,
    pub(super) structured_outputs: Sourced<bool>,
}

impl ResolvedGateway {
    /// Rejects a custom header that would replace the header carrying the credential. `auth` is
    /// `None` when no explicit credential is set, so only a stored login or `/login` could send it.
    pub(super) fn check_credential_header(&self, auth: Option<AuthSource>) -> Result<()> {
        let setting = match self.custom_headers.source {
            Source::Env(_) => CUSTOM_HEADERS_VAR,
            _ => "client.headers",
        };
        let name = auth.map_or(AUTHORIZATION, |auth| auth.kind().header_name());
        if !self.custom_headers.value.contains_key(&name) {
            return Ok(());
        }
        let holder = auth.map_or_else(
            || "a stored login".to_owned(),
            |auth| format!("the {auth} credential"),
        );
        bail!("invalid {setting}: {name} carries {holder} and cannot be overridden");
    }
}

impl GatewayCompat {
    /// Resolves the gateway settings from the environment first and the client table second.
    pub(super) fn resolve(client: &ClientTable, origins: &Origins) -> Result<ResolvedGateway> {
        let custom_headers = match env_string(CUSTOM_HEADERS_VAR) {
            Some(raw) => Sourced::new(
                parse_headers(&raw.value).context("invalid ANTHROPIC_CUSTOM_HEADERS")?,
                raw.source,
            ),
            None => origins
                .sourced("client.headers", client.headers.clone())
                .unwrap_or_else(|| Sourced::fallback(HeaderMap::new())),
        };
        let attribution = resolve_flag(
            "CLAUDE_CODE_ATTRIBUTION_HEADER",
            origins.sourced("client.attribution", client.attribution),
            true,
        )?;
        let structured_outputs = match env_bool("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS")? {
            Some(disabled) => disabled.map(|disabled| !disabled),
            None => origins
                .sourced("client.structured_outputs", client.structured_outputs)
                .unwrap_or_else(|| Sourced::fallback(true)),
        };
        Ok(ResolvedGateway {
            custom_headers,
            attribution,
            structured_outputs,
        })
    }

    pub(crate) fn snapshot(&self) -> GatewaySnapshot {
        GatewaySnapshot {
            custom_header_names: self
                .custom_headers
                .keys()
                .map(|name| name.as_str().to_owned())
                .collect(),
            attribution: self.attribution,
            structured_outputs: self.structured_outputs,
        }
    }
}

/// Deserializes `[client.headers]` into a validated map, so an invalid name, value, or duplicate
/// fails at load.
pub(super) fn file_headers<'de, D>(deserializer: D) -> Result<Option<HeaderMap>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
    let mut headers = HeaderMap::new();
    for (name, value) in raw {
        insert_header(&mut headers, &name, &value)
            .map_err(|e| de::Error::custom(format!("invalid client.headers: {e:#}")))?;
    }
    Ok(Some(headers))
}

// ── GatewaySnapshot ──

/// [`GatewayCompat`] for display: header names only, since values often carry gateway secrets.
#[derive(Debug, Clone)]
pub struct GatewaySnapshot {
    pub custom_header_names: Vec<String>,
    pub attribution: bool,
    pub structured_outputs: bool,
}

// ── Custom Headers ──

/// `authorization` and `x-api-key` are checked against the selected credential so a gateway can
/// use the other header for its own credential.
const RESERVED: &[&str] = &[
    "anthropic-beta",
    "anthropic-version",
    "content-type",
    "x-claude-code-session-id",
];

/// Adds one header, returning an error that names it but never its value. The caller adds where
/// the header came from.
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name =
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| anyhow!("invalid header name"))?;
    ensure!(
        !RESERVED.contains(&name.as_str()),
        "{name} is set by ox and cannot be overridden"
    );
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| anyhow!("the {name} value contains a character a header cannot carry"))?;
    value.set_sensitive(true);
    ensure!(
        headers.insert(name.clone(), value).is_none(),
        "{name} is set more than once"
    );
    Ok(())
}

/// Parses `ANTHROPIC_CUSTOM_HEADERS`: newline-separated `Name: value` lines, skipping blank ones.
/// Errors name the 1-based line and never echo a value, since custom headers often carry gateway
/// secrets. Values are marked sensitive so `Debug` redacts them too.
pub(super) fn parse_headers(raw: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (index, line) in raw.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }

        let Some((name, value)) = line.split_once(':') else {
            bail!("line {line_no}: expected `Name: value`");
        };
        insert_header(&mut headers, name.trim(), value.trim())
            .map_err(|e| anyhow!("line {line_no}: {e}"))?;
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;
    use crate::config::merge::merge;
    use crate::config::provenance::Layer;
    use crate::config::schema::FileConfig;

    fn layer() -> Layer {
        Layer::User(PathBuf::from("/ox/config.toml"))
    }

    fn pairs(headers: &HeaderMap) -> Vec<(&str, &str)> {
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.to_str().unwrap()))
            .collect()
    }

    const ENV_KEYS: [&str; 3] = [
        "ANTHROPIC_CUSTOM_HEADERS",
        "CLAUDE_CODE_ATTRIBUTION_HEADER",
        "CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS",
    ];

    /// Resolves the gateway from a single config file `body`, merged as the loader does, with the
    /// gateway variables set to `vars` alone.
    fn resolve_with(body: &str, vars: &[(&str, &str)]) -> Result<ResolvedGateway> {
        let (merged, origins) = merge(vec![(layer(), body.parse().unwrap())]);
        let config: FileConfig = merged.try_into().unwrap();
        let vars = ENV_KEYS.map(|key| {
            let value = vars.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
            (key, value)
        });
        temp_env::with_vars(vars, || GatewayCompat::resolve(&config.client, &origins))
    }

    mod resolve {
        use super::*;

        #[test]
        fn file_values_supply_defaults_and_environment_replaces_each_setting() {
            let file = Source::File(layer());
            let env = Source::Env;
            for (body, vars, expected_headers, attribution, structured_outputs, sources) in [
                (
                    "",
                    vec![],
                    vec![],
                    true,
                    true,
                    [Source::Default, Source::Default, Source::Default],
                ),
                (
                    indoc! {"
                        client.headers = { x-file = 'private-header-sentinel' }
                        client.attribution = false
                        client.structured_outputs = false
                    "},
                    vec![],
                    vec![("x-file", "private-header-sentinel")],
                    false,
                    false,
                    [file.clone(), file.clone(), file.clone()],
                ),
                (
                    indoc! {"
                        client.headers = { x-file = 'private-header-sentinel' }
                        client.attribution = false
                        client.structured_outputs = false
                    "},
                    vec![
                        (CUSTOM_HEADERS_VAR, "x-env: env"),
                        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "1"),
                        ("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS", "0"),
                    ],
                    vec![("x-env", "env")],
                    true,
                    true,
                    [
                        env(CUSTOM_HEADERS_VAR),
                        env("CLAUDE_CODE_ATTRIBUTION_HEADER"),
                        env("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS"),
                    ],
                ),
                (
                    indoc! {"
                        client.headers = {}
                        client.attribution = true
                        client.structured_outputs = true
                    "},
                    vec![
                        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
                        ("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS", "1"),
                    ],
                    vec![],
                    false,
                    false,
                    [
                        file.clone(),
                        env("CLAUDE_CODE_ATTRIBUTION_HEADER"),
                        env("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS"),
                    ],
                ),
                (
                    "client.headers = { x-file = 'private-header-sentinel' }",
                    vec![(CUSTOM_HEADERS_VAR, " \n ")],
                    vec![],
                    true,
                    true,
                    [env(CUSTOM_HEADERS_VAR), Source::Default, Source::Default],
                ),
            ] {
                let gateway = resolve_with(body, &vars).unwrap();

                assert_eq!(
                    pairs(&gateway.custom_headers.value),
                    expected_headers,
                    "{body}"
                );
                assert_eq!(gateway.attribution.value, attribution, "{body}");
                assert_eq!(
                    gateway.structured_outputs.value, structured_outputs,
                    "{body}"
                );
                assert!(!format!("{gateway:?}").contains("private-header-sentinel"));
                assert_eq!(
                    [
                        gateway.custom_headers.source,
                        gateway.attribution.source,
                        gateway.structured_outputs.source,
                    ],
                    sources,
                    "{body}"
                );
            }
        }

        #[test]
        fn malformed_headers_name_the_variable_and_line() {
            let err = resolve_with(
                "",
                &[(
                    "ANTHROPIC_CUSTOM_HEADERS",
                    "x-team: platform\nmissing colon",
                )],
            )
            .expect_err("malformed header must fail startup");
            assert_eq!(
                format!("{err:#}"),
                "invalid ANTHROPIC_CUSTOM_HEADERS: line 2: expected `Name: value`",
            );
        }

        #[test]
        fn unrecognized_toggle_names_the_variable() {
            for key in [
                "CLAUDE_CODE_ATTRIBUTION_HEADER",
                "CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS",
            ] {
                let err = resolve_with("", &[(key, "maybe")]).expect_err(key);
                assert!(
                    err.to_string()
                        .starts_with(&format!("invalid {key} \"maybe\"")),
                    "{err}",
                );
            }
        }
    }

    mod check_credential_header {
        use super::*;

        #[test]
        fn rejects_only_the_active_credential_header() {
            let gateway = resolve_with(
                "",
                &[(
                    "ANTHROPIC_CUSTOM_HEADERS",
                    "Authorization: Bearer gateway-jwt",
                )],
            )
            .unwrap();
            gateway
                .check_credential_header(Some(AuthSource::ApiKeyEnv))
                .expect("x-api-key carries the credential, so a custom Authorization is free");

            let err = gateway
                .check_credential_header(Some(AuthSource::OAuthTokenEnv))
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "invalid ANTHROPIC_CUSTOM_HEADERS: \
                 authorization carries the OAuth (CLAUDE_CODE_OAUTH_TOKEN) credential \
                 and cannot be overridden",
            );
            assert!(!err.to_string().contains("gateway-jwt"));
        }

        #[test]
        fn names_the_file_setting_for_a_file_header() {
            let gateway =
                resolve_with("client.headers = { x-api-key = 'gateway-key' }", &[]).unwrap();

            let err = gateway
                .check_credential_header(Some(AuthSource::ApiKeyEnv))
                .unwrap_err();

            assert_eq!(
                err.to_string(),
                "invalid client.headers: \
                 x-api-key carries the API key (ANTHROPIC_API_KEY) credential \
                 and cannot be overridden",
            );
        }

        #[test]
        fn names_a_stored_login_when_no_explicit_credential_is_set() {
            let gateway = resolve_with(
                "",
                &[(CUSTOM_HEADERS_VAR, "Authorization: Bearer gateway-jwt")],
            )
            .unwrap();

            let err = gateway.check_credential_header(None).unwrap_err();

            assert_eq!(
                err.to_string(),
                "invalid ANTHROPIC_CUSTOM_HEADERS: \
                 authorization carries a stored login and cannot be overridden",
            );
        }
    }

    mod parse_headers {
        use super::*;

        #[test]
        fn accepts_well_formed_lines() {
            for (raw, expected) in [
                ("X-Team: platform", vec![("x-team", "platform")]),
                ("x-a: 1\nx-b: 2", vec![("x-a", "1"), ("x-b", "2")]),
                ("x-a: 1\r\nx-b: 2\r\n", vec![("x-a", "1"), ("x-b", "2")]),
                ("\n  \nx-a: 1\n\n", vec![("x-a", "1")]),
                (
                    "x-upstream: https://example.com:8443/v1",
                    vec![("x-upstream", "https://example.com:8443/v1")],
                ),
                (
                    "  x-pad  :   spaced value  ",
                    vec![("x-pad", "spaced value")],
                ),
                ("x-empty:", vec![("x-empty", "")]),
                ("user-agent: custom/1.0", vec![("user-agent", "custom/1.0")]),
            ] {
                let headers = parse_headers(raw).unwrap_or_else(|e| panic!("{raw:?}: {e:#}"));
                assert_eq!(pairs(&headers), expected, "{raw:?}");
            }
        }

        #[test]
        fn marks_values_sensitive() {
            let headers = parse_headers("x-gateway-key: secret-value").unwrap();
            assert!(headers["x-gateway-key"].is_sensitive());
            assert!(!format!("{headers:?}").contains("secret-value"));
        }

        #[test]
        fn rejects_malformed_lines_with_line_number() {
            for (raw, expected) in [
                ("x-a: 1\nno colon here", "line 2: expected `Name: value`"),
                (": value", "line 1: invalid header name"),
                ("X-Upstream-Auth=user:pass", "line 1: invalid header name"),
                (
                    "x-a: 1\n\nx-key: bad\u{7}secret",
                    "line 3: the x-key value contains a character a header cannot carry",
                ),
                ("x-a: 1\nX-A: 2", "line 2: x-a is set more than once"),
            ] {
                let err = parse_headers(raw).expect_err(raw);
                assert_eq!(err.to_string(), expected, "{raw:?}");
            }
        }

        #[test]
        fn rejects_reserved_names_case_insensitively() {
            for (line, name) in [
                ("ANTHROPIC-BETA: x", "anthropic-beta"),
                ("Anthropic-Version: x", "anthropic-version"),
                ("Content-Type: x", "content-type"),
                ("X-Claude-Code-Session-Id: x", "x-claude-code-session-id"),
            ] {
                let err = parse_headers(line).expect_err(line);
                assert_eq!(
                    err.to_string(),
                    format!("line 1: {name} is set by ox and cannot be overridden"),
                );
            }
        }
    }
}
