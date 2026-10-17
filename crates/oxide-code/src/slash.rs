//! Slash-command surface.
//!
//! [`parse_slash`] detects commands. [`dispatch`] resolves them via the registry. Each command is
//! a [`registry::SlashCommand`] impl in its own submodule. Adding one is one file plus an entry
//! in [`registry::BUILT_INS`].
//!
//! Persistence: commands never write config. Mutations are session-local. Restart returns to the
//! user-declared config (see `docs/design/slash/commands.md` § Design Decisions 6).

mod archive;
mod clear;
mod compact;
mod config;
mod confirm;
mod context;
mod delete;
mod diff;
mod effort;
mod effort_slider;
mod help;
mod init;
mod login;
mod matcher;
mod model;
mod parser;
mod picker;
mod registry;
mod rename;
mod resume;
mod sessions;
mod status;
mod theme;

#[cfg(test)]
pub(crate) use context::testing::test_session_info;
pub(crate) use context::{LiveSessionInfo, SlashContext};
pub(crate) use matcher::MatchedCommand;
pub(crate) use parser::{Parsed, PopupState, parse_slash, popup_state, unescape_literal};
pub(crate) use registry::{ArgCompletion, SlashKind, SlashOutcome};
pub(crate) use resume::ResumePicker;

/// Filter the built-in registry against a popup query (leading `/` stripped).
pub(crate) fn filter_built_ins(query: &str) -> Vec<MatchedCommand> {
    matcher::filter_and_rank(query, registry::BUILT_INS)
}

/// Typed-arg completions for `cmd_name`, prefix-filtered. Returns empty for unknown commands
/// or commands without a curated roster.
pub(crate) fn complete_arg_for(cmd_name: &str, prefix: &str) -> Vec<ArgCompletion> {
    registry::lookup_in(registry::BUILT_INS, cmd_name)
        .map(|cmd| cmd.complete_arg(prefix))
        .unwrap_or_default()
}

/// `usage()` string for `cmd_name`, or `None` if the command publishes no arg placeholder.
pub(crate) fn arg_placeholder_for(cmd_name: &str) -> Option<&'static str> {
    registry::lookup_in(registry::BUILT_INS, cmd_name).and_then(registry::SlashCommand::usage)
}

/// Whether the typed `/foo args` line should echo into chat history. Unknown commands echo
/// so the dispatcher's error block has the original line for context.
pub(crate) fn echoes_input(parsed: &Parsed) -> bool {
    registry::lookup_in(registry::BUILT_INS, &parsed.name)
        .is_none_or(|cmd| cmd.echoes_input(&parsed.args))
}

/// Resolves and runs `parsed` against the built-in registry. `Err` carries the chat-ready
/// message for an unknown or failed command.
pub(crate) fn dispatch(parsed: &Parsed, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
    dispatch_with(registry::BUILT_INS, parsed, ctx)
}

fn dispatch_with(
    commands: &[&dyn registry::SlashCommand],
    parsed: &Parsed,
    ctx: &SlashContext<'_>,
) -> Result<SlashOutcome, String> {
    let Some(cmd) = registry::lookup_in(commands, &parsed.name) else {
        return Err(format!(
            "Unknown command: /{name}. Available: {available}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            available = format_available(commands),
        ));
    };
    cmd.execute(&parsed.args, ctx)
        .map_err(|msg| format!("/{}: {msg}", parsed.name))
}

fn format_available(commands: &[&dyn registry::SlashCommand]) -> String {
    commands
        .iter()
        .map(|cmd| format!("/{}", cmd.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `parsed` is safe to dispatch mid-turn. Only `ReadOnly` may run while the agent is
/// streaming; the caller defers `Mutating` and rejects `Unknown`.
pub(crate) fn classify(parsed: &Parsed) -> SlashKind {
    classify_in(registry::BUILT_INS, parsed)
}

fn classify_in(commands: &[&dyn registry::SlashCommand], parsed: &Parsed) -> SlashKind {
    match registry::lookup_in(commands, &parsed.name) {
        None => SlashKind::Unknown,
        Some(cmd) => cmd.classify(&parsed.args),
    }
}

/// 36-byte UUID-shaped id seeded from `byte`. Matches the wire shape of real session ids so
/// `validate_session_id` accepts them and `id_prefix` slicing lines up.
#[cfg(test)]
pub(crate) fn stamped_id(byte: u8) -> String {
    let s = format!("{byte:02x}");
    format!(
        "{s}{s}1111-2222-3333-4444-{s}{s}{s}{s}{s}{s}",
        s = s.repeat(2),
    )
}

/// Run `f` with a fresh `XDG_DATA_HOME` so `SessionStore::open` lands in a private tempdir.
/// Hands the directory back so callers can layer extra stores beneath the same root.
#[cfg(test)]
pub(crate) fn with_isolated_xdg<R>(f: impl FnOnce(&std::path::Path) -> R) -> R {
    let dir = tempfile::tempdir().unwrap();
    temp_env::with_var("XDG_DATA_HOME", Some(dir.path().as_os_str()), || {
        f(dir.path())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::registry::testing::FakeCmd;

    fn parsed(name: &str, args: &str) -> Parsed {
        Parsed {
            name: name.to_owned(),
            args: args.to_owned(),
        }
    }

    fn run_dispatch(name: &str) -> Result<SlashOutcome, String> {
        let parsed = parsed(name, "");
        dispatch(&parsed, &SlashContext::new(&test_session_info(), None))
    }

    // ── echoes_input ──

    #[test]
    fn echoes_input_modal_only_commands_suppress_their_typed_line() {
        for name in ["status", "config", "help"] {
            let parsed = parsed(name, "");
            assert!(!echoes_input(&parsed), "/{name} must not echo");
        }
    }

    #[test]
    fn echoes_input_picker_or_typed_commands_split_on_args() {
        for name in ["effort", "model", "theme"] {
            let bare = parsed(name, "");
            assert!(!echoes_input(&bare), "bare /{name} must not echo");
            let typed = parsed(name, "x");
            assert!(echoes_input(&typed), "typed /{name} must echo");
        }
    }

    #[test]
    fn echoes_input_unknown_command_echoes_so_error_block_has_context() {
        let parsed = parsed("no-such-thing", "");
        assert!(echoes_input(&parsed));
    }

    // ── dispatch ──

    #[test]
    fn dispatch_known_command_returns_its_outcome() {
        run_dispatch("help").unwrap().into_modal();
    }

    #[test]
    fn dispatch_unknown_command_lists_every_built_in() {
        let msg = run_dispatch("etc").expect_err("unknown command must error");
        for cmd in registry::BUILT_INS {
            let needle = format!("/{}", cmd.name());
            assert!(msg.contains(&needle), "should list `{needle}`: {msg}");
        }
    }

    // ── dispatch_with ──

    fn dispatch_fake(name: &str) -> String {
        let parsed = parsed(name, "");
        let info = test_session_info();
        dispatch_with(
            &[&FakeCmd {
                name: "failing",
                aliases: &["bust", "boom"],
                error: "Explicit failure.",
                ..FakeCmd::DEFAULT
            }],
            &parsed,
            &SlashContext::new(&info, None),
        )
        .unwrap_err()
    }

    #[test]
    fn dispatch_with_command_failure_prefixes_the_command_name() {
        assert_eq!(dispatch_fake("failing"), "/failing: Explicit failure.");
    }

    #[test]
    fn dispatch_with_alias_routes_to_canonical_impl() {
        // Error must echo the alias the user typed.
        assert_eq!(dispatch_fake("boom"), "/boom: Explicit failure.");
    }

    #[test]
    fn dispatch_with_unknown_command_lists_available_and_the_escape() {
        assert_eq!(
            dispatch_fake("etc"),
            "Unknown command: /etc. Available: /failing. Use //etc to send the literal text.",
        );
    }

    // ── classify ──

    #[test]
    fn classify_built_in_read_only_command_is_read_only() {
        let parsed = parsed("help", "");
        assert_eq!(classify(&parsed), SlashKind::ReadOnly);
    }

    #[test]
    fn classify_built_in_state_mutating_command_is_mutating() {
        for name in ["clear", "new", "reset", "init"] {
            let parsed = parsed(name, "");
            assert_eq!(classify(&parsed), SlashKind::Mutating, "{name}");
        }
    }

    #[test]
    fn classify_unknown_command_is_unknown() {
        let parsed = parsed("no-such-thing", "");
        assert_eq!(classify(&parsed), SlashKind::Unknown);
    }
}
