# Init

`/init` asks the model to write or update the project's instruction file, `AGENTS.md` or `CLAUDE.md`, which later sessions load as [project instructions](../../guide/instructions.md).

## Implementation

`/init` looks for `AGENTS.md` and `CLAUDE.md` at the project root and in its `.claude/` directory, the two locations checked when loading root-level instructions. Paths that resolve to the same file, such as a `CLAUDE.md` symlink to `AGENTS.md`, count once, under the path that is not a symlink. The project root is the Git repository root, or the working directory when outside a repository. At startup, the TUI driver resolves it once through `util::git::repo_root`, the same lookup prompt assembly uses, and stores it in `LiveSessionInfo`. The working directory stays fixed for the session, so the root cannot change.

- **No file exists.** A picker asks which file to create. Besides oxide-code, Codex and other agents that follow the `AGENTS.md` convention read `AGENTS.md`, and Claude Code reads `CLAUDE.md`. Choosing a file forwards a prompt that names it. Pressing Esc closes the picker without starting a turn, and the `/init` line stays in the chat, as the command line does when the `/model` or `/theme` picker is cancelled. If auto-compaction clears the chat during a turn started from the picker, the `/init` line is not replayed above the summary.
- **A file exists.** `/init` asks no questions. It forwards a prompt that names each existing file and asks the model to read it, explore the codebase, show proposed changes as a unified diff with a reason for each, and end the turn. The model edits the file only after the user confirms in a later prompt.

Both prompts share one rubric: include only information an agent would otherwise get wrong, such as commands that differ from language defaults, architecture spanning several files, conventions that diverge from defaults, and constraints that the code cannot reveal. Omit standard conventions, file listings, generic advice, and anything not grounded in files the model has read.

## Design Decisions

1. **Ask only for the file name.** The model can learn the commands, conventions, and architecture by reading the repository. If neither file exists, the repository cannot determine which name to use: either works, and the choice depends on which other tools the team runs. If a file already exists, updating it in place is the only reasonable choice, so there is nothing to ask.

2. **No option to create both.** oxide-code loads the first file with content at each location, checking `CLAUDE.md` before `AGENTS.md`, and does not resolve `@` imports. A second file beside the first would never reach an oxide-code session and would drift out of sync.

3. **The diff is a prompt contract.** In `manual` mode, edits inside the workspace run without approval, so the permission gate could let the model overwrite an existing instruction file without showing anything. The update prompt therefore asks the model to show the diff and stop the turn before editing, and Claude Code's newer `/init` likewise asks for diffs on an existing `CLAUDE.md`. This depends on the model following the prompt. Protecting instruction files would enforce the contract, but would require approval for every edit to them in every session, and headless runs could edit them only with that protection turned off.

4. **The picker is client-side.** oxide-code has no tool for the model to ask the user a question, so the TUI presents the question in a modal before the turn starts. The prompt then includes the answer, and the turn needs no further input.

## Out of Scope

- **Enforcing the diff.** A guard scoped to the `/init` turn could require approval for edits to instruction files, ensuring the approval preview shows the diff regardless of what the model does. This requires per-turn state in the permission gate, tracked in [#261](https://github.com/hakula139/oxide-code/issues/261).
- **Other frontends.** `/init` runs only in the TUI. The `--no-tui` REPL rejects it like every command without a typed form, and `-p` sends `/init` to the model as plain text.

## Sources

- `crates/oxide-code/src/driver/tui.rs`: resolves the project root and stores it in `LiveSessionInfo`.
- `crates/oxide-code/src/prompt/instructions.rs`: `INSTRUCTION_FILENAMES` and `INSTRUCTION_DIRS`, the files and directories checked by `/init`.
- `crates/oxide-code/src/slash/context.rs`: `LiveSessionInfo::project_root`.
- `crates/oxide-code/src/slash/init.rs`: `InitCmd`, the file picker, and the create and update prompts.
- `crates/oxide-code/src/util/git.rs`: `repo_root`, the async lookup for the repository root.
