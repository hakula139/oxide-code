//! Slash-command output block: a `▎` left-bar in `accent` plus body
//! in `text`. Errors keep their own `ErrorBlock` styling so a
//! transcript scan distinguishes informational from error output.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx};
use crate::tui::glyphs::{BAR, TOOL_BORDER_PREFIX};
use crate::tui::wrap::wrap_line;

/// Output from a locally-dispatched slash command.
pub(crate) struct SystemMessageBlock {
    text: String,
}

impl SystemMessageBlock {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl ChatBlock for SystemMessageBlock {
    #[cfg(test)]
    fn system_text(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let bar_style = ctx.theme.accent();
        let body_style = ctx.theme.text();
        let width = usize::from(ctx.width);
        let cont_prefix = bar_continuation_prefix(bar_style);
        let indent = TOOL_BORDER_PREFIX.width();
        let mut out = Vec::new();
        for body_line in self.text.lines() {
            let line = Line::from(vec![
                Span::styled(TOOL_BORDER_PREFIX.to_owned(), bar_style),
                Span::styled(body_line.to_owned(), body_style),
            ]);
            out.extend(wrap_line(line, width, indent, Some(&cont_prefix)));
        }
        if out.is_empty() {
            // Empty content → single bar so the block is visible rather than silently dropped.
            out.push(Line::from(Span::styled(
                TOOL_BORDER_PREFIX.to_owned(),
                bar_style,
            )));
        }
        out
    }
}

fn bar_continuation_prefix(bar_style: Style) -> Vec<Span<'static>> {
    let bar_pos = TOOL_BORDER_PREFIX
        .find(BAR)
        .expect("TOOL_BORDER_PREFIX contains BAR");
    let trailing = &TOOL_BORDER_PREFIX[bar_pos + BAR.len()..];
    vec![Span::styled(BAR, bar_style), Span::raw(trailing.to_owned())]
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;
    use crate::tui::theme::Theme;

    fn ctx_at(width: u16, theme: &Theme) -> RenderCtx<'_> {
        RenderCtx {
            width,
            theme,
            show_thinking: false,
        }
    }

    // ── render ──

    #[test]
    fn render_each_input_line_gets_bar_prefix() {
        let theme = Theme::default();
        let block = SystemMessageBlock::new(indoc! {"
            first
            second
            third
        "});
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 3);
        for (i, expected) in ["first", "second", "third"].iter().enumerate() {
            assert_eq!(lines[i].spans.len(), 2, "row {i}: bar + body");
            assert_eq!(lines[i].spans[0].content, TOOL_BORDER_PREFIX);
            assert_eq!(lines[i].spans[0].style, theme.accent());
            assert_eq!(lines[i].spans[1].content, *expected);
            assert_eq!(lines[i].spans[1].style, theme.text());
        }
    }

    #[test]
    fn render_empty_text_still_emits_a_bar_line() {
        // Empty payload still produces a visible block — silently
        // swallowing it would hide a bug at the call site.
        let theme = Theme::default();
        let block = SystemMessageBlock::new("");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content, TOOL_BORDER_PREFIX);
    }

    #[test]
    fn render_trailing_newline_does_not_emit_extra_blank_row() {
        // Pin `str::lines()` semantics — `split('\n')` would emit an extra empty trailing row.
        let theme = Theme::default();
        let block = SystemMessageBlock::new(indoc! {"
            alpha
            beta
        "});
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].spans[1].content, "beta");
    }

    #[test]
    fn render_wraps_long_body_under_bar_at_viewport_width() {
        // Wrapped continuation lines must re-emit the bar so the block reads as one visual unit.
        let theme = Theme::default();
        let block = SystemMessageBlock::new("alpha beta gamma delta epsilon zeta");
        let lines = block.render(&ctx_at(16, &theme));
        assert!(lines.len() >= 2, "expected wrap, got {lines:#?}");
        for (i, line) in lines.iter().enumerate() {
            // First row: bar+space as one span. Continuation: split
            // into [bar, space] so the bar carries `accent`.
            let head = &line.spans[0];
            let content = head.content.as_ref();
            assert!(
                content.starts_with(BAR),
                "row {i} bar prefix missing: {content:?}",
            );
            assert_eq!(head.style, theme.accent());
        }
    }

    #[test]
    fn render_per_logical_line_wraps_independently() {
        // Two long source lines wrap separately; a body that joined
        // them would render as one paragraph.
        let theme = Theme::default();
        let block = SystemMessageBlock::new(
            "first really long line of text\nsecond really long line of text",
        );
        let lines = block.render(&ctx_at(20, &theme));
        let bodies: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let joined = bodies.join("\n");
        assert!(joined.contains("first"));
        assert!(joined.contains("second"));
        // Each logical line wraps to >= 2 visual rows at width 20.
        assert!(lines.len() >= 4, "expected ≥4 wrapped rows: {bodies:#?}");
    }
}
