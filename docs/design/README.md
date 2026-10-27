# Design Specs

Architecture decisions and implementation specs for oxide-code.

Each page covers one topic, and [`docs/research/`](../research/) holds the external surveys and API references behind them.

## Overview

| Document                        | Description                                          |
| ------------------------------- | ---------------------------------------------------- |
| [Architecture](architecture.md) | Top-level module map and the layers imports follow   |
| [Client](client.md)             | Request shape, beta sets, cache breakpoints, retries |

## Agent Loop

| Document                    | Description                                                           |
| --------------------------- | --------------------------------------------------------------------- |
| [Engine](engine.md)         | `Op` / `Event` contract, terminal events, turn pipeline               |
| [Compaction](compaction.md) | Manual and automatic compaction, threshold, summarizer, failure reset |

## Session

| Document                                 | Description                                                                   |
| ---------------------------------------- | ----------------------------------------------------------------------------- |
| [Persistence](persistence.md)            | JSONL schema, writes and recovery, prompt context, archiving, titles, listing |
| [File Change Tracking](file-tracking.md) | Read-before-Edit gate, staleness detection, persistence                       |

## Commands

| Document                            | Description                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------- |
| [Slash Commands](slash-commands.md) | Registry, dispatch, autocomplete, per-command behavior                           |
| [Init](init.md)                     | `/init`, the instruction-file picker, diff-first updates                         |
| [Login](login.md)                   | `/login` and `/logout`, browser sign-in, credential stores, signed-out sessions  |
| [Modal UI](modals.md)               | Modal trait and stack, shared primitives, pickers, editors, approvals, overviews |
| [Resume](resume.md)                 | Session picker, search, archive view, listing, in-process resume                 |

## Tools

| Document                                  | Description                                                          |
| ----------------------------------------- | -------------------------------------------------------------------- |
| [Permissions](permissions.md)             | Tiered deny / ask / allow gate, modes, rule grammar                  |
| [Shell Execution](bash.md)                | `bash` process tree, terminal detachment, and environment            |
| [Background Shells](background-shells.md) | `bash` background runs, `bash_output`, and `bash_stop`               |
| [Output Truncation](truncation.md)        | Per-tool view-shape caps, a centralized byte budget, and spill files |
| [Task List](tasks.md)                     | `tasks` tool, task panel, resume and compaction                      |

## Terminal UI

| Document                                         | Description                                                                 |
| ------------------------------------------------ | --------------------------------------------------------------------------- |
| [Terminal UI](tui.md)                            | Core stack, rendering strategy, streaming architecture                      |
| [Cancellation and Queued Input](cancellation.md) | Cancel, exit, and mid-turn queued prompts                                   |
| [Mouse Interactions](mouse.md)                   | Mouse capture for wheel scroll, chat selection and copy, OSC 8 PR hyperlink |
| [Status Line](status-line.md)                    | Configurable status-line segments and usage display                         |
| [Welcome Screen](welcome.md)                     | Empty-state renderer, session-seeded starter picks, width ladder            |
