//! `/init` — synthesizes a prompt asking the model to author or update the project's
//! `AGENTS.md` / `CLAUDE.md`.

use indoc::indoc;

use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;

pub(super) struct InitCmd;

impl SlashCommand for InitCmd {
    fn name(&self) -> &'static str {
        "init"
    }

    fn description(&self) -> &'static str {
        "Generate or update the project's `AGENTS.md` / `CLAUDE.md`"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        SlashKind::Mutating
    }

    fn execute(&self, _args: &str, _ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        Ok(SlashOutcome::Forward(UserAction::SubmitPrompt(
            PROMPT.to_owned(),
        )))
    }
}

/// System prompt template for `/init` — instructs the model to author a fresh `AGENTS.md` (or
/// propose a diff against an existing one) grounded in files it actually reads, with explicit
/// exclusion rules to avoid generic / boilerplate output.
const PROMPT: &str = indoc! {r"
    Please analyze this codebase and create an `AGENTS.md` file at the project
    root that future AI coding assistants will read when working on it.

    If neither `AGENTS.md` nor `CLAUDE.md` exists, create `AGENTS.md`. If one
    already exists, do not overwrite it — propose specific improvements as a
    diff and explain why each change matters. If both exist, update each in
    place rather than migrating between them.

    Include only what an agent would get wrong without it:

    1. Build / lint / test commands the agent can't infer from manifest files.
       Include any flags or sequences that differ from the language defaults
       (e.g., how to run a single test).
    2. High-level architecture that requires reading multiple files to
       understand — modules, layering, ownership, and the data flow between
       them.
    3. Project-specific conventions that diverge from language defaults
       (import grouping, error-handling style, naming, blank-line rules).
    4. External constraints the code can't reveal — required env vars,
       platform-only behavior, services that must be running, workflow steps
       the agent can't infer.

    Exclude:

    - Standard language conventions the agent already knows (`cargo test`,
      `npm test`, etc.).
    - File-by-file structure or component lists — these are discoverable via
      `glob` / `ls`.
    - Generic development advice (`write tests`, `handle errors`).
    - Information that changes frequently — reference the source file by
      relative path so the agent reads the current version.
    - Sections you can't ground in files you actually read (no fabricated
      `Common Tasks`, `Tips for Development`, or `Support` headers).

    Be specific. `Use 2-space indentation in TypeScript` is better than `Format
    code properly`. Don't restate the same fact in multiple sections. Every
    line should answer `what would a fresh agent get wrong without this?` —
    if the answer is `nothing`, cut the line.

    If a `README.md`, `.cursor/rules/`, `.cursorrules`, or
    `.github/copilot-instructions.md` exists, extract the load-bearing parts
    (commands, conventions, gotchas) and merge them into `AGENTS.md` without
    duplication. Skip prose that restates language defaults.

    Prefix the file with:

    ```
    # AGENTS.md

    This file provides guidance to AI coding assistants (Claude Code, Codex, oxide-code, and others) when working with code in this repository.
    ```
"};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;
    use crate::tui::theme::Theme;

    fn run_execute() -> (ChatView, Result<SlashOutcome, String>) {
        let mut chat = ChatView::new(&Theme::default(), false);
        let info = test_session_info();
        let outcome = InitCmd.execute("", &mut SlashContext::new(&mut chat, &info));
        (chat, outcome)
    }

    // ── InitCmd metadata ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(InitCmd.name(), "init");
        assert!(InitCmd.aliases().is_empty());
        assert!(InitCmd.usage().is_none());
    }

    #[test]
    fn classify_is_mutating() {
        assert_eq!(InitCmd.classify(""), SlashKind::Mutating);
    }

    // ── InitCmd::execute ──

    #[test]
    fn execute_does_not_push_chat_blocks() {
        let (chat, _outcome) = run_execute();
        assert_eq!(chat.entry_count(), 0);
    }

    #[test]
    fn execute_prompt_targets_agents_md_and_claude_md() {
        let (_chat, outcome) = run_execute();
        assert!(
            matches!(
                &outcome,
                Ok(SlashOutcome::Forward(UserAction::SubmitPrompt(p)))
                    if p.contains("AGENTS.md") && p.contains("CLAUDE.md")
            ),
            "prompt must target both AGENTS.md and CLAUDE.md: {outcome:?}",
        );
    }

    #[test]
    fn execute_prompt_says_do_not_overwrite_existing_file() {
        let (_chat, outcome) = run_execute();
        assert!(
            matches!(
                &outcome,
                Ok(SlashOutcome::Forward(UserAction::SubmitPrompt(p)))
                    if p.contains("already exists") && p.contains("not overwrite")
            ),
            "prompt must instruct the model not to overwrite an existing file: {outcome:?}",
        );
    }
}
