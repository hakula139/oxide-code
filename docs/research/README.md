# Research Notes

External research and API reference for oxide-code development. Covers Claude Code, OpenAI Codex, opencode, and the Anthropic API.

Organized by topic. Each subdirectory mirrors the corresponding directory in [`docs/design/`](../design/), where shipped decisions live.

## API References

| Document                                      | Description                                        |
| --------------------------------------------- | -------------------------------------------------- |
| [Anthropic API](api/anthropic.md)             | OAuth flow, required headers, system prompt prefix |
| [Extended Thinking](api/extended-thinking.md) | Content block types, signatures, round-tripping    |
| [System Prompt](api/system-prompt.md)         | Section assembly, CLAUDE.md, caching, block layout |

## Agent Loop

| Document                                    | Description                                           |
| ------------------------------------------- | ----------------------------------------------------- |
| [Auto-Compaction](agent/auto-compaction.md) | Automatic compaction thresholds, triggers, fail-safes |

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

| Document                                 | Description                                            |
| ---------------------------------------- | ------------------------------------------------------ |
| [Output Truncation](tools/truncation.md) | Per-tool vs central caps, spillover strategies         |
| [Permissions](tools/permissions.md)      | Tool approval modes, rule grammar, decision precedence |

## Terminal UI

| Document                                             | Description                                                                              |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [Overview](tui/overview.md)                          | Reference TUI patterns, flickering prevention, ecosystem                                 |
| [Cancellation and Queued Input](tui/cancellation.md) | Cancel, exit, and input queueing patterns                                                |
| [Mouse Interactions](tui/mouse-interactions.md)      | Mouse capture, click handling, OSC 8 / OSC 52 across coding CLIs, xterm.js parser quirks |
| [Status Line](tui/status-line.md)                    | Segment ordering, usage, and billing patterns across coding CLIs                         |
| [Welcome Screen](tui/welcome.md)                     | Empty-state surfaces and layout primitives across the three CLIs                         |
