# Init

`/init` asks the model to write or update the project's instruction file, `AGENTS.md` or `CLAUDE.md`, which later sessions load as [project instructions](../../guide/instructions.md).

## Implementation

`/init` looks for the files that loading tries at the project root, in the same order: `AGENTS.md`, `CLAUDE.md`, and `.claude/CLAUDE.md`. As when loading, only a file with content counts, so an empty `CLAUDE.md` leaves the project without an instruction file, and neither does a symlink that loading skips, such as one that resolves outside the project root or to a file without the `.md` extension. Paths that resolve to the same file, such as a `CLAUDE.md` symlink to `AGENTS.md`, count once, under the path that is not a symlink. The project root is the nearest directory at or above the working directory that holds `.git`, or the working directory when outside a repository. At startup, the TUI and the `--no-tui` REPL each resolve it once through `util::git::project_root`, the same lookup config and instruction loading use. The working directory stays fixed for the session, so the root cannot change.

- **A file exists.** Bare `/init` asks no questions. It starts a turn with a prompt that names each existing file and asks the model to read it, explore the codebase, show proposed changes as a unified diff with a reason for each, and end the turn. The model edits the file only after the user confirms in a later prompt.
- **No file exists.** `/init AGENTS.md` or `/init CLAUDE.md` starts a turn with a prompt that names the file to create, matching the name ignoring case. Besides oxide-code, Codex and other agents that follow the `AGENTS.md` convention read `AGENTS.md`, and Claude Code reads `CLAUDE.md`. Bare `/init` needs that choice first. In the TUI, it opens a picker whose choice sends the same prompt. Pressing Esc closes the picker without starting a turn, and the `/init` line stays in the chat, since bare `/init` echoes like a typed command ([commands.md](commands.md) decision 13). The REPL has no picker, so it rejects bare `/init` with an error naming both commands.
- **A file name with a file present.** Naming a file the project has, as `/init CLAUDE.md` does when `.claude/CLAUDE.md` exists, sends the update prompt that bare `/init` sends. Naming the other file is rejected with the existing paths and a pointer to bare `/init`. A name other than `AGENTS.md` or `CLAUDE.md` is rejected with a list of the two.

`command::init` holds the discovery, both prompts, and `resolve`, which turns the arguments into the prompt to send or reports that the file must be picked first. The TUI's `slash/init.rs` adds only the picker, whose choice runs as the line `/init <file>`. Every `/init` prompt carries its command line, typed or picked, as the session's first-prompt title, so `ox --list` shows `/init AGENTS.md` until the TUI's AI title replaces it. Because `-p` runs no slash commands, it sends `/init` to the model as text.

Both prompts share one rubric: include only information an agent would otherwise get wrong, such as commands that differ from language defaults, architecture spanning several files, conventions that diverge from defaults, and constraints that the code cannot reveal. Omit standard conventions, file listings, generic advice, and anything not grounded in files the model has read.

## Design Decisions

1. **Ask only for the file name.** The model can learn the commands, conventions, and architecture by reading the repository. If neither file exists, the repository cannot determine which name to use: either works, and the choice depends on which other tools the team runs. If a file already exists, updating it in place is the only reasonable choice, so there is nothing to ask. Naming that file runs the same update, while naming the other one is an error, since creating it would add the second file decision 2 rules out.

2. **No option to create both.** oxide-code loads one file per directory, the first with content among `AGENTS.md`, `CLAUDE.md`, and `.claude/CLAUDE.md`, and does not resolve `@` imports. A second file beside the first would never reach an oxide-code session and would drift out of sync.

3. **The diff is a prompt contract.** In `manual` mode, edits inside the workspace run without approval, so the permission gate could let the model overwrite an existing instruction file without showing anything. The update prompt therefore asks the model to show the diff and stop the turn before editing, and Claude Code's newer `/init` likewise asks for diffs on an existing `CLAUDE.md`. This depends on the model following the prompt. Protecting instruction files would enforce the contract, but would require approval for every edit to them in every session, and headless runs could edit them only with that protection turned off.

4. **The answer comes before the turn.** oxide-code has no tool for the model to ask the user a question, so the file name arrives with the command, typed as its argument or chosen in the TUI's picker. The prompt then includes the answer, and the turn needs no further input.

## Out of Scope

- **Enforcing the diff.** A guard scoped to the `/init` turn could require approval for edits to instruction files, ensuring the approval preview shows the diff regardless of what the model does. This requires per-turn state in the permission gate, tracked in [#261](https://github.com/hakula139/oxide-code/issues/261).

## Sources

- `crates/ox-core/src/command.rs`: `parse`, which routes `/init` to `command::init`, and `typed_op`, which titles the REPL's prompt.
- `crates/ox-core/src/command/init.rs`: `FILE_CHOICES`, `resolve`, the instruction-file discovery, and the create and update prompts.
- `crates/ox-core/src/prompt/instructions.rs`: `project_candidates` and `read_project`, the files `/init` checks and the rule for which of them count.
- `crates/ox-core/src/util/git.rs`: `project_root`, the lookup for the project root.
- `crates/ox-tui/src/lib.rs`: `run`, which resolves the project root and stores it in `LiveSessionInfo`.
- `crates/ox-tui/src/session_info.rs`: `LiveSessionInfo::project_root`.
- `crates/ox-tui/src/app/queue.rs`: `QueuedPrompt::title`, which titles the TUI's prompt.
- `crates/ox-tui/src/slash/init.rs`: `InitCmd` and the file picker.
- `crates/ox/src/driver/repl.rs`: resolves the project root and runs `/init` in the bare REPL.
