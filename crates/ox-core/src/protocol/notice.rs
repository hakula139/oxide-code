//! The text every frontend shows for an event, so the TUI and the stdio drivers word it alike. An
//! event whose wording needs engine state, such as the settings before a change, carries the text
//! in a field, as [`Event::ConfigChanged`](super::Event::ConfigChanged) does.

use super::ConfigPick;
use crate::config::compaction::AutoCompactionConfig;
use crate::config::{Effort, display_auto_compaction};
use crate::model::ModelSpec;
use crate::permission::Mode;
use crate::util::text::plural;

/// The error a frontend reports when the engine ends before the op it waits on.
pub const ENGINE_STOPPED: &str = "ox stopped unexpectedly";

/// Explains the pause an [`Event::AutoCompactionStarted`](super::Event::AutoCompactionStarted)
/// opens before the prompt goes out.
pub const AUTO_COMPACTING: &str = "Compacting the conversation before sending the prompt";

/// Confirms an [`Event::ModeChanged`](super::Event::ModeChanged).
pub fn mode_changed(mode: Mode) -> String {
    format!("Switched to {mode} mode")
}

/// Heads the boundary an [`Event::SessionCompacted`](super::Event::SessionCompacted) or a replayed
/// [`Interaction::Compacted`](super::Interaction::Compacted) leaves.
pub fn compacted(pre_count: u32, instructions: Option<&str>) -> String {
    let messages = plural(pre_count, "message", "messages");
    let notice = format!("Compacted {pre_count} {messages} into a summary");
    match instructions {
        Some(focus) => format!("{notice} (focus: {focus})"),
        None => notice,
    }
}

/// The settings an [`Event::ConfigChanged`](super::Event::ConfigChanged) notice compares, before
/// or after the change.
pub(crate) struct ConfigSettings {
    pub(crate) model: ModelSpec,
    pub(crate) effort: Option<Effort>,
    pub(crate) auto_compaction: AutoCompactionConfig,
}

/// Confirms an [`Event::ConfigChanged`](super::Event::ConfigChanged), surfacing an effort or
/// compaction threshold the change shifted without being asked.
pub(crate) fn config_changed(
    before: &ConfigSettings,
    after: &ConfigSettings,
    requested: &ConfigPick,
) -> String {
    let requested_effort = match *requested {
        ConfigPick::Model { effort, .. } => effort,
        ConfigPick::Effort(pick) => {
            return match after.effort {
                Some(effort) => format!("Effort set to {effort}"),
                None => pick.unapplied_notice(),
            };
        }
    };
    let model = &after.model;
    let action = if model.id() == before.model.id() {
        "Using"
    } else {
        "Switched to"
    };
    let named = format!("{action} {} ({})", model.display_name(), model.id());
    let (head, effort) = match (requested_effort, before.effort, after.effort) {
        (Some(req), _, None) => (named, Some(req.unapplied_notice())),
        (None, None, None) => (named, None),
        (None, Some(_), None) => (
            named,
            Some("Effort cleared (model has no effort tier)".to_owned()),
        ),
        (None, None, Some(eff)) => (format!("{named} · effort {eff} (model default)"), None),
        (_, _, Some(eff)) => (format!("{named} · effort {eff}"), None),
    };
    let compaction = (before.auto_compaction != after.auto_compaction).then(|| {
        format!(
            "Auto compaction {}",
            display_auto_compaction(after.auto_compaction)
        )
    });
    match (effort, compaction) {
        (None, None) => head,
        (Some(effort), None) => format!("{head}. {effort}."),
        (None, Some(compaction)) => format!("{head}. {compaction}."),
        (Some(effort), Some(compaction)) => format!("{head}. {effort}. {compaction}."),
    }
}

/// The message that reports a session-writer failure.
pub(crate) fn session_write_failed(failure: &str) -> String {
    format!("Session write failed: {failure}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::compaction::testing::WINDOW_1M_THRESHOLD;
    use crate::model::ResolvedModelId;

    mod compacted {
        use super::*;

        #[test]
        fn counts_the_messages_in_the_right_number() {
            assert_eq!(compacted(1, None), "Compacted 1 message into a summary");
            assert_eq!(compacted(7, None), "Compacted 7 messages into a summary");
        }

        #[test]
        fn names_the_focus_the_user_gave() {
            let notice = compacted(4, Some("the login flow"));

            assert_eq!(
                notice,
                "Compacted 4 messages into a summary (focus: the login flow)"
            );
        }
    }

    mod config_changed {
        use super::*;

        fn spec(id: &str) -> ModelSpec {
            ModelSpec::resolve(id.to_owned())
        }

        fn model_pick(model: &ModelSpec, effort: Option<Effort>) -> ConfigPick {
            ConfigPick::Model {
                id: ResolvedModelId::new(model.id().to_owned()),
                effort,
            }
        }

        fn settings(model: &ModelSpec, effort: Option<Effort>) -> ConfigSettings {
            ConfigSettings {
                model: model.clone(),
                effort,
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(155_000),
                },
            }
        }

        #[test]
        fn swap_both_none_omits_effort_clause() {
            let (opus, custom) = (spec("claude-opus-5-5"), spec("custom-model"));

            let s = config_changed(
                &settings(&opus, None),
                &settings(&custom, None),
                &model_pick(&custom, None),
            );

            assert_eq!(s, "Switched to custom-model (custom-model)");
        }

        #[test]
        fn swap_to_no_tier_model_surfaces_requested_effort() {
            // The user needs to know the requested tier didn't apply, or else a silent drop reads
            // as a landed config change.
            let (opus, custom) = (spec("claude-opus-5-5"), spec("custom-model"));

            let s = config_changed(
                &settings(&opus, None),
                &settings(&custom, None),
                &model_pick(&custom, Some(Effort::High)),
            );

            assert_eq!(
                s,
                "Switched to custom-model (custom-model). \
                 Effort unchanged (model has no effort tier, asked for high).",
            );
        }

        #[test]
        fn swap_clears_effort_when_new_model_drops_it() {
            let (opus, custom) = (spec("claude-opus-5-5"), spec("custom-model"));

            let s = config_changed(
                &settings(&opus, Some(Effort::Xhigh)),
                &settings(&custom, None),
                &model_pick(&custom, None),
            );

            assert_eq!(
                s,
                "Switched to custom-model (custom-model). \
                 Effort cleared (model has no effort tier)."
            );
        }

        #[test]
        fn swap_marks_default_when_previous_was_none() {
            // None → Some means the new model's default kicked in. Distinguishing this from "user's
            // pick survived" keeps the user from thinking they chose this tier.
            let (custom, opus) = (spec("custom-model"), spec("claude-opus-5-5"));

            let s = config_changed(
                &settings(&custom, None),
                &settings(&opus, Some(Effort::Medium)),
                &model_pick(&opus, None),
            );

            assert_eq!(
                s,
                "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort medium (model default)"
            );
        }

        #[test]
        fn swap_reports_carried_or_picked_effort_plainly() {
            let (sonnet, opus) = (spec("claude-sonnet-5-5"), spec("claude-opus-5-5"));
            for (prev, requested, new) in [
                (Effort::High, None, Effort::High),
                (Effort::Medium, Some(Effort::Max), Effort::Max),
            ] {
                let s = config_changed(
                    &settings(&sonnet, Some(prev)),
                    &settings(&opus, Some(new)),
                    &model_pick(&opus, requested),
                );

                assert_eq!(
                    s,
                    format!("Switched to Claude Opus 5.5 (claude-opus-5-5) · effort {new}"),
                );
            }
        }

        #[test]
        fn model_pick_of_the_active_model_reads_as_using_it() {
            let opus = spec("claude-opus-5-5");

            let s = config_changed(
                &settings(&opus, Some(Effort::High)),
                &settings(&opus, Some(Effort::High)),
                &model_pick(&opus, None),
            );

            assert_eq!(s, "Using Claude Opus 5.5 (claude-opus-5-5) · effort high");
        }

        #[test]
        fn effort_explicit_pick_matches_resolution() {
            let opus = spec("claude-opus-5-5");

            let s = config_changed(
                &settings(&opus, Some(Effort::High)),
                &settings(&opus, Some(Effort::Xhigh)),
                &ConfigPick::Effort(Effort::Xhigh),
            );

            assert_eq!(s, "Effort set to xhigh");
        }

        #[test]
        fn effort_pick_on_no_tier_model_surfaces_loss() {
            let custom = spec("custom-model");

            let s = config_changed(
                &settings(&custom, None),
                &settings(&custom, None),
                &ConfigPick::Effort(Effort::High),
            );

            assert_eq!(
                s,
                "Effort unchanged (model has no effort tier, asked for high)"
            );
        }

        #[test]
        fn model_swap_mentions_compaction_threshold_change() {
            let (opus, sonnet) = (spec("claude-opus-5-5"), spec("claude-sonnet-5-5"));
            let after = ConfigSettings {
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(WINDOW_1M_THRESHOLD),
                },
                ..settings(&sonnet, Some(Effort::High))
            };

            let s = config_changed(
                &settings(&opus, Some(Effort::High)),
                &after,
                &model_pick(&sonnet, None),
            );

            assert_eq!(
                s,
                format!(
                    "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                     Auto compaction at {WINDOW_1M_THRESHOLD} tokens.",
                ),
            );
        }

        #[test]
        fn model_swap_reports_lost_effort_and_compaction_change() {
            let (opus, custom) = (spec("claude-opus-5-5"), spec("custom-model"));
            let after = ConfigSettings {
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(WINDOW_1M_THRESHOLD),
                },
                ..settings(&custom, None)
            };

            let s = config_changed(
                &settings(&opus, Some(Effort::High)),
                &after,
                &model_pick(&custom, Some(Effort::Max)),
            );

            assert_eq!(
                s,
                format!(
                    "Switched to custom-model (custom-model). \
                     Effort unchanged (model has no effort tier, asked for max). \
                     Auto compaction at {WINDOW_1M_THRESHOLD} tokens.",
                ),
            );
        }
    }
}
