//! Git probes used by the session header and the status bar. Best-effort: every probe collapses
//! to `None` on missing git, non-repo cwd, detached HEAD, or non-UTF-8 output. Failures log at
//! `debug` so they don't pollute normal use but are recoverable when the status bar misbehaves.

use std::path::Path;
use std::process::Command;

use tracing::debug;

/// Probe the current branch via `git branch --show-current`. Detached HEAD comes back as empty
/// stdout, which we collapse to `None`.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    let Some(cwd_str) = cwd.to_str() else {
        debug!(cwd = ?cwd, "git branch probe: cwd is not valid UTF-8");
        return None;
    };
    let output = match Command::new("git")
        .args([
            "-C",
            cwd_str,
            "--no-optional-locks",
            "branch",
            "--show-current",
        ])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            debug!(cwd = cwd_str, error = %e, "git branch probe: spawn failed");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            cwd = cwd_str,
            code = output.status.code().unwrap_or(-1),
            stderr = stderr_summary(&output.stderr),
            "git branch probe: non-zero exit",
        );
        return None;
    }
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
    let Some(cwd_str) = cwd.to_str() else {
        debug!(cwd = ?cwd, "gh pr probe: cwd is not valid UTF-8");
        return None;
    };
    let output = match Command::new("gh")
        .args(["pr", "view", "--json", "number,url"])
        .current_dir(cwd_str)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            debug!(cwd = cwd_str, error = %e, "gh pr probe: spawn failed");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            cwd = cwd_str,
            code = output.status.code().unwrap_or(-1),
            stderr = stderr_summary(&output.stderr),
            "gh pr probe: non-zero exit",
        );
        return None;
    }
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
}
