//! Post-`/compact` boundary block.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx, bar_continuation_prefix, prepend_markdown_prefix};
use crate::tui::glyphs::TOOL_BORDER_PREFIX;
use crate::tui::markdown::render_markdown;
use crate::tui::wrap::wrap_line;

/// Boundary marker rendered after `/compact`.
pub(crate) struct CompactedBlock {
    header: String,
    summary: String,
}

impl CompactedBlock {
    pub(crate) fn new(
        pre_count: u32,
        instructions: Option<&str>,
        summary: impl Into<String>,
    ) -> Self {
        let plural = if pre_count == 1 { "" } else { "s" };
        let header = match instructions {
            Some(focus) => {
                format!("Compacted {pre_count} message{plural} → 1 summary (focus: {focus}).")
            }
            None => format!("Compacted {pre_count} message{plural} → 1 summary."),
        };
        Self {
            header,
            summary: summary.into(),
        }
    }
}

impl ChatBlock for CompactedBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        let bar_style = ctx.theme.accent();
        let header_style = bar_style.add_modifier(Modifier::BOLD);
        let body_style = ctx.theme.text();
        let width = usize::from(ctx.width);
        let bar_width = TOOL_BORDER_PREFIX.width();
        let md_width = width.saturating_sub(bar_width);
        let cont_prefix = bar_continuation_prefix(TOOL_BORDER_PREFIX, bar_style);

        let mut out: Vec<Line<'static>> = Vec::new();

        let header_line = Line::from(vec![
            Span::styled(TOOL_BORDER_PREFIX.to_owned(), bar_style),
            Span::styled(self.header.clone(), header_style),
        ]);
        out.extend(wrap_line(header_line, width, bar_width, Some(&cont_prefix)));

        if !self.summary.trim().is_empty() {
            // Bar-only gutter row separates header from body, mirroring AssistantThinking's
            // layout so the boundary reads as one visual unit.
            out.push(Line::from(Span::styled(
                TOOL_BORDER_PREFIX.to_owned(),
                bar_style,
            )));
            let rendered = render_markdown(&self.summary, ctx.theme, md_width);
            for line in rendered.lines {
                let mut prefixed = prepend_markdown_prefix(line, TOOL_BORDER_PREFIX, bar_style);
                // Markdown render may set Line::style for code blocks; preserve that and only
                // patch the body spans to the chat text style when no fg is set.
                for span in &mut prefixed.spans[1..] {
                    if span.style.fg.is_none() {
                        span.style = span.style.patch(body_style);
                    }
                }
                out.push(prefixed);
            }
        }

        out
    }
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

    fn header_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    // ── CompactedBlock::new ──

    #[test]
    fn header_pluralizes_message_count() {
        let one = CompactedBlock::new(1, None, "x");
        assert!(one.header.contains("1 message →"));
        let many = CompactedBlock::new(7, None, "x");
        assert!(many.header.contains("7 messages →"));
    }

    #[test]
    fn header_includes_focus_when_instructions_present() {
        let block = CompactedBlock::new(4, Some("focus on auth"), "x");
        assert!(block.header.contains("(focus: focus on auth)"));
    }

    #[test]
    fn header_omits_focus_segment_when_no_instructions() {
        let block = CompactedBlock::new(4, None, "x");
        assert!(!block.header.contains("focus:"));
    }

    // ── CompactedBlock::render ──

    #[test]
    fn render_first_line_is_bold_header_with_bar_prefix() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "summary body");
        let lines = block.render(&ctx_at(60, &theme));
        let head = &lines[0];
        let text = header_text(head);
        assert!(text.starts_with(TOOL_BORDER_PREFIX), "bar prefix: {text:?}");
        assert!(
            text.contains("Compacted 4 messages"),
            "header text: {text:?}"
        );
        // Header span carries BOLD.
        let header_span = &head.spans[1];
        assert!(
            header_span.style.add_modifier.contains(Modifier::BOLD),
            "header should be bold: {:?}",
            header_span.style,
        );
    }

    #[test]
    fn render_inserts_gutter_line_between_header_and_body() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "body line");
        let lines = block.render(&ctx_at(60, &theme));
        assert!(lines.len() >= 3, "header + gutter + body: {lines:?}");
        let gutter: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(gutter, TOOL_BORDER_PREFIX, "gutter is bar-only");
    }

    #[test]
    fn render_empty_body_emits_header_only() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "   \n  ");
        let lines = block.render(&ctx_at(60, &theme));
        assert_eq!(lines.len(), 1, "only header: {lines:?}");
    }

    #[test]
    fn render_summary_body_lines_carry_bar_prefix() {
        let theme = Theme::default();
        let block = CompactedBlock::new(
            4,
            None,
            indoc! {"
            - Done: shipped /compact
            - Next: tests
        "},
        );
        let lines = block.render(&ctx_at(60, &theme));
        // Body lines start at index 2 (header + gutter).
        for (i, line) in lines.iter().enumerate().skip(2) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(TOOL_BORDER_PREFIX),
                "row {i}: bar prefix missing: {text:?}",
            );
        }
    }

    #[test]
    fn render_focus_label_appears_in_header() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, Some("the build error"), "body");
        let lines = block.render(&ctx_at(80, &theme));
        let text = header_text(&lines[0]);
        assert!(
            text.contains("(focus: the build error)"),
            "header: {text:?}"
        );
    }

    #[test]
    fn render_wrapped_header_keeps_bar_accent() {
        let theme = Theme::default();
        let block = CompactedBlock::new(42, Some("focus on a long failing build transcript"), "");
        let lines = block.render(&ctx_at(24, &theme));

        assert!(lines.len() > 1, "header should wrap: {lines:?}");
        for (i, line) in lines.iter().enumerate().skip(1) {
            let bar = &line.spans[0];
            assert_eq!(bar.content.as_ref(), "▎", "row {i}: {line:?}");
            assert_eq!(bar.style, theme.accent(), "row {i}: {line:?}");
        }
    }
}
