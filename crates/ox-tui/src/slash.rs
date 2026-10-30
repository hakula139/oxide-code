//! Slash commands: [`parse_slash`](ox_core::command::parse_slash) detects them, and [`dispatch`]
//! runs them through the registry of [`registry::SlashCommand`] impls. No command writes the config
//! files, so a setting one changes reverts on restart.

mod archive;
mod clear;
mod compact;
mod config;
mod confirm;
mod delete;
mod diff;
mod effort;
mod effort_slider;
mod exit;
mod help;
mod init;
mod login;
mod matcher;
mod mode;
mod model;
mod registry;
mod rename;
mod resume;
mod sessions;
mod status;
mod theme;

pub(crate) use self::delete::confirm_modal as delete_confirm_modal;
pub(crate) use self::diff::{GitDiffRow, WorkingTreeDiff};
pub(crate) use self::matcher::MatchedCommand;
pub(crate) use self::registry::{ArgCompletion, SlashOutcome};
pub(crate) use self::resume::ResumePicker;
pub(crate) use self::sessions::{
    CLEAR_THEN_DELETE_HINT, archive_message, delete_lookup_error, saved_error_message,
};

use futures::TryFutureExt as _;

use ox_core::command::Parsed;

use crate::session_info::LiveSessionInfo;

/// Returns whether `name` is a built-in slash command or one of its aliases.
pub fn is_built_in(name: &str) -> bool {
    registry::lookup_in(registry::BUILT_INS, name).is_some()
}

/// Filters the built-in registry against a popup query (leading `/` stripped).
pub(crate) fn filter_built_ins(query: &str) -> Vec<MatchedCommand> {
    matcher::filter_and_rank(registry::BUILT_INS, query)
}

/// Curated arg completions for `cmd_name`, matching ASCII case-insensitively with prefix matches
/// before other substring matches. Unknown commands and commands without a roster return no rows.
pub(crate) fn complete_arg_for(cmd_name: &str, prefix: &str) -> Vec<ArgCompletion> {
    registry::lookup_in(registry::BUILT_INS, cmd_name)
        .map(|cmd| cmd.complete_arg(prefix))
        .unwrap_or_default()
}

/// `usage()` string for `cmd_name`, or `None` if the command publishes no arg placeholder.
pub(crate) fn arg_placeholder_for(cmd_name: &str) -> Option<&'static str> {
    registry::lookup_in(registry::BUILT_INS, cmd_name).and_then(registry::SlashCommand::usage)
}

/// Returns whether the typed `/foo args` line should echo into chat history. Unknown commands echo
/// so the dispatcher's error block has the original line for context.
pub(crate) fn echoes_input(parsed: &Parsed) -> bool {
    registry::lookup_in(registry::BUILT_INS, &parsed.name)
        .is_none_or(|cmd| cmd.echoes_input(&parsed.args))
}

/// Resolves and runs `parsed` against the built-in registry. `Err` carries the chat-ready message
/// for an unknown or failed command, as does a failed [`SlashOutcome::Background`] task.
pub(crate) fn dispatch(parsed: &Parsed, info: &LiveSessionInfo) -> Result<SlashOutcome, String> {
    dispatch_with(registry::BUILT_INS, parsed, info)
}

fn dispatch_with(
    commands: &[&dyn registry::SlashCommand],
    parsed: &Parsed,
    info: &LiveSessionInfo,
) -> Result<SlashOutcome, String> {
    let Some(cmd) = registry::lookup_in(commands, &parsed.name) else {
        return Err(format!(
            "Unknown command: `/{name}`. \
             Available: {available}. \
             Use `//{name}` to send the literal text.",
            name = parsed.name,
            available = format_available(commands),
        ));
    };
    match cmd.execute(&parsed.args, info) {
        Ok(SlashOutcome::Background(task)) => {
            let parsed = parsed.clone();
            Ok(SlashOutcome::Background(Box::pin(
                task.map_err(move |msg| parsed.named_error(&msg)),
            )))
        }
        outcome => outcome.map_err(|msg| parsed.named_error(&msg)),
    }
}

fn format_available(commands: &[&dyn registry::SlashCommand]) -> String {
    commands
        .iter()
        .map(|cmd| format!("/{}", cmd.name()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns whether the command `name` waits for an idle App. An unknown command runs mid-turn,
/// which reports the error.
pub(crate) fn idle_only(name: &str) -> bool {
    registry::lookup_in(registry::BUILT_INS, name).is_some_and(registry::SlashCommand::idle_only)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    pub(crate) use super::diff::testing::{added, context, removed};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_info::testing::test_session_info;
    use crate::slash::registry::testing::FakeCmd;

    fn parsed(name: &str, args: &str) -> Parsed {
        Parsed {
            name: name.to_owned(),
            args: args.to_owned(),
        }
    }

    mod echoes_input {
        use super::*;

        #[test]
        fn modal_only_commands_suppress_their_typed_line() {
            for name in ["status", "config", "help"] {
                let parsed = parsed(name, "");
                assert!(!echoes_input(&parsed), "/{name} must not echo");
            }
        }

        #[test]
        fn picker_or_typed_commands_split_on_args() {
            for name in ["effort", "model", "theme"] {
                let bare = parsed(name, "");
                assert!(!echoes_input(&bare), "bare /{name} must not echo");
                let typed = parsed(name, "x");
                assert!(echoes_input(&typed), "typed /{name} must echo");
            }
        }

        #[test]
        fn unknown_command_echoes_so_error_block_has_context() {
            let parsed = parsed("no-such-thing", "");
            assert!(echoes_input(&parsed));
        }
    }

    mod dispatch {
        use super::*;

        fn run_dispatch(name: &str) -> Result<SlashOutcome, String> {
            let parsed = parsed(name, "");
            dispatch(&parsed, &test_session_info())
        }

        #[test]
        fn unknown_command_lists_every_built_in() {
            let msg = run_dispatch("etc").expect_err("unknown command must error");
            for cmd in registry::BUILT_INS {
                let needle = format!("/{}", cmd.name());
                assert!(msg.contains(&needle), "should list `{needle}`: {msg}");
            }
        }
    }

    mod dispatch_with {
        use super::*;

        fn dispatch_fake(name: &str) -> String {
            let parsed = parsed(name, "");
            let info = test_session_info();
            dispatch_with(
                &[&FakeCmd {
                    name: "clear",
                    ..FakeCmd::DEFAULT
                }],
                &parsed,
                &info,
            )
            .unwrap_err()
        }

        #[test]
        fn command_failure_prefixes_the_command_name() {
            assert_eq!(dispatch_fake("clear"), "/clear: Explicit failure.");
        }

        #[test]
        fn alias_routes_to_canonical_impl() {
            // Error must echo the alias the user typed.
            assert_eq!(dispatch_fake("reset"), "/reset: Explicit failure.");
        }

        #[tokio::test]
        async fn background_failure_prefixes_the_name_the_user_typed() {
            let info = test_session_info();
            let cmd = FakeCmd {
                name: "clear",
                background: true,
                ..FakeCmd::DEFAULT
            };

            for (typed, expected) in [
                ("clear", "/clear: Explicit failure."),
                ("reset", "/reset: Explicit failure."),
            ] {
                let task = dispatch_with(&[&cmd], &parsed(typed, ""), &info)
                    .unwrap()
                    .into_background();
                assert_eq!(task.await.unwrap_err(), expected);
            }
        }

        #[test]
        fn unknown_command_lists_available_and_the_escape() {
            assert_eq!(
                dispatch_fake("etc"),
                "Unknown command: `/etc`. Available: /clear. Use `//etc` to send the literal text.",
            );
        }
    }

    mod idle_only {
        use super::*;

        #[test]
        fn pins_which_commands_wait_for_an_idle_app() {
            let cases = [
                ("archive", true),
                ("clear", true),
                ("new", true),
                ("compact", true),
                ("config", false),
                ("delete", true),
                ("diff", false),
                ("effort", true),
                ("exit", false),
                ("quit", false),
                ("help", false),
                ("init", true),
                ("login", true),
                ("logout", true),
                ("mode", false),
                ("model", true),
                ("rename", true),
                ("resume", true),
                ("continue", true),
                ("status", false),
                ("theme", false),
                ("unarchive", true),
                ("no-such-thing", false),
            ];

            for (name, expected) in cases {
                assert_eq!(idle_only(name), expected, "/{name}");
            }
        }
    }
}
