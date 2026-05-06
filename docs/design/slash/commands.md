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

1. **Trait registry, not enum.** `trait SlashCommand` mirrors `tool::Tool`; one file per command. Codex's giant `match` is rejected — adding `/foo` should mean editing only the new file.
2. **Parse at submit, not in `InputArea`.** `App::dispatch_user_action` runs `parse_slash` first, then dispatches locally or forwards.
3. **One synthetic block kind: `SystemMessageBlock`.** Left-bar in `accent`. Errors reuse `ErrorBlock`.
4. **Two-column popup, plain rows.** Name left, description right. Filter ranks name-prefix > alias-prefix > name-substring > alias-substring, alphabetical within each tier. Names accept `:` and `.` for future `/plugin:cmd` namespace.
5. **Mid-session model + effort swap via `&mut Client`.** Both `/model <id>` and `/effort <level>` return `Forward(UserAction::SwapConfig { model, effort })` — the same payload both modals emit, so the typed-arg path and the modals share one resolver. Per-request paths re-read config every call so betas / `output_config` pick up the swap. `classify(&self, args: &str) -> SlashKind` lets bare `/model` (combined picker) and bare `/effort` (slider) dispatch mid-turn as read-only modals; arg-bearing forms refuse. See [modals.md](modals.md).
6. **Slash commands never write user config files.** Session-only state. Restart returns to config.toml values. Deliberate rejection of Claude Code's silent mega-file writes.
7. **Aliases resolve to canonical but display by surface.** `/clear` is canonical; `/new` and `/reset` are aliases. The popup shows only the alias the user typed.
8. **No `/quit` or `/exit`.** Ctrl+C x2 / Ctrl+D already exit.
9. **`/config` is read-only in v1.** Prints resolved effective config + layered file paths.
10. **Built-in only in v1.** The trait registry leaves room for `~/.config/ox/commands/*.md` discovery later.
11. **Read-only commands fast-path the busy turn.** `classify` defaults to `SlashKind::ReadOnly`; dispatcher runs them client-side even when input is disabled. State-mutating commands override to `SlashKind::Mutating` and refuse mid-turn.
12. **Two command kinds, one trait return: `SlashOutcome { Done, Forward(UserAction) }`.** `Done` covers read-only commands. `Forward(_)` is state-mutating: handed back to the App, which forwards to the agent loop.
13. **Modals open via a `SlashContext` side-channel, not a third `SlashOutcome` variant.** Commands set `ctx.open_modal(Box::new(...))` and return `Done`; the dispatcher harvests the slot after `execute` and pushes onto the App's modal stack. Keeps `SlashOutcome` derive-clean. See [modals.md](modals.md) for the full modal design.

## Per-Command Notes

One bullet per command — non-obvious mechanics or tradeoffs only. Surface details (aliases, args, output) belong in [the user guide](../../guide/slash-commands.md).

- **`/clear`** — Send-first ordering: forward `UserAction::Clear` first, drop chat history only on success. `AgentEvent::SessionTitleUpdated` carries the originating session id so a slow Haiku title call straddling `/clear` doesn't repaint the cleared session.
- **`/init`** — Returns `Forward(UserAction::SubmitPrompt(PROMPT))` with a static body asking the model to author / update AGENTS.md. The expanded body is invisible in the live session but recorded in JSONL for resume.
- **`/model`** — Bare opens the combined picker ([modals.md](modals.md)). `/model <arg>` resolves via alias → exact / dated id → unique suffix → unique substring; `[1m]` is an opt-in tag (strip → resolve → reattach). Both forms emit `UserAction::SwapConfig` and re-clamp current effort against the new model.
- **`/effort`** — Bare opens the Speed ↔ Intelligence slider ([modals.md](modals.md)); two-axis picker would force users through models they didn't mean to change. `/effort <level>` accepts the five concrete tiers — no `auto` state.
- **`/status`** — Bare opens the read-only overview modal ([modals.md](modals.md)). No args, no chat output. Esc / Enter both dismiss.

## Sources

- `crates/oxide-code/src/slash.rs` — dispatch, `SlashOutcome`.
- `crates/oxide-code/src/slash/registry.rs` — `SlashCommand` trait, `BUILT_INS`, `SlashOutcome`.
- `crates/oxide-code/src/slash/context.rs` — `SlashContext`, `open_modal` / `take_modal`.
- `crates/oxide-code/src/slash/clear.rs` — `ClearCmd`, send-first ordering.
- `crates/oxide-code/src/slash/init.rs` — `InitCmd`, `PROMPT`.
- `crates/oxide-code/src/slash/model.rs` — `ModelCmd`, resolver.
- `crates/oxide-code/src/slash/effort.rs` — `EffortCmd`, level parser.
- `crates/oxide-code/src/slash/picker.rs` — combined model + effort picker modal.
- `crates/oxide-code/src/slash/effort_slider.rs` — bare `/effort` Speed ↔ Intelligence slider modal.
- `crates/oxide-code/src/slash/status_modal.rs` — `/status` overview modal.
- `crates/oxide-code/src/tui/app.rs` — `dispatch_user_action`, `apply_action_locally`, modal gate.
- `crates/oxide-code/src/tui/modal.rs` — `Modal` trait, `ModalStack`, key routing.
- `crates/oxide-code/src/tui/modal/list_picker.rs` — generic `ListPicker` primitive.
- `crates/oxide-code/src/agent.rs` — `agent_turn` Clear and SwapConfig arms.
