//! Runtime environment detection for system prompt context.

use std::path::Path;

use indoc::formatdoc;
use platform_info::{PlatformInfo, PlatformInfoAPI, UNameAPI};
use serde::{Deserialize, Serialize};
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::model::ModelSpec;
use crate::util::env;
use crate::util::time::local_offset;

// ── Environment ──

/// The machine and directory a session runs in, which the system prompt's environment section
/// describes. Sessions record it so a resumed prompt keeps its prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(Default))]
pub(super) struct Environment {
    cwd: String,
    is_git: bool,
    platform: String,
    shell: String,
    os_version: String,
}

impl Environment {
    /// Best-effort detection, with a fallback value for each probe that fails.
    pub(super) fn detect(cwd: &Path, git_root: Option<&Path>) -> Self {
        let is_git = git_root.is_some();
        let platform = normalize_node_platform(std::env::consts::OS).to_owned();
        let shell = detect_shell();
        let os_version = detect_os_version();

        Self {
            cwd: cwd.to_string_lossy().into_owned(),
            is_git,
            platform,
            shell,
            os_version,
        }
    }

    pub(super) fn describes(&self, cwd: &Path) -> bool {
        self.cwd == cwd.to_string_lossy()
    }

    pub(super) fn render(&self) -> String {
        let Self {
            cwd,
            is_git,
            platform,
            shell,
            os_version,
        } = self;
        formatdoc! {"
            # Environment

            You have been invoked in the following environment:

            - Primary working directory: {cwd}
              - Is a git repository: {is_git}
            - Platform: {platform}
            - Shell: {shell}
            - OS Version: {os_version}"
        }
    }
}

// ── Platform Detection ──

/// Rust's `std::env::consts::OS` → Node's `process.platform` values.
fn normalize_node_platform(os: &str) -> &'static str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        "linux" => "linux",
        _ => "unknown",
    }
}

// ── Shell Detection ──

fn detect_shell() -> String {
    let shell = env::string("SHELL").unwrap_or_else(|| "unknown".to_owned());
    Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&shell)
        .to_owned()
}

// ── OS Version Detection ──

fn detect_os_version() -> String {
    let Ok(info) = PlatformInfo::new() else {
        return std::env::consts::OS.to_owned();
    };
    format_os_version(
        &info.sysname().to_string_lossy(),
        &info.release().to_string_lossy(),
    )
}

/// `sysname release` as `uname` reports them, or the target OS name when `sysname` is empty.
fn format_os_version(sysname: &str, release: &str) -> String {
    if sysname.is_empty() {
        std::env::consts::OS.to_owned()
    } else {
        format!("{sysname} {release}")
    }
}

// ── Date Detection ──

pub(super) fn current_date() -> String {
    format_local_date(OffsetDateTime::now_utc(), local_offset())
}

fn format_local_date(now: OffsetDateTime, offset: UtcOffset) -> String {
    now.to_offset(offset)
        .date()
        .format(format_description!("[year]-[month]-[day]"))
        .expect("static `[year]-[month]-[day]` description never fails on a valid date")
}

// ── Model Identity ──

/// Names `model`, its exact ID, and its knowledge cutoff when the catalog knows the model.
pub(super) fn model_identity(model: &ModelSpec) -> String {
    let id = model.id();
    let Some(info) = model.info() else {
        return format!("You are powered by the model {id}.");
    };
    format!(
        "You are powered by the model named {}. \
         The exact model ID is {id}. \
         Assistant knowledge cutoff is {}.",
        info.display_name, info.cutoff,
    )
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use time::macros::{datetime, offset};

    use super::*;

    mod detect {
        use super::*;

        #[test]
        fn records_the_cwd_whether_it_is_in_a_repo_and_each_probe() {
            let cwd = Path::new("/repo/sub");
            let uname = PlatformInfo::new().unwrap();
            for (git_root, is_git) in [(Some(Path::new("/repo")), true), (None, false)] {
                let env = temp_env::with_var("SHELL", Some("/bin/zsh"), || {
                    Environment::detect(cwd, git_root)
                });

                assert_eq!(
                    env,
                    Environment {
                        cwd: cwd.to_string_lossy().into_owned(),
                        is_git,
                        platform: normalize_node_platform(std::env::consts::OS).to_owned(),
                        shell: "zsh".to_owned(),
                        os_version: format_os_version(
                            &uname.sysname().to_string_lossy(),
                            &uname.release().to_string_lossy(),
                        ),
                    },
                );
            }
        }
    }

    mod render {
        use super::*;

        #[test]
        fn lists_every_field() {
            let env = Environment {
                cwd: "/home/user/project".to_owned(),
                is_git: true,
                platform: "linux".to_owned(),
                shell: "bash".to_owned(),
                os_version: "Linux 6.1.0".to_owned(),
            };

            assert_eq!(
                env.render(),
                indoc! {"
                    # Environment

                    You have been invoked in the following environment:

                    - Primary working directory: /home/user/project
                      - Is a git repository: true
                    - Platform: linux
                    - Shell: bash
                    - OS Version: Linux 6.1.0"
                },
            );
        }
    }

    mod normalize_node_platform {
        use super::*;

        #[test]
        fn maps_rust_names_to_node_names() {
            assert_eq!(normalize_node_platform("macos"), "darwin");
            assert_eq!(normalize_node_platform("linux"), "linux");
            assert_eq!(normalize_node_platform("windows"), "win32");
        }

        #[test]
        fn unrecognized_name_is_unknown() {
            assert_eq!(normalize_node_platform("haiku"), "unknown");
        }
    }

    mod detect_shell {
        use super::*;

        #[test]
        fn names_the_shell_by_its_file_name_or_unknown() {
            for (shell, expected) in [
                (Some("/usr/local/bin/fish"), "fish"),
                (Some("zsh"), "zsh"),
                (None, "unknown"),
            ] {
                let detected = temp_env::with_var("SHELL", shell, detect_shell);

                assert_eq!(detected, expected, "{shell:?}");
            }
        }
    }

    mod format_os_version {
        use super::*;

        #[test]
        fn joins_sysname_and_release() {
            assert_eq!(format_os_version("Darwin", "27.0.0"), "Darwin 27.0.0");
        }

        #[test]
        fn without_a_sysname_names_the_target_os() {
            assert_eq!(format_os_version("", "27.0.0"), std::env::consts::OS);
        }
    }

    mod format_local_date {
        use super::*;

        #[test]
        fn applies_offset_across_midnight() {
            let now = datetime!(2026-04-12 20:30:00 UTC);
            assert_eq!(format_local_date(now, offset!(+8)), "2026-04-13");
            assert_eq!(format_local_date(now, offset!(-5)), "2026-04-12");
            assert_eq!(format_local_date(now, UtcOffset::UTC), "2026-04-12");
        }
    }

    mod model_identity {
        use super::*;

        #[test]
        fn names_a_catalog_model_with_its_cutoff() {
            assert_eq!(
                model_identity(&ModelSpec::resolve("claude-opus-5-5")),
                "You are powered by the model named Claude Opus 5.5. \
                 The exact model ID is claude-opus-5-5. \
                 Assistant knowledge cutoff is June 2026.",
            );
        }

        #[test]
        fn names_an_unknown_model_by_its_id() {
            assert_eq!(
                model_identity(&ModelSpec::resolve("custom-model-v1")),
                "You are powered by the model custom-model-v1.",
            );
        }
    }
}
