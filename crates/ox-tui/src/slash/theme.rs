//! `/theme` opens the picker, or swaps directly with `/theme <name>`. Picker live-previews on
//! cursor moves and reverts on cancel.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::action::Action;
use crate::modal::chrome::CONFIRM_HINTS;
use crate::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;
use crate::theme::builtin::{self, BUILT_IN, BuiltinTheme};

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
        if args.is_empty() {
            SlashKind::ReadOnly
        } else {
            SlashKind::Steering
        }
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

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, arg: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if arg.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(ThemePicker::new(
                &ctx.info.config.theme_name,
            ))));
        }
        let theme = resolve_theme_arg(arg)?;
        Ok(SlashOutcome::Forward(Action::SwapTheme(theme)))
    }
}

/// Resolves a typed `/theme <name>` argument against the curated catalog. Custom themes require
/// an edit to `config.toml`, so file paths are rejected.
fn resolve_theme_arg(arg: &str) -> Result<&'static BuiltinTheme, String> {
    builtin::find(&arg.to_ascii_lowercase())
        .ok_or_else(|| format!("Unknown theme: `{arg}`. Valid: {}.", builtin::names()))
}

// ── PickerItem ──

struct ThemeRow {
    theme: &'static BuiltinTheme,
    is_active: bool,
    hint: Option<char>,
}

impl ThemeRow {
    fn build(active_name: &str) -> Vec<Self> {
        BUILT_IN
            .iter()
            .enumerate()
            .map(|(idx, theme)| Self {
                theme,
                is_active: theme.name == active_name,
                hint: numeric_hint(idx),
            })
            .collect()
    }
}

impl PickerItem for ThemeRow {
    fn label(&self) -> &str {
        self.theme.name
    }

    fn description(&self) -> Option<&str> {
        Some(self.theme.description)
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
        list.cursor_to(|row| row.is_active);
        Self {
            list,
            original_name: active_name.to_owned(),
        }
    }

    /// Enter commits when the cursor moved off the original theme and cancels otherwise.
    fn submit(&self) -> ModalKey {
        let theme = self.list.selected().theme;
        if theme.name == self.original_name {
            return ModalKey::Cancelled;
        }
        ModalKey::Submitted(ModalAction::Dispatch(Action::SwapTheme(theme)))
    }
}

impl Modal for ThemePicker {
    fn height(&self, width: u16, max_height: u16) -> u16 {
        self.list.height_with_footer(width, max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.list
            .render_with_footer(frame, area, &CONFIRM_HINTS, theme);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.code == KeyCode::Enter {
            return self.submit();
        }
        if !self.list.navigate(event) {
            return ModalKey::Consumed;
        }
        let theme = self.list.selected().theme;
        ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme(theme)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modal::testing::render_rows;
    use crate::slash::registry::testing::run_execute;

    mod classify {
        use super::*;

        #[test]
        fn splits_on_args() {
            assert_eq!(ThemeCmd.classify(""), SlashKind::ReadOnly);
            assert_eq!(ThemeCmd.classify("latte"), SlashKind::Steering);
        }
    }

    mod complete_arg {
        use super::*;

        fn arg_values(prefix: &str) -> Vec<String> {
            ThemeCmd
                .complete_arg(prefix)
                .into_iter()
                .map(|c| c.value.into_owned())
                .collect()
        }

        #[test]
        fn empty_prefix_lists_full_roster_in_curated_order() {
            let expected: Vec<String> = BUILT_IN.iter().map(|t| t.name.to_owned()).collect();
            assert_eq!(arg_values(""), expected);
        }

        #[test]
        fn prefix_filter_narrows_to_matching_themes() {
            assert_eq!(arg_values("m"), vec!["mocha", "macchiato", "material"]);
        }

        #[test]
        fn substring_match_below_prefix_tier() {
            assert_eq!(arg_values("te"), vec!["latte", "material"]);
        }

        #[test]
        fn is_case_insensitive() {
            assert_eq!(arg_values("MOCHA"), vec!["mocha"]);
        }
    }

    mod execute {
        use super::*;

        #[test]
        fn no_args_opens_the_picker() {
            run_execute(&ThemeCmd, "").unwrap().into_modal();
        }

        #[test]
        fn with_known_name_forwards_swap_theme() {
            for theme in BUILT_IN {
                assert_eq!(
                    run_execute(&ThemeCmd, theme.name).unwrap().into_forward(),
                    Action::SwapTheme(theme),
                    "`{}` must forward SwapTheme",
                    theme.name,
                );
            }
        }

        #[test]
        fn is_case_insensitive() {
            assert_eq!(
                run_execute(&ThemeCmd, "LATTE").unwrap().into_forward(),
                Action::SwapTheme(builtin::find("latte").unwrap()),
            );
        }

        #[test]
        fn unknown_name_errors_listing_valid_options() {
            let err = run_execute(&ThemeCmd, "solarized").unwrap_err();

            assert_eq!(
                err,
                "Unknown theme: `solarized`. Valid: mocha, macchiato, frappe, latte, material.",
            );
        }
    }

    mod theme_row {
        use super::*;

        mod build {
            use super::*;

            #[test]
            fn rows_expose_curated_picker_values() {
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
        }
    }

    mod theme_picker {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn positions_cursor_on_active_theme() {
                let p = ThemePicker::new("latte");
                let row = p.list.selected();
                assert_eq!(row.theme.name, "latte");
                assert!(row.is_active);
            }

            #[test]
            fn with_unknown_active_keeps_cursor_on_first_row() {
                // A custom path matches no built-in row, so the cursor falls back to the first.
                let p = ThemePicker::new("~/themes/custom.toml");
                let row = p.list.selected();
                assert_eq!(row.theme.name, "mocha");
            }
        }

        mod render {
            use super::*;

            #[test]
            fn lists_themes_above_the_footer() {
                let p = ThemePicker::new("latte");

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
        }

        mod handle_key {
            use super::*;

            #[test]
            fn arrows_preview_the_row_they_land_on_without_popping() {
                for (code, expected) in [
                    (KeyCode::Down, "macchiato"),
                    (KeyCode::Up, BUILT_IN.last().unwrap().name),
                ] {
                    let mut p = ThemePicker::new("mocha");

                    let outcome = p.handle_key(&KeyEvent::from(code));

                    let ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme(theme))) =
                        outcome
                    else {
                        panic!("expected a preview for {code:?}, got {outcome:?}");
                    };
                    assert_eq!(theme.name, expected, "{code:?} from mocha");
                }
            }

            #[test]
            fn numeric_jump_emits_preview_for_target_row() {
                let mut p = ThemePicker::new("mocha");
                let outcome = p.handle_key(&KeyEvent::from(KeyCode::Char('4')));
                match outcome {
                    ModalKey::Preview(ModalAction::Dispatch(Action::PreviewTheme(theme))) => {
                        assert_eq!(theme.name, "latte", "row 4 is latte");
                    }
                    other => panic!("expected Preview, got {other:?}"),
                }
            }

            #[test]
            fn numeric_jump_past_roster_is_a_consumed_noop() {
                // An out-of-range digit stays Consumed. Deriving the first digit past the roster
                // keeps the test valid as themes are added.
                let past_end = char::from_digit(u32::try_from(BUILT_IN.len()).unwrap() + 1, 10)
                    .expect("roster fits in a single digit");
                let mut p = ThemePicker::new("mocha");
                let outcome = p.handle_key(&KeyEvent::from(KeyCode::Char(past_end)));
                assert!(matches!(outcome, ModalKey::Consumed));
            }

            #[test]
            fn enter_on_unchanged_cursor_cancels() {
                let mut p = ThemePicker::new("mocha");
                let outcome = p.handle_key(&KeyEvent::from(KeyCode::Enter));
                assert!(matches!(outcome, ModalKey::Cancelled));
            }

            #[test]
            fn enter_after_cursor_move_emits_swap_for_new_theme() {
                let mut p = ThemePicker::new("mocha");
                p.handle_key(&KeyEvent::from(KeyCode::Down)); // → macchiato
                let outcome = p.handle_key(&KeyEvent::from(KeyCode::Enter));
                match outcome {
                    ModalKey::Submitted(ModalAction::Dispatch(Action::SwapTheme(theme))) => {
                        assert_eq!(theme.name, "macchiato");
                    }
                    other => panic!("expected Submitted(SwapTheme), got {other:?}"),
                }
            }

            #[test]
            fn other_keys_are_consumed_and_modal_stays_open() {
                let mut p = ThemePicker::new("mocha");
                for code in [KeyCode::Tab, KeyCode::Char('x'), KeyCode::Char('0')] {
                    let outcome = p.handle_key(&KeyEvent::from(code));
                    assert!(
                        matches!(outcome, ModalKey::Consumed),
                        "{code:?} must be consumed",
                    );
                }
            }
        }
    }
}
