//! Reads nested shell invocations and the environment their scripts inherit.

use super::word::{Word, expand_tilde};
use super::wrapper::{Fed, Unwrapped, assigned};
use super::{Operand, ShellWord};

/// Shells and the commands that start one as another user. The walk follows the literal `-c`
/// script each runs, so deny rules see the commands inside it, and reads the script file a shell
/// runs, as [`shell_run`] finds them.
pub(super) const SHELLS: &[&str] = &[
    "ash", "bash", "csh", "dash", "fish", "hush", "ksh", "mksh", "pwsh", "runuser", "sh", "su",
    "tcsh", "zsh",
];

// ── Shell ──

/// What a shell runs, as [`shell_run`] reads its arguments.
pub(super) enum ShellRun<'w> {
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
pub(super) struct ShellArgs<'w> {
    pub(super) run: ShellRun<'w>,
    /// The startup files a long option names, which an interactive shell sources, each with the
    /// index of its argument.
    pub(super) startup: Vec<(usize, &'w str)>,
}

/// A variable a shell reads as it starts, which names a startup file or a directory of them.
pub(super) struct StartupVariable {
    pub(super) name: &'static str,
    /// Whether the value names a directory the shell may read any file below, since the startup
    /// files each shell and mode reads there differ, or else the file itself, which the shell
    /// expands as [`expanded_file`] reads it.
    pub(super) directory: bool,
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
pub(super) const HOME_VARIABLE: StartupVariable = StartupVariable::directory("HOME");

/// Variables used by a leading tilde or an expansion such as `$PWD`. Where a command sets one,
/// the analysis cannot resolve those expansions from its own environment.
pub(super) const DIR_VARIABLES: &[&str] = &["HOME", "OLDPWD", "PWD"];

/// The files that `value`, a startup file's name, may name once a shell expands it: as written,
/// as dash leaves it, and with a leading tilde expanded, as bash does it. `None` when it may name
/// any file, since a parameter expansion or command substitution in it, which bash runs, has no
/// value the analysis knows, or its tilde names a user the analysis cannot find or expands from a
/// variable the command sets, which `tilde_known` rules out.
pub(super) fn expanded_file(value: &str, tilde_known: bool) -> Option<Vec<String>> {
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
pub(super) struct ShellReads {
    /// Words that may name a script file it runs, as the words `su` hands its user's shell may,
    /// and the trees of startup files it may run below a directory a startup variable names.
    pub(super) possible: Vec<Operand>,
    /// Whether it may run any file, such as one an `xargs` input word names or one a startup
    /// variable names with a value that expands.
    pub(super) anything: bool,
}

/// What a program inherits from the rest of its simple command.
pub(super) struct Inherited<'c> {
    /// What an `xargs` or `find` around the command passes it.
    pub(super) fed: &'c Fed,
    /// Where the program's words start among the command's, which `fed` indexes.
    pub(super) start: usize,
    /// Each variable that the enclosing commands, the command's prefix assignments, and its
    /// wrappers set for the program, in order, with its value when literal.
    pub(super) variables: Vec<(String, Option<String>)>,
}

impl<'c> Inherited<'c> {
    /// What the program at `start` among the words `all` inherits from the `environment` the
    /// enclosing commands set and from the prefix `assignments`, the wrapper assignments at the
    /// indices `unwrapped` lists, and the input `unwrapped` feeds it. An appending `NAME+=VALUE`,
    /// or a value that input words replace, has no known value.
    pub(super) fn new(
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
    pub(super) fn replaced(&self, index: usize, word: &Word) -> bool {
        word.value
            .as_deref()
            .is_some_and(|text| self.fed.replaced(self.start + index, text))
    }

    /// The value the program's variables last set `name` to, `None` when none sets it, and
    /// `Some(None)` when that value is unknown.
    pub(super) fn value(&self, name: &str) -> Option<&Option<String>> {
        self.variables
            .iter()
            .rev()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value)
    }

    /// Returns whether the program's variables set one of the [`DIR_VARIABLES`].
    pub(super) fn sets_dir_variable(&self) -> bool {
        sets_dir_variable(&self.variables)
    }
}

/// Returns whether `variables` set one of the [`DIR_VARIABLES`].
pub(super) fn sets_dir_variable(variables: &[(String, Option<String>)]) -> bool {
    variables
        .iter()
        .any(|(name, _)| DIR_VARIABLES.contains(&name.as_str()))
}

/// Returns whether `text` may expand a tilde or one of the [`DIR_VARIABLES`].
pub(super) fn reads_dir_variable(text: &str) -> bool {
    text.contains('~')
        || ["$HOME", "${HOME}", "$PWD", "${PWD}"]
            .iter()
            .any(|variable| text.contains(variable))
}

/// The variable a `NAME=VALUE` word that `env` or `sudo` takes sets, which is everything before
/// the first `=`, whatever it holds.
pub(super) fn env_name(word: &str) -> &str {
    word.split_once('=').map_or(word, |(name, _)| name)
}

/// The startup options of a shell, as its manual lists them.
pub(super) struct ShellGrammar {
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
    pub(super) variables: &'static [StartupVariable],
}

/// What `shell` runs with `args`, read as bash reads its startup options: long options first, then
/// `-` and `+` clusters up to the first operand or a `-` or `--` that ends them, where an option
/// that takes a value takes the next word. The first operand is the script `-c` runs, or else,
/// without `-s`, the script file, and with neither the shell reads its script from standard input.
/// A shell without a [`ShellGrammar`] runs an unknown script once it has an option.
pub(super) fn shell_run<'w>(shell: &str, args: &'w [Word]) -> ShellArgs<'w> {
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
pub(super) fn shell_grammar(shell: &str) -> Option<&'static ShellGrammar> {
    match shell {
        "bash" => Some(&BASH_GRAMMAR),
        "zsh" => Some(&ZSH_GRAMMAR),
        "ash" | "dash" | "hush" | "ksh" | "mksh" | "sh" => Some(&POSIX_SHELL_GRAMMAR),
        _ => None,
    }
}

/// Bash's startup options in versions 3.2 through 5.3, one of which may reject an option another
/// takes and then exit before running anything.
pub(super) const BASH_GRAMMAR: ShellGrammar = ShellGrammar {
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
pub(super) const POSIX_SHELL_GRAMMAR: ShellGrammar = ShellGrammar {
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
pub(super) const ZSH_GRAMMAR: ShellGrammar = ShellGrammar {
    ends: "b",
    plus_ends: true,
    variables: &[StartupVariable::directory("ZDOTDIR")],
    ..POSIX_SHELL_GRAMMAR
};

/// The arguments of `su` or `runuser` as util-linux reads them.
pub(super) struct SuArgs<'w> {
    /// The words it passes its user's shell after the user name.
    pub(super) shell: &'w [Word],
    /// Whether the shell keeps the `HOME` the command sets: an option preserves the environment,
    /// as `-m`, `-p`, or `--preserve-environment` does, and none asks for a login shell, as `-`,
    /// `-l`, or `--login` does.
    pub(super) keeps_home: bool,
}

/// What `su` or `runuser` with `args` passes its user's shell, as util-linux reads its arguments:
/// options end at the first operand, a lone `-` after them asks for a login shell, and the next
/// word names the user. A word that expands where an option stands ends the options as the user
/// does, so every later word counts.
pub(super) fn su_args(args: &[Word]) -> SuArgs<'_> {
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
pub(super) fn sourced_file(args: &[Word]) -> Option<&str> {
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
pub(super) fn command_option_script(args: &[Word]) -> Option<String> {
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
