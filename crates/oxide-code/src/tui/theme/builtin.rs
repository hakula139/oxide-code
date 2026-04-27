//! Built-in theme catalogue.
//!
//! Each TOML body is embedded at compile time via `include_str!` so
//! the binary stays self-contained and the vendored files in
//! `crates/oxide-code/themes/` double as user-facing examples.

pub(super) const MOCHA: &str = include_str!("../../../themes/mocha.toml");
pub(super) const LATTE: &str = include_str!("../../../themes/latte.toml");
pub(super) const FRAPPE: &str = include_str!("../../../themes/frappe.toml");
pub(super) const MACCHIATO: &str = include_str!("../../../themes/macchiato.toml");

/// Name → embedded body lookup table. Order is the suggested
/// dark→light ordering for documentation.
pub(super) const BUILT_IN: &[(&str, &str)] = &[
    ("mocha", MOCHA),
    ("macchiato", MACCHIATO),
    ("frappe", FRAPPE),
    ("latte", LATTE),
];

/// Look up a built-in theme's TOML body by name.
pub(super) fn lookup(name: &str) -> Option<&'static str> {
    BUILT_IN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| *body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_each_builtin_by_name() {
        for (name, body) in BUILT_IN {
            let resolved = lookup(name).unwrap_or_else(|| panic!("lookup({name:?}) returned None"));
            assert_eq!(resolved, *body, "wrong body for {name:?}");
        }
    }

    #[test]
    fn lookup_unknown_name_returns_none() {
        assert!(lookup("solarized").is_none());
        assert!(lookup("").is_none());
    }
}
