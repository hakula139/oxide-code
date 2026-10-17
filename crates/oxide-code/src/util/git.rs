//! Git probes for the session's repository: its root, current branch, and pull request.
//! Best-effort: every probe collapses to `None` on missing git, non-repo cwd, detached HEAD, or
//! non-UTF-8 output. Failures log at `debug` to help diagnose a misbehaving status bar.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde::Deserialize;
use tracing::debug;

use crate::util::text::truncate_to_width;

/// Probes the current branch via `git branch --show-current`, returning `None` on a detached HEAD.
pub(crate) fn current_branch(cwd: &Path) -> Option<String> {
    let cwd_str = cwd_to_str(cwd, "git branch")?;
    let output = check_output("git branch", cwd_str, branch_command(cwd_str).output())?;
    stdout_line(&output.stdout)
}

/// [`current_branch`] for async callers. Dropping the future kills the child process.
pub(crate) async fn current_branch_async(cwd: &Path) -> Option<String> {
    let cwd_str = cwd_to_str(cwd, "git branch")?;
    let output = check_output(
        "git branch",
        cwd_str,
        output_async(branch_command(cwd_str)).await,
    )?;
    stdout_line(&output.stdout)
}

fn branch_command(cwd: &str) -> Command {
    let mut command = Command::new("git");
    command.args(["-C", cwd, "--no-optional-locks", "branch", "--show-current"]);
    command
}

/// Probes the top-level directory of the repository containing `cwd` via
/// `git rev-parse --show-toplevel`. Dropping the future kills the child process.
pub(crate) async fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut command = Command::new("git");
    command
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd);
    let output = check_output(
        "git rev-parse",
        &cwd.to_string_lossy(),
        output_async(command).await,
    )?;
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

/// Open pull request for the current branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PullRequest {
    pub(crate) number: u64,
    pub(crate) url: String,
    /// `None` until a check reports, or when the probe skipped the checks.
    pub(crate) checks: Option<Checks>,
    /// `None` when branch protection requires no review, since GitHub then reports no decision
    /// whatever the reviews say, or when the probe skipped the review state.
    pub(crate) review: Option<ReviewDecision>,
}

/// A pull request's checks, summarized by their worst state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Checks {
    Failing(usize),
    Pending(usize),
    Cancelled(usize),
    Passing,
}

/// A pull request's review state, as GitHub's merge gate sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// How much of a pull request [`current_pull_request`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PullRequestDetail {
    /// The number and URL.
    Link,
    /// The number and URL, plus the checks and review state, which take a heavier `gh` query.
    Status,
}

impl PullRequestDetail {
    const fn json_fields(self) -> &'static str {
        match self {
            Self::Link => "number,url",
            Self::Status => "number,url,statusCheckRollup,reviewDecision",
        }
    }
}

/// Probes the open pull request for `cwd`'s current branch via `gh pr view`, reading `detail`.
/// Returns `None` when `gh` is missing, the user is unauthenticated, or no PR is open. Dropping
/// the future kills the child process.
pub(crate) async fn current_pull_request(
    cwd: &Path,
    detail: PullRequestDetail,
) -> Option<PullRequest> {
    let cwd_str = cwd_to_str(cwd, "gh pr")?;
    let mut command = Command::new("gh");
    command
        .args(["pr", "view", "--json", detail.json_fields()])
        .current_dir(cwd_str);
    let output = check_output("gh pr", cwd_str, output_async(command).await)?;
    parse_pull_request(&output.stdout)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestView {
    number: u64,
    url: String,
    status_check_rollup: Option<Vec<CheckView>>,
    review_decision: Option<String>,
}

/// A check run, or a commit status, which has a `context` and reports only `state`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckView {
    context: Option<String>,
    name: Option<String>,
    workflow_name: Option<String>,
    started_at: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

impl CheckView {
    /// Identifies the check across its runs.
    fn key(&self) -> (&str, &str, &str) {
        (
            self.context.as_deref().unwrap_or_default(),
            self.name.as_deref().unwrap_or_default(),
            self.workflow_name.as_deref().unwrap_or_default(),
        )
    }

    fn state(&self) -> &str {
        match (
            self.state.as_deref().unwrap_or_default(),
            self.status.as_deref().unwrap_or_default(),
        ) {
            ("", "COMPLETED") => self.conclusion.as_deref().unwrap_or_default(),
            ("", status) => status,
            (state, _) => state,
        }
    }
}

fn parse_pull_request(stdout: &[u8]) -> Option<PullRequest> {
    let view: PullRequestView = serde_json::from_slice(stdout).ok()?;
    if view.url.is_empty() {
        return None;
    }
    let review = match view.review_decision.as_deref() {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };
    Some(PullRequest {
        number: view.number,
        url: view.url,
        checks: summarize_checks(view.status_check_rollup.unwrap_or_default()),
        review,
    })
}

/// Buckets the latest run of each check the way `gh pr checks` does, except that a workflow that
/// failed to start counts as failing, where `gh` leaves it pending for good. `None` when no check
/// has reported.
fn summarize_checks(mut checks: Vec<CheckView>) -> Option<Checks> {
    if checks.is_empty() {
        return None;
    }
    checks.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let mut seen = HashSet::new();
    let (mut failing, mut pending, mut cancelled) = (0, 0, 0);
    for check in &checks {
        if !seen.insert(check.key()) {
            continue;
        }
        match check.state() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "ERROR" | "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                failing += 1;
            }
            "CANCELLED" => cancelled += 1,
            _ => pending += 1,
        }
    }
    Some(if failing > 0 {
        Checks::Failing(failing)
    } else if pending > 0 {
        Checks::Pending(pending)
    } else if cancelled > 0 {
        Checks::Cancelled(cancelled)
    } else {
        Checks::Passing
    })
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
pub(crate) mod testing {
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

    /// `git` in `cwd` under [`ISOLATED_GIT_ENV`].
    pub(crate) fn isolated_git(cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command.current_dir(cwd);
        for (key, value) in ISOLATED_GIT_ENV {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }

    /// Runs `f` under [`ISOLATED_GIT_ENV`], so production probes under test see only the fixture.
    /// `git rebase --exec` exports `GIT_DIR`, which would otherwise aim them at the caller's repo.
    pub(crate) fn with_isolated_git_env<R>(f: impl FnOnce() -> R) -> R {
        temp_env::with_vars(ISOLATED_GIT_ENV, f)
    }

    /// [`with_isolated_git_env`] for async probes.
    pub(crate) async fn with_isolated_git_env_async<F: Future>(future: F) -> F::Output {
        temp_env::async_with_vars(ISOLATED_GIT_ENV, future).await
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsStr;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::path::PathBuf;

    use super::testing::{isolated_git, with_isolated_git_env, with_isolated_git_env_async};
    use super::*;

    // ── current_branch ──

    #[test]
    fn current_branch_in_a_real_repo_yields_the_branch_name() {
        let dir = tempfile::tempdir().unwrap();
        if !init_fixture_repo(dir.path()) {
            return;
        }
        assert_eq!(
            with_isolated_git_env(|| current_branch(dir.path())),
            Some("fixture-branch".to_owned())
        );
    }

    #[test]
    fn current_branch_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(with_isolated_git_env(|| current_branch(dir.path())), None);
    }

    #[test]
    #[cfg(unix)]
    fn current_branch_with_non_utf8_cwd_is_absent() {
        // Unix paths are bytes, so a non-UTF-8 one fails `cwd_to_str` before git spawns.
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(current_branch(&cwd), None);
    }

    /// Commits once on `fixture-branch` in `cwd`. Returns `false` when git is unavailable.
    fn init_fixture_repo(cwd: &Path) -> bool {
        let Ok(status) = isolated_git(cwd)
            .args(["init", "-q", "-b", "fixture-branch"])
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
            ["commit", "-q", "--allow-empty", "-m", "init"].as_slice(),
        ] {
            isolated_git(cwd).args(args).status().unwrap();
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
            with_isolated_git_env_async(current_branch_async(dir.path())).await,
            Some("fixture-branch".to_owned()),
        );
    }

    #[tokio::test]
    async fn current_branch_async_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            with_isolated_git_env_async(current_branch_async(dir.path())).await,
            None
        );
    }

    // ── repo_root ──

    #[tokio::test]
    async fn repo_root_in_a_subdirectory_finds_the_top_level() {
        let dir = tempfile::tempdir().unwrap();
        if !init_fixture_repo(dir.path()) {
            return;
        }
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
    async fn repo_root_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            with_isolated_git_env_async(repo_root(dir.path())).await,
            None
        );
    }

    // ── stdout_line ──

    #[test]
    fn stdout_line_keeps_one_trimmed_line_and_drops_everything_else() {
        assert_eq!(stdout_line(b"feat/login\n"), Some("feat/login".to_owned()));
        // `branch --show-current` prints empty on detached HEAD.
        assert_eq!(stdout_line(b""), None);
        assert_eq!(stdout_line(b"   \n"), None);
        assert_eq!(stdout_line(&[0xff, 0xfe, b'\n']), None);
    }

    // ── current_pull_request ──

    #[tokio::test]
    async fn current_pull_request_outside_a_repo_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        for detail in [PullRequestDetail::Link, PullRequestDetail::Status] {
            assert_eq!(
                with_isolated_git_env_async(current_pull_request(dir.path(), detail)).await,
                None,
                "{detail:?}",
            );
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn current_pull_request_with_non_utf8_cwd_is_absent() {
        let cwd = PathBuf::from(OsStr::from_bytes(b"/tmp/\xff"));
        assert_eq!(
            current_pull_request(&cwd, PullRequestDetail::Link).await,
            None
        );
    }

    // ── parse_pull_request ──

    /// `gh pr view` output for PR 86 with `rollup` and `review_decision`.
    fn pull_request_json(rollup: &serde_json::Value, review_decision: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "number": 86,
            "url": "https://github.com/o/r/pull/86",
            "statusCheckRollup": rollup,
            "reviewDecision": review_decision,
        }))
        .unwrap()
    }

    fn run(name: &str, started_at: &str, status: &str, conclusion: &str) -> serde_json::Value {
        serde_json::json!({
            "__typename": "CheckRun",
            "name": name,
            "workflowName": "CI",
            "startedAt": started_at,
            "status": status,
            "conclusion": conclusion,
        })
    }

    fn status_context(started_at: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "__typename": "StatusContext",
            "context": "ci/legacy",
            "startedAt": started_at,
            "state": state,
        })
    }

    fn checks_of(rollup: &serde_json::Value) -> Option<Checks> {
        parse_pull_request(&pull_request_json(rollup, ""))
            .unwrap()
            .checks
    }

    #[test]
    fn parse_pull_request_extracts_number_and_url() {
        assert_eq!(
            parse_pull_request(br#"{"number":86,"url":"https://github.com/o/r/pull/86"}"#),
            Some(PullRequest {
                number: 86,
                url: "https://github.com/o/r/pull/86".to_owned(),
                checks: None,
                review: None,
            }),
        );
    }

    #[test]
    fn parse_pull_request_reads_the_review_decision() {
        let review = |decision| {
            parse_pull_request(&pull_request_json(&serde_json::json!([]), decision))
                .unwrap()
                .review
        };
        assert_eq!(review("APPROVED"), Some(ReviewDecision::Approved));
        assert_eq!(
            review("CHANGES_REQUESTED"),
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(
            review("REVIEW_REQUIRED"),
            Some(ReviewDecision::ReviewRequired)
        );
        assert_eq!(review(""), None, "branch protection requires no review");
    }

    #[test]
    fn parse_pull_request_summarizes_checks_by_their_worst_state() {
        let t = "2026-10-16T06:37:52Z";
        let status = |state: &str| status_context(t, state);

        assert_eq!(checks_of(&serde_json::json!([])), None);
        assert_eq!(checks_of(&serde_json::Value::Null), None);
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Build", t, "COMPLETED", "SUCCESS"),
                run("Docs", t, "COMPLETED", "SKIPPED"),
                status("SUCCESS"),
            ])),
            Some(Checks::Passing),
        );
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Build", t, "COMPLETED", "SUCCESS"),
                run("Test", t, "COMPLETED", "CANCELLED"),
                run("Lint", t, "COMPLETED", "CANCELLED"),
            ])),
            Some(Checks::Cancelled(2)),
        );
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Build", t, "COMPLETED", "SUCCESS"),
                run("Test", t, "IN_PROGRESS", ""),
                run("Lint", t, "COMPLETED", "CANCELLED"),
                status("PENDING"),
            ])),
            Some(Checks::Pending(2)),
        );
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Build", t, "COMPLETED", "TIMED_OUT"),
                run("Deploy", t, "COMPLETED", "STARTUP_FAILURE"),
                run("Test", t, "IN_PROGRESS", ""),
                run("Lint", t, "COMPLETED", "CANCELLED"),
                status("ERROR"),
            ])),
            Some(Checks::Failing(3)),
        );
    }

    #[test]
    fn parse_pull_request_counts_only_the_latest_run_of_a_rerun_check() {
        let (earlier, later) = ("2026-10-16T06:00:00Z", "2026-10-16T07:00:00Z");
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Test", earlier, "COMPLETED", "FAILURE"),
                run("Test", later, "COMPLETED", "SUCCESS"),
            ])),
            Some(Checks::Passing),
        );
        assert_eq!(
            checks_of(&serde_json::json!([
                status_context(earlier, "FAILURE"),
                status_context(later, "SUCCESS"),
            ])),
            Some(Checks::Passing),
        );

        let mut nightly = run("Test", earlier, "COMPLETED", "FAILURE");
        nightly["workflowName"] = "Nightly".into();
        assert_eq!(
            checks_of(&serde_json::json!([
                run("Test", later, "COMPLETED", "SUCCESS"),
                nightly
            ])),
            Some(Checks::Failing(1)),
            "a same-named check in another workflow is a separate check",
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
    fn cwd_to_str_utf8_path_is_preserved() {
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
    fn check_output_success_preserves_stdout() {
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
    fn stderr_summary_blank_lines_have_no_summary() {
        assert_eq!(stderr_summary(b"\n   \n\t\n"), "");
    }
}
