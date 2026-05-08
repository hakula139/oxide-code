# Session Resume

`/resume` (alias `/continue`) — bare opens a searchable session picker; typed-arg jumps directly. Shares listing budget with `ox --list`.

Companions: [commands.md](commands.md), [modals.md](modals.md). Underlying research: [research/slash/resume.md](../../research/slash/resume.md).

## Implementation

[`crates/oxide-code/src/slash/resume.rs`](../../../crates/oxide-code/src/slash/resume.rs) hosts both `ResumeCmd` and `ResumePicker`. Bare opens the picker via `ctx.open_modal`; typed-arg resolves through `match_in_scope` (current project first, widen to all projects on miss) and forwards `UserAction::Resume { session_id }` carrying the full id.

The picker composes [`SearchableList<SessionRow>`](../../../crates/oxide-code/src/tui/modal/searchable_list.rs) + a single-line search input + a key-hint footer. Each row paints `id-prefix · timestamp · title`. Tab toggles current-project ↔ all-projects, reloading rows from `SessionStore::list_paged` while preserving the typed query. Enter submits the focused row; empty / no-row submit cancels; Esc / Ctrl+C cancel via the universal stack gate.

[`crates/oxide-code/src/session/handle.rs`](../../../crates/oxide-code/src/session/handle.rs) gains `roll_into(...)` mirroring `roll(...)`: snapshot the file tracker, clear it, load + sanitize the target, swap the handle in place, finalize the old session. Returns a flat [`RollIntoOutcome`](../../../crates/oxide-code/src/session/handle.rs) — `messages`, `title`, `tool_result_metadata`, plus the diagnostic pair `finalize_failure` + `drifted_paths`.

[`crates/oxide-code/src/main.rs`](../../../crates/oxide-code/src/main.rs)'s agent loop adds an `apply_resume` helper: drive `roll_into`, rebind `Client::set_session_id`, emit `AgentEvent::SessionResumed { id, title, messages, tool_metadata }`, then surface the old session's finalize failure and any tracker-drift warning as distinct `AgentEvent::Error`s. The TUI's `App::apply_session_resumed` clears the chat, replays `load_history`, drops queued prompts (surfaces the count as a system message), and clears the modal stack.

`SessionStore::list_paged(limit, all) -> ListPage` is the new entry point. It stats every candidate file, sorts by mtime desc, truncates, then runs `read_session_info` only on the survivors. `ListPage` exposes `sessions()` / `into_sessions()` and `total()` (the pre-truncation count). `list()` / `list_all()` survive as thin wrappers. `ox --list --limit N` (default `30`, `0` = unbounded) prints `... and N more (use --limit to widen)` when the cap clipped output.

## Design Decisions

1. **Bare opens the picker; typed-arg resolves directly.** Same shape as `/model`, `/effort`, `/theme` — bare → modal, typed-arg → switch. Reference CLIs converged on this exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).
2. **Alias `continue`, not `sessions`.** Matches the existing `--continue` / `-c` CLI flag — same word in both surfaces. Opencode's triple-alias (`/sessions`, `/resume`, `/continue`) is noise. `sessions` would also hint at a multi-action verb (list / rename / delete) that doesn't exist.
3. **`classify` is always `Mutating`.** Bare and typed-arg both refuse mid-turn. The picker eventually submits a `UserAction::Resume`, so allowing it mid-turn would interleave a session swap with an in-flight stream — the read-only fast path is structurally wrong here.
4. **Mid-session re-init via `roll_into`, not process replacement.** `Command::exec` would kill the modal stack, queued prompts, live theme preview, and the in-flight tool tracker. In-process swap is the only honest answer; reuses the `roll` pattern `/clear` already validated.
5. **`RollIntoOutcome` is flat.** Nesting a `ResumedSession` field added a wrapper layer for no consumer; the agent loop reads each piece independently. Diagnostic fields (`finalize_failure`, `drifted_paths`) sit alongside the transcript fields for the same reason.
6. **Surface old-session finalize failures and tracker drift as distinct errors.** Two failures with two causes: the previous session failing to finalize cleanly (writer), and tracked files drifting since the resumed session (filesystem). Distinct phrasing prevents the user from reading either as a current-writer fault.
7. **Drop queued prompts on resume; surface the count.** Queued prompts belong to the source thread — replaying them in a different transcript is silent corruption. `/clear` keeps queued prompts (same identity, fresh slate); `/resume` drops them and prints `N queued prompt(s) discarded` so the user knows their typing didn't carry over.
8. **New primitive `SearchableList`, not extended `ListPicker`.** `ListPicker` is small and focused; bolting search + viewport onto it doubles its surface area for the simpler pickers. `SearchableList` sits sibling-style — same trait pattern, same `render_row` delegation. If a third pattern shows up, the abstraction can fold; until then, two narrow primitives beat one wide one.
9. **Two-method `SearchableItem`: `haystack` + `render_row`.** Substring filter and a layout callback are the only things the primitive needs from items. Date grouping, numeric mnemonics, and other UX shapes were prototyped and dropped — they bloat the trait for one consumer.
10. **Substring search, no fuzzy in v1.** Codex and opencode both ship plain substring and it works. `nucleo-matcher` is a small dep with a clean upgrade path if users complain.
11. **Project scoping defaults to current cwd; Tab widens.** Matches the existing `ox -c` resolver semantics. CLI `--all` widens up front; in-picker `Tab` toggles, and the typed query persists across the rebuild — the user's filter is theirs, not the scope's.
12. **30-row default page; no background load-more in v1.** Codex's near-bottom prefetch is genuinely nice but adds a non-trivial state machine for marginal gain at typical session counts. >30 sessions in a project is search territory.
13. **Listing API consolidates `list()` / `list_all()` into `list_paged(limit, all)`.** Existing wrappers stay for back-compat. The win is cap-before-tail-scan ordering — for 1000 sessions, only the first N files get parsed.
14. **`--list --limit 0` opts back into unbounded listing.** Scripts piping output need the full set; `0` is the explicit opt-out, not a magic large default.
15. **Modal-only echoing on bare; typed-arg echoes.** Mirrors the universal rule: the modal IS the response, so the typed `> /resume` line stays out of history. Typed `> /resume <id>` echoes — the swap-confirmation system message anchors the pair.
16. **Always show the picker, even with one or zero matches.** Auto-resuming on a single match would surprise users who typed `/resume` to peek the list; an empty list with a clear message beats silently doing nothing.

## Per-Component Notes

- **`ResumeCmd`** — Canonical `resume`, alias `continue`. `classify` is always `Mutating`. `echoes_input` returns false for bare (modal IS the response) and true for typed-arg (echo anchors the swap-confirmation message). Typed-arg path validates non-empty / non-whitespace, then calls `match_in_scope` against `current-project` first and widens on miss.
- **`ResumePicker`** — Wraps `SearchableList<SessionRow>` plus the search input and footer. Loads rows from `SessionStore::list_paged(Some(limit), all)`. Tab toggles `all` and rebuilds while preserving the query. Enter on a focused row dispatches `UserAction::Resume`; empty submit / no rows cancel quietly so the user can Tab the scope or Esc out.
- **`SearchableList<T>`** — Owns `query`, `items`, `visible: Vec<usize>`, `cursor`, and viewport offset. Substring filter recomputes on every `set_query`; cursor clamps to visible bounds. `SearchableItem::haystack` returns the composite filter source (id + title + cwd); `render_row(width, is_cursor, theme)` paints one `Line`.
- **`SessionRow`** — Carries the 8-char id prefix, full id, title-or-`(untitled)`, tildified cwd, and `last_active_at`. `haystack` joins the searchable fields; `render_row` paints id · timestamp · title under a fixed prefix width.
- **`SessionStore::list_paged`** — `(limit: Option<usize>, all: bool) -> Result<ListPage>`. Builds a `Vec<(PathBuf, OffsetDateTime)>`, sorts by mtime desc, truncates to `limit`, then `read_session_info`'s the survivors. `ListPage::total()` returns the pre-truncation count for the `... and N more` footer.
- **`/resume <id>` resolver** — `match_in_scope(store, prefix, live_id, all=false)` runs the prefix lookup against the current project; on miss, retries with `all=true`. Returns the full session id or a user-readable error.
- **`UserAction::Resume`** — Single field `session_id: String`. The agent loop owns the `roll_into` call; the slash command never touches the session handle directly.
- **`AgentEvent::SessionResumed`** — Carries id + title + messages + tool_metadata. App-only reaction; `StdioSink` ignores it (TUI-only event, like `SessionRolled`).
- **`apply_resume` (agent loop)** — Drives `roll_into`, rebinds `Client::set_session_id`, emits `SessionResumed`, then surfaces `finalize_failure` and `drifted_paths` as distinct `AgentEvent::Error`s. Channel-closed on the resume event logs a desync warning — the TUI is then stuck on the OLD chat.
- **`apply_session_resumed` (TUI)** — Swaps the session id, repaints the title, clears + replays chat history, drops queued prompts (with a system-message surface), clears the modal stack, and resumes idle.
- **`--list --limit N`** — `0` opts out of the cap; positive values cap. Footer line `... and N more (use --limit to widen)` only when the cap clipped output.

## Out of Scope / Deferred

- **CLI `--resume` / `-r` flag.** Picker entry from the shell makes sense; not blocking the slash work and adds a new clap arm + alt-screen run mode. Worth its own PR.
- **Date grouping in the picker.** Considered (`Today` / `Yesterday` / ISO date) but dropped — flat mtime list reads fine at 30 rows, and the `last_active_at` column already encodes recency.
- **Numeric `1`–`9` mnemonics.** Cursor + Enter is enough; mnemonics conflict with type-to-search.
- **Worktree / branch tree expansion** (Claude Code's `▼` / `▶`) and **side preview pane.** Both double layout complexity for marginal gain.
- **Custom-title rename in-picker.** Sessions get an AI-generated title shortly after the first prompt; user-settable titles are a separate feature.
- **Background load-more on scroll near bottom.** 30-row page + search covers the typical case. Add when a user complaint shows a >30-session-in-one-project workflow.
- **Fuzzy search.** Substring is enough for v1; `nucleo-matcher` is the upgrade path.
- **Auto-resume on single match.** Users who type `/resume` to peek the list deserve to see it.
- **`/sessions` listing variant.** One slash command per intent; `ox --list` covers script-friendly listing.

## Sources

- `crates/oxide-code/src/slash/resume.rs` — `ResumeCmd`, `ResumePicker`, `SessionRow`, `match_in_scope`.
- `crates/oxide-code/src/tui/modal/searchable_list.rs` — `SearchableList<T>` + `SearchableItem`.
- `crates/oxide-code/src/session/store.rs` — `list_paged`, `ListPage`; `list` / `list_all` wrappers.
- `crates/oxide-code/src/session/handle.rs` — `roll_into`, `RollIntoOutcome`.
- `crates/oxide-code/src/session/resolver.rs` — `normalize_resume_arg` reused unchanged.
- `crates/oxide-code/src/session/list_view.rs` — `render_list` uses `list_paged`.
- `crates/oxide-code/src/agent/event.rs` — `UserAction::Resume`, `AgentEvent::SessionResumed`.
- `crates/oxide-code/src/main.rs` — `apply_resume`, `format_drift_warning`; `Cli::limit`.
- `crates/oxide-code/src/tui/app.rs` — `apply_session_resumed`.
- `crates/oxide-code/src/slash/registry.rs` — `BUILT_INS` adds `&ResumeCmd`.
