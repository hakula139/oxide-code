# Slash Commands

Client-side command surface: `/help`, `/clear`, `/model`, `/status`, and friends.

## Commands

`BUILT_INS` registers 19 commands.

- **`/archive`** / **`/unarchive`**: Set whether a session shows in default listings and `ox -c`. Bare `/archive` forwards `Op::Archive`. The engine appends an `Archive` entry to the live session, then rolls like `/clear` and emits `SessionRolled { old_session: Archived }`, which the chat answers with a note naming the archived session. A session without messages has no file, so archiving one reports an error and stays put. The typed forms resolve a saved session other than the live one and call `session::saved::set_archived` on its file synchronously, the same operation as the `/resume` picker's Ctrl+X, printing a confirmation, or a note when the session already has that state. `/archive <prefix>` refuses a prefix that matches only the live session and points at bare `/archive`, while `/unarchive` on the live session prints a no-op note. Both are `Mutating`. `/unarchive` is typed-arg only, and its bare form points at the picker's Ctrl+A view, where Ctrl+X unarchives the highlighted session.

- **`/clear`**: Rolls the session UUID and clears the chat and the file tracker. Send-first ordering: `ClearCmd` forwards `Op::Clear`, and the App drops chat history only when `SessionRolled` confirms the roll. `Event::SessionTitleUpdated` carries the originating session ID so a slow Haiku title call straddling `/clear` doesn't repaint the cleared session.

- **`/compact`**: Refuses mid-turn, streams a summarization request, writes a compact boundary and a synthetic continuation message, resets the file tracker, and keeps queued prompts for the post-compact turn. Full design: [compact.md](compact.md).

- **`/config`**: Opens a [`KvOverview`](modals.md) of the resolved config, with the layer that set each value, and the config files loaded at startup. The values start from that load. A `/model`, `/effort`, or `/mode` switch, or Shift+Tab, credits the model, effort, or mode it sets to the session, while an effort the new model derives on its own shows as `default`. Values derived from the model, such as Max Tokens and Auto Compaction, and the Auth value after `/login` or `/logout` keep their loaded source.

- **`/delete`**: Typed-arg only, and bare explains the missing argument. The picker entry is Ctrl+D inside `/resume`. Both push `ConfirmDeleteSessionModal` ([modals.md](modals.md)), whose confirmation removes the session's JSONL and its artifact directory. Live-session refusal is layered across the picker filter, the resolver, and `session::saved::delete`.

- **`/diff`**: Pushes uncommitted git changes plus untracked files into chat as a diff block, or a `Working tree clean` system message when there are none. After the first commit it uses `git diff HEAD`. Before the first commit it combines `git diff --cached` and `git diff` because `HEAD` does not exist yet. Untracked paths come from `git ls-files -z --others --exclude-standard --directory --no-empty-directory`, which lists a wholly untracked directory once and skips empty ones, as `git status` does. Git runs from the work tree root, which `/diff` resolves on each run because `ls-files` lists only the paths under its working directory, so a session started in a subdirectory still sees the whole repository, even after the user fixes a repository git refused at startup. It runs as a `Background` outcome, so a slow repository leaves input and a streaming reply responsive. Each call skips git's optional locks, and each diff pins the config that shapes its output, such as `core.quotePath`, `diff.renames`, the `a/` and `b/` prefixes, and external diff and textconv drivers. The output is parsed once into a `WorkingTreeDiff` of typed rows, capped at 64 KiB on a line boundary, with a note naming the command that prints what the cap cut: the diff command, or `git status` when only untracked paths were cut. Paths git still quotes have their quotes, backslashes, and non-ASCII bytes decoded, while control-character escapes stay visible as written. When git refuses the repository, as for dubious ownership, the error carries git's own message and the fix it names. Modal output would crop without scrollback, so the diff earns its place in the transcript.

- **`/effort`**: Bare opens the Speed ↔ Intelligence slider ([modals.md](modals.md)), since a two-axis picker would force users through models they didn't mean to change. `/effort <level>` accepts the five concrete levels, with no `auto` state. Both forms are mutating and idle-gated because they can emit `Op::SetModel`.

- **`/exit`** (alias `/quit`): Forwards `Op::Shutdown`, the same op as Ctrl+D on an empty idle input, and quits `ox`. It is `Steering`, so it runs mid-turn, and a turn in flight ends as a shutdown does.

- **`/help`**: Opens a [`KvOverview`](modals.md) listing every registered command with its description.

- **`/init`**: Returns `Prompt(..)` with the prompt `command::init::resolve` builds. With an instruction file at the project root or in its `.claude/` directory, the body asks the model to propose its changes as a diff before it edits. With none, `/init <file>` sends the prompt that creates that file, and bare `/init` opens a picker whose choice sends the same prompt. The expanded body is invisible in the live session but recorded in JSONL for resume. Full design: [init.md](init.md).

- **`/login`** / **`/logout`**: Forward `Op::Login` and `Op::Logout`. The engine runs the browser sign-in and swaps the client's credential, and the TUI opens the browser and reports the credential requests use afterward. Both are `Mutating`. Full design: [login.md](login.md).

- **`/mode`**: Switches the permission mode. Bare opens a list picker of the three permission modes ([modals.md](modals.md)), and `/mode <mode>` forwards `Op::SetMode` directly. Bare is `ReadOnly`, so its picker opens mid-turn and submits the op as a modal action. Typed is `Steering`, so the dispatcher forwards its op mid-turn. Either switch applies from the turn's next gated call. Shift+Tab sends the same op. [permissions.md](../tools/permissions.md#switching-modes) covers how the engine applies it.

- **`/model`**: Swaps the active model mid-session through `Engine::set_model`. Bare opens the combined picker ([modals.md](modals.md)). `/model <arg>` resolves an exact catalog ID first, then a unique substring, so `fable`, `opus`, `sonnet`, and `haiku` act as aliases. Both forms are mutating and idle-gated because they can emit `Op::SetModel`, which re-resolves the current effort against the new model.

- **`/rename`**: Bare opens a title editor ([modals.md](modals.md)), and `/rename <title>` applies directly. Both forms build their `Op::Rename` through `command::rename`, which keeps the title's first non-blank line and truncates it to the 80-column cap that first-prompt titles share. Both lock out AI title generation for the rest of the session so a slow Haiku call can't overwrite the user's pick. `classify` is always `Mutating`.

- **`/resume`** (alias `/continue`): Swaps to a different session in place. Bare opens a searchable session picker ([modals.md](modals.md)), which forwards `Op::Resume` with the chosen session file. `/resume <id-prefix>` forwards the prefix, which the engine resolves through a current-project-first lookup that widens to all projects on a miss. Both forms refuse mid-turn. Full design: [resume.md](resume.md).

- **`/status`**: Opens a [`KvOverview`](modals.md) of session descriptors.

- **`/theme`**: Bare opens a live-preview list picker ([modals.md](modals.md)) over the built-in palettes. `/theme <name>` validates against the curated roster and swaps directly. Custom file-path themes aren't accepted via the slash form.

## Design Decisions

1. **Trait registry.** `trait SlashCommand` mirrors `tool::Tool`, with one `slash/<name>.rs` file per command or per pair such as `/login` and `/logout`. Adding `/foo` means implementing the trait and adding it to `BUILT_INS` in alphabetical order.

2. **Parse at submit.** `App::dispatch_action` hands an `Action::Submit` to `handle_submit_prompt`, which runs `command::parse_slash` first, then dispatches locally or forwards.

3. **Command output reuses the chat's blocks.** Confirmations and notes print as a `SystemMessageBlock`, with its left bar in `accent`, and errors as an `ErrorBlock`. `/diff` is the one command whose output has a block of its own, `GitDiffBlock`, since its rows need diff styling.

4. **Two-column popup, plain rows.** Name left, description right. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier. Names accept `:` and `.`, so namespaced names such as `/plugin:cmd` parse. The popup and `parse_slash` split the line with the same `split_slash`, so they agree on every name, and once a space follows the name, the input shows the command's usage as `/help` lists it, dim at the cursor, until an argument starts.

5. **Swap the model and effort mid-session via `&mut Client`.** Both `/model <id>` and `/effort <level>` return `Forward(Action::Op(Op::SetModel(pick)))`, the same payload both modals emit, so the typed-arg path and the modals share one resolver. The `ConfigPick` names a model, with an optional effort, or an effort alone, so every pick sets at least one axis. Each request reads the client's active model and effort, so betas and `output_config` pick up the swap. `Engine::set_model` also re-resolves the compaction threshold for the new model. See [modals.md](modals.md).

6. **Slash commands never write user config files.** State is session-only, so a restart reverts to the configured values. Session commands should feel reversible, and any write that outlives the session requires an explicit user action.

7. **Aliases resolve to canonical but display by surface.** `/clear` is canonical, while `/new` and `/reset` are aliases. The alias table lives in `command`, so the TUI registry and the bare REPL resolve the same aliases. The popup shows only the alias the user typed.

8. **`/exit` quits, with `/quit` as its alias.** Claude Code, Codex, and pi all accept a typed quit command, so users reach for one before they learn the keys. It forwards `Op::Shutdown`, the same op as Ctrl+D on an empty idle input. It is `Steering`, unlike a busy Ctrl+D, because typing the command is deliberate where a habitual EOF press is not. The bare REPL runs it too, stopping before the lines after it and exiting with success.

9. **Built-in commands only.** `BUILT_INS` is the entire registry, with no discovery of user-defined commands. Skills as commands are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

10. **Read-only commands fast-path the busy turn.** `classify` defaults to `SlashKind::ReadOnly`, and the dispatcher runs them client-side even when input is disabled. Registry tests check that no read-only form forwards an action or starts a turn, and that a turn starts only from a `Prompt` outcome whose line the chat echoes. State-mutating commands override to `SlashKind::Mutating` and refuse mid-turn. `SlashKind::Steering` marks a form whose forwarded action applies mid-turn, either a TUI-only change or an op the engine applies to the turn in flight, so the dispatcher forwards it mid-turn too. `/exit`, `/mode <mode>`, and `/theme <name>` steer. An unknown name has no kind, so `slash::classify` returns `None` and the dispatcher runs it like a read-only command, which reports the error.

11. **One trait return carries every effect: `SlashOutcome`.** `Forward(Action)` hands a state-mutating action back to the App, which applies a TUI-only action locally and sends an op to the engine. `Prompt(String)` starts a turn with a prompt the command built, which the chat and the prompt queue show as the typed line, the line a fresh session also takes as its first-prompt title. `Modal(_)` opens a modal, and `SystemMessage` prints into chat. `Background` carries a future for a command whose work is slow I/O, which the App runs in a `JoinSet` off the event loop. It prints the `ChatOutput` the future resolves to, and a session switch drops the future unfinished, so its output never lands in the next session's chat. A failure returns `Err(String)`, which the dispatcher prefixes with the typed command name, and it wraps a background future so that future's failure gets the same prefix.

12. **Commands read a borrowed `SlashContext` and write nothing.** The context carries `LiveSessionInfo` and the live session title. The App applies the returned outcome in one place on both the idle and busy paths, so no command reaches into `ChatView` or the modal stack. See [modals.md](modals.md) for the full modal design.

13. **A command suppresses its echo when its response stands alone.** `SlashCommand::echoes_input(args) -> bool` defaults to true. Modal-only forms (`/status`, `/config`, and `/help`, which open a read-only [`KvOverview`](modals.md#modals), plus bare `/effort`, `/mode`, `/model`, `/rename`, `/resume`, and `/theme`) override it to false because the modal is the response, and the typed `/foo` line would be orphaned in history once the modal closes. Bare `/unarchive` and `/delete` also override it to false so the error explaining the missing argument stands alone. Typed forms keep echoing, so chat records the command, usually followed by a confirmation such as `Switched to plan mode`. Bare `/init` echoes too, although it opens a picker when the project has no instruction file, because it usually sends its prompt at once, and the line then heads the turn it starts.

14. **Typed forms parse in core, so the bare REPL runs them too.** `command::typed_op` turns the typed forms of `/clear`, `/compact`, `/effort`, `/exit`, `/init`, `/mode`, `/model`, `/rename`, and `/resume` into the `Op` each one runs. The TUI commands call the same parsers and add what needs the TUI: the pickers and modals behind the bare forms, and a check that rejects `/effort` before sending it when the live model has no effort levels. `command::typed_op` resolves aliases for the `--no-tui` REPL, which sends the op and renders until `TurnEnd::of` reads its terminal event, so an effort pick the model has no level for fails there. The REPL rejects any other command with a message naming the commands it runs, and a bare form that needs an argument gets its usage.

## Sources

- `crates/ox-core/src/command.rs`: the alias table, `typed_op`, `parse`, and the typed forms of `/clear`, `/compact`, `/effort`, `/mode`, `/model`, `/rename`, and `/resume`, plus the model resolver.
- `crates/ox-core/src/command/init.rs`: the typed form of `/init`, the instruction-file discovery, and the create and update prompts.
- `crates/ox-core/src/command/parser.rs`: `parse_slash`, which parses a typed line into a command name and its arguments, `split_slash`, the borrowed split it shares with the popup, and `unescape_literal` for the `//` escape.
- `crates/ox-core/src/engine.rs`: `Engine::run` arms for `Op::Clear`, `Op::SetModel`, and `Op::SetMode`, `Engine::set_model`, which re-resolves the compaction threshold for the new model and words the `ConfigChanged` notice against the settings before the swap, and `TurnEnd::of`.
- `crates/ox-core/src/engine/sessions.rs`: `Engine::archive`, which waits for the archive entry to reach disk before rolling the session.
- `crates/ox-core/src/protocol/notice.rs`: `config_changed` and `mode_changed`, the confirmations every frontend shows.
- `crates/ox-core/src/session/display.rs`: shared `id_prefix` and `display_title`, `archive_notice`, and the `MAX_TITLE_WIDTH` cap that `fit_title` applies to every title.
- `crates/ox-core/src/session/resolver.rs`: `resolve_prefix_to_info` checks the current project first, widens the search on a miss, and returns a typed `ResolveError` that the CLI prints as is and slash commands rephrase. `resolve_saved_prefix` reports a live-only match as `ResolveError::Live` for `/archive`, `/unarchive`, `/delete`, and the engine's prefix resume.
- `crates/ox-core/src/session/saved.rs`: `set_archived` and `delete`, shared by the typed commands and the `/resume` picker, which refuse the live session and return a typed `SavedError`.
- `crates/ox-core/src/session/store.rs`: `delete_at` and `set_archived_at`, the file operations behind `saved`.
- `crates/ox-tui/src/action.rs`: `Action`, which slash commands forward.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_action`, `dispatch_op`, `run_slash`, `apply_slash_outcome`, modal gate, echo gate.
- `crates/ox-tui/src/app/events.rs`: `apply_slash_task`, which prints a finished `Background` outcome, and `apply_session_rolled`, which clears the chat when the roll lands.
- `crates/ox-tui/src/components/chat/blocks/git_diff.rs`: `GitDiffBlock`, which renders `/diff` output.
- `crates/ox-tui/src/components/chat/blocks/system.rs`: `SystemMessageBlock`, which renders confirmations and notes.
- `crates/ox-tui/src/components/input/popup.rs`: `popup_state`, which reads the typed line as a command name or an argument, and `SlashPopup`, which lists the matching commands or the typed command's arg completions for Tab and Enter to complete.
- `crates/ox-tui/src/modal.rs`: `Modal` trait, `ModalStack`, key routing.
- `crates/ox-tui/src/modal/kv_overview.rs`: generic `KvOverview` / `KvSection` primitive.
- `crates/ox-tui/src/modal/list_picker.rs`: generic `ListPicker` primitive.
- `crates/ox-tui/src/modal/searchable_list.rs`: generic `SearchableList` primitive.
- `crates/ox-tui/src/slash.rs`: dispatch and the shared test fixtures in `slash::testing`.
- `crates/ox-tui/src/slash/archive.rs`: `ArchiveCmd` and `UnarchiveCmd`.
- `crates/ox-tui/src/slash/clear.rs`: `ClearCmd`, which forwards `Op::Clear`.
- `crates/ox-tui/src/slash/compact.rs`: `CompactCmd`.
- `crates/ox-tui/src/slash/config.rs`: `/config` row builder plus sectioned `KvOverview` constructor.
- `crates/ox-tui/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` (destructive-action gate, inline error kept until a retry or close).
- `crates/ox-tui/src/slash/context.rs`: `SlashContext` (the App's `LiveSessionInfo` plus the live title).
- `crates/ox-tui/src/slash/delete.rs`: `DeleteCmd`, which resolves its prefix through `ox-core`'s `resolve_saved_prefix` and pushes the confirm-delete modal.
- `crates/ox-tui/src/slash/diff.rs`: `DiffCmd` and the git calls it makes from the work tree root.
- `crates/ox-tui/src/slash/diff/view.rs`: `WorkingTreeDiff`, the typed rows parsed from git's output within the 64 KiB cap.
- `crates/ox-tui/src/slash/effort.rs`: `EffortCmd`.
- `crates/ox-tui/src/slash/effort_slider.rs`: bare `/effort` Speed ↔ Intelligence slider modal.
- `crates/ox-tui/src/slash/exit.rs`: `ExitCmd`.
- `crates/ox-tui/src/slash/help.rs`: `/help` row builder plus `KvOverview` constructor.
- `crates/ox-tui/src/slash/init.rs`: `InitCmd` and the file picker.
- `crates/ox-tui/src/slash/login.rs`: `LoginCmd` and `LogoutCmd`.
- `crates/ox-tui/src/slash/matcher.rs`: `filter_and_rank`, the popup's four-tier ranking, and `rank_by_prefix` for arg completions.
- `crates/ox-tui/src/slash/mode.rs`: `ModeCmd` plus `ModePicker`.
- `crates/ox-tui/src/slash/model.rs`: `ModelCmd`.
- `crates/ox-tui/src/slash/picker.rs`: combined model and effort picker modal.
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS`, `SlashKind`, `SlashOutcome`, `ChatOutput`, and the `SlashCommand` trait.
- `crates/ox-tui/src/slash/rename.rs`: `RenameCmd` plus `RenameModal` editor.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd` plus `ResumePicker` (see `resume.md`).
- `crates/ox-tui/src/slash/sessions.rs`: `open_store`, `resolve_error_message`, `saved_error_message`, and the live-session recovery hints, shared by the session commands, plus `format_metadata_line` for the delete confirm modal.
- `crates/ox-tui/src/slash/status.rs`: `/status` row builder plus `KvOverview` constructor.
- `crates/ox-tui/src/slash/theme.rs`: `ThemeCmd`, picker open plus typed-arg validator.
- `crates/ox/src/driver/repl.rs`: `command_op`, which runs typed slash commands in the bare REPL.
- `crates/ox/src/driver/stdio.rs`: `render_op`, plus the `StdioSink` confirmations for the session and config events.
