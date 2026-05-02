# /model

Lists the models known to oxide-code or swaps the active one mid-session. Bare `/model` prints a table of `(id, marketing name)` rows with the active row marked. `/model <id>` substring-matches the argument against the same table; on a unique match the agent loop calls `Client::set_model` (re-clamping `config.effort` against the new caps) and emits `AgentEvent::ModelSwitched`. The TUI's handler refreshes `session_info`, the cached status-bar label, and the chat with a `Switched to ...` confirmation block.

This is the first slash command that takes an argument and the first runtime-mutable mid-session swap. The cross-command surface (registry, parser, popup, dispatch) lives in [Slash Commands](README.md); this doc covers `/model` only.

## Reference

- **Claude Code** (`commands/model/index.ts`, `commands/model/model.tsx`) — `local-jsx` modal picker. Bare `/model` opens a fullscreen Ink picker with arrow-key navigation, an effort-cycling indicator, and a "current model" marker. `/model <arg>` accepts exact ids OR a small alias set (`sonnet`, `opus`, `haiku`, `best`, `sonnet[1m]`, `opus[1m]`, `opusplan`); unknown args return `Model '<id>' not found`. Confirmation: `Set model to **<marketing>**` plus optional `with **<effort>** effort`. Persists the pick to `~/.claude/settings.json` on every change.
- **Codex** (`SlashCommand::Model`) — opens an in-transcript model selection popup; arg form not surfaced. No persistence beyond the running process.
- **opencode** — slash-popover dispatches a dialog picker. No textual arg form.

oxide-code adopts the **list + arg-resolve** shape: text-only output, no modal overlay. The interactive picker would need a new ChatBlock-or-overlay component and key-routing changes — deferred until the bare list view turns out to be insufficient. The textual form already covers the daily case ("swap to Sonnet 4.6") in one line.

## Mapping table

Per-step alignment with Claude Code's reference flow. Steps oxide-code skips are called out so the boundary stays explicit.

| Claude Code step                         | oxide-code surface                                          | Notes                                                                          |
| ---------------------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Open Ink modal picker                    | `ModelCmd::execute("")` pushes a `SystemMessageBlock`       | List, not picker. See [Deferred](#deferred).                                   |
| Effort-cycling `[●]` indicator           | n/a                                                         | The list table doesn't show effort per row; `/status` and `/config` do.        |
| `/model <id>` exact id + alias map       | `resolve_model_arg` substring-matches `MODELS[*].id_substr` | Substring match covers both forms (a leaf id is its own unique substring).     |
| Unknown id → `Model '<id>' not found`    | `Err("unknown model: <arg>. Run /model for the list.")`     | Renders via the dispatcher's `/{name}: {msg}` envelope as an `ErrorBlock`.     |
| API test call to validate                | n/a                                                         | oxide-code trusts the local `MODELS` table; a 4xx surfaces on the next stream. |
| `Set model to **<name>**` confirmation   | `Switched to <marketing> (<id>) · effort <level>.`          | Effort clause omitted when the new model can't accept `output_config.effort`.  |
| `mainLoopModelForSession: null` reset    | n/a                                                         | No plan-mode override to clear.                                                |
| Persist to `~/.claude/settings.json`     | n/a                                                         | Session-only by stance — see decision 4 below.                                 |
| Auto-toggle Fast Mode on incompatibility | n/a                                                         | No Fast Mode equivalent.                                                       |

## oxide-code Today

The two surfaces:

- **Bare `/model`** — `ModelCmd::execute` pushes a `SystemMessageBlock` rendered by `render_model_list`. The active row is the one `crate::model::lookup` resolves the session's `model_id` to (substring equality on `id_substr` would also mark family-base rows when the user is on a specific 4.x release). The footer reminds the user that a unique substring works (`/model haiku-4-5`) and that effort clamps to the new model's ceiling.
- **`/model <id>`** — `resolve_model_arg` walks `MODELS`, collects every row whose `id_substr` contains the argument, and returns the canonical id on a unique match. Zero matches → `unknown model: <arg>`. Two or more → `ambiguous: <arg> matches N models (...)`. On a unique match, `ModelCmd::execute` returns `Ok(SlashOutcome::Action(UserAction::SwitchModel(id)))`. The dispatcher hands the `UserAction` to `App::apply_action_locally`, which forwards it through `user_tx` to the agent loop's `SwitchModel` arm. The arm calls `Client::set_model`, which:
  1. Re-clamps `config.effort` against the new caps (`Some(pick) → caps.clamp_effort`, `None → caps.default_effort`).
  2. Resolves the marketing name via `prompt::environment::marketing_name`, falling back to the raw id for unknown rows.
  3. Updates `config.model` in place.
  4. Returns a `ModelSwap { model_id, marketing, effort }` so the agent loop builds `AgentEvent::ModelSwitched` without a second `model::lookup`.

The `ModelSwitched` arm in `App::handle_agent_event` then refreshes:

- `session_info.model` and `session_info.config.{model_id, effort}` for `/status` and `/config`.
- `StatusBar::set_model` so the next render shows the new marketing name without re-reading `session_info`.
- A `SystemMessageBlock` reading `Switched to <marketing> (<id>)` (with `· effort <level>` appended when the model accepts the field).

## Design Decisions for oxide-code

1. **List, not modal picker.** The textual list keeps the slash-command surface uniform — every other built-in lands as a `SystemMessageBlock` or `ErrorBlock`. A real picker needs a new component and key-routing branch; the textual form ships value at a fraction of the diff and leaves room for a picker follow-up.
2. **Substring resolution against `MODELS[*].id_substr`.** Walking the table once classifies the argument as not-found / unique / ambiguous in one pass — the result slice match (`[]` / `[id]` / `_`) makes the algorithm fall out of pattern matching. Family-base ids (`claude-opus-4`) are substrings of every more-specific row, so they always come back ambiguous; that's the intended UX (`opus-4` could mean 4.7, 4.6, 4.5, or 4.1, so the user must say which).
3. **Effort re-clamps lossy on round-trip.** `set_model` re-clamps the _current_ `config.effort` rather than the user's original raw pick. An `xhigh` user who swaps to Sonnet 4.6 gets `high`; swapping back to Opus 4.7 leaves them at `high`, not the original `xhigh`. Tracking the raw pick alongside the clamped value would fix this — deferred. The simpler shape unblocks the feature; the lost precision is recoverable via env / restart.
4. **Session-only persistence.** `/model` writes nothing to disk. Restart returns to `~/.config/ox/config.toml` / `ox.toml` / `ANTHROPIC_MODEL`. Per cross-command decision 6 in [README.md](README.md#design-decisions-for-oxide-code) — Claude Code's `~/.claude/settings.json` mega-write is rejected. When persistence earns its keep, it lands as an explicit subcommand (`/model save <id>`) writing to a user-opted-in path.
5. **`is_read_only = false` for both forms.** The argument-swap form races the in-flight `Client`; refusing mid-turn is mandatory. The list form is technically read-only, but the trait method has no view of args, so refusing both uniformly avoids args-aware classification. The list view is one turn away — accepted.
6. **`Client::set_model` returns `ModelSwap`.** The agent loop's arm needs `marketing` and `effort` for the `ModelSwitched` event. Returning them as a struct rather than re-deriving avoids a second `marketing_name` / `clamp_effort` call site that could drift out of sync with `set_model`'s own resolution rules.
7. **App handler refreshes three surfaces in one shot.** `session_info.model`, `session_info.config.model_id`, `session_info.config.effort`, `StatusBar::set_model`, and a `SystemMessageBlock`. The status-bar label is cached separately so per-frame rendering doesn't re-read `session_info`; mutating only the snapshot would leave the bar out of sync until something else triggered a setter call.
8. **Confirmation message is single-line.** `Switched to <marketing> (<id>).` plus optional `· effort <level>.` Claude Code's confirmation grows extra clauses for Fast Mode and 1M billing — neither concept exists in oxide-code, so the simpler shape is honest.

## Deferred

Behaviors Claude Code's `/model` ships that oxide-code skips today, with the subsystem each gates on:

1. **Interactive picker.** A modal overlay or a chat-anchored pick list with arrow-key navigation and Enter-to-confirm. Needs new key routing and either a fullscreen layer or a new `ChatBlock` variant. Lands when the textual list view turns out to be insufficient; the substring resolver already covers the daily swap case.
2. **Lossless effort across swaps.** Track the user's raw `effort_pick` separately from the resolved `effort`. `Config::load` already has both values inline; persisting `effort_pick` on `Config` and re-clamping from it on every `set_model` would close the round-trip gap. Defer — most users never re-clamp by hand and the lost precision is recoverable via env or restart.
3. **Argument-aware popup completion.** Today the autocomplete popup ranks command names; after typing `/model` plus a space, the popup goes silent. Extending `SlashCommand` with a `complete(args_partial: &str) -> Vec<MatchedCompletion>` hook would let `ModelCmd` suggest model ids inline. Lands when a second arg-taking command (likely `/theme`) makes the abstraction earn its keep.
4. **Persistence subcommand** (`/model save <id>`). Writes to a user-opted-in `~/.config/ox/config.toml.local`. Lands when there's a clear case for cross-restart persistence beyond env / config edits.
5. **Per-row effort hint.** The list could show each row's `default_effort` (e.g. `claude-opus-4-7  Claude Opus 4.7  · default xhigh`). Cosmetic — defer.
6. **Family-base disambiguation list.** When `/model opus-4` is ambiguous, the error lists every match. A more polished UX would render the same five rows as a follow-up `SystemMessageBlock` with a hint like "did you mean `/model opus-4-7`?". Defer until users actually hit this enough to ask for it.

## Sources

- `crates/oxide-code/src/slash/model.rs` — `ModelCmd`, `resolve_model_arg`, `render_model_list`, `is_read_only` override, tests.
- `crates/oxide-code/src/slash/registry.rs` — registers `ModelCmd` in `BUILT_INS`.
- `crates/oxide-code/src/agent/event.rs` — `UserAction::SwitchModel(String)`, `AgentEvent::ModelSwitched { model_id, marketing, effort }`.
- `crates/oxide-code/src/client/anthropic.rs` — `Client::set_model`, `ModelSwap`, effort re-clamp tests.
- `crates/oxide-code/src/main.rs` — `agent_loop_task` `SwitchModel` arm: calls `set_model`, emits `ModelSwitched`.
- `crates/oxide-code/src/tui/app.rs` — `apply_action_locally` slash branch (forwards `Action(SwitchModel(_))`), `handle_agent_event::ModelSwitched` arm.
- `crates/oxide-code/src/tui/components/status.rs` — `StatusBar::set_model` setter.
- `crates/oxide-code/src/model.rs` — `MODELS`, `lookup`, `Capabilities::clamp_effort`, `default_effort`.
- `claude-code/src/commands/model/model.tsx` — reference flow for the picker, alias map, and confirmation wording.
