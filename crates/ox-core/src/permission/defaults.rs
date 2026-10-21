//! The rules oxide-code ships, in named groups that `[permission.defaults]` turns off. Most groups
//! name files whose edits ask, the `rm` group asks about forced recursive deletes, and the rest
//! add patterns to the deny or ask tier.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::permission::rule::{Rule, escaped};
use crate::permission::{PermissionClass, folded_names, resolve};
use crate::util::path::{XdgBase, app_dir, lexical_normalize, xdg_dir};

// ── Shipped ──

/// A named group of shipped rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shipped {
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
    Cloud,
    Credentials,
}

impl Shipped {
    pub(super) const ALL: [Self; 12] = [
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
        Self::Cloud,
        Self::Credentials,
    ];

    /// The group's name, its key under `[permission.defaults]`.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Direnv => "direnv",
            Self::Editors => "editors",
            Self::Cargo => "cargo",
            Self::Shell => "shell",
            Self::Agents => "agents",
            Self::Ox => "ox",
            Self::Devices => "devices",
            Self::Rm => "rm",
            Self::Ssh => "ssh",
            Self::Cloud => "cloud",
            Self::Credentials => "credentials",
        }
    }

    /// The names this group asks about when edited, or `None` for any other group.
    const fn names(self) -> Option<&'static Names> {
        match self {
            Self::Git => Some(&GIT),
            Self::Direnv => Some(&DIRENV),
            Self::Editors => Some(&EDITORS),
            Self::Cargo => Some(&CARGO),
            Self::Shell => Some(&SHELL),
            Self::Agents => Some(&AGENTS),
            Self::Ox => Some(&OX),
            Self::Devices | Self::Rm | Self::Ssh | Self::Cloud | Self::Credentials => None,
        }
    }

    /// The tier the group holds calls back in: deny for the secret files, ask for the rest.
    pub const fn tier(self) -> ShippedTier {
        match self {
            Self::Ssh | Self::Credentials => ShippedTier::Deny,
            Self::Git
            | Self::Direnv
            | Self::Editors
            | Self::Cargo
            | Self::Shell
            | Self::Agents
            | Self::Ox
            | Self::Devices
            | Self::Rm
            | Self::Cloud => ShippedTier::Ask,
        }
    }

    /// The rules of a group of patterns, or `None` for any other group.
    pub(super) fn rules(self) -> Result<Option<Vec<Rule>>> {
        let home = std::env::home_dir;
        let pattern = |path: PathBuf| escaped(&lexical_normalize(&path));
        let (class, patterns): (PermissionClass, Vec<String>) = match self {
            Self::Devices => (
                PermissionClass::Edit,
                if cfg!(unix) {
                    vec!["/dev/**".to_owned()]
                } else {
                    Vec::new()
                },
            ),
            Self::Ssh => (
                PermissionClass::Read,
                home()
                    .map(|home| pattern(home.join(".ssh")))
                    .into_iter()
                    .collect(),
            ),
            Self::Credentials => (
                PermissionClass::Read,
                [
                    app_dir(XdgBase::Data).map(|dir| dir.join("credentials.json")),
                    home().map(|home| home.join(".claude").join(".credentials.json")),
                    home().map(|home| home.join(".git-credentials")),
                    xdg_dir(XdgBase::Config).map(|dir| dir.join("git").join("credentials")),
                ]
                .into_iter()
                .flatten()
                .map(pattern)
                .collect(),
            ),
            Self::Git
            | Self::Direnv
            | Self::Editors
            | Self::Cargo
            | Self::Shell
            | Self::Agents
            | Self::Ox
            | Self::Rm
            | Self::Cloud => return Ok(None),
        };
        let rules = patterns
            .iter()
            .map(|pattern| {
                Rule::parse(class, pattern)
                    .with_context(|| format!("invalid built-in `{self}` pattern {pattern:?}"))
            })
            .collect::<Result<_>>()?;
        Ok(Some(rules))
    }
}

impl fmt::Display for Shipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The tier a shipped group's rules join. A shipped default only holds calls back, so none joins
/// the allow tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShippedTier {
    Deny,
    Ask,
}

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

/// The files a shell runs when it starts or a login shell exits, and fish's autoloaded config and
/// functions.
const SHELL: Names = Names {
    dirs: &[],
    files: &[
        ".bash_aliases",
        ".bash_login",
        ".bash_logout",
        ".bash_profile",
        ".bashrc",
        ".cshrc",
        ".kshrc",
        ".login",
        ".logout",
        ".mkshrc",
        ".profile",
        ".tcshrc",
        ".zlogin",
        ".zlogout",
        ".zprofile",
        ".zshenv",
        ".zshrc",
    ],
    nested_dirs: &[("fish", "conf.d"), ("fish", "functions")],
    nested_files: &[("fish", "config.fish")],
};

/// Codex's project config, Claude Code's hooks and the settings that register them, and the
/// files that register MCP servers it starts. The rest of `.claude`, such as worktrees and plans,
/// stays editable.
const AGENTS: Names = Names {
    dirs: &[".codex"],
    files: &[".claude.json", ".mcp.json"],
    nested_dirs: &[(".claude", "hooks")],
    nested_files: &[
        (".claude", "settings.json"),
        (".claude", "settings.local.json"),
    ],
};

/// oxide-code's project config. [`protected_roots`] adds its own directories, where an edit could
/// rewrite its config or session history.
const OX: Names = Names {
    dirs: &[".ox"],
    files: &[],
    nested_dirs: &[],
    nested_files: &[],
};

impl Names {
    /// Returns whether the folded `names` of a path hold one of these names where it takes effect.
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

/// The protected locations whose insides belong to a group wherever their names sit: the
/// [`home_locations`], plus where each protected name directly under `cwd` or a workspace root
/// resolves, so a `.vscode` symlinked to `tools/vscode` protects its target too.
pub(super) fn protected_roots(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<(Shipped, PathBuf)> {
    let extra = workspace_roots.iter().filter(|root| *root != cwd);
    let bases: Vec<&Path> = std::iter::once(cwd)
        .chain(extra.map(PathBuf::as_path))
        .collect();
    let mut roots: Vec<(Shipped, PathBuf)> = home_locations();
    for group in Shipped::ALL {
        let Some(names) = group.names() else {
            continue;
        };
        for name in names.paths() {
            roots.extend(
                bases
                    .iter()
                    .filter_map(|base| resolve(&base.join(&name)).ok())
                    .map(|path| (group, path)),
            );
        }
    }
    roots
}

/// oxide-code's own config, data, and state directories, the XDG git directory and the git
/// credential store, `~/.ssh`, whose `authorized_keys`, `rc`, and `environment` set what a login
/// runs, and the cloud CLIs' config directories, whose credential helpers run commands. Each comes
/// with its group, resolved like a target so a symlinked home or XDG base still compares equal.
fn home_locations() -> Vec<(Shipped, PathBuf)> {
    let app_dirs = XdgBase::ALL
        .into_iter()
        .filter_map(app_dir)
        .map(|dir| (Shipped::Ox, dir));
    let git_dir = xdg_dir(XdgBase::Config).map(|dir| (Shipped::Git, dir.join("git")));
    let home = std::env::home_dir();
    let in_home = home.iter().flat_map(|home| {
        [
            (Shipped::Git, ".git-credentials"),
            (Shipped::Ssh, ".ssh"),
            (Shipped::Cloud, ".aws"),
            (Shipped::Cloud, ".azure"),
            (Shipped::Cloud, ".config/gcloud"),
            (Shipped::Cloud, ".docker"),
            (Shipped::Cloud, ".kube"),
        ]
        .map(|(group, name)| (group, home.join(name)))
    });
    app_dirs
        .chain(git_dir)
        .chain(in_home)
        .map(|(group, path)| (group, resolve(&path).unwrap_or(path)))
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
    use crate::util::path::local_testing::abs;
    use crate::util::path::slash_separated;
    use crate::util::path::testing::HOME_VAR;

    mod shipped {
        use super::*;

        #[test]
        fn puts_each_pattern_group_in_its_tier_and_class() {
            temp_env::with_vars(
                [
                    (HOME_VAR, Some(abs("/home/u"))),
                    ("XDG_DATA_HOME", Some(abs("/data"))),
                    ("XDG_CONFIG_HOME", Some(abs("/config"))),
                ],
                || {
                    let patterns = |group: Shipped| {
                        let rules = group.rules().unwrap().unwrap();
                        let classes: Vec<PermissionClass> = rules.iter().map(Rule::class).collect();
                        let patterns: Vec<String> =
                            rules.iter().map(|rule| rule.pattern().to_owned()).collect();
                        (group.tier(), classes, patterns)
                    };
                    let home = slash_separated(Path::new(&abs("/home/u")));
                    let data = slash_separated(Path::new(&abs("/data")));
                    let config = slash_separated(Path::new(&abs("/config")));
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
                            vec![PermissionClass::Read; 4],
                            vec![
                                format!("{data}/ox/credentials.json"),
                                format!("{home}/.claude/.credentials.json"),
                                format!("{home}/.git-credentials"),
                                format!("{config}/git/credentials"),
                            ]
                        )
                    );
                    assert!(Shipped::Git.rules().unwrap().is_none());
                    assert!(Shipped::Rm.rules().unwrap().is_none());
                    assert!(Shipped::Cloud.rules().unwrap().is_none());
                },
            );
        }

        mod as_str {
            use super::*;

            #[test]
            fn names_each_group_by_its_defaults_key() {
                for group in Shipped::ALL {
                    assert_eq!(
                        toml::Value::try_from(group).unwrap().as_str(),
                        Some(group.as_str())
                    );
                }
            }
        }

        mod rules {
            use super::*;

            #[test]
            fn name_the_home_directory_however_its_variable_spells_it() {
                let home = abs("/home/u");
                let expected = slash_separated(Path::new(&home));
                for spelling in [
                    format!("{home}/"),
                    format!("{home}/."),
                    format!("{home}/x/.."),
                ] {
                    let patterns = temp_env::with_vars(
                        [
                            (HOME_VAR, Some(spelling.as_str())),
                            ("XDG_DATA_HOME", None),
                            ("XDG_CONFIG_HOME", None),
                        ],
                        || {
                            [Shipped::Ssh, Shipped::Credentials].map(|group| {
                                let rules = group.rules().unwrap().unwrap();
                                rules
                                    .iter()
                                    .map(|rule| rule.pattern().to_owned())
                                    .collect::<Vec<_>>()
                            })
                        },
                    );
                    assert_eq!(
                        patterns,
                        [
                            vec![format!("{expected}/.ssh")],
                            vec![
                                format!("{expected}/.local/share/ox/credentials.json"),
                                format!("{expected}/.claude/.credentials.json"),
                                format!("{expected}/.git-credentials"),
                                format!("{expected}/.config/git/credentials"),
                            ],
                        ],
                        "{spelling}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    mod protected_roots {
        use super::*;

        #[test]
        fn tags_where_each_symlinked_name_resolves_with_its_group() {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dunce::canonicalize(dir.path()).unwrap();
            std::fs::create_dir_all(cwd.join("tools/vscode")).unwrap();
            std::os::unix::fs::symlink("tools/vscode", cwd.join(".vscode")).unwrap();

            let roots = protected_roots(&cwd, std::slice::from_ref(&cwd));

            assert!(roots.contains(&(Shipped::Editors, cwd.join("tools/vscode"))));
            assert!(!roots.iter().any(
                |(group, path)| *group != Shipped::Editors && *path == cwd.join("tools/vscode")
            ));
        }
    }

    mod protected_by {
        use super::*;

        fn groups(raw: &str) -> Vec<Shipped> {
            protected_by(&[Path::new(raw)], &[])
        }

        #[test]
        fn names_the_group_of_each_protected_name_at_any_depth_and_spelling() {
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
                ("/repo/.zlogout", Shipped::Shell),
                ("/repo/.bash_logout", Shipped::Shell),
                ("/repo/.bash_aliases", Shipped::Shell),
                ("/repo/.cshrc", Shipped::Shell),
                ("/repo/.tcshrc", Shipped::Shell),
                ("/repo/.kshrc", Shipped::Shell),
                ("/repo/.mkshrc", Shipped::Shell),
                ("/repo/.login", Shipped::Shell),
                ("/repo/.logout", Shipped::Shell),
                ("/home/u/.config/fish/config.fish", Shipped::Shell),
                ("/home/u/.config/fish/conf.d/env.fish", Shipped::Shell),
                ("/home/u/.config/fish/functions/ls.fish", Shipped::Shell),
                ("/repo/.codex/config.toml", Shipped::Agents),
                ("/repo/.claude/settings.json", Shipped::Agents),
                ("/repo/.claude/settings.local.json", Shipped::Agents),
                ("/repo/.claude/hooks/format.sh", Shipped::Agents),
                ("/home/u/.claude/settings.json", Shipped::Agents),
                ("/home/u/.claude.json", Shipped::Agents),
                ("/repo/.mcp.json", Shipped::Agents),
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
        fn leaves_lookalike_names_unprotected() {
            for raw in [
                "/repo/.github/workflows/ci.yml",
                "/repo/src/git/mod.rs",
                "/repo/cargo/config.toml",
                "/repo/.cargo/registry/x.rs",
                "/repo/docs/.zshrc.md",
                "/repo/box/config.toml",
                "/repo/.claude/worktrees/feat/src/main.rs",
                "/repo/.claude/plans/next.md",
                "/repo/.claude/settings.json/notes.md",
                "/repo/hooks/format.sh",
                "/repo/fish/notes.md",
                "/repo/docs/config.fish",
            ] {
                assert_eq!(groups(raw), [], "{raw}");
            }
        }

        #[test]
        fn covers_paths_under_a_root_case_insensitively() {
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
}
