# Status Line (Reference)

Research on status-line composition in terminal coding assistants. Sources are Claude Code, OpenAI Codex, opencode, and local reference Claude / Codex configurations.

## Claude Code

Claude Code delegates the whole status line to a command. The reference script renders:

- Row 1: tty, current directory, git branch, and git dirtiness.
- Row 2: model, context use, session cost, ccusage block / daily totals, and current time.

The useful data split is provider-local versus external:

- **Provider-local**: context and session cost come from Claude Code's JSON input. Current usage includes input, cache creation, and cache read tokens.
- **External billing**: block and daily totals come from `ccusage blocks --json`, cached for 30 seconds by the script.
- **Rendering boundary**: the command owns formatting, while Claude Code owns structured data export.

## OpenAI Codex

Codex exposes a roster-style `tui.status_line` array. The reference config orders:

- current directory
- git branch
- model with reasoning
- context used
- five-hour and weekly limits
- PR number
- run state
- thread title
- task progress

The important pattern is not the exact segment list. The useful part for oxide-code is the ordered roster, where users can place trustworthy built-in segments without adopting a command DSL.

## opencode

opencode keeps usage in a fixed prompt footer with no configurable status line. It derives context from the latest assistant message usage, includes cache read and cache write tokens, and sums assistant-message cost across the session. Context percentage disappears when the provider model has no advertised context limit.

## Pricing Reference

Anthropic's pricing page lists first-party Claude API prices in USD per million tokens and separate prompt-cache rates for 5-minute writes, 1-hour writes, cache reads, and output tokens. The rows for oxide-code's model table are:

| Model                     | Input | 5m cache write | 1h cache write | Cache read | Output |
| ------------------------- | ----- | -------------- | -------------- | ---------- | ------ |
| Opus 5.5                  | $4    | $5             | $8             | $0.20      | $20    |
| Sonnet 5.5                | $2    | $2.50          | $4             | $0.20      | $10    |
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

## Additional Patterns

1. **ccusage integration.** Block and daily billing are useful, but shelling out to an external npm package adds installation, trust, cache, and schema concerns.
2. **Command-based status lines.** Claude Code's command hook moves rendering, errors, and latency outside the Rust UI loop. Tracked in [#166](https://github.com/hakula139/oxide-code/issues/166).
