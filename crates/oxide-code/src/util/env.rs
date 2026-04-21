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

// Unit tests require `env::set_var` (unsafe under edition 2024); the deferred
// `temp-env` integration suite covers end-to-end precedence instead.
