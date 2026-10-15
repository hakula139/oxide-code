# Status Line

Design for a configurable TUI status line.

## Scope

The TUI status line renders a user-ordered roster of built-in segments. Most segments read in-process state, `git-branch` and `pull-request` probe `git` and the GitHub CLI, and the context and session-cost segments count prompt-cache tokens.

Implemented segments:

| Segment                | Config value        |
| ---------------------- | ------------------- |
| current directory      | `current-dir`       |
| git branch             | `git-branch`        |
| pull request           | `pull-request`      |
| model                  | `model`             |
| model with effort      | `model-with-effort` |
| context used           | `context-used`      |
| estimated session cost | `session-cost`      |
| run state              | `run-state`         |
| thread title           | `thread-title`      |
| current time           | `current-time`      |

## Implementation

`StatusLineSegment` in `config.rs` defines the roster above. `[tui] status_line = [...]` controls the order. `OX_STATUS_LINE` accepts the same comma-separated names. Segment colors and the separator glyph come from the active theme.

`StatusBar` keeps component state and delegates segment formatting to `tui/components/status/line.rs`.

Segment render rules:

- Return `None` when data is unavailable, so an absent branch, title, usage, or price renders no placeholder.
- Use active theme styles, leaving color customization to theme overrides.
- Join only rendered segments, so omitted segments do not leave extra separators.
- When the row is too narrow, omit lower-utility segments before truncating the last remaining segment. Run state and model have the highest utility.
- The `model` and `model-with-effort` segments drop the `Claude` family prefix to save width (e.g., `Opus 5.5 (xhigh)`). Other surfaces (welcome screen, prompt environment, error blocks) keep the full `Claude Opus 5.5` label.

## Usage Data

Anthropic `Usage` carries `cache_creation_input_tokens` and `cache_read_input_tokens`. `TokenUsage::context_tokens()` returns input + cache creation + cache read. `TokenUsage::total_tokens()` returns context + output and is the auto-compaction trigger input.

Each `agent_turn` reports the latest provider usage and the total across its requests, including a request the user cancelled or that failed partway. `Engine` keeps the latest usage for the next auto-compaction check, updates the displayed usage snapshot, adds the turn's estimated cost when rates are known, then emits `Event::UsageUpdated(UsageSnapshot)` before the turn's terminal event. The summarization requests behind `/compact` and auto-compaction add their estimated cost the same way.

Known first-party Claude API rates live in `model/pricing.rs`, and the `model.rs` catalogue attaches them to each model. Unknown models render context without session cost. The estimate excludes account discounts, marketplace billing, data-residency multipliers, fast mode, and server-side tool surcharges.

On model swap, recompute the snapshot with the new model's context window. The session cost remains the accumulated estimate from requests that reported usage. Manual and automatic compaction clear the context usage because the visible transcript basis changed, and keep the session cost, which now includes the summary. Each compaction emits `UsageUpdated` once its summary requests end, so the summary's cost shows before the next turn, and the context segment stays hidden until the next request reports usage. `/clear` and `/resume` reset both.

## Default Order

The default order is:

```toml
status_line = [
  "current-dir",
  "git-branch",
  "pull-request",
  "model-with-effort",
  "context-used",
  "session-cost",
  "run-state",
  "thread-title",
]
```

Unlike Claude Code's reference script, oxide-code stays one row because Ratatui owns the app chrome, and its cost segment is a local estimate.

The default order tracks reading flow: location and branch orient the user, model and usage describe cost and context pressure, run state sits near the end because it changes most often.

## Design Decisions

1. **Typed segment roster.** A typed segment list is enough for first-party data and keeps rendering inside Ratatui, so the config needs no format DSL.
2. **Implemented segments only.** Unsupported names fail config parsing, so the accepted vocabulary matches the shipped segments.
3. **Local usage first.** Context and session cost use provider usage already observed by the current process.
4. **Segment omission.** Missing usage, unknown pricing, absent branch, and blank titles disappear cleanly with no placeholder.
5. **Background probes.** `git-branch` and `pull-request` re-probe as background child processes, 5 s and 60 s respectively after the previous probe finishes, with at most one run of each in flight. The bar stays current after `git checkout` or `gh pr create`, and a slow `gh` network round-trip never stalls input or rendering.

## Related Issues

Planned segments, including account usage limits and task progress, are tracked in [#140](https://github.com/hakula139/oxide-code/issues/140). Restoring the session cost on resume is [#185](https://github.com/hakula139/oxide-code/issues/185), a user-command segment is [#166](https://github.com/hakula139/oxide-code/issues/166), and pricing for other providers comes with the data-driven model catalog in [#152](https://github.com/hakula139/oxide-code/issues/152).

## Sources

- [Status line research](../../research/tui/status-line.md)
- [Auto-compaction design](../agent/auto-compaction.md)
- `crates/oxide-code/src/engine.rs`: `Engine` usage accounting and `UsageUpdated` emission.
- `crates/oxide-code/src/tui/components/status.rs`: `StatusBar` state and the run-state `Status`.
- `crates/oxide-code/src/tui/components/status/line.rs`: `StatusLine` segment rendering and width fitting.
- `crates/oxide-code/src/tui/git_probes.rs`: `GitProbes` background branch and pull-request polling.
- `crates/oxide-code/src/util/git.rs`: `current_branch_async` and `current_pull_request` probes.
