//! Parses the words an `exec` permission pattern matches.

use anyhow::{Result, bail, ensure};
use tree_sitter::Node;

use super::PatternWord;
use super::walk::{command_parts, parse_tree, splits_words_like_bash, words};
use super::word::assignment_word;

const NAMES_NO_COMMAND: &str = "it must name a command, such as `cargo test *`";

const ONE_COMMAND: &str = "it must be one simple command, such as `cargo test *`. \
                           Write a pattern for each command of a chain or pipeline.";

const HOLDS_REDIRECTION: &str = "it holds a redirection. \
                                 Leave it out, since a pattern matches a command \
                                 with its redirections set aside, \
                                 and an `edit` pattern covers the file it writes.";

/// Parses an `exec` pattern into its words: one simple command, optionally led by assignments,
/// whose words are each literal or a lone unquoted `*`.
pub(super) fn parse(pattern: &str) -> Result<Vec<PatternWord>> {
    let tree = parse_tree(pattern);
    let root = tree.root_node();
    ensure!(!root.has_error(), "it does not parse as a shell command");
    ensure!(
        splits_words_like_bash(root, pattern),
        "the shell parser `ox` uses would split its words where bash does not. \
         Write it without escaped blanks or line continuations between words."
    );
    let mut cursor = root.walk();
    let nodes: Vec<Node<'_>> = root.named_children(&mut cursor).collect();
    let command = match nodes.as_slice() {
        [] => bail!("{NAMES_NO_COMMAND}"),
        [command] => *command,
        [..] => bail!("{ONE_COMMAND}"),
    };
    match command.kind() {
        "command" => {}
        "redirected_statement" => bail!("{HOLDS_REDIRECTION}"),
        "variable_assignment" => bail!("{NAMES_NO_COMMAND}"),
        _ => bail!("{ONE_COMMAND}"),
    }

    let mut parsed = Vec::new();
    let mut nodes = Vec::new();
    let mut names_command = false;
    for child in command_parts(command) {
        match child.kind() {
            "variable_assignment" => {
                let word = assignment_word(child, pattern);
                ensure!(
                    word.literal,
                    "the assignment {:?} expands at run time. \
                     Quote its value to match it as written.",
                    word.text
                );
                parsed.push(PatternWord::Literal(word.text));
            }
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                bail!("{HOLDS_REDIRECTION}");
            }
            "command_name" => {
                names_command = true;
                match child.named_child(0) {
                    Some(name) => nodes.push(name),
                    None => bail!("{NAMES_NO_COMMAND}"),
                }
            }
            _ => nodes.push(child),
        }
    }
    for word in words(&nodes, pattern) {
        let written = &pattern[word.span.clone()];
        if written == "*" {
            parsed.push(PatternWord::Any);
            continue;
        }
        let Some(value) = word.value else {
            bail!("the word {written:?} expands at run time. Quote it to match it as written.");
        };
        if word.glob {
            bail!(
                "the word {written:?} holds a glob, \
                 but only a `*` standing alone as a word is a wildcard. \
                 Write a pattern for each command it should cover, \
                 or quote the word to match it as written."
            );
        }
        parsed.push(PatternWord::Literal(value));
    }
    ensure!(names_command, "{NAMES_NO_COMMAND}");
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::HOME_VAR;

    mod parse {
        use super::*;

        fn pattern(raw: &str) -> Vec<PatternWord> {
            parse(raw).unwrap()
        }

        fn literal(text: &str) -> PatternWord {
            PatternWord::Literal(text.to_owned())
        }

        #[test]
        fn unquotes_each_word_and_reads_a_lone_star_as_a_wildcard() {
            temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                let home = PathBuf::from(abs("/home/u")).join("x");
                assert_eq!(
                    pattern(r#"GIT_PAGER=cat git  commit -m "a b" '*' \* ~/x *"#),
                    [
                        literal("GIT_PAGER=cat"),
                        literal("git"),
                        literal("commit"),
                        literal("-m"),
                        literal("a b"),
                        literal("*"),
                        literal("*"),
                        literal(&home.to_string_lossy()),
                        PatternWord::Any,
                    ]
                );
            });
            assert_eq!(pattern("*"), [PatternWord::Any]);
            assert_eq!(
                pattern("git * --force"),
                [literal("git"), PatternWord::Any, literal("--force")]
            );
        }
    }
}
