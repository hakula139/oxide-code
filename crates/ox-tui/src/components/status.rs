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

use ox_core::config::StatusLineSegment;
use ox_core::tool::tasks::Progress;
use ox_core::util::git;
use ox_core::util::time::now_local;

use self::line::{RenderedStatusLine, StatusLine, StatusLineState};
use crate::glyphs::SPINNER_FRAMES;
use crate::session_info::LiveSessionInfo;
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

// ── Run State ──

/// The run-state segment's text and how it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunLabel {
    pub(crate) text: Cow<'static, str>,
    pub(crate) tone: RunTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunTone {
    /// Nothing runs, in the muted style.
    Ready,
    /// Work is in flight, behind an animated spinner in the info style.
    Busy,
    /// The user must act soon, in the warning style.
    Alert,
}

impl RunLabel {
    pub(crate) const READY: Self = Self {
        text: Cow::Borrowed("Ready"),
        tone: RunTone::Ready,
    };
}

// ── StatusBar ──

/// The App's state one status-bar render shows.
pub(crate) struct StatusView<'a> {
    pub(crate) session: &'a LiveSessionInfo,
    pub(crate) run: RunLabel,
    pub(crate) queued_prompts: usize,
    /// `None` while the session keeps no task list.
    pub(crate) task_progress: Option<Progress>,
}

/// Status bar at the top of the TUI. It renders a [`StatusView`] of the App's state and keeps only
/// what it owns: the spinner, the clock, the git probe results, and the links it last drew.
pub(crate) struct StatusBar {
    line: StatusLine,
    /// Minute of the local day the current-time segment shows, or `None` when the line has none.
    current_minute: Option<u16>,
    git_branch: Option<String>,
    pull_request: Option<git::PullRequest>,
    /// The run state of the last render, which drives the spinner.
    run: RunLabel,
    spinner_frame: usize,
    next_spinner_frame: Instant,
    /// Hyperlink cells captured during the most recent render.
    pending_hyperlinks: Vec<StatusHyperlink>,
}

impl StatusBar {
    /// Rows the bar fills: the status line and the border under it.
    pub(crate) const HEIGHT: u16 = 2;

    /// Shows `segments`, with `git_branch` as the branch until the first git probe reports.
    pub(crate) fn new(segments: Vec<StatusLineSegment>, git_branch: Option<String>) -> Self {
        let current_minute = segments
            .contains(&StatusLineSegment::CurrentTime)
            .then(local_minute);
        Self {
            line: StatusLine::new(segments),
            current_minute,
            git_branch,
            pull_request: None,
            run: RunLabel::READY,
            spinner_frame: 0,
            next_spinner_frame: crate::now(),
            pending_hyperlinks: Vec::new(),
        }
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

    /// The spinner frame the bar shows while work is in flight.
    pub(crate) fn spinner(&self) -> char {
        SPINNER_FRAMES[self.spinner_frame]
    }

    #[cfg(test)]
    pub(crate) fn git_branch(&self) -> Option<&str> {
        self.git_branch.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn pull_request(&self) -> Option<&git::PullRequest> {
        self.pull_request.as_ref()
    }

    /// Returns `true` when the time or the spinner changed and the caller should repaint.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let mut dirty = self.refresh_current_time(local_minute());
        if self.run.tone == RunTone::Busy && now >= self.next_spinner_frame {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            self.next_spinner_frame = now + SPINNER_FRAME_INTERVAL;
            dirty = true;
        }
        dirty
    }

    /// When [`Self::tick`] next changes the bar, on the event loop's clock: the next spinner frame
    /// while busy, or the next minute the current-time segment shows.
    pub(crate) fn next_tick(&self, now: Instant) -> Option<Instant> {
        self.next_tick_at(now, now_local())
    }

    fn next_tick_at(&self, now: Instant, wall_now: OffsetDateTime) -> Option<Instant> {
        let spinner = (self.run.tone == RunTone::Busy).then_some(self.next_spinner_frame);
        let minute = self
            .current_minute
            .map(|_| now + until_next_minute(wall_now.time()));
        spinner.into_iter().chain(minute).min()
    }

    fn refresh_current_time(&mut self, current: u16) -> bool {
        let Some(shown) = self.current_minute else {
            return false;
        };
        if current == shown {
            return false;
        }
        self.current_minute = Some(current);
        true
    }
}

impl StatusBar {
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        view: StatusView<'_>,
    ) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(theme.border_unfocused())
            .style(theme.surface());
        let rendered = self.render_line(area.width, theme, view);
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
        let text = self.run.text.clone();
        match self.run.tone {
            RunTone::Ready => Span::styled(text, theme.muted()),
            RunTone::Busy => {
                let spinner = SPINNER_FRAMES[self.spinner_frame];
                Span::styled(format!("{spinner} {text}"), theme.info())
            }
            RunTone::Alert => Span::styled(text, theme.warning()),
        }
    }

    fn render_line(
        &mut self,
        width: u16,
        theme: &Theme,
        view: StatusView<'_>,
    ) -> RenderedStatusLine {
        let StatusView {
            session,
            run,
            queued_prompts,
            task_progress,
        } = view;
        self.follow_run(run);
        let config = &session.config;
        self.line.render(
            theme,
            &StatusLineState {
                model: config.model.short_display_name(),
                effort: config.effort,
                title: session.title.as_deref(),
                usage: session.usage,
                cwd: &session.display_cwd,
                git_branch: self.git_branch.as_deref(),
                pull_request: self.pull_request.as_ref(),
                rate_limits: session.rate_limits,
                permission_mode: config.permission_mode,
                status_span: self.status_span(theme),
                queued_prompts,
                task_progress,
                session_id: &session.session_id,
                theme_name: &config.theme_name,
                current_minute: self.current_minute,
            },
            width,
        )
    }

    /// Takes `run` as the run state, restarting the spinner when the text changes.
    fn follow_run(&mut self, run: RunLabel) {
        if run.text != self.run.text {
            self.spinner_frame = 0;
            self.next_spinner_frame = crate::now() + SPINNER_FRAME_INTERVAL;
        }
        self.run = run;
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
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use time::macros::{datetime, time};

    use ox_core::client::{RateLimitWindow, RateLimits};
    use ox_core::model::{Effort, ModelSpec};
    use ox_core::protocol::UsageSnapshot;
    use ox_core::util::git::testing::pull_request;

    use super::*;
    use crate::session_info::testing::unknown_model_session_info;

    /// A bar and the App state it renders.
    struct Scene {
        bar: StatusBar,
        session: LiveSessionInfo,
        run: RunLabel,
        queued_prompts: usize,
        task_progress: Option<Progress>,
    }

    impl Scene {
        /// An idle `test-model` session at `High` effort in `~/test` on `main`, showing `segments`.
        fn new(segments: Vec<StatusLineSegment>) -> Self {
            let session = unknown_model_session_info();
            Self {
                bar: StatusBar::new(segments, Some("main".to_owned())),
                session,
                run: RunLabel::READY,
                queued_prompts: 0,
                task_progress: None,
            }
        }

        /// The default segments on an `Opus 5.5` session at `Xhigh` effort in `cwd`.
        fn opus(cwd: &str) -> Self {
            let mut scene = Self::new(StatusLineSegment::DEFAULT.to_vec());
            scene.session.config.model = ModelSpec::resolve("claude-opus-5-5");
            scene.session.config.effort = Some(Effort::Xhigh);
            cwd.clone_into(&mut scene.session.display_cwd);
            scene
        }

        /// Shows `segment` between the directory and the model, with no branch.
        fn segment(segment: StatusLineSegment) -> Self {
            let mut scene = Self::opus("~/projects/demo");
            scene.bar = StatusBar::new(
                vec![
                    StatusLineSegment::CurrentDir,
                    segment,
                    StatusLineSegment::ModelWithEffort,
                    StatusLineSegment::RunState,
                ],
                None,
            );
            scene
        }

        fn render(&mut self, width: u16) -> TestBackend {
            let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
            let view = StatusView {
                session: &self.session,
                run: self.run.clone(),
                queued_prompts: self.queued_prompts,
                task_progress: self.task_progress,
            };
            terminal
                .draw(|frame| {
                    self.bar
                        .render(frame, frame.area(), &Theme::default(), view);
                })
                .unwrap();
            terminal.backend().clone()
        }

        fn top_row(&mut self, width: u16) -> String {
            let backend = self.render(width);
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

        /// Renders the bar 100 columns wide, then 40.
        fn wide_and_narrow(&mut self) -> String {
            [100, 40]
                .map(|width| self.render(width).to_string())
                .join("\n")
        }
    }

    /// A busy run state showing `text`.
    fn busy(text: &'static str) -> RunLabel {
        RunLabel {
            text: text.into(),
            tone: RunTone::Busy,
        }
    }

    /// A bar whose last render showed `run`.
    fn bar_showing(run: RunLabel) -> StatusBar {
        let mut bar = StatusBar::new(vec![StatusLineSegment::RunState], None);
        bar.follow_run(run);
        bar
    }

    /// Ticks the busy bar at each of its next `frames` spinner frames.
    fn advance_spinner(bar: &mut StatusBar, frames: usize) {
        for _ in 0..frames {
            assert!(bar.tick(bar.next_spinner_frame));
        }
    }

    mod set_git_branch {
        use super::*;

        #[test]
        fn reports_only_real_changes() {
            let mut bar =
                StatusBar::new(StatusLineSegment::DEFAULT.to_vec(), Some("main".to_owned()));
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
            let mut bar = StatusBar::new(vec![StatusLineSegment::PullRequest], None);
            assert!(!bar.set_pull_request(None));
            assert!(bar.set_pull_request(Some(pull_request(1))));
            assert!(!bar.set_pull_request(Some(pull_request(1))));
            assert!(bar.set_pull_request(Some(pull_request(2))));
            assert_eq!(bar.pull_request, Some(pull_request(2)));
        }
    }

    mod spinner {
        use super::*;

        #[test]
        fn follows_the_animation_frame() {
            let mut bar = bar_showing(busy("Streaming"));

            advance_spinner(&mut bar, 3);

            assert_eq!(bar.spinner(), SPINNER_FRAMES[3]);
        }
    }

    mod tick {
        use super::*;

        #[test]
        fn idle_needs_no_repaint() {
            let mut bar = bar_showing(RunLabel::READY);

            assert!(!bar.tick(bar.next_spinner_frame));
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn without_current_time_segment_skips_minute_refresh() {
            let mut bar = StatusBar::new(
                vec![StatusLineSegment::Model, StatusLineSegment::RunState],
                None,
            );

            assert_eq!(bar.current_minute, None);
            assert!(!bar.tick(crate::now()));
            assert_eq!(bar.current_minute, None);
        }

        #[test]
        fn streaming_keeps_the_frame_until_the_frame_interval_passes() {
            let mut bar = bar_showing(busy("Streaming"));
            let before = bar.next_spinner_frame.checked_sub(Duration::from_millis(1));

            assert!(!bar.tick(before.unwrap()));
            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn streaming_advances_the_frame_once_the_interval_passes() {
            let mut bar = bar_showing(busy("Streaming"));
            let due = bar.next_spinner_frame;

            assert!(bar.tick(due));
            assert_eq!(bar.spinner_frame, 1);
            assert_eq!(bar.next_spinner_frame, due + SPINNER_FRAME_INTERVAL);
        }

        #[test]
        fn wraps_spinner_frames() {
            let mut bar = bar_showing(busy("Running bash"));

            advance_spinner(&mut bar, SPINNER_FRAMES.len());

            assert_eq!(bar.spinner_frame, 0);
        }
    }

    mod next_tick {
        use super::*;

        #[test]
        fn is_the_next_spinner_frame_while_busy() {
            let bar = bar_showing(busy("Streaming"));

            assert_eq!(bar.next_tick(crate::now()), Some(bar.next_spinner_frame));
        }

        #[test]
        fn never_changes_an_idle_bar_without_a_clock() {
            let bar = StatusBar::new(vec![StatusLineSegment::Model], None);

            assert_eq!(bar.next_tick(Instant::now()), None);
        }
    }

    mod next_tick_at {
        use super::*;

        const WALL_NOW: OffsetDateTime = datetime!(2026-01-01 12:00:00 UTC);

        #[test]
        fn is_the_next_minute_for_the_current_time_segment() {
            for (time, wait) in [
                (time!(12:34:00), Duration::from_mins(1)),
                (time!(12:34:59.950), Duration::from_millis(50)),
            ] {
                let bar = StatusBar::new(vec![StatusLineSegment::CurrentTime], None);
                let now = crate::now();

                assert_eq!(
                    bar.next_tick_at(now, WALL_NOW.replace_time(time)),
                    Some(now + wait),
                    "{time}",
                );
            }
        }

        #[test]
        fn selects_the_earlier_of_the_spinner_and_the_minute() {
            for (time, wait) in [
                (time!(12:34:00), SPINNER_FRAME_INTERVAL),
                (time!(12:34:59.950), Duration::from_millis(50)),
            ] {
                let mut bar = StatusBar::new(
                    vec![StatusLineSegment::RunState, StatusLineSegment::CurrentTime],
                    None,
                );
                bar.follow_run(busy("Streaming"));
                let now = crate::now();
                bar.next_spinner_frame = now + SPINNER_FRAME_INTERVAL;

                assert_eq!(
                    bar.next_tick_at(now, WALL_NOW.replace_time(time)),
                    Some(now + wait),
                    "{time}",
                );
            }
        }
    }

    mod refresh_current_time {
        use super::*;

        #[test]
        fn updates_only_when_the_displayed_minute_changes() {
            for (shown, current, changed) in [(720, 720, false), (720, 721, true), (1439, 0, true)]
            {
                let mut bar = StatusBar::new(vec![StatusLineSegment::CurrentTime], None);
                bar.current_minute = Some(shown);

                let dirty = bar.refresh_current_time(current);

                assert_eq!(dirty, changed, "{shown} → {current}");
                assert_eq!(bar.current_minute, Some(current), "{shown} → {current}");
            }
        }
    }

    mod render {
        use super::*;

        /// 2100-01-01, a reset no test run reaches.
        const FUTURE_RESET: i64 = 4_102_444_800;

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
        fn takes_the_run_state_of_its_view() {
            let mut scene = Scene::new(vec![StatusLineSegment::RunState]);
            scene.run = busy("Streaming");

            let row = scene.top_row(40);

            assert_eq!(row, format!("  {} Streaming", SPINNER_FRAMES[0]));
            assert_eq!(
                scene.bar.next_tick(crate::now()),
                Some(scene.bar.next_spinner_frame)
            );
        }

        #[test]
        fn idle_with_title_shows_model_title_and_cwd() {
            let mut scene = Scene::opus("~/projects/demo");
            scene.session.set_title(Some("Fix login flow".to_owned()));
            insta::assert_snapshot!(scene.render(100));
        }

        #[test]
        fn idle_without_title_leaves_slot_unused() {
            let mut scene = Scene::opus("~/projects/demo");
            insta::assert_snapshot!(scene.render(80));
        }

        #[test]
        fn puts_the_run_state_after_the_model() {
            let mut scene = Scene::opus("~/projects/demo");

            scene.run = busy("Streaming · Esc to interrupt");

            insta::assert_snapshot!(scene.render(80));
        }

        #[test]
        fn pull_request_checks_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::PullRequestChecks);
            scene.bar.set_pull_request(Some(git::PullRequest {
                checks: Some(git::Checks::Failing(2)),
                ..pull_request(86)
            }));
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn pull_request_review_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::PullRequestReview);
            scene.bar.set_pull_request(Some(git::PullRequest {
                review: Some(git::ReviewDecision::ChangesRequested),
                ..pull_request(86)
            }));
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn usage_shows_context_and_session_cost() {
            let mut scene = Scene::opus("~/projects/demo");
            scene.session.usage = Some(UsageSnapshot {
                context_tokens: Some(124_000),
                context_window: Some(1_000_000),
                estimated_cost_usd: Some(0.4321),
            });
            let output = scene.top_row(120);
            assert!(
                output.contains("Ctx: 12% (124k/1M)"),
                "usage slot should render before status: {output:?}",
            );
            assert!(output.contains("Cost: $0.43"));
            assert!(output.contains("Ready"));
        }

        #[test]
        fn five_hour_limit_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::FiveHourLimit);
            scene.session.rate_limits = Some(rate_limits());
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn weekly_limit_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::WeeklyLimit);
            scene.session.rate_limits = Some(rate_limits());
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn queued_prompts_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::QueuedPrompts);
            scene.queued_prompts = 2;
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn task_progress_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::TaskProgress);
            scene.task_progress = Some(Progress {
                completed: 2,
                total: 5,
            });
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn session_id_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::SessionId);
            scene.session.session_id = "3f2a9c1e-5b7d-4e8f-9a0b-1c2d3e4f5a6b".to_owned();
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn theme_at_wide_and_narrow_widths() {
            let mut scene = Scene::segment(StatusLineSegment::Theme);
            scene.session.config.theme_name = "latte".to_owned();
            insta::assert_snapshot!(scene.wide_and_narrow());
        }

        #[test]
        fn configured_segments_control_order() {
            let mut scene = Scene::opus("~/projects/demo");
            scene.bar = StatusBar::new(
                vec![
                    StatusLineSegment::RunState,
                    StatusLineSegment::Model,
                    StatusLineSegment::CurrentDir,
                ],
                Some("main".to_owned()),
            );
            let output = scene.top_row(120);
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
            let mut scene = Scene::opus("~/projects/demo");
            scene.bar = StatusBar::new(
                vec![
                    StatusLineSegment::CurrentDir,
                    StatusLineSegment::GitBranch,
                    StatusLineSegment::ModelWithEffort,
                    StatusLineSegment::RunState,
                ],
                Some("feat/status-line".to_owned()),
            );
            let output = scene.top_row(120);
            assert!(
                output.contains("~/projects/demo │ feat/status-line │ Opus 5.5 (xhigh) │ Ready")
            );
        }

        #[test]
        fn narrow_width_preserves_model_and_run_state() {
            let mut scene = Scene::opus("~/projects/demo/long");
            scene
                .session
                .set_title(Some("A rather long session title".to_owned()));
            insta::assert_snapshot!(scene.render(40));
        }

        #[test]
        fn wide_shows_title_after_status() {
            let mut scene = Scene::new(StatusLineSegment::DEFAULT.to_vec());
            scene.session.set_title(Some("Fix auth bug".to_owned()));
            let output = scene.top_row(120);
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
            let mut scene = Scene::new(StatusLineSegment::DEFAULT.to_vec());
            let long =
                "A very long session title that keeps going well past any reasonable width limit";
            scene.session.set_title(Some(long.to_owned()));
            let output = scene.top_row(200);
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
            let mut scene = Scene::new(StatusLineSegment::DEFAULT.to_vec());
            let output = scene.top_row(120);
            assert!(output.contains("~/test"));
            assert!(
                !output.contains("..."),
                "no ellipsis without title: {output:?}"
            );
        }

        #[test]
        fn empty_cwd_drops_cwd_slot_entirely() {
            let mut scene = Scene::new(StatusLineSegment::DEFAULT.to_vec());
            scene.session.display_cwd = String::new();
            let output = scene.top_row(120);
            assert!(output.contains("test-model"));
            assert!(output.contains("Ready"));
            assert!(
                !output.contains('~'),
                "no tildified path should appear: {output:?}",
            );
        }

        #[test]
        fn records_pull_request_hyperlink_rect_for_post_flush_emission() {
            let mut scene = Scene::new(vec![
                StatusLineSegment::PullRequest,
                StatusLineSegment::RunState,
            ]);
            scene.bar.pull_request = Some(pull_request(86));
            scene.render(40);
            let links = scene.bar.take_pending_hyperlinks();
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
            let mut scene = Scene::new(StatusLineSegment::DEFAULT.to_vec());
            scene.render(40);
            assert!(scene.bar.take_pending_hyperlinks().is_empty());
        }

        #[test]
        fn drops_the_hyperlink_when_the_bar_is_too_narrow_for_its_segment() {
            let mut scene = Scene::new(vec![
                StatusLineSegment::PullRequest,
                StatusLineSegment::RunState,
            ]);
            scene.bar.pull_request = Some(pull_request(86));
            scene.render(10);
            assert!(scene.bar.take_pending_hyperlinks().is_empty());
        }
    }

    mod take_pending_hyperlinks {
        use super::*;

        #[test]
        fn drains_after_first_call() {
            let mut scene = Scene::new(vec![
                StatusLineSegment::PullRequest,
                StatusLineSegment::RunState,
            ]);
            scene.bar.pull_request = Some(pull_request(86));
            scene.render(40);
            assert_eq!(scene.bar.take_pending_hyperlinks().len(), 1);
            assert!(
                scene.bar.take_pending_hyperlinks().is_empty(),
                "second take leaves nothing behind",
            );
        }
    }

    mod status_span {
        use super::*;

        #[test]
        fn ready_reads_in_the_muted_style() {
            let bar = bar_showing(RunLabel::READY);

            assert_eq!(
                bar.status_span(&Theme::default()),
                Span::styled("Ready", Theme::default().muted())
            );
        }

        #[test]
        fn busy_label_follows_the_spinner_in_the_info_style() {
            let mut bar = bar_showing(busy("Streaming"));
            advance_spinner(&mut bar, 1);

            assert_eq!(
                bar.status_span(&Theme::default()),
                Span::styled(
                    format!("{} Streaming", SPINNER_FRAMES[1]),
                    Theme::default().info()
                )
            );
        }

        #[test]
        fn alert_reads_as_a_warning() {
            let bar = bar_showing(RunLabel {
                text: "Press Ctrl+C again to exit".into(),
                tone: RunTone::Alert,
            });

            assert_eq!(
                bar.status_span(&Theme::default()),
                Span::styled("Press Ctrl+C again to exit", Theme::default().warning())
            );
        }
    }

    mod follow_run {
        use super::*;

        #[test]
        fn resets_spinner_on_transition() {
            let mut bar = bar_showing(busy("Streaming"));
            advance_spinner(&mut bar, 3);

            bar.follow_run(busy("Running bash"));

            assert_eq!(bar.spinner_frame, 0);
        }

        #[test]
        fn same_label_preserves_spinner() {
            let mut bar = bar_showing(busy("Streaming"));
            advance_spinner(&mut bar, 2);

            bar.follow_run(busy("Streaming"));

            assert_eq!(bar.spinner_frame, 2);
        }

        #[test]
        fn to_ready_resets_spinner() {
            let mut bar = bar_showing(busy("Streaming"));
            advance_spinner(&mut bar, 2);

            bar.follow_run(RunLabel::READY);

            assert_eq!(bar.spinner_frame, 0);
            assert!(!bar.tick(bar.next_spinner_frame));
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
