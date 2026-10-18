//! Inherited settings removed from CLI and live subprocesses.

use std::process::Command;

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
