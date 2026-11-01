//! The TOML schema every config file shares: the `[client]`, `[permission]`, and `[tui]` tables.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::{self, Unexpected};
use serde::{Deserialize, Deserializer};
use toml::Table;

use super::StatusLineSegment;
use super::compaction::ThresholdPercent;
use crate::model::{Effort, PromptCacheTtl};
use crate::permission::defaults::Shipped;
use crate::permission::{Mode, PermissionClass};
use crate::util::env::whole_number_in;
use crate::util::path::expand_absolute;

/// One config file, or several merged. Every key is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct FileConfig {
    pub(super) client: ClientTable,
    pub(super) permission: PermissionTable,
    pub(super) tui: TuiTable,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ClientTable {
    pub(super) auth: Option<ClientAuth>,
    pub(super) base_url: Option<String>,
    pub(super) extra_ca_certs: Option<String>,
    #[serde(deserialize_with = "super::gateway::file_headers")]
    pub(super) headers: Option<HeaderMap>,
    pub(super) attribution: Option<bool>,
    pub(super) structured_outputs: Option<bool>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<Effort>,
    #[serde(deserialize_with = "count")]
    pub(super) max_tokens: Option<u32>,
    #[serde(deserialize_with = "count")]
    pub(super) max_tool_rounds: Option<u32>,
    pub(super) prompt_cache_ttl: Option<PromptCacheTtl>,
    pub(super) compaction: CompactionTable,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    deny_unknown_fields,
    expecting = "a table with type bearer, api-key, oauth, or login"
)]
pub(super) enum ClientAuth {
    Bearer(AuthValue),
    ApiKey(AuthValue),
    Oauth(AuthValue),
    Login(LoginAuth),
}

/// The `login` credential type, which takes no other keys.
#[derive(Debug, Deserialize)]
#[serde(try_from = "Table")]
pub(super) struct LoginAuth;

impl TryFrom<Table> for LoginAuth {
    type Error = anyhow::Error;

    fn try_from(table: Table) -> Result<Self> {
        ensure!(
            table.is_empty(),
            "client.auth type login takes no other keys"
        );
        Ok(Self)
    }
}

/// One credential source, validated when it deserializes.
#[derive(Deserialize)]
#[serde(try_from = "AuthValueTable")]
pub(super) enum AuthValue {
    /// An inline secret that can travel in an HTTP header value.
    Value(String),
    /// An absolute or `~/` path to a file holding the secret.
    File(PathBuf),
}

impl fmt::Debug for AuthValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthValue").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthValueTable {
    value: Option<String>,
    file: Option<String>,
}

impl TryFrom<AuthValueTable> for AuthValue {
    type Error = anyhow::Error;

    fn try_from(table: AuthValueTable) -> Result<Self> {
        match (table.value, table.file) {
            (Some(value), None) => Ok(Self::Value(inline_secret("client.auth.value", value)?)),
            (None, Some(file)) => expand_absolute(&file)
                .map(Self::File)
                .map_err(|e| anyhow!("invalid client.auth.file: {e:#}")),
            _ => bail!("client.auth requires exactly one of value or file"),
        }
    }
}

/// Validates a secret that must travel in an HTTP header value, naming `key` in an error.
fn inline_secret(key: &str, value: String) -> Result<String> {
    ensure!(!value.is_empty(), "{key} must not be empty");
    HeaderValue::from_str(&value)
        .with_context(|| format!("invalid {key}: expected an HTTP header value"))?;
    Ok(value)
}

impl fmt::Debug for ClientTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTable")
            .field("auth", &self.auth)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CompactionTable {
    #[serde(rename = "auto_enabled")]
    pub(super) enabled: Option<bool>,
    #[serde(rename = "auto_threshold_tokens", deserialize_with = "tokens")]
    pub(super) threshold_tokens: Option<u32>,
    #[serde(rename = "auto_threshold_percent", deserialize_with = "percent")]
    pub(super) threshold_percent: Option<u8>,
}

/// Raw patterns and roots compile after layering, retaining each entry's file for errors.
/// `defaults` turns shipped rule groups off by name.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct PermissionTable {
    pub(super) mode: Option<Mode>,
    pub(super) extra_roots: Option<Vec<String>>,
    pub(super) deny: Option<TierTable>,
    pub(super) ask: Option<TierTable>,
    pub(super) allow: Option<TierTable>,
    pub(super) defaults: Option<BTreeMap<Shipped, bool>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TierTable {
    pub(super) read: Option<Vec<String>>,
    pub(super) edit: Option<Vec<String>>,
    pub(super) exec: Option<Vec<String>>,
    pub(super) net: Option<Vec<String>>,
    pub(super) mcp: Option<Vec<String>>,
}

impl TierTable {
    /// Each class key with its patterns.
    pub(super) const fn classes(&self) -> [(PermissionClass, &Option<Vec<String>>); 5] {
        [
            (PermissionClass::Read, &self.read),
            (PermissionClass::Edit, &self.edit),
            (PermissionClass::Exec, &self.exec),
            (PermissionClass::Net, &self.net),
            (PermissionClass::Mcp, &self.mcp),
        ]
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct TuiTable {
    pub(super) show_thinking: Option<bool>,
    pub(super) show_welcome: Option<bool>,
    pub(super) status_line: Option<Vec<StatusLineSegment>>,
    pub(super) theme: ThemeTable,
}

/// `overrides` stays raw TOML because the TUI owns the slot patch format.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ThemeTable {
    pub(super) base: Option<String>,
    pub(super) overrides: toml::Table,
}

// ── Whole Numbers ──

// Each range matches the env var that overrides the key, so both report the same accepted values.

fn count<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    whole_number(deserializer, 1..=u32::MAX)
}

fn tokens<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    whole_number(deserializer, 0..=u32::MAX)
}

fn percent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u8>, D::Error> {
    whole_number(deserializer, ThresholdPercent::RANGE)
}

fn whole_number<'de, D, T>(deserializer: D, range: RangeInclusive<T>) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: fmt::Display + PartialOrd + TryFrom<i64>,
{
    deserializer.deserialize_option(WholeNumber(range))
}

struct WholeNumber<T>(RangeInclusive<T>);

impl<'de, T> de::Visitor<'de> for WholeNumber<T>
where
    T: fmt::Display + PartialOrd + TryFrom<i64>,
{
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&whole_number_in(&self.0))
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_i64(self)
    }

    fn visit_i64<E: de::Error>(self, raw: i64) -> Result<Self::Value, E> {
        match T::try_from(raw) {
            Ok(value) if self.0.contains(&value) => Ok(Some(value)),
            _ => Err(E::invalid_value(Unexpected::Signed(raw), &self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use indoc::formatdoc;

    use super::*;
    use crate::util::path::testing::HOME_VAR;

    /// The message the TOML parser reports for `body`, which must fail to parse as a config file.
    fn rejection(body: &str) -> String {
        toml::from_str::<FileConfig>(body)
            .expect_err(body)
            .message()
            .to_owned()
    }

    mod file_config {
        use super::*;

        #[test]
        fn debug_omits_inline_credentials_and_custom_header_values() {
            for auth in [
                "auth = { type = 'bearer', value = 'private-token-sentinel' }",
                "auth = { type = 'api-key', value = 'private-token-sentinel' }",
                "auth = { type = 'oauth', value = 'private-token-sentinel' }",
            ] {
                let config: FileConfig = toml::from_str(&formatdoc! {"
                    [client]
                    {auth}
                    headers = {{ x-private = 'private-header-sentinel' }}
                    model = 'claude-haiku-5-5'
                "})
                .unwrap();

                let debug = format!("{config:?}");

                assert!(!debug.contains("private-token-sentinel"), "{debug}");
                assert!(!debug.contains("private-header-sentinel"), "{debug}");
                assert!(debug.contains("claude-haiku-5-5"), "{debug}");
            }
        }
    }

    mod login_auth {
        use super::*;

        mod try_from {
            use super::*;

            #[test]
            fn accepts_the_type_alone() {
                let config: FileConfig =
                    toml::from_str("client.auth = { type = 'login' }").unwrap();

                assert!(matches!(config.client.auth, Some(ClientAuth::Login(_))));
            }

            #[test]
            fn rejects_any_other_key() {
                for body in [
                    "client.auth = { type = 'login', value = 'token' }",
                    "client.auth = { type = 'login', file = '/token' }",
                ] {
                    assert_eq!(
                        rejection(body),
                        "client.auth type login takes no other keys",
                        "{body}"
                    );
                }
            }
        }
    }

    mod auth_value {
        use super::*;

        mod try_from {
            use super::*;

            fn bearer_auth(fields: &str) -> Option<ClientAuth> {
                let body = format!("client.auth = {{ type = 'bearer', {fields} }}");
                let config: FileConfig = toml::from_str(&body).unwrap();
                config.client.auth
            }

            #[test]
            fn takes_an_inline_value() {
                assert!(matches!(
                    bearer_auth("value = 'token'"),
                    Some(ClientAuth::Bearer(AuthValue::Value(value))) if value == "token"
                ));
            }

            #[test]
            fn takes_an_absolute_or_home_relative_file() {
                let dir = tempfile::tempdir().unwrap();
                let absolute = dir.path().join("token");
                let raw = toml::Value::String(absolute.display().to_string());

                temp_env::with_var(HOME_VAR, Some(dir.path()), || {
                    for (fields, expected) in [
                        (format!("file = {raw}"), &absolute),
                        ("file = '~/token'".to_owned(), &absolute),
                    ] {
                        assert!(
                            matches!(
                                bearer_auth(&fields),
                                Some(ClientAuth::Bearer(AuthValue::File(ref path))) if path == expected
                            ),
                            "{fields}"
                        );
                    }
                });
            }

            #[test]
            fn rejects_neither_or_both_sources() {
                for fields in ["", ", value = 'token', file = '/token'"] {
                    let body = format!("client.auth = {{ type = 'bearer'{fields} }}");

                    assert_eq!(
                        rejection(&body),
                        "client.auth requires exactly one of value or file",
                        "{body}"
                    );
                }
            }

            #[test]
            fn rejects_an_empty_value() {
                assert_eq!(
                    rejection("client.auth = { type = 'bearer', value = '' }"),
                    "client.auth.value must not be empty"
                );
            }

            #[test]
            fn rejects_a_file_path_that_is_not_absolute_or_under_home() {
                for file in ["token", ""] {
                    let body = format!("client.auth = {{ type = 'bearer', file = '{file}' }}");

                    assert_eq!(
                        rejection(&body),
                        "invalid client.auth.file: expected an absolute or `~/` path",
                        "{file:?}"
                    );
                }
            }
        }
    }

    mod inline_secret {
        use super::*;

        #[test]
        fn passes_a_header_safe_value_through() {
            assert_eq!(
                inline_secret("client.auth.value", "token".to_owned()).unwrap(),
                "token"
            );
        }

        #[test]
        fn rejects_a_value_no_header_can_carry() {
            let err = inline_secret("client.auth.value", "bad\u{7}key".to_owned()).unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                "invalid client.auth.value: expected an HTTP header value: \
                 failed to parse header value"
            );
        }
    }
}
