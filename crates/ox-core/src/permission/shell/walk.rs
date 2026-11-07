//! Extracts simple commands while keeping unsupported syntax unmatched.

use tree_sitter::{Node, Parser, Tree};

use super::Segment;
use super::word::{Word, assignment_word};

#[derive(Default)]
pub(super) struct Walk {
    pub(super) segments: Vec<Segment>,
    pub(super) unrecognized: Option<String>,
}

impl Walk {
    pub(super) fn script(&mut self, source: &str) {
        let tree = parse_tree(source);
        let root = tree.root_node();
        if root.has_error() || !splits_words_like_bash(root, source) {
            self.mark_unrecognized(source);
        }
        self.visit(root, source);
    }

    fn mark_unrecognized(&mut self, text: &str) {
        self.unrecognized
            .get_or_insert_with(|| text.chars().take(40).collect());
    }

    fn visit(&mut self, node: Node<'_>, src: &str) {
        match node.kind() {
            "program" | "list" | "pipeline" => self.visit_children(node, src),
            "comment" => {}
            "command" => self.command(node, src),
            _ => {
                self.mark_unrecognized(&src[node.byte_range()]);
                self.visit_children(node, src);
            }
        }
    }

    fn visit_children(&mut self, node: Node<'_>, src: &str) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child, src);
        }
    }

    fn command(&mut self, command: Node<'_>, src: &str) {
        let mut named = Vec::new();
        let mut nodes = Vec::new();
        for part in command_parts(command) {
            match part.kind() {
                "variable_assignment" => named.push(assignment_word(part, src)),
                "command_name" => {
                    if let Some(name) = part.named_child(0) {
                        nodes.push(name);
                    }
                }
                "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                    self.mark_unrecognized(&src[part.byte_range()]);
                }
                _ => nodes.push(part),
            }
        }
        let program = named.len();
        named.extend(words(&nodes, src).iter().map(|word| word.shell_word(src)));
        if named.iter().any(|word| !word.literal) {
            self.mark_unrecognized(&src[command.byte_range()]);
        }
        if program < named.len() {
            self.segments.push(Segment { named, program });
        } else {
            self.mark_unrecognized(&src[command.byte_range()]);
        }
        self.nested_commands(command, src);
    }

    fn nested_commands(&mut self, node: Node<'_>, src: &str) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "command" => self.visit(child, src),
                "simple_expansion"
                | "expansion"
                | "command_substitution"
                | "process_substitution" => {
                    self.mark_unrecognized(&src[child.byte_range()]);
                    self.nested_commands(child, src);
                }
                _ => self.nested_commands(child, src),
            }
        }
    }
}

/// The children of `command` that make up its simple command: assignments, name, redirections, and
/// arguments, including an argument the grammar reads as an anonymous token, such as a bare `==`,
/// which bash passes as a word.
pub(super) fn command_parts(command: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = command.walk();
    command
        .children(&mut cursor)
        .zip(0..)
        .filter(|&(part, index)| {
            part.is_named() || command.field_name_for_child(index) == Some("argument")
        })
        .map(|(part, _)| part)
        .collect()
}

/// The words bash reads from `nodes`, in source order, joining the parts of one word that the
/// grammar splits where only backslash escapes separate them.
pub(super) fn words(nodes: &[Node<'_>], src: &str) -> Vec<Word> {
    nodes
        .chunk_by(|left, right| Gap::between(*left, *right, src).joins())
        .map(|parts| Word::new(parts, src))
        .collect()
}

/// What separates two tokens tree-sitter-bash parses apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gap {
    /// Nothing.
    Empty,
    /// Unescaped spaces, tabs, or newlines, with any line continuations, which end a word.
    Blank,
    /// Backslash escapes of characters other than a newline, which bash reads as part of a word.
    Escapes,
    /// Line continuations, which bash removes before it reads any token, so the tokens abut.
    Continuations,
    /// Anything else, such as an escape beside a blank or a mix of escapes and continuations.
    Other,
}

impl Gap {
    fn between(left: Node<'_>, right: Node<'_>, src: &str) -> Self {
        Self::of(&src[left.end_byte()..right.start_byte()])
    }

    fn of(text: &str) -> Self {
        let (mut blank, mut escape, mut continuation) = (false, false, false);
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' | '\n' => blank = true,
                '\\' => match chars.next() {
                    Some('\n') => continuation = true,
                    Some(_) | None => escape = true,
                },
                _ => return Self::Other,
            }
        }
        match (blank, escape, continuation) {
            (false, false, false) => Self::Empty,
            (true, false, _) => Self::Blank,
            (false, true, false) => Self::Escapes,
            (false, false, true) => Self::Continuations,
            (_, true, _) => Self::Other,
        }
    }

    /// Returns whether bash reads the tokens on either side as one word.
    fn joins(self) -> bool {
        matches!(self, Self::Empty | Self::Escapes | Self::Continuations)
    }

    /// The text bash reads in `text`, a gap that [`joins`](Self::joins): each escaped character,
    /// with line continuations removed.
    pub(super) fn value(text: &str) -> String {
        let mut value = String::new();
        let mut chars = text.chars();
        while chars.next().is_some() {
            value.extend(chars.next().filter(|&escaped| escaped != '\n'));
        }
        value
    }
}

/// Returns whether tree-sitter-bash ends every word in the tree under `root` where bash does, so
/// the words a rule sees are the words bash runs. Outside quotes, only blanks and line
/// continuations may separate tokens. Escapes alone may separate only the parts of one command
/// word, which [`words`] joins, and line continuations alone may only follow a control operator,
/// since bash removes them before it reads tokens. A `$` expansion may not span a blank. Command
/// and process substitutions are checked as scripts of their own, even inside quotes.
pub(super) fn splits_words_like_bash(root: Node<'_>, src: &str) -> bool {
    let edge = |text: &str| matches!(Gap::of(text), Gap::Empty | Gap::Blank | Gap::Continuations);
    edge(&src[..root.start_byte()])
        && edge(&src[root.end_byte()..])
        && node_splits_words_like_bash(root, src, false)
}

fn node_splits_words_like_bash(node: Node<'_>, src: &str, quoted: bool) -> bool {
    let quoted = match node.kind() {
        "command_substitution" | "process_substitution" => false,
        "expansion" | "heredoc_body" | "string" | "translated_string" => true,
        _ => quoted,
    };
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    (quoted
        || children
            .windows(2)
            .all(|pair| separates_like_bash(node, pair[0], pair[1], src)))
        && children
            .iter()
            .all(|&child| node_splits_words_like_bash(child, src, quoted))
}

/// Returns whether bash reads the source between the sibling tokens `left` and `right` of `parent`
/// as tree-sitter-bash does.
fn separates_like_bash(parent: Node<'_>, left: Node<'_>, right: Node<'_>, src: &str) -> bool {
    let text = &src[left.end_byte()..right.start_byte()];
    match Gap::of(text) {
        Gap::Empty => true,
        Gap::Blank => parent.kind() != "simple_expansion",
        Gap::Escapes => parent.kind() == "command" && is_word_part(left) && is_word_part(right),
        Gap::Continuations => {
            matches!(left.kind(), "&" | "&&" | ";" | "|" | "|&" | "||") && right.is_named()
        }
        Gap::Other => parent.kind() == "redirected_statement" && is_dropped_dash(text),
    }
}

/// Returns whether `part`, a child of a command, is its name or an argument.
fn is_word_part(part: Node<'_>) -> bool {
    !matches!(
        part.kind(),
        "variable_assignment" | "file_redirect" | "herestring_redirect" | "heredoc_redirect"
    )
}

/// Returns whether `gap` holds only the lone `-` word tree-sitter-bash drops from the end of a
/// command before a numbered redirection or a heredoc, as in `python3 - <<EOF`, between blanks.
fn is_dropped_dash(gap: &str) -> bool {
    let blank = |c: char| c == ' ' || c == '\t';
    gap.starts_with(blank) && gap.ends_with(blank) && gap.trim_matches(blank) == "-"
}

pub(super) fn parse_tree(source: &str) -> Tree {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .expect("the bundled bash grammar matches the tree-sitter ABI");
    parser
        .parse(source, None)
        .expect("parsing stops early only on a timeout or cancellation, and neither is set")
}

/// The last component of a command path, so `/bin/sh` counts as `sh`.
pub(super) fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}
