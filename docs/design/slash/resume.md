# Session Resume

`/resume` slash command + searchable session picker, mid-session re-init, and a paginated listing API shared with `ox --list`.

Companion: [commands.md](commands.md) — slash-command registry. Underlying research: [research/slash/resume.md](../../research/slash/resume.md).

## Goals

Three intents the user has:

1. **Browse and pick.** Bare `/resume` opens a searchable picker over recent sessions; pick one, jump to it.
2. **Direct jump.** `/resume <id-prefix>` resolves the same way `ox -c <prefix>` does — alias the existing CLI semantics into slash.
3. **Bulk listing.** `ox --list` is a script-friendly text view of the same data; today it dumps every session unbounded, which scales poorly.

The picker is also where the modal infrastructure ([modals.md](modals.md)) earns its second user-facing primitive after `ListPicker`. Resume needs three things `ListPicker` can't provide — search input, scrollable viewport, multi-column rows — so it gets its own primitive.

## Implementation

### Slash command

[`crates/oxide-code/src/slash/resume.rs`](../../../crates/oxide-code/src/slash/resume.rs):

- `ResumeCmd` implements `SlashCommand`. Canonical name `resume`; alias `continue`. Mirrors the existing `--continue` CLI flag terminology.
- `classify(args)`: bare → `SlashKind::ReadOnly` (picker is read-only until Enter); typed-arg → `SlashKind::Mutating` (forwards `UserAction::Resume`).
- `complete_arg(prefix)`: top 9 sessions in this project, sorted by recency, with title preview. Substring filter on session id / title.
- `execute(args)`:
  - empty → open `ResumePicker` modal seeded from `SessionStore::list_paged(limit, all=false)`.
  - non-empty → resolve via the same `normalize_resume_arg` + project listing as the CLI; on success, return `SlashOutcome::Forward(UserAction::Resume { session_id })`.

Error cases match the CLI: empty / whitespace prefix → "use bare `/resume` to open the picker"; ambiguous prefix → list of matching ids; no match → "no session matching prefix".

### Picker modal

`ResumePicker` lives in the same `slash/resume.rs` file. It composes:

- A search input (one row, single-line, prefix glyph + cursor).
- A scrollable list viewport (variable height, sized to fill the modal band).
- A footer key-hint line.

The viewport renders date-grouped rows: a non-selectable group header (`Today`, `Yesterday`, `This week`, then ISO date for older entries) above each cluster. Cursor skips group headers on Up / Down. Numeric-mnemonic jump (`1`–`9`) jumps to the Nth visible _session row_, skipping headers.

Each row paints in three columns aligned to a shared label width:

```text
> 1.  a1b2c3d4   Fix auth flow for mobile      2 hours ago
```

`>` plus space is the cursor marker; `1.` is the hint (only the first nine visible rows); `a1b2c3d4` is the 8-char id prefix; the title fills the middle (truncated to budget); the right column is relative time (`now`, `5m ago`, `2h ago`, `Apr 18`). When `--all` (CLI) or the `Show all projects` toggle is on, project path inserts as a fourth column between title and time.

Submit → `ModalKey::Submitted(ModalAction::User(UserAction::Resume { session_id }))`. No-row submit (empty list) cancels. Esc / Ctrl+C cancel via the universal-cancel gate in `ModalStack`.

### Searchable list primitive

[`crates/oxide-code/src/tui/modal/searchable_list.rs`](../../../crates/oxide-code/src/tui/modal/searchable_list.rs) — generic `SearchableList<T: SearchableItem>` modeled on `ListPicker<T>`:

- `query: String` — current search.
- `items: Vec<T>` — full unfiltered set.
- `visible: Vec<usize>` — indices of items matching `query`, recomputed on every `set_query`.
- `cursor: usize` — index into `visible`.
- `viewport_offset: usize` — first row painted; tracks the cursor so it stays on screen.

Trait `SearchableItem`:

```rust
pub(crate) trait SearchableItem {
    /// Composite haystack: substring filter matches against this. Should include every field
    /// the user might want to search (title, id, cwd, branch).
    fn haystack(&self) -> Cow<'_, str>;

    /// Optional grouping label. Items with the same label cluster under one header.
    /// `None` = no grouping; the list renders flat. Date grouping for resume passes
    /// `Today` / `Yesterday` / ISO date.
    fn group_label(&self) -> Option<&str> { None }

    /// Render one row given a layout. The primitive owns the cursor gutter; the implementation
    /// paints columns right of it. `is_cursor` controls bold / dim.
    fn render_row(
        &self,
        line: &mut Line<'static>,
        width_budget: usize,
        is_cursor: bool,
        theme: &Theme,
    );
}
```

Search is **case-insensitive substring** on `haystack()`. No fuzzy in v1 — `nucleo-matcher` would be the obvious upgrade if users ask.

Render order: title (1 row) + search input (1 row) + group / item rows for the visible window. The primitive computes its own `height(width)` from a target viewport height (clamped against the available band).

### Listing API

[`crates/oxide-code/src/session/store.rs`](../../../crates/oxide-code/src/session/store.rs):

- `SessionStore::list_paged(limit: Option<usize>, all: bool) -> Result<Vec<SessionInfo>>` — replaces the dual `list()` / `list_all()` callers. `None` means "no cap" (current behavior); `Some(n)` caps after sorting.
- Internal pipeline: read directory entries → stat each (cheap mtime + cwd from path) → sort by mtime desc → take `limit` → parse JSONL header + tail-scan for title / summary on the survivors.
- Old `list()` / `list_all()` stay as thin wrappers (`list_paged(None, false)` etc.) so existing call sites don't churn.

The win: for 1000s of sessions, only the first `limit` files get the full title / summary scan. The listing budget is the cap, not the directory scan.

### Picker pagination

V1 fetches a single capped page (default 30) and renders them all in the modal viewport. No background "load more on scroll near bottom" — that's worth landing only if the 30-session page proves too small in practice. The picker's `Page Down` key Just scrolls the existing viewport.

If the user has more than 30 sessions and wants to find an older one, they search by title or use `/resume <id-prefix>` directly. This is the **substring as a navigation primitive** approach Codex takes; it works and is simpler than infinite scroll.

### Mid-session re-init

[`crates/oxide-code/src/session/handle.rs`](../../../crates/oxide-code/src/session/handle.rs) gains `roll_into(...)` mirroring the existing `roll(...)`:

```rust
pub(crate) struct RollIntoOutcome {
    pub(crate) resumed: ResumedSession,
    pub(crate) finalize_failure: Option<String>,
}

pub(crate) async fn roll_into(
    session: &mut SessionHandle,
    store: &SessionStore,
    file_tracker: &FileTracker,
    target_session_id: &str,
) -> Result<RollIntoOutcome>;
```

Same pattern as `roll`: snapshot file tracker → clear → load + sanitize target → swap handle in place → finalize old session. The new session inherits the old client config (model / effort / theme).

### Agent loop wiring

[`crates/oxide-code/src/main.rs`](../../../crates/oxide-code/src/main.rs)'s `agent_loop_task` adds one arm:

```rust
UserAction::Resume { session_id } => {
    match handle::roll_into(&mut session, &store, &file_tracker, &session_id).await {
        Ok(outcome) => {
            sink.session_write_error(outcome.finalize_failure.as_deref());
            client.set_session_id(outcome.resumed.handle.session_id().to_owned());
            messages = outcome.resumed.messages;
            // Forward display-only payload to the App via a SessionResumed event.
            let _ = sink.send(AgentEvent::SessionResumed {
                id: session.session_id().to_owned(),
                title: outcome.resumed.title,
                messages_for_chat: outcome.resumed.messages.clone(),
                tool_metadata: outcome.resumed.tool_result_metadata,
            });
        }
        Err(e) => {
            let _ = sink.send(AgentEvent::Error(format!("Resume failed: {e:#}")));
        }
    }
}
```

[`crates/oxide-code/src/agent/event.rs`](../../../crates/oxide-code/src/agent/event.rs) adds:

```rust
AgentEvent::SessionResumed {
    id: String,
    title: Option<String>,
    messages_for_chat: Vec<Message>,
    tool_metadata: HashMap<String, ToolMetadata>,
}
```

```rust
UserAction::Resume { session_id: String }
```

### App handler

[`crates/oxide-code/src/tui/app.rs`](../../../crates/oxide-code/src/tui/app.rs)'s `handle_agent_event` adds one arm next to the existing `SessionRolled`:

```rust
AgentEvent::SessionResumed { id, title, messages_for_chat, tool_metadata } => {
    self.chat.clear();
    self.chat.load_history(&messages_for_chat, &tool_metadata, self.tools.as_ref());
    self.session_info.session_id = id;
    self.status_bar.set_title(title);
    self.pending_calls.clear();
    self.pending_prompts.clear();
    self.modals.cancel_all();
    self.dirty = true;
}
```

The same `chat.load_history` path the constructor uses on TUI startup runs again — symmetric with the resume-on-launch path that already works.

### `ox --list` fix

`render_list` calls `store.list_paged(Some(DEFAULT_LIST_LIMIT), all)` and prints `... and N more (showing first M; use --limit to widen)` after the rows when the cap clipped output. New CLI flag:

```rust
/// Cap `--list` to N most-recent sessions. 0 means no cap.
#[arg(long, value_name = "N", requires = "list", default_value = "30")]
limit: usize,
```

`--limit 0` opts back into the old unbounded behavior for piping. Default 30 is one screenful for a typical terminal.

## Design Decisions

1. **Bare `/resume` opens the picker; `/resume <id>` resolves directly.** Same shape as `/model`, `/effort`, `/theme` — bare opens a modal, typed-arg shortcuts. Reuses the `classify(args)` split we already use to gate state-mutating commands mid-turn. Reference CLIs all settled on this same exploratory-vs-direct split independently (Claude Code's `-r` vs `-c`, Codex's bare `resume` vs `resume <id>`).
2. **Alias `continue`, not `sessions`.** Opencode triple-aliases (`/sessions`, `/resume`, `/continue`); three names for one command is noise. `continue` matches the existing CLI flag (`--continue` / `-c`) — same word in both surfaces. `sessions` would hint at a multi-action verb (list / rename / delete) that doesn't exist.
3. **Mid-session re-init via `roll_into`, not process replacement.** Even though `Command::exec` would work on Unix, it kills the modal stack, queued prompts, live theme preview, and the agent loop's in-flight tool tracker. In-process swap is the only honest answer; reuse the `roll` pattern that `/clear` already validated.
4. **`ResumedSession` shape comes back from `roll_into` unchanged.** The same struct that startup-time `resolve_session` builds for the App constructor. No new payload type — `AgentEvent::SessionResumed` carries the display fields the App needs, the agent loop keeps the handle. Symmetry between startup and mid-session resume keeps the App's "load chat from messages" path single.
5. **New primitive `SearchableList`, not extended `ListPicker`.** `ListPicker` is small and focused; bolting search + viewport + grouping onto it doubles its surface area and bloats the API for the simpler pickers. `SearchableList` is the second primitive next to it, sibling-style — same trait pattern (`PickerItem` / `SearchableItem`), same render-row delegation, both implement `Modal` via concrete wrappers. If a third pattern shows up, the abstraction can fold; until then, two narrow primitives beat one wide one.
6. **Substring search, no fuzzy in v1.** Codex and opencode both ship plain substring and it works. `nucleo-matcher` is a small dep with no transitive cruft and is the obvious upgrade path if users complain — but the YAGNI cost of shipping it now isn't justified by the data.
7. **Date grouping: `Today` / `Yesterday` / `This week` / ISO date.** Borrowed from opencode. Free with relative-time formatting; reads cleaner than a flat mtime list once the view spans more than a day. Headers are inert rows (no cursor land); navigation skips them.
8. **Project scoping defaults to current cwd.** Matches the existing `ox -c` resolver semantics. CLI `--all` widens; in-picker `Tab` toggles project-scope (project-only ↔ all projects). Surfaces a `Project` column when widened, alphabetical tildified path. Codex's `Tab` toggles sort key (Created↔Updated); we use `Tab` for scope toggle because mtime is the only sensible sort and toggling scope is more useful.
9. **30-row default page; no background load-more in v1.** Codex's near-bottom prefetch is genuinely nice but adds a non-trivial state machine for marginal gain at typical session counts. If a user has >30 sessions in a project, search is the path. Re-evaluate after telemetry / user reports.
10. **Listing API consolidates `list()` / `list_all()` into `list_paged(limit, all)`.** Existing wrappers stay for back-compat at call sites; the picker and `--list` use the new entry point. The win is the cap-before-tail-scan ordering — for 1000 sessions, only the first N files get parsed. Resolves the unbounded-listing concern while serving the picker's pagination need with one code path.
11. **`--list --limit 0` opts back into unbounded listing.** Scripts that pipe the output to `wc -l` or `awk` need the full set; `--limit 0` is the explicit opt-out, not a magic large default.
12. **Reuse `normalize_resume_arg`.** The CLI's prefix vs path classifier is exactly what the slash form wants — one line of code, no new policy. The slash form rejects `Path` form (file-path resume is a CLI-only escape hatch; the picker is the in-session UX) but reuses `Prefix` resolution verbatim.
13. **Modal-only echoing on the bare form, but typed-arg echoes.** Mirrors `/theme`'s `echoes_input` rule: bare `/resume` → modal IS the response, typed `> /resume` line stays out of history. `/resume <id>` echoes (the swap-confirmation system message anchors the pair).
14. **Empty / single session both still show the picker.** Auto-resuming on a single match would surprise users who typed `/resume` to peek; the picker on a one-row list is cheap and instructive.

## Per-Component Notes

- **`ResumePicker` modal** — wraps `SearchableList<SessionRow>` plus the search input field and footer. Reads from a `Vec<SessionInfo>` snapshot loaded at open time. Esc cancels (universal); Enter submits the focused row. Up / Down navigate; Page Up / Down jump by viewport height. Tab toggles project-scope (cwd-only ↔ all-projects) — re-runs `list_paged` and rebuilds rows. Numeric `1`–`9` jump to the Nth visible session row.
- **`SearchableList<T>` primitive** — owns query string, visible indices, cursor, and viewport offset. Render contract: title (header) + search input row + visible item rows. Filter recomputes on `set_query`; cursor clamps to visible bounds. Group headers paint as inert rows; `select_next` / `select_prev` skip them.
- **`SessionRow` (`PickerItem` for resume)** — fields: id (8-char prefix), full id, title (or `(untitled)`), cwd (tildified), `last_active_at` (relative-time formatted), group label (`Today` / `Yesterday` / ISO). `haystack` joins title + full id + cwd for substring search.
- **`SessionStore::list_paged`** — takes `(limit: Option<usize>, all: bool)`. Builds a `Vec<(PathBuf, OffsetDateTime)>` of all candidate files, sorts by mtime desc, truncates to `limit`, then runs `read_session_info` only on the survivors. The `read_session_info` cheap-prefix scan stays as today.
- **`/resume <id>` resolver** — reuses `normalize_resume_arg` (rejecting `Path` form), then runs the `Prefix` lookup branch from `resolve_session` against the current scope. Output: `Result<String, String>` (full id or error message).
- **`UserAction::Resume`** — single field `session_id: String`. The agent loop owns the `roll_into` call; the slash command never touches the session handle directly.
- **`AgentEvent::SessionResumed`** — carries id + title + messages + tool_metadata. App-only reaction; `StdioSink` ignores it (TUI-only event, like `SessionRolled`).
- **`--list --limit N`** — `0` opts out of the cap; positive values cap. Footer line `... and N more (use --limit to widen)` only when the cap clipped output. Below-cap row counts get no footer.

## Out of Scope / Deferred

- **CLI `--resume` / `-r` flag.** A picker entry from the shell makes sense; not blocking the slash work and adds a new clap arm + alt-screen run mode. Worth its own PR.
- **Cross-project copy-to-clipboard gate** (Claude Code). The store already supports cross-project resume via prefix match under `--all`; the picker should mirror that, not add friction.
- **Worktree / branch tree expansion** (Claude Code's `▼` / `▶`). Date grouping is enough structure for now.
- **Side preview pane.** Useful but doubles the layout complexity. Defer.
- **Custom-title rename in-picker.** Sessions get an AI-generated title shortly after the first prompt; user-settable titles are a separate feature.
- **Background load-more on scroll near bottom.** 30-row page + search covers the typical case. Add when a user complaint shows a >30-session-in-one-project workflow.
- **Fuzzy search.** Substring is enough for v1; `nucleo-matcher` is the upgrade path.
- **Auto-resume on single match.** Users who type `/resume` to peek the list deserve to see it.
- **`/sessions` listing variant.** One slash command per intent; `ox --list` covers the script-friendly listing.

## Sources

- `crates/oxide-code/src/slash/resume.rs` — `ResumeCmd`, `ResumePicker`, `SessionRow`, prefix resolver.
- `crates/oxide-code/src/tui/modal/searchable_list.rs` — generic `SearchableList<T>` + `SearchableItem` trait.
- `crates/oxide-code/src/session/store.rs` — `list_paged`; existing `list` / `list_all` become wrappers.
- `crates/oxide-code/src/session/handle.rs` — `roll_into` mirrors `roll`.
- `crates/oxide-code/src/session/resolver.rs` — `normalize_resume_arg` reused unchanged.
- `crates/oxide-code/src/session/list_view.rs` — `render_list` uses `list_paged`; new `--limit` flag plumb through.
- `crates/oxide-code/src/agent/event.rs` — `UserAction::Resume`, `AgentEvent::SessionResumed`.
- `crates/oxide-code/src/main.rs` — `agent_loop_task` adds the `Resume` arm; `Cli` adds `--limit`.
- `crates/oxide-code/src/tui/app.rs` — `handle_agent_event` adds the `SessionResumed` arm.
- `crates/oxide-code/src/slash/registry.rs` — `BUILT_INS` adds `&ResumeCmd`.
