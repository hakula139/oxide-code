// ── Readers ──

/// Reads an env var, treating unset and empty as equivalent `None` so a
/// stray empty value from the shell doesn't shadow a config-file default.
pub(crate) fn string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// `Some(true)` for `"1"` / `"true"`, `Some(false)` for any other set value,
/// `None` when unset or empty. The `Some(false)` case is deliberate: any
/// non-`"1"`/`"true"` value is an explicit "off" override against config.
pub(crate) fn bool(key: &str) -> Option<bool> {
    string(key).map(|v| v == "1" || v == "true")
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "OX_TEST_ENV_HELPER";

    // ── string ──

    #[test]
    fn string_unset_returns_none() {
        temp_env::with_var_unset(KEY, || {
            assert_eq!(string(KEY), None);
        });
    }

    #[test]
    fn string_empty_returns_none() {
        // Empty-is-absent: a stray empty value in the shell must not
        // shadow a config-file default.
        temp_env::with_var(KEY, Some(""), || {
            assert_eq!(string(KEY), None);
        });
    }

    #[test]
    fn string_non_empty_returns_owned_value() {
        temp_env::with_var(KEY, Some("hello"), || {
            assert_eq!(string(KEY).as_deref(), Some("hello"));
        });
    }

    // ── bool ──

    #[test]
    fn bool_recognizes_true_values() {
        for truthy in ["1", "true"] {
            temp_env::with_var(KEY, Some(truthy), || {
                assert_eq!(bool(KEY), Some(true), "input={truthy}");
            });
        }
    }

    #[test]
    fn bool_any_other_set_value_is_explicit_false() {
        // `Some(false)` means "set to something-that-isn't-truthy", an
        // explicit off override against a config-file `true`.
        for falsy in ["0", "false", "no", "yes", "TRUE"] {
            temp_env::with_var(KEY, Some(falsy), || {
                assert_eq!(bool(KEY), Some(false), "input={falsy}");
            });
        }
    }

    #[test]
    fn bool_unset_and_empty_both_return_none() {
        temp_env::with_var_unset(KEY, || {
            assert_eq!(bool(KEY), None);
        });
        temp_env::with_var(KEY, Some(""), || {
            assert_eq!(bool(KEY), None);
        });
    }
}
