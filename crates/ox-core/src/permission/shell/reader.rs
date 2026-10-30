//! Classifies read-only commands and resolves the files their arguments read.

use std::iter::Peekable;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::str::Chars;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;

use super::walk::FIND_ACTIONS;
use super::word::Word;
use super::wrapper::{Value, sets_only_tracing_options};
use super::{Operand, ReaderPath};

/// Commands whose file operands the gate weighs against `read` rules, whether or not they run as
/// [known-safe](safe_operands) reads.
pub(super) const READERS: &[&str] = &["cat", "find", "grep", "head", "ls", "sed", "tail", "wc"];

/// Files one globbing word may match before the analysis gives up on listing them.
const MAX_GLOB_MATCHES: usize = 1024;

/// A `sed` script of commands that only print, delete, substitute, or move text between buffers. It
/// leaves out `r`, `w`, and `e`, and the `w` and `e` substitution flags, which read a file, write
/// one, or run a command.
static SAFE_SED_SCRIPT: LazyLock<Regex> = LazyLock::new(|| {
    let text = r"(?:[^/\\\n]|\\.)*";
    let address = format!(r"(?:\d+(?:~\d+)?|\$|/{text}/I?)");
    let command = format!(
        r"(?:{address}(?:\s*,\s*{address})?\s*!?\s*)?(?:[=dDgGhHlnNpPqQxz]|s/{text}/{text}/[gpiIm0-9]*|y/{text}/{text}/)"
    );
    Regex::new(&format!(
        r"^\s*(?:{command}\s*(?:[;\n]\s*{command}\s*)*)?;?\s*$"
    ))
    .expect("the sed script pattern is valid")
});

// ── Known-Safe Commands ──

/// The operators and the closing `]` of a `test` or `[` call.
const TEST_OPERATORS: &[&str] = &["!", "!=", "(", ")", "<", "=", "==", ">", "]"];

/// The words that may name a file read by the shipped known-safe read-only command `name`, or
/// `None` when `name` is no such command or `args` take it out of the safe set. Every argument must
/// be literal and free of globs, so each file it reads is one the gate can check. Every word of a
/// `test` or `[` call other than an option or a [`TEST_OPERATORS`] word counts as a file, so a
/// string it compares reads as one too.
pub(super) fn safe_operands(name: &str, args: &[Word]) -> Option<Vec<Operand>> {
    let values = args.iter().map(Word::literal).collect::<Option<Vec<_>>>()?;
    let files = || {
        let known: Vec<Option<&str>> = values.iter().copied().map(Some).collect();
        let operands = reader_args(name, &known);
        (!operands.unknown).then(|| operands.reads(None).collect())
    };
    match name {
        ":" | "echo" | "export" | "false" | "printf" | "pwd" | "true" => Some(Vec::new()),
        "set" => sets_only_tracing_options(args).then(Vec::new),
        "[" | "test" => Some(
            values
                .iter()
                .filter(|value| !value.starts_with('-') && !TEST_OPERATORS.contains(value))
                .map(|&value| Operand::possible(ReaderPath::exact(value.to_owned())))
                .collect(),
        ),
        "cat" | "head" | "ls" | "tail" | "wc" => files(),
        "find" if !values.iter().any(|value| FIND_ACTIONS.contains(value)) => files(),
        "grep" if !values.iter().any(|value| is_recursive_grep(value)) => files(),
        "sed" => sed_files(&values).map(|files| files.into_iter().map(Operand::certain).collect()),
        _ => None,
    }
}

/// Returns whether a `grep` argument makes it read every file under a directory. `--directories`
/// and `-d` count whatever their value, since grep accepts `rec` for `recurse`.
fn is_recursive_grep(arg: &str) -> bool {
    match long_option(arg) {
        Some(name) => ["dereference-recursive", "directories", "recursive"]
            .iter()
            .any(|option| option.starts_with(name)),
        None => arg.starts_with('-') && arg.contains(['d', 'r', 'R']),
    }
}

/// The files a `sed` call reads, or `None` when it may edit a file in place, read a script file, or
/// run a script [`SAFE_SED_SCRIPT`] does not accept. An option after an operand is `None` too,
/// since BSD `sed` reads it as a file and may then take the operand before it as the script. A `--`
/// there is a file to BSD and ends the options for GNU, which read the same script.
fn sed_files(values: &[&str]) -> Option<Vec<String>> {
    let mut scripts = Vec::new();
    let mut files = Vec::new();
    let mut args = values.iter().copied();
    while let Some(arg) = args.next() {
        if !files.is_empty() && arg.starts_with('-') && !matches!(arg, "-" | "--") {
            return None;
        }
        if arg == "--" {
            if !files.is_empty() {
                files.push(arg);
            }
            files.extend(args.by_ref());
        } else if let Some(name) = long_option(arg) {
            if ["file", "in-place"]
                .iter()
                .any(|option| option.starts_with(name))
            {
                return None;
            }
            if "expression".starts_with(name) {
                match arg.split_once('=') {
                    Some((_, script)) => scripts.push(script),
                    None => scripts.push(args.next()?),
                }
            }
        } else if let Some(cluster) = arg.strip_prefix('-').filter(|c| !c.is_empty()) {
            if cluster.contains(['I', 'f', 'i']) {
                return None;
            }
            if let Some(at) = cluster.find('e') {
                let attached = &cluster[at + 1..];
                scripts.push(if attached.is_empty() {
                    args.next()?
                } else {
                    attached
                });
            }
        } else {
            files.push(arg);
        }
    }
    if scripts.is_empty() && !files.is_empty() {
        scripts.push(files.remove(0));
    }
    let safe = !scripts.is_empty()
        && scripts
            .iter()
            .all(|script| SAFE_SED_SCRIPT.is_match(script));
    safe.then(|| files.into_iter().map(str::to_owned).collect())
}

/// The files a `sed` script reads and writes and whether it runs a command. It follows the
/// commands of the `dialect` build: `r` and GNU's `R` read the file named by the rest of the line,
/// `w`, GNU's `W`, and the `s///w` flag write one, and GNU's `e` and `s///e` flag run a command.
/// GNU's commands cover BSD's, so a BSD script parses the same in GNU. Like [`SAFE_SED_SCRIPT`], it
/// ends a regular expression at the first unescaped delimiter.
///
/// # Errors
///
/// For a script the parser cannot read, which may do anything, it returns the files written by
/// the commands before the unreadable part, since both builds open them while compiling the script.
pub(super) fn sed_script(script: &str, dialect: Dialect) -> Result<SedScript, Vec<String>> {
    let mut parsed = SedScript::default();
    match read_sed_script(script, dialect, &mut parsed) {
        Some(()) => Ok(parsed),
        None => Err(parsed.writes),
    }
}

/// Adds what `script` does to `parsed`, as [`sed_script`] reads it, stopping with `None` at the
/// first part it cannot read.
fn read_sed_script(script: &str, dialect: Dialect, parsed: &mut SedScript) -> Option<()> {
    let gnu = dialect == Dialect::Gnu;
    let mut chars = script.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace() || *c == ';').is_some() {}
        let Some(&next) = chars.peek() else {
            return Some(());
        };
        if next == '#' {
            rest_of_line(&mut chars);
            continue;
        }
        if sed_address(&mut chars)? {
            skip_blanks(&mut chars);
            if chars.next_if_eq(&',').is_some() {
                skip_blanks(&mut chars);
                if chars.next_if(|c| matches!(c, '+' | '~')).is_some() {
                    skip_number(&mut chars)?;
                } else if !sed_address(&mut chars)? {
                    return None;
                }
            }
        }
        skip_blanks(&mut chars);
        while chars.next_if_eq(&'!').is_some() {
            skip_blanks(&mut chars);
        }
        match chars.next()? {
            'e' | 'F' | 'L' | 'Q' | 'R' | 'T' | 'v' | 'W' | 'z' if !gnu => return None,
            '{' => continue,
            '}' | '=' | 'd' | 'D' | 'F' | 'g' | 'G' | 'h' | 'H' | 'n' | 'N' | 'p' | 'P' | 'x'
            | 'z' => {}
            'l' | 'L' | 'q' | 'Q' => {
                skip_blanks(&mut chars);
                while chars.next_if(char::is_ascii_digit).is_some() {}
            }
            ':' | 'b' | 't' | 'T' | 'v' => {
                while chars.next_if(|c| !matches!(c, ';' | '\n')).is_some() {}
            }
            'a' | 'i' | 'c' => text_lines(&mut chars),
            command @ ('r' | 'R' | 'w' | 'W') => {
                let file = file_name(&mut chars)?;
                if matches!(command, 'r' | 'R') {
                    parsed.reads.push(file);
                } else {
                    parsed.writes.push(file);
                }
                continue;
            }
            'e' => {
                rest_of_line(&mut chars);
                parsed.runs = true;
                continue;
            }
            's' => {
                let delimiter = sed_delimiter(&mut chars)?;
                sed_text(&mut chars, delimiter)?;
                sed_text(&mut chars, delimiter)?;
                loop {
                    match chars.peek() {
                        Some('g' | 'p' | 'i' | 'I' | 'm' | 'M' | '0'..='9') => {
                            chars.next();
                        }
                        Some('e') if gnu => {
                            chars.next();
                            parsed.runs = true;
                        }
                        Some('w') => {
                            chars.next();
                            parsed.writes.push(file_name(&mut chars)?);
                            break;
                        }
                        _ => break,
                    }
                }
            }
            'y' => {
                let delimiter = sed_delimiter(&mut chars)?;
                sed_text(&mut chars, delimiter)?;
                sed_text(&mut chars, delimiter)?;
            }
            _ => return None,
        }
        skip_blanks(&mut chars);
        if chars
            .peek()
            .is_some_and(|c| !matches!(c, ';' | '\n' | '}' | '#'))
        {
            return None;
        }
    }
}

/// What a `sed` script does beyond printing, deleting, substituting, and moving text.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SedScript {
    pub(super) reads: Vec<String>,
    pub(super) writes: Vec<String>,
    pub(super) runs: bool,
}

type SedChars<'a> = Peekable<Chars<'a>>;

fn skip_blanks(chars: &mut SedChars<'_>) {
    while chars.next_if(|c| matches!(c, ' ' | '\t')).is_some() {}
}

fn rest_of_line(chars: &mut SedChars<'_>) -> String {
    let mut line = String::new();
    while let Some(c) = chars.next_if(|c| *c != '\n') {
        line.push(c);
    }
    line
}

fn skip_number(chars: &mut SedChars<'_>) -> Option<()> {
    chars.next_if(char::is_ascii_digit)?;
    while chars.next_if(char::is_ascii_digit).is_some() {}
    Some(())
}

/// The non-empty file name a command takes: the rest of the line after leading blanks.
fn file_name(chars: &mut SedChars<'_>) -> Option<String> {
    skip_blanks(chars);
    Some(rest_of_line(chars)).filter(|file| !file.is_empty())
}

/// Consumes the text of an `a`, `i`, or `c` command: the rest of the line, continued onto the next
/// one while a line ends in an unescaped backslash.
fn text_lines(chars: &mut SedChars<'_>) {
    loop {
        let line = rest_of_line(chars);
        let trailing = line.chars().rev().take_while(|c| *c == '\\').count();
        if trailing % 2 == 0 || chars.next().is_none() {
            return;
        }
    }
}

/// Consumes one address, returning whether there was one: a line number, possibly with `~step`,
/// `$`, or a regular expression between slashes or after `\` with its own delimiter, with its
/// `I` and `M` flags.
fn sed_address(chars: &mut SedChars<'_>) -> Option<bool> {
    match chars.peek() {
        Some('0'..='9') => {
            skip_number(chars)?;
            if chars.next_if_eq(&'~').is_some() {
                skip_number(chars)?;
            }
        }
        Some('$') => {
            chars.next();
        }
        Some('/') => {
            chars.next();
            sed_text(chars, '/')?;
            while chars.next_if(|c| matches!(c, 'I' | 'M')).is_some() {}
        }
        Some('\\') => {
            chars.next();
            let delimiter = sed_delimiter(chars)?;
            sed_text(chars, delimiter)?;
            while chars.next_if(|c| matches!(c, 'I' | 'M')).is_some() {}
        }
        _ => return Some(false),
    }
    Some(true)
}

fn sed_delimiter(chars: &mut SedChars<'_>) -> Option<char> {
    chars.next().filter(|c| !matches!(c, '\n' | '\\'))
}

/// Consumes text up to and including the unescaped `delimiter`.
fn sed_text(chars: &mut SedChars<'_>, delimiter: char) -> Option<()> {
    loop {
        match chars.next()? {
            '\\' => {
                chars.next()?;
            }
            c if c == delimiter => return Some(()),
            _ => {}
        }
    }
}

/// The name of a long option such as `--name` or `--name=value`, which GNU tools accept abbreviated
/// to any prefix that names one option.
fn long_option(arg: &str) -> Option<&str> {
    let option = arg.strip_prefix("--")?;
    let name = option.split_once('=').map_or(option, |(name, _)| name);
    (!name.is_empty()).then_some(name)
}

// ── Readers ──

/// The paths reader `name` may read, expanding whole words before assigning their argument roles.
/// A multi-match expansion that can change roles leaves the reads unknown and keeps every possible
/// named operand. Only the words before it, and the literal words after it that are certain with
/// each glob read as one word, as an expanding word is, stay certain. Glob match ordering is not
/// assumed, since bash's locale can change it.
pub(super) fn reader_operands(name: &str, args: &[Word], cwd: &Path) -> (Vec<Operand>, bool) {
    let written: Vec<_> = args.iter().map(|word| word.value.as_deref()).collect();
    let written = reader_args(name, &written);
    let mut anything = written.unknown;
    let mut values = Vec::new();
    let mut origins = Vec::new();
    let mut groups = Vec::new();
    for (index, word) in args.iter().enumerate() {
        if let Some(pattern) = word.value.as_deref().filter(|_| word.glob) {
            match expand_glob(pattern, cwd) {
                Some(matches) if !matches.is_empty() => {
                    if matches.len() > 1 {
                        groups.push((values.len()..values.len() + matches.len(), index));
                    }
                    origins.extend(matches.iter().map(|_| None));
                    values.extend(matches.into_iter().map(Some));
                    continue;
                }
                None => anything = true,
                Some(_) => {}
            }
        }
        origins.push((!word.glob).then_some(index));
        values.push(word.value.clone());
    }
    let values: Vec<_> = values.iter().map(Option::as_deref).collect();
    let (files, unknown) = expanded_reader_paths(name, &written, &values, &origins, &groups);
    (files, anything || unknown)
}

/// The reads of `values`, the expanded words of a reader whose `written` words a glob expanded
/// into, where `origins` names the written word of each value that is not a glob match and each
/// of `groups` spans the matches of a multi-match glob with the index of its written word.
fn expanded_reader_paths(
    name: &str,
    written: &ReaderArgs,
    values: &[Option<&str>],
    origins: &[Option<usize>],
    groups: &[(Range<usize>, usize)],
) -> (Vec<Operand>, bool) {
    let grammar = reader_grammar(name);
    let expanded = reader_args(name, values);
    let ambiguous = groups
        .iter()
        .find(|(range, source)| {
            !written.operands.contains(source)
                || range.clone().any(|index| {
                    !expanded.operands.contains(&index)
                        || values[index].is_some_and(|value| {
                            value.starts_with('-') && expanded.options_at(values, range.start)
                                || grammar.operands == OperandKind::Find
                                    && grammar.find_expression(value)
                        })
                })
        })
        .map(|(range, _)| range.start);
    let anything = written.unknown || expanded.unknown || ambiguous.is_some();
    let late_literals: Vec<usize> = ambiguous.map_or_else(Vec::new, |start| {
        origins[start..].iter().flatten().copied().collect()
    });
    let mut files: Vec<Operand> = expanded
        .reads(ambiguous)
        .chain(written.files.iter().map(|(index, path)| Operand {
            read: path.clone(),
            certain: index.is_some_and(|index| {
                late_literals.contains(&index) && written.opened.contains(&index)
            }),
        }))
        .collect();
    if let Some(start) = ambiguous {
        let recursive = files.iter().any(|operand| operand.read.recursive)
            || grammar.may_recurse(values, start, &expanded);
        for (index, value) in values.iter().enumerate() {
            if index < start && expanded.fixed_values.contains(&index) {
                continue;
            }
            if let Some(path) = value.filter(|path| {
                !path.is_empty() && (grammar.operands != OperandKind::Stream || *path != "-")
            }) {
                files.push(Operand::possible(ReaderPath {
                    path: path.to_owned(),
                    recursive,
                }));
            }
        }
        let consumes_operand = expanded.options_at(values, start)
            && values[start..].iter().flatten().any(|value| {
                reader_args(name, &[Some(value), Some("")])
                    .option_values
                    .contains(&1)
            });
        let expression = grammar.operands == OperandKind::Find
            && values[start..]
                .iter()
                .flatten()
                .any(|value| grammar.find_expression(value));
        if recursive && (consumes_operand || expression) {
            files.push(Operand::possible(ReaderPath {
                path: ".".to_owned(),
                recursive: true,
            }));
        }
    }
    files.sort_unstable_by(|left, right| left.read.path.cmp(&right.read.path));
    files.dedup_by(|next, current| {
        let same = next.read.path == current.read.path;
        if same {
            current.read.recursive |= next.read.recursive;
            current.certain |= next.certain;
        }
        same
    });
    (files, anything)
}

/// What one of [`READERS`] reads, from its arguments.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ReaderArgs {
    /// Each path with the index of its word, or no index for an implicit cwd: each operand but a
    /// leading pattern or script, and each value of an option that names a file.
    files: Vec<(Option<usize>, ReaderPath<String>)>,
    /// Words that supply an option's value in every supported argument order and build.
    fixed_values: Vec<usize>,
    /// Words that are ordinary file operands in every supported argument order and build.
    operands: Vec<usize>,
    /// Words that some supported argument order or build opens as a file.
    opened: Vec<usize>,
    /// Whether some supported argument order or build searches the implicit cwd.
    searches_cwd: bool,
    pub(super) script: ScriptArgs,
    /// Words that supply an option's value in any supported argument order or build.
    option_values: Vec<usize>,
    /// Whether it may read a file the arguments do not name: a word expands, or an option names a
    /// file that lists the files to read.
    unknown: bool,
}

/// Where a reader takes its pattern or script from, and whether it rewrites its files.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ScriptArgs {
    /// Each word that any supported argument order or build takes as the pattern or script.
    pub(super) words: Vec<ScriptWord>,
    /// Whether some supported argument order or build reads the pattern or script from a file.
    pub(super) from_file: bool,
    /// Whether some supported argument order or build rewrites its file operands in place.
    pub(super) in_place: bool,
    /// Whether some supported argument order or build rewrites a symlink's target in place.
    pub(super) follows_links: bool,
    /// Each backup suffix some supported argument order or build gives an in-place rewrite.
    pub(super) backups: Vec<Backup>,
}

/// The backup suffix one build gives an in-place rewrite, from the word at `index`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Backup {
    pub(super) dialect: Dialect,
    pub(super) index: usize,
    /// The suffix, or `None` when its word expands.
    pub(super) suffix: Option<String>,
}

impl Backup {
    /// Where this build moves `file` before rewriting it, unless the suffix expands. GNU's build
    /// replaces each `*` in the suffix with `file` as written, while BSD's, which has no template,
    /// and GNU's for a suffix without `*` append the suffix.
    pub(super) fn path(&self, file: &str) -> Option<String> {
        let suffix = self.suffix.as_deref()?;
        Some(if self.dialect == Dialect::Gnu && suffix.contains('*') {
            suffix.replace('*', file)
        } else {
            format!("{file}{suffix}")
        })
    }
}

/// The word at `index`, with its value or `None` when it expands, that some argument order of the
/// `dialect` build takes as a reader's pattern or script.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ScriptWord {
    pub(super) dialect: Dialect,
    index: usize,
    pub(super) value: Option<String>,
}

impl ReaderArgs {
    /// Each path in `files`, certain when its word is `opened` and comes before the first match
    /// of an `ambiguous` glob, whose matches may shift the roles of later words, or when it is an
    /// implicit cwd some order or build searches and no glob is ambiguous.
    fn reads(&self, ambiguous: Option<usize>) -> impl Iterator<Item = Operand> + '_ {
        self.files.iter().map(move |(index, read)| Operand {
            read: read.clone(),
            certain: match index {
                Some(index) => {
                    self.opened.contains(index) && ambiguous.is_none_or(|start| *index < start)
                }
                None => self.searches_cwd && ambiguous.is_none(),
            },
        })
    }

    fn options_at(&self, values: &[Option<&str>], index: usize) -> bool {
        !values[..index]
            .iter()
            .enumerate()
            .any(|(index, value)| *value == Some("--") && !self.option_values.contains(&index))
    }
}

/// Reads the arguments of reader `name`, given each word's value or `None` for one that expands.
/// The gate cannot tell whether a GNU or a BSD build of `name` runs, or whether getopt stops
/// reading options at the first operand, as BSD and `POSIXLY_CORRECT` make it, so it counts each
/// file any of them would read as one it reads. A word that expands may split into any words, so
/// it leaves what the reader reads unknown wherever it stands, as does `find -files0-from`, which
/// reads the paths to search from a file.
pub(super) fn reader_args(name: &str, values: &[Option<&str>]) -> ReaderArgs {
    let grammar = reader_grammar(name);
    let mut args = ReaderArgs {
        fixed_values: (0..values.len()).collect(),
        operands: (0..values.len()).collect(),
        unknown: values.iter().any(Option::is_none)
            || name == "find" && values.contains(&Some("-files0-from")),
        ..ReaderArgs::default()
    };
    let orders: &[bool] = if grammar.getopt {
        &[true, false]
    } else {
        &[true]
    };
    for dialect in [Dialect::Gnu, Dialect::Bsd] {
        for &permute in orders {
            let read = grammar.read(dialect, permute, values);
            for word in read.script.words {
                if !args.script.words.contains(&word) {
                    args.script.words.push(word);
                }
            }
            args.script.from_file |= read.script.from_file;
            args.script.in_place |= read.script.in_place;
            args.script.follows_links |= read.script.follows_links;
            args.script.backups.extend(read.script.backups);
            args.fixed_values
                .retain(|index| read.option_values.contains(index));
            args.operands.retain(|index| read.operands.contains(index));
            args.opened.extend(read.opened);
            args.searches_cwd |= read.searches_cwd;
            args.files.extend(read.files);
            args.option_values.extend(read.option_values);
            args.unknown |= read.unknown;
        }
    }
    args.files.sort_unstable();
    args.files.dedup_by(|next, current| {
        if next.0 != current.0 || next.1.path != current.1.path {
            return false;
        }
        current.1.recursive |= next.1.recursive;
        true
    });
    args
}

/// A build of the readers, whose option parsing and `sed` commands can differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dialect {
    /// GNU, as on Linux.
    Gnu,
    /// BSD, as macOS ships.
    Bsd,
}

/// A reader's option grammar, matching both its GNU and its BSD build. It lists the options that
/// take a value in either build, so a missing one reads its value as a file, which can only make
/// the gate more cautious. It also lists, as a [`switch`], each option whose full name abbreviates
/// a listed one, since getopt takes an exact name before a prefix. An option one build rejects may
/// stay listed, since the reader then exits before reading.
struct ReaderGrammar {
    options: &'static [ReaderOpt],
    /// Whether it reads its options with getopt, which may stop at the first operand.
    getopt: bool,
    /// Whether `-NUM` sets a count, as in `tail -20`.
    numeric: bool,
    /// Whether the first operand is a pattern or script unless an option gives one.
    pattern: bool,
    operands: OperandKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OperandKind {
    Stream,
    Directory,
    Find,
}

impl ReaderGrammar {
    const NONE: Self = Self {
        options: &[],
        getopt: true,
        numeric: false,
        pattern: false,
        operands: OperandKind::Stream,
    };

    /// The files the `dialect` build reads from `values`, taking options after operands until `--`
    /// when it may `permute` them. A short option that takes a value takes the rest of its cluster
    /// or the next word, and a long one its `=` value or the next word. A long option the grammar
    /// does not list counts its `=` value as a file, and since it may be one that gives the
    /// pattern, the leading operand then counts as a file too.
    fn read(&self, dialect: Dialect, permute: bool, values: &[Option<&str>]) -> ReaderArgs {
        let mut args = ReaderArgs::default();
        let mut operands = Vec::new();
        let mut pattern_given = false;
        let mut recursive = self.operands == OperandKind::Find;
        let mut take = |args: &mut ReaderArgs, role: Role, index: usize, value: Option<&str>| {
            pattern_given |= matches!(role, Role::Pattern | Role::PatternFile);
            args.script.from_file |= matches!(role, Role::PatternFile);
            args.script.in_place |= matches!(role, Role::InPlace);
            args.script.follows_links |= matches!(role, Role::FollowsLinks);
            if matches!(role, Role::InPlace)
                && value != Some("")
                && (value.is_some() || values[index].is_none())
            {
                args.script.backups.push(Backup {
                    dialect,
                    index,
                    suffix: value.map(str::to_owned),
                });
            }
            if matches!(role, Role::Pattern) {
                args.script.words.push(ScriptWord {
                    dialect,
                    index,
                    value: value.map(str::to_owned),
                });
            }
            recursive |= role.recurses(value);
            if value.is_some() {
                args.option_values.push(index);
            }
            args.unknown |= matches!(role, Role::FileList);
            match (role, value) {
                (Role::File | Role::PatternFile | Role::Directory | Role::FileList, Some(file))
                    if !file.is_empty() =>
                {
                    args.files.push((
                        Some(index),
                        ReaderPath {
                            path: file.to_owned(),
                            recursive: matches!(role, Role::Directory),
                        },
                    ));
                }
                _ => {}
            }
        };
        let mut unplaced = false;
        let mut i = 0;
        let mut options = true;
        while let Some(&value) = values.get(i) {
            let index = i;
            i += 1;
            let Some(arg) = value.filter(|arg| options && arg.starts_with('-') && *arg != "-")
            else {
                options &= permute;
                if value != Some("") {
                    operands.push((index, value));
                }
                continue;
            };
            if arg == "--" {
                options = false;
            } else if let Some(long) = arg.strip_prefix("--") {
                let (name, attached) = long
                    .split_once('=')
                    .map_or((long, None), |(name, value)| (name, Some(value)));
                let Some(option) = self.long(name) else {
                    unplaced = true;
                    take(&mut args, Role::File, index, attached);
                    continue;
                };
                let (index, value) = option.argument(dialect, index, attached, values, &mut i);
                take(&mut args, option.role, index, value);
            } else {
                let cluster = &arg[1..];
                if self.numeric && cluster.starts_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
                for (offset, c) in cluster.char_indices() {
                    let Some(option) = self.short(c) else {
                        continue;
                    };
                    if option.value(dialect) == Value::None {
                        take(&mut args, option.role, index, None);
                        continue;
                    }
                    let rest = &cluster[offset + c.len_utf8()..];
                    let attached = (!rest.is_empty()).then_some(rest);
                    let (index, value) = option.argument(dialect, index, attached, values, &mut i);
                    take(&mut args, option.role, index, value);
                    break;
                }
            }
        }
        let leading_pattern = self.pattern && !pattern_given && !unplaced;
        self.place_operands(args, dialect, values, &operands, leading_pattern, recursive)
    }

    /// Completes `args` with the `operands` [`Self::read`] found among `values`, the first of them
    /// the pattern or script when `leading_pattern` holds, and each a tree when `recursive` does.
    fn place_operands(
        &self,
        mut args: ReaderArgs,
        dialect: Dialect,
        values: &[Option<&str>],
        operands: &[(usize, Option<&str>)],
        leading_pattern: bool,
        recursive: bool,
    ) -> ReaderArgs {
        let skip = usize::from(leading_pattern);
        if let Some(&(index, value)) = operands.first().filter(|_| leading_pattern) {
            args.script.words.push(ScriptWord {
                dialect,
                index,
                value: value.map(str::to_owned),
            });
        }
        let tree_given = args.files.iter().any(|(_, path)| path.recursive);
        let paths = self.operand_paths(
            values,
            operands.iter().copied().skip(skip),
            recursive,
            tree_given,
        );
        args.operands
            .extend(paths.iter().filter_map(|(index, _)| *index));
        args.files.extend(paths);
        let tree_end = self.tree_end(values);
        args.opened = args
            .files
            .iter()
            .filter_map(|(index, _)| index.filter(|index| *index < tree_end))
            .collect();
        args.searches_cwd = args.files.iter().any(|(index, _)| index.is_none());
        args.fixed_values.clone_from(&args.option_values);
        args
    }

    /// Where the trees end and, for `find`, its expression starts, whose operands `find` never
    /// opens, such as the value of `-name`.
    fn tree_end(&self, values: &[Option<&str>]) -> usize {
        if self.operands == OperandKind::Find {
            values
                .iter()
                .position(|value| value.is_some_and(|value| self.find_expression(value)))
                .unwrap_or(values.len())
        } else {
            values.len()
        }
    }

    fn may_recurse(&self, values: &[Option<&str>], ambiguous: usize, args: &ReaderArgs) -> bool {
        if !args.options_at(values, ambiguous) {
            return false;
        }
        let recurses = |values: &[Option<&str>]| {
            [Dialect::Gnu, Dialect::Bsd].into_iter().any(|dialect| {
                self.read(dialect, true, values)
                    .files
                    .iter()
                    .any(|(_, path)| path.recursive)
            })
        };
        for (index, value) in values.iter().enumerate() {
            if index < ambiguous && args.fixed_values.contains(&index) {
                continue;
            }
            let next = if index + 1 < ambiguous {
                values[index + 1]
            } else {
                values[ambiguous..]
                    .iter()
                    .copied()
                    .flatten()
                    .find(|value| !value.is_empty() && "recurse".starts_with(value))
            };
            let probe = [*value, next];
            let probe = if next.is_some() {
                &probe[..]
            } else {
                &probe[..1]
            };
            if recurses(probe) {
                return true;
            }
        }
        false
    }

    fn operand_paths<'a>(
        &self,
        values: &[Option<&str>],
        operands: impl Iterator<Item = (usize, Option<&'a str>)>,
        recursive: bool,
        tree_given: bool,
    ) -> Vec<(Option<usize>, ReaderPath<String>)> {
        let operands: Vec<_> = operands.collect();
        let has_operand = !operands.is_empty();
        let tree_end = self.tree_end(values);
        let stdin = self.operands == OperandKind::Stream;
        let mut files: Vec<_> = operands
            .into_iter()
            .filter_map(move |(index, operand)| {
                let path = operand?;
                if stdin && path == "-" {
                    return None;
                }
                Some((
                    Some(index),
                    ReaderPath {
                        path: path.to_owned(),
                        recursive: recursive && index < tree_end,
                    },
                ))
            })
            .collect();
        if recursive
            && (self.operands == OperandKind::Find || !has_operand)
            && !tree_given
            && !files.iter().any(|(_, path)| path.recursive)
        {
            files.push((
                None,
                ReaderPath {
                    path: ".".to_owned(),
                    recursive: true,
                },
            ));
        }
        files
    }

    fn find_expression(&self, arg: &str) -> bool {
        if matches!(arg, "(" | "!") {
            return true;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|_| arg != "--") else {
            return false;
        };
        for c in cluster.chars() {
            let Some(option) = self.short(c) else {
                return true;
            };
            if option.gnu != Value::None || option.bsd != Value::None {
                return false;
            }
        }
        false
    }

    /// The long option `--name`, or the one listed option `name` abbreviates, as getopt accepts. An
    /// abbreviation getopt finds ambiguous among options the grammar leaves out makes the reader
    /// exit before reading.
    fn long(&self, name: &str) -> Option<&ReaderOpt> {
        let longs = || {
            self.options
                .iter()
                .filter_map(|option| Some((option.name.strip_prefix("--")?, option)))
        };
        if let Some((_, option)) = longs().find(|(long, _)| *long == name) {
            return Some(option);
        }
        let mut abbreviated = longs().filter(|(long, _)| long.starts_with(name));
        match (abbreviated.next(), abbreviated.next()) {
            (Some((_, option)), None) => Some(option),
            _ => None,
        }
    }

    /// The single-letter option `-c`.
    fn short(&self, c: char) -> Option<&ReaderOpt> {
        self.options.iter().find(|option| {
            option
                .name
                .strip_prefix('-')
                .is_some_and(|letter| letter.chars().eq([c]))
        })
    }
}

/// The option grammar of reader `name`. Every operand of `find`'s expression counts as a file.
fn reader_grammar(name: &str) -> &'static ReaderGrammar {
    match name {
        "find" => &FIND_GRAMMAR,
        "grep" => &GREP_GRAMMAR,
        "head" => &HEAD_GRAMMAR,
        "ls" => &LS_GRAMMAR,
        "sed" => &SED_GRAMMAR,
        "tail" => &TAIL_GRAMMAR,
        "wc" => &WC_GRAMMAR,
        _ => &ReaderGrammar::NONE,
    }
}

const FIND_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("-D"),
        switch("-E"),
        switch("-H"),
        switch("-L"),
        takes("-O"),
        switch("-P"),
        switch("-X"),
        switch("-d"),
        takes("-f").with(Role::Directory),
        switch("-s"),
        switch("-x"),
    ],
    getopt: false,
    operands: OperandKind::Find,
    ..ReaderGrammar::NONE
};

const GREP_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("-A"),
        takes("-B"),
        takes("-C"),
        takes("-D"),
        takes("-d").with(Role::Directories),
        takes("-e").with(Role::Pattern),
        takes("-f").with(Role::PatternFile),
        takes("-m"),
        switch("-r").with(Role::Recursive),
        switch("-R").with(Role::Recursive),
        takes("--after-context"),
        takes("--before-context"),
        takes("--binary-files"),
        switch("--binary"),
        attached("--color"),
        attached("--colour"),
        takes("--context").bsd(Value::Optional),
        takes("--devices"),
        switch("--dereference-recursive").with(Role::Recursive),
        takes("--directories").with(Role::Directories),
        takes("--exclude"),
        takes("--exclude-dir"),
        takes("--exclude-from").with(Role::File),
        takes("--file").with(Role::PatternFile),
        takes("--group-separator"),
        takes("--include"),
        takes("--include-dir"),
        takes("--label"),
        takes("--max-count"),
        takes("--regexp").with(Role::Pattern),
        switch("--recursive").with(Role::Recursive),
    ],
    numeric: true,
    pattern: true,
    ..ReaderGrammar::NONE
};

const HEAD_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[takes("-c"), takes("-n"), takes("--bytes"), takes("--lines")],
    numeric: true,
    ..ReaderGrammar::NONE
};

const LS_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("-D").bsd_only(),
        takes("-I").gnu_only(),
        switch("-R").with(Role::Recursive),
        takes("-T").gnu_only(),
        takes("-w").gnu_only(),
        takes("--block-size"),
        attached("--color"),
        takes("--format"),
        takes("--hide"),
        attached("--hyperlink"),
        takes("--ignore"),
        takes("--indicator-style"),
        takes("--quoting-style"),
        switch("--recursive").with(Role::Recursive),
        takes("--sort"),
        takes("--tabsize"),
        takes("--time"),
        takes("--time-style"),
        takes("--width"),
    ],
    operands: OperandKind::Directory,
    ..ReaderGrammar::NONE
};

// Mirrors GNU sed 4.10, listing every long option it takes.
const SED_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("-e").with(Role::Pattern),
        takes("-f").with(Role::PatternFile),
        takes("-I").with(Role::InPlace),
        attached("-i").bsd(Value::Required).with(Role::InPlace),
        takes("-l").gnu_only(),
        switch("--binary"),
        switch("--debug"),
        takes("--expression").with(Role::Pattern),
        takes("--file").with(Role::PatternFile),
        switch("--follow-symlinks").with(Role::FollowsLinks),
        switch("--help"),
        attached("--in-place").with(Role::InPlace),
        takes("--line-length"),
        switch("--null-data"),
        switch("--posix"),
        switch("--quiet"),
        switch("--regexp-extended"),
        switch("--sandbox"),
        switch("--separate"),
        switch("--silent"),
        switch("--unbuffered"),
        switch("--version"),
        switch("--zero-terminated"),
    ],
    pattern: true,
    ..ReaderGrammar::NONE
};

const TAIL_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("-b"),
        takes("-c"),
        takes("-n"),
        takes("-s"),
        takes("--blocks"),
        takes("--bytes"),
        attached("--follow"),
        takes("--lines"),
        takes("--max-unchanged-stats"),
        takes("--pid"),
        takes("--sleep-interval"),
    ],
    numeric: true,
    ..ReaderGrammar::NONE
};

const WC_GRAMMAR: ReaderGrammar = ReaderGrammar {
    options: &[
        takes("--files0-from").with(Role::FileList),
        takes("--libxo"),
        takes("--total"),
    ],
    ..ReaderGrammar::NONE
};

/// One reader option, which takes a value in either build unless it is a [`switch`].
#[derive(Clone, Copy)]
struct ReaderOpt {
    name: &'static str,
    /// What the GNU build takes: [`Value::Required`] for a value attached or in the next word,
    /// [`Value::Optional`] for one only attached, or [`Value::None`] for none.
    gnu: Value,
    /// What the BSD build takes, in the same terms.
    bsd: Value,
    role: Role,
}

impl ReaderOpt {
    const fn with(self, role: Role) -> Self {
        Self { role, ..self }
    }

    /// This option taking `value` in the BSD build.
    const fn bsd(self, value: Value) -> Self {
        Self { bsd: value, ..self }
    }

    /// This option as a flag in the BSD build.
    const fn gnu_only(self) -> Self {
        self.bsd(Value::None)
    }

    /// This option as a flag in the GNU build.
    const fn bsd_only(self) -> Self {
        Self {
            gnu: Value::None,
            ..self
        }
    }

    const fn value(&self, dialect: Dialect) -> Value {
        match dialect {
            Dialect::Gnu => self.gnu,
            Dialect::Bsd => self.bsd,
        }
    }

    fn argument<'a>(
        &self,
        dialect: Dialect,
        index: usize,
        attached: Option<&'a str>,
        values: &[Option<&'a str>],
        next: &mut usize,
    ) -> (usize, Option<&'a str>) {
        match (self.value(dialect), attached) {
            (Value::None, _) | (Value::Optional, None) => (index, None),
            (_, Some(value)) => (index, Some(value)),
            (Value::Required, None) => match values.get(*next) {
                Some(&value) => {
                    let index = *next;
                    *next += 1;
                    (index, value)
                }
                None => (index, None),
            },
        }
    }
}

const fn takes(name: &'static str) -> ReaderOpt {
    ReaderOpt {
        name,
        gnu: Value::Required,
        bsd: Value::Required,
        role: Role::Value,
    }
}

const fn attached(name: &'static str) -> ReaderOpt {
    ReaderOpt {
        gnu: Value::Optional,
        bsd: Value::Optional,
        ..takes(name)
    }
}

const fn switch(name: &'static str) -> ReaderOpt {
    ReaderOpt {
        gnu: Value::None,
        bsd: Value::None,
        ..takes(name)
    }
}

/// What a reader option's value names.
#[derive(Clone, Copy)]
enum Role {
    /// Neither a file nor the pattern, such as a count.
    Value,
    /// A file it reads.
    File,
    /// The pattern or script, so no operand is one.
    Pattern,
    /// A file it reads the pattern or script from, so no operand is one.
    PatternFile,
    /// A file that lists the files it reads.
    FileList,
    /// A suffix for the backup of each file it rewrites in place, given or not.
    InPlace,
    /// A flag that rewrites a symlink's target in place.
    FollowsLinks,
    /// A directory supplied through a `find -f` option.
    Directory,
    /// A flag that recursively traverses directory operands.
    Recursive,
    /// A value selecting whether `grep` traverses directories.
    Directories,
}

impl Role {
    fn recurses(self, value: Option<&str>) -> bool {
        match self {
            Self::Recursive => true,
            Self::Directories => {
                value.is_some_and(|value| !value.is_empty() && "recurse".starts_with(value))
            }
            _ => false,
        }
    }
}

/// The filesystem paths matching `pattern` in `cwd`, including brace alternatives. Each component
/// with `*`, `?`, `[`, or `{` matches its directory's entries, skipping dot names unless the
/// component starts with `.` or includes a brace. This does not emit unmatched lexical brace words.
/// `None` when one name matches more than [`MAX_GLOB_MATCHES`] files, fails to compile, or meets a
/// file whose name is not UTF-8.
fn expand_glob(pattern: &str, cwd: &Path) -> Option<Vec<String>> {
    let mut paths = vec![PathBuf::new()];
    for component in Path::new(pattern).components() {
        let name = component.as_os_str().to_string_lossy();
        if !matches!(component, Component::Normal(_)) || !name.contains(['*', '?', '[', '{']) {
            for path in &mut paths {
                path.push(component);
            }
            continue;
        }
        let glob = globset::GlobBuilder::new(&name)
            .literal_separator(true)
            .build()
            .ok()?
            .compile_matcher();
        let dotted = name.starts_with('.') || name.contains('{');
        let mut matched = Vec::new();
        for dir in &paths {
            let Ok(entries) = std::fs::read_dir(cwd.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let entry_name = entry.file_name();
                let entry_name = entry_name.to_str()?;
                if (dotted || !entry_name.starts_with('.')) && glob.is_match(entry_name) {
                    matched.push(dir.join(entry_name));
                    if matched.len() > MAX_GLOB_MATCHES {
                        return None;
                    }
                }
            }
        }
        paths = matched;
    }
    Some(
        paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::Workspace;
    use crate::permission::shell::CommandTarget;
    use crate::permission::shell::local_testing::{is_safe, parse, resolve};
    use crate::util::path::local_testing::canonical_tempdir;
    use crate::util::path::slash_separated;

    mod safe_operands {
        use super::*;

        #[test]
        fn names_every_word_that_may_be_a_file() {
            let command = CommandTarget::parse(
                "grep -fpats --file=more -e x -- -y src",
                &Workspace::new(Path::new("/repo")),
            );
            let at = |name: &str| resolve(name, &Workspace::new(Path::new("/repo")));
            assert!(command.segments[0].is_safe());
            assert_eq!(command.reads, [at("pats"), at("more"), at("-y"), at("src")]);
            assert!(!is_safe("wc --files0-from=list"));
            let echo = parse("echo done");
            assert!(echo.segments[0].is_safe());
            assert_eq!(echo.reads, []);
        }

        #[test]
        fn admits_only_shipped_commands_with_literal_arguments() {
            for command in [
                "cat a.rs",
                "head -n 5 a",
                "ls -la src",
                "wc -l a",
                "pwd",
                "true",
                "echo 'a $b'",
            ] {
                assert!(is_safe(command), "{command:?}");
            }
            for command in [
                "cat *.rs",
                "cat $F",
                "cat {a,b}",
                "echo $HOME *",
                "echo $TOKEN",
                "echo \"${TOKEN}\"",
                "echo *",
                "rm a",
                "./cat a",
                "git status",
            ] {
                assert!(!is_safe(command), "{command:?}");
            }
        }

        #[test]
        fn admits_builtins_that_only_print_test_files_or_trace() {
            let at =
                |name: &str| ReaderPath::exact(resolve(name, &Workspace::new(Path::new("/repo"))));
            for (command, reads) in [
                (": done", &[][..]),
                ("printf '%s\\n' a", &[]),
                ("export RUST_LOG=debug", &[]),
                ("set -eux -o pipefail", &[]),
                ("[ -f Cargo.toml ]", &[at("Cargo.toml")]),
                ("test ! -d /etc -a x = y", &[at("/etc"), at("x"), at("y")]),
            ] {
                let parsed = parse(command);

                assert!(parsed.segments[0].is_safe(), "{command:?}");
                assert_eq!(parsed.segments[0].reads(), reads, "{command:?}");
                assert!(parsed.unrecognized().is_none(), "{command:?}");
            }
            for command in ["set -o posix", "set -a", "[ -f $F ]"] {
                assert!(!is_safe(command), "{command:?}");
            }
        }

        #[test]
        fn guards_find_and_grep() {
            assert!(is_safe("find . -name '*.rs' -type f"));
            for action in FIND_ACTIONS {
                assert!(!is_safe(&format!("find . {action} x")), "{action}");
            }
            assert!(is_safe("grep -n TODO src/main.rs"));
            for command in [
                "grep -r TODO .",
                "grep -Rn TODO .",
                "grep --recursive TODO .",
                "grep -d recurse TODO .",
                "grep --directories=recurse TODO .",
                "grep --rec TODO .",
                "grep --dir=rec TODO .",
                "grep -d rec TODO .",
            ] {
                assert!(!is_safe(command), "{command:?}");
            }
        }
    }

    mod sed_files {
        use super::*;

        #[test]
        fn admits_scripts_that_only_print_or_rewrite_text() {
            for command in [
                "sed -n '1,20p' a.rs",
                "sed 's/foo/bar/g' a.rs",
                "sed -e '/^$/d' -e 's/a\\/b/c/2' a.rs",
                "sed --expression='$p' a.rs",
                "sed --expression '$p' a.rs",
                "sed --quiet 1p a.rs",
                "sed -es/a/b/ a.rs",
                "sed --exp=1p a.rs",
                "sed 1p -- a.rs",
                "sed -ne '3p;5q' a.rs",
                "sed -n -- '1!G;h;$p' a.rs",
            ] {
                assert!(is_safe(command), "{command:?}");
            }
        }

        #[test]
        fn leaves_a_call_unrecognized_when_its_script_or_backup_is_unbounded() {
            for command in [
                "sed -i $X s/a/b/ a.rs",
                "sed -I \"$X\" s/a/b/ a.rs",
                "sed -i bk/* s/a/b/ a.rs",
                "sed --follow-symlinks -i.bak s/a/b/ a.rs",
                "sed -e \"$S\" a.rs",
                "sed k a.rs",
            ] {
                assert!(parse(command).unrecognized().is_some(), "{command:?}");
            }
            for command in [
                "sed -i.bak s/a/b/ a.rs",
                "sed -i 'bk/*' s/a/b/ a.rs",
                "sed --follow-symlinks -i s/a/b/ a.rs",
                "sed -i s/a/b/ README.md",
            ] {
                assert!(parse(command).unrecognized().is_none(), "{command:?}");
            }
        }

        #[test]
        fn refuses_in_place_edits_script_files_and_writing_or_running_commands() {
            for command in [
                "sed -i 's/a/b/' a.rs",
                "sed -I '' 's/a/b/' a.rs",
                "sed --in 's/a/b/' a.rs",
                "sed --fi=script.sed a.rs",
                "sed --exp='w out' p a.rs",
                "sed 'w out' -e p a.rs",
                "sed -ni 'p' a.rs",
                "sed --in-place=.bak 's/a/b/' a.rs",
                "sed -f script.sed a.rs",
                "sed --file=script.sed a.rs",
                "sed 's/a/b/w out' a.rs",
                "sed 's/a/b/e' a.rs",
                "sed -n 'w out' a.rs",
                "sed '1r /etc/passwd' a.rs",
                "sed 'e date' a.rs",
                "sed -e",
                "sed --expression",
                "sed",
            ] {
                assert!(!is_safe(command), "{command:?}");
            }
        }

        #[test]
        fn reads_the_files_past_the_script() {
            for command in ["sed -n 1p a.rs", "sed -e 1p -- a.rs"] {
                let parsed = parse(command);

                assert!(parsed.segments[0].is_safe(), "{command:?}");
                assert_eq!(
                    parsed.reads,
                    [resolve("a.rs", &Workspace::new(Path::new("/repo")))],
                    "{command:?}"
                );
            }
        }
    }

    mod sed_script {
        use super::*;

        #[test]
        fn names_the_files_a_script_reads_and_writes_and_whether_it_runs_a_command() {
            let effects = |reads: &[&str], writes: &[&str], runs| SedScript {
                reads: reads.iter().map(|&path| path.to_owned()).collect(),
                writes: writes.iter().map(|&path| path.to_owned()).collect(),
                runs,
            };
            for (script, expected) in [
                ("p", effects(&[], &[], false)),
                ("$!{N;s/\\n/ /};/^#/d", effects(&[], &[], false)),
                ("2,+3p;3,~4d;\\,x,Ip;0~2p", effects(&[], &[], false)),
                ("1,5d", effects(&[], &[], false)),
                (":a;N;ba", effects(&[], &[], false)),
                ("l 40;$q5", effects(&[], &[], false)),
                ("y/abc/xyz/", effects(&[], &[], false)),
                ("a\\\ntext r x\n1p", effects(&[], &[], false)),
                ("# r x\np", effects(&[], &[], false)),
                ("1r /etc/hosts", effects(&["/etc/hosts"], &[], false)),
                ("/x/ R notes\n$w out", effects(&["notes"], &["out"], false)),
                ("/a/,/b/w out", effects(&[], &["out"], false)),
                ("0~2 W even", effects(&[], &["even"], false)),
                ("s/a/b/gw log", effects(&[], &["log"], false)),
                ("s|a|b|e", effects(&[], &[], true)),
                ("1e date", effects(&[], &[], true)),
            ] {
                assert_eq!(sed_script(script, Dialect::Gnu), Ok(expected), "{script:?}");
            }
        }

        #[test]
        fn takes_only_the_commands_bsd_sed_has_for_a_bsd_script() {
            let expected = SedScript {
                reads: vec!["in".to_owned()],
                writes: vec!["out".to_owned(), "log".to_owned()],
                runs: false,
            };

            assert_eq!(
                sed_script("1r in\n$w out\ns/a/b/w log", Dialect::Bsd),
                Ok(expected)
            );
            for script in [
                "R in",
                "W out",
                "1e date",
                "s/a/b/e",
                "F",
                "z",
                "Q",
                "README.md",
            ] {
                assert_eq!(sed_script(script, Dialect::Bsd), Err(vec![]), "{script:?}");
            }
        }

        #[test]
        fn refuses_a_script_it_cannot_read_keeping_the_files_it_writes_before() {
            for (script, writes) in [
                ("k", &[][..]),
                ("s/a/b", &[]),
                ("r", &[]),
                ("1r ", &[]),
                ("pq", &[]),
                ("s/a/b/x", &[]),
                ("/x", &[]),
                ("1,p", &[]),
                ("w locked\nk", &["locked"]),
                ("1r in\ns/a/b/w log\n$k\nw out", &["log"]),
            ] {
                let writes = writes.iter().map(|&path| path.to_owned()).collect();

                assert_eq!(sed_script(script, Dialect::Gnu), Err(writes), "{script:?}");
            }
        }
    }

    mod reader_operands {
        use super::*;

        #[test]
        fn flag_a_reader_word_whose_files_are_unknown() {
            for command in [
                "cat $F",
                r#"cat "$X/.ssh/id_rsa""#,
                "/bin/cat $F",
                "grep -r x $DIR",
                "head -n $N a",
                "cat x[",
            ] {
                assert!(parse(command).segments[0].reads_anything(), "{command:?}");
            }
            for command in ["cat a", "cat *.rs", "echo $F", "cp $F b"] {
                assert!(!parse(command).segments[0].reads_anything(), "{command:?}");
            }
        }

        #[test]
        fn flag_a_read_after_a_change_the_walk_cannot_spell() {
            for (command, anything) in [
                ("cd $D; cat a", true),
                ("HOME=d cd; cat a", true),
                ("cd; cat a", false),
                ("pushd -n $D; cat ~1/a", true),
                ("pushd -n $D; cat a", false),
                ("pushd -n src; cat ~1/a", false),
                ("pushd -n +1; cat ~1/a", false),
            ] {
                let parsed = parse(command);

                let last = parsed.segments.last().unwrap();

                assert_eq!(last.reads_anything(), anything, "{command:?}");
            }
        }

        #[test]
        fn keeps_recursive_roots_for_each_ambiguous_expansion_order() {
            for (reader, written, expanded, start, end, source, root) in [
                (
                    "grep",
                    "-e PRIVATE -* pats",
                    "-e PRIVATE -f -R pats",
                    2,
                    4,
                    2,
                    ".",
                ),
                (
                    "grep",
                    "-e PRIVATE -* pats",
                    "-e PRIVATE -R -f pats",
                    2,
                    4,
                    2,
                    ".",
                ),
                (
                    "ls",
                    "-D vault -* 80",
                    "-D vault -w -R 80",
                    2,
                    4,
                    2,
                    "vault",
                ),
                (
                    "ls",
                    "-D vault -* 80",
                    "-D vault -R -w 80",
                    2,
                    4,
                    2,
                    "vault",
                ),
                ("find", "* )", "vault ( -name )", 0, 3, 0, "."),
                ("find", "* )", "( -name vault )", 0, 3, 0, "."),
                (
                    "ls",
                    "--format *",
                    "--format -R long vault",
                    1,
                    4,
                    1,
                    "vault",
                ),
                (
                    "ls",
                    "--format *",
                    "--format long -R vault",
                    1,
                    4,
                    1,
                    "vault",
                ),
                (
                    "grep",
                    "-r -e needle -? patterns/* src",
                    "-r -e needle -f patterns/z-vault patterns/a-patterns src",
                    4,
                    6,
                    4,
                    "patterns/z-vault",
                ),
                (
                    "grep",
                    "-r -e needle -? patterns/* src",
                    "-r -e needle -f patterns/a-patterns patterns/z-vault src",
                    4,
                    6,
                    4,
                    "patterns/z-vault",
                ),
                (
                    "grep",
                    "-e PRIVATE -[f] pats -* more",
                    "-e PRIVATE -f pats -f -R more",
                    4,
                    6,
                    4,
                    ".",
                ),
            ] {
                let written: Vec<_> = written.split_whitespace().map(Some).collect();
                let expanded: Vec<_> = expanded.split_whitespace().map(Some).collect();
                let args = reader_args(reader, &written);

                let origins: Vec<_> = (0..expanded.len())
                    .map(|index| match index {
                        _ if index < start => Some(index),
                        _ if index < end => None,
                        _ => Some(index + source + 1 - end),
                    })
                    .collect();

                let (paths, unknown) = expanded_reader_paths(
                    reader,
                    &args,
                    &expanded,
                    &origins,
                    &[(start..end, source)],
                );

                assert!(unknown, "{reader} {expanded:?}");
                assert!(
                    paths.iter().any(|operand| operand.read
                        == ReaderPath {
                            path: root.to_owned(),
                            recursive: true,
                        }),
                    "{reader} {expanded:?}: {paths:?}"
                );
            }
        }

        #[test]
        fn read_each_literal_operand_and_each_file_a_glob_matches() {
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
            std::fs::create_dir(cwd.join("src")).unwrap();
            std::fs::write(cwd.join("src/main.rs"), "").unwrap();
            let workspace = Workspace::new(cwd);

            let command = CommandTarget::parse("grep -r x src/*.rs docs", &workspace);

            let segment = &command.segments[0];
            assert!(!segment.is_safe());
            let at = |name: &str| resolve(name, &workspace);
            let tree = |name: &str| ReaderPath {
                path: at(name),
                recursive: true,
            };
            assert_eq!(segment.reads(), [tree("docs"), tree("src/*.rs")]);
            let main = Path::new("src").join("main.rs");
            assert_eq!(command.reads, [at("docs"), at(&main.to_string_lossy())]);
        }
    }

    mod reader_args {
        use super::*;

        // cspell:ignore drecurse rfpatterns
        #[test]
        fn distinguishes_recursive_operands_from_exact_auxiliary_files() {
            for (reader, args, expected) in [
                (
                    "grep",
                    "-r -f patterns src",
                    &[("patterns", false), ("src", true)][..],
                ),
                (
                    "grep",
                    "-R --exclude-from=skip -e pat src",
                    &[("skip", false), ("src", true)],
                ),
                ("grep", "-d recurse pat src", &[("src", true)]),
                ("grep", "-drecurse pat src", &[("src", true)]),
                ("grep", "--directories=rec pat src", &[("src", true)]),
                ("grep", "--dereference-rec pat src", &[("src", true)]),
                ("grep", "--rec pat src", &[("src", true)]),
                ("grep", "pat src -r", &[("-r", false), ("src", true)]),
                ("grep", "-d skip pat src", &[("src", false)]),
                ("grep", "-e -r src", &[("src", false)]),
                ("grep", "-r pat -", &[]),
                ("grep", "-r pat", &[(".", true)]),
                ("grep", "-rfpatterns", &[(".", true), ("patterns", false)]),
                ("grep", "-- -r src", &[("src", false)]),
                ("ls", "-laR src", &[("src", true)]),
                ("ls", "--rec", &[(".", true)]),
                ("ls", "-- -R", &[("-R", false)]),
                (
                    "find",
                    "-L src -name needle",
                    &[("needle", false), ("src", true)],
                ),
                ("find", "-name needle", &[(".", true), ("needle", false)]),
                (
                    "find",
                    "-HL src -name needle",
                    &[("needle", false), ("src", true)],
                ),
                ("find", "-f src", &[("src", true)]),
                ("find", "-D search -O3 src", &[("src", true)]),
                ("find", "", &[(".", true)]),
            ] {
                let values: Vec<_> = args.split_whitespace().map(Some).collect();

                let read = reader_args(reader, &values);

                let mut actual: Vec<_> = read
                    .files
                    .iter()
                    .map(|(_, file)| (file.path.as_str(), file.recursive))
                    .collect();
                actual.sort_unstable();
                actual.dedup();
                assert_eq!(actual, expected, "{reader} {args}");
                assert!(!read.unknown, "{reader} {args}");
            }
        }

        /// The files `reader` reads from the literal words of `args`.
        fn files(reader: &str, args: &str) -> Vec<String> {
            let values: Vec<Option<&str>> = args.split(' ').map(Some).collect();
            let read = reader_args(reader, &values);
            assert!(!read.unknown, "{reader} {args}");
            read.files.into_iter().map(|(_, file)| file.path).collect()
        }

        #[test]
        fn reads_option_values_as_values() {
            for (reader, args) in [
                ("tail", "-20 x"),
                ("tail", "-n 20 x"),
                ("tail", "-n20 x"),
                ("tail", "--lines=20 x"),
                ("tail", "--lines 20 x"),
                ("tail", "-fn 20 x"),
                ("head", "-c 5 x"),
                ("head", "-5 x"),
                ("grep", "-A 5 pat x"),
                ("grep", "-nA5 pat x"),
                ("grep", "-3 pat x"),
                ("grep", "--color=auto -m 1 pat x"),
                ("grep", "-e pat x"),
                ("grep", "--regexp=pat -- x"),
                ("grep", "--reg=pat x"),
                ("grep", "--regex pat x"),
                ("grep", "--binary pat x"),
                ("grep", "--exclude=*.md pat x"),
                ("tail", "--lin 20 x"),
                ("ls", "--sort=size x"),
                ("sed", "-i.bak s/a/b/ x"),
                ("wc", "-l --total=never x"),
                ("cat", "-n - x"),
            ] {
                assert_eq!(files(reader, args), ["x"], "{reader} {args}");
            }
        }

        #[test]
        fn counts_each_file_a_gnu_or_bsd_build_reads_in_either_option_order() {
            for (reader, args, expected) in [
                ("ls", "-I target x", &["target", "x"][..]),
                ("ls", "-D %s x", &["%s", "x"]),
                ("sed", "-n -l 80 1p x", &["1p", "x"]),
                ("sed", "-i .bak s/a/b/ x", &["s/a/b/", "x"]),
                ("grep", "--context 3 pat x", &["pat", "x"]),
                ("tail", "x -n 5", &["x", "-n", "5"]),
                ("grep", "pat x -e y", &["pat", "x", "-e", "y"]),
                ("cat", "x -n", &["x", "-n"]),
            ] {
                assert_eq!(files(reader, args), expected, "{reader} {args}");
            }
        }

        #[test]
        fn counts_a_value_that_names_a_file_and_every_operand_after_a_given_pattern() {
            for (reader, args, expected) in [
                ("grep", "-f patterns.txt x", &["patterns.txt", "x"][..]),
                ("grep", "-fpats --file more y", &["pats", "more", "y"]),
                ("grep", "--exclude-from=skip pat x", &["skip", "x"]),
                ("sed", "-f script.sed x", &["script.sed", "x"]),
                ("find", ". -name x", &[".", "x"]),
            ] {
                assert_eq!(files(reader, args), expected, "{reader} {args}");
            }
        }

        #[test]
        fn reads_an_option_it_cannot_place_as_naming_a_file() {
            for (reader, args, expected) in [
                ("tail", "-z 20 x", &["20", "x"][..]),
                ("grep", "--unknown=f pat x", &["f", "pat", "x"]),
                ("grep", "--ex=a pat x", &["a", "pat", "x"]),
                ("tail", "-n", &[]),
            ] {
                assert_eq!(files(reader, args), expected, "{reader} {args}");
            }
        }

        #[test]
        fn leaves_what_it_reads_unknown_for_a_file_list_or_a_word_that_expands() {
            for (reader, values) in [
                ("wc", &[Some("--files0-from=list")][..]),
                ("find", &[Some("-files0-from"), Some("list")]),
                ("head", &[Some("-n"), None, Some("x")]),
                ("grep", &[None, Some("x")]),
            ] {
                assert!(reader_args(reader, values).unknown, "{reader} {values:?}");
            }
        }
    }

    mod expand_glob {
        use super::*;

        #[test]
        fn lists_what_bash_would_and_skips_dot_names_unless_a_name_asks_for_them() {
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
            for dir in ["src/sub", ".ssh", "docs"] {
                std::fs::create_dir_all(cwd.join(dir)).unwrap();
            }
            for file in [
                "src/a.rs",
                "src/b.rs",
                "src/c.md",
                "src/sub/d.rs",
                ".ssh/id_rsa",
                ".ssh/id_ed25519",
                "docs/id_x",
                ".env",
            ] {
                std::fs::write(cwd.join(file), "").unwrap();
            }
            let expand = |pattern: &str| {
                let mut paths: Vec<String> = expand_glob(pattern, cwd)
                    .unwrap()
                    .iter()
                    .map(|path| slash_separated(Path::new(path)))
                    .collect();
                paths.sort();
                paths
            };
            let root = slash_separated(cwd);

            assert_eq!(expand("src/*.rs"), ["src/a.rs", "src/b.rs"]);
            assert_eq!(expand("./src/*/*.rs"), ["./src/sub/d.rs"]);
            assert_eq!(expand("*/id_*"), ["docs/id_x"]);
            assert_eq!(expand(".*/id_*"), [".ssh/id_ed25519", ".ssh/id_rsa"]);
            assert_eq!(
                expand("{.ssh,docs}/id_[er]*"),
                [".ssh/id_ed25519", ".ssh/id_rsa"]
            );
            assert_eq!(expand("?env"), [""; 0]);
            assert_eq!(expand("missing/*"), [""; 0]);
            assert_eq!(
                expand(&format!("{root}/.ss?/id_rsa")),
                [format!("{root}/.ssh/id_rsa")]
            );
        }

        #[test]
        fn gives_up_past_the_match_cap_or_on_a_pattern_globset_rejects() {
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
            for i in 0..MAX_GLOB_MATCHES {
                std::fs::write(cwd.join(format!("{i}.txt")), "").unwrap();
            }
            assert_eq!(
                expand_glob("*.txt", cwd).map(|paths| paths.len()),
                Some(MAX_GLOB_MATCHES)
            );

            std::fs::write(cwd.join("last.txt"), "").unwrap();

            assert_eq!(expand_glob("*.txt", cwd), None);
            assert_eq!(expand_glob("[", cwd), None);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn gives_up_on_a_file_name_that_is_not_utf8() {
            use std::os::unix::ffi::OsStrExt as _;
            let cwd_dir = canonical_tempdir();

            let cwd = cwd_dir.path();
            std::fs::write(cwd.join(std::ffi::OsStr::from_bytes(b"a\xff")), "").unwrap();

            assert_eq!(expand_glob("*", cwd), None);
        }
    }
}
