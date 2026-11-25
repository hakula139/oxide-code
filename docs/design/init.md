# Init

`/init` asks the model to create or update the project's instruction file (`AGENTS.md` or `CLAUDE.md`) loaded by later sessions as [project instructions](../guide/instructions.md).

## File Resolution

At startup, the TUI resolves the project root once via `util::path::project_root` (nearest directory at or above cwd containing `.git`, or cwd outside a repo), matching config and instruction loading. Because cwd is fixed per session, the root never changes.

`/init` checks the root candidates in load order (`AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md`), counting only non-empty files that loading accepts (excluding symlinks resolving outside the root) and deduplicating symlinks to the same file under the non-symlink path:

- **A file exists.** Bare `/init` starts a turn naming each existing file and asking the model to read it, explore the repo, present proposed edits as a unified diff with reasons, and stop until the user confirms.
- **No file exists.** `/init AGENTS.md` or `/init CLAUDE.md` (case-insensitive) starts a creation turn (`AGENTS.md` is shared with Codex and similar tools, `CLAUDE.md` with Claude Code). Bare `/init` opens a TUI picker for that choice. Esc closes it without starting a turn, leaving the echoed `/init` line in chat ([**A command suppresses its echo when its response stands alone**](slash-commands.md#design-decisions)).
- **Named file when one exists.** Naming an existing file (including `/init CLAUDE.md` when `.claude/CLAUDE.md` exists) runs the update prompt. Naming the other file errors with the existing paths and a pointer to bare `/init`, and unknown names error listing the two choices.

`command::init` owns discovery, both prompts, and `resolve`. `slash/init.rs` adds only the picker, which runs `/init <file>`. Every `/init` prompt uses its command line as the initial session title (until AI titling replaces it). `-p` ignores slash commands and sends `/init` as plain text.

Both prompts share one rubric: include only non-obvious project facts (non-default commands, cross-file architecture, custom conventions, constraints not visible in code) grounded in read files, omitting standard conventions, file listings, and generic advice.

## Design Decisions

1. **Ask only for the file name.** The model discovers repo details by reading files. When no file exists, only the user knows which tool ecosystem they target, while an existing file has only one sensible action: updating it in place and rejecting the alternate name (**No option to create both**).
2. **No option to create both.** `ox` loads only the first non-empty file per directory among `AGENTS.md`, `CLAUDE.md`, and `.claude/CLAUDE.md` without `@` imports, so a second file would be ignored and drift.
3. **The diff is a prompt contract.** Under allow rules, `auto`, or `yolo`, edits can run without approval, so the update prompt asks the model to show a diff and end the turn first (matching Claude Code's `/init` diff prompt). Enforcing via instruction-file protection would force approval on every edit in every session and complicate headless runs.
4. **The answer comes before the turn.** Selecting from the fixed `FILE_CHOICES` via argument or TUI picker avoids burning a model turn just to pose the question through `ask`.

## Out of Scope

- **Enforcing the diff.** Scoping a mandatory approval guard to `/init` turns requires per-turn permission-gate state ([#261](https://github.com/hakulab/oxide-code/issues/261)).

## Sources

- `crates/ox-core/src/command.rs`: `typed_form`, which routes `/init` to `command::init`.
- `crates/ox-core/src/command/init.rs`: `FILE_CHOICES`, `resolve`, the instruction-file discovery, and the create and update prompts.
- `crates/ox-core/src/prompt/instructions.rs`: `project_candidates` and `read_project`, the files `/init` checks and the rule for which of them count.
- `crates/ox-core/src/util/path.rs`: `project_root`, the lookup for the project root.
- `crates/ox-tui/src/app/queue.rs`: `QueuedPrompt::title`, which titles the TUI's prompt.
- `crates/ox-tui/src/lib.rs`: `run`, which resolves the project root and stores it in `LiveSessionInfo`.
- `crates/ox-tui/src/session_info.rs`: `LiveSessionInfo::project_root`.
- `crates/ox-tui/src/slash/init.rs`: `InitCmd` and the file picker.
