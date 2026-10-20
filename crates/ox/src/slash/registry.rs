//! Slash-command trait and built-in registry.

use std::borrow::Cow;

use ox_core::command;

use super::archive::{ArchiveCmd, UnarchiveCmd};
use super::clear::ClearCmd;
use super::compact::CompactCmd;
use super::config::ConfigCmd;
use super::context::SlashContext;
use super::delete::DeleteCmd;
use super::diff::DiffCmd;
use super::effort::EffortCmd;
use super::help::HelpCmd;
use super::init::InitCmd;
use super::login::{LoginCmd, LogoutCmd};
use super::mode::ModeCmd;
use super::model::ModelCmd;
use super::rename::RenameCmd;
use super::resume::ResumeCmd;
use super::status::StatusCmd;
use super::theme::ThemeCmd;
use crate::tui::action::Action;
use crate::tui::modal::Modal;

/// What a command hands back for the App to apply.
pub(crate) enum SlashOutcome {
    /// Dispatches a state-mutating action, applied locally or sent to the engine as an op.
    Forward(Action),
    /// Opens a modal over the input. The modal is the command's whole response.
    Modal(Box<dyn Modal>),
    /// Prints a system message into chat.
    SystemMessage(String),
    /// Prints a unified git diff into chat.
    GitDiff(String),
}

/// Hand-rolled because `Box<dyn Modal>` can't derive `Debug` without forcing every impl to.
impl std::fmt::Debug for SlashOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forward(action) => f.debug_tuple("Forward").field(action).finish(),
            Self::Modal(_) => f.write_str("Modal(<modal>)"),
            Self::SystemMessage(text) => f.debug_tuple("SystemMessage").field(text).finish(),
            Self::GitDiff(diff) => f.debug_tuple("GitDiff").field(diff).finish(),
        }
    }
}

#[cfg(test)]
impl SlashOutcome {
    pub(crate) fn into_forward(self) -> Action {
        match self {
            Self::Forward(action) => action,
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    pub(crate) fn into_modal(self) -> Box<dyn Modal> {
        match self {
            Self::Modal(modal) => modal,
            other => panic!("expected Modal, got {other:?}"),
        }
    }

    pub(crate) fn into_system_message(self) -> String {
        match self {
            Self::SystemMessage(text) => text,
            other => panic!("expected SystemMessage, got {other:?}"),
        }
    }
}

/// Drives busy-gating: `ReadOnly` and `Steering` may dispatch mid-turn, while the App refuses
/// `Mutating` and rejects `Unknown` while a stream is in flight.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashKind {
    /// Pure local read (chat output / modal open).
    ReadOnly,
    /// Forwards an op the engine applies to the in-flight turn, such as a permission mode switch.
    Steering,
    /// Touches session state or forwards an op that races the in-flight client.
    Mutating,
    /// Returned only by the free dispatcher when the name doesn't resolve.
    Unknown,
}

/// One row in the typed-arg autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArgCompletion {
    pub(crate) value: Cow<'static, str>,
    pub(crate) description: Cow<'static, str>,
}

/// A locally dispatched `/name args` command.
pub(super) trait SlashCommand: Sync {
    fn name(&self) -> &'static str;

    /// Other names the command answers to, from the [`command::aliases`] table the REPL shares.
    fn aliases(&self) -> &'static [&'static str] {
        command::aliases(self.name())
    }

    fn description(&self) -> &'static str;

    /// Per-form: `ReadOnly`, `Steering`, or `Mutating`, never `Unknown`.
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

/// Resolves `name` to the command it names, canonically or as an alias.
pub(super) fn lookup_in<'a>(
    commands: &'a [&'a dyn SlashCommand],
    name: &str,
) -> Option<&'a dyn SlashCommand> {
    commands
        .iter()
        .find(|cmd| cmd.name() == name || cmd.aliases().contains(&name))
        .copied()
}

// ── Testing ──

#[cfg(test)]
pub(super) mod testing {
    use super::{SlashCommand, SlashContext, SlashOutcome};
    use crate::slash::testing::test_session_info;

    /// Runs `cmd` with `args` against [`test_session_info`] and no session title.
    pub(crate) fn run_execute(cmd: &dyn SlashCommand, args: &str) -> Result<SlashOutcome, String> {
        cmd.execute(args, &SlashContext::new(&test_session_info(), None))
    }

    pub(crate) struct FakeCmd {
        pub(crate) name: &'static str,
        pub(crate) aliases: &'static [&'static str],
        pub(crate) description: &'static str,
        pub(crate) error: &'static str,
    }

    impl FakeCmd {
        pub(crate) const DEFAULT: Self = Self {
            name: "fake",
            aliases: &[],
            description: "fake",
            error: "Explicit failure.",
        };
    }

    impl SlashCommand for FakeCmd {
        fn name(&self) -> &'static str {
            self.name
        }
        fn aliases(&self) -> &'static [&'static str] {
            self.aliases
        }
        fn description(&self) -> &'static str {
            self.description
        }
        fn execute(&self, _: &str, _: &SlashContext<'_>) -> Result<SlashOutcome, String> {
            Err(self.error.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ox_core::command::testing::aliased_names;
    use ox_core::protocol::Op;

    use super::*;
    use crate::slash::registry::testing::FakeCmd;
    use crate::slash::testing::test_session_info;
    use crate::tui::modal::ModalAction;
    use crate::tui::modal::testing::ScriptedModal;

    mod slash_outcome {
        use super::*;

        fn done() -> SlashOutcome {
            SlashOutcome::SystemMessage("done".to_owned())
        }

        #[test]
        fn debug_names_each_variant() {
            let modal = SlashOutcome::Modal(Box::new(ScriptedModal::new(ModalAction::Cancelled)));
            assert_eq!(
                format!("{:?}", SlashOutcome::Forward(Action::Op(Op::Clear))),
                "Forward(Op(Clear))"
            );
            assert_eq!(format!("{modal:?}"), "Modal(<modal>)");
            assert_eq!(format!("{:?}", done()), r#"SystemMessage("done")"#);
            assert_eq!(
                format!("{:?}", SlashOutcome::GitDiff("+x".to_owned())),
                r#"GitDiff("+x")"#
            );
        }

        mod into_forward {
            use super::*;

            #[test]
            #[should_panic(expected = r#"expected Forward, got SystemMessage("done")"#)]
            fn names_the_variant_it_got() {
                done().into_forward();
            }
        }

        mod into_modal {
            use super::*;

            #[test]
            #[should_panic(expected = r#"expected Modal, got GitDiff("+x")"#)]
            fn names_the_variant_it_got() {
                SlashOutcome::GitDiff("+x".to_owned()).into_modal();
            }
        }

        mod into_system_message {
            use super::*;

            #[test]
            #[should_panic(expected = "expected SystemMessage, got Forward(Op(Clear))")]
            fn names_the_variant_it_got() {
                SlashOutcome::Forward(Action::Op(Op::Clear)).into_system_message();
            }
        }
    }

    mod lookup_in {
        use super::*;

        const ALIASED: FakeCmd = FakeCmd {
            name: "primary",
            aliases: &["alt", "shortcut"],
            ..FakeCmd::DEFAULT
        };

        #[test]
        fn resolves_canonical_name() {
            let cmd = lookup_in(BUILT_INS, "help").expect("/help is registered");
            assert_eq!(cmd.name(), "help");
        }

        #[test]
        fn resolves_each_alias_to_canonical_impl() {
            let registry: &[&dyn SlashCommand] = &[&ALIASED];
            for alias in ["alt", "shortcut"] {
                let cmd = lookup_in(registry, alias).expect("alias must resolve");
                assert_eq!(cmd.name(), "primary");
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
                    cmd.aliases()
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
                name: "collider",
                aliases: &["help"],
                description: "collide",
                ..FakeCmd::DEFAULT
            };
            let registry: &[&dyn SlashCommand] = &[&HelpCmd, &collider];
            assert_eq!(alias_collisions(registry), vec![("collider", "help")]);
        }

        #[test]
        fn take_their_aliases_from_the_shared_table() {
            for cmd in BUILT_INS {
                assert_eq!(
                    cmd.aliases(),
                    command::aliases(cmd.name()),
                    "/{}",
                    cmd.name()
                );
            }
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
        fn steering_forms_forward_only_ops_the_engine_takes_mid_turn() {
            let info = test_session_info();
            let ctx = SlashContext::new(&info, None);
            let mut steering = 0;

            for cmd in BUILT_INS {
                let completions = cmd
                    .complete_arg("")
                    .into_iter()
                    .map(|c| c.value.into_owned());
                let forms = [String::new(), "x".to_owned()]
                    .into_iter()
                    .chain(completions);
                for args in forms.filter(|args| cmd.classify(args) == SlashKind::Steering) {
                    match cmd.execute(&args, &ctx) {
                        Ok(SlashOutcome::Forward(Action::Op(Op::Queue(_) | Op::SetMode(_))))
                        | Err(_) => {}
                        other => panic!("/{} {args} forwards {other:?}", cmd.name()),
                    }
                    steering += 1;
                }
            }

            assert!(steering > 0, "no built-in form steers");
        }
    }
}
