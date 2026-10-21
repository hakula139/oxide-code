//! Tool permission gate. A resolved [`Policy`] (mode plus the deny, ask, and allow rule tiers) and
//! a [`Target`] describing a tool call resolve to a [`Verdict`] through the pure
//! [`Policy::verdict`] pipeline: a [`Decision`] of allow / ask / deny and the [`Basis`] that
//! settled it.
//!
//! oxide-code has no sandbox, so this gate is the whole safety boundary around a tool call.

pub mod defaults;
pub mod rule;
pub(crate) mod shell;

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, LazyLock};

use anyhow::{Result, bail, ensure};
use caseless::Caseless;
use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::permission::defaults::{Shipped, ShippedTier, protected_by, protected_roots};
use crate::permission::rule::{MatchDiscipline, Rule};
use crate::permission::shell::{CommandTarget, Segment};
use crate::util::path::{expand_user, lexical_normalize};

// ── Mode ──

/// The standing permission posture, which settles the calls no rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Asks about every call the rules leave open.
    #[default]
    Manual,
    /// Read-only analysis: every edit, exec, and net call is denied.
    Plan,
    /// Runs every call the rules leave open. Deny and ask rules still hold.
    Yolo,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Manual, Self::Plan, Self::Yolo];
    pub(crate) const VALID_VALUES: &str = "manual, plan, yolo";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Plan => "plan",
            Self::Yolo => "yolo",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Manual => "Ask before tool calls that no permission rule allows or denies",
            Self::Plan => "Read-only: deny edits and commands",
            Self::Yolo => "Run tool calls without asking unless a rule denies or asks",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "manual" => Ok(Self::Manual),
            "plan" => Ok(Self::Plan),
            "yolo" => Ok(Self::Yolo),
            _ => bail!("expected one of {}", Self::VALID_VALUES),
        }
    }
}

// ── Permission Class ──

/// The rule namespace a tool's calls fall under, which also sets the pipeline's default treatment.
/// Rules address classes, so one `read` deny pattern covers every tool that reads a file. Each tool
/// declares its class in its [`crate::tool::ToolSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionClass {
    /// Reads the workspace or acts only on state the session owns (`read`, `grep`, `glob`, `tasks`,
    /// `bash_output`, `bash_stop`), so it allows unless a deny or ask rule matches.
    Read,
    /// Mutates the file the call names (`edit`, `write`).
    Edit,
    /// Runs a command with unbounded authority (`bash`).
    Exec,
    /// Reaches a network host. No built-in tool is in this class yet.
    Net,
    /// Calls a tool on an MCP server. No built-in tool is in this class yet.
    Mcp,
}

impl PermissionClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Exec => "exec",
            Self::Net => "net",
            Self::Mcp => "mcp",
        }
    }
}

impl fmt::Display for PermissionClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Tier ──

/// Where a rule list sits in the pipeline: deny outranks ask, which outranks allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Deny,
    Ask,
    Allow,
}

impl Tier {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Workspace ──

/// Where a session's tools run: the launch directory, which relative paths resolve against, and
/// the roots inside which an edit needs no rule. The roots are the launch directory plus
/// `permission.extra_roots`, each resolved the way a path target is, and there are none when the
/// launch directory is `/`, `$HOME`, or an ancestor of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    /// The directories anchored patterns apply below: the cwd, then each extra root while there
    /// are roots.
    anchors: Vec<Anchor>,
}

/// One anchor directory in both of its spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    resolved: PathBuf,
    /// As configured or launched, lexically normalized.
    spelled: PathBuf,
}

impl Workspace {
    /// The workspace launched in `cwd` with no extra roots.
    pub(crate) fn new(cwd: &Path) -> Self {
        let resolved = resolve(cwd).ok();
        Self {
            cwd: cwd.to_path_buf(),
            roots: resolved
                .clone()
                .filter(|cwd| is_workspace(cwd))
                .into_iter()
                .collect(),
            anchors: vec![Anchor {
                resolved: resolved.unwrap_or_else(|| cwd.to_path_buf()),
                spelled: lexical_normalize(cwd),
            }],
        }
    }

    /// The workspace launched in `cwd` plus the configured `extra_roots`, which config load
    /// validates even when the launch directory leaves the workspace without roots.
    pub(crate) fn resolve(cwd: &Path, extra_roots: &[String]) -> Result<Self> {
        let extra_roots = extra_roots
            .iter()
            .map(|raw| {
                Ok(Anchor {
                    resolved: extra_root(raw)?,
                    spelled: lexical_normalize(&expand_user(raw)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut workspace = Self::new(cwd);
        if !workspace.roots.is_empty() {
            for root in extra_roots {
                if !workspace.roots.contains(&root.resolved) {
                    workspace.roots.push(root.resolved.clone());
                    workspace.anchors.push(root);
                }
            }
        }
        Ok(workspace)
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Returns whether the resolved `path` sits inside one of the roots.
    fn contains(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(root))
    }

    /// The cwd as resolved, or as given when it cannot be.
    fn resolved_cwd(&self) -> &Path {
        &self.anchors[0].resolved
    }

    /// Every spelling of `path` relative to an anchor directory in either of its spellings, with
    /// names compared by [`fold_name`].
    fn loose_rooted(&self, path: &Path) -> impl Iterator<Item = String> {
        self.anchors
            .iter()
            .flat_map(|anchor| [&anchor.resolved, &anchor.spelled])
            .filter_map(move |base| loose_relative_to(path, base))
    }
}

/// Resolves the configured extra root `raw`, which must be absolute or `~/`-rooted, resolvable, and
/// neither `/`, `$HOME`, nor an ancestor of it.
pub(crate) fn extra_root(raw: &str) -> Result<PathBuf> {
    let root = expand_user(raw)?;
    ensure!(
        root.is_absolute(),
        "invalid extra root {raw:?}: expected an absolute or `~/` path"
    );
    let root = match resolve(&root) {
        Ok(root) => root,
        Err(unreachable) => bail!("invalid extra root {raw:?}: {unreachable}"),
    };
    ensure!(
        is_workspace(&root),
        "invalid extra root {raw:?}: \
         `/`, the home directory, and their ancestors cannot be workspace roots"
    );
    Ok(root)
}

// ── Target ──

/// Whether a path target sits where an edit needs no rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Zone {
    /// Inside a [`Workspace`] root, so an edit allows without a rule unless a protected name or an
    /// ask rule holds it.
    Workspace,
    /// Anywhere else, so an edit needs an allow rule.
    Outside,
}

/// What a tool call acts on, matched against rule patterns. `bash` carries its parsed command, and
/// path tools carry the spellings described on [`Access::Path`]. `None` is a call with no
/// extractable target (a missing path argument): only a `*` rule can match it.
#[derive(Debug, Clone)]
pub(crate) enum Target<'a> {
    None,
    Command(&'a CommandTarget),
    Path {
        canonical: &'a str,
        relative: Option<&'a str>,
        rooted: &'a [String],
        requested: &'a str,
        deny_rooted: &'a [String],
        zone: Zone,
        protected: &'a [Shipped],
    },
    /// A network host or an MCP `server/tool` name.
    Name(&'a str),
}

/// What a tool call accesses, which [`crate::tool::Tool::prepare`] resolves from its input once so
/// the gate checks and the run acts on the same values. The borrowing [`Self::as_target`] feeds the
/// allocation-free matcher. Owned because a resolved path is not a substring of the input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum Access {
    /// No extractable target, so only a `*` rule matches.
    #[default]
    None,
    Command(CommandTarget),
    /// A path in two spellings plus their forms relative to the anchor directories.
    Path {
        /// Where the call lands with every symlink followed. Allow rules, the zone, and the tool's
        /// run use this spelling alone.
        canonical: String,
        /// `canonical` as the file system names it, kept only when a name in it is not valid
        /// UTF-8, which `canonical` spells with replacement characters.
        #[serde(skip)]
        exact: Option<PathBuf>,
        /// `canonical` under the resolved cwd, set only while it sits inside it.
        relative: Option<String>,
        /// `canonical` under each resolved anchor directory that holds it.
        rooted: Vec<String>,
        /// The path as the call names it, lexically normalized. Deny and ask rules and the
        /// protected check also match it.
        requested: String,
        /// Either spelling relative to either spelling of each anchor directory, with names
        /// compared by [`fold_name`] so a case-variant prefix still counts. Only deny and ask rules
        /// match these.
        deny_rooted: Vec<String>,
        zone: Zone,
        /// The shipped groups whose protected names either spelling holds.
        protected: Vec<Shipped>,
    },
    /// A network host or an MCP `server/tool` name.
    Name(String),
}

impl Access {
    /// Parses a `bash` command, resolving the files it reads and writes in `workspace`.
    pub(crate) fn for_command(command: &str, workspace: &Workspace) -> Self {
        Self::Command(CommandTarget::parse(command, workspace))
    }

    /// Builds a path target for the location the tool will touch, resolving `path` against the
    /// workspace's cwd with `resolve`. A path `resolve` cannot place keeps its joined spelling with
    /// no rooted forms, so it never auto-allows. The tool follows native filesystem behavior.
    /// A resolved path that is not valid UTF-8 never auto-allows either, since the input a file
    /// tool opens can only pin its lossy spelling.
    pub(crate) fn for_path(path: &str, workspace: &Workspace) -> Self {
        let joined = workspace.cwd().join(path);
        let requested = lexical_normalize(&joined);
        let resolved = resolve(&joined).ok();
        let cwd = workspace.resolved_cwd();
        let relative = resolved.as_deref().and_then(|r| relative_to(r, cwd));
        let rooted = resolved.as_deref().map_or_else(Vec::new, |r| {
            workspace
                .anchors
                .iter()
                .filter_map(|anchor| relative_to(r, &anchor.resolved))
                .collect()
        });
        let mut deny_rooted: Vec<String> = [resolved.as_deref(), Some(&*requested)]
            .into_iter()
            .flatten()
            .flat_map(|path| workspace.loose_rooted(path))
            .collect();
        deny_rooted.sort();
        deny_rooted.dedup();
        let in_root = resolved.as_deref().is_some_and(|r| workspace.contains(r));
        let canonical = resolved.unwrap_or(joined);

        let lossy = canonical.to_str().is_none();
        let roots = protected_roots(cwd, workspace.roots());
        let protected = protected_by(&[&canonical, &requested], &roots);
        let zone = if in_root && !lossy && !is_hard_linked(&canonical) {
            Zone::Workspace
        } else {
            Zone::Outside
        };
        Self::Path {
            canonical: canonical.to_string_lossy().into_owned(),
            exact: lossy.then_some(canonical),
            relative,
            rooted,
            requested: requested.to_string_lossy().into_owned(),
            deny_rooted,
            zone,
            protected,
        }
    }

    /// Where a path target lands, exactly as the file system names it.
    pub(crate) fn resolved(&self) -> Option<PathBuf> {
        match self {
            Self::Path {
                canonical, exact, ..
            } => Some(exact.clone().unwrap_or_else(|| PathBuf::from(canonical))),
            Self::None | Self::Command(_) | Self::Name(_) => None,
        }
    }

    pub(crate) fn as_target(&self) -> Target<'_> {
        match self {
            Self::None => Target::None,
            Self::Command(command) => Target::Command(command),
            Self::Path {
                canonical,
                relative,
                rooted,
                requested,
                deny_rooted,
                zone,
                protected,
                ..
            } => Target::Path {
                canonical,
                relative: relative.as_deref(),
                rooted,
                requested,
                deny_rooted,
                zone: *zone,
                protected,
            },
            Self::Name(name) => Target::Name(name),
        }
    }
}

/// `path` relative to `base` when it sits under it. A relative base never matches, since
/// `strip_prefix("")` succeeds for every path.
fn relative_to(path: &Path, base: &Path) -> Option<String> {
    if !base.is_absolute() {
        return None;
    }
    path.strip_prefix(base)
        .ok()
        .map(|r| r.to_string_lossy().into_owned())
}

/// [`relative_to`] with names compared by [`fold_name`], keeping the remainder as `path` spells it.
fn loose_relative_to(path: &Path, base: &Path) -> Option<String> {
    if !base.is_absolute() {
        return None;
    }
    relative_to_folded(path, &folded_names(base))
}

/// [`loose_relative_to`] against a base already split into [`fold_name`]-folded names.
fn relative_to_folded(path: &Path, base: &[String]) -> Option<String> {
    let names: Vec<&OsStr> = names(path).collect();
    let rest = names.get(base.len()..)?;
    names
        .iter()
        .zip(base)
        .all(|(name, folded)| fold_name(&name.to_string_lossy()) == *folded)
        .then(|| {
            rest.iter()
                .collect::<PathBuf>()
                .to_string_lossy()
                .into_owned()
        })
}

/// Symlinks followed while resolving one path before it counts as a loop, Linux's `MAXSYMLINKS`.
const MAX_SYMLINK_HOPS: usize = 40;

/// Why [`resolve`] cannot place a path, naming the component where the walk stopped.
#[derive(Debug)]
enum Unreachable {
    /// The symlink whose hop exceeded [`MAX_SYMLINK_HOPS`].
    Loop(PathBuf),
    /// A component the walk could not step through, and why.
    Component { at: PathBuf, error: io::Error },
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Loop(link) => write!(f, "`{}` is in a symlink loop", link.display()),
            Self::Component { at, error } => match error.kind() {
                io::ErrorKind::NotADirectory => write!(f, "`{}` is not a directory", at.display()),
                io::ErrorKind::PermissionDenied => {
                    let dir = at.parent().unwrap_or(at);
                    write!(f, "cannot search `{}`", dir.display())
                }
                _ => write!(f, "cannot reach `{}`: {error}", at.display()),
            },
        }
    }
}

/// Resolves `path` to the location a tool will touch, walking it one component at a time the way
/// the kernel does. Every existing symlink is followed, a dangling one included since `open`
/// follows it too, and `..` steps back from wherever the walk stands. Past the first missing
/// component nothing exists yet, so the rest applies as `create_dir_all` builds it. It fails where
/// the kernel would: at a symlink loop, a directory it cannot search, or a file that more
/// components follow.
fn resolve(path: &Path) -> Result<PathBuf, Unreachable> {
    let mut absolute = std::path::absolute(path).map_err(|error| Unreachable::Component {
        at: path.to_path_buf(),
        error,
    })?;
    if requires_directory(path) {
        absolute.push("");
    }
    let mut resolved = PathBuf::new();
    let mut pending = Vec::new();
    enqueue(&absolute, &mut resolved, &mut pending);
    let mut hops = 0;

    while let Some(name) = pending.pop() {
        if name == "." {
            if !resolved.is_dir() {
                resolved.push("");
            }
            continue;
        }
        if name == ".." {
            _ = resolved.pop();
            continue;
        }
        resolved.push(&name);
        match std::fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(Unreachable::Loop(resolved));
                }
                let target =
                    std::fs::read_link(&resolved).map_err(|error| Unreachable::Component {
                        at: resolved.clone(),
                        error,
                    })?;
                _ = resolved.pop();
                enqueue(&target, &mut resolved, &mut pending);
            }
            Ok(meta) if !meta.is_dir() && !pending.is_empty() => {
                return Err(Unreachable::Component {
                    at: resolved,
                    error: io::ErrorKind::NotADirectory.into(),
                });
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(Unreachable::Component {
                    at: resolved,
                    error,
                });
            }
            _ => {}
        }
    }

    #[cfg(windows)]
    let resolved = {
        let directory = requires_directory(&resolved);
        let mut resolved = long_names(resolved, Path::canonicalize);
        if directory {
            resolved.push("");
        }
        resolved
    };
    Ok(resolved)
}

/// `path` with its deepest existing ancestor respelled as `canonicalize` names it and the rest
/// kept. Windows canonicalization spells 8.3 short names in full and drops NTFS stream suffixes, so
/// `GIT~1` and `.git::$INDEX_ALLOCATION` compare as `.git`.
#[cfg(windows)]
fn long_names(path: PathBuf, canonicalize: impl Fn(&Path) -> io::Result<PathBuf>) -> PathBuf {
    path.ancestors()
        .find_map(|existing| {
            let long = canonicalize(existing).ok()?;
            let mut spelled = respell(existing, &long);
            spelled.extend(path.strip_prefix(existing).ok()?);
            Some(spelled)
        })
        .unwrap_or(path)
}

/// `existing` with the names of `long`, its canonical form, under the root as written. [`resolve`]
/// followed every symlink in `existing`, so only the spelling differs, and the canonical `\\?\` or
/// UNC root would match neither cwd nor rules. When the name count differs, as under a drive
/// substituted for a directory, the canonical path stays whole.
#[cfg(windows)]
fn respell(existing: &Path, long: &Path) -> PathBuf {
    if names(existing).count() != names(long).count() {
        return dunce::simplified(long).to_path_buf();
    }
    existing
        .components()
        .take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
        .chain(
            long.components()
                .filter(|c| matches!(c, Component::Normal(_))),
        )
        .collect()
}

/// Pushes the components of `path` onto the `pending` stack so they pop in order, first restarting
/// `resolved` at the root when `path` is absolute. A final `.` preserves the directory constraint
/// that [`Path::components`] drops from trailing separators and `/.`.
fn enqueue(path: &Path, resolved: &mut PathBuf, pending: &mut Vec<OsString>) {
    if path.has_root() {
        *resolved = path
            .components()
            .take_while(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
            .collect();
    }
    if requires_directory(path) {
        pending.push(OsString::from("."));
    }
    pending.extend(path.components().rev().filter_map(|c| match c {
        Component::ParentDir => Some(OsString::from("..")),
        Component::Normal(name) => Some(name.to_owned()),
        Component::Prefix(_) | Component::RootDir | Component::CurDir => None,
    }));
}

fn requires_directory(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .rsplit(|byte| std::path::is_separator(char::from(*byte)))
        .next()
        .is_some_and(|last| last.is_empty() || last == b".")
}

fn folded_names(path: &Path) -> Vec<String> {
    names(path)
        .map(|name| fold_name(&name.to_string_lossy()))
        .collect()
}

fn names(path: &Path) -> impl Iterator<Item = &OsStr> {
    path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name),
        _ => None,
    })
}

/// Code points a file system may drop from a name, as HFS+ drops the one in `.g\u{200c}it`.
static DEFAULT_IGNORABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\p{Default_Ignorable_Code_Point}").expect("a Unicode property regex compiles")
});

/// One file name in the form protected names and deny rules compare by: default-ignorable code
/// points dropped, Unicode compatibility-caseless (D146: NFKD with full case folding), and trailing
/// dots and spaces trimmed as Windows does. A ligature, long-s, or `.git.` spelling then compares
/// equal to the ASCII name it opens as.
fn fold_name(name: &str) -> String {
    let folded: String = name
        .chars()
        .nfd()
        .default_case_fold()
        .nfkd()
        .default_case_fold()
        .nfkd()
        .collect();
    DEFAULT_IGNORABLE
        .replace_all(&folded, "")
        .trim_end_matches(['.', ' '])
        .to_owned()
}

/// Returns whether the resolved `dir` may be a workspace root, whose contents auto-allow edits.
/// `/`, `$HOME`, and the ancestors of `$HOME` may not, since their insides span every dotfile.
fn is_workspace(dir: &Path) -> bool {
    let home = std::env::home_dir().map(|home| resolve(&home).unwrap_or(home));
    dir.parent().is_some() && !home.is_some_and(|home| holds(dir, &home))
}

/// Returns whether `dir` is `home` or an ancestor of it, compared by identity as well as by
/// spelling. An alias that `resolve` keeps, such as macOS's case-insensitive `/users/<name>` or the
/// `/System/Volumes/Data` firmlink, reaches `home` through the same trailing names.
fn holds(dir: &Path, home: &Path) -> bool {
    home.starts_with(dir)
        || home.ancestors().any(|ancestor| {
            home.strip_prefix(ancestor)
                .is_ok_and(|rest| same_file::is_same_file(dir.join(rest), home).unwrap_or(false))
        })
}

/// Returns whether `path` is a regular file with more than one hard link, one of which may sit
/// outside the workspace.
fn is_hard_linked(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.nlink() > 1)
    }
    // std's link count is unstable on Windows, and a zero access mode is enough to query a handle.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
            && std::fs::OpenOptions::new()
                .access_mode(0)
                .open(path)
                .and_then(winapi_util::file::information)
                .is_ok_and(|info| info.number_of_links() > 1)
    }
}

// ── Decision ──

/// The pipeline's verdict for one call. `Ask` goes to the frontend, which answers it (the gate
/// itself stays UI-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Ask,
    Deny,
}

/// A call's [`Decision`] and what settled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) basis: Basis,
}

/// What settled a call's [`Decision`], which the gate logs, a denial names to the model, and an
/// approval prompt shows the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Basis {
    /// A configured pattern in the deny, ask, or allow tier.
    Rule { tier: Tier, rule: Rule },
    /// A session allow rule.
    Session(Rule),
    /// An enabled built-in rule.
    Builtin(Shipped),
    /// The mode: `plan` denying an edit, exec, or net call, or settling a call no rule does.
    Mode(Mode),
    /// A read-class call, which runs unless a deny or ask rule holds it.
    Read,
    /// An edit inside a workspace root.
    Workspace,
    /// A known-safe read, which runs without a rule.
    SafeRead,
    /// An allowed command, by what let each of its simple commands and its writes through.
    Command(Vec<Basis>),
}

impl Basis {
    /// The sentence an approval prompt shows for a call this basis asks about, or `None` when the
    /// mode asks because no rule settles the call.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Mode(_) => None,
            basis => Some(format!("Held by {basis}")),
        }
    }
}

impl fmt::Display for Basis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rule { tier, rule } => {
                write!(
                    f,
                    "the `{tier}.{}` pattern `{}`",
                    rule.class(),
                    rule.pattern()
                )
            }
            Self::Session(rule) => write!(
                f,
                "the session `allow.{}` pattern `{}`",
                rule.class(),
                rule.pattern()
            ),
            Self::Builtin(group) => write!(f, "the built-in `{group}` rule"),
            Self::Mode(mode) => write!(f, "`{mode}` mode"),
            Self::Read => f.write_str("the default for reads"),
            Self::Workspace => f.write_str("the default for edits inside the workspace"),
            Self::SafeRead => f.write_str("a known-safe read"),
            Self::Command(parts) => {
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{part}")?;
                }
                Ok(())
            }
        }
    }
}

// ── Policy ──

/// Resolved permission policy: the active mode plus the parsed rule tiers. Built once from config
/// and consulted by [`Self::decide`] on every gated call.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    mode: Mode,
    /// User, project, and enabled shipped deny rules.
    deny: Vec<TierRule>,
    /// User, project, and enabled shipped ask rules.
    ask: Vec<TierRule>,
    allow: Vec<Rule>,
    /// Rules the user granted from an approval for the rest of the session. In memory only.
    session_allow: Vec<Rule>,
    /// The shipped groups `[permission.defaults]` leaves on.
    enabled: Vec<Shipped>,
}

/// A deny or ask rule, with the shipped group it comes from.
#[derive(Debug, Clone)]
struct TierRule {
    rule: Rule,
    builtin: Option<Shipped>,
}

impl TierRule {
    const fn configured(rule: Rule) -> Self {
        Self {
            rule,
            builtin: None,
        }
    }

    /// The basis of a decision this rule settles from `tier`.
    fn basis(&self, tier: Tier) -> Basis {
        match self.builtin {
            Some(group) => Basis::Builtin(group),
            None => Basis::Rule {
                tier,
                rule: self.rule.clone(),
            },
        }
    }
}

/// A manual-mode policy with every shipped group on but none of their patterns.
#[cfg(any(test, feature = "testing"))]
impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            deny: Vec::new(),
            ask: Vec::new(),
            allow: Vec::new(),
            session_allow: Vec::new(),
            enabled: Shipped::ALL.to_vec(),
        }
    }
}

impl Policy {
    /// Builds a policy from parsed rule tiers, with every shipped group on but no shipped patterns.
    #[cfg(test)]
    pub(crate) fn new(mode: Mode, deny: Vec<Rule>, ask: Vec<Rule>, allow: Vec<Rule>) -> Self {
        Self {
            mode,
            deny: deny.into_iter().map(TierRule::configured).collect(),
            ask: ask.into_iter().map(TierRule::configured).collect(),
            allow,
            ..Self::default()
        }
    }

    /// Builds a policy from a resolved mode and the parsed rule tiers, plus the shipped defaults
    /// that `disabled` leaves on. Deny and ask rules also cover where their literal prefix
    /// resolves, an anchored one from each anchor directory of `workspace`.
    pub(crate) fn resolve(
        mode: Mode,
        deny: Vec<Rule>,
        ask: Vec<Rule>,
        allow: Vec<Rule>,
        disabled: &[Shipped],
        workspace: &Workspace,
    ) -> Result<Self> {
        let enabled = || {
            Shipped::ALL
                .into_iter()
                .filter(|group| !disabled.contains(group))
        };
        let mut deny: Vec<TierRule> = deny.into_iter().map(TierRule::configured).collect();
        let mut ask: Vec<TierRule> = ask.into_iter().map(TierRule::configured).collect();
        for group in enabled() {
            if let Some(rules) = group.rules()? {
                let shipped = rules.into_iter().map(|rule| TierRule {
                    rule,
                    builtin: Some(group),
                });
                match group.tier() {
                    ShippedTier::Deny => deny.extend(shipped),
                    ShippedTier::Ask => ask.extend(shipped),
                }
            }
        }
        let anchors: Vec<PathBuf> = workspace
            .anchors
            .iter()
            .map(|anchor| anchor.spelled.clone())
            .collect();
        let with_prefixes = |rules: Vec<TierRule>| {
            rules
                .into_iter()
                .map(|entry| {
                    Ok(TierRule {
                        rule: entry.rule.with_resolved_prefixes(&anchors)?,
                        builtin: entry.builtin,
                    })
                })
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            mode,
            deny: with_prefixes(deny)?,
            ask: with_prefixes(ask)?,
            allow,
            session_allow: Vec::new(),
            enabled: enabled().collect(),
        })
    }

    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) const fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub(crate) fn session_allow(&self) -> &[Rule] {
        &self.session_allow
    }

    /// Adds a session allow rule, which [`Self::decide`] weighs like a configured allow rule.
    pub(crate) fn allow_for_session(&mut self, rule: Rule) {
        if !self.session_allow.contains(&rule) {
            self.session_allow.push(rule);
        }
    }

    pub(crate) fn clear_session_allow(&mut self) {
        self.session_allow.clear();
    }

    /// The "allow for this session" offer for a call that asks: the narrowest rule naming it, when
    /// a session allow rule could let the call through at all. `None` when a protected name or an
    /// ask rule holds the call, since both outrank every allow rule.
    pub(crate) fn session_grant(
        &self,
        class: PermissionClass,
        target: &Access,
    ) -> Option<SessionGrant> {
        let suggested = Rule::for_call(class, &target.as_target())?;
        let mut trial = self.clone();
        trial.session_allow.push(suggested.clone());
        (trial.decide(class, &target.as_target()) == Decision::Allow).then(|| SessionGrant {
            class,
            target: target.clone(),
            suggested,
        })
    }

    /// The `read` deny and ask rules for files below a search tool's root.
    pub(crate) fn read_filter(&self) -> ReadFilter {
        ReadFilter(
            self.deny
                .iter()
                .chain(&self.ask)
                .map(|entry| &entry.rule)
                .filter(|rule| rule.class() == PermissionClass::Read)
                .cloned()
                .collect(),
        )
    }

    /// Resolves a call to a [`Decision`], as [`Self::verdict`] does.
    pub(crate) fn decide(&self, class: PermissionClass, target: &Target<'_>) -> Decision {
        self.verdict(class, target).decision
    }

    /// Resolves a call to a [`Verdict`] without I/O.
    ///
    /// Order, stopping at the first match:
    /// 1. A deny rule denies, in every mode.
    /// 2. `plan` denies an edit, exec, or net call.
    /// 3. An ask rule or an enabled protected name asks, in every mode.
    /// 4. A read-class call allows.
    /// 5. An edit-class call inside a workspace root allows.
    /// 6. An allow rule or a session allow rule allows.
    /// 7. `yolo` allows, and the other modes ask.
    ///
    /// A `bash` command is denied when a deny rule matches one of its simple commands, an edit of a
    /// file it writes, or a read of a file it redirects into its input. It asks at step 3 in each
    /// case [`Self::asks`] lists, and step 6 allows it only when it is not opaque and
    /// [`Self::allows_command`] holds.
    pub(crate) fn verdict(&self, class: PermissionClass, target: &Target<'_>) -> Verdict {
        let verdict = |decision, basis| Verdict { decision, basis };
        if let Some(rule) = held_by(&self.deny, class, target) {
            return verdict(Decision::Deny, rule.basis(Tier::Deny));
        }

        if self.mode == Mode::Plan
            && matches!(
                class,
                PermissionClass::Edit | PermissionClass::Exec | PermissionClass::Net
            )
        {
            return verdict(Decision::Deny, Basis::Mode(Mode::Plan));
        }

        if let Some(basis) = self.asks(class, target) {
            return verdict(Decision::Ask, basis);
        }

        if class == PermissionClass::Read {
            return verdict(Decision::Allow, Basis::Read);
        }
        if class == PermissionClass::Edit
            && matches!(
                target,
                Target::Path {
                    zone: Zone::Workspace,
                    ..
                }
            )
        {
            return verdict(Decision::Allow, Basis::Workspace);
        }
        if let Some(basis) = self.allows(class, target) {
            return verdict(Decision::Allow, basis);
        }

        let fallback = if self.mode == Mode::Yolo {
            Decision::Allow
        } else {
            Decision::Ask
        };
        verdict(fallback, Basis::Mode(self.mode))
    }

    /// What holds the call in the ask tier: an ask rule, the protected group of a path the call
    /// edits or a command writes, the [`read` rule](Self::read_holder) for a file a reader opens,
    /// or the `rm` group for a [forced recursive delete](Segment::removes_recursively_by_force).
    fn asks(&self, class: PermissionClass, target: &Target<'_>) -> Option<Basis> {
        if let Some(rule) = held_by(&self.ask, class, target) {
            return Some(rule.basis(Tier::Ask));
        }
        if class == PermissionClass::Edit
            && let Some(group) = self.protected_group(target)
        {
            return Some(Basis::Builtin(group));
        }
        let Target::Command(command) = target else {
            return None;
        };
        command
            .writes()
            .iter()
            .find_map(|write| self.protected_group(&write.as_target()))
            .map(Basis::Builtin)
            .or_else(|| {
                command
                    .segments()
                    .iter()
                    .find_map(|segment| self.read_holder(segment))
            })
            .or_else(|| {
                (self.enabled.contains(&Shipped::Rm)
                    && command
                        .segments()
                        .iter()
                        .any(Segment::removes_recursively_by_force))
                .then_some(Basis::Builtin(Shipped::Rm))
            })
    }

    /// The deny or ask `read` rule that holds a file `segment` may read, or the first `read` rule
    /// of either tier when it may read any file.
    fn read_holder(&self, segment: &Segment) -> Option<Basis> {
        let tiers = || {
            self.deny
                .iter()
                .map(|rule| (rule, Tier::Deny))
                .chain(self.ask.iter().map(|rule| (rule, Tier::Ask)))
        };
        segment
            .reads()
            .iter()
            .find_map(|read| {
                let read = read.as_target();
                tiers()
                    .find(|(rule, _)| {
                        rule.rule
                            .matches(PermissionClass::Read, &read, MatchDiscipline::Deny)
                    })
                    .map(|(rule, tier)| rule.basis(tier))
            })
            .or_else(|| {
                segment
                    .reads_anything()
                    .then(|| {
                        tiers()
                            .find(|(rule, _)| rule.rule.class() == PermissionClass::Read)
                            .map(|(rule, tier)| rule.basis(tier))
                    })
                    .flatten()
            })
    }

    /// The enabled protected group that holds `target`, if it is a path one does.
    fn protected_group(&self, target: &Target<'_>) -> Option<Shipped> {
        match target {
            Target::Path { protected, .. } => protected
                .iter()
                .copied()
                .find(|group| self.enabled.contains(group)),
            _ => None,
        }
    }

    /// The allow rule or session allow rule that matches the call, or for a command what lets
    /// each of its parts through.
    fn allows(&self, class: PermissionClass, target: &Target<'_>) -> Option<Basis> {
        match target {
            Target::Command(command) => self.allows_command(command),
            _ => self.allow_rule(|rule| rule.matches(class, target, MatchDiscipline::Allow)),
        }
    }

    /// What lets every part of a command through, when each does: it is not opaque and runs at
    /// least one simple command, each simple command is allowed by a rule or is a known-safe read,
    /// and each file it writes is inside a workspace root or allowed for editing.
    fn allows_command(&self, command: &CommandTarget) -> Option<Basis> {
        if command.is_opaque() || command.segments().is_empty() {
            return None;
        }
        let segments = command.segments().iter().map(|segment| {
            self.allow_rule(|rule| rule.allows(segment))
                .or_else(|| segment.safe_reads().map(|_| Basis::SafeRead))
        });
        let writes = command.writes().iter().map(|write| {
            let write = write.as_target();
            if matches!(
                write,
                Target::Path {
                    zone: Zone::Workspace,
                    ..
                }
            ) {
                Some(Basis::Workspace)
            } else {
                self.allow_rule(|rule| {
                    rule.matches(PermissionClass::Edit, &write, MatchDiscipline::Allow)
                })
            }
        });
        segments
            .chain(writes)
            .collect::<Option<Vec<_>>>()
            .map(Basis::Command)
    }

    /// The first allow rule, then the first session allow rule, that `accepts`.
    fn allow_rule(&self, accepts: impl Fn(&Rule) -> bool) -> Option<Basis> {
        self.allow
            .iter()
            .find(|rule| accepts(rule))
            .map(|rule| Basis::Rule {
                tier: Tier::Allow,
                rule: rule.clone(),
            })
            .or_else(|| {
                self.session_allow
                    .iter()
                    .find(|rule| accepts(rule))
                    .map(|rule| Basis::Session(rule.clone()))
            })
    }
}

/// The first of the deny or ask `rules` that matches the call, or, for a command, an edit of a
/// file it writes or a read of a file it redirects into its input.
fn held_by<'a>(
    rules: &'a [TierRule],
    class: PermissionClass,
    target: &Target<'_>,
) -> Option<&'a TierRule> {
    rules
        .iter()
        .find(|entry| entry.rule.matches(class, target, MatchDiscipline::Deny))
        .or_else(|| {
            let Target::Command(command) = target else {
                return None;
            };
            command
                .writes()
                .iter()
                .find_map(|write| held_by(rules, PermissionClass::Edit, &write.as_target()))
                .or_else(|| {
                    command
                        .reads()
                        .iter()
                        .find_map(|read| held_by(rules, PermissionClass::Read, &read.as_target()))
                })
        })
}

/// An approval's "allow for this session" offer: the suggested rule, plus the call that any edit
/// of it must keep covering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGrant {
    class: PermissionClass,
    target: Access,
    suggested: Rule,
}

impl SessionGrant {
    pub fn suggested(&self) -> &Rule {
        &self.suggested
    }

    /// Parses the user's edit of the suggested pattern in the call's class and
    /// [admits](Self::admit) it.
    pub fn accept(&self, pattern: &str) -> Result<Rule> {
        self.admit(Rule::parse(self.class, pattern)?)
    }

    /// Checks that `rule` stays in the call's class and still allows the call.
    pub(crate) fn admit(&self, rule: Rule) -> Result<Rule> {
        ensure!(
            rule.class() == self.class,
            "the rule must stay an `{}` rule",
            self.class
        );
        ensure!(
            rule.matches(self.class, &self.target.as_target(), MatchDiscipline::Allow),
            "the pattern no longer covers this call"
        );
        Ok(rule)
    }
}

/// The `read` deny and ask rules a search tool applies to each file it finds, since the gate
/// checked only the search root and a walk cannot ask about each file. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadFilter(Arc<[Rule]>);

impl ReadFilter {
    /// The filter for one walk of `base`, the resolved search root, which the call spelled as
    /// `requested` relative to `workspace`'s cwd.
    pub(crate) fn walk(
        &self,
        base: &Path,
        requested: &Path,
        workspace: &Workspace,
    ) -> WalkFilter<'_> {
        let requested = lexical_normalize(&workspace.cwd().join(requested));
        WalkFilter {
            rules: &self.0,
            base: base.to_path_buf(),
            requested_base: (requested != base).then_some(requested),
            folded_anchors: workspace
                .anchors
                .iter()
                .flat_map(|anchor| [&anchor.resolved, &anchor.spelled])
                .filter(|base| base.is_absolute())
                .map(|base| folded_names(base))
                .collect(),
        }
    }
}

/// [`ReadFilter`] bound to one walk, with the spellings every file of the walk shares.
pub(crate) struct WalkFilter<'a> {
    rules: &'a [Rule],
    base: PathBuf,
    /// The base as the call spelled it, when that differs from `base`.
    requested_base: Option<PathBuf>,
    /// Each spelling of each anchor directory, split into [`fold_name`]-folded names.
    folded_anchors: Vec<Vec<String>>,
}

impl WalkFilter<'_> {
    /// Returns whether a rule withholds `path`, a file found below the walk's base. The walk
    /// follows no symlink, so `path` is where the file resolves. The same names below the requested
    /// base spell it the way the call does.
    pub(crate) fn withholds(&self, path: &Path) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let requested = self
            .requested_base
            .as_ref()
            .and_then(|base| Some(base.join(path.strip_prefix(&self.base).ok()?)));
        let mut deny_rooted: Vec<String> = [Some(path), requested.as_deref()]
            .into_iter()
            .flatten()
            .flat_map(|spelling| {
                self.folded_anchors
                    .iter()
                    .filter_map(move |anchor| relative_to_folded(spelling, anchor))
            })
            .collect();
        deny_rooted.sort();
        deny_rooted.dedup();

        let canonical = path.to_string_lossy();
        let requested = requested.as_deref().map(Path::to_string_lossy);
        // Deny matching tries every `deny_rooted` spelling, which covers `rooted`.
        let target = Target::Path {
            canonical: &canonical,
            relative: None,
            rooted: &[],
            requested: requested.as_deref().unwrap_or(&canonical),
            deny_rooted: &deny_rooted,
            zone: Zone::Outside,
            protected: &[],
        };
        self.rules
            .iter()
            .any(|rule| rule.matches(PermissionClass::Read, &target, MatchDiscipline::Deny))
    }
}

// ── Testing ──

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::path::Path;

    use super::{Access, PermissionClass, Policy, SessionGrant, Workspace};

    /// The session grant the default policy offers for running `command` in `/repo`.
    pub fn exec_grant(command: &str) -> SessionGrant {
        Policy::default()
            .session_grant(
                PermissionClass::Exec,
                &Access::for_command(command, &Workspace::new(Path::new("/repo"))),
            )
            .expect("the default policy offers a grant for the command")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::local_testing::sample_header;
    use crate::session::store::{SessionStore, artifact_dir};
    use crate::util::fs::testing::{symlink_dir, symlink_file};
    use crate::util::path::local_testing::abs;
    use crate::util::path::slash_separated;
    use crate::util::path::testing::HOME_VAR;

    const READ: PermissionClass = PermissionClass::Read;
    const EDIT: PermissionClass = PermissionClass::Edit;
    const EXEC: PermissionClass = PermissionClass::Exec;

    fn rules(entries: &[(PermissionClass, &str)]) -> Vec<Rule> {
        entries
            .iter()
            .map(|(class, pattern)| Rule::parse(*class, pattern).unwrap())
            .collect()
    }

    /// A policy with every shipped group on and the given tiers, but none of the shipped patterns.
    fn policy(
        mode: Mode,
        deny: &[(PermissionClass, &str)],
        ask: &[(PermissionClass, &str)],
        allow: &[(PermissionClass, &str)],
    ) -> Policy {
        Policy::new(mode, rules(deny), rules(ask), rules(allow))
    }

    /// [`Policy::resolve`] in `cwd`, with every shipped default on.
    fn resolved_policy(
        mode: Mode,
        deny: &[(PermissionClass, &str)],
        ask: &[(PermissionClass, &str)],
        allow: &[(PermissionClass, &str)],
        cwd: &Path,
    ) -> Policy {
        Policy::resolve(
            mode,
            rules(deny),
            rules(ask),
            rules(allow),
            &[],
            &Workspace::new(cwd),
        )
        .unwrap()
    }

    fn command(s: &str) -> Access {
        Access::for_command(s, &Workspace::new(Path::new("/repo")))
    }

    /// A path target at `canonical`, spelled the same way, rooted at `relative` when given.
    fn path(canonical: &str, relative: Option<&str>, zone: Zone, protected: &[Shipped]) -> Access {
        let rooted: Vec<String> = relative.map(str::to_owned).into_iter().collect();
        Access::Path {
            canonical: canonical.to_owned(),
            exact: None,
            relative: relative.map(str::to_owned),
            rooted: rooted.clone(),
            requested: canonical.to_owned(),
            deny_rooted: rooted,
            zone,
            protected: protected.to_vec(),
        }
    }

    fn workspace(canonical: &str, relative: &str) -> Access {
        path(canonical, Some(relative), Zone::Workspace, &[])
    }

    fn outside(canonical: &str) -> Access {
        path(canonical, None, Zone::Outside, &[])
    }

    fn decide(policy: &Policy, class: PermissionClass, target: &Access) -> Decision {
        policy.decide(class, &target.as_target())
    }

    /// A canonicalized tempdir to use as cwd, so assertions compare resolved paths (macOS puts
    /// tempdirs behind the `/var` → `/private/var` symlink).
    fn temp_cwd() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        (dir, cwd)
    }

    /// The canonical and relative spellings as paths, which compare equal across separators.
    fn resolved(target: &Access) -> (&Path, Option<&Path>, Zone) {
        let Access::Path {
            canonical,
            relative,
            zone,
            ..
        } = target
        else {
            panic!("expected a path target, got {target:?}");
        };
        (
            Path::new(canonical),
            relative.as_deref().map(Path::new),
            *zone,
        )
    }

    fn protected(target: &Access) -> &[Shipped] {
        let Access::Path { protected, .. } = target else {
            panic!("expected a path target, got {target:?}");
        };
        protected
    }

    fn rule_basis(tier: Tier, class: PermissionClass, pattern: &str) -> Basis {
        Basis::Rule {
            tier,
            rule: Rule::parse(class, pattern).unwrap(),
        }
    }

    mod mode {
        use super::*;

        mod from_str {
            use super::*;

            #[test]
            fn parses_every_valid_value_and_names_them_for_any_other() {
                for mode in [Mode::Manual, Mode::Plan, Mode::Yolo] {
                    assert_eq!(mode.as_str().parse::<Mode>().unwrap(), mode);
                }
                let err = "bypass".parse::<Mode>().unwrap_err().to_string();
                assert_eq!(err, "expected one of manual, plan, yolo");
            }
        }
    }

    mod workspace {
        use super::*;

        mod resolve {
            use super::*;

            #[test]
            fn adds_each_extra_root_once_after_the_cwd() {
                let (_dir, base) = temp_cwd();
                let (repo, shared, home) =
                    (base.join("repo"), base.join("shared"), base.join("home"));
                for dir in [&repo, &shared, &home] {
                    std::fs::create_dir(dir).unwrap();
                }
                let extra = [
                    slash_separated(&shared),
                    format!("{}/../shared/", slash_separated(&shared)),
                    "~/notes".to_owned(),
                ];

                let workspace = temp_env::with_var(HOME_VAR, Some(&home), || {
                    Workspace::resolve(&repo, &extra).unwrap()
                });

                assert_eq!(workspace.cwd(), repo);
                let anchor = |dir: &Path| Anchor {
                    resolved: dir.to_path_buf(),
                    spelled: dir.to_path_buf(),
                };
                assert_eq!(
                    workspace.anchors,
                    [anchor(&repo), anchor(&shared), anchor(&home.join("notes"))]
                );
                assert_eq!(workspace.roots(), [repo, shared, home.join("notes")]);
            }

            #[test]
            fn rejects_a_relative_or_too_broad_extra_root() {
                let (_dir, base) = temp_cwd();
                let (repo, home) = (base.join("repo"), base.join("home"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&home).unwrap();
                let too_broad = |raw: &str| {
                    format!(
                        "invalid extra root {raw:?}: \
                         `/`, the home directory, and their ancestors cannot be workspace roots"
                    )
                };
                let fs_root = slash_separated(base.ancestors().last().unwrap());
                let base_raw = slash_separated(&base);

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for (raw, expected) in [
                        (
                            "shared",
                            r#"invalid extra root "shared": expected an absolute or `~/` path"#
                                .to_owned(),
                        ),
                        ("~", too_broad("~")),
                        (fs_root.as_str(), too_broad(&fs_root)),
                        (base_raw.as_str(), too_broad(&base_raw)),
                    ] {
                        let err = Workspace::resolve(&repo, &[raw.to_owned()]).unwrap_err();
                        assert_eq!(err.to_string(), expected, "{raw}");
                    }
                });
            }

            #[test]
            fn rejects_an_extra_root_through_a_symlink_loop() {
                let (_dir, base) = temp_cwd();
                let looped = base.join("loop");
                symlink_file(&looped, &looped);
                let raw = format!("{}/inner", looped.display());

                let err = Workspace::resolve(&base, std::slice::from_ref(&raw)).unwrap_err();

                assert_eq!(
                    err.to_string(),
                    format!(
                        "invalid extra root {raw:?}: `{}` is in a symlink loop",
                        looped.display()
                    ),
                );
            }

            #[test]
            fn rejects_an_extra_root_under_a_file() {
                let (_dir, base) = temp_cwd();
                let notes = base.join("notes");
                std::fs::write(&notes, "").unwrap();
                let raw = notes.join("inner").display().to_string();

                let err = Workspace::resolve(&base, std::slice::from_ref(&raw)).unwrap_err();

                assert_eq!(
                    err.to_string(),
                    format!(
                        "invalid extra root {raw:?}: `{}` is not a directory",
                        notes.display()
                    ),
                );
            }

            #[test]
            fn has_no_roots_when_launched_from_home() {
                // A launch from `$HOME` drops every root, so a configured extra root cannot
                // auto-allow edits outside any project. Each extra root must still be valid.
                let (_dir, base) = temp_cwd();
                let (home, shared) = (base.join("home"), base.join("shared"));
                std::fs::create_dir(&home).unwrap();
                std::fs::create_dir(&shared).unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::resolve(&home, &[slash_separated(&shared)]).unwrap();
                    assert_eq!(workspace.roots(), [] as [PathBuf; 0]);
                    assert_eq!(workspace.anchors.len(), 1, "the cwd still anchors patterns");
                    let target =
                        Access::for_path(shared.join("a.rs").to_str().unwrap(), &workspace);
                    assert_eq!(resolved(&target).2, Zone::Outside);

                    let err = Workspace::resolve(&home, &["shared".to_owned()]).unwrap_err();
                    assert_eq!(
                        err.to_string(),
                        r#"invalid extra root "shared": expected an absolute or `~/` path"#,
                    );
                });
            }

            #[test]
            fn rejects_home_and_its_parent_under_another_name() {
                // A symlink to `$HOME` resolves to it. An alias `resolve` keeps, such as `up` here,
                // still reaches `$HOME` through the trailing names, so it is caught too.
                let (_dir, base) = temp_cwd();
                let (repo, home) = (base.join("repo"), base.join("home"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&home).unwrap();
                symlink_dir(&home, &base.join("home-link"));
                symlink_dir(&base, &base.join("up"));
                let link = slash_separated(&base.join("home-link"));

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let err = Workspace::resolve(&repo, std::slice::from_ref(&link)).unwrap_err();
                    assert_eq!(
                        err.to_string(),
                        format!(
                            "invalid extra root {link:?}: \
                             `/`, the home directory, and their ancestors cannot be workspace roots"
                        ),
                    );
                    assert!(!is_workspace(&base.join("up")));
                    assert!(!is_workspace(&base.join("up/home")));
                    assert!(is_workspace(&base.join("up/repo")));
                });
            }
        }
    }

    mod access {
        use super::*;

        mod for_path {
            use super::*;

            #[test]
            fn inside_an_extra_root_is_a_workspace_target_rooted_there() {
                let (_dir, base) = temp_cwd();
                let (repo, shared) = (base.join("repo"), base.join("shared"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&shared).unwrap();
                let workspace = Workspace::resolve(&repo, &[slash_separated(&shared)]).unwrap();

                for (raw, zone, groups, rooted) in [
                    ("../shared/a.rs", Zone::Workspace, &[][..], &["a.rs"][..]),
                    (
                        "../shared/.git/config",
                        Zone::Workspace,
                        &[Shipped::Git],
                        &[".git/config"],
                    ),
                    ("../elsewhere/a.rs", Zone::Outside, &[], &[]),
                ] {
                    let target = Access::for_path(raw, &workspace);
                    let (canonical, relative, actual) = resolved(&target);
                    assert_eq!(canonical, lexical_normalize(&repo.join(raw)), "{raw}");
                    assert_eq!(relative, None, "{raw}: outside the cwd");
                    assert_eq!(actual, zone, "{raw}");
                    assert_eq!(protected(&target), groups, "{raw}");
                    let Access::Path { rooted: actual, .. } = &target else {
                        unreachable!();
                    };
                    let actual: Vec<&Path> = actual.iter().map(Path::new).collect();
                    let rooted: Vec<&Path> = rooted.iter().map(Path::new).collect();
                    assert_eq!(actual, rooted, "{raw}");
                }
            }

            #[test]
            fn inside_cwd_is_a_workspace_target() {
                let (_dir, cwd) = temp_cwd();
                std::fs::write(cwd.join("a.rs"), "x").unwrap();

                let target = Access::for_path("a.rs", &Workspace::new(&cwd));
                let (canonical, relative, zone) = resolved(&target);
                assert_eq!(canonical, cwd.join("a.rs"));
                assert_eq!(relative, Some(Path::new("a.rs")));
                assert_eq!(zone, Zone::Workspace);
                assert_eq!(protected(&target), []);
            }

            #[test]
            fn brand_new_file_is_still_a_workspace_target() {
                // A new file cannot canonicalize, but its missing tail still lands inside cwd.
                let (_dir, cwd) = temp_cwd();
                let target = Access::for_path("new/file.rs", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (
                        cwd.join("new/file.rs").as_path(),
                        Some(Path::new("new/file.rs")),
                        Zone::Workspace,
                    ),
                );
            }

            #[test]
            fn outside_cwd_has_no_relative_component() {
                let (_dir, cwd) = temp_cwd();
                let target = Access::for_path("/etc/hosts", &Workspace::new(&cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!((relative, zone), (None, Zone::Outside));
            }

            #[test]
            fn parent_traversal_is_resolved_before_the_cwd_test() {
                // Both shapes must leave cwd. In the second, `create_dir_all` builds `missing` and
                // then steps back out through it.
                let (_dir, root) = temp_cwd();
                let cwd = root.join("repo");
                std::fs::create_dir(&cwd).unwrap();

                for raw in ["../escape.rs", "missing/../../escape.rs"] {
                    let target = Access::for_path(raw, &Workspace::new(&cwd));
                    assert_eq!(
                        resolved(&target),
                        (root.join("escape.rs").as_path(), None, Zone::Outside),
                        "{raw}",
                    );
                }
            }

            #[test]
            fn records_the_group_of_a_protected_name_inside_cwd() {
                let (_dir, cwd) = temp_cwd();
                let target = Access::for_path(".vscode/tasks.json", &Workspace::new(&cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!(relative, Some(Path::new(".vscode/tasks.json")));
                assert_eq!(zone, Zone::Workspace);
                assert_eq!(protected(&target), [Shipped::Editors]);
            }

            #[test]
            fn app_dirs_belong_to_the_ox_group() {
                let (_dir, root) = temp_cwd();
                temp_env::with_vars(
                    [
                        ("XDG_CONFIG_HOME", Some(root.join("config"))),
                        ("XDG_DATA_HOME", Some(root.join("data"))),
                        ("XDG_STATE_HOME", Some(root.join("state"))),
                    ],
                    || {
                        for raw in [
                            "config/ox/config.toml",
                            "data/ox/sessions/s.jsonl",
                            "state/ox/log/oxide-code.log",
                        ] {
                            let target = Access::for_path(raw, &Workspace::new(&root));
                            assert_eq!(protected(&target), [Shipped::Ox], "{raw}");
                        }
                        let sibling =
                            Access::for_path("config/other/x.toml", &Workspace::new(&root));
                        assert_eq!(protected(&sibling), []);
                    },
                );
            }

            #[test]
            fn cwd_containing_home_is_not_a_workspace() {
                // Launched from `$HOME` or any ancestor of it, "inside cwd" spans every dotfile, so
                // edits there must not auto-allow. Anchored patterns still see the path.
                let (_dir, root) = temp_cwd();
                let home = root.join("home");
                std::fs::create_dir(&home).unwrap();
                let fs_root = root.ancestors().last().unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for cwd in [home.as_path(), root.as_path(), fs_root] {
                        let target = Access::for_path("notes.md", &Workspace::new(cwd));
                        let (_, relative, zone) = resolved(&target);
                        assert_eq!(relative, Some(Path::new("notes.md")), "{cwd:?}");
                        assert_eq!(zone, Zone::Outside, "{cwd:?}");
                    }

                    let project = home.join("project");
                    std::fs::create_dir(&project).unwrap();
                    let target = Access::for_path("notes.md", &Workspace::new(&project));
                    assert_eq!(resolved(&target).2, Zone::Workspace);
                });
            }

            #[test]
            fn with_a_non_absolute_cwd_is_not_a_workspace_target() {
                // An empty cwd has no root, and `strip_prefix("")` succeeds for any path, so the
                // target must get no cwd-relative form either.
                let target = Access::for_path("a.rs", &Workspace::new(Path::new("")));
                let (_, relative, zone) = resolved(&target);
                assert_eq!((relative, zone), (None, Zone::Outside));
            }

            #[test]
            fn relative_cwd_parent_traversal_preserves_permission_decisions() {
                // With `$HOME` at `../..`, the spellings cover a workspace and a non-workspace cwd.
                let policy = policy(Mode::Manual, &[], &[], &[(EDIT, "src/**")]);
                let absolute_cwd = std::env::current_dir().unwrap();
                let home = dunce::canonicalize(absolute_cwd.join("../..")).unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for cwd in ["..", "../..", "missing/../.."] {
                        for raw in ["notes.md", ".git/config", "src/a.rs"] {
                            let target = Access::for_path(raw, &Workspace::new(Path::new(cwd)));
                            let absolute =
                                Access::for_path(raw, &Workspace::new(&absolute_cwd.join(cwd)));
                            let (_, relative, zone) = resolved(&target);
                            let (_, absolute_relative, absolute_zone) = resolved(&absolute);
                            assert_eq!(
                                (relative, zone),
                                (absolute_relative, absolute_zone),
                                "{cwd:?}/{raw}",
                            );
                            assert_eq!(
                                decide(&policy, EDIT, &target),
                                decide(&policy, EDIT, &absolute),
                                "{cwd:?}/{raw}",
                            );
                        }
                    }
                });
            }

            #[test]
            fn deep_existing_file_stays_a_workspace_target() {
                // Windows canonicalizes a path past 260 chars to a `\\?\` spelling the cwd lacks.
                let (_dir, cwd) = temp_cwd();
                let deep: PathBuf = std::iter::repeat_n("d".repeat(50), 6).collect();
                std::fs::create_dir_all(cwd.join(&deep)).unwrap();
                let file = deep.join("a.rs");
                std::fs::write(cwd.join(&file), "").unwrap();

                assert_eq!(
                    resolved(&Access::for_path(
                        file.to_str().unwrap(),
                        &Workspace::new(&cwd)
                    )),
                    (&*cwd.join(&file), Some(&*file), Zone::Workspace),
                );
            }

            #[test]
            fn hard_linked_file_is_not_a_workspace_target() {
                // The second link sits outside cwd, so an edit through `a.rs` would change it too.
                let (_dir, cwd) = temp_cwd();
                let (_outside_dir, outside) = temp_cwd();
                std::fs::write(outside.join("a.rs"), "x").unwrap();
                std::fs::hard_link(outside.join("a.rs"), cwd.join("a.rs")).unwrap();
                std::fs::write(cwd.join("b.rs"), "x").unwrap();

                assert_eq!(
                    resolved(&Access::for_path("a.rs", &Workspace::new(&cwd))),
                    (
                        cwd.join("a.rs").as_path(),
                        Some(Path::new("a.rs")),
                        Zone::Outside
                    ),
                );
                assert_eq!(
                    resolved(&Access::for_path("b.rs", &Workspace::new(&cwd))).2,
                    Zone::Workspace
                );
            }

            #[test]
            fn new_file_under_a_symlinked_parent_resolves_through_the_link() {
                let (_dir, cwd) = temp_cwd();
                let (_outside_dir, outside) = temp_cwd();
                symlink_dir(&outside, &cwd.join("link"));

                let target = Access::for_path("link/new.rs", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("new.rs").as_path(), None, Zone::Outside),
                );
            }

            #[test]
            fn dangling_symlink_resolves_to_its_missing_target() {
                // `canonicalize` fails on the dangling `notes.md -> ../escape.txt`, yet `fs::write`
                // follows it, so the gate must follow it too.
                let (_dir, root) = temp_cwd();
                let cwd = root.join("repo");
                std::fs::create_dir(&cwd).unwrap();
                symlink_file(Path::new("../escape.txt"), &cwd.join("notes.md"));

                let target = Access::for_path("notes.md", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (root.join("escape.txt").as_path(), None, Zone::Outside),
                );
            }

            #[test]
            fn dangling_symlink_chain_through_a_directory_is_followed() {
                // A dangling link mid-path (`out/new.rs` with `out -> ../gone/dir`) and a second
                // hop (`gone -> elsewhere`) must both resolve, each splice re-entering the walk.
                let (_dir, root) = temp_cwd();
                let cwd = root.join("repo");
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(root.join("elsewhere")).unwrap();
                symlink_dir(Path::new("elsewhere"), &root.join("gone"));
                symlink_dir(Path::new("../gone/dir"), &cwd.join("out"));

                let target = Access::for_path("out/new.rs", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (
                        root.join("elsewhere/dir/new.rs").as_path(),
                        None,
                        Zone::Outside,
                    ),
                );
            }

            #[test]
            fn follows_a_symlink_reached_by_stepping_out_of_a_missing_dir() {
                // `missing/..` must not hide the later `link` from the walk: the write lands where
                // the link points, and through `tools -> .git/hooks` that would be a hook.
                let (_dir, cwd) = temp_cwd();
                let (_outside_dir, outside) = temp_cwd();
                symlink_dir(&outside, &cwd.join("link"));
                std::fs::create_dir_all(cwd.join(".git/hooks")).unwrap();
                symlink_dir(&cwd.join(".git/hooks"), &cwd.join("tools"));

                let target = Access::for_path("missing/../link/x", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("x").as_path(), None, Zone::Outside),
                );

                let target = Access::for_path("x/../tools/pre-commit", &Workspace::new(&cwd));
                assert_eq!(protected(&target), [Shipped::Git]);
                let p = resolved_policy(Mode::Manual, &[], &[], &[(EDIT, "*")], &cwd);
                assert_eq!(decide(&p, EDIT, &target), Decision::Ask);
            }

            #[test]
            fn dangling_symlink_target_with_a_missing_dot_dot_is_walked_component_by_component() {
                let (_dir, cwd) = temp_cwd();
                let (_outside_dir, outside) = temp_cwd();
                symlink_dir(&outside, &cwd.join("out"));
                symlink_file(Path::new("missing/../out/new"), &cwd.join("notes.md"));

                let target = Access::for_path("notes.md", &Workspace::new(&cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("new").as_path(), None, Zone::Outside),
                );
            }

            #[test]
            fn symlinked_protected_name_protects_both_spellings() {
                // `.vscode -> tools/vscode` at the top and `sub/.idea -> ../tools/idea` deeper: the
                // link target named directly and the nested link named as spelled must both ask.
                let (_dir, cwd) = temp_cwd();
                std::fs::create_dir_all(cwd.join("tools/vscode")).unwrap();
                std::fs::create_dir_all(cwd.join("tools/idea")).unwrap();
                std::fs::create_dir(cwd.join("sub")).unwrap();
                symlink_dir(Path::new("tools/vscode"), &cwd.join(".vscode"));
                symlink_dir(Path::new("../tools/idea"), &cwd.join("sub/.idea"));

                for raw in [
                    ".vscode/tasks.json",
                    "tools/vscode/tasks.json",
                    "sub/.idea/workspace.xml",
                ] {
                    let target = Access::for_path(raw, &Workspace::new(&cwd));
                    assert_eq!(protected(&target), [Shipped::Editors], "{raw}");
                }
                let sibling = Access::for_path("tools/other.rs", &Workspace::new(&cwd));
                assert_eq!(protected(&sibling), []);
            }

            #[test]
            fn symlinked_protected_name_in_an_extra_root_protects_its_target() {
                let (_dir, base) = temp_cwd();
                let (repo, shared) = (base.join("repo"), base.join("shared"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir_all(shared.join("tools/vscode")).unwrap();
                symlink_dir(Path::new("tools/vscode"), &shared.join(".vscode"));
                let workspace = Workspace::resolve(&repo, &[slash_separated(&shared)]).unwrap();

                for (raw, groups) in [
                    ("../shared/.vscode/tasks.json", &[Shipped::Editors][..]),
                    ("../shared/tools/vscode/tasks.json", &[Shipped::Editors]),
                    ("../shared/tools/other.rs", &[]),
                ] {
                    let target = Access::for_path(raw, &workspace);
                    assert_eq!(protected(&target), groups, "{raw}");
                }
            }

            #[test]
            fn symlink_loop_never_auto_allows() {
                let (_dir, cwd) = temp_cwd();
                symlink_file(Path::new("b"), &cwd.join("a"));
                symlink_file(Path::new("a"), &cwd.join("b"));

                let target = Access::for_path("a/new.rs", &Workspace::new(&cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!((relative, zone), (None, Zone::Outside));
            }

            #[test]
            fn past_a_file_or_an_overlong_name_keeps_the_joined_spelling() {
                let (_dir, cwd) = temp_cwd();
                std::fs::write(cwd.join("notes"), "").unwrap();

                for path in [
                    Path::new("notes").join("sub"),
                    PathBuf::from("a".repeat(256)),
                ] {
                    let target = Access::for_path(path.to_str().unwrap(), &Workspace::new(&cwd));

                    assert_eq!(
                        resolved(&target),
                        (&*cwd.join(&path), None, Zone::Outside),
                        "{path:?}",
                    );
                }
            }

            // A Unix name is raw bytes, so only there can one fail to be UTF-8, as `caf\xe9` does.
            #[cfg(unix)]
            #[test]
            fn resolving_to_a_name_that_is_not_utf8_never_auto_allows() {
                use std::os::unix::ffi::OsStrExt as _;
                let (_dir, base) = temp_cwd();
                let cwd = base.join(OsStr::from_bytes(b"caf\xe9"));

                let target = Access::for_path("notes.md", &Workspace::new(&cwd));

                assert_eq!(target.resolved(), Some(cwd.join("notes.md")));
                let (canonical, _, zone) = resolved(&target);
                assert_eq!(
                    canonical,
                    Path::new(&*cwd.join("notes.md").to_string_lossy())
                );
                assert_eq!(zone, Zone::Outside);
            }

            // Windows drops `..` lexically before a lookup, so only Unix stops at a file before it.
            #[cfg(unix)]
            #[test]
            fn dot_dot_past_a_file_is_not_resolved_lexically() {
                // The kernel cannot step back out of the file `notes`, so the target keeps its
                // spelling and never auto-allows as `x` or `link/x`.
                let (_dir, cwd) = temp_cwd();
                let (_outside_dir, outside) = temp_cwd();
                symlink_dir(&outside, &cwd.join("link"));
                std::fs::write(cwd.join("notes"), "").unwrap();

                for path in ["notes/../x", "notes/sub/../../link/x"] {
                    let target = Access::for_path(path, &Workspace::new(&cwd));

                    assert_eq!(
                        resolved(&target),
                        (&*cwd.join(path), None, Zone::Outside),
                        "{path}",
                    );
                }
            }

            #[cfg(windows)]
            #[test]
            fn stream_suffixes_compare_as_the_names_they_open_as() {
                let (_dir, cwd) = temp_cwd();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                std::fs::write(cwd.join(r".git\config"), "").unwrap();
                std::fs::create_dir(cwd.join("secrets-store")).unwrap();
                std::fs::write(cwd.join(r"secrets-store\key"), "").unwrap();

                let target =
                    Access::for_path(r".git::$INDEX_ALLOCATION\config", &Workspace::new(&cwd));
                let (path, relative, _) = resolved(&target);
                assert_eq!(path.as_os_str(), cwd.join(r".git\config").as_os_str());
                assert_eq!(relative, Some(Path::new(r".git\config")));
                assert_eq!(protected(&target), [Shipped::Git]);
                let p = policy(Mode::Manual, &[(EDIT, "secrets-store/key")], &[], &[]);
                let target = Access::for_path(r"secrets-store\key::$DATA", &Workspace::new(&cwd));
                assert_eq!(decide(&p, EDIT, &target), Decision::Deny);

                // The home directory spelled with a stream suffix is still never a workspace.
                let home = cwd.join("home-directory");
                std::fs::create_dir(&home).unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let home_alias = cwd.join("home-directory::$INDEX_ALLOCATION");
                    let target = Access::for_path("notes.md", &Workspace::new(&home_alias));
                    assert_eq!(
                        resolved(&target),
                        (
                            &*home.join("notes.md"),
                            Some(Path::new("notes.md")),
                            Zone::Outside
                        ),
                    );
                });
            }
        }
    }

    mod unreachable {
        use super::*;

        #[test]
        fn names_the_directory_it_cannot_search_and_any_other_cause() {
            let at = Path::new("locked").join("sub");
            let denied = Unreachable::Component {
                at: at.clone(),
                error: io::ErrorKind::PermissionDenied.into(),
            };
            let other = Unreachable::Component {
                at: at.clone(),
                error: io::Error::other("disk on fire"),
            };

            assert_eq!(denied.to_string(), "cannot search `locked`");
            assert_eq!(
                other.to_string(),
                format!("cannot reach `{}`: disk on fire", at.display()),
            );
        }
    }

    mod resolve {
        use super::*;

        #[test]
        fn preserves_directory_constraints() {
            let (_dir, cwd) = temp_cwd();
            std::fs::write(cwd.join("notes"), "original").unwrap();
            std::fs::create_dir(cwd.join("folder")).unwrap();

            for suffix in ["/", "/."] {
                let error = resolve(&cwd.join(format!("notes{suffix}"))).unwrap_err();
                assert!(matches!(error, Unreachable::Component { error, .. }
                    if error.kind() == io::ErrorKind::NotADirectory));
                let directory = resolve(&cwd.join(format!("folder{suffix}"))).unwrap();
                assert_eq!(directory.as_os_str(), cwd.join("folder").as_os_str());
                let missing = resolve(&cwd.join(format!("missing{suffix}"))).unwrap();
                let mut expected = cwd.join("missing").into_os_string();
                expected.push(std::path::MAIN_SEPARATOR_STR);
                assert_eq!(missing.as_os_str(), expected);
            }
        }

        #[test]
        fn preserves_directory_constraints_in_symlink_targets() {
            let (_dir, cwd) = temp_cwd();
            std::fs::write(cwd.join("notes"), "original").unwrap();
            symlink_file(Path::new("notes/"), &cwd.join("file-link"));
            symlink_dir(Path::new("missing/"), &cwd.join("missing-link"));

            let error = resolve(&cwd.join("file-link")).unwrap_err();
            assert!(matches!(error, Unreachable::Component { error, .. }
                if error.kind() == io::ErrorKind::NotADirectory));
            let missing = resolve(&cwd.join("missing-link")).unwrap();
            let mut expected = cwd.join("missing").into_os_string();
            expected.push(std::path::MAIN_SEPARATOR_STR);
            assert_eq!(missing.as_os_str(), expected);
        }

        #[cfg(windows)]
        #[test]
        fn preserves_directory_constraints_on_verbatim_paths() {
            let (_dir, cwd) = temp_cwd();
            std::fs::write(cwd.join("notes"), "original").unwrap();
            let verbatim = std::fs::canonicalize(&cwd).unwrap();

            for suffix in ["\\", "\\."] {
                let file = PathBuf::from(format!("{}\\notes{suffix}", verbatim.display()));
                let error = resolve(&file).unwrap_err();
                assert!(matches!(error, Unreachable::Component { error, .. }
                    if error.kind() == io::ErrorKind::NotADirectory));
                let path = PathBuf::from(format!("{}\\missing{suffix}", verbatim.display()));
                let missing = resolve(&path).unwrap();
                let mut expected = verbatim.join("missing").into_os_string();
                expected.push(std::path::MAIN_SEPARATOR_STR);
                assert_eq!(missing.as_os_str(), expected);
            }
        }
    }

    #[cfg(windows)]
    mod long_names {
        use super::*;

        #[test]
        fn spells_short_names_in_full_and_keeps_the_missing_tail() {
            let canonicalize = |path: &Path| {
                [
                    (r"C:\repo\GIT~1", r"\\?\C:\repo\.git"),
                    (r"C:\repo\SECRET~1\key", r"\\?\C:\repo\secrets-store\key"),
                    (r"S:\GIT~1", r"\\?\C:\work\repo\.git"),
                    (r"Z:\GIT~1", r"\\?\UNC\server\share\.git"),
                ]
                .into_iter()
                .find(|(short, _)| path == Path::new(short))
                .map(|(_, long)| PathBuf::from(long))
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
            };

            for (path, expected) in [
                (r"C:\repo\GIT~1\config", r"C:\repo\.git\config"),
                (r"C:\repo\SECRET~1\key", r"C:\repo\secrets-store\key"),
                (r"S:\GIT~1\config", r"C:\work\repo\.git\config"),
                (r"Z:\GIT~1\config", r"Z:\.git\config"),
                (r"C:\new\notes.md", r"C:\new\notes.md"),
            ] {
                assert_eq!(
                    long_names(PathBuf::from(path), canonicalize),
                    Path::new(expected),
                    "{path}",
                );
            }
        }
    }

    mod basis {
        use super::*;

        #[test]
        fn display_names_each_basis_as_a_denial_or_log_cites_it() {
            let make = Rule::parse(EXEC, "make *").unwrap();
            for (basis, expected) in [
                (
                    rule_basis(Tier::Deny, READ, ".env"),
                    "the `deny.read` pattern `.env`",
                ),
                (
                    Basis::Session(make.clone()),
                    "the session `allow.exec` pattern `make *`",
                ),
                (Basis::Builtin(Shipped::Ssh), "the built-in `ssh` rule"),
                (Basis::Mode(Mode::Plan), "`plan` mode"),
                (Basis::Read, "the default for reads"),
                (
                    Basis::Workspace,
                    "the default for edits inside the workspace",
                ),
                (Basis::SafeRead, "a known-safe read"),
                (
                    Basis::Command(vec![Basis::SafeRead, Basis::Session(make)]),
                    "a known-safe read, the session `allow.exec` pattern `make *`",
                ),
            ] {
                assert_eq!(basis.to_string(), expected);
            }
        }

        mod reason {
            use super::*;

            #[test]
            fn names_the_rule_that_holds_a_call_but_not_the_mode() {
                assert_eq!(
                    rule_basis(Tier::Ask, EXEC, "git push *")
                        .reason()
                        .as_deref(),
                    Some("Held by the `ask.exec` pattern `git push *`"),
                );
                assert_eq!(
                    Basis::Builtin(Shipped::Git).reason().as_deref(),
                    Some("Held by the built-in `git` rule"),
                );
                assert_eq!(Basis::Mode(Mode::Manual).reason(), None);
            }
        }
    }

    mod policy {
        use super::*;

        #[test]
        fn allows_for_the_session_what_asked_until_cleared() {
            let mut p = policy(Mode::Manual, &[], &[], &[]);
            let hosts = abs("/etc/hosts");
            let hosts_glob = slash_separated(Path::new(&hosts));
            assert_eq!(
                decide(&p, EXEC, &command("cargo test --all")),
                Decision::Ask
            );

            for (class, pattern) in [
                (EXEC, "cargo test *"),
                (EXEC, "cargo test *"),
                (EDIT, &*hosts_glob),
            ] {
                p.allow_for_session(Rule::parse(class, pattern).unwrap());
            }
            let held: Vec<&str> = p.session_allow().iter().map(Rule::pattern).collect();
            assert_eq!(held, ["cargo test *", hosts_glob.as_str()]);
            assert_eq!(
                decide(&p, EXEC, &command("cargo test --all")),
                Decision::Allow
            );
            assert_eq!(decide(&p, EDIT, &outside(&hosts)), Decision::Allow);

            p.clear_session_allow();
            assert_eq!(p.session_allow(), []);
            assert_eq!(
                decide(&p, EXEC, &command("cargo test --all")),
                Decision::Ask
            );
        }

        mod resolve {
            use super::*;

            #[test]
            fn shipped_defaults_each_hold_a_call_they_name_until_disabled() {
                let (_dir, root) = temp_cwd();
                let (cwd, home, data) = (root.join("repo"), root.join("home"), root.join("data"));
                for dir in [&cwd, &home, &data] {
                    std::fs::create_dir(dir).unwrap();
                }
                let ssh_key = slash_separated(&home.join(".ssh/id_ed25519"));
                let credentials = slash_separated(&data.join("ox/credentials.json"));
                let claude = slash_separated(&home.join(".claude/.credentials.json"));
                let mut cases = vec![
                    (Shipped::Rm, EXEC, command("rm -rf target"), Decision::Ask),
                    (Shipped::Rm, EXEC, command("rm -fr /tmp/x"), Decision::Ask),
                    (Shipped::Ssh, READ, outside(&ssh_key), Decision::Deny),
                    (
                        Shipped::Credentials,
                        READ,
                        outside(&credentials),
                        Decision::Deny,
                    ),
                    (Shipped::Credentials, READ, outside(&claude), Decision::Deny),
                ];
                if cfg!(unix) {
                    cases.push((
                        Shipped::Devices,
                        EXEC,
                        command("echo x > /dev/sda"),
                        Decision::Ask,
                    ));
                }

                temp_env::with_vars(
                    [(HOME_VAR, Some(&home)), ("XDG_DATA_HOME", Some(&data))],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let allow_all = rules(&[(READ, "*"), (EDIT, "*"), (EXEC, "*")]);
                        for (group, class, target, held) in &cases {
                            let resolve = |disabled: &[Shipped]| {
                                Policy::resolve(
                                    Mode::Manual,
                                    Vec::new(),
                                    Vec::new(),
                                    allow_all.clone(),
                                    disabled,
                                    &workspace,
                                )
                                .unwrap()
                            };
                            assert_eq!(decide(&resolve(&[]), *class, target), *held, "{group:?}");
                            assert_eq!(
                                decide(&resolve(&[*group]), *class, target),
                                Decision::Allow,
                                "{group:?} off"
                            );
                        }
                    },
                );
            }

            #[test]
            fn protects_the_user_git_and_ssh_config_until_disabled() {
                let (_dir, root) = temp_cwd();
                let (cwd, home, config) =
                    (root.join("repo"), root.join("home"), root.join("config"));
                std::fs::create_dir(&cwd).unwrap();
                temp_env::with_vars(
                    [(HOME_VAR, Some(&home)), ("XDG_CONFIG_HOME", Some(&config))],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let resolve = |disabled: &[Shipped]| {
                            Policy::resolve(
                                Mode::Yolo,
                                Vec::new(),
                                Vec::new(),
                                rules(&[(EDIT, "*")]),
                                disabled,
                                &workspace,
                            )
                            .unwrap()
                        };
                        let edit =
                            |path: PathBuf| Access::for_path(&slash_separated(&path), &workspace);

                        for (path, group) in [
                            (config.join("git/config"), Shipped::Git),
                            (home.join(".ssh/config"), Shipped::Ssh),
                            (home.join(".ssh/authorized_keys"), Shipped::Ssh),
                        ] {
                            let target = edit(path.clone());
                            assert_eq!(
                                decide(&resolve(&[]), EDIT, &target),
                                Decision::Ask,
                                "{path:?}"
                            );
                            assert_eq!(
                                decide(&resolve(&[group]), EDIT, &target),
                                Decision::Allow,
                                "{path:?} with {group} off"
                            );
                        }
                        let known_hosts = edit(home.join(".ssh/known_hosts"));
                        assert_eq!(decide(&resolve(&[]), EDIT, &known_hosts), Decision::Allow);
                    },
                );
            }

            #[test]
            fn disables_one_protected_group_and_keeps_the_rest() {
                let (_dir, cwd) = temp_cwd();
                let workspace = Workspace::new(&cwd);
                let p = Policy::resolve(
                    Mode::Manual,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &[Shipped::Cargo],
                    &workspace,
                )
                .unwrap();

                let cargo = Access::for_path(".cargo/config.toml", &workspace);
                let git = Access::for_path(".git/config", &workspace);
                assert_eq!(decide(&p, EDIT, &cargo), Decision::Allow);
                assert_eq!(decide(&p, EDIT, &git), Decision::Ask);
                assert_eq!(
                    decide(
                        &p,
                        EXEC,
                        &Access::for_command("echo x > .cargo/config", &workspace)
                    ),
                    Decision::Allow
                );
            }

            #[test]
            fn deny_and_ask_rules_also_cover_where_their_literal_prefix_resolves() {
                // `secrets -> ../vault` and `alias -> real`: naming the link targets directly must
                // still hold the call back, for an anchored rule and an absolute one alike.
                let (_dir, root) = temp_cwd();
                let cwd = root.join("repo");
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(root.join("vault")).unwrap();
                std::fs::create_dir(root.join("real")).unwrap();
                symlink_dir(Path::new("../vault"), &cwd.join("secrets"));
                symlink_dir(Path::new("real"), &root.join("alias"));

                let alias = slash_separated(&root.join("alias"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[(READ, "secrets/**")],
                    &[(READ, &alias)],
                    &[],
                    &cwd,
                );
                for (raw, decision) in [
                    ("../vault/key", Decision::Deny),
                    ("../real/x", Decision::Ask),
                    ("../other/x", Decision::Allow),
                ] {
                    let target = Access::for_path(raw, &Workspace::new(&cwd));
                    assert_eq!(decide(&p, READ, &target), decision, "{raw}");
                }
            }
        }

        mod session_grant {
            use super::*;

            #[test]
            fn suggests_a_rule_that_allows_the_call() {
                let p = policy(Mode::Manual, &[], &[], &[]);
                let target = command("RUST_LOG=debug cargo test 2>&1");

                let grant = p.session_grant(EXEC, &target).unwrap();
                assert_eq!(grant.suggested().pattern(), "cargo test");
                let mut granted = p.clone();
                granted.allow_for_session(grant.suggested().clone());
                assert_eq!(decide(&granted, EXEC, &target), Decision::Allow);
                assert!(
                    p.session_grant(EXEC, &command("cargo test && git push"))
                        .is_none()
                );
            }

            #[test]
            fn declines_a_call_that_no_allow_rule_can_let_through() {
                let (_dir, cwd) = temp_cwd();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                let p = policy(Mode::Manual, &[], &[(EXEC, "git push *")], &[]);

                let protected = Access::for_path(".git/config", &Workspace::new(&cwd));
                assert!(p.session_grant(EDIT, &protected).is_none());
                assert!(p.session_grant(EXEC, &command("git push origin")).is_none());
                assert!(p.session_grant(EXEC, &command("ls *.rs")).is_none());
            }
        }

        mod read_filter {
            use super::*;

            #[test]
            fn withholds_exactly_the_files_the_gate_holds_back() {
                let (_dir, cwd) = temp_cwd();
                let secrets = slash_separated(&cwd.join("secrets"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[(READ, ".env"), (EDIT, "notes.md")],
                    &[(READ, &secrets)],
                    &[],
                    &cwd,
                );
                let filter = p.read_filter();
                let workspace = Workspace::new(&cwd);
                let walk = filter.walk(&cwd, Path::new("."), &workspace);

                for (name, held) in [
                    (".env", Some(Decision::Deny)),
                    ("sub/.ENV", Some(Decision::Deny)),
                    ("secrets/key", Some(Decision::Ask)),
                    ("notes.md", None),
                    ("src/main.rs", None),
                ] {
                    let gate = Access::for_path(name, &workspace);
                    assert_eq!(walk.withholds(&cwd.join(name)), held.is_some(), "{name}");
                    assert_eq!(
                        decide(&p, READ, &gate),
                        held.unwrap_or(Decision::Allow),
                        "{name}"
                    );
                }
                let empty = ReadFilter::default();
                assert!(
                    !empty
                        .walk(&cwd, Path::new("."), &workspace)
                        .withholds(&cwd.join(".env"))
                );
            }
        }

        mod decide {
            use super::*;

            #[test]
            fn deny_beats_ask_and_ask_beats_allow_in_every_mode() {
                for mode in [Mode::Manual, Mode::Plan, Mode::Yolo] {
                    let p = policy(
                        mode,
                        &[(READ, "secret.txt")],
                        &[(READ, "*.txt")],
                        &[(READ, "*")],
                    );
                    assert_eq!(
                        decide(&p, READ, &workspace("/repo/secret.txt", "secret.txt")),
                        Decision::Deny,
                        "{mode}"
                    );
                    assert_eq!(
                        decide(&p, READ, &workspace("/repo/notes.txt", "notes.txt")),
                        Decision::Ask,
                        "{mode}"
                    );
                    assert_eq!(
                        decide(&p, READ, &workspace("/repo/a.rs", "a.rs")),
                        Decision::Allow,
                        "{mode}"
                    );
                }
            }

            #[test]
            fn ask_rule_outranks_the_workspace_allow_and_a_session_grant() {
                let mut p = policy(
                    Mode::Manual,
                    &[],
                    &[(EDIT, "Cargo.lock"), (EXEC, "git push *")],
                    &[],
                );
                p.allow_for_session(Rule::parse(EXEC, "git *").unwrap());
                p.allow_for_session(Rule::parse(EDIT, "*").unwrap());

                assert_eq!(
                    decide(&p, EDIT, &workspace("/repo/Cargo.lock", "Cargo.lock")),
                    Decision::Ask
                );
                assert_eq!(
                    decide(&p, EXEC, &command("git push origin main")),
                    Decision::Ask
                );
                assert_eq!(decide(&p, EXEC, &command("git status")), Decision::Allow);
                assert_eq!(
                    decide(&p, EDIT, &workspace("/repo/Cargo.toml", "Cargo.toml")),
                    Decision::Allow
                );
            }

            #[test]
            fn read_class_allows_unless_a_deny_or_ask_rule_holds_it() {
                let p = policy(
                    Mode::Manual,
                    &[(READ, ".env")],
                    &[(READ, &abs("/etc/**"))],
                    &[],
                );
                assert_eq!(
                    decide(&p, READ, &outside(&abs("/usr/share/x"))),
                    Decision::Allow
                );
                assert_eq!(
                    decide(&p, READ, &workspace("/repo/.env", ".env")),
                    Decision::Deny
                );
                assert_eq!(
                    decide(&p, READ, &outside(&abs("/etc/hosts"))),
                    Decision::Ask
                );
            }

            #[test]
            fn star_holds_a_call_with_no_target() {
                let p = policy(Mode::Manual, &[(READ, "*")], &[], &[]);
                assert_eq!(decide(&p, READ, &Access::None), Decision::Deny);
                let p = policy(Mode::Manual, &[], &[(READ, "*")], &[]);
                assert_eq!(decide(&p, READ, &Access::None), Decision::Ask);
                let p = policy(Mode::Manual, &[], &[], &[]);
                assert_eq!(decide(&p, READ, &Access::None), Decision::Allow);
            }

            #[test]
            fn protected_edit_asks_even_under_a_broad_allow_and_in_yolo() {
                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = policy(mode, &[], &[], &[(EDIT, "*")]);
                    let envrc = path(
                        "/repo/.envrc",
                        Some(".envrc"),
                        Zone::Workspace,
                        &[Shipped::Direnv],
                    );
                    assert_eq!(decide(&p, EDIT, &envrc), Decision::Ask, "{mode}");
                    assert_eq!(
                        decide(&p, READ, &envrc),
                        Decision::Allow,
                        "{mode}: reads pass"
                    );
                }
            }

            #[test]
            fn spilled_tool_output_reads_without_a_prompt_and_asks_to_be_edited() {
                let (_dir, root) = temp_cwd();
                let (cwd, home, data) = (root.join("repo"), root.join("home"), root.join("data"));
                std::fs::create_dir(&cwd).unwrap();
                temp_env::with_vars(
                    [(HOME_VAR, Some(&home)), ("XDG_DATA_HOME", Some(&data))],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let store = SessionStore::open(&cwd).unwrap();
                        store.create(&sample_header("s1"), &[]).unwrap();
                        let session_file = store.find_session_path("s1").unwrap();
                        let spill = artifact_dir(&session_file, "s1").join("tool-results/x.txt");
                        let target = Access::for_path(&slash_separated(&spill), &workspace);

                        for mode in [Mode::Manual, Mode::Yolo] {
                            let p = Policy::resolve(
                                mode,
                                Vec::new(),
                                Vec::new(),
                                rules(&[(EDIT, "*")]),
                                &[],
                                &workspace,
                            )
                            .unwrap();
                            assert_eq!(decide(&p, READ, &target), Decision::Allow, "{mode}");
                            assert_eq!(decide(&p, EDIT, &target), Decision::Ask, "{mode}");
                        }
                    },
                );
            }

            #[test]
            fn workspace_edit_allows_and_an_outside_edit_needs_an_allow_rule() {
                let p = policy(Mode::Manual, &[], &[], &[(EDIT, &abs("/tmp/**"))]);
                assert_eq!(
                    decide(&p, EDIT, &workspace("/repo/new.rs", "new.rs")),
                    Decision::Allow
                );
                assert_eq!(
                    decide(&p, EDIT, &outside(&abs("/etc/passwd"))),
                    Decision::Ask
                );
                assert_eq!(
                    decide(&p, EDIT, &outside(&abs("/tmp/scratch.txt"))),
                    Decision::Allow
                );
            }

            #[test]
            fn allow_rule_matches_only_the_spelling_it_names() {
                let p = policy(Mode::Manual, &[], &[], &[(EDIT, &abs("/tmp/notes/**"))]);
                assert_eq!(
                    decide(&p, EDIT, &outside(&abs("/tmp/notes/a.md"))),
                    Decision::Allow
                );
                for raw in [
                    "/tmp/NOTES/a.md",
                    "/tmp/notes./a.md",
                    "/tmp/no\u{200b}tes/a.md",
                ] {
                    assert_eq!(
                        decide(&p, EDIT, &outside(&abs(raw))),
                        Decision::Ask,
                        "{raw}"
                    );
                }
            }

            #[test]
            fn anchored_patterns_apply_below_each_extra_root() {
                let (_dir, base) = temp_cwd();
                let (repo, shared) = (base.join("repo"), base.join("shared"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&shared).unwrap();
                let workspace = Workspace::resolve(&repo, &[slash_separated(&shared)]).unwrap();
                let p = policy(
                    Mode::Manual,
                    &[(READ, "./secrets")],
                    &[(EDIT, "generated/**")],
                    &[],
                );

                for (raw, class, decision) in [
                    ("secrets/key", READ, Decision::Deny),
                    ("../shared/secrets/key", READ, Decision::Deny),
                    ("../shared/src/secrets/key", READ, Decision::Allow),
                    ("../shared/generated/x.rs", EDIT, Decision::Ask),
                    ("../shared/src/x.rs", EDIT, Decision::Allow),
                ] {
                    let target = Access::for_path(raw, &workspace);
                    assert_eq!(decide(&p, class, &target), decision, "{raw}");
                }
            }

            #[test]
            fn deny_matches_case_normalization_and_ignorable_variants_of_a_name() {
                // A case-insensitive or normalizing file system opens each spelling as the denied
                // file, and an uppercased cwd prefix still names the same directory there.
                let (_dir, cwd) = temp_cwd();
                let p = policy(
                    Mode::Manual,
                    &[(READ, ".env"), (READ, "caf\u{e9}.txt")],
                    &[],
                    &[],
                );
                let upper_cwd = format!("{}/.env", cwd.to_str().unwrap().to_uppercase());
                for raw in [
                    ".ENV",
                    ".env.",
                    ".e\u{200b}nv",
                    "cafe\u{301}.txt",
                    "CAF\u{c9}.TXT",
                    &upper_cwd,
                ] {
                    let target = Access::for_path(raw, &Workspace::new(&cwd));
                    assert_eq!(decide(&p, READ, &target), Decision::Deny, "{raw}");
                }
            }

            #[test]
            fn deny_matches_a_symlinked_path_as_spelled_even_in_yolo() {
                // `.env -> ../shared/.env` resolves outside cwd and `secrets -> ../vault` away from
                // the absolute glob, yet both deny rules must fire on the path the call names.
                let (_dir, root) = temp_cwd();
                let cwd = root.join("repo");
                std::fs::create_dir_all(root.join("shared")).unwrap();
                std::fs::create_dir_all(root.join("vault")).unwrap();
                std::fs::create_dir(&cwd).unwrap();
                std::fs::write(root.join("shared/.env"), "SECRET=1").unwrap();
                symlink_file(Path::new("../shared/.env"), &cwd.join(".env"));
                symlink_dir(Path::new("../vault"), &cwd.join("secrets"));

                let secrets = slash_separated(&cwd.join("secrets"));
                let p = policy(
                    Mode::Yolo,
                    &[(READ, ".env"), (EDIT, ".env"), (READ, &secrets)],
                    &[],
                    &[],
                );
                let env = Access::for_path(".env", &Workspace::new(&cwd));
                assert_eq!(resolved(&env).1, None, "the link resolves outside cwd");
                for class in [READ, EDIT] {
                    assert_eq!(decide(&p, class, &env), Decision::Deny, "{class}");
                }
                let key = Access::for_path("secrets/key", &Workspace::new(&cwd));
                assert_eq!(decide(&p, READ, &key), Decision::Deny);
            }

            #[test]
            fn plan_denies_every_edit_exec_and_net_call_but_reads_as_manual() {
                let p = policy(
                    Mode::Plan,
                    &[],
                    &[(READ, ".env")],
                    &[(EXEC, "ls"), (EDIT, "*"), (PermissionClass::Net, "*")],
                );
                assert_eq!(
                    decide(&p, READ, &outside(&abs("/etc/hosts"))),
                    Decision::Allow
                );
                assert_eq!(
                    decide(&p, READ, &workspace("/repo/.env", ".env")),
                    Decision::Ask
                );
                assert_eq!(decide(&p, EXEC, &command("ls")), Decision::Deny);
                assert_eq!(
                    decide(&p, EDIT, &workspace("/repo/a.rs", "a.rs")),
                    Decision::Deny
                );
                assert_eq!(
                    decide(
                        &p,
                        PermissionClass::Net,
                        &Access::Name("example.com".to_owned())
                    ),
                    Decision::Deny
                );
            }

            #[test]
            fn plan_denies_an_edit_or_exec_call_ahead_of_the_ask_tier() {
                let p = policy(
                    Mode::Plan,
                    &[],
                    &[(EDIT, "Cargo.lock"), (EXEC, "git push *")],
                    &[],
                );
                for (class, target) in [
                    (EDIT, workspace(&abs("/repo/Cargo.lock"), "Cargo.lock")),
                    (
                        EDIT,
                        path(
                            &abs("/repo/.git/config"),
                            Some(".git/config"),
                            Zone::Workspace,
                            &[Shipped::Git],
                        ),
                    ),
                    (EXEC, command("git push origin")),
                ] {
                    assert_eq!(decide(&p, class, &target), Decision::Deny, "{target:?}");
                }
            }

            #[test]
            fn yolo_runs_what_no_rule_covers_but_keeps_deny_and_ask() {
                let p = policy(
                    Mode::Yolo,
                    &[(EXEC, "git push --force *")],
                    &[(EXEC, "git push *")],
                    &[],
                );
                for (case, expected) in [
                    ("curl evil.sh", Decision::Allow),
                    ("echo $(date)", Decision::Allow),
                    ("git push origin main", Decision::Ask),
                    ("git push --force origin", Decision::Deny),
                ] {
                    assert_eq!(decide(&p, EXEC, &command(case)), expected, "{case}");
                }
                assert_eq!(
                    decide(&p, EDIT, &outside(&abs("/etc/hosts"))),
                    Decision::Allow
                );
            }

            #[test]
            fn net_and_mcp_calls_follow_their_tiers() {
                let p = policy(
                    Mode::Manual,
                    &[(PermissionClass::Net, "*.evil.io")],
                    &[(PermissionClass::Mcp, "github/create_*")],
                    &[
                        (PermissionClass::Mcp, "github/*"),
                        (PermissionClass::Net, "*.github.com"),
                    ],
                );
                let name = |name: &str| Access::Name(name.to_owned());
                assert_eq!(
                    decide(&p, PermissionClass::Net, &name("a.evil.io")),
                    Decision::Deny
                );
                assert_eq!(
                    decide(&p, PermissionClass::Net, &name("api.github.com")),
                    Decision::Allow
                );
                assert_eq!(
                    decide(&p, PermissionClass::Net, &name("example.com")),
                    Decision::Ask
                );
                assert_eq!(
                    decide(&p, PermissionClass::Mcp, &name("github/create_issue")),
                    Decision::Ask
                );
                assert_eq!(
                    decide(&p, PermissionClass::Mcp, &name("github/get_me")),
                    Decision::Allow
                );
            }

            #[test]
            fn command_allows_only_when_every_simple_command_is_allowed_or_known_safe() {
                let p = policy(Mode::Manual, &[], &[], &[(EXEC, "cargo test *")]);
                for (case, expected) in [
                    ("cargo test && cat Cargo.toml | wc -l", Decision::Allow),
                    ("RUST_LOG=debug nice -n 5 cargo test --all", Decision::Allow),
                    ("CARGO_TARGET_DIR=/tmp cargo test", Decision::Ask),
                    ("cargo test && rm -rf target", Decision::Ask),
                    ("git status", Decision::Ask),
                    ("FOO=1", Decision::Ask),
                ] {
                    assert_eq!(decide(&p, EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn command_deny_and_ask_hold_a_command_whatever_its_words_expand_to() {
                let p = policy(
                    Mode::Manual,
                    &[(EXEC, "git push --force *")],
                    &[(EXEC, "git push *")],
                    &[(EXEC, "git *")],
                );
                for (case, expected) in [
                    ("git status", Decision::Allow),
                    ("git ${X:-push} origin", Decision::Ask),
                    ("git $'push' origin", Decision::Ask),
                    ("git pu$'sh' origin", Decision::Ask),
                    ("git -c a=b push origin", Decision::Ask),
                    ("git push $'--force'", Decision::Deny),
                    ("git push origin --force", Decision::Deny),
                ] {
                    assert_eq!(decide(&p, EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn command_asks_for_every_spelling_of_a_forced_recursive_rm_even_in_yolo() {
                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = policy(mode, &[], &[], &[(EXEC, "*")]);
                    for case in [
                        "rm -rfv /",
                        "rm -r --force /",
                        "rm / -rf",
                        "rm -f -R x",
                        "rm $'-rf' /",
                        "$'rm' -rf /",
                        "ls && /bin/rm --rec --f x",
                    ] {
                        assert_eq!(
                            decide(&p, EXEC, &command(case)),
                            Decision::Ask,
                            "{mode:?}: {case}"
                        );
                    }
                    for case in ["rm -r x", "rm -f x", "rm -- -rf", "rmdir -rf x"] {
                        assert_eq!(
                            decide(&p, EXEC, &command(case)),
                            Decision::Allow,
                            "{mode:?}: {case}"
                        );
                    }
                }
            }

            #[test]
            fn command_never_allows_an_opaque_command_but_still_denies_one() {
                let p = policy(Mode::Manual, &[(EXEC, "rm -rf *")], &[], &[(EXEC, "*")]);
                assert_eq!(decide(&p, EXEC, &command("ls")), Decision::Allow);
                assert_eq!(decide(&p, EXEC, &command("eval ls")), Decision::Ask);
                for case in [
                    "echo $(rm -rf /)",
                    "sh -c 'rm -rf /'",
                    "\"/bin/rm\" -rf /",
                    "exec rm -rf /",
                    "command rm -rf /",
                ] {
                    assert_eq!(decide(&p, EXEC, &command(case)), Decision::Deny, "{case}");
                }
            }

            #[test]
            fn command_weighs_each_redirection_target_as_an_edit() {
                let (_dir, cwd) = temp_cwd();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                let outside = abs("/etc/hosts");
                let p = resolved_policy(
                    Mode::Manual,
                    &[(EDIT, "secret.txt")],
                    &[(EDIT, "*.lock")],
                    &[],
                    &cwd,
                );
                let decide = |case: &str| {
                    p.decide(
                        EXEC,
                        &Access::for_command(case, &Workspace::new(&cwd)).as_target(),
                    )
                };
                assert_eq!(decide("echo hi > out.txt 2>&1"), Decision::Allow);
                assert_eq!(decide("echo hi > .envrc"), Decision::Ask);
                assert_eq!(decide("echo hi > .git/hooks/pre-commit"), Decision::Ask);
                assert_eq!(decide("echo hi > Cargo.lock"), Decision::Ask);
                assert_eq!(decide("env -C /etc echo hi > hosts"), Decision::Ask);
                assert_eq!(decide(&format!("echo hi >> {outside}")), Decision::Ask);
                assert_eq!(decide("echo hi > secret.txt"), Decision::Deny);

                let (_elsewhere_dir, elsewhere) = temp_cwd();
                let notes = slash_separated(&elsewhere.join("notes.txt"));
                let allowed = resolved_policy(Mode::Manual, &[], &[], &[(EDIT, &notes)], &cwd);
                assert_eq!(
                    allowed.decide(
                        EXEC,
                        &Access::for_command(&format!("echo hi >> {notes}"), &Workspace::new(&cwd))
                            .as_target()
                    ),
                    Decision::Allow
                );
            }

            #[test]
            fn command_asks_for_a_known_safe_read_of_a_denied_or_asked_file() {
                let (_dir, cwd) = temp_cwd();
                let p = policy(
                    Mode::Manual,
                    &[(READ, ".env")],
                    &[(READ, "secrets")],
                    &[(EXEC, "cat *")],
                );
                let decide = |case: &str| {
                    p.decide(
                        EXEC,
                        &Access::for_command(case, &Workspace::new(&cwd)).as_target(),
                    )
                };
                assert_eq!(decide("cat README.md"), Decision::Allow);
                assert_eq!(decide("cat .env"), Decision::Ask);
                assert_eq!(decide("cat secrets/key"), Decision::Ask);
                assert_eq!(decide("grep --file=.env x"), Decision::Ask);
            }

            #[test]
            fn command_asks_for_a_reader_glob_or_variable_that_may_name_a_denied_file() {
                let (_dir, root) = temp_cwd();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(cwd.join("src")).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                std::fs::write(cwd.join("src/main.rs"), "").unwrap();
                std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    let resolve = |disabled: &[Shipped]| {
                        Policy::resolve(
                            Mode::Manual,
                            Vec::new(),
                            Vec::new(),
                            rules(&[(EXEC, "cat *"), (EXEC, "grep *")]),
                            disabled,
                            &workspace,
                        )
                        .unwrap()
                    };
                    let decide = |p: &Policy, case: &str| {
                        p.decide(EXEC, &Access::for_command(case, &workspace).as_target())
                    };

                    let p = resolve(&[]);
                    for (case, expected) in [
                        ("cat src/*.rs", Decision::Allow),
                        ("cat ~/*/id_rsa", Decision::Allow),
                        ("cat ~/.ssh/id_*", Decision::Ask),
                        ("cat ~/.ss?/id_rsa", Decision::Ask),
                        ("cat ~/.*/id_rsa", Decision::Ask),
                        ("grep -r key ~/.ssh", Decision::Ask),
                        ("cat $HOME/.ssh/id_rsa", Decision::Ask),
                    ] {
                        assert_eq!(decide(&p, case), expected, "{case}");
                    }
                    let unguarded = resolve(&[Shipped::Ssh, Shipped::Credentials]);
                    assert_eq!(
                        decide(&unguarded, "cat $HOME/.ssh/id_rsa"),
                        Decision::Allow,
                        "no read rule is left to hold it"
                    );
                });
            }

            #[test]
            fn command_weighs_each_input_redirection_as_a_read() {
                let (_dir, cwd) = temp_cwd();
                let p = policy(
                    Mode::Yolo,
                    &[(READ, ".env")],
                    &[(READ, "secrets")],
                    &[(EXEC, "*")],
                );
                let decide = |case: &str| {
                    p.decide(
                        EXEC,
                        &Access::for_command(case, &Workspace::new(&cwd)).as_target(),
                    )
                };
                for (case, expected) in [
                    ("cat < README.md", Decision::Allow),
                    ("cat < .env", Decision::Deny),
                    ("wc -l 3< config/.env", Decision::Deny),
                    ("python3 run.py < secrets/key", Decision::Ask),
                ] {
                    assert_eq!(decide(case), expected, "{case}");
                }
            }

            #[test]
            fn command_denies_an_input_redirection_from_a_shipped_read_deny() {
                let (_dir, base) = temp_cwd();
                let home = base.join("home");
                let key = slash_separated(&home.join(".ssh/id_ed25519"));
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&base);
                    let p = Policy::resolve(
                        Mode::Manual,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        &[],
                        &workspace,
                    )
                    .unwrap();

                    let decide = |case: &str| {
                        p.decide(EXEC, &Access::for_command(case, &workspace).as_target())
                    };

                    assert_eq!(decide(&format!("cat < {key}")), Decision::Deny);
                    assert_eq!(decide(&format!("cat {key}")), Decision::Ask);
                });
            }
        }

        mod verdict {
            use super::*;

            fn verdict(
                policy: &Policy,
                class: PermissionClass,
                target: &Access,
            ) -> (Decision, Basis) {
                let verdict = policy.verdict(class, &target.as_target());
                (verdict.decision, verdict.basis)
            }

            #[test]
            fn names_the_configured_rule_or_default_that_settles_each_call() {
                let notes = abs("/notes/**");
                let p = policy(
                    Mode::Manual,
                    &[(READ, ".env")],
                    &[(EXEC, "git push *")],
                    &[(EDIT, &notes)],
                );

                for (class, target, expected) in [
                    (
                        READ,
                        workspace("/repo/.env", ".env"),
                        (Decision::Deny, rule_basis(Tier::Deny, READ, ".env")),
                    ),
                    (
                        EXEC,
                        command("git push origin"),
                        (Decision::Ask, rule_basis(Tier::Ask, EXEC, "git push *")),
                    ),
                    (
                        READ,
                        outside(&abs("/etc/hosts")),
                        (Decision::Allow, Basis::Read),
                    ),
                    (
                        EDIT,
                        workspace("/repo/a.rs", "a.rs"),
                        (Decision::Allow, Basis::Workspace),
                    ),
                    (
                        EDIT,
                        outside(&abs("/notes/todo.md")),
                        (Decision::Allow, rule_basis(Tier::Allow, EDIT, &notes)),
                    ),
                    (
                        EXEC,
                        command("git status"),
                        (Decision::Ask, Basis::Mode(Mode::Manual)),
                    ),
                ] {
                    assert_eq!(verdict(&p, class, &target), expected, "{target:?}");
                }
            }

            #[test]
            fn of_an_allowed_command_names_what_lets_each_part_through() {
                let make = Rule::parse(EXEC, "make *").unwrap();
                let mut p = policy(Mode::Manual, &[], &[], &[(EXEC, "cargo test *")]);
                p.allow_for_session(make.clone());

                let settled = verdict(
                    &p,
                    EXEC,
                    &command("cargo test > out.txt && make all | wc -l"),
                );

                assert_eq!(
                    settled,
                    (
                        Decision::Allow,
                        Basis::Command(vec![
                            rule_basis(Tier::Allow, EXEC, "cargo test *"),
                            Basis::Session(make),
                            Basis::SafeRead,
                            Basis::Workspace,
                        ]),
                    ),
                );
            }

            #[test]
            fn of_a_held_command_names_the_read_rule_or_built_in_group_that_holds_it() {
                let p = policy(
                    Mode::Manual,
                    &[],
                    &[(READ, "*.pem")],
                    &[(EXEC, "cat *"), (EXEC, "rm *")],
                );
                let pem = rule_basis(Tier::Ask, READ, "*.pem");

                for (case, expected) in [
                    ("cat key.pem", pem.clone()),
                    ("cat $F", pem),
                    ("rm -rf build", Basis::Builtin(Shipped::Rm)),
                ] {
                    assert_eq!(
                        verdict(&p, EXEC, &command(case)),
                        (Decision::Ask, expected),
                        "{case}"
                    );
                }
                assert_eq!(
                    verdict(
                        &p,
                        EDIT,
                        &path(
                            "/repo/.git/config",
                            Some(".git/config"),
                            Zone::Workspace,
                            &[Shipped::Git]
                        ),
                    ),
                    (Decision::Ask, Basis::Builtin(Shipped::Git)),
                );
            }

            #[test]
            fn credits_a_shipped_pattern_to_its_built_in_group() {
                let (_dir, base) = temp_cwd();
                let home = base.join("home");
                let key = slash_separated(&home.join(".ssh/id_ed25519"));
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&base);
                    let p = Policy::resolve(
                        Mode::Yolo,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        &[],
                        &workspace,
                    )
                    .unwrap();
                    let settle =
                        |case: &str| verdict(&p, EXEC, &Access::for_command(case, &workspace));

                    assert_eq!(
                        settle(&format!("cat < {key}")),
                        (Decision::Deny, Basis::Builtin(Shipped::Ssh)),
                    );
                    assert_eq!(
                        settle(&format!("cat {key}")),
                        (Decision::Ask, Basis::Builtin(Shipped::Ssh)),
                    );
                });
            }

            #[test]
            fn names_the_mode_when_it_settles_the_call() {
                let plan = policy(Mode::Plan, &[], &[], &[(EXEC, "ls")]);
                let yolo = policy(Mode::Yolo, &[], &[], &[]);

                assert_eq!(
                    verdict(&plan, EXEC, &command("ls")),
                    (Decision::Deny, Basis::Mode(Mode::Plan)),
                );
                assert_eq!(
                    verdict(&yolo, EXEC, &command("git status")),
                    (Decision::Allow, Basis::Mode(Mode::Yolo)),
                );
                assert_eq!(
                    verdict(&yolo, EDIT, &Access::Name("notes".to_owned())),
                    (Decision::Allow, Basis::Mode(Mode::Yolo)),
                    "an edit that names no path has no protected group",
                );
            }
        }
    }

    mod session_grant {
        use super::*;

        #[test]
        fn takes_an_edit_that_still_covers_the_call() {
            let target = command("cargo test --all");
            let grant = policy(Mode::Manual, &[], &[], &[])
                .session_grant(EXEC, &target)
                .unwrap();

            let rule = grant.accept(" cargo test * ").unwrap();
            assert_eq!((rule.class(), rule.pattern()), (EXEC, "cargo test *"));
            for (text, error) in [
                ("cargo build", "no longer covers this call"),
                ("cargo test; ls", "one simple command"),
            ] {
                let err = format!("{:#}", grant.accept(text).unwrap_err());
                assert!(err.contains(error), "{text}: {err}");
            }
            let edit = Rule::parse(EDIT, "*").unwrap();
            let err = grant.admit(edit).unwrap_err().to_string();
            assert_eq!(err, "the rule must stay an `exec` rule");
        }

        #[test]
        fn round_trips_through_json_and_still_admits_the_call() {
            let (_dir, root) = temp_cwd();
            let cwd = root.join("cwd");
            std::fs::create_dir(&cwd).unwrap();
            let target = Access::for_path("../notes.md", &Workspace::new(&cwd));
            let grant = policy(Mode::Manual, &[], &[], &[])
                .session_grant(EDIT, &target)
                .unwrap();

            let json = serde_json::to_value(&grant).unwrap();
            assert_eq!(json["class"], "edit");
            assert_eq!(json["target"]["type"], "path");
            assert_eq!(json["suggested"]["pattern"], grant.suggested().pattern());
            let back: SessionGrant = serde_json::from_value(json).unwrap();
            assert_eq!(back, grant);
            assert_eq!(
                back.accept(grant.suggested().pattern()).unwrap(),
                *grant.suggested()
            );
        }
    }

    mod walk_filter {
        use super::*;

        mod withholds {
            use super::*;

            #[test]
            fn parent_traversal_matches_the_gate_decision() {
                let (_dir, cwd) = temp_cwd();
                let root = cwd.ancestors().last().unwrap();
                let absolute_rule = slash_separated(&cwd);
                let policy = policy(Mode::Manual, &[(READ, &absolute_rule)], &[], &[]);
                let filter = policy.read_filter();
                let file = cwd.join("secret.txt");
                let workspace = Workspace::new(root);
                let target = Access::for_path(file.to_str().unwrap(), &workspace);
                assert_eq!(decide(&policy, READ, &target), Decision::Deny);
                for requested in [Path::new(".."), Path::new("../.."), Path::new(".")] {
                    let walk = filter.walk(root, requested, &workspace);
                    assert!(walk.withholds(&file), "{requested:?}");
                }
            }

            #[test]
            fn a_file_by_the_spelling_the_call_used() {
                let (_dir, root) = temp_cwd();
                let (cwd, outside) = (root.join("cwd"), root.join("outside"));
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(&outside).unwrap();
                symlink_dir(&outside, &cwd.join("config"));
                let p = resolved_policy(Mode::Manual, &[(READ, ".env")], &[], &[], &cwd);
                let filter = p.read_filter();
                let env = outside.join(".env");
                let workspace = Workspace::new(&cwd);

                for (spelled, denied) in [("config", true), ("../outside", false)] {
                    let walk = filter.walk(&outside, Path::new(spelled), &workspace);
                    assert_eq!(walk.withholds(&env), denied, "{spelled}");
                    let gate = Access::for_path(&format!("{spelled}/.env"), &workspace);
                    assert_eq!(
                        decide(&p, READ, &gate) == Decision::Deny,
                        denied,
                        "{spelled}"
                    );
                }
            }

            #[test]
            fn where_a_rule_prefix_resolves_through_a_symlink() {
                let (_dir, root) = temp_cwd();
                let (cwd, outside) = (root.join("cwd"), root.join("outside"));
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(&outside).unwrap();
                symlink_dir(&outside, &cwd.join("link"));
                let p = resolved_policy(Mode::Manual, &[], &[(READ, "link/**")], &[], &cwd);

                let filter = p.read_filter();
                let walk = filter.walk(&outside, Path::new("../outside"), &Workspace::new(&cwd));
                assert!(walk.withholds(&outside.join("key")));
            }
        }
    }
}
