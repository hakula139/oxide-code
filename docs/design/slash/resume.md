# Session Resume

`/resume` (alias `/continue`) opens a searchable session picker when bare, or jumps directly when given a session ID. Shares its listing budget with `ox --list`. At startup, `ox --resume` opens the same picker.

Companions: [commands.md](commands.md), [modals.md](modals.md). Underlying research: [research/slash/resume.md](../../research/slash/resume.md).

## Implementation

`slash/resume` hosts both `ResumeCmd` and `ResumePicker`. Bare returns the picker as `SlashOutcome::Modal`, which forwards `Op::Resume` with `ResumeTarget::Path`, the chosen session file. Typed-arg forwards `ResumeTarget::Prefix` through `command::resume`, which the bare REPL runs too, and the engine resolves the prefix through `resolve_saved_prefix` (current project first, widening to all projects on miss).

The picker wraps [`SearchableList`](modals.md) and adds these controls and display rules:

- The picker's named actions come from `ACTIONS`, a [`Keymap`](modals.md) that pairs each action with its key and footer label, and the footer renders its hints from the same entries. Up / Down and PageUp / PageDown, which move the cursor, are handled outside the table, and other keys edit the query.
- Each row paints a two-line title plus dim metadata block (ID prefix · relative time · message count · branch · project) followed by a trailing blank.
- Tab toggles current-project ↔ all-projects, and Ctrl+A toggles the active ↔ archived view, retitling the header to `Resume archived session`. Both reload rows from `SessionStore::list_paged` while preserving the typed query.
- Enter on a focused row submits, while Enter with no selection keeps the picker open so the user can Tab the scope or Esc out.
- Esc dismisses through the keymap, since the picker captures it, and Ctrl+C cancels via the universal stack gate.
- The footer leads with the Enter and Esc hints, then counts the loaded rows and names the scope, then lists the other actions whose labels apply. Keeping the status beside Esc leaves it visible on a medium-width footer. Labels follow the view, such as `Ctrl+X to unarchive` in the archived view. The row actions' hints hide while no row is highlighted, and the Ctrl+U hint shows only while a query is typed. If more sessions follow the 30-row cap, it says so (`30 most recent sessions`, then `more not loaded`), since queries search only loaded rows.
- Ctrl+D on the cursor row pushes [`ConfirmDeleteSessionModal`](modals.md). On focus regain the picker reloads and re-seeks the cursor to the previously selected row, so cancel-delete keeps the user in place. After a delete, the row that followed takes the highlight.
- Ctrl+X archives the cursor row, or unarchives it in the archived view, through `session::saved::set_archived`, the operation the typed `/archive` and `/unarchive` forms also run. The picker then reloads with the cursor index and scroll position kept, so the cursor lands on the row that followed while the visible rows hold still, and the footer names the moved session until the next key. A refusal or failed write shows in the footer as an error and leaves the row in place.

`ox --resume` starts the TUI in a fresh session with no file yet, then opens the picker on the first frame. `main.rs` builds it with `ResumePicker::at_startup`, and `driver::tui::run` pushes `Startup::picker` onto the modal stack before the event loop starts. The startup picker carries `Dismissal::Quit`, so Esc / Ctrl+C call `on_cancel`, which resolves to `Op::Shutdown`, and the footer reads `Esc to quit`. Pressing Enter resumes through the same `roll_into` path as `/resume`. A session file is created only with the first message, so neither path leaves an empty session on disk.

`session/handle` provides `roll_into(...)`, which mirrors `roll(...)`. It loads and sanitizes the target first, so a load failure leaves the live session untouched. Loading an archived target appends an entry unarchiving it, which `RollIntoOutcome::unarchived` reports. It then snapshots and clears the file tracker, restores the target's tracker state through `restore_verified`, swaps the handle in place, and finalizes the old session. `RollIntoOutcome` carries the target's `LoadedTranscript` (messages, recorded prompt context, compact boundary, tool-result metadata, and recorded usage) and title, plus a diagnostic pair (finalize failure of the prior session, drift list from the file tracker).

`Engine::resume` in `engine.rs` resolves the target to a session file, drives `roll_into`, and adopts the loaded transcript, which rebinds `Client::set_session_id` and replays the recorded prompt context. It then emits `Event::SessionResumed { id, title, unarchived, history }`, followed by an `Event::UsageUpdated` carrying the restored session cost when it is non-zero, and surfaces the old session's finalize failure and any tracker-drift warning as distinct `Event::Error`s. The history comes from `Engine::take_history`, the same projection the TUI paints at startup, which runs the adopted transcript through `session::history::interactions`: the compaction boundary first, then text, thinking, and each tool call paired with its result, with icons, labels, and result views already resolved. The TUI's `App::apply_session_resumed` clears the chat, replays the history through `ChatView::load_history`, notes an unarchived session, drops queued prompts (surfaces the count as a system message), and clears the modal stack.

`SessionStore::list_paged` stats every candidate file, sorts by mtime desc, then parses files in order until it holds one readable session in the requested archive state past the page. `ListPage` carries the page and whether more matching sessions follow it. `SessionStore::list_matching` filters the same walk by the ID in each file name, so a prefix lookup parses only the matching files. Each `SessionInfo` carries the file it was read from, and resume and delete act on that file. `ox --list --limit N` (default `30`, `0` = unbounded) prints ``... more (use `--limit N` to raise the cap or `--limit 0` to list all)`` when more sessions follow the cap.

## Design Decisions

1. **Bare opens the picker, typed-arg resolves directly.** Same shape as `/model`, `/effort`, `/theme`: bare opens a modal, typed-arg switches directly. Reference CLIs converged on this exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).

2. **Alias `continue`.** Matches the existing `--continue` / `-c` CLI flag, using the same word in both surfaces.

3. **`classify` is always `Mutating`.** Both bare and typed-arg refuse mid-turn, since the picker eventually submits a `Op::Resume`, and allowing it mid-stream would interleave a session swap with an in-flight response, which is exactly what the read-only fast path was designed to rule out.

4. **Mid-session re-init uses `roll_into`.** The in-process swap preserves the process and terminal state, avoids an alt-screen exit or relaunch flash, and reuses the `roll` pattern from `/clear`. It clears modals, rolls back any theme preview, and discards queued prompts.

5. **`RollIntoOutcome` nests the `LoadedTranscript`.** The engine adopts a loaded transcript as one unit, at startup from `ResumedSession` and on `/resume` from `RollIntoOutcome`, so both carry the same struct. The title, the `unarchived` flag, and the diagnostic fields (`finalize_failure`, `drifted_paths`) sit beside it, since the engine reads each one separately.

6. **Surface old-session finalize failures and tracker drift as distinct errors.** Two failures with two causes: the previous session failing to finalize cleanly (writer), and tracked files drifting since the resumed session (filesystem). Distinct phrasing prevents the user from reading either as a current-writer fault.

7. **Drop queued prompts on resume, then surface the count.** Queued prompts belong to the source thread, so replaying them in a different transcript would amount to silent corruption. `/clear` keeps the queue while starting a new session, but `/resume` drops it and prints `N queued prompt(s) discarded` so the user knows their typing didn't carry over.

8. **`SearchableList` as a sibling primitive to `ListPicker`.** `ListPicker` is small and focused, and bolting search + viewport onto it would double its surface area for the simpler pickers. `SearchableList` sits sibling-style with the same trait pattern and the same delegation.

9. **Two-method `SearchableItem`: `haystack` plus `render`.** A substring filter and a layout callback are the only things the primitive needs from items. Date grouping, numeric mnemonics, and other UX shapes were prototyped and dropped because they bloat the trait for one consumer.

10. **Substring search only.** Codex and opencode both ship plain substring and it works.

11. **Project scoping defaults to current cwd, with Tab widening.** Matches the existing `ox -c` resolver semantics: CLI `--all` widens up front, in-picker `Tab` toggles, and the typed query persists across the rebuild.

12. **Default to a 30-row page with no background load-more.** Codex's near-bottom prefetch is genuinely nice but adds a non-trivial state machine for marginal gain at typical session counts. More than 30 sessions in a project is search territory anyway.

13. **Listing API centralizes pagination in `list_paged(limit, scope)`.** The page, the archive filter, and whether more sessions follow come from one walk that stops one session past the page, so a listing's cost scales with the page. Listings report no exact total, since counting the sessions in one archive state would parse every file in scope. Bare `ox -c` stops at the first unarchived session through `latest`, and a prefix lookup parses only matching files.

14. **`--list --limit 0` opts back into unbounded listing.** Scripts piping output need the full set, so `0` is the explicit unbounded value and no magic large default is needed.

15. **Bare suppresses echo, typed-arg echoes.** Mirrors the universal rule: the modal is the response, so the typed `> /resume` line stays out of history. Typed `> /resume <id>` echoes because the swap-confirmation system message anchors the pair.

16. **Always show the picker, even with one or zero matches.** Auto-resuming on a single match would surprise users who typed `/resume` just to peek the list, and an empty list with a clear message is better than silently doing nothing.

17. **Ctrl+A switches to the archived view.** Archived sessions stay out of the default rows but remain one key away, and resuming one from that view unarchives it, matching `ox -c <prefix>`. The view composes with Tab, so archived sessions can be listed across every project.

18. **`ox --resume` reuses the in-TUI picker, and dismissing it quits.** Opening the picker over a fresh session lets startup share `/resume`'s modal and its in-process swap, avoiding a separate pre-TUI selection screen. Claude Code's `--resume` and Codex's `resume` subcommand both open a picker at startup. Codex starts a fresh session when you press Esc, but plain `ox` already does that, so Esc quits. Esc always keeps its universal modal meaning, even when you've typed a query, and Ctrl+U clears the query. The picker holds focus until it closes, so resume or compaction can't clear the stack and trigger its quitting `on_cancel` while it's open.

19. **One keymap drives keys and hints.** Each named action carries its key and a footer label that can depend on the view, so `handle_key` dispatches through the table that renders the footer, and adding a key adds its hint. The picker captures Esc so dismissal is one of those actions. Status such as the count and scope stays out of the table. [#278](https://github.com/hakula139/oxide-code/issues/278) tracks rebinding these actions, and [#281](https://github.com/hakula139/oxide-code/issues/281) tracks fitting their hints to narrow footers.

20. **Ctrl+X moves a session between views in place.** Clearing out a long list takes one key per session, with no ID prefix to read off and type. Ctrl+X is free among the picker's keys. The filter's `LineEditor` maps it to cutting a selection, which the editor never draws, so the picker shadows it just as it shadows the editor's Ctrl+A (line start), Ctrl+D (delete forward), and Ctrl+U (undo). The highlight moves to the next row, so repeated presses work through a run of sessions.

## Per-Component Notes

- **`ResumeCmd`**: Canonical `resume`, alias `continue`. `classify` is always `Mutating`. `echoes_input` returns false for bare (the modal is the response) and true for typed-arg (echo anchors the swap-confirmation message). Typed-arg path forwards the trimmed prefix through `command::resume` for the engine to resolve.

- **`ResumePicker`**: Wraps `SearchableList<SessionRow>` plus a footer row. Loads rows from `SessionStore::list_paged`. Dispatches keys through `ACTIONS`, whose entries bind each `PickerAction` to a key and label: Enter resumes, Esc dismisses, Ctrl+X archives or unarchives, Ctrl+D opens the delete confirmation, Tab toggles `all`, Ctrl+A toggles `archived`, and Ctrl+U clears the query. The toggles rebuild while preserving the query, and other unbound keys edit it through the shared `LineEditor`. Enter on a focused row dispatches `Op::Resume`. Enter with no selection keeps the picker open so the user can Tab the scope or Esc out. `at_startup` builds the `ox --resume` variant, whose `Dismissal::Quit` turns a dismissal into `Op::Shutdown`.

- **`SearchableList<T>`**: Owns the query editor, items, filtered visible index, cursor, and viewport. The substring filter recomputes whenever the query changes, through `set_query` or an `edit_query` keystroke, and resets the cursor to the top. The viewport sizes itself from the height each render grants, and paging steps by the rows last shown. `SearchableItem::haystack` returns the composite filter source. `render(width, is_cursor, theme)` paints one or more `Line`s per row.

- **`SessionRow`**: Carries the full ID, session file path, title-or-`(untitled)`, `last_active_at`, message count, optional git branch, and (in all-projects mode) the tildified project path. `haystack` joins the ID and title, plus the project when visible. `render` paints a two-line title plus dim metadata block under a fixed width budget. The metadata line is built via the shared `session::display::format_metadata_line` helper so the confirm-delete modal can reuse the same shape.

- **`SessionStore::list_paged`**: Takes a `ListScope` with project and archive flags. Stats every candidate, sorts by mtime desc, then parses files in order, keeping readable sessions in the requested archive state until it holds one past the page. `ListPage::has_more()` reports whether a matching session follows the page, for the `--list` footer and the picker's `more not loaded` note.

- **`/resume <id>` resolver**: `Engine::roll_into` calls `resolve_saved_prefix`, which runs the prefix lookup against the current project, then retries across all projects on miss, skipping the live session. A prefix without a unique match, or one that matches only the live session, fails the resume with a message saying why.

- **`Op::Resume`**: `target` is a `ResumeTarget`, either the `Path` of the session file the picker chose or the ID `Prefix` the user typed. The engine resolves the target and owns the `roll_into` call, so the slash command never touches the session store or handle.

- **`Event::SessionResumed`**: Carries the ID, title, unarchived flag, and transcript projected for display, so the TUI renders it without a tool registry. `StdioSink` prints the resumed ID prefix and title on stderr, plus a note when the resume unarchived the session.

- **`Engine::resume` (`engine.rs`)**: Drives `roll_into`, rebinds `Client::set_session_id`, emits `SessionResumed` and, for a non-zero restored cost, `UsageUpdated`, then surfaces `finalize_failure` and `drifted_paths` as distinct `Event::Error`s. A prefix without a unique match or a `roll_into` failure ends the resume with `Event::TurnFailed` and keeps the live session.

- **`apply_session_resumed` (TUI)**: Swaps the session ID, repaints the title, clears and replays chat history, notes an unarchived session, drops queued prompts (with a system-message surface), clears the modal stack, and resumes idle. While the resume runs, `Status::Resuming` holds queued prompts, and a failed resume returns to idle with them still queued.

- **`--list --limit N`**: `0` opts out of the cap, while positive values cap it. The `... more` footer line appears only when more sessions follow the cap.

## Out of Scope

- **Date grouping in the picker.** Considered (`Today` / `Yesterday` / ISO date) but dropped, because flat mtime list reads fine at 30 rows and the `last_active_at` column already encodes recency.
- **Numeric `1`–`9` mnemonics.** Cursor plus Enter is enough, and mnemonics conflict with type-to-search.
- **Worktree / branch tree expansion** (Claude Code's `▼` / `▶`) and **side preview pane.** Both double layout complexity for marginal gain.
- **In-picker rename of other sessions.** `/rename` covers the live session's title.
- **Background load-more on scroll near bottom.** A 30-row page plus search covers the typical case.
- **Fuzzy search.** Substring search covers typical session counts.
- **Auto-resume on single match.** Users who type `/resume` to peek the list deserve to see it.
- **`/sessions` listing variant.** One slash command per intent. `ox --list` already covers script-friendly listing.

## Sources

- `crates/oxide-code/src/command.rs`: `resume`, the typed form shared with the bare REPL.
- `crates/oxide-code/src/driver/stdio.rs`: `StdioSink`, which confirms a resume on stderr.
- `crates/oxide-code/src/driver/tui.rs`: `Startup::picker`, pushed over the first frame.
- `crates/oxide-code/src/engine.rs`: `Engine::resume`, `Engine::roll_into`, `Engine::take_history`, `format_drift_warning`.
- `crates/oxide-code/src/main.rs`: `Cli::archived`, `Cli::limit`, `Cli::resume`.
- `crates/oxide-code/src/protocol.rs`: `Op::Resume`, `ResumeTarget`, `Event::SessionResumed`, `Interaction`.
- `crates/oxide-code/src/session/display.rs`: shared `format_metadata_line` used by the picker row.
- `crates/oxide-code/src/session/handle.rs`: `roll_into`, `RollIntoOutcome`, `LoadedTranscript`.
- `crates/oxide-code/src/session/history.rs`: `interactions`, the display projection `SessionResumed` carries.
- `crates/oxide-code/src/session/list_view.rs`: `render_list` over `list_paged`.
- `crates/oxide-code/src/session/resolver.rs`: `resolve_saved_prefix`, shared by `Engine::roll_into`, `/archive`, `/unarchive`, and `/delete`, and the `resolve_prefix_to_info` lookup it wraps.
- `crates/oxide-code/src/session/saved.rs`: `set_archived` behind Ctrl+X, and `delete` behind the confirm-delete modal.
- `crates/oxide-code/src/session/store/listing.rs`: `list_paged`, `latest`, `list_matching`, `ListPage`, `ListScope`.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` pushed by Ctrl+D.
- `crates/oxide-code/src/slash/registry.rs`: `BUILT_INS` entry `&ResumeCmd`.
- `crates/oxide-code/src/slash/resume.rs`: `ResumeCmd`, `ResumePicker` and its `at_startup` variant, `PickerAction` and its `ACTIONS` keymap, and `SessionRow`.
- `crates/oxide-code/src/tui/app.rs`: `push_modal`.
- `crates/oxide-code/src/tui/app/events.rs`: `apply_session_resumed`.
- `crates/oxide-code/src/tui/components/chat.rs`: `ChatView::load_history`.
- `crates/oxide-code/src/tui/modal/keymap.rs`: `Keymap`, `KeyAction`, and `KeyChord`, behind the picker's keys and footer hints.
- `crates/oxide-code/src/tui/modal/searchable_list.rs`: `SearchableList<T>` plus `SearchableItem` plus `cursor_to` for cursor preservation across reload.
