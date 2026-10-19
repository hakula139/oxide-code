//! `/init` asks the model to write the project's instruction file, or to propose changes to the
//! existing ones as a diff. When neither `AGENTS.md` nor `CLAUDE.md` exists, a picker first asks
//! which one to create.

use std::path::{Path, PathBuf};
use std::{fs, iter};

use crossterm::event::{KeyCode, KeyEvent};
use indoc::{formatdoc, indoc};
use ratatui::Frame;
use ratatui::layout::Rect;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::prompt::instructions::{INSTRUCTION_DIRS, INSTRUCTION_FILENAMES};
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

    fn execute(&self, _args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let root = &ctx.info.project_root;
        let existing = existing_instruction_files(root);
        if existing.is_empty() {
            Ok(SlashOutcome::Modal(Box::new(InitPicker::new(root))))
        } else {
            Ok(SlashOutcome::Forward(prompt(update_prompt(
                root, &existing,
            ))))
        }
    }
}

/// The instruction files at `root` and in its instruction directories, as root-relative paths.
/// Paths that resolve to the same file are listed once, under a path that is not a symlink when
/// one exists.
fn existing_instruction_files(root: &Path) -> Vec<PathBuf> {
    let candidates = iter::once("")
        .chain(INSTRUCTION_DIRS.iter().copied())
        .flat_map(|dir| {
            INSTRUCTION_FILENAMES
                .iter()
                .map(move |name| Path::new(dir).join(name))
        });
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in candidates {
        let full = root.join(&path);
        let Ok(canonical) = fs::canonicalize(&full) else {
            continue;
        };
        if !canonical.is_file() {
            continue;
        }
        match files.iter_mut().find(|(_, seen)| *seen == canonical) {
            None => files.push((path, canonical)),
            Some((kept, _)) if root.join(&*kept).is_symlink() && !full.is_symlink() => *kept = path,
            Some(_) => {}
        }
    }
    files.into_iter().map(|(path, _)| path).collect()
}

fn prompt(text: String) -> Action {
    Action::Op(Op::Prompt(text))
}

// ── Prompts ──

/// What an instruction file should hold, shared by the create and update prompts.
const RUBRIC: &str = indoc! {"
    An instruction file should include only what an agent would get wrong without it:

    1. Build / lint / test commands the agent can't infer from manifest files. Include any flags or sequences that differ from the language defaults (e.g., how to run a single test).
    2. High-level architecture that requires reading multiple files to understand: modules, layering, ownership, and the data flow between them.
    3. Project-specific conventions that diverge from language defaults (import grouping, error-handling style, naming, blank-line rules).
    4. External constraints the code can't reveal: required env vars, platform-only behavior, services that must be running, workflow steps the agent can't infer.

    Exclude:

    - Standard language conventions the agent already knows (`cargo test`, `npm test`, etc.).
    - File-by-file structure or component lists. These are discoverable via `glob` / `ls`.
    - Generic development advice (`write tests`, `handle errors`).
    - Information that changes frequently. Reference the source file by relative path so the agent reads the current version.
    - Sections you can't ground in files you actually read. Do not fabricate `Common Tasks`, `Tips for Development`, or `Support` headers.

    Be specific. `Use 2-space indentation in TypeScript` is better than `Format code properly`. Don't restate the same fact in multiple sections. Every line should answer `what would a fresh agent get wrong without this?` If the answer is `nothing`, cut the line.

    If a `README.md`, `.cursor/rules/`, `.cursorrules`, or `.github/copilot-instructions.md` exists, extract the load-bearing parts (commands, conventions, gotchas) and merge them into the instructions without duplication. Skip prose that restates language defaults."
};

/// The prompt asking the model to create `file` at `root`, for a project with no instruction
/// file yet.
fn create_prompt(root: &Path, file: &str) -> String {
    let root = root.display();
    formatdoc! {"
        Please analyze this codebase and create `{file}` in `{root}`. Future AI coding assistants will read it as project instructions when working on this codebase.

        {RUBRIC}

        Prefix the file with:

        ```
        # {file}

        This file provides guidance to AI coding assistants when working with code in this repository.
        ```"
    }
}

/// The prompt asking the model to propose changes to the `existing` instruction files at `root`
/// as a diff, and to apply them only once the user confirms.
fn update_prompt(root: &Path, existing: &[PathBuf]) -> String {
    let root = root.display();
    let files = existing
        .iter()
        .map(|path| format!("- `{}`", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    formatdoc! {"
        Please analyze this codebase and improve its project instructions, which future AI coding assistants read when working on it. They live in these files in `{root}`:

        {files}

        Read each file, but do not edit any file in this turn. Show the changes you propose as a unified diff in a `diff` code block, explain why each change helps, then end your turn. Apply the changes only after the user confirms them. If nothing needs to change, say so. Update each file in place, without moving content between files.

        {RUBRIC}"
    }
}

// ── PickerItem ──

/// Each file `/init` can create, with the tools besides oxide-code that read it.
const FILE_CHOICES: [(&str, &str); 2] = [
    ("AGENTS.md", "Also read by Codex and other agents"),
    ("CLAUDE.md", "Also read by Claude Code"),
];

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
        let list = ListPicker::new("Create instruction file", rows).with_description(
            "This project has no AGENTS.md or CLAUDE.md. Pick the file the model writes.",
        );
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
        ModalKey::Submitted(ModalAction::Dispatch(prompt(create_prompt(
            &self.root, row.name,
        ))))
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

    mod init_cmd {
        use super::*;

        #[test]
        fn metadata_matches_built_ins_contract() {
            assert_eq!(InitCmd.name(), "init");
            assert_eq!(InitCmd.aliases(), [] as [&str; 0]);
            assert!(InitCmd.usage().is_none());
        }

        mod classify {
            use super::*;

            #[test]
            fn is_mutating() {
                assert_eq!(InitCmd.classify(""), SlashKind::Mutating);
            }
        }

        mod execute {
            use super::*;

            fn execute_in(project_root: &Path) -> SlashOutcome {
                let mut info = test_session_info();
                info.project_root = project_root.to_owned();
                InitCmd
                    .execute("", &SlashContext::new(&info, None))
                    .unwrap()
            }

            #[test]
            fn without_an_instruction_file_opens_the_picker() {
                let root = tempfile::tempdir().unwrap();

                let mut picker = execute_in(root.path()).into_modal();

                assert_eq!(
                    press(picker.as_mut(), KeyCode::Enter),
                    Some(prompt(create_prompt(root.path(), "AGENTS.md"))),
                );
            }

            #[test]
            fn with_an_instruction_file_forwards_the_update_prompt() {
                let root = tempfile::tempdir().unwrap();
                fs::write(root.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();

                let action = execute_in(root.path()).into_forward();

                assert_eq!(
                    action,
                    prompt(update_prompt(root.path(), &[PathBuf::from("AGENTS.md")])),
                );
            }
        }
    }

    mod existing_instruction_files {
        use super::*;

        #[test]
        fn finds_root_and_instruction_dir_files() {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            fs::write(root.path().join("CLAUDE.md"), "").unwrap();
            fs::write(root.path().join(".claude/AGENTS.md"), "").unwrap();
            fs::create_dir(root.path().join("AGENTS.md")).unwrap();

            assert_eq!(
                existing_instruction_files(root.path()),
                [
                    PathBuf::from("CLAUDE.md"),
                    Path::new(".claude").join("AGENTS.md")
                ],
            );
        }

        #[cfg(unix)]
        #[test]
        fn lists_a_symlinked_file_once_under_its_real_path() {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("AGENTS.md"), "").unwrap();
            symlink("AGENTS.md", root.path().join("CLAUDE.md")).unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            symlink("../AGENTS.md", root.path().join(".claude/CLAUDE.md")).unwrap();

            assert_eq!(
                existing_instruction_files(root.path()),
                [PathBuf::from("AGENTS.md")],
            );
        }
    }

    mod create_prompt {
        use super::*;

        #[test]
        fn names_the_file_in_its_header() {
            let prompt = create_prompt(Path::new("/repo"), "CLAUDE.md");

            assert_eq!(
                prompt,
                formatdoc! {"
                    Please analyze this codebase and create `CLAUDE.md` in `/repo`. Future AI coding assistants will read it as project instructions when working on this codebase.

                    {RUBRIC}

                    Prefix the file with:

                    ```
                    # CLAUDE.md

                    This file provides guidance to AI coding assistants when working with code in this repository.
                    ```"
                },
            );
        }
    }

    mod update_prompt {
        use super::*;

        #[test]
        fn lists_every_existing_file_and_defers_edits() {
            let existing = [
                PathBuf::from("CLAUDE.md"),
                Path::new(".claude").join("AGENTS.md"),
            ];

            let prompt = update_prompt(Path::new("/repo"), &existing);

            let nested = existing[1].display();
            assert_eq!(
                prompt,
                formatdoc! {"
                    Please analyze this codebase and improve its project instructions, which future AI coding assistants read when working on it. They live in these files in `/repo`:

                    - `CLAUDE.md`
                    - `{nested}`

                    Read each file, but do not edit any file in this turn. Show the changes you propose as a unified diff in a `diff` code block, explain why each change helps, then end your turn. Apply the changes only after the user confirms them. If nothing needs to change, say so. Update each file in place, without moving content between files.

                    {RUBRIC}"
                },
            );
        }
    }

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

        fn create_action(file: &str) -> Action {
            prompt(create_prompt(Path::new("/repo"), file))
        }

        #[test]
        fn enter_creates_the_highlighted_file() {
            let mut picker = InitPicker::new(Path::new("/repo"));
            assert_eq!(
                press(&mut picker, KeyCode::Enter),
                Some(create_action("AGENTS.md"))
            );

            for code in [
                KeyCode::Down,
                KeyCode::Char('j'),
                KeyCode::Up,
                KeyCode::Char('k'),
                KeyCode::Char('2'),
            ] {
                let mut picker = InitPicker::new(Path::new("/repo"));
                assert_eq!(press(&mut picker, code), None, "{code:?} moves the cursor");
                assert_eq!(
                    press(&mut picker, KeyCode::Enter),
                    Some(create_action("CLAUDE.md")),
                    "{code:?} from the first row lands on the second",
                );
            }
        }

        #[test]
        fn other_keys_keep_the_picker_open_and_the_cursor_in_place() {
            let mut picker = InitPicker::new(Path::new("/repo"));
            for code in [KeyCode::Tab, KeyCode::Char('x'), KeyCode::Char('9')] {
                assert_eq!(press(&mut picker, code), None, "{code:?}");
            }
            assert_eq!(
                press(&mut picker, KeyCode::Enter),
                Some(create_action("AGENTS.md"))
            );
        }
    }
}
