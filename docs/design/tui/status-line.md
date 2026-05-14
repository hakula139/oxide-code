# Status Line

Design for a configurable TUI status line.

## Scope

The TUI status line should be an ordered roster of built-in segments. This ships:

- User-controlled segment order.
- Segments backed by local state today.
- Cache-aware context and session-cost display.

Implemented segments:

- current directory
- git branch
- model
- model with effort
- context used
- estimated session cost
- run state
- thread title
- current time

Out of scope:

- Command-based custom renderers.
- ccusage block / daily totals.
- Five-hour or weekly account-limit display.
- Pull request and task-progress segments.
- Persisting cost totals across resume.
- Non-Anthropic provider pricing.

## Implementation

Add `StatusLineSegment` in `config.rs` with TOML values:

- `current-dir`
- `git-branch`
- `model`
- `model-with-effort`
- `context-used`
- `session-cost`
- `run-state`
- `thread-title`
- `current-time`

`[tui] status_line = [...]` controls the order. `OX_STATUS_LINE` accepts the same comma-separated names. Segment colors always come from the active theme.

`StatusBar` keeps component state and delegates segment formatting to `tui/components/status/line.rs`.

Segment render rules:

- Return `None` when data is unavailable.
- Do not render placeholders for absent branch, title, usage, or pricing.
- Use active theme styles; color customization belongs in theme overrides.
- Join only rendered segments, so omitted segments do not leave extra separators.
- When the row is too narrow, omit lower-utility segments before truncating the last remaining segment. Run state and model have the highest utility.

## Usage Data

Extend Anthropic `Usage` with `cache_creation_input_tokens` and `cache_read_input_tokens`. `TokenUsage::context_tokens()` returns input + cache creation + cache read. `TokenUsage::total_tokens()` returns context + output and remains the auto-compaction trigger input.

A successful `agent_turn` returns the latest provider usage. `AgentLoopTask` keeps that usage for the next auto-compaction check, updates the displayed usage snapshot, adds the turn's estimated cost when rates are known, then emits `AgentEvent::UsageUpdated(UsageSnapshot)` before `TurnComplete`.

Known first-party Claude API rates live in `model.rs` beside the model catalogue. Unknown models render context without session cost. The estimate excludes account discounts, marketplace billing, data-residency multipliers, fast mode, and server-side tool surcharges.

On model swap, recompute the snapshot with the new model's context window if display usage exists. The session cost remains the accumulated estimate from turns that actually reported usage. On `/clear`, `/resume`, manual compaction, and automatic compaction, clear displayed usage because the visible transcript basis changed.

## Default Order

The default order is:

```toml
status_line = [
  "current-dir",
  "git-branch",
  "model-with-effort",
  "context-used",
  "session-cost",
  "run-state",
  "thread-title",
]
```

This differs from the user's Claude Code script in two ways:

- oxide-code stays one row because Ratatui already owns the app chrome.
- External billing data is omitted until there is a first-class provider boundary.

Order rationale:

- Location and branch lead because they orient the user.
- Model and usage follow because they describe request cost and context pressure.
- Run state stays near the end because it changes most often.

## Design Decisions

1. **Roster, not DSL.** A typed segment list is enough for first-party data and keeps rendering inside Ratatui.
2. **Implemented segments only.** Unsupported names fail config parsing instead of silently reserving future vocabulary.
3. **Local usage first.** Context and session cost use provider usage already observed by the current process.
4. **Segment omission over placeholders.** Missing usage, unknown pricing, absent branch, and blank titles disappear cleanly.

## Deferred

- ccusage or another account-usage provider boundary.
- Five-hour and weekly account-limit telemetry.
- Pull request and task-progress segments.
- Persisted cost restore after resume.
- Command-based custom status-line renderer.

## Sources

- [Status line research](../../research/tui/status-line.md)
- [Auto-compaction design](../agent/auto-compaction.md)
- `crates/oxide-code/src/tui/components/status.rs`
- `crates/oxide-code/src/tui/components/status/line.rs`
- `crates/oxide-code/src/main.rs`
