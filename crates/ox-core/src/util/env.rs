//! Env-var readers with empty-is-absent semantics, so a stray empty shell value never shadows a
//! config-file default.

use std::fmt::Display;
use std::ops::RangeInclusive;
use std::str::FromStr;

use anyhow::{Result, bail};

// ── Readers ──

/// Reads an env var, treating unset and empty as equivalent `None`.
pub fn string(key: &str) -> Option<String> {
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

/// Reads a whole-number env var within `range`. Unset or empty is `None`, and any other value,
/// including one that overflows `T`, is an error naming the range.
pub(crate) fn number<T>(key: &str, range: RangeInclusive<T>) -> Result<Option<T>>
where
    T: Display + FromStr + PartialOrd,
{
    let Some(raw) = string(key) else {
        return Ok(None);
    };
    match raw.parse::<T>() {
        Ok(value) if range.contains(&value) => Ok(Some(value)),
        _ => bail!(
            "invalid {key} {raw:?}: expected {}",
            whole_number_in(&range)
        ),
    }
}

/// The accepted values of a whole-number setting, phrased the same for env vars and config files.
pub(crate) fn whole_number_in<T: Display>(range: &RangeInclusive<T>) -> String {
    format!("a whole number from {} to {}", range.start(), range.end())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "OX_TEST_ENV_HELPER";

    mod string {
        use super::*;

        #[test]
        fn unset_or_empty_is_absent() {
            temp_env::with_var_unset(KEY, || {
                assert_eq!(string(KEY), None);
            });
            temp_env::with_var(KEY, Some(""), || {
                assert_eq!(string(KEY), None);
            });
        }

        #[test]
        fn non_empty_reads_value() {
            temp_env::with_var(KEY, Some("hello"), || {
                assert_eq!(string(KEY).as_deref(), Some("hello"));
            });
        }
    }

    mod bool {
        use super::*;

        #[test]
        fn recognizes_true_values_case_insensitively() {
            for truthy in ["1", "true", "yes", "on", "TRUE", "Yes", "ON"] {
                temp_env::with_var(KEY, Some(truthy), || {
                    assert_eq!(bool(KEY).unwrap(), Some(true), "input={truthy}");
                });
            }
        }

        #[test]
        fn recognizes_false_values_case_insensitively() {
            for falsy in ["0", "false", "no", "off", "FALSE", "No", "OFF"] {
                temp_env::with_var(KEY, Some(falsy), || {
                    assert_eq!(bool(KEY).unwrap(), Some(false), "input={falsy}");
                });
            }
        }

        #[test]
        fn unset_or_empty_is_absent() {
            temp_env::with_var_unset(KEY, || {
                assert_eq!(bool(KEY).unwrap(), None);
            });
            temp_env::with_var(KEY, Some(""), || {
                assert_eq!(bool(KEY).unwrap(), None);
            });
        }

        #[test]
        fn unrecognized_value_is_an_error_naming_key_and_value() {
            for bad in ["2", "enabled", " true"] {
                temp_env::with_var(KEY, Some(bad), || {
                    let msg = bool(KEY).unwrap_err().to_string();
                    assert_eq!(
                        msg,
                        format!(
                            "invalid {KEY} {bad:?}: \
                             expected one of 1, true, yes, on, 0, false, no, off"
                        ),
                    );
                });
            }
        }
    }

    mod number {
        use super::*;

        #[test]
        fn reads_a_value_within_range() {
            for (raw, expected) in [("1", 1), ("42", 42), ("100", 100)] {
                temp_env::with_var(KEY, Some(raw), || {
                    assert_eq!(number::<u8>(KEY, 1..=100).unwrap(), Some(expected));
                });
            }
        }

        #[test]
        fn unset_or_empty_is_absent() {
            temp_env::with_var_unset(KEY, || {
                assert_eq!(number::<u32>(KEY, 1..=10).unwrap(), None);
            });
            temp_env::with_var(KEY, Some(""), || {
                assert_eq!(number::<u32>(KEY, 1..=10).unwrap(), None);
            });
        }

        #[test]
        fn rejects_text_overflow_and_out_of_range_values_with_the_range() {
            for bad in ["32k", "-1", "0", "101", "256", " 5"] {
                temp_env::with_var(KEY, Some(bad), || {
                    let msg = number::<u8>(KEY, 1..=100).unwrap_err().to_string();
                    assert_eq!(
                        msg,
                        format!("invalid {KEY} {bad:?}: expected a whole number from 1 to 100"),
                    );
                });
            }
        }
    }
}
