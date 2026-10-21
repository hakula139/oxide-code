//! The scratch environment of CLI and live subprocesses, without the settings they would inherit.

use std::path::Path;
use std::process::Command;

/// Points the home, config, data, and state directories of `command` under `root`.
pub(super) fn use_scratch_dirs(command: &mut Command, root: &Path) {
    command
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"));
}

pub(super) fn is_provider_var(name: &str) -> bool {
    name.starts_with("ANTHROPIC_")
        || name.starts_with("CLAUDE_CODE_")
        || matches!(
            name,
            "OX_EXTRA_CA_CERTS"
                | "NODE_EXTRA_CA_CERTS"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "ALL_PROXY"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "NO_PROXY"
                | "all_proxy"
                | "http_proxy"
                | "https_proxy"
                | "no_proxy"
        )
}

pub(super) fn is_ox_setting(name: &str) -> bool {
    name.starts_with("OX_") && !is_provider_var(name)
}

pub(super) fn remove_inherited(command: &mut Command, predicate: impl Fn(&str) -> bool) {
    // Without USER, ox skips the Claude Code login in the macOS Keychain.
    command.env_remove("USER");
    for (name, _) in std::env::vars_os() {
        let remove = name.to_str().is_some_and(|name| {
            #[cfg(windows)]
            let name = &name.to_ascii_uppercase();

            predicate(name)
        });
        if remove {
            command.env_remove(name);
        }
    }
}
