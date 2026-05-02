//! `/init` — synthesizes a prompt asking the model to author or
//! update the project's `AGENTS.md` / `CLAUDE.md`. Returns
//! [`SlashOutcome::PromptSubmit`]; the dispatcher forwards the body
//! to the agent loop. See `docs/research/design/slash-commands/init.md`.

use indoc::indoc;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome};

pub(super) struct InitCmd;

impl SlashCommand for InitCmd {
    fn name(&self) -> &'static str {
        "init"
    }

    fn description(&self) -> &'static str {
        "Generate or update the project's `AGENTS.md` / `CLAUDE.md` instruction file"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn execute(&self, _: &str, _: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::PromptSubmit(PROMPT.to_owned()))
    }
}

/// Body forwarded to the agent loop on `/init`. Adapted from Claude
/// Code's `OLD_INIT_PROMPT` for the AGENTS.md / CLAUDE.md convention.
const PROMPT: &str = indoc! {r"
    Please analyze this codebase and create an `AGENTS.md` file at the project root that future AI coding assistants (oxide-code, Claude Code, Codex, etc.) will read when working on it.

    If `AGENTS.md` or `CLAUDE.md` already exists, do not overwrite it. Read it, propose specific improvements as a diff, and explain why each change matters. Prefer updating the file with the broader scope (`AGENTS.md`) when both exist.

    Include only what an agent would get wrong without it:
    1. Build / lint / test commands the agent can't infer from manifest files. Include any flags or sequences that differ from the language defaults (e.g., how to run a single test).
    2. High-level architecture that requires reading multiple files to understand — modules, layering, ownership, and the data flow between them.
    3. Project-specific conventions that diverge from language defaults (import grouping, error-handling style, naming, blank-line rules).
    4. Non-obvious gotchas — required env vars, platform constraints, workflow quirks, or constraints not obvious from the code.

    Exclude:
    - Standard language conventions the agent already knows (`cargo test`, `npm test`, etc.).
    - File-by-file structure or component lists — these are discoverable via `glob` / `ls`.
    - Generic development advice (`write tests`, `handle errors`).
    - Information that changes frequently — reference the source file by relative path so the agent reads the current version.

    Be specific. `Use 2-space indentation in TypeScript` is better than `Format code properly`. Every line should answer `what would a fresh agent get wrong without this?` — if the answer is `nothing`, cut the line.

    If a `README.md`, `.cursor/rules/`, `.cursorrules`, or `.github/copilot-instructions.md` exists, fold the load-bearing parts into `AGENTS.md` instead of duplicating them.

    Prefix the file with:

    ```
    # AGENTS.md

    This file provides guidance to AI coding assistants (oxide-code, Claude Code, Codex, etc.) when working with code in this repository.
    ```
"};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::{test_session_info, test_user_tx};
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn run_execute() -> (Result<SlashOutcome, String>, ChatView) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let (user_tx, _user_rx) = test_user_tx();
        let result = InitCmd.execute("", &mut SlashContext::new(&mut chat, &info, &user_tx));
        (result, chat)
    }

    // ── InitCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(InitCmd.name(), "init");
        assert!(InitCmd.aliases().is_empty());
        assert!(!InitCmd.description().is_empty());
        assert!(InitCmd.usage().is_none());
    }

    #[test]
    fn is_read_only_is_false_so_busy_dispatch_refuses_init() {
        // Override is load-bearing: a parallel turn would race the
        // in-flight one over `messages` / the session writer.
        assert!(!InitCmd.is_read_only());
    }

    // ── InitCmd::execute ──

    #[test]
    fn execute_returns_prompt_submit_with_non_empty_body() {
        let (result, _chat) = run_execute();
        let SlashOutcome::PromptSubmit(prompt) =
            result.expect("/init must succeed when nothing is wrong with ctx")
        else {
            panic!("/init must return PromptSubmit, not Local");
        };
        assert!(!prompt.is_empty(), "/init prompt must not be empty");
    }

    #[test]
    fn execute_does_not_push_chat_blocks() {
        // The agent loop's response stream is the only block source —
        // an extra push here would land before the typed `/init` row.
        let (_result, chat) = run_execute();
        assert_eq!(chat.entry_count(), 0);
    }

    #[test]
    fn execute_prompt_targets_agents_md_and_claude_md() {
        let (result, _chat) = run_execute();
        let SlashOutcome::PromptSubmit(prompt) = result.unwrap() else {
            unreachable!("see test above");
        };
        assert!(prompt.contains("AGENTS.md"), "prompt missing AGENTS.md");
        assert!(prompt.contains("CLAUDE.md"), "prompt missing CLAUDE.md");
    }

    #[test]
    fn execute_prompt_says_do_not_overwrite_existing_file() {
        // The "don't clobber existing instructions" rule is load-bearing.
        let (result, _chat) = run_execute();
        let SlashOutcome::PromptSubmit(prompt) = result.unwrap() else {
            unreachable!("see test above");
        };
        assert!(
            prompt.contains("not overwrite") || prompt.contains("not silently overwrite"),
            "prompt must instruct the model not to overwrite an existing file: {prompt}",
        );
    }
}
