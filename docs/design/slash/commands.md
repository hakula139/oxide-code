# Slash Commands

Client-side command surface: `/help`, `/clear`, `/model`, `/status`, and friends.

## Implementation

18 built-ins: `/archive`, `/clear`, `/compact`, `/config`, `/delete`, `/diff`, `/effort`, `/help`, `/init`, `/login`, `/logout`, `/mode`, `/model`, `/rename`, `/resume`, `/status`, `/theme`, `/unarchive`.

- `/clear` rolls the session UUID and clears the chat and the file tracker.
- `/compact` compresses the visible transcript into a summary and resets the continuation chain.
- `/model` swaps the active model mid-session via `Engine::set_model`.
- `/effort` sets an explicit effort tier.
- `/mode` switches the permission mode, even mid-turn.
- `/theme` swaps the TUI palette mid-session, with bare opening a live-preview list picker.
- `/init` asks the model to create the project's AGENTS.md or CLAUDE.md, picking the file first when neither exists, or to propose changes to the existing ones as a diff. Full design: [init.md](init.md).
- `/rename` sets the session title manually and locks out the AI title generator. Bare opens a single-line editor pre-filled with the current title.
- `/resume` swaps to a different session in place. Bare opens a searchable picker, while typed-arg jumps directly. Full design: [resume.md](resume.md).
- `/delete <id-prefix>` removes a saved session's JSONL and its artifact directory after a Y/N confirmation. The `/resume` picker offers the same gesture via Ctrl+D on the cursor row.
- `/archive` and `/unarchive` set whether a session shows in default listings and bare `ox -c`. Bare `/archive` archives the live session and rolls like `/clear`.
- `/config`, `/help`, and `/status` open a read-only [`KvOverview`](modals.md) modal. `/diff` is the lone printer because its output can run to hundreds of lines, where scrollback value beats modal cropping.

## Design Decisions

1. **Trait registry.** `trait SlashCommand` mirrors `tool::Tool`, with one `slash/<name>.rs` file per command or per pair such as `/login` and `/logout`. Adding `/foo` means implementing the trait and adding it to `BUILT_INS` in alphabetical order.

2. **Parse at submit.** `App::dispatch_action` hands an `Action::Submit` to `handle_submit_prompt`, which runs `command::parse_slash` first, then dispatches locally or forwards.

3. **One synthetic block kind: `SystemMessageBlock`.** Left-bar in `accent`. Errors reuse `ErrorBlock`.

4. **Two-column popup, plain rows.** Name left, description right. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier. Names accept `:` and `.`, so namespaced names such as `/plugin:cmd` parse. The popup and `parse_slash` split the line with the same `split_slash`, so they agree on every name, and once a space follows the name, the input shows the command's usage as `/help` lists it, dim at the cursor, until an argument starts.

5. **Swap the model and effort mid-session via `&mut Client`.** Both `/model <id>` and `/effort <level>` return `Forward(Action::Op(Op::SetModel(pick)))`, the same payload both modals emit, so the typed-arg path and the modals share one resolver. The `ConfigPick` names a model, with an optional effort, or an effort alone, so every pick sets at least one axis. Each request reads the client's active model and effort, so betas and `output_config` pick up the swap. `Engine::set_model` also re-resolves the compaction threshold for the new model. See [modals.md](modals.md).

6. **Slash commands never write user config files.** State is session-only, so a restart reverts to the configured values. Session commands should feel reversible, and any write that outlives the session requires an explicit user action.

7. **Aliases resolve to canonical but display by surface.** `/clear` is canonical, while `/new` and `/reset` are aliases. The alias table lives in `command`, so the TUI registry and the bare REPL resolve the same aliases. The popup shows only the alias the user typed.

8. **No `/quit` or `/exit`.** Pressing Ctrl+C twice or Ctrl+D exits.

9. **Read-only kv views go through one shared modal primitive.** `/status`, `/config`, and `/help` all open a [`KvOverview`](modals.md) with a title, sectioned label-value rows, and a footer. The modal is the response, so the typed `> /foo` line stays out of chat history (see decision 14).

10. **Built-in commands only.** `BUILT_INS` is the entire registry, with no discovery of user-defined commands. Skills as commands are tracked in [#163](https://github.com/hakula139/oxide-code/issues/163).

11. **Read-only commands fast-path the busy turn.** `classify` defaults to `SlashKind::ReadOnly`, and the dispatcher runs them client-side even when input is disabled, discarding any action they forward. State-mutating commands override to `SlashKind::Mutating` and refuse mid-turn. `SlashKind::Steering` marks a form whose forwarded op the engine applies to the turn in flight, so the dispatcher sends it mid-turn too. `/mode <mode>` is the only one.

12. **One trait return carries every effect: `SlashOutcome`.** `Forward(Action)` hands a state-mutating action back to the App, which applies a TUI-only action locally and sends an op to the engine. `Modal(_)` opens a modal, and `SystemMessage` / `GitDiff` print into chat. A failure returns `Err(String)`, which the dispatcher prefixes with the typed command name.

13. **Commands read a borrowed `SlashContext` and write nothing.** The context carries `LiveSessionInfo` and the live session title. The App applies the returned outcome in one place on both the idle and busy paths, so no command reaches into `ChatView` or the modal stack. See [modals.md](modals.md) for the full modal design.

14. **A command suppresses its echo when its response stands alone.** `SlashCommand::echoes_input(args) -> bool` defaults to true. Modal-only forms (`/status`, `/config`, `/help`, plus bare `/effort`, `/mode`, `/model`, `/rename`, `/resume`, and `/theme`) override it to false because the modal is the response, and the typed `/foo` line would be orphaned in history once the modal closes. Bare `/unarchive` and `/delete` also override it to false so the error explaining the missing argument stands alone. Typed forms keep echoing, so chat records the command, usually followed by a confirmation such as `Switched to plan mode`.

15. **Typed forms parse in core, so the bare REPL runs them too.** `command::parse` turns the typed forms of `/clear`, `/compact`, `/effort`, `/init`, `/mode`, `/model`, `/rename`, and `/resume` into the `Op` each one runs. The TUI commands call the same parsers and add what needs the TUI: the pickers and modals behind the bare forms, and a check that rejects `/effort` before sending it when the live model has no effort tier. `command::typed_op` resolves aliases for the `--no-tui` REPL, which sends the op and renders until `TurnEnd::of` reads its terminal event, so an effort pick the model has no tier for fails there. The REPL rejects any other command with a message naming the commands it runs, and a bare form that needs an argument gets its usage.

## Per-Command Notes

- **`/clear`**: Send-first ordering. Forward `Op::Clear` first, drop chat history only on success. `Event::SessionTitleUpdated` carries the originating session ID so a slow Haiku title call straddling `/clear` doesn't repaint the cleared session.

- **`/compact`**: Refuses mid-turn, streams a summarization request, writes a compact boundary and a synthetic continuation message, resets the file tracker, and keeps queued prompts for the post-compact turn. Full design: [compact.md](compact.md).

- **`/init`**: Returns `Forward(Action::Op(Op::Prompt(..)))` with the prompt `command::init::resolve` builds. With an instruction file at the project root or in its `.claude/` directory, the body asks the model to propose its changes as a diff before it edits. With none, `/init <file>` forwards the prompt that creates that file, and bare `/init` opens a picker whose choice forwards the same prompt. The expanded body is invisible in the live session but recorded in JSONL for resume. Full design: [init.md](init.md).

- **`/login`** / **`/logout`**: Forward `Op::Login` and `Op::Logout`. The engine runs the browser sign-in and swaps the client's credential, and the TUI opens the browser and reports the credential requests use afterward. Both are `Mutating`. Full design: [login.md](login.md).

- **`/model`**: Bare opens the combined picker ([modals.md](modals.md)). `/model <arg>` resolves an exact catalog ID first, then a unique substring, so `fable`, `opus`, `sonnet`, and `haiku` act as aliases. Both forms are mutating and idle-gated because they can emit `Op::SetModel`, which re-resolves the current effort against the new model.

- **`/effort`**: Bare opens the Speed ↔ Intelligence slider ([modals.md](modals.md)), since a two-axis picker would force users through models they didn't mean to change. `/effort <level>` accepts the five concrete tiers, with no `auto` state. Both forms are mutating and idle-gated because they can emit `Op::SetModel`.

- **`/mode`**: Bare opens a list picker of the three permission modes ([modals.md](modals.md)), and `/mode <mode>` forwards `Op::SetMode` directly. Bare is `ReadOnly`, so its picker opens mid-turn and submits the op as a modal action. Typed is `Steering`, so the dispatcher forwards its op mid-turn. Either switch applies from the turn's next gated call. Shift+Tab sends the same op. [permissions.md](../tools/permissions.md#switching-modes) covers how the engine applies it.

- **`/theme`**: Bare opens a live-preview list picker ([modals.md](modals.md)) over the built-in palettes. Up / Down repaints the TUI in the candidate, and Esc snaps back. `/theme <name>` validates against the curated roster and swaps directly. Custom file-path themes aren't accepted via the slash form.

- **`/status`**: Opens a [`KvOverview`](modals.md) of session descriptors. No args, no chat output. Esc / Ctrl+C dismiss.

- **`/config`**: Opens a [`KvOverview`](modals.md) with two headed sections: resolved effective config with the layer that set each value, and the config files loaded at startup. Both come from the startup load, so the files listed are the ones behind the values shown.

- **`/help`**: Opens a [`KvOverview`](modals.md) listing every registered command with its description. Aliases follow the canonical name in parentheses, then the `usage()` placeholder.

- **`/diff`**: The lone printer. Pushes uncommitted git changes plus untracked files into chat as a diff block, capped at 64 KB on a UTF-8 boundary, or a `Working tree clean` system message when there are none. After the first commit it uses `git diff HEAD`. Before the first commit it combines `git diff --cached` and `git diff` because `HEAD` does not exist yet. Modal output would crop without scrollback, so the diff earns its place in the transcript.

- **`/rename`**: Bare opens a single-line title editor pre-filled with the current title, and `/rename <title>` applies directly. Both forms build their `Op::Rename` through `command::rename`, which keeps the title's first non-blank line and truncates it to the 80-column cap that first-prompt titles share, and the editor stops taking input at that cap. Both lock out AI title generation for the rest of the session so a slow Haiku call can't overwrite the user's pick. `classify` is always `Mutating`. See [modals.md](modals.md).

- **`/resume`** (alias `/continue`): Bare opens a searchable session picker ([modals.md](modals.md)), which forwards `Op::Resume` with the chosen session file. `/resume <id-prefix>` forwards the prefix, which the engine resolves through a current-project-first lookup that widens to all projects on a miss. Both forms refuse mid-turn. Full design: [resume.md](resume.md).

- **`/delete`**: Typed-arg only, and bare explains the missing argument. The picker entry is Ctrl+D inside `/resume`. Both push `ConfirmDeleteSessionModal` ([modals.md](modals.md)), which runs `session::saved::delete` on the session's file on Y / Enter and emits a `Deleted session {id}: {title}` line in chat. Live-session refusal is layered across the picker filter, the resolver, and `session::saved::delete`.

- **`/archive`** / **`/unarchive`**: Bare `/archive` forwards `Op::Archive`. The engine appends an `Archive` entry to the live session, then rolls like `/clear` and emits `SessionRolled { archived: true }`, which the chat answers with a note naming the archived session. A session without messages has no file, so archiving one reports an error and stays put. The typed forms resolve a saved session other than the live one and call `session::saved::set_archived` on its file synchronously, the same operation as the `/resume` picker's Ctrl+X, printing a confirmation, or a note when the session already has that state. `/archive <prefix>` refuses a prefix that matches only the live session and points at bare `/archive`, while `/unarchive` on the live session prints a no-op note. Both are `Mutating`. `/unarchive` is typed-arg only, and its bare form points at the picker's Ctrl+A view, where Ctrl+X unarchives the highlighted session.

## Sources

- `crates/ox-core/src/command.rs`: the alias table, `typed_op`, `parse`, and the typed forms of `/clear`, `/compact`, `/effort`, `/mode`, `/model`, `/rename`, and `/resume`, plus the model resolver.
- `crates/ox-core/src/command/init.rs`: the typed form of `/init`, the instruction-file discovery, and the create and update prompts.
- `crates/ox-core/src/command/parser.rs`: `parse_slash`, which parses a typed line into a command name and its arguments, `split_slash`, the borrowed split it shares with the popup, and `unescape_literal` for the `//` escape.
- `crates/ox-core/src/engine.rs`: `Engine::run` arms for `Op::Clear`, `Op::SetModel`, and `Op::SetMode`, `Engine::set_model`, which re-resolves the compaction threshold for the new model, `Engine::archive`, which waits for the archive entry to reach disk before rolling the session, and `TurnEnd::of`.
- `crates/ox-core/src/session/display.rs`: shared `id_prefix`, `display_title`, and `format_metadata_line` for the picker row and delete confirm modal, `archive_notice`, and the `MAX_TITLE_WIDTH` cap that `fit_title` applies to first-prompt and `/rename` titles.
- `crates/ox-core/src/session/resolver.rs`: `resolve_prefix_to_info` checks the current project first, widens the search on a miss, and returns a typed `ResolveError` that the CLI prints as is and slash commands rephrase. `resolve_saved_prefix` reports a live-only match as `ResolveError::Live` for `/archive`, `/unarchive`, `/delete`, and the engine's prefix resume.
- `crates/ox-core/src/session/saved.rs`: `set_archived` and `delete`, shared by the typed commands and the `/resume` picker, which refuse the live session and return a typed `SavedError`.
- `crates/ox-core/src/session/store.rs`: `delete_at` and `set_archived_at`, the file operations behind `saved`.
- `crates/ox-tui/src/action.rs`: `Action`, which slash commands forward.
- `crates/ox-tui/src/app/dispatch.rs`: `dispatch_action`, `dispatch_op`, `run_slash`, `apply_slash_outcome`, modal gate, echo gate.
- `crates/ox-tui/src/components/input/popup.rs`: `popup_state`, which reads the typed line as a command name or an argument, and `SlashPopup`, which lists the matching commands or the typed command's arg completions for Tab and Enter to complete.
- `crates/ox-tui/src/modal.rs`: `Modal` trait, `ModalStack`, key routing.
- `crates/ox-tui/src/modal/kv_overview.rs`: generic `KvOverview` / `KvSection` primitive.
- `crates/ox-tui/src/modal/list_picker.rs`: generic `ListPicker` primitive.
- `crates/ox-tui/src/modal/searchable_list.rs`: generic `SearchableList` primitive.
- `crates/ox-tui/src/slash.rs`: dispatch and the shared test fixtures in `slash::testing`.
- `crates/ox-tui/src/slash/archive.rs`: `ArchiveCmd` and `UnarchiveCmd`.
- `crates/ox-tui/src/slash/clear.rs`: `ClearCmd` sends first.
- `crates/ox-tui/src/slash/config.rs`: `/config` row builder plus sectioned `KvOverview` constructor.
- `crates/ox-tui/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` (destructive-action gate, sticky inline error cleared only on deliberate Y / N).
- `crates/ox-tui/src/slash/context.rs`: `SlashContext` (`LiveSessionInfo` plus the live title).
- `crates/ox-tui/src/slash/delete.rs`: `DeleteCmd` refuses prefixes that match only the live session.
- `crates/ox-tui/src/slash/diff.rs`: `/diff` printer with 64 KB UTF-8-boundary cap.
- `crates/ox-tui/src/slash/effort.rs`: `EffortCmd`.
- `crates/ox-tui/src/slash/effort_slider.rs`: bare `/effort` Speed ↔ Intelligence slider modal.
- `crates/ox-tui/src/slash/help.rs`: `/help` row builder plus `KvOverview` constructor.
- `crates/ox-tui/src/slash/init.rs`: `InitCmd` and the file picker.
- `crates/ox-tui/src/slash/login.rs`: `LoginCmd` and `LogoutCmd`.
- `crates/ox-tui/src/slash/mode.rs`: `ModeCmd` plus `ModePicker`.
- `crates/ox-tui/src/slash/model.rs`: `ModelCmd`.
- `crates/ox-tui/src/slash/picker.rs`: combined model and effort picker modal.
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS`, `SlashKind`, `SlashOutcome`, and the `SlashCommand` trait, whose aliases default to `command::aliases`.
- `crates/ox-tui/src/slash/rename.rs`: `RenameCmd` plus `RenameModal` editor.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd` plus `ResumePicker` (see `resume.md`).
- `crates/ox-tui/src/slash/sessions.rs`: `open_store`, `resolve_error_message`, `saved_error_message`, and the live-session recovery hints, shared by the session commands.
- `crates/ox-tui/src/slash/status.rs`: `/status` row builder plus `KvOverview` constructor.
- `crates/ox-tui/src/slash/theme.rs`: `ThemeCmd`, picker open plus typed-arg validator.
- `crates/ox/src/driver/repl.rs`: `command_op`, which runs typed slash commands in the bare REPL.
- `crates/ox/src/driver/stdio.rs`: `render_op`, plus the `StdioSink` confirmations for the session and config events.
