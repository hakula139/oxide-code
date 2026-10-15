//! Git probes used by the session header and the status bar. Best-effort: every probe collapses
//! to `None` on missing git, non-repo cwd, detached HEAD, or non-UTF-8 output. Failures log at
//! `debug` so they're recoverable when the status bar misbehaves.

use std::io;
use std::path::Path;
use std::process::{Command, Output};

use tracing::debug;

use crate::util::text::truncate_to_width;

/// Probe the current branch via `git branch --show-current`. Detached HEAD comes back as empty
/// stdout, which we collapse to `None`.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    let cwd_str = cwd_to_str(cwd, "git branch")?;
    let output = check_output("git branch", cwd_str, branch_command(cwd_str).output())?;
    parse_branch(&output.stdout)
}

/// [`current_branch`] for async callers. Dropping the future kills the child process.
pub(crate) async fn current_branch_async(cwd: &Path) -> Option<String> {
    let cwd_str = cwd_to_str(cwd, "git branch")?;
    let output = check_output(
        "git branch",
        cwd_str,
        output_async(branch_command(cwd_str)).await,
    )?;
    parse_branch(&output.stdout)
}

/// `&str` overload for callers that already hold a string-shaped cwd.
pub(crate) fn current_branch_str(cwd: &str) -> Option<String> {
    current_branch(Path::new(cwd))
}

fn branch_command(cwd: &str) -> Command {
    let mut command = Command::new("git");
    command.args(["-C", cwd, "--no-optional-locks", "branch", "--show-current"]);
    command
}

fn parse_branch(stdout: &[u8]) -> Option<String> {
    let branch = std::str::from_utf8(stdout).ok()?.trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_owned())
    }
}

/// Open pull request for the current branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequest {
    pub(crate) number: u64,
    pub(crate) url: String,
}

/// Probe the open pull request for `cwd`'s current branch via `gh pr view --json number,url`.
/// Returns `None` when `gh` is missing, the user is unauthenticated, or no PR is open. Dropping
/// the future kills the child process.
pub(crate) async fn current_pull_request(cwd: &Path) -> Option<PullRequest> {
    let cwd_str = cwd_to_str(cwd, "gh pr")?;
    let mut command = Command::new("gh");
    command
        .args(["pr", "view", "--json", "number,url"])
        .current_dir(cwd_str);
    let output = check_output("gh pr", cwd_str, output_async(command).await)?;
    parse_pull_request(&output.stdout)
}

fn parse_pull_request(stdout: &[u8]) -> Option<PullRequest> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let number = value.get("number")?.as_u64()?;
    let url = value.get("url")?.as_str()?.to_owned();
    if url.is_empty() {
        return None;
    }
    Some(PullRequest { number, url })
}

/// Coerces `cwd` to a `&str` so it can flow into `Command::args` / `current_dir`. Logs and
/// surfaces `None` when the path isn't valid UTF-8.
fn cwd_to_str<'a>(cwd: &'a Path, probe: &str) -> Option<&'a str> {
    if let Some(s) = cwd.to_str() {
        Some(s)
    } else {
        debug!(cwd = ?cwd, "{probe} probe: cwd is not valid UTF-8");
        None
    }
}

/// Runs `command` on the tokio runtime, killing the child if the caller drops the future.
async fn output_async(command: Command) -> io::Result<Output> {
    tokio::process::Command::from(command)
        .kill_on_drop(true)
        .output()
        .await
}

/// Logs a spawn failure or non-zero exit and returns the successful output or `None`. `cwd` rides
/// along on the log records so a user can pinpoint which worktree the probe failed in.
fn check_output(probe: &str, cwd: &str, result: io::Result<Output>) -> Option<Output> {
    let output = match result {
        Ok(output) => output,
        Err(e) => {
            debug!(error = %e, cwd = %cwd, "{probe} probe: spawn failed");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            code = output.status.code().unwrap_or(-1),
            stderr = stderr_summary(&output.stderr),
            cwd = %cwd,
            "{probe} probe: non-zero exit",
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

    // ── current_branch ──

    #[test]
    fn current_branch_in_a_real_repo_yields_the_branch_name() {
        let dir = tempfile::tempdir().unwrap();
        if !init_fixture_repo(dir.path()) {
            return;
        }
        assert_eq!(
            current_branch(dir.path()),
            Some("fixture-branch".to_owned())
        );
    }

    #[test]
    fn current_branch_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_branch(dir.path()), None);
    }

    #[test]
    #[cfg(unix)]
    fn current_branch_with_non_utf8_cwd_is_absent() {
        // Linux paths are bytes; embedding a non-UTF-8 byte hits the cwd_to_str failure branch
        // without ever spawning git.
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(current_branch(&cwd), None);
    }

    /// Commits once on `fixture-branch` in `cwd`. Returns `false` when git is unavailable.
    fn init_fixture_repo(cwd: &Path) -> bool {
        let Ok(status) = Command::new("git")
            .args(["init", "-q", "-b", "fixture-branch"])
            .current_dir(cwd)
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
        for args in [
            ["config", "user.email", "test@example.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
            ["config", "commit.gpgsign", "false"].as_slice(),
            ["commit", "-q", "--allow-empty", "-m", "init"].as_slice(),
        ] {
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
        }
        true
    }

    // ── current_branch_async ──

    #[tokio::test]
    async fn current_branch_async_in_a_real_repo_yields_the_branch_name() {
        let dir = tempfile::tempdir().unwrap();
        if !init_fixture_repo(dir.path()) {
            return;
        }
        assert_eq!(
            current_branch_async(dir.path()).await,
            Some("fixture-branch".to_owned()),
        );
    }

    #[tokio::test]
    async fn current_branch_async_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_branch_async(dir.path()).await, None);
    }

    // ── current_branch_str ──

    #[test]
    fn current_branch_str_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_branch_str(dir.path().to_str().unwrap()), None);
    }

    // ── parse_branch ──

    #[test]
    fn parse_branch_keeps_branch_names_and_drops_everything_else() {
        assert_eq!(parse_branch(b"feat/login\n"), Some("feat/login".to_owned()));
        // `branch --show-current` prints empty on detached HEAD.
        assert_eq!(parse_branch(b""), None);
        assert_eq!(parse_branch(b"   \n"), None);
        assert_eq!(parse_branch(&[0xff, 0xfe, b'\n']), None);
    }

    // ── current_pull_request ──

    #[tokio::test]
    async fn current_pull_request_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_pull_request(dir.path()).await, None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn current_pull_request_with_non_utf8_cwd_is_absent() {
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(current_pull_request(&cwd).await, None);
    }

    // ── parse_pull_request ──

    #[test]
    fn parse_pull_request_extracts_number_and_url() {
        assert_eq!(
            parse_pull_request(br#"{"number":86,"url":"https://github.com/o/r/pull/86"}"#),
            Some(PullRequest {
                number: 86,
                url: "https://github.com/o/r/pull/86".to_owned(),
            }),
        );
    }

    #[test]
    fn parse_pull_request_drops_invalid_payloads() {
        assert_eq!(parse_pull_request(b""), None);
        assert_eq!(parse_pull_request(b"not json"), None);
        assert_eq!(parse_pull_request(br#"{"number":86}"#), None);
        assert_eq!(parse_pull_request(br#"{"url":"https://x"}"#), None);
        assert_eq!(parse_pull_request(br#"{"number":86,"url":""}"#), None);
        assert_eq!(parse_pull_request(br#"{"number":-1,"url":"x"}"#), None);
        assert_eq!(parse_pull_request(br#"{"number":86,"url":42}"#), None);
    }

    // ── cwd_to_str ──

    #[test]
    fn cwd_to_str_returns_path_when_utf8() {
        assert_eq!(cwd_to_str(Path::new("/tmp/a"), "p"), Some("/tmp/a"));
    }

    #[test]
    #[cfg(unix)]
    fn cwd_to_str_is_absent_when_not_utf8() {
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(cwd_to_str(&cwd, "p"), None);
    }

    // ── check_output ──

    #[test]
    fn check_output_returns_output_on_success() {
        let output = check_output(
            "test",
            "/tmp/cwd",
            Ok(Output {
                status: status_with_code(0),
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
            }),
        )
        .expect("success path keeps the output");
        assert_eq!(output.stdout, b"ok");
    }

    #[test]
    fn check_output_drops_output_on_non_zero_exit() {
        let result = check_output(
            "test",
            "/tmp/cwd",
            Ok(Output {
                status: status_with_code(1),
                stdout: b"unused".to_vec(),
                stderr: b"fatal: not a git repository\n".to_vec(),
            }),
        );
        assert!(result.is_none());
    }

    #[test]
    fn check_output_drops_output_on_spawn_failure() {
        let result = check_output(
            "test",
            "/tmp/cwd",
            Err(io::Error::new(io::ErrorKind::NotFound, "binary missing")),
        );
        assert!(result.is_none());
    }

    fn status_with_code(code: i32) -> std::process::ExitStatus {
        // `Command::new("sh").arg("-c").arg(format!("exit {code}"))` is the portable way to mint
        // an `ExitStatus` with a specific code from tests.
        Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .unwrap()
    }

    // ── stderr_summary ──

    #[test]
    fn stderr_summary_picks_first_meaningful_line_and_caps_length() {
        assert_eq!(stderr_summary(b""), "");
        assert_eq!(
            stderr_summary(b"\n  \nfatal: not a git repository\nmore detail\n"),
            "fatal: not a git repository",
        );
        let huge = vec![b'x'; 500];
        assert_eq!(stderr_summary(&huge), format!("{}...", "x".repeat(197)));
    }

    #[test]
    fn stderr_summary_caps_multibyte_lines_on_char_boundaries() {
        // 303 bytes of three-byte chars, so a byte-indexed cut at 200 would land mid-char.
        let line = "中".repeat(101);
        assert_eq!(
            stderr_summary(line.as_bytes()),
            format!("{}...", "中".repeat(98)),
        );
    }

    #[test]
    fn stderr_summary_returns_empty_when_every_line_is_blank() {
        assert_eq!(stderr_summary(b"\n   \n\t\n"), "");
    }
}
