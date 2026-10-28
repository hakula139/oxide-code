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

    pub(super) fn into_system_message(self) -> String {
        match self {
            Self::SystemMessage(text) => text,
            other => panic!("expected SystemMessage, got {other:?}"),
        }
    }

    pub(super) fn into_background(self) -> BoxFuture<'static, Result<SlashOutcome, String>> {
        match self {
            Self::Background(task) => task,
            other => panic!("expected Background, got {other:?}"),
        }
    }
}

/// Drives busy-gating: while a stream is in flight, the App runs `ReadOnly` and `Steering` forms
/// and refuses `Mutating` ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlashKind {
    /// Pure local read (chat output / modal open).
    ReadOnly,
    /// Forwards an action that applies mid-turn: a TUI-only change, such as a theme swap, or an op
    /// the engine applies to the in-flight turn, such as a permission mode switch.
    Steering,
    /// Touches session state or forwards an op that races the in-flight client.
    Mutating,
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

    /// The [`SlashKind`] of the form `args`, which decides whether it may run mid-turn.
    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::ReadOnly
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
        cmd.execute(args, &SlashContext::new(&test_session_info(), None))
    }

    /// [`run_execute`] awaiting the background task a saved-session command returns.
    pub(crate) async fn run_background(
        cmd: &dyn SlashCommand,
        args: &str,
    ) -> Result<SlashOutcome, String> {
        run_execute(cmd, args)?.into_background().await
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

    use ox_core::command::testing::aliased_names;
    use ox_core::protocol::Op;

    use super::testing::FakeCmd;
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

        mod into_forward {
            use super::*;

            #[test]
            #[should_panic(expected = r#"expected Forward, got Prompt("Create AGENTS.md")"#)]
            fn names_the_variant_it_got() {
                SlashOutcome::Prompt("Create AGENTS.md".to_owned()).into_forward();
            }
        }

        mod into_modal {
            use super::*;

            #[test]
            #[should_panic(expected = "expected Modal, got Background(<task>)")]
            fn names_the_variant_it_got() {
                background().into_modal();
            }
        }

        mod into_background {
            use super::*;

            #[test]
            #[should_panic(expected = r#"expected Background, got Prompt("Create AGENTS.md")"#)]
            fn names_the_variant_it_got() {
                _ = SlashOutcome::Prompt("Create AGENTS.md".to_owned()).into_background();
            }
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

        fn alias_collisions<'a>(commands: &[&'a dyn SlashCommand]) -> Vec<(&'a str, &'a str)> {
            let names: HashSet<_> = commands.iter().map(|c| c.name()).collect();
            commands
                .iter()
                .flat_map(|cmd| {
                    command::aliases(cmd.name())
                        .iter()
                        .filter(|alias| names.contains(*alias))
                        .map(move |alias| (cmd.name(), *alias))
                })
                .collect()
        }

        fn empty_metadata_offenders<'a>(commands: &[&'a dyn SlashCommand]) -> Vec<&'a str> {
            commands
                .iter()
                .filter(|c| c.name().is_empty() || c.description().is_empty())
                .map(|c| c.name())
                .collect()
        }

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

        fn forms_of_kind(kind: SlashKind) -> Vec<(&'static dyn SlashCommand, String)> {
            forms()
                .into_iter()
                .filter(|(cmd, args)| cmd.classify(args) == kind)
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
            let collisions = alias_collisions(BUILT_INS);
            assert!(collisions.is_empty(), "alias collisions: {collisions:?}");
        }

        #[test]
        fn alias_collisions_finds_a_synthetic_overlap() {
            let collider = FakeCmd {
                name: "new",
                ..FakeCmd::DEFAULT
            };
            let registry: &[&dyn SlashCommand] = &[&ClearCmd, &collider];
            assert_eq!(alias_collisions(registry), vec![("clear", "new")]);
        }

        #[test]
        fn include_every_command_the_shared_table_aliases() {
            for name in aliased_names() {
                let found = lookup_in(BUILT_INS, name).map(SlashCommand::name);
                assert_eq!(found, Some(name));
            }
        }

        #[test]
        fn have_non_empty_name_and_description() {
            let offenders = empty_metadata_offenders(BUILT_INS);
            assert!(
                offenders.is_empty(),
                "commands with empty name or description: {offenders:?}",
            );
        }

        #[test]
        fn empty_metadata_offenders_reports_command_with_empty_description() {
            let empty = FakeCmd {
                name: "no-desc",
                description: "",
                ..FakeCmd::DEFAULT
            };
            assert_eq!(empty_metadata_offenders(&[&empty]), vec!["no-desc"]);
        }

        #[test]
        fn read_only_forms_never_forward_or_prompt() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);
            let forms = forms_of_kind(SlashKind::ReadOnly);
            assert!(!forms.is_empty(), "no built-in form is read-only");

            let forwarding: Vec<String> = forms
                .iter()
                .filter_map(|(cmd, args)| match cmd.execute(args, &ctx) {
                    Ok(outcome @ (SlashOutcome::Forward(_) | SlashOutcome::Prompt(_))) => {
                        Some(format!("/{} {args} returns {outcome:?}", cmd.name()))
                    }
                    _ => None,
                })
                .collect();

            assert_eq!(forwarding, [] as [String; 0]);
        }

        #[test]
        fn turns_start_only_from_prompts_whose_line_is_echoed() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);

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

        #[test]
        fn steering_forms_forward_only_ops_the_engine_takes_mid_turn() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);
            let forms = forms_of_kind(SlashKind::Steering);
            assert!(!forms.is_empty(), "no built-in form steers");

            let racing: Vec<String> = forms
                .iter()
                .filter_map(|(cmd, args)| match cmd.execute(args, &ctx) {
                    Ok(SlashOutcome::Forward(
                        Action::SwapTheme(_) | Action::Op(Op::SetMode(_) | Op::Shutdown),
                    ))
                    | Err(_) => None,
                    other => Some(format!("/{} {args} forwards {other:?}", cmd.name())),
                })
                .collect();

            assert_eq!(racing, [] as [String; 0]);
        }
    }
}
