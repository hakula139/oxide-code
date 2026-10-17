# Task List

The `tasks` tool lets the model maintain a checklist for multi-step work. The engine owns the list, which every frontend displays and which persists through resume and compaction.

## Tool

Each call sends the full list in order, replacing the session's previous list. Each task has a `content` description and a `status` of `pending`, `in_progress`, or `completed`.

- **Validation.** A call is rejected with an error explaining how to fix it if a task has blank content, an unknown status, or a second `in_progress` status. Rejected calls leave the list unchanged.
- **Clearing.** The list clears when all tasks are completed or when it is empty, so a finished checklist disappears once the work is done.
- **Result.** The result reports progress (`Updated the task list: 1 of 3 completed.`) under a title such as `1 of 3 tasks completed`. The chat block lists the tasks sent in the call, keeping the step that finished the checklist visible after the list clears.
- **Permission.** The tool belongs to the `read` class because it touches no files. It runs without approval in every mode, including `plan`.

## Engine Flow

The tool returns its list in `ToolOutput::tasks`. Immediately after the call's `ToolCallEnd`, `run_tool_round` emits `Event::TasksUpdated` with the full list and records it in `ToolRound`. `agent_turn` passes the round's list through as `TurnReport::tasks`, and `Engine::submit_prompt` adopts it when the turn ends. If a turn is cancelled or fails after a `tasks` call, it still adopts the list because the call's result is already in the transcript.

`/clear` starts the new session with an empty list. After a resumed session restores a non-empty list, `Engine::spawn` and `/resume` emit `TasksUpdated`. A frontend clears its copy on `SessionRolled` and `SessionResumed`, then replaces the list on each event without merging.

## Frontends

- **TUI.** A task panel above the queued-prompt preview shows up to five tasks, followed by a `+N more` row when needed. Its window starts one task before the first unfinished one, keeping the step just finished beside the work ahead. Markers are `○` for pending, `●` for in progress, and `✓` for completed. While the list contains a task, the `task-progress` status-line segment shows `Tasks: 1/3`.
- **REPL.** `--no-tui` prints the list to stderr after each update.
- **Headless.** `stream-json` output includes the `tasks_updated` event and the `tasks` result view, as described in the [headless guide](../../guide/headless.md#events).

## Persistence

The list has no entry type of its own. When resuming, the loader uses the last successful `tasks` call on the resolved chain. If there is none, it uses the `tasks` field from the last `compact` entry. If that is missing too, the list starts empty. Reading it from the transcript keeps each branch of a forked session’s list independent.

## Compaction

Manual and automatic compaction both pass the engine's list to `replace_session_with_summary`. The `compact` entry records the list because the summary replaces the calls that built it. The synthetic summary message ends with a `<system-reminder>` block listing each task and its status, and asking the model to keep the list current. Since the engine retains the list, the panel and status line stay unchanged across the boundary.

## Design Decisions

1. **Replace the whole list.** Claude Code's `TodoWrite` and Codex's `update_plan` both take the complete list on each call. The model restates every status, so calls cannot leave stale items behind and tasks need no IDs. oh-my-pi's `todo` edits phases through operations, which suits long plans at the cost of a larger schema.
2. **Reject a second task in progress.** Claude Code and Codex ask for one task in progress in the tool prompt but do not check it, while oh-my-pi demotes extras to pending. Rejecting the call tells the model which rule it broke and preserves the list it sent. A silent repair could mark the wrong task as active.
3. **Clear a finished list.** Claude Code's `TodoWrite` also drops the list once every task is completed. The panel then shows only the work that remains.
4. **Use the transcript as the source of truth.** The call's input already records the list, so a dedicated entry would duplicate it and could disagree with the resolved branch. Claude Code's `extractTodosFromTranscript` restores the list from the last `TodoWrite` call, and oh-my-pi rehydrates it from the branch's tool results.
5. **Remind the model once at the boundary.** Compaction is the only point where the calls that built the list leave the model's context. A single reminder in the summary message restores it without periodic nudges like those Claude Code adds, which would resend the list on turns when the model can already see it.

## Sources

- `crates/oxide-code/src/driver/stdio.rs`: renders the REPL's task list.
- `crates/oxide-code/src/engine.rs`: defines `Engine::tasks`, adopted after each turn, and `emit_restored_tasks`.
- `crates/oxide-code/src/engine/compact_boundary.rs`: defines `Boundary`, which carries the list into compaction.
- `crates/oxide-code/src/engine/dispatch.rs`: defines `run_tool_round`, which emits `TasksUpdated` and records `ToolRound::tasks`.
- `crates/oxide-code/src/engine/turn.rs`: defines `TurnReport::tasks`.
- `crates/oxide-code/src/protocol.rs`: defines `Event::TasksUpdated`.
- `crates/oxide-code/src/session/entry.rs`: defines the `tasks` field of `Entry::Compact`.
- `crates/oxide-code/src/session/handle.rs`: defines `LoadedTranscript::tasks`, the list restored from a resumed transcript.
- `crates/oxide-code/src/session/post_compact.rs`: defines `synthesize_post_compact_message`, which appends the reminder.
- `crates/oxide-code/src/tool.rs`: defines `ToolOutput::tasks` and `ToolResultView::Tasks`.
- `crates/oxide-code/src/tool/tasks.rs`: defines `TasksTool`, validation, `latest`, and `reminder`.
- `crates/oxide-code/src/tui/components/chat/blocks/tool/tasks.rs`: defines the chat block for a `tasks` call.
- `crates/oxide-code/src/tui/components/status/line.rs`: defines the `task-progress` segment.
- `crates/oxide-code/src/tui/components/tasks.rs`: defines `TaskPanel` and `window`.
