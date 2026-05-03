# Design Specs

Architecture decisions and implementation specs for oxide-code.

| Document                                                          | Description                                             |
| ----------------------------------------------------------------- | ------------------------------------------------------- |
| [Session Persistence](session-persistence.md)                     | JSONL format, actor-owned writes, resume semantics      |
| [Terminal UI](tui.md)                                             | Core stack, rendering strategy, streaming architecture  |
| [Tool Output Truncation](tool-truncation.md)                      | Per-tool view-shape caps + centralized byte-budget      |
| [File Change Tracking](file-tracking.md)                          | Read-before-Edit gate, staleness detection, persistence |
| [Cancellation and Queued Input](cancellation-and-queued-input.md) | Cancel, exit, and mid-turn queued prompts               |
| [Slash Commands](slash-commands.md)                               | Registry, dispatch, popup, per-command notes            |
