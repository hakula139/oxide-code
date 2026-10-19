//! `/init` forwards the prompt [`init::resolve`] builds. A bare `/init` in a project with neither
//! `AGENTS.md` nor `CLAUDE.md` first opens a picker asking which one to create.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::context::SlashContext;
use super::matcher::rank_by_prefix;
use super::registry::{ArgCompletion, SlashCommand, SlashKind, SlashOutcome};
use crate::command::init::{self, FILE_CHOICES};
use crate::protocol::Op;
use crate::tui::action::Action;
use crate::tui::modal::chrome::{CONFIRM_HINTS, FOOTER_HEIGHT, render_footer};
use crate::tui::modal::list_picker::{ListPicker, PickerItem, numeric_hint};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;

// ── InitCmd ──

pub(super) struct InitCmd;

impl SlashCommand for InitCmd {
    fn name(&self) -> &'static str {
        "init"
    }

    fn description(&self) -> &'static str {
        "Create or update AGENTS.md / CLAUDE.md"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<file>]")
    }

    fn complete_arg(&self, prefix: &str) -> Vec<ArgCompletion> {
        rank_by_prefix(&FILE_CHOICES, prefix, |&(name, _)| name)
            .into_iter()
            .map(|&(name, description)| ArgCompletion {
                value: Cow::Borrowed(name),
                description: Cow::Borrowed(description),
            })
            .collect()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let root = &ctx.info.project_root;
        Ok(match init::resolve(args, root)? {
            Some(op) => SlashOutcome::Forward(Action::Op(op)),
            None => SlashOutcome::Modal(Box::new(InitPicker::new(root))),
        })
    }
}

// ── PickerItem ──

struct FileRow {
    name: &'static str,
    description: &'static str,
    hint: Option<char>,
}

impl PickerItem for FileRow {
    fn label(&self) -> &str {
        self.name
    }
    fn description(&self) -> Option<&str> {
        Some(self.description)
    }
    fn key_hint(&self) -> Option<char> {
        self.hint
    }
}

// ── InitPicker ──

/// Asks which instruction file to create, then forwards the prompt that creates it.
struct InitPicker {
    list: ListPicker<FileRow>,
    root: PathBuf,
}

impl InitPicker {
    fn new(root: &Path) -> Self {
        let rows = FILE_CHOICES
            .iter()
            .enumerate()
            .map(|(idx, &(name, description))| FileRow {
                name,
                description,
                hint: numeric_hint(idx),
            })
            .collect();
        let no_file = init::no_file_sentence();
        let list = ListPicker::new("Create instruction file", rows)
            .with_description(format!("{no_file} Pick the file the model writes."));
        Self {
            list,
            root: root.to_owned(),
        }
    }

    fn submit(&self) -> ModalKey {
        let row = self
            .list
            .selected()
            .expect("the picker lists every file choice");
        let prompt = init::create_prompt(&self.root, row.name);
        ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Prompt(prompt))))
    }
}

impl Modal for InitPicker {
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
            KeyCode::Enter => return self.submit(),
            KeyCode::Up | KeyCode::Char('k') => self.list.select_prev(),
            KeyCode::Down | KeyCode::Char('j') => self.list.select_next(),
            KeyCode::Char(c @ '1'..='9') => _ = self.list.select_by_hint(c),
            _ => {}
        }
        ModalKey::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::testing::test_session_info;
    use crate::tui::modal::testing::render_rows;

    /// The action that forwards the prompt creating `file` at `root`.
    fn create_action(root: &Path, file: &str) -> Action {
        Action::Op(Op::Prompt(init::create_prompt(root, file)))
    }

    /// The action `key` submits, or `None` when it leaves the picker open.
    fn submitted(key: ModalKey) -> Option<Action> {
        match key {
            ModalKey::Submitted(ModalAction::Dispatch(action)) => Some(action),
            _ => None,
        }
    }

    fn press(picker: &mut dyn Modal, code: KeyCode) -> Option<Action> {
        submitted(picker.handle_key(&KeyEvent::from(code)))
    }

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(InitCmd.name(), "init");
        assert_eq!(InitCmd.aliases(), [] as [&str; 0]);
        assert_eq!(InitCmd.usage(), Some("[<file>]"));
    }

    mod classify {
        use super::*;

        #[test]
        fn is_mutating() {
            assert_eq!(InitCmd.classify(""), SlashKind::Mutating);
            assert_eq!(InitCmd.classify("AGENTS.md"), SlashKind::Mutating);
        }
    }

    mod complete_arg {
        use super::*;

        #[test]
        fn ranks_the_files_by_prefix_with_who_else_reads_them() {
            let rows = |prefix| -> Vec<(String, String)> {
                InitCmd
                    .complete_arg(prefix)
                    .into_iter()
                    .map(|c| (c.value.into_owned(), c.description.into_owned()))
                    .collect()
            };

            assert_eq!(
                rows("")
                    .into_iter()
                    .map(|(value, _)| value)
                    .collect::<Vec<_>>(),
                ["AGENTS.md", "CLAUDE.md"]
            );
            assert_eq!(
                rows("cl"),
                [(
                    "CLAUDE.md".to_owned(),
                    "Also read by Claude Code".to_owned()
                )]
            );
        }
    }

    mod execute {
        use super::*;

        fn execute_in(project_root: &Path, args: &str) -> Result<SlashOutcome, String> {
            let mut info = test_session_info();
            info.project_root = project_root.to_owned();
            InitCmd.execute(args, &SlashContext::new(&info, None))
        }

        #[test]
        fn bare_without_an_instruction_file_opens_the_picker() {
            let root = tempfile::tempdir().unwrap();

            let mut picker = execute_in(root.path(), "").unwrap().into_modal();

            assert_eq!(
                press(picker.as_mut(), KeyCode::Enter),
                Some(create_action(root.path(), "AGENTS.md")),
            );
        }

        #[test]
        fn with_a_file_name_forwards_its_create_prompt() {
            let root = tempfile::tempdir().unwrap();

            let action = execute_in(root.path(), "CLAUDE.md").unwrap().into_forward();

            assert_eq!(action, create_action(root.path(), "CLAUDE.md"));
        }

        #[test]
        fn unknown_file_name_errors_listing_valid_options() {
            let root = tempfile::tempdir().unwrap();

            assert_eq!(
                execute_in(root.path(), "README.md").unwrap_err(),
                "Unknown instruction file: `README.md`. Valid: AGENTS.md, CLAUDE.md.",
            );
        }
    }

    mod init_picker {
        use super::*;

        mod render {
            use super::*;

            #[test]
            fn lists_both_files_with_who_else_reads_them() {
                let picker = InitPicker::new(Path::new("/repo"));

                let rows = render_rows(&picker, 80, picker.height(80, 20));

                let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
                assert_eq!(
                    rows,
                    [
                        "Create instruction file",
                        "This project has no AGENTS.md or CLAUDE.md. Pick the file the model writes.",
                        "",
                        "> 1. AGENTS.md    Also read by Codex and other agents",
                        "  2. CLAUDE.md    Also read by Claude Code",
                        "",
                        "Enter to confirm  ·  Esc to cancel",
                    ]
                );
            }
        }

        mod handle_key {
            use super::*;

            #[test]
            fn enter_creates_the_highlighted_file() {
                let root = Path::new("/repo");
                let mut picker = InitPicker::new(root);
                assert_eq!(
                    press(&mut picker, KeyCode::Enter),
                    Some(create_action(root, "AGENTS.md"))
                );

                for code in [
                    KeyCode::Down,
                    KeyCode::Char('j'),
                    KeyCode::Up,
                    KeyCode::Char('k'),
                    KeyCode::Char('2'),
                ] {
                    let mut picker = InitPicker::new(root);
                    assert_eq!(press(&mut picker, code), None, "{code:?} moves the cursor");
                    assert_eq!(
                        press(&mut picker, KeyCode::Enter),
                        Some(create_action(root, "CLAUDE.md")),
                        "{code:?} from the first row lands on the second",
                    );
                }
            }

            #[test]
            fn other_keys_keep_the_picker_open_and_the_cursor_in_place() {
                let root = Path::new("/repo");
                let mut picker = InitPicker::new(root);
                for code in [KeyCode::Tab, KeyCode::Char('x'), KeyCode::Char('9')] {
                    assert_eq!(press(&mut picker, code), None, "{code:?}");
                }
                assert_eq!(
                    press(&mut picker, KeyCode::Enter),
                    Some(create_action(root, "AGENTS.md"))
                );
            }
        }
    }
}
