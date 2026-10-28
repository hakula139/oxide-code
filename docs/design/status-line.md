# Status Line

The status line is the TUI's top row, a user-ordered roster of built-in segments. Most segments read in-process state, `git-branch` and the pull request segments probe `git` and the GitHub CLI, the context and session-cost segments count prompt-cache tokens, and the usage-limit segments read response headers. The [`[tui]` section of the configuration guide](../guide/configuration.md#tui-terminal-ui) lists each segment, what it renders, and how often it refreshes, and segments outside the [default order](#default-order) are opt-in.

## Segments

`StatusLineSegment` in `config.rs` defines the roster. `[tui] status_line = [...]` controls the order. `OX_STATUS_LINE` accepts the same comma-separated names. Segment colors and the separator glyph come from the active theme.

`StatusBar` keeps only its own state (the spinner, the clock minute, the git probe results, and the links it last drew) and delegates segment formatting to `ox_tui::components::status::line`. Each frame, `App` hands it a `StatusView` that borrows `LiveSessionInfo` for the session's values, such as the model, title, usage, and usage limits, and adds the run state, the queued prompt count, and the task list's progress, so every value the bar shows has one owner.

Segment render rules:

- Return `None` when data is unavailable, so an absent branch, title, usage, or price renders no placeholder.
- Use active theme styles, leaving color customization to theme overrides.
- Join only rendered segments, so omitted segments do not leave extra separators.
- The model segments drop the `Claude` family prefix to save width, while other surfaces keep the full label.
- The review segment appears only when branch protection requires a review, because GitHub otherwise reports no review decision.
- Segments that approach a limit escalate their color as **Escalate usage by the percent shown** describes, and the permission mode takes the colors **Show every configured permission mode** gives.

## Usage Data

`TokenUsage` reads Anthropic's `usage` payload directly, including `cache_creation_input_tokens` and `cache_read_input_tokens`. `TokenUsage::context_tokens()` returns input + cache creation + cache read. Auto-compaction reads the context tokens of the latest request plus an estimate of the messages appended since it went out.

Each `agent_turn` reports the latest provider usage and the total across its requests, including a request the user cancelled or that failed partway. `Engine` keeps the turn's estimate of the next request for the next auto-compaction check, updates the displayed usage snapshot, adds the turn's estimated cost when rates are known, then emits `Event::UsageUpdated(UsageSnapshot)` before the turn's terminal event. The summarization requests behind `/compact` and auto-compaction add their estimated cost the same way. Each of these requests also lands in the session's `usage` rows as a record carrying its model ID and prompt-cache TTL, so a resumed session can price each request at the rates it ran with. The background title request runs alongside the first turn, and once its response reports usage, it records its own row and hands the record to `Engine`, which adds the Haiku-priced cost the next time it emits `UsageUpdated`.

Known first-party Claude API rates live in `model/pricing.rs`, and the `model.rs` catalog attaches them to each model. Unknown models render context without session cost. The estimate excludes account discounts, marketplace billing, data-residency multipliers, fast mode, and server-side tool surcharges.

On model swap, recompute the snapshot with the new model's context window. The session cost remains the accumulated estimate from requests that reported usage. Manual and automatic compaction clear the context usage because the visible transcript basis changed, and keep the session cost, which [Session Cost](persistence.md#session-cost) accounts for along with `/clear` and resume. Each compaction emits `UsageUpdated` once its summary requests end, so the summary's cost shows before the next request, and the context segment stays hidden until the next request reports usage. `/clear` resets both, and a title cost already recorded but not yet shown stays with the session it titled. `/resume` and `ox -c` clear the context usage and restore the session cost as Session Cost describes.

## Usage Limits

A Claude subscription sign-in's responses carry `anthropic-ratelimit-unified-5h-utilization` and `-5h-reset`, and the same pair for `7d`. Utilization is the spent share of the window, from 0 to 1, and reset is the window's end in Unix seconds. API keys receive neither pair, so the segments stay hidden for them.

The SSE pump reads the headers of each attempt's response and forwards them as `ResponseEvent::RateLimits`, ahead of the stream's events. The agent loop emits `Event::RateLimitsUpdated`, and `LiveSessionInfo` keeps the latest. The summarization requests behind a compaction report their limits the same way.

A window renders its utilization floored to a whole percent, after rounding to basis points so a share like `0.29` keeps its exact percent. The App's tick drops a window once its reset time passes, since the spent share no longer applies until the next response reports the new window. A sign-in or sign-out drops both windows, which belonged to the previous credential.

## Default Order

`StatusLineSegment::DEFAULT` owns the default order, which the [configuration example](../guide/configuration.md#config-file) spells out.

Claude Code's status line is a command you configure, and each line it prints becomes a row. oxide-code renders its built-in segments in one row because Ratatui owns the app chrome.

The default order tracks reading flow: location and branch orient the user, model and usage describe cost and context pressure, the permission mode says what the next call may do, and run state sits near the end because it changes most often.

## Narrow Rows

The status line retains the highest-priority segment, truncating it if needed, then fits the other segments from highest to lowest utility and renders them in configured order. A smaller segment can use space left after a larger one is omitted. From lowest to highest utility, the order is `theme` → `session-id` → `thread-title` → `current-time` → `pull-request-review` → `pull-request-checks` → `pull-request` → `git-branch` → `current-dir` → `task-progress` → `queued-prompts` → `session-cost` → `weekly-limit` → `five-hour-limit` → `context-used` → `permission-mode` in `manual` → `model` → `model-with-effort` → `permission-mode` in `plan` or `yolo` → `run-state`.

## Design Decisions

1. **Typed segment roster.** A typed segment list is enough for first-party data and keeps rendering inside Ratatui, so the config needs no format DSL.
2. **Implemented segments only.** Unsupported names fail config parsing, so the accepted vocabulary matches the shipped segments.
3. **Local usage first.** Context and session cost use provider usage that oxide-code observed, either in the current process or recorded in the session store.
4. **Segment omission.** Missing usage, unknown pricing, absent branch, and blank titles disappear cleanly with no placeholder.
5. **Background probes.** `git-branch` and the pull request segments re-probe as background child processes, 5 s and 60 s respectively after the previous probe finishes, with at most one run of each in flight, and each probe runs only while the roster has a segment that reads it. One `gh pr view` call serves all three PR segments. It reads the checks and review state only when the roster shows them, since those fields add every check on the head commit to the query. The bar stays current after `git checkout` or `gh pr create`, and a slow `gh` network round-trip never stalls input or rendering.
6. **A branch change clears the pull request.** The segments clear and probe again at once, so the bar never pairs the new branch with the previous branch's link, checks, or review state.
7. **Probe deadline.** A probe that outlives 30 s is killed, so a hung `git` or network-bound `gh` cannot freeze its segment for the rest of the session.
8. **Checks like `gh pr checks`.** The checks segment buckets each check's state the way the GitHub CLI does and counts only the latest run of a re-run check, so a passing re-run clears an earlier failure. Cancelled checks get their own bucket, below failing and pending. A workflow that failed to start counts as failing, where `gh` leaves it pending even though it will never run.
9. **Usage limits from response headers.** Every response already reports the account's utilization, so the segments add no extra request and refresh with each turn.
10. **Show every configured permission mode.** The segment confirms the active mode after a switch, including a return to `manual`. In a narrow row, `manual` drops before the model, while `plan` and `yolo` rank just below run state so the active restriction or bypass stays visible. `manual` takes `muted`, `yolo` takes `error`, as Claude Code colors its bypass mode, and `plan` takes `accent`, since it only restricts.
11. **Escalate usage by the percent shown.** The context and limit segments use the same pair of thresholds and compare the floored percent in their labels, keeping each label's color aligned with its number. Session cost has no limit to approach, so it keeps one color. The 70% warning matches Claude Code's `WARNING_THRESHOLD` in `src/services/rateLimitMessages.ts`. Below that threshold, Claude Code shows no usage-limit warning. Default [auto-compaction](compaction.md) begins near 97% of the window, which puts context at 90% about as close to its end as a limit at 90%. oh-my-pi changes its context color at 50%, 70%, and 90%, as well as at 150k, 270k, and 500k tokens. On a 1M window, those token thresholds would flag most of a long session. Its quota windows change color at 50% and 80%.

## Out of Scope

- **A user-command segment.** It would run a shell command on each refresh, which needs its own failure and refresh policy ([#166](https://github.com/hakula139/oxide-code/issues/166)).
- **ccusage integration.** It would re-read stored sessions to total cost, while the status line already shows the response's own usage.
- **Pricing for other providers.** It waits for the data-driven model catalog ([#152](https://github.com/hakula139/oxide-code/issues/152)).

## Sources

- `crates/ox-core/src/client.rs`: `ResponseEvent::RateLimits` and the `RateLimits` windows.
- `crates/ox-core/src/client/anthropic/sse.rs`: `pump_events`, which forwards each response's limits.
- `crates/ox-core/src/client/anthropic/transport.rs`: `rate_limits`, the unified rate-limit header parser.
- `crates/ox-core/src/config.rs`: `StatusLineSegment`, the segment roster and its default order.
- `crates/ox-core/src/engine/turn.rs`: `collect_response`, which emits `RateLimitsUpdated`.
- `crates/ox-core/src/engine/usage.rs`: `Engine` usage accounting, `UsageUpdated` emission, and restored-cost pricing.
- `crates/ox-core/src/git.rs`: the `current_branch` probe.
- `crates/ox-core/src/model/pricing.rs`: `TokenCostRates`, the per-model rate cards and the request estimate.
- `crates/ox-core/src/model/usage.rs`: `TokenUsage`, the token counts one request reports.
- `crates/ox-core/src/session/store.rs`: `UsageRecord`, the per-request record a resume prices.
- `crates/ox-core/src/session/title_generator.rs`: `spawn`, which records the title request's usage and hands it to the engine.
- `crates/ox-tui/src/app/render.rs`: `App::draw_frame`, which builds the `StatusView` the status bar renders.
- `crates/ox-tui/src/components/status.rs`: `StatusBar`, `StatusView`, and `RunLabel`, the run-state segment's text and tone.
- `crates/ox-tui/src/components/status/line.rs`: `StatusLine` segment rendering and width fitting.
- `crates/ox-tui/src/git_probes.rs`: `GitProbes` background branch and pull-request polling, at the detail the roster needs.
- `crates/ox-tui/src/git_probes/pull_request.rs`: the `current_pull_request` probe and the checks summary.
- `crates/ox-tui/src/session_info.rs`: `LiveSessionInfo`, which holds the title, usage, and usage limits the bar shows and drops a usage-limit window once it resets.
