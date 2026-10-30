//! `bash` command analysis for the permission gate. tree-sitter-bash splits a command into the
//! simple commands it runs and the files its output redirections write, so rules weigh each simple
//! command on its own. Leading assignments and transparent wrappers are stripped first. The walk
//! recognizes bash syntax by an allowlist, and a construct outside it leaves the command
//! unrecognized: rules may deny it or leave it to ask, but never allow it. `exec` patterns parse
//! here too, so they read words the way commands do.

use std::iter::{self, Peekable};
use std::mem;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::str::Chars;
use std::sync::LazyLock;

use anyhow::{Result, bail, ensure};
use regex::Regex;
use tree_sitter::{Node, Parser, Tree};

use super::{PathResolver, PathTarget, Workspace, Zone, fold_name};
use crate::util::path::expand_user;

/// Shells and the commands that start one as another user. The walk follows the literal `-c`
/// script each runs, so deny rules see the commands inside it, and reads the script file a shell
/// runs, as [`shell_run`] finds them.
const SHELLS: &[&str] = &[
    "ash", "bash", "csh", "dash", "fish", "hush", "ksh", "mksh", "pwsh", "runuser", "sh", "su",
    "tcsh", "zsh",
];

/// Builtins that change how bash finds or runs later commands, run code from a trap, the history,
/// or completion, or evaluate arithmetic, which may run a substitution, so a command that runs one
/// is unrecognized.
const DISPATCH_BUILTINS: &[&str] = &[
    "alias", "bind", "compgen", "complete", "compopt", "enable", "fc", "hash", "history", "let",
    "shopt", "trap",
];

/// Builtins that change the working directory, after which a relative redirection target no longer
/// names the file the gate resolved.
const DIRECTORY_CHANGERS: &[&str] = &["cd", "popd", "pushd"];

/// Commands whose file operands the gate weighs against `read` rules, whether or not they run as
/// [known-safe](safe_operands) reads.
const READERS: &[&str] = &["cat", "find", "grep", "head", "ls", "sed", "tail", "wc"];

/// Files one globbing word may match before the analysis gives up on listing them.
const MAX_GLOB_MATCHES: usize = 1024;

/// Variables a command may set or export without changing what a rule names, since none of them
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

/// Redirection operators that write their target.
const WRITE_OPERATORS: &[&str] = &[">", ">>", ">|", "&>", "&>>", ">&"];

/// Redirection targets that are devices, so redirecting to or from them touches no file.
const DEVICES: &[&str] = &["/dev/null", "/dev/stderr", "/dev/stdout", "/dev/tty"];

/// Characters of an unrecognized construct a prompt names before cutting it short.
const MAX_CONSTRUCT_CHARS: usize = 40;

/// Wrappers that run a command given after their own options, so rules see through them.
const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "builtin",
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "busybox",
        ..Wrapper::RUNNER
    },
    Wrapper {
        name: "command",
        options: &[flag("-V"), flag("-p"), flag("-v")],
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "coproc",
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "doas",
        options: &[valued("-a"), flag("-n"), flag("-s"), valued("-u")],
        ..Wrapper::RUNNER
    },
    Wrapper {
        name: "env",
        options: &[
            flag("-"),
            flag("-0"),
            valued("-C").with(Effect::Directory),
            valued("-S").with(Effect::Splits),
            flag("-i"),
            valued("-u"),
            flag("-v"),
            valued("--chdir").with(Effect::Directory),
            flag("--debug"),
            flag("--ignore-environment"),
            flag("--null"),
            valued("--split-string").with(Effect::Splits),
            valued("--unset"),
        ],
        assignments: Assignments::After,
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "exec",
        options: &[valued("-a"), flag("-c"), flag("-l")],
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "nice",
        options: &[valued("-n"), valued("--adjustment")],
        numeric: true,
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "nohup",
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "pkexec",
        options: &[
            valued("-u"),
            flag("--disable-internal-agent"),
            flag("--keep-cwd"),
            valued("--user"),
        ],
        elsewhere: true,
        ..Wrapper::RUNNER
    },
    Wrapper {
        name: "stdbuf",
        options: &[
            valued("-e"),
            valued("-i"),
            valued("-o"),
            valued("--error"),
            valued("--input"),
            valued("--output"),
        ],
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "sudo",
        options: &[
            flag("-A"),
            flag("-B"),
            valued("-C"),
            valued("-D").with(Effect::Directory),
            flag("-E"),
            flag("-H"),
            flag("-N"),
            flag("-P"),
            valued("-R").with(Effect::Directory),
            flag("-S"),
            valued("-T"),
            valued("-a"),
            flag("-b"),
            valued("-c"),
            valued("-g"),
            flag("-i"),
            flag("-k"),
            flag("-n"),
            valued("-p"),
            valued("-r"),
            flag("-s"),
            valued("-t"),
            valued("-u"),
            flag("--askpass"),
            flag("--background"),
            flag("--bell"),
            valued("--chdir").with(Effect::Directory),
            valued("--chroot").with(Effect::Directory),
            valued("--close-from"),
            valued("--command-timeout"),
            valued("--group"),
            valued("--login-class"),
            flag("--login"),
            flag("--no-update"),
            flag("--non-interactive"),
            optional("--preserve-env"),
            flag("--preserve-groups"),
            valued("--prompt"),
            flag("--reset-timestamp"),
            valued("--role"),
            flag("--set-home"),
            flag("--shell"),
            flag("--stdin"),
            valued("--type"),
            valued("--user"),
        ],
        assignments: Assignments::Among,
        ..Wrapper::RUNNER
    },
    Wrapper {
        name: "time",
        options: &[flag("-p")],
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "timeout",
        options: &[
            valued("-k"),
            valued("-s"),
            flag("-v"),
            flag("--foreground"),
            valued("--kill-after"),
            flag("--preserve-status"),
            valued("--signal"),
            flag("--verbose"),
        ],
        positionals: 1,
        ..Wrapper::TRANSPARENT
    },
    Wrapper {
        name: "xargs",
        options: &[
            flag("-0"),
            valued("-E"),
            valued("-I").with(Effect::Replace),
            valued("-J").with(Effect::Insert),
            valued("-L"),
            valued("-P"),
            valued("-R"),
            valued("-S"),
            valued("-a").with(Effect::Reads),
            valued("-d"),
            optional("-e"),
            optional("-i").with(Effect::Replace),
            optional("-l"),
            valued("-n"),
            flag("-o"),
            flag("-p"),
            flag("-r"),
            valued("-s"),
            flag("-t"),
            flag("-x"),
            valued("--arg-file").with(Effect::Reads),
            valued("--delimiter"),
            optional("--eof"),
            flag("--exit"),
            flag("--interactive"),
            valued("--max-args"),
            valued("--max-chars"),
            optional("--max-lines"),
            valued("--max-procs"),
            flag("--no-run-if-empty"),
            flag("--null"),
            flag("--open-tty"),
            optional("--replace").with(Effect::Replace),
            flag("--verbose"),
        ],
        kind: Kind::Input,
        ..Wrapper::TRANSPARENT
    },
];

/// The placeholder `xargs -i` and `--replace` replace when given no value, and `find` replaces with
/// each path it finds.
const DEFAULT_REPLACE: &str = "{}";

/// The word that stands for the arguments `xargs` reads from its input.
const INPUT_WORD: &str = "<input>";

/// `find` primaries that run a command or write a file.
const FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-ok", "-okdir",
];

/// `runuser`'s options, which run the command after them directly when `-u` names the user.
const RUNUSER: Wrapper = Wrapper {
    name: "runuser",
    options: &[
        valued("-G"),
        flag("-P"),
        valued("-g"),
        flag("-m"),
        flag("-p"),
        valued("-u"),
        valued("-w"),
        valued("--group"),
        flag("--preserve-environment"),
        flag("--pty"),
        valued("--supp-group"),
        valued("--user"),
        valued("--whitelist-environment"),
    ],
    ..Wrapper::RUNNER
};

/// `find` primaries that run the command after them.
const FIND_RUNNERS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

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

// ── Command Target ──

/// A `bash` call's command and what the analysis found in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandTarget {
    /// Each simple command it runs, including those inside substitutions and literal `sh -c` or
    /// `eval` scripts.
    segments: Vec<Segment>,
    /// The files it writes, resolved like an edit's path: each output or read-write redirection,
    /// and each file a `sed` script or in-place edit writes.
    writes: Vec<PathTarget>,
    /// The files it reads for certain, resolved like a read's path: each input or read-write
    /// redirection, each file a wrapper option names, the script `source`, `.`, or a shell runs,
    /// and each [certain](Operand::certain) operand of one of [`READERS`], including the root of a
    /// tree it traverses.
    reads: Vec<PathTarget>,
    /// The first construct the analysis does not recognize, as [`construct`] cuts it for a prompt.
    unrecognized: Option<String>,
}

impl CommandTarget {
    /// Parses `command`, resolving the files it reads and writes in `workspace`.
    pub(crate) fn parse(command: &str, workspace: &Workspace) -> Self {
        Self::parse_inheriting(command, workspace, &|name| std::env::var_os(name).is_some())
    }

    /// Parses `command` as [`Self::parse`] does, where `inherited` tells whether the environment
    /// `bash` starts with sets a variable.
    fn parse_inheriting(
        command: &str,
        workspace: &Workspace,
        inherited: &dyn Fn(&str) -> bool,
    ) -> Self {
        let mut walk = Walk {
            workspace,
            paths: PathResolver::new(workspace),
            inherited,
            segments: Vec::new(),
            writes: Vec::new(),
            reads: Vec::new(),
            dir_change: None,
            entered: EnteredDirs::default(),
            environment: Vec::new(),
            leaves_workspace: false,
            repeats: 0,
            relative_path: false,
            expands: false,
            unrecognized: None,
        };
        walk.script(command);
        if walk.expands {
            for segment in &mut walk.segments {
                segment.safe = false;
            }
        }
        let runs_elsewhere = walk.leaves_workspace
            && walk
                .segments
                .iter()
                .flat_map(Segment::programs)
                .any(|program| !DIRECTORY_CHANGERS.contains(&program[0].text.as_str()));
        let moved = walk
            .dir_change
            .filter(|_| walk.relative_path || runs_elsewhere);
        Self {
            segments: walk.segments,
            writes: walk.writes,
            reads: walk.reads,
            unrecognized: walk.unrecognized.or(moved),
        }
    }

    pub(super) fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub(super) fn writes(&self) -> &[PathTarget] {
        &self.writes
    }

    pub(super) fn reads(&self) -> &[PathTarget] {
        &self.reads
    }

    /// The first construct the analysis does not recognize, or `None` when it recognizes the whole
    /// command, which only then may an allow rule cover. A directory change counts when a relative
    /// path or another program follows it outside the workspace, since the rules would then weigh
    /// other files or programs than the ones bash finds.
    pub(super) fn unrecognized(&self) -> Option<&str> {
        self.unrecognized.as_deref()
    }
}

/// One simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Segment {
    /// The words an allow rule must name, without redirections. Leading assignments of
    /// [`SAFE_VARIABLES`] and wrappers allow rules see through are left out, up to the first
    /// assignment of another variable or other wrapper, which starts the list and keeps every word
    /// after it. Ends in [`INPUT_WORD`] when `xargs` appends its input words.
    named: Vec<ShellWord>,
    /// Where the program may start in `named`, first past every assignment and wrapper, then at
    /// each word past a wrapper option the analysis does not know.
    programs: Vec<usize>,
    /// The files one of [`READERS`] may read beyond those the command reads for certain, resolved
    /// like a read's path: each tree it may traverse, whose descendants a rule may match below its
    /// root, and each word whose role or matches the analysis cannot pin. Empty for any other
    /// command.
    reads: Vec<ReaderPath<PathTarget>>,
    /// Whether one of [`READERS`] takes a word whose files the analysis cannot list, such as one
    /// that expands a variable, so it may read any file.
    reads_anything: bool,
    /// Whether it is a shipped known-safe read-only command, whose reads the analysis lists, in a
    /// command that expands no variable anywhere.
    safe: bool,
}

impl Segment {
    pub(super) fn named(&self) -> &[ShellWord] {
        &self.named
    }

    /// The words of each program this may run, with its arguments.
    pub(super) fn programs(&self) -> impl Iterator<Item = &[ShellWord]> {
        self.programs.iter().map(|&start| &self.named[start..])
    }

    /// Returns whether this is a known-safe read-only command.
    pub(super) const fn is_safe(&self) -> bool {
        self.safe
    }

    pub(super) fn reads(&self) -> &[ReaderPath<PathTarget>] {
        &self.reads
    }

    pub(super) const fn reads_anything(&self) -> bool {
        self.reads_anything
    }

    /// Returns whether this may run `rm` with both its recursive and force options before any `--`,
    /// in one short cluster or two, such as `-rf` or `-R -f`, or as `--recursive` and `--force` or
    /// a prefix GNU `rm` takes for either. A word that expands, the program name included, may
    /// become any of them, and the input words `xargs` runs as the program may hold them all. GNU
    /// `rm` reads options after operands too, so `rm / -rf` counts.
    pub(super) fn removes_recursively_by_force(&self) -> bool {
        self.programs().any(|program| {
            let (name, args) = program
                .split_first()
                .expect("a simple command runs a program");
            if name.is_input() {
                return true;
            }
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
        })
    }
}

/// A reader's path and whether it may traverse the tree beneath it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ReaderPath<T> {
    pub(super) path: T,
    pub(super) recursive: bool,
}

impl<T> ReaderPath<T> {
    const fn exact(path: T) -> Self {
        Self {
            path,
            recursive: false,
        }
    }
}

impl ReaderPath<String> {
    fn resolve(&self, paths: &mut PathResolver<'_>) -> ReaderPath<PathTarget> {
        let path = paths.resolve(&self.path);
        let recursive = self.recursive && !path.resolved().is_file();
        ReaderPath { path, recursive }
    }
}

/// A path a command may read, as its words name it, and how sure the analysis is that it does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Operand {
    read: ReaderPath<String>,
    /// Whether some supported argument order or build opens the word as a file or as the root of
    /// a tree it traverses, so the deny and ask tiers weigh it as a read in every mode.
    certain: bool,
}

impl Operand {
    const fn certain(path: String) -> Self {
        Self {
            read: ReaderPath::exact(path),
            certain: true,
        }
    }

    /// The root of a tree read for certain, below which the command may read any file.
    const fn tree(path: String) -> Self {
        Self {
            read: ReaderPath {
                path,
                recursive: true,
            },
            certain: true,
        }
    }

    const fn possible(read: ReaderPath<String>) -> Self {
        Self {
            read,
            certain: false,
        }
    }

    /// This operand as a command in `dir` names it.
    fn spelled_from(mut self, dir: &Path) -> Self {
        self.read.path = spelled_from(dir, &self.read.path);
        self
    }
}

/// `program` with its name's directory dropped, when the name has one, so `/bin/rm -rf x` also
/// reads as `rm -rf x`.
pub(super) fn bare_program(program: &[ShellWord]) -> Option<Vec<ShellWord>> {
    let (name, args) = program.split_first()?;
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

/// One word of a simple command as rules compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Returns whether this stands for the input words `xargs` appends.
    fn is_input(&self) -> bool {
        !self.literal && self.text == INPUT_WORD
    }

    pub(super) const fn is_literal(&self) -> bool {
        self.literal
    }
}

// ── Walk ──

/// Node kinds the walk recognizes by walking their children: the statements, words, and redirection
/// parts that only group or spell what the kinds it weighs on their own hold.
const WALKED_KINDS: &[&str] = &[
    "ansi_c_string",
    "brace_expression",
    "case_item",
    "case_statement",
    "command_name",
    "command_substitution",
    "comment",
    "compound_statement",
    "concatenation",
    "do_group",
    "elif_clause",
    "else_clause",
    "extglob_pattern",
    "file_descriptor",
    "heredoc_body",
    "heredoc_content",
    "heredoc_end",
    "heredoc_redirect",
    "heredoc_start",
    "herestring_redirect",
    "if_statement",
    "list",
    "negated_command",
    "number",
    "pipeline",
    "process_substitution",
    "program",
    "raw_string",
    "regex",
    "special_variable_name",
    "string",
    "string_content",
    "subshell",
    "test_operator",
    "translated_string",
    "variable_assignments",
    "variable_name",
    "word",
];

/// What one command and the scripts nested in it run and write, gathered over its syntax tree.
struct Walk<'a> {
    workspace: &'a Workspace,
    paths: PathResolver<'a>,
    /// Whether the environment `bash` starts with sets a variable.
    inherited: &'a dyn Fn(&str) -> bool,
    segments: Vec<Segment>,
    writes: Vec<PathTarget>,
    reads: Vec<PathTarget>,
    /// The first directory change, as [`construct`] cuts it for a prompt.
    dir_change: Option<String>,
    entered: EnteredDirs,
    /// The variables that the commands enclosing the node the walk is at set for it, as
    /// [`Inherited::variables`] lists them, such as a `BASH_ENV` before an `eval` or a `bash -c`.
    environment: Vec<(String, Option<String>)>,
    /// Whether some directory change may leave the workspace. Only a first `cd` or `pushd` that
    /// runs once, to a target [`Walk::enters_workspace`] accepts, stays inside.
    leaves_workspace: bool,
    /// How many loop bodies enclose the node the walk is at, where a directory change may run more
    /// than once.
    repeats: usize,
    /// Whether some redirection, reader operand, script that `source` or a shell runs, or program
    /// spelled with a directory names a relative path.
    relative_path: bool,
    /// Whether any part of the command expands a variable at run time, wherever the value ends up.
    expands: bool,
    /// The first construct the walk does not recognize, as [`construct`] cuts it for a prompt.
    unrecognized: Option<String>,
}

/// The directories the `cd` and `pushd` commands of a command may have entered.
#[derive(Default)]
struct EnteredDirs {
    /// Each directory as its literal targets spell it from the working directory. A later relative
    /// path also names a file under each, since a failed change, a subshell, or `||` may leave any
    /// of them, or the working directory itself, current.
    dirs: Vec<PathBuf>,
    /// Each directory a `pushd -n` adds to the directory stack without entering it, as written,
    /// which only a [`stack_tilde`] names, resolved from the directory current where it does.
    stacked: Vec<PathBuf>,
    /// Whether a change may have entered a directory the analysis cannot spell, so a later
    /// relative path may name any file.
    unknown: bool,
    /// Whether a `pushd -n` may have stacked a directory the analysis cannot spell, so a later
    /// [`stack_tilde`] may name any file.
    unknown_stacked: bool,
}

impl Walk<'_> {
    fn script(&mut self, source: &str) {
        let tree = parse_tree(source);
        let root = tree.root_node();
        if root.has_error() {
            let error = first_error(root).map_or("", |node| &source[node.byte_range()]);
            self.mark_unrecognized(if error.trim().is_empty() {
                source
            } else {
                error
            });
        } else if !splits_words_like_bash(root, source) {
            self.mark_unrecognized(source);
        }
        self.visit(root, source);
    }

    /// Records `text` as the construct that leaves the command unrecognized, unless an earlier one
    /// already has.
    fn mark_unrecognized(&mut self, text: &str) {
        self.unrecognized.get_or_insert_with(|| construct(text));
    }

    /// Returns whether a command may set the variable `name` and stay recognized: it is one of the
    /// [`SAFE_VARIABLES`], or a local name, which only later `$name` expansions read, as
    /// [`is_local_name`] decides.
    fn may_set(&self, name: &str) -> bool {
        SAFE_VARIABLES.contains(&name) || is_local_name(name, self.inherited)
    }

    /// Visits `node`, recognizing it by its kind. Each kind the walk does not recognize leaves the
    /// command unrecognized, and the walk still visits its children, so deny rules see the commands
    /// inside it.
    fn visit(&mut self, node: Node<'_>, src: &str) {
        match node.kind() {
            "command" => self.command(node, &[], src),
            "redirected_statement" => self.redirected_statement(node, Vec::new(), src),
            "file_redirect" => self.redirect(node, src),
            "declaration_command" | "unset_command" => self.declaration(node, src),
            "test_command" => self.test(node, src),
            "expansion" => self.expansion(node, src),
            "simple_expansion" => {
                self.expands = true;
                self.visit_children(node, src);
            }
            "variable_assignment" => {
                let local = node
                    .child_by_field_name("name")
                    .is_some_and(|name| self.may_set(&src[name.byte_range()]));
                if !local {
                    self.mark_unrecognized(&src[node.byte_range()]);
                }
                self.visit_children(node, src);
            }
            "array" => {
                let mut cursor = node.walk();
                for element in node.named_children(&mut cursor) {
                    if keys_by_arithmetic(element, src) {
                        self.mark_unrecognized(&src[element.byte_range()]);
                    }
                }
                self.visit_children(node, src);
            }
            "for_statement" => {
                let local = node.child(0).is_some_and(|keyword| keyword.kind() == "for")
                    && node
                        .child_by_field_name("variable")
                        .is_some_and(|name| self.may_set(&src[name.byte_range()]));
                if !local {
                    self.mark_unrecognized(&src[node.byte_range()]);
                }
                self.repeated(node, src);
            }
            "while_statement" => self.repeated(node, src),
            "compound_statement" if node.child(0).is_some_and(|open| open.kind() == "((") => {
                self.mark_unrecognized(&src[node.byte_range()]);
                self.visit_children(node, src);
            }
            kind if WALKED_KINDS.contains(&kind) => self.visit_children(node, src),
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

    /// Visits the children of a loop, whose body may run a directory change more than once.
    fn repeated(&mut self, node: Node<'_>, src: &str) {
        self.repeats += 1;
        self.visit_children(node, src);
        self.repeats -= 1;
    }

    /// Visits a redirected `statement`, whose redirections may carry words bash passes to the
    /// simple command it runs, followed by the `trailing` words of a statement around it.
    fn redirected_statement<'t>(
        &mut self,
        statement: Node<'t>,
        trailing: Vec<Node<'t>>,
        src: &str,
    ) {
        let mut cursor = statement.walk();
        let redirects: Vec<Node<'t>> = statement
            .children_by_field_name("redirect", &mut cursor)
            .collect();
        let words: Vec<Node<'t>> = redirects
            .iter()
            .flat_map(|&redirect| redirect_words(redirect))
            .chain(trailing)
            .collect();
        match statement.child_by_field_name("body") {
            Some(body) => self.statement(body, words, src),
            None if !words.is_empty() => self.parts(&[], &words, src),
            None => {}
        }
        for redirect in redirects {
            self.visit(redirect, src);
        }
    }

    /// Visits `node`, handing the `trailing` words a redirection carries to the simple command bash
    /// gives them: the last one of a pipeline or list, the one a `!` negates, or a new one after
    /// any assignments that stand alone. Under any other statement, which bash rejects, they leave
    /// the command unrecognized.
    fn statement<'t>(&mut self, node: Node<'t>, trailing: Vec<Node<'t>>, src: &str) {
        if trailing.is_empty() {
            return self.visit(node, src);
        }
        let mut cursor = node.walk();
        match node.kind() {
            "command" => self.command(node, &trailing, src),
            "redirected_statement" => self.redirected_statement(node, trailing, src),
            "list" | "negated_command" | "pipeline" => {
                let children: Vec<Node<'t>> = node.named_children(&mut cursor).collect();
                if let Some((&last, rest)) = children.split_last() {
                    for &child in rest {
                        self.visit(child, src);
                    }
                    self.statement(last, trailing, src);
                }
            }
            "variable_assignment" => self.parts(&[node], &trailing, src),
            "variable_assignments" => {
                let assignments: Vec<Node<'t>> = node.named_children(&mut cursor).collect();
                self.parts(&assignments, &trailing, src);
            }
            _ => {
                self.mark_unrecognized(&src[node.byte_range()]);
                self.visit(node, src);
            }
        }
    }

    fn command<'t>(&mut self, node: Node<'t>, trailing: &[Node<'t>], src: &str) {
        let parts = command_parts(node);
        if runs_a_reserved_word(&parts, src) {
            self.mark_unrecognized(&src[node.byte_range()]);
        }
        self.parts(&parts, trailing, src);
    }

    /// Records the simple command whose assignments, name, arguments, and redirections are `parts`,
    /// in source order, followed by the `trailing` words its statement's redirections carry.
    fn parts<'t>(&mut self, parts: &[Node<'t>], trailing: &[Node<'t>], src: &str) {
        let nodes: Vec<Node<'t>> = parts
            .iter()
            .flat_map(|&part| match part.kind() {
                "command_name" => part.named_child(0).into_iter().collect(),
                "file_redirect" | "heredoc_redirect" => redirect_words(part),
                "variable_assignment" | "herestring_redirect" => Vec::new(),
                _ => vec![part],
            })
            .collect();
        let trailing = words(trailing, src);
        let mut words = words(&nodes, src);
        words.extend(parts.last().and_then(|&last| dropped_dash(last, src)));
        words.extend(trailing);
        let prefix = (parts.iter())
            .filter(|part| part.kind() == "variable_assignment")
            .map(|part| assigned(&src[part.byte_range()]));
        let wrapped = (words.iter())
            .filter_map(|word| word.value.as_deref()?.split_once('='))
            .map(|(name, _)| name);
        let mut environment = self.environment.clone();
        environment.extend(
            prefix
                .chain(wrapped)
                .filter(|name| DIR_VARIABLES.contains(name))
                .map(|name| (name.to_owned(), None)),
        );
        self.within(environment, |walk| walk.assigned_parts(parts, words, src));
    }

    /// Records the simple command of `parts` with its `words`, once the environment holds each of
    /// the [`DIR_VARIABLES`] the command may set, which its words then cannot resolve.
    fn assigned_parts(&mut self, parts: &[Node<'_>], words: Vec<Word>, src: &str) {
        let words: Vec<Word> = (words.into_iter())
            .map(|word| self.resolvable(word, src))
            .collect();
        let mut assignments = Vec::new();
        let mut unsafe_assignment = None;
        for &part in parts {
            if part.kind() == "variable_assignment" {
                if !SAFE_VARIABLES.contains(&assigned(&src[part.byte_range()])) {
                    unsafe_assignment.get_or_insert(assignments.len());
                }
                assignments.push(self.assignment_word(part, src));
                self.visit_children(part, src);
            } else if part.is_named() {
                self.visit(part, src);
            }
        }

        self.simple_command(&words, assignments, unsafe_assignment, Fed::default(), src);
    }

    /// Returns whether the environment sets one of the [`DIR_VARIABLES`], so the analysis cannot
    /// resolve a tilde or a [`DirVariable`] from its own.
    fn sets_dir_variable(&self) -> bool {
        sets_dir_variable(&self.environment)
    }

    /// `word` with no value the analysis knows where it may expand one of the [`DIR_VARIABLES`]
    /// the environment sets.
    fn resolvable(&self, mut word: Word, src: &str) -> Word {
        if self.sets_dir_variable() && reads_dir_variable(&src[word.span.clone()]) {
            word.value = None;
        }
        word
    }

    /// The prefix assignment `node` as [`assignment_word`] reads it, which is not literal where it
    /// may expand one of the [`DIR_VARIABLES`] the environment sets.
    fn assignment_word(&self, node: Node<'_>, src: &str) -> ShellWord {
        let text = &src[node.byte_range()];
        if self.sets_dir_variable() && reads_dir_variable(text) {
            return ShellWord {
                text: text.to_owned(),
                literal: false,
            };
        }
        assignment_word(node, src)
    }

    /// Records a `declare`, `export`, `local`, `readonly`, `typeset`, or `unset` command, which the
    /// grammar parses apart from other simple commands, as the simple command bash runs.
    fn declaration(&mut self, node: Node<'_>, src: &str) {
        let mut cursor = node.walk();
        let mut children = node.children(&mut cursor);
        let keyword = children
            .next()
            .expect("a declaration command starts with its keyword");
        let parts: Vec<Node<'_>> = children.filter(Node::is_named).collect();
        let mut all = vec![Word::token(keyword, src)];
        all.extend(words(&parts, src));
        for &part in &parts {
            if part.kind() == "variable_assignment" {
                self.visit_children(part, src);
            } else {
                self.visit(part, src);
            }
        }
        self.simple_command(&all, Vec::new(), None, Fed::default(), src);
    }

    /// Records a `[ ... ]` test as the simple command of the `[` builtin, whose words are the
    /// tokens between the brackets in source order. `[[ ... ]]` evaluates its operands as
    /// arithmetic, and a token the grammar reads as an operator where bash reads a redirection or a
    /// list operator, such as `>` or `&&`, means the rules would weigh other words than bash runs,
    /// so either leaves the command unrecognized.
    fn test(&mut self, node: Node<'_>, src: &str) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
        let mut tokens = Vec::new();
        let recognized = match children.as_slice() {
            [open, inner @ .., close] if open.kind() == "[" && close.kind() == "]" => inner
                .iter()
                .all(|&child| test_tokens(child, &mut tokens))
                .then_some((*open, *close)),
            _ => None,
        };
        let Some((open, close)) = recognized else {
            self.mark_unrecognized(&src[node.byte_range()]);
            self.visit_children(node, src);
            return;
        };
        for &token in &tokens {
            if token.is_named() {
                self.visit(token, src);
            }
        }
        let mut all = vec![Word::token(open, src)];
        all.extend(words(&tokens, src));
        all.push(Word::token(close, src));
        self.simple_command(&all, Vec::new(), None, Fed::default(), src);
    }

    /// Weighs a `${...}` expansion, which leaves its word expanding, and the words of its operands.
    /// It stays recognized when it only substitutes text, as [`substitutes_text`] decides.
    fn expansion(&mut self, node: Node<'_>, src: &str) {
        self.expands = true;
        if !substitutes_text(node, src) {
            self.mark_unrecognized(&src[node.byte_range()]);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "special_variable_name" | "variable_name" => {}
                "subscript" => {
                    if let Some(index) = child.child_by_field_name("index") {
                        self.visit(index, src);
                    }
                }
                _ => self.visit(child, src),
            }
        }
    }

    /// Records the simple command of `all` and the commands it runs through `find`, led by the
    /// `assignments` before it, of which the one at `unsafe_assignment` is the first to set a
    /// variable outside [`SAFE_VARIABLES`]. `fed` holds what an `xargs` or `find` around it passes
    /// it, such as the placeholder a `find` action's paths replace.
    fn simple_command(
        &mut self,
        all: &[Word],
        mut assignments: Vec<ShellWord>,
        unsafe_assignment: Option<usize>,
        fed: Fed,
        src: &str,
    ) {
        if all.is_empty() {
            return;
        }
        let unwrapped = self.unwrap(all, fed, src);
        let pinned = unsafe_assignment.is_some() || unwrapped.named < unwrapped.starts[0];
        let safe = all[unwrapped.starts[0]..]
            .split_first()
            .filter(|_| !pinned)
            .and_then(|(first, args)| {
                let name = first.literal()?;
                if matches!(name, "cd" | "pushd") {
                    self.enters_workspace(name, args).then(Vec::new)
                } else {
                    safe_operands(name, args)
                }
            });
        let mut operands: Vec<_> = unwrapped
            .reads
            .iter()
            .cloned()
            .map(Operand::certain)
            .collect();
        let mut reads_anything = false;
        let mut finds = Vec::new();
        for (i, &start) in unwrapped.starts.iter().enumerate() {
            let Some(first) = all.get(start) else {
                continue;
            };
            let Some(name) = first
                .literal()
                .filter(|name| !unwrapped.fed.replaced(start, name))
            else {
                if i == 0 {
                    self.mark_unrecognized(span_text(&all[start..], src));
                }
                continue;
            };
            let inherited =
                Inherited::new(&self.environment, &unwrapped, start, &assignments, all, src);
            let shell = self.program(name, &all[start..], i == 0, &inherited, src);
            reads_anything |= shell.anything;
            operands.extend(shell.possible);
            let folded = fold_name(basename(name));
            if folded == "find" {
                finds.push((start + 1, inherited.variables));
            }
            match &safe {
                Some(files) if i == 0 => operands.extend(files.iter().cloned()),
                _ if READERS.contains(&folded.as_str()) => {
                    let args = &all[start + 1..];
                    reads_anything |= self.reader_reads(&folded, args, &mut operands, src)
                        || unwrapped.fed.passes_input();
                }
                _ => {}
            }
        }
        let (reads, moved_anywhere) = self.resolve_operands(&operands);
        reads_anything |= moved_anywhere;

        let mut words: Vec<ShellWord> = all
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let mut word = word.shell_word(src);
                word.literal &= !unwrapped.fed.replaced(i, &word.text);
                word
            })
            .collect();
        let mut named: Vec<ShellWord> = match unsafe_assignment {
            Some(first) => assignments
                .split_off(first)
                .into_iter()
                .chain(words)
                .collect(),
            None => words.split_off(unwrapped.named),
        };
        let programs = unwrapped
            .starts
            .iter()
            .map(|start| named.len() + start - all.len())
            .collect();
        if unwrapped.fed.appends {
            named.push(ShellWord {
                text: INPUT_WORD.to_owned(),
                literal: false,
            });
        }
        self.segments.push(Segment {
            named,
            programs,
            reads,
            reads_anything,
            safe: safe.is_some(),
        });
        for (args, environment) in finds {
            self.within(environment, |walk| walk.find_actions(&all[args..], src));
        }
    }

    /// Runs `walk` on this walk with `environment` as the variables the enclosing commands set.
    fn within<T>(
        &mut self,
        environment: Vec<(String, Option<String>)>,
        walk: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let outer = mem::replace(&mut self.environment, environment);
        let result = walk(self);
        self.environment = outer;
        result
    }

    /// Adds the files reader `name` with `args` reads to `operands`, its own and, for `sed`, those
    /// its scripts read, and returns whether it may read any file.
    fn reader_reads(
        &mut self,
        name: &str,
        args: &[Word],
        operands: &mut Vec<Operand>,
        src: &str,
    ) -> bool {
        let (runs, mut anything) = self.reader_operands(name, args);
        if name == "sed" {
            anything |= self.sed_scripts(args, &runs, operands, src);
        }
        operands.extend(runs.into_iter().flat_map(|(dir, files)| {
            files
                .into_iter()
                .map(move |operand| operand.spelled_from(&dir))
        }));
        anything
    }

    /// Resolves the `operands` of one simple command under each directory it may run in,
    /// recording each file it reads for certain as the command's read, and returns those it may
    /// read, each uncertain operand and each tree it traverses, whose descendants a rule may hold
    /// below its root, with whether a relative operand may name any file after a directory change
    /// the walk cannot spell.
    fn resolve_operands(&mut self, operands: &[Operand]) -> (Vec<ReaderPath<PathTarget>>, bool) {
        let relative = operands
            .iter()
            .any(|operand| Path::new(&operand.read.path).is_relative());
        self.relative_path |= relative;
        let unknown = relative && self.entered.unknown
            || self.entered.unknown_stacked
                && operands
                    .iter()
                    .any(|operand| stack_tilde(&operand.read.path).is_some());
        let mut possible = Vec::new();
        for operand in operands {
            for path in self.spellings(&operand.read.path) {
                let spelled = ReaderPath {
                    path,
                    recursive: operand.read.recursive,
                };
                let read = spelled.resolve(&mut self.paths);
                if operand.certain {
                    self.reads.push(read.path.clone());
                }
                if !operand.certain || read.recursive {
                    possible.push(read);
                }
            }
        }
        (possible, unknown)
    }

    /// Weighs the program `name` that `words` run with their arguments: a shell whose literal
    /// script the walk follows, a directory change, a path relative to the working directory, and,
    /// at the `primary` start bash runs, a builtin, as [`Self::builtin`] weighs it. It returns
    /// what [`Self::nested_script`] does for a shell.
    fn program(
        &mut self,
        name: &str,
        words: &[Word],
        primary: bool,
        inherited: &Inherited<'_>,
        src: &str,
    ) -> ShellReads {
        let args = &words[1..];
        self.relative_path |= is_relative_program(name);
        let base = basename(name);
        let folded = fold_name(base);
        let (shell, builtin) = self.within(inherited.variables.clone(), |walk| {
            let shell = [base, folded.as_str()]
                .into_iter()
                .find(|shell| SHELLS.contains(shell))
                .map(|shell| walk.nested_script(shell, words, inherited, src))
                .unwrap_or_default();
            (shell, !primary || walk.builtin(name, args, src))
        });
        if !builtin {
            self.mark_unrecognized(span_text(words, src));
        }
        if DIRECTORY_CHANGERS.contains(&name) {
            let inside = self.enters_workspace(name, args);
            self.change_dir(inside, words, src);
            self.enter_dir(name, args);
        }
        shell
    }

    /// Returns whether the walk recognizes running `name` with `args`, which holds for any name no
    /// builtin has. The [`DISPATCH_BUILTINS`] are unrecognized, as is a builtin that sets a
    /// variable other than one [`Self::may_set`] admits, or tests a variable whose name may hold a
    /// subscript, since a subscript is an arithmetic context that may run a substitution. `eval` is
    /// recognized with a literal script, which the walk follows, `source` with a literal path,
    /// `set` only with the options [`sets_only_tracing_options`] admits, and `export` only for the
    /// [`SAFE_VARIABLES`].
    fn builtin(&mut self, name: &str, args: &[Word], src: &str) -> bool {
        let local = |word: Option<&Word>| {
            word.and_then(|word| word.value.as_deref())
                .is_some_and(|name| self.may_set(name))
        };
        match name {
            _ if DISPATCH_BUILTINS.contains(&name) => false,
            "." | "source" => match sourced_file(args) {
                Some(path) => {
                    self.open(path, true);
                    true
                }
                None => false,
            },
            "eval" => match literal_script(args) {
                Some(script) => {
                    self.script(&script);
                    true
                }
                None => false,
            },
            "export" => {
                !args.is_empty()
                    && args.iter().all(|word| {
                        declared_name(word, src).is_some_and(|name| SAFE_VARIABLES.contains(&name))
                    })
            }
            // `-i` evaluates each later value as arithmetic, `-n` sends later writes to the
            // variable a value names, and `-x` exports the variable.
            "declare" | "local" | "readonly" | "typeset" => {
                builtin_options(args, src, "", "", "inx").is_some_and(|(_, operands)| {
                    operands
                        .iter()
                        .all(|word| declared_name(word, src).is_some_and(|name| self.may_set(name)))
                })
            }
            "getopts" => local(args.get(1)),
            "jobs" => builtin_options(args, src, "", "", "x").is_some(),
            "mapfile" | "readarray" => builtin_options(args, src, "cdnOsu", "", "C")
                .is_some_and(|(_, operands)| operands.iter().all(|word| local(Some(word)))),
            "printf" => match args {
                [first, rest @ ..] => match first.value.as_deref() {
                    Some(option) if option.starts_with("-v") => match &option[2..] {
                        "" => local(rest.first()),
                        attached => self.may_set(attached),
                    },
                    Some(_) => true,
                    None => rest.first().is_none_or(|word| local(Some(word))),
                },
                [] => true,
            },
            "read" => {
                builtin_options(args, src, "dinNptu", "a", "").is_some_and(|(names, operands)| {
                    names.iter().all(|name| self.may_set(name))
                        && operands.iter().all(|word| local(Some(word)))
                })
            }
            "set" => sets_only_tracing_options(args),
            "test" | "[" => !may_test_a_subscript(args),
            "unset" => builtin_options(args, src, "", "", "")
                .is_some_and(|(_, operands)| operands.iter().all(|word| local(Some(word)))),
            "wait" => builtin_options(args, src, "", "p", "")
                .is_some_and(|(names, _)| names.iter().all(|name| self.may_set(name))),
            _ => true,
        }
    }

    /// Records a directory change by `words`, which may leave the workspace unless it is the first,
    /// it runs once, and `inside` holds.
    fn change_dir(&mut self, inside: bool, words: &[Word], src: &str) {
        self.leaves_workspace |= self.dir_change.is_some() || self.repeats > 0 || !inside;
        self.dir_change
            .get_or_insert_with(|| construct(span_text(words, src)));
    }

    /// Records where `name` with `args` may move the working directory: a `cd` or `pushd` to one
    /// literal target, or a bare `cd` to the home directory, joined to each directory the walk
    /// already knows. A `pushd -n` stacks its literal target as written, or else leaves the stack
    /// unknown. Any other change, or one that may run more than once, leaves the directory unknown.
    fn enter_dir(&mut self, name: &str, args: &[Word]) {
        let target = match dir_target(name, args) {
            DirTarget::Home => home().filter(|_| !self.sets_dir_variable()),
            DirTarget::Path(path) => Some(path.to_owned()),
            DirTarget::Stay(Stacked::Nothing) => return,
            DirTarget::Stay(Stacked::Path(path)) => {
                let path = PathBuf::from(path);
                if !self.entered.stacked.contains(&path) {
                    self.entered.stacked.push(path);
                }
                return;
            }
            DirTarget::Stay(Stacked::Unknown) => {
                self.entered.unknown_stacked = true;
                return;
            }
            DirTarget::Unknown => None,
        };
        let Some(target) = target.filter(|_| self.repeats == 0) else {
            self.entered.unknown = true;
            return;
        };
        let joined: Vec<_> = iter::once(Path::new(""))
            .chain(self.entered.dirs.iter().map(PathBuf::as_path))
            .map(|dir| dir.join(&target))
            .collect();
        for dir in joined {
            if !self.entered.dirs.contains(&dir) {
                self.entered.dirs.push(dir);
            }
        }
    }

    /// The [`reader_operands`] of reader `name` with `args`, expanding each glob in the working
    /// directory and in each directory the walk may have entered, where a match is spelled from
    /// that directory. Each run pairs the operands with the directory they are spelled from, empty
    /// for the working directory.
    fn reader_operands(&self, name: &str, args: &[Word]) -> (Vec<(PathBuf, Vec<Operand>)>, bool) {
        let cwd = self.workspace.cwd();
        let (files, mut anything) = reader_operands(name, args, cwd);
        let mut runs = vec![(PathBuf::new(), files)];
        if args.iter().any(|word| word.glob) {
            for dir in &self.entered.dirs {
                let base = cwd.join(dir);
                let (files, unknown) = reader_operands(name, args, &base);
                anything |= unknown;
                runs.push((base, files));
            }
        }
        (runs, anything)
    }

    /// `path` as the working directory and each directory the walk may have entered name it. A
    /// path that starts with a [`stack_tilde`] also names its rest under the working directory,
    /// each entered directory, and each stacked one as either of those resolves it, any of which
    /// the tilde may name.
    fn spellings(&self, path: &str) -> Vec<String> {
        let mut spellings = vec![path.to_owned()];
        if Path::new(path).is_relative() {
            spellings.extend(
                self.entered
                    .dirs
                    .iter()
                    .map(|dir| dir.join(path).to_string_lossy().into_owned()),
            );
        }
        if let Some(rest) = stack_tilde(path) {
            let rest = rest.trim_start_matches('/');
            let currents =
                iter::once(Path::new("")).chain(self.entered.dirs.iter().map(PathBuf::as_path));
            let named = currents.flat_map(|current| {
                iter::once(current.to_path_buf()).chain(
                    self.entered
                        .stacked
                        .iter()
                        .map(move |stacked| current.join(stacked)),
                )
            });
            spellings.extend(named.map(|dir| match dir.join(rest) {
                spelled if spelled.as_os_str().is_empty() => ".".to_owned(),
                spelled => spelled.to_string_lossy().into_owned(),
            }));
        }
        spellings
    }

    /// Returns whether `name` with `args` stays inside a workspace root, either as a `pushd -n`,
    /// which enters nothing whatever it stacks, or by entering one literal directory in a root
    /// spelled without `..`, which bash's `cd` applies to the spelled path while the gate resolves
    /// links first.
    fn enters_workspace(&mut self, name: &str, args: &[Word]) -> bool {
        let path = match dir_target(name, args) {
            DirTarget::Path(path) => path,
            DirTarget::Stay(_) => return true,
            DirTarget::Home | DirTarget::Unknown => return false,
        };
        Some(path)
            .filter(|path| {
                !Path::new(path)
                    .components()
                    .any(|c| c == Component::ParentDir)
            })
            .is_some_and(|path| self.paths.resolve(path).zone == Zone::Workspace)
    }

    /// Strips the wrappers that lead `words`. The named words keep every wrapper from the first one
    /// allow rules do not see through, that sets a variable outside [`SAFE_VARIABLES`], that is
    /// spelled with a directory, since `./nice` may be any program, or whose options the analysis
    /// cannot read. An option it does not know hides where the command starts, so the program may
    /// then start at any later word. What an enclosing `xargs` or `find` has `fed` the words joins
    /// what the wrappers feed it, and input that replaces part of an assignment's name, which
    /// sets a variable the analysis cannot name, leaves the command unrecognized.
    fn unwrap(&mut self, words: &[Word], fed: Fed, src: &str) -> Unwrapped {
        let mut unwrapped = Unwrapped::default();
        let mut pending = vec![0];
        // Every word from here on has been pending, so an unknown option past it adds none.
        let mut spread = words.len();
        let mut input_runs = false;
        while let Some(mut at) = pending.pop() {
            let primary = unwrapped.starts.is_empty();
            let mut pinned = false;
            while let Some((name, args)) = words[at..].split_first()
                && let Some(name) = name.value.as_deref()
                && let Some(wrapper) = WRAPPERS.iter().find(|w| w.name == basename(name))
            {
                self.relative_path |= is_relative_program(name);
                let run = match wrapper.invocation(args) {
                    Ok(run) => run,
                    Err(unknown) => {
                        unwrapped.fed.appends |= wrapper.kind == Kind::Input;
                        let from = at + 1 + unknown;
                        if wrapper.assignments != Assignments::None {
                            unwrapped.assignments.extend(
                                (from..words.len())
                                    .filter(|&index| src[words[index].span.clone()].contains('=')),
                            );
                        }
                        if from < spread {
                            pending.extend((from..spread).rev());
                            spread = from;
                        }
                        break;
                    }
                };
                if run.elsewhere {
                    self.change_dir(false, &words[at..], src);
                }
                if run.splits {
                    self.mark_unrecognized(span_text(&words[at..], src));
                }
                unwrapped.reads.extend(run.reads);
                if run.start == args.len() {
                    let input = wrapper.kind == Kind::Input;
                    input_runs |= unwrapped.fed.appends && !input;
                    unwrapped.fed.appends |= input && run.replace.is_none();
                    break;
                }
                let sets_unsafe = (run.assignments.iter())
                    .filter_map(|&index| args[index].value.as_deref())
                    .any(|arg| !SAFE_VARIABLES.contains(&env_name(arg)));
                (unwrapped.assignments).extend(run.assignments.iter().map(|index| at + 1 + index));
                at += 1 + run.start;
                if wrapper.kind == Kind::Input {
                    match run.replace {
                        Some(placeholder)
                            if !placeholder.whole
                                || words[at..].iter().any(|word| {
                                    word.value.as_deref() == Some(placeholder.text.as_str())
                                }) =>
                        {
                            unwrapped.fed.replaces.push((at, placeholder));
                        }
                        _ => unwrapped.fed.appends = true,
                    }
                }
                pinned |= sets_unsafe || name != wrapper.name || wrapper.kind != Kind::Transparent;
                if primary && !pinned {
                    unwrapped.named = at;
                }
            }
            if !unwrapped.starts.contains(&at) {
                unwrapped.starts.push(at);
            }
        }
        if input_runs {
            unwrapped.starts.push(words.len());
        }
        unwrapped.fed.appends |= fed.appends;
        unwrapped.fed.replaces.extend(fed.replaces);
        if unwrapped.assignments.iter().any(|&index| {
            (words[index].value.as_deref())
                .is_some_and(|text| unwrapped.fed.replaced(index, env_name(text)))
        }) {
            self.mark_unrecognized(span_text(words, src));
        }
        unwrapped
    }

    /// Records the commands `find`'s `-exec`, `-execdir`, `-ok`, and `-okdir` actions run, each up
    /// to its `;`, or its `+` after `{}`.
    fn find_actions(&mut self, args: &[Word], src: &str) {
        let mut rest = args;
        while let Some(at) = rest.iter().position(|word| {
            word.value
                .as_deref()
                .is_some_and(|value| FIND_RUNNERS.contains(&value))
        }) {
            let body = &rest[at + 1..];
            let end = body
                .iter()
                .enumerate()
                .position(|(i, word)| match word.value.as_deref() {
                    Some(";") => true,
                    Some("+") => i > 0 && body[i - 1].value.as_deref() == Some("{}"),
                    _ => false,
                })
                .unwrap_or(body.len());
            if rest[at]
                .value
                .as_deref()
                .is_some_and(|value| value.ends_with("dir"))
            {
                self.change_dir(false, &rest[at..at + 1 + end], src);
            }
            let found = Placeholder {
                text: DEFAULT_REPLACE.to_owned(),
                whole: false,
            };
            let fed = Fed {
                appends: false,
                replaces: vec![(0, found)],
            };
            self.simple_command(&body[..end], Vec::new(), None, fed, src);
            rest = body.get(end + 1..).unwrap_or_default();
        }
    }

    /// Weighs what `runner`, one of the [`SHELLS`], runs with the arguments after `words[0]`: the
    /// literal script a shell's `-c` runs, which the walk follows so deny rules see the commands
    /// inside it, the script file and startup files it runs, which are reads, or the command
    /// `runuser -u` runs without a shell. A script [`shell_run`] cannot tell, or one the
    /// `inherited` input replaces, leaves the command unrecognized. It returns the files the shell
    /// may run that the walk cannot name for certain or reads as trees.
    fn nested_script(
        &mut self,
        runner: &str,
        words: &[Word],
        inherited: &Inherited<'_>,
        src: &str,
    ) -> ShellReads {
        let args = &words[1..];
        if runner == "runuser"
            && let Ok(run) = RUNUSER.invocation(args)
            && args[..run.start].iter().any(|word| {
                word.value
                    .as_deref()
                    .is_some_and(|arg| arg == "-u" || arg == "--user" || arg.starts_with("--user="))
            })
        {
            let fed = inherited
                .fed
                .for_command_at(inherited.start + 1 + run.start);
            self.simple_command(&args[run.start..], Vec::new(), None, fed, src);
            return ShellReads::default();
        }
        // `su` and `runuser` hand the words after the user name to the user's shell, whose
        // grammar the analysis does not know, so a command option anywhere names their script and
        // any other word there, or an input word, may name a script file. The shell keeps the
        // environment but for `HOME`, which they set to the user's home directory unless an
        // option preserves the environment.
        if matches!(runner, "runuser" | "su") {
            let script = command_option_script(args);
            if let Some(script) = &script {
                if (1..words.len()).any(|index| inherited.replaced(index, &words[index])) {
                    self.mark_unrecognized(span_text(words, src));
                }
                self.script(script);
            }
            let su = su_args(args);
            let home = su.keeps_home.then_some(&HOME_VARIABLE);
            let variables = [&BASH_GRAMMAR, &POSIX_SHELL_GRAMMAR, &ZSH_GRAMMAR]
                .into_iter()
                .flat_map(|grammar| grammar.variables)
                .chain(home);
            let mut reads = self.startup_variables(variables, inherited, words, src);
            reads.anything |= script.is_none() && inherited.fed.passes_input();
            reads.possible.extend(
                su.shell
                    .iter()
                    .filter_map(Word::literal)
                    .filter(|arg| !arg.starts_with('-'))
                    .map(|path| Operand::possible(ReaderPath::exact(path.to_owned()))),
            );
            return reads;
        }
        let shell = shell_run(runner, args);
        let variables = shell_grammar(runner)
            .map_or(&[][..], |grammar| grammar.variables)
            .iter()
            .chain([&HOME_VARIABLE]);
        let mut reads = self.startup_variables(variables, inherited, words, src);
        for (index, file) in shell.startup {
            match expand_tilde(file).filter(|_| !inherited.replaced(1 + index, &args[index])) {
                Some(file) => self.open(&file, true),
                None => reads.anything = true,
            }
        }
        match shell.run {
            ShellRun::Command { index, script } if !inherited.replaced(1 + index, &args[index]) => {
                self.script(script);
            }
            ShellRun::File { index, .. } if inherited.replaced(1 + index, &args[index]) => {
                reads.anything = true;
            }
            ShellRun::File { path, .. } => self.open(path, true),
            ShellRun::Bare => reads.anything |= inherited.fed.appends,
            ShellRun::Stdin => {}
            ShellRun::Command { .. } | ShellRun::Unknown => {
                if let Some(script) = command_option_script(args) {
                    self.script(&script);
                }
                self.mark_unrecognized(span_text(words, src));
            }
        }
        reads
    }

    /// Weighs the startup files that the `inherited` values of the startup `variables` name for a
    /// shell: a file a value names opens, and the directory one names is a tree the shell may read
    /// any file below. The shell `words` run stays unrecognized when a file's name runs a command
    /// substitution as bash expands it.
    fn startup_variables<'v>(
        &mut self,
        variables: impl IntoIterator<Item = &'v StartupVariable>,
        inherited: &Inherited<'_>,
        words: &[Word],
        src: &str,
    ) -> ShellReads {
        let mut reads = ShellReads::default();
        for variable in variables {
            match inherited.value(variable.name).map(Option::as_deref) {
                None | Some(Some("")) => {}
                Some(Some(dir)) if variable.directory => {
                    reads.possible.push(Operand::tree(dir.to_owned()));
                }
                Some(Some(value)) => {
                    if value.contains("$(") || value.contains('`') {
                        self.mark_unrecognized(span_text(words, src));
                    }
                    match expanded_file(value, !inherited.sets_dir_variable()) {
                        Some(files) => files.iter().for_each(|file| self.open(file, true)),
                        None => reads.anything = true,
                    }
                }
                Some(None) => reads.anything = true,
            }
        }
        reads
    }

    fn redirect(&mut self, node: Node<'_>, src: &str) {
        self.visit_children(node, src);
        let mut cursor = node.walk();
        let Some(operator) = node.children(&mut cursor).find(|child| !child.is_named()) else {
            return;
        };
        // tree-sitter-bash has no `<>`, so it parses one as a `<` before an error `>` or as an
        // error `<` before a `>`. Bash drops a line continuation between the two first.
        let read_write = match operator.kind() {
            "<" => src[operator.end_byte()..]
                .trim_start_matches("\\\n")
                .starts_with('>'),
            ">" => src[..operator.start_byte()]
                .trim_end_matches("\\\n")
                .strip_suffix('<')
                .is_some_and(|before| {
                    before.chars().rev().take_while(|&c| c == '\\').count() % 2 == 0
                }),
            _ => false,
        };
        let operator = operator.kind();
        let reads = operator == "<";
        if !reads && !WRITE_OPERATORS.contains(&operator) {
            return;
        }
        let destinations: Vec<Node<'_>> = node
            .children_by_field_name("destination", &mut cursor)
            .collect();
        // Further destinations are the command's words, which `redirect_words` hands to it.
        let Some(&destination) = destinations.first() else {
            return;
        };

        let word = Word::new(&[destination], src);
        let Some(path) = word.literal() else {
            self.mark_unrecognized(&src[node.byte_range()]);
            return;
        };
        if operator == ">&" && (destination.kind() == "number" || path == "-") {
            return;
        }
        if reads || read_write {
            self.open(path, true);
        }
        if !reads || read_write {
            self.open(path, false);
        }
    }

    /// Records that the command opens `path` to read it, or else to write it, unless it is one of
    /// the [`DEVICES`].
    fn open(&mut self, path: &str, read: bool) {
        if DEVICES.contains(&path) {
            return;
        }
        self.relative_path |= Path::new(path).is_relative();
        for spelled in self.spellings(path) {
            let access = self.paths.resolve(&spelled);
            if read {
                self.reads.push(access);
            } else {
                self.writes.push(access);
            }
        }
    }

    /// Weighs what the `sed` call with `args` does beyond reading the operands in `runs`, adding
    /// the files its scripts read to `operands` and returning whether it may read any file. Each
    /// file any argument order or build reads or writes counts for certain. An in-place call
    /// writes each operand and each backup a suffix makes of it, spelled from the operand's
    /// directory, and a suffix that expands, or one GNU's build takes beside `--follow-symlinks`,
    /// which backs up the link's target, leaves the command unrecognized. Each word some build
    /// takes as the script parses with that build's commands, and the files it reads and writes
    /// count, even those before a part the analysis cannot read. A script that expands, that may
    /// run a command, or that comes from a file leaves the command unrecognized, as does one GNU's
    /// build takes that does not parse, since another GNU release may accept it. A word BSD's
    /// build fails to parse adds only the files it writes before the error, where BSD exits.
    fn sed_scripts(
        &mut self,
        args: &[Word],
        runs: &[(PathBuf, Vec<Operand>)],
        operands: &mut Vec<Operand>,
        src: &str,
    ) -> bool {
        let values: Vec<_> = args.iter().map(|word| word.value.as_deref()).collect();
        let parsed = reader_args("sed", &values);
        let backups = &parsed.script.backups;
        let mut opaque = parsed.script.from_file
            || parsed.script.follows_links
                && backups.iter().any(|backup| backup.dialect == Dialect::Gnu)
            || backups
                .iter()
                .any(|backup| backup.suffix.is_none() || args[backup.index].glob);
        if parsed.script.in_place {
            for (dir, files) in runs {
                for file in files {
                    let path = &file.read.path;
                    self.open(&spelled_from(dir, path), false);
                    for backup in backups.iter().filter_map(|backup| backup.path(path)) {
                        self.open(&spelled_from(dir, &backup), false);
                    }
                }
            }
        }
        for word in parsed.script.words {
            let Some(value) = word.value else {
                opaque = true;
                continue;
            };
            let script = sed_script(&value, word.dialect).unwrap_or_else(|writes| {
                opaque |= word.dialect == Dialect::Gnu;
                SedScript {
                    writes,
                    ..SedScript::default()
                }
            });
            opaque |= script.runs;
            operands.extend(script.reads.into_iter().map(Operand::certain));
            for path in &script.writes {
                self.open(path, false);
            }
        }
        if opaque {
            self.mark_unrecognized(span_text(args, src));
        }
        opaque
    }
}

/// `path` as a command in `dir` names it, which is `path` itself for an empty `dir`.
fn spelled_from(dir: &Path, path: &str) -> String {
    dir.join(path).to_string_lossy().into_owned()
}

/// Where a `cd`, `pushd`, or `popd` with `args` moves the working directory.
#[derive(Debug, PartialEq, Eq)]
enum DirTarget<'w> {
    /// The home directory, where a bare `cd` goes.
    Home,
    /// One literal directory.
    Path(&'w str),
    /// Nowhere, since `pushd -n` changes only the directory stack.
    Stay(Stacked<'w>),
    /// Anywhere: `cd -`, `popd`, a stack rotation, a [`stack_tilde`], or a target that expands.
    Unknown,
}

/// What a `pushd -n` adds to the directory stack.
#[derive(Debug, PartialEq, Eq)]
enum Stacked<'w> {
    /// Nothing, since with no operand or a rotation it only reorders the stack.
    Nothing,
    /// The literal directory it names, as written, which bash resolves only when a
    /// [`stack_tilde`] uses it.
    Path(&'w str),
    /// A directory the analysis cannot spell, as a target that expands or a [`stack_tilde`] names.
    Unknown,
}

/// The target of directory changer `name` with `args`, after the options bash's `cd` (`-L`, `-P`,
/// and `-e`) or `pushd` (`-n`) takes and a `--` that ends them. `cd -@` enters a file's extended
/// attributes, which no path names, so it goes anywhere, and `pushd -n` stays.
fn dir_target<'w>(name: &str, args: &'w [Word]) -> DirTarget<'w> {
    let options = match name {
        "cd" => "LPe",
        "pushd" => "n",
        _ => return DirTarget::Unknown,
    };
    let mut rest = args;
    let mut ended = false;
    let mut stays = false;
    while let Some((first, after)) = rest.split_first() {
        let Some(value) = first.literal() else {
            break;
        };
        if value == "--" {
            ended = true;
        } else if !value
            .strip_prefix('-')
            .is_some_and(|flags| !flags.is_empty() && flags.chars().all(|c| options.contains(c)))
        {
            break;
        } else {
            stays |= name == "pushd";
        }
        rest = after;
        if ended {
            break;
        }
    }
    let target = match rest {
        [] if name == "cd" => return DirTarget::Home,
        [] if stays => return DirTarget::Stay(Stacked::Nothing),
        [target] => target
            .literal()
            .filter(|path| !path.is_empty() && stack_tilde(path).is_none()),
        _ => None,
    };
    match target {
        Some(rotation) if stays && !ended && is_stack_rotation(rotation) => {
            DirTarget::Stay(Stacked::Nothing)
        }
        Some(path) if ended || !path.starts_with(['-', '+']) => {
            if stays {
                DirTarget::Stay(Stacked::Path(path))
            } else {
                DirTarget::Path(path)
            }
        }
        _ if stays => DirTarget::Stay(Stacked::Unknown),
        _ => DirTarget::Unknown,
    }
}

/// Returns whether `arg` rotates the directory stack, as in `pushd +1` or `pushd -0`.
fn is_stack_rotation(arg: &str) -> bool {
    arg.strip_prefix(['+', '-'])
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// The children of `command` that make up its simple command: assignments, name, redirections, and
/// arguments, including an argument the grammar reads as an anonymous token, such as a bare `==`,
/// which bash passes as a word.
fn command_parts(command: Node<'_>) -> Vec<Node<'_>> {
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
fn words(nodes: &[Node<'_>], src: &str) -> Vec<Word> {
    nodes
        .chunk_by(|left, right| Gap::between(*left, *right, src).joins())
        .map(|parts| Word::new(parts, src))
        .collect()
}

/// What separates two tokens tree-sitter-bash parses apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gap {
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
    fn value(text: &str) -> String {
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
fn splits_words_like_bash(root: Node<'_>, src: &str) -> bool {
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

/// The lone `-` bash passes after `last`, the last part of a simple command, when tree-sitter-bash
/// drops it, as [`is_dropped_dash`] describes.
fn dropped_dash(last: Node<'_>, src: &str) -> Option<Word> {
    let rest = &src[last.end_byte()..];
    let start = last.end_byte() + rest.find(|c| c != ' ' && c != '\t')?;
    let dash = start..start + 1;
    (start > last.end_byte()
        && src[start..].starts_with('-')
        && src[dash.end..].starts_with([' ', '\t']))
    .then(|| Word {
        span: dash,
        value: Some("-".to_owned()),
        glob: false,
        splits: false,
    })
}

/// The words bash passes to the command `redirect` belongs to, which tree-sitter-bash parses as
/// part of it: the destinations of a file redirection after its first, and the words after a
/// heredoc operator, with those of a file redirection inside the heredoc, in source order.
fn redirect_words(redirect: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = redirect.walk();
    match redirect.kind() {
        "file_redirect" => redirect
            .children_by_field_name("destination", &mut cursor)
            .skip(1)
            .collect(),
        "heredoc_redirect" => {
            let mut words: Vec<Node<'_>> = redirect
                .children_by_field_name("argument", &mut cursor)
                .collect();
            words.extend(
                redirect
                    .children_by_field_name("redirect", &mut cursor)
                    .flat_map(redirect_words),
            );
            words.sort_by_key(Node::start_byte);
            words
        }
        _ => Vec::new(),
    }
}

/// Bash's reserved words other than `time`, which tree-sitter-bash reads as a command name and
/// [`runs_a_reserved_word`] reads past. The grammar gives each compound command it parses a node of
/// its own, so a command word in bash's command position spelled as one of these comes from a
/// mis-parse: of `coproc`, which the grammar lacks, or of a compound after `time`.
const RESERVED_WORDS: &[&str] = &[
    "!", "[[", "]]", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "until", "while", "{", "}",
];

/// Returns whether bash reads the word in the command position of the command made of `parts` as a
/// reserved word: its name, or the first word after a leading `time` keyword and its options.
fn runs_a_reserved_word(parts: &[Node<'_>], src: &str) -> bool {
    if parts
        .first()
        .is_none_or(|name| name.kind() != "command_name")
    {
        return false;
    }
    let mut after_time = false;
    for part in parts {
        match &src[part.byte_range()] {
            "time" => after_time = true,
            "-p" | "--" if after_time => {}
            word => return RESERVED_WORDS.contains(&word),
        }
    }
    false
}

/// The first node under `root`, in source order, that tree-sitter-bash could not parse or inserted
/// to recover from an error.
fn first_error(root: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        if node.has_error() && cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

/// The first line of `text`, cut to [`MAX_CONSTRUCT_CHARS`] characters, as a prompt names an
/// unrecognized construct.
fn construct(text: &str) -> String {
    let text = text.trim();
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(MAX_CONSTRUCT_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None if line.len() < text.len() => format!("{line}..."),
        None => line.to_owned(),
    }
}

/// The source `words` span, from the first one's start to the last one's end.
fn span_text<'s>(words: &[Word], src: &'s str) -> &'s str {
    let start = words.iter().map(|word| word.span.start).min().unwrap_or(0);
    let end = words
        .iter()
        .map(|word| word.span.end)
        .max()
        .unwrap_or(start);
    &src[start..end]
}

/// Collects into `tokens` the tokens of `node`, part of a `[ ... ]` test, that bash passes to `[`
/// as words, or returns `false` when one is a token bash reads otherwise, as a redirection or a
/// list operator, or that only `[[ ... ]]` and arithmetic have.
fn test_tokens<'t>(node: Node<'t>, tokens: &mut Vec<Node<'t>>) -> bool {
    match node.kind() {
        "binary_expression" | "unary_expression" => {
            let mut cursor = node.walk();
            let children: Vec<Node<'t>> = node.children(&mut cursor).collect();
            children.into_iter().all(|child| test_tokens(child, tokens))
        }
        "!"
        | "!="
        | "="
        | "=="
        | "ansi_c_string"
        | "command_substitution"
        | "concatenation"
        | "expansion"
        | "extglob_pattern"
        | "number"
        | "raw_string"
        | "simple_expansion"
        | "string"
        | "test_operator"
        | "word" => {
            tokens.push(node);
            true
        }
        _ => false,
    }
}

/// Operators a `${...}` expansion may apply and still only substitute text: a default, an
/// alternate, or an error for an unset or empty value, a pattern removed or replaced, or a change
/// of case. The grammar names each after the variable.
const TEXT_OPERATORS: &[&str] = &[
    ":-", "-", ":+", "+", ":?", "?", "#", "##", "%", "%%", "/", "//", "/#", "/%", "^", "^^", ",",
    ",,",
];

/// Returns whether `expansion`, a `${...}` node, only substitutes text: it names a value as
/// [`names_a_value`] reads one, led by at most a `#` for its length or followed by at most one of
/// the [`TEXT_OPERATORS`]. Any other form assigns, evaluates an arithmetic context that may run a
/// substitution, as an offset or another subscript does, follows a name to another variable, or
/// transforms the value, as `@P` prompt expansion does. The grammar leaves a pattern operand as one
/// `regex` token even where bash expands a `$` or a backquote in it, so such a pattern counts as
/// unrecognized too.
fn substitutes_text(expansion: Node<'_>, src: &str) -> bool {
    let mut cursor = expansion.walk();
    let parts: Vec<Node<'_>> = expansion
        .children(&mut cursor)
        .filter(|part| !matches!(part.kind(), "${" | "}"))
        .collect();
    let rest = match parts.as_slice() {
        // `${#}` and `${!}`, the argument count and the last background process.
        [only] if matches!(only.kind(), "#" | "!") => return true,
        [length, name] if length.kind() == "#" => return names_a_value(*name, src),
        [name, rest @ ..] if names_a_value(*name, src) => rest,
        _ => return false,
    };
    match rest {
        [] => true,
        [operator, operands @ ..] => {
            let replaces = operator.kind().starts_with('/');
            TEXT_OPERATORS.contains(&operator.kind())
                && operands.iter().all(|operand| match operand.kind() {
                    "/" => replaces,
                    "regex" => !src[operand.byte_range()].contains(['$', '`']),
                    _ => operand.is_named(),
                })
        }
    }
}

/// Returns whether `node`, the name in a `${...}` expansion, names a value without evaluating
/// anything: a variable, a special parameter, or an array element by number or all elements by `@`
/// or `*`.
fn names_a_value(node: Node<'_>, src: &str) -> bool {
    match node.kind() {
        "special_variable_name" | "variable_name" => true,
        "subscript" => node.child_by_field_name("index").is_some_and(|index| {
            index.kind() == "number" || matches!(&src[index.byte_range()], "@" | "*")
        }),
        _ => false,
    }
}

/// Returns whether `element`, part of an `( ... )` array value, sets a key other than a number,
/// which an indexed array evaluates as arithmetic, where a name's value may run a substitution.
fn keys_by_arithmetic(element: Node<'_>, src: &str) -> bool {
    let mut cursor = element.walk();
    let parts: Vec<Node<'_>> = element.children(&mut cursor).collect();
    let text = |node: &Node<'_>| &src[node.byte_range()];
    match parts.as_slice() {
        [open, rest @ ..] if element.kind() == "concatenation" && text(open) == "[" => {
            !matches!(rest, [key, close, ..] if key.kind() == "number" && text(close) == "]")
        }
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
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// Returns whether bash runs the program `name` from a path relative to the working directory, as
/// it does for a name with a `/` that does not start at the root, such as `./build.sh`.
fn is_relative_program(name: &str) -> bool {
    name.contains('/') && Path::new(name).is_relative()
}

// ── Shell ──

/// What a shell runs, as [`shell_run`] reads its arguments.
enum ShellRun<'w> {
    /// The literal script `-c` runs, from the argument at `index`.
    Command { index: usize, script: &'w str },
    /// The literal script file it runs, from the argument at `index`.
    File { index: usize, path: &'w str },
    /// The script `-s` reads from standard input, whose operands are its arguments.
    Stdin,
    /// No script word, so it reads standard input, or runs the script file the first word an
    /// `xargs` appends names.
    Bare,
    /// A script the analysis cannot tell, since a word may expand or an option is one the shell's
    /// [`ShellGrammar`] does not list.
    Unknown,
}

/// A shell's arguments as [`shell_run`] reads them.
struct ShellArgs<'w> {
    run: ShellRun<'w>,
    /// The startup files a long option names, which an interactive shell sources, each with the
    /// index of its argument.
    startup: Vec<(usize, &'w str)>,
}

/// A variable a shell reads as it starts, which names a startup file or a directory of them.
struct StartupVariable {
    name: &'static str,
    /// Whether the value names a directory the shell may read any file below, since the startup
    /// files each shell and mode reads there differ, or else the file itself, which the shell
    /// expands as [`expanded_file`] reads it.
    directory: bool,
}

impl StartupVariable {
    const fn file(name: &'static str) -> Self {
        Self {
            name,
            directory: false,
        }
    }

    const fn directory(name: &'static str) -> Self {
        Self {
            name,
            directory: true,
        }
    }
}

/// The variable every shell reads its startup files below, whatever its grammar.
const HOME_VARIABLE: StartupVariable = StartupVariable::directory("HOME");

/// The variables a leading tilde or a [`DirVariable`] expands from. Where a command sets one, the
/// analysis cannot resolve those expansions from its own environment.
const DIR_VARIABLES: &[&str] = &["HOME", "OLDPWD", "PWD"];

/// The files that `value`, a startup file's name, may name once a shell expands it: as written,
/// as dash leaves it, and with a leading tilde expanded, as bash does it. `None` when it may name
/// any file, since a parameter expansion or command substitution in it, which bash runs, has no
/// value the analysis knows, or its tilde names a user the analysis cannot find or expands from a
/// variable the command sets, which `tilde_known` rules out.
fn expanded_file(value: &str, tilde_known: bool) -> Option<Vec<String>> {
    if value.contains(['$', '`']) || !tilde_known && value.contains('~') {
        return None;
    }
    let expanded = expand_tilde(value)?;
    let mut files = vec![value.to_owned()];
    if expanded != value {
        files.push(expanded);
    }
    Some(files)
}

/// The files a shell may run that the walk cannot name for certain.
#[derive(Default)]
struct ShellReads {
    /// Words that may name a script file it runs, as the words `su` hands its user's shell may,
    /// and the trees of startup files it may run below a directory a startup variable names.
    possible: Vec<Operand>,
    /// Whether it may run any file, such as one an `xargs` input word names or one a startup
    /// variable names with a value that expands.
    anything: bool,
}

/// What a program inherits from the rest of its simple command.
struct Inherited<'c> {
    /// What an `xargs` or `find` around the command passes it.
    fed: &'c Fed,
    /// Where the program's words start among the command's, which `fed` indexes.
    start: usize,
    /// Each variable that the enclosing commands, the command's prefix assignments, and its
    /// wrappers set for the program, in order, with its value when literal.
    variables: Vec<(String, Option<String>)>,
}

impl<'c> Inherited<'c> {
    /// What the program at `start` among the words `all` inherits from the `environment` the
    /// enclosing commands set and from the prefix `assignments`, the wrapper assignments at the
    /// indices `unwrapped` lists, and the input `unwrapped` feeds it. An appending `NAME+=VALUE`,
    /// or a value that input words replace, has no known value.
    fn new(
        environment: &[(String, Option<String>)],
        unwrapped: &'c Unwrapped,
        start: usize,
        assignments: &[ShellWord],
        all: &[Word],
        src: &str,
    ) -> Self {
        let fed = &unwrapped.fed;
        let prefix = assignments.iter().map(|word| {
            let name = assigned(&word.text);
            let value = word.text[name.len()..]
                .strip_prefix('=')
                .filter(|_| word.literal);
            (name, value)
        });
        let wrapped = (unwrapped.assignments.iter())
            .filter(|&&index| index < start)
            .map(|&index| {
                let word = &all[index];
                let literal = word.literal();
                let text = literal.unwrap_or(&src[word.span.clone()]);
                let value = (text.split_once('=').map(|(_, value)| value))
                    .filter(|_| literal.is_some() && !fed.replaced(index, text));
                (env_name(text), value)
            });
        let mut variables = environment.to_vec();
        variables.extend(
            prefix
                .chain(wrapped)
                .map(|(name, value)| (name.to_owned(), value.map(str::to_owned))),
        );
        Self {
            fed,
            start,
            variables,
        }
    }

    /// Returns whether input words replace part of `word`, the program's word at `index`.
    fn replaced(&self, index: usize, word: &Word) -> bool {
        word.value
            .as_deref()
            .is_some_and(|text| self.fed.replaced(self.start + index, text))
    }

    /// The value the program's variables last set `name` to, `None` when none sets it, and
    /// `Some(None)` when that value is unknown.
    fn value(&self, name: &str) -> Option<&Option<String>> {
        self.variables
            .iter()
            .rev()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value)
    }

    /// Returns whether the program's variables set one of the [`DIR_VARIABLES`].
    fn sets_dir_variable(&self) -> bool {
        sets_dir_variable(&self.variables)
    }
}

/// Returns whether `variables` set one of the [`DIR_VARIABLES`].
fn sets_dir_variable(variables: &[(String, Option<String>)]) -> bool {
    variables
        .iter()
        .any(|(name, _)| DIR_VARIABLES.contains(&name.as_str()))
}

/// Returns whether the word written `text` may expand a tilde or a [`DirVariable`], each of which
/// one of the [`DIR_VARIABLES`] names.
fn reads_dir_variable(text: &str) -> bool {
    text.contains('~')
        || ["$HOME", "${HOME}", "$PWD", "${PWD}"]
            .iter()
            .any(|variable| text.contains(variable))
}

/// The variable a `NAME=VALUE` word that `env` or `sudo` takes sets, which is everything before
/// the first `=`, whatever it holds.
fn env_name(word: &str) -> &str {
    word.split_once('=').map_or(word, |(name, _)| name)
}

/// The startup options of a shell, as its manual lists them.
struct ShellGrammar {
    /// The long options it takes before any other, with one dash or two, and whether each names a
    /// startup file in the next word.
    long: &'static [(&'static str, bool)],
    /// The letters a `-` or `+` cluster may hold besides `c` and `s`, each a flag or, as `values`
    /// says, an option that takes a value.
    letters: &'static str,
    values: &'static str,
    /// Whether each value letter in a cluster takes the next word in turn, as bash's do. Otherwise
    /// one before the end of its cluster leaves the run unknown, since zsh, ksh, and mksh take the
    /// rest of the cluster as its value, and dash rejects it.
    stacked_values: bool,
    /// The letters that end the options with their cluster, as zsh's `-b` does.
    ends: &'static str,
    /// Whether a lone `+` ends the options, as in zsh, or is skipped, as in bash.
    plus_ends: bool,
    /// The variables besides [`HOME_VARIABLE`] whose value names a startup file it may run.
    variables: &'static [StartupVariable],
}

/// What `shell` runs with `args`, read as bash reads its startup options: long options first, then
/// `-` and `+` clusters up to the first operand or a `-` or `--` that ends them, where an option
/// that takes a value takes the next word. The first operand is the script `-c` runs, or else,
/// without `-s`, the script file, and with neither the shell reads its script from standard input.
/// A shell without a [`ShellGrammar`] runs an unknown script once it has an option.
fn shell_run<'w>(shell: &str, args: &'w [Word]) -> ShellArgs<'w> {
    let grammar = shell_grammar(shell);
    let mut startup = Vec::new();
    let run = 'run: {
        let (mut command, mut stdin) = (false, false);
        let mut long = true;
        let mut words = args.iter().enumerate();
        let operand = loop {
            let Some((index, word)) = words.next() else {
                break None;
            };
            let Some(arg) = word.literal() else {
                break 'run ShellRun::Unknown;
            };
            if !arg.starts_with(['-', '+']) {
                break Some((index, word));
            }
            let Some(grammar) = grammar else {
                break 'run ShellRun::Unknown;
            };
            if matches!(arg, "-" | "--") || arg == "+" && grammar.plus_ends {
                break words.next();
            }
            let named = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'));
            if long
                && let Some(&(_, sources)) =
                    grammar.long.iter().find(|(name, _)| Some(*name) == named)
            {
                if sources {
                    let Some(file) = words
                        .next()
                        .and_then(|(index, word)| Some((index, word.literal()?)))
                    else {
                        break 'run ShellRun::Unknown;
                    };
                    startup.push(file);
                }
                continue;
            }
            long = false;
            let cluster = &arg[1..];
            for (offset, letter) in cluster.char_indices() {
                match letter {
                    'c' => command = true,
                    's' => stdin = true,
                    _ if !grammar.letters.contains(letter) => break 'run ShellRun::Unknown,
                    _ if grammar.values.contains(letter) => {
                        let last = offset + letter.len_utf8() == cluster.len();
                        if !last && !grammar.stacked_values
                            || words.next().and_then(|(_, word)| word.literal()).is_none()
                        {
                            break 'run ShellRun::Unknown;
                        }
                    }
                    _ => {}
                }
            }
            if cluster.contains(|letter| grammar.ends.contains(letter)) {
                break words.next();
            }
        };
        let operand = operand.map(|(index, word)| (index, word.literal()));
        match (operand, command) {
            (Some((index, Some(script))), true) => ShellRun::Command { index, script },
            (_, true) => ShellRun::Unknown,
            _ if stdin => ShellRun::Stdin,
            (Some((index, Some(path))), false) => ShellRun::File { index, path },
            (Some((_, None)), false) => ShellRun::Unknown,
            (None, false) => ShellRun::Bare,
        }
    };
    ShellArgs { run, startup }
}

/// The grammar of `shell`, or `None` for one whose options differ from the Bourne shells', such as
/// `fish`, `csh`, or `pwsh`.
fn shell_grammar(shell: &str) -> Option<&'static ShellGrammar> {
    match shell {
        "bash" => Some(&BASH_GRAMMAR),
        "zsh" => Some(&ZSH_GRAMMAR),
        "ash" | "dash" | "hush" | "ksh" | "mksh" | "sh" => Some(&POSIX_SHELL_GRAMMAR),
        _ => None,
    }
}

/// Bash's startup options in versions 3.2 through 5.3, one of which may reject an option another
/// takes and then exit before running anything.
const BASH_GRAMMAR: ShellGrammar = ShellGrammar {
    long: &[
        ("debug", false),
        ("debugger", false),
        ("dump-po-strings", false),
        ("dump-strings", false),
        ("help", false),
        ("init-file", true),
        ("login", false),
        ("noediting", false),
        ("noprofile", false),
        ("norc", false),
        ("posix", false),
        ("pretty-print", false),
        ("protected", false),
        ("rcfile", true),
        ("restricted", false),
        ("verbose", false),
        ("version", false),
        ("wordexp", false),
    ],
    // cspell:ignore abefhiklmnoprtuvx BCDEHIOPT
    letters: "abefhiklmnoprtuvxBCDEHIOPT",
    values: "oO",
    stacked_values: true,
    ends: "",
    plus_ends: false,
    variables: &[
        StartupVariable::file("BASH_ENV"),
        StartupVariable::file("ENV"),
    ],
};

/// The startup options POSIX gives `sh`, which the other Bourne-style shells share.
const POSIX_SHELL_GRAMMAR: ShellGrammar = ShellGrammar {
    long: &[],
    // cspell:ignore Cefhilmnouvx
    letters: "abCefhilmnouvx",
    values: "o",
    stacked_values: false,
    ends: "",
    plus_ends: false,
    variables: &[StartupVariable::file("ENV")],
};

/// zsh's startup options among POSIX's, where `-b` and a lone `+` also end the options.
const ZSH_GRAMMAR: ShellGrammar = ShellGrammar {
    ends: "b",
    plus_ends: true,
    variables: &[StartupVariable::directory("ZDOTDIR")],
    ..POSIX_SHELL_GRAMMAR
};

/// The arguments of `su` or `runuser` as util-linux reads them.
struct SuArgs<'w> {
    /// The words it passes its user's shell after the user name.
    shell: &'w [Word],
    /// Whether the shell keeps the `HOME` the command sets: an option preserves the environment,
    /// as `-m`, `-p`, or `--preserve-environment` does, and none asks for a login shell, as `-`,
    /// `-l`, or `--login` does.
    keeps_home: bool,
}

/// What `su` or `runuser` with `args` passes its user's shell, as util-linux reads its arguments:
/// options end at the first operand, a lone `-` after them asks for a login shell, and the next
/// word names the user. A word that expands where an option stands ends the options as the user
/// does, so every later word counts.
fn su_args(args: &[Word]) -> SuArgs<'_> {
    const VALUES: &[&str] = &[
        "command",
        "group",
        "session-command",
        "shell",
        "supp-group",
        "user",
        "whitelist-environment",
    ];
    // cspell:ignore cgGsuw
    const VALUE_LETTERS: &str = "cgGsuw";
    let (mut login, mut preserves) = (false, false);
    let mut i = 0;
    while let Some(arg) = args.get(i).and_then(Word::literal) {
        if arg == "-" || !arg.starts_with('-') {
            break;
        }
        i += 1;
        if arg == "--" {
            break;
        }
        let takes = if let Some(long) = arg.strip_prefix("--") {
            let name = long.split_once('=').map_or(long, |(name, _)| name);
            login |= "login".starts_with(name);
            preserves |= "preserve-environment".starts_with(name);
            !long.contains('=') && VALUES.iter().any(|value| value.starts_with(long))
        } else {
            let cluster = &arg[1..];
            let value = cluster.find(|letter| VALUE_LETTERS.contains(letter));
            let flags = &cluster[..value.unwrap_or(cluster.len())];
            login |= flags.contains('l');
            preserves |= flags.contains(['m', 'p']);
            value.is_some_and(|at| at + 1 == cluster.len())
        };
        i += usize::from(takes);
    }
    if args.get(i).and_then(Word::literal) == Some("-") {
        login = true;
        i += 1;
    }
    SuArgs {
        shell: args.get(i + 1..).unwrap_or(&[]),
        keeps_home: preserves && !login,
    }
}

/// The literal file `source` or `.` with `args` runs, after a `--` that ends its options. `None`
/// for any option, since bash 5.3's `-p` searches a path for the file and other options fail.
fn sourced_file(args: &[Word]) -> Option<&str> {
    match args {
        [first, rest @ ..] if first.literal() == Some("--") => rest.first()?.literal(),
        [first, ..] => first
            .literal()
            .filter(|path| *path == "-" || !path.starts_with('-')),
        [] => None,
    }
}

/// The script the word after a `--command` or a `-c` cluster such as `-lc` names, wherever it
/// stands, or a `--command=` value.
fn command_option_script(args: &[Word]) -> Option<String> {
    args.iter().enumerate().find_map(|(i, word)| {
        let arg = word.value.as_deref()?;
        if let Some(script) = arg.strip_prefix("--command=") {
            return Some(script.to_owned());
        }
        let command = arg.starts_with('-') && !arg.starts_with("--") && arg.contains('c');
        (command || arg == "--command")
            .then(|| args.get(i + 1)?.value.clone())
            .flatten()
    })
}

// ── Wrapper ──

/// A wrapper's option grammar and what running a command through it changes.
struct Wrapper {
    name: &'static str,
    options: &'static [Opt],
    /// Whether `-N` sets a number, as `nice -10` does.
    numeric: bool,
    /// Words after the options that precede the command, such as `timeout`'s duration.
    positionals: usize,
    assignments: Assignments,
    kind: Kind,
    /// Whether the command runs in another directory whatever the options say.
    elsewhere: bool,
}

impl Wrapper {
    /// A wrapper allow rules see through, with no options.
    const TRANSPARENT: Self = Self {
        name: "",
        options: &[],
        numeric: false,
        positionals: 0,
        assignments: Assignments::None,
        kind: Kind::Transparent,
        elsewhere: false,
    };

    /// A runner allow rules must name, with no options.
    const RUNNER: Self = Self {
        kind: Kind::Runner,
        ..Self::TRANSPARENT
    };

    /// Parses the options that lead `args`, or returns the index of the first word that hides where
    /// the command starts: an option the grammar does not know or one that expands, or any word
    /// before the command that bash may expand into other than one word.
    fn invocation(&self, args: &[Word]) -> Result<Invocation, usize> {
        let parsed = self.options(args);
        let end = parsed.as_ref().map_or_else(|&at| at, |run| run.start);
        match args[..end].iter().position(|word| word.splits) {
            Some(at) => Err(at),
            None => parsed,
        }
    }

    fn options(&self, args: &[Word]) -> Result<Invocation, usize> {
        let mut run = Invocation {
            start: args.len(),
            elsewhere: self.elsewhere,
            ..Invocation::default()
        };
        let mut i = 0;
        while let Some(word) = args.get(i) {
            let at = i;
            let arg = word.value.as_deref().ok_or(at)?;
            if arg == "--" {
                i += 1;
                break;
            }
            if !arg.starts_with('-') || (arg == "-" && self.option("-").is_none()) {
                if self.assignments == Assignments::Among && self.assignments.takes(arg) {
                    run.assignments.push(at);
                    i += 1;
                    continue;
                }
                break;
            }
            i += 1;
            if arg == "-" {
                continue;
            }
            if arg.starts_with("--") {
                let (name, attached) = arg
                    .split_once('=')
                    .map_or((arg, None), |(name, value)| (name, Some(value)));
                let option = self.option(name).ok_or(at)?;
                let value = match (option.value, attached) {
                    (Value::None, Some(_)) => return Err(at),
                    (Value::Required, None) => {
                        let Some(next) = args.get(i) else {
                            return Ok(run);
                        };
                        i += 1;
                        Arg::of(next)
                    }
                    (_, Some(value)) => Arg::Literal(value.to_owned()),
                    (_, None) => Arg::None,
                };
                run.apply(option.effect, value).ok_or(at)?;
                continue;
            }
            let cluster = &arg[1..];
            if self.numeric && cluster.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            for (offset, c) in cluster.char_indices() {
                let option = self.short(c).ok_or(at)?;
                let rest = &cluster[offset + c.len_utf8()..];
                let value = match option.value {
                    Value::None => Arg::None,
                    _ if !rest.is_empty() => Arg::Literal(rest.to_owned()),
                    Value::Optional => Arg::None,
                    Value::Required => {
                        let Some(next) = args.get(i) else {
                            return Ok(run);
                        };
                        i += 1;
                        Arg::of(next)
                    }
                };
                run.apply(option.effect, value).ok_or(at)?;
                if option.value != Value::None {
                    break;
                }
            }
        }
        i += self.positionals;
        while args
            .get(i)
            .and_then(|arg| arg.value.as_deref())
            .is_some_and(|arg| self.assignments.takes(arg))
        {
            run.assignments.push(i);
            i += 1;
        }
        run.start = i.min(args.len());
        Ok(run)
    }

    fn option(&self, name: &str) -> Option<&Opt> {
        self.options.iter().find(|option| option.name == name)
    }

    /// The single-letter option `-c`, which may share a cluster with others, as in `-nE`.
    fn short(&self, c: char) -> Option<&Opt> {
        self.options.iter().find(|option| {
            option
                .name
                .strip_prefix('-')
                .is_some_and(|letter| letter.chars().eq([c]))
        })
    }
}

/// One option of a wrapper.
#[derive(Clone, Copy)]
struct Opt {
    name: &'static str,
    value: Value,
    effect: Effect,
}

impl Opt {
    const fn with(self, effect: Effect) -> Self {
        Self { effect, ..self }
    }
}

const fn flag(name: &'static str) -> Opt {
    Opt {
        name,
        value: Value::None,
        effect: Effect::None,
    }
}

const fn valued(name: &'static str) -> Opt {
    Opt {
        value: Value::Required,
        ..flag(name)
    }
}

const fn optional(name: &'static str) -> Opt {
    Opt {
        value: Value::Optional,
        ..flag(name)
    }
}

/// Whether an option takes a value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Value {
    None,
    /// A value as the next word or attached, as in `-n 5`, `-n5`, or `--signal=KILL`.
    Required,
    /// A value only when attached, as in `xargs -i{}`.
    Optional,
}

/// Where a wrapper takes `NAME=VALUE` words that set the command's environment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Assignments {
    None,
    /// After the options or a `--` that ends them, as `env` takes any word holding a `=`, whatever
    /// its name, even one that starts with `-`.
    After,
    /// Before, between, and after the options, as `sudo` takes any word holding a `=` that does not
    /// start with `/` or `=`.
    Among,
}

impl Assignments {
    /// Returns whether a wrapper that takes assignments this way reads `arg` as one.
    fn takes(self, arg: &str) -> bool {
        match self {
            Self::None => false,
            Self::After => arg.contains('='),
            Self::Among => arg.contains('=') && !arg.starts_with(['/', '=']),
        }
    }
}

/// What a wrapper changes about the command it runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// It changes only how the command runs, so allow rules and known-safe reads see through it.
    Transparent,
    /// It changes who runs the command, so it stays in the words an allow rule must name.
    Runner,
    /// It appends the words it reads from its input to the command, as `xargs` does, so it stays in
    /// the words an allow rule must name too.
    Input,
}

/// The value an option takes.
enum Arg {
    None,
    Literal(String),
    /// A value that expands when the command runs.
    Expands,
}

impl Arg {
    fn of(word: &Word) -> Self {
        word.value.clone().map_or(Self::Expands, Self::Literal)
    }
}

/// What an option changes about the command the wrapper runs.
#[derive(Clone, Copy)]
enum Effect {
    None,
    /// The command runs in another directory.
    Directory,
    /// The value is a placeholder that each input word replaces, in place of being appended.
    Replace,
    /// The value is a placeholder that the input words replace when it stands as a whole word, as
    /// BSD `xargs -J` takes it, and that leaves them appended otherwise.
    Insert,
    /// The value names a file the wrapper reads.
    Reads,
    /// The value is a command line the wrapper splits into words of its own, as `env -S` does,
    /// which the analysis does not follow.
    Splits,
}

/// How a wrapper runs its command, from the options before it.
#[derive(Default)]
struct Invocation {
    /// Where the command starts among the wrapper's arguments, which is past them all when it runs
    /// none.
    start: usize,
    elsewhere: bool,
    /// The placeholder that input words replace, when an option sets one.
    replace: Option<Placeholder>,
    /// The files an option names for the wrapper to read.
    reads: Vec<String>,
    /// Whether an option hands the wrapper a command line it splits itself.
    splits: bool,
    /// The arguments the wrapper takes as `NAME=VALUE` assignments.
    assignments: Vec<usize>,
}

impl Invocation {
    /// Records an option's `effect` with its `value`, or returns `None` when the effect needs a
    /// value the analysis cannot read.
    fn apply(&mut self, effect: Effect, value: Arg) -> Option<()> {
        match effect {
            Effect::None => {}
            Effect::Directory => self.elsewhere = true,
            Effect::Replace | Effect::Insert => {
                let text = match value {
                    Arg::None => DEFAULT_REPLACE.to_owned(),
                    Arg::Literal(value) => value,
                    Arg::Expands => return None,
                };
                self.replace = Some(Placeholder {
                    text,
                    whole: matches!(effect, Effect::Insert),
                });
            }
            Effect::Reads => match value {
                Arg::Literal(path) => self.reads.push(path),
                Arg::None | Arg::Expands => return None,
            },
            Effect::Splits => self.splits = true,
        }
        Some(())
    }
}

/// A word that input words replace when the command runs.
#[derive(Clone)]
struct Placeholder {
    text: String,
    /// Whether only a word that is exactly `text` is replaced.
    whole: bool,
}

/// What the wrappers leading a simple command leave to run, as indices into its words.
#[derive(Default)]
struct Unwrapped {
    /// Where the words an allow rule must name start.
    named: usize,
    /// Where the program may start: first where it does when every wrapper option is known, then
    /// each word past an option that is not. The word count stands for the input words of an
    /// `xargs` whose wrapped wrapper takes its command from them.
    starts: Vec<usize>,
    fed: Fed,
    /// The files wrapper options read.
    reads: Vec<String>,
    /// The `NAME=VALUE` words that wrappers such as `env` take as assignments, or may take past an
    /// option the analysis cannot read.
    assignments: Vec<usize>,
}

/// The words an `xargs` or `find` passes a command, as indices into its words.
#[derive(Default)]
struct Fed {
    /// Whether an `xargs` appends its input words to the command.
    appends: bool,
    /// The placeholders `xargs` or `find` replaces with its input words or found paths, each with
    /// where its command starts.
    replaces: Vec<(usize, Placeholder)>,
}

impl Fed {
    /// What reaches the command whose words start at `offset` among these words.
    fn for_command_at(&self, offset: usize) -> Self {
        Self {
            appends: self.appends,
            replaces: self
                .replaces
                .iter()
                .map(|(start, placeholder)| (start.saturating_sub(offset), placeholder.clone()))
                .collect(),
        }
    }

    /// Returns whether `xargs` or `find` passes words to the command, which may name any file.
    fn passes_input(&self) -> bool {
        self.appends || !self.replaces.is_empty()
    }

    /// Returns whether input words replace part of `text`, the word at `index`.
    fn replaced(&self, index: usize, text: &str) -> bool {
        self.replaces.iter().any(|(start, placeholder)| {
            index >= *start
                && if placeholder.whole {
                    text == placeholder.text
                } else {
                    text.contains(placeholder.text.as_str())
                }
        })
    }
}

/// The variable a `NAME=VALUE`, `NAME+=VALUE`, or `NAME[i]=VALUE` assignment sets.
fn assigned(assignment: &str) -> &str {
    assignment
        .find(['=', '+', '['])
        .map_or(assignment, |end| &assignment[..end])
}

/// Returns whether `name` is a shell identifier: a letter or `_`, then letters, digits, or `_`.
fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Returns whether a write to the variable `name` can only change later `$name` expansions, which
/// the analysis already reads as unknown: it is an identifier with a lowercase letter, which no
/// variable a noninteractive bash reads has, and the environment `bash` starts with does not set
/// it, as `inherited` tells, so the write exports nothing.
fn is_local_name(name: &str, inherited: &dyn Fn(&str) -> bool) -> bool {
    is_identifier(name) && name.bytes().any(|b| b.is_ascii_lowercase()) && !inherited(name)
}

/// The variable a `declare`-family or `export` argument names: its value up to an `=` or `+=`, or
/// when the value expands, the identifier it is written with before an `=`. `None` for an option, a
/// name the analysis cannot read, or a literal value that assigns `(...)`, which the builtin parses
/// again as an array whose keys and values may run substitutions the walk never sees.
fn declared_name<'w>(word: &'w Word, src: &'w str) -> Option<&'w str> {
    let text = word.value.as_deref().unwrap_or(&src[word.span.clone()]);
    let (name, value) = text.split_once('=').unwrap_or((text, ""));
    let name = name.strip_suffix('+').unwrap_or(name);
    let reparsed = word.value.is_some() && value.starts_with('(');
    (is_identifier(name) && !reparsed).then_some(name)
}

/// The script `eval` runs from `args`, joined by spaces after a leading `--`, which `eval` drops,
/// or `None` when one of them expands.
fn literal_script(args: &[Word]) -> Option<String> {
    let args = match args {
        [first, rest @ ..] if first.value.as_deref() == Some("--") => rest,
        _ => args,
    };
    args.iter()
        .map(|word| word.value.as_deref())
        .collect::<Option<Vec<_>>>()
        .map(|values| values.join(" "))
}

/// The variables a builtin's options name and the operands after them, read past option clusters
/// where each letter in `valued` takes a value, and each in `naming` takes a variable's name.
/// `None` when an option holds a letter in `refused`, whose effect the analysis does not follow, or
/// a word that expands may be an option or a name. A word written from a letter or `_`, as an
/// assignment with an array value is, cannot expand to an option. `--` ends the options.
fn builtin_options<'w>(
    args: &'w [Word],
    src: &str,
    valued: &str,
    naming: &str,
    refused: &str,
) -> Option<(Vec<&'w str>, &'w [Word])> {
    let mut names = Vec::new();
    let mut at = 0;
    while let Some(word) = args.get(at) {
        let Some(value) = word.value.as_deref() else {
            src[word.span.clone()]
                .starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
                .then_some(())?;
            break;
        };
        let Some(cluster) = value
            .strip_prefix('-')
            .filter(|cluster| !cluster.is_empty())
        else {
            break;
        };
        at += 1;
        if cluster == "-" {
            break;
        }
        for (offset, letter) in cluster.char_indices() {
            if refused.contains(letter) {
                return None;
            }
            if !valued.contains(letter) && !naming.contains(letter) {
                continue;
            }
            let attached = &cluster[offset + letter.len_utf8()..];
            let value = if attached.is_empty() {
                at += 1;
                args.get(at - 1).map(|word| word.value.as_deref())
            } else {
                Some(Some(attached))
            };
            if naming.contains(letter) {
                names.push(value.flatten()?);
            }
            break;
        }
    }
    Some((names, &args[at.min(args.len())..]))
}

/// Returns whether `args` only turn on or off the `set` options that make a script stop on errors
/// or trace what it runs: `e`, `u`, `v`, and `x` as letters, and `errexit`, `nounset`, `pipefail`,
/// `verbose`, and `xtrace` by name after `-o` or `+o`.
fn sets_only_tracing_options(args: &[Word]) -> bool {
    let mut values = args.iter().map(|word| word.value.as_deref());
    let mut any = false;
    while let Some(value) = values.next() {
        let Some(cluster) = value
            .and_then(|value| value.strip_prefix(['-', '+']))
            .filter(|cluster| !cluster.is_empty())
        else {
            return false;
        };
        let (letters, named) = cluster
            .strip_suffix('o')
            .map_or((cluster, false), |letters| (letters, true));
        if !letters.chars().all(|letter| "euvx".contains(letter)) {
            return false;
        }
        if named
            && !values.next().flatten().is_some_and(|name| {
                ["errexit", "nounset", "pipefail", "verbose", "xtrace"].contains(&name)
            })
        {
            return false;
        }
        any = true;
    }
    any
}

/// Returns whether `test` or `[` may evaluate a word as a variable's name with `-v` or `-R`, whose
/// subscript runs any substitution in it: a word that is or may expand to either operator, followed
/// by one that expands or holds a `[`.
fn may_test_a_subscript(args: &[Word]) -> bool {
    args.windows(2).any(|pair| {
        pair[0]
            .value
            .as_deref()
            .is_none_or(|operator| matches!(operator, "-v" | "-R"))
            && pair[1]
                .value
                .as_deref()
                .is_none_or(|name| name.contains('['))
    })
}

// ── Word ──

/// One word of a simple command.
struct Word {
    /// Where the word is written in the source.
    span: Range<usize>,
    /// The value the shell gives the word after tilde expansion and quote removal, or `None` when
    /// part of it expands at run time.
    value: Option<String>,
    /// Whether an unquoted `*`, `?`, `[`, or `{` leaves the word to pathname or brace expansion.
    glob: bool,
    /// Whether bash may expand the word into other than one word, as [`splits`] decides.
    splits: bool,
}

impl Word {
    /// The word bash reads from `parts`, which the grammar may split one word into with no blank
    /// between them, as in `=="x"`.
    fn new(parts: &[Node<'_>], src: &str) -> Self {
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
    fn token(node: Node<'_>, src: &str) -> Self {
        Self {
            span: node.byte_range(),
            value: Some(src[node.byte_range()].to_owned()),
            glob: false,
            splits: false,
        }
    }

    /// The word's value when it is known and leaves nothing to pathname or brace expansion.
    fn literal(&self) -> Option<&str> {
        self.value.as_deref().filter(|_| !self.glob)
    }

    fn shell_word(&self, src: &str) -> ShellWord {
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
fn home() -> Option<String> {
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
/// on the changes before it, which [`Walk::spellings`] weighs.
fn expand_tilde(path: &str) -> Option<String> {
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
fn stack_tilde(path: &str) -> Option<&str> {
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

// ── Known-Safe Commands ──

/// The operators and the closing `]` of a `test` or `[` call.
const TEST_OPERATORS: &[&str] = &["!", "!=", "(", ")", "<", "=", "==", ">", "]"];

/// The words that may name a file read by the shipped known-safe read-only command `name`, or
/// `None` when `name` is no such command or `args` take it out of the safe set. Every argument must
/// be literal and free of globs, so each file it reads is one the gate can check. Every word of a
/// `test` or `[` call other than an option or a [`TEST_OPERATORS`] word counts as a file, so a
/// string it compares reads as one too.
fn safe_operands(name: &str, args: &[Word]) -> Option<Vec<Operand>> {
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
fn sed_script(script: &str, dialect: Dialect) -> Result<SedScript, Vec<String>> {
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
struct SedScript {
    reads: Vec<String>,
    writes: Vec<String>,
    runs: bool,
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
fn reader_operands(name: &str, args: &[Word], cwd: &Path) -> (Vec<Operand>, bool) {
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
struct ReaderArgs {
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
    script: ScriptArgs,
    /// Words that supply an option's value in any supported argument order or build.
    option_values: Vec<usize>,
    /// Whether it may read a file the arguments do not name: a word expands, or an option names a
    /// file that lists the files to read.
    unknown: bool,
}

/// Where a reader takes its pattern or script from, and whether it rewrites its files.
#[derive(Debug, Default, PartialEq, Eq)]
struct ScriptArgs {
    /// Each word that any supported argument order or build takes as the pattern or script.
    words: Vec<ScriptWord>,
    /// Whether some supported argument order or build reads the pattern or script from a file.
    from_file: bool,
    /// Whether some supported argument order or build rewrites its file operands in place.
    in_place: bool,
    /// Whether some supported argument order or build rewrites a symlink's target in place.
    follows_links: bool,
    /// Each backup suffix some supported argument order or build gives an in-place rewrite.
    backups: Vec<Backup>,
}

/// The backup suffix one build gives an in-place rewrite, from the word at `index`.
#[derive(Debug, PartialEq, Eq)]
struct Backup {
    dialect: Dialect,
    index: usize,
    /// The suffix, or `None` when its word expands.
    suffix: Option<String>,
}

impl Backup {
    /// Where this build moves `file` before rewriting it, unless the suffix expands. GNU's build
    /// replaces each `*` in the suffix with `file` as written, while BSD's, which has no template,
    /// and GNU's for a suffix without `*` append the suffix.
    fn path(&self, file: &str) -> Option<String> {
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
struct ScriptWord {
    dialect: Dialect,
    index: usize,
    value: Option<String>,
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
fn reader_args(name: &str, values: &[Option<&str>]) -> ReaderArgs {
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
enum Dialect {
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
    use indoc::indoc;

    use super::*;
    use crate::util::path::local_testing::{abs, canonical_tempdir};
    use crate::util::path::slash_separated;
    use crate::util::path::testing::HOME_VAR;

    fn parse(command: &str) -> CommandTarget {
        parse_inheriting(command, &[])
    }

    /// Parses `command` where the environment `bash` starts with sets only `inherited`.
    fn parse_inheriting(command: &str, inherited: &[&str]) -> CommandTarget {
        CommandTarget::parse_inheriting(command, &Workspace::new(Path::new("/repo")), &|name| {
            inherited.contains(&name)
        })
    }

    fn is_safe(command: &str) -> bool {
        parse(command).segments[0].is_safe()
    }

    fn resolve(path: &str, workspace: &Workspace) -> PathTarget {
        PathResolver::new(workspace).resolve(path)
    }

    fn joined(words: &[ShellWord]) -> String {
        words
            .iter()
            .map(ShellWord::text)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The program `segment` runs when every wrapper option is known.
    fn program(segment: &Segment) -> &[ShellWord] {
        segment.programs().next().unwrap()
    }

    mod command_target {
        use super::*;

        mod parse {
            use super::*;

            /// Each simple command's named words, joined on spaces.
            fn texts(command: &str) -> Vec<String> {
                parse(command)
                    .segments
                    .iter()
                    .map(|segment| joined(segment.named()))
                    .collect()
            }

            fn words(command: &str) -> Vec<String> {
                program(&parse(command).segments[0])
                    .iter()
                    .map(|word| word.text().to_owned())
                    .collect()
            }

            #[test]
            fn hands_the_words_a_redirection_carries_to_the_command_bash_gives_them() {
                for (command, expected) in [
                    ("cat <<'EOF' a b\nx\nEOF", &["cat a b"][..]),
                    ("cat x <<'EOF' a\nx\nEOF", &["cat x a"]),
                    ("true && cat <<'EOF' a\nx\nEOF", &["true", "cat a"]),
                    ("ls | cat <<'EOF' a\nx\nEOF", &["ls", "cat a"]),
                    ("! git <<'EOF' push\nx\nEOF", &["git push"]),
                    ("FOO=1 <<'EOF' git push\nx\nEOF", &["FOO=1 git push"]),
                    (">/dev/null <<'EOF' git push\nx\nEOF", &["git push"]),
                    ("git <<'EOF' 2>&1 push -f\nx\nEOF", &["git push -f"]),
                    ("git >/dev/null push -f", &["git push -f"]),
                    ("echo >&2 hi", &["echo hi"]),
                ] {
                    assert_eq!(texts(command), expected, "{command:?}");
                }
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
                assert!(parse("cargo test --all").unrecognized().is_none());
            }

            #[test]
            fn strips_leading_assignments_and_keeps_words_unquoted() {
                assert_eq!(texts(r#"LANG=C TZ="x y" cargo test"#), ["cargo test"]);
                assert_eq!(
                    words(indoc! {r#"
                        "rm" -rf 'a b' c\ d "e\"f\
                        g\h" "$X"
                    "#}),
                    ["rm", "-rf", "a b", "c d", "e\"fg\\h", "\"$X\""]
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
                    ("PATH=/tmp ls", "PATH=/tmp ls", "ls"),
                    (
                        "env BASH_ENV=./x.sh ./build.sh",
                        "env BASH_ENV=./x.sh ./build.sh",
                        "./build.sh",
                    ),
                    (
                        "nice env LD_PRELOAD=./x.so nohup cat a",
                        "env LD_PRELOAD=./x.so nohup cat a",
                        "cat",
                    ),
                ] {
                    let command = parse(command);
                    let segment = &command.segments[0];
                    assert_eq!(joined(segment.named()), text, "{command:?}");
                    assert_eq!(program(segment)[0].text(), name, "{command:?}");
                    assert!(!segment.is_safe(), "{command:?}");
                    assert!(command.unrecognized().is_none(), "{command:?}");
                }
                for (command, text) in [
                    ("LD_PRELOAD=$PWD/x.so cat a", "LD_PRELOAD=./x.so cat a"),
                    ("LD_PRELOAD=$PWD.so cat a", "LD_PRELOAD=$PWD.so cat a"),
                ] {
                    let segment = &parse(command).segments[0];

                    assert_eq!(joined(segment.named()), text, "{command:?}");
                }
                assert!(is_safe("LANG=C TZ=UTC env NO_COLOR=1 cat a"));
            }

            #[test]
            fn flags_a_command_that_sets_a_variable_bash_finds_or_expands_commands_by() {
                for command in [
                    "BASH_CMDS[ls]=/bin/rm; ls -rf x",
                    "BASH_CMDS+=([ls]=/bin/rm); ls -rf x",
                    "declare -A BASH_CMDS=([ls]=/bin/rm); ls -rf x",
                    "POSIXLY_CORRECT=1\nBASH_ALIASES[ls]=rm\nls -rf x",
                    "export PATH=/tmp:$PATH; ls",
                    "export -n FOO",
                    "X=1; echo hi",
                    "for IFS in x; do ls; done",
                    "local -n r=BASH_CMDS",
                    ": ${BASH_CMDS[ls]:=/bin/rm}",
                    "(( PATH = 1 ))",
                    "echo $(( IFS = 1 ))",
                    "a[IFS=1]=2",
                    "read -r 'BASH_CMDS[ls]' <<< /bin/rm",
                    "printf -v PATH %s /tmp",
                    "declare \"BASH_CMDS[ls]=/bin/rm\"; ls -rf x",
                    "declare BASH_CMDS\\[ls\\]=/bin/rm; ls -rf x",
                    "declare 'BASH_CMDS'[ls]=/bin/rm; ls -rf x",
                    "builtin declare \"BASH_CMDS[ls]=/bin/rm\"; ls -rf x",
                    "command typeset \"BASH_CMDS[ls]=/bin/rm\"; ls -rf x",
                    "builtin export PATH=/tmp",
                    "unset PATH",
                    "let PATH=1",
                    "sh -c 'BASH_CMDS[ls]=/bin/rm; ls -rf x'",
                    "declare -a 'x=([$(id)]=1)'",
                    "declare 'x=($(id))'",
                    "local x='([0]=$(id))'",
                    "typeset -A m+='([k]=$(id))'",
                    "export 'RUST_LOG=($(id))'",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "echo $PATH ${PATH:-x} ${#PATH} ${BASH_CMDS[0]}",
                    "x=1; echo hi",
                    "LANG=C ls",
                    "export RUST_LOG=debug; cargo test",
                    "declare -a list=([2]=a b)",
                    "read -r line",
                    "read -p 'ENV? ' answer",
                    "printf '%s\\n' 'Set HOME first'",
                    "test -v PATH",
                    "[ -v HOME ]",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn flags_a_statement_level_assignment_before_a_later_command() {
                for command in [
                    "GIT_EDITOR='rm -rf y'; git commit --amend",
                    "EDITOR=vi; git rebase -i HEAD~2",
                    "FOO=bar\ncat file",
                    "FOO=bar",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "RUST_LOG=debug; cargo test",
                    "LANG=C; ls",
                    "FOO=bar ls",
                    "http_proxy=http://x; curl example.com",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
                assert!(
                    parse_inheriting("http_proxy=http://x; curl example.com", &["http_proxy"])
                        .unrecognized()
                        .is_some()
                );
            }

            #[test]
            fn flags_an_expansion_that_runs_code_or_evaluates_a_name() {
                for command in [
                    "cargo build ${x:n}",
                    "echo ${arr[n]}",
                    "echo ${!n}",
                    "echo ${x@P}",
                    "echo ${x:1:2}",
                    "echo ${x%$(id)}",
                    "echo ${arr[k]}",
                    "a=([k]=v)",
                    "test -v \"$n\"",
                    "[ -v \"$n\" ]",
                    "printf -v \"$n\" hi",
                    "read -r 'a[$(rm -rf y)]'",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "echo $x ${x} ${x:-a} ${x-a} ${x:+a} ${x+a}",
                    "echo ${#x} ${x/a/b} ${x//a} ${x%.txt} ${x^^} ${arr[@]} ${#} ${!}",
                    "a=([0]=x \"$y\" z)",
                    "cargo build ${profile:-release}",
                    "test -f file",
                    "read -r line",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn leaves_a_variable_the_shell_or_a_later_program_reads_unrecognized() {
                for command in [
                    "PATH=.; ls",
                    "HOME=/; cat ~/x",
                    "export GIT_PAGER=sh; git log",
                    "declare -x PATH",
                    "declare \"BASH_CMDS[ls]=/bin/rm\"; ls -rf x",
                    "declare BASH_CMDS\\[ls\\]=/bin/rm; ls -rf x",
                    "declare 'BASH_CMDS'[ls]=/bin/rm; ls -rf x",
                    "for PATH in .; do ls; done",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "export RUST_LOG=debug; cargo test",
                    "for f in a b; do cat \"$f\"; done",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn expands_a_leading_tilde_home_or_working_directory_only() {
                temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                    let home = abs("/home/u");
                    let expanded = PathBuf::from(&home).join("x");
                    assert_eq!(
                        words(r#"cat ~/x "~/y" a~b"#),
                        [
                            "cat".to_owned(),
                            expanded.to_string_lossy().into_owned(),
                            "~/y".to_owned(),
                            "a~b".to_owned()
                        ]
                    );
                    assert_eq!(
                        words("cat ~no-such-user/x ~- ~1/x ~+2 ~-0/x"),
                        ["cat", "~no-such-user/x", "~-", "~1/x", "~+2", "~-0/x"],
                        "an unknown user and the directory stack are kept as written"
                    );
                    assert_eq!(words("cat ~+/x ~+"), ["cat", "./x", "."]);
                    assert_eq!(
                        words("env A=~/x B=~+/y 1=~/z C=a=~/w"),
                        [
                            "env".to_owned(),
                            format!("A={}", expanded.to_string_lossy()),
                            "B=./y".to_owned(),
                            "1=~/z".to_owned(),
                            "C=a=~/w".to_owned()
                        ],
                        "a tilde after the first `=` of an assignment expands"
                    );
                    assert_eq!(
                        words("cat ~\\\n/x"),
                        ["cat", expanded.to_str().unwrap()],
                        "a line continuation goes before the tilde prefix is read"
                    );
                    assert_eq!(
                        words(r#"cat ~"/x" ~+"/x" ~root"/x" ~\r/x ~/"x""#),
                        [
                            "cat",
                            "~/x",
                            "~+/x",
                            "~root/x",
                            "~r/x",
                            expanded.to_str().unwrap()
                        ],
                        "a quoted tilde prefix is kept as written"
                    );
                    assert_eq!(
                        words(r#"cat $PWD/x "${PWD}/y" "$PWD" a$PWD"#),
                        ["cat", "./x", "./y", ".", "a$PWD"]
                    );
                    assert_eq!(
                        words(
                            "cat ${PWD}x $PWD.log \"$PWD\"-old \"$PWD.x\" \" $PWD/x\" \"\n$PWD/x\""
                        ),
                        [
                            "cat",
                            "${PWD}x",
                            "$PWD.log",
                            "\"$PWD\"-old",
                            "\"$PWD.x\"",
                            "\" $PWD/x\"",
                            "\"\n$PWD/x\""
                        ],
                        "$PWD keeps no value unless it starts the word and ends it or a `/` follows"
                    );
                    assert_eq!(
                        words(r#"cat $HOME/x "${HOME}/y z" "$HOME" a$HOME $HOMES "$HOME$X""#),
                        [
                            "cat".to_owned(),
                            format!("{home}/x"),
                            format!("{home}/y z"),
                            home.clone(),
                            "a$HOME".to_owned(),
                            "$HOMES".to_owned(),
                            "\"$HOME$X\"".to_owned(),
                        ]
                    );
                });
            }

            #[cfg(unix)]
            #[test]
            fn expands_a_leading_tilde_user_to_that_users_home() {
                let root = nix::unistd::User::from_name("root").unwrap().unwrap();
                let home = root.dir.to_string_lossy();

                assert_eq!(
                    words("cat ~root/x ~root"),
                    ["cat".to_owned(), format!("{home}/x"), home.into_owned()]
                );
            }

            #[test]
            fn marks_each_word_that_expands_or_globs_as_not_literal() {
                let command = temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                    parse(r#"FOO="a b" BAR=$X EMPTY= a[$i]=1 cat 'x*' *.rs "$X" ~/n"#)
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
                        ("\"$X\"", false),
                        (home.as_str(), true),
                    ]
                );
                assert_eq!(
                    joined(program(segment)),
                    format!("cat x* *.rs \"$X\" {home}")
                );
            }

            #[test]
            fn reads_an_ansi_c_string_as_the_literal_word_it_decodes_to() {
                let command = parse(r"$'rm' pu$'sh' $'\055rf' $'\x80'");

                let program: Vec<(&str, bool)> = program(&command.segments[0])
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
            fn joins_the_parts_of_a_word_a_backslash_escape_splits() {
                assert_eq!(words("cat a\\\tb c"), ["cat", "a\tb", "c"]);
                assert_eq!(words(r#"echo "x"\ "y""#), ["echo", "x y"]);
                assert_eq!(texts("r\\\nm -rf x"), ["rm -rf x"]);
            }

            #[test]
            fn recovers_a_lone_dash_the_grammar_drops_before_a_redirection() {
                assert_eq!(words("python3 - <<'EOF'\nprint(1)\nEOF"), ["python3", "-"]);
                assert_eq!(words("cmd x - 2>&1"), ["cmd", "x", "-"]);
                assert_eq!(words("cmd - <<EOF a\nx\nEOF"), ["cmd", "-", "a"]);
            }

            #[test]
            fn turns_a_command_whose_word_boundaries_differ_from_bash_unparsed() {
                for command in [
                    "LC_ALL=$ rm -rf y",
                    "echo  \\  x",
                    "\\\trm x",
                    "rm x\\\n#y",
                    "x=1\\\nrm y",
                    "x=1\\\trm y",
                    "r\\\nm x",
                    "sh -c 'echo  \\  x'",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
            }

            #[test]
            fn turns_a_reserved_word_the_grammar_reads_as_a_command_unparsed() {
                for command in [
                    "coproc rm -rf x",
                    "coproc worker { rm -rf x; }",
                    "time while true; do rm -rf x; done",
                    "time -p ! rm -rf x",
                    "time -- ! rm -rf x",
                    "time time if true; then rm -rf x; fi",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
            }

            #[test]
            fn keeps_everyday_commands_parsed() {
                for command in [
                    "LC_ALL=$y rm -rf z",
                    "cargo test \\\n  --all",
                    "cat a\\\tb",
                    "rm x \\\n#y",
                    "ls |\\\ngrep x",
                    r#"echo "$a $b" "Price: $ 5" "${x:-a b}" "$(date) $x""#,
                    "for f in *.rs; do echo \"$f $f\"; done",
                    "cat > f <<EOF\nhello $USER\nEOF",
                    "python3 - <<'EOF'\nprint(1)\nEOF",
                    "kubectl apply -f - <<EOF\nx\nEOF",
                    "cmd - 2>&1",
                    "time cargo test",
                    "time -p",
                    "help coproc",
                    "echo if then fi",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn keeps_an_argument_the_grammar_reads_as_an_operator_as_a_literal_word() {
                assert_eq!(
                    words("git == push =~ -f"),
                    ["git", "==", "push", "=~", "-f"]
                );
                for (command, expected) in
                    [("cat == x", "cat == x"), (r#"cat =="x" y"#, "cat ==x y")]
                {
                    let named = parse(command).segments[0].named().to_vec();

                    assert_eq!(joined(&named), expected, "{command:?}");
                    assert!(named.iter().all(ShellWord::is_literal), "{command:?}");
                }
            }

            #[test]
            fn reads_a_bare_dollar_as_part_of_a_word_that_expands() {
                for (command, word) in [(r#"cat $"x" y"#, r#"$"x""#), ("cat a$ y", "a$")] {
                    let named: Vec<(String, bool)> = parse(command).segments[0]
                        .named()
                        .iter()
                        .map(|word| (word.text().to_owned(), word.is_literal()))
                        .collect();

                    assert_eq!(
                        named,
                        [
                            ("cat".to_owned(), true),
                            (word.to_owned(), false),
                            ("y".to_owned(), true),
                        ],
                        "{command:?}"
                    );
                }
            }

            #[test]
            fn strips_transparent_wrappers_and_their_options() {
                for command in [
                    "env -i -u HOME TZ=UTC rm -rf x",
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
                        joined(program(&command.segments[0])),
                        "rm -rf x",
                        "{command:?}"
                    );
                    assert_eq!(
                        joined(command.segments[0].named()),
                        "rm -rf x",
                        "{command:?}"
                    );
                    assert!(command.unrecognized().is_none(), "{command:?}");
                }
                assert_eq!(texts("env --chdir=/tmp rm -rf x"), ["rm -rf x"]);
            }

            #[test]
            fn keeps_a_wrapper_spelled_with_a_directory_in_the_named_words() {
                for (command, named, runs) in [
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
                    assert_eq!(joined(program(segment)), runs, "{command:?}");
                    assert!(!segment.is_safe(), "{command:?}");
                }
            }

            #[test]
            fn keeps_a_wrapper_that_changes_who_runs_the_command_in_the_named_words() {
                for (command, runs) in [
                    ("sudo -u root rm -rf x", "rm -rf x"),
                    ("sudo -nE --user=root FOO=1 rm -rf x", "rm -rf x"),
                    ("sudo FOO=1 -u root BAR=2 -n rm -rf x", "rm -rf x"),
                    ("sudo 1=x cat a", "cat a"),
                    ("sudo -- cat a", "cat a"),
                    ("doas -n -u root cat a", "cat a"),
                    ("busybox rm -rf x", "rm -rf x"),
                    ("nice sudo nohup cat a", "cat a"),
                ] {
                    let parsed = parse(command);
                    let segment = &parsed.segments[0];

                    assert_eq!(joined(program(segment)), runs, "{command:?}");
                    assert!(
                        joined(segment.named()).ends_with(&format!(" {runs}")),
                        "{command:?}"
                    );
                    assert!(!segment.is_safe(), "{command:?}");
                    assert!(parsed.unrecognized().is_none(), "{command:?}");
                }
                assert_eq!(texts("nice sudo nohup cat a"), ["sudo nohup cat a"]);
                assert_eq!(
                    texts("pkexec --user root echo hi"),
                    ["pkexec --user root echo hi"]
                );
            }

            #[test]
            fn reads_option_clusters_with_a_value_attached_or_in_the_next_word() {
                for command in [
                    "env -iu HOME rm -rf x",
                    "stdbuf -oL -e 0 rm -rf x",
                    "timeout -vk 5 10 rm -rf x",
                    "sudo -Eu root rm -rf x",
                ] {
                    let parsed = parse(command);
                    assert_eq!(
                        joined(program(&parsed.segments[0])),
                        "rm -rf x",
                        "{command:?}"
                    );
                    assert!(parsed.unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn reads_a_lone_dash_a_missing_value_and_an_optional_value_left_off() {
                for (command, runs) in [
                    ("env - rm -rf x", "rm -rf x"),
                    ("nice --adjustment", "nice --adjustment"),
                    ("nice -n", "nice -n"),
                    ("xargs -i cp {} d", "cp {} d"),
                ] {
                    let parsed = parse(command);

                    assert_eq!(joined(program(&parsed.segments[0])), runs, "{command:?}");
                    assert!(parsed.unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn pins_the_named_words_at_a_wrapper_whose_options_hide_what_it_runs() {
                for (command, named) in [
                    ("sudo --background=x rm", "sudo --background=x rm"),
                    ("xargs -I $P cp $P d", "xargs -I $P cp $P d <input>"),
                    ("xargs -a $F echo", "xargs -a $F echo <input>"),
                    ("sudo -s rm -rf x", "sudo -s rm -rf x"),
                    ("sudo --login rm -rf x", "sudo --login rm -rf x"),
                    ("doas -s", "doas -s"),
                    ("nice --weird rm", "nice --weird rm"),
                    ("nice $LEVEL rm", "nice $LEVEL rm"),
                    ("timeout --what 5 rm", "timeout --what 5 rm"),
                    ("find . $ACTION rm", "find . $ACTION rm"),
                ] {
                    let parsed = parse(command);

                    assert_eq!(joined(parsed.segments[0].named()), named, "{command:?}");
                    assert!(parsed.unrecognized().is_none(), "{command:?}");
                }
                assert_eq!(
                    joined(program(&parse("sudo -i rm -rf x").segments[0])),
                    "rm -rf x"
                );
            }

            #[test]
            fn reads_every_env_word_with_an_equals_sign_as_an_assignment() {
                for (command, runs) in [
                    ("env X.Y=1 cat a", "cat a"),
                    ("env 1X=y 'A B=c' cat a", "cat a"),
                    ("env -i =x cat a", "cat a"),
                ] {
                    let parsed = parse(command);

                    let program = joined(program(&parsed.segments[0]));

                    assert_eq!(program, runs, "{command:?}");
                }
            }

            #[test]
            fn leaves_a_command_line_env_splits_unrecognized() {
                for command in [
                    "env -S 'cat a'",
                    "env -vS'cat a'",
                    "env --split-string='cat a'",
                    "env --split-string 'cat a'",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
            }

            #[test]
            fn adds_the_input_words_xargs_passes_to_its_command() {
                for (command, named, runs) in [
                    ("xargs rm -rf", "xargs rm -rf <input>", "rm -rf <input>"),
                    ("xargs -0 -n1 rm", "xargs -0 -n1 rm <input>", "rm <input>"),
                    ("xargs", "xargs <input>", "xargs <input>"),
                    (
                        "xargs -J % cp a% dst",
                        "xargs -J % cp a% dst <input>",
                        "cp a% dst <input>",
                    ),
                ] {
                    let parsed = parse(command);
                    let segment = &parsed.segments[0];

                    assert_eq!(joined(segment.named()), named, "{command:?}");
                    assert_eq!(joined(program(segment)), runs, "{command:?}");
                    assert!(!segment.named().last().unwrap().is_literal(), "{command:?}");
                    assert!(parsed.unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn marks_the_words_xargs_replaces_as_not_literal() {
                for command in [
                    "xargs -I X cp X dst",
                    "xargs -iX cp X dst",
                    "xargs -J X cp X dst",
                ] {
                    let parsed = parse(command);
                    let literal: Vec<(&str, bool)> = program(&parsed.segments[0])
                        .iter()
                        .map(|word| (word.text(), word.is_literal()))
                        .collect();

                    assert_eq!(
                        literal,
                        [("cp", true), ("X", false), ("dst", true)],
                        "{command:?}"
                    );
                }
                assert_eq!(
                    texts("xargs --replace cp {} dst"),
                    ["xargs --replace cp {} dst"]
                );
            }

            #[test]
            fn lets_a_reader_xargs_runs_read_any_file() {
                for command in ["xargs cat", "xargs -I X head X", "xargs -0 cat a"] {
                    assert!(parse(command).segments[0].reads_anything(), "{command:?}");
                }
                assert!(parse("find . -exec cat {} +").segments[1].reads_anything());
                assert!(!parse("nice cat a").segments[0].reads_anything());
            }

            #[test]
            fn reads_the_arguments_file_xargs_names() {
                let parsed = parse("xargs -a .env echo");

                assert_eq!(
                    Path::new(&parsed.reads[0].requested),
                    Path::new("/repo/.env")
                );
            }

            #[test]
            fn lets_input_words_run_as_the_command_of_a_wrapper_xargs_runs_alone() {
                for command in ["xargs sudo", "xargs -0 nice"] {
                    let parsed = parse(command);
                    let segment = &parsed.segments[0];

                    let last = segment.programs().last().unwrap();
                    assert_eq!(joined(last), "<input>", "{command:?}");
                    assert_eq!(joined(segment.named()), format!("{command} <input>"));
                }
            }

            #[test]
            fn lets_the_program_start_at_any_word_past_a_wrapper_option_it_does_not_know() {
                let parsed = parse("sudo --weird value rm -rf x");

                let programs: Vec<String> = parsed.segments[0].programs().map(joined).collect();

                assert_eq!(
                    programs,
                    [
                        "sudo --weird value rm -rf x",
                        "--weird value rm -rf x",
                        "value rm -rf x",
                        "rm -rf x",
                        "-rf x",
                        "x",
                    ]
                );
                assert_eq!(
                    joined(parsed.segments[0].named()),
                    "sudo --weird value rm -rf x"
                );
                assert_eq!(
                    parse("sudo --x nice --y rm -rf x").segments[0]
                        .programs()
                        .filter(|program| joined(program) == "rm -rf x")
                        .count(),
                    1
                );
            }

            #[test]
            fn lets_the_program_start_at_any_word_past_a_wrapper_word_bash_may_split() {
                for command in [
                    "nice -n {5,rm,-rf,x} true",
                    "nice -n $n true",
                    "timeout * true",
                    "env $assignment true",
                    "nice -n {5,rm,-rf,x}",
                ] {
                    let parsed = parse(command);

                    assert_eq!(joined(parsed.segments[0].named()), command, "{command:?}");
                    assert!(!is_safe(command), "{command:?}");
                }
                for command in ["nice -n \"$n\" true", "nice -n '{5,rm}' true"] {
                    assert_eq!(
                        joined(parse(command).segments[0].named()),
                        "true",
                        "{command:?}"
                    );
                }
            }

            #[test]
            fn collects_the_commands_find_runs() {
                for (command, expected) in [
                    (
                        r"find . -name '*.tmp' -exec rm -rf {} \;",
                        &["find . -name *.tmp -exec rm -rf {} ;", "rm -rf {}"][..],
                    ),
                    (
                        r"find . -exec cat {} + -ok sudo rm {} \; -print",
                        &[
                            "find . -exec cat {} + -ok sudo rm {} ; -print",
                            "cat {}",
                            "sudo rm {}",
                        ],
                    ),
                    (
                        "find . -exec echo + x",
                        &["find . -exec echo + x", "echo + x"],
                    ),
                    (r"find . -exec \;", &[r"find . -exec ;"]),
                ] {
                    assert_eq!(texts(command), expected, "{command:?}");
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
                assert!(parse(r"find . -execdir cat x \;").unrecognized().is_some());
            }

            #[test]
            fn keeps_a_wrapper_that_runs_no_command() {
                assert_eq!(texts("env FOO=1"), ["env FOO=1"]);
                assert_eq!(texts("nice"), ["nice"]);
            }

            #[test]
            fn names_the_first_construct_it_does_not_recognize_on_one_short_line() {
                let long = format!("(( {} ))", "x".repeat(60));
                let cut = format!("{}...", &long[..MAX_CONSTRUCT_CHARS]);
                for (command, construct) in [
                    ("echo $((1 + 2)); let x=1", "$((1 + 2))"),
                    ("[ a > b ] && ls", "[ a > b ]"),
                    ("if true; then ls", "if true; then ls"),
                    ("f() {\n  ls\n}", "f() {..."),
                    (long.as_str(), cut.as_str()),
                ] {
                    assert_eq!(
                        parse(command).unrecognized(),
                        Some(construct),
                        "{command:?}"
                    );
                }
            }

            #[test]
            fn flags_every_construct_it_does_not_recognize() {
                for command in [
                    "$CMD arg",
                    "${X} arg",
                    "echo $((1 + 2))",
                    "(( x++ ))",
                    "[[ -f x ]]",
                    "ls 'unterminated",
                    "cd /etc && echo > passwd",
                    "env -C /etc echo > hosts",
                    "env --chdir=/etc echo > hosts",
                    "cd / && cat etc/shadow",
                    "cd /tmp/evil && ./build.sh",
                    "pushd /tmp/evil; bin/build",
                    "env -C /tmp/evil ./build.sh",
                    "cd /tmp/evil && ./nice cargo test",
                    "cd /tmp/evil && echo x | xargs ./build.sh",
                    "cd x && ./build.sh",
                    "cd /tmp/evil && make",
                    "cd $DIR && make",
                    "cd && make",
                    "cd - && make",
                    "cd -@ x && make",
                    "cd -P -- -x/.. && make",
                    "cd x* && make",
                    "popd && make",
                    "cd x && cd y && make",
                    "cd x/../y && make",
                    "cd .. && make",
                    "for i in 1 2; do cd x; done; make",
                    "until make; do cd x; done",
                    "f() { cd x; }; f; f; make",
                    "env -C x make",
                    "pkexec make",
                    "sudo -D /tmp cat a",
                    "find . -execdir wc {} +",
                    "{ cat; } <<'EOF' a\nx\nEOF",
                    "/bin/s? -c ls",
                    "cat < $F",
                    "cat < *.txt",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "cat <<'EOF'\n$(id)\nEOF",
                    "echo $(date)",
                    "echo `whoami`",
                    "diff <(ls a) b",
                    "tee >(cat) x",
                    "x=$(id) ls",
                    "cat <<EOF\n$(id)\nEOF",
                    "eval 'ls'",
                    "source env.sh",
                    ". env.sh",
                    "sh -c 'ls'",
                    "curl x | sh",
                    "/bin/bash script.sh",
                    "exec sh -c 'ls'",
                    "command eval ls",
                    "builtin . env.sh",
                    "BASH -c ls",
                    "/bin/ZSH script.zsh",
                    "echo '$(id)'",
                    "cd x && echo > /dev/null",
                    "cd x && ls",
                    &format!("cd x && cat {}", abs("/etc/hosts")),
                    &format!("cd x && {}", abs("/usr/bin/make")),
                    "./build.sh",
                    "cd x && make",
                    "cd -P x && make",
                    "cd -L -- x && make",
                    "pushd -n /tmp/evil && make",
                    "cd /repo/x && make",
                    "for i in 1 2; do echo $i; done; cd x && make",
                    "cd /tmp/evil",
                    "cd x; cd /tmp/evil",
                    "ls $HOME",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn recognizes_the_scripts_an_agent_commonly_writes() {
                for command in [
                    "cargo test -p ox-core 2>&1 | tail -20",
                    "git log --oneline -5 && git status",
                    "for f in src/*.rs; do wc -l \"$f\"; done",
                    "count=0; for x in a b; do echo \"$x\"; done",
                    "if [ -f Cargo.toml ]; then cargo build; fi",
                    "test -d build || mkdir build",
                    "find . -name '*.rs' | xargs grep -n TODO",
                    "source .venv/bin/activate && pytest -q",
                    "name=$(basename \"$path\" .txt); echo \"${name%.tmp}\"",
                    "while read -r line; do echo \"$line\"; done < list.txt",
                    "case \"$1\" in start) make run;; *) echo usage;; esac",
                    "export RUST_BACKTRACE=1; cargo run",
                    "set -euo pipefail; make",
                    "cd crates/ox && cargo test",
                    "diff <(sort a) <(sort b)",
                    "cat <<'EOF' > notes.md\nhello\nEOF",
                    "files=(a b c); printf '%s\\n' \"${files[@]}\" | sort -u",
                    "echo {1..3} $\"hello\"",
                    "sh -c 'npm ci && npm test'",
                    "ls -la ~/.config 2>/dev/null || true",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn recognizes_a_bash_builtin_unless_it_may_run_hidden_code_or_rebind_a_name() {
                for command in [
                    "alias ll=ls",
                    "bind -x '\"\\C-t\": ls'",
                    "compgen -C ls x",
                    "complete -C ls x",
                    "compopt -o default x",
                    "enable -f ./x.so x",
                    "fc -s",
                    "hash -p /tmp/x ls",
                    "history -s ls",
                    "let x=1",
                    "shopt -s expand_aliases",
                    "trap ls EXIT",
                    "jobs -x ls",
                    "mapfile -C ls x",
                    "source \"$f\"",
                    "source *.sh",
                    "eval \"$x\"",
                    "export PATH=/x",
                    "declare -n r=x",
                    "declare -i n",
                    "declare -x foo=1",
                    "set -o posix",
                    "set foo",
                    "read 'a[$(id)]'",
                    "read -a \"$n\"",
                    "printf -v PATH x",
                    "printf -vPATH x",
                    "printf \"$f\" PATH",
                    "unset PATH",
                    "getopts ab PATH",
                    "wait -p PATH",
                    "wait -np 'a[$(id)]'",
                    "wait \"$p\"",
                    "test -v 'a[$(id)]'",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    ". ./env.sh",
                    ":",
                    "[ -f x ]",
                    "bg",
                    "break",
                    "builtin echo",
                    "caller",
                    "cd src",
                    "command ls",
                    "continue",
                    "declare -a list",
                    "dirs",
                    "disown",
                    "echo",
                    "exec ls",
                    "exit 1",
                    "false",
                    "fg",
                    "getopts ab opt",
                    "help cd",
                    "jobs",
                    "kill 1",
                    "local -r x=1",
                    "logout",
                    "mapfile lines",
                    "popd",
                    "printf x",
                    "printf",
                    "printf -vx %s y",
                    "printf \"$f\" out",
                    "read -ax",
                    "unset -- x",
                    "pushd src",
                    "pwd",
                    "read line",
                    "readarray lines",
                    "readonly x",
                    "return",
                    "set -e",
                    "shift",
                    "source ./env.sh",
                    "suspend",
                    "test -f x",
                    "times",
                    "true",
                    "type ls",
                    "typeset x",
                    "ulimit -n",
                    "umask 022",
                    "unalias ll",
                    "unset x",
                    "wait",
                    "wait -n -p job 1 2",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn collects_commands_inside_substitutions_and_literal_scripts() {
                for command in [
                    "echo $(rm -rf /)",
                    "sh -c 'rm -rf /'",
                    "bash -lc \"rm -rf /\"",
                    "eval rm -rf /",
                    "eval -- 'rm -rf /'",
                    "exec sh -c 'rm -rf /'",
                    "busybox sh -c 'rm -rf /'",
                    "su -c 'rm -rf /' root",
                    "su --command 'rm -rf /' root",
                    "su --command='rm -rf /' root",
                    "busybox hush -c 'rm -rf /'",
                    "runuser -u nobody rm -rf /",
                    "runuser --user=nobody -- rm -rf /",
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
                    "xargs sudo",
                    "sudo FOO=1 -u root rm -rf x",
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

        /// Each write's path relative to `cwd`, or as requested outside it.
        fn writes(command: &str, cwd: &Path) -> Vec<String> {
            spelled(CommandTarget::parse(command, &Workspace::new(cwd)).writes)
        }

        /// Each certain read's path relative to `cwd`, or as requested outside it.
        fn reads(command: &str, cwd: &Path) -> Vec<String> {
            spelled(CommandTarget::parse(command, &Workspace::new(cwd)).reads)
        }

        fn spelled(targets: Vec<PathTarget>) -> Vec<String> {
            targets
                .into_iter()
                .map(|target| target.relative.unwrap_or(target.requested))
                .collect()
        }

        mod nested_script {
            use super::*;

            #[test]
            fn reads_the_script_and_startup_files_a_shell_runs_after_its_options() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                for (command, read) in [
                    ("bash -- -x", &["-x"][..]),
                    ("bash - -x", &["-x"]),
                    ("bash + f", &["f"]),
                    ("bash -o -c f", &["f"]),
                    ("bash -oO a b f", &["f"]),
                    ("bash -x --norc f", &[]),
                    ("bash -s f", &[]),
                    ("bash -s -- f", &[]),
                    ("bash --rcfile rc -i", &["rc"]),
                    ("bash -init-file rc f", &["f", "rc"]),
                    ("zsh -b -x f", &["-x"]),
                    ("zsh + -x f", &["-x"]),
                    ("dash -o vi f", &["f"]),
                    ("fish f", &["f"]),
                ] {
                    let mut read_paths = reads(command, cwd);
                    read_paths.sort();

                    assert_eq!(read_paths, read, "{command:?}");
                }
            }

            #[test]
            fn leaves_a_shell_unrecognized_when_its_script_is_unknown() {
                // cspell:ignore eovi
                for command in [
                    "bash $F",
                    "bash -- $F",
                    "bash -c",
                    "bash -c \"$X\"",
                    "bash -c -- \"$X\"",
                    "bash --rcfile",
                    "bash --rcfile $F f",
                    "bash -o",
                    "bash -o $X f",
                    "bash --bogus f",
                    "zsh -ovi f",
                    "ksh -ovi f",
                    "mksh -eovi f",
                    "fish -n f",
                    "source",
                    "source -p . key",
                ] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
                }
                for command in [
                    "bash -s -- \"$V\"",
                    "bash -c ls",
                    "bash -oO a b f",
                    "zsh -o vi f",
                ] {
                    assert!(parse(command).unrecognized().is_none(), "{command:?}");
                }
            }

            #[test]
            fn leaves_a_shell_unrecognized_when_input_words_replace_its_script() {
                for (command, replaced) in [
                    ("echo a | xargs -I % bash -c 'cat %'", true),
                    ("echo a | xargs -i bash -c 'cat {}'", true),
                    ("echo a | xargs -I % su -c 'cat %' root", true),
                    ("echo a | xargs -I % runuser -u root bash -c 'cat %'", true),
                    ("find . -exec sh -c 'cat {}' \\;", true),
                    ("find . -execdir bash -c 'cat {}' \\;", true),
                    ("echo bash | xargs -I % % -c true", true),
                    ("BASH_ENV='$(true)' bash -c true", true),
                    ("env 'ENV=`true`' sh -i", true),
                    ("echo a | xargs -I % env %=x bash -c true", true),
                    ("echo a | xargs -I % sudo A%=x bash -c true", true),
                    ("echo a | xargs -I % env A=% bash -c true", false),
                    ("echo a | xargs -I % bash -c 'cat x' %", false),
                    ("echo a | xargs su -c 'cat x' root", false),
                    ("find . -exec sh -c 'cat x' \\;", false),
                ] {
                    let parsed = parse(command);

                    assert_eq!(parsed.unrecognized().is_some(), replaced, "{command:?}");
                }
            }

            #[test]
            fn flags_a_shell_that_may_run_any_file() {
                for (command, anything) in [
                    ("echo a | xargs bash", true),
                    ("echo a | xargs -I % bash %", true),
                    ("find . -exec bash '{}' \\;", true),
                    ("echo a | xargs su root", true),
                    ("echo a | xargs runuser -u root bash", true),
                    ("BASH_ENV=$F bash -c true", true),
                    ("env BASH_ENV=\"$F\" bash -c true", true),
                    ("BASH_ENV+=x bash -c true", true),
                    ("ENV=$F dash -c true", true),
                    ("ZDOTDIR=$D zsh -c true", true),
                    ("HOME=$H bash -i", true),
                    ("BASH_ENV='$HOME/x' bash -c true", true),
                    ("BASH_ENV='~no-such-user/x' bash -c true", true),
                    ("echo a | xargs -I % bash --rcfile % -i", true),
                    ("echo a | xargs -I % env BASH_ENV=% bash -c true", true),
                    ("BASH_ENV=$F su root -c true", true),
                    ("BASH_ENV=$F eval 'bash -c true'", true),
                    ("HOME=$H bash -c true", true),
                    ("HOME=$H fish -c true", true),
                    ("HOME=$H su -m root -c true", true),
                    ("env BASH_ENV=a* bash -c true", true),
                    ("HOME=$H su root -c true", false),
                    ("env --bogus BASH_ENV bash -c true", false),
                    ("echo a | xargs bash script.sh", false),
                    ("echo a | xargs su -c true root", false),
                    ("BASH_ENV= bash -c true", false),
                    ("ENV=$F zsh -c true", false),
                    ("BASH_ENV=$F fish", false),
                    ("bash -c true", false),
                ] {
                    let parsed = parse(command);

                    let flagged = parsed.segments.iter().any(Segment::reads_anything);

                    assert_eq!(flagged, anything, "{command:?}");
                }
            }

            #[test]
            fn reads_the_startup_files_a_variable_names() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let tilde_path = Path::new("~+").join("a").to_string_lossy().into_owned();
                for (command, read) in [
                    ("BASH_ENV=a bash -c true", &["a"][..]),
                    ("env ENV=a BASH_ENV=b bash x", &["a", "b", "x"]),
                    ("ENV=a sh -i", &["a"]),
                    ("BASH_ENV=a ENV=b dash -c true", &["b"]),
                    ("BASH_ENV=a BASH_ENV=b bash -c true", &["b"]),
                    ("BASH_ENV='~+/a' bash -c true", &["a", &tilde_path]),
                    ("BASH_ENV=a true; bash -c true", &[]),
                    ("BASH_ENV=a eval 'bash -c true'", &["a"]),
                    ("BASH_ENV=a sh -c 'bash -c true'", &["a"]),
                    ("BASH_ENV=a find . -exec bash -c true \\;", &["", "a"]),
                    ("BASH_ENV=a su root -c true", &["a"]),
                    ("BASH_ENV=a runuser -u root bash -c true", &["a"]),
                    ("ZDOTDIR=d zsh", &["d"]),
                    ("ZDOTDIR=d HOME=h zsh", &["d", "h"]),
                    ("BASH_ENV=a HOME=h bash -c true", &["a", "h"]),
                    ("HOME=h ksh -i", &["h"]),
                    ("HOME=h fish", &["h"]),
                    ("HOME=h su -m root -c true", &["h"]),
                    ("HOME=h su --preserve-environment root", &["h"]),
                    ("HOME=h runuser -p root -c true", &["h"]),
                    ("HOME=h su -c -l -m root", &["h"]),
                    ("HOME=h su -m -sl root -c true", &["h"]),
                    ("env --bogus bash -c true BASH_ENV=a", &[]),
                    ("HOME=h su root -c true", &[]),
                    ("HOME=h su -m - root -c true", &[]),
                    ("HOME=h su -mpl root -c true", &[]),
                    ("HOME=h su -m --login root -c true", &[]),
                    ("BASH_ENV= bash -c true", &[]),
                    ("ENV=a zsh", &[]),
                    ("ENV=a fish", &[]),
                ] {
                    let mut read_paths = reads(command, cwd);
                    read_paths.sort();

                    assert_eq!(read_paths, read, "{command:?}");
                }
            }
        }

        mod visit {
            use std::collections::BTreeSet;

            use tree_sitter::Language;

            use super::*;

            #[test]
            fn leaves_unrecognized_only_the_grammar_kinds_that_evaluate_arithmetic_or_define_a_function()
             {
                let language: Language = tree_sitter_bash::LANGUAGE.into();
                let kinds: BTreeSet<&str> = (0..language.node_kind_count())
                    .filter_map(|id| u16::try_from(id).ok())
                    .filter(|&id| {
                        language.node_kind_is_named(id) && language.node_kind_is_visible(id)
                    })
                    .filter_map(|id| language.node_kind_for_id(id))
                    .collect();
                let weighed = [
                    "array",
                    "command",
                    "declaration_command",
                    "expansion",
                    "file_redirect",
                    "for_statement",
                    "redirected_statement",
                    "simple_expansion",
                    "test_command",
                    "unset_command",
                    "variable_assignment",
                    "while_statement",
                ];

                let unknown: Vec<&&str> = WALKED_KINDS
                    .iter()
                    .chain(&weighed)
                    .filter(|kind| !kinds.contains(**kind))
                    .collect();
                let unrecognized: Vec<&str> = kinds
                    .into_iter()
                    .filter(|kind| !WALKED_KINDS.contains(kind) && !weighed.contains(kind))
                    .collect();

                assert!(unknown.is_empty(), "{unknown:?}");
                assert_eq!(
                    unrecognized,
                    [
                        "arithmetic_expansion",
                        "binary_expression",
                        "c_style_for_statement",
                        "function_definition",
                        "parenthesized_expression",
                        "postfix_expression",
                        "subscript",
                        "ternary_expression",
                        "unary_expression",
                    ]
                );
            }

            #[test]
            fn withholds_safety_from_every_segment_once_anything_expands_a_variable() {
                let safety = |command: &str| -> Vec<bool> {
                    parse(command)
                        .segments
                        .iter()
                        .map(Segment::is_safe)
                        .collect()
                };
                for (literal, expanding) in [
                    ("cat <<< 'key=$TOKEN'", "cat <<< \"key=$TOKEN\""),
                    ("cat <<< TOKEN", "cat <<< ${TOKEN}"),
                    ("cat <<'EOF'\n$TOKEN\nEOF", "cat <<EOF\n$TOKEN\nEOF"),
                    (
                        "cat <<'EOF' | head\n$TOKEN\nEOF",
                        "cat <<EOF | head\n$TOKEN\nEOF",
                    ),
                    ("echo hi; cat <<< '$TOKEN'", "echo hi; cat <<< $TOKEN"),
                    (
                        "case a in a*) echo a;; esac",
                        "case $TOKEN in a*) echo a;; esac",
                    ),
                    ("LC_ALL=C true", "LC_ALL=$TOKEN true"),
                    ("LC_ALL=C; echo hi", "LC_ALL=$TOKEN; echo hi"),
                    (
                        "for t in a; do echo; done",
                        "for t in $TOKEN; do echo; done",
                    ),
                ] {
                    let safe = safety(literal);
                    assert!(
                        !safe.is_empty() && safe.iter().all(|&safe| safe),
                        "{literal:?}"
                    );
                    assert_eq!(safety(expanding), vec![false; safe.len()], "{expanding:?}");
                }
            }
        }

        mod redirect {
            use super::*;

            #[test]
            fn counts_each_file_an_output_redirection_writes() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                assert_eq!(
                    writes("echo a > out.txt 2>> log &> all >| clobber >& both", cwd),
                    ["out.txt", "log", "all", "clobber", "both"]
                );
                let outside = abs("/etc/hosts");
                assert_eq!(
                    CommandTarget::parse(&format!("echo a > {outside}"), &Workspace::new(cwd))
                        .writes,
                    [resolve(&outside, &Workspace::new(cwd))]
                );
            }

            #[test]
            fn counts_each_file_an_input_redirection_reads() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();

                let target = CommandTarget::parse(
                    "sort < in.txt 3< more.txt > out.txt",
                    &Workspace::new(cwd),
                );

                assert_eq!(spelled(target.reads.clone()), ["in.txt", "more.txt"]);
                assert_eq!(spelled(target.writes.clone()), ["out.txt"]);
                assert!(target.unrecognized().is_none());
                assert!(
                    parse("cd x && cat < in.txt").unrecognized().is_some(),
                    "a relative input after a cd may name another file"
                );
            }

            #[test]
            fn counts_a_read_write_redirection_as_both() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                for (command, read, written) in [
                    ("cat <> key", "key", &["key"][..]),
                    ("cat 3<>key", "key", &["key"]),
                    ("sort <>in.txt > out.txt", "in.txt", &["in.txt", "out.txt"]),
                    ("cat <\\\n>key", "key", &["key"]),
                    ("cat <\\\n> key", "key", &["key"]),
                    ("echo a\\\\<>key", "key", &["key"]),
                ] {
                    let target = CommandTarget::parse(command, &Workspace::new(cwd));

                    assert_eq!(spelled(target.reads.clone()), [read], "{command:?}");
                    assert_eq!(spelled(target.writes.clone()), written, "{command:?}");
                }
            }

            #[test]
            fn skips_heredocs_descriptors_and_devices() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                for command in [
                    "cat <<< x",
                    "cat <<EOF\nx\nEOF",
                    "make 2>&1",
                    "make 3>&-",
                    "make <&3",
                    "make > /dev/null 2> /dev/stderr < /dev/null",
                ] {
                    assert!(writes(command, cwd).is_empty(), "{command:?}");
                    assert!(reads(command, cwd).is_empty(), "{command:?}");
                    assert!(
                        CommandTarget::parse(command, &Workspace::new(cwd))
                            .unrecognized()
                            .is_none(),
                        "{command:?}"
                    );
                }
            }

            #[test]
            fn to_a_target_it_cannot_place_is_unrecognized() {
                for command in ["echo > \"$OUT\"", "echo > *.txt"] {
                    assert!(parse(command).unrecognized().is_some(), "{command:?}");
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

    mod dir_target {
        use super::*;

        /// The words of the first simple command in `command`.
        fn command_words(command: &str) -> Vec<Word> {
            fn first_command(node: Node<'_>) -> Option<Node<'_>> {
                if node.kind() == "command" {
                    return Some(node);
                }
                let mut cursor = node.walk();
                node.named_children(&mut cursor).find_map(first_command)
            }
            let tree = parse_tree(command);
            let node = first_command(tree.root_node()).unwrap();
            let parts: Vec<Node<'_>> = command_parts(node)
                .into_iter()
                .map(|part| match part.kind() {
                    "command_name" => part.named_child(0).unwrap(),
                    _ => part,
                })
                .collect();
            words(&parts, command)
        }

        #[test]
        fn reads_where_a_change_goes_and_what_pushd_n_stacks() {
            for (command, expected) in [
                ("cd", DirTarget::Home),
                ("cd -P -- src", DirTarget::Path("src")),
                ("pushd src", DirTarget::Path("src")),
                ("pushd -n", DirTarget::Stay(Stacked::Nothing)),
                ("pushd -n +1", DirTarget::Stay(Stacked::Nothing)),
                ("pushd -n src", DirTarget::Stay(Stacked::Path("src"))),
                ("pushd -n -- -x", DirTarget::Stay(Stacked::Path("-x"))),
                ("pushd -n \"$D\"", DirTarget::Stay(Stacked::Unknown)),
                ("pushd -n ~1", DirTarget::Stay(Stacked::Unknown)),
                ("pushd \"$D\"", DirTarget::Unknown),
                ("cd -", DirTarget::Unknown),
                ("popd", DirTarget::Unknown),
            ] {
                let words = command_words(command);

                let target = dir_target(words[0].literal().unwrap(), &words[1..]);

                assert_eq!(target, expected, "{command:?}");
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
