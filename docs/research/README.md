# Research Notes

Architecture research and API reference notes for oxide-code development. Split by direction:

- [`api/`](api/) — outward-facing: how the Anthropic API works and what oxide-code has to send to it.
- [`design/`](design/) — inward-facing: surveys of reference projects + design choices for oxide-code.

## API references

| Document                                      | Description                                                            |
| --------------------------------------------- | ---------------------------------------------------------------------- |
| [Anthropic API](api/anthropic-api.md)         | Anthropic API auth: OAuth flow, required headers, system prompt prefix |
| [Extended Thinking](api/extended-thinking.md) | Extended thinking: content block types, signatures, round-tripping     |
| [System Prompt](api/system-prompt.md)         | System prompt architecture: section assembly, CLAUDE.md, caching       |

## Design surveys

| Document                                                                 | Description                                                           |
| ------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| [Session Persistence](design/session-persistence.md)                     | Session persistence: JSONL format, storage layout, listing strategy   |
| [Terminal UI](design/tui.md)                                             | TUI research: reference projects, flickering prevention, crate stack  |
| [Tool Output Truncation](design/tool-truncation.md)                      | Tool dispatcher truncation: per-tool vs central, caps and spillover   |
| [File Change Tracking](design/file-tracking.md)                          | Read-before-Edit gate, staleness detection, persistence across resume |
| [Cancellation and Queued Input](design/cancellation-and-queued-input.md) | Esc / Ctrl+C cancel, double-press exit, queued prompts, run-state UX  |
| [Slash Commands](design/slash-commands/)                                 | Slash-command surface: registry shape, popup UX, mid-session state    |
