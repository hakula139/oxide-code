//! Slash-command trait and built-in registry.

use std::borrow::Cow;

use futures::future::BoxFuture;

use ox_core::command;

use super::archive::{ArchiveCmd, UnarchiveCmd};
use super::clear::ClearCmd;
use super::compact::CompactCmd;
use super::config::ConfigCmd;
use super::context::SlashContext;
use super::delete::DeleteCmd;
use super::diff::{DiffCmd, WorkingTreeDiff};
use super::effort::EffortCmd;
use super::exit::ExitCmd;
use super::help::HelpCmd;
use super::init::InitCmd;
use super::login::{LoginCmd, LogoutCmd};
use super::mode::ModeCmd;
use super::model::ModelCmd;
use super::rename::RenameCmd;
use super::resume::ResumeCmd;
use super::status::StatusCmd;
use super::theme::ThemeCmd;
use crate::action::Action;
use crate::modal::Modal;

/// What a command hands back for the App to apply, at submit or once its background task finishes.
pub(crate) enum SlashOutcome {
    /// Prints a system message into chat.
    SystemMessage(String),
    /// Appends a git diff block to chat.
    GitDiff(WorkingTreeDiff),
    /// Dispatches a state-mutating action, applied locally or sent to the engine as an op.
    Forward(Action),
    /// Starts a turn with a prompt the command built, headed by the line the user typed.
    Prompt(String),
    /// Opens a modal over the input. The modal is the command's whole response.
    Modal(Box<dyn Modal>),
    /// Runs off the event loop so slow I/O does not stall input or rendering, then applies the
    /// outcome it resolves to, never a nested `Background`. A session switch drops it unfinished.
    Background(BoxFuture<'static, Result<SlashOutcome, String>>),
}

impl std::fmt::Debug for SlashOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemMessage(text) => f.debug_tuple("SystemMessage").field(text).finish(),
            Self::GitDiff(diff) => f.debug_tuple("GitDiff").field(diff).finish(),
            Self::Forward(action) => f.debug_tuple("Forward").field(action).finish(),
            Self::Prompt(prompt) => f.debug_tuple("Prompt").field(prompt).finish(),
            Self::Modal(_) => f.write_str("Modal(<modal>)"),
            Self::Background(_) => f.write_str("Background(<task>)"),
        }
    }
}

#[cfg(test)]
impl SlashOutcome {
    pub(super) fn into_forward(self) -> Action {
        match self {
            Self::Forward(action) => action,
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    pub(super) fn into_modal(self) -> Box<dyn Modal> {
        match self {
            Self::Modal(modal) => modal,
            other => panic!("expected Modal, got {other:?}"),
        }
    }

    pub(super) fn into_background(self) -> BoxFuture<'static, Result<SlashOutcome, String>> {
        match self {
            Self::Background(task) => task,
            other => panic!("expected Background, got {other:?}"),
        }
    }
}

/// One row in the typed-arg autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArgCompletion {
    pub(crate) value: Cow<'static, str>,
    pub(crate) description: Cow<'static, str>,
}

/// A locally dispatched `/name args` command. Every method that takes `args` receives them trimmed,
/// as [`command::parse_slash`] leaves them.
pub(super) trait SlashCommand: Sync {
    fn name(&self) -> &'static str;

    fn description(&self) -> &'static str;

    /// Whether the command waits for an idle App, because any of its forms builds a prompt or sends
    /// an op that [`ox_core::protocol::Op::is_idle_only`] holds for, directly or from the modal it
    /// opens. The App runs every other command mid-turn.
    fn idle_only(&self) -> bool {
        false
    }

    fn usage(&self) -> Option<&'static str> {
        None
    }

    /// Curated arg suggestions, matching ASCII case-insensitively with prefix matches before other
    /// substring matches. An empty result disables autocomplete.
    fn complete_arg(&self, _prefix: &str) -> Vec<ArgCompletion> {
        Vec::new()
    }

    /// Returns whether the typed line echoes into chat. A form whose modal is its whole response
    /// returns `false`, since the echo would sit above nothing.
    fn echoes_input(&self, _args: &str) -> bool {
        true
    }

    /// `Err(msg)` renders as an `ErrorBlock`.
    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String>;
}

/// Alphabetical for stable `/help` and empty-query popup ordering.
pub(super) const BUILT_INS: &[&dyn SlashCommand] = &[
    &ArchiveCmd,
    &ClearCmd,
    &CompactCmd,
    &ConfigCmd,
    &DeleteCmd,
    &DiffCmd,
    &EffortCmd,
    &ExitCmd,
    &HelpCmd,
    &InitCmd,
    &LoginCmd,
    &LogoutCmd,
    &ModeCmd,
    &ModelCmd,
    &RenameCmd,
    &ResumeCmd,
    &StatusCmd,
    &ThemeCmd,
    &UnarchiveCmd,
];

/// Forwards the op [`command::typed_form`] parses for `name`, so the TUI runs each form it shares
/// with the bare REPL as the REPL does.
///
/// # Panics
///
/// If `name` has no typed form in [`command`].
pub(super) fn typed_form(
    name: &str,
    args: &str,
    ctx: &SlashContext<'_>,
) -> Result<SlashOutcome, String> {
    let op = command::typed_form(name, args, &ctx.info.project_root)
        .unwrap_or_else(|| panic!("/{name} has no typed form"))?;
    Ok(SlashOutcome::Forward(Action::Op(op)))
}

/// Resolves `name` to the command it names, canonically or as one of its [`command::aliases`].
pub(super) fn lookup_in<'a>(
    commands: &'a [&'a dyn SlashCommand],
    name: &str,
) -> Option<&'a dyn SlashCommand> {
    commands
        .iter()
        .find(|cmd| cmd.name() == name || command::aliases(cmd.name()).contains(&name))
        .copied()
}

// ── Testing ──

#[cfg(test)]
pub(super) mod testing {
    use super::{SlashCommand, SlashContext, SlashOutcome};
    use crate::session_info::testing::test_session_info;

    /// Runs `cmd` with `args` against [`test_session_info`] and no session title.
    pub(crate) fn run_execute(cmd: &dyn SlashCommand, args: &str) -> Result<SlashOutcome, String> {
        cmd.execute(args, &SlashContext::new(&test_session_info()))
    }

    /// A command that fails with `error`, answering to the aliases the shared table gives `name`.
    pub(crate) struct FakeCmd {
        pub(crate) name: &'static str,
        pub(crate) description: &'static str,
        pub(crate) error: &'static str,
        /// Whether `execute` fails from a [`SlashOutcome::Background`] task.
        pub(crate) background: bool,
    }

    impl FakeCmd {
        pub(crate) const DEFAULT: Self = Self {
            name: "fake",
            description: "fake",
            error: "Explicit failure.",
            background: false,
        };
    }

    impl SlashCommand for FakeCmd {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            self.description
        }

        fn execute(&self, _: &str, _: &SlashContext<'_>) -> Result<SlashOutcome, String> {
            let error = self.error.to_owned();
            if self.background {
                Ok(SlashOutcome::Background(Box::pin(async { Err(error) })))
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ox_core::protocol::Op;

    use super::*;
    use crate::modal::ModalAction;
    use crate::modal::testing::ScriptedModal;
    use crate::session_info::testing::test_session_info;

    mod slash_outcome {
        use super::*;

        fn background() -> SlashOutcome {
            SlashOutcome::Background(Box::pin(async { Err("never run".to_owned()) }))
        }

        #[test]
        fn debug_names_each_variant() {
            let modal = SlashOutcome::Modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
            assert_eq!(
                format!("{:?}", SlashOutcome::SystemMessage("clean".to_owned())),
                r#"SystemMessage("clean")"#
            );
            assert!(
                format!("{:?}", SlashOutcome::GitDiff(WorkingTreeDiff::default()))
                    .starts_with("GitDiff("),
            );
            assert_eq!(
                format!("{:?}", SlashOutcome::Forward(Action::Op(Op::Clear))),
                "Forward(Op(Clear))"
            );
            assert_eq!(
                format!("{:?}", SlashOutcome::Prompt("Create AGENTS.md".to_owned())),
                r#"Prompt("Create AGENTS.md")"#
            );
            assert_eq!(format!("{modal:?}"), "Modal(<modal>)");
            assert_eq!(format!("{:?}", background()), "Background(<task>)");
        }
    }

    mod lookup_in {
        use super::*;

        #[test]
        fn resolves_canonical_name() {
            let cmd = lookup_in(BUILT_INS, "help").expect("/help is registered");
            assert_eq!(cmd.name(), "help");
        }

        #[test]
        fn resolves_each_alias_to_canonical_impl() {
            for alias in ["new", "reset"] {
                let cmd = lookup_in(BUILT_INS, alias).expect("alias must resolve");
                assert_eq!(cmd.name(), "clear");
            }
        }

        #[test]
        fn unknown_name_resolves_to_no_command() {
            assert!(lookup_in(BUILT_INS, "nonexistent").is_none());
        }
    }

    mod built_ins {
        use super::*;

        /// Every `(command, args)` form reachable from an empty or junk argument or a completion.
        fn forms() -> Vec<(&'static dyn SlashCommand, String)> {
            BUILT_INS
                .iter()
                .flat_map(|&cmd| {
                    let completions = cmd
                        .complete_arg("")
                        .into_iter()
                        .map(|c| c.value.into_owned());
                    [String::new(), "x".to_owned()]
                        .into_iter()
                        .chain(completions)
                        .map(move |args| (cmd, args))
                })
                .collect()
        }

        #[test]
        fn have_unique_canonical_names() {
            let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
            assert_eq!(
                names.len(),
                BUILT_INS.len(),
                "duplicate canonical name in BUILT_INS",
            );
        }

        #[test]
        fn aliases_do_not_collide_with_any_canonical_name() {
            let names: HashSet<_> = BUILT_INS.iter().map(|c| c.name()).collect();
            let collisions: Vec<_> = BUILT_INS
                .iter()
                .flat_map(|cmd| {
                    command::aliases(cmd.name())
                        .iter()
                        .filter(|alias| names.contains(*alias))
                        .map(move |alias| (cmd.name(), *alias))
                })
                .collect();
            assert_eq!(collisions, []);
        }

        #[test]
        fn include_every_command_with_a_typed_form() {
            for name in command::names() {
                let found = lookup_in(BUILT_INS, name).map(SlashCommand::name);
                assert_eq!(found, Some(name));
            }
        }

        #[test]
        fn have_non_empty_name_and_description() {
            let offenders: Vec<_> = BUILT_INS
                .iter()
                .filter(|c| c.name().is_empty() || c.description().is_empty())
                .map(|c| c.name())
                .collect();
            assert_eq!(offenders, [] as [&str; 0]);
        }

        #[test]
        fn forms_that_run_mid_turn_never_start_a_turn_or_send_an_idle_only_op() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info);
            let forms: Vec<_> = forms()
                .into_iter()
                .filter(|(cmd, _)| !cmd.idle_only())
                .collect();
            assert!(
                forms.iter().any(|(cmd, args)| matches!(
                    cmd.execute(args, &ctx),
                    Ok(SlashOutcome::Forward(Action::Op(_)))
                )),
                "no mid-turn form sends an op",
            );

            let racing: Vec<String> = forms
                .iter()
                .filter_map(|(cmd, args)| {
                    let outcome = cmd.execute(args, &ctx);
                    let races = match &outcome {
                        Ok(SlashOutcome::Forward(Action::Op(op))) => op.is_idle_only(),
                        Ok(SlashOutcome::Modal(modal)) => modal.sends_idle_only_ops(),
                        other => matches!(
                            other,
                            Ok(SlashOutcome::Prompt(_)
                                | SlashOutcome::Forward(Action::SlashPrompt { .. }))
                        ),
                    };
                    races.then(|| format!("/{} {args} returns {outcome:?}", cmd.name()))
                })
                .collect();

            assert_eq!(racing, [] as [String; 0]);
        }

        #[test]
        fn overview_commands_open_a_modal() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info);

            for name in ["help", "status", "config"] {
                let cmd = lookup_in(BUILT_INS, name).unwrap();

                let outcome = cmd.execute("", &ctx);

                assert!(
                    matches!(outcome, Ok(SlashOutcome::Modal(_))),
                    "/{name} returns {outcome:?}",
                );
            }
        }

        #[test]
        fn turns_start_only_from_prompts_whose_line_is_echoed() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info);

            let offenders: Vec<String> = forms()
                .iter()
                .filter_map(|(cmd, args)| match cmd.execute(args, &ctx) {
                    Ok(SlashOutcome::Forward(Action::Op(Op::Prompt { .. } | Op::Queue(_)))) => {
                        Some(format!("/{} {args} bypasses the prompt queue", cmd.name()))
                    }
                    Ok(SlashOutcome::Prompt(_)) if !cmd.echoes_input(args) => Some(format!(
                        "/{} {args} starts a turn without its line",
                        cmd.name()
                    )),
                    _ => None,
                })
                .collect();

            assert_eq!(offenders, [] as [String; 0]);
        }
    }
}
