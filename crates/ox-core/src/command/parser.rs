//! Slash-command line parser. Splits `/cmd args...` into a name and args. Names accept ASCII
//! alphanumerics, `_`, `-`, `:`, and `.`, so namespaced names such as `/plugin:cmd` parse.

/// `name` with its leading `/` removed, and the trimmed `args`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub name: String,
    pub args: String,
}

impl Parsed {
    /// `msg` prefixed with the command name as the user typed it.
    pub fn named_error(&self, msg: &str) -> String {
        format!("/{}: {msg}", self.name)
    }

    /// The command line as the user typed it, with one space between the name and any args.
    pub(super) fn line(&self) -> String {
        if self.args.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("/{} {}", self.name, self.args)
        }
    }
}

/// `None` for plain prompts, `//` escape, bare `/`, or names with disallowed chars.
pub fn parse_slash(input: &str) -> Option<Parsed> {
    let (name, args) = split_slash(input)?;
    if name.is_empty() {
        return None;
    }
    Some(Parsed {
        name: name.to_owned(),
        args: args.unwrap_or_default().trim_end().to_owned(),
    })
}

/// Splits a slash-command line into its name and, once whitespace follows the name, the text after
/// that whitespace with its start trimmed. `None` for plain prompts, the `//` escape, and names
/// with disallowed chars. The name may be empty, as for a bare `/`, which a caller completing a
/// name accepts and [`parse_slash`] rejects.
pub fn split_slash(input: &str) -> Option<(&str, Option<&str>)> {
    let rest = input.trim_start().strip_prefix('/')?;
    if rest.starts_with('/') {
        return None;
    }
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, Some(args.trim_start())),
        None => (rest, None),
    };
    name.chars().all(is_name_char).then_some((name, args))
}

/// Returns whether `c` may appear in a command name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')
}

/// The prompt text a `//` escape stands for: `//etc` becomes `/etc`. Other input is unchanged.
pub fn unescape_literal(input: &str) -> &str {
    input
        .trim_start()
        .strip_prefix('/')
        .filter(|rest| rest.starts_with('/'))
        .unwrap_or(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parsed {
        use super::*;

        mod line {
            use super::*;

            #[test]
            fn joins_the_name_and_args_with_one_space() {
                assert_eq!(parse_slash("/init").unwrap().line(), "/init");
                assert_eq!(
                    parse_slash(" /init   AGENTS.md ").unwrap().line(),
                    "/init AGENTS.md"
                );
            }
        }
    }

    mod parse_slash {
        use super::*;

        #[test]
        fn bare_name_has_empty_args() {
            let parsed = parse_slash("/help").unwrap();
            assert_eq!(parsed.name, "help");
            assert_eq!(parsed.args, "");
        }

        #[test]
        fn name_with_args_keeps_inner_whitespace() {
            let parsed = parse_slash("/init  please  write CLAUDE.md  ").unwrap();

            assert_eq!(parsed.name, "init");
            assert_eq!(parsed.args, "please  write CLAUDE.md");
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

    mod split_slash {
        use super::*;

        #[test]
        fn tells_a_bare_name_from_one_followed_by_whitespace() {
            assert_eq!(split_slash("/help"), Some(("help", None)));
            assert_eq!(split_slash("/help "), Some(("help", Some(""))));
            assert_eq!(split_slash("/model  opus "), Some(("model", Some("opus "))));
        }

        #[test]
        fn keeps_an_empty_name_for_the_caller_to_judge() {
            assert_eq!(split_slash("/"), Some(("", None)));
            assert_eq!(split_slash("/ x"), Some(("", Some("x"))));
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
