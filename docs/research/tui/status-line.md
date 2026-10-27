# Status Line (Reference)

Research on status-line composition in terminal coding assistants. Based on [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

Verified 2026-10-26 against Claude Code 2.1.87 (the frozen source snapshot), Codex `2351d9e1b608`, and opencode `v1.4.11-1713-g7d5e91b80`.

## Claude Code (TypeScript)

Claude Code delegates the whole status line to a command you configure. The command receives session data as JSON on stdin and prints the row, and Claude Code reruns it on session events, debounced at 300 ms, plus an optional `refreshInterval` timer ([status line docs](https://code.claude.com/docs/en/statusline), read on 2026-10-18).

The useful data split is provider-local versus external:

- **Provider-local**: context and session cost come from Claude Code's JSON input. `context_window.current_usage` splits the last request into input, cache creation, and cache read tokens, and `cost.total_cost_usd` estimates the session cost at list price.
- **External billing**: block and daily totals need an external tool such as `ccusage`, which the command runs and caches itself.
- **Rendering boundary**: the command owns formatting, while Claude Code owns structured data export.

## OpenAI Codex (Rust)

Codex exposes a roster-style `tui.status_line` array of built-in item names. At Codex commit ce0323f2f (2026-10-17), the default roster is `model-with-reasoning`, `current-dir`, and `thread-name`. Other items include the git branch, PR number, run state, context used, five-hour and weekly limits, thread title, and task progress (`StatusLineItem` in `codex-rs/tui/src/bottom_pane/status_line_setup.rs`).

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
