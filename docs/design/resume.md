# Session Resume

`/resume` (alias `/continue`) opens a searchable session picker when bare, or jumps directly when given a session ID. The picker loads sessions a page at a time as the cursor nears the end of the loaded rows. At startup, `ox --resume` without a session opens the same picker.

Companion docs: [Slash Commands](slash-commands.md), [Modals](modals.md). Underlying research: [research/slash/resume.md](../research/slash/resume.md).

## Resume Surfaces

`slash/resume` hosts both `ResumeCmd` and `ResumePicker`. Bare returns the picker as `SlashOutcome::Modal`, which forwards `Op::Resume` with `ResumeTarget::Id`, the chosen session's ID. Typed-arg forwards `ResumeTarget::Prefix` through `command::resume`, which the bare REPL runs too, and `roll_into` resolves the prefix through `resolve_prefix` (current project first, widening to all projects on miss). `ResumeTarget::Path` names a session export to import, as `ox -r <file>` gives at startup. The resolver skips the live session, and a prefix that matches only the live session fails the resume with a message saying so.

The picker wraps [`SearchableList`](modals.md) and adds these controls and display rules:

- The picker's named actions come from `ACTIONS`, a [`Keymap`](modals.md) that pairs each action with its key and footer label, and the footer renders its hints from the same entries. Up / Down and PageUp / PageDown, which move the cursor, are handled outside the table, and other keys edit the query through the shared `LineEditor`.
- A query matches each row's ID and title, plus the project when the all-projects view shows it.
- Each row is one line of columns: the ID prefix, the title, the branch, the project (only in the all-projects view), the message count, abbreviated past 999 as in `12k msgs`, and the relative time, abbreviated as in `3h ago`. The title takes the width the fixed-width columns leave and truncates with an ellipsis. When the title would drop below 24 columns, the ID goes first, then the branch, the project, and the message count, so an 80-column all-projects row keeps the title, project, count, and time, and a narrow row keeps only the title and the time. Below a 12-column title the time drops too, so the title always shows. Relative times count from when the listing last started, on open or on a Tab or Ctrl+A reload, so later pages age with the first one, while the Ctrl+D confirmation counts from when it opens.
- Rows load in pages of 30 from a `SessionWalk`, each read by one query on a worker thread that hands the page back to the picker's `Modal::tick`. A row whose session [forked](persistence.md#lazy-fork) from another shows `(fork of {id8})` after its title. The picker asks for the next page after each key while fewer than one viewport of rows passing the filter sits below the cursor, so it loads ahead and typical scrolling never waits on the store, and a query keeps loading pages until it fills the viewport with matches or the walk ends. While pages remain, the cursor stops at the last row in place of wrapping, and a query without matches reads `Searching for ...`. A walk that fails to list the sessions, or a worker that dies without a page, which only a panic does, takes over the footer with a load error, so a failure cannot pass for an empty list.
- Tab toggles current-project ↔ all-projects, and Ctrl+A toggles the active ↔ archived view, retitling the header to `Resume archived session`. Both restart the walk while preserving the typed query, and the old view's page is discarded when its worker finishes.
- Enter on a focused row submits, while Enter with no selection keeps the picker open so the user can Tab the scope or Esc out.
- Esc dismisses through the keymap, since the picker captures it, and Ctrl+C cancels via the universal stack gate.
- The footer leads with the Enter and Esc hints, then names the scope, then lists the other actions whose labels apply. Keeping the scope beside Esc leaves it visible on a medium-width footer. Labels follow the view, so the Ctrl+X action reads as archiving or unarchiving. The row actions' hints hide while no row is highlighted, and the Ctrl+U hint shows only while a query is typed.
- Ctrl+D on the cursor row pushes [`ConfirmDeleteSessionModal`](modals.md). When the engine reports the delete through `Modal::on_engine`, the picker drops that row and the row that followed takes the highlight, while a cancelled or failed delete leaves the rows and the cursor in place.
- Ctrl+X archives the cursor row, or unarchives it in the archived view, by sending `Op::SetArchive` with the row's ID, the op the typed `/archive` and `/unarchive` forms send with a prefix. Until `ArchiveUpdated` arrives, the picker ignores Enter, Ctrl+X, and Ctrl+D, while navigation and view changes stay available. When it reports success, the picker drops the row with the cursor index and scroll position kept, so the cursor lands on the row that followed while the visible rows hold still, and the footer names the moved session until the next key or for 3 seconds, the same `NOTICE_DURATION` as the copy notice. A refusal or failed write shows in the footer as an error and leaves the row in place.

`ox --resume` without a session starts the TUI in a fresh session with no row yet, then opens the picker on the first frame. `main.rs` builds it with `ResumePicker::at_startup`, and `ox_tui::run` pushes `Startup::picker` onto the modal stack before the event loop starts. The startup picker carries `Dismissal::Quit`, so Esc / Ctrl+C call `on_cancel`, which resolves to `Op::Shutdown`, and the footer labels that key as quitting. Pressing Enter resumes through the same `roll_into` path as `/resume`. A session's row is created only with the first message, so neither path leaves an empty session in the store.

`session/handle` provides `roll_into(...)`, which mirrors `roll(...)`. On the blocking pool, it first resolves the target to a session ID through `resolver::resolve_target`, which imports a `ResumeTarget::Path` export, then takes the session's liveness lock and loads and sanitizes its chain, so a resolve or load failure leaves the live session untouched. Resuming an archived target unarchives it, and `RollIntoOutcome::prior` reports the target's archive state before that. It then swaps the handle in place and finalizes the old session with the file tracker snapshots the engine passes in. `RollIntoOutcome` carries the target's `LoadedTranscript` (messages, recorded prompt context, compact boundary, tool-result metadata, recorded usage, task list, and file tracker snapshots), its title, and the old session's finalize failure.

`Engine::resume` in `engine/sessions.rs` drives `roll_into` through the live transcript and adopts the loaded transcript, so neither form of the command resolves a prefix or touches the session handle. Adopting it replays the recorded prompt context and builds a fresh file tracker through `FileTracker::restored`, which holds back the paths that drifted on disk. It then emits `Event::SessionResumed { id, title, prior, history }`, followed in order by the old session's finalize failure as an `Event::Error`, an `Event::UsageUpdated` carrying the restored session cost when it is non-zero, the restored task list, an `Event::Notice` for each instruction file the rebuilt prompt cannot read, an `Event::Notice` for any tracker drift, and the interrupted-response `Event::Error` when the transcript ends mid-reply. `Event::SessionReady` ends the resume, while a prefix without a unique match or a `roll_into` failure ends it with `Event::TurnFailed` and keeps the live session. The next API request carries the resumed session's ID in `x-claude-code-session-id` and `metadata.user_id`, and the next tool call runs with its ID and artifact directory, since both read them from the live handle. The history comes from `Engine::take_history`, the same projection the TUI paints at startup. That projection runs the adopted transcript through `session::history::interactions`: the compaction boundary first, then text, thinking, and each tool call paired with its result, with icons, labels, and result views already resolved, so the TUI renders it without a tool registry.

On `SessionResumed`, `apply_session_resumed` rebuilds the chat, clears the modals, and discards the prompts queued before the resume with a notice. At `SessionReady`, the TUI returns to idle and submits the first prompt typed while the resume ran, or only returns to idle after a cancel that lost the race to the swap. A failed resume returns to idle with the queue intact and held, and a retried resume that lands keeps the prompts typed during the failed attempt ([**A cancel leaves the queue to the user**](cancellation.md#design-decisions)).

`SessionStore::walk` returns a `SessionWalk` over the sessions in a `ListScope`, most recently active first, and each `next_page` is one query that continues after the last row's `updated_at` and ID, so pages never overlap. `SessionStore::list_paged` takes one page, asking for one session past it, and `ListPage` carries the page and whether more matching sessions follow it. `SessionStore::list_matching` looks sessions up by ID prefix in either archive state, and resume, archive, and delete act on the ID it returns. The picker and the startup picker list from the store the process opened at startup. `ox --list --limit N` (default `30`, `0` = unbounded) prints ``... more (use `--limit N` to raise the cap or `--limit 0` to list all)`` when more sessions follow the cap.

## Design Decisions

1. **Bare opens the picker, typed-arg resolves directly.** Same shape as `/model`, `/effort`, `/theme`: bare opens a modal, typed-arg switches directly. Reference CLIs converged on this exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).

2. **Alias `continue`.** Claude Code's `/resume` also takes `continue` as an alias, so users of either tool can type the word they know.

3. **Both forms are idle-only.** Bare and typed-arg refuse mid-turn, since the picker eventually submits an `Op::Resume`, and allowing it mid-stream would interleave a session swap with an in-flight response.

4. **Mid-session re-init uses `roll_into`.** The in-process swap preserves the process and terminal state, avoids an alt-screen exit or relaunch flash, and reuses the `roll` pattern from `/clear`. It clears modals, rolls back any theme preview, and discards the prompts queued before it.

5. **`RollIntoOutcome` nests the `LoadedTranscript`.** The engine adopts a loaded transcript as one unit, at startup from `ResumedSession` and on `/resume` from `RollIntoOutcome`, so both carry the same struct. The title, the `prior` archive state, and `finalize_failure` sit beside it, since the engine reads each one separately. The tracker snapshots travel inside the transcript, so startup and `/resume` both restore the tracker in the engine, and every frontend gets the drift notice.

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

- `crates/ox-core/src/command.rs`: `resume`, the typed form shared with the bare REPL.
- `crates/ox-core/src/engine.rs`: `Engine::take_history`.
- `crates/ox-core/src/engine/sessions.rs`: `Engine::resume`, `Engine::roll_into`, `format_drift_warning`.
- `crates/ox-core/src/protocol.rs`: `Op::Resume`, `ResumeTarget`, `Event::SessionResumed`, `Interaction`.
- `crates/ox-core/src/session/handle.rs`: `roll_into`, `RollIntoOutcome`, `LoadedTranscript`.
- `crates/ox-core/src/session/history.rs`: `interactions`, the display projection `SessionResumed` carries.
- `crates/ox-core/src/session/resolver.rs`: `resolve_target`, which maps a `ResumeTarget` to a session ID for startup and `session::handle::roll_into`, importing an export path, and `resolve_prefix` and `resolve_id`, shared with the saved-session operations.
- `crates/ox-core/src/session/saved.rs`: `set_archived` behind Ctrl+X, and `delete` behind the confirm-delete modal, both run by the engine.
- `crates/ox-core/src/session/store/listing.rs`: `walk`, `SessionWalk`, `list_paged`, `latest`, `list_matching`, `ListPage`, `ListScope`, `ProjectScope`.
- `crates/ox-core/src/util/text.rs`: `compact_count`, which abbreviates the message count, shared with the status line's token counts.
- `crates/ox-tui/src/app.rs`: `push_modal`.
- `crates/ox-tui/src/app/events.rs`: `apply_session_resumed`.
- `crates/ox-tui/src/components/chat.rs`: `ChatView::load_history`.
- `crates/ox-tui/src/keymap.rs`: `Keymap`, `KeyAction`, and `KeyChord`, behind the picker's keys and footer hints.
- `crates/ox-tui/src/lib.rs`: `Startup::picker`, pushed over the first frame, and `NOTICE_DURATION`, shared by the copy notice and the picker's notice.
- `crates/ox-tui/src/modal/searchable_list.rs`: `SearchableList<T>`, `SearchableItem`, `append_items` and `set_has_more` for paged loading, and `retain_items`, which keeps the cursor when rows drop.
- `crates/ox-tui/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` pushed by Ctrl+D.
- `crates/ox-tui/src/slash/registry.rs`: `BUILT_INS` entry `&ResumeCmd`.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd`, `ResumePicker` and its `at_startup` variant, `PickerAction` and its `ACTIONS` keymap, `Pages`, and `SessionRow` with its `Column` layout.
- `crates/ox-tui/src/slash/sessions.rs`: `format_short_relative_time` for the picker row's time column, and `format_metadata_line` for the delete confirmation.
- `crates/ox/src/driver/stdio.rs`: `StdioSink`, which confirms a resume on stderr.
- `crates/ox/src/main.rs`: `Cli::archived`, `Cli::limit`, `Cli::resume`.
- `crates/ox/src/session_list.rs`: the `ox --list` table over `list_paged`.
