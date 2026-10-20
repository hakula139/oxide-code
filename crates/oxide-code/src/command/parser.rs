//! Slash-command line parser. Splits `/cmd args...` into a name and args. Names accept ASCII
//! alphanumerics, `_`, `-`, `:`, and `.`, so namespaced names such as `/plugin:cmd` parse.

/// `name` with its leading `/` removed, and the trimmed `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) name: String,
    pub(crate) args: String,
}

impl Parsed {
    /// `msg` prefixed with the command name as the user typed it.
    pub(crate) fn named_error(&self, msg: &str) -> String {
        format!("/{}: {msg}", self.name)
    }
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

/// Returns whether `c` may appear in a command name.
pub(crate) fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')
}

/// The prompt text a `//` escape stands for: `//etc` becomes `/etc`. Other input is unchanged.
pub(crate) fn unescape_literal(input: &str) -> &str {
    input
        .trim_start()
        .strip_prefix('/')
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parse_slash {
        use super::*;

        #[test]
        fn bare_name_has_empty_args() {
            let parsed = parse_slash("/help").unwrap();
            assert_eq!(parsed.name, "help");
            assert_eq!(parsed.args, "");
        }

        #[test]
        fn name_with_args_keeps_remainder_trimmed() {
            let parsed = parse_slash("/model claude-sonnet-5-5").unwrap();
            assert_eq!(parsed.name, "model");
            assert_eq!(parsed.args, "claude-sonnet-5-5");
        }

        #[test]
        fn collapses_inner_whitespace_into_args() {
            let parsed = parse_slash("/init please write CLAUDE.md").unwrap();
            assert_eq!(parsed.name, "init");
            assert_eq!(parsed.args, "please write CLAUDE.md");
        }

        #[test]
        fn tolerates_leading_whitespace() {
            let parsed = parse_slash("   /help").unwrap();
            assert_eq!(parsed.name, "help");
        }

        #[test]
        fn trailing_whitespace_in_args_is_trimmed() {
            let parsed = parse_slash("/help   ").unwrap();
            assert_eq!(parsed.args, "");
        }

        #[test]
        fn plain_prompt_is_not_a_command() {
            assert!(parse_slash("hello").is_none());
            assert!(parse_slash("explain /etc/hosts").is_none());
        }

        #[test]
        fn bare_slash_is_not_a_command() {
            assert!(parse_slash("/").is_none());
            assert!(parse_slash("  /  ").is_none());
        }

        #[test]
        fn double_slash_is_not_a_command() {
            assert!(parse_slash("//help").is_none());
        }

        #[test]
        fn accepts_plugin_namespace_in_name() {
            let parsed = parse_slash("/context7-plugin:docs").unwrap();
            assert_eq!(parsed.name, "context7-plugin:docs");
            assert_eq!(parsed.args, "");
        }

        #[test]
        fn rejects_non_ascii_or_special_chars_in_name() {
            assert!(parse_slash("/foo🦀").is_none());
            assert!(parse_slash("/foo!").is_none());
            assert!(parse_slash("/foo,bar").is_none());
        }
    }

    mod unescape_literal {
        use super::*;

        #[test]
        fn strips_one_slash_from_the_escape() {
            assert_eq!(unescape_literal("//etc/hosts"), "/etc/hosts");
            assert_eq!(unescape_literal("  //help me"), "/help me");
            assert_eq!(
                unescape_literal("///x"),
                "//x",
                "only one slash is consumed"
            );
        }

        #[test]
        fn leaves_other_input_unchanged() {
            for input in ["hello", "/help", " explain //etc", ""] {
                assert_eq!(unescape_literal(input), input);
            }
        }
    }
}
