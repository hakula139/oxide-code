# Research Notes

External research and API reference for oxide-code development. The surveys compare [Claude Code](https://github.com/hakula139/claude-code), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode), and the API notes summarize Anthropic's documentation. The terminal UI surveys also cover terminal emulators and other TUI apps. Each survey records the date and versions it was verified against.

Each note informs the matching page in [`docs/design/`](../design/), where the shipped decisions live.

## API References

| Document                                      | Description                                                                          |
| --------------------------------------------- | ------------------------------------------------------------------------------------ |
| [Anthropic API](api/anthropic.md)             | Credentials, headers, caching, request fields, gateway restrictions, token refresh   |
| [Extended Thinking](api/extended-thinking.md) | Thinking configuration, streamed blocks, signatures, replay constraints              |
| [System Prompt](api/system-prompt.md)         | Prompt assembly, instruction discovery, context injection, caching, request metadata |

## Agent Loop

| Document                          | Description                                                                                     |
| --------------------------------- | ----------------------------------------------------------------------------------------------- |
| [Compaction](agent/compaction.md) | Manual and automatic triggers, token signals, prompts, replacement strategies, failure handling |

## Commands

| Document                      | Description                                                      |
| ----------------------------- | ---------------------------------------------------------------- |
| [Commands](slash/commands.md) | Registry shape, popup UX, execution models                       |
| [Modals](slash/modals.md)     | Picker and dialog primitives in Claude Code, Codex, and opencode |
| [Resume](slash/resume.md)     | CLI flags, picker UX, search and pagination, mid-session reload  |

## Tools

| Document                                 | Description                                                   |
| ---------------------------------------- | ------------------------------------------------------------- |
| [Output Truncation](tools/truncation.md) | Per-tool and central caps, spillover strategies               |
| [Permissions](tools/permissions.md)      | Approval modes, rules, decision precedence, grant persistence |

## Terminal UI

| Document                                             | Description                                                                              |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [Cancellation and Queued Input](tui/cancellation.md) | Cancel, exit, and input queueing patterns                                                |
| [Mouse Interactions](tui/mouse-interactions.md)      | Mouse capture, click handling, OSC 8 / OSC 52 across coding CLIs, xterm.js parser quirks |
| [Status Line](tui/status-line.md)                    | Segment ordering, usage, and billing patterns across coding CLIs                         |
| [Welcome Screen](tui/welcome.md)                     | Empty-state surfaces and layout primitives across coding CLIs                            |
