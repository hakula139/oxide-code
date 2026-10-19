# Task List

The `tasks` tool lets the model maintain a task list for multi-step work. The engine owns the list, which every frontend displays and which persists across resume and compaction.

## Tool

Each call sends the full list in order, replacing the session's previous list. Each task has a `content` description and a `status` of `pending`, `in_progress`, or `completed`.

- **Validation.** Each task's content is collapsed onto one line. If a task has blank content, an unknown status, or a second `in_progress` status, the call is rejected with an error explaining how to fix it. Rejected calls leave the list unchanged.
- **Clearing.** The list clears when all tasks are completed or when the model sends an empty list. This removes a finished list once the work is done.
- **Result.** The result reports progress (`Updated the task list: 1 of 3 completed.`) and prompts the model to continue, with a title such as `Updated 3 tasks, 1 completed`. The chat block lists the tasks sent in the call, so the step that finished the list remains visible after the list clears.
- **Permission.** The tool belongs to the `read` class because it changes nothing in the workspace. It therefore runs without approval in every mode, including `plan`. A path-scoped `read` rule never matches it, while a class-wide `read` deny blocks it.

## Engine Flow

The tool returns its list in `ToolOutput::tasks`. Immediately after the call's `ToolCallEnd`, `run_tool_round` emits `Event::TasksUpdated` with the full list and records it in `ToolRound`. `agent_turn` retains the last list set by a round as `TurnReport::tasks`, and `Engine::submit_prompt` adopts it when the turn ends. If a turn is cancelled or fails after a `tasks` call, the list is still adopted because the call's result is already in the transcript.

`/clear` starts the new session with an empty list. After a resumed session restores a non-empty list, `Engine::spawn` and `/resume` emit `TasksUpdated`. A frontend clears its copy on `SessionRolled` and `SessionResumed`, then replaces the list on each event without merging.

## Frontends

- **TUI.** A task panel above the queued-prompt preview opens with a blank row that sets it apart from the chat, then uses up to six rows for tasks. For longer lists, it shows five tasks and a final row counting the rest, such as `... +3 tasks`. The window starts one task before the task in progress, or before the first unfinished task if none is in progress, keeping the step just finished beside the work ahead. The input, popup, modal, and queued-prompt preview get their rows first. A shorter panel keeps its count row and gives up the gap if retaining it would leave no room for a task row. Markers are `○` for pending, `●` for in progress, and `✓` for completed. The opt-in `task-progress` status-line segment shows `Tasks: 1/3` while the list contains a task.
- **REPL.** `--no-tui` prints the list to stderr after each update, under a `Tasks: 1/3` header.
- **Headless.** Every output format prints the list to stderr, as the REPL does. `stream-json` output also includes the `tasks_updated` event and the `tasks` result view, as described in the [headless guide](../../guide/headless.md#events).

## Persistence

The list has no dedicated entry type. When resuming, the loader uses the last successful `tasks` call on the resolved chain. If there is no such call, it uses the `tasks` field from the last `compact` entry. If that field is also missing, the list starts empty. Restoring it from the transcript keeps each branch's list independent in a forked session.

## Compaction

Manual and automatic compaction both pass the engine's list to `replace_session_with_summary`. The `compact` entry records the list because the summary replaces the calls that built it. The synthetic summary message ends with a `<system-reminder>` block listing each task and its status, and asking the model to keep the list current. Since the engine retains the list, the panel and status line stay unchanged across the boundary.

## Design Decisions

1. **Replace the whole list.** Claude Code's `TodoWrite` and Codex's `update_plan` both take the complete list on every call. The model restates every status, so calls cannot leave stale items behind and tasks need no IDs. oh-my-pi's `todo` edits phases through operations, which works well for long plans but requires a larger schema.
2. **Reject a second task in progress.** Claude Code and Codex ask the model in the tool prompt to keep one task in progress, but do not enforce it. oh-my-pi demotes extra tasks to pending. Rejecting the call tells the model which rule it broke and preserves the list it sent. A silent repair could mark the wrong task as active.
3. **Clear a finished list.** Claude Code's `TodoWrite` also drops the list once every task is completed, so the panel shows only the work that remains. Its description also asks the model to remove tasks that no longer apply and send an empty list when the user changes course, since otherwise a stale list would stay on screen.
4. **Use the transcript as the source of truth.** The call's input already records the list, so a dedicated entry would duplicate it and could disagree with the resolved branch. Claude Code's `extractTodosFromTranscript` restores the list from the last `TodoWrite` call, while oh-my-pi rehydrates it from the branch's tool results.
5. **Remind the model once at the boundary.** Compaction is the only point where the calls that built the list leave the model's context. A single reminder in the summary message restores it, avoiding periodic nudges like those Claude Code adds. Those nudges resend the list on turns when the model can already see it.
6. **Count hidden tasks like every other overflow.** The panel's last row uses the `... +N` wording of the chat's overflow footers and the queued-prompt preview, so every cut-off list in the TUI reads the same. The window can hide tasks on both sides of it, which the count does not tell apart, while the chat's block for the latest `tasks` call lists every task in the expanded view.

## Sources

- `crates/oxide-code/src/driver/stdio.rs`: `task_lines`, the REPL's task list.
- `crates/oxide-code/src/engine.rs`: `Engine::tasks`, adopted after each turn, and `emit_restored_tasks`.
- `crates/oxide-code/src/engine/compact_boundary.rs`: `Boundary`, which carries the list into compaction.
- `crates/oxide-code/src/engine/compaction.rs`: `AutoCompact::tasks`, the list passed through automatic compaction.
- `crates/oxide-code/src/engine/dispatch.rs`: `run_tool_round`, which emits `TasksUpdated` and records `ToolRound::tasks`.
- `crates/oxide-code/src/engine/turn.rs`: `TurnReport::tasks`.
- `crates/oxide-code/src/protocol.rs`: `Event::TasksUpdated`.
- `crates/oxide-code/src/session/entry.rs`: the `tasks` field in `Entry::Compact`.
- `crates/oxide-code/src/session/handle.rs`: `LoadedTranscript::tasks`, the list restored from a resumed transcript.
- `crates/oxide-code/src/session/post_compact.rs`: `synthesize_post_compact_message` and `task_reminder`, the block listing tasks after the summary.
- `crates/oxide-code/src/tool.rs`: `ToolOutput::tasks` and `ToolResultView::Tasks`.
- `crates/oxide-code/src/tool/tasks.rs`: `TasksTool`, validation, and `latest`.
- `crates/oxide-code/src/tui/app/events.rs`: the panel's updates on `TasksUpdated`, `SessionRolled`, and `SessionResumed`.
- `crates/oxide-code/src/tui/app/render.rs`: `draw_frame`, which sizes the panel after the higher-priority bands.
- `crates/oxide-code/src/tui/components/chat/blocks/tool/tasks.rs`: the chat block for a `tasks` call.
- `crates/oxide-code/src/tui/components/status/line.rs`: the `task-progress` segment.
- `crates/oxide-code/src/tui/components/tasks.rs`: `TaskPanel` and `window`.
- `crates/oxide-code/src/tui/glyphs.rs`: `task_marker`.
