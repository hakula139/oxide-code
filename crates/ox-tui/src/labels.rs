//! Labels for the resolved settings that `/status`, `/config`, and the welcome screen show.

use ox_core::auth::AuthSource;
use ox_core::model::Effort;

/// The credential label while signed out.
const NOT_SIGNED_IN: &str = "Not signed in";

/// The label of a setting that holds no value.
pub(crate) const NONE: &str = "none";

pub(crate) fn auth(auth: Option<AuthSource>) -> String {
    auth.map_or_else(|| NOT_SIGNED_IN.to_owned(), |source| source.to_string())
}

/// The credential kind alone, as the welcome screen's environment line shows it.
pub(crate) fn auth_kind(auth: Option<AuthSource>) -> &'static str {
    auth.map_or(NOT_SIGNED_IN, |source| source.kind().label())
}

pub(crate) fn effort(effort: Option<Effort>) -> String {
    effort.map_or_else(
        || format!("{NONE} (model has no effort levels)"),
        |e| e.to_string(),
    )
}

pub(crate) const fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

pub(crate) fn max_tool_rounds(cap: Option<u32>) -> String {
    cap.map_or_else(|| "unbounded".to_owned(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod auth {
        use super::*;

        #[test]
        fn names_the_source_or_the_signed_out_state() {
            assert_eq!(auth(Some(AuthSource::OxLogin)), "OAuth (oxide-code login)");
            assert_eq!(auth(None), "Not signed in");
        }
    }

    mod auth_kind {
        use super::*;

        #[test]
        fn names_the_kind_or_the_signed_out_state() {
            assert_eq!(auth_kind(Some(AuthSource::OxLogin)), "OAuth");
            assert_eq!(auth_kind(None), "Not signed in");
        }
    }

    mod effort {
        use super::*;

        #[test]
        fn names_the_effective_level_or_its_absence() {
            assert_eq!(effort(Some(Effort::High)), "high");
            assert_eq!(effort(None), "none (model has no effort levels)");
        }
    }

    mod on_off {
        use super::*;

        #[test]
        fn names_the_two_flag_states() {
            assert_eq!(on_off(true), "on");
            assert_eq!(on_off(false), "off");
        }
    }

    mod max_tool_rounds {
        use super::*;

        #[test]
        fn names_the_cap_or_its_absence() {
            assert_eq!(max_tool_rounds(Some(100)), "100");
            assert_eq!(max_tool_rounds(None), "unbounded");
        }
    }
}
