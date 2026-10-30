//! Reads command wrappers, their option effects, and the input they feed commands.

use anyhow::Result;

use super::word::Word;

/// Variables a command may set or export without changing what a rule names, since none of them
/// changes which program runs, what it loads, or where it writes.
pub(super) const SAFE_VARIABLES: &[&str] = &[
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

/// Wrappers that run a command given after their own options, so rules see through them.
pub(super) const WRAPPERS: &[Wrapper] = &[
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
pub(super) const DEFAULT_REPLACE: &str = "{}";

/// The word that stands for the arguments `xargs` reads from its input.
pub(super) const INPUT_WORD: &str = "<input>";

/// `runuser`'s options, which run the command after them directly when `-u` names the user.
pub(super) const RUNUSER: Wrapper = Wrapper {
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

// ── Wrapper ──

/// A wrapper's option grammar and what running a command through it changes.
pub(super) struct Wrapper {
    pub(super) name: &'static str,
    options: &'static [Opt],
    /// Whether `-N` sets a number, as `nice -10` does.
    numeric: bool,
    /// Words after the options that precede the command, such as `timeout`'s duration.
    positionals: usize,
    pub(super) assignments: Assignments,
    pub(super) kind: Kind,
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
    pub(super) fn invocation(&self, args: &[Word]) -> Result<Invocation, usize> {
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
pub(super) enum Value {
    None,
    /// A value as the next word or attached, as in `-n 5`, `-n5`, or `--signal=KILL`.
    Required,
    /// A value only when attached, as in `xargs -i{}`.
    Optional,
}

/// Where a wrapper takes `NAME=VALUE` words that set the command's environment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Assignments {
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
pub(super) enum Kind {
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
    /// The value is a placeholder that each input word replaces.
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
pub(super) struct Invocation {
    /// Where the command starts among the arguments, or past them all when it runs none.
    pub(super) start: usize,
    pub(super) elsewhere: bool,
    /// The placeholder that input words replace, when an option sets one.
    pub(super) replace: Option<Placeholder>,
    /// The files an option names for the wrapper to read.
    pub(super) reads: Vec<String>,
    /// Whether an option hands the wrapper a command line it splits itself.
    pub(super) splits: bool,
    /// The arguments the wrapper takes as `NAME=VALUE` assignments.
    pub(super) assignments: Vec<usize>,
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
pub(super) struct Placeholder {
    pub(super) text: String,
    /// Whether only a word that is exactly `text` is replaced.
    pub(super) whole: bool,
}

/// What the wrappers leading a simple command leave to run, as indices into its words.
#[derive(Default)]
pub(super) struct Unwrapped {
    /// Where the words an allow rule must name start.
    pub(super) named: usize,
    /// Where the program may start: first where it does when every wrapper option is known, then
    /// each word past an option that is not. The word count stands for the input words of an
    /// `xargs` whose wrapped wrapper takes its command from them.
    pub(super) starts: Vec<usize>,
    pub(super) fed: Fed,
    /// The files wrapper options read.
    pub(super) reads: Vec<String>,
    /// The `NAME=VALUE` words that wrappers such as `env` take as assignments, or may take past an
    /// option the analysis cannot read.
    pub(super) assignments: Vec<usize>,
}

/// The words an `xargs` or `find` passes a command, as indices into its words.
#[derive(Default)]
pub(super) struct Fed {
    /// Whether an `xargs` appends its input words to the command.
    pub(super) appends: bool,
    /// The placeholders `xargs` or `find` replaces with its input words or found paths, each with
    /// where its command starts.
    pub(super) replaces: Vec<(usize, Placeholder)>,
}

impl Fed {
    /// What reaches the command whose words start at `offset` among these words.
    pub(super) fn for_command_at(&self, offset: usize) -> Self {
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
    pub(super) fn passes_input(&self) -> bool {
        self.appends || !self.replaces.is_empty()
    }

    /// Returns whether input words replace part of `text`, the word at `index`.
    pub(super) fn replaced(&self, index: usize, text: &str) -> bool {
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
pub(super) fn assigned(assignment: &str) -> &str {
    assignment
        .find(['=', '+', '['])
        .map_or(assignment, |end| &assignment[..end])
}

/// Returns whether `name` is a shell identifier: a letter or `_`, then letters, digits, or `_`.
pub(super) fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Returns whether a write to the variable `name` can only change later `$name` expansions, which
/// the analysis already reads as unknown: it is an identifier with a lowercase letter, which no
/// variable a noninteractive bash reads has, and the environment `bash` starts with does not set
/// it, as `inherited` tells, so the write exports nothing.
pub(super) fn is_local_name(name: &str, inherited: &dyn Fn(&str) -> bool) -> bool {
    is_identifier(name) && name.bytes().any(|b| b.is_ascii_lowercase()) && !inherited(name)
}

/// The variable a `declare`-family or `export` argument names: its value up to an `=` or `+=`, or
/// when the value expands, the identifier it is written with before an `=`. `None` for an option, a
/// name the analysis cannot read, or a literal value that assigns `(...)`, which the builtin parses
/// again as an array whose keys and values may run substitutions the walk never sees.
pub(super) fn declared_name<'w>(word: &'w Word, src: &'w str) -> Option<&'w str> {
    let text = word.value.as_deref().unwrap_or(&src[word.span.clone()]);
    let (name, value) = text.split_once('=').unwrap_or((text, ""));
    let name = name.strip_suffix('+').unwrap_or(name);
    let reparsed = word.value.is_some() && value.starts_with('(');
    (is_identifier(name) && !reparsed).then_some(name)
}

/// The script `eval` runs from `args`, joined by spaces after a leading `--`, which `eval` drops,
/// or `None` when one of them expands.
pub(super) fn literal_script(args: &[Word]) -> Option<String> {
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
pub(super) fn builtin_options<'w>(
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
pub(super) fn sets_only_tracing_options(args: &[Word]) -> bool {
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
pub(super) fn may_test_a_subscript(args: &[Word]) -> bool {
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
