//! `bash` command analysis for the permission gate. tree-sitter-bash splits a command into the
//! simple commands it runs and the files its output redirections write, so rules weigh each simple
//! command on its own. Leading assignments and transparent wrappers are stripped first. A command
//! the analysis cannot vouch for is opaque: rules may deny it or leave it to ask, but never allow
//! it. `exec` patterns parse here too, so they read words the way commands do.
//!
//! Design: `docs/design/tools/permissions.md`.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Tree};

use crate::permission::{Access, Workspace, fold_name};
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

/// Commands whose file operands the gate weighs against `read` rules, whether or not they run as
/// [known-safe](safe_operands) reads.
const READERS: &[&str] = &["cat", "find", "grep", "head", "ls", "sed", "tail", "wc"];

/// Files one globbing word may match before the analysis gives up on listing them.
const MAX_GLOB_MATCHES: usize = 1024;

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

/// Redirection targets that are devices, so redirecting to or from them touches no file.
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
    /// Each simple command it runs, including those inside substitutions and literal `sh -c` or
    /// `eval` scripts.
    segments: Vec<Segment>,
    /// The files its output redirections write, resolved like an edit's path.
    writes: Vec<Access>,
    /// The files its input redirections read, resolved like a read's path.
    reads: Vec<Access>,
    /// Whether it runs code the analysis does not see or cannot place.
    opaque: bool,
}

impl CommandTarget {
    /// Parses `command`, resolving the files it reads and writes in `workspace`.
    pub(crate) fn parse(command: &str, workspace: &Workspace) -> Self {
        let mut walk = Walk {
            workspace,
            segments: Vec::new(),
            writes: Vec::new(),
            reads: Vec::new(),
            opaque: false,
            changes_dir: false,
            relative_path: false,
        };
        walk.script(command);
        Self {
            segments: walk.segments,
            writes: walk.writes,
            reads: walk.reads,
            opaque: walk.opaque || (walk.changes_dir && walk.relative_path),
        }
    }

    pub(super) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub(super) fn writes(&self) -> &[Access] {
        &self.writes
    }

    pub(super) fn reads(&self) -> &[Access] {
        &self.reads
    }

    pub(super) const fn is_opaque(&self) -> bool {
        self.opaque
    }
}

/// One simple command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Segment {
    /// The words an allow rule must name, without redirections. Leading assignments of
    /// [`SAFE_VARIABLES`] and transparent wrappers are left out, up to the first assignment of
    /// another variable, which starts the list and keeps every word after it.
    named: Vec<ShellWord>,
    /// Where the program that runs starts in `named`, past every assignment and wrapper.
    program: usize,
    /// The files one of [`READERS`] may read, resolved like a read's path: each literal operand,
    /// and each file a globbing one matches when the analysis runs. Empty for any other command.
    reads: Vec<Access>,
    /// Whether one of [`READERS`] takes a word whose files the analysis cannot list, such as one
    /// that expands a variable, so it may read any file.
    reads_anything: bool,
    /// Whether it is a shipped known-safe read-only command, whose `reads` are all it reads.
    safe: bool,
}

impl Segment {
    pub(super) fn named(&self) -> &[ShellWord] {
        &self.named
    }

    pub(super) fn program(&self) -> &[ShellWord] {
        &self.named[self.program..]
    }

    /// The program words with the program's directory dropped, when its name has one, so
    /// `/bin/rm -rf x` also reads as `rm -rf x`.
    pub(super) fn bare_program(&self) -> Option<Vec<ShellWord>> {
        let (name, args) = self.program().split_first()?;
        let bare = basename(&name.text);
        (bare != name.text).then(|| {
            std::iter::once(ShellWord {
                text: bare.to_owned(),
                literal: name.literal,
            })
            .chain(args.iter().cloned())
            .collect()
        })
    }

    /// The files a known-safe read-only command reads, or `None` for any other command.
    pub(super) fn safe_reads(&self) -> Option<&[Access]> {
        self.safe.then_some(self.reads.as_slice())
    }

    pub(super) fn reads(&self) -> &[Access] {
        &self.reads
    }

    pub(super) const fn reads_anything(&self) -> bool {
        self.reads_anything
    }

    /// Returns whether this may run `rm` with both its recursive and force options before any `--`,
    /// in one short cluster or two, such as `-rf` or `-R -f`, or as `--recursive` and `--force` or
    /// a prefix GNU `rm` takes for either. A word that expands, the program name included, may
    /// become any of them, and GNU `rm` reads options after operands too, so `rm / -rf` counts.
    pub(super) fn removes_recursively_by_force(&self) -> bool {
        let (name, args) = self
            .program()
            .split_first()
            .expect("a simple command runs a program");
        if name.literal && fold_name(basename(&name.text)) != "rm" {
            return false;
        }
        let (mut recursive, mut force) = (false, false);
        for arg in args {
            if !arg.literal {
                return true;
            }
            if arg.text == "--" {
                break;
            }
            if let Some(long) = arg.text.strip_prefix("--") {
                recursive |= !long.is_empty() && "recursive".starts_with(long);
                force |= !long.is_empty() && "force".starts_with(long);
            } else if let Some(cluster) = arg.text.strip_prefix('-') {
                recursive |= cluster.contains(['r', 'R']);
                force |= cluster.contains('f');
            }
        }
        recursive && force
    }
}

/// One word of a simple command as rules compare it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ShellWord {
    /// The word unquoted, or as written where part of it expands at run time.
    text: String,
    /// Whether `text` is the only value the word can take, since it neither expands nor globs.
    literal: bool,
}

impl ShellWord {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) const fn is_literal(&self) -> bool {
        self.literal
    }
}

// ── Walk ──

/// What one command and the scripts nested in it run and write, gathered over its syntax tree.
struct Walk<'a> {
    workspace: &'a Workspace,
    segments: Vec<Segment>,
    writes: Vec<Access>,
    reads: Vec<Access>,
    opaque: bool,
    /// Whether some segment changes the working directory.
    changes_dir: bool,
    /// Whether some redirection or known-safe read names a relative path.
    relative_path: bool,
}

impl Walk<'_> {
    fn script(&mut self, source: &str) {
        let tree = parse_tree(source);
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
        let mut assignments = Vec::new();
        let mut unsafe_assignment = None;
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_assignment" {
                if !SAFE_VARIABLES.contains(&assigned(&src[child.byte_range()])) {
                    unsafe_assignment.get_or_insert(assignments.len());
                }
                assignments.push(assignment_word(child, src));
                self.visit_children(child, src);
            } else {
                self.visit(child, src);
            }
        }

        let all = &words;
        let (words, named) = self.strip_wrappers(all);
        let Some((first, args)) = words.split_first() else {
            return;
        };
        let name = first.value.as_deref().filter(|_| !first.glob);
        match name {
            None => self.opaque = true,
            Some(name) => {
                let base = basename(name);
                let folded = fold_name(base);
                let runner = [base, folded.as_str()]
                    .into_iter()
                    .find(|runner| SHELLS.contains(runner) || CODE_BUILTINS.contains(runner));
                if let Some(runner) = runner {
                    self.opaque = true;
                    self.nested_script(runner, args);
                }
                self.changes_dir |= DIRECTORY_CHANGERS.contains(&name);
                // `shopt` can turn on `dotglob` or `globstar`, which change what a glob matches.
                self.opaque |= name == "shopt";
            }
        }
        let pinned = unsafe_assignment.is_some() || named.len() > words.len();
        let safe = name
            .filter(|_| !pinned)
            .and_then(|name| safe_operands(name, args));
        let reader = name.is_some_and(|name| READERS.contains(&fold_name(basename(name)).as_str()));
        let (operands, reads_anything) = match (&safe, reader) {
            (Some(operands), _) => (operands.clone(), false),
            (None, true) => reader_operands(args, self.workspace.cwd()),
            (None, false) => (Vec::new(), false),
        };
        self.relative_path |= operands.iter().any(|path| Path::new(path).is_relative());
        let reads = operands
            .iter()
            .map(|operand| Access::for_path(operand, self.workspace))
            .collect();
        let named: Vec<ShellWord> = match unsafe_assignment {
            Some(first) => assignments
                .split_off(first)
                .into_iter()
                .chain(all.iter().map(|word| word.shell_word(src)))
                .collect(),
            None => named.iter().map(|word| word.shell_word(src)).collect(),
        };
        self.segments.push(Segment {
            program: named.len() - words.len(),
            named,
            reads,
            reads_anything,
            safe: safe.is_some(),
        });
    }

    /// Strips the transparent wrappers that lead `words`, returning the words of the command that
    /// runs and the words a rule must name. The latter keep every wrapper from the first one that
    /// sets a variable outside [`SAFE_VARIABLES`] or is spelled with a directory, since
    /// `./nice` may be any program. A wrapper option the analysis does not know leaves the command
    /// where it cannot be found, so it turns the command opaque.
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
                    pinned |= sets_unsafe || name != wrapper.name;
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
        let Some(operator) = node
            .children(&mut cursor)
            .find(|child| !child.is_named())
            .map(|child| child.kind())
        else {
            return;
        };
        let reads = operator == "<";
        if !reads && !WRITE_OPERATORS.contains(&operator) {
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
        if operator == ">&" && (destination.kind() == "number" || path == "-") {
            return;
        }
        if DEVICES.contains(&path.as_str()) {
            return;
        }
        self.relative_path |= Path::new(&path).is_relative();
        let access = Access::for_path(&path, self.workspace);
        if reads {
            self.reads.push(access);
        } else {
            self.writes.push(access);
        }
    }
}

/// Returns whether `node` runs code no simple command shows: a substitution, or arithmetic, which
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

fn parse_tree(source: &str) -> Tree {
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

/// Returns whether a shell option cluster such as `-c` or `-lc` takes the script as the next word.
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
    /// Returns whether `options` run the command in another directory.
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

/// Returns whether `arg` is `option` with its value attached.
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

/// Returns whether `arg` is a `NAME=VALUE` environment assignment.
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

    fn shell_word(&self, src: &str) -> ShellWord {
        ShellWord {
            text: self
                .value
                .clone()
                .unwrap_or_else(|| src[self.node.byte_range()].to_owned()),
            literal: self.value.is_some() && !self.glob,
        }
    }
}

/// A leading `NAME=VALUE` assignment as a word, with its value unquoted and a leading `~`
/// expanded. Neither pathname nor brace expansion applies to an assignment, so only a value or a
/// subscript that expands keeps it from being literal.
fn assignment_word(node: Node<'_>, src: &str) -> ShellWord {
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

/// `node`'s value and whether it globs, for [`Word`]. Tilde expansion applies to a `leading` word
/// alone, and `~user` has no value, since the analysis does not look users up.
fn unquote(node: Node<'_>, src: &str, leading: bool) -> Option<(String, bool)> {
    let text = &src[node.byte_range()];
    match node.kind() {
        "word" => unquote_word(text, leading),
        "number" => Some((text.to_owned(), false)),
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
fn take_digits(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    radix: u32,
    max: usize,
) -> Option<(u32, u32)> {
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

// ── Patterns ──

/// One word of an `exec` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PatternWord {
    Literal(String),
    /// An unquoted `*` standing alone.
    Any,
}

const NAMES_NO_COMMAND: &str = "it must name a command, such as `cargo test *`";

const ONE_COMMAND: &str = "it must be one simple command, such as `cargo test *`. \
                           Write a pattern for each command of a chain or pipeline.";

const HOLDS_REDIRECTION: &str = "it holds a redirection. \
                                 Leave it out, since a pattern matches a command \
                                 with its redirections set aside, \
                                 and an `edit` pattern covers the file it writes.";

/// Parses an `exec` pattern into its words: one simple command, optionally led by assignments,
/// whose words are each literal or a lone unquoted `*`.
pub(super) fn pattern_words(pattern: &str) -> Result<Vec<PatternWord>> {
    let tree = parse_tree(pattern);
    let root = tree.root_node();
    ensure!(!root.has_error(), "it does not parse as a shell command");
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

    let mut words = Vec::new();
    let mut names_command = false;
    let mut cursor = command.walk();
    for child in command.named_children(&mut cursor) {
        let node = match child.kind() {
            "variable_assignment" => {
                let word = assignment_word(child, pattern);
                ensure!(
                    word.literal,
                    "the assignment {:?} expands at run time. \
                     Quote its value to match it as written.",
                    word.text
                );
                words.push(PatternWord::Literal(word.text));
                continue;
            }
            "file_redirect" | "herestring_redirect" | "heredoc_redirect" => {
                bail!("{HOLDS_REDIRECTION}");
            }
            "command_name" => {
                names_command = true;
                match child.named_child(0) {
                    Some(name) => name,
                    None => bail!("{NAMES_NO_COMMAND}"),
                }
            }
            _ => child,
        };
        let written = &pattern[node.byte_range()];
        if node.kind() == "word" && written == "*" {
            words.push(PatternWord::Any);
            continue;
        }
        let word = Word::new(node, pattern);
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
        words.push(PatternWord::Literal(value));
    }
    ensure!(names_command, "{NAMES_NO_COMMAND}");
    Ok(words)
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
    let files = || {
        operands(&values)
            .into_iter()
            .map(|(_, file)| file)
            .collect()
    };
    match name {
        "cat" | "head" | "ls" | "tail" | "wc" => Some(files()),
        "find" if !values.iter().any(|value| FIND_ACTIONS.contains(value)) => Some(files()),
        "grep" if !values.iter().any(|value| is_recursive_grep(value)) => Some(files()),
        "sed" => sed_files(&values),
        _ => None,
    }
}

/// Every argument that may name a file, with the index of the value it comes from: each word past
/// the options, the value of a long option such as `--file=x`, and the rest of a short option
/// cluster, which may hold an attached value.
fn operands(values: &[&str]) -> Vec<(usize, String)> {
    let mut operands = Vec::new();
    let mut options = true;
    for (index, &value) in values.iter().enumerate() {
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
            operands.push((index, operand.to_owned()));
        }
    }
    operands
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

// ── Readers ──

/// The files one of [`READERS`] that is no known-safe read may read, from `args`: every word that
/// may name a file, with each globbing one also expanded in `cwd` as bash expands it. The second
/// value is whether a word expands a variable or globs to files [`expand_glob`] cannot list, which
/// leaves what it reads unknown.
fn reader_operands(args: &[Word<'_>], cwd: &Path) -> (Vec<String>, bool) {
    let known: Vec<(&str, bool)> = args
        .iter()
        .filter_map(|word| Some((word.value.as_deref()?, word.glob)))
        .collect();
    let mut anything = known.len() < args.len();
    let values: Vec<&str> = known.iter().map(|(value, _)| *value).collect();
    let mut files = Vec::new();
    for (index, operand) in operands(&values) {
        if known[index].1 {
            match expand_glob(&operand, cwd) {
                Some(matches) => files.extend(matches),
                None => anything = true,
            }
        }
        files.push(operand);
    }
    (files, anything)
}

/// The paths bash's brace and pathname expansion give `pattern` in `cwd`: each name with `*`, `?`,
/// `[`, or `{` matches the entries of its directory, skipping those that start with `.` unless the
/// name starts with `.` too or holds a brace, whose alternatives bash writes out before globbing.
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
    use indoc::indoc;

    use super::*;
    use crate::util::path::local_testing::abs;
    use crate::util::path::slash_separated;
    use crate::util::path::testing::HOME_VAR;

    fn temp_cwd() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        (dir, cwd)
    }

    fn parse(command: &str) -> CommandTarget {
        CommandTarget::parse(command, &Workspace::new(Path::new("/repo")))
    }

    fn is_safe(command: &str) -> bool {
        parse(command).segments[0].safe_reads().is_some()
    }

    mod command_target {
        use super::*;

        mod parse {
            use super::*;

            fn joined(words: &[ShellWord]) -> String {
                words
                    .iter()
                    .map(ShellWord::text)
                    .collect::<Vec<_>>()
                    .join(" ")
            }

            /// Each simple command's named words, joined on spaces.
            fn texts(command: &str) -> Vec<String> {
                parse(command)
                    .segments
                    .iter()
                    .map(|segment| joined(segment.named()))
                    .collect()
            }

            fn words(command: &str) -> Vec<String> {
                parse(command).segments[0]
                    .program()
                    .iter()
                    .map(|word| word.text().to_owned())
                    .collect()
            }

            #[test]
            fn splits_simple_commands_across_every_separator() {
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
                assert!(!parse("cargo test --all").is_opaque());
            }

            #[test]
            fn strips_leading_assignments_and_keeps_words_unquoted() {
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
            fn keeps_an_assignment_that_may_change_the_program_in_the_text_a_rule_names() {
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
                    assert_eq!(joined(segment.named()), text, "{command:?}");
                    assert_eq!(segment.program()[0].text(), name, "{command:?}");
                    assert!(segment.safe_reads().is_none(), "{command:?}");
                    assert!(!command.is_opaque(), "{command:?}");
                }
                assert!(is_safe("LANG=C TZ=UTC env NO_COLOR=1 cat a"));
            }

            #[test]
            fn turns_a_variable_the_shell_or_a_later_program_reads_opaque() {
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
            fn expands_a_leading_tilde_only() {
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

            #[test]
            fn marks_each_word_that_expands_or_globs_as_not_literal() {
                let command = temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                    parse(r#"FOO="a b" BAR=$X EMPTY= a[$i]=1 cat 'x*' *.rs "$HOME" ~/n"#)
                });
                let segment = &command.segments[0];
                let named: Vec<(&str, bool)> = segment
                    .named()
                    .iter()
                    .map(|word| (word.text(), word.is_literal()))
                    .collect();
                let home = Path::new(&abs("/home/u"))
                    .join("n")
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(
                    named,
                    [
                        ("FOO=a b", true),
                        ("BAR=$X", false),
                        ("EMPTY=", true),
                        ("a[$i]=1", false),
                        ("cat", true),
                        ("x*", true),
                        ("*.rs", false),
                        ("\"$HOME\"", false),
                        (home.as_str(), true),
                    ]
                );
                assert_eq!(
                    joined(segment.program()),
                    format!("cat x* *.rs \"$HOME\" {home}")
                );
            }

            #[test]
            fn reads_an_ansi_c_string_as_the_literal_word_it_decodes_to() {
                let command = parse(r"$'rm' pu$'sh' $'\055rf' $'\x80'");

                let program: Vec<(&str, bool)> = command.segments[0]
                    .program()
                    .iter()
                    .map(|word| (word.text(), word.is_literal()))
                    .collect();

                assert_eq!(
                    program,
                    [
                        ("rm", true),
                        ("push", true),
                        ("-rf", true),
                        (r"$'\x80'", false)
                    ]
                );
            }

            #[test]
            fn drops_the_program_directory_for_the_bare_program_only() {
                let command = parse("FOO=1 /usr/bin/rm -rf x");
                let segment = &command.segments[0];
                assert_eq!(joined(&segment.bare_program().unwrap()), "rm -rf x");
                assert_eq!(joined(segment.named()), "FOO=1 /usr/bin/rm -rf x");
                assert!(parse("rm -rf x").segments[0].bare_program().is_none());
            }

            #[test]
            fn strips_transparent_wrappers_and_their_options() {
                for command in [
                    "env -i -u HOME TZ=UTC rm -rf x",
                    "env --chdir=/tmp rm -rf x",
                    "nice -n 5 rm -rf x",
                    "nice -10 rm -rf x",
                    "nice -n5 rm -rf x",
                    "nohup rm -rf x",
                    "time -p rm -rf x",
                    "timeout -s KILL --preserve-status 5m rm -rf x",
                    "nice nohup -- timeout 5 rm -rf x",
                    "builtin rm -rf x",
                    "command -p rm -rf x",
                    "exec -a name rm -rf x",
                ] {
                    let command = parse(command);
                    assert_eq!(
                        joined(command.segments[0].program()),
                        "rm -rf x",
                        "{command:?}"
                    );
                    assert_eq!(
                        joined(command.segments[0].named()),
                        "rm -rf x",
                        "{command:?}"
                    );
                    assert!(!command.is_opaque(), "{command:?}");
                }
            }

            #[test]
            fn keeps_a_wrapper_spelled_with_a_directory_in_the_named_words() {
                for (command, named, program) in [
                    (
                        "/usr/bin/nice nohup -- timeout 5 rm -rf x",
                        "/usr/bin/nice nohup -- timeout 5 rm -rf x",
                        "rm -rf x",
                    ),
                    ("./nice cat a", "./nice cat a", "cat a"),
                    ("nice tools/env cat a", "tools/env cat a", "cat a"),
                ] {
                    let command = parse(command);
                    let segment = &command.segments[0];

                    assert_eq!(joined(segment.named()), named, "{command:?}");
                    assert_eq!(joined(segment.program()), program, "{command:?}");
                    assert!(segment.safe_reads().is_none(), "{command:?}");
                }
            }

            #[test]
            fn keeps_a_wrapper_that_runs_no_command_and_turns_an_unknown_option_opaque() {
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

            #[test]
            fn flags_every_command_it_cannot_see_into_as_opaque() {
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
                    "/bin/s? -c ls",
                    "BASH -c ls",
                    "/bin/ZSH script.zsh",
                    "cat < $F",
                    "cat < *.txt",
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
            fn collects_commands_inside_substitutions_and_literal_scripts() {
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
        }
    }

    mod segment {
        use super::*;

        mod removes_recursively_by_force {
            use super::*;

            #[test]
            fn reads_both_options_in_every_spelling_before_a_double_dash() {
                for command in [
                    "rm -rf /",
                    "rm -fr x",
                    "rm -rfv /",
                    "rm -Rf x",
                    "rm -f -R x",
                    "rm -r --force /",
                    "rm --recursive --force x",
                    "rm --rec --f x",
                    "rm / -rf",
                    "RM -Rf x",
                    "/bin/rm -rf x",
                    "nice -n 5 rm -rf x",
                    "rm $FLAGS x",
                    "$RM -rf x",
                ] {
                    assert!(
                        parse(command).segments[0].removes_recursively_by_force(),
                        "{command:?}"
                    );
                }
                for command in [
                    "rm -r x",
                    "rm -f x",
                    "rm --recursive x",
                    "rm -- -rf",
                    "rm -f -- -r",
                    "rm --forced -r x",
                    "rmdir -rf x",
                    "$RM x",
                ] {
                    assert!(
                        !parse(command).segments[0].removes_recursively_by_force(),
                        "{command:?}"
                    );
                }
            }
        }
    }

    mod walk {
        use super::*;

        mod redirect {
            use super::*;

            /// Each write's path relative to `cwd`, or as requested outside it.
            fn writes(command: &str, cwd: &Path) -> Vec<String> {
                spelled(CommandTarget::parse(command, &Workspace::new(cwd)).writes)
            }

            /// Each input redirection's path relative to `cwd`, or as requested outside it.
            fn reads(command: &str, cwd: &Path) -> Vec<String> {
                spelled(CommandTarget::parse(command, &Workspace::new(cwd)).reads)
            }

            fn spelled(targets: Vec<Access>) -> Vec<String> {
                targets
                    .into_iter()
                    .map(|target| match target {
                        Access::Path {
                            relative,
                            requested,
                            ..
                        } => relative.unwrap_or(requested),
                        other => panic!("a redirection target is a path, got {other:?}"),
                    })
                    .collect()
            }

            #[test]
            fn counts_each_file_an_output_redirection_writes() {
                let (_dir, cwd) = temp_cwd();
                assert_eq!(
                    writes("echo a > out.txt 2>> log &> all >| clobber >& both", &cwd),
                    ["out.txt", "log", "all", "clobber", "both"]
                );
                let outside = abs("/etc/hosts");
                assert_eq!(
                    CommandTarget::parse(&format!("echo a > {outside}"), &Workspace::new(&cwd))
                        .writes,
                    [Access::for_path(&outside, &Workspace::new(&cwd))]
                );
            }

            #[test]
            fn counts_each_file_an_input_redirection_reads() {
                let (_dir, cwd) = temp_cwd();

                let target = CommandTarget::parse(
                    "sort < in.txt 3< more.txt > out.txt",
                    &Workspace::new(&cwd),
                );

                assert_eq!(spelled(target.reads.clone()), ["in.txt", "more.txt"]);
                assert_eq!(spelled(target.writes.clone()), ["out.txt"]);
                assert!(!target.is_opaque());
                assert!(
                    parse("cd x && cat < in.txt").is_opaque(),
                    "a relative input after a cd may name another file"
                );
            }

            #[test]
            fn skips_heredocs_descriptors_and_devices() {
                let (_dir, cwd) = temp_cwd();
                for command in [
                    "cat <<< x",
                    "cat <<EOF\nx\nEOF",
                    "make 2>&1",
                    "make 3>&-",
                    "make <&3",
                    "make > /dev/null 2> /dev/stderr < /dev/null",
                ] {
                    assert!(writes(command, &cwd).is_empty(), "{command:?}");
                    assert!(reads(command, &cwd).is_empty(), "{command:?}");
                    assert!(
                        !CommandTarget::parse(command, &Workspace::new(&cwd)).is_opaque(),
                        "{command:?}"
                    );
                }
            }

            #[test]
            fn to_a_target_it_cannot_place_is_opaque() {
                for command in ["echo > \"$OUT\"", "echo > *.txt", "echo >&2 hi"] {
                    assert!(parse(command).is_opaque(), "{command:?}");
                }
            }
        }
    }

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

    mod pattern_words {
        use super::*;

        fn pattern(raw: &str) -> Vec<PatternWord> {
            pattern_words(raw).unwrap()
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

    mod safe_operands {
        use super::*;

        #[test]
        fn names_every_word_that_may_be_a_file() {
            let command = CommandTarget::parse(
                "grep -fpats --file=more -e x -- -y src",
                &Workspace::new(Path::new("/repo")),
            );
            let at = |name: &str| Access::for_path(name, &Workspace::new(Path::new("/repo")));
            assert_eq!(
                command.segments[0].safe_reads(),
                Some(&[at("pats"), at("more"), at("x"), at("-y"), at("src")][..])
            );
            assert_eq!(
                parse("echo $HOME *").segments[0].safe_reads(),
                Some(&[][..])
            );
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
                "sed -ne '3p;5q' a.rs",
                "sed -n -- '1!G;h;$p' a.rs",
            ] {
                assert!(is_safe(command), "{command:?}");
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
                let reads = parse(command).segments[0].safe_reads().unwrap().to_vec();
                assert_eq!(
                    reads,
                    [Access::for_path(
                        "a.rs",
                        &Workspace::new(Path::new("/repo"))
                    )],
                    "{command:?}"
                );
            }
        }
    }

    mod reader_operands {
        use super::*;

        #[test]
        fn flag_a_reader_word_whose_files_are_unknown() {
            for command in [
                "cat $F",
                r#"cat "$HOME/.ssh/id_rsa""#,
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
        fn read_each_literal_operand_and_each_file_a_glob_matches() {
            let (_dir, cwd) = temp_cwd();
            std::fs::create_dir(cwd.join("src")).unwrap();
            std::fs::write(cwd.join("src/main.rs"), "").unwrap();
            let workspace = Workspace::new(&cwd);

            let command = CommandTarget::parse("grep -r x src/*.rs docs", &workspace);

            let segment = &command.segments[0];
            assert_eq!(segment.safe_reads(), None);
            let at = |name: &str| Access::for_path(name, &workspace);
            let main = Path::new("src").join("main.rs");
            assert_eq!(
                segment.reads(),
                [
                    at("x"),
                    at(&main.to_string_lossy()),
                    at("src/*.rs"),
                    at("docs")
                ]
            );
        }
    }

    mod expand_glob {
        use super::*;

        #[test]
        fn lists_what_bash_would_and_skips_dot_names_unless_a_name_asks_for_them() {
            let (_dir, cwd) = temp_cwd();
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
                let mut paths: Vec<String> = expand_glob(pattern, &cwd)
                    .unwrap()
                    .iter()
                    .map(|path| slash_separated(Path::new(path)))
                    .collect();
                paths.sort();
                paths
            };
            let root = slash_separated(&cwd);

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
            let (_dir, cwd) = temp_cwd();
            for i in 0..MAX_GLOB_MATCHES {
                std::fs::write(cwd.join(format!("{i}.txt")), "").unwrap();
            }
            assert_eq!(
                expand_glob("*.txt", &cwd).map(|paths| paths.len()),
                Some(MAX_GLOB_MATCHES)
            );

            std::fs::write(cwd.join("last.txt"), "").unwrap();

            assert_eq!(expand_glob("*.txt", &cwd), None);
            assert_eq!(expand_glob("[", &cwd), None);
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn gives_up_on_a_file_name_that_is_not_utf8() {
            use std::os::unix::ffi::OsStrExt as _;

            let (_dir, cwd) = temp_cwd();
            std::fs::write(cwd.join(std::ffi::OsStr::from_bytes(b"a\xff")), "").unwrap();

            assert_eq!(expand_glob("*", &cwd), None);
        }
    }
}
