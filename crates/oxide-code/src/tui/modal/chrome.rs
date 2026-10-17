//! Chrome shared by modal bodies: the title header and the bottom hint footer.

use std::borrow::Borrow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── Constants ──

pub(crate) const HINT_SEPARATOR: &str = "  ·  ";
pub(crate) const KEY_UP_DOWN: &str = "↑ / ↓";
pub(crate) const KEY_LEFT_RIGHT: &str = "← / →";

/// Footer hints for pickers that commit on Enter.
pub(crate) const CONFIRM_HINTS: [&str; 2] = ["Enter to confirm", "Esc to cancel"];

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

/// Paints `hints` joined by [`HINT_SEPARATOR`] on the bottom row of `area`, provided the gap row
/// above it fits too. A narrow row cuts the tail, so callers list the essential hints first.
pub(crate) fn render_footer<S: Borrow<str>>(
    frame: &mut Frame<'_>,
    area: Rect,
    hints: &[S],
    theme: &Theme,
) {
    render_footer_text(frame, area, &hints.join(HINT_SEPARATOR), theme.dim(), theme);
}

/// Paints `text` in `style` where [`render_footer`] paints its hints, truncated to the row.
pub(crate) fn render_footer_text(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    style: Style,
    theme: &Theme,
) {
    if area.height < FOOTER_HEIGHT {
        return;
    }
    let row = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let text = truncate_to_width(text, usize::from(area.width));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))).style(theme.surface()),
        row,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::modal::testing::render_rows_with;

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
    fn render_footer_joins_hints_on_the_bottom_row_below_a_gap() {
        let rows = render_rows_with(40, 4, |frame, area, theme| {
            render_footer(frame, area, &CONFIRM_HINTS, theme);
        });
        assert_eq!(rows[2].trim_end(), "");
        assert_eq!(rows[3].trim_end(), "Enter to confirm  ·  Esc to cancel");
    }

    #[test]
    fn render_footer_truncates_the_tail_hints_on_a_narrow_row() {
        let rows = render_rows_with(24, 2, |frame, area, theme| {
            render_footer(frame, area, &CONFIRM_HINTS, theme);
        });
        assert_eq!(rows[1], "Enter to confirm  ·  ...");
    }

    #[test]
    fn render_footer_skips_an_area_without_room_for_the_gap() {
        let rows = render_rows_with(20, 1, |frame, area, theme| {
            render_footer(frame, area, &["Esc to close"], theme);
        });
        assert_eq!(rows[0].trim_end(), "");
    }

    // ── render_footer_text ──

    #[test]
    fn render_footer_text_truncates_a_message_in_its_own_style() {
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(12, 2)).unwrap();
        terminal
            .draw(|frame| {
                render_footer_text(
                    frame,
                    frame.area(),
                    "! permission denied",
                    theme.error(),
                    &theme,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let row: String = (0..12).map(|x| buffer[(x, 1)].symbol()).collect();
        assert_eq!(row, "! permiss...");
        assert_eq!(buffer[(0, 1)].fg, theme.error().fg.unwrap());
    }
}
