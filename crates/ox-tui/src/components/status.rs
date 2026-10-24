//! Configurable status line component.

mod line;

use std::borrow::Cow;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};
use time::{OffsetDateTime, Time};

use ox_core::client::RateLimits;
use ox_core::config::StatusLineSegment;
use ox_core::model::Effort;
use ox_core::permission::Mode;
use ox_core::protocol::UsageSnapshot;
use ox_core::tool::tasks::Progress;
use ox_core::util::git;
use ox_core::util::time::now_local;

use self::line::{RenderedStatusLine, StatusLine, StatusLineState};
use crate::glyphs::SPINNER_FRAMES;
use crate::theme::Theme;

pub(crate) const SPINNER_FRAME_INTERVAL: Duration = Duration::from_millis(80);

// ── Hyperlinks ──

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

// ── Status ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    ToolRunning { name: String },
    Compacting,
    SigningIn,
    SigningOut,
    Clearing,
    Resuming,
    Archiving,
    Cancelling,
    ExitArmed { until: Instant },
}

impl Status {
    /// Returns whether no work is in flight, so a submit starts a turn.
    pub(crate) const fn is_idle(&self) -> bool {
        !self.is_busy()
    }

    /// Returns whether work is in flight, which the bar marks with an animated spinner.
    pub(crate) const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Streaming
                | Self::ToolRunning { .. }
                | Self::Compacting
                | Self::SigningIn
                | Self::SigningOut
                | Self::Clearing
                | Self::Resuming
                | Self::Archiving
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
            Self::SigningOut => "Signing out".into(),
            Self::Clearing => "Clearing".into(),
            Self::Resuming => "Resuming".into(),
            Self::Archiving => "Archiving".into(),
            Self::Cancelling => "Cancelling".into(),
            Self::ExitArmed { .. } => "Press Ctrl+C again to exit".into(),
        }
    }
}

// ── StatusBar ──

/// Status bar at the top of the TUI.
pub(crate) struct StatusBar {
    line: StatusLine,
    /// Minute of the local day the current-time segment shows, or `None` when the line has no such
    /// segment.
    current_minute: Option<u16>,
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
    next_spinner_frame: Instant,
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

impl StatusBar {
    /// Rows the bar fills: the status line and the border under it.
    pub(crate) const HEIGHT: u16 = 2;

    pub(crate) fn new(init: StatusBarInit) -> Self {
        let StatusBarInit {
            segments,
            model,
            effort,
            cwd,
            git_branch,
        } = init;
        let current_minute = segments
            .contains(&StatusLineSegment::CurrentTime)
            .then(local_minute);
        Self {
            line: StatusLine::new(segments),
            current_minute,
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
            next_spinner_frame: crate::now(),
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

    pub(crate) fn set_status(&mut self, status: Status) {
        if status != self.status {
            self.spinner_frame = 0;
            self.next_spinner_frame = crate::now() + SPINNER_FRAME_INTERVAL;
        }
        self.status = status;
    }

    /// The spinner frame the bar shows while work is in flight.
    pub(crate) fn spinner(&self) -> char {
        SPINNER_FRAMES[self.spinner_frame]
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

    /// Returns `true` when the time, the usage limits, or the spinner changed and the caller should
    /// repaint.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let mut dirty = self.refresh_current_time();
        dirty |= self.expire_rate_limits();
        if self.status.is_busy() && now >= self.next_spinner_frame {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            self.next_spinner_frame = now + SPINNER_FRAME_INTERVAL;
            dirty = true;
        }
        dirty
    }

    /// When [`Self::tick`] next changes the bar, on the event loop's clock: the next spinner frame
    /// while busy, the next minute the current-time segment shows, or the next usage-limit reset.
    pub(crate) fn next_tick(&self, now: Instant) -> Option<Instant> {
        let spinner = self.status.is_busy().then_some(self.next_spinner_frame);
        let minute = self
            .current_minute
            .map(|_| now + until_next_minute(now_local().time()));
        let unix_now = OffsetDateTime::now_utc().unix_timestamp();
        let reset = self.rate_limits.iter().flat_map(|limits| {
            [limits.five_hour, limits.seven_day]
                .into_iter()
                .flatten()
                .map(|window| {
                    let wait = u64::try_from(window.resets_at - unix_now).unwrap_or(0);
                    now + Duration::from_secs(wait)
                })
        });
        spinner.into_iter().chain(minute).chain(reset).min()
    }

    fn refresh_current_time(&mut self) -> bool {
        let Some(shown) = self.current_minute else {
            return false;
        };
        let current = local_minute();
        if current == shown {
            return false;
        }
        self.current_minute = Some(current);
        true
    }

    /// Drops each usage-limit window whose reset has passed, since its spent share no longer
    /// applies until a response reports the new window.
    fn expire_rate_limits(&mut self) -> bool {
        let Some(limits) = &mut self.rate_limits else {
            return false;
        };
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let mut expired = false;
        for window in [&mut limits.five_hour, &mut limits.seven_day] {
            expired |= window.take_if(|window| window.resets_at <= now).is_some();
        }
        expired
    }
}

impl StatusBar {
    pub(crate) fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme.border_unfocused())
            .style(theme.surface());
        let rendered = self.render_line(area.width, theme);
        frame.render_widget(Paragraph::new(rendered.line).block(block), area);
        // Snapshot link cells after the paint so emit_status_hyperlinks can replay them.
        self.pending_hyperlinks.clear();
        let buf = frame.buffer_mut();
        for link in rendered.hyperlinks {
            let rect = Rect::new(area.x.saturating_add(link.col), area.y, link.width, 1);
            let mut cells = Vec::with_capacity(usize::from(rect.width));
            for x in rect.x..rect.x.saturating_add(rect.width) {
                if let Some(cell) = buf.cell((x, rect.y)) {
                    cells.push(HyperlinkCell {
                        symbol: cell.symbol().to_owned(),
                        style: cell.style(),
                    });
                }
            }
            self.pending_hyperlinks.push(StatusHyperlink {
                rect,
                url: link.url,
                cells,
            });
        }
    }

    /// Drains and returns the hyperlinks captured during the most recent render.
    pub(crate) fn take_pending_hyperlinks(&mut self) -> Vec<StatusHyperlink> {
        std::mem::take(&mut self.pending_hyperlinks)
    }
}

// ── Render Helpers ──

impl StatusBar {
    fn status_span(&self, theme: &Theme) -> Span<'static> {
        let label = self.status.label();
        if self.status.is_busy() {
            let spinner = SPINNER_FRAMES[self.spinner_frame];
            return Span::styled(format!("{spinner} {label}"), theme.info());
        }
        let style = if matches!(self.status, Status::ExitArmed { .. }) {
            theme.warning()
        } else {
            theme.muted()
        };
        Span::styled(label, style)
    }

    fn render_line(&self, width: u16, theme: &Theme) -> RenderedStatusLine {
        self.line.render(
            theme,
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
                status_span: self.status_span(theme),
                queued_prompts: self.queued_prompts,
                task_progress: self.task_progress,
                session_id: &self.session_id,
                theme_name: &self.theme_name,
                current_minute: self.current_minute,
            },
            width,
        )
    }
}

fn local_minute() -> u16 {
    let now = now_local();
    u16::from(now.hour()) * 60 + u16::from(now.minute())
}

fn until_next_minute(time: Time) -> Duration {
    let into_minute = Duration::new(u64::from(time.second()), time.nanosecond());
    Duration::from_mins(1).saturating_sub(into_minute)
}

// ── Testing ──

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
    use time::macros::time;

    use ox_core::client::RateLimitWindow;

    use super::testing::status_bar_init;
    use super::*;

    /// 2100-01-01, a reset no test run reaches.
    const FUTURE_RESET: i64 = 4_102_444_800;

    fn test_bar() -> StatusBar {
        StatusBar::new(StatusBarInit {
            effort: Some(Effort::High),
            git_branch: Some("main".to_owned()),
            ..status_bar_init(StatusLineSegment::DEFAULT.to_vec())
        })
    }

    fn pr_bar() -> StatusBar {
        StatusBar::new(status_bar_init(vec![
            StatusLineSegment::PullRequest,
            StatusLineSegment::RunState,
        ]))
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
                bar.render(frame, frame.area(), &Theme::default());
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

    /// Ticks the busy bar at each of its next `frames` spinner frames.
    fn advance_spinner(bar: &mut StatusBar, frames: usize) {
        for _ in 0..frames {
            assert!(bar.tick(bar.next_spinner_frame));
        }
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
            advance_spinner(&mut bar, 3);

            bar.set_status(Status::ToolRunning {
                name: "bash".to_owned(),
            });

            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn same_status_preserves_spinner() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);
            advance_spinner(&mut bar, 2);

            bar.set_status(Status::Streaming);

            assert_eq!(bar.spinner_frame, 2);
        }

        #[test]
        fn to_idle_resets_spinner() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);
            advance_spinner(&mut bar, 2);

            bar.set_status(Status::Idle);

            assert_eq!(bar.spinner_frame, 0);
            assert!(!bar.tick(bar.next_spinner_frame));
        }
    }

    mod spinner {
        use super::*;

        #[test]
        fn follows_the_animation_frame() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);

            advance_spinner(&mut bar, 3);

            assert_eq!(bar.spinner(), SPINNER_FRAMES[3]);
        }
    }

    mod tick {
        use super::*;

        #[test]
        fn idle_needs_no_repaint() {
            let mut bar = test_bar();

            assert!(!bar.tick(bar.next_spinner_frame));
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn without_current_time_segment_skips_minute_refresh() {
            let mut bar = StatusBar::new(status_bar_init(vec![
                StatusLineSegment::Model,
                StatusLineSegment::RunState,
            ]));

            assert_eq!(bar.current_minute, None);
            assert!(!bar.tick(crate::now()));
            assert_eq!(bar.current_minute, None);
        }

        #[test]
        fn idle_current_time_marks_dirty_on_minute_change() {
            let mut bar = StatusBar::new(status_bar_init(vec![StatusLineSegment::CurrentTime]));
            let current = local_minute();
            bar.current_minute = Some((current + 1) % 1440);

            assert!(bar.tick(crate::now()));
            assert_eq!(bar.current_minute, Some(current));
            assert!(!bar.tick(crate::now()));
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

            assert!(bar.tick(crate::now()));
            assert_eq!(
                bar.rate_limits(),
                Some(RateLimits {
                    five_hour: None,
                    seven_day: window(FUTURE_RESET),
                }),
            );
            assert!(!bar.tick(crate::now()), "the open window stays");
        }

        #[test]
        fn streaming_keeps_the_frame_until_the_frame_interval_passes() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);
            let before = bar.next_spinner_frame.checked_sub(Duration::from_millis(1));

            assert!(!bar.tick(before.unwrap()));
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn streaming_advances_the_frame_once_the_interval_passes() {
            let mut bar = test_bar();
            bar.set_status(Status::Streaming);
            let due = bar.next_spinner_frame;

            assert!(bar.tick(due));
            assert_eq!(bar.spinner_frame, 1);
            assert_eq!(bar.next_spinner_frame, due + SPINNER_FRAME_INTERVAL);
        }

        #[test]
        fn wraps_spinner_frames() {
            let mut bar = test_bar();
            bar.set_status(Status::ToolRunning {
                name: "bash".to_owned(),
            });

            advance_spinner(&mut bar, SPINNER_FRAMES.len());

            assert_eq!(bar.spinner_frame, 0);
        }
    }

    mod next_tick {
        use super::*;

        fn bar_with(segments: Vec<StatusLineSegment>) -> StatusBar {
            StatusBar::new(status_bar_init(segments))
        }

        #[test]
        fn is_the_next_spinner_frame_while_busy() {
            let mut bar = bar_with(vec![StatusLineSegment::RunState]);
            bar.set_status(Status::Streaming);

            assert_eq!(bar.next_tick(crate::now()), Some(bar.next_spinner_frame));
        }

        #[test]
        fn is_the_next_minute_for_the_current_time_segment() {
            let bar = bar_with(vec![StatusLineSegment::CurrentTime]);
            let now = Instant::now();

            let due = bar.next_tick(now).unwrap();

            assert!(due > now && due <= now + Duration::from_mins(1), "{due:?}");
        }

        #[test]
        fn is_the_earliest_usage_limit_reset() {
            let mut bar = bar_with(vec![StatusLineSegment::Model]);
            let unix_now = OffsetDateTime::now_utc().unix_timestamp();
            let window = |resets_at| {
                Some(RateLimitWindow {
                    utilization: 0.5,
                    resets_at,
                })
            };
            bar.set_rate_limits(Some(RateLimits {
                five_hour: window(unix_now + 120),
                seven_day: window(FUTURE_RESET),
            }));
            let now = Instant::now();

            let due = bar.next_tick(now).unwrap();

            let wait = due - now;
            assert!(
                (Duration::from_secs(119)..=Duration::from_secs(120)).contains(&wait),
                "{wait:?}"
            );
        }

        #[test]
        fn an_expired_usage_limit_precedes_the_spinner_and_minute() {
            let mut bar = bar_with(vec![
                StatusLineSegment::RunState,
                StatusLineSegment::CurrentTime,
            ]);
            bar.set_status(Status::Streaming);
            bar.set_rate_limits(Some(RateLimits {
                five_hour: Some(RateLimitWindow {
                    utilization: 0.5,
                    resets_at: 0,
                }),
                seven_day: None,
            }));
            let now = bar
                .next_spinner_frame
                .checked_sub(SPINNER_FRAME_INTERVAL)
                .unwrap();

            assert_eq!(bar.next_tick(now), Some(now));
        }

        #[test]
        fn never_changes_an_idle_bar_without_a_clock_or_usage_limits() {
            let bar = bar_with(vec![StatusLineSegment::Model]);

            assert_eq!(bar.next_tick(Instant::now()), None);
        }
    }

    mod render {
        use super::*;

        fn bar_idle(title: Option<&str>, cwd: &str) -> StatusBar {
            let mut bar = StatusBar::new(StatusBarInit {
                segments: StatusLineSegment::DEFAULT.to_vec(),
                model: "Opus 5.5".to_owned(),
                effort: Some(Effort::Xhigh),
                cwd: cwd.into(),
                git_branch: Some("main".to_owned()),
            });
            bar.set_title(title.map(ToOwned::to_owned));
            bar
        }

        /// Shows `segment` between the directory and the model.
        fn segment_bar(segment: StatusLineSegment) -> StatusBar {
            StatusBar::new(StatusBarInit {
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
            })
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
        fn puts_the_run_state_after_the_model() {
            let mut bar = bar_idle(None, "~/projects/demo");

            bar.set_status(Status::Streaming);

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
            assert!(output.contains("Cost: $0.43"));
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
            let mut bar = StatusBar::new(StatusBarInit {
                segments: vec![
                    StatusLineSegment::RunState,
                    StatusLineSegment::Model,
                    StatusLineSegment::CurrentDir,
                ],
                model: "Opus 5.5".to_owned(),
                effort: Some(Effort::Xhigh),
                cwd: "~/projects/demo".to_owned(),
                git_branch: Some("main".to_owned()),
            });
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
            let mut bar = StatusBar::new(StatusBarInit {
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
            });
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
            let mut bar = StatusBar::new(StatusBarInit {
                cwd: String::new(),
                ..status_bar_init(StatusLineSegment::DEFAULT.to_vec())
            });
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
        fn drops_the_hyperlink_when_the_bar_is_too_narrow_for_its_segment() {
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

    mod status_span {
        use super::*;

        #[test]
        fn ready_reads_in_the_muted_style() {
            let bar = test_bar();

            assert_eq!(
                bar.status_span(&Theme::default()),
                Span::styled("Ready", Theme::default().muted())
            );
        }

        #[test]
        fn busy_labels_follow_the_spinner_in_the_info_style() {
            let cases = [
                (Status::Streaming, "Streaming · Esc to interrupt"),
                (
                    Status::ToolRunning {
                        name: "bash".to_owned(),
                    },
                    "Running bash · Esc to interrupt",
                ),
                (Status::Compacting, "Compacting · Esc to interrupt"),
                (Status::SigningIn, "Signing in · Esc to cancel"),
                (Status::SigningOut, "Signing out"),
                (Status::Clearing, "Clearing"),
                (Status::Resuming, "Resuming"),
                (Status::Archiving, "Archiving"),
                (Status::Cancelling, "Cancelling"),
            ];
            for (status, label) in cases {
                let mut bar = test_bar();

                bar.set_status(status);

                assert_eq!(
                    bar.status_span(&Theme::default()),
                    Span::styled(
                        format!("{} {label}", SPINNER_FRAMES[0]),
                        Theme::default().info()
                    )
                );
            }
        }

        #[test]
        fn armed_exit_hint_reads_as_a_warning() {
            let mut bar = test_bar();
            bar.set_status(Status::ExitArmed {
                until: Instant::now(),
            });

            assert_eq!(
                bar.status_span(&Theme::default()),
                Span::styled("Press Ctrl+C again to exit", Theme::default().warning())
            );
        }
    }

    mod until_next_minute {
        use super::*;

        #[test]
        fn is_the_rest_of_the_current_minute() {
            assert_eq!(
                until_next_minute(time!(09:05:59.250)),
                Duration::from_millis(750)
            );
        }

        #[test]
        fn is_a_whole_minute_on_the_minute() {
            assert_eq!(until_next_minute(time!(09:05:00)), Duration::from_mins(1));
        }
    }
}
