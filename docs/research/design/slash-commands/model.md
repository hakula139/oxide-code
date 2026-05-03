# /model

Lists selectable Claude models or swaps the active model mid-session. Bare `/model` prints a text table from `SELECTABLE`; `/model <arg>` resolves against the wider `MODELS` table and emits `UserAction::SwitchModel(id)`.

The cross-command surface lives in [Slash Commands](README.md); this doc covers the model / effort-specific choices.

## Reference

- **Claude Code** (`commands/model/index.ts`, `commands/model/model.tsx`) — fullscreen Ink picker with model navigation and `← →` effort adjustment. Persists changes to `~/.claude/settings.json`.
- **Codex** (`SlashCommand::Model`) — in-transcript model popup. No cross-restart persistence.
- **opencode** — dialog picker. No textual arg form.

oxide-code ships the text form first: it matches the existing slash-output pattern and covers daily one-shot swaps (`/model opus`, `/model sonnet[1m]`) without adding modal key routing.

## oxide-code Today

- **Bare `/model`** lists the curated five-row `SELECTABLE` set and marks the exact active row. If the active model is an older config-only id, the footer names it so a list with no starred row is not confusing.
- **`/model <arg>`** strips a trailing `[1m]`, resolves the base, checks 1M support, then reattaches the tag. Resolution order: alias → exact / dated id → unique suffix → unique substring. Exact / dated pass-through is narrow by design, so malformed ids that merely contain a known model are rejected locally.
- **Effort coupling** stays explicit and lossy. `Client::set_model` re-clamps the current effective effort against the new model. If a swap lowers `xhigh` to `high`, swapping back does not restore `xhigh`; the confirmation says what happened and `/effort xhigh` is the recovery path.

## Design Decisions for oxide-code

1. **List, not picker.** The textual list keeps slash output uniform. The combined model + effort picker is deferred to a modal component.
2. **`SELECTABLE` curates browsing; `MODELS` gates acceptance.** The list shows current daily choices; manual entry still reaches older known ids needed for capability lookup.
3. **`[1m]` is an opt-in tag, not a model row.** Strip → resolve → reattach lets `opus[1m]` reuse the alias path and rejects `haiku[1m]` through the same capability check as spelled-out ids.
4. **Four-tier resolver.** Aliases cover daily use; exact / dated ids preserve manual API ids; unique suffix handles short forms like `opus-4`; unique substring is the final convenience tier.
5. **Session-only persistence.** Slash swaps mutate runtime state only. Restart returns to config / environment.
6. **`/effort` is explicit-only.** The list view mirrors Claude Code's speed / intelligence levels, but the text command only accepts concrete tiers (`low`, `medium`, `high`, `xhigh`, `max`). No `auto` state is tracked.

## Deferred

- Combined `/model` + `/effort` picker with arrow-key model navigation and `← →` effort adjustment.
- Argument-aware completion for model ids and effort levels.
- Explicit persistence subcommand if cross-restart slash choices become worth storing.

## Sources

- `crates/oxide-code/src/slash/model.rs` — `ModelCmd`, `SELECTABLE`, `ALIASES`, resolver, list renderer.
- `crates/oxide-code/src/slash/effort.rs` — `EffortCmd`, explicit-level parser, effort list renderer.
- `crates/oxide-code/src/client/anthropic.rs` — `Client::set_model`, `Client::set_effort`.
- `crates/oxide-code/src/agent/event.rs` — switch actions and events.
- `crates/oxide-code/src/tui/app.rs` — switch confirmations.
- `crates/oxide-code/src/model.rs` — `MODELS` and `Capabilities`.
- `claude-code/src/commands/model/model.tsx` — picker reference.
