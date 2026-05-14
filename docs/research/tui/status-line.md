# Status Line (Reference)

Research on status-line composition in terminal coding assistants. Sources are Claude Code, OpenAI Codex, opencode, and the user's managed Claude / Codex setup.

## Claude Code

Claude Code delegates the whole status line to a command. The user's configured script renders:

- Row 1: tty, current directory, git branch, and git dirtiness.
- Row 2: model, context use, session cost, ccusage block / daily totals, and current time.

The useful data split is provider-local versus external:

- **Provider-local**: context and session cost come from Claude Code's JSON input. Current usage includes input, cache creation, and cache read tokens.
- **External billing**: block and daily totals come from `ccusage blocks --json`, cached for 30 seconds by the script.
- **Rendering boundary**: the command owns formatting, while Claude Code owns structured data export.

## OpenAI Codex

Codex exposes a roster-style `tui.status_line` array. The user's setup orders:

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

opencode keeps usage in the prompt footer rather than a fully configurable status line. It derives context from the latest assistant message usage, includes cache read and cache write tokens, and sums assistant-message cost across the session. Context percentage disappears when the provider model has no advertised context limit.

## Pricing Reference

Anthropic's pricing page lists first-party Claude API prices in USD per million tokens and separate prompt-cache rates for 5-minute writes, 1-hour writes, cache reads, and output tokens. Checked on 2026-05-14, the relevant rows for oxide-code's model table are:

| Family               | Input | 5m cache write | 1h cache write | Cache read | Output |
| -------------------- | ----- | -------------- | -------------- | ---------- | ------ |
| Opus 4.7 / 4.6 / 4.5 | $5    | $6.25          | $10            | $0.50      | $25    |
| Sonnet 4.x           | $3    | $3.75          | $6             | $0.30      | $15    |
| Haiku 4.5            | $1    | $1.25          | $2             | $0.10      | $5     |

Cost display should stay best-effort because account discounts, marketplace billing, data residency, fast mode, and server-side tool pricing can change the final bill.

## Patterns Worth Borrowing for oxide-code

1. **Typed ordered roster.** A list of built-in segment names is simpler and safer than a command DSL for the first version.
2. **Local usage first.** Context use and in-process session cost should come from provider usage already observed by oxide-code.
3. **Cache-aware accounting.** Context and cost should include cache creation and cache read tokens because prompt caching is part of the real request shape.
4. **Graceful omission.** Segments with no data should disappear instead of rendering placeholders.
5. **Theme-owned colors.** Status-line colors should come from the active theme rather than a separate status-line flag.

## Patterns to Defer

1. **ccusage integration.** Block and daily billing are useful, but shelling out to an external npm package adds installation, trust, cache, and schema concerns. A future version should either define a first-class external provider boundary or persist enough local usage to avoid it.
2. **Account-limit telemetry.** Five-hour and weekly limits need provider-specific state that is separate from per-turn token usage.
3. **PR and task-progress segments.** Those need reliable project metadata and task state that oxide-code does not own yet.
4. **Command-based status lines.** Claude Code's command hook is powerful but moves rendering, errors, and latency outside the Rust UI loop.
