//! Claude Code's macOS Keychain entry, which holds its login ahead of the credentials file.

#[cfg(target_os = "macos")]
use anyhow::Context as _;
use anyhow::Result;
#[cfg(target_os = "macos")]
use tracing::debug;

use super::LoginError;
#[cfg(target_os = "macos")]
use crate::util::env;
#[cfg(any(target_os = "macos", test))]
use crate::util::path::claude_config_var;

#[cfg(any(target_os = "macos", test))]
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Claude Code's macOS Keychain entry, holding the same JSON document as the credentials file.
pub(super) trait Keychain: Sync {
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
pub(super) struct SystemKeychain;

#[cfg(target_os = "macos")]
impl Keychain for SystemKeychain {
    fn read(&self) -> Result<Option<String>, LoginError> {
        use security_framework::passwords::{PasswordOptions, generic_password};

        let account = match keychain_account() {
            Ok(account) => account,
            Err(e) => {
                debug!(error = %format_args!("{e:#}"), "skipping the Keychain");
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
                    debug!(error = %e, "failed to read the Claude Code login from the Keychain");
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

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use std::collections::BTreeMap;

    use super::*;
    #[cfg(target_os = "macos")]
    use crate::util::log::testing::LogCapture;

    #[cfg(target_os = "macos")]
    mod system_keychain {
        use super::*;

        mod read {
            use super::*;

            #[test]
            fn skips_without_an_os_username() {
                let logs = LogCapture::default();

                let result = temp_env::with_var("USER", None::<&str>, || {
                    tracing::dispatcher::with_default(&logs.dispatch(), || SystemKeychain.read())
                })
                .unwrap();

                assert!(result.is_none());
                assert_eq!(
                    logs.events(),
                    [BTreeMap::from([
                        (
                            "error".to_owned(),
                            "failed to determine the OS username".to_owned(),
                        ),
                        ("message".to_owned(), "skipping the Keychain".to_owned()),
                    ])]
                );
            }
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
}
