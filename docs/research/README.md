# Research Notes

External research and API reference for oxide-code development. Covers Claude Code, OpenAI Codex, opencode, and the Anthropic API.

## API References

| Document                                  | Description                                        |
| ----------------------------------------- | -------------------------------------------------- |
| [Anthropic API](anthropic-api.md)         | OAuth flow, required headers, system prompt prefix |
| [Extended Thinking](extended-thinking.md) | Content block types, signatures, round-tripping    |
| [System Prompt](system-prompt.md)         | Section assembly, CLAUDE.md, caching, block layout |

## Design Surveys

| Document                                                          | Description                                              |
| ----------------------------------------------------------------- | -------------------------------------------------------- |
| [Session Persistence](session-persistence.md)                     | JSONL format, storage layout, write strategy             |
| [Terminal UI](tui.md)                                             | Reference TUI patterns, flickering prevention, ecosystem |
| [Tool Output Truncation](tool-truncation.md)                      | Per-tool vs central caps, spillover strategies           |
| [File Change Tracking](file-tracking.md)                          | Read-before-Edit gates, staleness detection              |
| [Cancellation and Queued Input](cancellation-and-queued-input.md) | Cancel, exit, and input queueing patterns                |
| [Slash Commands](slash-commands.md)                               | Registry shape, popup UX, execution models               |
