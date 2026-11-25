# Status Line

The status line is the TUI's top row, a user-ordered roster of built-in segments reading in-process state, background `git` / `gh` probes, prompt-cache token counts, and response rate-limit headers. The [`[tui]` configuration guide](../guide/configuration.md#tui-terminal-ui) lists each segment's output and refresh interval, and segments outside the [default order](#default-order) are opt-in.

## Segments

`StatusLineSegment` in `config.rs` defines the roster, configured via `[tui] status_line = [...]` or `OX_STATUS_LINE` (comma-separated). Theme slots supply segment colors and the separator glyph.

`StatusBar` holds only local UI state (spinner, clock minute, git probe results, last drawn link rects) and delegates formatting to `ox_tui::components::status::line`. Each frame, `App` builds a `StatusView` borrowing `LiveSessionInfo` (model, title, usage, rate limits) plus run state, queued prompt count, and task progress, keeping one owner per value.

Render rules:

- Unavailable data returns `None`, omitting the segment and its separator with no placeholder.
- Model segments strip the `Claude` family prefix to save width, while other surfaces keep the full label.
- `pull-request-review` appears only when branch protection requires a review (otherwise GitHub reports no decision).
- Usage segments escalate color by displayed percentage (**Escalate usage by the percent shown**), and permission modes carry per-mode colors (**Show every configured permission mode**).

## Usage Data

`TokenUsage` maps Anthropic's `usage` payload directly, with `TokenUsage::context_tokens()` returning input + cache creation + cache read. Auto-compaction adds an estimate of messages appended since the latest request.

Each `agent_turn` reports latest and total usage across all requests (including cancelled or failed ones). `Engine` saves the next-request estimate for auto-compaction, updates the usage snapshot, adds estimated cost when rates are known, and emits `Event::UsageUpdated(UsageSnapshot)` before the turn ends. Compaction summaries add cost the same way, and every request writes a `usage` row with model ID and prompt-cache TTL for accurate pricing on resume. The background title task records its own row and passes it to `Engine`, which folds its Haiku cost into the next `UsageUpdated`.

First-party Claude rates live in `model/pricing.rs`, attached via `model.rs`. Unknown models show context without cost. Estimates exclude account discounts, marketplace billing, data-residency multipliers, fast mode, and server-tool surcharges.

Basis resets ([Session Cost](persistence.md#session-cost) covers persistence):

- **Model swap.** Recomputes the snapshot against the new model's context window.
- **Compaction.** Clears context usage while keeping session cost, emitting `UsageUpdated` after summary requests finish and hiding context until the next request reports usage.
- **`/clear`.** Resets both, leaving any pending title cost with the old session.
- **Resume.** `/resume` and `ox -c` clear context usage and restore session cost.

## Usage Limits

Claude subscription responses include `anthropic-ratelimit-unified-5h-utilization` / `-5h-reset` and the `7d` pair (utilization from 0 to 1 and Unix-second reset). API keys receive neither, so both segments stay hidden.

The SSE pump emits `ResponseEvent::RateLimits` from response headers ahead of stream events (including for compaction summaries), and the engine emits `Event::RateLimitsUpdated` into `LiveSessionInfo`. Utilization renders floored to a whole percent after rounding to basis points (preserving exact values like `0.29`). The App tick drops expired windows after their reset time, and sign-in or sign-out drops both windows.

## Default Order

`StatusLineSegment::DEFAULT` owns the default order shown in the [configuration example](../guide/configuration.md#config-file). Unlike Claude Code's external-command status line, `ox` renders built-in segments in one Ratatui row following reading flow: location and branch orient the user, model and usage track cost and context pressure, permission mode states allowed actions, and run state sits near the end because it changes most often.

## Narrow Rows

The bar keeps the highest-priority segment (truncating if needed), greedily fits remaining segments from highest to lowest utility, and renders survivors in configured order so smaller segments can fill space left by omitted larger ones. Utility order (lowest → highest): `theme` → `session-id` → `thread-title` → `current-time` → `pull-request-review` → `pull-request-checks` → `pull-request` → `git-branch` → `current-dir` → `task-progress` → `queued-prompts` → `session-cost` → `weekly-limit` → `five-hour-limit` → `context-used` → `permission-mode` (`auto` / `manual`) → `model` → `model-with-effort` → `permission-mode` (`plan` / `yolo`) → `run-state`.

## Design Decisions

1. **Typed segment roster.** First-party segments keep rendering in Ratatui without a format DSL, leaving colors to theme overrides.
2. **Implemented segments only.** Unknown names fail config parsing so the vocabulary matches shipped segments.
3. **Local usage first.** Context and cost reflect provider usage observed in-process or stored in the session database.
4. **Segment omission.** Unavailable values vanish cleanly with no placeholder.
5. **Background probes.** `git-branch` and PR probes run as background child processes 5 s and 60 s after the prior probe ends (at most one of each in flight, active only when configured). One `gh pr view` serves all three PR segments, requesting checks and review state only when configured to avoid fetching all head-commit checks unnecessarily, keeping `git checkout` and `gh pr create` reflected without stalling input or rendering.
6. **A branch change clears the pull request.** Clearing and re-probing immediately prevents pairing a new branch with the old branch's link, checks, or review state.
7. **Probe deadline.** Killing probes after 30 s prevents a hung `git` or `gh` from freezing a segment for the session.
8. **Checks like `gh pr checks`.** Buckets match the GitHub CLI and count only the latest run of re-run checks so a passing re-run clears an earlier failure. Cancelled checks get their own bucket below failing and pending, and workflows that failed to start count as failing even though `gh` leaves them pending.
9. **Usage limits from response headers.** Reading response headers avoids extra requests and refreshes every turn.
10. **Show every configured permission mode.** Confirms switches even back to `auto` or `manual`. On narrow rows, `auto` / `manual` drop before the model while `plan` / `yolo` sit just below run state so active restrictions or bypasses stay visible. Tones: `auto` / `manual` use `muted`, `yolo` uses `error` (matching Claude Code's bypass color), and `plan` uses `accent`.
11. **Escalate usage by the percent shown.** Context and limit segments share thresholds against their floored displayed percent so color matches the number, while session cost has no limit and keeps one color. The 70% warning matches Claude Code's usage-limit warning threshold, and 90% puts context near default [auto-compaction](compaction.md) (near 97%) just as a limit nears exhaustion. oh-my-pi uses 50%, 70%, and 90% plus fixed 150k, 270k, and 500k token thresholds (which would flag most of a 1M window) and 50% and 80% for quota windows.

## Out of Scope

- **A user-command segment.** Needs its own refresh and failure policy ([#166](https://github.com/hakulab/oxide-code/issues/166)).
- **ccusage integration.** Re-reads stored sessions when the bar already tracks response usage directly.
- **Pricing for other providers.** Awaits the data-driven model catalog ([#152](https://github.com/hakulab/oxide-code/issues/152)).

## Sources

- `crates/ox-core/src/config.rs`: `StatusLineSegment`, the segment roster and its default order.
- `crates/ox-core/src/engine/usage.rs`: `Engine` usage accounting, `UsageUpdated` emission, and restored-cost pricing.
- `crates/ox-core/src/model/pricing.rs`: `TokenCostRates`, the per-model rate cards and the request estimate.
- `crates/ox-core/src/model/usage.rs`: `TokenUsage`, the token counts one request reports.
- `crates/ox-tui/src/components/status.rs`: `StatusBar`, `StatusView`, and `RunLabel`, the run-state segment's text and tone.
- `crates/ox-tui/src/components/status/line.rs`: `StatusLine` segment rendering and width fitting.
- `crates/ox-tui/src/git_probes.rs`: `GitProbes` background branch and pull-request polling, at the detail the roster needs.
- `crates/ox-tui/src/git_probes/pull_request.rs`: the `current_pull_request` probe and the checks summary.
