//! `/rename` sets the session title manually. Bare opens a modal pre-filled with the current
//! title. `/rename <title>` applies immediately. Suppresses AI title generation for the rest of
//! the session so a slow Haiku response can't overwrite the user's pick.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::command;
use crate::protocol::Op;
use crate::tui::action::Action;
use crate::tui::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::tui::modal::line_editor::LineEditor;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

const MODAL_TITLE: &str = "Rename session";
const FOOTER_HINTS: [&str; 2] = ["Enter to save", "Esc to cancel"];

/// Mirrors the actor's first-prompt title cap so manual titles fit one `--list` row.
const MAX_TITLE_CHARS: usize = 80;

// ── RenameCmd ──

pub(super) struct RenameCmd;

impl SlashCommand for RenameCmd {
    fn name(&self) -> &'static str {
        "rename"
    }

    fn description(&self) -> &'static str {
        "Rename the current session"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<title>]")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if args.trim().is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(RenameModal::new(ctx.title))));
        }
        Ok(SlashOutcome::Forward(Action::Op(command::rename(args)?)))
    }
}

// ── RenameModal ──

struct RenameModal {
    header: Header,
    editor: LineEditor,
}

impl RenameModal {
    fn new(initial: Option<&str>) -> Self {
        Self {
            header: Header::new(MODAL_TITLE),
            editor: LineEditor::new(initial.unwrap_or_default()).with_max_chars(MAX_TITLE_CHARS),
        }
    }

    fn submit(&self) -> ModalKey {
        let trimmed = self.editor.text().trim();
        if trimmed.is_empty() {
            return ModalKey::Consumed;
        }
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename {
            title: trimmed.to_owned(),
        })))
    }
}

impl Modal for RenameModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        self.header.height() + 1 + FOOTER_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut lines = Vec::new();
        self.header.push_lines(&mut lines, area.width, theme);
        lines.push(self.editor.line(area.width, theme));
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        render_footer(frame, area, &FOOTER_HINTS, theme);

        let input_y = self.header.height();
        if input_y < area.height {
            self.editor.place_cursor(frame, area, area.y + input_y);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.code == KeyCode::Enter {
            return self.submit();
        }
        self.editor.handle_key(event);
        ModalKey::Consumed
    }

    fn handle_paste(&mut self, text: &str) {
        self.editor.paste(text);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::slash::context::SlashContext;
    use crate::tui::modal::testing::render_rows;

    fn run_execute(args: &str, title: Option<&str>) -> SlashOutcome {
        let info = crate::slash::test_session_info();
        RenameCmd
            .execute(args, &SlashContext::new(&info, title))
            .unwrap()
    }

    // ── RenameCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(RenameCmd.name(), "rename");
        assert_eq!(RenameCmd.aliases(), [] as [&str; 0]);
        assert_ne!(RenameCmd.description(), "");
        assert_eq!(RenameCmd.usage(), Some("[<title>]"));
    }

    // ── RenameCmd::classify ──

    #[test]
    fn classify_is_always_mutating() {
        assert_eq!(RenameCmd.classify(""), SlashKind::Mutating);
        assert_eq!(RenameCmd.classify("New Title"), SlashKind::Mutating);
    }

    // ── RenameCmd::echoes_input ──

    #[test]
    fn echoes_input_only_for_typed_arg_form() {
        assert!(!RenameCmd.echoes_input(""));
        assert!(!RenameCmd.echoes_input("   "));
        assert!(RenameCmd.echoes_input("New Title"));
    }

    // ── RenameCmd::execute ──

    #[test]
    fn execute_typed_arg_forwards_rename_action_with_trimmed_title() {
        assert_eq!(
            run_execute("  Fix auth bug  ", None).into_forward(),
            Action::Op(Op::Rename {
                title: "Fix auth bug".to_owned(),
            }),
        );
    }

    #[test]
    fn execute_bare_form_opens_modal_pre_filled_with_current_title() {
        let mut modal = run_execute("", Some("Existing title")).into_modal();
        let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = result
        else {
            panic!("Enter on pre-filled buffer must submit; got {result:?}");
        };
        assert_eq!(title, "Existing title");
    }

    #[test]
    fn execute_bare_form_with_no_current_title_opens_empty_modal() {
        let mut modal = run_execute("", None).into_modal();
        let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(result, ModalKey::Consumed),
            "empty Enter must not submit; got {result:?}",
        );
    }

    // ── RenameModal::height ──

    #[test]
    fn height_sums_to_five_rows() {
        // Title (1) + gap (1) + input (1) + gap (1) + footer (1) = 5. Render tests assume the
        // input row sits at y=2.
        assert_eq!(RenameModal::new(None).height(40, u16::MAX), 5);
    }

    // ── RenameModal::render ──

    #[test]
    fn render_lays_out_title_prompt_and_footer_in_order() {
        let modal = RenameModal::new(Some("Fix auth"));
        let width: u16 = 40;
        let rows = render_rows(&modal, width, modal.height(width, u16::MAX));

        let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
        assert_eq!(
            rows,
            [
                "Rename session",
                "",
                "/ Fix auth",
                "",
                "Enter to save  ·  Esc to cancel",
            ]
        );
    }

    #[test]
    fn render_scrolls_a_title_wider_than_the_row_to_its_tail() {
        let long = format!("{}tail", "x".repeat(60));
        let modal = RenameModal::new(Some(&long));
        let width: u16 = 20;
        let rows = render_rows(&modal, width, modal.height(width, u16::MAX));

        let input = &rows[2];
        assert_eq!(input.trim_end(), format!("/ {}tail", "x".repeat(13)));
    }

    #[test]
    fn render_places_cursor_after_visible_buffer_text() {
        let theme = Theme::default();
        let modal = RenameModal::new(Some("ab"));
        let width: u16 = 40;
        let h = modal.height(width, u16::MAX);
        let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, Rect::new(0, 0, width, h), &theme))
            .unwrap();
        let pos = terminal.get_cursor_position().unwrap();

        // The input row sits below the title and a gap, and the cursor lands after "/ ab".
        assert_eq!(
            (pos.x, pos.y),
            (4, 2),
            "cursor must land after the rendered buffer",
        );
    }

    #[test]
    fn render_clamps_cursor_when_buffer_overflows_visible_row() {
        let theme = Theme::default();
        let long: String = "x".repeat(100);
        let modal = RenameModal::new(Some(&long));
        let width: u16 = 20;
        let h = modal.height(width, u16::MAX);
        let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, Rect::new(0, 0, width, h), &theme))
            .unwrap();
        let pos = terminal.get_cursor_position().unwrap();

        assert_eq!(pos.x, width - 1, "cursor must clamp to the right edge");
    }

    #[test]
    fn render_skips_cursor_when_area_too_short_for_input_row() {
        let theme = Theme::default();
        let modal = RenameModal::new(Some("ab"));
        let width: u16 = 40;
        let mut terminal = Terminal::new(TestBackend::new(width, 5)).unwrap();
        // Render the modal into an oversized terminal but pass a 1-row Rect so the input row is
        // entirely off-screen. `place_cursor` should bail without panicking.
        terminal
            .draw(|frame| modal.render(frame, Rect::new(0, 0, width, 1), &theme))
            .expect("short-area render must not panic");
    }

    // ── RenameModal::handle_key ──

    #[test]
    fn handle_key_typing_appends_to_buffer_and_enter_submits_trimmed_title() {
        let mut modal = RenameModal::new(None);
        for ch in "  Fix auth  ".chars() {
            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Char(ch))),
                ModalKey::Consumed,
            ));
        }
        let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = outcome
        else {
            panic!("trim happens at submit time; got {outcome:?}");
        };
        assert_eq!(title, "Fix auth");
    }

    #[test]
    fn handle_key_backspace_removes_last_char_when_buffer_non_empty() {
        let mut modal = RenameModal::new(Some("ab"));
        assert!(matches!(
            modal.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            ModalKey::Consumed,
        ));
        let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = outcome
        else {
            panic!("expected Submitted, got {outcome:?}");
        };
        assert_eq!(title, "a");
    }

    #[test]
    fn handle_key_backspace_on_empty_buffer_is_a_silent_noop() {
        let mut modal = RenameModal::new(None);
        assert!(matches!(
            modal.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            ModalKey::Consumed,
        ));
    }

    #[test]
    fn handle_key_char_at_max_length_drops_extra_input() {
        let prefilled: String = "a".repeat(MAX_TITLE_CHARS);
        let mut modal = RenameModal::new(Some(&prefilled));
        modal.handle_key(&KeyEvent::from(KeyCode::Char('z')));
        let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = outcome
        else {
            panic!("expected Submitted, got {outcome:?}");
        };
        assert_eq!(title, prefilled);
    }

    #[test]
    fn handle_key_edits_at_the_cursor_and_never_inserts_control_chords() {
        let mut modal = RenameModal::new(Some("ac"));
        for event in [
            KeyEvent::from(KeyCode::Left),
            KeyEvent::from(KeyCode::Char('b')),
            KeyEvent::from(KeyCode::Tab),
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        ] {
            let outcome = modal.handle_key(&event);
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "{event:?}: {outcome:?}"
            );
        }
        let submit = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Rename { title }))) = submit
        else {
            panic!("buffer should still submit cleanly; got {submit:?}");
        };
        assert_eq!(title, "abc", "Tab and Ctrl+G must not edit the title");
    }

    #[test]
    fn handle_key_blank_only_buffer_does_not_submit() {
        let mut modal = RenameModal::new(None);
        for _ in 0..3 {
            modal.handle_key(&KeyEvent::from(KeyCode::Char(' ')));
        }
        let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(outcome, ModalKey::Consumed),
            "all-whitespace Enter must not submit; got {outcome:?}",
        );
    }

    // ── RenameModal::handle_paste ──

    #[test]
    fn handle_paste_inserts_one_line_at_the_cursor() {
        let mut modal = RenameModal::new(Some("Fix"));
        modal.handle_paste(" auth\r\nflow");
        let rows = render_rows(&modal, 40, modal.height(40, u16::MAX));
        assert_eq!(rows[2].trim_end(), "/ Fix auth flow");
    }
}
