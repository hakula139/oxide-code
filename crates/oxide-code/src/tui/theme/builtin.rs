//! Built-in theme catalog. TOML bodies are embedded via `include_str!` so the binary stays
//! self-contained and the vendored files in `crates/oxide-code/themes/` double as user examples.

pub(super) const MOCHA: &str = include_str!("../../../themes/mocha.toml");

pub(crate) struct BuiltinTheme {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(super) body: &'static str,
}

/// Catppuccin variants listed dark → light, then non-Catppuccin palettes. `/theme` lists them in
/// this order.
pub(crate) const BUILT_IN: &[BuiltinTheme] = &[
    BuiltinTheme {
        name: "mocha",
        description: "Catppuccin dark (default)",
        body: MOCHA,
    },
    BuiltinTheme {
        name: "macchiato",
        description: "Catppuccin dark (medium)",
        body: include_str!("../../../themes/macchiato.toml"),
    },
    BuiltinTheme {
        name: "frappe",
        description: "Catppuccin dark (soft)",
        body: include_str!("../../../themes/frappe.toml"),
    },
    BuiltinTheme {
        name: "latte",
        description: "Catppuccin light",
        body: include_str!("../../../themes/latte.toml"),
    },
    BuiltinTheme {
        name: "material",
        description: "Material dark",
        body: include_str!("../../../themes/material.toml"),
    },
];

pub(super) fn lookup(name: &str) -> Option<&'static str> {
    BUILT_IN
        .iter()
        .find(|theme| theme.name == name)
        .map(|theme| theme.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── lookup ──

    #[test]
    fn lookup_finds_each_builtin_by_name() {
        for BuiltinTheme { name, body, .. } in BUILT_IN {
            let resolved = lookup(name).unwrap_or_else(|| panic!("lookup({name:?}) returned None"));
            assert_eq!(resolved, *body, "wrong body for {name:?}");
        }
    }

    #[test]
    fn lookup_unknown_name_is_absent() {
        assert!(lookup("solarized").is_none());
        assert!(lookup("").is_none());
    }
}
