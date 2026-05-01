//! Slash-command surface.
//!
//! [`parse_slash`] decides whether a submitted prompt is a slash
//! command; [`dispatch`] resolves the parsed command against the
//! registry and runs it. The TUI's `App::apply_action_locally` calls
//! both: an unmatched prompt continues to the agent loop, a matched
//! one stays local.
//!
//! Each command is a `SlashCommand` impl in its own submodule. Adding
//! a new command means one file plus one entry in [`registry::BUILT_INS`].
//!
//! Output: every command pushes either a `SystemMessageBlock` (info /
//! results) or an `ErrorBlock` (unknown command, bad args) to the
//! chat. No modal overlays, no toasts.
//!
//! Persistence: slash commands never write to user config files
//! (see `docs/research/design/slash-commands.md` § Design Decisions
//! 6). Mutations are session-local; restart returns to the
//! user-declared config.

mod config;
mod context;
mod diff;
mod format;
mod help;
mod parser;
mod registry;
mod status;

pub(crate) use context::{SessionInfo, SlashContext};
pub(crate) use parser::{Parsed, parse_slash};

/// Resolves and runs a parsed slash command against the built-in
/// registry. See [`dispatch_with`].
pub(crate) fn dispatch(parsed: &Parsed, ctx: &mut SlashContext<'_>) {
    dispatch_with(registry::BUILT_INS, parsed, ctx);
}

/// Resolves `parsed` against `commands` and runs the matching impl.
/// Renders an `ErrorBlock` on unknown name or on `Err` returned by
/// the command; successful commands push their own informational
/// output (`SystemMessageBlock`) before returning `Ok`.
///
/// Extracted so tests can drive the dispatcher with a synthetic
/// registry — exercising the alias-resolution and error-wrapping
/// branches against fake commands rather than reimplementing the
/// dispatcher's tail in test code.
fn dispatch_with(
    commands: &[&dyn registry::SlashCommand],
    parsed: &Parsed,
    ctx: &mut SlashContext<'_>,
) {
    let Some(cmd) = registry::lookup_in(commands, &parsed.name) else {
        ctx.chat.push_error(&format!(
            "unknown command: /{name}. Available: {available}. \
             Use //{name} to send the literal text.",
            name = parsed.name,
            available = format_available(commands),
        ));
        return;
    };
    if let Err(msg) = cmd.execute(&parsed.args, ctx) {
        ctx.chat.push_error(&format!("/{}: {msg}", parsed.name));
    }
}

/// Comma-separated `/name` list for the unknown-command hint. Writes
/// directly into a single `String` to avoid the `Vec<String> + join`
/// double allocation.
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

/// Shared test fixture — a fully-populated `SessionInfo` so per-command
/// tests don't repeat the boilerplate. Lives at the module root so
/// every sibling `slash::*::tests` module can pull it in.
#[cfg(test)]
pub(crate) fn test_session_info() -> SessionInfo {
    use crate::config::{ConfigSnapshot, Effort, PromptCacheTtl};

    SessionInfo {
        model: "Test Model".to_owned(),
        cwd: "~/test".to_owned(),
        version: "0.0.0-test",
        session_id: "test-session".to_owned(),
        config: ConfigSnapshot {
            auth_label: "API key",
            base_url: "https://api.test.invalid".to_owned(),
            model_id: "claude-test-1-0".to_owned(),
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
    use crate::slash::registry::SlashCommand;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── dispatch ──

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
        // The error message must surface (a) what to try instead and
        // (b) the `//foo` escape so the user can send the literal text.
        // Without these, the user has no way to discover the command
        // set or to send `/etc/hosts`-style prompts.
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

    #[test]
    fn dispatch_known_command_runs_and_does_not_push_error() {
        let mut chat = fresh_chat();
        let info = test_session_info();
        let parsed = Parsed {
            name: "help".to_owned(),
            args: String::new(),
        };
        dispatch(&parsed, &mut SlashContext::new(&mut chat, &info));
        assert!(!chat.last_is_error());
        assert_eq!(chat.entry_count(), 1);
        // The last block is a SystemMessageBlock, which inherits the
        // ChatBlock::error_text default returning None. Pin that so a
        // refactor that flipped the default to Some(_) — and silently
        // started letting non-error blocks claim error wording — fails
        // here.
        assert_eq!(chat.last_error_text(), None);
    }

    // ── dispatch_with ──

    /// Synthetic command with two aliases that always returns `Err` —
    /// drives the real dispatcher's alias-resolution and error-wrapping
    /// branches against a registry whose contents the test controls.
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
        fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<(), String> {
            Err("explicit failure".to_owned())
        }
    }

    #[test]
    fn failing_fixture_metadata_matches_what_dispatcher_tests_assume() {
        // The dispatcher tests below rely on `Failing` carrying both
        // aliases and a deliberate `Err`. Pin the metadata directly so
        // a fixture edit that drifted from those assumptions trips
        // here — and so the trait's required `description` slot isn't
        // a silently-uncovered stub.
        assert_eq!(Failing.name(), "failing");
        assert_eq!(Failing.aliases(), &["bust", "boom"]);
        assert_eq!(Failing.description(), "test");
    }

    #[test]
    fn dispatch_with_command_failure_renders_error_block_prefixed_with_name() {
        // Pin the dispatcher's actual error-wrapping shape: an `Err`
        // returned by a command must land as `/name: msg` in an
        // ErrorBlock. Driven through `dispatch_with` so the test
        // exercises the real production path, not a hand-rolled
        // reimplementation of its tail.
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
        // Submitting an alias must run the same impl as the canonical
        // name — and the dispatcher's error wrapping must use the
        // typed name (the alias), not the canonical one, so the user
        // sees what they typed echoed back.
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
}
