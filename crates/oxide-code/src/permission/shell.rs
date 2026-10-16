//! `bash` command analysis for the permission gate. tree-sitter-bash splits a command into the
//! simple commands it runs and the files its output redirections write, so rules weigh each simple
//! command on its own. Leading assignments and transparent wrappers are stripped first. A command
//! the analysis cannot vouch for is opaque: rules may deny it or leave it to ask, but never allow
//! it. Design: `docs/design/tools/permissions.md`.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use crate::permission::GateTarget;
use crate::util::path::expand_user;

/// Shells, whose `-c` script, script file, or stdin is code the analysis does not see.
const SHELLS: &[&str] = &[
    "ash", "bash", "busybox", "csh", "dash", "fish", "ksh", "mksh", "pwsh", "sh", "tcsh", "zsh",
];

/// Builtins that run a string or a file as shell code, or evaluate one as arithmetic, which can
/// itself hold a substitution.
const CODE_BUILTINS: &[&str] = &[".", "eval", "let", "source", "trap"];

/// Builtins that change the working directory, after which a relative redirection target no longer
/// names the file the gate resolved.
const DIRECTORY_CHANGERS: &[&str] = &["cd", "popd", "pushd"];

/// Variables a leading assignment may set without changing what a rule names, since none of them
/// changes which program runs, what it loads, or where it writes.
const SAFE_VARIABLES: &[&str] = &[
    "BLOCKSIZE",
    "BLOCK_SIZE",
    "CARGO_TERM_COLOR",
    "CGO_ENABLED",
    "CHARSET",
    "COLORTERM",
    "FORCE_COLOR",
    "GCC_COLORS",
    "GO111MODULE",
    "GOARCH",
    "GOEXPERIMENT",
    "GOOS",
    "GREP_COLOR",
    "GREP_COLORS",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TIME",
    "LSCOLORS",
    "LS_COLORS",
    "NODE_ENV",
    "NO_COLOR",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONUNBUFFERED",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "TERM",
    "TIME_STYLE",
    "TZ",
];

/// Variables the shell itself reads to find programs, expand words, or run startup code, so
/// assigning one changes what later commands run.
const SHELL_VARIABLES: &[&str] = &[
    "BASHOPTS",
    "BASH_ENV",
    "CDPATH",
    "ENV",
    "EXECIGNORE",
    "GLOBIGNORE",
    "HOME",
    "IFS",
    "OLDPWD",
    "PATH",
    "PS4",
    "PWD",
    "SHELLOPTS",
];

/// Redirection operators that write their target.
const WRITE_OPERATORS: &[&str] = &[">", ">>", ">|", "&>", "&>>", ">&"];

/// Redirection targets that are devices, so writing them edits no file.
const DEVICES: &[&str] = &["/dev/null", "/dev/stderr", "/dev/stdout", "/dev/tty"];

/// Wrappers that run the command after their own options, so rules see through them.
const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "builtin",
        flags: &[],
        valued: &[],
        directory: &[],
        numeric: false,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "command",
        flags: &["-V", "-p", "-v"],
        valued: &[],
        directory: &[],
        numeric: false,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "env",
        flags: &[
            "-",
            "-0",
            "-i",
            "-v",
            "--debug",
            "--ignore-environment",
            "--null",
        ],
        valued: &["-C", "-u", "--chdir", "--unset"],
        directory: &["-C", "--chdir"],
        numeric: false,
        positionals: 0,
        assignments: true,
    },
    Wrapper {
        name: "exec",
        flags: &["-c", "-l"],
        valued: &["-a"],
        directory: &[],
        numeric: false,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "nice",
        flags: &[],
        valued: &["-n", "--adjustment"],
        directory: &[],
        numeric: true,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "nohup",
        flags: &[],
        valued: &[],
        directory: &[],
        numeric: false,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "time",
        flags: &["-p"],
        valued: &[],
        directory: &[],
        numeric: false,
        positionals: 0,
        assignments: false,
    },
    Wrapper {
        name: "timeout",
        flags: &["-v", "--foreground", "--preserve-status", "--verbose"],
        valued: &["-k", "-s", "--kill-after", "--signal"],
        directory: &[],
        numeric: false,
        positionals: 1,
        assignments: false,
    },
];

/// `find` primaries that run a command or write a file.
const FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-ok", "-okdir",
];

/// A `sed` script of commands that only print, delete, substitute, or move text between buffers.
/// It leaves out `r`, `w`, and `e`, and the `w` and `e` substitution flags, which read a file,
/// write one, or run a command.
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

// ── Command Target ──

/// A `bash` call's command and what the analysis found in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommandTarget {
    /// The command as the call passes it.
    command: String,
    /// Each simple command it runs, including those inside substitutions and literal `sh -c` or
    /// `eval` scripts.
    segments: Vec<Segment>,
    /// The files its output redirections write, resolved like an edit's path.
    writes: Vec<GateTarget>,
    /// Whether it runs code the analysis does not see or cannot place.
    opaque: bool,
}

impl CommandTarget {
    /// Parses `command`, resolving the files it reads and writes against `cwd`.
    pub(crate) fn parse(command: &str, cwd: &Path) -> Self {
        let mut walk = Walk {
            cwd,
            segments: Vec::new(),
            writes: Vec::new(),
            opaque: false,
            changes_dir: false,
            relative_path: false,
        };
        walk.script(command);
        Self {
            command: command.to_owned(),
            segments: walk.segments,
            writes: walk.writes,
            opaque: walk.opaque || (walk.changes_dir && walk.relative_path),
        }
    }

    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(super) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub(super) fn writes(&self) -> &[GateTarget] {
        &self.writes
    }

    pub(super) const fn is_opaque(&self) -> bool {
        self.opaque
    }
}

/// One simple command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Segment {
    /// The command as written, from where a rule must name it. Leading assignments of
    /// [`SAFE_VARIABLES`] and transparent wrappers are left out, up to the first assignment of
    /// another variable.
    text: String,
    /// The words of the program that runs, past every assignment and wrapper, unquoted, or as
    /// written where a word expands.
    words: Vec<String>,
    /// The files a shipped known-safe read-only command reads, resolved like a read's path, or
    /// `None` for any other command.
    safe_reads: Option<Vec<GateTarget>>,
}

impl Segment {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn words(&self) -> &[String] {
        &self.words
    }

    pub(super) fn safe_reads(&self) -> Option<&[GateTarget]> {
        self.safe_reads.as_deref()
    }
}

// ── Walk ──

/// What one command and the scripts nested in it run and write, gathered over its syntax tree.
struct Walk<'a> {
    cwd: &'a Path,
    segments: Vec<Segment>,
    writes: Vec<GateTarget>,
    opaque: bool,
    /// Whether some segment changes the working directory.
    changes_dir: bool,
    /// Whether some write or known-safe read names a relative path.
    relative_path: bool,
}

impl Walk<'_> {
    fn script(&mut self, source: &str) {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .expect("the bundled bash grammar matches the tree-sitter ABI");
        let tree = parser
            .parse(source, None)
            .expect("parsing stops early only on a timeout or cancellation, and neither is set");
        let root = tree.root_node();
        self.opaque |= root.has_error();
        self.visit(root, source);
    }

    fn visit(&mut self, node: Node<'_>, src: &str) {
        match node.kind() {
            "command" => self.command(node, src),
            "file_redirect" => self.redirect(node, src),
            "variable_assignment" => {
                self.opaque |= SHELL_VARIABLES.contains(&assigned(&src[node.byte_range()]));
                self.visit_children(node, src);
            }
            "declaration_command" => {
                let mut cursor = node.walk();
                self.opaque |= node.named_children(&mut cursor).any(|child| {
                    let name = match child.kind() {
                        "variable_assignment" => assigned(&src[child.byte_range()]),
                        "variable_name" => &src[child.byte_range()],
                        _ => return false,
                    };
                    !SAFE_VARIABLES.contains(&name)
                });
                self.visit_children(node, src);
            }
            "for_statement" => {
                self.opaque |= node
                    .child_by_field_name("variable")
                    .is_some_and(|name| SHELL_VARIABLES.contains(&&src[name.byte_range()]));
                self.visit_children(node, src);
            }
            _ => {
                self.opaque |= runs_hidden_code(node);
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

    fn command(&mut self, node: Node<'_>, src: &str) {
        let mut cursor = node.walk();
        let words: Vec<Word<'_>> = node
            .named_children(&mut cursor)
            .filter_map(|child| match child.kind() {
                "command_name" => child.named_child(0),
                "variable_assignment"
                | "file_redirect"
                | "herestring_redirect"
                | "heredoc_redirect" => None,
                _ => Some(child),
            })
            .map(|word| Word::new(word, src))
            .collect();
        let mut unsafe_assignment = None;
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_assignment" {
                if !SAFE_VARIABLES.contains(&assigned(&src[child.byte_range()])) {
                    unsafe_assignment.get_or_insert(child);
                }
                self.visit_children(child, src);
            } else {
                self.visit(child, src);
            }
        }

        let (words, named) = self.strip_wrappers(&words);
        let Some((first, args)) = words.split_first() else {
            return;
        };
        let name = first.value.as_deref();
        match name {
            None => self.opaque = true,
            Some(name) => {
                let base = basename(name);
                if SHELLS.contains(&base) || CODE_BUILTINS.contains(&base) {
                    self.opaque = true;
                    self.nested_script(base, args);
                }
                self.changes_dir |= DIRECTORY_CHANGERS.contains(&name);
            }
        }
        let pinned = unsafe_assignment.is_some() || named.len() > words.len();
        let safe_reads = name
            .filter(|_| !pinned)
            .and_then(|name| safe_operands(name, args))
            .map(|operands| {
                self.relative_path |= operands.iter().any(|path| Path::new(path).is_relative());
                operands
                    .iter()
                    .map(|operand| GateTarget::for_path(operand, self.cwd))
                    .collect()
            });
        let start = unsafe_assignment.unwrap_or(named[0].node).start_byte();
        self.segments.push(Segment {
            text: src[start..node.end_byte()].to_owned(),
            words: words.iter().map(|word| word.shell_value(src)).collect(),
            safe_reads,
        });
    }

    /// Strips the transparent wrappers that lead `words`, returning the words of the command that
    /// runs and the words a rule must name. The latter keep every wrapper from the first one that
    /// sets a variable outside [`SAFE_VARIABLES`]. A wrapper option the analysis does not know
    /// leaves the command where it cannot be found, so it turns the command opaque.
    fn strip_wrappers<'w, 't>(
        &mut self,
        words: &'w [Word<'t>],
    ) -> (&'w [Word<'t>], &'w [Word<'t>]) {
        let (mut runs, mut named, mut pinned) = (words, words, false);
        while let Some((name, args)) = runs.split_first()
            && let Some(name) = name.value.as_deref()
            && let Some(wrapper) = WRAPPERS.iter().find(|w| w.name == basename(name))
        {
            match wrapper.command_start(args) {
                Start::At(start) => {
                    let options = &args[..start];
                    self.changes_dir |= wrapper.changes_dir(options);
                    let sets_unsafe = options.iter().any(|word| {
                        word.value.as_deref().is_some_and(|arg| {
                            is_assignment(arg) && !SAFE_VARIABLES.contains(&assigned(arg))
                        })
                    });
                    runs = &args[start..];
                    pinned |= sets_unsafe;
                    if !pinned {
                        named = runs;
                    }
                }
                Start::Alone => break,
                Start::Unknown => {
                    self.opaque = true;
                    break;
                }
            }
        }
        (runs, named)
    }

    /// Walks the literal script a shell's `-c` option or `eval` runs, so deny rules see the
    /// commands inside it.
    fn nested_script(&mut self, runner: &str, args: &[Word<'_>]) {
        let script = if runner == "eval" {
            args.iter()
                .map(|word| word.value.as_deref())
                .collect::<Option<Vec<_>>>()
                .map(|values| values.join(" "))
        } else if SHELLS.contains(&runner) {
            args.iter()
                .position(|word| word.value.as_deref().is_some_and(is_command_option))
                .and_then(|i| args.get(i + 1)?.value.clone())
        } else {
            None
        };
        if let Some(script) = script {
            self.script(&script);
        }
    }

    fn redirect(&mut self, node: Node<'_>, src: &str) {
        self.visit_children(node, src);
        let mut cursor = node.walk();
        let operator = node
            .children(&mut cursor)
            .find(|child| !child.is_named())
            .map(|child| child.kind());
        if !operator.is_some_and(|op| WRITE_OPERATORS.contains(&op)) {
            return;
        }
        let destinations: Vec<Node<'_>> = node
            .children_by_field_name("destination", &mut cursor)
            .collect();
        // tree-sitter-bash folds the words after `>&2` into further destinations, which leaves
        // the command's real arguments unknown.
        let &[destination] = destinations.as_slice() else {
            self.opaque = true;
            return;
        };

        let word = Word::new(destination, src);
        let Some(path) = word.value.filter(|_| !word.glob) else {
            self.opaque = true;
            return;
        };
        if operator == Some(">&") && (destination.kind() == "number" || path == "-") {
            return;
        }
        if DEVICES.contains(&path.as_str()) {
            return;
        }
        self.relative_path |= Path::new(&path).is_relative();
        self.writes.push(GateTarget::for_path(&path, self.cwd));
    }
}

/// Whether `node` runs code no simple command shows: a substitution, or arithmetic, which
/// evaluates a variable's value as an expression that can itself hold a substitution.
fn runs_hidden_code(node: Node<'_>) -> bool {
    match node.kind() {
        "arithmetic_expansion"
        | "c_style_for_statement"
        | "command_substitution"
        | "process_substitution"
        | "test_command" => true,
        "compound_statement" => node.child(0).is_some_and(|open| open.kind() == "(("),
        _ => false,
    }
}

/// The last component of a command path, so `/bin/sh` counts as `sh`.
pub(super) fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Whether a shell option cluster such as `-c` or `-lc` takes the script as the next word.
fn is_command_option(arg: &str) -> bool {
    arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c')
}

// ── Wrapper ──

/// A transparent wrapper's option grammar.
struct Wrapper {
    name: &'static str,
    flags: &'static [&'static str],
    /// Options that take a value, either as the next word or attached (`-n5`, `--signal=KILL`).
    valued: &'static [&'static str],
    /// Valued options that run the command in another directory.
    directory: &'static [&'static str],
    /// Whether `-N` sets a number, as `nice -10` does.
    numeric: bool,
    /// Words after the options that precede the command, such as `timeout`'s duration.
    positionals: usize,
    /// Whether `NAME=VALUE` words may follow the options, as with `env`.
    assignments: bool,
}

/// Where a wrapper's command starts among its arguments.
enum Start {
    At(usize),
    /// No command follows, so the wrapper runs alone.
    Alone,
    /// An option the grammar does not know hides where the command starts.
    Unknown,
}

impl Wrapper {
    /// Whether `options` run the command in another directory.
    fn changes_dir(&self, options: &[Word<'_>]) -> bool {
        options
            .iter()
            .filter_map(|word| word.value.as_deref())
            .any(|arg| {
                self.directory
                    .iter()
                    .any(|option| arg == *option || is_attached(arg, option))
            })
    }

    fn command_start(&self, args: &[Word<'_>]) -> Start {
        let mut i = 0;
        while let Some(arg) = args.get(i) {
            let Some(arg) = arg.value.as_deref() else {
                return Start::Unknown;
            };
            if arg == "--" {
                i += 1;
                break;
            }
            if !arg.starts_with('-') || (arg == "-" && !self.flags.contains(&arg)) {
                break;
            }
            i += if self.flags.contains(&arg) {
                1
            } else if self.valued.contains(&arg) {
                2
            } else if self.valued.iter().any(|option| is_attached(arg, option))
                || (self.numeric && arg.len() > 1 && arg[1..].bytes().all(|b| b.is_ascii_digit()))
            {
                1
            } else {
                return Start::Unknown;
            };
        }
        i += self.positionals;
        if self.assignments {
            while args
                .get(i)
                .and_then(|arg| arg.value.as_deref())
                .is_some_and(is_assignment)
            {
                i += 1;
            }
        }
        if i < args.len() {
            Start::At(i)
        } else {
            Start::Alone
        }
    }
}

/// Whether `arg` is `option` with its value attached.
fn is_attached(arg: &str, option: &str) -> bool {
    arg.strip_prefix(option).is_some_and(|value| {
        if option.starts_with("--") {
            value.starts_with('=')
        } else {
            !value.is_empty()
        }
    })
}

/// The variable a `NAME=VALUE`, `NAME+=VALUE`, or `NAME[i]=VALUE` assignment sets.
fn assigned(assignment: &str) -> &str {
    assignment
        .find(['=', '+', '['])
        .map_or(assignment, |end| &assignment[..end])
}

/// Whether `arg` is a `NAME=VALUE` environment assignment.
fn is_assignment(arg: &str) -> bool {
    arg.split_once('=').is_some_and(|(name, _)| {
        name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
            && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
    })
}

// ── Word ──

/// One word of a simple command.
struct Word<'t> {
    node: Node<'t>,
    /// The value the shell gives the word after tilde expansion and quote removal, or `None`
    /// when part of it expands at run time.
    value: Option<String>,
    /// Whether an unquoted `*`, `?`, `[`, or `{` leaves the word to pathname or brace expansion.
    glob: bool,
}

impl<'t> Word<'t> {
    fn new(node: Node<'t>, src: &str) -> Self {
        let (value, glob) = unquote(node, src, true).unzip();
        Self {
            node,
            value,
            glob: glob.unwrap_or(false),
        }
    }

    fn shell_value(&self, src: &str) -> String {
        self.value
            .clone()
            .unwrap_or_else(|| src[self.node.byte_range()].to_owned())
    }
}

/// `node`'s value and whether it globs, for [`Word`]. Tilde expansion applies to a `leading` word
/// alone, and `~user` has no value, since the analysis does not look users up.
fn unquote(node: Node<'_>, src: &str, leading: bool) -> Option<(String, bool)> {
    let text = &src[node.byte_range()];
    match node.kind() {
        "word" => unquote_word(text, leading),
        "number" => Some((text.to_owned(), false)),
        "raw_string" => Some((text.get(1..text.len() - 1)?.to_owned(), false)),
        "string" => {
            let mut cursor = node.walk();
            if !node
                .named_children(&mut cursor)
                .all(|part| part.kind() == "string_content")
            {
                return None;
            }
            Some((unquote_double(text.get(1..text.len() - 1)?), false))
        }
        "concatenation" => {
            let mut cursor = node.walk();
            let mut value = String::new();
            let mut glob = false;
            for (i, part) in node.named_children(&mut cursor).enumerate() {
                let (part, part_glob) = unquote(part, src, leading && i == 0)?;
                value.push_str(&part);
                glob |= part_glob;
            }
            Some((value, glob))
        }
        _ => None,
    }
}

/// An unquoted word's value, with backslash escapes removed and a leading `~` expanded.
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
    if leading && text.starts_with('~') {
        if value != "~" && !value.starts_with("~/") {
            return None;
        }
        value = expand_user(&value).ok()?.to_string_lossy().into_owned();
    }
    Some((value, glob))
}

/// A double-quoted string's value: a backslash escapes only `$`, `` ` ``, `"`, `\`, and a line
/// break.
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

// ── Known-Safe Commands ──

/// The words that may name a file read by the shipped known-safe read-only command `name`, or
/// `None` when `name` is no such command or `args` take it out of the safe set. Every argument
/// must be literal and free of globs, so each file it reads is one the gate can check.
fn safe_operands(name: &str, args: &[Word<'_>]) -> Option<Vec<String>> {
    if matches!(name, "echo" | "false" | "pwd" | "true") {
        return Some(Vec::new());
    }
    let values = args
        .iter()
        .map(|word| word.value.as_deref().filter(|_| !word.glob))
        .collect::<Option<Vec<_>>>()?;
    match name {
        "cat" | "head" | "ls" | "tail" | "wc" => Some(operands(&values)),
        "find" if !values.iter().any(|value| FIND_ACTIONS.contains(value)) => {
            Some(operands(&values))
        }
        "grep" if !values.iter().any(|value| is_recursive_grep(value)) => Some(operands(&values)),
        "sed" => sed_files(&values),
        _ => None,
    }
}

/// Every argument that may name a file: each word past the options, the value of a long option
/// such as `--file=x`, and the rest of a short option cluster, which may hold an attached value.
fn operands(values: &[&str]) -> Vec<String> {
    let mut operands = Vec::new();
    let mut options = true;
    for &value in values {
        if options && value == "--" {
            options = false;
            continue;
        }
        let operand = if !options || !value.starts_with('-') {
            value
        } else if let Some(long) = value.strip_prefix("--") {
            long.split_once('=').map_or("", |(_, value)| value)
        } else {
            let mut cluster = value[1..].chars();
            cluster.next();
            cluster.as_str()
        };
        if !operand.is_empty() && operand != "-" {
            operands.push(operand.to_owned());
        }
    }
    operands
}

/// Whether a `grep` argument makes it read every file under a directory.
/// `--directories` and `-d` count whatever their value, since grep accepts `rec` for `recurse`.
fn is_recursive_grep(arg: &str) -> bool {
    match long_option(arg) {
        Some(name) => ["dereference-recursive", "directories", "recursive"]
            .iter()
            .any(|option| option.starts_with(name)),
        None => arg.starts_with('-') && arg.contains(['d', 'r', 'R']),
    }
}

/// The files a `sed` call reads, or `None` when it may edit a file in place, read a script file, or
/// run a script [`SAFE_SED_SCRIPT`] does not accept.
fn sed_files(values: &[&str]) -> Option<Vec<String>> {
    let mut scripts = Vec::new();
    let mut files = Vec::new();
    let mut args = values.iter().copied();
    while let Some(arg) = args.next() {
        if arg == "--" {
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

/// The name of a long option such as `--name` or `--name=value`, which GNU tools accept
/// abbreviated to any prefix that names one option.
fn long_option(arg: &str) -> Option<&str> {
    let option = arg.strip_prefix("--")?;
    let name = option.split_once('=').map_or(option, |(name, _)| name);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;
    use crate::util::path::testing::{HOME_VAR, abs};

    fn temp_cwd() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        (dir, cwd)
    }

    fn parse(command: &str) -> CommandTarget {
        CommandTarget::parse(command, Path::new("/repo"))
    }

    fn texts(command: &str) -> Vec<String> {
        parse(command)
            .segments
            .into_iter()
            .map(|segment| segment.text)
            .collect()
    }

    fn words(command: &str) -> Vec<String> {
        parse(command).segments.remove(0).words
    }

    /// Each write's path relative to `cwd`, or as requested outside it.
    fn writes(command: &str, cwd: &Path) -> Vec<String> {
        CommandTarget::parse(command, cwd)
            .writes
            .into_iter()
            .map(|write| match write {
                GateTarget::Path {
                    relative,
                    requested,
                    ..
                } => relative.unwrap_or(requested),
                other => panic!("a write is a path target, got {other:?}"),
            })
            .collect()
    }

    fn is_safe(command: &str) -> bool {
        parse(command).segments[0].safe_reads.is_some()
    }

    // ── CommandTarget::parse: segments ──

    #[test]
    fn parse_splits_simple_commands_across_every_separator() {
        for (command, expected) in [
            ("a; b", &["a", "b"][..]),
            ("a && b || c", &["a", "b", "c"]),
            ("a | b |& c", &["a", "b", "c"]),
            ("a & b", &["a", "b"]),
            ("a\nb", &["a", "b"]),
            ("(cd x && ls) ; { echo a; }", &["cd x", "ls", "echo a"]),
            ("! git diff --quiet", &["git diff --quiet"]),
            ("if true; then ls -la; fi", &["true", "ls -la"]),
        ] {
            assert_eq!(texts(command), expected, "{command:?}");
        }
        let command = parse("cargo test --all");
        assert_eq!(command.command(), "cargo test --all");
        assert!(!command.is_opaque());
    }

    #[test]
    fn parse_strips_leading_assignments_and_keeps_words_unquoted() {
        assert_eq!(texts(r#"LANG=C TZ="x y" cargo test"#), ["cargo test"]);
        assert_eq!(
            words(indoc! {r#"
                "rm" -rf 'a b' c\ d "e\"f\
                g\h" "$HOME"
            "#}),
            ["rm", "-rf", "a b", "c d", "e\"fg\\h", "\"$HOME\""]
        );
        assert!(texts("FOO=1").is_empty(), "an assignment runs no command");
    }

    #[test]
    fn parse_keeps_an_assignment_that_may_change_the_program_in_the_text_a_rule_names() {
        for (command, text, name) in [
            (
                "RUST_LOG=debug LD_PRELOAD=./x.so cat a",
                "LD_PRELOAD=./x.so cat a",
                "cat",
            ),
            ("GIT_PAGER=sh git log", "GIT_PAGER=sh git log", "git"),
            (
                "nice env PATH=. nohup cat a",
                "env PATH=. nohup cat a",
                "cat",
            ),
        ] {
            let command = parse(command);
            let segment = &command.segments[0];
            assert_eq!(segment.text, text, "{command:?}");
            assert_eq!(segment.words[0], name, "{command:?}");
            assert!(segment.safe_reads.is_none(), "{command:?}");
            assert!(!command.is_opaque(), "{command:?}");
        }
        assert!(is_safe("LANG=C TZ=UTC env NO_COLOR=1 cat a"));
    }

    #[test]
    fn parse_turns_a_variable_the_shell_or_a_later_program_reads_opaque() {
        for command in [
            "PATH=.; ls",
            "HOME=/; cat ~/x",
            "export GIT_PAGER=sh; git log",
            "declare -x PATH",
            "for PATH in .; do ls; done",
        ] {
            assert!(parse(command).is_opaque(), "{command:?}");
        }
        for command in [
            "X=1; echo hi",
            "export RUST_LOG=debug; cargo test",
            "for f in a b; do cat \"$f\"; done",
        ] {
            assert!(!parse(command).is_opaque(), "{command:?}");
        }
    }

    #[test]
    fn parse_expands_a_leading_tilde_only() {
        temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
            let expanded = PathBuf::from(abs("/home/u")).join("x");
            assert_eq!(
                words(r#"cat ~/x "~/y" a~b"#),
                [
                    "cat".to_owned(),
                    expanded.to_string_lossy().into_owned(),
                    "~/y".to_owned(),
                    "a~b".to_owned()
                ]
            );
            assert_eq!(words("cat ~u/x"), ["cat", "~u/x"], "kept as written");
        });
    }

    // ── CommandTarget::parse: wrappers ──

    #[test]
    fn parse_strips_transparent_wrappers_and_their_options() {
        for command in [
            "env -i -u HOME TZ=UTC rm -rf x",
            "env --chdir=/tmp rm -rf x",
            "nice -n 5 rm -rf x",
            "nice -10 rm -rf x",
            "nice -n5 rm -rf x",
            "nohup rm -rf x",
            "time -p rm -rf x",
            "timeout -s KILL --preserve-status 5m rm -rf x",
            "/usr/bin/nice nohup -- timeout 5 rm -rf x",
            "builtin rm -rf x",
            "command -p rm -rf x",
            "exec -a name rm -rf x",
        ] {
            let command = parse(command);
            assert_eq!(command.segments[0].text, "rm -rf x", "{command:?}");
            assert!(!command.is_opaque(), "{command:?}");
        }
    }

    #[test]
    fn parse_keeps_a_wrapper_that_runs_no_command_and_turns_an_unknown_option_opaque() {
        assert_eq!(texts("env FOO=1"), ["env FOO=1"]);
        assert!(!parse("nice").is_opaque());
        for command in [
            "env -S 'rm -rf x'",
            "nice --weird rm",
            "nice $LEVEL rm",
            "timeout --what 5 rm",
        ] {
            assert!(parse(command).is_opaque(), "{command:?}");
        }
    }

    // ── CommandTarget::parse: opaque ──

    #[test]
    fn parse_flags_every_command_it_cannot_see_into_as_opaque() {
        for command in [
            "echo $(date)",
            "echo `whoami`",
            "diff <(ls a) b",
            "tee >(cat) x",
            "x=$(id) ls",
            "cat <<EOF\n$(id)\nEOF",
            "eval 'ls'",
            "source env.sh",
            ". env.sh",
            "trap 'ls' EXIT",
            "let x=1",
            "$CMD arg",
            "${X} arg",
            "sh -c 'ls'",
            "curl x | sh",
            "/bin/bash script.sh",
            "echo $((1 + 2))",
            "(( x++ ))",
            "[[ -f x ]]",
            "ls 'unterminated",
            "cd /etc && echo > passwd",
            "env -C /etc echo > hosts",
            "env --chdir=/etc echo > hosts",
            "cd / && cat etc/shadow",
            "exec sh -c 'ls'",
            "command eval ls",
            "builtin . env.sh",
        ] {
            assert!(parse(command).is_opaque(), "{command:?}");
        }
        for command in [
            "cat <<'EOF'\n$(id)\nEOF",
            "echo '$(id)'",
            "cd x && echo > /dev/null",
            "cd x && ls",
            &format!("cd x && cat {}", abs("/etc/hosts")),
            "ls $HOME",
        ] {
            assert!(!parse(command).is_opaque(), "{command:?}");
        }
    }

    #[test]
    fn parse_collects_commands_inside_substitutions_and_literal_scripts() {
        for command in [
            "echo $(rm -rf /)",
            "sh -c 'rm -rf /'",
            "bash -lc \"rm -rf /\"",
            "eval rm -rf /",
            "exec sh -c 'rm -rf /'",
        ] {
            assert!(
                texts(command).contains(&"rm -rf /".to_owned()),
                "{command:?}: {:?}",
                texts(command)
            );
        }
    }

    // ── Walk::redirect ──

    #[test]
    fn redirect_counts_each_file_an_output_redirection_writes() {
        let (_dir, cwd) = temp_cwd();
        assert_eq!(
            writes("echo a > out.txt 2>> log &> all >| clobber >& both", &cwd),
            ["out.txt", "log", "all", "clobber", "both"]
        );
        let outside = abs("/etc/hosts");
        assert_eq!(
            CommandTarget::parse(&format!("echo a > {outside}"), &cwd).writes,
            [GateTarget::for_path(&outside, &cwd)]
        );
    }

    #[test]
    fn redirect_skips_inputs_descriptors_and_devices() {
        let (_dir, cwd) = temp_cwd();
        for command in [
            "cat < in.txt",
            "cat <<< x",
            "make 2>&1",
            "make 3>&-",
            "make > /dev/null 2> /dev/stderr",
        ] {
            assert!(writes(command, &cwd).is_empty(), "{command:?}");
            assert!(
                !CommandTarget::parse(command, &cwd).is_opaque(),
                "{command:?}"
            );
        }
    }

    #[test]
    fn redirect_to_a_target_it_cannot_place_is_opaque() {
        for command in ["echo > \"$OUT\"", "echo > *.txt", "echo >&2 hi"] {
            assert!(parse(command).is_opaque(), "{command:?}");
        }
    }

    // ── safe_operands ──

    #[test]
    fn safe_operands_names_every_word_that_may_be_a_file() {
        let command =
            CommandTarget::parse("grep -fpats --file=more -e x -- -y src", Path::new("/repo"));
        let at = |name: &str| GateTarget::for_path(name, Path::new("/repo"));
        assert_eq!(
            command.segments[0].safe_reads,
            Some(vec![at("pats"), at("more"), at("x"), at("-y"), at("src")])
        );
        assert_eq!(
            parse("echo $HOME *").segments[0].safe_reads,
            Some(Vec::new())
        );
    }

    #[test]
    fn safe_operands_admits_only_shipped_commands_with_literal_arguments() {
        for command in [
            "cat a.rs",
            "head -n 5 a",
            "ls -la src",
            "wc -l a",
            "pwd",
            "true",
        ] {
            assert!(is_safe(command), "{command:?}");
        }
        for command in [
            "cat *.rs",
            "cat $F",
            "cat {a,b}",
            "rm a",
            "./cat a",
            "git status",
        ] {
            assert!(!is_safe(command), "{command:?}");
        }
    }

    #[test]
    fn safe_operands_guards_find_and_grep() {
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

    // ── sed_files ──

    #[test]
    fn sed_files_admits_scripts_that_only_print_or_rewrite_text() {
        for command in [
            "sed -n '1,20p' a.rs",
            "sed 's/foo/bar/g' a.rs",
            "sed -e '/^$/d' -e 's/a\\/b/c/2' a.rs",
            "sed --expression='$p' a.rs",
            "sed --expression '$p' a.rs",
            "sed --quiet 1p a.rs",
            "sed -es/a/b/ a.rs",
            "sed --exp=1p a.rs",
            "sed -ne '3p;5q' a.rs",
            "sed -n -- '1!G;h;$p' a.rs",
        ] {
            assert!(is_safe(command), "{command:?}");
        }
    }

    #[test]
    fn sed_files_refuses_in_place_edits_script_files_and_writing_or_running_commands() {
        for command in [
            "sed -i 's/a/b/' a.rs",
            "sed -I '' 's/a/b/' a.rs",
            "sed --in 's/a/b/' a.rs",
            "sed --fi=script.sed a.rs",
            "sed --exp='w out' p a.rs",
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
    fn sed_files_reads_the_files_past_the_script() {
        for command in ["sed -n 1p a.rs", "sed -e 1p -- a.rs"] {
            let reads = parse(command).segments[0].safe_reads.clone().unwrap();
            assert_eq!(
                reads,
                [GateTarget::for_path("a.rs", Path::new("/repo"))],
                "{command:?}"
            );
        }
    }
}
