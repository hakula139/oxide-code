# Slash Commands

Client-side command surface: `/help`, `/clear`, `/model`, `/status`, and friends.

## Implementation

13 built-ins: `/clear`, `/compact`, `/config`, `/delete`, `/diff`, `/effort`, `/help`, `/init`, `/model`, `/rename`, `/resume`, `/status`, `/theme`. Each lives in its own `slash/<name>.rs` file implementing `SlashCommand`. Adding one is a new file plus an entry in `BUILT_INS` (alphabetical).

- `/clear` rolls the session UUID and clears chat + file tracker.
- `/compact` compresses the visible transcript into a summary and resets the continuation chain.
- `/model` swaps the active model mid-session via `Client::set_model`.
- `/effort` sets an explicit effort tier.
- `/theme` swaps the TUI palette mid-session, with bare opening a live-preview list picker.
- `/init` synthesizes an AGENTS.md / CLAUDE.md author-or-update prompt and forwards to the agent loop.
- `/rename` sets the session title manually and locks out the AI title generator. Bare opens a single-line editor pre-filled with the current title.
- `/resume` swaps to a different session in place. Bare opens a searchable picker, while typed-arg jumps directly. Full design: [resume.md](resume.md).
- `/delete <id-prefix>` unlinks a saved session's JSONL after a Y/N confirm. The `/resume` picker offers the same gesture via Ctrl+D / Delete on the cursor row.
- `/config`, `/help`, and `/status` open a read-only [`KvOverview`](modals.md) modal. `/diff` is the lone printer because its output can run to hundreds of lines, where scrollback value beats modal cropping.

## Design Decisions

1. **Trait registry.** `trait SlashCommand` mirrors `tool::Tool`, with one file per command. Adding `/foo` means editing only the new file, with no central match arm of the kind Codex carries.

2. **Parse at submit.** `App::dispatch_user_action` runs `parse_slash` first, then dispatches locally or forwards.

3. **One synthetic block kind: `SystemMessageBlock`.** Left-bar in `accent`. Errors reuse `ErrorBlock`.

4. **Two-column popup, plain rows.** Name left, description right. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier. Names accept `:` and `.` for future `/plugin:cmd` namespace.

5. **Mid-session model + effort swap via `&mut Client`.** Both `/model <id>` and `/effort <level>` return `Forward(UserAction::SwapConfig { model, effort })`, the same payload both modals emit, so the typed-arg path and the modals share one resolver. Per-request paths re-read config every call so betas and `output_config` pick up the swap. Bare `/model` (combined picker) and bare `/effort` (slider) are also mutating because their submit path emits `SwapConfig`, so both forms refuse mid-turn. See [modals.md](modals.md).

6. **Slash commands never write user config files.** State stays session-only and a restart returns to config.toml values. Cross-session persistence will land as an explicit subcommand writing to a user-opted-in path, never a silent merge into a `~/.claude.json`-style file.

7. **Aliases resolve to canonical but display by surface.** `/clear` is canonical, while `/new` and `/reset` are aliases. The popup shows only the alias the user typed.

8. **No `/quit` or `/exit`.** Ctrl+C x2 / Ctrl+D already exit.

9. **Read-only kv views go through one shared modal primitive.** `/status`, `/config`, and `/help` all open a [`KvOverview`](modals.md) with title + sectioned label-value rows + footer. The modal is the response, so the typed `> /foo` line stays out of chat history (see decision 14).

10. **Built-in only in v1.** The trait registry leaves room for `~/.config/ox/commands/*.md` discovery later.

11. **Read-only commands fast-path the busy turn.** `classify` defaults to `SlashKind::ReadOnly`, and the dispatcher runs them client-side even when input is disabled. State-mutating commands override to `SlashKind::Mutating` and refuse mid-turn.

12. **Two command kinds, one trait return: `SlashOutcome { Done, Forward(UserAction) }`.** `Done` covers read-only commands. `Forward(_)` is state-mutating, handed back to the App which forwards to the agent loop.

13. **Modals open via a `SlashContext` side-channel.** Commands set `ctx.open_modal(Box::new(...))` and return `Done`. The dispatcher then harvests the slot after `execute` and pushes onto the App's modal stack. This keeps `SlashOutcome` derive-clean rather than adding a third variant. See [modals.md](modals.md) for the full modal design.

14. **Modal-only commands suppress their own echo.** `SlashCommand::echoes_input(args) -> bool` defaults to true. Modal-only forms (`/status`, `/config`, `/help`, plus bare `/effort`, `/model`, `/rename`, `/resume`, and `/theme`) override to false, since the modal IS the response and the typed `> /foo` line would orphan in history once the modal closes. Typed forms keep echoing because the swap-confirmation system message anchors the pair.

## Per-Command Notes

- **`/clear`**: Send-first ordering. Forward `UserAction::Clear` first, drop chat history only on success. `AgentEvent::SessionTitleUpdated` carries the originating session id so a slow Haiku title call straddling `/clear` doesn't repaint the cleared session.

- **`/compact`**: Refuses mid-turn, streams a summarization request, writes a compact boundary plus synthetic continuation message, resets the file tracker, and keeps queued prompts for the post-compact turn. Full design: [compact.md](compact.md).

- **`/init`**: Returns `Forward(UserAction::SubmitPrompt(PROMPT))` with a static body asking the model to author / update AGENTS.md. The expanded body is invisible in the live session but recorded in JSONL for resume.

- **`/model`**: Bare opens the combined picker ([modals.md](modals.md)). `/model <arg>` resolves via alias → exact / dated id → unique suffix → unique substring. `[1m]` is an opt-in tag (strip → resolve → reattach). Both forms are mutating and idle-gated because they can emit `UserAction::SwapConfig` and re-clamp current effort against the new model.

- **`/effort`**: Bare opens the Speed ↔ Intelligence slider ([modals.md](modals.md)), since a two-axis picker would force users through models they didn't mean to change. `/effort <level>` accepts the five concrete tiers, with no `auto` state. Both forms are mutating and idle-gated because they can emit `UserAction::SwapConfig`.

- **`/theme`**: Bare opens a live-preview list picker ([modals.md](modals.md)) over the built-in palettes. Up / Down repaints the TUI in the candidate, and Esc snaps back. `/theme <name>` validates against the curated roster and swaps directly. Custom file-path themes aren't accepted via the slash form.

- **`/status`**: Opens a [`KvOverview`](modals.md) of session descriptors. No args, no chat output. Esc / Enter both dismiss.

- **`/config`**: Opens a [`KvOverview`](modals.md) with two headed sections: resolved effective config, and the layered TOML source paths it was assembled from. Path discovery runs per-invocation so mid-session edits surface immediately.

- **`/help`**: Opens a [`KvOverview`](modals.md) listing every registered command with its description. Aliases parenthesize after the canonical name, and `usage()` placeholder appends.

- **`/diff`**: The lone printer. Pushes `git diff HEAD` plus untracked files into chat as a system message, capped at 64 KB on a UTF-8 boundary. Before the first commit, it combines `git diff --cached` and `git diff` because `HEAD` does not exist yet. Modal output would crop without scrollback, so the diff earns its place in the transcript.

- **`/rename`**: Bare opens a single-line title editor pre-filled with the current title (cap 80 chars, mirroring the actor's first-prompt cap), and `/rename <title>` applies directly. Both forms forward `UserAction::Rename` and lock out AI title generation for the rest of the session so a slow Haiku call can't overwrite the user's pick. `classify` is always `Mutating`. Bare suppresses echo because the modal IS the response, while typed-arg echoes since the swap-confirmation system message anchors the pair. See [modals.md](modals.md).

- **`/resume`** (alias `/continue`): Bare opens a searchable session picker ([modals.md](modals.md)), while `/resume <id-prefix>` resolves directly via a current-project-first lookup that widens to all projects on miss. Both forms refuse mid-turn and forward `UserAction::Resume`. Bare suppresses echo while typed-arg echoes. Full design: [resume.md](resume.md).

- **`/delete`**: Typed-arg only, while bare returns a friendly redirect. Picker entry is Ctrl+D / Delete inside `/resume`. Both push `ConfirmDeleteSessionModal` ([modals.md](modals.md)), which runs `SessionStore::delete` on Y / Enter and emits a `Deleted session {id}: {title}` line in chat. Live-session refusal is layered across the picker filter, the resolver, and the store-layer bail.

## Sources

- `crates/oxide-code/src/agent.rs`: `agent_turn` `Clear` and `SwapConfig` arms.
- `crates/oxide-code/src/session/display.rs`: shared `id_prefix`, `display_title`, and `format_metadata_line` used by the picker row and the delete confirm modal.
- `crates/oxide-code/src/session/resolver.rs`: `resolve_prefix_to_info` (current-project-first, widen on miss, scoped error messages).
- `crates/oxide-code/src/session/store.rs`: `SessionStore::delete` (live-session refusal at the FS boundary).
- `crates/oxide-code/src/slash.rs`: dispatch, `SlashOutcome`, shared test fixtures (`stamped_id`, `with_isolated_xdg`).
- `crates/oxide-code/src/slash/clear.rs`: `ClearCmd`, send-first ordering.
- `crates/oxide-code/src/slash/config.rs`: `/config` row builder plus sectioned `KvOverview` constructor.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` (destructive-action gate, sticky inline error cleared only on deliberate Y / N).
- `crates/oxide-code/src/slash/context.rs`: `SlashContext`, `open_modal` / `take_modal`.
- `crates/oxide-code/src/slash/delete.rs`: `DeleteCmd` plus live-id-aware resolver wrapper.
- `crates/oxide-code/src/slash/diff.rs`: `/diff` printer with 64 KB UTF-8-boundary cap.
- `crates/oxide-code/src/slash/effort.rs`: `EffortCmd`, level parser.
- `crates/oxide-code/src/slash/effort_slider.rs`: bare `/effort` Speed ↔ Intelligence slider modal.
- `crates/oxide-code/src/slash/help.rs`: `/help` row builder plus `KvOverview` constructor.
- `crates/oxide-code/src/slash/init.rs`: `InitCmd`, `PROMPT`.
- `crates/oxide-code/src/slash/model.rs`: `ModelCmd`, resolver.
- `crates/oxide-code/src/slash/picker.rs`: combined model + effort picker modal.
- `crates/oxide-code/src/slash/registry.rs`: `SlashCommand` trait, `BUILT_INS`, `SlashOutcome`.
- `crates/oxide-code/src/slash/rename.rs`: `RenameCmd` plus `RenameModal` editor.
- `crates/oxide-code/src/slash/resume.rs`: `ResumeCmd` plus `ResumePicker` (see `resume.md`).
- `crates/oxide-code/src/slash/status.rs`: `/status` row builder plus `KvOverview` constructor.
- `crates/oxide-code/src/slash/theme.rs`: `ThemeCmd`, picker open plus typed-arg validator.
- `crates/oxide-code/src/tui/app.rs`: `dispatch_user_action`, `apply_action_locally`, modal gate, echo gate.
- `crates/oxide-code/src/tui/modal.rs`: `Modal` trait, `ModalStack`, key routing.
- `crates/oxide-code/src/tui/modal/kv_overview.rs`: generic `KvOverview` / `KvSection` primitive.
- `crates/oxide-code/src/tui/modal/list_picker.rs`: generic `ListPicker` primitive.
- `crates/oxide-code/src/tui/modal/searchable_list.rs`: generic `SearchableList` primitive.
