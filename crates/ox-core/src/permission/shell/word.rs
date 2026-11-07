//! Reads shell words with quote removal, expansion, and literal-value tracking.

use std::iter::Peekable;
use std::ops::Range;
use std::str::Chars;

use tree_sitter::Node;

use super::ShellWord;
use super::walk::Gap;

/// One word of a simple command.
pub(super) struct Word {
    /// Where the word is written in the source.
    pub(super) span: Range<usize>,
    /// The value after quote removal, or `None` when part expands at run time.
    pub(super) value: Option<String>,
    /// Whether an unquoted `*`, `?`, `[`, or `{` leaves the word to pathname or brace expansion.
    pub(super) glob: bool,
}

impl Word {
    /// The word bash reads from `parts`, which the grammar may split one word into with no blank
    /// between them, as in `=="x"`.
    pub(super) fn new(parts: &[Node<'_>], src: &str) -> Self {
        let (value, glob) = unquote_parts(parts.iter().copied(), src, true).unzip();
        Self {
            span: parts[0].start_byte()..parts[parts.len() - 1].end_byte(),
            value,
            glob: glob.unwrap_or(false),
        }
    }

    /// The word's value when it is known and leaves nothing to pathname or brace expansion.
    pub(super) fn literal(&self) -> Option<&str> {
        self.value.as_deref().filter(|_| !self.glob)
    }

    pub(super) fn shell_word(&self, src: &str) -> ShellWord {
        ShellWord {
            text: self
                .value
                .clone()
                .unwrap_or_else(|| src[self.span.clone()].to_owned()),
            literal: self.literal().is_some(),
        }
    }
}

/// A leading `NAME=VALUE` assignment after quote removal. Expanding values and subscripts stay
/// dynamic. Pathname and brace expansion do not apply to assignment values.
pub(super) fn assignment_word(node: Node<'_>, src: &str) -> ShellWord {
    let written = || ShellWord {
        text: src[node.byte_range()].to_owned(),
        literal: false,
    };
    if node
        .child_by_field_name("name")
        .is_none_or(|name| name.kind() != "variable_name")
    {
        return written();
    }
    let Some(value) = node.child_by_field_name("value") else {
        return ShellWord {
            text: src[node.byte_range()].to_owned(),
            literal: true,
        };
    };
    match unquote(value, src, true) {
        Some((unquoted, _)) => ShellWord {
            text: format!("{}{unquoted}", &src[node.start_byte()..value.start_byte()]),
            literal: true,
        },
        None => written(),
    }
}

/// The literal value and glob flag of a word part.
fn unquote(node: Node<'_>, src: &str, leading: bool) -> Option<(String, bool)> {
    let text = &src[node.byte_range()];
    match node.kind() {
        "word" => unquote_word(text, leading),
        "!" | "!=" | "=" | "==" | "=~" | "number" | "test_operator" | "variable_name" => {
            Some((text.to_owned(), false))
        }
        "variable_assignment" => {
            let word = assignment_word(node, src);
            word.literal.then_some((word.text, false))
        }
        "raw_string" => Some((text.get(1..text.len() - 1)?.to_owned(), false)),
        "ansi_c_string" => Some((unquote_ansi_c(text.get(2..text.len() - 1)?)?, false)),
        "string" => {
            let mut cursor = node.walk();
            if !node
                .named_children(&mut cursor)
                .all(|part| part.kind() == "string_content")
            {
                return None;
            }
            let inner = src.get(node.start_byte() + 1..node.end_byte() - 1)?;
            Some((unquote_double(inner), false))
        }
        "concatenation" => {
            let mut cursor = node.walk();
            unquote_parts(node.children(&mut cursor), src, leading)
        }
        _ => None,
    }
}

/// The value and glob flag of the one word `parts` spell in order, each read as [`unquote`] reads
/// it and joined by the [`Gap::value`] between them, or `None` when any part has none. Adjacent
/// unquoted parts share escape removal. Bash leaves a tilde prefix with a quoted suffix literal.
fn unquote_parts<'t>(
    parts: impl IntoIterator<Item = Node<'t>>,
    src: &str,
    leading: bool,
) -> Option<(String, bool)> {
    let parts: Vec<Node<'t>> = parts.into_iter().collect();
    let mut value = String::new();
    let mut glob = false;
    let mut previous: Option<Node<'t>> = None;
    let mut runs = parts
        .chunk_by(|left, right| left.kind() == "word" && right.kind() == "word")
        .peekable();
    while let Some(run) = runs.next() {
        let (first, last) = (run[0], run[run.len() - 1]);
        if let Some(previous) = previous {
            value.push_str(&Gap::value(&src[previous.end_byte()..first.start_byte()]));
        }
        let leading = leading && previous.is_none();
        let (text, run_glob) = if first.kind() == "word" {
            let text = &src[first.start_byte()..last.end_byte()];
            let quoted_prefix =
                text.starts_with('~') && !text.contains('/') && runs.peek().is_some();
            unquote_word(text, leading && !quoted_prefix)?
        } else {
            unquote(first, src, leading)?
        };
        value.push_str(&text);
        glob |= run_glob;
        previous = Some(last);
    }
    Some((value, glob))
}

fn unquote_word(text: &str, leading: bool) -> Option<(String, bool)> {
    let mut value = String::with_capacity(text.len());
    let mut glob = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => value.extend(chars.next().filter(|&next| next != '\n')),
            '*' | '?' | '[' | '{' => {
                glob = true;
                value.push(c);
            }
            _ => value.push(c),
        }
    }
    if leading && tilde_at(&text.replace("\\\n", "")).is_some() {
        return None;
    }
    Some((value, glob))
}

fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn tilde_at(word: &str) -> Option<usize> {
    if word.starts_with('~') {
        return Some(0);
    }
    let (name, value) = word.split_once('=')?;
    (is_identifier(name) && value.starts_with('~')).then_some(name.len() + 1)
}

/// A double-quoted string's value: a backslash escapes only `$`, `` ` ``, `"`, `\`, and newlines.
fn unquote_double(inner: &str) -> String {
    let mut value = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&'\n')) => _ = chars.next(),
            ('\\', Some(&next @ ('$' | '`' | '"' | '\\'))) => {
                value.push(next);
                chars.next();
            }
            _ => value.push(c),
        }
    }
    value
}

/// A `$'...'` string's value, with its escapes decoded as bash does, or `None` when an escape names
/// no character, names a lone byte past ASCII, or names NUL, where bash cuts the word short.
fn unquote_ansi_c(inner: &str) -> Option<String> {
    let mut value = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            value.push(c);
            continue;
        }
        let escape = chars.next()?;
        let decoded = match escape {
            'a' => '\u{7}',
            'b' => '\u{8}',
            'e' | 'E' => '\u{1b}',
            'f' => '\u{c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\u{b}',
            '\\' | '\'' | '"' | '?' => escape,
            '0'..='7' => {
                let first = escape.to_digit(8)?;
                let code = take_digits(&mut chars, 8, 2)
                    .map_or(first, |(rest, width)| first * 8_u32.pow(width) + rest);
                char::from_u32(code & 0xff).filter(char::is_ascii)?
            }
            'x' => char::from_u32(take_digits(&mut chars, 16, 2)?.0).filter(char::is_ascii)?,
            'u' => char::from_u32(take_digits(&mut chars, 16, 4)?.0)?,
            'U' => char::from_u32(take_digits(&mut chars, 16, 8)?.0)?,
            'c' => match chars.next()? {
                '?' => '\u{7f}',
                control => char::from_u32(u32::from(control) & 0x1f)?,
            },
            other => {
                value.push('\\');
                other
            }
        };
        if decoded == '\0' {
            return None;
        }
        value.push(decoded);
    }
    Some(value)
}

/// Up to `max` digits in `radix` from the front of `chars`, as their value and their count, or
/// `None` when none leads.
fn take_digits(chars: &mut Peekable<Chars<'_>>, radix: u32, max: usize) -> Option<(u32, u32)> {
    let mut taken = None;
    for _ in 0..max {
        let Some(digit) = chars.peek().and_then(|c| c.to_digit(radix)) else {
            break;
        };
        chars.next();
        let (value, count) = taken.unwrap_or((0, 0));
        taken = Some((value * radix + digit, count + 1));
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    mod unquote_ansi_c {
        use super::*;

        #[test]
        fn decodes_each_escape_bash_does() {
            for (inner, value) in [
                ("push", "push"),
                (r"pu\x73h", "push"),
                (r"\162m", "rm"),
                (r"\55rf", "-rf"),
                (r"\0101", "\u{8}1"),
                (r"\u002drf", "-rf"),
                (r"\U0000002drf", "-rf"),
                (r"\u00e9", "\u{e9}"),
                (r"a\tb\n\r\v\f\a\b", "a\tb\n\r\u{b}\u{c}\u{7}\u{8}"),
                (r"\e[0m\E", "\u{1b}[0m\u{1b}"),
                (r#"\'\"\\\?"#, r#"'"\?"#),
                (r"\cA", "\u{1}"),
                (r"\c?", "\u{7f}"),
                (r"\q", r"\q"),
            ] {
                assert_eq!(unquote_ansi_c(inner).as_deref(), Some(value), "{inner:?}");
            }
        }

        #[test]
        fn declines_an_escape_that_names_no_ascii_character_or_nul() {
            for inner in [
                r"\x80", r"\200", r"\xZ", r"\0", r"\400", r"a\x00b", r"\c", "a\\",
            ] {
                assert_eq!(unquote_ansi_c(inner), None, "{inner:?}");
            }
        }
    }
}
