use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::component::{Action, Component};
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
    status: Status,
    cwd: String,
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
            status: Status::Idle,
            cwd,
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
    fn handle_event(&mut self, _event: &Event) -> Option<Action> {
        None
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let sep = self.theme.separator_span();

        let name = Span::styled("ox", self.theme.accent());
        let model = Span::styled(self.model.as_str(), self.theme.text());

        let status_span = match self.status {
            Status::Idle => Span::styled("ready", self.theme.success()),
            Status::Streaming => {
                let spinner = SPINNER_FRAMES[self.spinner_frame];
                Span::styled(format!("{spinner} streaming..."), self.theme.warning())
            }
            Status::ToolRunning => {
                let spinner = SPINNER_FRAMES[self.spinner_frame];
                Span::styled(format!("{spinner} running tool..."), self.theme.warning())
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
