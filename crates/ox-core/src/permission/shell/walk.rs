//! Walks bash syntax to collect commands, reads, writes, and unresolved constructs.

use std::path::{Component, Path, PathBuf};
use std::{iter, mem};

use tree_sitter::{Node, Parser, Tree};

use super::invocation::{
    BASH_GRAMMAR, DIR_VARIABLES, HOME_VARIABLE, Inherited, POSIX_SHELL_GRAMMAR, SHELLS, ShellReads,
    ShellRun, StartupVariable, ZSH_GRAMMAR, command_option_script, env_name, expanded_file,
    reads_dir_variable, sets_dir_variable, shell_grammar, shell_run, sourced_file, su_args,
};
use super::reader::{
    Dialect, READERS, SedScript, reader_args, reader_operands, safe_operands, sed_script,
};
use super::word::{Word, assignment_word, expand_tilde, home, stack_tilde};
use super::wrapper::{
    Assignments, DEFAULT_REPLACE, Fed, INPUT_WORD, Kind, Placeholder, RUNUSER, SAFE_VARIABLES,
    Unwrapped, WRAPPERS, assigned, builtin_options, declared_name, is_local_name, literal_script,
    may_test_a_subscript, sets_only_tracing_options,
};
use super::{Operand, ReaderPath, Segment, ShellWord};
use crate::permission::{PathResolver, PathTarget, Workspace, Zone, fold_name};

/// Builtins that change how bash finds or runs later commands, run code from a trap, the history,
/// or completion, or evaluate arithmetic, which may run a substitution, so a command that runs one
/// is unrecognized.
const DISPATCH_BUILTINS: &[&str] = &[
    "alias", "bind", "compgen", "complete", "compopt", "enable", "fc", "hash", "history", "let",
    "shopt", "trap",
];

/// Builtins that change the working directory, after which a relative redirection target no longer
/// names the file the gate resolved.
pub(super) const DIRECTORY_CHANGERS: &[&str] = &["cd", "popd", "pushd"];

/// Redirection operators that write their target.
const WRITE_OPERATORS: &[&str] = &[">", ">>", ">|", "&>", "&>>", ">&"];

/// Redirection targets that are devices, so redirecting to or from them touches no file.
const DEVICES: &[&str] = &["/dev/null", "/dev/stderr", "/dev/stdout", "/dev/tty"];

/// Characters of an unrecognized construct a prompt names before cutting it short.
pub(super) const MAX_CONSTRUCT_CHARS: usize = 40;

/// `find` primaries that run a command or write a file.
pub(super) const FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-fls", "-fprint", "-fprint0", "-fprintf", "-ok", "-okdir",
];

/// `find` primaries that run the command after them.
const FIND_RUNNERS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

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
pub(super) struct Walk<'a> {
    pub(super) workspace: &'a Workspace,
    pub(super) paths: PathResolver<'a>,
    /// Whether the environment `bash` starts with sets a variable.
    pub(super) inherited: &'a dyn Fn(&str) -> bool,
    pub(super) segments: Vec<Segment>,
    pub(super) writes: Vec<PathTarget>,
    pub(super) reads: Vec<PathTarget>,
    /// The first directory change, as [`construct`] cuts it for a prompt.
    pub(super) dir_change: Option<String>,
    pub(super) entered: EnteredDirs,
    /// The variables that the commands enclosing the node the walk is at set for it, as
    /// [`Inherited::variables`] lists them, such as a `BASH_ENV` before an `eval` or a `bash -c`.
    pub(super) environment: Vec<(String, Option<String>)>,
    /// Whether some directory change may leave the workspace. Only a first `cd` or `pushd` that
    /// runs once, to a target [`Walk::enters_workspace`] accepts, stays inside.
    pub(super) leaves_workspace: bool,
    /// How many loop bodies enclose the node the walk is at, where a directory change may run more
    /// than once.
    pub(super) repeats: usize,
    /// Whether some redirection, reader operand, script that `source` or a shell runs, or program
    /// spelled with a directory names a relative path.
    pub(super) relative_path: bool,
    /// Whether any part of the command expands a variable at run time, wherever the value ends up.
    pub(super) expands: bool,
    /// The first construct the walk does not recognize, as [`construct`] cuts it for a prompt.
    pub(super) unrecognized: Option<String>,
}

/// The directories the `cd` and `pushd` commands of a command may have entered.
#[derive(Default)]
pub(super) struct EnteredDirs {
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
    pub(super) fn script(&mut self, source: &str) {
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
    /// resolve a tilde or directory variable from its own.
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
pub(super) fn spelled_from(dir: &Path, path: &str) -> String {
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
pub(super) fn construct(text: &str) -> String {
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

/// Returns whether bash runs the program `name` from a path relative to the working directory, as
/// it does for a name with a `/` that does not start at the root, such as `./build.sh`.
fn is_relative_program(name: &str) -> bool {
    name.contains('/') && Path::new(name).is_relative()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tree_sitter::Language;

    use super::*;
    use crate::permission::shell::CommandTarget;
    use crate::permission::shell::local_testing::{parse, resolve};
    use crate::util::path::local_testing::{abs, canonical_tempdir};

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

    mod visit {

        use super::*;

        #[test]
        fn leaves_unrecognized_only_the_grammar_kinds_that_evaluate_arithmetic_or_define_a_function()
         {
            let language: Language = tree_sitter_bash::LANGUAGE.into();
            let kinds: BTreeSet<&str> = (0..language.node_kind_count())
                .filter_map(|id| u16::try_from(id).ok())
                .filter(|&id| language.node_kind_is_named(id) && language.node_kind_is_visible(id))
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
                CommandTarget::parse(&format!("echo a > {outside}"), &Workspace::new(cwd)).writes,
                [resolve(&outside, &Workspace::new(cwd))]
            );
        }

        #[test]
        fn counts_each_file_an_input_redirection_reads() {
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();

            let target =
                CommandTarget::parse("sort < in.txt 3< more.txt > out.txt", &Workspace::new(cwd));

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
}
