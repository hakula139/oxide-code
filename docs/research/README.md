# Research Notes

External research and API reference for oxide-code development. The notes cite [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), [opencode](https://github.com/anomalyco/opencode), and [oh-my-pi](https://github.com/can1357/oh-my-pi) as reference implementations, along with Anthropic's API documentation. The terminal UI notes also cover terminal emulators and other TUI apps.

Organized by topic. Each subdirectory except `api/` shares its topic with a directory in [`docs/design/`](../design/), where shipped decisions live.

## API References

| Document                                      | Description                                                                          |
| --------------------------------------------- | ------------------------------------------------------------------------------------ |
| [Anthropic API](api/anthropic.md)             | Credentials, headers, caching, request fields, gateway restrictions, token refresh   |
| [Extended Thinking](api/extended-thinking.md) | Thinking configuration, streamed blocks, signatures, replay constraints              |
| [System Prompt](api/system-prompt.md)         | Prompt assembly, instruction discovery, context injection, caching, request metadata |

## Agent Loop

| Document                                    | Description                                                                |
| ------------------------------------------- | -------------------------------------------------------------------------- |
| [Auto-Compaction](agent/auto-compaction.md) | Reference triggers, token signals, thresholds, summaries, failure handling |

## Session

| Document                                         | Description                                  |
| ------------------------------------------------ | -------------------------------------------- |
| [Persistence](session/persistence.md)            | JSONL format, storage layout, write strategy |
| [File Change Tracking](session/file-tracking.md) | Read-before-Edit gates, staleness detection  |

## Slash Commands

| Document                      | Description                                                   |
| ----------------------------- | ------------------------------------------------------------- |
| [Commands](slash/commands.md) | Registry shape, popup UX, execution models                    |
| [Compact](slash/compact.md)   | Context-compression triggers, prompts, replacement strategies |
| [Modals](slash/modals.md)     | Picker / dialog primitives across the three CLIs              |
| [Resume](slash/resume.md)     | CLI flags, picker UX, search / pagination, mid-session reload |

## Tools

| Document                                 | Description                                                              |
| ---------------------------------------- | ------------------------------------------------------------------------ |
| [Output Truncation](tools/truncation.md) | Per-tool vs central caps, spillover strategies                           |
| [Permissions](tools/permissions.md)      | Approval modes, rules, decision precedence, oxide-code integration seams |

## Terminal UI

| Document                                             | Description                                                                              |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [Overview](tui/overview.md)                          | Reference TUI patterns, flickering prevention, ecosystem                                 |
| [Cancellation and Queued Input](tui/cancellation.md) | Cancel, exit, and input queueing patterns                                                |
| [Mouse Interactions](tui/mouse-interactions.md)      | Mouse capture, click handling, OSC 8 / OSC 52 across coding CLIs, xterm.js parser quirks |
| [Status Line](tui/status-line.md)                    | Segment ordering, usage, and billing patterns across coding CLIs                         |
| [Welcome Screen](tui/welcome.md)                     | Empty-state surfaces and layout primitives across the three CLIs                         |
