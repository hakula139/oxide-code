use std::time::Instant;

use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::agent::event::UserAction;
use crate::tui::component::Component;
use crate::tui::glyphs::SPINNER_FRAMES;
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

/// Number of 16 ms ticks between spinner frame advances (~80 ms).
const TICKS_PER_FRAME: usize = 5;

/// Upper bound on the session-title width before ellipsis-truncation. Chosen
/// so the title cannot crowd out the status slot on typical 80-column
/// terminals: core left (`  ox │ model │ streaming...`) is ~30 columns, 40
/// leaves breathing room for cwd on the right.
const MAX_TITLE_WIDTH: usize = 40;

/// Status bar at the top of the TUI.
///
/// Displays the product name, model, optional session title, current status
/// with a braille spinner, and the working directory (right-aligned). The
/// title and cwd slots drop gracefully when the terminal is too narrow.
pub(crate) struct StatusBar {
    theme: Theme,
    model: String,
    /// Session title. `None` until a title is set — either the first-prompt
    /// title from session resume, or an AI-generated title appended during
    /// the turn. Truncated to [`MAX_TITLE_WIDTH`] on render.
    title: Option<String>,
    cwd: String,
    status: Status,
    spinner_frame: usize,
    tick_counter: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    /// A tool is running. `name` is the tool's display name (`bash`,
    /// `read`, `edit`, etc.) so the busy hint can read
    /// `"<name> · Esc to interrupt"` rather than a generic label.
    ToolRunning {
        name: String,
    },
    /// User-issued `Cancel` is being honored — the partial turn is
    /// being torn down, but [`crate::agent::event::AgentEvent::Cancelled`]
    /// hasn't arrived yet.
    Cancelling,
    /// First Ctrl+C from idle armed an exit confirmation. A second
    /// Ctrl+C before `until` exits; otherwise the timer expires and
    /// the status returns to [`Self::Idle`] silently.
    ExitArmed {
        until: Instant,
    },
}

impl StatusBar {
    pub(crate) fn new(theme: &Theme, model: String, cwd: String) -> Self {
        Self {
            theme: theme.clone(),
            model,
            title: None,
            cwd,
            status: Status::Idle,
            spinner_frame: 0,
            tick_counter: 0,
        }
    }

    /// Sets or clears the session title displayed between model and status.
    /// Pass `None` or an empty string to remove the title entirely (the slot
    /// and its separator disappear from the bar).
    pub(crate) fn set_title(&mut self, title: Option<String>) {
        self.title = title.filter(|t| !t.trim().is_empty());
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        if status != self.status {
            self.spinner_frame = 0;
            self.tick_counter = 0;
        }
        self.status = status;
    }

    /// Current status. Used by [`App`](super::super::app::App) to
    /// arm-or-exit on Ctrl+C and to clear an expired
    /// [`Status::ExitArmed`] from the tick arm.
    pub(crate) fn status(&self) -> &Status {
        &self.status
    }

    /// Current title slot, or `None` when no title is set. Same
    /// rationale as [`status`][Self::status].
    #[cfg(test)]
    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Advances the spinner animation. Returns `true` if the frame
    /// changed (caller should mark dirty). The non-spinner states
    /// ([`Status::Idle`], [`Status::ExitArmed`]) short-circuit.
    pub(crate) fn tick(&mut self) -> bool {
        if !is_animated(&self.status) {
            return false;
        }
        self.tick_counter += 1;
        if self.tick_counter >= TICKS_PER_FRAME {
            self.tick_counter = 0;
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            return true;
        }
        false
    }
}

impl Component for StatusBar {
    fn handle_event(&mut self, _event: &Event) -> Option<UserAction> {
        None
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let sep = self.theme.separator_span();
        let area_width = usize::from(area.width);

        // Core: `  ox │ model │ status` — always rendered.
        let core = vec![
            Span::raw("  "),
            Span::styled("ox", self.theme.accent()),
            sep.clone(),
            Span::styled(self.model.as_str(), self.theme.text()),
            sep.clone(),
            self.status_span(),
        ];
        let core_width: usize = core.iter().map(Span::width).sum();

        // Title: `│ title` inserted between model and status when there is
        // room. Truncated to MAX_TITLE_WIDTH with a trailing ellipsis.
        let title_slot = self
            .title
            .as_deref()
            .map(|t| title_slot_spans(t, &sep, self.theme.muted()));
        let title_slot_width = title_slot.as_deref().map_or(0, slot_width);

        // CWD: `<gap> cwd  ` on the right edge. Dropped when the remaining
        // budget is too small to fit `gap + cwd + 2-space margin`. The +1
        // accounts for the minimum gap column between status and cwd.
        let cwd_slot_content_width = self.cwd.width() + 2;
        let cwd_display_width = if self.cwd.is_empty() {
            0
        } else {
            cwd_slot_content_width + 1
        };

        // Greedy fit: try [core + title + cwd] → [core + title] →
        // [core + cwd] → [core]. Title is sacrificed before cwd because cwd
        // provides location context that's hard to recover elsewhere.
        let mut spans = core;
        let (include_title, include_cwd) =
            fit_layout(area_width, core_width, title_slot_width, cwd_display_width);
        if include_title && let Some(slot) = title_slot {
            // Insert title spans between model and status. Status is the
            // last span of `core`, so replace-with-insert-before-status.
            let status = spans.pop().expect("core always has the status span");
            spans.extend(slot);
            spans.push(status);
        }
        if include_cwd {
            let used: usize = spans.iter().map(Span::width).sum();
            let gap = area_width - used - cwd_slot_content_width;
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(&self.cwd, self.theme.dim()));
            spans.push(Span::raw("  "));
        }

        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(self.theme.border_unfocused())
            .style(self.theme.surface());
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
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
            Status::Cancelling => self.busy_span("Cancelling..."),
            Status::ExitArmed { .. } => {
                Span::styled("Press Ctrl+C again to exit", self.theme.warning())
            }
        }
    }

    fn busy_span(&self, label: &str) -> Span<'static> {
        let spinner = SPINNER_FRAMES[self.spinner_frame];
        Span::styled(format!("{spinner} {label}"), self.theme.info())
    }
}

/// Whether the status drives the braille spinner. Idle is at rest;
/// [`Status::ExitArmed`] uses a static hint instead of an animation.
fn is_animated(status: &Status) -> bool {
    matches!(
        status,
        Status::Streaming | Status::ToolRunning { .. } | Status::Cancelling,
    )
}

/// Builds the `title │` insert. The leading `│` is provided by the separator
/// core already places after `model`, so the slot itself is
/// `[title, trailing_sep]` — inserting it before `status` yields the
/// `model │ title │ status` sequence without a doubled bar.
fn title_slot_spans<'a>(
    title: &'a str,
    sep: &Span<'a>,
    style: ratatui::style::Style,
) -> Vec<Span<'a>> {
    vec![
        Span::styled(truncate_to_width(title, MAX_TITLE_WIDTH), style),
        sep.clone(),
    ]
}

/// Total visual width of a slot's spans. Free helper so both the fit check
/// and the final insert share the same measurement.
fn slot_width(slot: &[Span<'_>]) -> usize {
    slot.iter().map(Span::width).sum()
}

/// Greedy fit: which optional slots can we afford? Returns
/// `(include_title, include_cwd)`. Rules:
///
/// - `core` is always included.
/// - cwd is preserved before title when both can't fit — cwd carries
///   location context (which directory you're in) that the title does not.
/// - a slot is "affordable" only when one column of breathing room remains
///   after it (strict `<`, not `≤`, to avoid the bar hitting the right edge).
fn fit_layout(area_width: usize, core: usize, title: usize, cwd: usize) -> (bool, bool) {
    let fits = |extra: usize| core + extra < area_width;
    match (
        title > 0 && fits(title + cwd),
        cwd > 0 && fits(cwd),
        title > 0 && fits(title),
    ) {
        (true, _, _) => (true, cwd > 0),
        (false, true, _) => (false, true),
        (false, false, true) => (true, false),
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn test_bar() -> StatusBar {
        StatusBar::new(
            &Theme::default(),
            "test-model".to_owned(),
            "~/test".to_owned(),
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
    fn tick_idle_returns_false() {
        let mut bar = test_bar();
        assert!(!bar.tick());
        assert_eq!(bar.spinner_frame, 0);
        assert_eq!(bar.tick_counter, 0);
    }

    #[test]
    fn tick_streaming_increments_counter_before_threshold() {
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

    // ── handle_event ──

    #[test]
    fn handle_event_is_inert() {
        // The status bar observes state via setters (`set_status`,
        // `set_title`, `tick`); crossterm events pass through untouched.
        let mut bar = test_bar();
        let key = Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(bar.handle_event(&key).is_none());
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

    /// Returns row 0 as a plain string for substring assertions about
    /// relative ordering, presence of ellipsis, or conditional slot
    /// drops — cases where a full snapshot would be more brittle than
    /// informative.
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
        // Real callers pass the pre-converted marketing name (see
        // `main::marketing_name`), so the snapshots mirror what users
        // see on screen rather than the raw API id.
        let mut bar = StatusBar::new(&Theme::default(), "Claude Opus 4.7".into(), cwd.into());
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
    fn render_tool_running_status() {
        let mut bar = bar_idle(None, "~/projects/demo");
        bar.set_status(Status::ToolRunning {
            name: "bash".to_owned(),
        });
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
    fn render_narrow_width_drops_cwd_and_title_slots() {
        // At 40 cols both slots drop entirely (title first, then cwd);
        // ellipsis truncation is covered by
        // `render_truncates_long_title_with_ellipsis` at generous widths.
        let mut bar = bar_idle(Some("A rather long session title"), "~/projects/demo/long");
        insta::assert_snapshot!(render_status(&mut bar, 40));
    }

    #[test]
    fn render_wide_shows_title_between_model_and_status() {
        let mut bar = test_bar();
        bar.set_title(Some("Fix auth bug".to_owned()));
        let output = render_top_row(&mut bar, 120);
        let model_at = output.find("test-model").unwrap();
        let title_at = output.find("Fix auth bug").unwrap();
        let status_at = output.find("Ready").unwrap();
        assert!(model_at < title_at, "title should follow model: {output:?}");
        assert!(
            title_at < status_at,
            "title should precede status: {output:?}"
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
    fn render_drops_title_first_when_tight() {
        // Core `  ox │ test-model │ ready` is 25 cols, cwd slot is 9 cols
        // (6-char "~/test" + 2-char margin + 1-col gap), title slot is 15
        // cols ("Some long title" + trailing " │ "). Width 40 leaves room
        // for core + cwd (25 + 9 = 34 < 40) but not core + cwd + title
        // (25 + 9 + 15 = 49 > 40). Title must drop, cwd survives.
        let mut bar = test_bar();
        bar.set_title(Some("Some long title".to_owned()));
        let output = render_top_row(&mut bar, 40);
        assert!(output.contains("~/test"), "cwd should survive: {output:?}");
        assert!(
            !output.contains("Some long title"),
            "title should drop before cwd: {output:?}",
        );
    }

    #[test]
    fn render_no_title_still_shows_cwd_wide() {
        let mut bar = test_bar();
        let output = render_top_row(&mut bar, 120);
        // Sanity check that the no-title path still renders cwd.
        assert!(output.contains("~/test"));
        assert!(
            !output.contains("..."),
            "no ellipsis without title: {output:?}"
        );
    }

    #[test]
    fn render_empty_cwd_drops_cwd_slot_entirely() {
        // Empty cwd (current_dir failed) must short-circuit — no trailing
        // gap, no stray right margin. Generous width exercises the
        // `cwd.is_empty()` guard without racing the title-dropped path.
        let mut bar = StatusBar::new(&Theme::default(), "test-model".to_owned(), String::new());
        let output = render_top_row(&mut bar, 120);
        assert!(output.contains("ox"));
        assert!(output.contains("test-model"));
        assert!(output.contains("Ready"));
        assert!(
            !output.contains('~'),
            "no tildified path should appear: {output:?}",
        );
    }

    // ── fit_layout ──

    #[test]
    fn fit_layout_keeps_both_slots_when_everything_fits() {
        // Wide bar: core + title + cwd all fit with room to spare.
        assert_eq!(fit_layout(80, 25, 10, 10), (true, true));
    }

    #[test]
    fn fit_layout_drops_title_before_cwd_when_combined_too_wide() {
        // core (25) + title (10) + cwd (10) = 45, too wide for 40. cwd alone
        // fits (35 < 40). Title is sacrificed — cwd carries location context
        // that's harder to recover elsewhere.
        assert_eq!(fit_layout(40, 25, 10, 10), (false, true));
    }

    #[test]
    fn fit_layout_keeps_title_when_cwd_is_too_wide_to_fit_alone() {
        // core (25) + cwd (20) = 45, too wide for 40 — cwd drops.
        // core (25) + title (5) = 30 < 40 — title survives.
        // Fallback arm: when cwd can't fit anywhere, show the title instead
        // of an empty right side.
        assert_eq!(fit_layout(40, 25, 5, 20), (true, false));
    }

    #[test]
    fn fit_layout_drops_both_when_nothing_extra_fits() {
        // core already fills the bar; neither optional slot earns its column.
        assert_eq!(fit_layout(26, 25, 5, 5), (false, false));
    }
}
