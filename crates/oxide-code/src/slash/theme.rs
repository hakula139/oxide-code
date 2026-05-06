//! `/theme` — open the picker, or swap with `/theme <name>`. Bare form opens a list picker
//! whose cursor moves live-preview the highlighted theme by repainting the full TUI; Esc snaps
//! back to the original, Enter commits for the rest of the session.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::tui::modal::list_picker::{ListPicker, PickerItem};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

/// Curated roster shown in the picker. Order matches `tui/theme/builtin.rs` — Catppuccin variants
/// dark→light, then non-Catppuccin palettes. Static descriptions live alongside so the picker
/// can render a one-glance summary without re-resolving each TOML.
const LISTED_THEMES: &[(&str, &str)] = &[
    ("mocha", "Catppuccin dark (default)"),
    ("macchiato", "Catppuccin dark (medium)"),
    ("frappe", "Catppuccin dark (soft)"),
    ("latte", "Catppuccin light"),
    ("material", "Material dark"),
];

// ── PickerItem ──

struct ThemeRow {
    name: &'static str,
    description: &'static str,
    is_active: bool,
    hint: Option<char>,
}

impl ThemeRow {
    fn build(active_name: &str) -> Vec<Self> {
        LISTED_THEMES
            .iter()
            .enumerate()
            .map(|(idx, (name, description))| Self {
                name,
                description,
                is_active: *name == active_name,
                hint: numeric_hint(idx),
            })
            .collect()
    }
}

/// `'1'`–`'9'` for the first nine rows; `None` after that.
fn numeric_hint(idx: usize) -> Option<char> {
    let digit = u32::try_from(idx).ok()?.checked_add(1)?;
    if (1..=9).contains(&digit) {
        char::from_digit(digit, 10)
    } else {
        None
    }
}

impl PickerItem for ThemeRow {
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> Option<&str> {
        Some(self.description)
    }
    fn is_active(&self) -> bool {
        self.is_active
    }
    fn key_hint(&self) -> Option<char> {
        self.hint
    }
}

// ── ThemePicker ──

pub(super) struct ThemePicker {
    list: ListPicker<ThemeRow>,
    /// Active theme at open — Enter on the same row cancels rather than firing a no-op swap.
    original_name: String,
}

impl ThemePicker {
    pub(super) fn new(active_name: &str) -> Self {
        let rows = ThemeRow::build(active_name);
        let mut list = ListPicker::new("Select theme", rows).with_description(
            "Switch the active theme. Applies to this session only — restart returns to your config.",
        );
        list.select_initial(|row| row.is_active);
        Self {
            list,
            original_name: active_name.to_owned(),
        }
    }

    /// `Preview` for cursor-driven moves on the picker; `Cancelled` when there's no current row.
    fn preview_current(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Preview(ModalAction::User(UserAction::PreviewTheme {
                name: row.name.to_owned(),
            })),
            None => ModalKey::Consumed,
        }
    }

    /// Enter: commit if the cursor moved off the original theme; otherwise cancel.
    fn submit(&self) -> ModalKey {
        let Some(row) = self.list.selected() else {
            return ModalKey::Cancelled;
        };
        if row.name == self.original_name {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::User(UserAction::SwapTheme {
            name: row.name.to_owned(),
        }))
    }
}

impl Modal for ThemePicker {
    fn height(&self, width: u16) -> u16 {
        // List body + spacer + footer.
        self.list.height(width).saturating_add(2)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_h = self.list.height(area.width);
        let list_area = Rect {
            height: list_h.min(area.height),
            ..area
        };
        self.list.render(frame, list_area, theme);

        let remaining = area.height.saturating_sub(list_h);
        if remaining >= 2 {
            let footer_area = Rect {
                x: area.x,
                y: area.y.saturating_add(list_h).saturating_add(1),
                width: area.width,
                height: 1,
            };
            let footer = Line::from(Span::styled(
                "Enter to confirm  ·  Esc to cancel",
                theme.dim(),
            ));
            frame.render_widget(Paragraph::new(footer).style(theme.surface()), footer_area);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.list.select_prev();
                self.preview_current()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list.select_next();
                self.preview_current()
            }
            KeyCode::Char(c @ '1'..='9') => {
                if self.list.select_by_hint(c) {
                    self.preview_current()
                } else {
                    ModalKey::Consumed
                }
            }
            _ => ModalKey::Consumed,
        }
    }
}

// ── ThemeCmd ──

pub(super) struct ThemeCmd;

impl SlashCommand for ThemeCmd {
    fn name(&self) -> &'static str {
        "theme"
    }

    fn description(&self) -> &'static str {
        "Open the theme picker or switch directly with `/theme <name>`"
    }

    fn classify(&self, args: &str) -> SlashKind {
        if args.trim().is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Mutating
        }
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<name>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            ctx.open_modal(Box::new(ThemePicker::new(&ctx.info.config.theme_name)));
            return Ok(SlashOutcome::Done);
        }
        let name = resolve_theme_arg(arg)?;
        Ok(SlashOutcome::Forward(UserAction::SwapTheme {
            name: name.to_owned(),
        }))
    }
}

/// Validates `arg` against the curated picker roster. Custom file paths aren't accepted via
/// `/theme <name>` — users who maintain a custom TOML edit `~/.config/ox/config.toml` directly.
fn resolve_theme_arg(arg: &str) -> Result<&'static str, String> {
    let lower = arg.to_ascii_lowercase();
    LISTED_THEMES
        .iter()
        .find(|(name, _)| *name == lower)
        .map(|(name, _)| *name)
        .ok_or_else(|| format!("Unknown theme: `{arg}`. Valid: {}.", valid_names()))
}

fn valid_names() -> String {
    LISTED_THEMES
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn run_execute(
        args: &str,
    ) -> (
        ChatView,
        Option<Box<dyn Modal>>,
        Result<SlashOutcome, String>,
    ) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let (outcome, modal) = {
            let mut ctx = SlashContext::new(&mut chat, &info);
            let outcome = ThemeCmd.execute(args, &mut ctx);
            (outcome, ctx.take_modal())
        };
        (chat, modal, outcome)
    }

    fn picker_with_active(name: &str) -> ThemePicker {
        ThemePicker::new(name)
    }

    // ── ThemeCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ThemeCmd.name(), "theme");
        assert!(ThemeCmd.aliases().is_empty());
        assert!(!ThemeCmd.description().is_empty());
        assert_eq!(ThemeCmd.usage(), Some("[<name>]"));
    }

    #[test]
    fn classify_splits_on_args() {
        // Bare form opens the picker (read-only); typed arg races the client (mutating).
        assert_eq!(ThemeCmd.classify(""), SlashKind::ReadOnly);
        assert_eq!(ThemeCmd.classify("   "), SlashKind::ReadOnly);
        assert_eq!(ThemeCmd.classify("latte"), SlashKind::Mutating);
    }

    // ── ThemeCmd::execute ──

    #[test]
    fn execute_no_args_opens_picker_and_pushes_no_chat_block() {
        let (chat, modal, outcome) = run_execute("");
        assert_eq!(outcome, Ok(SlashOutcome::Done));
        assert!(modal.is_some(), "bare /theme must populate the modal slot");
        assert_eq!(chat.entry_count(), 0, "chat must stay clean on open");
    }

    #[test]
    fn execute_with_known_name_forwards_swap_theme() {
        for (name, _) in LISTED_THEMES {
            let (_, _, outcome) = run_execute(name);
            assert_eq!(
                outcome,
                Ok(SlashOutcome::Forward(UserAction::SwapTheme {
                    name: (*name).to_owned(),
                })),
                "`{name}` must forward SwapTheme",
            );
        }
    }

    #[test]
    fn execute_is_case_insensitive() {
        let (_, _, outcome) = run_execute("LATTE");
        assert_eq!(
            outcome,
            Ok(SlashOutcome::Forward(UserAction::SwapTheme {
                name: "latte".to_owned(),
            })),
        );
    }

    #[test]
    fn execute_unknown_name_errors_listing_valid_options() {
        let (chat, _, outcome) = run_execute("solarized");
        let msg = outcome.expect_err("unknown name must error");
        assert!(msg.starts_with("Unknown theme: `solarized`."), "{msg}");
        for (name, _) in LISTED_THEMES {
            assert!(msg.contains(name), "lists `{name}`: {msg}");
        }
        assert_eq!(chat.entry_count(), 0, "execute must not push on Err");
    }

    // ── ThemePicker::new ──

    #[test]
    fn new_positions_cursor_on_active_theme() {
        let p = picker_with_active("latte");
        let row = p.list.selected().expect("active row");
        assert_eq!(row.name, "latte");
        assert!(row.is_active);
    }

    #[test]
    fn new_with_unknown_active_keeps_cursor_on_first_row() {
        // User-provided custom file path won't match any built-in row; cursor falls back to the
        // first listed theme rather than panicking or silently picking nothing.
        let p = picker_with_active("~/themes/custom.toml");
        let row = p.list.selected().expect("first row");
        assert_eq!(row.name, "mocha");
    }

    // ── ThemePicker::handle_key ──

    #[test]
    fn down_arrow_emits_preview_for_next_row_without_popping() {
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Down));
        match outcome {
            ModalKey::Preview(ModalAction::User(UserAction::PreviewTheme { name })) => {
                assert_eq!(name, "macchiato", "Down from mocha lands on macchiato");
            }
            other => panic!("expected Preview(PreviewTheme), got {other:?}"),
        }
    }

    #[test]
    fn numeric_jump_emits_preview_for_target_row() {
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Char('4')));
        match outcome {
            ModalKey::Preview(ModalAction::User(UserAction::PreviewTheme { name })) => {
                assert_eq!(name, "latte", "row 4 is latte");
            }
            other => panic!("expected Preview, got {other:?}"),
        }
    }

    #[test]
    fn numeric_jump_unknown_digit_is_a_consumed_noop() {
        // Only digits 1..=N (curated count) jump; out-of-range digits leave cursor and emit no
        // preview, so the App doesn't repaint for nothing.
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Char('9')));
        assert!(matches!(outcome, ModalKey::Consumed));
    }

    #[test]
    fn enter_on_unchanged_cursor_returns_cancelled() {
        // No-touch Enter mirrors Esc — nothing to commit.
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn enter_after_cursor_move_emits_swap_for_new_theme() {
        let mut p = picker_with_active("mocha");
        p.handle_key(&key(KeyCode::Down)); // → macchiato
        let outcome = p.handle_key(&key(KeyCode::Enter));
        match outcome {
            ModalKey::Submitted(ModalAction::User(UserAction::SwapTheme { name })) => {
                assert_eq!(name, "macchiato");
            }
            other => panic!("expected Submitted(SwapTheme), got {other:?}"),
        }
    }

    #[test]
    fn other_keys_are_consumed_and_modal_stays_open() {
        let mut p = picker_with_active("mocha");
        for code in [KeyCode::Tab, KeyCode::Char('x'), KeyCode::Char('0')] {
            let outcome = p.handle_key(&key(code));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "{code:?} must be consumed",
            );
        }
    }

    // ── ThemePicker::render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let p = picker_with_active("mocha");
        let theme = Theme::default();
        for width in [40_u16, 80, 120] {
            let h = p.height(width).min(20);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| p.render(frame, Rect::new(0, 0, width, h), &theme))
                .expect("render must not panic");
        }
    }

    // ── ThemeRow ──

    #[test]
    fn theme_row_picker_item_methods_return_curated_values() {
        // Pins the trait-impl surface for ThemeRow — label / description / is_active / key_hint
        // each have a path the picker depends on and only the render smoke test exercises
        // indirectly.
        let rows = ThemeRow::build("latte");
        let names: Vec<&str> = rows.iter().map(PickerItem::label).collect();
        assert_eq!(
            names,
            LISTED_THEMES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        );

        let active_count = rows.iter().filter(|r| r.is_active()).count();
        assert_eq!(active_count, 1, "exactly one row marks the active theme");
        assert!(rows.iter().find(|r| r.is_active()).unwrap().label() == "latte");

        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(row.key_hint(), numeric_hint(idx), "idx={idx}");
            assert_eq!(row.description(), Some(LISTED_THEMES[idx].1));
        }
    }

    // ── numeric_hint ──

    #[test]
    fn numeric_hint_covers_first_nine_rows_then_returns_none() {
        for idx in 0..9 {
            let expected = char::from_digit(u32::try_from(idx + 1).unwrap(), 10);
            assert_eq!(numeric_hint(idx), expected, "idx={idx}");
        }
        assert_eq!(numeric_hint(9), None, "10th row has no hint");
    }
}
