//! Slash-command surface.
//!
//! [`parse_slash`] detects commands; [`dispatch`] resolves them via the registry. Each command is
//! a [`registry::SlashCommand`] impl in its own submodule — adding one is one file plus an entry
//! in [`registry::BUILT_INS`].
//!
//! Persistence: commands never write config. Mutations are session-local; restart returns to the
//! user-declared config (see `docs/design/slash/commands.md` § Design Decisions 6).

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
mod matcher;
mod model;
mod parser;
mod picker;
mod registry;
mod rename;
mod resume;
mod status;
mod theme;

pub(crate) use context::{LiveSessionInfo, SlashContext};
pub(crate) use matcher::MatchedCommand;
pub(crate) use parser::{Parsed, PopupState, parse_slash, popup_state};
pub(crate) use registry::{ArgCompletion, SlashKind};

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

/// Resolves and runs `parsed` against the built-in registry.
pub(crate) fn dispatch(
    parsed: &Parsed,
    ctx: &mut SlashContext<'_>,
) -> Option<crate::agent::event::UserAction> {
    dispatch_with(registry::BUILT_INS, parsed, ctx)
}

/// `Some(action)` for state-mutating commands; `None` when the command handled its own output.
/// Extracted so tests can drive a synthetic registry.
fn dispatch_with(
    commands: &[&dyn registry::SlashCommand],
    parsed: &Parsed,
    ctx: &mut SlashContext<'_>,
) -> Option<crate::agent::event::UserAction> {
    let Some(cmd) = registry::lookup_in(commands, &parsed.name) else {
        ctx.chat.push_error(&format!(
            "unknown command: /{name}. Available: {available}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            available = format_available(commands),
        ));
        return None;
    };
    match cmd.execute(&parsed.args, ctx) {
        Ok(registry::SlashOutcome::Done) => None,
        Ok(registry::SlashOutcome::Forward(action)) => Some(action),
        Err(msg) => {
            ctx.chat.push_error(&format!("/{}: {msg}", parsed.name));
            None
        }
    }
}

fn format_available(commands: &[&dyn registry::SlashCommand]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for cmd in commands {
        if !out.is_empty() {
            out.push_str(", ");
        }
        _ = write!(out, "/{}", cmd.name());
    }
    out
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

/// Fully-populated `LiveSessionInfo` for per-command tests.
#[cfg(test)]
pub(crate) fn test_session_info() -> LiveSessionInfo {
    use crate::config::{
        AutoCompactionConfig, CompactionConfig, ConfigSnapshot, Effort, PromptCacheTtl,
    };

    // Real MODELS row so `display_name()` resolves to a known label.
    LiveSessionInfo {
        cwd: "~/test".to_owned(),
        version: "0.0.0-test",
        session_id: "test-session".to_owned(),
        config: ConfigSnapshot {
            auth_label: "API key",
            base_url: "https://api.test.invalid".to_owned(),
            extra_ca_certs: None,
            model_id: "claude-opus-4-7".to_owned(),
            effort: Some(Effort::High),
            max_tokens: 32_000,
            max_tool_rounds: None,
            prompt_cache_ttl: PromptCacheTtl::OneHour,
            compaction: CompactionConfig::resolved_for_test(AutoCompactionConfig {
                enabled: true,
                threshold_tokens: Some(155_000),
            }),
            show_thinking: false,
            show_welcome: true,
            theme_name: "mocha".to_owned(),
        },
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
    use crate::slash::registry::{SlashCommand, SlashOutcome};
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── dispatch ──

    #[test]
    fn dispatch_known_command_runs_and_does_not_push_error() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        let mut ctx = SlashContext::new(&mut chat, &info);
        let outcome = dispatch(&parsed, &mut ctx);
        assert!(outcome.is_none(), "/help is Done, not Forward");
        assert!(ctx.take_modal().is_some(), "/help opens a modal");
        assert!(!chat.last_is_error());
        assert_eq!(
            chat.entry_count(),
            0,
            "modal-only commands push no chat blocks"
        );
        assert_eq!(chat.last_error_text(), None);
    }

    #[test]
    fn dispatch_prompt_submit_command_produces_synthesized_body() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "init".to_owned(),
            args: String::new(),
        };
        let action = dispatch(&parsed, &mut SlashContext::new(&mut chat, &info))
            .expect("/init must return Some(action)");
        assert!(
            matches!(
                &action,
                crate::agent::event::UserAction::SubmitPrompt(body)
                    if body.contains("AGENTS.md")
            ),
            "expected SubmitPrompt with AGENTS.md body, got {action:?}",
        );
        assert_eq!(chat.entry_count(), 0, "the typed line is pushed by the App");
    }

    #[test]
    fn dispatch_unknown_command_pushes_error_block() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "no-such-command".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        assert!(
            chat.last_is_error(),
            "unknown command should land as an ErrorBlock",
        );
    }

    #[test]
    fn dispatch_unknown_command_message_lists_available_and_escape_hint() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "etc".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        let msg = chat.last_error_text().expect("error block present");
        assert!(msg.contains("/help"), "should list /help: {msg}");
        for cmd in registry::BUILT_INS {
            let needle = format!("/{}", cmd.name());
            assert!(msg.contains(&needle), "should list `{needle}`: {msg}");
        }
        assert!(
            msg.contains("//etc"),
            "should mention `//etc` escape: {msg}"
        );
    }

    // ── dispatch_with ──

    /// Always-erroring; exercises error-wrapping and alias resolution.
    struct Failing;
    impl registry::SlashCommand for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn aliases(&self) -> &'static [&'static str] {
            &["bust", "boom"]
        }
        fn description(&self) -> &'static str {
            "test"
        }
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
            Err("explicit failure".to_owned())
        }
    }

    #[test]
    fn failing_fixture_metadata_matches_what_dispatcher_tests_assume() {
        assert_eq!(Failing.name(), "failing");
        assert_eq!(Failing.aliases(), &["bust", "boom"]);
        assert_eq!(Failing.description(), "test");
    }

    #[test]
    fn dispatch_with_command_failure_renders_error_block_prefixed_with_name() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "failing".to_owned(),
            args: String::new(),
        };
        let registry: &[&dyn registry::SlashCommand] = &[&Failing];
        dispatch_with(registry, &parsed, &mut SlashContext::new(&mut chat, &info));
        assert_eq!(chat.last_error_text(), Some("/failing: explicit failure"),);
    }

    #[test]
    fn dispatch_with_alias_routes_to_canonical_impl() {
        // Error must echo the typed alias, not the canonical name.
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "boom".to_owned(),
            args: String::new(),
        };
        let registry: &[&dyn registry::SlashCommand] = &[&Failing];
        dispatch_with(registry, &parsed, &mut SlashContext::new(&mut chat, &info));
        assert_eq!(chat.last_error_text(), Some("/boom: explicit failure"));
    }

    // ── echoes_input ──

    #[test]
    fn echoes_input_modal_only_commands_suppress_their_typed_line() {
        for name in ["status", "config", "help"] {
            let parsed = Parsed {
                name: name.to_owned(),
                args: String::new(),
            };
            assert!(!echoes_input(&parsed), "/{name} must not echo");
        }
    }

    #[test]
    fn echoes_input_picker_or_typed_commands_split_on_args() {
        for name in ["effort", "model", "theme"] {
            let bare = Parsed {
                name: name.to_owned(),
                args: String::new(),
            };
            assert!(!echoes_input(&bare), "bare /{name} must not echo");
            let typed = Parsed {
                name: name.to_owned(),
                args: "x".to_owned(),
            };
            assert!(echoes_input(&typed), "typed /{name} must echo");
        }
    }

    #[test]
    fn echoes_input_unknown_command_echoes_so_error_block_has_context() {
        let parsed = Parsed {
            name: "no-such-thing".to_owned(),
            args: String::new(),
        };
        assert!(echoes_input(&parsed));
    }

    // ── classify ──

    #[test]
    fn classify_built_in_read_only_command_is_read_only() {
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        assert_eq!(classify(&parsed), SlashKind::ReadOnly);
    }

    #[test]
    fn classify_built_in_state_mutating_command_is_mutating() {
        for name in ["clear", "new", "reset", "init"] {
            let parsed = Parsed {
                name: name.to_owned(),
                args: String::new(),
            };
            assert_eq!(classify(&parsed), SlashKind::Mutating, "{name}");
        }
    }

    #[test]
    fn classify_unknown_command_is_unknown() {
        let parsed = Parsed {
            name: "no-such-thing".to_owned(),
            args: String::new(),
        };
        assert_eq!(classify(&parsed), SlashKind::Unknown);
    }
}
