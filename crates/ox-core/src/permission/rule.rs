//! Permission rules: a [`PermissionClass`] and a pattern in that class's grammar, held in the tier
//! lists of a [`crate::permission::Policy`].
//!
//! `exec` patterns are one simple command whose words match a command's words, with a lone `*` as
//! the only wildcard. `read` and `edit` patterns are gitignore-style globs anchored at `~/`, `/`,
//! or each anchor directory of the [`crate::permission::Workspace`]. `net` patterns are host globs
//! and `mcp` patterns `server/tool` globs. The pattern `*` covers the whole class.
//!
//! Allow rules widen what runs, so they match conservatively, and deny and ask rules hold calls
//! back, so they match aggressively (see [`MatchDiscipline`]). Matching a command narrows the ways
//! a rule can be fooled, but it is not a security boundary.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use super::shell::{PatternWord, Segment, ShellWord, bare_program, pattern_words};
use super::{PermissionClass, Target, fold_name, resolve};
use crate::util::path::{lexical_normalize, slash_separated};
use crate::util::text::is_hidden;

// ── Rule ──

/// One parsed permission rule, which serializes as its class and its pattern as written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RuleSpec", into = "RuleSpec")]
pub struct Rule {
    class: PermissionClass,
    /// The pattern as written, such as `cargo test *`.
    source: String,
    pattern: Pattern,
}

/// The serialized form of a [`Rule`].
#[derive(Serialize, Deserialize)]
struct RuleSpec {
    class: PermissionClass,
    pattern: String,
}

/// The matchable body of a rule, chosen by its class at parse time.
#[derive(Debug, Clone)]
enum Pattern {
    /// `*`, which matches every call of the class.
    Any,
    Exec(Vec<PatternWord>),
    /// The glob as written, then a copy rooted where each of its literal prefixes resolves.
    Path(Vec<PathGlob>),
    Host(GlobMatcher),
    Tool(GlobMatcher),
}

/// A `read` / `edit` glob that also matches everything below a path it matches, compiled as
/// written and over [`fold_name`]-folded names. Allow matches the first alone, and deny matches
/// either, so a case or normalization variant cannot slip past.
#[derive(Debug, Clone)]
struct PathGlob {
    /// The glob, `/`-separated, without the copy that matches below it.
    glob: String,
    /// Whether the glob matches absolute paths, or paths below each anchor directory.
    absolute: bool,
    exact: GlobSet,
    folded: GlobSet,
}

/// The tier a match is evaluated for, selecting how commands, symlinked paths, and name spellings
/// match. Naming it at every match keeps the allow / deny asymmetry legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MatchDiscipline {
    /// Every simple command of a command that is not opaque must match its named words, each
    /// literal one exactly, and a path matches only where it resolves, with names exactly as the
    /// pattern writes them. Neither a chained command, a symlink, nor a variant spelling widens.
    Allow,
    /// Any simple command may match, by its named words, its program words, or its program's bare
    /// name. The pattern's later words may sit among other words, each word compares folded as
    /// names do, and a word that expands matches any pattern word. A path matches as resolved or as
    /// spelled, with names folded too. A danger behind a safe head, an option, an expansion, a
    /// symlink, or a variant spelling is caught.
    Deny,
}

impl Rule {
    /// Parses `pattern` in `class`'s grammar. A malformed pattern is a hard error, so a typo fails
    /// loudly at config load where it would otherwise match nothing.
    pub fn parse(class: PermissionClass, pattern: &str) -> Result<Self> {
        let source = pattern.trim();
        let pattern = if source == "*" {
            Pattern::Any
        } else {
            match class {
                PermissionClass::Exec => Pattern::Exec(pattern_words(source)?),
                PermissionClass::Read | PermissionClass::Edit => {
                    Pattern::Path(vec![PathGlob::parse(source)?])
                }
                PermissionClass::Net => Pattern::Host(host_glob(source)?),
                PermissionClass::Mcp => Pattern::Tool(tool_glob(source)?),
            }
        };
        Ok(Self {
            class,
            source: source.to_owned(),
            pattern,
        })
    }

    /// The narrowest rule that allows the call in `class` with `target`: the one simple command a
    /// command runs as its named words, the path the tool resolved, under `./` inside cwd, or the
    /// name an MCP or network call targets. `None` when no pattern names the call alone, as for a
    /// command that runs several or has a word that expands, or when a character of it would not
    /// paint as itself, so the user always sees the exact rule they grant.
    pub(super) fn for_call(class: PermissionClass, target: &Target<'_>) -> Option<Self> {
        let pattern = match target {
            Target::Command(command) => match command.segments() {
                [segment] => command_pattern(segment.named())?,
                _ => return None,
            },
            Target::Path {
                canonical,
                relative,
                ..
            } => match relative {
                Some("") => return None,
                Some(relative) => format!("./{}", escaped(Path::new(relative))),
                None => escaped(Path::new(canonical)),
            },
            Target::Name(name) => globset::escape(name),
            Target::None => return None,
        };
        if !pattern.chars().all(paints_as_itself) {
            return None;
        }
        let rule = Self::parse(class, &pattern).ok()?;
        rule.matches(class, target, MatchDiscipline::Allow)
            .then_some(rule)
    }

    pub const fn class(&self) -> PermissionClass {
        self.class
    }

    /// The pattern as written.
    pub fn pattern(&self) -> &str {
        &self.source
    }

    /// Returns whether this rule matches a call in `class` with `target` under `discipline`.
    pub(super) fn matches(
        &self,
        class: PermissionClass,
        target: &Target<'_>,
        discipline: MatchDiscipline,
    ) -> bool {
        if self.class != class {
            return false;
        }
        match (&self.pattern, target) {
            (_, Target::Command(command)) if discipline == MatchDiscipline::Allow => {
                !command.is_opaque()
                    && !command.segments().is_empty()
                    && command
                        .segments()
                        .iter()
                        .all(|segment| self.allows(segment))
            }
            (Pattern::Any, _) => true,
            (Pattern::Exec(words), Target::Command(command)) => command
                .segments()
                .iter()
                .any(|segment| segment_held(words, segment)),
            (
                Pattern::Path(globs),
                Target::Path {
                    canonical,
                    rooted,
                    requested,
                    deny_rooted,
                    ..
                },
            ) => globs
                .iter()
                .any(|glob| glob.is_match(canonical, rooted, requested, deny_rooted, discipline)),
            (Pattern::Host(glob) | Pattern::Tool(glob), Target::Name(name)) => glob.is_match(name),
            _ => false,
        }
    }

    /// Returns whether this `exec` rule allows `segment`, one simple command of a command that is
    /// not opaque.
    pub(super) fn allows(&self, segment: &Segment) -> bool {
        match &self.pattern {
            Pattern::Any => self.class == PermissionClass::Exec,
            Pattern::Exec(words) => words_allowed(words, segment.named()),
            Pattern::Path(_) | Pattern::Host(_) | Pattern::Tool(_) => false,
        }
    }

    /// This rule plus a copy of its path glob rooted where the glob's literal prefix (the
    /// components before the first glob character) resolves, wherever that differs from the
    /// prefix as written. An anchored glob resolves from each of `anchors`, the anchor directories
    /// as spelled, and only absolute anchors count.
    pub(super) fn with_resolved_prefixes(mut self, anchors: &[PathBuf]) -> Result<Self> {
        let Pattern::Path(globs) = &mut self.pattern else {
            return Ok(self);
        };
        let written = globs[0].clone();
        let (prefix, rest) = literal_prefix(&written.glob);
        let literals: Vec<PathBuf> = if written.absolute {
            vec![PathBuf::from(prefix)]
        } else {
            anchors
                .iter()
                .filter(|anchor| anchor.is_absolute())
                .map(|anchor| anchor.join(prefix))
                .collect()
        };
        for literal in literals {
            let Some(target) = resolve(&literal)
                .ok()
                .filter(|t| *t != lexical_normalize(&literal))
            else {
                continue;
            };
            let target = escaped(&target);
            let glob = if rest.is_empty() {
                target
            } else {
                format!("{}/{rest}", target.trim_end_matches('/'))
            };
            globs.push(PathGlob::new(glob, true)?);
        }
        Ok(self)
    }
}

/// Rules compare by their class and pattern, which parse to the same matcher every time.
impl PartialEq for Rule {
    fn eq(&self, other: &Self) -> bool {
        self.class == other.class && self.source == other.source
    }
}

impl Eq for Rule {}

impl TryFrom<RuleSpec> for Rule {
    type Error = anyhow::Error;

    fn try_from(spec: RuleSpec) -> Result<Self> {
        Self::parse(spec.class, &spec.pattern)
    }
}

impl From<Rule> for RuleSpec {
    fn from(rule: Rule) -> Self {
        Self {
            class: rule.class,
            pattern: rule.source,
        }
    }
}

/// Returns whether `c` paints as one visible cell or more, or is a plain space.
fn paints_as_itself(c: char) -> bool {
    c == ' ' || !(c.is_whitespace() || is_hidden(c))
}

/// `path` with its separators as `/` and every glob character escaped, so it matches only itself.
pub(super) fn escaped(path: &Path) -> String {
    globset::escape(&slash_separated(path))
}

// ── Exec Patterns ──

/// The pattern naming exactly `words`, each quoted where the pattern grammar needs it, or `None`
/// when a word expands or globs, which no literal pattern word matches.
fn command_pattern(words: &[ShellWord]) -> Option<String> {
    let quoted: Option<Vec<String>> = words
        .iter()
        .map(|word| word.is_literal().then(|| quote(word.text())))
        .collect();
    quoted.map(|quoted| quoted.join(" "))
}

/// `word` as one shell word: bare when every character is plain, or single-quoted otherwise.
fn quote(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "%+,-./:=@_".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Returns whether a deny or ask pattern holds `segment` by its named words, or by the words of a
/// program it may run, as written or with the program's directory dropped.
fn segment_held(pattern: &[PatternWord], segment: &Segment) -> bool {
    words_held(pattern, segment.named())
        || segment.programs().any(|program| {
            words_held(pattern, program)
                || bare_program(program).is_some_and(|bare| words_held(pattern, &bare))
        })
}

/// Returns whether an allow `pattern` matches all of `words`. A trailing `*` matches whatever words
/// remain, and every other pattern word matches one literal word, equal to it, since a word that
/// expands or globs can become several.
fn words_allowed(pattern: &[PatternWord], words: &[ShellWord]) -> bool {
    match pattern.split_first() {
        None => words.is_empty(),
        Some((PatternWord::Any, [])) => true,
        Some((head, rest)) => words.split_first().is_some_and(|(word, tail)| {
            word.is_literal()
                && match head {
                    PatternWord::Literal(text) => word.text() == text,
                    PatternWord::Any => true,
                }
                && words_allowed(rest, tail)
        }),
    }
}

/// Returns whether a deny or ask `pattern` holds `words`: its first word matches the first of them,
/// and each later literal word matches one of the rest, in order, with every `*` left out. A
/// program's options can sit between the words a pattern names, as in `git -c a=b push`, so the
/// rest need not be adjacent.
fn words_held(pattern: &[PatternWord], words: &[ShellWord]) -> bool {
    pattern
        .split_first()
        .zip(words.split_first())
        .is_some_and(|((head, rest), (word, tail))| {
            let mut tail = tail.iter();
            word_held(head, word)
                && rest
                    .iter()
                    .filter(|pattern_word| **pattern_word != PatternWord::Any)
                    .all(|pattern_word| tail.any(|word| word_held(pattern_word, word)))
        })
}

/// Returns whether one deny or ask pattern word matches `word`. The two compare folded, so `RM`
/// matches `rm` as a case-insensitive file system runs it, and a word that expands or globs matches
/// any pattern word, since it may become that word when the command runs.
fn word_held(pattern_word: &PatternWord, word: &ShellWord) -> bool {
    match pattern_word {
        PatternWord::Literal(text) => {
            !word.is_literal() || fold_name(word.text()) == fold_name(text)
        }
        PatternWord::Any => true,
    }
}

// ── Path Patterns ──

impl PathGlob {
    /// Parses a gitignore-style `read` / `edit` pattern. `~/` and `/` anchor it absolutely, and
    /// `./` or a `/` before its last character anchors it at each anchor directory. Any other
    /// pattern matches a name at any depth below them. The pattern is [`slash_separated`], so a
    /// rooted pattern without a drive fails on Windows, where it could never match.
    fn parse(raw: &str) -> Result<Self> {
        ensure!(
            !raw.starts_with('!'),
            "a pattern cannot start with `!`, since patterns have no negation. \
             Write the paths to match instead, or turn off the built-in rule that covers them."
        );
        let mut pattern = slash_separated(Path::new(raw));
        if pattern == "~" || pattern.starts_with("~/") {
            let home = std::env::home_dir().context("failed to expand `~`: no home directory")?;
            let home = escaped(&lexical_normalize(&home));
            pattern = format!(
                "{}/{}",
                home.trim_end_matches('/'),
                pattern[1..].trim_start_matches('/')
            );
        }
        if pattern
            .split('/')
            .enumerate()
            .any(|(i, name)| name == ".." || (name == "." && i > 0))
        {
            if Path::new(&pattern).has_root() {
                bail!("the pattern holds a `.` or `..` component, which no resolved path does");
            }
            bail!(
                "the pattern holds a `.` or `..` component, which no resolved path does. \
                 Start it with `/` or `~/` \
                 to name a path outside the working directory and extra roots."
            );
        }
        let path = Path::new(&pattern);
        ensure!(
            path.is_absolute() || !path.has_root(),
            "a rooted pattern needs a drive on Windows, as in `C:/...`"
        );
        if path.is_absolute() {
            let trimmed = pattern.trim_end_matches('/');
            if Path::new(trimmed).is_absolute() {
                pattern.truncate(trimmed.len());
            }
            return Self::new(pattern, true);
        }

        let anchored = pattern.starts_with("./");
        let body = pattern
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        ensure!(!body.is_empty(), "the pattern names no path");
        if anchored || body.contains('/') {
            Self::new(body, false)
        } else {
            Self::new(format!("**/{body}"), false)
        }
    }

    fn new(glob: String, absolute: bool) -> Result<Self> {
        let below = format!("{}/**", glob.trim_end_matches('/'));
        let set = |globs: [&str; 2]| -> Result<GlobSet> {
            let mut builder = GlobSetBuilder::new();
            for glob in globs {
                builder.add(path_glob(glob)?);
            }
            Ok(builder.build()?)
        };
        Ok(Self {
            exact: set([&glob, &below])?,
            folded: set([&fold_path(&glob), &fold_path(&below)])?,
            glob,
            absolute,
        })
    }

    /// Returns whether this glob matches a path target under `discipline`, given the target's
    /// spellings as [`Target::Path`] holds them.
    fn is_match(
        &self,
        canonical: &str,
        rooted: &[String],
        requested: &str,
        deny_rooted: &[String],
        discipline: MatchDiscipline,
    ) -> bool {
        let deny = discipline == MatchDiscipline::Deny;
        let hit = |path: &str| {
            self.exact.is_match(path) || (deny && self.folded.is_match(fold_path(path)))
        };
        if self.absolute {
            hit(canonical) || (deny && requested != canonical && hit(requested))
        } else {
            rooted.iter().any(|path| hit(path))
                || (deny && deny_rooted.iter().any(|path| hit(path)))
        }
    }
}

/// `literal_separator` keeps `*` from crossing `/`, so `*.json` names one level of files.
fn path_glob(glob: &str) -> Result<Glob> {
    Ok(GlobBuilder::new(glob).literal_separator(true).build()?)
}

/// `glob` split into its literal prefix, the components before the first glob character, and the
/// rest after the separating `/`. A prefix that is a root, such as `/` or `C:/`, keeps its slash.
fn literal_prefix(glob: &str) -> (&str, &str) {
    match glob.find(['*', '?', '[', ']', '{', '}', '\\']) {
        None => (glob, ""),
        Some(meta) => match glob[..meta].rfind('/') {
            Some(slash) => {
                let root = slash == 0 || glob[..slash].ends_with(':');
                (
                    &glob[..if root { slash + 1 } else { slash }],
                    &glob[slash + 1..],
                )
            }
            None => ("", glob),
        },
    }
}

/// `path` with each name folded by [`fold_name`], rejoined on `/`.
fn fold_path(path: &str) -> String {
    path.split(std::path::is_separator)
        .map(fold_name)
        .collect::<Vec<_>>()
        .join("/")
}

// ── Net and MCP Patterns ──

/// A host glob compared without case, where `*` matches any run of characters.
fn host_glob(raw: &str) -> Result<GlobMatcher> {
    ensure!(
        !raw.is_empty() && !raw.contains(['/', ':']) && !raw.chars().any(char::is_whitespace),
        "expected a host name such as `*.example.com`, without a scheme, port, or path"
    );
    Ok(GlobBuilder::new(raw)
        .case_insensitive(true)
        .build()?
        .compile_matcher())
}

/// A `server/tool` glob, where `*` stays within the server or the tool name.
fn tool_glob(raw: &str) -> Result<GlobMatcher> {
    ensure!(
        raw.split_once('/').is_some_and(|(server, tool)| {
            !server.is_empty() && !tool.is_empty() && !tool.contains('/')
        }),
        "expected `server/tool`, such as `github/get_*`"
    );
    Ok(GlobBuilder::new(raw)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::{Access, Workspace, Zone};
    use crate::util::path::local_testing::{abs, canonical_tempdir};
    use crate::util::path::testing::HOME_VAR;

    fn rule(class: PermissionClass, pattern: &str) -> Rule {
        Rule::parse(class, pattern).unwrap()
    }

    fn exec(pattern: &str) -> Rule {
        rule(PermissionClass::Exec, pattern)
    }

    fn cmd(s: &str) -> Access {
        Access::for_command(s, &Workspace::new(Path::new("/repo")))
    }

    fn allows(rule: &Rule, command: &str) -> bool {
        rule.matches(
            PermissionClass::Exec,
            &cmd(command).as_target(),
            MatchDiscipline::Allow,
        )
    }

    /// A path target with `canonical` resolved, rooted at the given relative spellings.
    fn path<'a>(canonical: &'a str, rooted: &'a [String]) -> Target<'a> {
        Target::Path {
            canonical,
            relative: rooted.first().map(String::as_str),
            rooted,
            requested: canonical,
            deny_rooted: &[],
            zone: Zone::Outside,
            protected: &[],
        }
    }

    fn matches_path(rule: &Rule, target: &Target<'_>, discipline: MatchDiscipline) -> bool {
        rule.matches(rule.class(), target, discipline)
    }

    #[test]
    fn serializes_as_its_class_and_pattern() {
        let rule = exec("  cargo test * ");

        let json = serde_json::to_value(&rule).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"class": "exec", "pattern": "cargo test *"})
        );
        let back: Rule = serde_json::from_value(json).unwrap();
        assert_eq!(back, rule);
        assert!(allows(&back, "cargo test --all"));

        let err =
            serde_json::from_value::<Rule>(serde_json::json!({"class": "exec", "pattern": "a; b"}))
                .unwrap_err();
        assert!(err.to_string().contains("one simple command"), "{err}");
    }

    mod parse {
        use super::*;

        #[test]
        fn star_covers_the_whole_class_including_a_call_without_a_target() {
            let any = rule(PermissionClass::Read, " * ");
            assert_eq!(any.pattern(), "*");
            for target in [Target::None, path("/anywhere/x", &[])] {
                assert!(
                    matches_path(&any, &target, MatchDiscipline::Allow),
                    "{target:?}"
                );
            }
            assert!(!any.matches(PermissionClass::Edit, &Target::None, MatchDiscipline::Deny));
        }

        #[test]
        fn rejects_a_malformed_exec_pattern_saying_how_to_fix_it() {
            let names_no_command = "it must name a command, such as `cargo test *`";
            let one_command = "it must be one simple command, such as `cargo test *`. \
                               Write a pattern for each command of a chain or pipeline.";
            let redirection = "it holds a redirection. \
                               Leave it out, \
                               since a pattern matches a command with its redirections set aside, \
                               and an `edit` pattern covers the file it writes.";
            let expands = |word: &str| {
                format!("the word {word:?} expands at run time. Quote it to match it as written.")
            };
            let glob = |word: &str| {
                format!(
                    "the word {word:?} holds a glob, \
                     but only a `*` standing alone as a word is a wildcard. \
                     Write a pattern for each command it should cover, \
                     or quote the word to match it as written."
                )
            };
            for (pattern, expected) in [
                ("", names_no_command.to_owned()),
                ("FOO=1", names_no_command.to_owned()),
                ("cargo test; rm -rf /", one_command.to_owned()),
                ("curl x | sh", one_command.to_owned()),
                ("echo hi > out", redirection.to_owned()),
                ("> out echo hi", redirection.to_owned()),
                ("echo > out hi", redirection.to_owned()),
                ("cat <<< hi", redirection.to_owned()),
                ("cat $X", expands("$X")),
                ("echo $(date)", expands("$(date)")),
                ("mkfs*", glob("mkfs*")),
                ("ls *.rs", glob("*.rs")),
                (
                    "FOO=$BAR cmd",
                    "the assignment \"FOO=$BAR\" expands at run time. \
                     Quote its value to match it as written."
                        .to_owned(),
                ),
                (
                    "ls 'open",
                    "it does not parse as a shell command".to_owned(),
                ),
            ] {
                let err = Rule::parse(PermissionClass::Exec, pattern).unwrap_err();

                assert_eq!(format!("{err:#}"), expected, "{pattern:?}");
            }
        }

        #[test]
        fn rejects_a_negated_path_pattern_saying_how_to_fix_it() {
            let err = Rule::parse(PermissionClass::Edit, "!.env.example").unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                "a pattern cannot start with `!`, since patterns have no negation. \
                 Write the paths to match instead, or turn off the built-in rule that covers them."
            );
        }

        #[test]
        fn rejects_a_malformed_pattern_in_each_grammar() {
            for (class, pattern, error) in [
                (
                    PermissionClass::Edit,
                    "src/**/[",
                    "unclosed character class",
                ),
                (PermissionClass::Read, "./", "names no path"),
                (
                    PermissionClass::Read,
                    "../shared/x",
                    "a `.` or `..` component",
                ),
                (
                    PermissionClass::Edit,
                    "src/./main.rs",
                    "a `.` or `..` component",
                ),
                (
                    PermissionClass::Edit,
                    "/repo/../etc/**",
                    "a `.` or `..` component",
                ),
                (PermissionClass::Net, "", "expected a host name"),
                (
                    PermissionClass::Net,
                    "https://example.com",
                    "without a scheme",
                ),
                (
                    PermissionClass::Net,
                    "localhost:8080",
                    "without a scheme, port",
                ),
                (PermissionClass::Mcp, "github", "expected `server/tool`"),
                (PermissionClass::Mcp, "/get_me", "expected `server/tool`"),
                (PermissionClass::Mcp, "a/b/c", "expected `server/tool`"),
            ] {
                let err = format!("{:#}", Rule::parse(class, pattern).unwrap_err());
                assert!(err.contains(error), "{class} {pattern:?}: {err}");
            }
        }

        #[test]
        fn suggests_an_absolute_pattern_only_for_a_relative_one_with_a_dot_component() {
            let relative = format!(
                "{:#}",
                Rule::parse(PermissionClass::Read, "../shared/x").unwrap_err()
            );
            let absolute = format!(
                "{:#}",
                Rule::parse(PermissionClass::Read, &abs("/repo/../etc/**")).unwrap_err()
            );

            assert_eq!(
                relative,
                "the pattern holds a `.` or `..` component, which no resolved path does. \
                 Start it with `/` or `~/` \
                 to name a path outside the working directory and extra roots."
            );
            assert_eq!(
                absolute,
                "the pattern holds a `.` or `..` component, which no resolved path does"
            );
        }
    }

    mod for_call {
        use super::*;

        #[test]
        fn names_the_one_simple_command_by_its_quoted_named_words() {
            for (command, expected) in [
                (" cargo test ", "cargo test"),
                ("RUST_LOG=debug cargo test 2>&1", "cargo test"),
                (
                    "git commit -m \"it's done\"",
                    r"git commit -m 'it'\''s done'",
                ),
                ("GIT_PAGER=cat git log", "GIT_PAGER=cat git log"),
                ("echo '*' ''", "echo '*' ''"),
            ] {
                let target = cmd(command);
                let rule = Rule::for_call(PermissionClass::Exec, &target.as_target()).unwrap();
                assert_eq!(rule.pattern(), expected, "{command:?}");
                assert!(allows(&rule, command), "{command:?}");
            }
            assert!(!allows(&exec("cargo test"), "cargo test --all"));
        }

        #[test]
        fn names_the_path_under_dot_slash_inside_cwd_and_absolute_outside() {
            let canonical = abs("/repo/src/a[1].rs");
            let rooted = ["src/a[1].rs".to_owned()];
            let rule = Rule::for_call(PermissionClass::Edit, &path(&canonical, &rooted)).unwrap();
            assert_eq!(rule.pattern(), "./src/a[[]1[]].rs");
            let other = ["src/a1.rs".to_owned()];
            assert!(!matches_path(
                &rule,
                &path(&abs("/repo/src/a1.rs"), &other),
                MatchDiscipline::Allow
            ));

            let top = ["a.rs".to_owned()];
            let rule =
                Rule::for_call(PermissionClass::Edit, &path(&abs("/repo/a.rs"), &top)).unwrap();
            assert_eq!(
                rule.pattern(),
                "./a.rs",
                "a bare name would match at any depth"
            );

            let outside = abs("/etc/hosts");
            let rule = Rule::for_call(PermissionClass::Edit, &path(&outside, &[])).unwrap();
            assert_eq!(rule.pattern(), slash_separated(Path::new(&outside)));
        }

        #[test]
        fn declines_a_call_no_exact_rule_can_name() {
            for command in [
                "ls *.rs",
                "cat $F",
                "cargo test; rm -rf /",
                "  ",
                "cargo test \u{1b}[2K",
                "echo 'a\u{200b}b'",
                "echo 'a\tb'",
            ] {
                assert!(
                    Rule::for_call(PermissionClass::Exec, &cmd(command).as_target()).is_none(),
                    "{command:?}"
                );
            }
            let empty = [String::new()];
            let hidden = ["a\u{200b}.rs".to_owned()];
            let hidden_path = abs("/repo/a\u{200b}.rs");
            for target in [
                path(&abs("/repo"), &empty),
                path(&hidden_path, &hidden),
                Target::None,
            ] {
                assert!(
                    Rule::for_call(PermissionClass::Edit, &target).is_none(),
                    "{target:?}"
                );
            }
        }

        #[test]
        fn names_a_host_or_tool_by_its_escaped_name() {
            let host =
                Rule::for_call(PermissionClass::Net, &Target::Name("api.example.com")).unwrap();
            assert_eq!(host.pattern(), "api.example.com");

            let tool = Rule::for_call(PermissionClass::Mcp, &Target::Name("srv/a*b")).unwrap();
            assert_eq!(tool.pattern(), "srv/a[*]b");
            let matches = |name| {
                tool.matches(
                    PermissionClass::Mcp,
                    &Target::Name(name),
                    MatchDiscipline::Allow,
                )
            };
            assert!(matches("srv/a*b"));
            assert!(!matches("srv/axb"));
        }
    }

    mod matches {
        use super::*;

        fn edit(pattern: &str) -> Rule {
            rule(PermissionClass::Edit, pattern)
        }

        fn holds(rule: &Rule, command: &str) -> bool {
            rule.matches(
                PermissionClass::Exec,
                &cmd(command).as_target(),
                MatchDiscipline::Deny,
            )
        }

        #[test]
        fn exec_trailing_star_covers_zero_or_more_words_and_a_middle_one_covers_one() {
            let prefix = exec("cargo test *");
            for (command, expected) in [
                ("cargo test", true),
                ("cargo test --all -- --nocapture", true),
                ("cargo testbench", false),
                ("cargo build", false),
            ] {
                assert_eq!(allows(&prefix, command), expected, "{command}");
            }

            let middle = exec("git * --force");
            for (command, expected) in [
                ("git push --force", true),
                ("git --force", false),
                ("git push origin --force", false),
            ] {
                assert_eq!(allows(&middle, command), expected, "{command}");
            }
        }

        #[test]
        fn exec_compares_words_however_the_command_quotes_or_spaces_them() {
            let rule = exec("git commit -m 'fix the parser'");
            for command in [
                "git commit -m 'fix the parser'",
                "git  commit   -m \"fix the parser\"",
                "git commit -m fix\\ the\\ parser 2>&1",
                "nice -n 5 git commit -m 'fix the parser'",
            ] {
                assert!(allows(&rule, command), "{command}");
            }
            assert!(!allows(&rule, "git commit -m 'fix the'"));
            assert!(allows(&exec("echo '*'"), "echo '*'"));
            assert!(!allows(&exec("echo '*'"), "echo *"));
        }

        #[test]
        fn exec_allow_needs_literal_words_except_under_a_trailing_star() {
            // A word that expands or globs can become several words, so only a trailing `*` may
            // cover it.
            let middle = exec("git * status");
            assert!(allows(&middle, "git -C status"));
            assert!(!allows(&middle, "git $ARGS status"));
            assert!(!allows(&middle, "git *.c status"));
            assert!(!allows(&exec("cat a"), "cat $F"));
            assert!(allows(&exec("cat *"), "cat $F *.rs"));
        }

        #[test]
        fn exec_allow_names_a_kept_assignment_or_pinned_wrapper() {
            let plain = exec("git log *");
            let pinned = exec("GIT_PAGER=cat git log *");
            for (command, plain_allows, pinned_allows) in [
                ("git log -1", true, false),
                ("RUST_LOG=debug git log -1", true, false),
                ("GIT_PAGER=cat git log -1", false, true),
                ("GIT_PAGER=sh git log -1", false, false),
                ("env GIT_PAGER=cat git log -1", false, false),
            ] {
                assert_eq!(allows(&plain, command), plain_allows, "{command}");
                assert_eq!(allows(&pinned, command), pinned_allows, "{command}");
            }
            assert!(allows(
                &exec("env GIT_PAGER=cat git log *"),
                "env GIT_PAGER=cat git log"
            ));
        }

        #[test]
        fn exec_allow_refuses_a_command_that_runs_more() {
            // The load-bearing safety property: `cargo test *` must not allow a chained, piped, or
            // substituted `rm`.
            let rule = exec("cargo test *");
            for command in [
                "cargo test && rm -rf /",
                "cargo test; rm -rf /",
                "cargo test | sh",
                "cargo test $(rm -rf /)",
                "eval cargo test",
            ] {
                assert!(!allows(&rule, command), "{command}");
            }
        }

        #[test]
        fn exec_star_allows_only_a_command_the_analysis_sees_whole() {
            let rule = exec("*");
            assert!(allows(&rule, "cargo test && rm -rf /"));
            for command in [r#"eval "$CMD""#, "$EDITOR notes", r#"sh -c "$X""#, "FOO=1"] {
                assert!(!allows(&rule, command), "{command}");
            }
        }

        #[test]
        fn exec_deny_catches_any_simple_command_however_it_is_spelled_or_wrapped() {
            let rule = exec("rm -rf *");
            for command in [
                "rm -rf /",
                "ls && rm -rf /tmp/x",
                "echo hi; rm -rf .",
                r#""rm" '-rf' x"#,
                "/bin/rm -rf x",
                "FOO=1 nice -n 5 rm -rf x",
                "rm -rf $DIR",
                "echo $(rm -rf /)",
                "sh -c 'rm -rf /'",
            ] {
                assert!(holds(&rule, command), "{command}");
            }
            assert!(!holds(&rule, "rm -r x"));
            assert!(holds(&exec("GIT_PAGER=sh git *"), "GIT_PAGER=sh git log"));
        }

        #[test]
        fn exec_deny_finds_its_words_in_order_among_options_and_in_any_word_that_expands() {
            let rule = exec("git push --force *");
            for command in [
                "git push --force",
                "git push origin --force",
                "git -c a=b push --force",
                "git ${X:-push} --force",
                "git $'push' $'--force'",
                "git pu$'sh' --force",
                "git push $FLAGS",
                "$GIT push --force",
            ] {
                assert!(holds(&rule, command), "{command}");
            }
            for command in [
                "git push origin",
                "git --force push",
                "git status --force",
                "gh push --force",
            ] {
                assert!(!holds(&rule, command), "{command}");
            }

            let any_program = exec("* --force");
            assert!(holds(&any_program, "git push origin --force"));
            assert!(!holds(&any_program, "git push origin"));
        }

        #[test]
        fn exec_deny_folds_each_word_where_allow_compares_it_exactly() {
            let rule = exec("rm -rf *");
            for command in ["RM -Rf x", "/BIN/RM -RF x", "r\u{200c}m -rf x"] {
                assert!(holds(&rule, command), "{command}");
            }
            assert!(!allows(&rule, "RM -rf x"));
            assert!(allows(&rule, "rm -rf x"));
        }

        #[test]
        fn path_name_pattern_at_any_depth_below_an_anchor_and_everything_under_it() {
            let rule = edit("target");
            for (relative, expected) in [
                ("target", true),
                ("target/debug/ox", true),
                ("crates/x/target/a.o", true),
                ("targets/a", false),
                ("src/target.rs", false),
            ] {
                let rooted = [relative.to_owned()];
                let canonical = format!("/repo/{relative}");
                assert_eq!(
                    matches_path(&rule, &path(&canonical, &rooted), MatchDiscipline::Allow),
                    expected,
                    "{relative}"
                );
            }
            assert!(
                !matches_path(
                    &rule,
                    &path("/elsewhere/target", &[]),
                    MatchDiscipline::Deny
                ),
                "a name pattern applies only below an anchor directory"
            );
        }

        #[test]
        fn path_slash_anchors_a_pattern_at_each_anchor_directory() {
            for raw in ["src/*.rs", "./src/*.rs", "src/*.rs/"] {
                let rule = edit(raw);
                for (relative, expected) in [
                    ("src/a.rs", true),
                    ("src/a/b.rs", false),
                    ("crates/x/src/a.rs", false),
                ] {
                    let rooted = [relative.to_owned()];
                    assert_eq!(
                        matches_path(&rule, &path("/repo/x", &rooted), MatchDiscipline::Allow),
                        expected,
                        "{raw} vs {relative}"
                    );
                }
            }
            let top = edit("./a.rs");
            let nested = ["sub/a.rs".to_owned()];
            assert!(!matches_path(
                &top,
                &path("/repo/sub/a.rs", &nested),
                MatchDiscipline::Allow
            ));
        }

        #[test]
        fn path_absolute_and_home_patterns_match_the_resolved_path() {
            let etc = edit(&abs("/etc/**"));
            assert!(matches_path(
                &etc,
                &path(&abs("/etc/passwd"), &[]),
                MatchDiscipline::Allow
            ));
            let inside = ["etc/x".to_owned()];
            assert!(!matches_path(
                &etc,
                &path(&abs("/repo/etc/x"), &inside),
                MatchDiscipline::Allow
            ));

            let nginx = edit(&format!("{}/", abs("/etc/nginx")));
            for target in [abs("/etc/nginx"), abs("/etc/nginx/nginx.conf")] {
                assert!(
                    matches_path(&nginx, &path(&target, &[]), MatchDiscipline::Allow),
                    "a trailing `/` is ignored: {target}"
                );
            }

            temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                let notes = edit("~/notes/");
                assert!(matches_path(
                    &notes,
                    &path(&abs("/home/u/notes"), &[]),
                    MatchDiscipline::Allow
                ));
                assert!(matches_path(
                    &notes,
                    &path(&abs("/home/u/notes/today.md"), &[]),
                    MatchDiscipline::Allow
                ));
                let literal = ["~/notes/today.md".to_owned()];
                assert!(!matches_path(
                    &notes,
                    &path("/repo/~/notes/today.md", &literal),
                    MatchDiscipline::Allow
                ));
            });
        }

        #[test]
        fn path_home_pattern_expands_a_home_spelled_with_a_trailing_slash_or_dot() {
            let home = abs("/home/u");
            for (spelling, target) in [
                (format!("{home}/"), abs("/home/u/notes/a.md")),
                (format!("{home}/."), abs("/home/u/notes/a.md")),
                (format!("{home}/x/.."), abs("/home/u/notes/a.md")),
                (abs("/"), abs("/notes/a.md")),
            ] {
                temp_env::with_var(HOME_VAR, Some(&spelling), || {
                    let notes = edit("~/notes/*.md");
                    assert!(
                        matches_path(&notes, &path(&target, &[]), MatchDiscipline::Allow),
                        "{spelling}"
                    );
                });
            }
        }

        #[test]
        fn path_deny_also_matches_the_requested_spelling_and_folded_names() {
            // `.env -> /shared/.env`: a deny on the spelled path must fire, and an allow on it must
            // not reach a target that resolves elsewhere.
            let (canonical, requested) = (abs("/shared/.env"), abs("/repo/.env"));
            let deny_rooted = [".env".to_owned()];
            let env = Target::Path {
                canonical: &canonical,
                relative: None,
                rooted: &[],
                requested: &requested,
                deny_rooted: &deny_rooted,
                zone: Zone::Outside,
                protected: &[],
            };
            let absolute = edit(&abs("/repo/.env"));
            for rule in [edit(".env"), absolute] {
                assert!(
                    matches_path(&rule, &env, MatchDiscipline::Deny),
                    "{}",
                    rule.pattern()
                );
                assert!(
                    !matches_path(&rule, &env, MatchDiscipline::Allow),
                    "{}",
                    rule.pattern()
                );
            }

            let upper = [".ENV.".to_owned()];
            let folded = path("/repo/.ENV.", &upper);
            assert!(matches_path(&edit(".env"), &folded, MatchDiscipline::Deny));
            assert!(!matches_path(
                &edit(".env"),
                &folded,
                MatchDiscipline::Allow
            ));
        }

        #[cfg(windows)]
        #[test]
        fn path_reads_backslash_separators_and_needs_a_drive() {
            let rule = edit(r"src\*.rs");
            let rooted = [r"src\a.rs".to_owned()];
            assert!(matches_path(
                &rule,
                &path(r"C:\repo\src\a.rs", &rooted),
                MatchDiscipline::Allow
            ));
            let dotted = edit(r"C:\repo\secrets");
            let target = path(r"C:\repo\secrets.\key", &[]);
            assert!(matches_path(&dotted, &target, MatchDiscipline::Deny));
            assert!(!matches_path(&dotted, &target, MatchDiscipline::Allow));
            for raw in ["/secrets/**", r"\tmp\**"] {
                let err = format!("{:#}", Rule::parse(PermissionClass::Read, raw).unwrap_err());
                assert!(err.contains("needs a drive"), "{raw}: {err}");
            }
        }

        #[test]
        fn host_globs_without_case_and_tool_globs_within_each_name() {
            let host = rule(PermissionClass::Net, "*.github.com");
            for (name, expected) in [
                ("api.github.com", true),
                ("a.b.GitHub.com", true),
                ("github.com", false),
                ("github.com.evil.io", false),
            ] {
                assert_eq!(
                    host.matches(
                        PermissionClass::Net,
                        &Target::Name(name),
                        MatchDiscipline::Allow
                    ),
                    expected,
                    "{name}"
                );
            }

            let tool = rule(PermissionClass::Mcp, "github/get_*");
            for (name, expected) in [
                ("github/get_me", true),
                ("github/create_issue", false),
                ("github/get_x/y", false),
                ("gitlab/get_me", false),
            ] {
                assert_eq!(
                    tool.matches(
                        PermissionClass::Mcp,
                        &Target::Name(name),
                        MatchDiscipline::Deny
                    ),
                    expected,
                    "{name}"
                );
            }
        }
    }

    mod with_resolved_prefixes {
        use super::*;

        #[test]
        fn also_matches_where_a_literal_prefix_resolves() {
            let dir = canonical_tempdir();
            let root = dir.path().to_path_buf();
            std::fs::create_dir(root.join("real")).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
            // A Windows symlink needs Developer Mode or admin rights, which CI runners have.
            #[cfg(windows)]
            std::os::windows::fs::symlink_dir("real", root.join("alias")).unwrap();
            let alias = root.join("alias");
            let root_glob = slash_separated(&root);

            for (raw, anchor, inside, outside) in [
                (
                    format!("{root_glob}/alias/sub*/x.rs"),
                    &root,
                    "real/sub1/x.rs",
                    "real/other/x.rs",
                ),
                ("*.rs".to_owned(), &alias, "real/a.rs", "real/sub/b.c"),
                ("./x.rs".to_owned(), &alias, "real/x.rs", "real/y.rs"),
                (
                    format!("{root_glob}/alias/x.rs"),
                    &root,
                    "real/x.rs",
                    "real/y.rs",
                ),
            ] {
                let rule = Rule::parse(PermissionClass::Read, &raw)
                    .unwrap()
                    .with_resolved_prefixes(std::slice::from_ref(anchor))
                    .unwrap();
                assert_eq!(rule.pattern(), raw, "the copy keeps the pattern as written");
                let matches = |relative: &str| {
                    let canonical = format!("{root_glob}/{relative}");
                    matches_path(&rule, &path(&canonical, &[]), MatchDiscipline::Deny)
                };
                assert!(matches(inside), "{raw}");
                assert!(!matches(outside), "{raw}");
            }

            let unchanged = Rule::parse(PermissionClass::Read, &format!("{root_glob}/real/**"))
                .unwrap()
                .with_resolved_prefixes(std::slice::from_ref(&root))
                .unwrap();
            let Pattern::Path(globs) = &unchanged.pattern else {
                panic!("a read rule has a path pattern");
            };
            assert_eq!(globs.len(), 1);
        }
    }

    mod literal_prefix {
        use super::*;

        #[test]
        fn splits_at_the_last_slash_before_a_glob_and_keeps_a_root_slash() {
            for (glob, expected) in [
                ("/a/b*/c", ("/a", "b*/c")),
                ("/secrets*", ("/", "secrets*")),
                ("C:/secrets*", ("C:/", "secrets*")),
                ("src/a", ("src/a", "")),
                ("*.rs", ("", "*.rs")),
            ] {
                assert_eq!(literal_prefix(glob), expected, "{glob}");
            }
        }
    }
}
