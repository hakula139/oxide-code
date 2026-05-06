//! Slash-command parser. Splits `/cmd args...` into name + args. Names accept ASCII alphanumerics,
//! `_`, `-`, `:`, `.` for plugin-namespace forward-compat.

/// `name` has the leading `/` stripped; `args` is trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) name: String,
    pub(crate) args: String,
}

/// `None` for plain prompts, `//` escape, bare `/`, or names with disallowed chars.
pub(crate) fn parse_slash(input: &str) -> Option<Parsed> {
    let rest = input.trim_start().strip_prefix('/')?;
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if name.is_empty() || !name.chars().all(is_name_char) {
        return None;
    }
    Some(Parsed {
        name: name.to_owned(),
        args: args.trim().to_owned(),
    })
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')
}

/// In-progress popup context. `Name` while typing the command name (no whitespace yet);
/// `Arg` once the user has typed a space after the name. `None` for plain prompts and `//`
/// escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PopupState<'a> {
    Name(&'a str),
    Arg { name: &'a str, prefix: &'a str },
}

/// Classifies the cursor's position within a single-line input. Empty `prefix` in `Arg` means
/// the user typed `/cmd ` and is poised to start the argument — distinct from `Arg { prefix:
/// non-empty }` (already typing) and `Name` (still inside the name).
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

    // ── parse_slash ──

    #[test]
    fn parse_slash_bare_name_has_empty_args() {
        let parsed = parse_slash("/help").unwrap();
        assert_eq!(parsed.name, "help");
        assert_eq!(parsed.args, "");
    }

    #[test]
    fn parse_slash_name_with_args_keeps_remainder_trimmed() {
        let parsed = parse_slash("/model claude-sonnet-4-6").unwrap();
        assert_eq!(parsed.name, "model");
        assert_eq!(parsed.args, "claude-sonnet-4-6");
    }

    #[test]
    fn parse_slash_collapses_inner_whitespace_into_args() {
        let parsed = parse_slash("/init please write CLAUDE.md").unwrap();
        assert_eq!(parsed.name, "init");
        assert_eq!(parsed.args, "please write CLAUDE.md");
    }

    #[test]
    fn parse_slash_tolerates_leading_whitespace() {
        let parsed = parse_slash("   /help").unwrap();
        assert_eq!(parsed.name, "help");
    }

    #[test]
    fn parse_slash_trailing_whitespace_in_args_is_trimmed() {
        let parsed = parse_slash("/help   ").unwrap();
        assert_eq!(parsed.args, "");
    }

    #[test]
    fn parse_slash_plain_prompt_is_not_a_command() {
        assert!(parse_slash("hello").is_none());
        assert!(parse_slash("explain /etc/hosts").is_none());
    }

    #[test]
    fn parse_slash_bare_slash_is_not_a_command() {
        assert!(parse_slash("/").is_none());
        assert!(parse_slash("  /  ").is_none());
    }

    #[test]
    fn parse_slash_double_slash_is_not_a_command() {
        assert!(parse_slash("//help").is_none());
    }

    #[test]
    fn parse_slash_accepts_plugin_namespace_in_name() {
        let parsed = parse_slash("/context7-plugin:docs").unwrap();
        assert_eq!(parsed.name, "context7-plugin:docs");
        assert_eq!(parsed.args, "");
    }

    #[test]
    fn parse_slash_rejects_non_ascii_or_special_chars_in_name() {
        assert!(parse_slash("/foo🦀").is_none());
        assert!(parse_slash("/foo!").is_none());
        assert!(parse_slash("/foo,bar").is_none());
    }

    // ── popup_state ──

    fn name(s: &str) -> PopupState<'_> {
        PopupState::Name(s)
    }

    fn arg<'a>(name: &'a str, prefix: &'a str) -> PopupState<'a> {
        PopupState::Arg { name, prefix }
    }

    #[test]
    fn popup_state_bare_slash_is_empty_name() {
        assert_eq!(popup_state("/"), Some(name("")));
    }

    #[test]
    fn popup_state_partial_name_carries_typed_chars() {
        assert_eq!(popup_state("/cl"), Some(name("cl")));
        assert_eq!(popup_state("/clear"), Some(name("clear")));
    }

    #[test]
    fn popup_state_tolerates_leading_whitespace_before_slash() {
        assert_eq!(popup_state("   /he"), Some(name("he")));
    }

    #[test]
    fn popup_state_trailing_space_after_name_switches_to_empty_arg() {
        // The empty-prefix case is the trigger for the placeholder ghost-text — we have to be
        // able to distinguish it from `Name`, otherwise the popup never opens for arg mode.
        assert_eq!(popup_state("/clear "), Some(arg("clear", "")));
    }

    #[test]
    fn popup_state_typed_arg_carries_prefix_trimmed_of_leading_whitespace() {
        assert_eq!(popup_state("/model claude-"), Some(arg("model", "claude-")));
        assert_eq!(popup_state("/model    opus"), Some(arg("model", "opus")));
    }

    #[test]
    fn popup_state_inner_whitespace_in_args_is_kept_for_free_form_commands() {
        // /init takes a free-form sentence — the popup is hidden via empty `complete_arg`, but
        // the parser still classifies it as Arg with the full remainder as prefix.
        assert_eq!(
            popup_state("/init please write CLAUDE.md"),
            Some(arg("init", "please write CLAUDE.md")),
        );
    }

    #[test]
    fn popup_state_double_slash_escape_is_not_a_command() {
        assert!(popup_state("//etc/hosts").is_none());
        assert!(popup_state("//").is_none());
    }

    #[test]
    fn popup_state_plain_prompts_are_not_a_command() {
        assert!(popup_state("hello").is_none());
        assert!(popup_state("explain /etc/hosts").is_none());
        assert!(popup_state("").is_none());
    }

    #[test]
    fn popup_state_invalid_name_chars_reject_the_buffer() {
        assert!(popup_state("/foo🦀").is_none());
        assert!(popup_state("/foo!").is_none());
        // Disallowed-char names also fail in arg form so the popup doesn't show stale completions.
        assert!(popup_state("/foo! arg").is_none());
    }

    #[test]
    fn popup_state_empty_name_rejects_buffer() {
        // `/` followed by whitespace has no name to dispatch — must not parse as `Arg { name: "" }`,
        // which would route empty-string lookups through `complete_arg_for` / `arg_placeholder_for`.
        assert!(popup_state("/ ").is_none());
        assert!(popup_state("/    ").is_none());
        assert!(popup_state("/  arg").is_none());
    }
}
