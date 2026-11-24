//! The text every frontend shows for an event, so the TUI and the stdio drivers word it alike. An
//! event whose wording needs engine state, such as the settings before a change, carries the text
//! in a field, as [`Event::ConfigChanged`](super::Event::ConfigChanged) does.

use super::ConfigPick;
use crate::config::compaction::AutoCompactionConfig;
use crate::model::{Effort, ModelSpec};
use crate::permission::Mode;
use crate::session::display::id_prefix;
use crate::util::text::{join_sentences, plural};

/// The error a frontend reports when the engine ends before the op it waits on.
pub const ENGINE_STOPPED: &str = "ox stopped unexpectedly";

/// Explains the pause an [`Event::AutoCompactionStarted`](super::Event::AutoCompactionStarted)
/// opens before the prompt goes out or between the rounds of a turn.
pub const AUTO_COMPACTING: &str = "Compacting the conversation to free up context";

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

/// The settings an [`Event::ConfigChanged`](super::Event::ConfigChanged) notice compares, before or
/// after the change.
pub(crate) struct ConfigSettings {
    pub(crate) model: ModelSpec,
    pub(crate) effort: Option<Effort>,
    pub(crate) auto_compaction: AutoCompactionConfig,
}

/// Confirms an [`Event::ConfigChanged`](super::Event::ConfigChanged), surfacing an effort or
/// compaction threshold the change shifted without being asked, and the warning for a model ID
/// that narrowly misses a catalog row.
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
                None => effort_unapplied(pick),
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
        (Some(req), _, None) => (named, Some(effort_unapplied(req))),
        (None, None, None) => (named, None),
        (None, Some(_), None) => (
            named,
            Some("Effort cleared (model has no effort levels)".to_owned()),
        ),
        (None, None, Some(eff)) => (format!("{named} · effort {eff} (model default)"), None),
        (_, _, Some(eff)) => (format!("{named} · effort {eff}"), None),
    };
    let compaction = (before.auto_compaction != after.auto_compaction)
        .then(|| format!("Auto-compaction {}", after.auto_compaction.abbreviated()));
    let notice = match (effort, compaction) {
        (None, None) => head,
        (Some(effort), None) => format!("{head}. {effort}."),
        (None, Some(compaction)) => format!("{head}. {compaction}."),
        (Some(effort), Some(compaction)) => format!("{head}. {effort}. {compaction}."),
    };
    match model.unsupported_notice() {
        Some(unsupported) => join_sentences(&notice, &unsupported),
        None => notice,
    }
}

/// Notes that the model has no effort levels, so the `requested` effort did not apply.
fn effort_unapplied(requested: Effort) -> String {
    format!("Effort unchanged (model has no effort levels, asked for {requested})")
}

/// The message that reports a session-writer failure.
pub fn session_write_failed(failure: &str) -> String {
    format!("Session write failed: {failure}")
}

/// The message that reports a failure to finalize the session a roll or resume left behind.
pub(crate) fn previous_session_write_failed(failure: &str) -> String {
    format!("Previous session write failed: {failure}")
}

/// The message that reports the fork from session `from` to session `id`.
pub fn session_forked(id: &str, from: &str) -> String {
    format!(
        "Another ox process wrote to session {}, so this conversation continues as session {}",
        id_prefix(from),
        id_prefix(id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::compaction::testing::WINDOW_1M_THRESHOLD;
    use crate::protocol::testing::model_pick;
    use crate::util::text::compact_count;

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

        /// The warning a swap to the off-catalog `custom-model` appends.
        const CUSTOM_NOTICE: &str = "`custom-model` is not a supported model, \
             so requests go without thinking, effort, and context management, \
             and cost tracking stays off. \
             Auto-compaction requires an explicit token threshold.";

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

        fn settings_with_threshold(
            model: &ModelSpec,
            effort: Option<Effort>,
            threshold_tokens: u32,
        ) -> ConfigSettings {
            ConfigSettings {
                auto_compaction: AutoCompactionConfig {
                    enabled: true,
                    threshold_tokens: Some(threshold_tokens),
                },
                ..settings(model, effort)
            }
        }

        #[test]
        fn formats_model_swap_effort_notices() {
            let opus = ModelSpec::resolve("claude-opus-5-5");
            let sonnet = ModelSpec::resolve("claude-sonnet-5-5");
            let custom = ModelSpec::resolve("custom-model");
            let cases = [
                (
                    "both_none",
                    settings(&opus, None),
                    settings(&custom, None),
                    model_pick(custom.id(), None),
                    format!("Switched to custom-model (custom-model). {CUSTOM_NOTICE}"),
                ),
                (
                    "requested_effort_dropped",
                    settings(&opus, None),
                    settings(&custom, None),
                    model_pick(custom.id(), Some(Effort::High)),
                    format!(
                        "Switched to custom-model (custom-model). \
                         Effort unchanged (model has no effort levels, asked for high). \
                         {CUSTOM_NOTICE}"
                    ),
                ),
                (
                    "effort_cleared",
                    settings(&opus, Some(Effort::Xhigh)),
                    settings(&custom, None),
                    model_pick(custom.id(), None),
                    format!(
                        "Switched to custom-model (custom-model). \
                         Effort cleared (model has no effort levels). \
                         {CUSTOM_NOTICE}"
                    ),
                ),
                (
                    // None → Some distinguishes the model default from a carried user pick.
                    "model_default",
                    settings(&custom, None),
                    settings(&opus, Some(Effort::Medium)),
                    model_pick(opus.id(), None),
                    "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort medium (model default)"
                        .to_owned(),
                ),
                (
                    "carried_effort",
                    settings(&sonnet, Some(Effort::High)),
                    settings(&opus, Some(Effort::High)),
                    model_pick(opus.id(), None),
                    "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort high".to_owned(),
                ),
                (
                    "explicit_effort",
                    settings(&sonnet, Some(Effort::Medium)),
                    settings(&opus, Some(Effort::Max)),
                    model_pick(opus.id(), Some(Effort::Max)),
                    "Switched to Claude Opus 5.5 (claude-opus-5-5) · effort max".to_owned(),
                ),
                (
                    "same_model",
                    settings(&opus, Some(Effort::High)),
                    settings(&opus, Some(Effort::High)),
                    model_pick(opus.id(), None),
                    "Using Claude Opus 5.5 (claude-opus-5-5) · effort high".to_owned(),
                ),
            ];

            for (case, before, after, pick, expected) in cases {
                assert_eq!(config_changed(&before, &after, &pick), expected, "{case}");
            }
        }

        #[test]
        fn formats_model_swap_warning_and_compaction_notices() {
            let opus = ModelSpec::resolve("claude-opus-5-5");
            let sonnet = ModelSpec::resolve("claude-sonnet-5-5");
            let custom = ModelSpec::resolve("custom-model");
            let typo = ModelSpec::resolve("claude-opus-5-5-20261001");
            let cases = [
                (
                    "near_miss_hint",
                    settings(&opus, Some(Effort::High)),
                    settings(&typo, Some(Effort::High)),
                    model_pick(typo.id(), None),
                    "Switched to claude-opus-5-5-20261001 (claude-opus-5-5-20261001) · effort high. \
                     `claude-opus-5-5-20261001` is not a supported model, \
                     so requests go without thinking, effort, and context management, \
                     and cost tracking stays off. \
                     Auto-compaction requires an explicit token threshold. \
                     Did you mean `claude-opus-5-5` (Claude Opus 5.5)?"
                        .to_owned(),
                ),
                (
                    "compaction_threshold_change",
                    settings(&opus, Some(Effort::High)),
                    settings_with_threshold(&sonnet, Some(Effort::High), WINDOW_1M_THRESHOLD),
                    model_pick(sonnet.id(), None),
                    format!(
                        "Switched to Claude Sonnet 5.5 (claude-sonnet-5-5) · effort high. \
                         Auto-compaction at {} tokens.",
                        compact_count(WINDOW_1M_THRESHOLD),
                    ),
                ),
                (
                    "lost_effort_and_compaction_change",
                    settings(&opus, Some(Effort::High)),
                    settings_with_threshold(&custom, None, WINDOW_1M_THRESHOLD),
                    model_pick(custom.id(), Some(Effort::Max)),
                    format!(
                        "Switched to custom-model (custom-model). \
                         Effort unchanged (model has no effort levels, asked for max). \
                         Auto-compaction at {} tokens. \
                         {CUSTOM_NOTICE}",
                        compact_count(WINDOW_1M_THRESHOLD),
                    ),
                ),
            ];

            for (case, before, after, pick, expected) in cases {
                assert_eq!(config_changed(&before, &after, &pick), expected, "{case}");
            }
        }

        #[test]
        fn formats_effort_pick_notices() {
            let opus = ModelSpec::resolve("claude-opus-5-5");
            let custom = ModelSpec::resolve("custom-model");
            let typo = ModelSpec::resolve("claude-opus-5-5-20261001");
            let cases = [
                (
                    "explicit_pick",
                    settings(&opus, Some(Effort::High)),
                    settings(&opus, Some(Effort::Xhigh)),
                    ConfigPick::Effort(Effort::Xhigh),
                    "Effort set to xhigh",
                ),
                (
                    "model_without_levels",
                    settings(&custom, None),
                    settings(&custom, None),
                    ConfigPick::Effort(Effort::High),
                    "Effort unchanged (model has no effort levels, asked for high)",
                ),
                (
                    "unknown_model_skips_warning",
                    settings(&typo, None),
                    settings(&typo, None),
                    ConfigPick::Effort(Effort::High),
                    "Effort unchanged (model has no effort levels, asked for high)",
                ),
            ];

            for (case, before, after, pick, expected) in cases {
                assert_eq!(config_changed(&before, &after, &pick), expected, "{case}");
            }
        }
    }

    mod session_forked {
        use super::*;

        #[test]
        fn names_both_sessions_by_their_short_ids() {
            assert_eq!(
                session_forked("fork-session-id", "source-session-id"),
                "Another ox process wrote to session source-s, \
                 so this conversation continues as session fork-ses",
            );
        }
    }
}
