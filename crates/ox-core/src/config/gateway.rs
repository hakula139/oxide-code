//! Request-shape settings for LLM gateways, from user configuration and Claude Code env vars.

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use super::auth::AuthSource;
use super::schema::ClientTable;
use crate::util::env;

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

impl GatewayCompat {
    pub(super) fn from_client(client: &ClientTable) -> Result<Self> {
        let custom_headers = match env::string(CUSTOM_HEADERS_VAR) {
            Some(raw) => parse_headers(&raw).context("invalid ANTHROPIC_CUSTOM_HEADERS")?,
            None => client
                .headers
                .as_ref()
                .map(file_headers)
                .transpose()?
                .unwrap_or_default(),
        };
        Ok(Self {
            custom_headers,
            attribution: env::bool("CLAUDE_CODE_ATTRIBUTION_HEADER")?
                .or(client.attribution)
                .unwrap_or(true),
            structured_outputs: env::bool("CLAUDE_CODE_DISABLE_STRUCTURED_OUTPUTS")?
                .map(|disabled| !disabled)
                .or(client.structured_outputs)
                .unwrap_or(true),
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

    /// Rejects a custom header that would replace the header carrying the active credential.
    pub(super) fn check_credential_header_from(
        &self,
        auth: AuthSource,
        setting: &str,
    ) -> Result<()> {
        let name = auth.kind().header_name();
        if self.custom_headers.contains_key(&name) {
            bail!(
                "invalid {setting}: \
                 {name} carries the {auth} credential and cannot be overridden"
            );
        }
        Ok(())
    }
}

pub(super) fn file_headers(raw: &std::collections::BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in raw {
        let name =
            HeaderName::from_bytes(name.as_bytes()).context("invalid client.headers name")?;
        ensure!(
            !RESERVED.contains(&name.as_str()),
            "invalid client.headers: {name} is set by ox and cannot be overridden"
        );
        let mut value = HeaderValue::from_str(value).context("invalid client.headers value")?;
        value.set_sensitive(true);
        ensure!(
            headers.insert(name, value).is_none(),
            "client.headers sets the same header more than once"
        );
    }
    Ok(headers)
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
        let Ok(name) = HeaderName::from_bytes(name.trim().as_bytes()) else {
            bail!("line {line_no}: invalid header name");
        };
        if RESERVED.contains(&name.as_str()) {
            bail!("line {line_no}: {name} is set by ox and cannot be overridden");
        }
        let Ok(mut value) = HeaderValue::from_str(value.trim()) else {
            bail!("line {line_no}: the {name} value contains a character a header cannot carry");
        };
        value.set_sensitive(true);
        if headers.insert(name.clone(), value).is_some() {
            bail!("line {line_no}: {name} is set more than once");
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::config::schema::FileConfig;

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

    fn load_with(vars: &[(&str, &str)]) -> Result<GatewayCompat> {
        let vars = ENV_KEYS.map(|key| {
            let value = vars.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
            (key, value)
        });
        temp_env::with_vars(vars, || GatewayCompat::from_client(&ClientTable::default()))
    }

    mod from_client {
        use super::*;

        #[test]
        fn file_values_supply_defaults_and_environment_replaces_each_setting() {
            for (body, vars, expected_headers, attribution, structured_outputs) in [
                ("", vec![], vec![], true, true),
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
                ),
                (
                    "client.headers = { x-file = 'private-header-sentinel' }",
                    vec![(CUSTOM_HEADERS_VAR, " \n ")],
                    vec![],
                    true,
                    true,
                ),
            ] {
                let config: FileConfig = toml::from_str(body).unwrap();
                let vars = ENV_KEYS.map(|key| {
                    let value = vars
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, value)| *value);
                    (key, value)
                });

                let gateway = temp_env::with_vars(vars, || {
                    GatewayCompat::from_client(&config.client).unwrap()
                });

                assert_eq!(pairs(&gateway.custom_headers), expected_headers, "{body}");
                assert_eq!(gateway.attribution, attribution, "{body}");
                assert_eq!(gateway.structured_outputs, structured_outputs, "{body}");
                assert!(!format!("{gateway:?}").contains("private-header-sentinel"));
            }
        }

        #[test]
        fn malformed_headers_name_the_variable_and_line() {
            let err = load_with(&[(
                "ANTHROPIC_CUSTOM_HEADERS",
                "x-team: platform\nmissing colon",
            )])
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
                let err = load_with(&[(key, "maybe")]).expect_err(key);
                assert!(
                    err.to_string()
                        .starts_with(&format!("invalid {key} \"maybe\"")),
                    "{err}",
                );
            }
        }
    }

    mod check_credential_header_from {
        use super::*;

        #[test]
        fn rejects_only_the_active_credential_header() {
            let compat = load_with(&[(
                "ANTHROPIC_CUSTOM_HEADERS",
                "Authorization: Bearer gateway-jwt",
            )])
            .unwrap();
            compat
                .check_credential_header_from(AuthSource::ApiKeyEnv, CUSTOM_HEADERS_VAR)
                .expect("x-api-key carries the credential, so a custom Authorization is free");

            let err = compat
                .check_credential_header_from(AuthSource::OAuthTokenEnv, CUSTOM_HEADERS_VAR)
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "invalid ANTHROPIC_CUSTOM_HEADERS: \
                 authorization carries the OAuth (CLAUDE_CODE_OAUTH_TOKEN) credential \
                 and cannot be overridden",
            );
            assert!(!err.to_string().contains("gateway-jwt"));
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
