# Status Line (Reference)

Research on status-line composition in terminal coding assistants. Based on [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript)

Claude Code delegates the whole status line to a command you configure. The command receives session data as JSON on stdin and prints the row, and Claude Code reruns it on session events, debounced at 300 ms, plus an optional `refreshInterval` timer ([status line docs](https://code.claude.com/docs/en/statusline), read on 2026-10-18).

The useful data split is provider-local versus external:

- **Provider-local**: context and session cost come from Claude Code's JSON input. `context_window.current_usage` splits the last request into input, cache creation, and cache read tokens, and `cost.total_cost_usd` estimates the session cost at list price.
- **External billing**: block and daily totals need an external tool such as `ccusage`, which the command runs and caches itself.
- **Rendering boundary**: the command owns formatting, while Claude Code owns structured data export.

## OpenAI Codex (Rust)

Codex exposes a roster-style `tui.status_line` array of built-in item names. At Codex commit ce0323f2f (2026-10-17), the default roster is `model-with-reasoning`, `current-dir`, and `thread-name`. Other items cover the git branch, PR number, run state, context used, five-hour and weekly limits, thread title, and task progress (`StatusLineItem` in `codex-rs/tui/src/bottom_pane/status_line_setup.rs`).

The important pattern is not the exact segment list. The useful part for oxide-code is the ordered roster, where users can place trustworthy built-in segments without adopting a command DSL.

## opencode (TypeScript)

opencode keeps usage in a fixed prompt footer with no configurable status line. It derives context from the latest assistant message usage, includes cache read and cache write tokens, and sums assistant-message cost across the session. Context percentage disappears when the provider model has no advertised context limit.

## Pricing Reference

Anthropic's [pricing page](https://platform.claude.com/docs/en/about-claude/pricing) lists first-party Claude API prices in USD per million tokens, with separate rates for 5-minute cache writes, 1-hour cache writes, cache reads, and output tokens. As of 2026-10-17, the rows for oxide-code's model table are:

| Model                     | Input | 5m cache write | 1h cache write | Cache read | Output |
| ------------------------- | ----- | -------------- | -------------- | ---------- | ------ |
| Fable 5.1                 | $10   | $12.50         | $20            | $0.25      | $50    |
| Opus 5.5                  | $4    | $5             | $8             | $0.20      | $20    |
| Sonnet 5.5                | $2    | $2.50          | $4             | $0.10      | $10    |
| Haiku 5.5 (≤ 100K prompt) | $0.10 | $0.125         | $0.20          | $0.01      | $0.50  |
| Haiku 5.5 (> 100K prompt) | $0.50 | $0.625         | $1             | $0.05      | $2.50  |

Haiku 5.5 picks its tier per request from the prompt size (input + cache creation + cache read), so session cost has to be summed request by request.

Cost display should stay best-effort because account discounts, marketplace billing, data residency, fast mode, and server-side tool pricing can change the final bill.

## Patterns Worth Borrowing for oxide-code

1. **Typed ordered roster.** A list of built-in segment names is simpler and safer than a command DSL for the first version.
2. **Local usage first.** Context use and in-process session cost should come from provider usage already observed by oxide-code.
3. **Cache-aware accounting.** Context and cost should include cache creation and cache read tokens because prompt caching is part of the real request shape.
4. **Graceful omission.** Segments with no data should disappear entirely.
5. **Theme-owned colors.** Status-line colors should come from the active theme.
6. **Header-borne account limits.** Claude Code reads a subscription's five-hour and weekly utilization from the `anthropic-ratelimit-unified-{5h,7d}-{utilization,reset}` headers on every response. That keeps the limits current without the extra `/api/oauth/usage` request its `/usage` screen makes.

## Patterns Not Adopted

1. **ccusage integration.** Block and daily billing are useful, but shelling out to an external npm package adds installation, trust, cache, and schema concerns.
2. **Command-based status lines.** Claude Code's command hook moves rendering, errors, and latency outside the Rust UI loop. Tracked in [#166](https://github.com/hakula139/oxide-code/issues/166).
