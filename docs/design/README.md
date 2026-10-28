# Design Specs

Architecture decisions and implementation specs for oxide-code.

Each page covers one topic, and [`docs/research/`](../research/) holds the API references and the surveys behind open design questions.

## Overview

| Document                        | Description                                          |
| ------------------------------- | ---------------------------------------------------- |
| [Architecture](architecture.md) | Top-level module map and the layers imports follow   |
| [Client](client.md)             | Request shape, beta sets, cache breakpoints, retries |

## Agent Loop

| Document                    | Description                                                           |
| --------------------------- | --------------------------------------------------------------------- |
| [Engine](engine.md)         | `Op` / `Event` contract, terminal events, turn pipeline               |
| [System Prompt](prompt.md)  | Static sections, environment, user context, tool descriptions         |
| [Compaction](compaction.md) | Manual and automatic compaction, threshold, summarizer, failure reset |

## Session

| Document                                 | Description                                                                |
| ---------------------------------------- | -------------------------------------------------------------------------- |
| [Persistence](persistence.md)            | SQLite schema, writes, lazy forks, export, prompt context, titles, listing |
| [File Change Tracking](file-tracking.md) | Read-before-Edit gate, staleness detection, persistence                    |

## Commands

| Document                            | Description                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------- |
| [Slash Commands](slash-commands.md) | Registry, dispatch, autocomplete, `/diff`                                        |
| [Init](init.md)                     | `/init`, the instruction-file picker, diff-first updates                         |
| [Login](login.md)                   | `/login` and `/logout`, browser sign-in, credential stores, signed-out sessions  |
| [Modal UI](modals.md)               | Modal trait and stack, shared primitives, pickers, editors, approvals, overviews |
| [Resume](resume.md)                 | Session picker, search, archive view, listing, in-process resume                 |

## Tools

| Document                                  | Description                                                          |
| ----------------------------------------- | -------------------------------------------------------------------- |
| [Permissions](permissions.md)             | Tiered deny / ask / allow gate, modes, rule grammar                  |
| [File Mutation](file-mutation.md)         | Edit and write publication in place, metadata preservation           |
| [Shell Execution](bash.md)                | `bash` process tree, terminal detachment, and environment            |
| [Background Shells](background-shells.md) | `bash` background runs, `bash_output`, and `bash_stop`               |
| [Output Truncation](truncation.md)        | Per-tool view-shape caps, a centralized byte budget, and spill files |
| [Task List](tasks.md)                     | `tasks` tool, task panel, resume and compaction                      |

## Terminal UI

| Document                                         | Description                                                                 |
| ------------------------------------------------ | --------------------------------------------------------------------------- |
| [Terminal UI](tui.md)                            | Core stack, rendering strategy, streaming architecture, welcome screen      |
| [Cancellation and Queued Input](cancellation.md) | Cancel, exit, and mid-turn queued prompts                                   |
| [Mouse Interactions](mouse.md)                   | Mouse capture for wheel scroll, chat selection and copy, OSC 8 PR hyperlink |
| [Status Line](status-line.md)                    | Configurable status-line segments and usage display                         |
