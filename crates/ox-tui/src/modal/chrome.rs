//! Chrome shared by modal bodies: the title header and the bottom hint footer.

use std::borrow::Borrow;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr as _;

use ox_core::util::text::truncate_to_width;

use crate::theme::Theme;

// ── Constants ──

pub(crate) const HINT_SEPARATOR: &str = "  ·  ";
pub(crate) const KEY_UP_DOWN: &str = "Up / Down";
pub(crate) const KEY_LEFT_RIGHT: &str = "Left / Right";

/// Footer hints for pickers that commit on Enter.
pub(crate) const CONFIRM_HINTS: [&str; 2] = ["Enter to confirm", "Esc to cancel"];

/// Rows [`render_footer`] occupies: a blank gap above the hint line.
pub(crate) const FOOTER_HEIGHT: u16 = 2;

/// The height of a body sized by `body` within the rows `max_height` leaves above `below` fixed
/// rows, plus those rows, within `max_height`.
pub(crate) fn height_above(max_height: u16, below: u16, body: impl FnOnce(u16) -> u16) -> u16 {
    (body(max_height.saturating_sub(below)) + below).min(max_height)
}

/// `area` without its bottom `below` rows.
pub(crate) fn above(area: Rect, below: u16) -> Rect {
    Rect {
        height: area.height.saturating_sub(below),
        ..area
    }
}

/// Blank columns between two text columns of a modal row.
pub(crate) const COLUMN_GAP: usize = 2;

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
        lines.push(Line::default());
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

/// Packs `hints` into rows of at most `width` columns joined by [`HINT_SEPARATOR`], breaking only
/// between hints, so no hint loses its key to a cut. A hint wider than a whole row is truncated.
pub(crate) fn wrap_hints<S: Borrow<str>>(hints: &[S], width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for hint in hints {
        let hint = hint.borrow();
        match rows.last_mut() {
            Some(row) if row.width() + HINT_SEPARATOR.width() + hint.width() <= width => {
                row.push_str(HINT_SEPARATOR);
                row.push_str(hint);
            }
            _ => rows.push(truncate_to_width(hint, width)),
        }
    }
    rows
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
    use crate::modal::testing::render_rows_with;
    use crate::testing::buffer_rows;

    mod header {
        use super::*;

        mod height {
            use super::*;

            #[test]
            fn counts_the_title_blank_and_optional_description() {
                assert_eq!(Header::new("Pick").height(), 2);
                assert_eq!(Header::new("Pick").with_description("Choose").height(), 3);
            }
        }

        mod push_lines {
            use super::*;

            #[test]
            fn emits_title_description_then_blank_truncated_to_width() {
                let header = Header::new("Select theme").with_description("Switch the palette");
                let mut lines = Vec::new();

                header.push_lines(&mut lines, 12, &Theme::default());

                let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
                assert_eq!(text, ["Select theme", "Switch th...", ""]);
                assert_eq!(lines.len(), usize::from(header.height()));
            }
        }
    }

    mod render_footer {
        use super::*;

        #[test]
        fn joins_hints_on_the_bottom_row_below_a_gap() {
            let rows = render_rows_with(40, 4, |frame, area, theme| {
                render_footer(frame, area, &CONFIRM_HINTS, theme);
            });

            assert_eq!(rows[2].trim_end(), "");
            assert_eq!(rows[3].trim_end(), "Enter to confirm  ·  Esc to cancel");
        }

        #[test]
        fn truncates_the_tail_hints_on_a_narrow_row() {
            let rows = render_rows_with(24, 2, |frame, area, theme| {
                render_footer(frame, area, &CONFIRM_HINTS, theme);
            });

            assert_eq!(rows[1], "Enter to confirm  ·  ...");
        }

        #[test]
        fn skips_an_area_without_room_for_the_gap() {
            let rows = render_rows_with(20, 1, |frame, area, theme| {
                render_footer(frame, area, &["Esc to close"], theme);
            });

            assert_eq!(rows[0].trim_end(), "");
        }
    }

    mod wrap_hints {
        use super::*;

        #[test]
        fn breaks_rows_between_hints_and_cuts_only_an_oversized_hint() {
            let hints = ["Y to approve", "N to deny", "Esc to abort"];
            for (width, expected) in [
                (60, vec!["Y to approve  ·  N to deny  ·  Esc to abort"]),
                (30, vec!["Y to approve  ·  N to deny", "Esc to abort"]),
                (12, vec!["Y to approve", "N to deny", "Esc to abort"]),
                (10, vec!["Y to ap...", "N to deny", "Esc to ..."]),
            ] {
                assert_eq!(wrap_hints(&hints, width), expected, "width {width}");
            }
        }
    }

    mod render_footer_text {
        use super::*;

        #[test]
        fn truncates_a_message_in_its_own_style() {
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
            let rows = buffer_rows(buffer);
            let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
            assert_eq!(rows, ["", "! permiss..."]);
            assert_eq!(buffer[(0, 1)].fg, theme.error().fg.unwrap());
        }
    }
}
