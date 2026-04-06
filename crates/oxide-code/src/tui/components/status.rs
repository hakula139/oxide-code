use crossterm::event::Event;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::component::{Action, Component};
use crate::tui::theme::Theme;

/// Status bar at the top of the TUI.
///
/// Displays the product name, model, and current status. Uses pipe `│`
/// separators between items and dimmed labels with bright values, matching
/// the user's neovim / tmux style.
pub(crate) struct StatusBar {
    theme: Theme,
    model: String,
    status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Streaming,
    ToolRunning,
}

impl StatusBar {
    pub(crate) fn new(theme: Theme, model: String) -> Self {
        Self {
            theme,
            model,
            status: Status::Idle,
        }
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        self.status = status;
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
            Status::Streaming => Span::styled("streaming...", self.theme.warning()),
            Status::ToolRunning => Span::styled("running tool...", self.theme.warning()),
        };

        let line = Line::from(vec![
            Span::raw("  "),
            name,
            sep.clone(),
            model,
            sep,
            status_span,
        ]);

        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(self.theme.border_unfocused());
        let bar = Paragraph::new(line).block(block);
        frame.render_widget(bar, area);
    }
}
