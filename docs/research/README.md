# Research Notes

External research and API reference for oxide-code development. Covers Claude Code, OpenAI Codex, opencode, and the Anthropic API.

Organized by topic. Each subdirectory mirrors the corresponding directory in [`docs/design/`](../design/), where shipped decisions live.

## API References

| Document                                              | Description                                        |
| ----------------------------------------------------- | -------------------------------------------------- |
| [Anthropic API](api/anthropic.md)                     | OAuth flow, required headers, system prompt prefix |
| [Extended Thinking](api/extended-thinking.md)         | Content block types, signatures, round-tripping    |
| [System Prompt](api/system-prompt.md)                 | Section assembly, CLAUDE.md, caching, block layout |

## Session

| Document                                              | Description                                          |
| ----------------------------------------------------- | ---------------------------------------------------- |
| [Persistence](session/persistence.md)                 | JSONL format, storage layout, write strategy         |
| [File Change Tracking](session/file-tracking.md)      | Read-before-Edit gates, staleness detection          |

## Slash Commands

| Document                                              | Description                                          |
| ----------------------------------------------------- | ---------------------------------------------------- |
| [Commands](slash/commands.md)                         | Registry shape, popup UX, execution models           |
| [Modals](slash/modals.md)                             | Picker / dialog primitives across the three CLIs     |

## Tools

| Document                                              | Description                                          |
| ----------------------------------------------------- | ---------------------------------------------------- |
| [Output Truncation](tools/truncation.md)              | Per-tool vs central caps, spillover strategies       |

## Terminal UI

| Document                                              | Description                                              |
| ----------------------------------------------------- | -------------------------------------------------------- |
| [Overview](tui/overview.md)                           | Reference TUI patterns, flickering prevention, ecosystem |
| [Cancellation and Queued Input](tui/cancellation.md)  | Cancel, exit, and input queueing patterns                |
