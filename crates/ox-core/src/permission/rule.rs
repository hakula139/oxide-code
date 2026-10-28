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
use regex_syntax::hir::literal::Extractor;

use super::shell::{PatternWord, Segment, ShellWord, bare_program, pattern_words};
use super::{
    Access, DirectoryAliases, PathTarget, PermissionClass, fold_name, relative_to_folded, resolve,
};
use crate::util::path::{lexical_normalize, slash_separated};
use crate::util::text::is_hidden;

// ── Rule ──

/// One parsed permission rule.
#[derive(Debug, Clone)]
pub struct Rule {
    class: PermissionClass,
    /// The pattern as written, such as `cargo test *`.
    source: String,
    pattern: Pattern,
}

/// The matchable body of a rule, chosen by its class at parse time.
#[derive(Debug, Clone)]
enum Pattern {
    /// `*`, which matches every call of the class.
    Any,
    Exec(Vec<PatternWord>),
    Path(PathGlob),
    Host(GlobMatcher),
    Tool(GlobMatcher),
}

/// A `read` / `edit` glob that also matches everything below a path it matches, compiled as written
/// and over [`fold_name`]-folded names. Allow matches the first alone, and deny matches either, so
/// a case or normalization variant cannot slip past.
#[derive(Debug, Clone)]
struct PathGlob {
    /// Whether the glob matches absolute paths, or paths below each anchor directory.
    absolute: bool,
    /// Whether the glob also matches everything below a path it matches, as every pattern a user
    /// writes does.
    below: bool,
    literal: PathBuf,
    exact: GlobSet,
    folded: GlobSet,
}

/// Where a [built-in protected name](Rule::protected_name) takes effect.
#[derive(Debug, Clone, Copy)]
pub(super) enum Placement {
    /// At any depth on disk, under any root.
    Anywhere,
    /// Directly under each anchor directory, so the policy also protects where the name resolves
    /// when it is a link.
    Anchored,
}

/// The tier a match is evaluated for, selecting how commands, symlinked paths, and name spellings
/// match. Naming it at every match keeps the allow / deny asymmetry legible.
#[derive(Debug, Clone, Copy)]
pub(super) enum MatchDiscipline<'a> {
    /// Every simple command of a command the analysis recognizes must match its named words, each
    /// literal one exactly, and a path matches only where it resolves, with names exactly as the
    /// pattern writes them. Neither a chained command, a symlink, nor a variant spelling widens.
    Allow,
    /// Any simple command may match, by its named words, its program words, or its program's bare
    /// name. The pattern's later words may sit among other words, each word compares folded as
    /// names do, and a word that expands matches any pattern word. A path matches as resolved or as
    /// spelled, with names folded too, or below where the rule's literal prefix lands. A danger
    /// behind a safe head, an option, an expansion, a symlink, or a variant spelling is caught.
    Deny(Prefixes<'a>),
}

/// Where a deny or ask path rule's literal prefix lands, so a path that reaches it through another
/// spelling of the prefix still matches. Empty for any other rule.
#[derive(Debug, Clone, Copy)]
pub(super) enum Prefixes<'a> {
    /// The [`Rule::prefix_bases`] of the rule, each also tried where it resolves at match time.
    Located(&'a [PathBuf]),
    /// The same locations in one walk's spelling, split into [`fold_name`]-folded components.
    Walk(&'a [Vec<String>]),
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
                    Pattern::Path(PathGlob::parse(source)?)
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
    pub(super) fn for_call(class: PermissionClass, target: &Access) -> Option<Self> {
        let pattern = match target {
            Access::Command(command) => match command.segments() {
                [segment] => command_pattern(segment.named())?,
                _ => return None,
            },
            Access::Path(path) => match path.relative.as_deref() {
                Some("") => return None,
                Some(relative) => format!("./{}", escaped(Path::new(relative))),
                None => escaped(Path::new(&path.canonical)),
            },
            Access::Name(name) => globset::escape(name),
            Access::None => return None,
        };
        if !pattern.chars().all(paints_as_itself) {
            return None;
        }
        let rule = Self::parse(class, &pattern).ok()?;
        rule.matches(class, target, MatchDiscipline::Allow)
            .then_some(rule)
    }

    /// The built-in `edit` rule protecting `name`, a relative path such as `.git` or
    /// `.cargo/config`, where it sits at `placement`. A `dir` also covers everything below it, and
    /// any other name matches only itself.
    pub(super) fn protected_name(name: &str, placement: Placement, dir: bool) -> Result<Self> {
        let (glob, absolute, source) = match placement {
            Placement::Anywhere => (format!("**/{name}"), true, format!("/**/{name}")),
            Placement::Anchored => (name.to_owned(), false, format!("./{name}")),
        };
        Ok(Self {
            class: PermissionClass::Edit,
            source,
            pattern: Pattern::Path(PathGlob::new(&glob, absolute, dir)?),
        })
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
        target: &Access,
        discipline: MatchDiscipline,
    ) -> bool {
        if self.class != class {
            return false;
        }
        match (&self.pattern, target) {
            (_, Access::Path(path)) => self.path_matches(path, discipline),
            (_, Access::Command(command)) if matches!(discipline, MatchDiscipline::Allow) => {
                command.unrecognized().is_none()
                    && !command.segments().is_empty()
                    && command
                        .segments()
                        .iter()
                        .all(|segment| self.allows(segment))
            }
            (Pattern::Any, _) => true,
            (Pattern::Exec(words), Access::Command(command)) => command
                .segments()
                .iter()
                .any(|segment| segment_held(words, segment)),
            (Pattern::Host(glob) | Pattern::Tool(glob), Access::Name(name)) => glob.is_match(name),
            _ => false,
        }
    }

    /// Returns whether this rule matches an access of `path` in `class` under `discipline`.
    pub(super) fn matches_path(
        &self,
        class: PermissionClass,
        path: &PathTarget,
        discipline: MatchDiscipline,
    ) -> bool {
        self.class == class && self.path_matches(path, discipline)
    }

    fn path_matches(&self, path: &PathTarget, discipline: MatchDiscipline) -> bool {
        match &self.pattern {
            Pattern::Any => true,
            Pattern::Path(glob) => glob.is_match(path, discipline),
            Pattern::Exec(_) | Pattern::Host(_) | Pattern::Tool(_) => false,
        }
    }

    /// Returns whether this `exec` rule allows `segment`, one simple command of a command the
    /// analysis recognizes.
    pub(super) fn allows(&self, segment: &Segment) -> bool {
        match &self.pattern {
            Pattern::Any => self.class == PermissionClass::Exec,
            Pattern::Exec(words) => words_allowed(words, segment.named()),
            Pattern::Path(_) | Pattern::Host(_) | Pattern::Tool(_) => false,
        }
    }

    /// Returns whether a deny or ask `read` rule may match `path` or a recursive descendant, given
    /// the rule's [`Self::prefix_bases`].
    pub(super) fn holds_read(&self, path: &PathTarget, recursive: bool, bases: &[PathBuf]) -> bool {
        let discipline = MatchDiscipline::Deny(Prefixes::Located(bases));
        if self.matches_path(PermissionClass::Read, path, discipline) {
            return true;
        }
        self.class == PermissionClass::Read
            && recursive
            && matches!(self.pattern, Pattern::Path(_))
            && may_match_below(bases, [&path.canonical, &path.requested])
    }

    /// Where this rule's literal path prefix lands, rooted at each absolute anchor unless the
    /// pattern is absolute, plus where each such location resolves now. Empty for any rule but a
    /// path glob.
    pub(super) fn prefix_bases(&self, anchors: &[PathBuf]) -> Vec<PathBuf> {
        let Pattern::Path(glob) = &self.pattern else {
            return Vec::new();
        };
        let mut bases: Vec<PathBuf> = if glob.absolute {
            Some(glob.literal.clone())
                .filter(|literal| !literal.as_os_str().is_empty())
                .into_iter()
                .collect()
        } else if glob.literal.has_root() {
            Vec::new()
        } else {
            anchors
                .iter()
                .filter(|anchor| anchor.is_absolute())
                .map(|anchor| anchor.join(&glob.literal))
                .collect()
        };
        let resolved: Vec<PathBuf> = bases.iter().filter_map(|base| resolve(base).ok()).collect();
        bases.extend(resolved);
        bases.sort();
        bases.dedup();
        bases
    }
}

/// Rules compare by their class and pattern, which parse to the same matcher every time, and by
/// whether a path pattern covers what lies below a match, which only a built-in name can turn off.
impl PartialEq for Rule {
    fn eq(&self, other: &Self) -> bool {
        let below = |rule: &Self| match &rule.pattern {
            Pattern::Path(glob) => glob.below,
            Pattern::Any | Pattern::Exec(_) | Pattern::Host(_) | Pattern::Tool(_) => true,
        };
        self.class == other.class && self.source == other.source && below(self) == below(other)
    }
}

impl Eq for Rule {}

/// Returns whether `c` paints as one visible cell or more, or is a plain space.
fn paints_as_itself(c: char) -> bool {
    c == ' ' || !(c.is_whitespace() || is_hidden(c))
}

/// `path` with its separators as `/` and every glob character escaped, so it matches only itself.
pub(super) fn escaped(path: &Path) -> String {
    globset::escape(&slash_separated(path)).replace('\\', "\\\\")
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
            return Self::new(&pattern, true, true);
        }

        let anchored = pattern.starts_with("./");
        let body = pattern
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_owned();
        ensure!(!body.is_empty(), "the pattern names no path");
        if anchored || body.contains('/') {
            Self::new(&body, false, true)
        } else {
            Self::new(&format!("**/{body}"), false, true)
        }
    }

    /// Compiles `glob`, and with `below` everything under a path it matches.
    fn new(glob: &str, absolute: bool, below: bool) -> Result<Self> {
        let globs: Vec<String> = std::iter::once(glob.to_owned())
            .chain(below.then(|| format!("{}/**", glob.trim_end_matches('/'))))
            .collect();
        let set = |globs: &mut dyn Iterator<Item = String>| -> Result<GlobSet> {
            let mut builder = GlobSetBuilder::new();
            for glob in globs {
                builder.add(path_glob(&glob)?);
            }
            Ok(builder.build()?)
        };
        Ok(Self {
            exact: set(&mut globs.iter().cloned())?,
            folded: set(&mut globs.iter().map(|glob| fold_path(glob)))?,
            absolute,
            below,
            literal: literal_prefix(&path_glob(glob)?),
        })
    }

    /// Returns whether this glob matches `path` under `discipline`.
    fn is_match(&self, path: &PathTarget, discipline: MatchDiscipline) -> bool {
        let PathTarget {
            canonical,
            rooted,
            requested,
            deny_rooted,
            ..
        } = path;
        let prefixes = match discipline {
            MatchDiscipline::Allow => None,
            MatchDiscipline::Deny(prefixes) => Some(prefixes),
        };
        let deny = prefixes.is_some();
        let hit = |path: &str| {
            self.exact.is_match(path) || (deny && self.folded.is_match(fold_path(path)))
        };
        let written = if self.absolute {
            hit(canonical) || (deny && requested != canonical && hit(requested))
        } else {
            rooted.iter().any(|path| hit(path))
                || (deny && deny_rooted.iter().any(|path| hit(path)))
        };
        written
            || prefixes
                .is_some_and(|prefixes| self.prefix_matches(prefixes, [canonical, requested], hit))
    }

    fn prefix_matches(
        &self,
        prefixes: Prefixes,
        paths: [&str; 2],
        hit: impl Fn(&str) -> bool,
    ) -> bool {
        let projected_match = |relative: &str| {
            let mut projected = self.literal.clone();
            if !relative.is_empty() {
                projected.push(relative);
            }
            !paths.iter().any(|path| projected == Path::new(path))
                && hit(&slash_separated(&projected))
        };
        match prefixes {
            Prefixes::Walk(bases) => bases.iter().any(|base| {
                paths.iter().any(|path| {
                    relative_to_folded(Path::new(path), base)
                        .is_some_and(|relative| projected_match(&relative))
                })
            }),
            Prefixes::Located(bases) => {
                let mut aliases = DirectoryAliases::default();
                prefix_locations(bases).any(|base| {
                    paths.iter().any(|path| {
                        aliases
                            .relative_to(Path::new(path), &base)
                            .is_some_and(|relative| projected_match(&relative))
                    })
                })
            }
        }
    }
}

/// Returns whether either of `paths` and a location of `bases` sit one within the other.
fn may_match_below(bases: &[PathBuf], paths: [&str; 2]) -> bool {
    let mut aliases = DirectoryAliases::default();
    prefix_locations(bases).any(|base| {
        paths.iter().any(|path| {
            let path = Path::new(path);
            aliases.relative_to(&base, path).is_some() || aliases.relative_to(path, &base).is_some()
        })
    })
}

/// Each of `bases`, followed by where it resolves now when that differs.
fn prefix_locations(bases: &[PathBuf]) -> impl Iterator<Item = PathBuf> + '_ {
    bases.iter().flat_map(|base| {
        std::iter::once(base.clone()).chain(resolve(base).ok().filter(|path| path != base))
    })
}

/// `literal_separator` keeps `*` from crossing `/`, so `*.json` names one level of files.
fn path_glob(glob: &str) -> Result<Glob> {
    Ok(GlobBuilder::new(glob).literal_separator(true).build()?)
}

/// The complete fixed path components at the start of `glob`, decoded by its parsed grammar.
fn literal_prefix(glob: &Glob) -> PathBuf {
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .build()
        .parse(glob.regex())
        .expect("globset emits valid regular expressions");
    let literals = Extractor::new().limit_literal_len(usize::MAX).extract(&hir);
    let Some(mut prefix) = literals.longest_common_prefix() else {
        return PathBuf::new();
    };
    let complete = literals
        .literals()
        .is_some_and(|literals| matches!(literals, [literal] if literal.is_exact()));
    if !complete {
        let end = prefix
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |slash| {
                if slash == 0 || prefix[slash - 1] == b':' {
                    slash + 1
                } else {
                    slash
                }
            });
        prefix = &prefix[..end];
    }
    PathBuf::from(std::str::from_utf8(prefix).expect("a fixed path prefix is UTF-8"))
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

    fn edit(pattern: &str) -> Rule {
        rule(PermissionClass::Edit, pattern)
    }

    fn exec(pattern: &str) -> Rule {
        rule(PermissionClass::Exec, pattern)
    }

    fn cmd(s: &str) -> Access {
        Access::for_command(s, &Workspace::new(Path::new("/repo")))
    }

    fn allows(rule: &Rule, command: &str) -> bool {
        rule.matches(PermissionClass::Exec, &cmd(command), MatchDiscipline::Allow)
    }

    /// A path target with `canonical` resolved, rooted at the given relative spellings.
    fn path(canonical: &str, rooted: &[String]) -> PathTarget {
        PathTarget {
            canonical: canonical.to_owned(),
            exact: None,
            relative: rooted.first().cloned(),
            rooted: rooted.to_vec(),
            requested: canonical.to_owned(),
            deny_rooted: Vec::new(),
            zone: Zone::Outside,
        }
    }

    /// Deny discipline for a rule that holds no path pattern, and so has no prefix bases.
    const DENY: MatchDiscipline<'static> = MatchDiscipline::Deny(Prefixes::Located(&[]));

    fn allows_path(rule: &Rule, path: &PathTarget) -> bool {
        rule.matches_path(rule.class(), path, MatchDiscipline::Allow)
    }

    /// Whether `rule` holds `path` back with the prefix bases it has in a workspace at `anchor`.
    fn denies_path_from(rule: &Rule, path: &PathTarget, anchor: &Path) -> bool {
        let bases = rule.prefix_bases(&[anchor.to_path_buf()]);
        rule.matches_path(
            rule.class(),
            path,
            MatchDiscipline::Deny(Prefixes::Located(&bases)),
        )
    }

    fn denies_path(rule: &Rule, path: &PathTarget) -> bool {
        denies_path_from(rule, path, Path::new(&abs("/repo")))
    }

    mod parse {
        use super::*;

        #[test]
        fn star_covers_the_whole_class_including_a_call_without_a_target() {
            let any = rule(PermissionClass::Read, " * ");
            assert_eq!(any.pattern(), "*");
            for target in [Access::None, Access::Path(path("/anywhere/x", &[]))] {
                assert!(
                    any.matches(any.class(), &target, MatchDiscipline::Allow),
                    "{target:?}"
                );
            }
            assert!(!any.matches(PermissionClass::Edit, &Access::None, DENY));
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
                let rule = Rule::for_call(PermissionClass::Exec, &target).unwrap();
                assert_eq!(rule.pattern(), expected, "{command:?}");
                assert!(allows(&rule, command), "{command:?}");
            }
            assert!(!allows(&exec("cargo test"), "cargo test --all"));
        }

        #[test]
        fn names_the_path_under_dot_slash_inside_cwd_and_absolute_outside() {
            let canonical = abs("/repo/src/a[1].rs");
            let rooted = ["src/a[1].rs".to_owned()];
            let rule = Rule::for_call(
                PermissionClass::Edit,
                &Access::Path(path(&canonical, &rooted)),
            )
            .unwrap();
            assert_eq!(rule.pattern(), "./src/a[[]1[]].rs");
            let other = ["src/a1.rs".to_owned()];
            assert!(!allows_path(&rule, &path(&abs("/repo/src/a1.rs"), &other)));

            let top = ["a.rs".to_owned()];
            let rule = Rule::for_call(
                PermissionClass::Edit,
                &Access::Path(path(&abs("/repo/a.rs"), &top)),
            )
            .unwrap();
            assert_eq!(
                rule.pattern(),
                "./a.rs",
                "a bare name would match at any depth"
            );

            let outside = abs("/etc/hosts");
            let rule =
                Rule::for_call(PermissionClass::Edit, &Access::Path(path(&outside, &[]))).unwrap();
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
                    Rule::for_call(PermissionClass::Exec, &cmd(command)).is_none(),
                    "{command:?}"
                );
            }
            let empty = [String::new()];
            let hidden = ["a\u{200b}.rs".to_owned()];
            let hidden_path = abs("/repo/a\u{200b}.rs");
            for target in [
                Access::Path(path(&abs("/repo"), &empty)),
                Access::Path(path(&hidden_path, &hidden)),
                Access::None,
            ] {
                assert!(
                    Rule::for_call(PermissionClass::Edit, &target).is_none(),
                    "{target:?}"
                );
            }
        }

        #[test]
        fn names_a_host_or_tool_by_its_escaped_name() {
            let name = |name: &str| Access::Name(name.to_owned());
            let host = Rule::for_call(PermissionClass::Net, &name("api.example.com")).unwrap();
            assert_eq!(host.pattern(), "api.example.com");

            let tool = Rule::for_call(PermissionClass::Mcp, &name("srv/a*b")).unwrap();
            assert_eq!(tool.pattern(), "srv/a[*]b");
            let matches =
                |called| tool.matches(PermissionClass::Mcp, &name(called), MatchDiscipline::Allow);
            assert!(matches("srv/a*b"));
            assert!(!matches("srv/axb"));
        }
    }

    mod matches {
        use super::*;

        fn holds(rule: &Rule, command: &str) -> bool {
            rule.matches(PermissionClass::Exec, &cmd(command), DENY)
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
        fn exec_star_allows_only_a_command_the_analysis_recognizes() {
            let rule = exec("*");
            for command in ["cargo test && rm -rf /", "sh -c ls", "eval ls"] {
                assert!(allows(&rule, command), "{command}");
            }
            for command in [r#"eval "$CMD""#, r#"sh -c "$X""#, "$EDITOR notes", "FOO=1"] {
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
                    allows_path(&rule, &path(&canonical, &rooted)),
                    expected,
                    "{relative}"
                );
            }
            assert!(
                !denies_path(&rule, &path("/elsewhere/target", &[])),
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
                        allows_path(&rule, &path("/repo/x", &rooted)),
                        expected,
                        "{raw} vs {relative}"
                    );
                }
            }
            let top = edit("./a.rs");
            let nested = ["sub/a.rs".to_owned()];
            assert!(!allows_path(&top, &path("/repo/sub/a.rs", &nested)));
        }

        #[test]
        fn path_absolute_and_home_patterns_match_the_resolved_path() {
            let etc = edit(&abs("/etc/**"));
            assert!(allows_path(&etc, &path(&abs("/etc/passwd"), &[])));
            let inside = ["etc/x".to_owned()];
            assert!(!allows_path(&etc, &path(&abs("/repo/etc/x"), &inside)));

            let nginx = edit(&format!("{}/", abs("/etc/nginx")));
            for target in [abs("/etc/nginx"), abs("/etc/nginx/nginx.conf")] {
                assert!(
                    allows_path(&nginx, &path(&target, &[])),
                    "a trailing `/` is ignored: {target}"
                );
            }

            temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                let notes = edit("~/notes/");
                assert!(allows_path(&notes, &path(&abs("/home/u/notes"), &[])));
                assert!(allows_path(
                    &notes,
                    &path(&abs("/home/u/notes/today.md"), &[])
                ));
                let literal = ["~/notes/today.md".to_owned()];
                assert!(!allows_path(
                    &notes,
                    &path("/repo/~/notes/today.md", &literal)
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
                    assert!(allows_path(&notes, &path(&target, &[])), "{spelling}");
                });
            }
        }

        #[test]
        fn path_deny_also_matches_the_requested_spelling_and_folded_names() {
            // `.env -> /shared/.env`: a deny on the spelled path must fire, and an allow on it must
            // not reach a target that resolves elsewhere.
            let (canonical, requested) = (abs("/shared/.env"), abs("/repo/.env"));
            let deny_rooted = [".env".to_owned()];
            let env = PathTarget {
                relative: None,
                rooted: Vec::new(),
                requested,
                deny_rooted: deny_rooted.to_vec(),
                ..path(&canonical, &[])
            };
            let absolute = edit(&abs("/repo/.env"));
            for rule in [edit(".env"), absolute] {
                assert!(denies_path(&rule, &env), "{}", rule.pattern());
                assert!(!allows_path(&rule, &env), "{}", rule.pattern());
            }

            let upper = [".ENV.".to_owned()];
            let folded = path("/repo/.ENV.", &upper);
            assert!(denies_path(&edit(".env"), &folded));
            assert!(!allows_path(&edit(".env"), &folded));
        }

        #[cfg(windows)]
        #[test]
        fn path_reads_backslash_separators_and_needs_a_drive() {
            let rule = edit(r"src\*.rs");
            let rooted = [r"src\a.rs".to_owned()];
            assert!(allows_path(&rule, &path(r"C:\repo\src\a.rs", &rooted)));
            let dotted = edit(r"C:\repo\secrets");
            let target = path(r"C:\repo\secrets.\key", &[]);
            assert!(denies_path(&dotted, &target));
            assert!(!allows_path(&dotted, &target));
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
                        &Access::Name(name.to_owned()),
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
                    tool.matches(PermissionClass::Mcp, &Access::Name(name.to_owned()), DENY),
                    expected,
                    "{name}"
                );
            }
        }
    }

    mod holds_read {
        use super::*;

        #[test]
        fn intersects_recursive_trees_with_decoded_literal_prefixes() {
            let dir = canonical_tempdir();
            let root = dir.path();
            for (pattern, name, recursive, expected) in [
                ("./vault/key", "", true, true),
                ("./vault/key", "vault", true, true),
                ("./vault/key", "vault", false, false),
                ("./vault/key", "src", true, false),
                ("./vault/key", "VAULT", true, true),
                ("./vault/key", "vault/key", false, true),
                ("./vault/*.key", "vault/public", true, true),
                ("./vault/*.key", "src", true, false),
                ("./vault[[]1[]]/key", "vault[1]", true, true),
                ("./vault[[]1[]]/key", "vault1", true, false),
                ("secret", "src", true, true),
            ] {
                let rule = rule(PermissionClass::Read, pattern);
                let bases = rule.prefix_bases(&[root.to_path_buf()]);
                let canonical = slash_separated(&root.join(name));
                let rooted = [name.to_owned()];

                let held = rule.holds_read(&path(&canonical, &rooted), recursive, &bases);

                assert_eq!(
                    held, expected,
                    "{pattern}, path={name}, recursive={recursive}"
                );
            }
        }

        #[test]
        fn keeps_class_requirements() {
            let vault = path(&abs("/repo/vault"), &["vault".to_owned()]);
            let any = rule(PermissionClass::Read, "*");
            let edit = rule(PermissionClass::Edit, "*");

            assert!(any.holds_read(&vault, false, &[]));
            assert!(!edit.holds_read(&vault, true, &[]));
        }
    }

    mod prefix_bases {
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
                let rule = Rule::parse(PermissionClass::Read, &raw).unwrap();
                let matches = |relative: &str| {
                    let canonical = format!("{root_glob}/{relative}");
                    denies_path_from(&rule, &path(&canonical, &[]), anchor)
                };
                assert!(matches(inside), "{raw}");
                assert!(!matches(outside), "{raw}");
            }

            let unchanged =
                Rule::parse(PermissionClass::Read, &format!("{root_glob}/real/**")).unwrap();
            assert!(denies_path_from(
                &unchanged,
                &path(&format!("{root_glob}/real/key"), &[]),
                &root
            ));
        }

        #[cfg(windows)]
        #[test]
        fn absolute_prefixes_keep_drive_and_share_roots_distinct() {
            for (pattern, base, same, different) in [
                (
                    "C:/secrets/key",
                    "C:/real/key",
                    "c:/REAL/key",
                    "D:/real/key",
                ),
                (
                    "//server/one/secrets/key",
                    r"\\server\one\real\key",
                    "//SERVER/one/real/key",
                    "//server/two/real/key",
                ),
                (
                    "//server/one/secrets/key",
                    "//server/one/real/key",
                    r"\\SERVER\one\real\key",
                    r"\\server\two\real\key",
                ),
            ] {
                let rule = edit(pattern);
                let bases = [crate::permission::folded_components(Path::new(base))];
                let walk = MatchDiscipline::Deny(Prefixes::Walk(&bases));

                assert!(
                    rule.matches_path(rule.class(), &path(same, &[]), walk),
                    "{same}"
                );
                assert!(
                    !rule.matches_path(rule.class(), &path(different, &[]), walk),
                    "{different}"
                );
            }
        }
    }

    mod literal_prefix {
        use super::*;

        #[test]
        fn decodes_complete_fixed_components_and_keeps_a_root_slash() {
            for (glob, expected) in [
                ("/a/b*/c", "/a"),
                ("/secrets*", "/"),
                ("C:/secrets*", "C:/"),
                ("src/a", "src/a"),
                ("*.rs", ""),
                ("/a[[]1[]]/[*]/?.rs", "/a[1]/*"),
                ("/a[b]/[x-x]/{one}/file", "/ab/x/one/file"),
                ("/a/{one,two}/file", "/a"),
                ("/a/[!x]/file", "/a"),
                ("/a/{one/two,three}/file", "/a"),
                ("/a/{one/two}/file", "/a/one/two/file"),
                ("/a[/]b/file", "/a/b/file"),
                ("/caf\u{e9}/[[]/file", "/caf\u{e9}/[/file"),
                ("/{caf\u{e9},caf\u{e8}}/file", "/"),
            ] {
                assert_eq!(
                    literal_prefix(&path_glob(glob).unwrap()),
                    PathBuf::from(expected),
                    "{glob}"
                );
            }
            #[cfg(unix)]
            assert_eq!(
                literal_prefix(&path_glob(r"/a\/b/file").unwrap()),
                Path::new("/a/b/file")
            );
        }
    }
}
