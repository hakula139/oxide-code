# Session Resume

`/resume` (alias `/continue`) opens a searchable session picker when bare, or jumps directly when given a session id. Shares listing budget with `ox --list`.

Companions: [commands.md](commands.md), [modals.md](modals.md). Underlying research: [research/slash/resume.md](../../research/slash/resume.md).

## Implementation

`slash/resume` hosts both `ResumeCmd` and `ResumePicker`. Bare opens the picker via `ctx.open_modal`, while typed-arg resolves through `match_in_scope` (current project first, widening to all projects on miss) and forwards `UserAction::Resume { session_id }` carrying the full id.

The picker wraps [`SearchableList`](modals.md) and adds a footer line. Each row paints a two-line title plus dim metadata block (id prefix · relative time · message count · branch · project) followed by a trailing blank. Tab toggles current-project ↔ all-projects, reloading rows from `SessionStore::list_paged` while preserving the typed query. Enter on a focused row submits, while Enter on an empty filter or no selection cancels quietly. Esc / Ctrl+C cancel via the universal stack gate. Ctrl+D / Delete on the cursor row pushes [`ConfirmDeleteSessionModal`](modals.md). On focus regain the picker reloads and re-seeks the cursor to the previously selected row, so cancel-delete keeps the user in place.

`session/handle` gains `roll_into(...)` mirroring `roll(...)`: snapshot the file tracker, clear it, load + sanitize the target, swap the handle in place, finalize the old session. Returns a flat `RollIntoOutcome` carrying the resumed transcript (messages, title, tool-result metadata) plus a diagnostic pair (finalize failure of the prior session, drift list from the file tracker).

The agent loop in `main` adds an `apply_resume` helper: drive `roll_into`, rebind `Client::set_session_id`, emit `AgentEvent::SessionResumed { id, title, messages, tool_metadata }`, then surface the old session's finalize failure and any tracker-drift warning as distinct `AgentEvent::Error`s. The TUI's `App::apply_session_resumed` clears the chat, replays `load_history`, drops queued prompts (surfaces the count as a system message), and clears the modal stack.

`SessionStore::list_paged` is the new entry point: stat every candidate file, sort by mtime desc, truncate to the cap, then `read_session_info` only the survivors. `ListPage` carries the surviving sessions and the pre-truncation total. `list()` / `list_all()` survive as thin wrappers. `ox --list --limit N` (default `30`, `0` = unbounded) prints `... and N more (use --limit to widen)` when the cap clipped output.

## Design Decisions

1. **Bare opens the picker, typed-arg resolves directly.** Same shape as `/model`, `/effort`, `/theme`: bare opens a modal, typed-arg switches directly. Reference CLIs converged on this exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).

2. **Alias `continue`.** Matches the existing `--continue` / `-c` CLI flag, using the same word in both surfaces. Opencode's triple-alias (`/sessions`, `/resume`, `/continue`) is noise, and `sessions` would hint at a multi-action verb (list / rename / delete) that doesn't exist.

3. **`classify` is always `Mutating`.** Both bare and typed-arg refuse mid-turn, since the picker eventually submits a `UserAction::Resume`, and allowing it mid-stream would interleave a session swap with an in-flight response, which is exactly what the read-only fast path was designed to rule out.

4. **Mid-session re-init uses `roll_into`.** `Command::exec` would kill the modal stack, queued prompts, live theme preview, and in-flight tool tracker. The in-process swap preserves those surfaces and reuses the `roll` pattern `/clear` already validated.

5. **`RollIntoOutcome` is flat.** Nesting a `ResumedSession` field would add a wrapper layer for no consumer. The agent loop reads each field independently, so diagnostic fields (`finalize_failure`, `drifted_paths`) sit alongside the transcript fields rather than under a sub-struct.

6. **Surface old-session finalize failures and tracker drift as distinct errors.** Two failures with two causes: the previous session failing to finalize cleanly (writer), and tracked files drifting since the resumed session (filesystem). Distinct phrasing prevents the user from reading either as a current-writer fault.

7. **Drop queued prompts on resume, then surface the count.** Queued prompts belong to the source thread, so replaying them in a different transcript would amount to silent corruption. `/clear` keeps the queue (same identity, fresh slate), but `/resume` drops it and prints `N queued prompt(s) discarded` so the user knows their typing didn't carry over.

8. **New primitive `SearchableList` instead of extending `ListPicker`.** `ListPicker` is small and focused, and bolting search + viewport onto it would double its surface area for the simpler pickers. `SearchableList` sits sibling-style with the same trait pattern and the same delegation, and if a third pattern shows up the abstraction can fold. Until then, two narrow primitives beat one wide one.

9. **Two-method `SearchableItem`: `haystack` plus `render`.** A substring filter and a layout callback are the only things the primitive needs from items. Date grouping, numeric mnemonics, and other UX shapes were prototyped and dropped because they bloat the trait for one consumer.

10. **Substring search only in v1.** Codex and opencode both ship plain substring and it works. `nucleo-matcher` is a small dep with a clean upgrade path if users complain.

11. **Project scoping defaults to current cwd, with Tab widening.** Matches the existing `ox -c` resolver semantics: CLI `--all` widens up front, in-picker `Tab` toggles, and the typed query persists across the rebuild.

12. **30-row default page, no background load-more in v1.** Codex's near-bottom prefetch is genuinely nice but adds a non-trivial state machine for marginal gain at typical session counts. More than 30 sessions in a project is search territory anyway.

13. **Listing API consolidates `list()` / `list_all()` into `list_paged(limit, all)`.** Existing wrappers stay for back-compat. The win is cap-before-tail-scan ordering: for 1000 sessions, only the first N files get parsed.

14. **`--list --limit 0` opts back into unbounded listing.** Scripts piping output need the full set, so `0` is the explicit opt-out rather than a magic large default.

15. **Bare suppresses echo, typed-arg echoes.** Mirrors the universal rule: the modal IS the response, so the typed `> /resume` line stays out of history. Typed `> /resume <id>` echoes because the swap-confirmation system message anchors the pair.

16. **Always show the picker, even with one or zero matches.** Auto-resuming on a single match would surprise users who typed `/resume` just to peek the list, and an empty list with a clear message beats silently doing nothing.

## Per-Component Notes

- **`ResumeCmd`**: Canonical `resume`, alias `continue`. `classify` is always `Mutating`. `echoes_input` returns false for bare (modal IS the response) and true for typed-arg (echo anchors the swap-confirmation message). Typed-arg path validates non-empty / non-whitespace, then calls `match_in_scope` against the current project first and widens on miss.

- **`ResumePicker`**: Wraps `SearchableList<SessionRow>` plus a footer row. Loads rows from `SessionStore::list_paged`. Tab toggles `all` and rebuilds while preserving the query. Enter on a focused row dispatches `UserAction::Resume`. Empty submit or no rows cancel quietly so the user can Tab the scope or Esc out.

- **`SearchableList<T>`**: Owns query, items, filtered visible index, cursor, and viewport offset. Substring filter recomputes on every `set_query`, and the cursor clamps to filtered bounds. `SearchableItem::haystack` returns the composite filter source. `render(width, is_cursor, theme)` paints one or more `Line`s per row.

- **`SessionRow`**: Carries the 8-char id prefix, full id, title-or-`(untitled)`, `last_active_at`, message count, optional git branch, and (in all-projects mode) the tildified project path. `haystack` joins id + title plus project when visible. `render` paints a two-line title plus dim metadata block under a fixed width budget. The metadata line is built via the shared `session::display::format_metadata_line` helper so the confirm-delete modal can reuse the same shape.

- **`SessionStore::list_paged`**: Stats every candidate, sorts by mtime desc, truncates to the cap, then `read_session_info`'s the survivors. `ListPage::total()` returns the pre-truncation count for the `... and N more` footer.

- **`/resume <id>` resolver**: `match_in_scope` runs the prefix lookup against the current project, then retries across all projects on miss. Returns the full session id or a user-readable error.

- **`UserAction::Resume`**: Single `session_id` field. The agent loop owns the `roll_into` call, so the slash command never touches the session handle directly.

- **`AgentEvent::SessionResumed`**: Carries id + title + messages + tool_metadata. App-only reaction. `StdioSink` ignores it (TUI-only event, like `SessionRolled`).

- **`apply_resume` (agent loop)**: Drives `roll_into`, rebinds `Client::set_session_id`, emits `SessionResumed`, then surfaces `finalize_failure` and `drifted_paths` as distinct `AgentEvent::Error`s. Channel-closed on the resume event logs a desync warning, since the TUI is then stuck on the old chat.

- **`apply_session_resumed` (TUI)**: Swaps the session id, repaints the title, clears and replays chat history, drops queued prompts (with a system-message surface), clears the modal stack, and resumes idle.

- **`--list --limit N`**: `0` opts out of the cap, while positive values cap it. The footer line `... and N more (use --limit to widen)` appears only when the cap clipped output.

## Out of Scope / Deferred

- **CLI `--resume` / `-r` flag.** Picker entry from the shell makes sense, but it doesn't block the slash work and adds a new clap arm plus alt-screen run mode. Worth its own PR.
- **Date grouping in the picker.** Considered (`Today` / `Yesterday` / ISO date) but dropped, because flat mtime list reads fine at 30 rows and the `last_active_at` column already encodes recency.
- **Numeric `1`–`9` mnemonics.** Cursor plus Enter is enough, and mnemonics conflict with type-to-search.
- **Worktree / branch tree expansion** (Claude Code's `▼` / `▶`) and **side preview pane.** Both double layout complexity for marginal gain.
- **Custom-title rename in-picker.** Sessions get an AI-generated title shortly after the first prompt, and user-settable titles are a separate feature.
- **Background load-more on scroll near bottom.** 30-row page plus search covers the typical case. Add when a user complaint shows a >30-session-in-one-project workflow.
- **Fuzzy search.** Substring is enough for v1, and `nucleo-matcher` is the upgrade path if users complain.
- **Auto-resume on single match.** Users who type `/resume` to peek the list deserve to see it.
- **`/sessions` listing variant.** One slash command per intent. `ox --list` already covers script-friendly listing.

## Sources

- `crates/oxide-code/src/agent/event.rs`: `UserAction::Resume`, `AgentEvent::SessionResumed`.
- `crates/oxide-code/src/main.rs`: `apply_resume`, `format_drift_warning`, `Cli::limit`.
- `crates/oxide-code/src/session/display.rs`: shared `format_metadata_line` used by the picker row.
- `crates/oxide-code/src/session/handle.rs`: `roll_into`, `RollIntoOutcome`.
- `crates/oxide-code/src/session/list_view.rs`: `render_list` over `list_paged`.
- `crates/oxide-code/src/session/resolver.rs`: `normalize_resume_arg` reused unchanged.
- `crates/oxide-code/src/session/store.rs`: `list_paged`, `ListPage`, plus `list` / `list_all` wrappers.
- `crates/oxide-code/src/slash/confirm.rs`: `ConfirmDeleteSessionModal` pushed by Ctrl+D / Delete.
- `crates/oxide-code/src/slash/registry.rs`: `BUILT_INS` adds `&ResumeCmd`.
- `crates/oxide-code/src/slash/resume.rs`: `ResumeCmd`, `ResumePicker`, `SessionRow`, `match_in_scope`.
- `crates/oxide-code/src/tui/app.rs`: `apply_session_resumed`.
- `crates/oxide-code/src/tui/modal/searchable_list.rs`: `SearchableList<T>` plus `SearchableItem` plus `cursor_to` for cursor preservation across reload.
