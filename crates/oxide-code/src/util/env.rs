//! Env-var readers with empty-is-absent semantics, so a stray empty shell value never shadows a
//! config-file default.

use anyhow::{Result, bail};

// ── Readers ──

/// Reads an env var, treating unset and empty as equivalent `None`.
pub(crate) fn string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Reads a boolean env var: `1` / `true` / `yes` / `on` or `0` / `false` / `no` / `off`, matched
/// case-insensitively. Unset or empty is `None`, and any other value is an error.
pub(crate) fn bool(key: &str) -> Result<Option<bool>> {
    let Some(raw) = string(key) else {
        return Ok(None);
    };
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => bail!("invalid {key} {raw:?}: expected one of 1, true, yes, on, 0, false, no, off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "OX_TEST_ENV_HELPER";

    // ── string ──

    #[test]
    fn string_unset_or_empty_is_absent() {
        temp_env::with_var_unset(KEY, || {
            assert_eq!(string(KEY), None);
        });
        temp_env::with_var(KEY, Some(""), || {
            assert_eq!(string(KEY), None);
        });
    }

    #[test]
    fn string_non_empty_reads_value() {
        temp_env::with_var(KEY, Some("hello"), || {
            assert_eq!(string(KEY).as_deref(), Some("hello"));
        });
    }

    // ── bool ──

    #[test]
    fn bool_recognizes_true_values_case_insensitively() {
        for truthy in ["1", "true", "yes", "on", "TRUE", "Yes", "ON"] {
            temp_env::with_var(KEY, Some(truthy), || {
                assert_eq!(bool(KEY).unwrap(), Some(true), "input={truthy}");
            });
        }
    }

    #[test]
    fn bool_recognizes_false_values_case_insensitively() {
        for falsy in ["0", "false", "no", "off", "FALSE", "No", "OFF"] {
            temp_env::with_var(KEY, Some(falsy), || {
                assert_eq!(bool(KEY).unwrap(), Some(false), "input={falsy}");
            });
        }
    }

    #[test]
    fn bool_unset_or_empty_is_absent() {
        temp_env::with_var_unset(KEY, || {
            assert_eq!(bool(KEY).unwrap(), None);
        });
        temp_env::with_var(KEY, Some(""), || {
            assert_eq!(bool(KEY).unwrap(), None);
        });
    }

    #[test]
    fn bool_unrecognized_value_is_an_error_naming_key_and_value() {
        for bad in ["2", "enabled", " true"] {
            temp_env::with_var(KEY, Some(bad), || {
                let msg = bool(KEY).unwrap_err().to_string();
                assert!(msg.contains(KEY), "msg={msg}");
                assert!(msg.contains(&format!("{bad:?}")), "msg={msg}");
                assert!(msg.contains("expected one of"), "msg={msg}");
            });
        }
    }
}
