//! Task panel: the session's task list, shown above the input while it holds any task.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tool::tasks::{Progress, Task, TaskStatus};
use crate::tui::glyphs::task_marker;
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

/// Tasks the panel shows before it collapses the rest into `+N more`.
const MAX_VISIBLE: usize = 5;

/// Left margin that lines rows up with the chat blocks above.
const MARGIN: &str = "  ";

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

    /// Rows the panel needs: one per visible task, plus one counting the hidden ones.
    pub(crate) fn height(&self) -> u16 {
        let visible = window(&self.tasks, MAX_VISIBLE).len();
        let overflow = usize::from(self.tasks.len() > visible);
        u16::try_from(visible + overflow).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let shown = window(&self.tasks, MAX_VISIBLE);
        let hidden = self.tasks.len() - shown.len();
        let width = usize::from(area.width).saturating_sub(MARGIN.len());
        let mut lines: Vec<Line<'static>> = self.tasks[shown]
            .iter()
            .map(|task| {
                let row = format!("{} {}", task_marker(task.status), task.content);
                Line::from(vec![
                    Span::raw(MARGIN),
                    Span::styled(
                        truncate_to_width(&row, width),
                        theme.task_style(task.status),
                    ),
                ])
            })
            .collect();
        if hidden > 0 {
            lines.push(Line::from(Span::styled(
                format!("{MARGIN}+{hidden} more"),
                theme.dim(),
            )));
        }
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }
}

/// The tasks a view of at most `rows` rows shows: from the one before the first unfinished task, so
/// the view keeps the step just finished beside the work still ahead.
pub(crate) fn window(tasks: &[Task], rows: usize) -> Range<usize> {
    if tasks.len() <= rows {
        return 0..tasks.len();
    }
    let next = tasks
        .iter()
        .position(|task| task.status != TaskStatus::Completed)
        .unwrap_or(tasks.len());
    let start = next.saturating_sub(1).min(tasks.len() - rows);
    start..start + rows
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tool::tasks::testing::{plan, task};

    fn numbered(statuses: &[TaskStatus]) -> Vec<Task> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, &status)| task(&format!("Step {}", i + 1), status))
            .collect()
    }

    fn render_rows(panel: &TaskPanel, width: u16) -> Vec<String> {
        let height = panel.height();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    // ── TaskPanel::height ──

    #[test]
    fn height_counts_each_task_up_to_the_cap_plus_an_overflow_row() {
        let mut panel = TaskPanel::default();
        assert_eq!(panel.height(), 0);

        panel.set(plan());
        assert_eq!(panel.height(), 3);

        panel.set(numbered(&[TaskStatus::Pending; 8]));
        assert_eq!(panel.height(), 6);
    }

    // ── TaskPanel::render ──

    #[test]
    fn render_marks_each_task_by_status() {
        let mut panel = TaskPanel::default();
        panel.set(plan());

        assert_eq!(
            render_rows(&panel, 40),
            [
                "  ✓ Read the config loader",
                "  ● Add the entry type",
                "  ○ Write the tests",
            ],
        );
    }

    #[test]
    fn render_collapses_tasks_beyond_the_window_and_truncates_long_ones() {
        let mut tasks = numbered(&[
            TaskStatus::Completed,
            TaskStatus::Completed,
            TaskStatus::Completed,
            TaskStatus::InProgress,
            TaskStatus::Pending,
            TaskStatus::Pending,
            TaskStatus::Pending,
            TaskStatus::Pending,
        ]);
        tasks[4].content = "Step 5 has a description too long to fit".to_owned();
        let mut panel = TaskPanel::default();
        panel.set(tasks);

        assert_eq!(
            render_rows(&panel, 24),
            [
                "  ✓ Step 3",
                "  ● Step 4",
                "  ○ Step 5 has a desc...",
                "  ○ Step 6",
                "  ○ Step 7",
                "  +3 more",
            ],
        );
    }

    // ── window ──

    #[test]
    fn window_keeps_the_last_finished_task_before_the_next_one() {
        let tasks = numbered(&[
            TaskStatus::Completed,
            TaskStatus::Completed,
            TaskStatus::InProgress,
            TaskStatus::Pending,
            TaskStatus::Pending,
        ]);

        assert_eq!(window(&tasks, 3), 1..4);
        assert_eq!(window(&tasks, 5), 0..5);
    }

    #[test]
    fn window_stays_inside_the_list_at_either_end() {
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
