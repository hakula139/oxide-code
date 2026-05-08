//! `/rename` — set the session title manually. Bare opens a modal pre-filled with the current
//! title; `/rename <title>` applies immediately. Suppresses AI title generation for the rest of
//! the session so a slow Haiku response can't overwrite the user's pick.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

const MODAL_TITLE: &str = "Rename session";
const PROMPT: &str = "/ ";
const PROMPT_WIDTH: u16 = 2;
const FOOTER_HINT: &str = "Enter to save · Esc to cancel";
const TITLE_ROW_HEIGHT: u16 = 1;
const SECTION_GAP: u16 = 1;
const INPUT_ROW_HEIGHT: u16 = 1;
const FOOTER_ROW_HEIGHT: u16 = 1;
/// Mirrors the actor's first-prompt title cap so manual titles fit one `--list` row.
const MAX_TITLE_CHARS: usize = 80;

// ── RenameCmd ──

pub(super) struct RenameCmd;

impl SlashCommand for RenameCmd {
    fn name(&self) -> &'static str {
        "rename"
    }

    fn description(&self) -> &'static str {
        "Rename the current session — `/rename` for a modal, `/rename <title>` to apply directly"
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

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let trimmed = args.trim();
        if trimmed.is_empty() {
            ctx.open_modal(Box::new(RenameModal::new(ctx.title)));
            return Ok(SlashOutcome::Done);
        }
        Ok(SlashOutcome::Forward(UserAction::Rename {
            title: trimmed.to_owned(),
        }))
    }
}

// ── RenameModal ──

pub(super) struct RenameModal {
    buffer: String,
}

impl RenameModal {
    fn new(initial: Option<&str>) -> Self {
        Self {
            buffer: initial
                .map(|t| t.chars().take(MAX_TITLE_CHARS).collect())
                .unwrap_or_default(),
        }
    }

    fn submit(&self) -> ModalKey {
        let trimmed = self.buffer.trim();
        if trimmed.is_empty() {
            return ModalKey::Consumed;
        }
        ModalKey::Submitted(ModalAction::User(UserAction::Rename {
            title: trimmed.to_owned(),
        }))
    }
}

impl Modal for RenameModal {
    fn height(&self, _width: u16) -> u16 {
        TITLE_ROW_HEIGHT + SECTION_GAP + INPUT_ROW_HEIGHT + SECTION_GAP + FOOTER_ROW_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let lines: Vec<Line<'static>> = vec![
            Line::from(Span::styled(
                MODAL_TITLE.to_owned(),
                theme.accent().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            self.input_row(area.width, theme),
            Line::default(),
            Line::from(Span::styled(FOOTER_HINT.to_owned(), theme.dim())),
        ];
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        self.place_cursor(frame, area);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.buffer.pop();
                ModalKey::Consumed
            }
            KeyCode::Char(c) => {
                if self.buffer.chars().count() < MAX_TITLE_CHARS {
                    self.buffer.push(c);
                }
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }
}

impl RenameModal {
    fn input_row(&self, area_width: u16, theme: &Theme) -> Line<'static> {
        let prompt = Span::styled(PROMPT.to_owned(), theme.accent());
        let budget = usize::from(area_width.saturating_sub(PROMPT_WIDTH + 1));
        let shown = truncate_to_width(&self.buffer, budget);
        let body = Span::styled(shown, theme.text());
        Line::from(vec![prompt, body])
    }

    fn place_cursor(&self, frame: &mut Frame<'_>, area: Rect) {
        let input_y_offset = TITLE_ROW_HEIGHT + SECTION_GAP;
        if input_y_offset >= area.height {
            return;
        }
        let cursor_y = area.y.saturating_add(input_y_offset);
        let visible_width =
            u16::try_from(UnicodeWidthStr::width(self.buffer.as_str())).unwrap_or(u16::MAX);
        let raw_x = area
            .x
            .saturating_add(PROMPT_WIDTH)
            .saturating_add(visible_width);
        crate::tui::cursor::place_clamped(frame, raw_x, cursor_y, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::context::{LiveSessionInfo, SlashContext};
    use crate::tui::components::chat::ChatView;

    fn ctx_with_title<'a>(
        chat: &'a mut ChatView,
        info: &'a LiveSessionInfo,
        title: Option<&'a str>,
    ) -> SlashContext<'a> {
        SlashContext::with_title(chat, info, title)
    }

    fn fresh_chat() -> ChatView {
        ChatView::new(&Theme::default(), false)
    }

    // ── RenameCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(RenameCmd.name(), "rename");
        assert!(RenameCmd.aliases().is_empty());
        assert!(!RenameCmd.description().is_empty());
        assert_eq!(RenameCmd.usage(), Some("[<title>]"));
    }

    #[test]
    fn classify_is_always_mutating() {
        assert_eq!(RenameCmd.classify(""), SlashKind::Mutating);
        assert_eq!(RenameCmd.classify("New Title"), SlashKind::Mutating);
    }

    #[test]
    fn echoes_input_only_for_typed_arg_form() {
        assert!(!RenameCmd.echoes_input(""));
        assert!(!RenameCmd.echoes_input("   "));
        assert!(RenameCmd.echoes_input("New Title"));
    }

    // ── RenameCmd::execute ──

    #[test]
    fn execute_typed_arg_forwards_rename_action_with_trimmed_title() {
        let mut chat = fresh_chat();
        let info = crate::slash::test_session_info();
        let mut ctx = ctx_with_title(&mut chat, &info, None);

        let outcome = RenameCmd.execute("  Fix auth bug  ", &mut ctx).unwrap();

        assert_eq!(
            outcome,
            SlashOutcome::Forward(UserAction::Rename {
                title: "Fix auth bug".to_owned(),
            }),
        );
        assert!(
            ctx.take_modal().is_none(),
            "typed-arg form does not open a modal"
        );
    }

    #[test]
    fn execute_bare_form_opens_modal_pre_filled_with_current_title() {
        let mut chat = fresh_chat();
        let info = crate::slash::test_session_info();
        let mut ctx = ctx_with_title(&mut chat, &info, Some("Existing title"));

        let outcome = RenameCmd.execute("", &mut ctx).unwrap();

        assert_eq!(outcome, SlashOutcome::Done);
        let mut modal = ctx.take_modal().expect("bare form opens a modal");
        let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::User(UserAction::Rename { title })) = result else {
            panic!("Enter on pre-filled buffer must submit; got {result:?}");
        };
        assert_eq!(title, "Existing title");
    }

    #[test]
    fn execute_bare_form_with_no_current_title_opens_empty_modal() {
        let mut chat = fresh_chat();
        let info = crate::slash::test_session_info();
        let mut ctx = ctx_with_title(&mut chat, &info, None);

        let _ = RenameCmd.execute("", &mut ctx).unwrap();
        let mut modal = ctx.take_modal().expect("modal opens");
        let result = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(result, ModalKey::Consumed),
            "empty Enter must not submit; got {result:?}",
        );
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
        let ModalKey::Submitted(ModalAction::User(UserAction::Rename { title })) = outcome else {
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
        let ModalKey::Submitted(ModalAction::User(UserAction::Rename { title })) = outcome else {
            panic!("expected Submitted, got {outcome:?}");
        };
        assert_eq!(title, "a");
    }

    #[test]
    fn handle_key_backspace_on_empty_buffer_is_a_silent_noop() {
        // Without the noop guard, popping an empty String would panic in some std impls; here it
        // happens to return None, but the test pins the behavior either way.
        let mut modal = RenameModal::new(None);
        assert!(matches!(
            modal.handle_key(&KeyEvent::from(KeyCode::Backspace)),
            ModalKey::Consumed,
        ));
    }

    #[test]
    fn handle_key_char_at_max_length_drops_extra_input() {
        // Cap mirrors `MAX_TITLE_LEN` — over-long titles would visually overflow `--list` rows.
        let prefilled: String = "a".repeat(MAX_TITLE_CHARS);
        let mut modal = RenameModal::new(Some(&prefilled));
        modal.handle_key(&KeyEvent::from(KeyCode::Char('z')));
        let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::User(UserAction::Rename { title })) = outcome else {
            panic!("expected Submitted, got {outcome:?}");
        };
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS, "extra char dropped");
        assert!(!title.contains('z'), "the dropped char must not appear");
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
}
