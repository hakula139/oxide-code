//! Compaction boundary block, and the summary streaming in before it.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{ChatBlock, RenderCtx, bar_continuation_prefix, prepend_markdown_prefix};
use crate::tui::glyphs::TOOL_BORDER_PREFIX;
use crate::tui::markdown::render_markdown;
use crate::tui::wrap::wrap_line;

/// Header over a summary still streaming in.
const STREAMING_HEADER: &str = "Summarizing the conversation...";

/// Boundary marker where a manual or automatic compaction replaced the earlier messages.
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
                format!("Compacted {pre_count} message{plural} → 1 summary (focus: {focus})")
            }
            None => format!("Compacted {pre_count} message{plural} → 1 summary"),
        };
        Self {
            header,
            summary: summary.into(),
        }
    }

    /// Renders the summary of a compaction in flight, laid out like the boundary it becomes.
    pub(crate) fn render_streaming(summary: &str, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        render_boundary(STREAMING_HEADER, summary, ctx)
    }
}

impl ChatBlock for CompactedBlock {
    fn render(&self, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
        render_boundary(&self.header, &self.summary, ctx)
    }
}

fn render_boundary(header: &str, summary: &str, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
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
        Span::styled(header.to_owned(), header_style),
    ]);
    out.extend(wrap_line(header_line, width, bar_width, Some(&cont_prefix)));

    if !summary.trim().is_empty() {
        // Bar-only gutter row separates header from body, mirroring AssistantThinking's
        // layout so the boundary reads as one visual unit.
        out.push(Line::from(Span::styled(
            TOOL_BORDER_PREFIX.to_owned(),
            bar_style,
        )));
        let rendered = render_markdown(summary, ctx.theme, md_width);
        for line in rendered.lines {
            let mut prefixed = prepend_markdown_prefix(line, TOOL_BORDER_PREFIX, bar_style);
            // Plain code fences color the row via `Line::style`, which any span color overrides.
            if prefixed.style.fg.is_none() {
                for span in &mut prefixed.spans[1..] {
                    if span.style.fg.is_none() {
                        span.style = span.style.patch(body_style);
                    }
                }
            }
            out.push(prefixed);
        }
    }

    out
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

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    // ── CompactedBlock::new ──

    #[test]
    fn header_pluralizes_message_count() {
        let one = CompactedBlock::new(1, None, "x");
        let many = CompactedBlock::new(7, None, "x");

        assert_eq!(one.header, "Compacted 1 message → 1 summary");
        assert_eq!(many.header, "Compacted 7 messages → 1 summary");
    }

    #[test]
    fn header_includes_focus_when_instructions_present() {
        let block = CompactedBlock::new(4, Some("focus on auth"), "x");

        assert_eq!(
            block.header,
            "Compacted 4 messages → 1 summary (focus: focus on auth)"
        );
    }

    // ── CompactedBlock::render ──

    #[test]
    fn render_first_line_is_bold_header_with_bar_prefix() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "summary body");

        let lines = block.render(&ctx_at(60, &theme));

        let bar = theme.accent();
        assert_eq!(
            lines[0],
            Line::from(vec![
                Span::styled(TOOL_BORDER_PREFIX, bar),
                Span::styled(
                    "Compacted 4 messages → 1 summary",
                    bar.add_modifier(Modifier::BOLD),
                ),
            ])
        );
    }

    #[test]
    fn render_inserts_gutter_line_between_header_and_body() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "body line");

        let lines = block.render(&ctx_at(60, &theme));

        assert_eq!(
            rows(&lines),
            ["▎ Compacted 4 messages → 1 summary", "▎ ", "▎ body line"]
        );
    }

    #[test]
    fn render_empty_body_emits_header_only() {
        let theme = Theme::default();
        let block = CompactedBlock::new(4, None, "   \n  ");

        let lines = block.render(&ctx_at(60, &theme));

        assert_eq!(rows(&lines), ["▎ Compacted 4 messages → 1 summary"]);
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

        assert_eq!(
            rows(&lines),
            [
                "▎ Compacted 4 messages → 1 summary",
                "▎ ",
                "▎ - Done: shipped /compact",
                "▎ - Next: tests",
            ]
        );
    }

    #[test]
    fn render_plain_code_fence_keeps_code_color() {
        let theme = Theme::default();
        let block = CompactedBlock::new(
            4,
            None,
            indoc! {"
                ```
                cargo test
                ```"
            },
        );

        let lines = block.render(&ctx_at(60, &theme));

        let code = lines
            .iter()
            .find(|line| line.to_string().contains("cargo test"))
            .expect("code row");
        for span in &code.spans[1..] {
            let painted = code.style.patch(span.style);
            assert_eq!(painted.fg, theme.code().fg, "{code:?}");
        }
    }

    #[test]
    fn render_wrapped_header_keeps_bar_accent() {
        let theme = Theme::default();
        let block = CompactedBlock::new(42, Some("focus on a long failing build transcript"), "");

        let lines = block.render(&ctx_at(24, &theme));

        assert_eq!(
            rows(&lines),
            [
                "▎ Compacted 42 messages ",
                "▎ → 1 summary (focus: ",
                "▎ focus on a long ",
                "▎ failing build ",
                "▎ transcript)",
            ]
        );
        for (i, line) in lines.iter().enumerate().skip(1) {
            let bar = &line.spans[0];
            assert_eq!(bar.content.as_ref(), "▎", "row {i}: {line:?}");
            assert_eq!(bar.style, theme.accent(), "row {i}: {line:?}");
        }
    }
}
