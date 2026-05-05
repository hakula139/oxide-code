//! Slash-command surface.
//!
//! [`parse_slash`] decides whether a submitted prompt is a slash
//! command; [`dispatch`] resolves it against the registry and runs it.
//! Each command is a [`registry::SlashCommand`] impl in its own
//! submodule — adding one is one file plus one entry in
//! [`registry::BUILT_INS`].
//!
//! Output: every command pushes a `SystemMessageBlock` (info / results)
//! or an `ErrorBlock` (unknown command, bad args) to the chat. No modal overlays.
//!
//! Persistence: commands never write user config files. Mutations are
//! session-local; restart returns to the user-declared config (see
//! `docs/design/slash/commands.md` § Design Decisions 6).

mod clear;
mod config;
mod context;
mod diff;
mod effort;
mod format;
mod help;
mod init;
mod matcher;
mod model;
mod parser;
mod picker;
mod registry;
mod status;
mod status_modal;

pub(crate) use context::{SessionInfo, SlashContext};
pub(crate) use matcher::MatchedCommand;
pub(crate) use parser::{Parsed, parse_slash, popup_query};
pub(crate) use registry::SlashKind;

/// Filter the built-in registry against a popup query (leading `/` stripped).
pub(crate) fn filter_built_ins(query: &str) -> Vec<MatchedCommand> {
    matcher::filter_and_rank(query, registry::BUILT_INS)
}

/// Resolves and runs a parsed slash command against the built-in registry.
pub(crate) fn dispatch(
    parsed: &Parsed,
    ctx: &mut SlashContext<'_>,
) -> Option<crate::agent::event::UserAction> {
    dispatch_with(registry::BUILT_INS, parsed, ctx)
}

/// Resolves `parsed` against `commands` and runs the matching impl. Returns `Some(action)` for
/// state-mutating commands; `None` for local / unknown / errored paths (which already pushed the
/// appropriate chat block). Extracted so tests can drive a synthetic registry.
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

/// Comma-separated `/name` list for the unknown-command hint.
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

/// Classify whether `parsed` is safe to dispatch mid-turn.
pub(crate) fn classify(parsed: &Parsed) -> SlashKind {
    classify_in(registry::BUILT_INS, parsed)
}

fn classify_in(commands: &[&dyn registry::SlashCommand], parsed: &Parsed) -> SlashKind {
    match registry::lookup_in(commands, &parsed.name) {
        None => SlashKind::Unknown,
        Some(cmd) => cmd.classify(&parsed.args),
    }
}

/// Shared test fixture — a fully-populated `SessionInfo` for per-command test modules.
#[cfg(test)]
pub(crate) fn test_session_info() -> SessionInfo {
    use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};

    // model_id resolves to a real MODELS row so marketing_name() produces a known name in tests.
    SessionInfo {
        cwd: "~/test".to_owned(),
        version: "0.0.0-test",
        session_id: "test-session".to_owned(),
        config: ConfigSnapshot {
            auth_label: "API key",
            base_url: "https://api.test.invalid".to_owned(),
            model_id: "claude-opus-4-7".to_owned(),
            effort: Some(Effort::High),
            max_tokens: 32_000,
            prompt_cache_ttl: PromptCacheTtl::OneHour,
            show_thinking: false,
        },
    }
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
        let outcome = dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        assert!(outcome.is_none(), "/help is Done, not Forward");
        assert!(!chat.last_is_error());
        assert_eq!(chat.entry_count(), 1);
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

    /// Always-erroring command with aliases — drives dispatch error-wrapping and alias resolution.
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
        // Error wrapping must echo the typed alias, not the canonical name.
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
