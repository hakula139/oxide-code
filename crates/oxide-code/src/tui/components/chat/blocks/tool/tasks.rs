//! `tasks` body: the list the call sent, one marked row per task, windowed like the task panel in
//! the compact view.

use ratatui::style::Style;

use super::{MAX_TOOL_OUTPUT_LINES, overflow_footer};
use crate::tool::tasks::Task;
use crate::tui::components::chat::blocks::rows::body_row;
use crate::tui::components::chat::blocks::{BlockRows, RenderCtx};
use crate::tui::components::tasks::{row_text, window};

pub(super) fn render(
    out: &mut BlockRows,
    ctx: &RenderCtx<'_>,
    border_style: Style,
    tasks: &[Task],
) {
    let shown = window(tasks, ctx.cap(MAX_TOOL_OUTPUT_LINES));
    for task in &tasks[shown.clone()] {
        body_row(
            &mut out.lines,
            ctx,
            border_style,
            ctx.cap_row(&row_text(task)),
            ctx.theme.task_style(task.status),
        );
    }
    let hidden = tasks.len() - shown.len();
    overflow_footer(out, ctx, border_style, hidden, "task", "tasks", false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::tasks::TaskStatus;
    use crate::tool::tasks::testing::{plan, task};
    use crate::tui::glyphs::TOOL_BORDER_CONT;
    use crate::tui::theme::Theme;

    mod render {
        use super::*;

        /// Each row's text in the compact or expanded view, plus the style of the span after the bar.
        fn rows(tasks: &[Task], expanded: bool) -> Vec<(String, Style)> {
            let theme = Theme::default();
            let ctx = RenderCtx {
                width: 80,
                theme: &theme,
                show_thinking: false,
                expanded,
                expand_hint: None,
            };
            let mut out = BlockRows::default();
            render(&mut out, &ctx, theme.tool_border(), tasks);
            out.lines
                .iter()
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

        #[test]
        fn marks_and_styles_each_task_under_the_bar() {
            let theme = Theme::default();

            assert_eq!(
                rows(&plan(), false),
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
        fn counts_the_tasks_outside_the_window_until_expanded() {
            let tasks: Vec<Task> = (1..=8)
                .map(|n| task(&format!("Step {n}"), TaskStatus::Completed))
                .collect();
            let texts = |expanded| -> Vec<String> {
                rows(&tasks, expanded)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect()
            };

            let compact = texts(false);
            let expanded = texts(true);

            assert_eq!(
                compact,
                [
                    format!("{TOOL_BORDER_CONT}✓ Step 4"),
                    format!("{TOOL_BORDER_CONT}✓ Step 5"),
                    format!("{TOOL_BORDER_CONT}✓ Step 6"),
                    format!("{TOOL_BORDER_CONT}✓ Step 7"),
                    format!("{TOOL_BORDER_CONT}✓ Step 8"),
                    format!("{TOOL_BORDER_CONT}... +3 tasks"),
                ],
            );
            assert_eq!(
                expanded,
                (1..=8)
                    .map(|n| format!("{TOOL_BORDER_CONT}✓ Step {n}"))
                    .collect::<Vec<_>>(),
            );
        }
    }
}
