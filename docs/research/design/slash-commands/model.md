# /model

Lists the curated set of selectable Claude models or swaps the active one mid-session. Bare `/model` prints a `(* = active)` table; `/model <arg>` resolves the argument through three tiers (alias → exact id → unique substring) against the same curated set, and on a unique match the agent loop calls `Client::set_model` and emits `AgentEvent::ModelSwitched`. The TUI handler refreshes the status bar and `session_info`, then pushes a `Switched to ...` confirmation block that surfaces effort changes (clamped, cleared, model-default).

This is the first slash command that takes an argument and the first runtime-mutable mid-session swap. The cross-command surface (registry, parser, popup, dispatch) lives in [Slash Commands](README.md); this doc covers `/model` only.

## Reference

- **Claude Code** (`commands/model/index.ts`, `commands/model/model.tsx`) — `local-jsx` modal picker. Bare `/model` opens a fullscreen Ink picker with arrow-key navigation, an effort-cycling indicator (`← →` to adjust), and a current-model marker. `/model <arg>` accepts exact ids OR a small alias set (`sonnet`, `opus`, `haiku`, `best`, `sonnet[1m]`, `opus[1m]`, `opusplan`); unknown args return `Model '<id>' not found`. Confirmation: `Set model to **<marketing>**` plus optional `with **<effort>** effort`. Persists the pick to `~/.claude/settings.json` on every change.
- **Codex** (`SlashCommand::Model`) — opens an in-transcript model selection popup; arg form not surfaced in the popup. No persistence beyond the running process.
- **opencode** — slash-popover dispatches a dialog picker. No textual arg form.

oxide-code adopts the **list + arg-resolve** shape: text-only output, no modal overlay. The interactive picker is an explicit follow-up — it needs a new component, key-routing changes (Up / Down for model, Left / Right for effort, Enter to confirm), and either a fullscreen layer or a new chat-anchored block. The textual form ships value at a fraction of the diff and the substring + alias resolver already covers the daily case (`/model opus`, `/model sonnet[1m]`) in one line.

## Mapping table

Per-step alignment with Claude Code's reference flow. Only the rows where oxide-code does something different are listed; the n/a rows (Fast Mode, plan-mode reset, API test call, mega-file persistence) are uniformly "doesn't apply" and add nothing here.

| Claude Code step                       | oxide-code surface                                                       | Notes                                                                                              |
| -------------------------------------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| Open Ink modal picker                  | `ModelCmd::execute("")` pushes a `SystemMessageBlock`                    | List, not picker. See [Deferred](#deferred).                                                       |
| `/model <id>` exact id + alias map     | `resolve_model_arg`: alias → exact-match `SELECTABLE` → unique substring | `[1m]` is first-class — `/model opus[1m]` and `/model claude-opus-4-7[1m]` both resolve.           |
| Unknown id → `Model '<id>' not found`  | `Err("Unknown model: \`<arg>\`. Run \`/model\` for the list...")`        | Renders via the dispatcher's `/{name}: {msg}` envelope as an `ErrorBlock`.                         |
| `Set model to **<name>**` confirmation | `Switched to <marketing> (<id>)` plus an effort clause                   | Effort clause spells out clamped / cleared / model-default cases instead of silent loss.           |
| Persist to `~/.claude/settings.json`   | n/a                                                                      | Session-only by stance — see [README.md decision 6](README.md#design-decisions-for-oxide-code).    |

## oxide-code Today

Two surfaces:

- **Bare `/model`** — `ModelCmd::execute` pushes a `SystemMessageBlock` rendered by `render_model_list`. Header carries the `(* = active)` legend so the marker doesn't need a separate explanation. The active row is the **exact** match between `info.config.model_id` and a `SELECTABLE` entry — `[1m]` distinctness matters because the 1M-tagged variant is a separate selectable row. The footer documents the alias + `[1m]` syntax and the effort-clamp behavior. When the current model is not in `SELECTABLE` (e.g. the user has `model = "claude-opus-4-1"` set via config), an extra footer line names it explicitly so the user understands why nothing is starred.
- **`/model <arg>`** — `resolve_model_arg` walks three tiers in order:
  1. **Alias substitution.** `opus`/`sonnet`/`haiku` map to the latest non-1M row; `opus[1m]` and `sonnet[1m]` opt into the 1M variant.
  2. **Exact id match against `SELECTABLE`.** Wins over substring so `/model claude-opus-4-7` resolves to the bare row instead of being ambiguous with `claude-opus-4-7[1m]`.
  3. **Unique substring match against `SELECTABLE`.** Multiple matches surface a candidate list with an alias hint.

  On a unique resolution, `ModelCmd::execute` returns `Ok(SlashOutcome::Action(UserAction::SwitchModel(id)))`. The dispatcher hands the action to `App::apply_action_locally`, which forwards it through `user_tx` to the agent loop's `SwitchModel` arm. The arm calls `Client::set_model(id)`, which:

  1. Re-clamps effort against the new caps via `Capabilities::resolve_effort` (the same seam `Config::load` uses).
  2. Updates `config.model` in place so per-request `compute_betas`, `output_config`, and `context_management` paths re-read live on the next stream.
  3. Returns the resolved `Option<Effort>` so the agent loop can ship `AgentEvent::ModelSwitched { model_id, effort }` without re-deriving.

The `ModelSwitched` arm in `App::handle_agent_event` then refreshes:

- `session_info.config.model_id` and `session_info.config.effort` for `/status` and `/config`. Marketing name is derived on demand via `info.marketing_name()` so it can't drift from `model_id`.
- `StatusBar::set_model` so the next render shows the new marketing label without re-reading `session_info`.
- A `SystemMessageBlock` reading `Switched to <marketing> (<id>)` plus an explicit effort clause: `· effort <level>.` when unchanged, `· effort <level> (clamped from <prev>).` when clamped down, `· effort <level> (model default).` when the previous effort was None, or `Effort cleared (model has no effort tier).` when the new model has no effort tier.

## Design Decisions for oxide-code

1. **List, not modal picker.** The textual list keeps the slash-command surface uniform — every other built-in lands as a `SystemMessageBlock` or `ErrorBlock`. The interactive picker (Claude Code-style fullscreen with arrow-key model navigation and `← →` effort adjustment) is a [follow-up PR](#deferred); the textual form already covers the daily case.
2. **`SELECTABLE` is a curated subset of `MODELS`.** `crates/oxide-code/src/model.rs::MODELS` stays comprehensive (10 rows) so the capability layer keeps resolving betas / `output_config` / `context_management` for users who set older models via config or env. `slash::model::SELECTABLE` ships only the five rows the user actually swaps between (Opus 4.7, Sonnet 4.6, Haiku 4.5, plus the two `[1m]` variants). Separation of concerns: `MODELS` is "what we know how to talk to", `SELECTABLE` is "what `/model` shows in its picker".
3. **`[1m]` is first-class in `SELECTABLE`.** `claude-opus-4-7` and `claude-opus-4-7[1m]` are separate rows. Exact-match wins over substring so typing the bare id never silently routes to the 1M variant (or vice versa). This closes a hole the previous shape had: a user on `claude-opus-4-7[1m]` who typed `/model claude-opus-4-7` would have lost 1M context with no user-visible signal.
4. **Three-tier resolution: alias → exact → substring.** Aliases (`opus`/`sonnet`/`haiku`) cover the daily case. Exact match prevents `/model claude-opus-4-7` from being ambiguous against the `[1m]` variant. Substring covers everything in between (`/model haiku-4-5` → `claude-haiku-4-5`). Tier ordering is the heart of the resolver; pinned in tests.
5. **Effort re-clamps lossy on round-trip.** `set_model` re-clamps the _current_ `config.effort` rather than the user's original raw pick. An `xhigh` user who swaps to Sonnet 4.6 gets `high`; swapping back to Opus 4.7 leaves them at `high`, not the original `xhigh`. The confirmation message explicitly surfaces the clamp (`(clamped from xhigh)`) so the user sees what just happened. Lossless round-trip would require tracking the raw pick alongside the clamped value — deferred.
6. **Session-only persistence.** `/model` writes nothing to disk. Restart returns to `~/.config/ox/config.toml` / `ox.toml` / `ANTHROPIC_MODEL`. Per cross-command decision 6 in [README.md](README.md#design-decisions-for-oxide-code) — Claude Code's `~/.claude/settings.json` mega-write is rejected. When persistence earns its keep, it lands as an explicit subcommand (`/model save <id>`) writing to a user-opted-in path.
7. **Args-aware `is_read_only(&self, args: &str)`.** Bare `/model` is read-only and dispatches mid-turn (the user can browse models without waiting). The arg-swap form races the in-flight `Client` and refuses mid-turn. The trait method takes `args` so each command can classify per-form; default impl ignores args and reports read-only.
8. **`Client::set_model` returns `Option<Effort>`.** No wrapper struct — marketing name is derivable from `model_id` via `marketing_or_id`, so the agent loop ships `model_id` + `effort` and the TUI computes marketing locally where the chat block renders. One source of derivation, no stale-state risk between agent and TUI layers.
9. **Confirmation surfaces effort changes explicitly.** `format_swap_confirmation` distinguishes four cases: unchanged (`· effort high.`), clamped down (`· effort high (clamped from xhigh).`), model default (`· effort high (model default).` when previous was None), and cleared (`. Effort cleared (model has no effort tier).`). Lossy round-trip is documented but no longer silent.

## Deferred

Behaviors Claude Code's `/model` ships that oxide-code skips today, with the subsystem each gates on:

1. **Interactive picker.** A modal overlay or chat-anchored pick list with arrow-key model navigation, `← →` effort adjustment, and Enter-to-confirm — see Claude Code's `commands/model/model.tsx`. Needs new key routing (a modal-mode flag so `InputArea` doesn't eat the arrow keys), a new component (likely a chat-anchored interactive `ChatBlock`), and effort-adjuster state plumbing. Tracked in `docs/roadmap.md` § Slash Commands; lands in a follow-up PR.
2. **Lossless effort across swaps.** Track the user's raw `effort_pick` separately from the resolved `effort`. `Config::load` already has both values inline; persisting `effort_pick` on `Config` and re-clamping from it on every `set_model` would close the round-trip gap. Defer — most users never re-clamp by hand and the lost precision is recoverable via env or restart. The new explicit-clamp message at least surfaces the loss instead of silently degrading.
3. **Argument-aware popup completion.** Today the autocomplete popup ranks command names; after typing `/model` plus a space, the popup goes silent. Extending `SlashCommand` with a `complete(args_partial: &str) -> Vec<MatchedCompletion>` hook would let `ModelCmd` suggest model ids inline. Lands when a second arg-taking command (likely `/theme`) makes the abstraction earn its keep.
4. **Persistence subcommand** (`/model save <id>`). Writes to a user-opted-in `~/.config/ox/config.toml.local`. Lands when there's a clear case for cross-restart persistence beyond env / config edits.
5. **Per-row effort hint.** The list could show each row's `default_effort` (e.g. `claude-opus-4-7  Claude Opus 4.7  · default xhigh`). Cosmetic — defer.

## Sources

- `crates/oxide-code/src/slash/model.rs` — `ModelCmd`, `SELECTABLE`, `ALIASES`, `resolve_model_arg`, `render_model_list`, `is_read_only(args)`, tests.
- `crates/oxide-code/src/client/anthropic.rs` — `Client::set_model` returns `Option<Effort>` (no `ModelSwap` struct).
- `crates/oxide-code/src/agent/event.rs` — `UserAction::SwitchModel(String)`, `AgentEvent::ModelSwitched { model_id, effort }`.
- `crates/oxide-code/src/main.rs` — `agent_loop_task` `SwitchModel` arm.
- `crates/oxide-code/src/tui/app.rs` — `handle_agent_event::ModelSwitched` arm, `format_swap_confirmation` helper.
- `crates/oxide-code/src/slash/context.rs` — `SessionInfo::marketing_name` accessor (derives from `config.model_id`).
- `crates/oxide-code/src/model.rs` — `MODELS`, `Capabilities::resolve_effort`, `default_effort`, `clamp_effort`.
- `crates/oxide-code/src/prompt/environment.rs` — `marketing_or_id` helper (single seam for the unknown-id fallback).
- `claude-code/src/commands/model/model.tsx` — reference flow for the picker, alias map, and confirmation wording.
