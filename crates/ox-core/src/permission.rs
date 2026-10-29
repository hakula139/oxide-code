//! Tool permission checks. [`Policy::verdict`] combines a call's [`Access`] with its mode and rule
//! tiers to produce a [`Decision`] and the [`Basis`] that settled it.
//!
//! oxide-code has no OS sandbox. Command rules are guardrails because an allowed program can access
//! files or run other programs beyond what its command line reveals.

pub mod defaults;
pub(crate) mod project;
pub mod rule;
pub(crate) mod shell;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result, bail, ensure};
use caseless::Caseless as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr, VariantArray};
use unicode_normalization::UnicodeNormalization as _;

use self::defaults::{ProtectedRoots, Shipped, ShippedTier, protected_roots};
use self::rule::{MatchDiscipline, Prefixes, Rule};
use self::shell::{CommandTarget, Segment};
use crate::util::path::{
    expand_absolute, expand_user, lexical_normalize, requires_directory, slash_separated,
};
use crate::util::variant;

// ── Mode ──

/// The standing permission posture, which settles the calls no rule does.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Display,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "lowercase")]
#[strum(
    serialize_all = "lowercase",
    parse_err_ty = anyhow::Error,
    parse_err_fn = variant::unknown::<Self>
)]
pub enum Mode {
    /// Asks about every call the rules leave open.
    #[default]
    Manual,
    /// Read-only analysis: every edit, exec, and net call is denied.
    Plan,
    /// Runs every call no deny rule or configured ask rule holds, skipping the built-in ask rules.
    Yolo,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Manual => "Ask before tool calls that no permission rule allows or denies",
            Self::Plan => "Read-only: deny edits and commands",
            Self::Yolo => "Run tool calls unless a deny rule or your ask rules hold them",
        }
    }
}

// ── Permission Class ──

/// The rule namespace a tool's calls fall under, which also sets the pipeline's default treatment.
/// Rules address classes, so one `read` deny pattern covers every tool that reads a file. Each tool
/// declares its class in its [`crate::tool::ToolSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
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

// ── Tier ──

/// Where a rule list sits in the pipeline: deny outranks ask, which outranks allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Tier {
    Deny,
    Ask,
    Allow,
}

// ── Workspace ──

/// Where a session's tools run: the launch directory, which relative paths resolve against, and the
/// roots inside which an edit needs no rule. The roots are the launch directory plus
/// `permission.extra_roots`, each resolved the way a path target is, and there are none when the
/// launch directory is `/`, `$HOME`, or an ancestor of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    /// The directories anchored patterns apply below: the cwd, then each extra root while there are
    /// roots.
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
    fn loose_rooted(&self, path: &Path, aliases: &mut DirectoryAliases) -> Vec<String> {
        self.anchors
            .iter()
            .flat_map(|anchor| [&anchor.resolved, &anchor.spelled])
            .filter_map(|base| aliases.relative_to(path, base))
            .collect()
    }
}

/// Resolves the configured extra root `raw`, which must be absolute or `~/`-rooted, resolvable, and
/// neither `/`, `$HOME`, nor an ancestor of it.
pub(crate) fn extra_root(raw: &str) -> Result<PathBuf> {
    let root = expand_absolute(raw).with_context(|| format!("invalid extra root {raw:?}"))?;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Zone {
    /// Inside a [`Workspace`] root, so an edit allows without a rule unless a protected name or an
    /// ask rule holds it.
    Workspace,
    /// Anywhere else, so an edit needs an allow rule.
    Outside,
}

/// What a tool call accesses, matched against rule patterns, which [`crate::tool::Tool::prepare`]
/// resolves from its input once so the gate checks and the run act on the same values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Access {
    /// No extractable target, such as a missing path argument, so only a `*` rule matches.
    #[default]
    None,
    Command(CommandTarget),
    Path(PathTarget),
    /// A network host or an MCP `server/tool` name.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no network or MCP tool builds a name target yet")
    )]
    Name(String),
}

impl Access {
    /// Parses a `bash` command, resolving the files it reads and writes in `workspace`.
    pub(crate) fn for_command(command: &str, workspace: &Workspace) -> Self {
        Self::Command(CommandTarget::parse(command, workspace))
    }

    /// Builds a path target for the location the tool will touch, resolving `path` against the
    /// workspace's cwd with `resolve`. A path `resolve` cannot place keeps its joined spelling with
    /// no rooted forms, so it never auto-allows. The tool follows native filesystem behavior. A
    /// resolved path that is not valid UTF-8 never auto-allows either, since rules match it only by
    /// its lossy spelling.
    pub(crate) fn for_path(path: impl AsRef<Path>, workspace: &Workspace) -> Self {
        Self::Path(PathResolver::new(workspace).resolve(path))
    }

    /// Where a path target lands, exactly as the file system names it.
    pub(crate) fn resolved(&self) -> Option<PathBuf> {
        match self {
            Self::Path(path) => Some(path.resolved()),
            Self::None | Self::Command(_) | Self::Name(_) => None,
        }
    }
}

/// A path in two spellings plus their forms relative to the anchor directories. Owned because a
/// resolved path is not a substring of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathTarget {
    /// Where the call lands with every symlink followed. Allow rules, the zone, and the tool's run
    /// use this spelling alone.
    pub(crate) canonical: String,
    /// `canonical` as the file system names it, kept only when a name in it is not valid UTF-8,
    /// which `canonical` spells with replacement characters.
    exact: Option<PathBuf>,
    /// `canonical` under the resolved cwd, set only while it sits inside it.
    pub(crate) relative: Option<String>,
    /// `canonical` under each resolved anchor directory that holds it.
    rooted: Vec<String>,
    /// The path as the call names it, lexically normalized. Deny and ask rules and the protected
    /// check also match it.
    requested: String,
    /// Either spelling relative to either spelling of each anchor directory, with names compared
    /// by [`fold_name`] so a case-variant prefix still counts. Only deny and ask rules match these.
    deny_rooted: Vec<String>,
    pub(crate) zone: Zone,
    /// The shipped groups whose protected names either spelling holds.
    protected: Vec<Shipped>,
}

impl PathTarget {
    /// Where this lands, exactly as the file system names it.
    pub(crate) fn resolved(&self) -> PathBuf {
        self.exact
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.canonical))
    }
}

/// Path resolution for one tool call, sharing protected roots and directory metadata.
struct PathResolver<'a> {
    workspace: &'a Workspace,
    roots: Option<ProtectedRoots>,
    aliases: DirectoryAliases,
}

impl<'a> PathResolver<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            roots: None,
            aliases: DirectoryAliases::default(),
        }
    }

    fn resolve(&mut self, path: impl AsRef<Path>) -> PathTarget {
        let workspace = self.workspace;
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
            .flat_map(|path| workspace.loose_rooted(path, &mut self.aliases))
            .collect();
        deny_rooted.sort();
        deny_rooted.dedup();
        let in_root = resolved.as_deref().is_some_and(|r| workspace.contains(r));
        let canonical = resolved.unwrap_or(joined);

        let lossy = canonical.to_str().is_none();
        let roots = self
            .roots
            .get_or_insert_with(|| protected_roots(cwd, workspace.roots()).into());
        let protected = roots.protected_by(&[&canonical, &requested], &mut self.aliases);
        let zone = if in_root && !lossy && !is_hard_linked(&canonical) {
            Zone::Workspace
        } else {
            Zone::Outside
        };
        PathTarget {
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

/// Directory identities shared within one path check or walk setup. Each operation gets a fresh
/// instance so directories created or replaced between calls remain visible.
#[derive(Default)]
struct DirectoryAliases {
    identities: HashMap<PathBuf, Option<DirectoryIdentity>>,
}

impl DirectoryAliases {
    fn relative_to(&mut self, path: &Path, base: &Path) -> Option<String> {
        if !base.is_absolute() {
            return None;
        }
        let folded = folded_components(base);
        relative_to_folded(path, &folded).or_else(|| {
            let aliased = self.respell(path, base)?;
            if aliased == path {
                return None;
            }
            relative_to_folded(&aliased, &folded)
        })
    }

    /// `path` spelled below the deepest existing directory of `base` when an ancestor has the same
    /// filesystem identity, retaining its suffix even when it does not exist yet.
    fn respell(&mut self, path: &Path, base: &Path) -> Option<PathBuf> {
        let (existing, identity) = base
            .ancestors()
            .find_map(|ancestor| Some((ancestor, self.identity(ancestor)?)))?;
        if path.starts_with(existing) {
            return Some(path.to_path_buf());
        }
        path.ancestors().find_map(|ancestor| {
            if self.identity(ancestor)? != identity {
                return None;
            }
            let rest = path.strip_prefix(ancestor).ok()?;
            Some(if rest.as_os_str().is_empty() {
                existing.to_path_buf()
            } else {
                existing.join(rest)
            })
        })
    }

    fn identity(&mut self, path: &Path) -> Option<DirectoryIdentity> {
        self.identities
            .entry(path.to_path_buf())
            .or_insert_with(|| directory_identity(path))
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    handle: Arc<same_file::Handle>,
}

fn directory_identity(path: &Path) -> Option<DirectoryIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        Some(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    Some(DirectoryIdentity {
        handle: Arc::new(same_file::Handle::from_path(path).ok()?),
    })
}

/// `path` expressed in the directory spelling used by a walk of `base`.
fn walk_spelling(path: &Path, base: &Path) -> PathBuf {
    let mut aliases = DirectoryAliases::default();
    base.ancestors()
        .find_map(|ancestor| aliases.respell(path, ancestor))
        .unwrap_or_else(|| path.to_path_buf())
}

/// `path` beneath a base already split into [`fold_name`]-folded components, including its root.
fn relative_to_folded(path: &Path, base: &[String]) -> Option<String> {
    let components: Vec<Component<'_>> = path.components().collect();
    let rest = components.get(base.len()..)?;
    components
        .iter()
        .zip(base)
        .all(|(component, folded)| folded_component(*component) == *folded)
        .then(|| {
            rest.iter()
                .copied()
                .map(Component::as_os_str)
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

fn folded_names(path: &Path) -> Vec<String> {
    names(path)
        .map(|name| fold_name(&name.to_string_lossy()))
        .collect()
}

fn folded_components(path: &Path) -> Vec<String> {
    path.components().map(folded_component).collect()
}

fn folded_component(component: Component<'_>) -> String {
    fold_name(&slash_separated(Path::new(component.as_os_str())))
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
    /// Deny a call that would ask, in a run nobody attends.
    Unapproved,
}

/// A call's [`Decision`] and what settled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) basis: Basis,
}

/// What settled a call's [`Decision`], which the gate logs, a denial names to the model, and an
/// approval prompt shows the user.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A command holding `construct`, which the analysis does not recognize, so no allow rule
    /// covers it. `manual` asks, and `yolo` runs it unless a deny or configured ask rule holds what
    /// ox sees.
    Unrecognized { construct: String },
}

impl Basis {
    /// The sentence an approval prompt shows for a call this basis asks about, or `None` when the
    /// mode asks because no rule settles the call.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Mode(_) => None,
            Self::Unrecognized { .. } => Some(format!(
                "Asks because of {self}, so no permission rule can allow this command"
            )),
            basis => Some(format!("Asks because of {basis}")),
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
            Self::Unrecognized { construct } => {
                write!(f, "{}, which ox cannot analyze", code_span(construct))
            }
        }
    }
}

/// `text` as a Markdown code span, fenced past its longest backtick run and padded if it has one.
fn code_span(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    if longest > 0 {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
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
    /// Whether a person can answer an approval request in this run.
    attended: bool,
}

/// A deny or ask rule, with the shipped group it comes from and where its literal path prefix
/// lands in the policy's workspace.
#[derive(Debug, Clone)]
struct TierRule {
    rule: Rule,
    builtin: Option<Shipped>,
    /// The rule's [`Rule::prefix_bases`] from the workspace's anchor directories.
    bases: Vec<PathBuf>,
}

impl TierRule {
    /// Returns whether this rule holds back a call in `class` with `target`.
    fn matches(&self, class: PermissionClass, target: &Access) -> bool {
        self.rule.matches(class, target, self.discipline())
    }

    /// Returns whether this rule holds back an access of `path` in `class`.
    fn matches_path(&self, class: PermissionClass, path: &PathTarget) -> bool {
        self.rule.matches_path(class, path, self.discipline())
    }

    /// Returns whether this `read` rule may hold a read of `path`, or of a file below it when
    /// `recursive`.
    fn holds_read(&self, path: &PathTarget, recursive: bool) -> bool {
        self.rule.holds_read(path, recursive, &self.bases)
    }

    fn discipline(&self) -> MatchDiscipline<'_> {
        MatchDiscipline::Deny(Prefixes::Located(&self.bases))
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
            enabled: Shipped::VARIANTS.to_vec(),
            attended: true,
        }
    }
}

impl Policy {
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
            Shipped::VARIANTS
                .iter()
                .copied()
                .filter(|group| !disabled.contains(group))
        };
        let anchors: Vec<PathBuf> = workspace
            .anchors
            .iter()
            .map(|anchor| anchor.spelled.clone())
            .collect();
        let entry = |rule: Rule, builtin| TierRule {
            bases: rule.prefix_bases(&anchors),
            rule,
            builtin,
        };
        let mut deny: Vec<TierRule> = deny.into_iter().map(|rule| entry(rule, None)).collect();
        let mut ask: Vec<TierRule> = ask.into_iter().map(|rule| entry(rule, None)).collect();
        for group in enabled() {
            if let Some(rules) = group.rules()? {
                let shipped = rules.into_iter().map(|rule| entry(rule, Some(group)));
                match group.tier() {
                    ShippedTier::Deny => deny.extend(shipped),
                    ShippedTier::Ask => ask.extend(shipped),
                }
            }
        }
        Ok(Self {
            mode,
            deny,
            ask,
            allow,
            session_allow: Vec::new(),
            enabled: enabled().collect(),
            attended: true,
        })
    }

    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) const fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub(crate) const fn attended(&self) -> bool {
        self.attended
    }

    /// Marks the run as one nobody can answer an approval request in, so each call that would ask
    /// is [unapproved](Decision::Unapproved).
    pub(crate) const fn set_unattended(&mut self) {
        self.attended = false;
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
        let suggested = Rule::for_call(class, target)?;
        let mut trial = self.clone();
        trial.session_allow.push(suggested.clone());
        (trial.decide(class, target) == Decision::Allow).then(|| SessionGrant {
            class,
            target: target.clone(),
            suggested,
        })
    }

    /// The user config change that lets a call through in a run nobody attends. `None` when
    /// something else would still hold the call, or when the group that holds it guards secrets.
    pub(crate) fn config_fix(&self, class: PermissionClass, target: &Access) -> Option<ConfigFix> {
        let Basis::Builtin(group) = self.verdict(class, target).basis else {
            return self
                .session_grant(class, target)
                .map(|grant| ConfigFix::Allow(grant.suggested));
        };
        if group.tier() == ShippedTier::Deny {
            return None;
        }
        let mut trial = self.clone();
        trial.enabled.retain(|enabled| *enabled != group);
        trial.ask.retain(|entry| entry.builtin != Some(group));
        if trial.decide(class, target) == Decision::Allow {
            return Some(ConfigFix::Disable { group, allow: None });
        }
        trial
            .session_grant(class, target)
            .map(|grant| ConfigFix::Disable {
                group,
                allow: Some(grant.suggested),
            })
    }

    /// The `read` deny and ask rules for files below a search tool's root.
    pub(crate) fn read_filter(&self) -> ReadFilter {
        ReadFilter(
            self.deny
                .iter()
                .chain(&self.ask)
                .filter(|entry| entry.rule.class() == PermissionClass::Read)
                .cloned()
                .collect(),
        )
    }

    /// Resolves a call to a [`Decision`], as [`Self::verdict`] does.
    pub(crate) fn decide(&self, class: PermissionClass, target: &Access) -> Decision {
        self.verdict(class, target).decision
    }

    /// Resolves a call to a [`Verdict`] without I/O.
    ///
    /// Order, stopping at the first match:
    /// 1. A deny rule denies, in every mode.
    /// 2. `plan` denies an edit, exec, or net call.
    /// 3. Outside `yolo`, a call [`Self::asks`] holds asks. In `yolo`, a call a configured ask rule
    ///    holds asks.
    /// 4. A read-class call allows.
    /// 5. An edit-class call inside a workspace root allows.
    /// 6. An allow rule or a session allow rule allows.
    /// 7. `yolo` allows, and the other modes ask.
    ///
    /// A `bash` command is denied when a deny rule matches one of its simple commands, an edit of a
    /// file it writes, or a read of a file it reads for certain, such as an input redirection or a
    /// reader's literal file argument. It asks at step 3 in each case [`Self::asks`] lists, and
    /// step 6 allows it only when the analysis recognizes it and [`Self::allows_command`] holds.
    ///
    /// A call that would ask is [unapproved](Decision::Unapproved) in a run nobody attends.
    pub(crate) fn verdict(&self, class: PermissionClass, target: &Access) -> Verdict {
        let verdict = self.settle(class, target);
        if verdict.decision == Decision::Ask && !self.attended {
            return Verdict {
                decision: Decision::Unapproved,
                ..verdict
            };
        }
        verdict
    }

    /// The verdict of the pipeline [`Self::verdict`] lists, as if a person attended the run.
    fn settle(&self, class: PermissionClass, target: &Access) -> Verdict {
        let verdict = |decision, basis| Verdict { decision, basis };
        if let Some(rule) = held_by(&self.deny.iter(), class, target) {
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

        if self.mode == Mode::Yolo {
            let configured = self.ask.iter().filter(|entry| entry.builtin.is_none());
            if let Some(rule) = held_by(&configured, class, target) {
                return verdict(Decision::Ask, rule.basis(Tier::Ask));
            }
        } else if let Some(basis) = self.asks(class, target) {
            return verdict(Decision::Ask, basis);
        }

        if class == PermissionClass::Read {
            return verdict(Decision::Allow, Basis::Read);
        }
        if class == PermissionClass::Edit
            && let Access::Path(path) = target
            && path.zone == Zone::Workspace
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
    /// edits or a command writes, a command the analysis does not recognize, the [`read`
    /// rule](Self::possible_read_holder) for a file a reader may read, or the `rm` group for a
    /// [forced recursive delete](Segment::removes_recursively_by_force).
    fn asks(&self, class: PermissionClass, target: &Access) -> Option<Basis> {
        if let Some(rule) = held_by(&self.ask.iter(), class, target) {
            return Some(rule.basis(Tier::Ask));
        }
        if class == PermissionClass::Edit
            && let Access::Path(path) = target
            && let Some(group) = self.protected_group(path)
        {
            return Some(Basis::Builtin(group));
        }
        let Access::Command(command) = target else {
            return None;
        };
        if let Some(construct) = command.unrecognized() {
            return Some(Basis::Unrecognized {
                construct: construct.to_owned(),
            });
        }
        command
            .writes()
            .iter()
            .find_map(|write| self.protected_group(write))
            .map(Basis::Builtin)
            .or_else(|| {
                command
                    .segments()
                    .iter()
                    .find_map(|segment| self.possible_read_holder(segment))
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

    /// The first deny or ask `read` rule that may match a file `segment` may read beyond those the
    /// command reads for certain, or the first such rule when the segment may read any file.
    fn possible_read_holder(&self, segment: &Segment) -> Option<Basis> {
        let rules = self
            .deny
            .iter()
            .map(|rule| (rule, Tier::Deny))
            .chain(self.ask.iter().map(|rule| (rule, Tier::Ask)))
            .filter(|(rule, _)| rule.rule.class() == PermissionClass::Read);
        segment
            .reads()
            .iter()
            .find_map(|read| {
                rules
                    .clone()
                    .find(|(entry, _)| entry.holds_read(&read.path, read.recursive))
            })
            .or_else(|| {
                segment
                    .reads_anything()
                    .then(|| rules.clone().next())
                    .flatten()
            })
            .map(|(rule, tier)| rule.basis(tier))
    }

    /// The enabled protected group that holds `path`, if one does.
    fn protected_group(&self, path: &PathTarget) -> Option<Shipped> {
        path.protected
            .iter()
            .copied()
            .find(|group| self.enabled.contains(group))
    }

    /// The allow rule or session allow rule that matches the call, or for a command what lets each
    /// of its parts through.
    fn allows(&self, class: PermissionClass, target: &Access) -> Option<Basis> {
        match target {
            Access::Command(command) => self.allows_command(command),
            _ => self.allow_rule(|rule| rule.matches(class, target, MatchDiscipline::Allow)),
        }
    }

    /// What lets every part of a command through, when each does: the analysis recognizes it, it
    /// runs at least one simple command, each simple command is allowed by a rule or is a
    /// known-safe read, and each file it writes is inside a workspace root or allowed for editing.
    fn allows_command(&self, command: &CommandTarget) -> Option<Basis> {
        if command.unrecognized().is_some() || command.segments().is_empty() {
            return None;
        }
        let segments = command.segments().iter().map(|segment| {
            self.allow_rule(|rule| rule.allows(segment))
                .or_else(|| segment.is_safe().then_some(Basis::SafeRead))
        });
        let writes = command.writes().iter().map(|write| {
            if write.zone == Zone::Workspace {
                Some(Basis::Workspace)
            } else {
                self.allow_rule(|rule| {
                    rule.matches_path(PermissionClass::Edit, write, MatchDiscipline::Allow)
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

/// The first of the deny or ask `rules` that matches the call, or, for a command, an edit of a file
/// it writes or a read of a file it reads for certain.
fn held_by<'a>(
    rules: &(impl Iterator<Item = &'a TierRule> + Clone),
    class: PermissionClass,
    target: &Access,
) -> Option<&'a TierRule> {
    rules
        .clone()
        .find(|entry| entry.matches(class, target))
        .or_else(|| {
            let Access::Command(command) = target else {
                return None;
            };
            let path_held_by =
                |class, path| rules.clone().find(|entry| entry.matches_path(class, path));
            command
                .writes()
                .iter()
                .find_map(|write| path_held_by(PermissionClass::Edit, write))
                .or_else(|| {
                    command
                        .reads()
                        .iter()
                        .find_map(|read| path_held_by(PermissionClass::Read, read))
                })
        })
}

/// An approval's "allow for this session" offer: the suggested rule, plus the call that any edit of
/// it must keep covering.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            rule.matches(self.class, &self.target, MatchDiscipline::Allow),
            "the pattern no longer covers this call"
        );
        Ok(rule)
    }
}

/// A user config change that lets a held call through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigFix {
    /// An allow rule to add.
    Allow(Rule),
    /// A shipped group to turn off under `[permission.defaults]`, with the allow rule the call
    /// would then still need.
    Disable { group: Shipped, allow: Option<Rule> },
}

/// The `read` deny and ask rules a search tool applies to each file it finds, since the gate
/// checked only the search root and a walk cannot ask about each file. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub(crate) struct ReadFilter(Arc<[TierRule]>);

impl ReadFilter {
    /// The filter for one walk of `base`, the resolved search root, which the call spelled as
    /// `requested` relative to `workspace`'s cwd.
    pub(crate) fn walk(&self, base: &Path, requested: &Path, workspace: &Workspace) -> WalkFilter {
        let requested = lexical_normalize(&workspace.cwd().join(requested));
        WalkFilter {
            rules: self
                .0
                .iter()
                .map(|entry| WalkRule {
                    rule: entry.rule.clone(),
                    bases: entry
                        .bases
                        .iter()
                        .map(|prefix| {
                            let resolved = resolve(prefix).unwrap_or_else(|_| prefix.clone());
                            folded_components(&walk_spelling(&resolved, base))
                        })
                        .collect(),
                })
                .collect(),
            base: base.to_path_buf(),
            requested_base: (requested != base).then_some(requested),
            folded_anchors: workspace
                .anchors
                .iter()
                .flat_map(|anchor| [&anchor.resolved, &anchor.spelled])
                .filter(|base| base.is_absolute())
                .flat_map(|anchor| {
                    [
                        folded_components(anchor),
                        folded_components(&walk_spelling(anchor, base)),
                    ]
                })
                .collect(),
        }
    }
}

/// [`ReadFilter`] bound to one walk, with the spellings every file of the walk shares.
pub(crate) struct WalkFilter {
    rules: Vec<WalkRule>,
    base: PathBuf,
    /// The base as the call spelled it, when that differs from `base`.
    requested_base: Option<PathBuf>,
    /// Each spelling of each anchor directory, split into [`fold_name`]-folded components.
    folded_anchors: Vec<Vec<String>>,
}

impl WalkFilter {
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

        let canonical = path.to_string_lossy().into_owned();
        // Deny matching tries every `deny_rooted` spelling, which covers `rooted`.
        let target = PathTarget {
            requested: requested.map_or_else(
                || canonical.clone(),
                |requested| requested.to_string_lossy().into_owned(),
            ),
            canonical,
            exact: None,
            relative: None,
            rooted: Vec::new(),
            deny_rooted,
            zone: Zone::Outside,
            protected: Vec::new(),
        };
        self.rules.iter().any(|entry| {
            let discipline = MatchDiscipline::Deny(Prefixes::Walk(&entry.bases));
            entry
                .rule
                .matches_path(PermissionClass::Read, &target, discipline)
        })
    }
}

/// A `read` rule of a [`WalkFilter`]. Walks follow no symlink, so the rule's prefix locations are
/// resolved once, then spelled under the walk's base like every file it finds.
struct WalkRule {
    rule: Rule,
    bases: Vec<Vec<String>>,
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
    use crate::permission::rule::escaped;
    use crate::session::store::SessionStore;
    use crate::util::fs::testing::{symlink_dir, symlink_file};
    use crate::util::path::local_testing::{abs, canonical_tempdir};
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

    /// A policy in `/repo` with the given tiers and the shipped groups that hold no patterns.
    fn policy(
        mode: Mode,
        deny: &[(PermissionClass, &str)],
        ask: &[(PermissionClass, &str)],
        allow: &[(PermissionClass, &str)],
    ) -> Policy {
        policy_in(
            mode,
            deny,
            ask,
            allow,
            &Workspace::new(Path::new(&abs("/repo"))),
        )
    }

    /// A [`policy`] in `workspace`.
    fn policy_in(
        mode: Mode,
        deny: &[(PermissionClass, &str)],
        ask: &[(PermissionClass, &str)],
        allow: &[(PermissionClass, &str)],
        workspace: &Workspace,
    ) -> Policy {
        resolved_policy(
            mode,
            deny,
            ask,
            allow,
            &[Shipped::Devices, Shipped::Ssh, Shipped::Credentials],
            workspace,
        )
    }

    /// [`Policy::resolve`] in `workspace`, with the shipped defaults in `disabled` off.
    fn resolved_policy(
        mode: Mode,
        deny: &[(PermissionClass, &str)],
        ask: &[(PermissionClass, &str)],
        allow: &[(PermissionClass, &str)],
        disabled: &[Shipped],
        workspace: &Workspace,
    ) -> Policy {
        Policy::resolve(
            mode,
            rules(deny),
            rules(ask),
            rules(allow),
            disabled,
            workspace,
        )
        .unwrap()
    }

    fn command(s: &str) -> Access {
        Access::for_command(s, &Workspace::new(Path::new("/repo")))
    }

    /// A path target at `canonical`, spelled the same way, rooted at `relative` when given.
    fn path(canonical: &str, relative: Option<&str>, zone: Zone, protected: &[Shipped]) -> Access {
        let rooted: Vec<String> = relative.map(str::to_owned).into_iter().collect();
        Access::Path(PathTarget {
            canonical: canonical.to_owned(),
            exact: None,
            relative: relative.map(str::to_owned),
            rooted: rooted.clone(),
            requested: canonical.to_owned(),
            deny_rooted: rooted,
            zone,
            protected: protected.to_vec(),
        })
    }

    fn workspace(canonical: &str, relative: &str) -> Access {
        path(canonical, Some(relative), Zone::Workspace, &[])
    }

    fn outside(canonical: &str) -> Access {
        path(canonical, None, Zone::Outside, &[])
    }

    /// How `policy` settles running `command` in `workspace`.
    fn decide_command(policy: &Policy, command: &str, workspace: &Workspace) -> Decision {
        policy.decide(
            PermissionClass::Exec,
            &Access::for_command(command, workspace),
        )
    }

    /// The canonical and relative spellings as paths, which compare equal across separators.
    fn resolved(target: &Access) -> (&Path, Option<&Path>, Zone) {
        let Access::Path(PathTarget {
            canonical,
            relative,
            zone,
            ..
        }) = target
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
        let Access::Path(PathTarget { protected, .. }) = target else {
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

        #[test]
        fn round_trips_through_serde_and_fromstr() {
            for &mode in Mode::VARIANTS {
                let token = mode.to_string();
                assert_eq!(serde_json::to_value(mode).unwrap(), token);
                assert_eq!(token.parse::<Mode>().unwrap(), mode);
            }
        }

        mod from_str {
            use super::*;

            #[test]
            fn names_every_valid_value_for_an_unknown_one() {
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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                let base_raw = slash_separated(base);
                let home_raw = slash_separated(&home);

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for (raw, expected) in [
                        (
                            "shared",
                            r#"invalid extra root "shared": expected an absolute or `~/` path"#
                                .to_owned(),
                        ),
                        (home_raw.as_str(), too_broad(&home_raw)),
                        (fs_root.as_str(), too_broad(&fs_root)),
                        (base_raw.as_str(), too_broad(&base_raw)),
                    ] {
                        let err = Workspace::resolve(&repo, &[raw.to_owned()]).unwrap_err();
                        assert_eq!(format!("{err:#}"), expected, "{raw}");
                    }
                });
            }

            #[test]
            fn rejects_an_extra_root_through_a_symlink_loop() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let looped = base.join("loop");
                symlink_file(&looped, &looped);
                let raw = format!("{}/inner", looped.display());

                let err = Workspace::resolve(base, std::slice::from_ref(&raw)).unwrap_err();

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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let notes = base.join("notes");
                std::fs::write(&notes, "").unwrap();
                let raw = notes.join("inner").display().to_string();

                let err = Workspace::resolve(base, std::slice::from_ref(&raw)).unwrap_err();

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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                        format!("{err:#}"),
                        r#"invalid extra root "shared": expected an absolute or `~/` path"#,
                    );
                });
            }

            #[test]
            fn rejects_home_and_its_parent_under_another_name() {
                // A symlink to `$HOME` resolves to it. An alias `resolve` keeps, such as `up` here,
                // still reaches `$HOME` through the trailing names, so it is caught too.
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let (repo, home) = (base.join("repo"), base.join("home"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&home).unwrap();
                symlink_dir(&home, &base.join("home-link"));
                symlink_dir(base, &base.join("up"));
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

        mod for_command {
            use super::*;

            #[test]
            fn reads_a_quoted_tilde_under_the_cwd_as_bash_does() {
                let base_dir = canonical_tempdir();
                let root = base_dir.path().to_path_buf();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(cwd.join("~")).unwrap();
                std::fs::create_dir(&home).unwrap();
                std::fs::write(cwd.join(".env"), "").unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    let policy = Policy::resolve(
                        Mode::Manual,
                        rules(&[(READ, ".env")]),
                        Vec::new(),
                        Vec::new(),
                        &[],
                        &workspace,
                    )
                    .unwrap();
                    let run = |command: &str| {
                        policy.decide(EXEC, &Access::for_command(command, &workspace))
                    };

                    assert_eq!(run(r#"cat "~/../.env""#), Decision::Deny);
                    assert_eq!(run(r#"cat < "~/../.env""#), Decision::Deny);
                });
            }
        }

        mod for_path {
            use super::*;

            #[test]
            fn inside_an_extra_root_is_a_workspace_target_rooted_there() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                    let Access::Path(PathTarget { rooted: actual, .. }) = &target else {
                        unreachable!();
                    };
                    let actual: Vec<&Path> = actual.iter().map(Path::new).collect();
                    let rooted: Vec<&Path> = rooted.iter().map(Path::new).collect();
                    assert_eq!(actual, rooted, "{raw}");
                }
            }

            #[test]
            fn inside_cwd_is_a_workspace_target() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::write(cwd.join("a.rs"), "x").unwrap();

                let target = Access::for_path("a.rs", &Workspace::new(cwd));
                let (canonical, relative, zone) = resolved(&target);
                assert_eq!(canonical, cwd.join("a.rs"));
                assert_eq!(relative, Some(Path::new("a.rs")));
                assert_eq!(zone, Zone::Workspace);
                assert_eq!(protected(&target), []);
            }

            #[test]
            fn brand_new_file_is_still_a_workspace_target() {
                // A new file cannot canonicalize, but its missing tail still lands inside cwd.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let target = Access::for_path("new/file.rs", &Workspace::new(cwd));
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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let target = Access::for_path("/etc/hosts", &Workspace::new(cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!((relative, zone), (None, Zone::Outside));
            }

            #[test]
            fn parent_traversal_is_resolved_before_the_cwd_test() {
                // Both shapes must leave cwd. In the second, `create_dir_all` builds `missing` and
                // then steps back out through it.
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let target = Access::for_path(".vscode/tasks.json", &Workspace::new(cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!(relative, Some(Path::new(".vscode/tasks.json")));
                assert_eq!(zone, Zone::Workspace);
                assert_eq!(protected(&target), [Shipped::Editors]);
            }

            #[test]
            fn app_dirs_belong_to_the_ox_group() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                temp_env::with_vars(
                    [
                        ("XDG_CONFIG_HOME", Some(root.join("config"))),
                        ("XDG_DATA_HOME", Some(root.join("data"))),
                        ("XDG_STATE_HOME", Some(root.join("state"))),
                    ],
                    || {
                        for raw in [
                            "config/ox/config.toml",
                            "data/ox/sessions.db",
                            "state/ox/log/oxide-code.log",
                        ] {
                            let target = Access::for_path(raw, &Workspace::new(root));
                            assert_eq!(protected(&target), [Shipped::Ox], "{raw}");
                        }
                        let sibling =
                            Access::for_path("config/other/x.toml", &Workspace::new(root));
                        assert_eq!(protected(&sibling), []);
                    },
                );
            }

            #[test]
            fn cwd_containing_home_is_not_a_workspace() {
                // Launched from `$HOME` or any ancestor of it, "inside cwd" spans every dotfile, so
                // edits there must not auto-allow. Anchored patterns still see the path.
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let home = root.join("home");
                std::fs::create_dir(&home).unwrap();
                let fs_root = root.ancestors().last().unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for cwd in [home.as_path(), root, fs_root] {
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
                                policy.decide(EDIT, &target),
                                policy.decide(EDIT, &absolute),
                                "{cwd:?}/{raw}",
                            );
                        }
                    }
                });
            }

            #[test]
            fn deep_existing_file_stays_a_workspace_target() {
                // Windows canonicalizes a path past 260 chars to a `\\?\` spelling the cwd lacks.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let deep: PathBuf = std::iter::repeat_n("d".repeat(50), 6).collect();
                std::fs::create_dir_all(cwd.join(&deep)).unwrap();
                let file = deep.join("a.rs");
                std::fs::write(cwd.join(&file), "").unwrap();

                assert_eq!(
                    resolved(&Access::for_path(
                        file.to_str().unwrap(),
                        &Workspace::new(cwd)
                    )),
                    (&*cwd.join(&file), Some(&*file), Zone::Workspace),
                );
            }

            #[test]
            fn hard_linked_file_is_not_a_workspace_target() {
                // The second link sits outside cwd, so an edit through `a.rs` would change it too.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                std::fs::write(outside.join("a.rs"), "x").unwrap();
                std::fs::hard_link(outside.join("a.rs"), cwd.join("a.rs")).unwrap();
                std::fs::write(cwd.join("b.rs"), "x").unwrap();

                assert_eq!(
                    resolved(&Access::for_path("a.rs", &Workspace::new(cwd))),
                    (
                        cwd.join("a.rs").as_path(),
                        Some(Path::new("a.rs")),
                        Zone::Outside
                    ),
                );
                assert_eq!(
                    resolved(&Access::for_path("b.rs", &Workspace::new(cwd))).2,
                    Zone::Workspace
                );
            }

            #[test]
            fn new_file_under_a_symlinked_parent_resolves_through_the_link() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                symlink_dir(outside, &cwd.join("link"));

                let target = Access::for_path("link/new.rs", &Workspace::new(cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("new.rs").as_path(), None, Zone::Outside),
                );
            }

            #[test]
            fn dangling_symlink_resolves_to_its_missing_target() {
                // `canonicalize` fails on the dangling `notes.md -> ../escape.txt`, yet `fs::write`
                // follows it, so the gate must follow it too.
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
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
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                symlink_dir(outside, &cwd.join("link"));
                std::fs::create_dir_all(cwd.join(".git/hooks")).unwrap();
                symlink_dir(&cwd.join(".git/hooks"), &cwd.join("tools"));

                let target = Access::for_path("missing/../link/x", &Workspace::new(cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("x").as_path(), None, Zone::Outside),
                );

                let target = Access::for_path("x/../tools/pre-commit", &Workspace::new(cwd));
                assert_eq!(protected(&target), [Shipped::Git]);
                let p = resolved_policy(
                    Mode::Manual,
                    &[],
                    &[],
                    &[(EDIT, "*")],
                    &[],
                    &Workspace::new(cwd),
                );
                assert_eq!(p.decide(EDIT, &target), Decision::Ask);
            }

            #[test]
            fn dangling_symlink_target_with_a_missing_dot_dot_is_walked_component_by_component() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                symlink_dir(outside, &cwd.join("out"));
                symlink_file(Path::new("missing/../out/new"), &cwd.join("notes.md"));

                let target = Access::for_path("notes.md", &Workspace::new(cwd));
                assert_eq!(
                    resolved(&target),
                    (outside.join("new").as_path(), None, Zone::Outside),
                );
            }

            #[test]
            fn symlinked_protected_name_protects_both_spellings() {
                // `.vscode -> tools/vscode` at the top and `sub/.idea -> ../tools/idea` deeper: the
                // link target named directly and the nested link named as spelled must both ask.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
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
                    let target = Access::for_path(raw, &Workspace::new(cwd));
                    assert_eq!(protected(&target), [Shipped::Editors], "{raw}");
                }
                let sibling = Access::for_path("tools/other.rs", &Workspace::new(cwd));
                assert_eq!(protected(&sibling), []);
            }

            #[test]
            fn symlinked_protected_name_in_an_extra_root_protects_its_target() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                symlink_file(Path::new("b"), &cwd.join("a"));
                symlink_file(Path::new("a"), &cwd.join("b"));

                let target = Access::for_path("a/new.rs", &Workspace::new(cwd));
                let (_, relative, zone) = resolved(&target);
                assert_eq!((relative, zone), (None, Zone::Outside));
            }

            #[test]
            fn past_a_file_or_an_overlong_name_keeps_the_joined_spelling() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::write(cwd.join("notes"), "").unwrap();

                for path in [
                    Path::new("notes").join("sub"),
                    PathBuf::from("a".repeat(256)),
                ] {
                    let target = Access::for_path(path.to_str().unwrap(), &Workspace::new(cwd));

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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                symlink_dir(outside, &cwd.join("link"));
                std::fs::write(cwd.join("notes"), "").unwrap();

                for path in ["notes/../x", "notes/sub/../../link/x"] {
                    let target = Access::for_path(path, &Workspace::new(cwd));

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
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                std::fs::write(cwd.join(r".git\config"), "").unwrap();
                std::fs::create_dir(cwd.join("secrets-store")).unwrap();
                std::fs::write(cwd.join(r"secrets-store\key"), "").unwrap();

                let target =
                    Access::for_path(r".git::$INDEX_ALLOCATION\config", &Workspace::new(cwd));
                let (path, relative, _) = resolved(&target);
                assert_eq!(path.as_os_str(), cwd.join(r".git\config").as_os_str());
                assert_eq!(relative, Some(Path::new(r".git\config")));
                assert_eq!(protected(&target), [Shipped::Git]);
                let p = policy_in(
                    Mode::Manual,
                    &[(EDIT, "secrets-store/key")],
                    &[],
                    &[],
                    &Workspace::new(cwd),
                );
                let target = Access::for_path(r"secrets-store\key::$DATA", &Workspace::new(cwd));
                assert_eq!(p.decide(EDIT, &target), Decision::Deny);

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
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
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
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
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
            let cwd_dir = canonical_tempdir();
            let cwd = cwd_dir.path();
            std::fs::write(cwd.join("notes"), "original").unwrap();
            let verbatim = std::fs::canonicalize(cwd).unwrap();

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
                (
                    Basis::Unrecognized {
                        construct: "$((1 + 2))".to_owned(),
                    },
                    "`$((1 + 2))`, which ox cannot analyze",
                ),
                (
                    Basis::Unrecognized {
                        construct: "echo `id`".to_owned(),
                    },
                    "`` echo `id` ``, which ox cannot analyze",
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
                    Some("Asks because of the `ask.exec` pattern `git push *`"),
                );
                assert_eq!(
                    Basis::Builtin(Shipped::Git).reason().as_deref(),
                    Some("Asks because of the built-in `git` rule"),
                );
                assert_eq!(Basis::Mode(Mode::Manual).reason(), None);
            }

            #[test]
            fn says_that_no_rule_can_allow_a_command_ox_cannot_analyze() {
                let basis = Basis::Unrecognized {
                    construct: "alias ls=rm".to_owned(),
                };

                assert_eq!(
                    basis.reason().as_deref(),
                    Some(
                        "Asks because of `alias ls=rm`, which ox cannot analyze, \
                         so no permission rule can allow this command"
                    ),
                );
            }
        }
    }

    mod code_span {
        use super::*;

        #[test]
        fn fences_longer_than_the_longest_backtick_run_and_pads_against_the_fence() {
            for (text, span) in [
                ("alias ls=rm", "`alias ls=rm`"),
                ("echo `id`", "`` echo `id` ``"),
                ("echo ``id``", "``` echo ``id`` ```"),
                ("`id`", "`` `id` ``"),
            ] {
                assert_eq!(code_span(text), span, "{text}");
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
            assert_eq!(p.decide(EXEC, &command("cargo test --all")), Decision::Ask);

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
                p.decide(EXEC, &command("cargo test --all")),
                Decision::Allow
            );
            assert_eq!(p.decide(EDIT, &outside(&hosts)), Decision::Allow);

            p.clear_session_allow();
            assert_eq!(p.session_allow(), []);
            assert_eq!(p.decide(EXEC, &command("cargo test --all")), Decision::Ask);
        }

        mod resolve {
            use super::*;

            #[test]
            fn shipped_defaults_each_hold_a_call_they_name_until_disabled() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home, data, config) = (
                    root.join("repo"),
                    root.join("home"),
                    root.join("data"),
                    root.join("config"),
                );
                for dir in [&cwd, &home, &data] {
                    std::fs::create_dir(dir).unwrap();
                }
                let ssh_key = slash_separated(&home.join(".ssh/id_ed25519"));
                let credentials = slash_separated(&data.join("ox/credentials.json"));
                let claude = slash_separated(&home.join(".claude/.credentials.json"));
                let git_store = slash_separated(&home.join(".git-credentials"));
                let xdg_git_store = slash_separated(&config.join("git/credentials"));
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
                    (
                        Shipped::Credentials,
                        READ,
                        outside(&git_store),
                        Decision::Deny,
                    ),
                    (
                        Shipped::Credentials,
                        READ,
                        outside(&xdg_git_store),
                        Decision::Deny,
                    ),
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
                    [
                        (HOME_VAR, Some(&home)),
                        ("XDG_DATA_HOME", Some(&data)),
                        ("XDG_CONFIG_HOME", Some(&config)),
                        ("CLAUDE_CONFIG_DIR", None),
                    ],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let allow_all = [(READ, "*"), (EDIT, "*"), (EXEC, "*")];
                        for (group, class, target, held) in &cases {
                            let resolve = |disabled: &[Shipped]| {
                                resolved_policy(
                                    Mode::Manual,
                                    &[],
                                    &[],
                                    &allow_all,
                                    disabled,
                                    &workspace,
                                )
                            };
                            assert_eq!(resolve(&[]).decide(*class, target), *held, "{group:?}");
                            assert_eq!(
                                resolve(&[*group]).decide(*class, target),
                                Decision::Allow,
                                "{group:?} off"
                            );
                        }
                    },
                );
            }

            #[test]
            fn protects_the_user_git_ssh_cloud_and_claude_code_config_until_disabled() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home, config, claude) = (
                    root.join("repo"),
                    root.join("home"),
                    root.join("config"),
                    root.join("claude-work"),
                );
                std::fs::create_dir(&cwd).unwrap();
                temp_env::with_vars(
                    [
                        (HOME_VAR, Some(&home)),
                        ("XDG_CONFIG_HOME", Some(&config)),
                        ("CLAUDE_CONFIG_DIR", Some(&claude)),
                    ],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let resolve = |disabled: &[Shipped]| {
                            resolved_policy(
                                Mode::Manual,
                                &[],
                                &[],
                                &[(EDIT, "*")],
                                disabled,
                                &workspace,
                            )
                        };
                        let edit =
                            |path: PathBuf| Access::for_path(slash_separated(&path), &workspace);

                        for (path, group) in [
                            (config.join("git/config"), Shipped::Git),
                            (config.join("git/credentials"), Shipped::Git),
                            (home.join(".git-credentials"), Shipped::Git),
                            (home.join(".ssh/config"), Shipped::Ssh),
                            (home.join(".ssh/authorized_keys"), Shipped::Ssh),
                            (home.join(".ssh/authorized_keys2"), Shipped::Ssh),
                            (home.join(".ssh/rc"), Shipped::Ssh),
                            (home.join(".ssh/environment"), Shipped::Ssh),
                            (home.join(".aws/config"), Shipped::Cloud),
                            (home.join(".azure/config"), Shipped::Cloud),
                            (
                                home.join(".config/gcloud/configurations/config_default"),
                                Shipped::Cloud,
                            ),
                            (home.join(".docker/config.json"), Shipped::Cloud),
                            (home.join(".kube/config"), Shipped::Cloud),
                            (claude.join("settings.json"), Shipped::Agents),
                            (claude.join("hooks/pre.sh"), Shipped::Agents),
                        ] {
                            let target = edit(path.clone());
                            assert_eq!(
                                resolve(&[]).decide(EDIT, &target),
                                Decision::Ask,
                                "{path:?}"
                            );
                            assert_eq!(
                                resolve(&[group]).decide(EDIT, &target),
                                Decision::Allow,
                                "{path:?} with {group} off"
                            );
                        }
                        let append = Access::for_command(
                            &format!(
                                "echo key >> {}",
                                slash_separated(&home.join(".ssh/authorized_keys2"))
                            ),
                            &workspace,
                        );
                        assert_eq!(resolve(&[]).decide(EXEC, &append), Decision::Ask);
                        let notes = edit(home.join("notes.txt"));
                        assert_eq!(resolve(&[]).decide(EDIT, &notes), Decision::Allow);
                    },
                );
            }

            #[test]
            fn disables_one_protected_group_and_keeps_the_rest() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let workspace = Workspace::new(cwd);
                let p = resolved_policy(Mode::Manual, &[], &[], &[], &[Shipped::Cargo], &workspace);

                let cargo = Access::for_path(".cargo/config.toml", &workspace);
                let git = Access::for_path(".git/config", &workspace);
                assert_eq!(p.decide(EDIT, &cargo), Decision::Allow);
                assert_eq!(p.decide(EDIT, &git), Decision::Ask);
                assert_eq!(
                    p.decide(
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
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
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
                    &[],
                    &Workspace::new(&cwd),
                );
                for (raw, decision) in [
                    ("../vault/key", Decision::Deny),
                    ("../real/x", Decision::Ask),
                    ("../other/x", Decision::Allow),
                ] {
                    let target = Access::for_path(raw, &Workspace::new(&cwd));
                    assert_eq!(p.decide(READ, &target), decision, "{raw}");
                }
            }

            #[test]
            fn escaped_literal_directories_keep_symlink_targets_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                std::fs::create_dir(root.join("real")).unwrap();
                let workspace = Workspace::new(root);
                for name in [
                    "secrets[1]",
                    "secrets{one}",
                    #[cfg(unix)]
                    "secrets*",
                    #[cfg(unix)]
                    r"secrets\vault",
                ] {
                    symlink_dir(Path::new("real"), &root.join(name));
                    let pattern = format!("./{}/**", escaped(Path::new(name)));

                    let p =
                        resolved_policy(Mode::Yolo, &[(READ, &pattern)], &[], &[], &[], &workspace);
                    let target = Access::for_path("real/key", &workspace);

                    assert_eq!(p.decide(READ, &target), Decision::Deny, "{name}");
                    assert!(
                        p.read_filter()
                            .walk(root, root, &workspace)
                            .withholds(&root.join("real/key")),
                        "{name}"
                    );
                }
            }

            #[test]
            fn literal_separators_keep_symlink_targets_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                std::fs::create_dir(root.join("real")).unwrap();
                symlink_dir(Path::new("real"), &root.join("secrets"));
                let workspace = Workspace::new(root);
                for pattern in [
                    "./secrets[/]key",
                    "./{secrets/key}",
                    #[cfg(unix)]
                    r"./secrets\/key",
                ] {
                    let p =
                        resolved_policy(Mode::Yolo, &[(READ, pattern)], &[], &[], &[], &workspace);
                    let filter = p.read_filter();
                    let walk = filter.walk(root, root, &workspace);

                    for (name, expected) in [
                        ("real/key", Decision::Deny),
                        ("real/other", Decision::Allow),
                    ] {
                        let target = Access::for_path(name, &workspace);

                        assert_eq!(p.decide(READ, &target), expected, "{pattern}: {name}");
                        assert_eq!(
                            walk.withholds(&root.join(name)),
                            expected == Decision::Deny,
                            "{pattern}: {name}"
                        );
                    }
                }
            }

            #[test]
            fn a_decoded_absolute_prefix_stays_within_an_anchored_patterns_scope() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                for name in ["work", "vault"] {
                    std::fs::create_dir(root.join(name)).unwrap();
                }
                symlink_dir(Path::new("vault"), &root.join("alias"));
                let workspace = Workspace::new(&root.join("work"));
                let pattern = format!("./{{{}}}/**", escaped(&root.join("alias")));
                let p = resolved_policy(Mode::Yolo, &[(READ, &pattern)], &[], &[], &[], &workspace);

                let target = Access::for_path(root.join("vault/key"), &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Allow);
                assert!(
                    !p.read_filter()
                        .walk(&root.join("vault"), &root.join("vault"), &workspace)
                        .withholds(&root.join("vault/key"))
                );
            }

            #[test]
            fn escaped_home_directories_keep_shipped_symlink_targets_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let home = root.join("home[1]");
                std::fs::create_dir(&home).unwrap();
                std::fs::create_dir(root.join("vault")).unwrap();
                symlink_dir(Path::new("../vault"), &home.join(".ssh"));

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(root);
                    let p = resolved_policy(Mode::Yolo, &[], &[], &[], &[], &workspace);
                    let target = Access::for_path("vault/key", &workspace);

                    assert_eq!(p.decide(READ, &target), Decision::Deny);
                });
            }

            #[test]
            fn a_replaced_literal_directory_keeps_old_and_current_targets_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                for name in ["first", "second"] {
                    std::fs::create_dir(root.join(name)).unwrap();
                }
                let link = root.join("secrets[1]");
                symlink_dir(Path::new("first"), &link);
                let pattern = format!("./{}/**", globset::escape("secrets[1]"));
                let p = resolved_policy(Mode::Yolo, &[(READ, &pattern)], &[], &[], &[], &workspace);

                #[cfg(unix)]
                std::fs::remove_file(&link).unwrap();
                #[cfg(windows)]
                std::fs::remove_dir(&link).unwrap();
                symlink_dir(Path::new("second"), &link);

                for name in ["first/key", "second/key"] {
                    let target = Access::for_path(name, &workspace);

                    assert_eq!(p.decide(READ, &target), Decision::Deny, "{name}");
                    assert!(
                        p.read_filter()
                            .walk(root, root, &workspace)
                            .withholds(&root.join(name)),
                        "{name}"
                    );
                }
            }

            #[test]
            fn a_literal_directory_created_after_policy_load_is_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                std::fs::create_dir(root.join("real")).unwrap();
                let p = resolved_policy(
                    Mode::Yolo,
                    &[(READ, "./secrets/**")],
                    &[],
                    &[],
                    &[],
                    &workspace,
                );

                symlink_dir(Path::new("real"), &root.join("secrets"));
                let target = Access::for_path("real/key", &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Deny);
                assert!(
                    p.read_filter()
                        .walk(root, root, &workspace)
                        .withholds(&root.join("real/key"))
                );
            }

            #[test]
            fn a_literal_file_link_created_after_policy_load_is_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                std::fs::write(root.join("real"), "fixture").unwrap();
                let p = resolved_policy(Mode::Yolo, &[(READ, "./key")], &[], &[], &[], &workspace);

                symlink_file(Path::new("real"), &root.join("key"));
                let target = Access::for_path("real", &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Deny);
                assert!(
                    p.read_filter()
                        .walk(root, root, &workspace)
                        .withholds(&root.join("real"))
                );
            }

            #[test]
            fn a_literal_dangling_directory_link_created_after_policy_load_is_protected() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                let p = resolved_policy(
                    Mode::Yolo,
                    &[(EDIT, "./secrets/key")],
                    &[],
                    &[],
                    &[],
                    &workspace,
                );

                symlink_dir(Path::new("missing"), &root.join("secrets"));
                let target = Access::for_path("missing/key", &workspace);

                assert_eq!(p.decide(EDIT, &target), Decision::Deny);
            }

            #[cfg(target_os = "macos")]
            #[test]
            fn volume_aliases_keep_absolute_and_anchored_rules_effective() {
                let root_dir = tempfile::tempdir_in("/private/tmp").unwrap();
                let root = root_dir.path();
                std::fs::create_dir(root.join("secrets")).unwrap();
                std::fs::write(root.join("secrets/key"), "fixture").unwrap();
                let alias = Path::new("/System/Volumes/Data").join(root.strip_prefix("/").unwrap());
                assert!(same_file::is_same_file(root, &alias).unwrap());

                for (base, requested) in [(root, alias.as_path()), (alias.as_path(), root)] {
                    let workspace = Workspace::new(base);
                    let absolute = slash_separated(&base.join("secrets"));
                    for pattern in [&absolute, "./secrets"] {
                        for &mode in Mode::VARIANTS {
                            for class in [READ, EDIT] {
                                for (deny, ask, expected) in [
                                    (vec![(class, pattern)], vec![], Decision::Deny),
                                    (
                                        vec![],
                                        vec![(class, pattern)],
                                        if mode == Mode::Plan && class == EDIT {
                                            Decision::Deny
                                        } else {
                                            Decision::Ask
                                        },
                                    ),
                                ] {
                                    let p =
                                        resolved_policy(mode, &deny, &ask, &[], &[], &workspace);
                                    let filter = p.read_filter();
                                    let walk = filter.walk(requested, requested, &workspace);
                                    for name in ["secrets", "secrets/key", "secrets/new/deep/key"] {
                                        let file = requested.join(name);
                                        let target = Access::for_path(&file, &workspace);

                                        assert_eq!(
                                            p.decide(class, &target),
                                            expected,
                                            "{mode} {class} {pattern} {name}"
                                        );
                                        assert_eq!(
                                            walk.withholds(&file),
                                            class == READ,
                                            "{mode} {class} {pattern} {name}"
                                        );
                                    }
                                    assert!(!walk.withholds(&requested.join("secrets-other/key")));
                                }
                            }
                        }
                    }
                }
            }

            #[cfg(target_os = "macos")]
            #[test]
            fn volume_aliases_leave_allow_rules_exact() {
                let root_dir = tempfile::tempdir_in("/private/tmp").unwrap();
                let root = root_dir.path();
                std::fs::create_dir(root.join("repo")).unwrap();
                let workspace = Workspace::new(&root.join("repo"));
                let pattern = slash_separated(&root.join("notes"));
                let alias = Path::new("/System/Volumes/Data").join(root.strip_prefix("/").unwrap());
                let p =
                    resolved_policy(Mode::Manual, &[], &[], &[(EDIT, &pattern)], &[], &workspace);

                let exact = Access::for_path(root.join("notes/key"), &workspace);
                let aliased = Access::for_path(alias.join("notes/key"), &workspace);

                assert_eq!(p.decide(EDIT, &exact), Decision::Allow);
                assert_eq!(p.decide(EDIT, &aliased), Decision::Ask);
            }

            #[cfg(target_os = "macos")]
            #[test]
            fn volume_aliases_keep_requested_dangling_links_denied() {
                let root_dir = tempfile::tempdir_in("/private/tmp").unwrap();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                let pattern = slash_separated(&root.join("secrets"));
                let p = resolved_policy(Mode::Yolo, &[(READ, &pattern)], &[], &[], &[], &workspace);
                let alias = Path::new("/System/Volumes/Data").join(root.strip_prefix("/").unwrap());

                symlink_dir(Path::new("missing"), &root.join("secrets"));
                let target = Access::for_path(alias.join("secrets/key"), &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Deny);
            }

            #[cfg(target_os = "macos")]
            #[test]
            fn volume_aliases_keep_shipped_home_rules_effective() {
                let home_dir = tempfile::tempdir_in("/private/tmp").unwrap();
                let home = home_dir.path();
                std::fs::create_dir(home.join(".ssh")).unwrap();
                let alias = Path::new("/System/Volumes/Data").join(home.strip_prefix("/").unwrap());

                temp_env::with_var(HOME_VAR, Some(home), || {
                    let workspace = Workspace::new(&home.join("repo"));
                    for &mode in Mode::VARIANTS {
                        let p = resolved_policy(mode, &[], &[], &[(EDIT, "*")], &[], &workspace);
                        let target = Access::for_path(alias.join(".ssh/fixture"), &workspace);

                        assert_eq!(p.decide(READ, &target), Decision::Deny, "{mode}");
                        assert_eq!(
                            p.decide(EDIT, &target),
                            if mode == Mode::Plan {
                                Decision::Deny
                            } else if mode == Mode::Yolo {
                                Decision::Allow
                            } else {
                                Decision::Ask
                            },
                            "{mode}"
                        );
                    }
                });
            }

            #[cfg(unix)]
            #[test]
            fn denies_reading_a_process_environment_until_credentials_are_off() {
                let base_dir = canonical_tempdir();
                let cwd = base_dir.path().to_path_buf();
                let workspace = Workspace::new(&cwd);
                let resolve = |disabled: &[Shipped]| {
                    Policy::resolve(
                        Mode::Manual,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        disabled,
                        &workspace,
                    )
                    .unwrap()
                };

                for path in ["/proc/1/environ", "/proc/1/task/2/environ"] {
                    assert_eq!(
                        resolve(&[]).decide(READ, &outside(path)),
                        Decision::Deny,
                        "{path}"
                    );
                    assert_eq!(
                        resolve(&[Shipped::Credentials]).decide(READ, &outside(path)),
                        Decision::Allow,
                        "{path}"
                    );
                }
                assert_eq!(
                    resolve(&[]).decide(READ, &outside("/proc/1/status")),
                    Decision::Allow
                );
            }

            #[cfg(target_os = "linux")]
            #[test]
            fn holds_back_a_process_environment_reached_through_a_link() {
                let base_dir = canonical_tempdir();
                let cwd = base_dir.path().to_path_buf();
                symlink_file(Path::new("/proc/self/environ"), &cwd.join("env"));
                symlink_file(
                    Path::new("/proc/thread-self/environ"),
                    &cwd.join("thread-env"),
                );
                let workspace = Workspace::new(&cwd);
                let p = Policy::resolve(
                    Mode::Manual,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &[],
                    &workspace,
                )
                .unwrap();

                for name in ["env", "thread-env"] {
                    let target = Access::for_path(name, &workspace);
                    assert_eq!(p.decide(READ, &target), Decision::Deny, "{name}");
                }
                let cat = Access::for_command("cat env", &workspace);
                assert_eq!(p.decide(EXEC, &cat), Decision::Deny);
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
                assert_eq!(granted.decide(EXEC, &target), Decision::Allow);
                assert!(
                    p.session_grant(EXEC, &command("cargo test && git push"))
                        .is_none()
                );
            }

            #[test]
            fn suggests_a_rule_that_still_matches_a_word_the_grammar_reads_as_an_operator() {
                let p = policy(Mode::Manual, &[], &[], &[]);

                let grant = p.session_grant(EXEC, &command("cat == x")).unwrap();

                assert_eq!(grant.suggested().pattern(), "cat == x");
            }

            #[test]
            fn declines_a_call_that_no_allow_rule_can_let_through() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                let p = policy_in(
                    Mode::Manual,
                    &[],
                    &[(EXEC, "git push *")],
                    &[],
                    &Workspace::new(cwd),
                );

                let protected = Access::for_path(".git/config", &Workspace::new(cwd));
                assert!(p.session_grant(EDIT, &protected).is_none());
                assert!(p.session_grant(EXEC, &command("git push origin")).is_none());
                assert!(p.session_grant(EXEC, &command("ls *.rs")).is_none());
            }
        }

        mod config_fix {
            use super::*;

            #[test]
            fn turns_off_the_group_alone_when_the_call_then_runs() {
                let p = policy(Mode::Manual, &[], &[], &[(EXEC, "rm *")]);

                let fix = p.config_fix(EXEC, &command("rm -rf build"));

                assert_eq!(
                    fix,
                    Some(ConfigFix::Disable {
                        group: Shipped::Rm,
                        allow: None,
                    }),
                );
            }

            #[test]
            fn adds_the_allow_rule_the_mode_still_needs_once_the_group_is_off() {
                let p = policy(Mode::Manual, &[], &[], &[]);

                let fix = p.config_fix(EXEC, &command("rm -rf build"));

                assert_eq!(
                    fix,
                    Some(ConfigFix::Disable {
                        group: Shipped::Rm,
                        allow: Some(Rule::parse(EXEC, "rm -rf build").unwrap()),
                    }),
                );
            }

            #[test]
            fn suggests_the_allow_rule_for_a_call_no_group_holds() {
                let p = policy(Mode::Manual, &[], &[], &[]);

                let fix = p.config_fix(EXEC, &command("cargo test"));

                assert_eq!(
                    fix,
                    Some(ConfigFix::Allow(Rule::parse(EXEC, "cargo test").unwrap()))
                );
            }

            #[test]
            fn offers_nothing_for_a_group_that_guards_secrets_or_a_call_still_held() {
                temp_env::with_var(HOME_VAR, Some(abs("/home/u")), || {
                    let workspace = Workspace::new(Path::new(&abs("/repo")));
                    let p = resolved_policy(Mode::Manual, &[], &[], &[], &[], &workspace);
                    let held = command("rm -rf build > .git/log");
                    let key = Access::for_path(abs("/home/u/.ssh/id_rsa"), &workspace);
                    let ssh_rc = Access::for_path(abs("/home/u/.ssh/rc"), &workspace);
                    assert_eq!(p.verdict(EXEC, &held).basis, Basis::Builtin(Shipped::Git));
                    assert_eq!(p.verdict(READ, &key).basis, Basis::Builtin(Shipped::Ssh));
                    assert_eq!(p.verdict(EDIT, &ssh_rc).basis, Basis::Builtin(Shipped::Ssh));

                    assert_eq!(p.config_fix(EXEC, &held), None);
                    assert_eq!(p.config_fix(READ, &key), None);
                    assert_eq!(p.config_fix(EDIT, &ssh_rc), None);
                });
            }

            #[cfg(unix)]
            #[test]
            fn drops_the_patterns_of_the_group_it_turns_off() {
                let workspace = Workspace::new(Path::new("/repo"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[],
                    &[],
                    &[(EDIT, "/dev/**")],
                    &[],
                    &workspace,
                );
                let target = Access::for_path("/dev/null", &workspace);
                assert_eq!(
                    p.verdict(EDIT, &target).basis,
                    Basis::Builtin(Shipped::Devices)
                );

                let fix = p.config_fix(EDIT, &target);

                assert_eq!(
                    fix,
                    Some(ConfigFix::Disable {
                        group: Shipped::Devices,
                        allow: None,
                    }),
                );
            }
        }

        mod read_filter {
            use super::*;

            #[test]
            fn a_class_wide_read_hold_filters_every_file() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                let p = resolved_policy(Mode::Yolo, &[(READ, "*")], &[], &[], &[], &workspace);

                let filter = p.read_filter();
                let walk = filter.walk(root, root, &workspace);

                for name in ["src/main.rs", "notes.md"] {
                    assert!(walk.withholds(&root.join(name)), "{name}");
                }
            }

            #[test]
            fn withholds_exactly_the_files_the_gate_holds_back() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let secrets = slash_separated(&cwd.join("secrets"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[(READ, ".env"), (EDIT, "notes.md")],
                    &[(READ, &secrets)],
                    &[],
                    &[],
                    &Workspace::new(cwd),
                );
                let filter = p.read_filter();
                let workspace = Workspace::new(cwd);
                let walk = filter.walk(cwd, Path::new("."), &workspace);

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
                        p.decide(READ, &gate),
                        held.unwrap_or(Decision::Allow),
                        "{name}"
                    );
                }
                let empty = ReadFilter::default();
                assert!(
                    !empty
                        .walk(cwd, Path::new("."), &workspace)
                        .withholds(&cwd.join(".env"))
                );
            }
        }

        mod decide {
            use super::*;

            /// The decision in `mode` for a call only a built-in ask rule holds, which `yolo` runs.
            fn held_by_builtin(mode: Mode) -> Decision {
                if mode == Mode::Yolo {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }

            #[test]
            fn deny_beats_ask_and_ask_beats_allow_in_every_mode() {
                for &mode in Mode::VARIANTS {
                    let p = policy(
                        mode,
                        &[(READ, "secret.txt")],
                        &[(READ, "*.txt")],
                        &[(READ, "*")],
                    );
                    assert_eq!(
                        p.decide(READ, &workspace("/repo/secret.txt", "secret.txt")),
                        Decision::Deny,
                        "{mode}"
                    );
                    assert_eq!(
                        p.decide(READ, &workspace("/repo/notes.txt", "notes.txt")),
                        Decision::Ask,
                        "{mode}"
                    );
                    assert_eq!(
                        p.decide(READ, &workspace("/repo/a.rs", "a.rs")),
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
                    p.decide(EDIT, &workspace("/repo/Cargo.lock", "Cargo.lock")),
                    Decision::Ask
                );
                assert_eq!(
                    p.decide(EXEC, &command("git push origin main")),
                    Decision::Ask
                );
                assert_eq!(p.decide(EXEC, &command("git status")), Decision::Allow);
                assert_eq!(
                    p.decide(EDIT, &workspace("/repo/Cargo.toml", "Cargo.toml")),
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
                    p.decide(READ, &outside(&abs("/usr/share/x"))),
                    Decision::Allow
                );
                assert_eq!(
                    p.decide(READ, &workspace("/repo/.env", ".env")),
                    Decision::Deny
                );
                assert_eq!(p.decide(READ, &outside(&abs("/etc/hosts"))), Decision::Ask);
            }

            #[test]
            fn star_holds_a_call_with_no_target() {
                let p = policy(Mode::Manual, &[(READ, "*")], &[], &[]);
                assert_eq!(p.decide(READ, &Access::None), Decision::Deny);
                let p = policy(Mode::Manual, &[], &[(READ, "*")], &[]);
                assert_eq!(p.decide(READ, &Access::None), Decision::Ask);
                let p = policy(Mode::Manual, &[], &[], &[]);
                assert_eq!(p.decide(READ, &Access::None), Decision::Allow);
            }

            #[test]
            fn protected_edit_asks_under_a_broad_allow_and_runs_in_yolo() {
                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = policy(mode, &[], &[], &[(EDIT, "*")]);
                    let envrc = path(
                        "/repo/.envrc",
                        Some(".envrc"),
                        Zone::Workspace,
                        &[Shipped::Direnv],
                    );
                    assert_eq!(p.decide(EDIT, &envrc), held_by_builtin(mode), "{mode}");
                    assert_eq!(
                        p.decide(READ, &envrc),
                        Decision::Allow,
                        "{mode}: reads pass"
                    );
                }
            }

            #[test]
            fn spilled_tool_output_reads_freely_and_holds_an_edit_outside_yolo() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home, data) = (root.join("repo"), root.join("home"), root.join("data"));
                std::fs::create_dir(&cwd).unwrap();
                temp_env::with_vars(
                    [(HOME_VAR, Some(&home)), ("XDG_DATA_HOME", Some(&data))],
                    || {
                        let workspace = Workspace::new(&cwd);
                        let store = SessionStore::open(&cwd, &cwd).unwrap();
                        let spill = store.artifact_dir("s1").join("tool-results/x.txt");
                        let target = Access::for_path(slash_separated(&spill), &workspace);

                        for mode in [Mode::Manual, Mode::Yolo] {
                            let p =
                                resolved_policy(mode, &[], &[], &[(EDIT, "*")], &[], &workspace);
                            assert_eq!(p.decide(READ, &target), Decision::Allow, "{mode}");
                            assert_eq!(p.decide(EDIT, &target), held_by_builtin(mode), "{mode}");
                        }
                    },
                );
            }

            #[test]
            fn workspace_edit_allows_and_an_outside_edit_needs_an_allow_rule() {
                let p = policy(Mode::Manual, &[], &[], &[(EDIT, &abs("/tmp/**"))]);
                assert_eq!(
                    p.decide(EDIT, &workspace("/repo/new.rs", "new.rs")),
                    Decision::Allow
                );
                assert_eq!(p.decide(EDIT, &outside(&abs("/etc/passwd"))), Decision::Ask);
                assert_eq!(
                    p.decide(EDIT, &outside(&abs("/tmp/scratch.txt"))),
                    Decision::Allow
                );
            }

            #[test]
            fn allow_rule_matches_only_the_spelling_it_names() {
                let p = policy(Mode::Manual, &[], &[], &[(EDIT, &abs("/tmp/notes/**"))]);
                assert_eq!(
                    p.decide(EDIT, &outside(&abs("/tmp/notes/a.md"))),
                    Decision::Allow
                );
                for raw in [
                    "/tmp/NOTES/a.md",
                    "/tmp/notes./a.md",
                    "/tmp/no\u{200b}tes/a.md",
                ] {
                    assert_eq!(p.decide(EDIT, &outside(&abs(raw))), Decision::Ask, "{raw}");
                }
            }

            #[test]
            fn anchored_patterns_apply_below_each_extra_root() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let (repo, shared) = (base.join("repo"), base.join("shared"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&shared).unwrap();
                let workspace = Workspace::resolve(&repo, &[slash_separated(&shared)]).unwrap();
                let p = policy_in(
                    Mode::Manual,
                    &[(READ, "./secrets")],
                    &[(EDIT, "generated/**")],
                    &[],
                    &workspace,
                );

                for (raw, class, decision) in [
                    ("secrets/key", READ, Decision::Deny),
                    ("../shared/secrets/key", READ, Decision::Deny),
                    ("../shared/src/secrets/key", READ, Decision::Allow),
                    ("../shared/generated/x.rs", EDIT, Decision::Ask),
                    ("../shared/src/x.rs", EDIT, Decision::Allow),
                ] {
                    let target = Access::for_path(raw, &workspace);
                    assert_eq!(p.decide(class, &target), decision, "{raw}");
                }
            }

            #[test]
            fn deny_matches_case_normalization_and_ignorable_variants_of_a_name() {
                // A case-insensitive or normalizing file system opens each spelling as the denied
                // file, and an uppercased cwd prefix still names the same directory there.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let p = policy_in(
                    Mode::Manual,
                    &[(READ, ".env"), (READ, "caf\u{e9}.txt")],
                    &[],
                    &[],
                    &Workspace::new(cwd),
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
                    let target = Access::for_path(raw, &Workspace::new(cwd));
                    assert_eq!(p.decide(READ, &target), Decision::Deny, "{raw}");
                }
            }

            #[test]
            fn deny_matches_a_symlinked_path_as_spelled_even_in_yolo() {
                // `.env -> ../shared/.env` resolves outside cwd and `secrets -> ../vault` away from
                // the absolute glob, yet both deny rules must fire on the path the call names.
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let cwd = root.join("repo");
                std::fs::create_dir_all(root.join("shared")).unwrap();
                std::fs::create_dir_all(root.join("vault")).unwrap();
                std::fs::create_dir(&cwd).unwrap();
                std::fs::write(root.join("shared/.env"), "SECRET=1").unwrap();
                symlink_file(Path::new("../shared/.env"), &cwd.join(".env"));
                symlink_dir(Path::new("../vault"), &cwd.join("secrets"));

                let secrets = slash_separated(&cwd.join("secrets"));
                let p = policy_in(
                    Mode::Yolo,
                    &[(READ, ".env"), (EDIT, ".env"), (READ, &secrets)],
                    &[],
                    &[],
                    &Workspace::new(&cwd),
                );
                let env = Access::for_path(".env", &Workspace::new(&cwd));
                assert_eq!(resolved(&env).1, None, "the link resolves outside cwd");
                for class in [READ, EDIT] {
                    assert_eq!(p.decide(class, &env), Decision::Deny, "{class}");
                }
                let key = Access::for_path("secrets/key", &Workspace::new(&cwd));
                assert_eq!(p.decide(READ, &key), Decision::Deny);
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
                    p.decide(READ, &outside(&abs("/etc/hosts"))),
                    Decision::Allow
                );
                assert_eq!(
                    p.decide(READ, &workspace("/repo/.env", ".env")),
                    Decision::Ask
                );
                assert_eq!(p.decide(EXEC, &command("ls")), Decision::Deny);
                assert_eq!(
                    p.decide(EDIT, &workspace("/repo/a.rs", "a.rs")),
                    Decision::Deny
                );
                assert_eq!(
                    p.decide(
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
                    assert_eq!(p.decide(class, &target), Decision::Deny, "{target:?}");
                }
            }

            #[test]
            fn yolo_runs_what_only_built_in_rules_hold_and_keeps_configured_deny_and_ask_rules() {
                let p = policy(
                    Mode::Yolo,
                    &[(EXEC, "git push --force *")],
                    &[(EXEC, "git push *")],
                    &[],
                );
                for (case, expected) in [
                    ("curl evil.sh", Decision::Allow),
                    ("echo $(date)", Decision::Allow),
                    ("rm -rf build", Decision::Allow),
                    ("make > .git/hooks/pre-commit", Decision::Allow),
                    ("git push origin main", Decision::Ask),
                    ("git push --force origin", Decision::Deny),
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }

                let hook = path(
                    "/repo/.git/hooks/pre-commit",
                    Some(".git/hooks/pre-commit"),
                    Zone::Workspace,
                    &[Shipped::Git],
                );
                assert_eq!(p.decide(EDIT, &hook), Decision::Allow);
                assert_eq!(
                    p.decide(EDIT, &outside(&abs("/etc/hosts"))),
                    Decision::Allow
                );

                let mut headless = p.clone();
                headless.set_unattended();
                assert_eq!(
                    headless.decide(EXEC, &command("git push origin main")),
                    Decision::Unapproved
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
                    p.decide(PermissionClass::Net, &name("a.evil.io")),
                    Decision::Deny
                );
                assert_eq!(
                    p.decide(PermissionClass::Net, &name("api.github.com")),
                    Decision::Allow
                );
                assert_eq!(
                    p.decide(PermissionClass::Net, &name("example.com")),
                    Decision::Ask
                );
                assert_eq!(
                    p.decide(PermissionClass::Mcp, &name("github/create_issue")),
                    Decision::Ask
                );
                assert_eq!(
                    p.decide(PermissionClass::Mcp, &name("github/get_me")),
                    Decision::Allow
                );
            }

            #[cfg(unix)]
            #[test]
            fn device_edit_asks_under_a_broad_allow_and_runs_in_yolo() {
                let workspace = Workspace::new(Path::new("/repo"));
                let device = Access::for_path("/dev/null", &workspace);

                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = resolved_policy(mode, &[], &[], &[(EDIT, "*")], &[], &workspace);
                    assert_eq!(p.decide(EDIT, &device), held_by_builtin(mode), "{mode}");
                }
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
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
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
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn command_holds_every_spelling_of_a_forced_recursive_rm() {
                let p = policy(Mode::Manual, &[], &[], &[(EXEC, "*")]);
                for case in [
                    "rm -rfv /",
                    "rm -r --force /",
                    "rm / -rf",
                    "rm -f -R x",
                    "rm $'-rf' /",
                    "$'rm' -rf /",
                    "ls && /bin/rm --rec --f x",
                    "sudo -u root rm -rf /",
                    "busybox rm -rf x",
                    "echo / | xargs rm -rf",
                    "echo -rf / | xargs rm",
                    r"find . -exec rm -rf {} \;",
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Ask, "{case}");
                }
                for case in ["rm -r x", "rm -f x", "rm -- -rf", "rmdir -rf x"] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Allow, "{case}");
                }
            }

            #[test]
            fn command_never_allows_an_unrecognized_command_but_still_denies_one() {
                let p = policy(Mode::Manual, &[(EXEC, "rm -rf *")], &[], &[(EXEC, "*")]);
                assert_eq!(p.decide(EXEC, &command("ls")), Decision::Allow);
                assert_eq!(p.decide(EXEC, &command("eval \"$CMD\"")), Decision::Ask);
                for case in [
                    "echo $(rm -rf /)",
                    "sh -c 'rm -rf /'",
                    "\"/bin/rm\" -rf /",
                    "exec rm -rf /",
                    "command rm -rf /",
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Deny, "{case}");
                }
            }

            #[test]
            fn command_allows_nothing_after_a_directory_change_it_cannot_place_in_the_workspace() {
                let p = policy(
                    Mode::Manual,
                    &[],
                    &[],
                    &[
                        (EXEC, "./build.sh"),
                        (EXEC, "./nice *"),
                        (EXEC, "cargo build"),
                        (EXEC, "cd *"),
                    ],
                );
                for (case, expected) in [
                    ("./build.sh", Decision::Allow),
                    ("cd x && cargo build", Decision::Allow),
                    ("cd x && cargo build 2>&1 | tail -20", Decision::Allow),
                    ("cd x && cargo build | grep -A 5 error", Decision::Allow),
                    ("cd x && cargo build | tail -20 log", Decision::Ask),
                    ("cd /tmp/evil && ./build.sh", Decision::Ask),
                    ("cd x && ./build.sh", Decision::Ask),
                    ("cd x && ./nice cargo build", Decision::Ask),
                    ("cd /tmp/evil && cargo build", Decision::Ask),
                    ("cd $DIR; cargo build", Decision::Ask),
                    ("cd /tmp/evil && ls", Decision::Ask),
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn command_deny_holds_what_a_followed_runner_runs_even_in_yolo() {
                let p = policy(Mode::Yolo, &[(EXEC, "rm *")], &[], &[]);
                for case in [
                    "sudo rm x",
                    "doas -u root rm x",
                    "pkexec rm x",
                    "busybox rm x",
                    "echo x | xargs rm",
                    "echo rm x | xargs sudo",
                    r"find . -execdir sudo rm {} +",
                    "sudo --unknown value rm x",
                    "sudo FOO=1 -u root rm x",
                    "runuser -u nobody rm x",
                    "su --command='rm x' root",
                    "busybox hush -c 'rm x'",
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Deny, "{case}");
                }
                assert_eq!(p.decide(EXEC, &command("echo x | xargs")), Decision::Allow);
            }

            #[test]
            fn an_allow_rule_cannot_allow_a_command_that_hides_a_program_from_the_rules() {
                let allow = &[
                    (EXEC, "git commit *"),
                    (EXEC, "cargo build *"),
                    (EXEC, "cat *"),
                ];
                let p = policy(Mode::Manual, &[], &[], allow);
                for case in [
                    "GIT_EDITOR='rm -rf y'; git commit --amend",
                    "cargo build ${x:n}",
                    "cat ${!n}",
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Ask, "{case}");
                }

                let denied = policy(Mode::Yolo, &[(EXEC, "git *")], &[], allow);
                assert_eq!(
                    denied.decide(EXEC, &command("GIT_EDITOR=x; git commit --amend")),
                    Decision::Deny,
                );

                assert_eq!(
                    p.decide(EXEC, &command("cargo build ${profile:-release}")),
                    Decision::Allow,
                );
            }

            #[test]
            fn command_asks_in_manual_and_runs_in_yolo_when_ox_cannot_analyze_it_unless_denied() {
                let unrecognized = [
                    ("BASH_CMDS[ls]=/bin/rm; ls -rf x", "BASH_CMDS[ls]=/bin/rm"),
                    (
                        "BASH_CMDS+=([ls]=/bin/rm); ls -rf x",
                        "BASH_CMDS+=([ls]=/bin/rm)",
                    ),
                    (
                        "builtin declare \"BASH_CMDS[ls]=/bin/rm\"; ls -rf x",
                        "declare \"BASH_CMDS[ls]=/bin/rm\"",
                    ),
                    ("ls 'unterminated", "'unterminated"),
                    ("sh -c 'ls (('", "(("),
                ];
                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = policy(mode, &[(EXEC, "rm *")], &[], &[(EXEC, "*")]);
                    for (case, construct) in unrecognized {
                        let verdict = p.verdict(EXEC, &command(case));
                        let expected = match mode {
                            Mode::Yolo => (Decision::Allow, Basis::Mode(Mode::Yolo)),
                            _ => (
                                Decision::Ask,
                                Basis::Unrecognized {
                                    construct: construct.to_owned(),
                                },
                            ),
                        };
                        assert_eq!(
                            (verdict.decision, verdict.basis),
                            expected,
                            "{mode} {case:?}"
                        );
                    }
                    for case in ["rm x; ls 'unterminated", "PATH=/tmp rm -rf x"] {
                        assert_eq!(p.decide(EXEC, &command(case)), Decision::Deny, "{mode}");
                    }
                    assert_eq!(
                        p.decide(EXEC, &command("ls 'done'")),
                        Decision::Allow,
                        "{mode}"
                    );
                }
            }

            #[test]
            fn command_rules_see_the_words_a_heredoc_passes_to_its_command() {
                let base_dir = canonical_tempdir();
                let cwd = base_dir.path().to_path_buf();
                let workspace = Workspace::new(&cwd);
                let decide =
                    |p: &Policy, case: &str| p.decide(EXEC, &Access::for_command(case, &workspace));
                let yolo = policy_in(Mode::Yolo, &[(EXEC, "git push *")], &[], &[], &workspace);
                let manual = policy_in(Mode::Manual, &[(READ, ".env")], &[], &[], &workspace);

                for case in [
                    "git <<EOF push --force origin main\nx\nEOF",
                    "true && git <<EOF push\nx\nEOF",
                    "ls | git <<EOF push\nx\nEOF",
                    "! git <<EOF push\nx\nEOF",
                    "FOO=1 <<EOF git push\nx\nEOF",
                    "git <<EOF 2>&1 push\nx\nEOF",
                    "git >/dev/null push",
                ] {
                    assert_eq!(decide(&yolo, case), Decision::Deny, "{case:?}");
                }
                assert_eq!(
                    decide(&manual, "cat <<'EOF' README.md\nx\nEOF"),
                    Decision::Allow
                );
                assert_eq!(decide(&manual, "cat <<'EOF' .env\nx\nEOF"), Decision::Deny);
                for case in [
                    "find <<EOF . -delete\nx\nEOF",
                    "{ cat; } <<EOF README.md\nx\nEOF",
                ] {
                    assert_eq!(decide(&manual, case), Decision::Ask, "{case:?}");
                }
            }

            #[test]
            fn command_rules_see_a_joined_word_and_yolo_runs_a_split_ox_cannot_follow() {
                let yolo = policy(Mode::Yolo, &[(EXEC, "git push *")], &[], &[]);

                for (case, expected) in [
                    ("gi\\\nt push origin", Decision::Deny),
                    ("LC_ALL=$ git push origin", Decision::Allow),
                    ("git status \\  x", Decision::Allow),
                ] {
                    assert_eq!(yolo.decide(EXEC, &command(case)), expected, "{case:?}");
                }

                let yolo = policy(Mode::Yolo, &[(EXEC, "kubectl delete -f -")], &[], &[]);
                let case = "kubectl delete -f - <<EOF\nkind: Namespace\nEOF";
                assert_eq!(yolo.decide(EXEC, &command(case)), Decision::Deny);
            }

            #[test]
            fn command_rules_see_coproc_and_yolo_runs_a_compound_after_a_reserved_word() {
                let yolo = policy(Mode::Yolo, &[(EXEC, "rm *")], &[], &[]);

                assert_eq!(
                    yolo.decide(EXEC, &command("coproc rm -rf x")),
                    Decision::Deny
                );
                for case in [
                    "coproc worker { rm -rf x; }",
                    "time coproc worker { rm -rf x; }",
                    "time while true; do rm -rf x; done",
                    "time -p ! rm -rf x",
                    "time -- ! rm -rf x",
                ] {
                    let verdict = yolo.verdict(EXEC, &command(case));
                    assert_eq!(
                        (verdict.decision, verdict.basis),
                        (Decision::Allow, Basis::Mode(Mode::Yolo)),
                        "{case:?}"
                    );
                }
            }

            #[test]
            fn command_rules_see_the_program_a_runner_starts_past_an_operator_or_dollar_word() {
                let yolo = policy(Mode::Yolo, &[(EXEC, "rm *")], &[], &[]);
                let manual = policy(Mode::Manual, &[], &[], &[(EXEC, "git status")]);

                for case in [
                    "xargs -I == rm -rf x",
                    "xargs -I =~ rm -rf x",
                    r#"xargs -I =="x" rm -rf x"#,
                    "xargs -I =~'x' rm -rf x",
                    "xargs -I ==$x rm -rf x",
                    r#"xargs -I $"x" rm -rf x"#,
                    r#"xargs -I$"x" rm -rf x"#,
                ] {
                    assert_eq!(yolo.decide(EXEC, &command(case)), Decision::Deny, "{case}");
                }
                for case in ["git == status", r#"exec -a =="echo" rm -rf x"#] {
                    assert_eq!(manual.decide(EXEC, &command(case)), Decision::Ask, "{case}");
                }
            }

            #[test]
            fn command_allow_sees_through_only_wrappers_that_keep_who_runs_it_and_its_arguments() {
                let p = policy(Mode::Manual, &[], &[], &[(EXEC, "cat *"), (EXEC, "find *")]);
                for (case, expected) in [
                    ("nice timeout 5 cat a", Decision::Allow),
                    ("sudo cat a", Decision::Ask),
                    ("echo a | xargs cat", Decision::Ask),
                    (r"find . -exec cat {} +", Decision::Allow),
                    (r"find . -exec wc {} +", Decision::Ask),
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn command_weighs_each_redirection_target_as_an_edit() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::create_dir(cwd.join(".git")).unwrap();
                let outside = abs("/etc/hosts");
                let p = resolved_policy(
                    Mode::Manual,
                    &[(EDIT, "secret.txt")],
                    &[(EDIT, "*.lock")],
                    &[],
                    &[],
                    &Workspace::new(cwd),
                );
                let workspace = Workspace::new(cwd);
                let decide = |case: &str| decide_command(&p, case, &workspace);
                assert_eq!(decide("echo hi > out.txt 2>&1"), Decision::Allow);
                assert_eq!(decide("echo hi > .envrc"), Decision::Ask);
                assert_eq!(decide("echo hi > .git/hooks/pre-commit"), Decision::Ask);
                assert_eq!(decide("echo hi > Cargo.lock"), Decision::Ask);
                assert_eq!(decide("env -C /etc echo hi > hosts"), Decision::Ask);
                assert_eq!(decide(&format!("echo hi >> {outside}")), Decision::Ask);
                assert_eq!(decide("echo hi > secret.txt"), Decision::Deny);

                let elsewhere_dir = canonical_tempdir();

                let elsewhere = elsewhere_dir.path();
                let notes = slash_separated(&elsewhere.join("notes.txt"));
                let allowed = resolved_policy(
                    Mode::Manual,
                    &[],
                    &[],
                    &[(EDIT, &notes)],
                    &[],
                    &Workspace::new(cwd),
                );
                assert_eq!(
                    decide_command(&allowed, &format!("echo hi >> {notes}"), &workspace),
                    Decision::Allow
                );
            }

            #[test]
            fn command_settles_a_known_safe_read_of_a_held_file_by_its_rule_tier() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let workspace = Workspace::new(cwd);
                for mode in [Mode::Manual, Mode::Yolo] {
                    let p = policy_in(
                        mode,
                        &[(READ, ".env")],
                        &[(READ, "secrets")],
                        &[(EXEC, "cat *"), (EXEC, "grep *")],
                        &workspace,
                    );
                    for (case, expected) in [
                        ("cat README.md", Decision::Allow),
                        ("cat .env", Decision::Deny),
                        ("cat secrets/key", Decision::Ask),
                        ("grep --file=.env x", Decision::Deny),
                        ("grep --reg=x .env", Decision::Deny),
                        ("grep --binary x .env", Decision::Deny),
                        ("grep --unknown=x key .env", Decision::Deny),
                        ("grep x README.md -e .env", Decision::Deny),
                    ] {
                        assert_eq!(
                            decide_command(&p, case, &workspace),
                            expected,
                            "{mode:?}: {case}"
                        );
                    }
                }
            }

            #[test]
            fn command_denies_a_reader_glob_naming_a_denied_file_and_asks_for_a_variable() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(cwd.join("src")).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                std::fs::write(cwd.join("src/main.rs"), "").unwrap();
                std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    let resolve = |disabled: &[Shipped]| {
                        resolved_policy(
                            Mode::Manual,
                            &[],
                            &[],
                            &[(EXEC, "cat *"), (EXEC, "grep *")],
                            disabled,
                            &workspace,
                        )
                    };

                    let p = resolve(&[]);
                    for (case, expected) in [
                        ("cat src/*.rs", Decision::Allow),
                        ("cat ~/*/id_rsa", Decision::Allow),
                        ("cat ~/.ssh/id_*", Decision::Deny),
                        ("cat ~/.ss?/id_rsa", Decision::Deny),
                        ("cat ~/.*/id_rsa", Decision::Deny),
                        ("grep -r key ~/.ssh", Decision::Deny),
                        ("ls -R ~/.ssh", Decision::Deny),
                        ("grep -r key ~", Decision::Ask),
                        ("wc --files0-from ~/.ssh/id_rsa", Decision::Deny),
                        ("wc --files0-from=secrets/list", Decision::Ask),
                        ("cat $HOME/.ssh/id_rsa", Decision::Deny),
                        ("cat \"${HOME}/.ssh/id_rsa\"", Decision::Deny),
                        ("cat $F", Decision::Ask),
                    ] {
                        assert_eq!(decide_command(&p, case, &workspace), expected, "{case}");
                    }
                    let unguarded = resolve(&[Shipped::Ssh, Shipped::Credentials]);
                    assert_eq!(
                        decide_command(&unguarded, "cat $F", &workspace),
                        Decision::Allow,
                        "no read rule is left to hold it"
                    );
                });
            }

            #[test]
            fn yolo_holds_a_reader_only_for_the_files_it_reads_for_certain() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(&cwd).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();
                std::fs::write(cwd.join("-n"), "").unwrap();
                std::fs::write(cwd.join("notes"), "").unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    let yolo = resolved_policy(
                        Mode::Yolo,
                        &[],
                        &[(READ, "secrets")],
                        &[],
                        &[],
                        &workspace,
                    );

                    for (case, expected) in [
                        ("cat ~/.ssh/id_*", Decision::Deny),
                        ("cat $HOME/.ssh/id_rsa", Decision::Deny),
                        ("cat \"${HOME}/.ssh/id_rsa\"", Decision::Deny),
                        ("cat secrets/key", Decision::Ask),
                        ("grep -r key ~/.ssh", Decision::Deny),
                        ("grep -r key ~", Decision::Allow),
                        ("grep -r key secrets", Decision::Ask),
                        ("cat ~/.ssh/id_rsa *", Decision::Deny),
                        ("cat * ~/.ssh/id_rsa", Decision::Deny),
                        ("head * ~/.ssh/id_rsa", Decision::Deny),
                        ("grep key * ~/.ssh/id_rsa", Decision::Deny),
                        ("grep * ~/.ssh/id_rsa", Decision::Deny),
                        ("grep -e key * ~/.ssh/id_rsa", Decision::Deny),
                        ("find ~ -path ~/.ssh/id_rsa", Decision::Allow),
                        ("wc --files0-from ~/.ssh/id_rsa", Decision::Deny),
                        ("wc --files0-from=secrets/list", Decision::Ask),
                        ("test -f ~/.ssh/id_rsa", Decision::Allow),
                        ("cat $F", Decision::Allow),
                        ("echo x | xargs cat", Decision::Allow),
                    ] {
                        assert_eq!(decide_command(&yolo, case, &workspace), expected, "{case}");
                    }
                });
            }

            #[test]
            fn command_weighs_a_relative_path_under_each_directory_it_may_have_entered() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(cwd.join("secrets")).unwrap();
                std::fs::create_dir_all(cwd.join("src")).unwrap();
                std::fs::create_dir_all(cwd.join("vault")).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                std::fs::write(cwd.join("secrets/key"), "").unwrap();
                std::fs::write(cwd.join("notes"), "").unwrap();
                std::fs::write(cwd.join("vault/key"), "").unwrap();
                std::fs::write(cwd.join("vault/readme"), "").unwrap();
                std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();
                let sed_write = format!(
                    "cd secrets && sed -n w\\ key {}",
                    slash_separated(&cwd.join("notes"))
                );
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    for mode in [Mode::Manual, Mode::Yolo] {
                        let p = resolved_policy(
                            mode,
                            &[
                                (READ, "./secrets"),
                                (EDIT, "./secrets"),
                                (READ, "./vault/key"),
                            ],
                            &[],
                            &[(EXEC, "*")],
                            &[],
                            &workspace,
                        );
                        let moved = if mode == Mode::Yolo {
                            Decision::Allow
                        } else {
                            Decision::Ask
                        };
                        for (case, expected) in [
                            ("cd ~/.ssh && cat id_rsa", Decision::Deny),
                            ("cd -P ~/.ssh && cat id_rsa", Decision::Deny),
                            ("cd -L ~/.ssh && cat id_rsa", Decision::Deny),
                            ("cd -- ~/.ssh && cat id_rsa", Decision::Deny),
                            ("cd -Pe -- secrets && cat key", Decision::Deny),
                            ("pushd secrets; cat key", Decision::Deny),
                            ("pushd -n secrets; cat key", moved),
                            ("cd vault && cat k*", Decision::Deny),
                            ("cd -P vault; head -n 1 k* readme", Decision::Deny),
                            ("cd ~/.ssh && ls -R", Decision::Deny),
                            ("cd ~/.ssh; wc -l < id_rsa", Decision::Deny),
                            ("cd; cat .ssh/id_rsa", Decision::Deny),
                            ("cd secrets && cat key", Decision::Deny),
                            ("cd src && cd ../secrets && cat key", Decision::Deny),
                            ("cd secrets || cat key", Decision::Deny),
                            ("cd secrets && echo x > key", Decision::Deny),
                            (&sed_write, Decision::Deny),
                            ("cd src && cat main.rs", moved),
                            ("cd $DIR && cat key", moved),
                        ] {
                            assert_eq!(
                                decide_command(&p, case, &workspace),
                                expected,
                                "{mode} {case}"
                            );
                        }
                    }
                });
            }

            #[test]
            fn command_weighs_the_files_a_sed_script_reads_writes_and_runs() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, home) = (root.join("repo"), root.join("home"));
                std::fs::create_dir_all(cwd.join(".git")).unwrap();
                std::fs::create_dir_all(cwd.join("docs")).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                std::fs::write(cwd.join("README.md"), "").unwrap();
                std::fs::write(cwd.join(".git/config"), "").unwrap();
                std::fs::write(cwd.join("docs/guide.md"), "").unwrap();
                std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();
                let key = slash_separated(&home.join(".ssh/id_rsa"));
                let elsewhere = slash_separated(&root.join("elsewhere"));
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    for mode in [Mode::Manual, Mode::Yolo] {
                        let p = resolved_policy(
                            mode,
                            &[(EDIT, "./locked"), (EDIT, "./docs/old/guide.md")],
                            &[],
                            &[(EXEC, "cd *"), (EXEC, "sed *")],
                            &[],
                            &workspace,
                        );
                        let held = if mode == Mode::Yolo {
                            Decision::Allow
                        } else {
                            Decision::Ask
                        };
                        for (case, expected) in [
                            (format!("sed '1r {key}' README.md"), Decision::Deny),
                            (format!("sed -e p -e '$R {key}' README.md"), Decision::Deny),
                            (format!("sed --posix '1r {key}' README.md"), Decision::Deny),
                            (format!("sed --qu -s '1r {key}' README.md"), Decision::Deny),
                            (format!("sed '1r {key}' README.md --x"), Decision::Deny),
                            (format!("sed -i '1r {key}' README.md"), Decision::Deny),
                            (format!("sed -l 5 '1r {key}' README.md"), Decision::Deny),
                            ("sed 'w locked' README.md".to_owned(), Decision::Deny),
                            ("sed -i 'w locked' README.md".to_owned(), Decision::Deny),
                            ("sed -e 'w locked' --x README.md".to_owned(), Decision::Deny),
                            ("sed 's/a/b/w locked' README.md".to_owned(), Decision::Deny),
                            (
                                "sed -n $'w locked\\nk' README.md".to_owned(),
                                Decision::Deny,
                            ),
                            ("sed -i s/a/b/ locked".to_owned(), Decision::Deny),
                            ("sed --in-place=.bak 1d locked".to_owned(), Decision::Deny),
                            (
                                "sed -i'locked/*' s/a/b/ README.md".to_owned(),
                                Decision::Deny,
                            ),
                            (
                                "cd docs && sed -i'old/*' s/a/b/ g*.md".to_owned(),
                                Decision::Deny,
                            ),
                            (format!("sed -i'{elsewhere}/*' s/a/b/ README.md"), held),
                            ("sed -i $X s/a/b/ README.md".to_owned(), held),
                            ("sed -i s/a/b/ .git/config".to_owned(), held),
                            ("sed '1e date' README.md".to_owned(), held),
                            ("sed -f script.sed README.md".to_owned(), held),
                            ("sed -f script.sed --x README.md".to_owned(), held),
                            ("sed 's/a/b/' README.md".to_owned(), Decision::Allow),
                            ("sed -n 1,5p README.md".to_owned(), Decision::Allow),
                            ("sed -i s/a/b/ README.md".to_owned(), Decision::Allow),
                            ("sed -i s/a/b/ example.txt".to_owned(), Decision::Allow),
                            (
                                "sed -n '/x/p;$r notes' README.md".to_owned(),
                                Decision::Allow,
                            ),
                        ] {
                            assert_eq!(
                                decide_command(&p, &case, &workspace),
                                expected,
                                "{mode} {case}"
                            );
                        }
                    }
                });
            }

            #[test]
            fn yolo_holds_a_recursive_read_of_the_cwd_that_a_rule_names() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let workspace = Workspace::new(cwd);
                let yolo = resolved_policy(
                    Mode::Yolo,
                    &[],
                    &[(READ, &slash_separated(cwd))],
                    &[],
                    &[],
                    &workspace,
                );

                for case in ["grep -r key", "grep -r key .", "ls -R", "ls -R ."] {
                    assert_eq!(
                        decide_command(&yolo, case, &workspace),
                        Decision::Ask,
                        "{case}"
                    );
                }
            }

            #[test]
            fn command_weighs_each_input_redirection_as_a_read() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let workspace = Workspace::new(cwd);
                let p = policy_in(
                    Mode::Yolo,
                    &[(READ, ".env")],
                    &[(READ, "secrets")],
                    &[(EXEC, "*")],
                    &workspace,
                );
                let decide = |case: &str| decide_command(&p, case, &workspace);
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
            fn command_denies_a_read_of_a_shipped_read_deny_in_every_mode() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let home = base.join("home");
                let key = slash_separated(&home.join(".ssh/id_ed25519"));
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(base);
                    for mode in [Mode::Manual, Mode::Yolo] {
                        let p = resolved_policy(mode, &[], &[], &[], &[], &workspace);
                        for case in [format!("cat < {key}"), format!("cat {key}")] {
                            let access = Access::for_command(&case, &workspace);

                            let verdict = p.verdict(EXEC, &access);

                            assert_eq!(
                                (verdict.decision, verdict.basis),
                                (Decision::Deny, Basis::Builtin(Shipped::Ssh)),
                                "{mode:?}: {case}"
                            );
                        }
                    }
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
                let verdict = policy.verdict(class, target);
                (verdict.decision, verdict.basis)
            }

            #[test]
            fn recursive_readers_ask_for_shipped_descendants_outside_yolo() {
                let dir = canonical_tempdir();
                let (cwd, home) = (dir.path().join("repo"), dir.path().join("home"));
                std::fs::create_dir_all(cwd.join("src")).unwrap();
                std::fs::create_dir_all(home.join(".ssh")).unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(&cwd);
                    for (mode, expected) in
                        [(Mode::Manual, Decision::Ask), (Mode::Yolo, Decision::Allow)]
                    {
                        let p = resolved_policy(
                            mode,
                            &[],
                            &[],
                            &[(EXEC, "grep *"), (EXEC, "find *"), (EXEC, "ls *")],
                            &[],
                            &workspace,
                        );
                        for case in [
                            "grep -r PRIVATE ~",
                            "grep -R PRIVATE ~",
                            "grep -nr PRIVATE ~",
                            "grep -d recurse PRIVATE ~",
                            "grep --directories=recurse PRIVATE ~",
                            "grep --rec PRIVATE ~",
                            "grep --dereference-recursive PRIVATE ~",
                            "nice timeout 5 grep -r PRIVATE ~",
                            "sudo grep -r PRIVATE ~",
                            "cd src && grep -r PRIVATE ~",
                            r"find src -exec grep -r PRIVATE ~ \;",
                            "/bin/grep -r PRIVATE ~",
                            "find ~ -name id_rsa",
                            "find -L ~ -name id_rsa",
                            "find -HL ~ -name id_rsa",
                            "find -f ~",
                            "find -- ~ -name id_rsa",
                            "ls -laR ~",
                            "ls --recursive ~",
                            "grep -r PRIVATE $HOME",
                            "grep -r PRIVATE \"${HOME}\"",
                        ] {
                            let access = Access::for_command(case, &workspace);

                            let verdict = p.verdict(EXEC, &access);

                            assert_eq!(verdict.decision, expected, "{mode:?}: {case}");
                            if mode == Mode::Manual {
                                assert_eq!(verdict.basis, Basis::Builtin(Shipped::Ssh), "{case}");
                            }
                        }
                        for case in ["grep -r TODO src", "find src -name needle", "ls -R src"] {
                            assert_eq!(
                                decide_command(&p, case, &workspace),
                                Decision::Allow,
                                "{mode:?}: {case}"
                            );
                        }
                    }
                });
            }

            #[test]
            fn recursive_readers_keep_configured_tiers_and_unattended_decisions() {
                let dir = canonical_tempdir();
                let cwd = dir.path();
                temp_env::with_var(HOME_VAR, Some(cwd), || {
                    let workspace = Workspace::new(cwd);
                    for (tier, deny, ask) in [
                        (Tier::Deny, &[(READ, "./vault/key")][..], &[][..]),
                        (Tier::Ask, &[][..], &[(READ, "./vault/key")][..]),
                    ] {
                        for attended in [true, false] {
                            let mut p = resolved_policy(
                                Mode::Manual,
                                deny,
                                ask,
                                &[(EXEC, "grep *")],
                                &[Shipped::Ssh, Shipped::Credentials],
                                &workspace,
                            );
                            if !attended {
                                p.set_unattended();
                            }
                            let expected = if attended {
                                Decision::Ask
                            } else {
                                Decision::Unapproved
                            };
                            for case in [
                                "grep -r needle .",
                                "grep -r needle",
                                "grep -r needle $DIR",
                                "find",
                                "find -name needle",
                                r"find \( -name needle \)",
                                "find ! -name needle",
                                "ls -R",
                            ] {
                                let access = Access::for_command(case, &workspace);

                                let verdict = p.verdict(EXEC, &access);

                                assert_eq!(
                                    (verdict.decision, verdict.basis),
                                    (expected, rule_basis(tier, READ, "./vault/key")),
                                    "attended={attended}: {case}"
                                );
                            }
                        }
                    }
                });
            }

            #[test]
            fn recursive_reader_globs_reparse_expanded_argument_roles() {
                for (name, command, protected, files) in [
                    (
                        "expanded pattern-file value leaves a recursive search root",
                        "grep -r -f patterns/* src",
                        "./patterns/z-vault/key",
                        &["patterns/a-patterns", "patterns/z-vault/key", "src/file"][..],
                    ),
                    (
                        "expanded search operand supplies a recursive option",
                        "grep PRIVATE *",
                        "./vault/key",
                        &["-r", "src/file", "vault/key"][..],
                    ),
                    (
                        "expanded option upgrades a root in another source word",
                        "grep PRIVATE *r vault",
                        "./vault/key",
                        &["-r", "other", "vault/key"][..],
                    ),
                    (
                        "expanded long option supplies recursive directory mode",
                        "grep PRIVATE *",
                        "./vault/key",
                        &["--directories=recurse", "src/file", "vault/key"][..],
                    ),
                    (
                        "expanded directory mode can take another expansion's value",
                        "grep PRIVATE *",
                        "./vault/key",
                        &["-d", "recurse", "src/file", "vault/key"][..],
                    ),
                    (
                        "single match supplies a recursive option",
                        "grep PRIVATE *r vault",
                        "./vault/key",
                        &["-r", "vault/key"][..],
                    ),
                    (
                        "a build-specific option value may be an ordinary root",
                        "ls -D vault -* 80",
                        "./vault/key",
                        &["-w", "-R", "vault/key"][..],
                    ),
                    (
                        "an exposed ls flag retains recursion without a following value",
                        "ls --format *",
                        "./vault/key",
                        &["-R", "long", "vault/key"][..],
                    ),
                    (
                        "an expanded option may consume the final explicit root",
                        "grep -e PRIVATE -* pats",
                        "./vault/key",
                        &["-f", "-R", "pats", "vault/key"][..],
                    ),
                    (
                        "an expanded find expression may make cwd implicit",
                        r"find [-\(v]* \)",
                        "./secret/key",
                        &["vault/file", "(", "-name", "secret/key"][..],
                    ),
                    (
                        "an earlier single match changes a later group's roles",
                        "grep -r -e needle -? patterns/* src",
                        "./patterns/z-vault/key",
                        &[
                            "-f",
                            "patterns/a-patterns",
                            "patterns/z-vault/key",
                            "src/file",
                        ][..],
                    ),
                    (
                        "an earlier single match consumes an apparent fixed root",
                        "grep -e PRIVATE -[f] pats -* more",
                        "./vault/key",
                        &["-f", "-R", "pats", "more", "vault/key"][..],
                    ),
                ] {
                    let dir = canonical_tempdir();
                    let cwd = dir.path();
                    for file in files {
                        let path = cwd.join(file);
                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                        std::fs::write(path, "needle").unwrap();
                    }
                    temp_env::with_var(HOME_VAR, Some(cwd), || {
                        let workspace = Workspace::new(cwd);
                        for (tier, deny, ask) in [
                            (Tier::Deny, &[(READ, protected)][..], &[][..]),
                            (Tier::Ask, &[][..], &[(READ, protected)][..]),
                        ] {
                            let p = resolved_policy(
                                Mode::Manual,
                                deny,
                                ask,
                                &[(EXEC, "grep *"), (EXEC, "find *"), (EXEC, "ls *")],
                                &[Shipped::Ssh, Shipped::Credentials],
                                &workspace,
                            );

                            let access = Access::for_command(command, &workspace);
                            let verdict = p.verdict(EXEC, &access);

                            assert_eq!(
                                (verdict.decision, verdict.basis),
                                (Decision::Ask, rule_basis(tier, READ, protected)),
                                "{name}: {command}"
                            );
                        }
                    });
                }
            }

            #[test]
            fn recursive_reader_globs_keep_unrelated_roots_and_fixed_auxiliary_files_exact() {
                let dir = canonical_tempdir();
                let cwd = dir.path();
                for file in [
                    "aux/list",
                    "aux/protected/key",
                    "aux/public",
                    "src/a/file",
                    "src/b/file",
                    "--recursive",
                    "-r",
                    "other",
                ] {
                    let path = cwd.join(file);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, "needle").unwrap();
                }
                temp_env::with_var(HOME_VAR, Some(cwd), || {
                    let workspace = Workspace::new(cwd);
                    for mode in [Mode::Manual, Mode::Yolo] {
                        let p = resolved_policy(
                            mode,
                            &[(READ, "./aux/list"), (READ, "./aux/protected/key")],
                            &[],
                            &[(EXEC, "grep *"), (EXEC, "find *"), (EXEC, "ls *")],
                            &[Shipped::Ssh, Shipped::Credentials],
                            &workspace,
                        );
                        for (case, unknown) in [
                            ("grep -r -f aux/public* src", false),
                            ("grep -r -f aux/protected* src", false),
                            ("grep -f aux/p* PRIVATE src", true),
                            ("grep -r --file=aux/lis? src", false),
                            ("grep -r -faux/lis? src", false),
                            ("grep -r PRIVATE src/*", false),
                            ("find src/*", false),
                            ("ls -R src/*", false),
                            ("grep -- PRIVATE *r src", false),
                            ("grep -r -- PRIVATE -* src", false),
                            ("grep -- -* aux/protected", true),
                            ("grep -f aux/protected PRIVATE *r src", true),
                        ] {
                            let expected = if unknown && mode == Mode::Manual {
                                Decision::Ask
                            } else {
                                Decision::Allow
                            };

                            let decision = decide_command(&p, case, &workspace);

                            assert_eq!(decision, expected, "{mode:?}: {case}");
                        }
                    }
                });
            }

            #[test]
            fn recursive_options_keep_pattern_files_and_regular_files_exact() {
                let dir = canonical_tempdir();
                let cwd = dir.path();
                std::fs::create_dir(cwd.join("src")).unwrap();
                std::fs::write(cwd.join("patterns"), "needle").unwrap();
                std::fs::write(cwd.join("src/file"), "needle").unwrap();
                temp_env::with_var(HOME_VAR, Some(cwd), || {
                    let workspace = Workspace::new(cwd);
                    let p = resolved_policy(
                        Mode::Manual,
                        &[(READ, "./patterns/key"), (READ, "./src/file/key")],
                        &[],
                        &[(EXEC, "grep *")],
                        &[Shipped::Ssh, Shipped::Credentials],
                        &workspace,
                    );

                    for case in [
                        "grep -r -f patterns src/file",
                        "grep -r --file=patterns src/file",
                        "grep -r --exclude-from=patterns needle src/file",
                        "grep -r needle -",
                    ] {
                        assert_eq!(
                            decide_command(&p, case, &workspace),
                            Decision::Allow,
                            "{case}"
                        );
                    }
                    let protected = resolved_policy(
                        Mode::Manual,
                        &[(READ, "./patterns")],
                        &[],
                        &[(EXEC, "grep *")],
                        &[Shipped::Ssh, Shipped::Credentials],
                        &workspace,
                    );
                    assert_eq!(
                        decide_command(&protected, "grep -r -f patterns src", &workspace),
                        Decision::Deny
                    );
                });
            }

            #[test]
            fn recursive_readers_compare_requested_and_resolved_rule_prefixes() {
                let dir = canonical_tempdir();
                let cwd = dir.path();
                std::fs::create_dir(cwd.join("vault")).unwrap();
                symlink_dir(Path::new("vault"), &cwd.join("alias"));
                temp_env::with_var(HOME_VAR, Some(cwd), || {
                    let workspace = Workspace::new(cwd);
                    let p = resolved_policy(
                        Mode::Manual,
                        &[(READ, "./alias/nested/key")],
                        &[],
                        &[(EXEC, "grep *")],
                        &[Shipped::Ssh, Shipped::Credentials],
                        &workspace,
                    );

                    for (case, expected) in [
                        ("grep -r needle vault", Decision::Ask),
                        ("grep -r needle alias", Decision::Ask),
                        ("grep -r needle other", Decision::Allow),
                        ("grep -r needle ALIAS", Decision::Ask),
                    ] {
                        assert_eq!(decide_command(&p, case, &workspace), expected, "{case}");
                    }
                    let late = resolved_policy(
                        Mode::Manual,
                        &[(READ, "./late/nested/key")],
                        &[],
                        &[],
                        &[Shipped::Ssh, Shipped::Credentials],
                        &workspace,
                    );
                    symlink_dir(Path::new("vault"), &cwd.join("late"));

                    assert_eq!(
                        decide_command(&late, "grep -r needle vault", &workspace),
                        Decision::Ask
                    );
                });
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
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let home = base.join("home");
                let key = slash_separated(&home.join(".ssh/id_ed25519"));
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::new(base);
                    let p = resolved_policy(Mode::Yolo, &[], &[], &[], &[], &workspace);
                    let settle =
                        |case: &str| verdict(&p, EXEC, &Access::for_command(case, &workspace));

                    assert_eq!(
                        settle(&format!("cat < {key}")),
                        (Decision::Deny, Basis::Builtin(Shipped::Ssh)),
                    );
                    assert_eq!(
                        settle(&format!("cat {key}")),
                        (Decision::Deny, Basis::Builtin(Shipped::Ssh)),
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

            #[test]
            fn of_an_unattended_run_leaves_only_what_would_ask_unapproved() {
                let unattended = |mode| {
                    let mut p = policy(mode, &[(EXEC, "rm *")], &[(EXEC, "git push *")], &[]);
                    p.set_unattended();
                    p
                };
                let manual = unattended(Mode::Manual);
                let plan = unattended(Mode::Plan);

                for (p, class, target, expected) in [
                    (
                        &manual,
                        EXEC,
                        command("rm x"),
                        (Decision::Deny, rule_basis(Tier::Deny, EXEC, "rm *")),
                    ),
                    (
                        &plan,
                        EXEC,
                        command("ls"),
                        (Decision::Deny, Basis::Mode(Mode::Plan)),
                    ),
                    (
                        &manual,
                        READ,
                        outside(&abs("/etc/hosts")),
                        (Decision::Allow, Basis::Read),
                    ),
                    (
                        &manual,
                        EXEC,
                        command("git push origin"),
                        (
                            Decision::Unapproved,
                            rule_basis(Tier::Ask, EXEC, "git push *"),
                        ),
                    ),
                    (
                        &manual,
                        EXEC,
                        command("git status"),
                        (Decision::Unapproved, Basis::Mode(Mode::Manual)),
                    ),
                ] {
                    assert_eq!(verdict(p, class, &target), expected, "{target:?}");
                }
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
    }

    mod walk_filter {
        use super::*;

        mod withholds {
            use super::*;

            #[test]
            fn parent_traversal_matches_the_gate_decision() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let root = cwd.ancestors().last().unwrap();
                let absolute_rule = slash_separated(cwd);
                let policy = policy_in(
                    Mode::Manual,
                    &[(READ, &absolute_rule)],
                    &[],
                    &[],
                    &Workspace::new(root),
                );
                let filter = policy.read_filter();
                let file = cwd.join("secret.txt");
                let workspace = Workspace::new(root);
                let target = Access::for_path(file.to_str().unwrap(), &workspace);
                assert_eq!(policy.decide(READ, &target), Decision::Deny);
                for requested in [Path::new(".."), Path::new("../.."), Path::new(".")] {
                    let walk = filter.walk(root, requested, &workspace);
                    assert!(walk.withholds(&file), "{requested:?}");
                }
            }

            #[test]
            fn a_file_by_the_spelling_the_call_used() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, outside) = (root.join("cwd"), root.join("outside"));
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(&outside).unwrap();
                symlink_dir(&outside, &cwd.join("config"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[(READ, ".env")],
                    &[],
                    &[],
                    &[],
                    &Workspace::new(&cwd),
                );
                let filter = p.read_filter();
                let env = outside.join(".env");
                let workspace = Workspace::new(&cwd);

                for (spelled, denied) in [("config", true), ("../outside", false)] {
                    let walk = filter.walk(&outside, Path::new(spelled), &workspace);
                    assert_eq!(walk.withholds(&env), denied, "{spelled}");
                    let gate = Access::for_path(format!("{spelled}/.env"), &workspace);
                    assert_eq!(p.decide(READ, &gate) == Decision::Deny, denied, "{spelled}");
                }
            }

            #[test]
            fn where_a_rule_prefix_resolves_through_a_symlink() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let (cwd, outside) = (root.join("cwd"), root.join("outside"));
                std::fs::create_dir(&cwd).unwrap();
                std::fs::create_dir(&outside).unwrap();
                symlink_dir(&outside, &cwd.join("link"));
                let p = resolved_policy(
                    Mode::Manual,
                    &[],
                    &[(READ, "link/**")],
                    &[],
                    &[],
                    &Workspace::new(&cwd),
                );

                let filter = p.read_filter();
                let walk = filter.walk(&outside, Path::new("../outside"), &Workspace::new(&cwd));
                assert!(walk.withholds(&outside.join("key")));
            }
        }
    }
}
