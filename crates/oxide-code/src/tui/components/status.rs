//! Configurable status line component.

mod line;

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::agent::event::UsageSnapshot;
use crate::config::{Effort, StatusLineSegment};
use crate::tui::glyphs::SPINNER_FRAMES;
use crate::tui::theme::Theme;

use self::line::{StatusLine, StatusLineState};

const TICKS_PER_FRAME: usize = 5;

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
        git_branch: Option<String>,
    ) -> Self {
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

    /// Re-skin subsequent renders; the spinner / status state is unaffected.
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

    /// Returns `true` when time or animation state changed and the caller should repaint.
    pub(crate) fn tick(&mut self) -> bool {
        let mut dirty = self.refresh_current_time();
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
        bar.set_model("Claude Opus 4.7".to_owned());
        assert_eq!(bar.model(), "Claude Opus 4.7");
        let output = render_top_row(&mut bar, 80);
        assert!(
            output.contains("Claude Opus 4.7"),
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
            "Claude Opus 4.7".into(),
            Some(Effort::Xhigh),
            cwd.into(),
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
            "Claude Opus 4.7".into(),
            Some(Effort::Xhigh),
            "~/projects/demo".into(),
            Some("main".to_owned()),
        );
        let output = render_top_row(&mut bar, 120);
        let state_at = output.find("Ready").unwrap();
        let model_at = output.find("Claude Opus 4.7").unwrap();
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
            "Claude Opus 4.7".into(),
            Some(Effort::Xhigh),
            "~/projects/demo".into(),
            Some("feat/status-line".to_owned()),
        );
        let output = render_top_row(&mut bar, 120);
        assert!(
            output.contains("~/projects/demo │ feat/status-line │ Claude Opus 4.7 (xhigh) │ Ready")
        );
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
