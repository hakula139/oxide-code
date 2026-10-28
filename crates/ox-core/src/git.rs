//! Git and `gh` probes for the session's repository, which run with ox's secrets withheld.
//! Best-effort: every probe collapses to `None` on missing git, non-repo cwd, detached HEAD, or
//! non-UTF-8 output. Failures log at `debug` to help diagnose a misbehaving status bar.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::io::AsyncReadExt as _;
use tracing::debug;

use crate::config::credentials::withhold_secrets;
use crate::util::text::truncate_to_width;

// ── Repository ──

/// Probes the current branch via `git branch --show-current`, returning `None` on a detached HEAD.
/// Dropping the future kills the child process.
pub async fn current_branch(cwd: &Path) -> Option<String> {
    let output = probe(
        "git branch",
        cwd,
        "git",
        &["--no-optional-locks", "branch", "--show-current"],
    )
    .await?;
    stdout_line(&output.stdout)
}

/// Probes the top-level directory of the repository containing `cwd` via
/// `git rev-parse --show-toplevel`. Dropping the future kills the child process.
pub(crate) async fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let output = probe(
        "git rev-parse",
        cwd,
        "git",
        &["rev-parse", "--show-toplevel"],
    )
    .await?;
    stdout_line(&output.stdout).map(PathBuf::from)
}

/// The trimmed stdout of a one-line probe, or `None` when it is empty or not UTF-8.
fn stdout_line(stdout: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(stdout).ok()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

// ── Probes ──

/// Runs the `git` or `gh` `program` with `args` in `cwd` on the tokio runtime, with no stdin and
/// ox's secrets withheld through [`withhold_secrets`], killing the child if the caller drops the
/// future. Git runs commands a repository's own config names, so they must not see ox's secrets.
pub async fn output_async(program: &str, args: &[&str], cwd: &Path) -> io::Result<Output> {
    command(program, args, cwd).output().await
}

/// Runs `program` as [`output_async`] does, but reads at most `limit` bytes of stdout and then
/// kills the child, so a runaway output is bounded before the caller buffers it. Returns the
/// output with the stdout prefix and whether the process wrote more. Stderr is read to the end,
/// since a program on this path writes little there.
pub async fn output_async_capped(
    program: &str,
    args: &[&str],
    cwd: &Path,
    limit: usize,
) -> io::Result<(Output, bool)> {
    let mut child = command(program, args, cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stderr = tokio::spawn(async move {
        let mut stderr = Vec::new();
        _ = stderr_pipe.read_to_end(&mut stderr).await;
        stderr
    });

    let mut stdout = Vec::new();
    let mut bounded = stdout_pipe.take((limit as u64).saturating_add(1));
    bounded.read_to_end(&mut stdout).await?;
    let truncated = stdout.len() > limit;
    stdout.truncate(limit);
    drop(bounded);

    if truncated {
        child.kill().await?;
    }
    let status = child.wait().await?;
    let stderr = stderr.await.unwrap_or_default();
    Ok((
        Output {
            status,
            stdout,
            stderr,
        },
        truncated,
    ))
}

fn command(program: &str, args: &[&str], cwd: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    withhold_secrets(command.as_std_mut());
    command
}

/// Runs `program` with `args` in `cwd` through [`output_async`] and returns the output of a
/// successful run, logging any other outcome under the `name` probe.
pub async fn probe(name: &str, cwd: &Path, program: &str, args: &[&str]) -> Option<Output> {
    check_output(name, cwd, output_async(program, args, cwd).await)
}

/// Logs a spawn failure or non-zero exit and returns the successful output or `None`. `cwd` rides
/// along on the log records so a user can pinpoint which worktree the probe failed in.
fn check_output(name: &str, cwd: &Path, result: io::Result<Output>) -> Option<Output> {
    let output = match result {
        Ok(output) => output,
        Err(e) => {
            debug!(probe = name, error = %e, ?cwd, "probe spawn failed");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            code = output.status.code().unwrap_or(-1),
            stderr = stderr_summary(&output.stderr),
            ?cwd,
            probe = name,
            "probe exited unsuccessfully",
        );
        return None;
    }
    Some(output)
}

/// First non-blank stderr line, capped at `MAX_WIDTH` columns for terse log records.
fn stderr_summary(stderr: &[u8]) -> String {
    const MAX_WIDTH: usize = 200;
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    truncate_to_width(line.trim(), MAX_WIDTH)
}

// ── Testing ──

#[cfg(test)]
pub(crate) mod local_testing {
    use std::path::Path;

    use super::testing::isolated_git;

    /// Initializes a repository in `cwd` with one commit on `fixture-branch`. Panics when git
    /// fails, since the dev shell and CI both provide it.
    pub(crate) fn init_fixture_repo(cwd: &Path) {
        for args in [
            ["init", "-q", "-b", "fixture-branch"].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
            ["commit", "-q", "--allow-empty", "-m", "init"].as_slice(),
        ] {
            let status = isolated_git(cwd).args(args).status().unwrap();
            assert!(status.success(), "git {args:?} failed: {status}");
        }
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use std::path::Path;
    use std::process::Command;

    /// Clears git's repository-local variables (`git rev-parse --local-env-vars`) and discovery
    /// ceiling, and skips the system and global configs.
    const ISOLATED_GIT_ENV: [(&str, Option<&str>); 18] = [
        ("GIT_ALTERNATE_OBJECT_DIRECTORIES", None),
        ("GIT_CEILING_DIRECTORIES", None),
        ("GIT_COMMON_DIR", None),
        ("GIT_CONFIG", None),
        ("GIT_CONFIG_COUNT", None),
        ("GIT_CONFIG_PARAMETERS", None),
        ("GIT_DIR", None),
        ("GIT_GRAFT_FILE", None),
        ("GIT_IMPLICIT_WORK_TREE", None),
        ("GIT_INDEX_FILE", None),
        ("GIT_NO_REPLACE_OBJECTS", None),
        ("GIT_OBJECT_DIRECTORY", None),
        ("GIT_PREFIX", None),
        ("GIT_REPLACE_REF_BASE", None),
        ("GIT_SHALLOW_FILE", None),
        ("GIT_WORK_TREE", None),
        ("GIT_CONFIG_GLOBAL", Some("/dev/null")),
        ("GIT_CONFIG_NOSYSTEM", Some("1")),
    ];

    /// Applies [`ISOLATED_GIT_ENV`] to `command`.
    pub fn isolate_git(command: &mut Command) {
        for (key, value) in ISOLATED_GIT_ENV {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
    }

    /// `git` in `cwd` under [`ISOLATED_GIT_ENV`].
    pub fn isolated_git(cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command.current_dir(cwd);
        isolate_git(&mut command);
        command
    }

    /// Runs `future` under [`ISOLATED_GIT_ENV`] so probes see only their fixture.
    /// `git rebase --exec` exports `GIT_DIR`, which would otherwise aim them at the caller's repo.
    pub async fn with_isolated_git_env_async<F: Future>(future: F) -> F::Output {
        temp_env::async_with_vars(ISOLATED_GIT_ENV, future).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(target_os = "linux")]
    use std::ffi::OsStr;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStrExt as _;
    use std::process::ExitStatus;

    use super::local_testing::init_fixture_repo;
    use super::testing::{isolated_git, with_isolated_git_env_async};
    use super::*;
    use crate::config::credentials::secret_env_vars;
    use crate::util::log::testing::LogCapture;
    use crate::util::path::project_root;

    mod current_branch {
        use super::*;

        #[tokio::test]
        async fn in_a_real_repo_yields_the_branch_name() {
            let dir = tempfile::tempdir().unwrap();
            init_fixture_repo(dir.path());

            let branch = with_isolated_git_env_async(current_branch(dir.path())).await;

            assert_eq!(branch, Some("fixture-branch".to_owned()));
        }

        #[tokio::test]
        async fn outside_a_repo_is_absent() {
            let dir = tempfile::tempdir().unwrap();

            let branch = with_isolated_git_env_async(current_branch(dir.path())).await;

            assert_eq!(branch, None);
        }

        #[tokio::test]
        #[cfg(target_os = "linux")]
        async fn in_a_non_utf8_directory_yields_the_branch_name() {
            let dir = tempfile::tempdir().unwrap();
            init_fixture_repo(dir.path());
            let cwd = dir.path().join(OsStr::from_bytes(b"sub-\xff"));
            std::fs::create_dir(&cwd).unwrap();

            let branch = with_isolated_git_env_async(current_branch(&cwd)).await;

            assert_eq!(branch, Some("fixture-branch".to_owned()));
        }
    }

    mod repo_root {
        use super::*;

        #[tokio::test]
        async fn in_a_subdirectory_finds_the_top_level() {
            let dir = tempfile::tempdir().unwrap();
            init_fixture_repo(dir.path());
            let nested = dir.path().join("crates/app");
            std::fs::create_dir_all(&nested).unwrap();

            let root = with_isolated_git_env_async(repo_root(&nested))
                .await
                .expect("inside a repository");

            // macOS routes tempdirs through `/private`.
            assert_eq!(
                root.canonicalize().unwrap(),
                dir.path().canonicalize().unwrap()
            );
        }

        #[tokio::test]
        async fn follows_a_core_worktree_that_project_root_ignores() {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("outer").join("repo");
            let sub = repo.join("sub");
            std::fs::create_dir_all(&sub).unwrap();
            init_fixture_repo(&repo);
            let status = isolated_git(&repo)
                .args(["config", "core.worktree", "../.."])
                .status()
                .unwrap();
            assert!(status.success(), "git config failed: {status}");

            let git_root = with_isolated_git_env_async(repo_root(&sub)).await.unwrap();

            assert_eq!(
                git_root.canonicalize().unwrap(),
                dir.path().join("outer").canonicalize().unwrap(),
            );
            assert_eq!(project_root(&sub), repo);
        }

        #[tokio::test]
        async fn outside_a_repo_is_absent() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(
                with_isolated_git_env_async(repo_root(dir.path())).await,
                None
            );
        }
    }

    mod stdout_line {
        use super::*;

        #[test]
        fn keeps_one_trimmed_line_and_drops_everything_else() {
            assert_eq!(stdout_line(b"feat/login\n"), Some("feat/login".to_owned()));
            // `branch --show-current` prints empty on detached HEAD.
            assert_eq!(stdout_line(b""), None);
            assert_eq!(stdout_line(b"   \n"), None);
            assert_eq!(stdout_line(&[0xff, 0xfe, b'\n']), None);
        }
    }

    mod output_async {
        use indoc::indoc;

        use super::*;

        #[tokio::test]
        async fn withholds_the_secrets_ox_authenticates_with() {
            let dir = tempfile::tempdir().unwrap();
            let names: Vec<_> = secret_env_vars().chain(["OX_TEST_INHERITED"]).collect();
            let vars: Vec<_> = names.iter().map(|&var| (var, Some("secret"))).collect();
            let script = names
                .iter()
                .map(|var| format!("echo {var}=${{{var}-unset}}"))
                .collect::<Vec<_>>()
                .join("; ");
            // A shell alias runs the way a command a repository's config names would.
            let alias = format!("alias.print-env=!{script}");
            let args = ["-c", &alias, "print-env"];

            let output = temp_env::async_with_vars(vars, output_async("git", &args, dir.path()))
                .await
                .unwrap();

            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                indoc! {"
                    ANTHROPIC_AUTH_TOKEN=unset
                    ANTHROPIC_API_KEY=unset
                    CLAUDE_CODE_OAUTH_TOKEN=unset
                    ANTHROPIC_CUSTOM_HEADERS=unset
                    OX_TEST_INHERITED=secret
                "},
            );
        }
    }

    mod check_output {
        use super::*;

        fn exit_code_one() -> ExitStatus {
            #[cfg(unix)]
            let status = std::os::unix::process::ExitStatusExt::from_raw(1 << 8);
            #[cfg(windows)]
            let status = std::os::windows::process::ExitStatusExt::from_raw(1);
            status
        }

        #[test]
        fn success_preserves_stdout() {
            let output = check_output(
                "test",
                Path::new("/tmp/cwd"),
                Ok(Output {
                    status: ExitStatus::default(),
                    stdout: b"ok".to_vec(),
                    stderr: Vec::new(),
                }),
            )
            .expect("success path keeps the output");
            assert_eq!(output.stdout, b"ok");
        }

        #[test]
        fn records_the_probe_context_when_dropping_failed_output() {
            for (result, message, fields) in [
                (
                    Ok(Output {
                        status: exit_code_one(),
                        stdout: b"unused".to_vec(),
                        stderr: b"fatal: not a git repository\n".to_vec(),
                    }),
                    "probe exited unsuccessfully",
                    vec![("code", "1"), ("stderr", "fatal: not a git repository")],
                ),
                (
                    Err(io::Error::new(io::ErrorKind::NotFound, "binary missing")),
                    "probe spawn failed",
                    vec![("error", "binary missing")],
                ),
            ] {
                let logs = LogCapture::default();
                let cwd = Path::new("/repo");
                let mut expected = BTreeMap::from([
                    ("probe".to_owned(), "test".to_owned()),
                    ("cwd".to_owned(), format!("{cwd:?}")),
                    ("message".to_owned(), message.to_owned()),
                ]);
                expected.extend(
                    fields
                        .into_iter()
                        .map(|(key, value)| (key.to_owned(), value.to_owned())),
                );

                let output = tracing::dispatcher::with_default(&logs.dispatch(), || {
                    check_output("test", cwd, result)
                });

                assert!(output.is_none(), "{message}");
                assert_eq!(logs.events(), [expected], "{message}");
            }
        }
    }

    mod stderr_summary {
        use super::*;

        #[test]
        fn picks_first_meaningful_line_and_caps_length() {
            assert_eq!(stderr_summary(b""), "");
            assert_eq!(
                stderr_summary(b"\n  \nfatal: not a git repository\nmore detail\n"),
                "fatal: not a git repository",
            );
            let huge = vec![b'x'; 500];
            assert_eq!(stderr_summary(&huge), format!("{}...", "x".repeat(197)));
        }

        #[test]
        fn caps_multibyte_lines_on_char_boundaries() {
            // 303 bytes of three-byte chars, so a byte-indexed cut at 200 would land mid-char.
            let line = "中".repeat(101);
            assert_eq!(
                stderr_summary(line.as_bytes()),
                format!("{}...", "中".repeat(98)),
            );
        }

        #[test]
        fn blank_lines_have_no_summary() {
            assert_eq!(stderr_summary(b"\n   \n\t\n"), "");
        }
    }
}
