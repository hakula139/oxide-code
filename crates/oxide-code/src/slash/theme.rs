//! `/theme` opens the picker, or swaps directly with `/theme <name>`. Picker live-previews on
//! cursor moves and reverts on cancel.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::tui::action::Action;
use crate::tui::modal::chrome::{CONFIRM_HINTS, FOOTER_HEIGHT, render_footer};
use crate::tui::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::tui::theme::builtin::BUILT_IN;

// ── ThemeCmd ──

pub(super) struct ThemeCmd;

impl SlashCommand for ThemeCmd {
    fn name(&self) -> &'static str {
        "theme"
    }

    fn description(&self) -> &'static str {
        "Change the theme"
    }

    fn classify(&self, args: &str) -> SlashKind {
        if args.trim().is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Mutating
        }
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<name>]")
    }

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(BUILT_IN, prefix, |theme| theme.name)
            .into_iter()
            .map(|theme| ArgCompletion {
                value: Cow::Borrowed(theme.name),
                description: Cow::Borrowed(theme.description),
            })
            .collect()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(ThemePicker::new(
                &ctx.info.config.theme_name,
            ))));
        }
        let name = resolve_theme_arg(arg)?;
        Ok(SlashOutcome::Forward(Action::SwapTheme {
            name: name.to_owned(),
        }))
    }
}

/// Resolve a typed `/theme <name>` argument against the curated catalog. Custom file paths
/// are rejected — users with a custom TOML edit `~/.config/ox/config.toml` directly.
fn resolve_theme_arg(arg: &str) -> Result<&'static str, String> {
    let lower = arg.to_ascii_lowercase();
    BUILT_IN
        .iter()
        .find(|theme| theme.name == lower)
        .map(|theme| theme.name)
        .ok_or_else(|| format!("Unknown theme: `{arg}`. Valid: {}.", valid_names()))
}

fn valid_names() -> String {
    BUILT_IN
        .iter()
        .map(|theme| theme.name)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── PickerItem ──

struct ThemeRow {
    name: &'static str,
    description: &'static str,
    is_active: bool,
    hint: Option<char>,
}

impl ThemeRow {
    fn build(active_name: &str) -> Vec<Self> {
        BUILT_IN
            .iter()
            .enumerate()
            .map(|(idx, theme)| Self {
                name: theme.name,
                description: theme.description,
                is_active: theme.name == active_name,
                hint: numeric_hint(idx),
            })
            .collect()
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

struct ThemePicker {
    list: ListPicker<ThemeRow>,
    /// Theme active at open. Enter on its row cancels, since swapping to it would be a no-op.
    original_name: String,
}

impl ThemePicker {
    fn new(active_name: &str) -> Self {
        let rows = ThemeRow::build(active_name);
        let mut list = ListPicker::new("Select theme", rows).with_description(
            "Switch the active theme for this session. Restarting returns to your config.",
        );
        list.select_initial(|row| row.is_active);
        Self {
            list,
            original_name: active_name.to_owned(),
        }
    }

    /// `Preview` of the row under the cursor, or `Consumed` when the list is empty.
    fn preview_current(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme {
                name: row.name.to_owned(),
            })),
            None => ModalKey::Consumed,
        }
    }

    /// Enter commits when the cursor moved off the original theme and cancels otherwise.
    fn submit(&self) -> ModalKey {
        let Some(row) = self.list.selected() else {
            return ModalKey::Cancelled;
        };
        if row.name == self.original_name {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::Dispatch(Action::SwapTheme {
            name: row.name.to_owned(),
        }))
    }
}

impl Modal for ThemePicker {
    fn height(&self, width: u16, max_height: u16) -> u16 {
        let list_max = max_height.saturating_sub(FOOTER_HEIGHT);
        self.list.height(width, list_max) + FOOTER_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_area = Rect {
            height: area.height.saturating_sub(FOOTER_HEIGHT),
            ..area
        };
        self.list.render(frame, list_area, theme);
        render_footer(frame, area, &CONFIRM_HINTS, theme);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::modal::testing::render_rows;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn run_execute(args: &str) -> Result<SlashOutcome, String> {
        ThemeCmd.execute(args, &SlashContext::new(&test_session_info(), None))
    }

    fn picker_with_active(name: &str) -> ThemePicker {
        ThemePicker::new(name)
    }

    // ── ThemeCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(ThemeCmd.name(), "theme");
        assert_eq!(ThemeCmd.aliases(), [] as [&str; 0]);
        assert_ne!(ThemeCmd.description(), "");
        assert_eq!(ThemeCmd.usage(), Some("[<name>]"));
    }

    // ── ThemeCmd::classify ──

    #[test]
    fn classify_splits_on_args() {
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
        let expected: Vec<String> = BUILT_IN.iter().map(|t| t.name.to_owned()).collect();
        assert_eq!(arg_values(""), expected);
    }

    #[test]
    fn complete_arg_prefix_filter_narrows_to_matching_themes() {
        assert_eq!(arg_values("m"), vec!["mocha", "macchiato", "material"]);
    }

    #[test]
    fn complete_arg_substring_match_below_prefix_tier() {
        assert_eq!(arg_values("te"), vec!["latte", "material"]);
    }

    #[test]
    fn complete_arg_is_case_insensitive() {
        assert_eq!(arg_values("MOCHA"), vec!["mocha"]);
    }

    // ── ThemeCmd::execute ──

    #[test]
    fn execute_no_args_opens_the_picker() {
        run_execute("").unwrap().into_modal();
    }

    #[test]
    fn execute_with_known_name_forwards_swap_theme() {
        for theme in BUILT_IN {
            assert_eq!(
                run_execute(theme.name).unwrap().into_forward(),
                Action::SwapTheme {
                    name: theme.name.to_owned(),
                },
                "`{}` must forward SwapTheme",
                theme.name,
            );
        }
    }

    #[test]
    fn execute_is_case_insensitive() {
        assert_eq!(
            run_execute("LATTE").unwrap().into_forward(),
            Action::SwapTheme {
                name: "latte".to_owned(),
            },
        );
    }

    #[test]
    fn execute_unknown_name_errors_listing_valid_options() {
        let err = run_execute("solarized").unwrap_err();

        assert_eq!(
            err,
            "Unknown theme: `solarized`. Valid: mocha, macchiato, frappe, latte, material.",
        );
    }

    // ── ThemeRow::build ──

    #[test]
    fn build_rows_expose_curated_picker_values() {
        let rows = ThemeRow::build("latte");
        let names: Vec<&str> = rows.iter().map(PickerItem::label).collect();
        assert_eq!(names, BUILT_IN.iter().map(|t| t.name).collect::<Vec<_>>(),);

        let active_count = rows.iter().filter(|r| r.is_active()).count();
        assert_eq!(active_count, 1, "exactly one row marks the active theme");
        assert_eq!(
            rows.iter().find(|r| r.is_active()).unwrap().label(),
            "latte"
        );

        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(row.key_hint(), numeric_hint(idx), "idx={idx}");
            assert_eq!(row.description(), Some(BUILT_IN[idx].description));
        }
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
        // A custom file path matches no built-in row, so the cursor falls back to the first.
        let p = picker_with_active("~/themes/custom.toml");
        let row = p.list.selected().expect("first row");
        assert_eq!(row.name, "mocha");
    }

    // ── ThemePicker::render ──

    #[test]
    fn render_lists_themes_above_the_footer() {
        let p = picker_with_active("latte");

        let rows = render_rows(&p, 80, p.height(80, 20));

        let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
        assert_eq!(
            rows,
            [
                "Select theme",
                "Switch the active theme for this session. Restarting returns to your config.",
                "",
                "  1. mocha        Catppuccin dark (default)",
                "  2. macchiato    Catppuccin dark (medium)",
                "  3. frappe       Catppuccin dark (soft)",
                "> 4. latte      ✓ Catppuccin light",
                "  5. material     Material dark",
                "",
                "Enter to confirm  ·  Esc to cancel",
            ]
        );
    }

    // ── ThemePicker::handle_key ──

    #[test]
    fn handle_key_down_and_j_emit_preview_for_next_row_without_popping() {
        for code in [KeyCode::Down, KeyCode::Char('j')] {
            let mut p = picker_with_active("mocha");
            let outcome = p.handle_key(&key(code));
            match outcome {
                ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme { name })) => {
                    assert_eq!(name, "macchiato", "{code:?} from mocha lands on macchiato");
                }
                other => panic!("expected Preview(PreviewTheme) for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn handle_key_up_and_k_emit_preview_for_prev_row_without_popping() {
        // From the first row they wrap to the last via `select_prev`.
        for code in [KeyCode::Up, KeyCode::Char('k')] {
            let mut p = picker_with_active("mocha");
            let outcome = p.handle_key(&key(code));
            match outcome {
                ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme { name })) => {
                    assert_eq!(
                        name,
                        BUILT_IN.last().unwrap().name,
                        "{code:?} from first row wraps to last",
                    );
                }
                other => panic!("expected Preview for {code:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn handle_key_numeric_jump_emits_preview_for_target_row() {
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Char('4')));
        match outcome {
            ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme { name })) => {
                assert_eq!(name, "latte", "row 4 is latte");
            }
            other => panic!("expected Preview, got {other:?}"),
        }
    }

    #[test]
    fn handle_key_numeric_jump_past_roster_is_a_consumed_noop() {
        // An out-of-range digit stays Consumed. Deriving the first digit past the roster keeps the
        // test valid as themes are added.
        let past_end = char::from_digit(u32::try_from(BUILT_IN.len()).unwrap() + 1, 10)
            .expect("roster fits in a single digit");
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Char(past_end)));
        assert!(matches!(outcome, ModalKey::Consumed));
    }

    #[test]
    fn handle_key_enter_on_unchanged_cursor_returns_cancelled() {
        // Enter without moving behaves like Esc, since there is nothing to commit.
        let mut p = picker_with_active("mocha");
        let outcome = p.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ModalKey::Cancelled));
    }

    #[test]
    fn handle_key_enter_after_cursor_move_emits_swap_for_new_theme() {
        let mut p = picker_with_active("mocha");
        p.handle_key(&key(KeyCode::Down)); // → macchiato
        let outcome = p.handle_key(&key(KeyCode::Enter));
        match outcome {
            ModalKey::Submitted(ModalAction::Dispatch(Action::SwapTheme { name })) => {
                assert_eq!(name, "macchiato");
            }
            other => panic!("expected Submitted(SwapTheme), got {other:?}"),
        }
    }

    #[test]
    fn handle_key_other_keys_are_consumed_and_modal_stays_open() {
        let mut p = picker_with_active("mocha");
        for code in [KeyCode::Tab, KeyCode::Char('x'), KeyCode::Char('0')] {
            let outcome = p.handle_key(&key(code));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "{code:?} must be consumed",
            );
        }
    }
}
