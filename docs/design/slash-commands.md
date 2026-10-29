# Slash Commands

A slash command is an input line that starts with `/`, which the TUI runs itself by opening a modal, printing a note, forwarding an op to the engine, or starting a turn with a prompt the command builds. [Slash Commands](../guide/slash-commands.md) lists the commands and what each one does, and [Compaction](compaction.md), [Init](init.md), [Login](login.md), [Modals](modals.md), [Permissions](permissions.md), [Persistence](persistence.md#archiving), and [Resume](resume.md) cover the commands with mechanisms of their own.

## Registry

Each command implements `SlashCommand` in its own `slash/<name>.rs` file, or one file per pair such as `/login` and `/logout`, and `BUILT_INS` lists them in alphabetical order. `ox-core`'s `command` module holds the command table, with each name's aliases and the parsers for its typed forms, so the TUI and the `--no-tui` REPL resolve the same names and arguments.

## Dispatch

On submit, `handle_submit_prompt` parses the line with `command::parse_slash`. While an op is in flight, the dispatcher refuses every form of a command whose `SlashCommand::idle_only` holds with a notice, and runs every other command, including an unknown name, which reports its error at once. The command reads a borrowed `SlashContext`, which borrows the App's `LiveSessionInfo`, live title included, and returns a `SlashOutcome`. `Forward(Action)` hands an action back to the App, which applies a TUI-only action locally and sends an op to the engine. `Prompt(String)` starts a turn with a prompt the command built, which the chat and the prompt queue show as the typed line. `SystemMessage`, `GitDiff`, and `Modal` print a note, append a diff block, or open a modal. `Background` carries a future for slow I/O, which the App runs in a `JoinSet` off the event loop and applies through the same path when it resolves. A failure returns `Err(String)`, which the dispatcher prefixes with the typed command name.

The popup lists the matching commands in two columns, the name and the description, ranked by name prefix, then alias prefix, then name substring, then alias substring, alphabetically within each tier. It splits the line with the `split_slash` that `parse_slash` uses, so both agree on every name, including namespaced ones such as `/plugin:cmd`. Once a space follows the name, the input shows the command's usage, dim at the cursor, until an argument starts.

## `/diff`

`/diff` pushes the uncommitted changes and untracked files into the chat as a `GitDiffBlock`, since a diff needs the scrollback a modal would crop. It resolves the work tree root on each run and runs git there, because `ls-files` lists only the paths under its working directory. After the first commit it diffs against `HEAD`, and before it combines the staged and unstaged diffs. Untracked paths come from `ls-files` with `--directory --no-empty-directory`, so a wholly untracked directory lists once, as `git status` shows it. Each call skips git's optional locks and pins the config that shapes the output, such as `core.quotePath`, `diff.renames`, the diff prefixes, and external diff and textconv drivers. The output parses once into a `WorkingTreeDiff` of typed rows, capped at 64 KiB on a line boundary with a note naming the command that prints what the cap cut. When git refuses the repository, as for dubious ownership, the error carries git's own message and the fix it names.

## `/delete`

`/delete <id-prefix>` forwards `Op::FindSaved`, and the engine answers with `SavedFound`, which opens `ConfirmDeleteSessionModal` ([Modals](modals.md)) for the session it resolved or prints why no single session matched. Ctrl+D in the `/resume` picker opens the same modal for the cursor row. Y sends `Op::DeleteSaved` with the session's ID, and the engine deletes the session's rows and artifact directory, then emits `SavedDeleted`. Three layers refuse the live session, one on each path a target takes: the picker filters it out of every page, `saved::find` resolves a prefix with the live session excluded, and `saved::delete` refuses the live ID whichever op names it, so no frontend's check is load-bearing. The store also refuses a session another process holds open ([Liveness Locks](persistence.md#liveness-locks)).

## Design Decisions

1. **Trait registry.** `SlashCommand` mirrors `tool::Tool`, so adding a command is one file and one `BUILT_INS` entry, with no central match to extend.
2. **Command output reuses the chat's blocks.** Confirmations and notes print as a `SystemMessageBlock` and errors as an `ErrorBlock`, so commands add no block types. `/diff` is the one exception, `GitDiffBlock`, since its rows need diff styling.
3. **One return type carries every effect.** Commands read a borrowed `SlashContext` and write nothing, and the App applies the returned `SlashOutcome` in one place on both the idle and busy paths, so no command reaches into `ChatView` or the modal stack.
4. **Only commands that can start a turn or an idle-only op wait for idle.** `idle_only` holds for a command when any of its forms builds a prompt or sends an op `Op::is_idle_only` holds for, directly or from the modal it opens, so a command that only reads, such as `/status` or `/diff`, stays useful during a long turn, as do TUI-only changes such as `/theme` and ops the engine takes mid-turn, such as `/mode` and `/exit`. The engine's op contract already decides which ops race a turn, so the TUI keeps no category of its own for them. Registry tests check that no form running mid-turn starts a turn, forwards an idle-only op, or opens a modal that sends one, and that a turn starts only from a `Prompt` outcome whose line the chat echoes.
5. **The chat clears when the engine confirms the roll.** `/clear` forwards `Op::Clear`, and the App drops the chat only when `SessionRolled` arrives, and bare `/archive` shares that path, so an archive the engine refuses, as on a session with no messages, leaves the chat intact. `SessionTitleUpdated` names the session it titles, so a slow title request that straddles `/clear` cannot repaint the new session.
6. **Background outcomes die with their session.** A session switch drops an unfinished `Background` future, so a slow `/diff` never prints into the next session's chat.
7. **Swap the model and effort in the engine's `ModelSettings`.** `/model <id>`, `/effort <level>`, and both pickers emit the same `Op::SetModel`, whose `ConfigPick` names a model, an effort, or both, so the typed forms and the modals share one resolver. Each request carries the engine's current settings, so betas and `output_config` follow the swap, and `Engine::set_model` re-resolves the compaction threshold for the new model.
8. **Slash commands never write user config files.** State is session-only, so a restart reverts to the configured values. Session commands should feel reversible, and any write that outlives the session requires an explicit user action.
9. **Aliases resolve to canonical but display by surface.** `/clear` is canonical, while `/new` and `/reset` are aliases. The alias table lives in `command`, so the TUI registry and the bare REPL resolve the same aliases, and the popup shows only the alias the user typed.
10. **`/exit` quits, with `/quit` as its alias.** Claude Code, Codex, and pi all accept a typed quit command, so users reach for one before they learn the keys. It forwards `Op::Shutdown`, which the engine takes mid-turn, so it quits during a turn, while Ctrl+D quits only from an empty idle input and only on a second press, because typing the command is deliberate where a habitual EOF press is not.
11. **A command suppresses its echo when its response stands alone.** `SlashCommand::echoes_input(args)` defaults to true. Modal-only forms, such as `/status`, `/config`, `/help`, and the bare pickers, override it to false because the modal is the response, and the typed `/foo` line would be orphaned in history once the modal closes. Bare `/unarchive` and `/delete` also override it so the error explaining the missing argument stands alone. Typed forms keep echoing, so the chat records the command above its confirmation. Bare `/init` echoes too, because it usually sends its prompt at once, and the line then heads the turn it starts.
12. **Typed forms parse in core, so the bare REPL runs them too.** `command::typed_op` turns each typed form the REPL supports into the `Op` it runs. The TUI commands other than `/init`, which [Init](init.md) covers, run their typed forms through `command::typed_form`, the same table entry without the REPL's prompt title and error prefix, since the TUI adds both itself, and add only what needs the TUI, such as the pickers behind the bare forms. The REPL rejects any other command with a message naming the commands it runs.

## Out of Scope

- **User-defined commands.** `BUILT_INS` is the entire registry, and skills as commands are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

## Sources

- `crates/ox-core/src/command.rs`: the command table with each command's aliases and parser, `typed_op` and `typed_form`, and the model resolver.
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
