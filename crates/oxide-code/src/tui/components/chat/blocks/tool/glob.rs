//! `glob` tool body — flat list of cwd-relative paths under a dim
//! `pattern (visible of total)` header. The header keeps the block
//! self-describing once the status line scrolls out of view; the
//! footer combines TUI-side row hiding (`MAX_TOOL_OUTPUT_LINES`) with
//! the tool's own `MAX_RESULTS` cap into one parenthetical so users
//! see a single source of truth for "what's hidden".

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, STATUS_LINE_CONT,
    border_continuation_prefix, border_style_for, truncate_to_bytes,
};
use crate::tui::wrap::wrap_line;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    pattern: &str,
    files: &[String],
    total: usize,
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let width = usize::from(ctx.width);
    let cont_prefix = border_continuation_prefix(STATUS_LINE_CONT, border_style);

    if files.is_empty() {
        // Surface the empty state under the bar so the block doesn't look
        // like a stalled or broken render — every other tool variant emits
        // at least one body row, and the status header alone is easy to
        // miss when the chat is dense. The pattern header is suppressed
        // here — the empty-state row already labels the result.
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled("No files found", ctx.theme.dim()),
        ]));
        return;
    }

    let visible = files.len().min(MAX_TOOL_OUTPUT_LINES);
    let hidden = files.len() - visible;
    let truncated_by_tool = total > files.len();

    let header = format!("{pattern} ({visible} of {total})");
    let header_line = Line::from(vec![
        Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
        Span::styled(header, ctx.theme.dim()),
    ]);
    out.extend(wrap_line(
        header_line,
        width,
        STATUS_LINE_CONT.width(),
        Some(&cont_prefix),
    ));

    for path in &files[..visible] {
        let display = truncate_to_bytes(path, MAX_TOOL_OUTPUT_LINE_BYTES);
        let line = Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(display, ctx.theme.text()),
        ]);
        out.extend(wrap_line(
            line,
            width,
            STATUS_LINE_CONT.width(),
            Some(&cont_prefix),
        ));
    }

    if let Some(text) = footer_text(hidden, total, truncated_by_tool) {
        out.push(Line::from(vec![
            Span::styled(STATUS_LINE_CONT.to_owned(), border_style),
            Span::styled(text, ctx.theme.dim()),
        ]));
    }
}

/// Footer combining TUI-side hidden rows (`hidden`) with the tool's
/// own `MAX_RESULTS` truncation. Reports the total when the tool
/// capped — `total` is the unbounded match count, more useful than a
/// bare "limit reached" since glob can disclose it.
fn footer_text(hidden: usize, total: usize, truncated_by_tool: bool) -> Option<String> {
    let noun = |n: usize| if n == 1 { "file" } else { "files" };
    match (hidden, truncated_by_tool) {
        (0, false) => None,
        (0, true) => Some(format!("... {total} files total")),
        (n, false) => Some(format!("... +{n} {}", noun(n))),
        (n, true) => Some(format!("... +{n} {} of {total} total", noun(n))),
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::theme::Theme;

    use super::*;

    fn collect_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|sp| sp.content.as_ref()))
            .collect::<Vec<_>>()
            .join("|")
    }

    // ── render ──

    #[test]
    fn render_empty_files_shows_no_files_found_row() {
        // Empty result must render an explicit body row so the block has
        // a left bar and the user doesn't mistake "no matches" for a
        // half-rendered or stalled tool call. The pattern header is
        // intentionally suppressed when there's nothing to label.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, "**/*.rs", &[], 0, false);

        assert_eq!(out.len(), 1);
        let body = collect_text(&out);
        assert!(body.contains("No files found"), "body: {body}");
        assert!(!body.contains("**/*.rs"), "no header on empty: {body}");
    }

    #[test]
    fn render_short_list_emits_header_and_no_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let files = vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()];
        render(&mut out, &ctx, "**/*.rs", &files, 2, false);

        assert_eq!(
            out.len(),
            3,
            "expected header + two path rows, no footer: {out:#?}",
        );
        let body = collect_text(&out);
        assert!(body.contains("**/*.rs (2 of 2)"), "header text: {body}");
        assert!(body.contains("src/main.rs"));
        assert!(body.contains("src/lib.rs"));
        assert!(!body.contains("..."));
    }

    #[test]
    fn render_caps_at_max_output_lines_with_hidden_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        let files: Vec<String> = (0..MAX_TOOL_OUTPUT_LINES + 3)
            .map(|i| format!("f{i}.rs"))
            .collect();
        render(&mut out, &ctx, "**/*.rs", &files, files.len(), false);

        // Header + MAX_TOOL_OUTPUT_LINES body rows + one footer row.
        assert_eq!(out.len(), MAX_TOOL_OUTPUT_LINES + 2);
        let body = collect_text(&out);
        assert!(
            body.contains(&format!(
                "**/*.rs ({MAX_TOOL_OUTPUT_LINES} of {})",
                MAX_TOOL_OUTPUT_LINES + 3
            )),
            "header should report visible / total: {body}",
        );
        for i in 0..MAX_TOOL_OUTPUT_LINES {
            assert!(body.contains(&format!("f{i}.rs")), "row {i} hidden: {body}");
        }
        assert!(
            !body.contains(&format!("f{MAX_TOOL_OUTPUT_LINES}.rs")),
            "row past cap should be hidden: {body}",
        );
        assert!(body.contains("... +3 files"), "footer text: {body}");
    }

    #[test]
    fn render_error_flag_swaps_border_style() {
        // Pins that `is_error` flows into `border_style_for` rather than
        // being dropped on the floor — a regression here would render
        // failed glob calls with the success-color bar.
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let files = vec!["a.rs".to_owned()];

        let mut ok = Vec::new();
        render(&mut ok, &ctx, "**/*.rs", &files, 1, false);
        let mut err = Vec::new();
        render(&mut err, &ctx, "**/*.rs", &files, 1, true);

        assert_eq!(ok.len(), err.len());
        assert_ne!(
            ok[0].spans[0].style, err[0].spans[0].style,
            "is_error should swap the bar style",
        );
    }

    #[test]
    fn render_tool_truncation_surfaces_total_in_footer() {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: true,
        };
        let mut out = Vec::new();
        // Simulate the tool returning MAX_TOOL_OUTPUT_LINES + 5 entries
        // out of a wider universe of 1234 — the renderer should report
        // both how many returned rows it hid AND the unbounded total.
        let returned = MAX_TOOL_OUTPUT_LINES + 5;
        let files: Vec<String> = (0..returned).map(|i| format!("f{i}.rs")).collect();
        render(&mut out, &ctx, "**/*.rs", &files, 1234, false);

        let body = collect_text(&out);
        assert!(
            body.contains(&format!("**/*.rs ({MAX_TOOL_OUTPUT_LINES} of 1234)")),
            "header should anchor the body to the input pattern: {body}",
        );
        assert!(
            body.contains("... +5 files of 1234 total"),
            "footer: {body}"
        );
    }

    // ── footer_text ──

    #[test]
    fn footer_text_no_hidden_no_truncation_returns_none() {
        assert_eq!(footer_text(0, 5, false), None);
    }

    #[test]
    fn footer_text_hidden_uses_singular_or_plural() {
        assert_eq!(footer_text(1, 6, false), Some("... +1 file".to_owned()));
        assert_eq!(footer_text(3, 8, false), Some("... +3 files".to_owned()));
    }

    #[test]
    fn footer_text_tool_truncated_with_no_tui_hidden_reports_total() {
        // Exotic shape — tool cap hit but TUI fits everything. Defensive
        // arm; in practice MAX_RESULTS (100) is well above MAX_TOOL_OUTPUT_LINES (5).
        assert_eq!(
            footer_text(0, 200, true),
            Some("... 200 files total".to_owned()),
        );
    }

    #[test]
    fn footer_text_combines_hidden_and_total_when_tool_truncated() {
        assert_eq!(
            footer_text(95, 1234, true),
            Some("... +95 files of 1234 total".to_owned()),
        );
    }
}
