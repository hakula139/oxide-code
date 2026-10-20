//! Slash-command popup state: where the cursor sits in a `/cmd args...` line being typed, by the
//! name rules of [`command::parse_slash`](crate::command::parse_slash).

use crate::command::is_name_char;

/// In-progress popup context: `Name` while the user types the command name, and `Arg` once a
/// space follows it. `None` for plain prompts and the `//` escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PopupState<'a> {
    Name(&'a str),
    Arg { name: &'a str, prefix: &'a str },
}

/// Classifies the cursor's position within a single-line input. An empty `prefix` in `Arg` means
/// the user typed `/cmd ` and has not started the argument yet.
pub(crate) fn popup_state(buffer: &str) -> Option<PopupState<'_>> {
    let rest = buffer.trim_start().strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let Some((name, after)) = rest.split_once(char::is_whitespace) else {
        return rest
            .chars()
            .all(is_name_char)
            .then_some(PopupState::Name(rest));
    };
    if name.is_empty() || !name.chars().all(is_name_char) {
        return None;
    }
    Some(PopupState::Arg {
        name,
        prefix: after.trim_start(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod popup_state {
        use super::*;

        fn name(s: &str) -> PopupState<'_> {
            PopupState::Name(s)
        }

        fn arg<'a>(name: &'a str, prefix: &'a str) -> PopupState<'a> {
            PopupState::Arg { name, prefix }
        }

        #[test]
        fn bare_slash_is_empty_name() {
            assert_eq!(popup_state("/"), Some(name("")));
        }

        #[test]
        fn partial_name_carries_typed_chars() {
            assert_eq!(popup_state("/cl"), Some(name("cl")));
            assert_eq!(popup_state("/clear"), Some(name("clear")));
        }

        #[test]
        fn tolerates_leading_whitespace_before_slash() {
            assert_eq!(popup_state("   /he"), Some(name("he")));
        }

        #[test]
        fn trailing_space_after_name_switches_to_empty_arg() {
            assert_eq!(popup_state("/clear "), Some(arg("clear", "")));
        }

        #[test]
        fn typed_arg_carries_prefix_trimmed_of_leading_whitespace() {
            assert_eq!(popup_state("/model claude-"), Some(arg("model", "claude-")));
            assert_eq!(popup_state("/model    opus"), Some(arg("model", "opus")));
        }

        #[test]
        fn inner_whitespace_in_args_is_kept_for_free_form_commands() {
            assert_eq!(
                popup_state("/init please write CLAUDE.md"),
                Some(arg("init", "please write CLAUDE.md")),
            );
        }

        #[test]
        fn double_slash_escape_is_not_a_command() {
            assert!(popup_state("//etc/hosts").is_none());
            assert!(popup_state("//").is_none());
        }

        #[test]
        fn plain_prompts_are_not_a_command() {
            assert!(popup_state("hello").is_none());
            assert!(popup_state("explain /etc/hosts").is_none());
            assert!(popup_state("").is_none());
        }

        #[test]
        fn invalid_name_chars_reject_the_buffer() {
            assert!(popup_state("/foo🦀").is_none());
            assert!(popup_state("/foo!").is_none());
            assert!(popup_state("/foo! arg").is_none());
        }

        #[test]
        fn empty_name_rejects_buffer() {
            assert!(popup_state("/ ").is_none());
            assert!(popup_state("/    ").is_none());
            assert!(popup_state("/  arg").is_none());
        }
    }
}
