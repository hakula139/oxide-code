//! Default tool-result body: truncated text, with a `... +N lines` footer on overflow.

use ratatui::text::Line;

use super::{
    MAX_ROW_BYTES, MAX_TOOL_OUTPUT_LINES, RenderCtx, body_row, border_style_for, truncate_to_bytes,
};
use crate::tui::wrap::expand_tabs;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    content: &str,
    label: &str,
    is_error: bool,
) {
    let lines: Vec<&str> = content.lines().collect();
    let has_text = |line: &&str| !line.trim().is_empty();
    let (Some(first), Some(last)) = (
        lines.iter().position(has_text),
        lines.iter().rposition(has_text),
    ) else {
        return;
    };
    let mut output_lines = &lines[first..=last];
    if output_lines
        .first()
        .is_some_and(|l| l.trim() == label.trim())
    {
        output_lines = &output_lines[1..];
    }
    if output_lines.is_empty() {
        return;
    }

    let border_style = border_style_for(ctx.theme, is_error);
    let text_style = ctx.theme.dim();
    let truncated = output_lines.len() > MAX_TOOL_OUTPUT_LINES;
    let visible = if truncated {
        &output_lines[..MAX_TOOL_OUTPUT_LINES]
    } else {
        output_lines
    };

    for text_line in visible {
        let expanded = expand_tabs(text_line);
        let display_text = truncate_to_bytes(&expanded, MAX_ROW_BYTES);
        body_row(out, ctx, border_style, display_text, text_style);
    }

    if truncated {
        let n = output_lines.len() - MAX_TOOL_OUTPUT_LINES;
        let label = if n == 1 { "line" } else { "lines" };
        body_row(
            out,
            ctx,
            border_style,
            format!("... +{n} {label}"),
            ctx.theme.dim(),
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Span;

    use super::*;
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    /// Rows of `content` under `label`, wide enough that no row wraps.
    fn render_rows(content: &str, label: &str) -> Vec<String> {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 4096,
            theme: &theme,
            show_thinking: false,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, content, label, false);
        out.iter().map(ToString::to_string).collect()
    }

    fn body(texts: &[&str]) -> Vec<String> {
        texts
            .iter()
            .map(|text| format!("{TOOL_BORDER_CONT}{text}"))
            .collect()
    }

    // ── render ──

    #[test]
    fn render_caps_rows_and_counts_the_hidden_ones() {
        let shown: Vec<String> = (0..MAX_TOOL_OUTPUT_LINES)
            .map(|i| format!("line {i}"))
            .collect();
        for (extra, footer) in [
            (0, None),
            (1, Some("... +1 line")),
            (5, Some("... +5 lines")),
        ] {
            let content = (0..MAX_TOOL_OUTPUT_LINES + extra)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");

            let rows = render_rows(&content, "result");

            let mut expected: Vec<&str> = shown.iter().map(String::as_str).collect();
            expected.extend(footer);
            assert_eq!(rows, body(&expected), "{extra} extra lines");
        }
    }

    #[test]
    fn render_blank_content_emits_nothing() {
        for content in ["", "  \n  "] {
            assert_eq!(render_rows(content, "result"), body(&[]), "{content:?}");
        }
    }

    #[test]
    fn render_drops_a_first_line_that_repeats_the_label() {
        let cases: [(&str, &[&str]); 4] = [
            ("Found 2 files\na.rs\nb.rs", &["a.rs", "b.rs"]),
            ("  Found 2 files  \na.rs", &["a.rs"]),
            (
                "Found 2 files in cache\na.rs",
                &["Found 2 files in cache", "a.rs"],
            ),
            ("Found 2 files", &[]),
        ];
        for (content, texts) in cases {
            assert_eq!(
                render_rows(content, "Found 2 files"),
                body(texts),
                "{content:?}"
            );
        }
    }

    #[test]
    fn render_trims_blank_edge_lines_and_keeps_indents() {
        let cases: [(&str, &[&str]); 3] = [
            ("\n\n real line\n\n\n", &[" real line"]),
            (" a.rs | 1 +\n b.rs | 2 +", &[" a.rs | 1 +", " b.rs | 2 +"]),
            ("first\n\nlast", &["first", "", "last"]),
        ];
        for (content, texts) in cases {
            assert_eq!(render_rows(content, "out"), body(texts), "{content:?}");
        }
    }

    #[test]
    fn render_truncates_a_long_line_to_the_byte_budget() {
        let long_line = "x".repeat(MAX_ROW_BYTES + 100);

        let rows = render_rows(&long_line, "result");

        let kept = format!("{}...", "x".repeat(MAX_ROW_BYTES));
        assert_eq!(rows, body(&[&kept]));
    }

    #[test]
    fn render_error_paints_the_bar_in_the_error_color() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: false,
        };
        let mut out = Vec::new();

        render(&mut out, &ctx, "boom", "bash", true);

        assert_eq!(
            out,
            [Line::from(vec![
                Span::styled(TOOL_BORDER_CONT, theme.error()),
                Span::styled("boom", theme.dim()),
            ])]
        );
    }
}
