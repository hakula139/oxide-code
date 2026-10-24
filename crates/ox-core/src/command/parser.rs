//! Slash-command parsing. Names accept ASCII alphanumerics, `_`, `-`, `:`, and `.`, including
//! namespaced forms such as `/plugin:cmd`.

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

    /// Reconstructs the command line with one space between the name and any trimmed arguments.
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

/// Splits a slash command into its name and optional arguments with leading whitespace removed.
/// Returns `None` for plain prompts, `//` escapes, or invalid name characters. Accepts an empty
/// name for completion, while [`parse_slash`] rejects it.
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
        fn distinguishes_commands_from_literal_prompts() {
            for (case, input, expected) in [
                ("bare command", "/help", Some(("help", ""))),
                (
                    "inner whitespace",
                    "/init  please  write CLAUDE.md  ",
                    Some(("init", "please  write CLAUDE.md")),
                ),
                ("leading whitespace", "   /help", Some(("help", ""))),
                ("trailing whitespace", "/help   ", Some(("help", ""))),
                (
                    "plugin namespace",
                    "/context7-plugin:docs",
                    Some(("context7-plugin:docs", "")),
                ),
                ("plain prompt", "hello", None),
                ("embedded path", "explain /etc/hosts", None),
                ("bare slash", "/", None),
                ("blank slash", "  /  ", None),
                ("escaped slash", "//help", None),
                ("non-ASCII name", "/foo🦀", None),
                ("special character", "/foo!", None),
                ("comma in name", "/foo,bar", None),
            ] {
                let expected = expected.map(|(name, args)| Parsed {
                    name: name.to_owned(),
                    args: args.to_owned(),
                });

                assert_eq!(parse_slash(input), expected, "{case}");
            }
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
