//! Git probes used by the session header and the status bar. Best-effort: every probe collapses
//! to `None` on missing git, non-repo cwd, detached HEAD, or non-UTF-8 output. Failures log at
//! `debug` so they don't pollute normal use but are recoverable when the status bar misbehaves.

use std::path::Path;
use std::process::{Command, Output};

use tracing::debug;

/// Probe the current branch via `git branch --show-current`. Detached HEAD comes back as empty
/// stdout, which we collapse to `None`.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    let cwd_str = cwd_to_str(cwd, "git branch")?;
    let output = run_probe("git branch", cwd_str, || {
        Command::new("git")
            .args([
                "-C",
                cwd_str,
                "--no-optional-locks",
                "branch",
                "--show-current",
            ])
            .output()
    })?;
    parse_branch(&output.stdout)
}

/// `&str` overload for callers that already hold a string-shaped cwd.
pub(crate) fn current_branch_str(cwd: &str) -> Option<String> {
    current_branch(Path::new(cwd))
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
/// Returns `None` when `gh` is missing, the user is unauthenticated, or no PR is open.
pub(crate) fn current_pull_request(cwd: &Path) -> Option<PullRequest> {
    let cwd_str = cwd_to_str(cwd, "gh pr")?;
    let output = run_probe("gh pr", cwd_str, || {
        Command::new("gh")
            .args(["pr", "view", "--json", "number,url"])
            .current_dir(cwd_str)
            .output()
    })?;
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

/// Runs a `Command::output()` closure, logging on spawn failure or non-zero exit. Returns the
/// successful output or `None`. `cwd` rides along on the log records so a user can pinpoint which
/// worktree the probe failed in.
fn run_probe<F>(probe: &str, cwd: &str, spawn: F) -> Option<Output>
where
    F: FnOnce() -> std::io::Result<Output>,
{
    let output = match spawn() {
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

/// First non-blank stderr line, capped to keep log records terse. Surfaces the actionable signal
/// (`auth required`, `no pull requests found`, `not a git repository`) without dumping a wall of
/// hint text.
fn stderr_summary(stderr: &[u8]) -> String {
    const MAX_LEN: usize = 200;
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = line.trim();
    if trimmed.len() <= MAX_LEN {
        trimmed.to_owned()
    } else {
        format!("{}...", &trimmed[..MAX_LEN])
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;

    use super::*;

    // ── current_branch ──

    #[test]
    fn current_branch_in_a_real_repo_yields_the_branch_name() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let Ok(status) = Command::new("git")
            .args(["init", "-q", "-b", "fixture-branch"])
            .current_dir(cwd)
            .status()
        else {
            return;
        };
        if !status.success() {
            return;
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
        assert_eq!(current_branch(cwd), Some("fixture-branch".to_owned()));
    }

    #[test]
    fn current_branch_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_branch(dir.path()), None);
    }

    #[test]
    fn current_branch_with_non_utf8_cwd_is_absent() {
        // Linux paths are bytes; embedding a non-UTF-8 byte hits the cwd_to_str failure branch
        // without ever spawning git.
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(current_branch(&cwd), None);
    }

    // ── current_branch_str ──

    #[test]
    fn current_branch_str_delegates_to_current_branch() {
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

    #[test]
    fn current_pull_request_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(current_pull_request(dir.path()), None);
    }

    #[test]
    fn current_pull_request_with_non_utf8_cwd_is_absent() {
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(current_pull_request(&cwd), None);
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
        assert_eq!(parse_pull_request(b""), None);
        assert_eq!(parse_pull_request(b"not json"), None);
        assert_eq!(parse_pull_request(br#"{"number":86}"#), None);
        assert_eq!(parse_pull_request(br#"{"url":"https://x"}"#), None);
        assert_eq!(parse_pull_request(br#"{"number":86,"url":""}"#), None);
        assert_eq!(parse_pull_request(br#"{"number":-1,"url":"x"}"#), None);
        // Non-string url field exercises the second `?` in `as_str()?.to_owned()`.
        assert_eq!(parse_pull_request(br#"{"number":86,"url":42}"#), None);
    }

    // ── cwd_to_str ──

    #[test]
    fn cwd_to_str_returns_path_when_utf8() {
        assert_eq!(cwd_to_str(Path::new("/tmp/a"), "p"), Some("/tmp/a"));
    }

    #[test]
    fn cwd_to_str_is_absent_when_not_utf8() {
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(cwd_to_str(&cwd, "p"), None);
    }

    // ── run_probe ──

    #[test]
    fn run_probe_returns_output_on_success() {
        let output = run_probe("test", "/tmp/cwd", || {
            Ok(Output {
                status: status_with_code(0),
                stdout: b"ok".to_vec(),
                stderr: Vec::new(),
            })
        })
        .expect("success path keeps the output");
        assert_eq!(output.stdout, b"ok");
    }

    #[test]
    fn run_probe_drops_output_on_non_zero_exit() {
        let result = run_probe("test", "/tmp/cwd", || {
            Ok(Output {
                status: status_with_code(1),
                stdout: b"unused".to_vec(),
                stderr: b"fatal: not a git repository\n".to_vec(),
            })
        });
        assert!(result.is_none());
    }

    #[test]
    fn run_probe_drops_output_on_spawn_failure() {
        let result = run_probe("test", "/tmp/cwd", || {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary missing",
            ))
        });
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
        let summary = stderr_summary(&huge);
        assert_eq!(summary.len(), 203, "200 chars + '...': {summary}");
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn stderr_summary_returns_empty_when_every_line_is_blank() {
        assert_eq!(stderr_summary(b"\n   \n\t\n"), "");
    }
}
