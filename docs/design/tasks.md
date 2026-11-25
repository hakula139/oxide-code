# Task List

The `tasks` tool lets the model maintain an ordered task list for multi-step work, owned by the engine, rendered across frontends, and preserved across resume and compaction.

## Tool

Each call replaces the session's list with the full ordered list:

- **Validation.** Task content collapses to one line. Blank content, unknown statuses, or a second `in_progress` task reject the call with a fix message, leaving the list unchanged.
- **Clearing.** Completing every task or sending an empty list clears the list.
- **Result.** The chat block renders the submitted list so the finishing step stays visible after clearing.
- **Permission.** Belongs to `read` (no workspace effects), running without approval in every mode including `plan`. Path-scoped `read` rules never match it, while a class-wide `read` deny blocks it.

## Engine Flow

The engine emits `TasksUpdated` after each successful call and adopts the turn's latest list even if the turn fails or cancels (since the result is already in the transcript). `/clear` resets to empty, and `Engine::spawn` / `/resume` emit `TasksUpdated` when restoring a non-empty list. Frontends clear on `SessionRolled` and `SessionResumed` and replace without merging on each `TasksUpdated`.

## Frontends

| Frontend | Presentation                                                                                         |
| -------- | ---------------------------------------------------------------------------------------------------- |
| TUI      | Task panel and optional `task-progress` status-line segment                                          |
| Headless | Task list on stderr and [task events and result views](../guide/headless.md#events) in `stream-json` |

## Persistence and Compaction

The list has no dedicated DB column. Resume restores it from the last successful `tasks` call on the active chain, falling back to the compaction root's `tasks` field (otherwise empty), keeping forked branches independent.

Manual and automatic compaction store the current list in the `compact` entry via `Engine::replace_with_summary`, append a `<system-reminder>` listing tasks and statuses to the synthetic summary message, and keep the engine's live list unchanged across the boundary.

## Design Decisions

1. **Replace the whole list.** Matches Claude Code's `TodoWrite` and Codex's `update_plan`: restating all statuses prevents stale items and avoids task IDs (whereas oh-my-pi's phase-editing `todo` needs a larger schema).
2. **Reject a second task in progress.** Claude Code and Codex state the rule in prompts only, and oh-my-pi demotes extras silently. Rejecting tells the model what broke without silently activating the wrong task.
3. **Clear a finished list.** Matches Claude Code's `TodoWrite` so the panel shows only remaining work, with the description asking the model to send an empty list when changing course.
4. **Use the transcript as the source of truth.** Call inputs already record the list, avoiding duplicate state that could diverge from the active branch, matching Claude Code (last `TodoWrite` call) and oh-my-pi (branch tool results).
5. **Remind the model once at the boundary.** Compaction is the only time prior `tasks` calls drop from context, avoiding Claude Code's periodic reminders while the list is still visible.
6. **Count hidden tasks like every other overflow.** Uses `... +N` to match chat footers and queued prompts (the expanded `tasks` chat block always shows every item even when the panel window hides items on both sides).

## Sources

- `crates/ox-core/src/engine/compaction.rs`: `Engine::run_compaction`, which carries the list into the compact boundary.
- `crates/ox-core/src/engine/dispatch.rs`: `run_tool_round`, which emits `TasksUpdated` and records `ToolRound::tasks`.
- `crates/ox-core/src/engine/post_compact.rs`: `synthesize_post_compact_message` and `task_reminder`, the block listing tasks after the summary.
- `crates/ox-core/src/engine/sessions.rs`: `emit_restored_tasks`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::tasks`, adopted after each turn.
- `crates/ox-core/src/session/handle.rs`: `LoadedTranscript::tasks`, the list restored from a resumed transcript.
- `crates/ox-core/src/session/store.rs`: the `tasks` field in `CompactRecord`.
- `crates/ox-core/src/tool/tasks.rs`: `TasksTool`, validation, and `latest`.
- `crates/ox-tui/src/app/events.rs`: the panel's updates on `TasksUpdated`, `SessionRolled`, and `SessionResumed`.
- `crates/ox-tui/src/app/render.rs`: `draw_frame`, which sizes the panel after the higher-priority bands.
- `crates/ox-tui/src/components/chat/blocks/tool/tasks.rs`: the chat block for a `tasks` call.
- `crates/ox-tui/src/components/status/line.rs`: the `task-progress` segment.
- `crates/ox-tui/src/components/tasks.rs`: `TaskPanel` and `window`.
- `crates/ox/src/driver/stdio.rs`: `task_lines`, the headless task list.
