//! The open pull request for the current branch, read through `gh pr view`, with its checks
//! summarized the way `gh pr checks` buckets them.

use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;

use ox_core::git::probe;

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
pub(super) enum PullRequestDetail {
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
/// Returns `None` when `gh` is missing, the user is unauthenticated, or no PR is open. Dropping the
/// future kills the child process.
pub(super) async fn current_pull_request(
    cwd: &Path,
    detail: PullRequestDetail,
) -> Option<PullRequest> {
    let args = ["pr", "view", "--json", detail.json_fields()];
    let output = probe("gh pr", cwd, "gh", &args).await?;
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

// ── Testing ──

#[cfg(test)]
pub(crate) mod testing {
    use super::PullRequest;

    /// A pull request numbered `number` with a GitHub URL and no checks or review.
    pub(crate) fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
            checks: None,
            review: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ox_core::git::testing::with_isolated_git_env_async;

    use super::*;

    mod pull_request_detail {
        use super::*;

        mod json_fields {
            use super::*;

            #[test]
            fn requests_every_field_its_detail_parses() {
                let full = serde_json::json!({
                    "number": 86,
                    "url": "https://github.com/o/r/pull/86",
                    "statusCheckRollup": [{
                        "__typename": "CheckRun",
                        "name": "test",
                        "workflowName": "CI",
                        "startedAt": "2026-01-01T00:00:00Z",
                        "status": "COMPLETED",
                        "conclusion": "FAILURE",
                    }],
                    "reviewDecision": "APPROVED",
                });
                let cases = [
                    (PullRequestDetail::Link, None, None),
                    (
                        PullRequestDetail::Status,
                        Some(Checks::Failing(1)),
                        Some(ReviewDecision::Approved),
                    ),
                ];
                for (detail, checks, review) in cases {
                    let requested: serde_json::Map<_, _> = detail
                        .json_fields()
                        .split(',')
                        .map(|field| (field.to_owned(), full[field].clone()))
                        .collect();

                    let parsed = parse_pull_request(&serde_json::to_vec(&requested).unwrap());

                    let expected = PullRequest {
                        number: 86,
                        url: "https://github.com/o/r/pull/86".to_owned(),
                        checks,
                        review,
                    };
                    assert_eq!(parsed, Some(expected), "{detail:?}");
                }
            }
        }
    }

    mod current_pull_request {
        use super::*;

        #[tokio::test]
        async fn outside_a_repo_is_absent() {
            let dir = tempfile::tempdir().unwrap();
            for detail in [PullRequestDetail::Link, PullRequestDetail::Status] {
                assert_eq!(
                    with_isolated_git_env_async(current_pull_request(dir.path(), detail)).await,
                    None,
                    "{detail:?}",
                );
            }
        }
    }

    mod parse_pull_request {
        use super::*;

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
        fn extracts_number_and_url() {
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
        fn reads_the_review_decision() {
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
        fn summarizes_checks_by_their_worst_state() {
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
        fn counts_only_the_latest_run_of_a_rerun_check() {
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
        fn drops_invalid_payloads() {
            assert_eq!(parse_pull_request(b""), None);
            assert_eq!(parse_pull_request(b"not json"), None);
            assert_eq!(parse_pull_request(br#"{"number":86}"#), None);
            assert_eq!(parse_pull_request(br#"{"url":"https://x"}"#), None);
            assert_eq!(parse_pull_request(br#"{"number":86,"url":""}"#), None);
            assert_eq!(parse_pull_request(br#"{"number":-1,"url":"x"}"#), None);
            assert_eq!(parse_pull_request(br#"{"number":86,"url":42}"#), None);
        }
    }
}
