//! Background git / `gh` probes that keep the status bar's branch and pull-request segments
//! current. Probes run as tokio child processes and report back through a channel the event loop
//! selects on, so a slow `git` or a network-bound `gh pr view` never stalls input or rendering.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::util::git::{self, PullRequest};

/// Gap between a branch probe finishing and the next one starting. Branch changes outside the
/// session (manual `git checkout`) only become visible after one interval.
const BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Gap between pull-request probes, longer than the branch gap since `gh pr view` hits the network.
const PULL_REQUEST_REFRESH_INTERVAL: Duration = Duration::from_mins(1);

/// Result of one finished probe.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GitUpdate {
    Branch(Option<String>),
    PullRequest(Option<PullRequest>),
}

/// Schedules the probes and collects their results. At most one run per probe is in flight.
pub(crate) struct GitProbes {
    /// `None` disables every probe.
    cwd: Option<PathBuf>,
    branch: Schedule,
    /// `None` when the status line has no `pull-request` segment.
    pull_request: Option<Schedule>,
    tx: mpsc::UnboundedSender<GitUpdate>,
    rx: mpsc::UnboundedReceiver<GitUpdate>,
}

impl GitProbes {
    /// The caller resolved the branch at startup, so the first branch probe waits one interval.
    /// The pull request has no startup value and probes on the first [`poll`](Self::poll).
    pub(crate) fn new(cwd: Option<PathBuf>, track_pull_request: bool, now: Instant) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            cwd,
            branch: Schedule::new(BRANCH_REFRESH_INTERVAL, now + BRANCH_REFRESH_INTERVAL),
            pull_request: track_pull_request
                .then(|| Schedule::new(PULL_REQUEST_REFRESH_INTERVAL, now)),
            tx,
            rx,
        }
    }

    /// Spawns every probe that is due and not already running. Must run inside a tokio runtime.
    pub(crate) fn poll(&mut self, now: Instant) {
        let Some(cwd) = self.cwd.as_deref() else {
            return;
        };
        if self.branch.start(now) {
            let cwd = cwd.to_path_buf();
            spawn_probe(&self.tx, async move {
                GitUpdate::Branch(git::current_branch_async(&cwd).await)
            });
        }
        if let Some(schedule) = &mut self.pull_request
            && schedule.start(now)
        {
            let cwd = cwd.to_path_buf();
            spawn_probe(&self.tx, async move {
                GitUpdate::PullRequest(git::current_pull_request(&cwd).await)
            });
        }
    }

    /// Waits for the next finished probe and schedules that probe's next run one interval out.
    /// Cancel-safe, so it can sit in a `tokio::select!` arm.
    pub(crate) async fn recv(&mut self) -> Option<GitUpdate> {
        let update = self.rx.recv().await?;
        let schedule = match update {
            GitUpdate::Branch(_) => Some(&mut self.branch),
            GitUpdate::PullRequest(_) => self.pull_request.as_mut(),
        };
        if let Some(schedule) = schedule {
            schedule.finish(Instant::now());
        }
        Some(update)
    }

    #[cfg(test)]
    pub(crate) fn inject(&self, update: GitUpdate) {
        self.tx.send(update).unwrap();
    }
}

fn spawn_probe(
    tx: &mpsc::UnboundedSender<GitUpdate>,
    probe: impl Future<Output = GitUpdate> + Send + 'static,
) {
    let tx = tx.clone();
    tokio::spawn(async move {
        _ = tx.send(probe.await);
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
        if self.next_due.is_some_and(|due| now >= due) {
            self.next_due = None;
            true
        } else {
            false
        }
    }

    fn finish(&mut self, now: Instant) {
        self.next_due = Some(now + self.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECV_TIMEOUT: Duration = Duration::from_secs(10);

    // ── GitProbes::new ──

    #[test]
    fn new_defers_the_branch_probe_and_runs_the_pull_request_probe_first() {
        let now = Instant::now();
        let probes = GitProbes::new(Some(PathBuf::from("/tmp")), true, now);
        assert_eq!(probes.branch.next_due, Some(now + BRANCH_REFRESH_INTERVAL));
        assert_eq!(
            probes.pull_request.as_ref().and_then(|s| s.next_due),
            Some(now),
        );

        let untracked = GitProbes::new(Some(PathBuf::from("/tmp")), false, now);
        assert!(untracked.pull_request.is_none());
    }

    // ── GitProbes::recv ──

    #[tokio::test]
    async fn recv_delivers_each_probe_result_and_reschedules_from_completion() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut probes = GitProbes::new(Some(dir.path().to_path_buf()), true, now);

        probes.poll(now + BRANCH_REFRESH_INTERVAL);
        probes.poll(now + PULL_REQUEST_REFRESH_INTERVAL * 2);
        assert_eq!(probes.branch.next_due, None, "branch in flight");
        assert_eq!(
            probes.pull_request.as_ref().unwrap().next_due,
            None,
            "pull request in flight",
        );

        let mut updates = Vec::new();
        for _ in 0..2 {
            let update = tokio::time::timeout(RECV_TIMEOUT, probes.recv())
                .await
                .expect("probe finishes");
            updates.push(update.unwrap());
        }
        assert!(
            updates.contains(&GitUpdate::Branch(None)),
            "updates={updates:?}"
        );
        assert!(
            updates.contains(&GitUpdate::PullRequest(None)),
            "updates={updates:?}"
        );

        let after = Instant::now();
        assert!(probes.branch.next_due.is_some_and(|due| due > after));
        assert!(
            probes
                .pull_request
                .unwrap()
                .next_due
                .is_some_and(|due| due > after)
        );
    }

    // ── Schedule::start ──

    #[test]
    fn start_claims_a_due_slot_once_until_finish() {
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
