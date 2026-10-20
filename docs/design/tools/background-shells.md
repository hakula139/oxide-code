# Background Shells

The `bash` tool can start a command in a background shell that keeps running after the call returns. Use `bash_output` to read its output and `bash_stop` to end it. Background shells suit dev servers, watchers, and long builds that the model checks while it continues working. They end with the session.

## Tools

- **Starting.** Calling `bash` with `run_in_background: true` starts the command and returns immediately with an ID such as `shell-1`. IDs count up while `ox` runs, so they are not reused within a run. A session resumed in a new `ox` process starts again at `shell-1`. The call's `timeout` does not apply. It uses the `exec` permission class, like a foreground call, and its approval prompt is titled `Bash (background)`. The tool description tells the model that it is not notified when the command finishes and to set the flag instead of adding a trailing `&`.
- **Reading.** `bash_output` returns the output printed since the previous read, or `(no new output)`, followed by `(still running)`, `(exit code N)`, or `(killed by signal N)`. Each read has the same output limit as a foreground call: it retains the head and tail of each pipe and saves any pipe it cuts in full to a [spill file](truncation.md#spill-files) numbered by the read. An optional `timeout` in milliseconds, capped at 600000, waits for the command to finish before returning. The wait ends when the command exits or the timeout passes, so for a server it lasts the full timeout. An unknown ID returns an error listing the open shells and their states.
- **Stopping.** `bash_stop` ends the shell and its process tree, removes it from the list, and returns any unread output followed by `(stopped)`. If the shell had already finished, the result ends with `(stopped after it exited with code N)` or `(stopped after signal N killed it)`.
- **Limit.** A shell remains open, whether running or finished, until `bash_stop` stops it or the session ends. This keeps a finished shell's last output readable. A session can hold at most 16 open shells. If that limit is reached, `bash` refuses to start another and returns an error listing the open shells.
- **Permission.** `bash_output` and `bash_stop` belong to the `read` class and run without approval in every mode. They act only on shells started by an allowed `bash` call. A class-wide `read` deny blocks them, but `bash` can still start background shells. Those shells run unread until the session ends. Neither tool joins a parallel batch: reading empties the unread output, while stopping ends the shell another call may be reading.

## Process Lifecycle

Each shell runs under a supervisor task that owns its `ShellTree`: the process group on Unix or Job Object on Windows that also holds a foreground call. The supervisor copies stdout and stderr into the shell's captures as they arrive, then publishes the exit state when the shell exits.

A stop kills the whole tree while the shell is running. It also kills the tree after the shell exits if a descendant still holds one of its pipes, as with a server started with a trailing `&`. Once the shell has exited and its pipes have closed, the supervisor releases the tree, as it does for a foreground call. A descendant that redirected its output elsewhere then keeps running. `bash_stop` returns after the supervisor has killed or released the tree and drained the remaining output.

The engine ends every open shell and waits for each tree to be gone when it rolls to a new session on `/clear` or `/archive`, resumes another session, or shuts down. A failed resume leaves the shells running because the session stays active. Pressing Esc to interrupt a turn leaves background shells running because only the turn's own work stops.

## Compaction

Shells stay open through `/compact` and auto-compaction. Because the `bash` calls that started them are gone, a `<system-reminder>` block at the end of the synthetic summary message lists each open shell's ID, state, and command's first line. It follows the task list's block. The reminder is part of the persisted summary, so it remains visible when the session is resumed, even after the shells have ended. `bash_output` then reports that the ID is not open.

## Frontends

Whenever a shell starts, exits, or stops, the engine emits `Event::BackgroundShellsUpdated` with every open shell. Each entry includes the shell's ID, command, and state. Ending shells with the session emits no event, so frontends clear their copy on `SessionRolled` and `SessionResumed`, just as they do the task list.

- **TUI.** `/status` lists each shell under a Background Shells heading, along with its state and command's first line.
- **REPL.** `--no-tui` ignores the event, and the model receives output through `bash_output` results.
- **Headless.** `stream-json` output includes the `background_shells_updated` event, as described in the [headless guide](../../guide/headless.md#events).

## Design Decisions

1. **Read incrementally from memory.** Each `bash_output` read returns what has arrived since the previous read, so the model sees new lines without rereading old ones. Claude Code writes background output to a file and shows the model its tail. A model that reads before about 128 KB piles up sees the whole stream, and a read that cuts its window keeps only that window on disk.
2. **Report an exit on the next read.** Claude Code adds a `<task-notification>` message when a background task finishes. Here, a shell never starts a turn or adds a message by itself, and the tool text says so because a model used to Claude Code may wait for a notice. The model learns that the shell exited on its next `bash_output` read, whose `timeout` wait can cover a build.
3. **Keep finished shells open until stopped.** Closing a shell on exit would lose output the model has not yet read. The cap bounds what the session holds because each shell keeps at most one head-and-tail capture per pipe. Codex sets a soft cap of 64 processes, and oh-my-pi caps running jobs at 15.
4. **End shells with the session.** A dev server that outlived `ox` would keep its port open with nothing left to stop it. Codex also terminates its processes at teardown. The engine waits for each tree to end, so even a signal exit, which does not wait for spawned tasks, still kills them.
5. **Release a finished tree whose pipes are closed.** Once the shell has exited and nothing holds its pipes, oxide-code can no longer tell when the process group empties. Killing the group later could hit an unrelated group that reused its ID. Releasing it matches a foreground call, where a redirected server also keeps running.
6. **Skip approval for reads and stops.** Each `bash_output` or `bash_stop` call acts on a shell already started by an allowed `bash` call, so asking again would repeat that approval.

## Sources

- `crates/ox-core/src/engine.rs`: `Engine::shells`, which `Engine::new` shares with the three tools, attached to the event channel in `spawn` and ended in `adopt_session` and `finalize`.
- `crates/ox-core/src/engine/compact_boundary.rs`: `Boundary::shells`, the open shells listed in the summary message.
- `crates/ox-core/src/protocol.rs`: `Event::BackgroundShellsUpdated`.
- `crates/ox-core/src/session/post_compact.rs`: `shell_reminder`, the block listing open shells after the summary.
- `crates/ox-core/src/tool.rs`: `ToolRegistry::built_in`, which hands one `BackgroundShells` to the three tools.
- `crates/ox-core/src/tool/bash.rs`: `run_in_background`, `ShellTree`, `PipeCapture`, and `render_streams`.
- `crates/ox-core/src/tool/bash/background.rs`: `BackgroundShells`, `supervise`, `BashOutputTool`, and `BashStopTool`.
- `crates/ox/src/driver/stdio.rs`: `render`, which ignores `BackgroundShellsUpdated`.
- `crates/ox/src/slash/status.rs`: the Background Shells section.
- `crates/ox/src/tui/app/events.rs`: the list shown by `/status`, replaced on each `BackgroundShellsUpdated` and cleared on `SessionRolled` and `SessionResumed`.
