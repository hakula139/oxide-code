# Slash Commands

A slash command is an input line starting with `/` that the TUI handles by opening a modal, printing a note, forwarding an op, or starting a turn with a generated prompt. [Slash Commands](../guide/slash-commands.md) lists all commands, and [Compaction](compaction.md), [Init](init.md), [Login](login.md), [Modals](modals.md), [Permissions](permissions.md#mode-switches), [Persistence](persistence.md#archiving), and [Resume](resume.md) cover dedicated mechanisms.

## Registry and Completion

Each command (or pair like `/login` and `/logout`) implements `SlashCommand` in `slash/<name>.rs`, listed alphabetically in `BUILT_INS`. `ox-core::command` owns the command table, aliases, and typed-argument parsers.

The completion popup shows names and descriptions ranked by name prefix → alias prefix → name substring → alias substring (alphabetical within each tier). It parses via the same `split_slash` as `parse_slash` (supporting namespaced names like `/plugin:cmd`) and displays dim usage text after a space until an argument begins.

## `/diff`

`/diff` writes uncommitted changes and untracked files to chat as a `GitDiffBlock` (needing scrollback a modal would crop):

- **Git calls.** Runs from the resolved work-tree root (so `ls-files` sees the full tree), diffing against `HEAD` (or combining staged and unstaged diffs before the first commit), listing untracked entries with `ls-files --directory --no-empty-directory`, skipping optional locks, and pinning config such as `core.quotePath` and external diff drivers.
- **Output and errors.** Parses once into typed `WorkingTreeDiff` rows capped at 64 KiB on a line boundary (naming the git command for truncated output). Repository rejections such as dubious ownership pass through git's message and fix.

## `/delete`

`/delete <id-prefix>` sends `Op::FindSaved` → `SavedFound` to open `ConfirmDeleteSessionModal` ([Modals](modals.md)) or report ambiguity / miss (Ctrl+D in `/resume` opens the same modal). Confirming sends `Op::DeleteSaved` → `SavedDeleted`, removing rows and artifacts. Three layers block deleting the live session (picker filtering, `saved::find` prefix resolution, and `saved::delete` ID check), and the store rejects sessions held by another process ([Liveness Locks](persistence.md#liveness-locks)).

## Design Decisions

1. **Trait registry.** Mirrors `tool::Tool`: adding a command is one file and one `BUILT_INS` entry with no central match.
2. **Command output reuses the chat's blocks.** Notes use `SystemMessageBlock` and errors use `ErrorBlock`, with `GitDiffBlock` for `/diff` as the sole custom block.
3. **One return type carries every effect.** Commands read a borrowed `LiveSessionInfo` and return `SlashOutcome`, applied centrally in `App` for both idle and busy paths so commands never mutate `ChatView` or `ModalStack`.
4. **Only commands that can start a turn or an idle-only op wait for idle.** `idle_only` applies when any form builds a prompt or emits an `Op::is_idle_only` op (directly or via its modal), keeping read-only commands (`/status`), TUI-local changes (`/theme`), and mid-turn ops (`/mode`, `/exit`) usable during turns without a duplicate category. Registry tests enforce this invariant and ensure turns start only from echoed `Prompt` outcomes.
5. **The chat clears when the engine confirms the roll.** `/clear` (`Op::Clear`) and bare `/archive` clear chat only on `SessionRolled`, leaving chat intact if an archive fails (such as an empty session). `SessionTitleUpdated` carries its session ID so late titles never overwrite a rolled session.
6. **Background outcomes die with their session.** Results tag the session-switch counter from dispatch time and drop if a switch occurred, preventing slow `/diff` output from leaking into a new session while letting the read finish safely.
7. **Swap the model and effort in the engine's `ModelSettings`.** Typed `/model`, `/effort`, and both pickers share `Op::SetModel` with `ConfigPick`, updating `ModelSettings` and the compaction threshold in `Engine::set_model` so later requests derive matching betas and `output_config`.
8. **Slash commands never write user config files.** Keeps session commands reversible on restart and reserves config writes for explicit user edits.
9. **Aliases resolve to canonical but display by surface.** `command` maps aliases (such as `/new`, `/reset` → `/clear`), and completion shows only the typed alias.
10. **`/exit` quits, with `/quit` as its alias.** Matches Claude Code, Codex, and pi. Sends mid-turn `Op::Shutdown`, whereas Ctrl+D requires two presses on an empty idle input because typing `/exit` is deliberate.
11. **A command suppresses its echo when its response stands alone.** `SlashCommand::echoes_input(args)` defaults to true, overridden to false for modal-only forms (`/status`, `/config`, `/help`, bare pickers) and bare `/unarchive` / `/delete` errors so orphaned `/cmd` lines do not clutter history. Typed forms and bare `/init` keep echoing to head their confirmation or turn.
12. **Typed forms parse in core.** Non-`/init` typed forms run through `command::typed_form` ([Init](init.md) covers `/init`), while the TUI supplies pickers, prompt titles, and error prefixes.

## Out of Scope

- **User-defined commands.** `BUILT_INS` is the full registry, and skills as commands are tracked in [#163](https://github.com/hakulab/oxide-code/issues/163).

## Sources

- `crates/ox-core/src/command.rs`: the command table with each command's aliases and parser, `typed_form`, and the model resolver.
- `crates/ox-core/src/command/parser.rs`: `parse_slash`, `split_slash`, and `unescape_literal` for the `//` escape.
- `crates/ox-core/src/session/saved.rs`: `find` and `delete`, which refuse the live session.
- `crates/ox-tui/src/app/dispatch.rs`: `handle_submit_prompt`, `run_slash`, `apply_slash_outcome`, and the idle and echo gates.
- `crates/ox-tui/src/components/input/popup.rs`: `popup_state` and `SlashPopup`.
- `crates/ox-tui/src/slash.rs`: `dispatch_with`, which reports an unknown name and prefixes a failure with the typed name, `idle_only`, and `echoes_input`.
- `crates/ox-tui/src/slash/delete.rs`: `DeleteCmd`, which forwards `Op::FindSaved`, and `confirm_modal`, which builds the confirm-delete modal for the session the engine found.
- `crates/ox-tui/src/slash/diff.rs`: `DiffCmd` and the git calls it makes from the work tree root.
- `crates/ox-tui/src/slash/diff/view.rs`: `WorkingTreeDiff`, the typed rows parsed within the 64 KiB cap.
- `crates/ox-tui/src/slash/matcher.rs`: `filter_and_rank`, the popup's four-tier ranking.
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS`, `SlashOutcome`, the `SlashCommand` trait, and `typed_form`, which runs a shared typed form.
