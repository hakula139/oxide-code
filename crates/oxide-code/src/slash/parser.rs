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

/// In-progress query (leading `/` stripped) when `buffer` is a slash command being typed.
/// `None` for plain prompts, `//` escape, or once whitespace appears (args started).
pub(crate) fn popup_query(buffer: &str) -> Option<&str> {
    let rest = buffer.trim_start().strip_prefix('/')?;
    if rest.starts_with('/') || !rest.chars().all(is_name_char) {
        return None;
    }
    Some(rest)
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

    // ── popup_query ──

    #[test]
    fn popup_query_bare_slash_is_empty_query() {
        assert_eq!(popup_query("/"), Some(""));
    }

    #[test]
    fn popup_query_partial_name_produces_typed_chars() {
        assert_eq!(popup_query("/cl"), Some("cl"));
        assert_eq!(popup_query("/clear"), Some("clear"));
    }

    #[test]
    fn popup_query_tolerates_leading_whitespace() {
        assert_eq!(popup_query("   /he"), Some("he"));
    }

    #[test]
    fn popup_query_hides_once_whitespace_appears() {
        assert!(popup_query("/clear ").is_none());
        assert!(popup_query("/clear arg").is_none());
        assert!(popup_query("/cl ear").is_none());
    }

    #[test]
    fn popup_query_hides_for_double_slash_escape() {
        assert!(popup_query("//etc/hosts").is_none());
        assert!(popup_query("//").is_none());
    }

    #[test]
    fn popup_query_hides_for_plain_prompts() {
        assert!(popup_query("hello").is_none());
        assert!(popup_query("explain /etc/hosts").is_none());
        assert!(popup_query("").is_none());
    }

    #[test]
    fn popup_query_hides_when_name_chars_violated() {
        assert!(popup_query("/foo🦀").is_none());
        assert!(popup_query("/foo!").is_none());
    }
}
