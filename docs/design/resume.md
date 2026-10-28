# Session Resume

`/resume` (alias `/continue`) opens a searchable session picker when bare, or jumps directly when given a session ID. The picker loads sessions a page at a time as the cursor nears the end of the loaded rows. At startup, `ox --resume` without a session opens the same picker.

Companion docs: [Slash Commands](slash-commands.md), [Modals](modals.md).

## Resume Surfaces

`slash/resume` hosts `ResumeCmd` and `ResumePicker`. Bare `/resume` opens the picker, which forwards `Op::Resume` with `ResumeTarget::Id`, the chosen session's ID. The typed form forwards `ResumeTarget::Prefix` through `command::resume`, which the bare REPL runs too, and `ox -r <file>` names a session export to import as `ResumeTarget::Path`. The engine resolves a prefix through `resolve_prefix`, which searches the current project first and widens to every project on a miss, and skips the live session, so a prefix that matches only the live session fails with a message saying so.

`ox --resume` without a session starts the TUI in a fresh session with no row yet and pushes `ResumePicker::at_startup` before the event loop starts. That picker quits `ox` when dismissed, and Enter resumes through the same swap as `/resume`. A session's row is created only with its first message, so neither path leaves an empty session in the store.

## The Picker

The picker wraps [`SearchableList`](modals.md#shared-primitives), and [Resuming a Session](../guide/sessions.md#resuming-a-session) covers what its keys do. Its named actions come from the `ACTIONS` keymap (**One keymap drives keys and hints**).

Rows load in pages of 30 from a `SessionWalk`, each read by one query on a worker thread that sends the page as an app event, which the App hands to the picker. A row whose session [forked](persistence.md#lazy-fork) from another shows `(fork of {id8})` after its title. A query keeps loading until it fills the viewport or the walk ends (**Load pages in the background as the cursor nears the end**). Toggling the scope or the archive view restarts the walk with the query kept. Each page answers one numbered request, so the old view's page goes nowhere when its worker finishes, as does a page for a picker that closed. A walk that fails, or a worker that dies without a page, takes over the footer with a load error, so a failure cannot pass for an empty list.

Each row is one line of columns: the ID prefix, the title, the branch, the project in the all-projects view, the message count, and the relative time. The title takes the width the fixed columns leave, and a narrow row drops the other columns, the ID first, to keep the title at least 24 columns wide, then drops the time below 12, so the title always shows.

Archiving and deleting from the picker go through the engine. Ctrl+X sends `Op::SetArchive` with the row's ID, the op the typed `/archive` and `/unarchive` send with a prefix, and Ctrl+D pushes the confirm-delete modal. While an archive toggle is pending, the picker ignores Enter, Ctrl+X, and Ctrl+D, and once the engine reports a success, it drops the row with the cursor index kept (**Ctrl+X moves a session between views in place**). A failure leaves the row and shows an error in the footer.

## The Swap

`session::handle::roll_into` mirrors the `roll` behind `/clear`. On the blocking pool, it resolves the target to a session ID through `resolver::resolve_target`, which imports a `ResumeTarget::Path` export, takes the session's liveness lock, and loads and sanitizes its chain, so a resolve or load failure leaves the live session untouched. It then swaps the handle in place and finalizes the old session. Loading an archived target unarchives it. `RollIntoOutcome` carries the target's `LoadedTranscript`, its title, its prior archive state, and the old session's finalize failure.

`Engine::resume` drives `roll_into` and adopts the loaded transcript as startup does: it replays the recorded prompt context, restores the file tracker, and emits `Event::SessionResumed` with the display history from `session::history::interactions`, already resolved so the TUI renders it without a tool registry. The events that follow report the old session's finalize failure, the restored session cost and task list, unreadable instruction files, tracker drift, and an interrupted reply, and `Event::SessionReady` ends the resume. A failed resolve or load ends it with `Event::TurnFailed` and keeps the live session. Later requests and tool calls read the session ID and artifact directory from the live handle, so they follow the swap.

On `SessionResumed`, the TUI rebuilds the chat, clears the modals, and discards the prompts queued before the resume with a notice. At `SessionReady` it returns to idle and submits the first prompt typed while the resume ran. A failed resume returns to idle with the queue intact and held ([**A cancel leaves the queue to the user**](cancellation.md#design-decisions)).

## Design Decisions

1. **Bare opens the picker, typed-arg resolves directly.** Same shape as `/model`, `/effort`, `/theme`: bare opens a modal, typed-arg switches directly. Reference CLIs converged on this exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).
2. **Alias `continue`.** Claude Code's `/resume` also takes `continue` as an alias, so users of either tool can type the word they know.
3. **Both forms are idle-only.** Bare and typed-arg refuse mid-turn, since the picker eventually submits an `Op::Resume`, and allowing it mid-stream would interleave a session swap with an in-flight response.
4. **Mid-session re-init uses `roll_into`.** The in-process swap preserves the process and terminal state, avoids an alt-screen exit or relaunch flash, and reuses the `roll` pattern from `/clear`. It clears modals, rolls back any theme preview, and discards the prompts queued before it.
5. **`RollIntoOutcome` nests the `LoadedTranscript`.** The engine adopts a loaded transcript as one unit, at startup from `ResumedSession` and on `/resume` from `RollIntoOutcome`, so both carry the same struct. The title, the `prior` archive state, and the old session's `finalize` report sit beside it, since the engine reads each one separately. The tracker snapshots travel inside the transcript, so startup and `/resume` both restore the tracker in the engine, and every frontend gets the drift notice.
6. **Surface old-session finalize failures and tracker drift as distinct events.** They have two causes: the previous session failing to finalize (writer), and tracked files drifting since the resumed session (filesystem). The finalize failure is an error that names the previous session, so the user does not read it as a current-writer fault. Drift is a notice, since the Read-before-Edit gate handles it and nothing failed.
7. **Drop queued prompts on resume, then surface the count.** Queued prompts belong to the source thread, so replaying them in a different transcript would amount to silent corruption. Every session switch drops them, `/clear` and bare `/archive` included, and prints `Discarded N queued prompts from the previous session` so the user knows their typing didn't carry over. The policy is [**A cancel leaves the queue to the user**](cancellation.md#design-decisions).
8. **`SearchableList` as a sibling primitive to `ListPicker`.** `ListPicker` is small and focused, and adding search and a viewport would double its surface for the simpler pickers. `SearchableList` follows the same trait and delegation pattern beside it.
9. **Two-method `SearchableItem`: `haystack` plus `render`.** A substring filter and a layout callback are the only things the primitive needs from items.
10. **Substring search only.** Codex and opencode both use plain substring search.
11. **Project scoping defaults to the current project, with Tab widening.** Matches the `ox -c` resolver semantics: CLI `--all` widens up front, in-picker `Tab` toggles, and the typed query persists across the rebuild.
12. **Load pages in the background as the cursor nears the end.** Loading the next page while a viewport of matching rows remains below the cursor keeps paging out of sight and lets a query reach every session in scope. Each page is one query, which runs on a worker thread so a slow disk never stalls the render loop.
13. **Keyset pages through `SessionWalk`.** The picker walks a `SessionWalk`, and `ox --list` reads one page of it through `list_paged`. Each page is ordered by `(updated_at, id)` and starts after the last row of the page before, so pages never overlap, while a session another process writes between two pages may show twice or not at all. Each query asks for one row past the page to learn whether more sessions follow, so listings report no exact total. In the current project, the `sessions_by_project` index on `(project_root, updated_at)` serves the order, so a page reads the project's rows in order and stops once it fills. Across all projects no index serves it, so each page scans and sorts the whole table. `ox -c` reads one row through `latest`, and a prefix lookup searches the current project through `sessions_by_project` before it widens to every project.
14. **`--list --limit 0` opts back into unbounded listing.** Scripts piping output need the full set, so `0` is the explicit unbounded value and no magic large default is needed.
15. **Bare suppresses echo, typed-arg echoes.** Mirrors the universal rule: the modal is the response, so the typed `> /resume` line stays out of history. Typed `> /resume <id>` echoes, so the line shows while the swap runs and heads the error when the swap fails. A successful resume then replaces the chat with the target's history.
16. **Always show the picker, even with one or zero matches.** Auto-resuming on a single match would surprise users who typed `/resume` just to peek the list, and an empty list with a clear message is better than silently doing nothing.
17. **Ctrl+A switches to the archived view.** Archived sessions stay out of the default rows but remain one key away, and resuming one from that view unarchives it, matching `ox -r <prefix>`. The view composes with Tab, so archived sessions can be listed across every project.
18. **`ox --resume` without a session reuses the in-TUI picker, and dismissing it quits.** Opening the picker over a fresh session lets startup share `/resume`'s modal and its in-process swap, avoiding a separate pre-TUI selection screen. Claude Code's `--resume` and Codex's `resume` subcommand both open a picker at startup. Codex starts a fresh session when you press Esc, but plain `ox` already does that, so Esc quits. Esc always keeps its universal modal meaning, even when you've typed a query, and Ctrl+U clears the query. The picker holds focus until it closes, so resume or compaction can't clear the stack and trigger its quitting `on_cancel` while it's open.
19. **One keymap drives keys and hints.** Each named action carries its key and a footer label that can depend on the view, so `handle_key` dispatches through the table that renders the footer, and adding a key adds its hint. The picker captures Esc so dismissal is one of those actions. The scope status stays out of the table. [#278](https://github.com/hakula139/oxide-code/issues/278) tracks rebinding these actions, and [#281](https://github.com/hakula139/oxide-code/issues/281) tracks fitting their hints to narrow footers.
20. **Ctrl+X moves a session between views in place.** Clearing out a long list takes one key per session, with no ID prefix to read off and type. Ctrl+X is free among the picker's keys. The filter's `LineEditor` maps it to cutting a selection, which the editor never draws, so the picker shadows it just as it shadows the editor's Ctrl+A (line start), Ctrl+D (delete forward), and Ctrl+U (kill to line start). The highlight moves to the next row, so repeated presses work through a run of sessions.

## Out of Scope

- **Date grouping in the picker.** `Today` / `Yesterday` / ISO date groups would not improve on a list ordered by last activity, whose relative times already encode recency.
- **Numeric `1`–`9` mnemonics.** Cursor plus Enter is enough, and mnemonics conflict with type-to-search.
- **Worktree or branch tree expansion.** Claude Code's `▼` / `▶` tree and a side preview pane would both double layout complexity for marginal gain.
- **In-picker rename of other sessions.** `/rename` covers the live session's title.
- **Fuzzy or agentic AI search.** Substring search covers typical session counts, and a model-backed query would add a request to each search.
- **`/sessions` listing variant.** One slash command per intent. `ox --list` already covers script-friendly listing.

## Sources

- `crates/ox-core/src/engine/history.rs`: `interactions`, the display projection `SessionResumed` carries.
- `crates/ox-core/src/engine/sessions.rs`: `Engine::resume`, `format_drift_warning`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::roll_into`, which swaps the live transcript for the loaded one.
- `crates/ox-core/src/session/handle.rs`: `roll_into`, `RollIntoOutcome`, `LoadedTranscript`.
- `crates/ox-core/src/session/resolver.rs`: `resolve_target`, which maps a `ResumeTarget` to a session ID, importing an export path, and `resolve_prefix`.
- `crates/ox-core/src/session/store/listing.rs`: `walk`, `SessionWalk`, `list_paged`, `latest`, `list_matching`, `ListPage`, `ListScope`, `ProjectScope`.
- `crates/ox-tui/src/modal/searchable_list.rs`: `SearchableList<T>`, `SearchableItem`, `append_items` and `set_has_more` for paged loading, and `retain_items`, which keeps the cursor when rows drop.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd`, `ResumePicker` and its `at_startup` variant, `PickerAction` and its `ACTIONS` keymap, `Pages`, and `SessionRow` with its `Column` layout.
