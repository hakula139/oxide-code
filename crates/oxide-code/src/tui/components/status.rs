use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::agent::event::UserAction;
use crate::tui::component::Component;
use crate::tui::theme::Theme;

/// Braille spinner animation frames (~80 ms per frame at 60 FPS ticks).
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Number of 16 ms ticks between spinner frame advances (~80 ms).
const TICKS_PER_FRAME: usize = 5;

/// Status bar at the top of the TUI.
///
/// Displays the product name, model, current status with a braille spinner,
/// and the working directory (right-aligned).
pub(crate) struct StatusBar {
    theme: Theme,
    model: String,
    cwd: String,
    status: Status,
    spinner_frame: usize,
    tick_counter: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    ToolRunning,
}

impl StatusBar {
    pub(crate) fn new(theme: Theme, model: String, cwd: String) -> Self {
        Self {
            theme,
            model,
            cwd,
            status: Status::Idle,
            spinner_frame: 0,
            tick_counter: 0,
        }
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        if status != self.status {
            self.spinner_frame = 0;
            self.tick_counter = 0;
        }
        self.status = status;
    }

    /// Advance the spinner animation. Call on each tick when not idle.
    /// Returns `true` if the spinner frame changed (caller should mark dirty).
    pub(crate) fn tick(&mut self) -> bool {
        if self.status == Status::Idle {
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

        let name = Span::styled("ox", self.theme.accent());
        let model = Span::styled(self.model.as_str(), self.theme.text());

        let status_span = match self.status {
            Status::Idle => Span::styled("ready", self.theme.success()),
            Status::Streaming | Status::ToolRunning => {
                let spinner = SPINNER_FRAMES[self.spinner_frame];
                let label = match self.status {
                    Status::Streaming => "streaming...",
                    _ => "running tool...",
                };
                Span::styled(format!("{spinner} {label}"), self.theme.warning())
            }
        };

        // Left side: ox │ model │ status
        let left_spans = vec![Span::raw("  "), name, sep.clone(), model, sep, status_span];

        // Right side: cwd (dimmed, right-aligned)
        let cwd_span = Span::styled(&self.cwd, self.theme.dim());
        let cwd_display_width = cwd_span.width() + 2;

        let left_width: usize = left_spans.iter().map(Span::width).sum();
        let area_width = usize::from(area.width);

        let mut spans = left_spans;
        if left_width + cwd_display_width < area_width {
            let gap = area_width - left_width - cwd_display_width;
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(cwd_span);
            spans.push(Span::raw("  "));
        }

        let line = Line::from(spans);
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(self.theme.border_unfocused());
        let bar = Paragraph::new(line).block(block);
        frame.render_widget(bar, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bar() -> StatusBar {
        StatusBar::new(
            Theme::default(),
            "test-model".to_owned(),
            "~/test".to_owned(),
        )
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

        bar.set_status(Status::ToolRunning);
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
        bar.set_status(Status::ToolRunning);

        for _ in 0..SPINNER_FRAMES.len() * TICKS_PER_FRAME {
            bar.tick();
        }
        assert_eq!(bar.spinner_frame, 0);
    }

    // ── render ──

    fn render_to_string(bar: &StatusBar, width: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                bar.render(frame, Rect::new(0, 0, width, 2));
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..width)
            .map(|x| {
                buf.cell((x, 0))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn render_idle_shows_ready() {
        let bar = test_bar();
        let output = render_to_string(&bar, 80);
        assert!(output.contains("ox"));
        assert!(output.contains("test-model"));
        assert!(output.contains("ready"));
    }

    #[test]
    fn render_streaming_shows_spinner() {
        let mut bar = test_bar();
        bar.set_status(Status::Streaming);
        let output = render_to_string(&bar, 80);
        assert!(output.contains("streaming..."));
    }

    #[test]
    fn render_tool_running_shows_spinner() {
        let mut bar = test_bar();
        bar.set_status(Status::ToolRunning);
        let output = render_to_string(&bar, 80);
        assert!(output.contains("running tool..."));
    }

    #[test]
    fn render_wide_shows_cwd() {
        let bar = test_bar();
        let output = render_to_string(&bar, 120);
        assert!(output.contains("~/test"));
    }

    #[test]
    fn render_narrow_omits_cwd() {
        let bar = test_bar();
        let output = render_to_string(&bar, 30);
        assert!(!output.contains("~/test"));
    }
}
