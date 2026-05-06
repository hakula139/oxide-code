//! Multi-line input area with slash-command autocomplete.

mod popup;

use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::UnicodeWidthStr;

use self::popup::{PopupMode, SlashPopup};
use crate::agent::event::UserAction;
use crate::slash::popup_state;
use crate::tui::glyphs::{USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use crate::tui::theme::Theme;

// ── Constants ──

/// Maximum number of visible content lines before the input stops growing.
const MAX_VISIBLE_LINES: u16 = 6;

enum PopupKey {
    Action(UserAction),
    Consumed,
    Pass,
}

const PLACEHOLDER_IDLE: &str = "Ask anything...";
const PLACEHOLDER_BUSY: &str = "Type to queue a follow-up...";
const PLACEHOLDER_IDLE_QUEUED: &str = "Esc edits last queued · Enter adds another";

// ── InputArea ──

/// Multi-line input area with dynamic height and slash-command popup.
pub(crate) struct InputArea {
    theme: Theme,
    textarea: TextArea<'static>,
    popup: SlashPopup,
    enabled: bool,
    has_queued: bool,
    /// `Cell` because `render(&self)` is immutable.
    last_width: Cell<u16>,
    scroll_top: Cell<u16>,
}

// ── Public API ──

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
            popup: SlashPopup::new(theme),
            last_width: Cell::new(0),
            scroll_top: Cell::new(0),
        };
        input.refresh_placeholder();
        input
    }

    /// Re-skin subsequent renders. The textarea keeps a cached `Style`, so the previous theme's
    /// text / placeholder colors would persist if not reapplied here.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.textarea.set_style(theme.text());
        self.textarea.set_placeholder_style(theme.dim());
        self.popup.set_theme(theme);
    }

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
        self.refresh_placeholder();
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.textarea.lines().to_vec()
    }

    /// Replaces the buffer with `text` and parks the cursor at its end.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.textarea.select_all();
        self.textarea.cut();
        self.textarea.insert_str(text);
        self.scroll_top.set(0);
    }

    pub(crate) fn height(&self) -> u16 {
        let content_lines = self.visual_line_count();
        content_lines.min(MAX_VISIBLE_LINES) + 2
    }

    pub(crate) fn popup_visible(&self) -> bool {
        self.popup.is_visible()
    }

    pub(crate) fn popup_height(&self) -> u16 {
        self.popup.height()
    }

    pub(crate) fn render_popup(&self, frame: &mut Frame, area: Rect) {
        self.popup.render(frame, area);
    }
}

// ── Event Handling & Rendering ──

impl InputArea {
    pub(crate) fn handle_event(&mut self, event: &Event) -> Option<UserAction> {
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) = event
        {
            return if self.enabled && self.is_empty() {
                Some(UserAction::Quit)
            } else {
                None
            };
        }

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

        if self.popup.is_visible() {
            match self.handle_popup_key(event) {
                PopupKey::Action(action) => return Some(action),
                PopupKey::Consumed => return None,
                PopupKey::Pass => {}
            }
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
                self.refresh_popup();
                return None;
            }
            return self.submit();
        }

        // Scroll keys while busy belong to the chat view.
        if !self.enabled && Self::is_scroll_key(event) {
            return None;
        }

        self.textarea.input(event.clone());
        self.refresh_popup();
        None
    }

    pub(crate) fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP | Borders::BOTTOM)
            .border_style(self.theme.border_focused())
            .style(self.theme.surface());
        let inner = block.inner(area);
        frame.render_widget(block, area);

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

        self.last_width.set(textarea_area.width);

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

// ── Private Helpers ──

impl InputArea {
    // ── Render Helpers ──

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

    // ── Popup & State ──

    fn handle_popup_key(&mut self, event: &Event) -> PopupKey {
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        else {
            return PopupKey::Pass;
        };
        match code {
            KeyCode::Up => {
                self.popup.select_prev();
                PopupKey::Consumed
            }
            KeyCode::Down => {
                self.popup.select_next();
                PopupKey::Consumed
            }
            KeyCode::Esc => {
                self.popup.set_state(None);
                PopupKey::Consumed
            }
            KeyCode::Tab if modifiers.is_empty() => {
                self.popup_complete_to_buffer();
                PopupKey::Consumed
            }
            KeyCode::Enter if modifiers.is_empty() => match self.popup_submit_selected() {
                Some(action) => PopupKey::Action(action),
                None => PopupKey::Consumed,
            },
            _ => PopupKey::Pass,
        }
    }

    fn popup_complete_to_buffer(&mut self) {
        let Some(replacement) = self.popup_completion_text() else {
            return;
        };
        self.set_text(&replacement);
        self.popup.set_state(None);
    }

    /// Mode-aware Tab insertion. Name mode replaces the buffer with `/{name} `; arg mode keeps
    /// the typed `/cmd ` and substitutes the prefix with the picked value plus a trailing space.
    fn popup_completion_text(&self) -> Option<String> {
        let row = self.popup.selected()?;
        Some(match self.popup.mode()? {
            PopupMode::Name => format!("/{} ", row.value),
            PopupMode::Arg { cmd } => format!("/{cmd} {} ", row.value),
        })
    }

    fn popup_submit_selected(&mut self) -> Option<UserAction> {
        let row = self.popup.selected()?;
        let submission = match self.popup.mode()? {
            PopupMode::Name => format!("/{}", row.value),
            PopupMode::Arg { cmd } => format!("/{cmd} {}", row.value),
        };
        self.textarea.select_all();
        self.textarea.cut();
        self.scroll_top.set(0);
        self.popup.set_state(None);
        Some(UserAction::SubmitPrompt(submission))
    }

    fn refresh_popup(&mut self) {
        // Only the single-line case can be a slash command; multi-line buffers close the popup.
        let state = match self.textarea.lines() {
            [single] => popup_state(single),
            _ => None,
        };
        self.popup.set_state(state.as_ref());
    }

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

    pub(crate) fn is_empty(&self) -> bool {
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

        self.textarea.select_all();
        self.textarea.cut();
        self.scroll_top.set(0);

        Some(UserAction::SubmitPrompt(trimmed))
    }
}

// ── Free Functions ──

/// Lossy `usize → u16` for cursor / column positions, bounded by terminal dimensions.
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
    fn handle_event_ctrl_d_empty_buffer_quits_only_when_idle() {
        // POSIX EOF idiom: idle Ctrl+D on an empty buffer exits.
        let mut input = test_input();
        let idle_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(idle_action, Some(UserAction::Quit)));

        // Busy Ctrl+D is a no-op even with an empty buffer — a habitual
        // EOF press shouldn't tear down an in-flight turn.
        input.set_enabled(false);
        let busy_action = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(busy_action.is_none());
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
    fn handle_event_ctrl_c_busy_triggers_cancel() {
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(action, Some(UserAction::Cancel)));
    }

    #[test]
    fn handle_event_disabled_empty_enter_is_silent() {
        // Submit's empty-buffer guard short-circuits, so a stray Enter
        // mid-turn produces no action even though the textarea now accepts typing during busy.
        let mut input = test_input();
        input.set_enabled(false);
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
    }

    #[test]
    fn handle_event_disabled_typing_lands_in_textarea() {
        // Enables the queue UX: the user composes a follow-up while the spinner is still spinning.
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
        // composing mid-turn is allowed — same intent as the typed text styling above.
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
    fn submit_empty_produces_no_action() {
        let mut input = test_input();
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(action.is_none());
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

    // ── popup routing ──

    /// Drive the input into a popup-visible state by typing a `/`,
    /// so popup-key tests start from a known fixture.
    fn input_with_popup() -> InputArea {
        let mut input = test_input();
        input.handle_event(&key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(input.popup_visible(), "typing `/` opens the popup");
        input
    }

    fn selected_value(input: &InputArea) -> String {
        input.popup.selected().unwrap().value.clone().into_owned()
    }

    #[test]
    fn handle_event_popup_down_advances_selection() {
        let mut input = input_with_popup();
        let initial = selected_value(&input);
        let action = input.handle_event(&key(KeyCode::Down, KeyModifiers::NONE));
        assert!(action.is_none(), "Down is consumed silently");
        let after = selected_value(&input);
        assert_ne!(initial, after, "Down moves to a different command");
    }

    #[test]
    fn handle_event_popup_up_reverses_selection() {
        let mut input = input_with_popup();
        input.handle_event(&key(KeyCode::Down, KeyModifiers::NONE));
        let after_down = selected_value(&input);
        input.handle_event(&key(KeyCode::Up, KeyModifiers::NONE));
        let after_up = selected_value(&input);
        assert_ne!(after_down, after_up, "Up reverses Down");
    }

    #[test]
    fn handle_event_popup_visible_passes_unhandled_keys_to_textarea() {
        // Char keys are not popup nav — they fall through to the
        // textarea so refining the query keeps working.
        let mut input = input_with_popup();
        let action = input.handle_event(&key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(action.is_none());
        assert_eq!(input.textarea.lines(), vec!["/h"]);
        assert!(input.popup_visible(), "popup stays visible while typing");
    }

    #[test]
    fn handle_event_popup_visible_ignores_non_key_events() {
        // Resize and other non-key events reach handle_event when
        // routed directly; popup must let them pass without panicking.
        let mut input = input_with_popup();
        let action = input.handle_event(&Event::Resize(80, 24));
        assert!(action.is_none());
        assert!(input.popup_visible());
    }

    #[test]
    fn handle_event_popup_tab_in_name_mode_inserts_slash_name_and_space() {
        // Lone `/` shows the full roster; first row is `clear` (alphabetical). Tab inserts
        // `/clear ` and dismisses — pins the trailing-space contract that lets the user start
        // typing args immediately.
        let mut input = input_with_popup();
        let selected = selected_value(&input);
        let action = input.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(action.is_none(), "Tab is consumed, no UserAction");
        assert_eq!(input.textarea.lines(), vec![format!("/{selected} ")]);
        assert!(!input.popup_visible(), "popup dismisses after Tab");
    }

    #[test]
    fn handle_event_popup_tab_in_arg_mode_inserts_cmd_value_and_space() {
        // Type `/effort ` to enter arg mode against the curated effort roster (first row: low).
        // Tab must keep the `/effort` prefix and substitute the picked value plus trailing
        // space — `/{cmd} {value} `, not `/{value} ` (which would drop the cmd context).
        let mut input = test_input();
        type_text(&mut input, "/effort ");
        input.refresh_popup();
        assert!(input.popup_visible(), "popup opens for /effort arg mode");
        let picked = selected_value(&input);
        let action = input.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(action.is_none(), "Tab is consumed, no UserAction");
        assert_eq!(input.textarea.lines(), vec![format!("/effort {picked} ")]);
        assert!(!input.popup_visible(), "popup dismisses after Tab");
    }

    #[test]
    fn handle_event_popup_enter_in_name_mode_submits_slash_name() {
        // Enter on a name-mode row submits `/{name}` (no trailing space — submission is final).
        let mut input = input_with_popup();
        let selected = selected_value(&input);
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            Some(UserAction::SubmitPrompt(format!("/{selected}")))
        );
        assert!(input.textarea.is_empty(), "buffer clears on submit");
    }

    #[test]
    fn handle_event_popup_enter_in_arg_mode_submits_cmd_value() {
        // Enter on an arg-mode row submits `/{cmd} {value}` so the dispatcher receives the
        // full typed-arg form, not just the picked value.
        let mut input = test_input();
        type_text(&mut input, "/effort ");
        input.refresh_popup();
        let picked = selected_value(&input);
        let action = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            action,
            Some(UserAction::SubmitPrompt(format!("/effort {picked}"))),
        );
        assert!(input.textarea.is_empty(), "buffer clears on submit");
    }

    // ── render_popup ──

    #[test]
    fn render_popup_paints_when_visible() {
        let input = input_with_popup();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, 40, input.popup_height());
                input.render_popup(frame, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let first = buffer.cell(Position::new(0, 0)).unwrap().symbol();
        assert!(!first.is_empty(), "popup row paints something at (0,0)");
    }
}
