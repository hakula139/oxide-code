# Session Resume

`/resume` (alias `/continue`) opens a searchable paged session picker when bare, or jumps directly by session ID / prefix. Bare `ox --resume` opens the same picker at startup. [Slash Commands](slash-commands.md) and [Modals](modals.md) cover dispatch and the modal stack.

## Resume Surfaces

`slash/resume` hosts `ResumeCmd` and `ResumePicker`. Bare `/resume` opens the picker (`Op::Resume` with `SessionRef::Id`), while typed `/resume <prefix>` parses through `command::resume` (`SessionRef::Prefix`). `resolve_prefix` searches the current project first, widens to all projects on a miss, and rejects matching only the live session. Startup `ox -r <file>` imports exports in `resolve_session` before reaching the engine.

Bare `ox --resume` starts the TUI on a fresh session with no row yet and pushes `ResumePicker::at_startup` before the event loop (quitting on dismiss, resuming via the same swap on Enter, leaving no empty DB row). Both command forms follow [**A command suppresses its echo when its response stands alone**](slash-commands.md#design-decisions).

## The Picker

`ResumePicker` wraps [`SearchableList`](modals.md#shared-primitives) with keys driven by `ACTIONS` (**One keymap drives keys and hints**, user controls in [Resuming a Session](../guide/sessions.md#resuming-a-session)).

Rows load in 30-row pages from `SessionWalk` on a background worker via app events, continuing until the viewport fills or the walk ends (**Load pages in the background as the cursor nears the end**). Toggling scope (Tab) or archived view (Ctrl+A) restarts the walk preserving the query. Numbered request IDs drop stale pages, and walk errors or dead workers display a load error in the footer so failures never look empty.

Each row displays ID prefix, title (with `(fork of {id8})` suffix on [forks](persistence.md#lazy-fork)), branch, project (in all-projects view), message count, and relative time. Narrow rows drop other columns (ID first) to keep at least 24 columns for the title, then drop time below 12.

Ctrl+X sends `Op::SetArchive` for the highlighted row (same op as typed `/archive` / `/unarchive`), and Ctrl+D pushes confirm-delete. While an archive toggle is pending, Enter, Ctrl+X, and Ctrl+D are ignored. Success removes the row in place keeping the cursor index (**Ctrl+X moves a session between views in place**), and failure keeps the row with a footer error.

## The Swap

`session::handle::roll_into` mirrors `/clear`'s `roll` on the blocking pool: `resolver::resolve_ref` resolves the target (refusing the live session), `handle::resume` acquires its liveness lock, loads and sanitizes its chain (leaving the live session untouched on error), unarchives archived targets, swaps the handle in place, and finalizes the old session, returning `RollIntoOutcome` (`LoadedTranscript`, title, prior archive state, old finalize failure).

`Engine::resume` adopts `LoadedTranscript` identically to startup: replays prompt context, restores the file tracker, emits `Event::SessionResumed` with pre-resolved `engine::history::interactions` (so the TUI needs no tool registry), then emits any old finalize error, restored cost and tasks, unreadable instruction warnings, tracker drift notices, an interrupted-reply marker, and `Event::OpEnded`. Resolve or load failures emit `Event::TurnFailed` and leave the live session active. Later requests and tool calls read session ID and artifact dir from the live handle.

On `SessionResumed`, the TUI rebuilds chat, clears modals, and discards pre-resume queued prompts with a notice, submitting any prompt typed during the swap once `OpEnded` arrives. Failed resumes leave the queue intact and held ([**A cancel leaves the queue to the user**](cancellation.md#design-decisions)).

## Design Decisions

1. **Bare opens the picker, typed-arg resolves directly.** Matches `/model`, `/effort`, `/theme`, Claude Code (`-r` / `-c`), and Codex (`resume` / `resume <id>`).
2. **Alias `continue`.** Matches Claude Code's `/resume` alias.
3. **Both forms are idle-only.** Prevents interleaving `Op::Resume` with an in-flight stream.
4. **Mid-session re-init uses `roll_into`.** Swapping in process preserves terminal state with no alt-screen flash while reusing `/clear`'s roll path, clearing modals, reverting theme previews, and discarding prior queued prompts.
5. **`RollIntoOutcome` nests the `LoadedTranscript`.** Sharing one struct across startup (`ResumedSession`) and `/resume` (`RollIntoOutcome`) keeps transcript adoption, tracker restoration, and drift notices unified while keeping title, `prior` archive state, and `finalize` report separate for the engine.
6. **Surface old-session finalize failures and tracker drift as distinct events.** Finalize errors name the previous session so they are not misread as current-writer faults, while drift is a notice because Read-before-Edit handles it cleanly.
7. **Drop queued prompts on resume, then surface the count.** Prevents replaying prompts into the wrong thread on `/resume`, `/clear`, and bare `/archive`, printing `Discarded N queued prompts from the previous session` ([**A cancel leaves the queue to the user**](cancellation.md#design-decisions)).
8. **`SearchableList` as a sibling primitive to `ListPicker`.** Keeps `ListPicker` minimal for fixed rosters while sharing the same delegation pattern.
9. **Project scoping defaults to the current project, with Tab widening.** Mirrors `ox -c` / `--all` while preserving the search query across toggles.
10. **Load pages in the background as the cursor nears the end.** Fetching ahead when one viewport of matches remains hides paging latency and keeps disk reads off the render loop.
11. **Keyset pages through `SessionWalk`.** Ordered by `(updated_at, id)` past the previous page's last row so pages never overlap (concurrent cross-process writes may duplicate or skip a row), fetching one extra row for `has_more` with no exact total. Current-project queries and prefix lookups scan `sessions_by_project` `(project_root, updated_at)`, while all-projects pages scan and sort the table. `ox -c` reads one row via `latest`.
12. **`--list --limit 0` opts back into unbounded listing.** Gives scripts an explicit unbounded value without a magic cap.
13. **Always show the picker, even with one or zero matches.** Prevents surprising auto-resumes when peeking and gives an explicit empty state.
14. **Ctrl+A switches to the archived view.** Keeps archived sessions out of default rows yet one key away (composing with Tab), and resuming unarchives them like `ox -r <prefix>`.
15. **`ox --resume` without a session reuses the in-TUI picker, and dismissing it quits.** Shares `/resume`'s modal and swap without a separate pre-TUI screen (like Claude Code `--resume` and Codex `resume`), quitting on Esc since plain `ox` already starts fresh. Esc always dismisses while Ctrl+U clears the query, and focus prevents stack clears from firing its quitting `on_cancel`.
16. **One keymap drives keys and hints.** Unifies `handle_key` and dynamic footer hints, including captured Esc (excluding scope status). Rebinding is tracked in [#278](https://github.com/hakulab/oxide-code/issues/278) and narrow-footer fitting in [#281](https://github.com/hakulab/oxide-code/issues/281).
17. **Ctrl+X moves a session between views in place.** Enables rapid one-key cleanup with the cursor advancing to the next row, shadowing `LineEditor`'s unused selection cut (alongside Ctrl+A, Ctrl+D, and Ctrl+U).

## Out of Scope

- **Date grouping in the picker.** Relative times on a recency-sorted list already encode recency.
- **Numeric `1`–`9` mnemonics.** Conflict with type-to-search while cursor selection and Enter suffice.
- **Worktree or branch tree expansion.** Claude Code's `▼` / `▶` tree and preview pane double layout complexity for marginal gain.
- **In-picker rename of other sessions.** `/rename` covers the live session.
- **Fuzzy or agentic AI search.** Substring matching fits typical session counts without extra model requests.
- **`/sessions` listing variant.** `ox --list` already serves scriptable listing.

## Sources

- `crates/ox-core/src/engine/history.rs`: `interactions`, the display projection `SessionResumed` carries.
- `crates/ox-core/src/engine/sessions.rs`: `Engine::resume`, `format_drift_warning`.
- `crates/ox-core/src/engine/transcript.rs`: `LiveTranscript::roll_into`, which swaps the live transcript for the loaded one.
- `crates/ox-core/src/session/handle.rs`: `roll_into`, `RollIntoOutcome`, `LoadedTranscript`.
- `crates/ox-core/src/session/resolver.rs`: `SessionTarget`, the target `--continue` or `--resume` names, `resolve_session`, which imports an export path, `resolve_ref`, which maps a `SessionRef` to a session ID, and `resolve_prefix`.
- `crates/ox-core/src/session/store/listing.rs`: `walk`, `SessionWalk`, `list_paged`, `latest`, `list_matching`, `ListPage`, `ListScope`, `ProjectScope`.
- `crates/ox-tui/src/modal/searchable_list.rs`: `SearchableList<T>`, `SearchableItem`, `append_items` and `set_has_more` for paged loading, and `retain_items`, which keeps the cursor when rows drop.
- `crates/ox-tui/src/slash/resume.rs`: `ResumeCmd`, `ResumePicker` and its `at_startup` variant, `PickerAction` and its `ACTIONS` keymap, `Pages`, and `SessionRow` with its `Column` layout.
