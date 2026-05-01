//! Slash-command parser.
//!
//! Detects whether a submitted prompt is a local slash command and, if
//! so, splits it into a name + args pair. Names accept ASCII letters,
//! digits, `_`, `-`, `:`, and `.` so a future plugin-namespace layer
//! (e.g. `/plugin:cmd`) doesn't need a parser rewrite.

/// A parsed `/cmd args...` invocation. `name` is the command name with
/// the leading `/` stripped; `args` is the remainder, trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) name: String,
    pub(crate) args: String,
}

/// Parses `input` as a slash command, returning `None` for plain
/// prompts. Leading whitespace is tolerated; `//` (escape sequence),
/// bare `/`, and names containing characters outside the allowed set
/// all return `None`.
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
        // The split happens at the first whitespace run; arg-side
        // internal whitespace is preserved (e.g. `/init please write
        // CLAUDE.md`).
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
        // Just `/` typed alone (e.g. while opening the popup) is not a
        // command — the popup handles the in-flight buffer separately.
        assert!(parse_slash("/").is_none());
        assert!(parse_slash("  /  ").is_none());
    }

    #[test]
    fn parse_slash_double_slash_is_not_a_command() {
        // `//foo` is the escape sequence: the user wants to send `/foo`
        // as a literal prompt to the model.
        assert!(parse_slash("//help").is_none());
    }

    #[test]
    fn parse_slash_accepts_plugin_namespace_in_name() {
        // Forward-compat for plugin-namespace commands. v1 returns
        // "unknown command" at lookup time; the parser must not reject
        // the syntax so a future plugin layer can ride on top.
        let parsed = parse_slash("/context7-plugin:docs").unwrap();
        assert_eq!(parsed.name, "context7-plugin:docs");
        assert_eq!(parsed.args, "");
    }

    #[test]
    fn parse_slash_rejects_non_ascii_or_special_chars_in_name() {
        // Names with chars outside `[A-Za-z0-9_:.\-]` fall through as
        // "not a command" so the user's prompt isn't hijacked by
        // stray glyphs.
        assert!(parse_slash("/foo🦀").is_none());
        assert!(parse_slash("/foo!").is_none());
        assert!(parse_slash("/foo,bar").is_none());
    }
}
