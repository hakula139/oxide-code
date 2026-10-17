//! `tasks` body: the list the call sent, one marked row per task, windowed like the task panel.

use ratatui::text::Line;

use super::super::RenderCtx;
use super::{
    MAX_TOOL_OUTPUT_LINE_BYTES, MAX_TOOL_OUTPUT_LINES, border_style_for, bordered_row,
    truncate_to_bytes,
};
use crate::tool::tasks::Task;
use crate::tui::components::tasks::window;
use crate::tui::glyphs::task_marker;
use crate::util::text::plural;

pub(super) fn render(
    out: &mut Vec<Line<'static>>,
    ctx: &RenderCtx<'_>,
    tasks: &[Task],
    is_error: bool,
) {
    let border_style = border_style_for(ctx.theme, is_error);
    let shown = window(tasks, MAX_TOOL_OUTPUT_LINES);
    let hidden = tasks.len() - shown.len();
    for task in &tasks[shown] {
        let content = truncate_to_bytes(&task.content, MAX_TOOL_OUTPUT_LINE_BYTES);
        bordered_row::render(
            out,
            ctx,
            border_style,
            format!("{} {content}", task_marker(task.status)),
            ctx.theme.task_style(task.status),
        );
    }
    if hidden > 0 {
        let noun = plural(hidden, "task", "tasks");
        bordered_row::render(
            out,
            ctx,
            border_style,
            format!("... +{hidden} {noun}"),
            ctx.theme.dim(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::{plan, task};
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    fn rows(tasks: &[Task]) -> Vec<String> {
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
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    // ── render ──

    #[test]
    fn render_marks_each_task_under_the_bar() {
        assert_eq!(
            rows(&plan()),
            [
                format!("{TOOL_BORDER_CONT}✓ Read the config loader"),
                format!("{TOOL_BORDER_CONT}● Add the entry type"),
                format!("{TOOL_BORDER_CONT}○ Write the tests"),
            ],
        );
    }

    #[test]
    fn render_counts_the_tasks_outside_the_window() {
        let tasks: Vec<Task> = (1..=7)
            .map(|n| task(&format!("Step {n}"), TaskStatus::Pending))
            .collect();

        let rendered = rows(&tasks);

        assert_eq!(rendered.len(), MAX_TOOL_OUTPUT_LINES + 1);
        assert_eq!(rendered[0], format!("{TOOL_BORDER_CONT}○ Step 1"));
        assert_eq!(
            rendered[MAX_TOOL_OUTPUT_LINES],
            format!("{TOOL_BORDER_CONT}... +2 tasks"),
        );
    }
}
