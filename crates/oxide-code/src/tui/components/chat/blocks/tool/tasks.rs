//! `tasks` body: the list the call sent, one marked row per task, windowed like the task panel.

use ratatui::text::Line;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, border_style_for, bordered_row,
    truncate_to_bytes,
};
use crate::tool::tasks::Task;
use crate::tui::components::tasks::{hidden_summary, row_text, window};

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    tasks: &[Task],
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let shown = window(tasks, MAX_TOOL_OUTPUT_LINES);
    for task in &tasks[shown.clone()] {
        bordered_row::render(
            out,
            ctx,
            border_style,
            truncate_to_bytes(&row_text(task), MAX_TOOL_OUTPUT_LINE_BYTES),
            ctx.theme.task_style(task.status),
        );
    }
    if let Some(summary) = hidden_summary(tasks, &shown) {
        bordered_row::render(
            out,
            ctx,
            border_style,
            format!("... {summary}"),
            ctx.theme.dim(),
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Style;

    use super::*;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::{plan, task};
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    /// Each row's text, plus the style of the span after the bar.
    fn rows(tasks: &[Task]) -> Vec<(String, Style)> {
        let theme = Theme::default();
        let ctx = RenderCtx {
            width: 80,
            theme: &theme,
            show_thinking: false,
        };
        let mut out = Vec::new();
        render(&mut out, &ctx, tasks, false);
        out.iter()
            .map(|line| {
                let text = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>();
                let style = line
                    .spans
                    .last()
                    .map_or_else(Style::default, |span| span.style);
                (text, style)
            })
            .collect()
    }

    // ── render ──

    #[test]
    fn render_marks_and_styles_each_task_under_the_bar() {
        let theme = Theme::default();

        assert_eq!(
            rows(&plan()),
            [
                (
                    format!("{TOOL_BORDER_CONT}✓ Read the config loader"),
                    theme.task_style(TaskStatus::Completed),
                ),
                (
                    format!("{TOOL_BORDER_CONT}● Add the entry type"),
                    theme.task_style(TaskStatus::InProgress),
                ),
                (
                    format!("{TOOL_BORDER_CONT}○ Write the tests"),
                    theme.task_style(TaskStatus::Pending),
                ),
            ],
        );
    }

    #[test]
    fn render_summarizes_the_tasks_outside_the_window_by_status() {
        let tasks: Vec<Task> = (1..=8)
            .map(|n| task(&format!("Step {n}"), TaskStatus::Completed))
            .collect();

        let rendered: Vec<String> = rows(&tasks).into_iter().map(|(text, _)| text).collect();

        assert_eq!(
            rendered,
            [
                format!("{TOOL_BORDER_CONT}✓ Step 4"),
                format!("{TOOL_BORDER_CONT}✓ Step 5"),
                format!("{TOOL_BORDER_CONT}✓ Step 6"),
                format!("{TOOL_BORDER_CONT}✓ Step 7"),
                format!("{TOOL_BORDER_CONT}✓ Step 8"),
                format!("{TOOL_BORDER_CONT}... +3 completed"),
            ],
        );
    }
}
