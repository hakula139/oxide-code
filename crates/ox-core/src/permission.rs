//! Tool permission checks. [`Policy::verdict`] combines a call's [`Access`] with its mode and rule
//! tiers to produce a [`Decision`] and the [`Basis`] that settled it.
//!
//! oxide-code has no OS sandbox. Command rules are guardrails because an allowed program can access
//! files or run other programs beyond what its command line reveals.

pub(crate) mod project;
pub mod rule;
pub(crate) mod shell;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result, bail, ensure};
use caseless::Caseless as _;
use regex::Regex;
use serde::Deserialize;
use strum::{Display, EnumString, IntoStaticStr, VariantArray};
use unicode_normalization::UnicodeNormalization as _;

use self::rule::{MatchDiscipline, Prefixes, Rule};
use self::shell::CommandTarget;
use crate::util::path::{expand_absolute, lexical_normalize, requires_directory, slash_separated};
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
    /// Classifies calls the configured rules leave open, asking when approval is uncertain.
    #[default]
    Auto,
    /// Asks about every call the rules leave open.
    Manual,
    /// Read-only analysis: every edit, exec, and net call is denied.
    Plan,
    /// Runs every call no configured deny or ask rule holds.
    Yolo,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Auto => "Use a model to approve tool calls or ask for confirmation",
            Self::Manual => "Ask before tool calls that no permission rule allows or denies",
            Self::Plan => "Read-only: deny edits and commands",
            Self::Yolo => "Run tool calls unless a deny rule or your ask rules hold them",
        }
    }

    /// Whether the mode denies every call in `class`, whatever the allow and ask rules say.
    pub(crate) fn denies(self, class: PermissionClass) -> bool {
        self == Self::Plan
            && matches!(
                class,
                PermissionClass::Edit | PermissionClass::Exec | PermissionClass::Net
            )
    }
}

// ── Permission Class ──

/// The rule namespace a tool's calls fall under, which also sets the pipeline's default treatment.
/// Rules address classes, so one `read` deny pattern covers every tool that reads a file. Each tool
/// declares its class in its [`crate::tool::ToolSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
#[strum(serialize_all = "lowercase")]
pub enum PermissionClass {
    /// Reads files or acts only on state the session owns.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display)]
#[strum(serialize_all = "lowercase")]
pub enum Tier {
    Deny,
    Ask,
    Allow,
}

// ── Workspace ──

/// The launch directory and configured roots that anchor path rules and inform classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    /// The cwd followed by each distinct configured root, in configuration order.
    anchors: Vec<Anchor>,
}

/// One anchor directory in both of its spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Anchor {
    resolved: PathBuf,
    /// As configured or launched, lexically normalized.
    spelled: PathBuf,
}

impl Anchor {
    /// Resolves an absolute or `~/` extra root.
    pub(crate) fn resolve(raw: &str) -> Result<Self> {
        let spelled =
            expand_absolute(raw).with_context(|| format!("invalid extra root {raw:?}"))?;
        let resolved = match resolve(&spelled) {
            Ok(root) => root,
            Err(unreachable) => bail!("invalid extra root {raw:?}: {unreachable}"),
        };
        Ok(Self {
            resolved,
            spelled: lexical_normalize(&spelled),
        })
    }
}

impl Workspace {
    /// The workspace launched in `cwd` with no extra roots.
    pub(crate) fn new(cwd: &Path) -> Self {
        let resolved = resolve(cwd).ok();
        Self {
            cwd: cwd.to_path_buf(),
            roots: resolved.clone().into_iter().collect(),
            anchors: vec![Anchor {
                resolved: resolved.unwrap_or_else(|| cwd.to_path_buf()),
                spelled: lexical_normalize(cwd),
            }],
        }
    }

    #[cfg(test)]
    pub(crate) fn resolve(cwd: &Path, extra_roots: &[String]) -> Result<Self> {
        let roots = extra_roots
            .iter()
            .map(|raw| Anchor::resolve(raw))
            .collect::<Result<_>>()?;
        Ok(Self::with_roots(cwd, roots))
    }

    /// The workspace launched in `cwd` with additional rule anchors.
    pub(crate) fn with_roots(cwd: &Path, extra_roots: Vec<Anchor>) -> Self {
        let mut workspace = Self::new(cwd);
        for root in extra_roots {
            if !workspace.roots.contains(&root.resolved) {
                workspace.roots.push(root.resolved.clone());
                workspace.anchors.push(root);
            }
        }
        workspace
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The cwd as resolved, or as given when it cannot be.
    pub(crate) fn resolved_cwd(&self) -> &Path {
        &self.anchors[0].resolved
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
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

// ── Target ──

/// What a tool call accesses, matched against rule patterns, which [`crate::tool::Tool::prepare`]
/// resolves from its input once so the gate checks and the run act on the same values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Access {
    /// No extractable target, such as a missing path argument, so only a `*` rule matches.
    #[default]
    None,
    Command(CommandTarget),
    Path(PathTarget),
}

impl Access {
    /// Parses a `bash` command for explicit exec rules.
    pub(crate) fn for_command(command: &str) -> Self {
        Self::Command(CommandTarget::parse(command))
    }

    /// Builds a path target for the location the tool will touch, resolving `path` against the
    /// workspace's cwd with `resolve`. A path `resolve` cannot place keeps its joined spelling.
    /// Non-UTF-8 paths retain their exact spelling for the tool and a lossy spelling for rules.
    pub(crate) fn for_path(path: impl AsRef<Path>, workspace: &Workspace) -> Self {
        Self::Path(PathResolver::new(workspace).resolve(path))
    }

    /// Where a path target lands, exactly as the file system names it.
    pub(crate) fn resolved(&self) -> Option<PathBuf> {
        match self {
            Self::Path(path) => Some(path.resolved()),
            Self::None | Self::Command(_) => None,
        }
    }
}

/// A path in two spellings plus their forms relative to the anchor directories. Owned because a
/// resolved path is not a substring of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathTarget {
    /// Where the call lands with every symlink followed. Allow rules and the tool's run use this
    /// spelling alone.
    pub(crate) canonical: String,
    /// `canonical` as the file system names it, kept only when a name in it is not valid UTF-8,
    /// which `canonical` spells with replacement characters.
    exact: Option<PathBuf>,
    /// `canonical` under the resolved cwd, set only while it sits inside it.
    pub(crate) relative: Option<String>,
    /// `canonical` under each resolved anchor directory that holds it.
    rooted: Vec<String>,
    /// The path as the call names it, lexically normalized. Deny and ask rules also match it.
    requested: String,
    /// Either spelling relative to either spelling of each anchor directory, with names compared
    /// by [`fold_name`] so a case-variant prefix still counts. Only deny and ask rules match these.
    deny_rooted: Vec<String>,
}

impl PathTarget {
    /// Where this lands, exactly as the file system names it.
    pub(crate) fn resolved(&self) -> PathBuf {
        self.exact
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.canonical))
    }
}

/// Path resolution for one tool call, sharing directory metadata.
struct PathResolver<'a> {
    workspace: &'a Workspace,
    aliases: DirectoryAliases,
}

impl<'a> PathResolver<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
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
        let canonical = resolved.unwrap_or(joined);

        let lossy = canonical.to_str().is_none();
        PathTarget {
            canonical: canonical.to_string_lossy().into_owned(),
            exact: lossy.then_some(canonical),
            relative,
            rooted,
            requested: requested.to_string_lossy().into_owned(),
            deny_rooted,
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
    let name_count = |path: &Path| {
        path.components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count()
    };
    if name_count(existing) != name_count(long) {
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

fn folded_components(path: &Path) -> Vec<String> {
    path.components().map(folded_component).collect()
}

fn folded_component(component: Component<'_>) -> String {
    fold_name(&slash_separated(Path::new(component.as_os_str())))
}

/// Code points a file system may drop from a name, as HFS+ drops the one in `.g\u{200c}it`.
static DEFAULT_IGNORABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\p{Default_Ignorable_Code_Point}").expect("a Unicode property regex compiles")
});

/// One file name in the form deny and ask rules compare by: default-ignorable code
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

// ── Decision ──

/// The pipeline's verdict for one call. `Ask` goes to the frontend, which answers it (the gate
/// itself stays UI-agnostic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Classify,
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
    /// The automatic classifier's reason for requesting approval.
    Classifier { reason: String },
    /// An operation confined to the session's own state.
    Internal,
    /// The mode: `plan` denying an edit, exec, or net call, or settling a call no rule does.
    Mode(Mode),
    /// The rules allowing each simple command.
    Command(Vec<Basis>),
}

impl Basis {
    /// The sentence an approval prompt shows for a call this basis asks about, or `None` when the
    /// mode asks because no rule settles the call.
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Mode(_) => None,
            Self::Classifier { reason } => Some(reason.clone()),
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
            Self::Classifier { reason } => write!(f, "the automatic approval check: {reason}"),
            Self::Internal => f.write_str("a session-local operation"),
            Self::Mode(mode) => write!(f, "`{mode}` mode"),
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
    deny: Vec<TierRule>,
    ask: Vec<TierRule>,
    allow: Vec<Rule>,
    /// Rules the user granted from an approval for the rest of the session. In memory only.
    session_allow: Vec<Rule>,
    /// Whether a person can answer an approval request in this run.
    attended: bool,
}

/// A deny or ask rule with its literal path prefix located in the policy's workspace.
#[derive(Debug, Clone)]
struct TierRule {
    rule: Rule,
    /// The rule's [`Rule::prefix_bases`] from the workspace's anchor directories.
    bases: Vec<PathBuf>,
}

impl TierRule {
    /// Returns whether this rule holds back a call in `class` with `target`.
    fn matches(&self, class: PermissionClass, target: &Access) -> bool {
        self.rule.matches(class, target, self.discipline())
    }

    fn discipline(&self) -> MatchDiscipline<'_> {
        MatchDiscipline::Deny(Prefixes::Located(&self.bases))
    }

    /// The basis of a decision this rule settles from `tier`.
    fn basis(&self, tier: Tier) -> Basis {
        Basis::Rule {
            tier,
            rule: self.rule.clone(),
        }
    }
}

/// An auto-mode policy with no configured rules.
#[cfg(any(test, feature = "testing"))]
impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            deny: Vec::new(),
            ask: Vec::new(),
            allow: Vec::new(),
            session_allow: Vec::new(),
            attended: true,
        }
    }
}

impl Policy {
    /// Locates deny and ask prefixes under each workspace anchor.
    pub(crate) fn resolve(
        mode: Mode,
        deny: Vec<Rule>,
        ask: Vec<Rule>,
        allow: Vec<Rule>,
        workspace: &Workspace,
    ) -> Self {
        let anchors: Vec<PathBuf> = workspace
            .anchors
            .iter()
            .map(|anchor| anchor.spelled.clone())
            .collect();
        let entry = |rule: Rule| TierRule {
            bases: rule.prefix_bases(&anchors),
            rule,
        };
        let deny = deny.into_iter().map(entry).collect();
        let ask = ask.into_iter().map(entry).collect();
        Self {
            mode,
            deny,
            ask,
            allow,
            session_allow: Vec::new(),
            attended: true,
        }
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

    /// The narrowest session rule that could allow the call. An explicit deny or ask rule
    /// outranks session allowances, so a call it holds offers no grant.
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

    /// An allow pattern that would let an unattended call run.
    pub(crate) fn config_fix(&self, class: PermissionClass, target: &Access) -> Option<Rule> {
        self.session_grant(class, target)
            .map(|grant| grant.suggested)
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

    /// Resolves a call to a [`Verdict`], checking current deny and ask path prefixes.
    ///
    /// Order, stopping at the first match:
    /// 1. A deny rule denies, in every mode.
    /// 2. `plan` denies an edit, exec, or net call.
    /// 3. A configured ask rule asks.
    /// 4. An internal read operation, allow rule, or session allow rule allows.
    /// 5. `auto` classifies, `manual` asks, and `yolo` or a read-only `plan` call allows.
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

        if self.mode.denies(class) {
            return verdict(Decision::Deny, Basis::Mode(self.mode));
        }

        if let Some(rule) = held_by(&self.ask.iter(), class, target) {
            return verdict(Decision::Ask, rule.basis(Tier::Ask));
        }

        if class == PermissionClass::Read && matches!(target, Access::None) {
            return verdict(Decision::Allow, Basis::Internal);
        }

        if let Some(basis) = self.allows(class, target) {
            return verdict(Decision::Allow, basis);
        }

        let fallback = match self.mode {
            Mode::Auto => Decision::Classify,
            Mode::Manual => Decision::Ask,
            Mode::Yolo | Mode::Plan => Decision::Allow,
        };
        verdict(fallback, Basis::Mode(self.mode))
    }

    /// The allow rule or session allow rule that matches the call, or for a command what lets each
    /// of its parts through.
    fn allows(&self, class: PermissionClass, target: &Access) -> Option<Basis> {
        match target {
            Access::Command(command) => self.allows_command(command),
            _ => self.allow_rule(|rule| rule.matches(class, target, MatchDiscipline::Allow)),
        }
    }

    /// The allow rules covering every simple command in a recognized command.
    fn allows_command(&self, command: &CommandTarget) -> Option<Basis> {
        if let Some(basis) =
            self.allow_rule(|rule| rule.class() == PermissionClass::Exec && rule.pattern() == "*")
        {
            return Some(basis);
        }
        if !command.is_complete() || command.segments().is_empty() {
            return None;
        }
        let segments = command
            .segments()
            .iter()
            .map(|segment| self.allow_rule(|rule| rule.allows(segment)));
        segments.collect::<Option<Vec<_>>>().map(Basis::Command)
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

/// The first deny or ask rule that matches the call.
fn held_by<'a>(
    rules: &(impl Iterator<Item = &'a TierRule> + Clone),
    class: PermissionClass,
    target: &Access,
) -> Option<&'a TierRule> {
    rules.clone().find(|entry| entry.matches(class, target))
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
    use super::{Access, PermissionClass, Policy, SessionGrant};

    /// The session grant the default policy offers for running `command` in `/repo`.
    pub fn exec_grant(command: &str) -> SessionGrant {
        Policy::default()
            .session_grant(PermissionClass::Exec, &Access::for_command(command))
            .expect("the default policy offers a grant for the command")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::rule::escaped;
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

    /// A policy in `/repo` with the given tiers.
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
        Policy::resolve(mode, rules(deny), rules(ask), rules(allow), workspace)
    }

    fn command(s: &str) -> Access {
        Access::for_command(s)
    }

    /// A path target at `canonical`, spelled the same way, rooted at `relative` when given.
    fn path(canonical: &str, relative: Option<&str>) -> Access {
        let rooted: Vec<String> = relative.map(str::to_owned).into_iter().collect();
        Access::Path(PathTarget {
            canonical: canonical.to_owned(),
            exact: None,
            relative: relative.map(str::to_owned),
            rooted: rooted.clone(),
            requested: canonical.to_owned(),
            deny_rooted: rooted,
        })
    }

    fn workspace(canonical: &str, relative: &str) -> Access {
        path(canonical, Some(relative))
    }

    fn outside(canonical: &str) -> Access {
        path(canonical, None)
    }

    /// The canonical and relative spellings as paths, which compare equal across separators.
    fn resolved(target: &Access) -> (&Path, Option<&Path>) {
        let Access::Path(PathTarget {
            canonical,
            relative,
            ..
        }) = target
        else {
            panic!("expected a path target, got {target:?}");
        };
        (Path::new(canonical), relative.as_deref().map(Path::new))
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
        fn parses_each_name_through_serde_and_fromstr() {
            for &mode in Mode::VARIANTS {
                let token = mode.to_string();
                assert_eq!(
                    serde_json::from_value::<Mode>(serde_json::Value::String(token.clone()))
                        .unwrap(),
                    mode
                );
                assert_eq!(token.parse::<Mode>().unwrap(), mode);
            }
        }

        mod from_str {
            use super::*;

            #[test]
            fn names_every_valid_value_for_an_unknown_one() {
                let err = "bypass".parse::<Mode>().unwrap_err().to_string();
                assert_eq!(err, "expected one of auto, manual, plan, yolo");
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
            fn requires_an_absolute_or_tilde_extra_root() {
                let repo = canonical_tempdir();

                let err = Workspace::resolve(repo.path(), &["shared".to_owned()]).unwrap_err();

                assert_eq!(
                    format!("{err:#}"),
                    r#"invalid extra root "shared": expected an absolute or `~/` path"#
                );
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
            fn keeps_explicit_extra_roots_when_launched_from_home() {
                let dir = canonical_tempdir();
                let home = dir.path().join("home");
                let shared = dir.path().join("shared");
                std::fs::create_dir(&home).unwrap();
                std::fs::create_dir(&shared).unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let workspace = Workspace::resolve(&home, &[slash_separated(&shared)]).unwrap();
                    let p = policy_in(Mode::Manual, &[], &[], &[(READ, "./docs/**")], &workspace);
                    let target = Access::for_path(shared.join("docs/notes.md"), &workspace);

                    assert_eq!(p.decide(READ, &target), Decision::Allow);
                    assert_eq!(workspace.roots(), [home.clone(), shared.clone()]);
                });
            }

            #[test]
            fn explicit_home_root_and_ancestor_anchors_match_configured_path_rules() {
                let dir = canonical_tempdir();
                let base = dir.path();
                let home = base.join("home");
                let repo = base.join("repo");
                std::fs::create_dir(&home).unwrap();
                std::fs::create_dir(&repo).unwrap();
                let home_link = base.join("home-link");
                symlink_dir(&home, &home_link);
                let fs_root = base.ancestors().last().unwrap();

                temp_env::with_var(HOME_VAR, Some(&home), || {
                    for (raw, root) in [
                        (slash_separated(&home), home.as_path()),
                        ("~/".to_owned(), home.as_path()),
                        (slash_separated(base), base),
                        (slash_separated(fs_root), fs_root),
                        (slash_separated(&home_link), home.as_path()),
                    ] {
                        let workspace =
                            Workspace::resolve(&repo, std::slice::from_ref(&raw)).unwrap();
                        let p =
                            policy_in(Mode::Manual, &[], &[], &[(READ, "./docs/**")], &workspace);
                        let target = Access::for_path(root.join("docs/notes.md"), &workspace);

                        assert_eq!(p.decide(READ, &target), Decision::Allow, "{raw}");
                        assert!(
                            workspace.roots().iter().any(|anchor| anchor == root),
                            "{raw}"
                        );
                    }
                });
            }
        }
    }

    mod access {
        use super::*;

        mod for_path {
            use super::*;

            #[test]
            fn inside_an_extra_root_is_rooted_there() {
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let (repo, shared) = (base.join("repo"), base.join("shared"));
                std::fs::create_dir(&repo).unwrap();
                std::fs::create_dir(&shared).unwrap();
                let workspace = Workspace::resolve(&repo, &[slash_separated(&shared)]).unwrap();

                for (raw, rooted) in [
                    ("../shared/a.rs", &["a.rs"][..]),
                    ("../shared/.git/config", &[".git/config"]),
                    ("../elsewhere/a.rs", &[]),
                ] {
                    let target = Access::for_path(raw, &workspace);
                    let (canonical, relative) = resolved(&target);
                    assert_eq!(canonical, lexical_normalize(&repo.join(raw)), "{raw}");
                    assert_eq!(relative, None, "{raw}: outside the cwd");

                    let Access::Path(PathTarget { rooted: actual, .. }) = &target else {
                        unreachable!();
                    };
                    let actual: Vec<&Path> = actual.iter().map(Path::new).collect();
                    let rooted: Vec<&Path> = rooted.iter().map(Path::new).collect();
                    assert_eq!(actual, rooted, "{raw}");
                }
            }

            #[test]
            fn inside_cwd_has_a_relative_pattern_target() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                std::fs::write(cwd.join("a.rs"), "x").unwrap();

                let target = Access::for_path("a.rs", &Workspace::new(cwd));
                let (canonical, relative) = resolved(&target);
                assert_eq!(canonical, cwd.join("a.rs"));
                assert_eq!(relative, Some(Path::new("a.rs")));
            }

            #[test]
            fn brand_new_file_keeps_its_cwd_relative_path() {
                // A new file cannot canonicalize, but its missing tail still lands inside cwd.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let target = Access::for_path("new/file.rs", &Workspace::new(cwd));
                assert_eq!(
                    resolved(&target),
                    (
                        cwd.join("new/file.rs").as_path(),
                        Some(Path::new("new/file.rs")),
                    ),
                );
            }

            #[test]
            fn outside_cwd_has_no_relative_component() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let target = Access::for_path("/etc/hosts", &Workspace::new(cwd));
                let (_, relative) = resolved(&target);
                assert_eq!(relative, None);
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
                        (root.join("escape.rs").as_path(), None),
                        "{raw}",
                    );
                }
            }

            #[test]
            fn non_absolute_cwd_has_no_relative_pattern_target() {
                // An empty cwd has no root, and `strip_prefix("")` succeeds for any path, so the
                // target must get no cwd-relative form either.
                let target = Access::for_path("a.rs", &Workspace::new(Path::new("")));
                let (_, relative) = resolved(&target);
                assert_eq!(relative, None);
            }

            #[test]
            fn relative_cwd_parent_traversal_preserves_permission_decisions() {
                let policy = policy(Mode::Manual, &[], &[], &[(EDIT, "src/**")]);
                let absolute_cwd = std::env::current_dir().unwrap();

                for cwd in ["..", "../..", "missing/../.."] {
                    for raw in ["notes.md", ".git/config", "src/a.rs"] {
                        let target = Access::for_path(raw, &Workspace::new(Path::new(cwd)));
                        let absolute =
                            Access::for_path(raw, &Workspace::new(&absolute_cwd.join(cwd)));
                        let (_, relative) = resolved(&target);
                        let (_, absolute_relative) = resolved(&absolute);

                        assert_eq!(relative, absolute_relative, "{cwd:?}/{raw}");
                        assert_eq!(
                            policy.decide(EDIT, &target),
                            policy.decide(EDIT, &absolute),
                            "{cwd:?}/{raw}",
                        );
                    }
                }
            }

            #[test]
            fn deep_existing_file_keeps_its_cwd_relative_path() {
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
                    (&*cwd.join(&file), Some(&*file)),
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
                assert_eq!(resolved(&target), (outside.join("new.rs").as_path(), None),);
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
                assert_eq!(resolved(&target), (root.join("escape.txt").as_path(), None),);
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
                    (root.join("elsewhere/dir/new.rs").as_path(), None,),
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
                assert_eq!(resolved(&target), (outside.join("x").as_path(), None),);

                let target = Access::for_path("x/../tools/pre-commit", &Workspace::new(cwd));
                let p = policy_in(
                    Mode::Manual,
                    &[],
                    &[(EDIT, "./.git/**")],
                    &[(EDIT, "*")],
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
                assert_eq!(resolved(&target), (outside.join("new").as_path(), None),);
            }

            #[test]
            fn symlink_loop_has_no_relative_pattern_target() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                symlink_file(Path::new("b"), &cwd.join("a"));
                symlink_file(Path::new("a"), &cwd.join("b"));

                let target = Access::for_path("a/new.rs", &Workspace::new(cwd));
                let (_, relative) = resolved(&target);
                assert_eq!(relative, None);
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

                    assert_eq!(resolved(&target), (&*cwd.join(&path), None), "{path:?}",);
                }
            }

            // A Unix name is raw bytes, so only there can one fail to be UTF-8, as `caf\xe9` does.
            #[cfg(unix)]
            #[test]
            fn preserves_non_utf8_paths_and_their_lossy_pattern_spellings() {
                use std::ffi::OsStr;
                use std::os::unix::ffi::OsStrExt as _;
                let base_dir = canonical_tempdir();
                let base = base_dir.path();
                let cwd = base.join(OsStr::from_bytes(b"caf\xe9"));

                let target = Access::for_path("notes.md", &Workspace::new(&cwd));

                assert_eq!(target.resolved(), Some(cwd.join("notes.md")));
                let (canonical, _) = resolved(&target);
                assert_eq!(
                    canonical,
                    Path::new(&*cwd.join("notes.md").to_string_lossy())
                );
            }

            // Windows drops `..` lexically before a lookup, so only Unix stops at a file before it.
            #[cfg(unix)]
            #[test]
            fn dot_dot_past_a_file_is_not_resolved_lexically() {
                // The kernel cannot step back out of a file.
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let outside_dir = canonical_tempdir();
                let outside = outside_dir.path();
                symlink_dir(outside, &cwd.join("link"));
                std::fs::write(cwd.join("notes"), "").unwrap();

                for path in ["notes/../x", "notes/sub/../../link/x"] {
                    let target = Access::for_path(path, &Workspace::new(cwd));

                    assert_eq!(resolved(&target), (&*cwd.join(path), None), "{path}",);
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
                let (path, relative) = resolved(&target);
                assert_eq!(path.as_os_str(), cwd.join(r".git\config").as_os_str());
                assert_eq!(relative, Some(Path::new(r".git\config")));
                let p = policy_in(
                    Mode::Manual,
                    &[(EDIT, "secrets-store/key")],
                    &[],
                    &[],
                    &Workspace::new(cwd),
                );
                let target = Access::for_path(r"secrets-store\key::$DATA", &Workspace::new(cwd));
                assert_eq!(p.decide(EDIT, &target), Decision::Deny);

                // The stream suffix names the same home directory.
                let home = cwd.join("home-directory");
                std::fs::create_dir(&home).unwrap();
                temp_env::with_var(HOME_VAR, Some(&home), || {
                    let home_alias = cwd.join("home-directory::$INDEX_ALLOCATION");
                    let target = Access::for_path("notes.md", &Workspace::new(&home_alias));
                    assert_eq!(
                        resolved(&target),
                        (&*home.join("notes.md"), Some(Path::new("notes.md")),),
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
                (
                    Basis::Classifier {
                        reason: "Needs approval".to_owned(),
                    },
                    "the automatic approval check: Needs approval",
                ),
                (Basis::Internal, "a session-local operation"),
                (Basis::Mode(Mode::Plan), "`plan` mode"),
                (
                    Basis::Command(vec![Basis::Session(make)]),
                    "the session `allow.exec` pattern `make *`",
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
                    Basis::Classifier {
                        reason: "Needs approval".to_owned()
                    }
                    .reason()
                    .as_deref(),
                    Some("Needs approval"),
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
                let p = policy_in(
                    Mode::Manual,
                    &[(READ, "secrets/**")],
                    &[(READ, &alias)],
                    &[],
                    &Workspace::new(&cwd),
                );
                for (raw, decision) in [
                    ("../vault/key", Decision::Deny),
                    ("../real/x", Decision::Ask),
                    ("../other/x", Decision::Ask),
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

                    let p = policy_in(Mode::Yolo, &[(READ, &pattern)], &[], &[], &workspace);
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
                    let p = policy_in(Mode::Yolo, &[(READ, pattern)], &[], &[], &workspace);
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
                let p = policy_in(Mode::Yolo, &[(READ, &pattern)], &[], &[], &workspace);

                let target = Access::for_path(root.join("vault/key"), &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Allow);
                assert!(
                    !p.read_filter()
                        .walk(&root.join("vault"), &root.join("vault"), &workspace)
                        .withholds(&root.join("vault/key"))
                );
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
                let p = policy_in(Mode::Yolo, &[(READ, &pattern)], &[], &[], &workspace);

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
                let p = policy_in(Mode::Yolo, &[(READ, "./secrets/**")], &[], &[], &workspace);

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
                let p = policy_in(Mode::Yolo, &[(READ, "./key")], &[], &[], &workspace);

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
                let p = policy_in(Mode::Yolo, &[(EDIT, "./secrets/key")], &[], &[], &workspace);

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
                                    let p = policy_in(mode, &deny, &ask, &[], &workspace);
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
                let p = policy_in(Mode::Manual, &[], &[], &[(EDIT, &pattern)], &workspace);

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
                let p = policy_in(Mode::Yolo, &[(READ, &pattern)], &[], &[], &workspace);
                let alias = Path::new("/System/Volumes/Data").join(root.strip_prefix("/").unwrap());

                symlink_dir(Path::new("missing"), &root.join("secrets"));
                let target = Access::for_path(alias.join("secrets/key"), &workspace);

                assert_eq!(p.decide(READ, &target), Decision::Deny);
            }
        }

        mod session_grant {
            use super::*;

            #[test]
            fn suggests_a_rule_that_allows_the_call() {
                let p = policy(Mode::Manual, &[], &[], &[]);
                let target = command("RUST_LOG=debug cargo test");

                let grant = p.session_grant(EXEC, &target).unwrap();
                assert_eq!(grant.suggested().pattern(), "RUST_LOG=debug cargo test");
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

                assert!(p.session_grant(EXEC, &command("git push origin")).is_none());
                assert!(p.session_grant(EXEC, &command("ls *.rs")).is_none());
            }
        }

        mod config_fix {
            use super::*;

            #[test]
            fn suggests_the_allow_rule_for_an_unmatched_call() {
                let p = policy(Mode::Manual, &[], &[], &[]);

                let fix = p.config_fix(EXEC, &command("cargo test"));

                assert_eq!(fix, Some(Rule::parse(EXEC, "cargo test").unwrap()));
            }
        }

        mod read_filter {
            use super::*;

            #[test]
            fn a_class_wide_read_hold_filters_every_file() {
                let root_dir = canonical_tempdir();
                let root = root_dir.path();
                let workspace = Workspace::new(root);
                let p = policy_in(Mode::Yolo, &[(READ, "*")], &[], &[], &workspace);

                let filter = p.read_filter();
                let walk = filter.walk(root, root, &workspace);

                for name in ["src/main.rs", "notes.md"] {
                    assert!(walk.withholds(&root.join(name)), "{name}");
                }
            }

            #[test]
            fn withholds_exactly_the_files_matched_by_explicit_read_rules() {
                let cwd_dir = canonical_tempdir();
                let cwd = cwd_dir.path();
                let secrets = slash_separated(&cwd.join("secrets"));
                let p = policy_in(
                    Mode::Manual,
                    &[(READ, ".env"), (EDIT, "notes.md")],
                    &[(READ, &secrets)],
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
                        held.unwrap_or(Decision::Ask),
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
            fn ask_rule_outranks_allow_and_session_grants() {
                let mut p = policy(
                    Mode::Manual,
                    &[],
                    &[(EDIT, "Cargo.lock"), (EXEC, "git push *")],
                    &[(EDIT, "*"), (EXEC, "*")],
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
            fn unmatched_paths_and_commands_follow_the_mode_without_builtin_policy() {
                for (mode, read, edit, exec) in [
                    (
                        Mode::Auto,
                        Decision::Classify,
                        Decision::Classify,
                        Decision::Classify,
                    ),
                    (Mode::Manual, Decision::Ask, Decision::Ask, Decision::Ask),
                    (
                        Mode::Yolo,
                        Decision::Allow,
                        Decision::Allow,
                        Decision::Allow,
                    ),
                    (Mode::Plan, Decision::Allow, Decision::Deny, Decision::Deny),
                ] {
                    let p = policy(mode, &[], &[], &[]);

                    for target in [
                        workspace("/repo/.git/config", ".git/config"),
                        outside(&abs("/home/u/.ssh/key")),
                    ] {
                        assert_eq!(p.decide(READ, &target), read, "{mode}: {target:?}");
                        assert_eq!(p.decide(EDIT, &target), edit, "{mode}: {target:?}");
                    }
                    assert_eq!(p.decide(EXEC, &command("rm -rf build")), exec, "{mode}");
                }
            }

            #[test]
            fn session_local_reads_run_unless_explicitly_held() {
                for &mode in Mode::VARIANTS {
                    let p = policy(mode, &[(READ, "*")], &[(READ, "*")], &[(READ, "*")]);
                    assert_eq!(p.decide(READ, &Access::None), Decision::Deny, "{mode}");
                    let p = policy(mode, &[], &[(READ, "*")], &[(READ, "*")]);
                    assert_eq!(p.decide(READ, &Access::None), Decision::Ask, "{mode}");
                    let p = policy(mode, &[], &[], &[]);
                    assert_eq!(
                        p.verdict(READ, &Access::None),
                        Verdict {
                            decision: Decision::Allow,
                            basis: Basis::Internal
                        },
                        "{mode}"
                    );
                }
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
                    ("../shared/src/secrets/key", READ, Decision::Ask),
                    ("../shared/generated/x.rs", EDIT, Decision::Ask),
                    ("../shared/src/x.rs", EDIT, Decision::Ask),
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
            fn plan_denies_every_edit_exec_and_net_call_and_allows_unmatched_reads() {
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
                    p.decide(PermissionClass::Net, &Access::None),
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
                    (EDIT, path(&abs("/repo/.git/config"), Some(".git/config"))),
                    (EXEC, command("git push origin")),
                ] {
                    assert_eq!(p.decide(class, &target), Decision::Deny, "{target:?}");
                }
            }

            #[test]
            fn exec_star_allows_unsupported_calls_after_explicit_exec_holds() {
                for mode in [Mode::Auto, Mode::Manual, Mode::Yolo] {
                    for session in [false, true] {
                        let mut p = policy(
                            mode,
                            &[(EXEC, "git push *")],
                            &[(EXEC, "git commit *")],
                            if session { &[] } else { &[(EXEC, "*")] },
                        );
                        if session {
                            p.allow_for_session(Rule::parse(EXEC, "*").unwrap());
                        }

                        for (case, expected) in [
                            ("cargo build && cargo test", Decision::Allow),
                            ("$RUN task", Decision::Allow),
                            ("cargo test > output", Decision::Allow),
                            ("if true; then cargo test; fi", Decision::Allow),
                            ("git push origin main > output", Decision::Deny),
                            ("git commit -m update > output", Decision::Ask),
                        ] {
                            assert_eq!(
                                p.decide(EXEC, &command(case)),
                                expected,
                                "{mode} session={session}: {case}"
                            );
                        }
                    }
                }
            }

            #[test]
            fn patterned_exec_allow_requires_every_literal_component() {
                let p = policy(Mode::Auto, &[], &[], &[(EXEC, "cargo *")]);

                for (case, expected) in [
                    ("cargo build && cargo test", Decision::Allow),
                    ("cargo test | cargo report", Decision::Allow),
                    ("cargo build && printf done", Decision::Classify),
                    ("printf ready | cargo test", Decision::Classify),
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }
            }

            #[test]
            fn dynamic_or_unsupported_calls_follow_the_selected_mode() {
                for (mode, expected) in [
                    (Mode::Auto, Decision::Classify),
                    (Mode::Manual, Decision::Ask),
                    (Mode::Yolo, Decision::Allow),
                ] {
                    let p = policy(mode, &[], &[], &[(EXEC, "cargo *")]);

                    for case in [
                        "$RUN task",
                        "cargo test $FLAGS",
                        "cargo test *.rs",
                        "cargo test > output",
                        "if true; then cargo test; fi",
                    ] {
                        assert_eq!(p.decide(EXEC, &command(case)), expected, "{mode}: {case}");
                    }
                }
            }

            #[test]
            fn wrapper_names_are_matched_as_the_actual_commands() {
                let p = policy(Mode::Auto, &[], &[], &[(EXEC, "cat *")]);

                for (case, expected) in [
                    ("cat notes", Decision::Allow),
                    ("FOO=1 cat notes", Decision::Classify),
                    ("env cat notes", Decision::Classify),
                    ("nice cat notes", Decision::Classify),
                    ("sh -c 'cat notes'", Decision::Classify),
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), expected, "{case}");
                }
                let env = policy(Mode::Auto, &[], &[], &[(EXEC, "env *")]);
                assert_eq!(env.decide(EXEC, &command("env cat notes")), Decision::Allow);
            }

            #[test]
            fn path_rules_hold_native_calls_without_projecting_into_bash() {
                let p = policy(
                    Mode::Auto,
                    &[(READ, "./private")],
                    &[(EDIT, "./private")],
                    &[(EXEC, "*")],
                );
                let target = workspace("/repo/private/key", "private/key");

                assert_eq!(p.decide(READ, &target), Decision::Deny);
                assert_eq!(p.decide(EDIT, &target), Decision::Ask);
                for case in [
                    "cat private/key",
                    "cat < private/key",
                    "echo value > private/key",
                    "sed -i s/a/b/ private/key",
                    "HOME=private bash -c true",
                ] {
                    assert_eq!(p.decide(EXEC, &command(case)), Decision::Allow, "{case}");
                }
                let specific = policy(
                    Mode::Auto,
                    &[(READ, "./private"), (EDIT, "./private")],
                    &[],
                    &[(EXEC, "cat *"), (EXEC, "sed *")],
                );
                for case in ["cat private/key", "sed -i s/a/b/ private/key"] {
                    assert_eq!(
                        specific.decide(EXEC, &command(case)),
                        Decision::Allow,
                        "{case}"
                    );
                }
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
            fn names_the_configured_rule_or_mode_that_settles_each_call() {
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
                        (Decision::Ask, Basis::Mode(Mode::Manual)),
                    ),
                    (
                        EDIT,
                        workspace("/repo/a.rs", "a.rs"),
                        (Decision::Ask, Basis::Mode(Mode::Manual)),
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
                let mut p = policy(
                    Mode::Manual,
                    &[],
                    &[],
                    &[(EXEC, "cargo test *"), (EXEC, "wc *")],
                );
                p.allow_for_session(make.clone());

                let settled = verdict(&p, EXEC, &command("cargo test && make all | wc -l"));

                assert_eq!(
                    settled,
                    (
                        Decision::Allow,
                        Basis::Command(vec![
                            rule_basis(Tier::Allow, EXEC, "cargo test *"),
                            Basis::Session(make),
                            rule_basis(Tier::Allow, EXEC, "wc *"),
                        ]),
                    ),
                );
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
                    verdict(&yolo, EDIT, &Access::None),
                    (Decision::Allow, Basis::Mode(Mode::Yolo)),
                    "an unmatched edit follows the selected mode",
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
                        (Decision::Unapproved, Basis::Mode(Mode::Manual)),
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
                let p = policy_in(
                    Mode::Manual,
                    &[(READ, ".env")],
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
                let p = policy_in(
                    Mode::Manual,
                    &[],
                    &[(READ, "link/**")],
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
