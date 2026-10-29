//! Background git / `gh` probes that keep the status bar's branch and pull-request segments
//! current. Probes run as tokio child processes and report back through the App's event channel,
//! so a slow `git` or a network-bound `gh pr view` never stalls input or rendering.

pub(crate) mod pull_request;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::FutureExt as _;

use ox_core::config::StatusLineSegment;
use ox_core::git;

use self::pull_request::{PullRequest, PullRequestDetail};
use crate::app_event::{AppEvent, AppEventSender};

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
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProbeEnd {
    probe: Probe,
    update: Option<GitUpdate>,
}

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
    events: AppEventSender,
}

impl GitProbes {
    /// Probes the branch and the pull request only as far as `segments` read them. The first branch
    /// probe waits one interval, and the pull request probes on the first [`poll`](Self::poll).
    /// Each run reports its end to `events`, which the App hands back to [`Self::finish`].
    pub(crate) fn new(
        cwd: PathBuf,
        segments: &[StatusLineSegment],
        events: AppEventSender,
        now: Instant,
    ) -> Self {
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
            events,
        }
    }

    /// Spawns every probe that is due and not already running. Must run inside a tokio runtime.
    pub(crate) fn poll(&mut self, now: Instant) {
        if let Some(schedule) = &mut self.branch
            && schedule.start(now)
        {
            let cwd = self.cwd.clone();
            spawn_probe(&self.events, Probe::Branch, async move {
                GitUpdate::Branch(git::current_branch(&cwd).await)
            });
        }
        if let Some(schedule) = &mut self.pull_request
            && schedule.start(now)
        {
            self.pull_request_stale = false;
            let cwd = self.cwd.clone();
            let detail = self.pull_request_detail;
            spawn_probe(&self.events, Probe::PullRequest, async move {
                GitUpdate::PullRequest(pull_request::current_pull_request(&cwd, detail).await)
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

    /// The update a finished run reports, scheduling its probe's next run one interval after `now`.
    /// A stale pull-request run reports a clear and asks for a fresh one.
    pub(crate) fn finish(&mut self, end: ProbeEnd, now: Instant) -> Option<GitUpdate> {
        let ProbeEnd { probe, update } = end;
        let schedule = match probe {
            Probe::Branch => self.branch.as_mut(),
            Probe::PullRequest => self.pull_request.as_mut(),
        };
        if let Some(schedule) = schedule {
            schedule.finish(now);
        }
        if probe == Probe::PullRequest && self.pull_request_stale {
            if let Some(schedule) = self.pull_request.as_mut() {
                schedule.next_due = Some(now);
            }
            return Some(GitUpdate::PullRequest(None));
        }
        update
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
        let end = ProbeEnd {
            probe,
            update: Some(update),
        };
        self.events.send(AppEvent::Probe(end)).unwrap();
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

/// Runs `run` on its own task and reports its end to `events`, so a run that panics or times out
/// still frees its probe's schedule. A timed-out run is dropped, which kills its child process.
fn spawn_probe(
    events: &AppEventSender,
    probe: Probe,
    run: impl Future<Output = GitUpdate> + Send + 'static,
) {
    let events = events.clone();
    tokio::spawn(async move {
        let run = std::panic::AssertUnwindSafe(run).catch_unwind();
        let update = tokio::time::timeout(PROBE_TIMEOUT, run)
            .await
            .ok()
            .and_then(Result::ok);
        _ = events.send(AppEvent::Probe(ProbeEnd { probe, update }));
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
    use tokio::sync::mpsc;

    use ox_core::git::testing::with_isolated_git_env_async;

    use super::pull_request::testing::pull_request;
    use super::*;

    const PULL_REQUEST: &[StatusLineSegment] = &[StatusLineSegment::PullRequest];
    const BOTH: &[StatusLineSegment] =
        &[StatusLineSegment::GitBranch, StatusLineSegment::PullRequest];

    fn unread_probes(segments: &[StatusLineSegment], now: Instant) -> GitProbes {
        GitProbes::new(
            PathBuf::from("/repo"),
            segments,
            mpsc::unbounded_channel().0,
            now,
        )
    }

    fn probe_end(event: Option<AppEvent>) -> ProbeEnd {
        let Some(AppEvent::Probe(end)) = event else {
            panic!("expected a probe end");
        };
        end
    }

    mod new {
        use super::*;

        #[test]
        fn defers_the_branch_probe_and_runs_the_pull_request_probe_first() {
            let now = Instant::now();

            let probes = unread_probes(BOTH, now);

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
            let probes = |segments| unread_probes(segments, now);

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
                unread_probes(segments, Instant::now()).pull_request_detail
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
            let (events, _rx) = mpsc::unbounded_channel();
            let mut probes = GitProbes::new(dir.path().to_path_buf(), BOTH, events, now);

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
        async fn reports_each_finished_probe_to_the_app() {
            let cases: [(&[StatusLineSegment], _, _); 2] = [
                (
                    PULL_REQUEST,
                    Probe::PullRequest,
                    GitUpdate::PullRequest(None),
                ),
                (
                    &[StatusLineSegment::GitBranch],
                    Probe::Branch,
                    GitUpdate::Branch(None),
                ),
            ];
            for (segments, probe, update) in cases {
                let dir = tempfile::tempdir().unwrap();
                let now = Instant::now();
                let (events, mut rx) = mpsc::unbounded_channel();
                let mut probes = GitProbes::new(dir.path().to_path_buf(), segments, events, now);

                let event = with_isolated_git_env_async(async {
                    probes.poll(now + BRANCH_REFRESH_INTERVAL);
                    rx.recv().await
                })
                .await;

                let expected = ProbeEnd {
                    probe,
                    update: Some(update),
                };
                assert_eq!(probe_end(event), expected, "not a repo");
            }
        }
    }

    mod next_poll {
        use super::*;

        #[test]
        fn is_the_earliest_probe_not_yet_running() {
            let now = Instant::now();
            let mut probes = unread_probes(BOTH, now);
            assert_eq!(probes.next_poll(), Some(now));

            assert!(probes.pull_request.as_mut().unwrap().start(now));

            assert_eq!(probes.next_poll(), Some(now + BRANCH_REFRESH_INTERVAL));
        }
    }

    mod finish {
        use super::*;

        #[test]
        fn reschedules_only_the_probe_that_finished() {
            let start = Instant::now();
            let mut probes = unread_probes(BOTH, start);
            assert!(
                probes
                    .branch
                    .as_mut()
                    .unwrap()
                    .start(start + BRANCH_REFRESH_INTERVAL)
            );
            assert!(probes.pull_request.as_mut().unwrap().start(start));
            let branch = || GitUpdate::Branch(Some("main".to_owned()));
            let branch_end = ProbeEnd {
                probe: Probe::Branch,
                update: Some(branch()),
            };
            let pull_request_end = ProbeEnd {
                probe: Probe::PullRequest,
                update: Some(GitUpdate::PullRequest(None)),
            };

            let branch_at = start + Duration::from_secs(1);
            assert_eq!(probes.finish(branch_end, branch_at), Some(branch()));
            let branch_due = branch_at + BRANCH_REFRESH_INTERVAL;
            assert_eq!(probes.branch.as_ref().unwrap().next_due, Some(branch_due));
            assert_eq!(probes.pull_request.as_ref().unwrap().next_due, None);

            let pull_request_at = branch_at + Duration::from_secs(1);
            assert_eq!(
                probes.finish(pull_request_end, pull_request_at),
                Some(GitUpdate::PullRequest(None))
            );
            assert_eq!(
                probes.pull_request.unwrap().next_due,
                Some(pull_request_at + PULL_REQUEST_REFRESH_INTERVAL)
            );
            assert_eq!(probes.branch.unwrap().next_due, Some(branch_due));
        }

        #[test]
        fn a_stale_run_reports_a_clear_and_asks_for_a_fresh_one() {
            let start = Instant::now();
            let mut probes = unread_probes(BOTH, start);
            assert!(probes.pull_request.as_mut().unwrap().start(start));
            probes.invalidate_pull_request(start);
            let end = ProbeEnd {
                probe: Probe::PullRequest,
                update: Some(GitUpdate::PullRequest(Some(pull_request(7)))),
            };
            let finished = start + Duration::from_secs(1);

            assert_eq!(
                probes.finish(end, finished),
                Some(GitUpdate::PullRequest(None)),
                "the stale run must not report its value",
            );
            assert_eq!(
                probes.pull_request_due(),
                Some(finished),
                "the fresh run is due immediately",
            );
        }

        #[test]
        fn reschedules_a_panicked_run_without_an_update() {
            let now = Instant::now();
            let mut probes = unread_probes(BOTH, now);
            assert!(
                probes
                    .branch
                    .as_mut()
                    .unwrap()
                    .start(now + BRANCH_REFRESH_INTERVAL)
            );
            let end = ProbeEnd {
                probe: Probe::Branch,
                update: None,
            };

            let update = probes.finish(end, now);

            assert_eq!(update, None);
            assert!(
                probes.branch.unwrap().next_due.is_some(),
                "branch probe stuck in flight"
            );
        }
    }

    mod spawn_probe {
        use super::*;

        const STOPPED: ProbeEnd = ProbeEnd {
            probe: Probe::Branch,
            update: None,
        };

        #[tokio::test]
        async fn reports_a_panicked_run_without_an_update() {
            let (tx, mut rx) = mpsc::unbounded_channel();

            spawn_probe(&tx, Probe::Branch, async { panic!("probe failed") });
            drop(tx);

            assert_eq!(probe_end(rx.recv().await), STOPPED);
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

            assert_eq!(probe_end(rx.recv().await), STOPPED);
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
