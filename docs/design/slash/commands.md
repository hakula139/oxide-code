# Slash Commands

Client-side command surface: `/help`, `/clear`, `/model`, `/status`, and friends.

## Implementation

Eight built-ins: `/clear`, `/config`, `/diff`, `/effort`, `/help`, `/init`, `/model`, `/status`. Each lives in its own `slash/<name>.rs` file implementing `SlashCommand`. Adding one is a new file plus an entry in `BUILT_INS` (alphabetical).

- `/clear` rolls the session UUID and clears chat + file tracker.
- `/model` swaps the active model mid-session via `Client::set_model`.
- `/effort` sets an explicit effort tier.
- `/init` synthesizes an AGENTS.md / CLAUDE.md author-or-update prompt and forwards to the agent loop.
- `/config`, `/diff`, `/help`, `/status` are read-only.

## Design Decisions

1. **Trait registry, not enum.** `trait SlashCommand` mirrors `tool::Tool`; one file per command. Codex's giant `match` is rejected -- adding `/foo` should mean editing only the new file.
2. **Parse at submit, not in `InputArea`.** `App::dispatch_user_action` runs `parse_slash` first, then dispatches locally or forwards.
3. **One synthetic block kind: `SystemMessageBlock`.** Left-bar in `accent`. Errors reuse `ErrorBlock`.
4. **Two-column popup, plain rows.** Name left, description right. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier. Names accept `:` and `.` for future `/plugin:cmd` namespace.
5. **Mid-session model + effort swap via `&mut Client`.** `/model` returns `Forward(UserAction::SwitchModel(id))`, `/effort` returns `Forward(UserAction::SwitchEffort(pick))`. Per-request paths re-read config every call so betas / `output_config` pick up the swap. `classify(&self, args: &str) -> SlashKind` lets bare list-view forms dispatch mid-turn while arg-bearing forms refuse.
6. **Slash commands never write user config files.** Session-only state. Restart returns to config.toml values. Deliberate rejection of Claude Code's silent mega-file writes.
7. **Aliases resolve to canonical but display by surface.** `/clear` is canonical; `/new` and `/reset` are aliases. The popup shows only the alias the user typed.
8. **No `/quit` or `/exit`.** Ctrl+C x2 / Ctrl+D already exit.
9. **`/config` is read-only in v1.** Prints resolved effective config + layered file paths.
10. **Built-in only in v1.** The trait registry leaves room for `~/.config/ox/commands/*.md` discovery later.
11. **Read-only commands fast-path the busy turn.** `classify` defaults to `SlashKind::ReadOnly`; dispatcher runs them client-side even when input is disabled. State-mutating commands override to `SlashKind::Mutating` and refuse mid-turn.
12. **Two command kinds, one trait return: `SlashOutcome { Done, Forward(UserAction) }`.** `Done` covers read-only commands. `Forward(_)` is state-mutating: handed back to the App, which forwards to the agent loop.

## Per-Command Notes

### /clear

Rolls the session UUID, finalizes the old JSONL (still resumable via `ox -c`), drops in-memory messages, clears file tracker, clears AI title. Aliases: `/new`, `/reset`. No confirmation prompt -- the cleared session is resumable.

Key design: send-first ordering in `execute` -- forward `UserAction::Clear` to `user_tx` first; only on success drop the chat history. `SessionHandle::roll` is the testable extraction point (snapshot-before-clear, replace-before-finalize). `AgentEvent::SessionTitleUpdated` carries the originating session id so a slow Haiku title call straddling `/clear` doesn't paint the old title onto the fresh session.

### /init

Returns `SlashOutcome::Forward(UserAction::SubmitPrompt(PROMPT))` with a static body asking the model to author/update AGENTS.md. The App pushes the typed `/init` line as a `UserMessage` block, flips turn-start UI state, then forwards. The expanded body is invisible in the live session; on resume, JSONL records the full body.

### /model

Bare `/model` lists the curated `LISTED_MODELS` set marking the active row. `/model <arg>` resolves via: alias -> exact/dated id -> unique suffix -> unique substring. `[1m]` is an opt-in tag (strip -> resolve -> reattach). Effort coupling stays explicit and lossy -- re-clamps current effort against the new model.

### /effort

Mirrors `/model` shape. Accepts concrete tiers (`low`, `medium`, `high`, `xhigh`, `max`). No `auto` state.

## Sources

- `crates/oxide-code/src/slash.rs` -- dispatch, `SlashOutcome`.
- `crates/oxide-code/src/slash/registry.rs` -- `SlashCommand` trait, `BUILT_INS`, `SlashOutcome`.
- `crates/oxide-code/src/slash/clear.rs` -- `ClearCmd`, send-first ordering.
- `crates/oxide-code/src/slash/init.rs` -- `InitCmd`, `PROMPT`.
- `crates/oxide-code/src/slash/model.rs` -- `ModelCmd`, `LISTED_MODELS`, resolver.
- `crates/oxide-code/src/slash/effort.rs` -- `EffortCmd`, level parser.
- `crates/oxide-code/src/tui/app.rs` -- `dispatch_user_action`, `apply_action_locally`.
- `crates/oxide-code/src/agent.rs` -- `agent_loop_task` Clear arm, model/effort switch handling.
