//! `/init`'s typed form, which finds the project's instruction files and builds the prompt asking
//! the model to create one or to propose changes to the existing ones as a diff.
//!
//! Design: `docs/design/slash/init.md`.

use std::path::{Path, PathBuf};

use indoc::{formatdoc, indoc};

use crate::prompt::instructions::{self, AGENTS_MD, CLAUDE_MD};
use crate::protocol::Op;

/// Each file `/init` can create, with the tools besides oxide-code that read it.
pub const FILE_CHOICES: [(&str, &str); 2] = [
    (AGENTS_MD, "Also read by Codex and other agents"),
    (CLAUDE_MD, "Also read by Claude Code"),
];

/// The sentence saying the project has none of the [`FILE_CHOICES`].
pub fn no_file_sentence() -> String {
    let [(first, _), (second, _)] = FILE_CHOICES;
    format!("This project has no {first} or {second}.")
}

/// [`resolve`] for a frontend without a picker, where a bare `/init` in a project with no
/// instruction file errors naming both files.
pub(super) fn parse(args: &str, root: &Path) -> Result<Op, String> {
    resolve(args, root)?.ok_or_else(|| {
        let no_file = no_file_sentence();
        let [(first, _), (second, _)] = FILE_CHOICES;
        format!("{no_file} Run `/init {first}` or `/init {second}` to create one.")
    })
}

/// The op `/init [<file>]` runs at the project `root`: the create prompt for `<file>` when the
/// project has no instruction file, or else the update prompt, which a named `<file>` gets only
/// when one of the existing files has that name. `<file>` matches ignoring ASCII case. `None` for
/// a bare `/init` in a project with no instruction file, which must first pick the file to create.
pub fn resolve(args: &str, root: &Path) -> Result<Option<Op>, String> {
    let file = match args.trim() {
        "" => None,
        arg => Some(file_choice(arg)?),
    };
    let existing = existing_instruction_files(root);
    let prompt = match (file, existing.as_slice()) {
        (None, []) => return Ok(None),
        (Some(file), []) => create_prompt(root, file),
        (Some(file), _) if !existing.iter().any(|path| path.ends_with(file)) => {
            return Err(already_has(&existing));
        }
        _ => update_prompt(root, &existing),
    };
    Ok(Some(Op::Prompt(prompt)))
}

/// The [`FILE_CHOICES`] name that `arg` matches ignoring ASCII case, or the error listing them.
fn file_choice(arg: &str) -> Result<&'static str, String> {
    let names = FILE_CHOICES.map(|(name, _)| name);
    names
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case(arg))
        .ok_or_else(|| {
            let valid = names.join(", ");
            format!("Unknown instruction file: `{arg}`. Valid: {valid}.")
        })
}

/// The files at `root` that hold instructions, as root-relative paths in the order loading tries
/// them. Paths that resolve to the same file are listed once, under a path that is not a
/// symlink when one exists.
fn existing_instruction_files(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in instructions::project_candidates(Path::new("")) {
        let full = root.join(&path);
        let Ok(Some(instructions::Found { canonical, .. })) = instructions::read(&full) else {
            continue;
        };
        match files.iter_mut().find(|(_, seen)| *seen == canonical) {
            None => files.push((path, canonical)),
            Some((kept, _)) if root.join(&*kept).is_symlink() && !full.is_symlink() => *kept = path,
            Some(_) => {}
        }
    }
    files.into_iter().map(|(path, _)| path).collect()
}

/// The error for naming a file to create in a project that has the `existing` instruction files.
fn already_has(existing: &[PathBuf]) -> String {
    let files = existing
        .iter()
        .map(|path| format!("`{}`", path.display()))
        .collect::<Vec<_>>()
        .join(" and ");
    let them = if existing.len() == 1 { "it" } else { "them" };
    format!("This project already has {files}. Run `/init` without a file name to update {them}.")
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
pub fn create_prompt(root: &Path, file: &str) -> String {
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    mod parse {
        use super::*;

        #[test]
        fn bare_form_without_an_instruction_file_names_both_files() {
            let root = tempfile::tempdir().unwrap();

            assert_eq!(
                parse("  ", root.path()),
                Err("This project has no AGENTS.md or CLAUDE.md. \
                     Run `/init AGENTS.md` or `/init CLAUDE.md` to create one."
                    .to_owned()),
            );
        }
    }

    mod resolve {
        use super::*;

        #[test]
        fn bare_form_with_instruction_files_asks_for_their_update() {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("AGENTS.md"), "# AGENTS.md\n").unwrap();

            assert_eq!(
                resolve("", root.path()),
                Ok(Some(Op::Prompt(update_prompt(
                    root.path(),
                    &[PathBuf::from("AGENTS.md")],
                )))),
            );
        }

        #[test]
        fn file_name_of_an_existing_file_asks_for_the_update() {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            fs::write(root.path().join(".claude/CLAUDE.md"), "# CLAUDE.md\n").unwrap();

            assert_eq!(
                resolve("Claude.md", root.path()),
                Ok(Some(Op::Prompt(update_prompt(
                    root.path(),
                    &[Path::new(".claude").join("CLAUDE.md")],
                )))),
            );
        }

        #[test]
        fn file_name_without_an_instruction_file_asks_to_create_it() {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("CLAUDE.md"), "\n").unwrap();

            for (arg, file) in [("AGENTS.md", "AGENTS.md"), (" claude.md ", "CLAUDE.md")] {
                assert_eq!(
                    resolve(arg, root.path()),
                    Ok(Some(Op::Prompt(create_prompt(root.path(), file)))),
                    "{arg}",
                );
            }
        }

        #[test]
        fn bare_form_without_an_instruction_file_leaves_the_file_to_pick() {
            let root = tempfile::tempdir().unwrap();

            assert_eq!(resolve("", root.path()), Ok(None));
        }

        #[test]
        fn file_name_beside_other_instruction_files_names_them() {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            fs::write(root.path().join(".claude/CLAUDE.md"), "# CLAUDE.md\n").unwrap();
            let nested = Path::new(".claude").join("CLAUDE.md");
            let nested = nested.display();

            assert_eq!(
                resolve("AGENTS.md", root.path()),
                Err(format!(
                    "This project already has `{nested}`. \
                     Run `/init` without a file name to update it."
                )),
            );

            fs::write(root.path().join("CLAUDE.md"), "# CLAUDE.md\n").unwrap();
            assert_eq!(
                resolve("AGENTS.md", root.path()),
                Err(format!(
                    "This project already has `CLAUDE.md` and `{nested}`. \
                     Run `/init` without a file name to update them."
                )),
            );
        }

        #[test]
        fn unknown_file_name_lists_the_valid_ones() {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("AGENTS.md"), "").unwrap();

            assert_eq!(
                resolve("README.md", root.path()),
                Err(
                    "Unknown instruction file: `README.md`. Valid: AGENTS.md, CLAUDE.md."
                        .to_owned()
                ),
            );
        }
    }

    mod existing_instruction_files {
        use super::*;

        #[test]
        fn lists_files_with_instructions_in_loading_order() {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            fs::write(root.path().join(".claude/CLAUDE.md"), "Nested rules.").unwrap();
            fs::write(root.path().join(".claude/AGENTS.md"), "Unread rules.").unwrap();
            fs::write(root.path().join("CLAUDE.md"), "  \n").unwrap();
            fs::write(root.path().join("AGENTS.md"), "Rules.").unwrap();

            assert_eq!(
                existing_instruction_files(root.path()),
                [
                    PathBuf::from("AGENTS.md"),
                    Path::new(".claude").join("CLAUDE.md")
                ],
            );
        }

        #[cfg(unix)]
        #[test]
        fn lists_a_symlinked_file_once_under_its_real_path() {
            use std::os::unix::fs::symlink;

            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("CLAUDE.md"), "Rules.").unwrap();
            symlink("CLAUDE.md", root.path().join("AGENTS.md")).unwrap();
            fs::create_dir(root.path().join(".claude")).unwrap();
            symlink("../CLAUDE.md", root.path().join(".claude/CLAUDE.md")).unwrap();

            assert_eq!(
                existing_instruction_files(root.path()),
                [PathBuf::from("CLAUDE.md")],
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
}
