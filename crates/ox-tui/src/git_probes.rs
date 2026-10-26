//! Background git / `gh` probes that keep the status bar's branch and pull-request segments
//! current. Probes run as tokio child processes and report back through a channel the event loop
//! selects on, so a slow `git` or a network-bound `gh pr view` never stalls input or rendering.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use tokio::sync::mpsc;

use ox_core::config::StatusLineSegment;
use ox_core::util::git::{self, PullRequest, PullRequestDetail};

use crate::now;

/// Gap between a branch probe finishing and the next one starting. Branch changes outside the
/// session (manual `git checkout`) only become visible after one interval.
const BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Gap between pull-request probes, longer than the branch gap since `gh pr view` hits the network.
const PULL_REQUEST_REFRESH_INTERVAL: Duration = Duration::from_mins(1);

/// A probe that outlives this deadline is killed, so a hung `git` or network-bound `gh` cannot
/// freeze its segment for the rest of the session.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of one finished probe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GitUpdate {
    Branch(Option<String>),
    PullRequest(Option<PullRequest>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Branch,
    PullRequest,
}

/// One probe run's end, with no update when the run panicked or timed out.
type Finished = (Probe, Option<GitUpdate>);

/// Schedules the probes and collects their results. At most one run per probe is in flight.
pub(crate) struct GitProbes {
    cwd: PathBuf,
    /// `None` when no status line segment reads the branch.
    branch: Option<Schedule>,
    /// `None` when no status line segment reads the pull request.
    pull_request: Option<Schedule>,
    pull_request_detail: PullRequestDetail,
    /// Set while a pull-request run may still report a value from before a branch change.
    pull_request_stale: bool,
    tx: mpsc::UnboundedSender<Finished>,
    rx: mpsc::UnboundedReceiver<Finished>,
}

impl GitProbes {
    /// Probes the branch and the pull request only as far as `segments` read them. The first branch
    /// probe waits one interval, and the pull request probes on the first [`poll`](Self::poll).
    pub(crate) fn new(cwd: PathBuf, segments: &[StatusLineSegment], now: Instant) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let detail = segments
            .iter()
            .filter_map(|segment| match segment {
                StatusLineSegment::PullRequest => Some(PullRequestDetail::Link),
                StatusLineSegment::PullRequestChecks | StatusLineSegment::PullRequestReview => {
                    Some(PullRequestDetail::Status)
                }
                _ => None,
            })
            .max();
        Self {
            cwd,
            branch: segments
                .contains(&StatusLineSegment::GitBranch)
                .then(|| Schedule::new(BRANCH_REFRESH_INTERVAL, now + BRANCH_REFRESH_INTERVAL)),
            pull_request: detail.map(|_| Schedule::new(PULL_REQUEST_REFRESH_INTERVAL, now)),
            pull_request_detail: detail.unwrap_or(PullRequestDetail::Link),
            pull_request_stale: false,
            tx,
            rx,
        }
    }

    /// Spawns every probe that is due and not already running. Must run inside a tokio runtime.
    pub(crate) fn poll(&mut self, now: Instant) {
        if let Some(schedule) = &mut self.branch
            && schedule.start(now)
        {
            let cwd = self.cwd.clone();
            spawn_probe(&self.tx, Probe::Branch, async move {
                GitUpdate::Branch(git::current_branch(&cwd).await)
            });
        }
        if let Some(schedule) = &mut self.pull_request
            && schedule.start(now)
        {
            self.pull_request_stale = false;
            let cwd = self.cwd.clone();
            let detail = self.pull_request_detail;
            spawn_probe(&self.tx, Probe::PullRequest, async move {
                GitUpdate::PullRequest(git::current_pull_request(&cwd, detail).await)
            });
        }
    }

    /// When [`Self::poll`] next starts a probe, or `None` while every probe is off or running.
    pub(crate) fn next_poll(&self) -> Option<Instant> {
        [&self.branch, &self.pull_request]
            .into_iter()
            .flatten()
            .filter_map(|schedule| schedule.next_due)
            .min()
    }

    /// Waits for the next probe update, scheduling each finished probe's next run one interval out.
    /// A stale pull-request run reports a clear and asks for a fresh one. Cancel-safe, so it can sit
    /// in a `tokio::select!` arm.
    pub(crate) async fn recv(&mut self) -> Option<GitUpdate> {
        loop {
            let (probe, update) = self.rx.recv().await?;
            let current = now();
            let schedule = match probe {
                Probe::Branch => self.branch.as_mut(),
                Probe::PullRequest => self.pull_request.as_mut(),
            };
            if let Some(schedule) = schedule {
                schedule.finish(current);
            }
            if probe == Probe::PullRequest && self.pull_request_stale {
                if let Some(schedule) = self.pull_request.as_mut() {
                    schedule.next_due = Some(current);
                }
                return Some(GitUpdate::PullRequest(None));
            }
            if update.is_some() {
                return update;
            }
        }
    }

    /// Discards whatever a pull-request run in flight reports and asks for a fresh one, so a branch
    /// change never pairs the new branch with the old branch's link, checks, or review state.
    pub(crate) fn invalidate_pull_request(&mut self, now: Instant) {
        let Some(schedule) = &mut self.pull_request else {
            return;
        };
        self.pull_request_stale = true;
        if schedule.next_due.is_some() {
            schedule.next_due = Some(now);
        }
    }

    #[cfg(test)]
    pub(crate) fn inject(&self, update: GitUpdate) {
        let probe = match update {
            GitUpdate::Branch(_) => Probe::Branch,
            GitUpdate::PullRequest(_) => Probe::PullRequest,
        };
        self.tx.send((probe, Some(update))).unwrap();
    }

    #[cfg(test)]
    pub(crate) fn pull_request_due(&self) -> Option<Instant> {
        self.pull_request
            .as_ref()
            .and_then(|schedule| schedule.next_due)
    }

    #[cfg(test)]
    pub(crate) fn pull_request_stale(&self) -> bool {
        self.pull_request_stale
    }
}

/// Runs `run` on its own task and reports its end to `tx`, so a run that panics or times out still
/// frees its probe's schedule. A timed-out run is dropped, which kills its child process.
fn spawn_probe(
    tx: &mpsc::UnboundedSender<Finished>,
    probe: Probe,
    run: impl Future<Output = GitUpdate> + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        let run = std::panic::AssertUnwindSafe(run).catch_unwind();
        let update = tokio::time::timeout(PROBE_TIMEOUT, run)
            .await
            .ok()
            .and_then(Result::ok);
        _ = tx.send((probe, update));
    });
}

struct Schedule {
    interval: Duration,
    /// `None` while a run is in flight.
    next_due: Option<Instant>,
}

impl Schedule {
    fn new(interval: Duration, first_due: Instant) -> Self {
        Self {
            interval,
            next_due: Some(first_due),
        }
    }

    /// Claims a due, idle slot. Returns whether the caller should start a run.
    fn start(&mut self, now: Instant) -> bool {
        self.next_due.take_if(|due| now >= *due).is_some()
    }

    fn finish(&mut self, now: Instant) {
        self.next_due = Some(now + self.interval);
    }
}

#[cfg(test)]
mod tests {
    use ox_core::util::git::testing::{pull_request, with_isolated_git_env_async};

    use super::*;

    const PULL_REQUEST: &[StatusLineSegment] = &[StatusLineSegment::PullRequest];
    const BOTH: &[StatusLineSegment] =
        &[StatusLineSegment::GitBranch, StatusLineSegment::PullRequest];

    mod new {
        use super::*;

        #[test]
        fn defers_the_branch_probe_and_runs_the_pull_request_probe_first() {
            let now = Instant::now();

            let probes = GitProbes::new(PathBuf::from("/repo"), BOTH, now);

            assert_eq!(
                probes.branch.as_ref().and_then(|s| s.next_due),
                Some(now + BRANCH_REFRESH_INTERVAL)
            );
            assert_eq!(
                probes.pull_request.as_ref().and_then(|s| s.next_due),
                Some(now),
            );
        }

        #[test]
        fn probes_only_what_the_segments_read() {
            let now = Instant::now();
            let probes = |segments| GitProbes::new(PathBuf::from("/repo"), segments, now);

            let branch_only = probes(&[StatusLineSegment::GitBranch]);
            let pull_request_only = probes(PULL_REQUEST);
            let neither = probes(&[]);

            assert!(branch_only.branch.is_some() && branch_only.pull_request.is_none());
            assert!(pull_request_only.branch.is_none() && pull_request_only.pull_request.is_some());
            assert_eq!(neither.next_poll(), None);
        }

        #[test]
        fn reads_the_pull_request_status_only_for_a_segment_that_shows_it() {
            let detail = |segments: &[StatusLineSegment]| {
                GitProbes::new(PathBuf::from("/repo"), segments, Instant::now()).pull_request_detail
            };
            assert_eq!(detail(PULL_REQUEST), PullRequestDetail::Link);
            for status_segment in [
                StatusLineSegment::PullRequestChecks,
                StatusLineSegment::PullRequestReview,
            ] {
                assert_eq!(
                    detail(&[StatusLineSegment::PullRequest, status_segment]),
                    PullRequestDetail::Status,
                    "{status_segment:?}",
                );
            }
        }
    }

    mod poll {
        use super::*;

        #[tokio::test]
        async fn claims_each_probe_once_it_is_due() {
            let dir = tempfile::tempdir().unwrap();
            let now = Instant::now();
            let mut probes = GitProbes::new(dir.path().to_path_buf(), BOTH, now);

            probes.poll(now);
            assert_eq!(
                probes.branch.as_ref().unwrap().next_due,
                Some(now + BRANCH_REFRESH_INTERVAL),
                "branch not yet due",
            );
            assert_eq!(probes.pull_request.as_ref().unwrap().next_due, None);

            probes.poll(now + BRANCH_REFRESH_INTERVAL);
            assert_eq!(probes.branch.unwrap().next_due, None);
        }

        #[tokio::test]
        async fn reports_a_finished_pull_request_probe_to_recv() {
            let dir = tempfile::tempdir().unwrap();
            let now = Instant::now();
            let mut probes = GitProbes::new(dir.path().to_path_buf(), PULL_REQUEST, now);

            let update = with_isolated_git_env_async(async {
                probes.poll(now);
                probes.recv().await
            })
            .await;

            assert_eq!(update, Some(GitUpdate::PullRequest(None)), "not a repo");
        }

        #[tokio::test]
        async fn reports_a_finished_branch_probe_to_recv() {
            let dir = tempfile::tempdir().unwrap();
            let now = Instant::now();
            let segments = &[StatusLineSegment::GitBranch];
            let mut probes = GitProbes::new(dir.path().to_path_buf(), segments, now);

            let update = with_isolated_git_env_async(async {
                probes.poll(now + BRANCH_REFRESH_INTERVAL);
                probes.recv().await
            })
            .await;

            assert_eq!(update, Some(GitUpdate::Branch(None)), "not a repo");
        }
    }

    mod next_poll {
        use super::*;

        #[test]
        fn is_the_earliest_probe_not_yet_running() {
            let now = Instant::now();
            let mut probes = GitProbes::new(PathBuf::from("/repo"), BOTH, now);
            assert_eq!(probes.next_poll(), Some(now));

            assert!(probes.pull_request.as_mut().unwrap().start(now));

            assert_eq!(probes.next_poll(), Some(now + BRANCH_REFRESH_INTERVAL));
        }
    }

    mod recv {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn reschedules_only_the_probe_that_finished() {
            let start = now();
            let mut probes = GitProbes::new(PathBuf::from("/repo"), BOTH, start);
            assert!(
                probes
                    .branch
                    .as_mut()
                    .unwrap()
                    .start(start + BRANCH_REFRESH_INTERVAL)
            );
            assert!(probes.pull_request.as_mut().unwrap().start(start));

            tokio::time::advance(Duration::from_secs(1)).await;
            probes.inject(GitUpdate::Branch(Some("main".to_owned())));
            assert_eq!(
                probes.recv().await,
                Some(GitUpdate::Branch(Some("main".to_owned())))
            );
            let branch_due = now() + BRANCH_REFRESH_INTERVAL;
            assert_eq!(probes.branch.as_ref().unwrap().next_due, Some(branch_due));
            assert_eq!(probes.pull_request.as_ref().unwrap().next_due, None);

            tokio::time::advance(Duration::from_secs(1)).await;
            probes.inject(GitUpdate::PullRequest(None));
            assert_eq!(probes.recv().await, Some(GitUpdate::PullRequest(None)));
            assert_eq!(
                probes.pull_request.unwrap().next_due,
                Some(now() + PULL_REQUEST_REFRESH_INTERVAL)
            );
            assert_eq!(probes.branch.unwrap().next_due, Some(branch_due));
        }

        #[tokio::test(start_paused = true)]
        async fn a_stale_run_reports_a_clear_and_asks_for_a_fresh_one() {
            let start = now();
            let mut probes = GitProbes::new(PathBuf::from("/repo"), BOTH, start);
            assert!(probes.pull_request.as_mut().unwrap().start(start));
            probes.invalidate_pull_request(now());

            probes.inject(GitUpdate::PullRequest(Some(pull_request(7))));
            assert_eq!(
                probes.recv().await,
                Some(GitUpdate::PullRequest(None)),
                "the stale run must not report its value",
            );
            assert_eq!(
                probes.pull_request_due(),
                Some(now()),
                "the fresh run is due immediately",
            );
        }

        #[tokio::test]
        async fn reschedules_a_panicked_run_and_returns_the_next_update() {
            let now = Instant::now();
            let mut probes = GitProbes::new(PathBuf::from("/repo"), BOTH, now);
            assert!(
                probes
                    .branch
                    .as_mut()
                    .unwrap()
                    .start(now + BRANCH_REFRESH_INTERVAL)
            );
            probes.tx.send((Probe::Branch, None)).unwrap();
            probes.inject(GitUpdate::PullRequest(None));

            let update = probes.recv().await;

            assert_eq!(update, Some(GitUpdate::PullRequest(None)));
            assert!(
                probes.branch.unwrap().next_due.is_some(),
                "branch probe stuck in flight"
            );
        }
    }

    mod spawn_probe {
        use super::*;

        #[tokio::test]
        async fn reports_a_panicked_run_without_an_update() {
            let (tx, mut rx) = mpsc::unbounded_channel();

            spawn_probe(&tx, Probe::Branch, async { panic!("probe failed") });
            drop(tx);

            assert_eq!(rx.recv().await, Some((Probe::Branch, None)));
        }

        #[tokio::test(start_paused = true)]
        async fn reports_a_timed_out_run_without_an_update_and_drops_it() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel::<()>();

            spawn_probe(&tx, Probe::Branch, async move {
                let _sender = dropped_tx;
                std::future::pending::<GitUpdate>().await
            });
            drop(tx);
            tokio::time::advance(PROBE_TIMEOUT).await;

            assert_eq!(rx.recv().await, Some((Probe::Branch, None)));
            assert_eq!(
                dropped_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed),
                "the timed-out run is dropped",
            );
        }
    }

    mod schedule {
        use super::*;

        mod start {
            use super::*;

            #[test]
            fn claims_a_due_slot_once_until_finish() {
                let now = Instant::now();
                let mut schedule = Schedule::new(BRANCH_REFRESH_INTERVAL, now);

                assert!(schedule.start(now));
                assert!(
                    !schedule.start(now + BRANCH_REFRESH_INTERVAL * 10),
                    "in-flight run blocks the next start",
                );

                schedule.finish(now);
                assert!(!schedule.start(now + Duration::from_millis(100)));
                assert!(schedule.start(now + BRANCH_REFRESH_INTERVAL));
            }
        }
    }
}
