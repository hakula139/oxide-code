//! The rules oxide-code ships, in named groups that `[permission.defaults]` turns off. Most groups
//! name files whose edits ask, and the rest add patterns to the deny or ask tier.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::permission::rule::{Rule, escaped};
use crate::permission::{PermissionClass, folded_names, resolve};
use crate::util::path::{XdgBase, app_dir};

// ── Shipped ──

/// A named group of shipped rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Shipped {
    Git,
    Direnv,
    Editors,
    Cargo,
    Shell,
    Agents,
    Ox,
    Devices,
    Rm,
    Ssh,
    Credentials,
}

impl Shipped {
    pub(super) const ALL: [Self; 11] = [
        Self::Git,
        Self::Direnv,
        Self::Editors,
        Self::Cargo,
        Self::Shell,
        Self::Agents,
        Self::Ox,
        Self::Devices,
        Self::Rm,
        Self::Ssh,
        Self::Credentials,
    ];

    /// The names this group asks about when edited, or `None` for a group of patterns.
    const fn names(self) -> Option<&'static Names> {
        match self {
            Self::Git => Some(&GIT),
            Self::Direnv => Some(&DIRENV),
            Self::Editors => Some(&EDITORS),
            Self::Cargo => Some(&CARGO),
            Self::Shell => Some(&SHELL),
            Self::Agents => Some(&AGENTS),
            Self::Ox => Some(&OX),
            Self::Devices | Self::Rm | Self::Ssh | Self::Credentials => None,
        }
    }

    /// Whether this group protects names, so the policy weighs it through a path's protected
    /// groups.
    pub(super) const fn protects(self) -> bool {
        self.names().is_some()
    }

    /// The tier and rules of a group of patterns, or `None` for a group of protected names.
    pub(super) fn rules(self) -> Result<Option<(ShippedTier, Vec<Rule>)>> {
        let home = || std::env::home_dir().map(|home| escaped(&home));
        let (tier, class, patterns): (ShippedTier, PermissionClass, Vec<String>) = match self {
            Self::Devices => (
                ShippedTier::Ask,
                PermissionClass::Edit,
                if cfg!(unix) {
                    vec!["/dev/**".to_owned()]
                } else {
                    Vec::new()
                },
            ),
            Self::Rm => (
                ShippedTier::Ask,
                PermissionClass::Exec,
                RM_PATTERNS
                    .iter()
                    .map(|&pattern| pattern.to_owned())
                    .collect(),
            ),
            Self::Ssh => (
                ShippedTier::Deny,
                PermissionClass::Read,
                home()
                    .map(|home| format!("{home}/.ssh"))
                    .into_iter()
                    .collect(),
            ),
            Self::Credentials => (
                ShippedTier::Deny,
                PermissionClass::Read,
                [
                    app_dir(XdgBase::Data).map(|dir| escaped(&dir.join("credentials.json"))),
                    home().map(|home| format!("{home}/.claude/.credentials.json")),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ),
            Self::Git
            | Self::Direnv
            | Self::Editors
            | Self::Cargo
            | Self::Shell
            | Self::Agents
            | Self::Ox => return Ok(None),
        };
        let rules = patterns
            .iter()
            .map(|pattern| Rule::parse(class, pattern))
            .collect::<Result<_>>()?;
        Ok(Some((tier, rules)))
    }
}

/// The tier a group of shipped patterns joins. A shipped default only holds calls back, so none
/// joins the allow tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShippedTier {
    Deny,
    Ask,
}

/// The common spellings of a forced recursive `rm`. Deny and ask patterns compare words folded, so
/// `-Rf` and `-RF` match `-rf`.
const RM_PATTERNS: &[&str] = &[
    "rm -rf *",
    "rm -fr *",
    "rm -r -f *",
    "rm -f -r *",
    "rm --recursive --force *",
    "rm --force --recursive *",
];

// ── Protected Names ──

/// The names one group protects, each compared by [`fold_name`](super::fold_name).
struct Names {
    /// Directories protected at any depth.
    dirs: &'static [&'static str],
    /// File names protected wherever they sit.
    files: &'static [&'static str],
    /// `(parent, dir)` pairs protected at any depth only together.
    nested_dirs: &'static [(&'static str, &'static str)],
    /// `(parent, file)` pairs protected only together.
    nested_files: &'static [(&'static str, &'static str)],
}

/// VCS metadata, hook managers, and git config.
const GIT: Names = Names {
    dirs: &[".git", ".githooks", ".husky"],
    files: &[".gitconfig", ".gitmodules"],
    nested_dirs: &[],
    nested_files: &[],
};

const DIRENV: Names = Names {
    dirs: &[],
    files: &[".envrc"],
    nested_dirs: &[],
    nested_files: &[],
};

/// Editor and dev-container config, which can run tasks when the project opens.
const EDITORS: Names = Names {
    dirs: &[".devcontainer", ".idea", ".vscode"],
    files: &[],
    nested_dirs: &[],
    nested_files: &[],
};

/// Cargo config, which can install a `runner` or `rustc-wrapper` that executes on the next build,
/// and the rustup toolchain pin.
const CARGO: Names = Names {
    dirs: &[],
    files: &["rust-toolchain", "rust-toolchain.toml"],
    nested_dirs: &[],
    nested_files: &[(".cargo", "config"), (".cargo", "config.toml")],
};

/// Shell startup files.
const SHELL: Names = Names {
    dirs: &[],
    files: &[
        ".bash_login",
        ".bash_profile",
        ".bashrc",
        ".profile",
        ".zlogin",
        ".zprofile",
        ".zshenv",
        ".zshrc",
    ],
    nested_dirs: &[],
    nested_files: &[],
};

/// Codex's project config, plus Claude Code's hooks and the settings that register them. The rest
/// of `.claude`, such as worktrees and plans, stays editable.
const AGENTS: Names = Names {
    dirs: &[".codex"],
    files: &[],
    nested_dirs: &[(".claude", "hooks")],
    nested_files: &[
        (".claude", "settings.json"),
        (".claude", "settings.local.json"),
    ],
};

/// oxide-code's project config and the legacy `ox.toml`. [`protected_roots`] adds its own
/// directories, where an edit could rewrite its config or session history.
const OX: Names = Names {
    dirs: &[".ox"],
    files: &["ox.toml"],
    nested_dirs: &[],
    nested_files: &[],
};

impl Names {
    /// Whether the folded `names` of a path hold one of these names where it takes effect.
    fn hold(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.dirs.contains(name))
            || names.last().is_some_and(|name| self.files.contains(name))
            || names
                .windows(2)
                .any(|pair| self.nested_dirs.contains(&(pair[0], pair[1])))
            || names
                .windows(2)
                .last()
                .is_some_and(|pair| self.nested_files.contains(&(pair[0], pair[1])))
    }

    /// Every protected name relative to the directory it sits in.
    fn paths(&self) -> impl Iterator<Item = PathBuf> {
        self.dirs.iter().chain(self.files).map(PathBuf::from).chain(
            self.nested_dirs
                .iter()
                .chain(self.nested_files)
                .map(|(parent, name)| Path::new(parent).join(name)),
        )
    }
}

/// The protected locations whose insides belong to a group wherever their names sit: oxide-code's
/// own directories, plus where each protected name directly under `cwd` or a workspace root
/// resolves, so a `.vscode` symlinked to `tools/vscode` protects its target too.
pub(super) fn protected_roots(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<(Shipped, PathBuf)> {
    let extra = workspace_roots.iter().filter(|root| *root != cwd);
    let bases: Vec<&Path> = std::iter::once(cwd)
        .chain(extra.map(PathBuf::as_path))
        .collect();
    let mut roots: Vec<(Shipped, PathBuf)> = app_dirs()
        .into_iter()
        .map(|dir| (Shipped::Ox, dir))
        .collect();
    for group in Shipped::ALL {
        let Some(names) = group.names() else {
            continue;
        };
        for name in names.paths() {
            roots.extend(
                bases
                    .iter()
                    .filter_map(|base| resolve(&base.join(&name)))
                    .map(|path| (group, path)),
            );
        }
    }
    roots
}

/// oxide-code's own config, data, and state directories, resolved like a target so a symlinked
/// XDG base still compares equal.
fn app_dirs() -> Vec<PathBuf> {
    XdgBase::ALL
        .into_iter()
        .filter_map(app_dir)
        .map(|dir| resolve(&dir).unwrap_or(dir))
        .collect()
}

/// The groups whose protected names one of `paths` holds, or whose [`protected_roots`] it sits
/// under, each once and in [`Shipped::ALL`] order. Names compare by
/// [`fold_name`](super::fold_name), since a case-insensitive file system such as APFS opens
/// `.VSCODE` and `.vſcode` as `.vscode`.
pub(super) fn protected_by(paths: &[&Path], roots: &[(Shipped, PathBuf)]) -> Vec<Shipped> {
    let folded: Vec<Vec<String>> = paths.iter().map(|path| folded_names(path)).collect();
    let roots: Vec<(Shipped, Vec<String>)> = roots
        .iter()
        .map(|(group, root)| (*group, folded_names(root)))
        .collect();
    Shipped::ALL
        .into_iter()
        .filter(|group| {
            folded.iter().any(|folded| {
                let names: Vec<&str> = folded.iter().map(String::as_str).collect();
                group.names().is_some_and(|own| own.hold(&names))
                    || roots
                        .iter()
                        .any(|(owner, root)| owner == group && folded.starts_with(root))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::path::slash_separated;
    use crate::util::path::testing::{HOME_VAR, abs};

    fn groups(raw: &str) -> Vec<Shipped> {
        protected_by(&[Path::new(raw)], &[])
    }

    // ── Shipped::rules ──

    #[test]
    fn rules_put_each_pattern_group_in_its_tier_and_class() {
        temp_env::with_vars(
            [
                (HOME_VAR, Some(abs("/home/u"))),
                ("XDG_DATA_HOME", Some(abs("/data"))),
            ],
            || {
                let patterns = |group: Shipped| {
                    let (tier, rules) = group.rules().unwrap().unwrap();
                    let classes: Vec<PermissionClass> = rules.iter().map(Rule::class).collect();
                    let patterns: Vec<String> =
                        rules.iter().map(|rule| rule.pattern().to_owned()).collect();
                    (tier, classes, patterns)
                };
                let home = slash_separated(Path::new(&abs("/home/u")));
                let data = slash_separated(Path::new(&abs("/data")));
                assert_eq!(
                    patterns(Shipped::Devices),
                    (
                        ShippedTier::Ask,
                        if cfg!(unix) {
                            vec![PermissionClass::Edit]
                        } else {
                            Vec::new()
                        },
                        if cfg!(unix) {
                            vec!["/dev/**".to_owned()]
                        } else {
                            Vec::new()
                        },
                    )
                );
                assert_eq!(
                    patterns(Shipped::Rm),
                    (
                        ShippedTier::Ask,
                        vec![PermissionClass::Exec; RM_PATTERNS.len()],
                        RM_PATTERNS
                            .iter()
                            .map(|&pattern| pattern.to_owned())
                            .collect()
                    )
                );
                assert_eq!(
                    patterns(Shipped::Ssh),
                    (
                        ShippedTier::Deny,
                        vec![PermissionClass::Read],
                        vec![format!("{home}/.ssh")]
                    )
                );
                assert_eq!(
                    patterns(Shipped::Credentials),
                    (
                        ShippedTier::Deny,
                        vec![PermissionClass::Read; 2],
                        vec![
                            format!("{data}/ox/credentials.json"),
                            format!("{home}/.claude/.credentials.json"),
                        ]
                    )
                );
                assert!(Shipped::Git.rules().unwrap().is_none());
            },
        );
    }

    // ── protected_roots ──

    #[cfg(unix)]
    #[test]
    fn protected_roots_tags_where_each_symlinked_name_resolves_with_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(cwd.join("tools/vscode")).unwrap();
        std::os::unix::fs::symlink("tools/vscode", cwd.join(".vscode")).unwrap();

        let roots = protected_roots(&cwd, std::slice::from_ref(&cwd));

        assert!(roots.contains(&(Shipped::Editors, cwd.join("tools/vscode"))));
        assert!(
            !roots.iter().any(
                |(group, path)| *group != Shipped::Editors && *path == cwd.join("tools/vscode")
            )
        );
    }

    // ── protected_by ──

    #[test]
    fn protected_by_names_the_group_of_each_protected_name_at_any_depth_and_spelling() {
        for (raw, group) in [
            ("/repo/.git/hooks/pre-commit", Shipped::Git),
            ("/repo/vendor/sub/.GIT/config", Shipped::Git),
            ("/repo/.git", Shipped::Git),
            ("/repo/.gitmodules", Shipped::Git),
            ("/repo/.gitconfig", Shipped::Git),
            ("/repo/.husky/pre-commit", Shipped::Git),
            ("/repo/.githooks/pre-push", Shipped::Git),
            ("/repo/.envrc", Shipped::Direnv),
            ("/repo/.vscode/tasks.json", Shipped::Editors),
            ("/repo/.idea/workspace.xml", Shipped::Editors),
            ("/repo/.devcontainer/devcontainer.json", Shipped::Editors),
            ("/repo/.cargo/config", Shipped::Cargo),
            ("/repo/crates/x/.Cargo/config.toml", Shipped::Cargo),
            ("/repo/rust-toolchain", Shipped::Cargo),
            ("/repo/rust-toolchain.toml", Shipped::Cargo),
            ("/repo/.bashrc", Shipped::Shell),
            ("/repo/.bash_profile", Shipped::Shell),
            ("/repo/.bash_login", Shipped::Shell),
            ("/repo/.profile", Shipped::Shell),
            ("/repo/.zshrc", Shipped::Shell),
            ("/repo/.zshenv", Shipped::Shell),
            ("/repo/.zprofile", Shipped::Shell),
            ("/repo/.zlogin", Shipped::Shell),
            ("/repo/.codex/config.toml", Shipped::Agents),
            ("/repo/.claude/settings.json", Shipped::Agents),
            ("/repo/.claude/settings.local.json", Shipped::Agents),
            ("/repo/.claude/hooks/format.sh", Shipped::Agents),
            ("/home/u/.claude/settings.json", Shipped::Agents),
            ("/repo/sub/OX.toml", Shipped::Ox),
            ("/repo/sub/.OX/config.toml", Shipped::Ox),
            ("/repo/.cargo/con\u{fb01}g.toml", Shipped::Cargo),
            ("/repo/.v\u{17f}code/tasks.json", Shipped::Editors),
            ("/repo/.gitmodule\u{17f}", Shipped::Git),
            ("/repo/.git./config", Shipped::Git),
            ("/repo/.vscode /tasks.json", Shipped::Editors),
            ("/repo/.g\u{200b}it/config", Shipped::Git),
            ("/repo/.git\u{feff}/hooks/pre-commit", Shipped::Git),
        ] {
            assert_eq!(groups(raw), [group], "{raw}");
        }
        assert_eq!(
            groups("/repo/.git/.vscode/.envrc"),
            [Shipped::Git, Shipped::Direnv, Shipped::Editors]
        );
    }

    #[test]
    fn protected_by_leaves_lookalike_names_unprotected() {
        for raw in [
            "/repo/.github/workflows/ci.yml",
            "/repo/src/git/mod.rs",
            "/repo/cargo/config.toml",
            "/repo/.cargo/registry/x.rs",
            "/repo/docs/.zshrc.md",
            "/repo/ox.toml.bak",
            "/repo/box/config.toml",
            "/repo/.claude/worktrees/feat/src/main.rs",
            "/repo/.claude/plans/next.md",
            "/repo/.claude/settings.json/notes.md",
            "/repo/hooks/format.sh",
        ] {
            assert_eq!(groups(raw), [], "{raw}");
        }
    }

    #[test]
    fn protected_by_covers_paths_under_a_root_case_insensitively() {
        let roots = [(Shipped::Ox, PathBuf::from("/home/u/.config/ox"))];
        for raw in [
            "/home/u/.config/ox/config.toml",
            "/home/u/.CONFIG/Ox/config.toml",
            "/home/u/.con\u{fb01}g/ox/config.toml",
        ] {
            assert_eq!(
                protected_by(&[Path::new(raw)], &roots),
                [Shipped::Ox],
                "{raw}"
            );
        }
        assert_eq!(
            protected_by(&[Path::new("/home/u/.config/oxen/config.toml")], &roots),
            []
        );
    }
}
