//! The rules oxide-code ships, in named groups that `[permission.defaults]` turns off. Each group
//! adds patterns to the deny or ask tier, except `rm`, which asks about forced recursive deletes.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use strum::{Display, VariantArray};

use super::PermissionClass;
use super::rule::{Rule, escaped};
use crate::util::path::{
    CLAUDE_CREDENTIALS, XdgBase, app_dir, claude_dir, lexical_normalize, xdg_dir,
};

// ── Shipped ──

/// The Linux files that hold each process's environment, with every secret it started with. A path
/// through `/proc/self` or `/proc/thread-self` resolves to one of these. They ship on every Unix,
/// like `/dev/**`, and match nothing where `/proc` has no such files.
const PROCESS_ENVIRONMENTS: [&str; 2] = ["/proc/*/environ", "/proc/*/task/*/environ"];

/// A named group of shipped rules.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Display,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
    /// Returns whether the group guards secrets that no config change should offer to expose.
    pub(super) const fn guards_secrets(self) -> bool {
        matches!(self, Self::Ssh | Self::Credentials)
    }

    /// The group's rules, each with the tier it joins. Empty for `rm`.
    pub(super) fn rules(self) -> Result<Vec<(ShippedTier, Rule)>> {
        let mut rules = Vec::new();
        for name in self.names() {
            for pattern in [format!("/**/{name}"), format!("./{name}")] {
                let rule = Rule::parse(PermissionClass::Edit, &pattern)
                    .with_context(|| format!("invalid built-in `{self}` pattern {pattern:?}"))?;
                rules.push((ShippedTier::Ask, rule));
            }
        }
        let tiers = [
            (ShippedTier::Ask, PermissionClass::Edit, self.edit_asks()),
            (ShippedTier::Deny, PermissionClass::Read, self.read_denies()),
        ];
        for (tier, class, patterns) in tiers {
            for pattern in patterns {
                let rule = Rule::parse(class, &pattern)
                    .with_context(|| format!("invalid built-in `{self}` pattern {pattern:?}"))?;
                rules.push((tier, rule));
            }
        }
        Ok(rules)
    }

    /// The names this group protects, with their descendants, wherever they sit.
    const fn names(self) -> &'static [&'static str] {
        match self {
            Self::Git => GIT,
            Self::Direnv => DIRENV,
            Self::Editors => EDITORS,
            Self::Cargo => CARGO,
            Self::Shell => SHELL,
            Self::Agents => AGENTS,
            Self::Ox => OX,
            Self::Devices | Self::Rm | Self::Ssh | Self::Cloud | Self::Credentials => &[],
        }
    }

    /// The patterns whose edits this group asks about: oxide-code's own config, data, and state
    /// directories, the XDG git directory and the git credential store, Claude Code's user settings
    /// and hooks wherever `CLAUDE_CONFIG_DIR` moves them, `~/.ssh`, whose `authorized_keys`, `rc`,
    /// and `environment` set what a login runs, the cloud CLIs' config directories, whose
    /// credential helpers run commands, and on Unix the device files.
    fn edit_asks(self) -> Vec<String> {
        let home = std::env::home_dir;
        let in_home = |names: &[&str]| -> Vec<PathBuf> {
            home()
                .map(|home| names.iter().map(|name| home.join(name)).collect())
                .unwrap_or_default()
        };
        let paths: Vec<PathBuf> = match self {
            Self::Git => xdg_dir(XdgBase::Config)
                .map(|dir| dir.join("git"))
                .into_iter()
                .chain(in_home(&[".git-credentials"]))
                .collect(),
            Self::Agents => claude_dir()
                .into_iter()
                .flat_map(|dir| ["settings.json", "hooks"].map(|name| dir.join(name)))
                .collect(),
            Self::Ox => XdgBase::VARIANTS
                .iter()
                .copied()
                .filter_map(app_dir)
                .collect(),
            Self::Ssh => in_home(&[".ssh"]),
            Self::Cloud => in_home(&[".aws", ".azure", ".config/gcloud", ".docker", ".kube"]),
            Self::Devices => {
                return if cfg!(unix) {
                    vec!["/dev/**".to_owned()]
                } else {
                    Vec::new()
                };
            }
            Self::Direnv
            | Self::Editors
            | Self::Cargo
            | Self::Shell
            | Self::Rm
            | Self::Credentials => Vec::new(),
        };
        paths.iter().map(|path| path_pattern(path)).collect()
    }

    /// The patterns whose reads this group denies: `~/.ssh` and the files that hold credentials.
    fn read_denies(self) -> Vec<String> {
        let home = std::env::home_dir;
        match self {
            Self::Ssh => home()
                .map(|home| path_pattern(&home.join(".ssh")))
                .into_iter()
                .collect(),
            Self::Credentials => [
                app_dir(XdgBase::Data).map(|dir| dir.join("credentials.json")),
                claude_dir().map(|dir| dir.join(CLAUDE_CREDENTIALS)),
                home().map(|home| home.join(".git-credentials")),
                xdg_dir(XdgBase::Config).map(|dir| dir.join("git").join("credentials")),
            ]
            .into_iter()
            .flatten()
            .map(|path| path_pattern(&path))
            .chain(
                PROCESS_ENVIRONMENTS
                    .iter()
                    .filter(|_| cfg!(unix))
                    .map(|&pattern| pattern.to_owned()),
            )
            .collect(),
            Self::Git
            | Self::Direnv
            | Self::Editors
            | Self::Cargo
            | Self::Shell
            | Self::Agents
            | Self::Ox
            | Self::Devices
            | Self::Rm
            | Self::Cloud => Vec::new(),
        }
    }
}

/// The pattern that names exactly `path`, lexically normalized, and everything below it.
fn path_pattern(path: &std::path::Path) -> String {
    escaped(&lexical_normalize(path))
}

/// The tier a shipped rule joins. A shipped default only holds calls back, so none joins the allow
/// tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShippedTier {
    Deny,
    Ask,
}

// ── Protected Names ──

/// VCS metadata, hook managers, and git config.
const GIT: &[&str] = &[".git", ".githooks", ".husky", ".gitconfig", ".gitmodules"];

const DIRENV: &[&str] = &[".envrc"];

/// Editor and dev-container config, which can run tasks when the project opens.
const EDITORS: &[&str] = &[".devcontainer", ".idea", ".vscode"];

/// Cargo config, which can install a `runner` or `rustc-wrapper` that executes on the next build,
/// and the rustup toolchain pin.
const CARGO: &[&str] = &[
    "rust-toolchain",
    "rust-toolchain.toml",
    ".cargo/config",
    ".cargo/config.toml",
];

/// The startup and logout files of each shell, and fish's autoloaded config and functions.
const SHELL: &[&str] = &[
    "fish/conf.d",
    "fish/functions",
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
    "fish/config.fish",
];

/// Codex's project config, Claude Code's hooks, the settings that register them, and its MCP
/// server list. The rest of `.claude`, such as worktrees and plans, stays editable.
const AGENTS: &[&str] = &[
    ".codex",
    ".claude/hooks",
    ".claude.json",
    ".mcp.json",
    ".claude/settings.json",
    ".claude/settings.local.json",
];

/// oxide-code's project config. [`Shipped::edit_asks`] adds its own directories, where an edit
/// could rewrite its config or session history.
const OX: &[&str] = &[".ox"];

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::permission::rule::{MatchDiscipline, Prefixes};
    use crate::permission::{PathTarget, Zone};
    use crate::util::path::local_testing::abs;
    use crate::util::path::slash_separated;
    use crate::util::path::testing::HOME_VAR;

    /// The process-environment patterns `credentials` adds on Unix, spelled out.
    const PROC_ENVIRONS: [&str; 2] = ["/proc/*/environ", "/proc/*/task/*/environ"];

    /// Each rule of `group` as its tier, class, and pattern.
    fn patterns(group: Shipped) -> Vec<(ShippedTier, PermissionClass, String)> {
        group
            .rules()
            .unwrap()
            .iter()
            .map(|(tier, rule)| (*tier, rule.class(), rule.pattern().to_owned()))
            .collect()
    }

    /// The groups whose shipped ask rules hold an edit of `raw`, a resolved absolute path.
    fn protected_by(raw: &str) -> Vec<Shipped> {
        let target = PathTarget {
            canonical: raw.to_owned(),
            exact: None,
            relative: None,
            rooted: Vec::new(),
            requested: raw.to_owned(),
            deny_rooted: Vec::new(),
            zone: Zone::Outside,
        };
        Shipped::VARIANTS
            .iter()
            .copied()
            .filter(|group| {
                group.rules().unwrap().iter().any(|(tier, rule)| {
                    let bases = rule.prefix_bases(&[]);
                    let discipline = MatchDiscipline::Deny(Prefixes::Located(&bases));
                    *tier == ShippedTier::Ask
                        && rule.matches_path(PermissionClass::Edit, &target, discipline)
                })
            })
            .collect()
    }

    mod shipped {
        use super::*;

        #[test]
        fn puts_each_rule_in_its_tier_and_class() {
            temp_env::with_vars(
                [
                    (HOME_VAR, Some(abs("/home/u"))),
                    ("XDG_DATA_HOME", Some(abs("/data"))),
                    ("XDG_CONFIG_HOME", Some(abs("/config"))),
                    ("CLAUDE_CONFIG_DIR", None),
                ],
                || {
                    let home = slash_separated(Path::new(&abs("/home/u")));
                    let data = slash_separated(Path::new(&abs("/data")));
                    let config = slash_separated(Path::new(&abs("/config")));
                    let ask_edit =
                        |pattern: String| (ShippedTier::Ask, PermissionClass::Edit, pattern);
                    let deny_read =
                        |pattern: String| (ShippedTier::Deny, PermissionClass::Read, pattern);

                    let names = [".git", ".githooks", ".husky", ".gitconfig", ".gitmodules"];
                    let git: Vec<_> = names
                        .iter()
                        .flat_map(|name| [format!("/**/{name}"), format!("./{name}")])
                        .chain([format!("{config}/git"), format!("{home}/.git-credentials")])
                        .map(ask_edit)
                        .collect();
                    assert_eq!(patterns(Shipped::Git), git);
                    assert_eq!(
                        patterns(Shipped::Devices),
                        if cfg!(unix) {
                            vec![ask_edit("/dev/**".to_owned())]
                        } else {
                            Vec::new()
                        }
                    );
                    assert_eq!(patterns(Shipped::Rm), []);
                    assert_eq!(
                        patterns(Shipped::Ssh),
                        [
                            ask_edit(format!("{home}/.ssh")),
                            deny_read(format!("{home}/.ssh"))
                        ]
                    );
                    let cloud = [".aws", ".azure", ".config/gcloud", ".docker", ".kube"];
                    assert_eq!(
                        patterns(Shipped::Cloud),
                        cloud.map(|name| ask_edit(format!("{home}/{name}")))
                    );
                    let mut credentials = vec![
                        format!("{data}/ox/credentials.json"),
                        format!("{home}/.claude/.credentials.json"),
                        format!("{home}/.git-credentials"),
                        format!("{config}/git/credentials"),
                    ];
                    if cfg!(unix) {
                        credentials.extend(PROC_ENVIRONS.map(str::to_owned));
                    }
                    assert_eq!(
                        patterns(Shipped::Credentials),
                        credentials.into_iter().map(deny_read).collect::<Vec<_>>()
                    );
                },
            );
        }

        #[test]
        fn names_each_group_by_its_defaults_key() {
            for &group in Shipped::VARIANTS {
                assert_eq!(
                    toml::Value::try_from(group).unwrap().as_str(),
                    Some(group.to_string().as_str())
                );
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
                            ("CLAUDE_CONFIG_DIR", None),
                        ],
                        || {
                            [Shipped::Ssh, Shipped::Credentials].map(|group| {
                                patterns(group)
                                    .into_iter()
                                    .filter(|(_, class, _)| *class == PermissionClass::Read)
                                    .map(|(_, _, pattern)| pattern)
                                    .collect::<Vec<_>>()
                            })
                        },
                    );
                    let mut credentials = vec![
                        format!("{expected}/.local/share/ox/credentials.json"),
                        format!("{expected}/.claude/.credentials.json"),
                        format!("{expected}/.git-credentials"),
                        format!("{expected}/.config/git/credentials"),
                    ];
                    if cfg!(unix) {
                        credentials.extend(PROC_ENVIRONS.map(str::to_owned));
                    }
                    assert_eq!(
                        patterns,
                        [vec![format!("{expected}/.ssh")], credentials],
                        "{spelling}"
                    );
                }
            }

            #[test]
            fn name_claude_codes_login_inside_claude_config_dir() {
                let dir = abs("/srv/claude");

                let patterns = temp_env::with_var("CLAUDE_CONFIG_DIR", Some(dir.as_str()), || {
                    patterns(Shipped::Credentials)
                });

                let expected = format!("{}/.credentials.json", slash_separated(Path::new(&dir)));
                assert_eq!(patterns[1].2, expected);
            }
        }
    }

    mod protected_names {
        use super::*;

        #[test]
        fn name_the_group_of_each_protected_name_at_any_depth_and_spelling() {
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
                assert_eq!(protected_by(raw), [group], "{raw}");
            }
            assert_eq!(
                protected_by("/repo/.git/.vscode/.envrc"),
                [Shipped::Git, Shipped::Direnv, Shipped::Editors]
            );
        }

        #[test]
        fn protect_descendants_of_directories_named_like_config_files() {
            for (raw, group) in [
                ("/repo/.bashrc/notes.md", Shipped::Shell),
                ("/repo/.BASHRC/notes.md", Shipped::Shell),
                ("/repo/.cargo/config/notes.md", Shipped::Cargo),
                ("/repo/.claude/settings.json/notes.md", Shipped::Agents),
            ] {
                assert_eq!(protected_by(raw), [group], "{raw}");
            }
        }

        #[test]
        fn leave_lookalike_names_unprotected() {
            for raw in [
                "/repo/.github/workflows/ci.yml",
                "/repo/src/git/mod.rs",
                "/repo/cargo/config.toml",
                "/repo/.cargo/registry/x.rs",
                "/repo/docs/.zshrc.md",
                "/repo/box/config.toml",
                "/repo/.claude/worktrees/feat/src/main.rs",
                "/repo/.claude/plans/next.md",
                "/repo/hooks/format.sh",
                "/repo/fish/notes.md",
                "/repo/docs/config.fish",
            ] {
                assert_eq!(protected_by(raw), [], "{raw}");
            }
        }

        #[test]
        fn cover_paths_under_an_app_directory_case_insensitively() {
            let config = abs("/home/u/.config");
            temp_env::with_var("XDG_CONFIG_HOME", Some(&config), || {
                for raw in [
                    "/home/u/.config/ox/config.toml",
                    "/home/u/.CONFIG/Ox/config.toml",
                    "/home/u/.con\u{fb01}g/ox/config.toml",
                ] {
                    assert_eq!(protected_by(&abs(raw)), [Shipped::Ox], "{raw}");
                }
                assert_eq!(protected_by(&abs("/home/u/.config/oxen/config.toml")), []);
            });
        }
    }
}
