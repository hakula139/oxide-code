//! `bash` command analysis for the permission gate. tree-sitter-bash splits a command into the
//! simple commands it runs and the files its output redirections write, so rules weigh each simple
//! command on its own. Leading assignments and transparent wrappers are stripped first. A command
//! the analysis cannot vouch for is opaque: rules may deny it or leave it to ask, but never allow
//! it. `exec` patterns parse here too, so they read words the way commands do.

use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Tree};

use crate::permission::{Access, Workspace, fold_name};
use crate::util::path::expand_user;

/// Shells and the commands that start one as another user, whose `-c` script, script file, or
/// stdin is code the analysis does not see.
const SHELLS: &[&str] = &[
    "ash", "bash", "csh", "dash", "fish", "hush", "ksh", "mksh", "pwsh", "runuser", "sh", "su",
    "tcsh", "zsh",
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
        name: "doas",
        options: &[
            valued("-a"),
            flag("-n"),
            flag("-s").with(Effect::Shell),
            valued("-u"),
        ],
        ..Wrapper::RUNNER
    },
    Wrapper {
        name: "env",
        options: &[
            flag("-"),
            flag("-0"),
            valued("-C").with(Effect::Directory),
            flag("-i"),
            valued("-u"),
            flag("-v"),
            valued("--chdir").with(Effect::Directory),
            flag("--debug"),
            flag("--ignore-environment"),
            flag("--null"),
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
            flag("-i").with(Effect::Shell),
            flag("-k"),
            flag("-n"),
            valued("-p"),
            valued("-r"),
            flag("-s").with(Effect::Shell),
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
            flag("--login").with(Effect::Shell),
            flag("--no-update"),
            flag("--non-interactive"),
            optional("--preserve-env"),
            flag("--preserve-groups"),
            valued("--prompt"),
            flag("--reset-timestamp"),
            valued("--role"),
            flag("--set-home"),
            flag("--shell").with(Effect::Shell),
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

/// The placeholder `xargs -i` and `--replace` replace when given no value, and `find` replaces
/// with each path it finds.
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
            expands: false,
        };
        walk.script(command);
        if walk.expands {
            for segment in &mut walk.segments {
                segment.safe = false;
            }
        }
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
    /// [`SAFE_VARIABLES`] and wrappers allow rules see through are left out, up to the first
    /// assignment of another variable or other wrapper, which starts the list and keeps every word
    /// after it. Ends in [`INPUT_WORD`] when `xargs` appends its input words.
    named: Vec<ShellWord>,
    /// Where the program may start in `named`, first past every assignment and wrapper, then at
    /// each word past a wrapper option the analysis does not know.
    programs: Vec<usize>,
    /// The files one of [`READERS`] may read, resolved like a read's path: each literal operand,
    /// and each file a globbing one matches when the analysis runs. Empty for any other command.
    reads: Vec<Access>,
    /// Whether one of [`READERS`] takes a word whose files the analysis cannot list, such as one
    /// that expands a variable, so it may read any file.
    reads_anything: bool,
    /// Whether it is a shipped known-safe read-only command, whose `reads` are all it reads, in a
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

    /// Returns whether this stands for the input words `xargs` appends.
    fn is_input(&self) -> bool {
        !self.literal && self.text == INPUT_WORD
    }

    pub(super) const fn is_literal(&self) -> bool {
        self.literal
    }
}

// ── Walk ──

/// What one command and the scripts nested in it run and write, gathered over its syntax tree.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool records an independent fact the walk finds anywhere in the tree"
)]
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
    /// Whether any part of the command expands a variable at run time, wherever the value ends up.
    expands: bool,
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
            "command" => self.command(node, &[], src),
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
            "redirected_statement" => self.redirected_statement(node, Vec::new(), src),
            "for_statement" => {
                self.opaque |= node
                    .child_by_field_name("variable")
                    .is_some_and(|name| SHELL_VARIABLES.contains(&&src[name.byte_range()]));
                self.visit_children(node, src);
            }
            _ => {
                self.opaque |= runs_hidden_code(node);
                self.expands |= matches!(node.kind(), "expansion" | "simple_expansion");
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

    /// Visits `node`, handing the `trailing` words a redirection carries to the simple command
    /// bash gives them: the last one of a pipeline or list, the one a `!` negates, or a new one
    /// after any assignments that stand alone. Under any other statement, which bash rejects, they
    /// leave the command unknown.
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
                self.opaque = true;
                self.visit(node, src);
            }
        }
    }

    fn command<'t>(&mut self, node: Node<'t>, trailing: &[Node<'t>], src: &str) {
        let mut cursor = node.walk();
        // An argument can be an anonymous token, such as a bare `==`, which bash passes as a word.
        let parts: Vec<Node<'t>> = node
            .children(&mut cursor)
            .zip(0..)
            .filter(|&(part, index)| {
                part.is_named() || node.field_name_for_child(index) == Some("argument")
            })
            .map(|(part, _)| part)
            .collect();
        self.parts(&parts, trailing, src);
    }

    /// Records the simple command whose assignments, name, arguments, and redirections are
    /// `parts`, in source order, followed by the `trailing` words its statement's redirections
    /// carry.
    fn parts<'t>(&mut self, parts: &[Node<'t>], trailing: &[Node<'t>], src: &str) {
        let words: Vec<Word<'t>> = parts
            .iter()
            .flat_map(|&part| match part.kind() {
                "command_name" => part.named_child(0).into_iter().collect(),
                "file_redirect" | "heredoc_redirect" => redirect_words(part),
                "variable_assignment" | "herestring_redirect" => Vec::new(),
                _ => vec![part],
            })
            .chain(trailing.iter().copied())
            .map(|word| Word::new(word, src))
            .collect();
        let mut assignments = Vec::new();
        let mut unsafe_assignment = None;
        for &part in parts {
            if part.kind() == "variable_assignment" {
                if !SAFE_VARIABLES.contains(&assigned(&src[part.byte_range()])) {
                    unsafe_assignment.get_or_insert(assignments.len());
                }
                assignments.push(assignment_word(part, src));
                self.visit_children(part, src);
            } else {
                self.visit(part, src);
            }
        }

        self.simple_command(&words, assignments, unsafe_assignment, None, src);
    }

    /// Records the simple command of `all` and the commands it runs through `find`, led by the
    /// `assignments` before it, of which the one at `unsafe_assignment` is the first to set a
    /// variable outside [`SAFE_VARIABLES`]. `found` is the placeholder `find` replaces with each
    /// path it finds, for the command of a `find` action.
    fn simple_command(
        &mut self,
        all: &[Word<'_>],
        mut assignments: Vec<ShellWord>,
        unsafe_assignment: Option<usize>,
        found: Option<Placeholder>,
        src: &str,
    ) {
        if all.is_empty() {
            return;
        }
        let mut unwrapped = self.unwrap(all);
        unwrapped
            .replaces
            .extend(found.map(|placeholder| (0, placeholder)));
        let pinned = unsafe_assignment.is_some() || unwrapped.named < unwrapped.starts[0];
        let safe = all[unwrapped.starts[0]..]
            .split_first()
            .filter(|_| !pinned)
            .and_then(|(first, args)| {
                safe_operands(first.value.as_deref().filter(|_| !first.glob)?, args)
            });
        let mut operands = unwrapped.reads.clone();
        let mut reads_anything = false;
        let mut finds = Vec::new();
        for (i, &start) in unwrapped.starts.iter().enumerate() {
            let Some((first, args)) = all[start..].split_first() else {
                self.opaque = true;
                continue;
            };
            let Some(name) = first.value.as_deref().filter(|_| !first.glob) else {
                self.opaque = true;
                continue;
            };
            self.program(name, args, src);
            let folded = fold_name(basename(name));
            if folded == "find" {
                finds.push(start + 1);
            }
            match &safe {
                Some(files) if i == 0 => operands.extend(files.iter().cloned()),
                _ if READERS.contains(&folded.as_str()) => {
                    let (files, anything) = reader_operands(args, self.workspace.cwd());
                    operands.extend(files);
                    reads_anything |= anything || unwrapped.passes_input();
                }
                _ => {}
            }
        }
        self.relative_path |= operands.iter().any(|path| Path::new(path).is_relative());
        let reads = operands
            .iter()
            .map(|operand| Access::for_path(operand, self.workspace))
            .collect();

        let mut words: Vec<ShellWord> = all
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let mut word = word.shell_word(src);
                word.literal &= !unwrapped.replaced(i, &word.text);
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
        if unwrapped.appends {
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
        for args in finds {
            self.find_actions(&all[args..], src);
        }
    }

    /// Weighs the program `name` runs: a shell or builtin that runs code, or a directory change.
    fn program(&mut self, name: &str, args: &[Word<'_>], src: &str) {
        let base = basename(name);
        let folded = fold_name(base);
        let runner = [base, folded.as_str()]
            .into_iter()
            .find(|runner| SHELLS.contains(runner) || CODE_BUILTINS.contains(runner));
        if let Some(runner) = runner {
            self.opaque = true;
            self.nested_script(runner, args, src);
        }
        self.changes_dir |= DIRECTORY_CHANGERS.contains(&name);
        // `shopt` can turn on `dotglob` or `globstar`, which change what a glob matches.
        self.opaque |= name == "shopt";
    }

    /// Strips the wrappers that lead `words`. The named words keep every wrapper from the first one
    /// allow rules do not see through, that sets a variable outside [`SAFE_VARIABLES`], or that is
    /// spelled with a directory, since `./nice` may be any program. A wrapper option the analysis
    /// does not know hides where the command starts, so it turns the command opaque, and the
    /// program may then start at any later word.
    fn unwrap(&mut self, words: &[Word<'_>]) -> Unwrapped {
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
                let run = match wrapper.invocation(args) {
                    Ok(run) => run,
                    Err(unknown) => {
                        self.opaque = true;
                        unwrapped.appends |= wrapper.kind == Kind::Input;
                        let from = at + 1 + unknown;
                        if from < spread {
                            pending.extend((from..spread).rev());
                            spread = from;
                        }
                        break;
                    }
                };
                self.opaque |= run.shell;
                self.changes_dir |= run.elsewhere;
                unwrapped.reads.extend(run.reads);
                if run.start == args.len() {
                    let input = wrapper.kind == Kind::Input;
                    input_runs |= unwrapped.appends && !input;
                    unwrapped.appends |= input && run.replace.is_none();
                    break;
                }
                let sets_unsafe = args[..run.start].iter().any(|word| {
                    word.value.as_deref().is_some_and(|arg| {
                        is_assignment(arg) && !SAFE_VARIABLES.contains(&assigned(arg))
                    })
                });
                at += 1 + run.start;
                if wrapper.kind == Kind::Input {
                    match run.replace {
                        Some(placeholder)
                            if !placeholder.whole
                                || words[at..].iter().any(|word| {
                                    word.value.as_deref() == Some(placeholder.text.as_str())
                                }) =>
                        {
                            unwrapped.replaces.push((at, placeholder));
                        }
                        _ => unwrapped.appends = true,
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
        unwrapped
    }

    /// Records the commands `find`'s `-exec`, `-execdir`, `-ok`, and `-okdir` actions run, each up
    /// to its `;`, or its `+` after `{}`. A word that expands may be such an action, so it turns
    /// the command opaque.
    fn find_actions(&mut self, args: &[Word<'_>], src: &str) {
        self.opaque |= args.iter().any(|word| word.value.is_none());
        let mut rest = args;
        while let Some(at) = rest.iter().position(|word| {
            word.value
                .as_deref()
                .is_some_and(|value| FIND_RUNNERS.contains(&value))
        }) {
            self.changes_dir |= rest[at]
                .value
                .as_deref()
                .is_some_and(|value| value.ends_with("dir"));
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
            let found = Placeholder {
                text: DEFAULT_REPLACE.to_owned(),
                whole: false,
            };
            self.simple_command(&body[..end], Vec::new(), None, Some(found), src);
            rest = body.get(end + 1..).unwrap_or_default();
        }
    }

    /// Walks the literal script a shell's `-c` or `--command` option or `eval` runs, and the
    /// command `runuser -u` runs without a shell, so deny rules see the commands inside them.
    fn nested_script(&mut self, runner: &str, args: &[Word<'_>], src: &str) {
        if runner == "runuser"
            && let Ok(run) = RUNUSER.invocation(args)
            && args[..run.start].iter().any(|word| {
                word.value
                    .as_deref()
                    .is_some_and(|arg| arg == "-u" || arg == "--user" || arg.starts_with("--user="))
            })
        {
            self.simple_command(&args[run.start..], Vec::new(), None, None, src);
            return;
        }
        let script = if runner == "eval" {
            args.iter()
                .map(|word| word.value.as_deref())
                .collect::<Option<Vec<_>>>()
                .map(|values| values.join(" "))
        } else if SHELLS.contains(&runner) {
            args.iter().enumerate().find_map(|(i, word)| {
                let arg = word.value.as_deref()?;
                if let Some(script) = arg.strip_prefix("--command=") {
                    return Some(script.to_owned());
                }
                (is_command_option(arg) || arg == "--command")
                    .then(|| args.get(i + 1)?.value.clone())
                    .flatten()
            })
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
        // Further destinations are the command's words, which `redirect_words` hands to it.
        let Some(&destination) = destinations.first() else {
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

    /// Parses the options that lead `args`, or returns the index of the first word that hides
    /// where the command starts: an option the grammar does not know, or one that expands.
    fn invocation(&self, args: &[Word<'_>]) -> Result<Invocation, usize> {
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
                if self.assignments == Assignments::Among && is_sudo_assignment(arg) {
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
        let assignment = match self.assignments {
            Assignments::None => |_: &str| false,
            Assignments::After => is_assignment,
            Assignments::Among => is_sudo_assignment,
        };
        while args
            .get(i)
            .and_then(|arg| arg.value.as_deref())
            .is_some_and(assignment)
        {
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
    /// After the options, as `env` takes them.
    After,
    /// Before, between, and after the options, as `sudo` takes any word holding a `=` that does
    /// not start with `/` or `=`.
    Among,
}

/// What a wrapper changes about the command it runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// It changes only how the command runs, so allow rules and known-safe reads see through it.
    Transparent,
    /// It changes who runs the command, so it stays in the words an allow rule must name.
    Runner,
    /// It appends the words it reads from its input to the command, as `xargs` does, so it stays
    /// in the words an allow rule must name too.
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
    fn of(word: &Word<'_>) -> Self {
        word.value.clone().map_or(Self::Expands, Self::Literal)
    }
}

/// What an option changes about the command the wrapper runs.
#[derive(Clone, Copy)]
enum Effect {
    None,
    /// The command runs in another directory.
    Directory,
    /// A shell runs the command, after reading its startup files.
    Shell,
    /// The value is a placeholder that each input word replaces, in place of being appended.
    Replace,
    /// The value is a placeholder that the input words replace when it stands as a whole word, as
    /// BSD `xargs -J` takes it, and that leaves them appended otherwise.
    Insert,
    /// The value names a file the wrapper reads.
    Reads,
}

/// How a wrapper runs its command, from the options before it.
#[derive(Default)]
struct Invocation {
    /// Where the command starts among the wrapper's arguments, which is past them all when it
    /// runs none.
    start: usize,
    elsewhere: bool,
    shell: bool,
    /// The placeholder that input words replace, when an option sets one.
    replace: Option<Placeholder>,
    /// The files an option names for the wrapper to read.
    reads: Vec<String>,
}

impl Invocation {
    /// Records an option's `effect` with its `value`, or returns `None` when the effect needs a
    /// value the analysis cannot read.
    fn apply(&mut self, effect: Effect, value: Arg) -> Option<()> {
        match effect {
            Effect::None => {}
            Effect::Directory => self.elsewhere = true,
            Effect::Shell => self.shell = true,
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
        }
        Some(())
    }
}

/// A word that input words replace when the command runs.
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
    /// Whether an `xargs` appends its input words to the command.
    appends: bool,
    /// The placeholders `xargs` or `find` replaces with its input words or found paths, each with
    /// where its command starts.
    replaces: Vec<(usize, Placeholder)>,
    /// The files wrapper options read.
    reads: Vec<String>,
}

impl Unwrapped {
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

/// Returns whether `sudo` reads `arg` as an environment assignment.
fn is_sudo_assignment(arg: &str) -> bool {
    arg.contains('=') && !arg.starts_with(['/', '='])
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
        _ if !node.is_named() => Some((text.to_owned(), false)),
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
        "echo" | "false" | "pwd" | "true" => Some(Vec::new()),
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
            fn keeps_an_argument_the_grammar_reads_as_an_operator_as_a_literal_word() {
                assert_eq!(
                    words("git == push =~ -f"),
                    ["git", "==", "push", "=~", "-f"]
                );
                for (command, expected) in [
                    ("cat == x", "cat == x"),
                    ("cat >out == x", "cat == x"),
                    ("cat <<'EOF' =~ x\nx\nEOF", "cat =~ x"),
                ] {
                    let named = parse(command).segments[0].named().to_vec();

                    assert_eq!(joined(&named), expected, "{command:?}");
                    assert!(named.iter().all(ShellWord::is_literal), "{command:?}");
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
                    assert_eq!(program(segment)[0].text(), name, "{command:?}");
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
                    joined(program(segment)),
                    format!("cat x* *.rs \"$HOME\" {home}")
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
                        joined(program(&command.segments[0])),
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
                    assert!(segment.safe_reads().is_none(), "{command:?}");
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
                    ("pkexec --user root echo hi", "echo hi"),
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
                    assert!(segment.safe_reads().is_none(), "{command:?}");
                    assert!(!parsed.is_opaque(), "{command:?}");
                }
                assert_eq!(texts("nice sudo nohup cat a"), ["sudo nohup cat a"]);
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
                    assert!(!parsed.is_opaque(), "{command:?}");
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
                    assert!(!parsed.is_opaque(), "{command:?}");
                }
            }

            #[test]
            fn turns_a_wrapper_option_value_it_cannot_read_opaque() {
                for command in [
                    "sudo --background=x rm",
                    "xargs -I $P cp $P d",
                    "xargs -a $F echo",
                ] {
                    assert!(parse(command).is_opaque(), "{command:?}");
                }
            }

            #[test]
            fn turns_a_wrapper_that_runs_a_shell_or_moves_the_command_opaque() {
                for command in [
                    "sudo -s rm -rf x",
                    "sudo --login rm -rf x",
                    "doas -s",
                    "sudo -D /tmp cat a",
                    "pkexec cat a",
                ] {
                    let parsed = parse(command);
                    assert!(parsed.is_opaque(), "{command:?}");
                }
                assert_eq!(
                    joined(program(&parse("sudo -i rm -rf x").segments[0])),
                    "rm -rf x"
                );
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
                    assert!(!parsed.is_opaque(), "{command:?}");
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

                let Access::Path { requested, .. } = &parsed.segments[0].reads()[0] else {
                    panic!("expected a path read");
                };
                assert_eq!(Path::new(requested), Path::new("/repo/.env"));
            }

            #[test]
            fn lets_input_words_run_as_the_command_of_a_wrapper_xargs_runs_alone() {
                for command in ["xargs sudo", "xargs -0 nice"] {
                    let parsed = parse(command);
                    let segment = &parsed.segments[0];

                    let last = segment.programs().last().unwrap();
                    assert_eq!(joined(last), "<input>", "{command:?}");
                    assert!(parsed.is_opaque(), "{command:?}");
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
                assert!(parsed.is_opaque());
                assert_eq!(
                    parse("sudo --x nice --y rm -rf x").segments[0]
                        .programs()
                        .filter(|program| joined(program) == "rm -rf x")
                        .count(),
                    1
                );
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
                    assert!(!parse(command).is_opaque(), "{command:?}");
                }
                for command in [r"find . -execdir cat x \;", "find . $ACTION rm"] {
                    assert!(parse(command).is_opaque(), "{command:?}");
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
                    "{ cat; } <<'EOF' a\nx\nEOF",
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

        mod visit {
            use super::*;

            #[test]
            fn withholds_safety_from_every_segment_once_anything_expands_a_variable() {
                let safety = |command: &str| -> Vec<bool> {
                    parse(command)
                        .segments
                        .iter()
                        .map(|segment| segment.safe_reads().is_some())
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
                for command in ["echo > \"$OUT\"", "echo > *.txt"] {
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
            assert_eq!(parse("echo done").segments[0].safe_reads(), Some(&[][..]));
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
