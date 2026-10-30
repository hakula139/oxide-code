//! Reads shell words with quote removal, expansion, and literal-value tracking.

use std::iter::Peekable;
use std::ops::Range;
use std::str::Chars;

use tree_sitter::Node;

use super::ShellWord;
use super::walk::Gap;
use super::wrapper::is_identifier;
use crate::util::path::expand_user;

/// One word of a simple command.
pub(super) struct Word {
    /// Where the word is written in the source.
    pub(super) span: Range<usize>,
    /// The value the shell gives the word after tilde expansion and quote removal, or `None` when
    /// part of it expands at run time.
    pub(super) value: Option<String>,
    /// Whether an unquoted `*`, `?`, `[`, or `{` leaves the word to pathname or brace expansion.
    pub(super) glob: bool,
    /// Whether bash may expand the word into other than one word, as [`splits`] decides.
    pub(super) splits: bool,
}

impl Word {
    /// The word bash reads from `parts`, which the grammar may split one word into with no blank
    /// between them, as in `=="x"`.
    pub(super) fn new(parts: &[Node<'_>], src: &str) -> Self {
        let (value, glob) = unquote_parts(parts.iter().copied(), src, true)
            .filter(|(value, _)| pwd_bounded(parts[0], src, value))
            .unzip();
        Self {
            span: parts[0].start_byte()..parts[parts.len() - 1].end_byte(),
            value,
            glob: glob.unwrap_or(false),
            splits: parts.iter().any(|&part| splits(part, src)),
        }
    }

    /// The literal word an anonymous token spells, such as a declaration's keyword or a test's
    /// bracket.
    pub(super) fn token(node: Node<'_>, src: &str) -> Self {
        Self {
            span: node.byte_range(),
            value: Some(src[node.byte_range()].to_owned()),
            glob: false,
            splits: false,
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

/// A leading `NAME=VALUE` assignment as a word, with its value unquoted and a leading `~` expanded.
/// Neither pathname nor brace expansion applies to an assignment, so only a value or a subscript
/// that expands keeps it from being literal.
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
    match unquote(value, src, true).filter(|(unquoted, _)| pwd_bounded(value, src, unquoted)) {
        Some((unquoted, _)) => ShellWord {
            text: format!("{}{unquoted}", &src[node.start_byte()..value.start_byte()]),
            literal: true,
        },
        None => written(),
    }
}

/// `node`'s value and whether it globs, for [`Word`]. Tilde expansion and a [`DirVariable`]
/// expansion apply to a `leading` word alone.
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
        "simple_expansion" | "expansion" if leading => {
            Some((dir_prefix(node, src)?.0.value()?, false))
        }
        "string" => {
            let dir = dir_prefix(node, src).filter(|_| leading);
            let mut cursor = node.walk();
            if !node
                .named_children(&mut cursor)
                .skip(usize::from(dir.is_some()))
                .all(|part| part.kind() == "string_content")
            {
                return None;
            }
            let (prefix, start) = match dir {
                Some((dir, end)) => (dir.value()?, end),
                None => (String::new(), node.start_byte() + 1),
            };
            let inner = src.get(start..node.end_byte() - 1)?;
            Some((prefix + &unquote_double(inner), false))
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
/// unquoted parts are read as one, with the escapes and line continuations between them, so a
/// tilde prefix spans them. A leading tilde expands only when its unquoted run holds the `/` that
/// ends the prefix or ends the word, since bash leaves a prefix with a quoted character as written.
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

/// A variable that names a directory. The analysis resolves its plain expansion, such as `$HOME`
/// or `${PWD}`, at the start of a word, and `PWD`'s only where [`pwd_bounded`] holds.
#[derive(Clone, Copy)]
enum DirVariable {
    /// `HOME`, which bash reads from the environment as the analysis does.
    Home,
    /// `PWD`, which bash keeps current across directory changes, so it names the working
    /// directory as `.` does.
    Pwd,
}

impl DirVariable {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "$HOME" | "${HOME}" => Some(Self::Home),
            "$PWD" | "${PWD}" => Some(Self::Pwd),
            _ => None,
        }
    }

    /// The directory the variable expands to, or `None` without a home directory.
    fn value(self) -> Option<String> {
        match self {
            Self::Home => home(),
            Self::Pwd => Some(".".to_owned()),
        }
    }
}

/// The [`DirVariable`] whose plain expansion starts the word part `node`, bare or first in a
/// double-quoted string, with the byte where that expansion ends.
fn dir_prefix(node: Node<'_>, src: &str) -> Option<(DirVariable, usize)> {
    match node.kind() {
        "simple_expansion" | "expansion" => Some((
            DirVariable::parse(&src[node.byte_range()])?,
            node.end_byte(),
        )),
        "string" => node
            .named_child(0)
            .filter(|part| {
                part.start_byte() == node.start_byte() + 1
                    && matches!(part.kind(), "simple_expansion" | "expansion")
            })
            .and_then(|part| dir_prefix(part, src)),
        "concatenation" => dir_prefix(node.child(0)?, src),
        _ => None,
    }
}

/// Returns whether `value`, the word that the part `first` leads, names what bash expands it to.
/// The `.` a leading `$PWD` stands for does only when the expansion ends the word or a `/`
/// follows it, as bash requires of `~+`, since `$PWD.log` names a sibling of the directory.
fn pwd_bounded(first: Node<'_>, src: &str, value: &str) -> bool {
    !matches!(dir_prefix(first, src), Some((DirVariable::Pwd, _)))
        || value == "."
        || value.starts_with("./")
}

/// The home directory a leading `~` expands to, or `None` without one.
pub(super) fn home() -> Option<String> {
    Some(expand_user("~").ok()?.to_string_lossy().into_owned())
}

/// An unquoted word's value, with backslash escapes removed and the tilde [`tilde_at`] finds
/// expanded by [`expand_tilde`].
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
    let joined = text.replace("\\\n", "");
    if leading && let Some(at) = tilde_at(&joined) {
        let tail = &joined[at + 1..];
        let name = &tail[..tail.find('/').unwrap_or(tail.len())];
        if name.contains(['*', '?', '[', '{']) {
            return None;
        }
        if !name.contains('\\') {
            value = value[..at].to_owned() + &expand_tilde(&value[at..])?;
        }
    }
    Some((value, glob))
}

/// Where the tilde bash may expand in `word` stands: leading it, or after the first `=` of a word
/// that reads as an assignment, as in `env BASH_ENV=~/x`.
fn tilde_at(word: &str) -> Option<usize> {
    if word.starts_with('~') {
        return Some(0);
    }
    let (name, value) = word.split_once('=')?;
    (is_identifier(name) && value.starts_with('~')).then_some(name.len() + 1)
}

/// `path` with a leading tilde prefix expanded as bash expands it, or `None` for a user the
/// analysis cannot find. A [`stack_tilde`] stays as written, since the directory it names depends
/// on the changes before it, which [`super::walk::Walk::spellings`] weighs.
pub(super) fn expand_tilde(path: &str) -> Option<String> {
    let Some(tail) = path.strip_prefix('~') else {
        return Some(path.to_owned());
    };
    let name = &tail[..tail.find('/').unwrap_or(tail.len())];
    Some(match name {
        "" => expand_user(path).ok()?.to_string_lossy().into_owned(),
        "+" => format!(".{}", &path[2..]),
        _ if stack_tilde(path).is_some() => path.to_owned(),
        _ => user_home(name)? + &path[1 + name.len()..],
    })
}

/// The rest of `path` after a leading tilde that names a directory on the stack: `~-`, the
/// previous directory, or `~N`, `~+N`, or `~-N`, the stack's Nth entry. Each names the working
/// directory, a directory a `cd` or `pushd` entered, or one a `pushd -n` stacked, or else stays
/// as written, since `ox` starts `bash` without `OLDPWD` and with only the working directory on
/// the stack.
pub(super) fn stack_tilde(path: &str) -> Option<&str> {
    let tail = path.strip_prefix('~')?;
    let end = tail.find('/').unwrap_or(tail.len());
    let (name, rest) = tail.split_at(end);
    let index = name.strip_prefix(['+', '-']).unwrap_or(name);
    let stack = name == "-" || !index.is_empty() && index.chars().all(|c| c.is_ascii_digit());
    stack.then_some(rest)
}

/// The home directory of the user `name`, as bash's `~name` finds it.
#[cfg(unix)]
fn user_home(name: &str) -> Option<String> {
    let user = nix::unistd::User::from_name(name).ok()??;
    Some(user.dir.to_string_lossy().into_owned())
}

/// The home directory of the user `name`, which the analysis does not look up off Unix.
#[cfg(not(unix))]
fn user_home(_name: &str) -> Option<String> {
    None
}

/// Returns whether bash may expand the word part `node` into other than one word: by splitting an
/// unquoted expansion or substitution, by pathname expansion, or by brace expansion, which any
/// unquoted `{` but the literal `{}` may open.
fn splits(node: Node<'_>, src: &str) -> bool {
    match node.kind() {
        "arithmetic_expansion"
        | "brace_expression"
        | "command_substitution"
        | "expansion"
        | "simple_expansion" => true,
        "concatenation" => {
            let mut cursor = node.walk();
            node.children(&mut cursor).any(|part| splits(part, src))
        }
        "word" => {
            let mut chars = src[node.byte_range()].chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '*' | '?' | '[' => return true,
                    '{' if chars.peek() != Some(&'}') => return true,
                    _ => {}
                }
            }
            false
        }
        _ => false,
    }
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
            'c' => char::from_u32(u32::from(chars.next()?) & 0x1f)?,
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
