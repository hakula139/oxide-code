//! Task panel: the session's task list, shown above the input while it holds any task.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::LEFT_MARGIN;
use crate::tool::tasks::{Progress, Task, TaskStatus};
use crate::tui::glyphs::task_marker;
use crate::tui::saturating_u16;
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

/// Blank rows that set the panel apart from the chat above it.
const GAP_ROWS: u16 = 1;

/// Task rows the panel shows at most, the last one summarizing the tasks a longer list hides.
const MAX_ROWS: usize = 6;

#[derive(Debug, Default)]
pub(crate) struct TaskPanel {
    tasks: Vec<Task>,
}

impl TaskPanel {
    pub(crate) fn set(&mut self, tasks: Vec<Task>) {
        self.tasks = tasks;
    }

    pub(crate) fn clear(&mut self) {
        self.tasks.clear();
    }

    pub(crate) fn progress(&self) -> Option<Progress> {
        Progress::of(&self.tasks)
    }

    #[cfg(test)]
    pub(crate) fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// The gap plus a row per task up to the cap, or 0 for an empty list.
    pub(crate) fn height(&self) -> u16 {
        if self.tasks.is_empty() {
            return 0;
        }
        let rows = saturating_u16(self.tasks.len().min(MAX_ROWS));
        GAP_ROWS + rows
    }

    /// Fills `area` with tasks below the gap, giving its last row to a summary of the rest when
    /// they don't fit.
    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let gap = self.gap(area.height);
        let area = Rect {
            y: area.y + gap,
            height: area.height - gap,
            ..area
        };
        let rows = usize::from(area.height);
        let shown = if self.tasks.len() > rows {
            window(&self.tasks, rows.saturating_sub(1))
        } else {
            0..self.tasks.len()
        };
        let width = usize::from(area.width).saturating_sub(LEFT_MARGIN.width());
        let mut lines: Vec<Line<'static>> = self.tasks[shown.clone()]
            .iter()
            .map(|task| {
                Line::from(vec![
                    Span::raw(LEFT_MARGIN),
                    Span::styled(
                        truncate_to_width(&row_text(task), width),
                        theme.task_style(task.status),
                    ),
                ])
            })
            .collect();
        if let Some(summary) = hidden_summary(&self.tasks, &shown) {
            lines.push(Line::from(Span::styled(
                truncate_to_width(&format!("{LEFT_MARGIN}{summary}"), usize::from(area.width)),
                theme.dim(),
            )));
        }
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    /// The gap above a panel `height` rows tall. It stays while the rows below fit the whole list,
    /// or one task and the summary row when the list is longer, so it never displaces every task.
    fn gap(&self, height: u16) -> u16 {
        let below = usize::from(height.saturating_sub(GAP_ROWS));
        if self.tasks.len() <= below || below >= 2 {
            GAP_ROWS
        } else {
            0
        }
    }
}

/// A task's marker and content.
pub(super) fn row_text(task: &Task) -> String {
    format!("{} {}", task_marker(task.status), task.content)
}

/// The tasks a view of at most `rows` rows shows: from the one before the task in progress, or
/// else before the first unfinished task, so the step just finished stays beside the work ahead.
/// A single row shows that task alone.
pub(super) fn window(tasks: &[Task], rows: usize) -> Range<usize> {
    if tasks.len() <= rows {
        return 0..tasks.len();
    }
    let current = tasks
        .iter()
        .position(|task| task.status == TaskStatus::InProgress)
        .or_else(|| {
            tasks
                .iter()
                .position(|task| task.status != TaskStatus::Completed)
        })
        .unwrap_or(tasks.len());
    let start = current
        .saturating_sub(usize::from(rows > 1))
        .min(tasks.len() - rows);
    start..start + rows
}

/// The tasks outside `shown` counted by status, such as `+1 pending, 2 completed`. `None` when
/// every task shows.
pub(super) fn hidden_summary(tasks: &[Task], shown: &Range<usize>) -> Option<String> {
    let hidden: Vec<&Task> = tasks[..shown.start]
        .iter()
        .chain(&tasks[shown.end..])
        .collect();
    if hidden.is_empty() {
        return None;
    }
    let counts: Vec<String> = [
        (TaskStatus::InProgress, "in progress"),
        (TaskStatus::Pending, "pending"),
        (TaskStatus::Completed, "completed"),
    ]
    .into_iter()
    .filter_map(|(status, label)| {
        let n = hidden.iter().filter(|task| task.status == status).count();
        (n > 0).then(|| format!("{n} {label}"))
    })
    .collect();
    Some(format!("+{}", counts.join(", ")))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    use super::*;
    use crate::tool::tasks::testing::{plan, task};

    fn numbered(statuses: &[TaskStatus]) -> Vec<Task> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, &status)| task(&format!("Step {}", i + 1), status))
            .collect()
    }

    /// Three done, one in progress, four pending.
    fn long_plan() -> Vec<Task> {
        numbered(&[
            TaskStatus::Completed,
            TaskStatus::Completed,
            TaskStatus::Completed,
            TaskStatus::InProgress,
            TaskStatus::Pending,
            TaskStatus::Pending,
            TaskStatus::Pending,
            TaskStatus::Pending,
        ])
    }

    mod height {
        use super::*;

        #[test]
        fn counts_the_gap_and_each_task_up_to_the_row_cap() {
            let mut panel = TaskPanel::default();
            assert_eq!(panel.height(), 0);

            panel.set(plan());
            assert_eq!(panel.height(), 4);

            panel.set(long_plan());
            assert_eq!(panel.height(), 7);
        }
    }

    mod render {
        use super::*;

        /// Each row's text, plus the color of the cell where its content starts.
        fn render_rows(panel: &TaskPanel, width: u16, height: u16) -> Vec<(String, Color)> {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| panel.render(frame, frame.area(), &Theme::default()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..height)
                .map(|y| {
                    let text = (0..width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned();
                    (text, buffer[(2, y)].fg)
                })
                .collect()
        }

        fn texts(rows: Vec<(String, Color)>) -> Vec<String> {
            rows.into_iter().map(|(text, _)| text).collect()
        }

        #[test]
        fn marks_and_colors_each_task_by_status() {
            let theme = Theme::default();
            let color = |status| theme.task_style(status).fg.unwrap();
            let mut panel = TaskPanel::default();
            panel.set(plan());

            let rows = render_rows(&panel, 40, panel.height());

            assert_eq!(rows[0].0, "");
            assert_eq!(
                rows[1..],
                [
                    (
                        "  ✓ Read the config loader".to_owned(),
                        color(TaskStatus::Completed),
                    ),
                    (
                        "  ● Add the entry type".to_owned(),
                        color(TaskStatus::InProgress),
                    ),
                    ("  ○ Write the tests".to_owned(), color(TaskStatus::Pending)),
                ],
            );
        }

        #[test]
        fn summarizes_the_tasks_beyond_the_window_and_truncates_long_ones() {
            let mut tasks = long_plan();
            tasks[4].content = "Step 5 needs a very long row to describe it".to_owned();
            let mut panel = TaskPanel::default();
            panel.set(tasks);

            assert_eq!(
                texts(render_rows(&panel, 26, panel.height())),
                [
                    "",
                    "  ✓ Step 3",
                    "  ● Step 4",
                    "  ○ Step 5 needs a very...",
                    "  ○ Step 6",
                    "  ○ Step 7",
                    "  +1 pending, 2 completed",
                ],
            );
        }

        #[test]
        fn keeps_the_gap_while_a_task_and_the_summary_fit_under_it() {
            let mut panel = TaskPanel::default();
            panel.set(long_plan());

            assert_eq!(
                texts(render_rows(&panel, 40, 3)),
                ["", "  ● Step 4", "  +4 pending, 3 completed"],
            );
        }

        #[test]
        fn drops_the_gap_in_two_rows_unless_the_whole_list_fits_under_it() {
            let mut panel = TaskPanel::default();
            panel.set(long_plan());
            assert_eq!(
                texts(render_rows(&panel, 40, 2)),
                ["  ● Step 4", "  +4 pending, 3 completed"],
            );

            panel.set(vec![task("Add the entry type", TaskStatus::InProgress)]);
            assert_eq!(
                texts(render_rows(&panel, 40, 2)),
                ["", "  ● Add the entry type"],
            );
        }

        #[test]
        fn drops_the_gap_when_the_area_holds_one_row() {
            let mut panel = TaskPanel::default();
            panel.set(long_plan());

            assert_eq!(
                texts(render_rows(&panel, 40, 1)),
                ["  +1 in progress, 4 pending, 3 completed"],
            );
        }
    }

    mod window {
        use super::*;

        #[test]
        fn keeps_the_last_finished_task_before_the_next_one() {
            let tasks = numbered(&[
                TaskStatus::Completed,
                TaskStatus::Completed,
                TaskStatus::Pending,
                TaskStatus::Pending,
                TaskStatus::Pending,
            ]);

            assert_eq!(window(&tasks, 3), 1..4);
            assert_eq!(window(&tasks, 5), 0..5);
        }

        #[test]
        fn follows_a_task_in_progress_past_unfinished_ones() {
            let tasks = numbered(&[
                TaskStatus::Pending,
                TaskStatus::Pending,
                TaskStatus::Pending,
                TaskStatus::Pending,
                TaskStatus::Pending,
                TaskStatus::InProgress,
                TaskStatus::Pending,
            ]);

            assert_eq!(window(&tasks, 3), 4..7);
            assert_eq!(window(&tasks, 1), 5..6);
        }

        #[test]
        fn stays_inside_the_list_at_either_end() {
            let fresh = numbered(&[TaskStatus::Pending; 4]);
            assert_eq!(window(&fresh, 2), 0..2);

            let nearly_done = numbered(&[
                TaskStatus::Completed,
                TaskStatus::Completed,
                TaskStatus::Completed,
                TaskStatus::InProgress,
            ]);
            assert_eq!(window(&nearly_done, 2), 2..4);
        }
    }

    mod hidden_summary {
        use super::*;

        #[test]
        fn counts_hidden_tasks_by_status_and_gives_no_summary_when_all_show() {
            assert_eq!(
                hidden_summary(&plan(), &(0..0)).as_deref(),
                Some("+1 in progress, 1 pending, 1 completed"),
            );
            assert_eq!(hidden_summary(&plan(), &(0..3)), None);
        }
    }
}
