//! Chrome shared by modal bodies: the title header and the bottom hint footer.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

/// Footer hint for pickers that commit on Enter.
pub(crate) const CONFIRM_HINT: &str = "Enter to confirm  ·  Esc to cancel";

/// Rows [`render_footer`] occupies: a blank gap above the hint line.
pub(crate) const FOOTER_HEIGHT: u16 = 2;

// ── Header ──

/// Bold accent title, an optional dim description, and a blank row before the body.
pub(crate) struct Header {
    title: String,
    description: Option<String>,
}

impl Header {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            description: None,
        }
    }

    pub(crate) fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub(crate) fn height(&self) -> u16 {
        2 + u16::from(self.description.is_some())
    }

    /// Appends the header rows, each truncated to `width`.
    pub(crate) fn push_lines(&self, lines: &mut Vec<Line<'static>>, width: u16, theme: &Theme) {
        self.push_title_lines(lines, width, theme);
        lines.push(Line::default());
    }

    /// Appends the title and description rows without the gap, for a body that needs the room.
    pub(crate) fn push_title_lines(
        &self,
        lines: &mut Vec<Line<'static>>,
        width: u16,
        theme: &Theme,
    ) {
        let width = usize::from(width);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.title, width),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        if let Some(description) = &self.description {
            lines.push(Line::from(Span::styled(
                truncate_to_width(description, width),
                theme.dim(),
            )));
        }
    }
}

// ── Footer ──

/// Paints `hint` on the bottom row of `area`, provided the gap row above it fits too.
pub(crate) fn render_footer(frame: &mut Frame<'_>, area: Rect, hint: Line<'static>, theme: &Theme) {
    if area.height < FOOTER_HEIGHT {
        return;
    }
    let row = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    frame.render_widget(Paragraph::new(hint).style(theme.surface()), row);
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    // ── Header::height ──

    #[test]
    fn height_counts_the_title_blank_and_optional_description() {
        assert_eq!(Header::new("Pick").height(), 2);
        assert_eq!(Header::new("Pick").with_description("Choose").height(), 3);
    }

    // ── Header::push_lines ──

    #[test]
    fn push_lines_emits_title_description_then_blank_truncated_to_width() {
        let header = Header::new("Select theme").with_description("Switch the palette");
        let mut lines = Vec::new();
        header.push_lines(&mut lines, 12, &Theme::default());

        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(text, ["Select theme", "Switch th...", ""]);
        assert_eq!(lines.len(), usize::from(header.height()));
    }

    // ── render_footer ──

    #[test]
    fn render_footer_paints_the_bottom_row_below_a_gap() {
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();
        terminal
            .draw(|frame| {
                render_footer(
                    frame,
                    Rect::new(0, 0, 20, 4),
                    Line::from("Esc to close"),
                    &Theme::default(),
                );
            })
            .unwrap();
        assert_eq!(row_text(&terminal, 2), "");
        assert_eq!(row_text(&terminal, 3), "Esc to close");
    }

    #[test]
    fn render_footer_skips_an_area_without_room_for_the_gap() {
        let mut terminal = Terminal::new(TestBackend::new(20, 1)).unwrap();
        terminal
            .draw(|frame| {
                render_footer(
                    frame,
                    Rect::new(0, 0, 20, 1),
                    Line::from("Esc to close"),
                    &Theme::default(),
                );
            })
            .unwrap();
        assert_eq!(row_text(&terminal, 0), "");
    }
}
