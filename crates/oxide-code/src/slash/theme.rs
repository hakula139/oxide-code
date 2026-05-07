//! `/theme` — open the picker, or swap directly with `/theme <name>`. Picker live-previews on
//! cursor moves and reverts on cancel.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::tui::modal::list_picker::{ListPicker, PickerItem};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── Constants ──

/// Curated picker roster (name, one-line description). Order matches `tui/theme/builtin.rs`.
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

fn numeric_hint(idx: usize) -> Option<char> {
    char::from_digit(u32::try_from(idx).ok()? + 1, 10)
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
        // Reserve one blank spacer + one footer line below the list.
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

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(LISTED_THEMES, prefix, |(name, _)| *name)
            .into_iter()
            .map(|(name, description)| ArgCompletion {
                value: Cow::Borrowed(*name),
                description: Cow::Borrowed(*description),
            })
            .collect()
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

/// Resolve a typed `/theme <name>` argument against the curated catalogue. Custom file paths
/// are rejected — users with a custom TOML edit `~/.config/ox/config.toml` directly.
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

    // ── ThemeRow ──

    #[test]
    fn theme_row_picker_item_methods_return_curated_values() {
        let rows = ThemeRow::build("latte");
        let names: Vec<&str> = rows.iter().map(PickerItem::label).collect();
        assert_eq!(
            names,
            LISTED_THEMES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        );

        let active_count = rows.iter().filter(|r| r.is_active()).count();
        assert_eq!(active_count, 1, "exactly one row marks the active theme");
        assert_eq!(
            rows.iter().find(|r| r.is_active()).unwrap().label(),
            "latte"
        );

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
        // Custom file path doesn't match any built-in row; cursor falls back to the first.
        let p = picker_with_active("~/themes/custom.toml");
        let row = p.list.selected().expect("first row");
        assert_eq!(row.name, "mocha");
    }

    // ── ThemePicker::handle_key ──

    #[test]
    fn down_and_j_emit_preview_for_next_row_without_popping() {
        // Down arrow and `j` (vi binding) share an arm; both must advance to the next row and
        // emit a preview without popping the modal.
        for code in [KeyCode::Down, KeyCode::Char('j')] {
            let mut p = picker_with_active("mocha");
            let outcome = p.handle_key(&key(code));
            match outcome {
                ModalKey::Preview(ModalAction::User(UserAction::PreviewTheme { name })) => {
                    assert_eq!(name, "macchiato", "{code:?} from mocha lands on macchiato");
                }
                other => panic!("expected Preview(PreviewTheme) for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn up_and_k_emit_preview_for_prev_row_without_popping() {
        // Up arrow and `k` (vi binding) share an arm; both must cycle to the previous row and
        // emit a preview. From the first row they wrap to the last via `select_prev`.
        for code in [KeyCode::Up, KeyCode::Char('k')] {
            let mut p = picker_with_active("mocha");
            let outcome = p.handle_key(&key(code));
            match outcome {
                ModalKey::Preview(ModalAction::User(UserAction::PreviewTheme { name })) => {
                    assert_eq!(
                        name,
                        LISTED_THEMES.last().unwrap().0,
                        "{code:?} from first row wraps to last",
                    );
                }
                other => panic!("expected Preview for {code:?}, got {other:?}"),
            }
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
    fn numeric_jump_past_roster_is_a_consumed_noop() {
        // Out-of-range digits stay Consumed so the App doesn't repaint for nothing. Use the
        // first digit past the curated count so the test survives roster growth.
        let past_end = char::from_digit(u32::try_from(LISTED_THEMES.len()).unwrap() + 1, 10)
            .expect("roster fits in a single digit");
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Char(past_end)));
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

    // ── ThemeCmd::complete_arg ──

    fn arg_values(prefix: &str) -> Vec<String> {
        ThemeCmd
            .complete_arg(prefix)
            .into_iter()
            .map(|c| c.value.into_owned())
            .collect()
    }

    #[test]
    fn complete_arg_empty_prefix_lists_full_roster_in_curated_order() {
        let expected: Vec<String> = LISTED_THEMES
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect();
        assert_eq!(arg_values(""), expected);
    }

    #[test]
    fn complete_arg_prefix_filter_narrows_to_matching_themes() {
        // `m` prefixes mocha, macchiato, material; matters that all three surface and roster
        // order is preserved.
        assert_eq!(arg_values("m"), vec!["mocha", "macchiato", "material"]);
    }

    #[test]
    fn complete_arg_substring_match_below_prefix_tier() {
        // `te` is a substring of `latte` and `material`; preserves declared order.
        assert_eq!(arg_values("te"), vec!["latte", "material"]);
    }

    #[test]
    fn complete_arg_is_case_insensitive() {
        assert_eq!(arg_values("MOCHA"), vec!["mocha"]);
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
}
