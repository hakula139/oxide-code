use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use crate::agent::event::UserAction;
use crate::tui::component::Component;
use crate::tui::glyphs::{USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use crate::tui::theme::Theme;

/// Maximum number of visible content lines before the input stops growing.
const MAX_VISIBLE_LINES: u16 = 6;

/// Placeholder copy keyed off `(enabled, has_queued)`. Visible only
/// while the buffer is empty.
const PLACEHOLDER_IDLE: &str = "Ask anything...";
const PLACEHOLDER_BUSY: &str = "Type to queue a follow-up...";
const PLACEHOLDER_IDLE_QUEUED: &str = "Press Esc to edit queued, Enter to send";

/// Multi-line input area at the bottom of the TUI.
///
/// Wraps [`ratatui_textarea::TextArea`] for multi-line editing with
/// dynamic height. Grows from 1 to [`MAX_VISIBLE_LINES`] as content
/// expands. The placeholder text (visible only when the buffer is
/// empty) is the inline hint surface; mid-turn / interrupt hints
/// live on the status bar so they survive past the first keystroke.
///
/// Key bindings (idle):
///
/// - Enter: submit prompt
/// - Shift+Enter: insert newline
/// - Ctrl+C: arm exit (second press within 1 s exits)
/// - Ctrl+D: quit when the input is empty (POSIX EOF idiom);
///   no-op when the input has content
///
/// Key bindings (busy, i.e. disabled):
///
/// - Ctrl+C: cancel the in-flight turn
/// - Ctrl+D: quit
///
/// Esc routes through [`App::handle_crossterm_event`](super::super::app::App::handle_crossterm_event)
/// because its meaning depends on App-level state (queue, run state).
pub(crate) struct InputArea {
    theme: Theme,
    textarea: TextArea<'static>,
    enabled: bool,
    /// Whether the surrounding [`App`](super::super::app::App) has any
    /// queued prompts pending dispatch — drives the idle placeholder
    /// copy. Set explicitly because the input has no view of
    /// app-level queue state.
    has_queued: bool,
    /// Last render width for visual line count estimation. Updated each
    /// frame by `render()`, used by `height()` on the *next* frame.
    /// `Cell` because `render(&self)` is immutable.
    last_width: Cell<u16>,
    /// Tracked viewport scroll offset (screen line index of the topmost
    /// visible row). Mirrors ratatui-textarea's internal `viewport` which
    /// is not publicly accessible.
    scroll_top: Cell<u16>,
}

impl InputArea {
    pub(crate) fn new(theme: &Theme) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(theme.text());
        textarea.set_placeholder_style(theme.dim());
        textarea.set_wrap_mode(WrapMode::Word);
        textarea.set_block(Block::default());

        let mut input = Self {
            theme: theme.clone(),
            textarea,
            enabled: true,
            has_queued: false,
            last_width: Cell::new(0),
            scroll_top: Cell::new(0),
        };
        input.refresh_placeholder();
        input
    }

    /// Mirrors the parent's queue non-emptiness onto the placeholder
    /// so an empty buffer shows the queue hint instead of the default.
    pub(crate) fn set_has_queued(&mut self, has_queued: bool) {
        if self.has_queued == has_queued {
            return;
        }
        self.has_queued = has_queued;
        self.refresh_placeholder();
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        // Visual styling stays put — the user can keep composing while
        // a turn streams (typed prompts queue), so the input never
        // looks "switched off". Only the placeholder copy reflects the
        // run-state. Mid-turn cues live on the status bar.
        self.refresh_placeholder();
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Current buffer as logical lines. Exposed for cross-module tests
    /// (`tui::app`) so they can pin queue pop-back / `set_text` behavior
    /// without reaching through the private textarea.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.textarea.lines().to_vec()
    }

    /// Replaces the current buffer with `text` and parks the cursor at
    /// its end. Used by the queue pop-back path to surface a queued
    /// prompt for editing.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(text);
        self.scroll_top.set(0);
    }

    /// Returns the height this component needs (content lines + top +
    /// bottom borders).
    pub(crate) fn height(&self) -> u16 {
        let content_lines = self.visual_line_count();
        content_lines.min(MAX_VISIBLE_LINES) + 2
    }
}

impl Component for InputArea {
    fn handle_event(&mut self, event: &Event) -> Option<UserAction> {
        // Ctrl+D follows the POSIX EOF idiom: quit only when the input
        // buffer is empty so a stray press while composing never
        // discards work — applies in both states, since typing while
        // busy queues a follow-up and the buffer can carry content.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) = event
        {
            return if self.is_empty() {
                Some(UserAction::Quit)
            } else {
                None
            };
        }

        // Ctrl+C: cancel mid-turn; arm exit when idle. The arm-vs-exit
        // decision lives in `App::dispatch_user_action` since it owns
        // the [`Status::ExitArmed`] window.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) = event
        {
            return Some(if self.enabled {
                UserAction::ConfirmExit
            } else {
                UserAction::Cancel
            });
        }

        if let Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers,
            ..
        }) = event
        {
            // Native Kitty protocol: terminal reports SHIFT directly.
            // VS Code / Cursor keybinding: Shift+Enter sends \x1b\r (ESC CR),
            // which crossterm parses as Alt+Enter.
            if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
                self.textarea.insert_newline();
                return None;
            }
            return self.submit();
        }

        // Scroll keys while busy belong to the chat view; the textarea
        // would otherwise swallow them for cursor movement and the
        // user would lose the ability to scroll history mid-turn.
        if !self.enabled && Self::is_scroll_key(event) {
            return None;
        }

        // Typing flows through in both states so the user can compose
        // a follow-up that the queue will fire after the current turn.
        self.textarea.input(event.clone());
        None
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        // Border, marker, and textarea styling don't react to the
        // run-state — users compose mid-turn for the queue, so the
        // input always reads as live. The status bar carries the
        // streaming / running-tool cue.
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(self.theme.border_focused())
            .style(self.theme.surface());
        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Reserve the leftmost columns for the prompt marker so the
        // textarea content aligns with chat-history user blocks. The
        // marker only paints the first visible row; subsequent wrapped
        // rows leave the prompt gutter blank (hanging indent).
        let [prompt_area, textarea_area] = Layout::horizontal([
            Constraint::Length(USER_PROMPT_PREFIX_WIDTH),
            Constraint::Min(0),
        ])
        .areas(inner);

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                USER_PROMPT_PREFIX,
                self.theme.user(),
            ))),
            prompt_area,
        );

        frame.render_widget(&self.textarea, textarea_area);

        // Store width for visual line count estimation on the next frame.
        self.last_width.set(textarea_area.width);

        // screen_cursor().row is an absolute screen-line index across
        // all wrapped lines, not viewport-relative. Replicate the
        // scroll logic from ratatui-textarea's `next_scroll_top` to
        // convert to a position within the rendered area. Cursor
        // tracking runs in both run-states so typing into the queue
        // mid-turn updates the visible caret.
        let sc = self.textarea.screen_cursor();
        let cursor_row = to_u16(sc.row);
        let height = textarea_area.height;
        let prev = self.scroll_top.get();
        let top = if cursor_row < prev {
            cursor_row
        } else if height > 0 && prev + height <= cursor_row {
            cursor_row + 1 - height
        } else {
            prev
        };
        self.scroll_top.set(top);

        let cursor_x = textarea_area
            .x
            .saturating_add(to_u16(sc.col))
            .min(textarea_area.right().saturating_sub(1));
        let cursor_y = textarea_area.y + cursor_row - top;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

impl InputArea {
    // ── Render Helpers ──

    /// Picks the placeholder copy for the current `(enabled, has_queued)`
    /// combo. Visible only while the buffer is empty.
    fn refresh_placeholder(&mut self) {
        let text = if !self.enabled {
            PLACEHOLDER_BUSY
        } else if self.has_queued {
            PLACEHOLDER_IDLE_QUEUED
        } else {
            PLACEHOLDER_IDLE
        };
        self.textarea.set_placeholder_text(text);
    }

    // ── Private Helpers ──

    /// Whether `event` is one of the chat-scroll keys reserved for the
    /// surrounding `ChatView` while the input is busy.
    fn is_scroll_key(event: &Event) -> bool {
        matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End,
                ..
            }),
        )
    }

    /// Estimate the number of visual (screen) lines after word-wrap.
    ///
    /// Uses `last_width` from the previous render frame. Falls back to
    /// logical line count when no width is known yet (first frame).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "line count fits in u16 for any practical input"
    )]
    fn visual_line_count(&self) -> u16 {
        let width = self.last_width.get() as usize;
        if width == 0 {
            return (self.textarea.lines().len() as u16).max(1);
        }
        self.textarea
            .lines()
            .iter()
            .map(|line| {
                let w = UnicodeWidthStr::width(line.as_str());
                if w <= width {
                    1u16
                } else {
                    w.div_ceil(width) as u16
                }
            })
            .sum::<u16>()
            .max(1)
    }

    /// Whether the buffer contains only whitespace. Drives the
    /// POSIX-style Ctrl+D EOF gate (and short-circuits empty submits).
    fn is_empty(&self) -> bool {
        self.textarea
            .lines()
            .iter()
            .all(|line| line.trim().is_empty())
    }

    fn submit(&mut self) -> Option<UserAction> {
        if self.is_empty() {
            return None;
        }
        let trimmed = self.textarea.lines().join("\n").trim().to_owned();

        // Clear the textarea and reset scroll state.
        self.textarea.select_all();
        self.textarea.cut();
        self.scroll_top.set(0);

        Some(UserAction::SubmitPrompt(trimmed))
    }
}

/// Lossy `usize → u16` cast scoped to cursor / column positions, where
/// the source value is bounded by terminal dimensions. Centralises the
/// `cast_possible_truncation` lint suppression so the call sites stay
/// readable.
#[expect(
    clippy::cast_possible_truncation,
    reason = "cursor / column positions fit in u16 for terminal widths"
)]
fn to_u16(n: usize) -> u16 {
    n as u16
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;

    use super::*;

    fn test_input() -> InputArea {
        InputArea::new(&Theme::default())
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    // ── set_enabled ──

    #[test]
    fn set_enabled_toggles_state() {
        let mut input = test_input();
        assert!(input.is_enabled());

        input.set_enabled(false);
        assert!(!input.is_enabled());

        input.set_enabled(true);
        assert!(input.is_enabled());
    }

    // ── height ──

    #[test]
    fn height_empty_input_is_three() {
        let input = test_input();
        assert_eq!(input.height(), 3); // 1 content + 2 borders
    }

    #[test]
    fn height_grows_with_content() {
        let mut input = test_input();
        input.textarea.insert_newline();
        input.textarea.insert_newline();
        assert_eq!(input.height(), 5); // 3 content + 2 borders
    }

    #[test]
    fn height_capped_at_max() {
        let mut input = test_input();
        for _ in 0..10 {
            input.textarea.insert_newline();
        }
        assert_eq!(input.height(), MAX_VISIBLE_LINES + 2);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_ctrl_c_idle_arms_exit() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(UserAction::ConfirmExit)));
    }

    #[test]
    fn handle_event_ctrl_d_empty_buffer_quits_in_idle_and_busy() {
        // POSIX EOF idiom: Ctrl+D on an empty buffer exits in both
        // idle and busy states.
        let mut input = test_input();
        let idle_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(idle_action, Some(UserAction::Quit)));

        input.set_enabled(false);
        let busy_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(busy_action, Some(UserAction::Quit)));
    }

    #[test]
    fn handle_event_ctrl_d_with_content_is_a_noop_in_idle_and_busy() {
        // Pressing Ctrl+D mid-prompt must not discard the typed text —
        // matches bash / zsh / Codex behaviour. Applies in both idle
        // and busy states since typing flows into the buffer in both
        // (busy presses queue a follow-up).
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        let idle_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(idle_action.is_none());
        assert_eq!(input.textarea.lines(), vec!["h"]);

        input.set_enabled(false);
        let busy_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(busy_action.is_none());
        assert_eq!(input.textarea.lines(), vec!["h"]);
    }

    #[test]
    fn handle_event_ctrl_c_busy_returns_cancel() {
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(UserAction::Cancel)));
    }

    #[test]
    fn handle_event_disabled_empty_enter_is_silent() {
        // Submit's empty-buffer guard short-circuits, so a stray Enter
        // mid-turn produces no action even though the textarea now
        // accepts typing during busy.
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
    }

    #[test]
    fn handle_event_disabled_typing_lands_in_textarea() {
        // Enables the queue UX: the user composes a follow-up while
        // the spinner is still spinning.
        let mut input = test_input();
        input.set_enabled(false);
        input.handle_event(&key(KeyCode::Char('h'), KeyModifiers::NONE));
        input.handle_event(&key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(input.textarea.lines(), vec!["hi"]);
    }

    #[test]
    fn handle_event_disabled_enter_with_content_submits() {
        let mut input = test_input();
        input.set_enabled(false);
        input.handle_event(&key(KeyCode::Char('q'), KeyModifiers::NONE));
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(UserAction::SubmitPrompt(s)) if s == "q"));
    }

    #[test]
    fn handle_event_disabled_scroll_keys_pass_through() {
        // Arrow / Page keys while busy must reach `ChatView` for scroll;
        // returning `None` lets the parent route them.
        let mut input = test_input();
        input.set_enabled(false);
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert!(input.handle_event(&key(code, KeyModifiers::NONE)).is_none());
        }
    }

    #[test]
    fn handle_event_shift_enter_inserts_newline() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(action.is_none());
        assert_eq!(input.textarea.lines().len(), 2);
    }

    #[test]
    fn handle_event_alt_enter_inserts_newline() {
        // VS Code / Cursor keybinding sends \x1b\r for Shift+Enter,
        // which crossterm parses as ALT+Enter.
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(action.is_none());
        assert_eq!(input.textarea.lines().len(), 2);
    }

    #[test]
    fn handle_event_enter_submits_nonempty() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(UserAction::SubmitPrompt(s)) if s == "hi"));
    }

    // ── render ──

    fn render_to_backend(input: &InputArea, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                input.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
    }

    fn type_text(input: &mut InputArea, text: &str) {
        for ch in text.chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    #[test]
    fn render_empty_shows_placeholder() {
        let input = test_input();
        insta::assert_snapshot!(render_to_backend(&input, 60, 3));
    }

    #[test]
    fn render_with_text_shows_typed_content() {
        let mut input = test_input();
        type_text(&mut input, "hello world");
        insta::assert_snapshot!(render_to_backend(&input, 60, 3));
    }

    #[test]
    fn render_busy_state_keeps_text_styled_normally() {
        // Composing mid-turn (for the queue) must look identical to
        // composing idle — Claude Code does the same. Sample the first
        // textarea cell across both states and pin them to `text`.
        let theme = Theme::default();
        let mut input = InputArea::new(&theme);
        type_text(&mut input, "pending");

        let enabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(USER_PROMPT_PREFIX_WIDTH, 1))
            .unwrap()
            .fg;
        input.set_enabled(false);
        let disabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(USER_PROMPT_PREFIX_WIDTH, 1))
            .unwrap()
            .fg;

        assert_eq!(enabled_fg, theme.text().fg.unwrap());
        assert_eq!(disabled_fg, theme.text().fg.unwrap());
    }

    #[test]
    fn render_prompt_marker_always_uses_user_color() {
        // The chevron stays the user accent across run-states because
        // composing mid-turn is allowed — same intent as the typed
        // text styling above.
        let theme = Theme::default();
        let mut input = InputArea::new(&theme);

        let enabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(0, 1))
            .unwrap()
            .fg;
        input.set_enabled(false);
        let disabled_fg = render_to_backend(&input, 60, 3)
            .buffer()
            .cell(Position::new(0, 1))
            .unwrap()
            .fg;

        assert_eq!(enabled_fg, theme.user().fg.unwrap());
        assert_eq!(disabled_fg, theme.user().fg.unwrap());
    }

    #[test]
    fn render_multiline_grows_textarea_region() {
        let mut input = test_input();
        type_text(&mut input, "line 1");
        input.textarea.insert_newline();
        type_text(&mut input, "line 2");
        input.textarea.insert_newline();
        type_text(&mut input, "line 3");
        insta::assert_snapshot!(render_to_backend(&input, 60, input.height()));
    }

    #[test]
    fn render_long_line_wraps_and_engages_scroll_offset() {
        // Narrow width forces word-wrap; typing past the visible row
        // engages scroll_top so the cursor stays on-screen.
        let mut input = test_input();
        type_text(
            &mut input,
            "a long input that overflows a narrow terminal and forces the textarea to wrap",
        );
        insta::assert_snapshot!(render_to_backend(&input, 30, 5));
    }

    #[test]
    fn render_advances_scroll_top_when_cursor_below_viewport() {
        // After typing N+1 logical lines, the cursor sits at row N
        // while we render with only 3 visible content rows. The
        // scroll-tracking math must advance `scroll_top` so the
        // cursor stays on-screen — pin it to the expected offset.
        let mut input = test_input();
        for _ in 0..7 {
            input.textarea.insert_newline();
        }
        type_text(&mut input, "tail");

        render_to_backend(&input, 60, 5);

        assert_eq!(
            input.scroll_top.get(),
            5,
            "cursor at row 7 + height 3 → scroll_top = 7 + 1 - 3",
        );
    }

    #[test]
    fn render_rewinds_scroll_top_when_cursor_above_viewport() {
        // First render parks `scroll_top` past the start; moving the
        // cursor back to row 0 must rewind it so the cursor doesn't
        // disappear off the top of the viewport.
        let mut input = test_input();
        for _ in 0..7 {
            input.textarea.insert_newline();
        }
        type_text(&mut input, "tail");
        render_to_backend(&input, 60, 5);
        assert!(input.scroll_top.get() > 0);

        for _ in 0..7 {
            input.textarea.input(key(KeyCode::Up, KeyModifiers::NONE));
        }
        render_to_backend(&input, 60, 5);

        assert_eq!(
            input.scroll_top.get(),
            0,
            "cursor row 0 < prev → scroll_top tracks back down to cursor",
        );
    }

    // ── refresh_placeholder ──

    fn placeholder_text(input: &InputArea) -> String {
        input.textarea.placeholder_text().to_owned()
    }

    #[test]
    fn refresh_placeholder_idle_empty_queue_shows_default() {
        let input = test_input();
        assert_eq!(placeholder_text(&input), PLACEHOLDER_IDLE);
    }

    #[test]
    fn refresh_placeholder_busy_shows_queue_follow_up_copy() {
        let mut input = test_input();
        input.set_enabled(false);
        assert_eq!(placeholder_text(&input), PLACEHOLDER_BUSY);
    }

    #[test]
    fn refresh_placeholder_idle_with_queue_shows_edit_hint() {
        let mut input = test_input();
        input.set_has_queued(true);
        assert_eq!(placeholder_text(&input), PLACEHOLDER_IDLE_QUEUED);
    }

    #[test]
    fn refresh_placeholder_busy_takes_precedence_over_queue() {
        // Busy + queue (the user typed a follow-up while a turn is
        // running): the placeholder still encourages queueing rather
        // than the idle-queue copy that suggests editing.
        let mut input = test_input();
        input.set_has_queued(true);
        input.set_enabled(false);
        assert_eq!(placeholder_text(&input), PLACEHOLDER_BUSY);
    }

    // ── visual_line_count ──

    #[test]
    fn visual_line_count_no_width_falls_back_to_logical() {
        let mut input = test_input();
        // last_width is 0 (no render yet), so falls back to logical count.
        assert_eq!(input.visual_line_count(), 1);

        input.textarea.insert_newline();
        assert_eq!(input.visual_line_count(), 2);
    }

    #[test]
    fn visual_line_count_wraps_long_line() {
        let mut input = test_input();
        input.last_width.set(10);
        // Insert a 25-char line: wraps to ceil(25/10) = 3 visual lines.
        for ch in "abcdefghijklmnopqrstuvwxy".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(input.visual_line_count(), 3);
    }

    #[test]
    fn visual_line_count_mixed_logical_and_wrapped() {
        let mut input = test_input();
        input.last_width.set(10);
        // Line 1: 5 chars (fits in 10) -> 1 visual line.
        for ch in "hello".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        input.textarea.insert_newline();
        // Line 2: 15 chars -> ceil(15/10) = 2 visual lines.
        for ch in "abcdefghijklmno".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(input.visual_line_count(), 3);
    }

    #[test]
    fn height_accounts_for_visual_wrapping() {
        let mut input = test_input();
        input.last_width.set(10);
        // Single logical line, 25 chars -> 3 visual lines.
        for ch in "abcdefghijklmnopqrstuvwxy".chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
        // 3 content + 2 borders
        assert_eq!(input.height(), 5);
    }

    // ── submit ──

    #[test]
    fn submit_empty_produces_no_action() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
    }

    #[test]
    fn submit_clears_textarea() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));

        input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(input.textarea.lines(), vec![""]);
    }

    #[test]
    fn submit_trims_whitespace() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));

        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Some(UserAction::SubmitPrompt(s)) if s == "a"));
    }
}
