//! Multi-line input area with slash-command autocomplete.

mod popup;

use std::cell::Cell;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use self::popup::SlashPopup;
use crate::protocol::Op;
use crate::slash::{PopupState, arg_placeholder_for, popup_state};
use crate::tui::action::Action;
use crate::tui::glyphs::{USER_PROMPT_PREFIX, USER_PROMPT_PREFIX_WIDTH};
use crate::tui::saturating_u16;
use crate::tui::theme::Theme;
use crate::util::text::trim_blank_edges;

// ── Constants ──

/// Maximum number of visible content lines before the input stops growing.
const MAX_VISIBLE_LINES: u16 = 6;
const BORDER_ROWS: u16 = 2;

/// Result of routing one terminal event through [`InputArea::handle_event`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputOutcome {
    /// The app should dispatch this action.
    Action(Action),
    /// The input handled the event, so no other component may see it.
    Consumed,
    /// The input had no use for the event, so the caller may route it to the chat.
    Ignored,
}

// ── InputArea ──

/// Multi-line input area with dynamic height and slash-command popup.
pub(crate) struct InputArea {
    theme: Theme,
    textarea: TextArea<'static>,
    popup: SlashPopup,
    enabled: bool,
    /// Textarea width at the last render, which sets the wrap layout.
    rendered_width: Cell<u16>,
    /// Cached [`Self::visual_line_count`], cleared when the text or the rendered width changes.
    visual_lines: Cell<Option<u16>>,
}

// ── Public API ──

impl InputArea {
    pub(crate) fn new(theme: &Theme) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_style(theme.text());
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        textarea.set_block(Block::default());

        Self {
            theme: theme.clone(),
            textarea,
            enabled: true,
            popup: SlashPopup::new(theme),
            rendered_width: Cell::new(0),
            visual_lines: Cell::new(None),
        }
    }

    /// Re-skins every subsequent render, including the textarea's cached text style.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
        self.textarea.set_style(theme.text());
        self.popup.set_theme(theme);
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.textarea.lines().to_vec()
    }

    /// Replaces the buffer with `text`, parks the cursor at its end, and keeps the yank buffer.
    pub(crate) fn set_text(&mut self, text: &str) {
        // `cut` and `clear` would overwrite the yank buffer that Ctrl+Y pastes from.
        self.textarea.select_all();
        self.textarea.insert_str(text);
        self.visual_lines.set(None);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.textarea
            .lines()
            .iter()
            .all(|line| line.trim().is_empty())
    }

    pub(crate) fn height(&self) -> u16 {
        let content_lines = self.visual_line_count();
        content_lines.min(MAX_VISIBLE_LINES) + BORDER_ROWS
    }

    pub(crate) fn popup_visible(&self) -> bool {
        self.popup.is_visible()
    }

    pub(crate) fn popup_height(&self) -> u16 {
        self.popup.height()
    }

    pub(crate) fn render_popup(&self, frame: &mut Frame<'_>, area: Rect) {
        self.popup.render(frame, area);
    }
}

// ── Event Handling & Rendering ──

impl InputArea {
    pub(crate) fn handle_event(&mut self, event: &Event) -> InputOutcome {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => {
                self.paste(text);
                InputOutcome::Consumed
            }
            _ => InputOutcome::Ignored,
        }
    }

    pub(crate) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
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
        if self.rendered_width.replace(textarea_area.width) != textarea_area.width {
            self.visual_lines.set(None);
        }

        let sc = self.textarea.screen_cursor();
        let cursor_row = saturating_u16(sc.row);
        let (top, _) = self.textarea.scroll_offset();

        let raw_cursor_x = textarea_area.x.saturating_add(saturating_u16(sc.col));
        let cursor_y = textarea_area.y + cursor_row - top;

        if let Some(token) = self.ghost_text() {
            // Mirror `ratatui_textarea`'s built-in placeholder layout: the cursor sits at the
            // insertion column, the dim hint starts one column right.
            let ghost_x = raw_cursor_x.saturating_add(1);
            let width = textarea_area.right().saturating_sub(ghost_x);
            if width > 0 {
                let area = Rect::new(ghost_x, cursor_y, width, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(token, self.theme.dim()))),
                    area,
                );
            }
        }

        crate::tui::cursor::place_clamped(frame, raw_cursor_x, cursor_y, textarea_area);
    }
}

// ── Private Helpers ──

impl InputArea {
    // ── Key Routing ──

    fn handle_key(&mut self, key: &KeyEvent) -> InputOutcome {
        match (key.code, key.modifiers) {
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                return if self.enabled && self.is_empty() {
                    InputOutcome::Action(Action::Op(Op::Shutdown))
                } else {
                    InputOutcome::Consumed
                };
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                return InputOutcome::Action(if self.enabled {
                    Action::ConfirmExit
                } else {
                    Action::Op(Op::Interrupt)
                });
            }
            (KeyCode::BackTab, _) => return InputOutcome::Action(Action::CycleMode),
            (KeyCode::PageUp | KeyCode::PageDown, _)
            | (KeyCode::Home | KeyCode::End, KeyModifiers::CONTROL) => {
                return InputOutcome::Ignored;
            }
            _ => {}
        }

        if self.popup.is_visible() {
            let outcome = self.handle_popup_key(key);
            if outcome != InputOutcome::Ignored {
                return outcome;
            }
        }

        match key.code {
            KeyCode::Enter => {
                // Kitty reports SHIFT directly. VS Code and Cursor send Shift+Enter as `\x1b\r`
                // (ESC CR), which crossterm parses as Alt+Enter.
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.textarea.insert_newline();
                    self.visual_lines.set(None);
                    self.refresh_popup();
                    return InputOutcome::Consumed;
                }
                self.submit()
                    .map_or(InputOutcome::Consumed, InputOutcome::Action)
            }
            // An Up / Down the cursor can't follow scrolls the chat instead.
            KeyCode::Up | KeyCode::Down => {
                let before = self.textarea.cursor();
                self.input_key(*key);
                if self.textarea.cursor() == before {
                    InputOutcome::Ignored
                } else {
                    InputOutcome::Consumed
                }
            }
            _ => {
                self.input_key(*key);
                self.refresh_popup();
                InputOutcome::Consumed
            }
        }
    }

    /// Feeds one key to the textarea, dropping the cached line count when the text changed.
    fn input_key(&mut self, key: KeyEvent) {
        if self.textarea.input(key) {
            self.visual_lines.set(None);
        }
    }

    /// Inserts bracketed-paste text without submitting on its newlines.
    fn paste(&mut self, text: &str) {
        // Terminals send CRLF or bare CR line breaks, and the textarea splits only on LF.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.textarea.insert_str(text);
        self.visual_lines.set(None);
        self.refresh_popup();
    }

    // ── Popup & State ──

    fn handle_popup_key(&mut self, key: &KeyEvent) -> InputOutcome {
        match key.code {
            KeyCode::Up => {
                self.popup.select_prev();
                InputOutcome::Consumed
            }
            KeyCode::Down => {
                self.popup.select_next();
                InputOutcome::Consumed
            }
            KeyCode::Esc => {
                self.popup.set_state(None);
                InputOutcome::Consumed
            }
            KeyCode::Tab if key.modifiers.is_empty() => {
                self.popup_complete_to_buffer();
                InputOutcome::Consumed
            }
            KeyCode::Enter if key.modifiers.is_empty() => self
                .popup_submit_selected()
                .map_or(InputOutcome::Ignored, InputOutcome::Action),
            _ => InputOutcome::Ignored,
        }
    }

    fn popup_complete_to_buffer(&mut self) {
        let Some(completion) = self.popup.completion() else {
            return;
        };
        self.set_text(&format!("{completion} "));
        self.refresh_popup();
    }

    fn popup_submit_selected(&mut self) -> Option<Action> {
        let submission = self.popup.completion()?;
        self.set_text("");
        self.popup.set_state(None);
        Some(Action::Submit(submission))
    }

    fn refresh_popup(&mut self) {
        // Only a single line can be a slash command, so a multi-line buffer closes the popup.
        let state = match self.textarea.lines() {
            [single] => popup_state(single),
            _ => None,
        };
        self.popup.set_state(state.as_ref());
    }

    /// Dim placeholder painted at the cursor in arg-mode with an empty prefix.
    fn ghost_text(&self) -> Option<String> {
        let [single] = self.textarea.lines() else {
            return None;
        };
        let state = popup_state(single)?;
        let usage = match &state {
            PopupState::Arg { name, .. } => arg_placeholder_for(name),
            PopupState::Name(_) => None,
        };
        ghost_text_from_state(&state, usage)
    }

    /// Screen rows in the textarea's own wrap layout at its last-rendered width.
    fn visual_line_count(&self) -> u16 {
        if let Some(count) = self.visual_lines.get() {
            return count;
        }
        let mut probe = self.textarea.clone();
        probe.move_cursor(CursorMove::Bottom);
        probe.move_cursor(CursorMove::End);
        let count = saturating_u16(probe.screen_cursor().row.saturating_add(1));
        self.visual_lines.set(Some(count));
        count
    }

    fn submit(&mut self) -> Option<Action> {
        if self.is_empty() {
            return None;
        }
        let submission = trim_blank_edges(&self.textarea.lines().join("\n")).to_owned();

        self.set_text("");
        self.popup.set_state(None);

        Some(Action::Submit(submission))
    }
}

// ── Helpers ──

/// Fires only in `Arg` mode with an empty prefix and a `usage()` to surface.
fn ghost_text_from_state(state: &PopupState<'_>, usage: Option<&str>) -> Option<String> {
    let PopupState::Arg { prefix, .. } = state else {
        return None;
    };
    if !prefix.is_empty() {
        return None;
    }
    Some(normalize_placeholder(usage?))
}

/// Normalizes a `usage()` syntax fragment to a single bracketed token. Strips one optional layer
/// each of `[...]` and `<...>` so `[<id>]`, `<id>`, and `[id]` all render as `[id]`.
fn normalize_placeholder(usage: &str) -> String {
    let inner = usage.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner)
        .trim();
    let inner = inner
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(inner);
    format!("[{inner}]")
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::layout::Position;

    use super::*;

    fn test_input() -> InputArea {
        InputArea::new(&Theme::default())
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn type_text(input: &mut InputArea, text: &str) {
        for ch in text.chars() {
            input.textarea.input(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
            )));
        }
    }

    fn render_to_backend(input: &InputArea, width: u16, height: u16) -> TestBackend {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                input.render(frame, frame.area());
            })
            .unwrap();
        terminal.backend().clone()
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
        assert_eq!(input.height(), 1 + BORDER_ROWS);
    }

    #[test]
    fn height_grows_with_content() {
        let mut input = test_input();
        input.textarea.insert_newline();
        input.textarea.insert_newline();
        assert_eq!(input.height(), 3 + BORDER_ROWS);
    }

    #[test]
    fn height_accounts_for_visual_wrapping() {
        let mut input = test_input();
        type_text(&mut input, "aaaaaa bbbbbb cccccc dddddd");
        render_to_backend(&input, 12, 3);
        assert_eq!(input.height(), 4 + BORDER_ROWS, "4 word-wrapped rows");
    }

    #[test]
    fn height_capped_at_max() {
        let mut input = test_input();
        for _ in 0..10 {
            input.textarea.insert_newline();
        }
        assert_eq!(input.height(), MAX_VISIBLE_LINES + BORDER_ROWS);
    }

    // ── handle_event ──

    #[test]
    fn handle_event_ctrl_c_idle_arms_exit() {
        let mut input = test_input();
        let outcome = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(outcome, InputOutcome::Action(Action::ConfirmExit));
    }

    #[test]
    fn handle_event_ctrl_d_empty_buffer_quits_only_when_idle() {
        // POSIX EOF idiom: idle Ctrl+D on an empty buffer exits.
        let mut input = test_input();
        let idle = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(idle, InputOutcome::Action(Action::Op(Op::Shutdown)));

        // Busy Ctrl+D is a no-op even with an empty buffer, so a habitual EOF press can't tear
        // down an in-flight turn.
        input.set_enabled(false);
        let busy = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(busy, InputOutcome::Consumed);
    }

    #[test]
    fn handle_event_ctrl_d_with_content_is_a_noop_in_idle_and_busy() {
        // Ctrl+D mid-prompt keeps the typed text, as in bash, zsh, and Codex. Typing reaches the
        // buffer while busy too, so the same holds in both states.
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        let idle = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(idle, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["h"]);

        input.set_enabled(false);
        let busy = input.handle_event(&key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(busy, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["h"]);
    }

    #[test]
    fn handle_event_ctrl_c_busy_triggers_interrupt() {
        let mut input = test_input();
        input.set_enabled(false);
        let outcome = input.handle_event(&key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(outcome, InputOutcome::Action(Action::Op(Op::Interrupt)));
    }

    #[test]
    fn handle_event_disabled_empty_enter_is_silent() {
        // Submit's empty-buffer guard short-circuits, so a stray Enter mid-turn produces no action
        // even though the textarea accepts typing while busy.
        let mut input = test_input();
        input.set_enabled(false);
        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, InputOutcome::Consumed);
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
        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("q".to_owned())),
        );
    }

    #[test]
    fn handle_event_page_and_jump_keys_are_ignored_in_both_run_states() {
        // Chat owns these keys, so a draft must neither change nor swallow them.
        let mut input = test_input();
        type_text(&mut input, "draft");
        for enabled in [true, false] {
            input.set_enabled(enabled);
            for event in [
                key(KeyCode::PageUp, KeyModifiers::NONE),
                key(KeyCode::PageDown, KeyModifiers::NONE),
                key(KeyCode::Home, KeyModifiers::CONTROL),
                key(KeyCode::End, KeyModifiers::CONTROL),
            ] {
                assert_eq!(
                    input.handle_event(&event),
                    InputOutcome::Ignored,
                    "{event:?}"
                );
            }
            assert_eq!(input.textarea.lines(), vec!["draft"]);
            assert_eq!(input.textarea.cursor(), (0, 5));
        }
    }

    #[test]
    fn handle_event_up_down_on_empty_buffer_is_ignored() {
        let mut input = test_input();
        for code in [KeyCode::Up, KeyCode::Down] {
            assert_eq!(
                input.handle_event(&key(code, KeyModifiers::NONE)),
                InputOutcome::Ignored,
            );
        }
    }

    #[test]
    fn handle_event_up_down_move_the_cursor_until_the_buffer_edge() {
        let mut input = test_input();
        input.set_text("one\ntwo");

        let up = key(KeyCode::Up, KeyModifiers::NONE);
        let down = key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(input.handle_event(&up), InputOutcome::Consumed);
        assert_eq!(input.textarea.cursor().0, 0);
        assert_eq!(input.handle_event(&up), InputOutcome::Ignored, "top row");
        assert_eq!(input.handle_event(&down), InputOutcome::Consumed);
        assert_eq!(input.textarea.cursor().0, 1);
        assert_eq!(
            input.handle_event(&down),
            InputOutcome::Ignored,
            "bottom row"
        );
    }

    #[test]
    fn handle_event_plain_home_moves_the_cursor_while_busy() {
        let mut input = test_input();
        input.set_enabled(false);
        type_text(&mut input, "draft");
        let outcome = input.handle_event(&key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.cursor(), (0, 0));
    }

    #[test]
    fn handle_event_paste_normalizes_line_breaks_without_submitting() {
        let mut input = test_input();
        let outcome = input.handle_event(&Event::Paste("one\r\ntwo\rthree\nfour".to_owned()));
        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["one", "two", "three", "four"]);
    }

    #[test]
    fn handle_event_paste_of_a_slash_command_opens_the_popup() {
        let mut input = test_input();
        input.handle_event(&Event::Paste("/he".to_owned()));
        assert!(input.popup_visible());
    }

    #[test]
    fn handle_event_shift_enter_inserts_newline() {
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines().len(), 2);
    }

    #[test]
    fn handle_event_alt_enter_inserts_newline() {
        // VS Code and Cursor send Shift+Enter as `\x1b\r`, which crossterm parses as Alt+Enter.
        let mut input = test_input();
        input.textarea.input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        )));

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(outcome, InputOutcome::Consumed);
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

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("hi".to_owned())),
        );
    }

    // ── handle_event: popup ──

    /// An input with the popup open after typing `/`.
    fn input_with_popup() -> InputArea {
        let mut input = test_input();
        input.handle_event(&key(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(input.popup_visible(), "typing `/` opens the popup");
        input
    }

    fn popup_completion(input: &InputArea) -> String {
        input.popup.completion().unwrap()
    }

    #[test]
    fn handle_event_popup_down_advances_selection() {
        let mut input = input_with_popup();

        let outcome = input.handle_event(&key(KeyCode::Down, KeyModifiers::NONE));

        assert_eq!(outcome, InputOutcome::Consumed, "chat must not scroll too");
        assert_eq!(popup_completion(&input), "/clear");
    }

    #[test]
    fn handle_event_popup_up_reverses_selection() {
        let mut input = input_with_popup();
        input.handle_event(&key(KeyCode::Down, KeyModifiers::NONE));

        input.handle_event(&key(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(popup_completion(&input), "/archive");
    }

    #[test]
    fn handle_event_popup_visible_passes_unhandled_keys_to_textarea() {
        // Char keys fall through the popup to the textarea, so typing keeps refining the query.
        let mut input = input_with_popup();
        let outcome = input.handle_event(&key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["/h"]);
        assert!(input.popup_visible(), "popup stays visible while typing");
    }

    #[test]
    fn handle_event_popup_visible_ignores_non_key_events() {
        // Non-key events such as a resize can reach handle_event, and the open popup ignores them.
        let mut input = input_with_popup();
        let outcome = input.handle_event(&Event::Resize(80, 24));
        assert_eq!(outcome, InputOutcome::Ignored);
        assert!(input.popup_visible());
    }

    #[test]
    fn handle_event_popup_tab_in_name_mode_inserts_slash_name_and_space() {
        // The first row, `/archive`, has no arg roster, so the popup hides once Tab writes
        // `/archive ` and the line turns into an arg-mode query with no rows.
        let mut input = input_with_popup();

        let outcome = input.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["/archive "]);
        assert!(!input.popup_visible(), "no arg roster → popup hides");
    }

    #[test]
    fn handle_event_popup_tab_in_name_mode_chains_into_arg_mode_for_curated_commands() {
        let mut input = test_input();
        type_text(&mut input, "/mo");
        input.refresh_popup();
        let outcome = input.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["/mode "]);
        assert!(
            input.popup_visible(),
            "/mode has a curated arg roster, so the popup chains into arg mode",
        );
    }

    #[test]
    fn handle_event_popup_tab_in_arg_mode_inserts_cmd_value_and_space() {
        let mut input = test_input();
        type_text(&mut input, "/effort ");
        input.refresh_popup();
        assert!(input.popup_visible(), "popup opens for /effort arg mode");

        let outcome = input.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(outcome, InputOutcome::Consumed);
        assert_eq!(input.textarea.lines(), vec!["/effort low "]);
        assert!(!input.popup_visible(), "popup dismisses after Tab");
    }

    #[test]
    fn handle_event_popup_enter_in_name_mode_commits_picked_row() {
        let mut input = input_with_popup();

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("/archive".to_owned()))
        );
        assert!(input.textarea.is_empty());
    }

    #[test]
    fn handle_event_popup_enter_in_arg_mode_with_empty_prefix_commits_picked_row() {
        let mut input = test_input();
        type_text(&mut input, "/effort ");
        input.refresh_popup();

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("/effort low".to_owned()))
        );
        assert!(input.textarea.is_empty());
    }

    #[test]
    fn handle_event_popup_enter_with_typed_arg_prefix_still_commits_picked_row() {
        let mut input = test_input();
        type_text(&mut input, "/model claude-opus");
        input.refresh_popup();
        assert!(input.popup_visible());

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("/model claude-opus-5-5".to_owned()))
        );
        assert!(input.textarea.is_empty());
        assert!(!input.popup_visible());
    }

    #[test]
    fn handle_event_popup_esc_then_enter_submits_typed_buffer_literally() {
        // Popup is always a picker. Esc is the seam for submitting the typed text verbatim.
        let mut input = test_input();
        type_text(&mut input, "/model claude-opus");
        input.refresh_popup();
        assert!(input.popup_visible());

        input.handle_event(&key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!input.popup_visible());

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("/model claude-opus".to_owned())),
        );
    }

    // ── render ──

    #[test]
    fn render_empty_buffer_shows_only_prompt_marker_and_borders() {
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
        // Composing mid-turn for the queue looks the same as composing idle, as in Claude Code.
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
        // The chevron keeps the user accent while busy, since composing mid-turn is allowed.
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
        // A narrow width forces a word wrap, and text past the last visible row scrolls the
        // viewport so the cursor stays on screen.
        let mut input = test_input();
        type_text(
            &mut input,
            "a long input that overflows a narrow terminal and forces the textarea to wrap",
        );
        insta::assert_snapshot!(render_to_backend(&input, 30, 5));
    }

    #[test]
    fn render_paints_ghost_text_after_cursor_in_arg_mode() {
        // Mirrors the textarea's built-in placeholder layout: cursor at the insertion point,
        // dim hint starts the column right of it.
        let mut input = test_input();
        type_text(&mut input, "/model ");
        input.refresh_popup();
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).unwrap();
        terminal.draw(|f| input.render(f, f.area())).unwrap();

        let buf = terminal.backend().buffer();
        let row: String = (0..60).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(
            row.contains("/model  [id]"),
            "ghost-text must sit one column past the cursor: {row:?}",
        );

        let pos = terminal.get_cursor_position().unwrap();
        let cursor_col = USER_PROMPT_PREFIX_WIDTH + u16::try_from("/model ".len()).unwrap();
        assert_eq!(
            (pos.x, pos.y),
            (cursor_col, 1),
            "OS cursor must land at the typed insertion point, not on `[`",
        );
    }

    #[test]
    fn render_omits_ghost_text_once_user_types_arg_prefix() {
        let mut input = test_input();
        type_text(&mut input, "/model claude-");
        input.refresh_popup();
        let backend = render_to_backend(&input, 60, 3);
        let rendered = format!("{backend}");
        assert!(
            !rendered.contains("[id]"),
            "ghost-text must hide once prefix is typed: {rendered}",
        );
    }

    #[test]
    fn render_keeps_cursor_on_the_textarea_insertion_row_when_the_viewport_grows() {
        let mut input = test_input();
        for _ in 0..7 {
            input.textarea.insert_newline();
        }
        type_text(&mut input, "tail");
        let x = USER_PROMPT_PREFIX_WIDTH;

        for height in [5, 8] {
            let mut backend = render_to_backend(&input, 60, height);

            assert_eq!(input.textarea.scroll_offset(), (5, 0));
            assert_eq!(
                backend.get_cursor_position().unwrap(),
                Position::new(x + 4, 3)
            );
            let row: String = (x..x + 4)
                .map(|col| backend.buffer()[(col, 3)].symbol())
                .collect();
            assert_eq!(row, "tail");
        }
    }

    #[test]
    fn render_places_cursor_on_the_first_row_after_scrolling_back_up() {
        let mut input = test_input();
        for _ in 0..7 {
            input.textarea.insert_newline();
        }
        type_text(&mut input, "tail");
        render_to_backend(&input, 60, 5);
        for _ in 0..7 {
            input.textarea.input(key(KeyCode::Up, KeyModifiers::NONE));
        }

        let mut backend = render_to_backend(&input, 60, 5);

        assert_eq!(
            backend.get_cursor_position().unwrap(),
            Position::new(USER_PROMPT_PREFIX_WIDTH, 1),
        );
    }

    // ── ghost_text ──

    #[test]
    fn ghost_text_suppresses_in_name_mode() {
        let mut input = test_input();
        type_text(&mut input, "/mod");
        input.refresh_popup();
        assert_eq!(input.ghost_text(), None);
    }

    // ── visual_line_count ──

    #[test]
    fn visual_line_count_no_width_falls_back_to_logical() {
        // Before the first render the textarea has no wrap width.
        let mut input = test_input();
        assert_eq!(input.visual_line_count(), 1);

        input.handle_event(&key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(input.visual_line_count(), 2);
    }

    #[test]
    fn visual_line_count_tracks_edits_and_width_changes() {
        let mut input = test_input();
        render_to_backend(&input, 12, 6);
        assert_eq!(input.visual_line_count(), 1);

        for ch in "a".repeat(25).chars() {
            input.handle_event(&key(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(input.visual_line_count(), 3, "typing re-measures");

        input.handle_event(&Event::Paste("dddddddddd".to_owned()));
        assert_eq!(input.visual_line_count(), 4, "a paste re-measures");

        render_to_backend(&input, 40, 6);
        assert_eq!(input.visual_line_count(), 1, "a wider render re-measures");

        input.set_text("a\nb\nc");
        assert_eq!(input.visual_line_count(), 3, "set_text re-measures");
    }

    #[test]
    fn visual_line_count_breaks_at_word_boundaries() {
        // 27 chars fit 3 rows of 10 by width alone, but words cannot straddle a row break.
        let mut input = test_input();
        type_text(&mut input, "aaaaaa bbbbbb cccccc dddddd");
        render_to_backend(&input, 12, 6);
        assert_eq!(input.visual_line_count(), 4);
    }

    #[test]
    fn visual_line_count_splits_an_unbroken_token_at_the_width() {
        // A pasted URL or path wider than the band wraps onto the following rows.
        let mut input = test_input();
        type_text(&mut input, "abcdefghijklmnopqrstuvwxy");
        render_to_backend(&input, 12, 3);
        assert_eq!(input.visual_line_count(), 3);
    }

    #[test]
    fn visual_line_count_mixed_logical_and_wrapped() {
        let mut input = test_input();
        input.set_text("hello\nabcdefghijklmno");
        render_to_backend(&input, 12, 3);
        assert_eq!(input.visual_line_count(), 3, "1 row + 2 rows");
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
        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, InputOutcome::Consumed);
    }

    #[test]
    fn submit_keeps_the_yank_buffer() {
        // Ctrl+W yanks `bar`, and Ctrl+Y after the submit must still paste it.
        let mut input = test_input();
        type_text(&mut input, "foo bar");
        input.handle_event(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        input.handle_event(&key(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert_eq!(input.textarea.lines(), vec!["bar"]);
    }

    #[test]
    fn submit_trims_blank_edges_and_keeps_the_first_line_indent() {
        let mut input = test_input();
        input.set_text("\n  \n    fn main() {}\n  done  \n\n");

        let outcome = input.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            outcome,
            InputOutcome::Action(Action::Submit("    fn main() {}\n  done".to_owned())),
        );
    }

    // ── ghost_text_from_state ──

    #[test]
    fn ghost_text_from_state_arg_with_empty_prefix_and_usage_renders_token() {
        let state = PopupState::Arg {
            name: "model",
            prefix: "",
        };
        assert_eq!(
            ghost_text_from_state(&state, Some("[<id>]")),
            Some("[id]".to_owned()),
        );
    }

    #[test]
    fn ghost_text_from_state_non_empty_prefix_is_absent() {
        let state = PopupState::Arg {
            name: "model",
            prefix: "claude-",
        };
        assert_eq!(ghost_text_from_state(&state, Some("[<id>]")), None);
    }

    #[test]
    fn ghost_text_from_state_arg_without_usage_is_absent() {
        let state = PopupState::Arg {
            name: "init",
            prefix: "",
        };
        assert_eq!(ghost_text_from_state(&state, None), None);
    }

    #[test]
    fn ghost_text_from_state_name_mode_is_absent_even_with_usage() {
        let state = PopupState::Name("mo");
        assert_eq!(ghost_text_from_state(&state, Some("[<id>]")), None);
    }

    // ── normalize_placeholder ──

    #[test]
    fn normalize_placeholder_strips_outer_brackets_and_inner_angles() {
        assert_eq!(normalize_placeholder("[<id>]"), "[id]");
    }

    #[test]
    fn normalize_placeholder_handles_missing_layers() {
        assert_eq!(normalize_placeholder("<level>"), "[level]");
        assert_eq!(normalize_placeholder("[name]"), "[name]");
        assert_eq!(normalize_placeholder("topic"), "[topic]");
    }

    #[test]
    fn normalize_placeholder_trims_whitespace_inside_brackets() {
        assert_eq!(normalize_placeholder("[ <id> ]"), "[id]");
    }
}
