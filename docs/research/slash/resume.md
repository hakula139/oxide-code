# Session Resume / Continue (Reference)

Research on session-resume UX: CLI flag plumbing, picker rendering, search and pagination, mid-session reload. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode) (v1.3.15).

Companion to [commands.md](commands.md) and [modals.md](modals.md). Storage-layer notes (JSONL shapes, `parent_uuid` chain, listing scans) live in [session/persistence.md](../session/persistence.md). This file focuses on the _user-facing_ resume surface.

## Claude Code (TypeScript + Ink)

Two CLI entry points (direct and exploratory) plus a Ctrl-modifier-driven inline picker for mid-session resume.

- **CLI**: `-c, --continue` is the speed path, jumping straight to the most recent session. `-r, --resume [value]` is the exploratory path, opening the picker if no value is given and otherwise accepting an ID, custom title, or file path (`src/main.tsx:990`). Both route through `loadConversationForResume` for direct mode and `launchResumeChooser` for picker mode.

- **Picker**: [`LogSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/LogSelector.tsx) Ink component. Two-line rows: title (left, normalized + truncated via `normalizeAndTruncateToWidth`) plus a metadata sub-line (relative time, branch, msg count, user tag, agent, PR / repo). Column width `max(30, columns - 4)`. Sessions group by repo / worktree under expandable `▼` / `▶` headers, mtime-desc within each group.

- **Search**: Two layers. Typeahead substring on title / tag / branch, plus an opt-in agentic search (`agenticSessionSearch`) that runs across message bodies / summaries / tags / branches. Enter from the search field triggers the agentic path.

- **Pagination**: Lazy on-demand expansion. Initial fetch is 50 sessions (`INITIAL_ENRICH_COUNT = 50`). `onLoadMore()` pulls `visibleCount * 3` more near the loaded boundary. Footer shows `({focused} of {displayed})` on overflow. Metadata-only ("lite log") loads keep the picker instant, and full bodies load only on Enter.

- **Scope toggles**: `Ctrl+A` all-projects vs. current dir. `Ctrl+W` all-worktrees in same repo. `Ctrl+B` current-branch filter. `Ctrl+V` opens [`SessionPreview`](https://github.com/hakula139/claude-code/blob/main/src/components/SessionPreview.tsx) (last-N messages, side pane). `Ctrl+R` renames the selected session.

- **Mid-session form**: `/resume` (alias `/continue`) is a `local-jsx` slash command mounting the same `LogSelector` inline. Selection re-initializes the same TUI process via `context.resume?.(sessionId, fullLog, 'slash_command_picker')`, with no fork-exec.

- **Cross-project gate**: Picking from a different cwd shows a copy-to-clipboard `cd <path> && claude --resume ...` rather than resuming directly.

- **Empty / single states**: Both show the picker, no auto-resume on a single match. Zero sessions surfaces "No conversations found to resume" and exits.

## OpenAI Codex (Rust + Ratatui)

Resume is a subcommand, with a full-screen alt-screen picker running before chat starts.

- **CLI**: `codex resume [SESSION_ID]` and `codex fork [SESSION_ID]`. Modes: bare opens the picker, `--last` jumps to the most recent, positional `<id>` resolves directly (UUID first, falling back to thread name). Modifiers: `--all` drops the cwd filter, `--include-non-interactive` includes CLI-only / VSCode rollouts.

- **Picker**: Full-screen alt-screen (`resume_picker.rs:336`), pre-chat. Returns `SessionTarget { path, thread_id }` to the main loop, which then calls `app_server.resume(...)`. Layout: header, search line, column row (`Created | Updated | Branch | CWD | Conversation`), list, key-hint footer. Bold `>` plus space marks selection. `Tab` toggles sort key (Created ↔ Updated).

- **Search**: Client-side, case-insensitive substring on `preview` (first user message, ~50 chars) and `thread_name`. No fuzzy. Server-side `ThreadListParams { source_kinds, archived, cwd, model_providers }` filters up front, then the client narrows the loaded page.

- **Pagination**: Cursor-based at 25 rows/page (`PAGE_SIZE = 25`). Within 5 rows of the loaded bottom (`LOAD_NEAR_THRESHOLD = 5`), the next page fetches in the background. Hard scan cap of 10,000 files per request surfaces `"Search scanned first N sessions, more may exist"` when exceeded. Zero-hit visible filter keeps scanning in the background until a match lands or the cap hits.

- **Project scoping**: Cwd-filtered by default via `paths_match_after_normalization` (handles symlinks and trailing slashes). `--all` disables and surfaces the CWD column. No git-root detection, so "the project" is the cwd of invocation.

- **Key bindings**: Up / Down / PgUp / PgDn / Tab / Enter / Esc plus emacs-style Ctrl-P / Ctrl-N / Ctrl-^P / Ctrl-^N. No vim keys, no numeric shortcuts. Backspace pops one char from search. No preview pane.

- **Mid-session form**: `/resume` (`slash_command.rs:87`) is whitelisted to fire even during a running task, wired through the same picker and direct-id paths.

## opencode (TypeScript + Solid + Kobalte)

Three aliases for one overlay, plus a CLI-only listing subcommand.

- **CLI**: `/sessions`, `/resume`, `/continue` all open the same picker, plus a `Ctrl+X L` keybinding (`session_list`). A separate `session list` subcommand exists for headless / scripting use.

- **Picker**: `DialogSelect` overlay (not a Kobalte `Dialog`) with date-header groups (`Today` / `May 8, 2026` / ...). Sort: `time.updated` desc with ID tiebreak to prevent 1-minute-window flicker. Filter excludes child sessions (`parentID === undefined`), so only roots show. Limit 30 when filtering, otherwise all roots.

- **Search**: Typing while the picker is open. SDK call: `sdk.client.session.list({ search: query, limit: 30 })`. Server-side substring on title.

- **Submit**: `route.navigate({ type: "session", sessionID })` then `dialog.clear()`. Solid reactivity swaps the active session context without a full reload.

- **Web app** (out of scope for terminal lessons): sidebar groups by workspace, hover preview via `SessionHoverPreview`, drag-drop reordering. Doesn't transpose since TUIs don't hover.

- **Empty states**: Blank list when `sync.data.session` is empty. Web app shows a "New session" CTA. Neither surfaces a "no sessions found in this project" line.

- **Key bindings**: Up / Down / Ctrl+P / Ctrl+N, PgUp / PgDn (±10), Home / End, Enter / Esc. Mouse click works in the TUI but isn't required.

## Comparison

| Aspect            | Claude Code                              | Codex (Rust)                            | opencode                              |
| ----------------- | ---------------------------------------- | --------------------------------------- | ------------------------------------- |
| CLI surface       | `--continue` direct + `--resume` picker  | `resume` subcommand + `--last`          | `session list` (script-only)          |
| Mid-session entry | `/resume` (alias `/continue`)            | `/resume` (allowed during task)         | `/sessions`, `/resume`, `/continue`   |
| Picker layer      | inline modal in chat                     | full-screen alt-screen, pre-chat        | overlay (`DialogSelect`)              |
| Row shape         | title + metadata sub-line                | column row (Cre / Upd / Br / CWD / msg) | grouped by date, single-line          |
| Search            | substring + agentic AI                   | substring (case-insensitive, instant)   | substring via SDK call                |
| Page size         | 50 initial + `visibleCount * 3` more     | 25 + cursor-based, near-bottom prefetch | 30 when filtering, all otherwise      |
| Project scope     | Ctrl+A toggle + Ctrl+W worktrees         | cwd default, `--all` widens             | workspace-global (root sessions only) |
| Sort key          | mtime desc, grouped by repo              | mtime desc, Tab toggles Created vs Upd  | `time.updated` desc + ID tiebreak     |
| Preview pane      | Ctrl+V (last messages)                   | none                                    | hover preview (web only)              |
| Cross-project     | gated, copy-to-clipboard hint            | direct                                  | not surfaced (workspace-global)       |
| Empty / single    | always show picker                       | always show picker                      | blank list                            |
| Selection         | in-process re-init                       | in-process, returns to main loop        | URL navigation                        |

## Patterns Worth Borrowing for oxide-code

1. **Direct plus exploratory split.** `/resume` opens the picker, `/resume <id>` resolves directly. CLI keeps `-c` / `--continue` for the direct latest path. Claude Code and Codex both converged on this independently.

2. **Cwd-default plus `--all` widens.** The existing `ox --continue` resolver already does this. All three reference CLIs converge here because users almost always want their own project.

3. **In-process re-init.** Claude Code and Codex both swap state without forking. Keeps modals, theme preview, and queued prompts intact, and avoids the alt-screen flash of process exit-and-relaunch. Reuses the `roll`-style helper already shipped for `/clear`.

4. **Page size around 25-50.** Codex's 25 with near-bottom prefetch and Claude Code's 50 with `visibleCount * 3` both work. Pick the size that gives one screenful plus headroom.

5. **Date grouping (`Today` / `Yesterday` / date).** Opencode's idea. Easier to scan than a flat mtime list once you have more than a screenful, and free with relative-time formatting infra.

6. **Substring search only in v1.** Claude Code's agentic search is novel but expensive. Codex and opencode both ship plain substring and it works. Defer fuzzy until users ask.

7. **Listing budget on `ox --list`.** Currently uncapped. A default cap (with `--limit` opt-out) keeps `--list` snappy and matches every reference CLI's bulk-listing behavior.

8. **Footer key-hint line.** All three pickers use a one-row footer for discoverability. Stick to the `Enter to confirm  ·  Esc to cancel` shape and add `/ to search` etc.

## Patterns to Reject

1. **Ctrl-modifier toggle clusters** (Claude Code's `Ctrl+A` / `Ctrl+W` / `Ctrl+B` / `Ctrl+V`). Undiscoverable without footer hints, and the toggle state isn't visible mid-list. A single `Tab` to widen scope (Codex's pattern) or a typed `--all` flag (oxide-code's convention) covers the same ground with less keymap churn.

2. **Agentic AI search.** API call per query, latency, cost, opacity. Defer indefinitely.

3. **Worktree-tree expansion (`▼` / `▶`).** Adds a navigation axis. Date-grouped flat list reads cleaner, and users who care about worktree separation can filter by typing the path.

4. **Cross-project copy-to-clipboard gate.** Claude Code refuses to resume from another cwd in-process. oxide-code's store already supports cross-project resume via path or `--all`, so the picker should mirror that and resume directly on pick.

5. **Custom-title rename in-picker** (Claude Code's `Ctrl+R`). Sessions get an AI-generated title shortly after the first prompt, and user-overridable titles are a separate feature. Defer with "session metadata management."

6. **Triple alias** `/sessions` + `/resume` + `/continue` (opencode). Three names for one command is noise. One canonical (`/resume`) plus one alias (`/continue`) is enough.

7. **Process exit / relaunch.** Even though `std::env::current_exe()` plus `Command::exec` would work on Unix, it kills the modal stack, queued prompts, and theme preview. In-process replacement is the only honest answer.

8. **Preview pane.** Claude Code's preview is genuinely useful but doubles the picker's layout complexity (split-pane, focus model, scrollback). Land the picker first, then treat preview as a follow-up.
