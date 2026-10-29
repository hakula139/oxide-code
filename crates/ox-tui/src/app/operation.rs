//! The one operation the engine runs for the TUI, from its dispatch to its end, and what the status
//! bar shows while it runs.

use std::borrow::Cow;

use crossterm::event::KeyCode;

use ox_core::protocol::{Event, Op, OpEnd, OpId};

use crate::keymap::{KeyChord, hint};

const ESC: KeyChord = KeyChord::plain(KeyCode::Esc);

/// An idle-only op the engine runs, from its dispatch to the [`Event::OpEnded`] that ends it.
#[derive(Debug)]
pub(super) struct InFlight {
    op: Op,
    id: OpId,
    activity: Activity,
}

impl InFlight {
    /// Tracks `op`, submitted as `id`, until the engine ends it. `None` for an op the engine takes
    /// at any time, which the TUI never waits on.
    pub(super) fn new(op: &Op, id: OpId) -> Option<Self> {
        let activity = match op {
            Op::Prompt { .. } => Activity::Streaming,
            Op::Compact { .. } => Activity::Compacting,
            Op::Clear => Activity::Clearing,
            Op::Archive | Op::SetArchive { .. } => Activity::Archiving,
            Op::FindSaved { .. } => Activity::Searching,
            Op::DeleteSaved { .. } => Activity::Deleting,
            Op::Resume { .. } => Activity::Resuming,
            Op::Rename { .. } => Activity::Renaming,
            Op::SetModel(_) => Activity::UpdatingModel,
            Op::Login => Activity::SigningIn,
            Op::Logout => Activity::SigningOut,
            Op::Queue(_)
            | Op::Interrupt
            | Op::Resolve { .. }
            | Op::LoginCode(_)
            | Op::SetMode(_)
            | Op::Shutdown => return None,
        };
        Some(Self {
            op: op.clone(),
            id,
            activity,
        })
    }

    /// How `event` ends the op, if it is the op's [`Event::OpEnded`].
    pub(super) fn end(&self, event: &Event) -> Option<OpEnd> {
        event.end_of(self.id)
    }

    #[cfg(test)]
    pub(super) const fn id(&self) -> OpId {
        self.id
    }

    pub(super) const fn activity(&self) -> &Activity {
        &self.activity
    }

    /// Returns whether the op runs an agent turn.
    pub(super) const fn runs_turn(&self) -> bool {
        matches!(self.op, Op::Prompt { .. })
    }

    /// Returns whether a sign-in waits for the code the sign-in page shows.
    pub(super) fn awaits_sign_in_code(&self) -> bool {
        self.op == Op::Login && !self.cancelling()
    }

    pub(super) fn cancelling(&self) -> bool {
        self.activity == Activity::Cancelling
    }

    /// Marks the op cancelled, which the status shows until the op ends.
    pub(super) fn cancel(&mut self) {
        self.activity = Activity::Cancelling;
    }

    /// Shows what the running turn does now, unless a cancel is pending. Returns whether the turn
    /// left an automatic compaction, which releases the prompts it held.
    pub(super) fn set_activity(&mut self, activity: Activity) -> bool {
        if self.cancelling() {
            return false;
        }
        let left_compaction = self.activity == Activity::Compacting;
        self.activity = activity;
        left_compaction && self.activity != Activity::Compacting
    }

    /// Returns whether the engine can splice queued prompts into the op: a running turn that no
    /// cancel or compaction holds, since either can end the turn before its next round.
    pub(super) fn takes_queued_prompts(&self) -> bool {
        self.runs_turn() && !matches!(self.activity, Activity::Cancelling | Activity::Compacting)
    }
}

/// What the in-flight op is doing, which the status bar shows beside its spinner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Activity {
    Streaming,
    ToolRunning { name: String },
    Compacting,
    SigningIn,
    SigningOut,
    Clearing,
    Resuming,
    Archiving,
    Searching,
    Deleting,
    Renaming,
    UpdatingModel,
    Cancelling,
}

impl Activity {
    /// The status text, naming the key that stops the work where one does.
    pub(super) fn label(&self) -> Cow<'static, str> {
        let esc = |work: &str, effect: &str| -> Cow<'static, str> {
            format!("{work} · {}", hint(ESC, effect)).into()
        };
        match self {
            Self::Streaming => esc("Streaming", "interrupt"),
            Self::ToolRunning { name } => esc(&format!("Running {name}"), "interrupt"),
            Self::Compacting => esc("Compacting", "interrupt"),
            Self::SigningIn => esc("Signing in", "cancel"),
            Self::SigningOut => "Signing out".into(),
            Self::Clearing => "Clearing".into(),
            Self::Resuming => "Resuming".into(),
            Self::Archiving => "Archiving".into(),
            Self::Searching => "Searching".into(),
            Self::Deleting => "Deleting".into(),
            Self::Renaming => "Renaming".into(),
            Self::UpdatingModel => "Updating model".into(),
            Self::Cancelling => "Cancelling".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use ox_core::model::Effort;
    use ox_core::permission::Mode;
    use ox_core::protocol::testing::op_id;
    use ox_core::protocol::{ApprovalDecision, ConfigPick, SessionRef, SignInCode};
    use ox_core::session::store::listing::ArchiveState;

    use super::*;

    mod in_flight {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn tracks_exactly_the_idle_only_ops() {
                let ops = [
                    Op::prompt("hello"),
                    Op::Queue("queued".to_owned()),
                    Op::Interrupt,
                    Op::Resolve {
                        id: "call-1".to_owned(),
                        decision: ApprovalDecision::Approve,
                    },
                    Op::SetModel(ConfigPick::Effort(Effort::High)),
                    Op::SetMode(Mode::Plan),
                    Op::Compact { instructions: None },
                    Op::Clear,
                    Op::Archive,
                    Op::SetArchive {
                        target: SessionRef::Id("saved".to_owned()),
                        state: ArchiveState::Archived,
                    },
                    Op::FindSaved {
                        prefix: "1a2b".to_owned(),
                    },
                    Op::DeleteSaved {
                        session_id: "saved".to_owned(),
                    },
                    Op::Resume {
                        target: SessionRef::Id("saved".to_owned()),
                    },
                    Op::Rename {
                        title: "Renamed".to_owned(),
                    },
                    Op::Login,
                    Op::LoginCode(SignInCode("code#state".to_owned())),
                    Op::Logout,
                    Op::Shutdown,
                ];
                for op in ops {
                    assert_eq!(
                        InFlight::new(&op, op_id(0)).is_some(),
                        op.is_idle_only(),
                        "{op:?}"
                    );
                }
            }
        }

        mod end {
            use super::*;

            #[test]
            fn ends_only_on_the_op_ended_event_of_its_own_id() {
                let compact = InFlight::new(&Op::Compact { instructions: None }, op_id(1)).unwrap();

                for (event, expected) in [
                    (
                        Event::OpEnded {
                            id: op_id(1),
                            end: OpEnd::Cancelled,
                        },
                        Some(OpEnd::Cancelled),
                    ),
                    (
                        Event::OpEnded {
                            id: op_id(0),
                            end: OpEnd::Completed,
                        },
                        None,
                    ),
                    (Event::TurnFailed("Compaction failed".to_owned()), None),
                ] {
                    assert_eq!(compact.end(&event), expected, "{event:?}");
                }
            }
        }

        mod set_activity {
            use super::*;

            #[test]
            fn releases_held_prompts_only_when_a_turn_leaves_its_compaction() {
                let mut turn = InFlight::new(&Op::prompt("hello"), op_id(0)).unwrap();
                assert!(turn.takes_queued_prompts());

                assert!(!turn.set_activity(Activity::Compacting));
                assert!(!turn.takes_queued_prompts());

                assert!(turn.set_activity(Activity::Streaming));
                assert!(turn.takes_queued_prompts());
            }

            #[test]
            fn keeps_a_pending_cancel() {
                let mut turn = InFlight::new(&Op::prompt("hello"), op_id(0)).unwrap();
                turn.set_activity(Activity::Compacting);
                turn.cancel();

                let released = turn.set_activity(Activity::Streaming);

                assert!(!released);
                assert_eq!(turn.activity(), &Activity::Cancelling);
                assert!(!turn.takes_queued_prompts());
            }
        }

        mod takes_queued_prompts {
            use super::*;

            #[test]
            fn only_a_running_turn_takes_them() {
                let compact = InFlight::new(&Op::Compact { instructions: None }, op_id(0)).unwrap();

                assert!(!compact.takes_queued_prompts());
            }
        }
    }

    mod activity {
        use super::*;

        mod label {
            use super::*;

            #[test]
            fn names_the_work_and_the_key_that_stops_it() {
                for (activity, label) in [
                    (Activity::Streaming, "Streaming · Esc to interrupt"),
                    (
                        Activity::ToolRunning {
                            name: "bash".to_owned(),
                        },
                        "Running bash · Esc to interrupt",
                    ),
                    (Activity::Compacting, "Compacting · Esc to interrupt"),
                    (Activity::SigningIn, "Signing in · Esc to cancel"),
                    (Activity::SigningOut, "Signing out"),
                    (Activity::Clearing, "Clearing"),
                    (Activity::Resuming, "Resuming"),
                    (Activity::Archiving, "Archiving"),
                    (Activity::Searching, "Searching"),
                    (Activity::Deleting, "Deleting"),
                    (Activity::Renaming, "Renaming"),
                    (Activity::UpdatingModel, "Updating model"),
                    (Activity::Cancelling, "Cancelling"),
                ] {
                    assert_eq!(activity.label(), label, "{activity:?}");
                }
            }
        }
    }
}
