/// Read an environment variable, returning `None` when unset *or* empty.
///
/// Matches the common "unset or empty means absent" interpretation used
/// throughout config loading — an explicit empty string from the shell
/// should not shadow a config-file default.
pub(crate) fn string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Parse a boolean environment variable.
///
/// Returns `Some(true)` for `"1"` or `"true"`, `Some(false)` for any other
/// non-empty value, and `None` when unset or empty. The `Some(false)` case
/// is intentional: setting the variable to any value (even `"0"` or `"no"`)
/// is treated as an explicit override that prevents fallthrough to config
/// file values.
pub(crate) fn bool(key: &str) -> Option<bool> {
    string(key).map(|v| v == "1" || v == "true")
}

// Tests for this module require mutating the process environment, which is
// `unsafe` in edition 2024 and blocked by `unsafe_code = "forbid"`. End-to-end
// env precedence is covered by the deferred `temp-env` integration-test plan.
