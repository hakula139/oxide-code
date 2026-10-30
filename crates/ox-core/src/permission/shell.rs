//! `bash` command analysis for the permission gate. tree-sitter-bash splits a command into the
//! simple commands it runs and the files its output redirections write, so rules weigh each simple
//! command on its own. Leading assignments and transparent wrappers are stripped first. The walk
//! recognizes bash syntax by an allowlist, and a construct outside it leaves the command
//! unrecognized: rules may deny it or leave it to ask, but never allow it. `exec` patterns parse
//! here too, so they read words the way commands do.

mod invocation;
mod pattern;
mod reader;
mod walk;
mod word;
mod wrapper;

use std::path::Path;

use self::walk::{DIRECTORY_CHANGERS, EnteredDirs, Walk, basename, spelled_from};
use self::wrapper::INPUT_WORD;
use crate::permission::{PathResolver, PathTarget, Workspace, fold_name};

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
    /// and each [certain](Operand::certain) operand of a [reader](reader::READERS), including the
    /// root of a tree it traverses.
    reads: Vec<PathTarget>,
    /// The first unrecognized construct, cut for a prompt by [`walk::construct`].
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
    /// [safe variables](wrapper::SAFE_VARIABLES) and transparent wrappers are left out, up to the
    /// first other assignment or wrapper, which starts the list and keeps every word after it.
    /// Ends in [`INPUT_WORD`] when `xargs` appends its input words.
    named: Vec<ShellWord>,
    /// Where the program may start in `named`, first past every assignment and wrapper, then at
    /// each word past a wrapper option the analysis does not know.
    programs: Vec<usize>,
    /// The files a [reader](reader::READERS) may read beyond those read for certain, resolved like
    /// a read's path: each tree it may traverse, whose descendants a rule may match below its root,
    /// and each word whose role or matches the analysis cannot pin. Empty for any other command.
    reads: Vec<ReaderPath<PathTarget>>,
    /// Whether a [reader](reader::READERS) takes a word whose files the analysis cannot list, such
    /// as one that expands a variable, so it may read any file.
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

/// One word of an `exec` pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PatternWord {
    Literal(String),
    /// An unquoted `*` standing alone.
    Any,
}

/// Parses the words of one simple command for an `exec` rule.
pub(super) fn pattern_words(pattern: &str) -> anyhow::Result<Vec<PatternWord>> {
    self::pattern::parse(pattern)
}

// ── Testing ──

#[cfg(test)]
mod local_testing {
    use std::path::Path;

    use super::{CommandTarget, PathResolver, PathTarget, Segment, ShellWord, Workspace};

    pub(super) fn parse(command: &str) -> CommandTarget {
        parse_inheriting(command, &[])
    }

    /// Parses `command` where the environment `bash` starts with sets only `inherited`.
    pub(super) fn parse_inheriting(command: &str, inherited: &[&str]) -> CommandTarget {
        CommandTarget::parse_inheriting(command, &Workspace::new(Path::new("/repo")), &|name| {
            inherited.contains(&name)
        })
    }

    pub(super) fn is_safe(command: &str) -> bool {
        parse(command).segments[0].is_safe()
    }

    pub(super) fn resolve(path: &str, workspace: &Workspace) -> PathTarget {
        PathResolver::new(workspace).resolve(path)
    }

    pub(super) fn joined(words: &[ShellWord]) -> String {
        words
            .iter()
            .map(ShellWord::text)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The program `segment` runs when every wrapper option is known.
    pub(super) fn program(segment: &Segment) -> &[ShellWord] {
        segment.programs().next().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use indoc::indoc;

    use super::*;
    use crate::permission::shell::local_testing::{
        is_safe, joined, parse, parse_inheriting, program,
    };
    use crate::permission::shell::walk::MAX_CONSTRUCT_CHARS;
    use crate::util::path::local_testing::abs;
    use crate::util::path::testing::HOME_VAR;

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
}
