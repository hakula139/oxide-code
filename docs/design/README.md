# Design Specs

Architecture decisions and implementation specs for oxide-code.

Organized by topic. Each subdirectory shares its topic with a directory in [`docs/research/`](../research/), where the underlying research lives, although some specs, such as Background Shells and Task List, have no research note.

## Overview

| Document                        | Description                                        |
| ------------------------------- | -------------------------------------------------- |
| [Architecture](architecture.md) | Top-level module map and the layers imports follow |

## Agent Loop

| Document                                    | Description                                           |
| ------------------------------------------- | ----------------------------------------------------- |
| [Auto-Compaction](agent/auto-compaction.md) | Automatic compaction thresholds, triggers, fail-safes |

## Session

| Document                                         | Description                                                                   |
| ------------------------------------------------ | ----------------------------------------------------------------------------- |
| [Persistence](session/persistence.md)            | JSONL schema, writes and recovery, prompt context, archiving, titles, listing |
| [File Change Tracking](session/file-tracking.md) | Read-before-Edit gate, staleness detection, persistence                       |

## Slash Commands

| Document                      | Description                                                                      |
| ----------------------------- | -------------------------------------------------------------------------------- |
| [Commands](slash/commands.md) | Registry, dispatch, autocomplete, per-command behavior                           |
| [Compact](slash/compact.md)   | `/compact`, summarization, overflow retries, persistence boundary                |
| [Init](slash/init.md)         | `/init`, the instruction-file picker, diff-first updates                         |
| [Login](slash/login.md)       | `/login` and `/logout`, browser sign-in, credential stores, signed-out sessions  |
| [Modal UI](slash/modals.md)   | Modal trait and stack, shared primitives, pickers, editors, approvals, overviews |
| [Resume](slash/resume.md)     | Session picker, search, archive view, listing, in-process resume                 |

## Tools

| Document                                        | Description                                                          |
| ----------------------------------------------- | -------------------------------------------------------------------- |
| [Background Shells](tools/background-shells.md) | `bash` background runs, `bash_output`, and `bash_stop`               |
| [Output Truncation](tools/truncation.md)        | Per-tool view-shape caps, a centralized byte budget, and spill files |
| [Permissions](tools/permissions.md)             | Tiered deny / ask / allow gate, modes, rule grammar                  |
| [Task List](tools/tasks.md)                     | `tasks` tool, task panel, resume and compaction                      |

## Terminal UI

| Document                                             | Description                                                                 |
| ---------------------------------------------------- | --------------------------------------------------------------------------- |
| [Overview](tui/overview.md)                          | Core stack, rendering strategy, streaming architecture                      |
| [Cancellation and Queued Input](tui/cancellation.md) | Cancel, exit, and mid-turn queued prompts                                   |
| [Mouse Interactions](tui/mouse-interactions.md)      | Mouse capture for wheel scroll, chat selection and copy, OSC 8 PR hyperlink |
| [Status Line](tui/status-line.md)                    | Configurable status-line segments and usage display                         |
| [Welcome Screen](tui/welcome.md)                     | Empty-state renderer, `WelcomeSnapshot`, width ladder                       |
