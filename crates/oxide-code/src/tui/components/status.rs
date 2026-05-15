//! Configurable status line component.

mod line;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::agent::event::UsageSnapshot;
use crate::config::{Effort, StatusLineSegment};
use crate::tui::glyphs::SPINNER_FRAMES;
use crate::tui::theme::Theme;
use crate::util::git;

use self::line::{StatusLine, StatusLineState};

const TICKS_PER_FRAME: usize = 5;

/// How often the status bar re-probes git for the current branch. Branch changes outside the
/// session (manual `git checkout`) only become visible after one interval.
const GIT_BRANCH_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How often the status bar re-probes `gh` for the open pull request. Slower than the branch
/// probe because `gh pr view` hits the network.
const PR_REFRESH_INTERVAL: Duration = Duration::from_mins(1);

/// Status bar at the top of the TUI.
pub(crate) struct StatusBar {
    theme: Theme,
    line: StatusLine,
    current_time_minute: Option<u16>,
    model: String,
    effort: Option<Effort>,
    title: Option<String>,
    usage: Option<UsageSnapshot>,
    cwd: String,
    /// `None` collapses every git probe to a no-op.
    git_cwd: Option<PathBuf>,
    git_branch: Option<String>,
    pull_request: Option<git::PullRequest>,
    /// `true` while the `pull-request` segment is configured. Skips the `gh` probe entirely when
    /// the user hasn't opted in.
    track_pull_request: bool,
    last_branch_probe: Option<Instant>,
    last_pr_probe: Option<Instant>,
    status: Status,
    spinner_frame: usize,
    tick_counter: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    ToolRunning { name: String },
    Compacting,
    Cancelling,
    ExitArmed { until: Instant },
}

impl StatusBar {
    pub(crate) fn new(
        theme: &Theme,
        segments: Vec<StatusLineSegment>,
        model: String,
        effort: Option<Effort>,
        cwd: String,
        git_cwd: Option<PathBuf>,
        git_branch: Option<String>,
    ) -> Self {
        let current_time_minute = segments
            .contains(&StatusLineSegment::CurrentTime)
            .then(current_time_minute);
        let track_pull_request = segments.contains(&StatusLineSegment::PullRequest);
        Self {
            theme: theme.clone(),
            line: StatusLine::new(segments),
            current_time_minute,
            model,
            effort,
            title: None,
            usage: None,
            cwd,
            git_cwd,
            git_branch,
            pull_request: None,
            track_pull_request,
            last_branch_probe: None,
            last_pr_probe: None,
            status: Status::Idle,
            spinner_frame: 0,
            tick_counter: 0,
        }
    }

    pub(crate) fn set_title(&mut self, title: Option<String>) {
        self.title = title.filter(|t| !t.trim().is_empty());
    }

    pub(crate) fn set_model(&mut self, model: String) {
        debug_assert!(
            !model.trim().is_empty(),
            "set_model received empty / whitespace-only label",
        );
        self.model = model;
    }

    pub(crate) fn set_effort(&mut self, effort: Option<Effort>) {
        self.effort = effort;
    }

    pub(crate) fn set_usage(&mut self, usage: Option<UsageSnapshot>) {
        self.usage = usage;
    }

    /// Re-skin subsequent renders. The spinner / status state is unaffected.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        if status != self.status {
            self.spinner_frame = 0;
            self.tick_counter = 0;
        }
        self.status = status;
    }

    pub(crate) fn status(&self) -> &Status {
        &self.status
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> Option<UsageSnapshot> {
        self.usage
    }

    /// Returns `true` when time, animation, git-branch, or pull-request state changed and the
    /// caller should repaint.
    pub(crate) fn tick(&mut self) -> bool {
        let mut dirty = self.refresh_current_time();
        let now = Instant::now();
        if self.refresh_git_branch(now) {
            dirty = true;
        }
        if self.refresh_pull_request(now) {
            dirty = true;
        }
        if is_animated(&self.status) {
            self.tick_counter += 1;
            if self.tick_counter >= TICKS_PER_FRAME {
                self.tick_counter = 0;
                self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
                dirty = true;
            }
        }
        dirty
    }

    fn refresh_current_time(&mut self) -> bool {
        let Some(previous) = self.current_time_minute else {
            return false;
        };
        let current = current_time_minute();
        if current == previous {
            return false;
        }
        self.current_time_minute = Some(current);
        true
    }

    /// Re-probes the git branch when [`GIT_BRANCH_REFRESH_INTERVAL`] has elapsed. Returns `true`
    /// when the resolved branch changed.
    fn refresh_git_branch(&mut self, now: Instant) -> bool {
        let Some(cwd) = self.git_cwd.as_deref() else {
            return false;
        };
        if !should_probe(self.last_branch_probe, now, GIT_BRANCH_REFRESH_INTERVAL) {
            return false;
        }
        self.last_branch_probe = Some(now);
        let probed = git::current_branch(cwd);
        if probed == self.git_branch {
            return false;
        }
        self.git_branch = probed;
        true
    }

    /// Re-probes the open pull request via `gh` when [`PR_REFRESH_INTERVAL`] has elapsed. The
    /// probe is skipped entirely when the user hasn't configured the `pull-request` segment.
    fn refresh_pull_request(&mut self, now: Instant) -> bool {
        if !self.track_pull_request {
            return false;
        }
        let Some(cwd) = self.git_cwd.as_deref() else {
            return false;
        };
        if !should_probe(self.last_pr_probe, now, PR_REFRESH_INTERVAL) {
            return false;
        }
        self.last_pr_probe = Some(now);
        let probed = git::current_pull_request(cwd);
        if probed == self.pull_request {
            return false;
        }
        self.pull_request = probed;
        true
    }
}

/// Time-only predicate split out so the throttle can be exercised without shelling out.
fn should_probe(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last.is_none_or(|prev| now.duration_since(prev) >= interval)
}

impl StatusBar {
    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(self.theme.border_unfocused())
            .style(self.theme.surface());
        frame.render_widget(
            Paragraph::new(self.render_line(area.width)).block(block),
            area,
        );
    }
}

// ── Render Helpers ──

impl StatusBar {
    fn status_span(&self) -> Span<'static> {
        match &self.status {
            Status::Idle => Span::styled("Ready", self.theme.success()),
            Status::Streaming => self.busy_span("Streaming · Esc to interrupt"),
            Status::ToolRunning { name } => {
                self.busy_span(&format!("Running {name} · Esc to interrupt"))
            }
            Status::Compacting => self.busy_span("Compacting · Esc to interrupt"),
            Status::Cancelling => self.busy_span("Cancelling"),
            Status::ExitArmed { .. } => {
                Span::styled("Press Ctrl+C again to exit", self.theme.warning())
            }
        }
    }

    fn busy_span(&self, label: &str) -> Span<'static> {
        let spinner = SPINNER_FRAMES[self.spinner_frame];
        Span::styled(format!("{spinner} {label}"), self.theme.info())
    }

    fn render_line(&self, width: u16) -> ratatui::text::Line<'static> {
        self.line.render(
            &self.theme,
            &StatusLineState {
                model: &self.model,
                effort: self.effort,
                title: self.title.as_deref(),
                usage: self.usage,
                cwd: &self.cwd,
                git_branch: self.git_branch.as_deref(),
                pull_request: self.pull_request.as_ref(),
                status_span: self.status_span(),
            },
            width,
        )
    }
}

fn is_animated(status: &Status) -> bool {
    matches!(
        status,
        Status::Streaming | Status::ToolRunning { .. } | Status::Compacting | Status::Cancelling,
    )
}

fn current_time_minute() -> u16 {
    let now = time::OffsetDateTime::now_utc().to_offset(crate::util::time::local_offset());
    u16::from(now.hour()) * 60 + u16::from(now.minute())
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn test_bar() -> StatusBar {
        StatusBar::new(
            &Theme::default(),
            StatusLineSegment::DEFAULT.to_vec(),
            "test-model".to_owned(),
            Some(Effort::High),
            "~/test".to_owned(),
            None,
            Some("main".to_owned()),
        )
    }

    // ── set_title ──

    #[test]
    fn set_title_stores_non_empty_title() {
        let mut bar = test_bar();
        bar.set_title(Some("Fix auth bug".to_owned()));
        assert_eq!(bar.title.as_deref(), Some("Fix auth bug"));
    }

    #[test]
    fn set_title_none_clears_title() {
        let mut bar = test_bar();
        bar.set_title(Some("something".to_owned()));
        bar.set_title(None);
        assert!(bar.title.is_none());
    }

    #[test]
    fn set_title_drops_whitespace_only() {
        let mut bar = test_bar();
        bar.set_title(Some("   \n".to_owned()));
        assert!(bar.title.is_none());
    }

    // ── set_model ──

    #[test]
    fn set_model_replaces_displayed_model_label() {
        let mut bar = test_bar();
        bar.set_model("Opus 4.7".to_owned());
        assert_eq!(bar.model(), "Opus 4.7");
        let output = render_top_row(&mut bar, 80);
        assert!(
            output.contains("Opus 4.7"),
            "new label must reach the rendered bar: {output:?}",
        );
        assert!(
            !output.contains("test-model"),
            "old label must not survive: {output:?}",
        );
    }

    // ── set_status ──

    #[test]
    fn set_status_resets_spinner_on_transition() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);

        for _ in 0..TICKS_PER_FRAME * 3 {
            bar.tick();
        }
        assert_eq!(bar.spinner_frame, 3);

        bar.set_status(Status::ToolRunning {
            name: "bash".to_owned(),
        });
        assert_eq!(bar.spinner_frame, 0);
        assert_eq!(bar.tick_counter, 0);
    }

    #[test]
    fn set_status_same_status_preserves_spinner() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);

        for _ in 0..TICKS_PER_FRAME * 2 {
            bar.tick();
        }
        let frame_before = bar.spinner_frame;

        bar.set_status(Status::Streaming);
        assert_eq!(bar.spinner_frame, frame_before);
    }

    #[test]
    fn set_status_to_idle_resets_spinner() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);

        for _ in 0..TICKS_PER_FRAME * 2 {
            bar.tick();
        }

        bar.set_status(Status::Idle);
        assert_eq!(bar.spinner_frame, 0);
        assert_eq!(bar.tick_counter, 0);
        assert!(!bar.tick());
    }

    // ── tick ──

    #[test]
    fn tick_idle_is_false() {
        let mut bar = test_bar();
        assert!(!bar.tick());
        assert_eq!(bar.spinner_frame, 0);
        assert_eq!(bar.tick_counter, 0);
    }

    #[test]
    fn tick_without_current_time_segment_skips_minute_refresh() {
        let mut bar = StatusBar::new(
            &Theme::default(),
            vec![StatusLineSegment::Model, StatusLineSegment::RunState],
            "test-model".to_owned(),
            None,
            "~/test".to_owned(),
            None,
            None,
        );

        assert_eq!(bar.current_time_minute, None);
        assert!(!bar.tick());
        assert_eq!(bar.current_time_minute, None);
    }

    #[test]
    fn tick_idle_current_time_marks_dirty_on_minute_change() {
        let mut bar = StatusBar::new(
            &Theme::default(),
            vec![StatusLineSegment::CurrentTime],
            "test-model".to_owned(),
            None,
            "~/test".to_owned(),
            None,
            None,
        );
        let current = current_time_minute();
        bar.current_time_minute = Some((current + 1) % 1440);

        assert!(bar.tick());
        assert_eq!(bar.current_time_minute, Some(current));
        assert!(!bar.tick());
        assert_eq!(bar.spinner_frame, 0);
    }

    #[test]
    fn tick_streaming_before_threshold_does_not_advance_spinner_frame() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);

        for _ in 0..TICKS_PER_FRAME - 1 {
            assert!(!bar.tick());
        }
        assert_eq!(bar.tick_counter, TICKS_PER_FRAME - 1);
        assert_eq!(bar.spinner_frame, 0);
    }

    #[test]
    fn tick_streaming_advances_frame_at_threshold() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);

        for _ in 0..TICKS_PER_FRAME - 1 {
            bar.tick();
        }
        assert!(bar.tick());
        assert_eq!(bar.spinner_frame, 1);
        assert_eq!(bar.tick_counter, 0);
    }

    #[test]
    fn tick_wraps_spinner_frames() {
        let mut bar = test_bar();
        bar.set_status(Status::ToolRunning {
            name: "bash".to_owned(),
        });

        for _ in 0..SPINNER_FRAMES.len() * TICKS_PER_FRAME {
            bar.tick();
        }
        assert_eq!(bar.spinner_frame, 0);
    }

    #[test]
    fn tick_marks_dirty_when_git_branch_changes() {
        // With no animated status and no minute change, the only path flipping dirty is the git
        // probe surfacing a new branch. A future `refresh_git_branch` reordering could quietly
        // drop the dirty bit and leave the rendered branch label stale until the next user input.
        let dir = tempfile::tempdir().unwrap();
        let mut bar = test_bar();
        bar.git_cwd = Some(dir.path().to_path_buf());
        bar.git_branch = Some("stale".to_owned());
        bar.last_branch_probe = None;
        assert!(bar.tick());
        assert_eq!(bar.git_branch, None);
    }

    // ── refresh_git_branch ──

    #[test]
    fn refresh_git_branch_without_cwd_is_a_noop() {
        let mut bar = test_bar();
        let probed_at = bar.last_branch_probe;
        assert!(!bar.refresh_git_branch(Instant::now()));
        assert_eq!(bar.last_branch_probe, probed_at);
    }

    #[test]
    fn refresh_git_branch_arms_throttle_when_probe_returns_none() {
        // A non-repo cwd makes `git branch --show-current` return None. The throttle key must
        // still advance, since a regression that only stamps on `Some(branch)` would re-shell
        // out every tick on a non-repo cwd.
        let dir = tempfile::tempdir().unwrap();
        let mut bar = test_bar();
        bar.git_cwd = Some(dir.path().to_path_buf());
        let now = Instant::now();
        bar.refresh_git_branch(now);
        assert_eq!(bar.last_branch_probe, Some(now));
        assert!(
            !bar.refresh_git_branch(now + Duration::from_millis(100)),
            "second call within the interval must short-circuit",
        );
        assert_eq!(
            bar.last_branch_probe,
            Some(now),
            "stamp must not move while the throttle window is open",
        );
    }

    // ── refresh_pull_request ──

    #[test]
    fn refresh_pull_request_when_segment_disabled_is_a_noop() {
        let mut bar = test_bar();
        bar.track_pull_request = false;
        bar.git_cwd = Some(std::path::PathBuf::from("/tmp"));
        assert!(!bar.refresh_pull_request(Instant::now()));
        assert!(bar.last_pr_probe.is_none(), "must skip when not tracked");
    }

    #[test]
    fn refresh_pull_request_without_cwd_is_a_noop() {
        let mut bar = test_bar();
        bar.track_pull_request = true;
        assert!(!bar.refresh_pull_request(Instant::now()));
        assert!(bar.last_pr_probe.is_none(), "must skip without cwd");
    }

    #[test]
    fn refresh_pull_request_arms_throttle_when_probe_returns_none() {
        // Same throttle invariant as the git branch probe. A non-repo cwd (or a cwd where `gh`
        // can't find a PR) returns None, but the stamp must still advance so we don't re-shell
        // every tick.
        let dir = tempfile::tempdir().unwrap();
        let mut bar = test_bar();
        bar.track_pull_request = true;
        bar.git_cwd = Some(dir.path().to_path_buf());
        let now = Instant::now();
        assert!(!bar.refresh_pull_request(now));
        assert_eq!(bar.last_pr_probe, Some(now));
        assert!(
            !bar.refresh_pull_request(now + Duration::from_millis(100)),
            "second call within the interval must short-circuit",
        );
        assert_eq!(bar.last_pr_probe, Some(now));
    }

    // ── should_probe ──

    #[test]
    fn should_probe_runs_immediately_when_never_probed() {
        assert!(should_probe(
            None,
            Instant::now(),
            GIT_BRANCH_REFRESH_INTERVAL,
        ));
    }

    #[test]
    fn should_probe_skips_within_interval_and_runs_after() {
        let earlier = Instant::now();
        assert!(!should_probe(
            Some(earlier),
            earlier + Duration::from_millis(100),
            GIT_BRANCH_REFRESH_INTERVAL,
        ));
        assert!(should_probe(
            Some(earlier),
            earlier + GIT_BRANCH_REFRESH_INTERVAL,
            GIT_BRANCH_REFRESH_INTERVAL,
        ));
    }

    // ── render ──

    fn render_status(bar: &mut StatusBar, width: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
        terminal
            .draw(|frame| {
                bar.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn render_top_row(bar: &mut StatusBar, width: u16) -> String {
        let backend = render_status(bar, width);
        let buf = backend.buffer();
        (0..width)
            .map(|x| {
                buf.cell((x, 0))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn bar_idle(title: Option<&str>, cwd: &str) -> StatusBar {
        let mut bar = StatusBar::new(
            &Theme::default(),
            StatusLineSegment::DEFAULT.to_vec(),
            "Opus 4.7".into(),
            Some(Effort::Xhigh),
            cwd.into(),
            None,
            Some("main".to_owned()),
        );
        bar.set_title(title.map(ToOwned::to_owned));
        bar
    }

    #[test]
    fn render_idle_with_title_shows_model_title_and_cwd() {
        let mut bar = bar_idle(Some("Fix login flow"), "~/projects/demo");
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_idle_without_title_leaves_slot_unused() {
        let mut bar = bar_idle(None, "~/projects/demo");
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_streaming_shows_spinner_and_status_label() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::Streaming);
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_usage_shows_context_and_session_cost() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_usage(Some(UsageSnapshot {
            context_tokens: 124_000,
            context_window: Some(1_000_000),
            estimated_cost_usd: Some(0.4321),
        }));
        let output = render_top_row(&mut bar, 120);
        assert!(
            output.contains("Ctx: 12% (124k/1M)"),
            "usage slot should render before status: {output:?}",
        );
        assert!(output.contains("Sess: $0.4321"));
        assert!(output.contains("Ready"));
    }

    #[test]
    fn render_tool_running_status() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::ToolRunning {
            name: "bash".to_owned(),
        });
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_compacting_shows_spinner_and_status_label() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::Compacting);
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_cancelling_shows_spinner_and_label() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::Cancelling);
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_exit_armed_shows_static_hint_without_spinner() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::ExitArmed {
            until: Instant::now() + std::time::Duration::from_secs(1),
        });
        insta::assert_snapshot!(render_status(&mut bar, 80));
    }

    #[test]
    fn render_configured_segments_control_order() {
        let mut bar = StatusBar::new(
            &Theme::default(),
            vec![
                StatusLineSegment::RunState,
                StatusLineSegment::Model,
                StatusLineSegment::CurrentDir,
            ],
            "Opus 4.7".into(),
            Some(Effort::Xhigh),
            "~/projects/demo".into(),
            None,
            Some("main".to_owned()),
        );
        let output = render_top_row(&mut bar, 120);
        let state_at = output.find("Ready").unwrap();
        let model_at = output.find("Opus 4.7").unwrap();
        let cwd_at = output.find("~/projects/demo").unwrap();
        assert!(state_at < model_at, "run state should lead: {output:?}");
        assert!(model_at < cwd_at, "cwd should follow model: {output:?}");
        assert!(
            !output.contains("main"),
            "git branch was not requested: {output:?}"
        );
    }

    #[test]
    fn render_uses_theme_separator_and_segment_labels() {
        let mut bar = StatusBar::new(
            &Theme::default(),
            vec![
                StatusLineSegment::CurrentDir,
                StatusLineSegment::GitBranch,
                StatusLineSegment::ModelWithEffort,
                StatusLineSegment::RunState,
            ],
            "Opus 4.7".into(),
            Some(Effort::Xhigh),
            "~/projects/demo".into(),
            None,
            Some("feat/status-line".to_owned()),
        );
        let output = render_top_row(&mut bar, 120);
        assert!(output.contains("~/projects/demo │ feat/status-line │ Opus 4.7 (xhigh) │ Ready"));
    }

    #[test]
    fn render_narrow_width_preserves_model_and_run_state() {
        let mut bar = bar_idle(Some("A rather long session title"), "~/projects/demo/long");
        insta::assert_snapshot!(render_status(&mut bar, 40));
    }

    #[test]
    fn render_wide_shows_title_after_status() {
        let mut bar = test_bar();
        bar.set_title(Some("Fix auth bug".to_owned()));
        let output = render_top_row(&mut bar, 120);
        let model_at = output.find("test-model").unwrap();
        let status_at = output.find("Ready").unwrap();
        let title_at = output.find("Fix auth bug").unwrap();
        assert!(model_at < title_at, "title should follow model: {output:?}");
        assert!(
            status_at < title_at,
            "title should follow status: {output:?}"
        );
    }

    #[test]
    fn render_truncates_long_title_with_ellipsis() {
        let mut bar = test_bar();
        let long =
            "A very long session title that keeps going well past any reasonable width limit";
        bar.set_title(Some(long.to_owned()));
        let output = render_top_row(&mut bar, 200);
        assert!(
            output.contains("..."),
            "expected truncated title: {output:?}"
        );
        assert!(
            !output.contains(long),
            "full title should not render: {output:?}"
        );
    }

    #[test]
    fn render_no_title_still_shows_cwd_wide() {
        let mut bar = test_bar();
        let output = render_top_row(&mut bar, 120);
        assert!(output.contains("~/test"));
        assert!(
            !output.contains("..."),
            "no ellipsis without title: {output:?}"
        );
    }

    #[test]
    fn render_empty_cwd_drops_cwd_slot_entirely() {
        let mut bar = StatusBar::new(
            &Theme::default(),
            StatusLineSegment::DEFAULT.to_vec(),
            "test-model".to_owned(),
            None,
            String::new(),
            None,
            None,
        );
        let output = render_top_row(&mut bar, 120);
        assert!(output.contains("test-model"));
        assert!(output.contains("Ready"));
        assert!(
            !output.contains('~'),
            "no tildified path should appear: {output:?}",
        );
    }
}
