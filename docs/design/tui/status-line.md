# Status Line

Design for a configurable TUI status line.

## Scope

The TUI status line renders a user-ordered roster of built-in segments. Most segments read in-process state, `git-branch` and the pull request segments probe `git` and the GitHub CLI, the context and session-cost segments count prompt-cache tokens, and the usage-limit segments read response headers.

Implemented segments:

| Segment                | Config value          |
| ---------------------- | --------------------- |
| current directory      | `current-dir`         |
| git branch             | `git-branch`          |
| pull request           | `pull-request`        |
| pull request checks    | `pull-request-checks` |
| pull request review    | `pull-request-review` |
| model                  | `model`               |
| model with effort      | `model-with-effort`   |
| context used           | `context-used`        |
| estimated session cost | `session-cost`        |
| five-hour usage limit  | `five-hour-limit`     |
| weekly usage limit     | `weekly-limit`        |
| permission mode        | `permission-mode`     |
| run state              | `run-state`           |
| queued prompt count    | `queued-prompts`      |
| task progress          | `task-progress`       |
| thread title           | `thread-title`        |
| session ID prefix      | `session-id`          |
| active theme           | `theme`               |
| current time           | `current-time`        |

Segments outside the [default order](#default-order) are opt-in.

## Implementation

`StatusLineSegment` in `config.rs` defines the roster above. `[tui] status_line = [...]` controls the order. `OX_STATUS_LINE` accepts the same comma-separated names. Segment colors and the separator glyph come from the active theme.

`StatusBar` keeps component state and delegates segment formatting to `tui/components/status/line.rs`. `App` copies the queued prompt count, the task list's progress, the session ID, the theme name, and the permission mode into it before each frame, since it owns all five.

Segment render rules:

- Return `None` when data is unavailable, so an absent branch, title, usage, or price renders no placeholder.
- Use active theme styles, leaving color customization to theme overrides.
- Join only rendered segments, so omitted segments do not leave extra separators.
- When the row is too narrow, omit lower-utility segments before truncating the last remaining segment. Run state, the permission mode, and the model have the highest utility, in that order.
- The `model` and `model-with-effort` segments drop the `Claude` family prefix to save width (e.g., `Opus 5.5 (xhigh)`). Other surfaces (welcome screen, prompt environment, error blocks) keep the full `Claude Opus 5.5` label.
- The pull request checks and review segments color their state with the theme's `success`, `warning`, `error`, and `muted` slots, and link the PR's checks and changed-files pages.
- The review segment appears only when branch protection requires a review, because GitHub otherwise reports no review decision.
- The context and usage-limit segments use the `text` slot, switch to `warning` at 70%, and switch to `error` at 90%. Session cost stays `muted` because it has no limit to approach.
- `permission-mode` shows `Mode: plan` in the `accent` slot and `Mode: yolo` in the `error` slot, and nothing in `manual`, the default.
- `task-progress` counts the completed tasks on the session's [task list](../tools/tasks.md) and shows only while the list holds a task.

## Usage Data

`TokenUsage` reads Anthropic's `usage` payload directly, including `cache_creation_input_tokens` and `cache_read_input_tokens`. `TokenUsage::context_tokens()` returns input + cache creation + cache read. `TokenUsage::total_tokens()` returns context + output and is the auto-compaction trigger input.

Each `agent_turn` reports the latest provider usage and the total across its requests, including a request the user cancelled or that failed partway. `Engine` keeps the latest usage for the next auto-compaction check, updates the displayed usage snapshot, adds the turn's estimated cost when rates are known, then emits `Event::UsageUpdated(UsageSnapshot)` before the turn's terminal event. Summarization requests for `/compact` and auto-compaction add their estimated cost the same way. Each request also records a `usage` entry with its model ID and prompt-cache TTL in the session file, so a resumed session can price each request at the rates it ran with. The background title request runs alongside the first turn, appends its own entry once the API answers, and hands the record to `Engine`, which adds its Haiku-priced cost the next time it emits `UsageUpdated`.

Known first-party Claude API rates are defined in `model/pricing.rs`, and the `model.rs` catalog attaches them to each model. Unknown models render context without session cost. The estimate excludes account discounts, marketplace billing, data-residency multipliers, fast mode, and server-side tool surcharges.

On a model swap, recompute the snapshot with the new model's context window. The session cost remains the estimate accumulated from requests that reported usage. Both manual and automatic compaction clear context usage because the visible transcript basis changed, while preserving the session cost, which now includes the summary. Each compaction emits `UsageUpdated` once its summary requests end, displaying the summary's cost before the next turn and keeping the context segment hidden until the next request reports usage. `/clear` resets both, and any title cost the engine has not yet added stays with the session it titled, recorded in that session's file. `/resume` and `ox -c` clear the context usage and restore the session cost from every `usage` entry in the resumed file, whichever branch or compaction it belongs to. A file without `usage` entries resumes at zero.

## Usage Limits

A Claude subscription sign-in's responses carry `anthropic-ratelimit-unified-5h-utilization` and `-5h-reset`, and the same pair for `7d`. Utilization is the spent share of the window, from 0 to 1, and reset is the window's end in Unix seconds. API keys receive neither pair, so the segments stay hidden for them.

The SSE pump reads the headers of each attempt's response and forwards them as `ResponseEvent::RateLimits`, ahead of the stream's events. The agent loop emits `Event::RateLimitsUpdated`, and the status bar keeps the latest. The summarization requests behind a compaction report their limits the same way.

A window renders its utilization floored to a whole percent, after rounding to basis points so a share like `0.29` keeps its exact percent. The status bar's tick drops a window once its reset time passes, since the spent share no longer applies until the next response reports the new window. A sign-in or sign-out drops both windows, which belonged to the previous credential.

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
  "permission-mode",
  "run-state",
  "thread-title",
]
```

Claude Code's status line is a command you configure, and each line it prints becomes a row. oxide-code renders its built-in segments in one row because Ratatui owns the app chrome.

The default order tracks reading flow: location and branch orient the user, model and usage describe cost and context pressure, the permission mode says what the next call may do, and run state sits near the end because it changes most often.

## Design Decisions

1. **Typed segment roster.** A typed segment list is enough for first-party data and keeps rendering inside Ratatui, so the config needs no format DSL.
2. **Implemented segments only.** Unsupported names fail config parsing, so the accepted vocabulary matches the shipped segments.
3. **Local usage first.** Context and session cost use provider usage that oxide-code observed, either in the current process or recorded in the session file.
4. **Segment omission.** Missing usage, unknown pricing, absent branch, and blank titles disappear cleanly with no placeholder.
5. **Background probes.** `git-branch` and the pull request segments re-probe as background child processes, 5 s and 60 s respectively after the previous probe finishes, with at most one run of each in flight. One `gh pr view` call serves all three PR segments. It reads the checks and review state only when the roster shows them, since those fields add every check on the head commit to the query. The bar stays current after `git checkout` or `gh pr create`, and a slow `gh` network round-trip never stalls input or rendering.
6. **Checks like `gh pr checks`.** The checks segment buckets each check's state the way the GitHub CLI does and counts only the latest run of a re-run check, so a passing re-run clears an earlier failure. Cancelled checks get their own bucket, below failing and pending. A workflow that failed to start counts as failing, where `gh` leaves it pending even though it will never run.
7. **Usage limits from response headers.** Every response already reports the account's utilization, so the segments add no extra request and refresh with each turn.
8. **Show the mode only when it departs from the default.** `manual` is what a user expects, so the segment stays hidden there and appears for the two modes that change what runs. Its utility ranks just below run state and above the model, since a narrow row should still warn that `yolo` is on. `yolo` takes the `error` slot, as Claude Code colors its bypass mode, and `plan` takes `accent`, since it only restricts.

9. **Escalate usage by the percent shown.** The context and limit segments use the same pair of thresholds and compare the floored percent in their labels, keeping each label's color aligned with its number. The 70% warning matches Claude Code's `WARNING_THRESHOLD` in `src/services/rateLimitMessages.ts`. Below that threshold, Claude Code shows no usage-limit warning. Default auto-compaction begins near 97% of the window, which puts context at 90% about as close to its end as a limit at 90%. oh-my-pi changes its context color at 50%, 70%, and 90%, as well as at 150k, 270k, and 500k tokens. On a 1M window, those token thresholds would flag most of a long session. Its quota windows change color at 50% and 80%.

## Related Issues

A user-command segment is [#166](https://github.com/hakula139/oxide-code/issues/166), and pricing for other providers comes with the data-driven model catalog in [#152](https://github.com/hakula139/oxide-code/issues/152).

## Sources

- [Status line research](../../research/tui/status-line.md)
- [Auto-compaction design](../agent/auto-compaction.md)
- `crates/oxide-code/src/client.rs`: `ResponseEvent::RateLimits` and the `RateLimits` windows.
- `crates/oxide-code/src/client/anthropic/sse.rs`: `pump_events`, which forwards each response's limits.
- `crates/oxide-code/src/client/anthropic/transport.rs`: `rate_limits`, the unified rate-limit header parser.
- `crates/oxide-code/src/config.rs`: `StatusLineSegment`, the segment roster and its default order.
- `crates/oxide-code/src/engine.rs`: `Engine` usage accounting, `UsageUpdated` emission, and restored-cost pricing.
- `crates/oxide-code/src/engine/turn.rs`: `collect_response`, which emits `RateLimitsUpdated`.
- `crates/oxide-code/src/model/pricing.rs`: `TokenCostRates`, the per-model rate cards and the request estimate.
- `crates/oxide-code/src/model/usage.rs`: `TokenUsage`, the token counts one request reports.
- `crates/oxide-code/src/session/entry.rs`: `Entry::Usage` and `UsageRecord`, the per-request record that a resume prices.
- `crates/oxide-code/src/session/title_generator.rs`: `spawn`, which records the title request's usage and hands it to the engine.
- `crates/oxide-code/src/tui/app/render.rs`: `App::draw_frame`, which copies App-owned state, the permission mode included, into the status bar.
- `crates/oxide-code/src/tui/components/status.rs`: `StatusBar` state and the run-state `Status`.
- `crates/oxide-code/src/tui/components/status/line.rs`: `StatusLine` segment rendering and width fitting.
- `crates/oxide-code/src/tui/git_probes.rs`: `GitProbes` background branch and pull-request polling, at the detail the roster needs.
- `crates/oxide-code/src/util/git.rs`: `current_branch` and `current_pull_request` probes, and the checks summary.
