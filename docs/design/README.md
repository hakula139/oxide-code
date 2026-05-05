# Design Specs

Architecture decisions and implementation specs for oxide-code.

Organized by topic. Each subdirectory mirrors the corresponding directory in [`docs/research/`](../research/), where the underlying research lives.

## Session

| Document                                              | Description                                             |
| ----------------------------------------------------- | ------------------------------------------------------- |
| [Persistence](session/persistence.md)                 | JSONL format, actor-owned writes, resume semantics      |
| [File Change Tracking](session/file-tracking.md)      | Read-before-Edit gate, staleness detection, persistence |

## Slash Commands

| Document                                              | Description                                                          |
| ----------------------------------------------------- | -------------------------------------------------------------------- |
| [Commands](slash/commands.md)                         | Registry, dispatch, popup, per-command notes                         |
| [Modal UI](slash/modals.md)                           | `Modal` trait, `ModalStack`, `ListPicker`, model + effort + status   |

## Tools

| Document                                              | Description                                          |
| ----------------------------------------------------- | ---------------------------------------------------- |
| [Output Truncation](tools/truncation.md)              | Per-tool view-shape caps + centralized byte-budget   |

## Terminal UI

| Document                                              | Description                                            |
| ----------------------------------------------------- | ------------------------------------------------------ |
| [Overview](tui/overview.md)                           | Core stack, rendering strategy, streaming architecture |
| [Cancellation and Queued Input](tui/cancellation.md)  | Cancel, exit, and mid-turn queued prompts              |
