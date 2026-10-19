//! Configurable status line component.

mod line;

use std::borrow::Cow;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};

use self::line::{RenderedStatusLine, StatusLine, StatusLineState};
use crate::client::RateLimits;
use crate::config::{Effort, StatusLineSegment};
use crate::permission::Mode;
use crate::protocol::UsageSnapshot;
use crate::tool::tasks::Progress;
use crate::tui::glyphs::SPINNER_FRAMES;
use crate::tui::theme::Theme;
use crate::util::git;

pub(crate) const TICKS_PER_FRAME: usize = 5;

/// A status-bar segment that should be re-emitted as an OSC 8 hyperlink.
#[derive(Debug, Clone)]
pub(crate) struct StatusHyperlink {
    pub(crate) rect: Rect,
    pub(crate) url: String,
    pub(crate) cells: Vec<HyperlinkCell>,
}

#[derive(Debug, Clone)]
pub(crate) struct HyperlinkCell {
    pub(crate) symbol: String,
    pub(crate) style: Style,
}

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
    git_branch: Option<String>,
    pull_request: Option<git::PullRequest>,
    rate_limits: Option<RateLimits>,
    permission_mode: Mode,
    queued_prompts: usize,
    task_progress: Option<Progress>,
    session_id: String,
    theme_name: String,
    status: Status,
    spinner_frame: usize,
    tick_counter: usize,
    /// Hyperlink cells captured during the most recent render.
    pending_hyperlinks: Vec<StatusHyperlink>,
}

/// The segments a [`StatusBar`] shows and the session values it starts with.
pub(crate) struct StatusBarInit {
    pub(crate) segments: Vec<StatusLineSegment>,
    /// Display label, such as `Opus 5.5`.
    pub(crate) model: String,
    pub(crate) effort: Option<Effort>,
    /// Already shortened with `~` by `tildify`.
    pub(crate) cwd: String,
    pub(crate) git_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    ToolRunning { name: String },
    Compacting,
    SigningIn,
    Resuming,
    Cancelling,
    ExitArmed { until: Instant },
}

impl Status {
    /// Returns whether work is in flight, which the bar marks with an animated spinner.
    const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Streaming
                | Self::ToolRunning { .. }
                | Self::Compacting
                | Self::SigningIn
                | Self::Resuming
                | Self::Cancelling,
        )
    }

    fn label(&self) -> Cow<'static, str> {
        match self {
            Self::Idle => "Ready".into(),
            Self::Streaming => "Streaming · Esc to interrupt".into(),
            Self::ToolRunning { name } => format!("Running {name} · Esc to interrupt").into(),
            Self::Compacting => "Compacting · Esc to interrupt".into(),
            Self::SigningIn => "Signing in · Esc to cancel".into(),
            Self::Resuming => "Resuming".into(),
            Self::Cancelling => "Cancelling".into(),
            Self::ExitArmed { .. } => "Press Ctrl+C again to exit".into(),
        }
    }
}

impl StatusBar {
    /// Rows the bar fills: the status line and the border under it.
    pub(crate) const HEIGHT: u16 = 2;

    pub(crate) fn new(theme: &Theme, init: StatusBarInit) -> Self {
        let StatusBarInit {
            segments,
            model,
            effort,
            cwd,
            git_branch,
        } = init;
        let current_time_minute = segments
            .contains(&StatusLineSegment::CurrentTime)
            .then(current_time_minute);
        Self {
            theme: theme.clone(),
            line: StatusLine::new(segments),
            current_time_minute,
            model,
            effort,
            title: None,
            usage: None,
            cwd,
            git_branch,
            pull_request: None,
            rate_limits: None,
            permission_mode: Mode::default(),
            queued_prompts: 0,
            task_progress: None,
            session_id: String::new(),
            theme_name: String::new(),
            status: Status::Idle,
            spinner_frame: 0,
            tick_counter: 0,
            pending_hyperlinks: Vec::new(),
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

    /// Returns `true` when the branch changed and the caller should repaint.
    pub(crate) fn set_git_branch(&mut self, branch: Option<String>) -> bool {
        if branch == self.git_branch {
            return false;
        }
        self.git_branch = branch;
        true
    }

    /// Returns `true` when the pull request changed and the caller should repaint.
    pub(crate) fn set_pull_request(&mut self, pull_request: Option<git::PullRequest>) -> bool {
        if pull_request == self.pull_request {
            return false;
        }
        self.pull_request = pull_request;
        true
    }

    pub(crate) fn set_rate_limits(&mut self, rate_limits: Option<RateLimits>) {
        self.rate_limits = rate_limits;
    }

    pub(crate) fn set_permission_mode(&mut self, mode: Mode) {
        self.permission_mode = mode;
    }

    pub(crate) fn set_queued_prompts(&mut self, count: usize) {
        self.queued_prompts = count;
    }

    pub(crate) fn set_task_progress(&mut self, progress: Option<Progress>) {
        self.task_progress = progress;
    }

    pub(crate) fn set_session_id(&mut self, session_id: &str) {
        session_id.clone_into(&mut self.session_id);
    }

    pub(crate) fn set_theme_name(&mut self, theme_name: &str) {
        theme_name.clone_into(&mut self.theme_name);
    }

    /// Re-skins every subsequent render.
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

    #[cfg(test)]
    pub(crate) fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn rate_limits(&self) -> Option<RateLimits> {
        self.rate_limits
    }

    /// Returns `true` when the time, the usage limits, or the animation changed and the caller
    /// should repaint.
    pub(crate) fn tick(&mut self) -> bool {
        let mut dirty = self.refresh_current_time();
        dirty |= self.expire_rate_limits();
        if self.status.is_busy() {
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

    /// Drops each usage-limit window whose reset has passed, since its spent share no longer
    /// applies until a response reports the new window.
    fn expire_rate_limits(&mut self) -> bool {
        let Some(limits) = &mut self.rate_limits else {
            return false;
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let mut expired = false;
        for window in [&mut limits.five_hour, &mut limits.seven_day] {
            expired |= window.take_if(|window| window.resets_at <= now).is_some();
        }
        expired
    }
}

impl StatusBar {
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(self.theme.border_unfocused())
            .style(self.theme.surface());
        let rendered = self.render_line(area.width);
        self.pending_hyperlinks.clear();
        let mut hyperlink_rects = Vec::with_capacity(rendered.hyperlinks.len());
        for link in &rendered.hyperlinks {
            let link_x = area.x.saturating_add(link.col);
            let width = link.width.min(area.right().saturating_sub(link_x));
            if width == 0 {
                continue;
            }
            hyperlink_rects.push((Rect::new(link_x, area.y, width, 1), link.url.clone()));
        }
        frame.render_widget(Paragraph::new(rendered.line).block(block), area);
        // Snapshot link cells after the paint so emit_status_hyperlinks can replay them.
        let buf = frame.buffer_mut();
        for (rect, url) in hyperlink_rects {
            let mut cells = Vec::with_capacity(usize::from(rect.width));
            for x in rect.x..rect.x.saturating_add(rect.width) {
                if let Some(cell) = buf.cell((x, rect.y)) {
                    cells.push(HyperlinkCell {
                        symbol: cell.symbol().to_owned(),
                        style: cell.style(),
                    });
                }
            }
            self.pending_hyperlinks
                .push(StatusHyperlink { rect, url, cells });
        }
    }

    /// Drains and returns the hyperlinks captured during the most recent render.
    pub(crate) fn take_pending_hyperlinks(&mut self) -> Vec<StatusHyperlink> {
        std::mem::take(&mut self.pending_hyperlinks)
    }
}

// ── Render Helpers ──

impl StatusBar {
    fn status_span(&self) -> Span<'static> {
        let label = self.status.label();
        if self.status.is_busy() {
            let spinner = SPINNER_FRAMES[self.spinner_frame];
            return Span::styled(format!("{spinner} {label}"), self.theme.info());
        }
        let style = if matches!(self.status, Status::ExitArmed { .. }) {
            self.theme.warning()
        } else {
            self.theme.success()
        };
        Span::styled(label, style)
    }

    fn render_line(&self, width: u16) -> RenderedStatusLine {
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
                rate_limits: self.rate_limits,
                permission_mode: self.permission_mode,
                status_span: self.status_span(),
                queued_prompts: self.queued_prompts,
                task_progress: self.task_progress,
                session_id: &self.session_id,
                theme_name: &self.theme_name,
            },
            width,
        )
    }
}

fn current_time_minute() -> u16 {
    let now = time::OffsetDateTime::now_utc().to_offset(crate::util::time::local_offset());
    u16::from(now.hour()) * 60 + u16::from(now.minute())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::{StatusBarInit, StatusLineSegment};

    /// A `test-model` session in `~/test` with no effort or branch, showing `segments`.
    pub(crate) fn status_bar_init(segments: Vec<StatusLineSegment>) -> StatusBarInit {
        StatusBarInit {
            segments,
            model: "test-model".to_owned(),
            effort: None,
            cwd: "~/test".to_owned(),
            git_branch: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::testing::status_bar_init;
    use super::*;
    use crate::client::RateLimitWindow;

    /// 2100-01-01, a reset no test run reaches.
    const FUTURE_RESET: i64 = 4_102_444_800;

    fn test_bar() -> StatusBar {
        StatusBar::new(
            &Theme::default(),
            StatusBarInit {
                effort: Some(Effort::High),
                git_branch: Some("main".to_owned()),
                ..status_bar_init(StatusLineSegment::DEFAULT.to_vec())
            },
        )
    }

    fn pr_bar() -> StatusBar {
        StatusBar::new(
            &Theme::default(),
            status_bar_init(vec![
                StatusLineSegment::PullRequest,
                StatusLineSegment::RunState,
            ]),
        )
    }

    fn pr_state(number: u64) -> git::PullRequest {
        git::PullRequest {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
            checks: None,
            review: None,
        }
    }

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

    mod set_title {
        use super::*;

        #[test]
        fn stores_non_empty_title() {
            let mut bar = test_bar();
            bar.set_title(Some("Fix auth bug".to_owned()));
            assert_eq!(bar.title.as_deref(), Some("Fix auth bug"));
        }

        #[test]
        fn clears_a_stored_title() {
            let mut bar = test_bar();
            bar.set_title(Some("something".to_owned()));
            bar.set_title(None);
            assert!(bar.title.is_none());
        }

        #[test]
        fn drops_whitespace_only() {
            let mut bar = test_bar();
            bar.set_title(Some("   \n".to_owned()));
            assert!(bar.title.is_none());
        }
    }

    mod set_model {
        use super::*;

        #[test]
        fn replaces_displayed_model_label() {
            let mut bar = test_bar();
            bar.set_model("Opus 5.5".to_owned());
            assert_eq!(bar.model(), "Opus 5.5");
            let output = render_top_row(&mut bar, 80);
            assert!(
                output.contains("Opus 5.5"),
                "new label must reach the rendered bar: {output:?}",
            );
            assert!(
                !output.contains("test-model"),
                "old label must not survive: {output:?}",
            );
        }
    }

    mod set_git_branch {
        use super::*;

        #[test]
        fn reports_only_real_changes() {
            let mut bar = test_bar();
            assert!(!bar.set_git_branch(Some("main".to_owned())));
            assert!(bar.set_git_branch(Some("feat/x".to_owned())));
            assert_eq!(bar.git_branch(), Some("feat/x"));
            assert!(bar.set_git_branch(None));
            assert_eq!(bar.git_branch(), None);
        }
    }

    mod set_pull_request {
        use super::*;

        #[test]
        fn reports_only_real_changes() {
            let mut bar = pr_bar();
            assert!(!bar.set_pull_request(None));
            assert!(bar.set_pull_request(Some(pr_state(1))));
            assert!(!bar.set_pull_request(Some(pr_state(1))));
            assert!(bar.set_pull_request(Some(pr_state(2))));
            assert_eq!(bar.pull_request, Some(pr_state(2)));
        }
    }

    mod set_status {
        use super::*;

        #[test]
        fn resets_spinner_on_transition() {
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
        fn same_status_preserves_spinner() {
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
        fn to_idle_resets_spinner() {
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
    }

    mod tick {
        use super::*;

        #[test]
        fn idle_needs_no_repaint() {
            let mut bar = test_bar();
            assert!(!bar.tick());
            assert_eq!(bar.spinner_frame, 0);
            assert_eq!(bar.tick_counter, 0);
        }

        #[test]
        fn without_current_time_segment_skips_minute_refresh() {
            let mut bar = StatusBar::new(
                &Theme::default(),
                status_bar_init(vec![StatusLineSegment::Model, StatusLineSegment::RunState]),
            );

            assert_eq!(bar.current_time_minute, None);
            assert!(!bar.tick());
            assert_eq!(bar.current_time_minute, None);
        }

        #[test]
        fn idle_current_time_marks_dirty_on_minute_change() {
            let mut bar = StatusBar::new(
                &Theme::default(),
                status_bar_init(vec![StatusLineSegment::CurrentTime]),
            );
            let current = current_time_minute();
            bar.current_time_minute = Some((current + 1) % 1440);

            assert!(bar.tick());
            assert_eq!(bar.current_time_minute, Some(current));
            assert!(!bar.tick());
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn drops_a_usage_limit_window_once_it_resets() {
            let mut bar = test_bar();
            let window = |resets_at| {
                Some(RateLimitWindow {
                    utilization: 0.97,
                    resets_at,
                })
            };
            bar.set_rate_limits(Some(RateLimits {
                five_hour: window(0),
                seven_day: window(FUTURE_RESET),
            }));

            assert!(bar.tick());
            assert_eq!(
                bar.rate_limits(),
                Some(RateLimits {
                    five_hour: None,
                    seven_day: window(FUTURE_RESET),
                }),
            );
            assert!(!bar.tick(), "the open window stays");
        }

        #[test]
        fn streaming_before_threshold_does_not_advance_spinner_frame() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);

            for _ in 0..TICKS_PER_FRAME - 1 {
                assert!(!bar.tick());
            }
            assert_eq!(bar.tick_counter, TICKS_PER_FRAME - 1);
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn streaming_advances_frame_at_threshold() {
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
        fn wraps_spinner_frames() {
            let mut bar = test_bar();
            bar.set_status(Status::ToolRunning {
                name: "bash".to_owned(),
            });

            for _ in 0..SPINNER_FRAMES.len() * TICKS_PER_FRAME {
                bar.tick();
            }
            assert_eq!(bar.spinner_frame, 0);
        }
    }

    mod render {
        use super::*;

        fn bar_idle(title: Option<&str>, cwd: &str) -> StatusBar {
            let mut bar = StatusBar::new(
                &Theme::default(),
                StatusBarInit {
                    segments: StatusLineSegment::DEFAULT.to_vec(),
                    model: "Opus 5.5".to_owned(),
                    effort: Some(Effort::Xhigh),
                    cwd: cwd.into(),
                    git_branch: Some("main".to_owned()),
                },
            );
            bar.set_title(title.map(ToOwned::to_owned));
            bar
        }

        /// Shows `segment` between the directory and the model.
        fn segment_bar(segment: StatusLineSegment) -> StatusBar {
            StatusBar::new(
                &Theme::default(),
                StatusBarInit {
                    segments: vec![
                        StatusLineSegment::CurrentDir,
                        segment,
                        StatusLineSegment::ModelWithEffort,
                        StatusLineSegment::RunState,
                    ],
                    model: "Opus 5.5".to_owned(),
                    effort: Some(Effort::Xhigh),
                    cwd: "~/projects/demo".to_owned(),
                    git_branch: None,
                },
            )
        }

        /// Renders the bar 100 columns wide, then 40.
        fn render_wide_and_narrow(bar: &mut StatusBar) -> String {
            [100, 40]
                .map(|width| render_status(bar, width).to_string())
                .join("\n")
        }

        fn rate_limits() -> RateLimits {
            let window = |utilization| {
                Some(RateLimitWindow {
                    utilization,
                    resets_at: FUTURE_RESET,
                })
            };
            RateLimits {
                five_hour: window(0.42),
                seven_day: window(0.13),
            }
        }

        #[test]
        fn idle_with_title_shows_model_title_and_cwd() {
            let mut bar = bar_idle(Some("Fix login flow"), "~/projects/demo");
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn idle_without_title_leaves_slot_unused() {
            let mut bar = bar_idle(None, "~/projects/demo");
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn streaming_shows_spinner_and_status_label() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::Streaming);
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn tool_running_status() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::ToolRunning {
                name: "bash".to_owned(),
            });
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn compacting_shows_spinner_and_status_label() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::Compacting);
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn signing_in_shows_spinner_and_cancel_hint() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::SigningIn);
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn resuming_shows_spinner_and_label() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::Resuming);
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn cancelling_shows_spinner_and_label() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::Cancelling);
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn exit_armed_shows_static_hint_without_spinner() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_status(Status::ExitArmed {
                until: Instant::now() + std::time::Duration::from_secs(1),
            });
            insta::assert_snapshot!(render_status(&mut bar, 80));
        }

        #[test]
        fn pull_request_checks_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::PullRequestChecks);
            bar.set_pull_request(Some(git::PullRequest {
                checks: Some(git::Checks::Failing(2)),
                ..pr_state(86)
            }));
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn pull_request_review_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::PullRequestReview);
            bar.set_pull_request(Some(git::PullRequest {
                review: Some(git::ReviewDecision::ChangesRequested),
                ..pr_state(86)
            }));
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn usage_shows_context_and_session_cost() {
            let mut bar = bar_idle(None, "~/projects/demo");
            bar.set_usage(Some(UsageSnapshot {
                context_tokens: Some(124_000),
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
        fn five_hour_limit_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::FiveHourLimit);
            bar.set_rate_limits(Some(rate_limits()));
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn weekly_limit_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::WeeklyLimit);
            bar.set_rate_limits(Some(rate_limits()));
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn queued_prompts_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::QueuedPrompts);
            bar.set_queued_prompts(2);
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn task_progress_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::TaskProgress);
            bar.set_task_progress(Some(Progress {
                completed: 2,
                total: 5,
            }));
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn session_id_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::SessionId);
            bar.set_session_id("3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b");
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn theme_at_wide_and_narrow_widths() {
            let mut bar = segment_bar(StatusLineSegment::Theme);
            bar.set_theme_name("mocha");
            insta::assert_snapshot!(render_wide_and_narrow(&mut bar));
        }

        #[test]
        fn configured_segments_control_order() {
            let mut bar = StatusBar::new(
                &Theme::default(),
                StatusBarInit {
                    segments: vec![
                        StatusLineSegment::RunState,
                        StatusLineSegment::Model,
                        StatusLineSegment::CurrentDir,
                    ],
                    model: "Opus 5.5".to_owned(),
                    effort: Some(Effort::Xhigh),
                    cwd: "~/projects/demo".to_owned(),
                    git_branch: Some("main".to_owned()),
                },
            );
            let output = render_top_row(&mut bar, 120);
            let state_at = output.find("Ready").unwrap();
            let model_at = output.find("Opus 5.5").unwrap();
            let cwd_at = output.find("~/projects/demo").unwrap();
            assert!(state_at < model_at, "run state should lead: {output:?}");
            assert!(model_at < cwd_at, "cwd should follow model: {output:?}");
            assert!(
                !output.contains("main"),
                "git branch was not requested: {output:?}"
            );
        }

        #[test]
        fn uses_theme_separator_and_segment_labels() {
            let mut bar = StatusBar::new(
                &Theme::default(),
                StatusBarInit {
                    segments: vec![
                        StatusLineSegment::CurrentDir,
                        StatusLineSegment::GitBranch,
                        StatusLineSegment::ModelWithEffort,
                        StatusLineSegment::RunState,
                    ],
                    model: "Opus 5.5".to_owned(),
                    effort: Some(Effort::Xhigh),
                    cwd: "~/projects/demo".to_owned(),
                    git_branch: Some("feat/status-line".to_owned()),
                },
            );
            let output = render_top_row(&mut bar, 120);
            assert!(
                output.contains("~/projects/demo │ feat/status-line │ Opus 5.5 (xhigh) │ Ready")
            );
        }

        #[test]
        fn narrow_width_preserves_model_and_run_state() {
            let mut bar = bar_idle(Some("A rather long session title"), "~/projects/demo/long");
            insta::assert_snapshot!(render_status(&mut bar, 40));
        }

        #[test]
        fn wide_shows_title_after_status() {
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
        fn truncates_long_title_with_ellipsis() {
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
        fn no_title_still_shows_cwd_wide() {
            let mut bar = test_bar();
            let output = render_top_row(&mut bar, 120);
            assert!(output.contains("~/test"));
            assert!(
                !output.contains("..."),
                "no ellipsis without title: {output:?}"
            );
        }

        #[test]
        fn empty_cwd_drops_cwd_slot_entirely() {
            let mut bar = StatusBar::new(
                &Theme::default(),
                StatusBarInit {
                    cwd: String::new(),
                    ..status_bar_init(StatusLineSegment::DEFAULT.to_vec())
                },
            );
            let output = render_top_row(&mut bar, 120);
            assert!(output.contains("test-model"));
            assert!(output.contains("Ready"));
            assert!(
                !output.contains('~'),
                "no tildified path should appear: {output:?}",
            );
        }

        #[test]
        fn records_pull_request_hyperlink_rect_for_post_flush_emission() {
            let mut bar = pr_bar();
            bar.pull_request = Some(pr_state(86));
            render_status(&mut bar, 40);
            let links = bar.take_pending_hyperlinks();
            assert_eq!(
                links.len(),
                1,
                "PR segment reports exactly one link: {links:?}",
            );
            let link = &links[0];
            assert_eq!(link.url, "https://github.com/o/r/pull/86");
            assert_eq!(link.rect.x, 2);
            assert_eq!(link.rect.width, 3);
            let visible: String = link.cells.iter().map(|c| c.symbol.as_str()).collect();
            assert_eq!(visible, "#86");
        }

        #[test]
        fn records_no_hyperlinks_when_pull_request_absent() {
            let mut bar = test_bar();
            render_status(&mut bar, 40);
            assert!(bar.take_pending_hyperlinks().is_empty());
        }

        #[test]
        fn drops_hyperlink_when_segment_clipped_to_zero_width() {
            let mut bar = pr_bar();
            bar.pull_request = Some(pr_state(86));
            render_status(&mut bar, 10);
            assert!(bar.take_pending_hyperlinks().is_empty());
        }
    }

    mod take_pending_hyperlinks {
        use super::*;

        #[test]
        fn drains_after_first_call() {
            let mut bar = pr_bar();
            bar.pull_request = Some(pr_state(86));
            render_status(&mut bar, 40);
            assert_eq!(bar.take_pending_hyperlinks().len(), 1);
            assert!(
                bar.take_pending_hyperlinks().is_empty(),
                "second take leaves nothing behind",
            );
        }
    }
}
