# Session Resume / Continue (Reference)

Research on session-resume UX: CLI flag plumbing, picker rendering, search and pagination, mid-session reload. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode) (v1.3.15).

Companion to [commands.md](commands.md) and [modals.md](modals.md). Storage-layer notes (JSONL shapes, parent_uuid chain, listing scans) live in [session/persistence.md](../session/persistence.md). This file focuses on the _user-facing_ resume surface.

## Claude Code (TypeScript + Ink)

Two flags split intent. `-c, --continue` is the **speed path** with no picker, jumping straight to the most recent session. `-r, --resume [value]` is the **exploratory path** that opens the picker if no value, and accepts an ID / custom title / file path otherwise (`src/main.tsx:990`). Both route through `loadConversationForResume(undefined)` for direct mode and `launchResumeChooser()` for picker mode.

The picker is the [`LogSelector`](https://github.com/hakula139/claude-code/blob/main/src/components/LogSelector.tsx) Ink component (`src/components/LogSelector.tsx:143`). Layout is two lines per row: the title (left, normalized + truncated via `normalizeAndTruncateToWidth`, `:62`) and a metadata line below (relative time, git branch, message count, user-assigned tag, agent setting, PR number / repo). Column width is `max(30, columns - 4)` (`:563`). Sessions group by repo / worktree under expandable `▼ / ▶` headers, with sort within each group as mtime descending.

**Search** has two layers: a typeahead substring filter on title / tag / branch, and an opt-in **agentic search** (`agenticSessionSearch`, `resume.tsx:17`) that the model runs across message bodies / summaries / tags / branches for fuzzy semantic matches. Type to filter, and Enter from the search field triggers agentic search.

**Pagination** is lazy with on-demand expansion. Initial fetch is 50 sessions (`INITIAL_ENRICH_COUNT = 50`, `:4577`). Each row is 3 visual lines, so visible-count is `floor((maxHeight - headerLines - 2) / 3)`. When the focused index approaches the loaded boundary, `onLoadMore()` fires and pulls `visibleCount * 3` more (`:1209`). Footer shows `({focused} of {displayed})` when overflow exists. Metadata-only ("lite log") loads keep the picker instant, since full bodies only load on Enter.

**Project / scope toggles** all live on Ctrl-modifiers, discoverable via the footer hint:

- `Ctrl+A` toggles all-projects vs. current directory.
- `Ctrl+W` toggles all-worktrees in same repo.
- `Ctrl+B` toggles the current-branch filter.
- `Ctrl+V` opens or closes [`SessionPreview`](https://github.com/hakula139/claude-code/blob/main/src/components/SessionPreview.tsx), showing last-N messages of the focused session in a side pane.
- `Ctrl+R` renames the selected session (custom title).

**Mid-session form**: `/resume` (alias `/continue`) is a `local-jsx` slash command (`src/commands/resume/index.ts:7`) that mounts the same `LogSelector` inline. Selection re-initializes the same TUI process via `context.resume?.(sessionId, fullLog, 'slash_command_picker')`, with no fork-exec. Cross-project selection is gated: when picking a session from a different cwd, the picker shows a copy-to-clipboard `cd <path> && claude --resume ...` instead of resuming directly.

**Empty / single-session states** both show the picker (no auto-resume on a single match). Zero sessions surfaces "No conversations found to resume" and exits.

## OpenAI Codex (Rust + Ratatui)

Resume is a **subcommand** rather than a flag: `codex resume [SESSION_ID]` and `codex fork [SESSION_ID]` (`codex-rs/cli/src/main.rs:153`). Modes: bare opens the picker, `--last` jumps to the most recent, and positional `<id>` resolves directly (UUID first, falling back to thread name). Modifiers: `--all` drops the cwd filter, and `--include-non-interactive` includes CLI-only / VSCode rollouts.

The picker is full-screen alt-screen (`codex-rs/tui/src/resume_picker.rs:336`). It runs **before** the chat starts and returns a `SessionTarget { path, thread_id }` to the main loop, which then calls `app_server.resume(...)`. Layout: header, search line, column row (`Created | Updated | Branch | CWD | Conversation`), list, key-hint footer. Selection marker is a bold `>` plus space, and `Tab` toggles sort key (Created ↔ Updated).

**Search** is client-side, case-insensitive, and instantaneous substring on `preview` (first user message, ~50 chars) and `thread_name` (`:807`). No fuzzy match. Server-side filtering happens up front via `ThreadListParams { source_kinds, archived, cwd, model_providers }` (`:998`), and the client-side filter narrows the loaded page.

**Pagination** is cursor-based at 25 rows per page (`PAGE_SIZE = 25`, `:41`). When the cursor lands within 5 rows of the loaded bottom (`LOAD_NEAR_THRESHOLD = 5`), the next page fetches in the background. There is a hard scan cap of 10,000 files per request (`rollout/src/list.rs:105`), which surfaces a `"Search scanned first N sessions, more may exist"` footer when exceeded. If the visible filter has zero hits but more pages exist, the picker keeps scanning in the background until a match lands or the cap hits.

**Project scoping** is cwd-filtered by default via `paths_match_after_normalization`, which handles symlinks and trailing slashes. `--all` disables the filter and surfaces the CWD column. There is no git-root detection, so "the project" is the cwd of the invocation.

**Key bindings**: Up / Down / PgUp / PgDn / Tab / Enter / Esc plus emacs-style Ctrl-P / Ctrl-N / Ctrl-^P / Ctrl-^N. No vim keys and no numeric shortcuts. Backspace pops one char from search. No preview pane.

A mid-session `/resume` slash command exists (`slash_command.rs:87`) and is whitelisted to fire even during a running task, but the handler is wired through the same picker and direct-id paths.

## opencode (TypeScript + Solid + Kobalte)

Three names point at one surface: `/sessions`, `/resume`, `/continue` (`packages/opencode/src/cli/cmd/tui/app.tsx:457`). Plus a `Ctrl+X L` keybinding mapped to `session_list`. A CLI-only `session list` subcommand exists separately for headless / scripting use (`packages/opencode/src/cli/cmd/session.ts:74`).

The TUI picker is a `DialogSelect` overlay (not a Kobalte `Dialog`), with categorized list rows under date headers (`Today` / `May 8, 2026` / ...). Sort key is `time.updated` desc with **ID tiebreak** to prevent flicker when sessions update within a 1-minute window (`packages/app/src/pages/layout/helpers.ts:17`). Filter excludes child sessions (`parentID === undefined`, `dialog-session-list.tsx:40`), so only roots show. Limit is **30 when filtering**, otherwise all roots.

**Search** triggers via typing while the picker is open, with a client SDK call: `sdk.client.session.list({ search: query, limit: 30 })`. Server-side substring on title.

**Submit semantics**: `route.navigate({ type: "session", sessionID })` then `dialog.clear()`. Solid reactivity swaps the active session context without a full reload.

**Web app** (out of scope for terminal lessons): sidebar groups by workspace directory, hover preview via `SessionHoverPreview` (last messages plus agent tint), drag-drop reordering. None of this transposes, because TUIs don't hover.

**Empty states** are minimal. The TUI renders a blank list when `sync.data.session` is empty, and the web app shows a "New session" CTA. Neither surfaces an explicit "no sessions found in this project" line.

**Key bindings**: Up / Down / Ctrl+P / Ctrl+N, PgUp / PgDn (±10), Home / End, Enter / Esc. Mouse click works in the TUI but isn't required.

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

1. **Direct plus exploratory split.** `/resume` (or alias `/continue`) opens the picker, while `/resume <id>` resolves directly. CLI keeps `-c` / `--continue` for the direct latest path. This mirrors Claude Code's `-r` vs `-c` split and Codex's `resume <id>` vs `resume --last`. Both communities settled on the same shape independently, so the two paths should stay separate.
2. **Cwd-default plus `--all` widens.** The existing `ox --continue` resolver already does this, and the picker should match. All three CLIs converge on this default because users almost always want their own project.
3. **In-process re-init.** Claude Code and Codex both swap state without forking. Keeps modals, preview-theme, pending-prompts, and queued state intact, and avoids the alt-screen flash of process exit-and-relaunch. Reuses the `roll`-style helper already shipped for `/clear`.
4. **Page-size around 25-50.** Codex's 25 plus cursor-near-bottom prefetch and Claude Code's 50 plus `visibleCount * 3` both work. Either is fine, so pick the size that gives one initial screenful plus headroom.
5. **Date grouping (`Today` / `Yesterday` / date), opencode's idea.** Easier to scan than a flat mtime list once you have more than a screenful. Free with relative-time formatting infra.
6. **Substring search only in v1.** Claude Code's agentic search is novel but expensive. Codex and opencode both ship plain substring and it works. Defer fuzzy until users ask.
7. **Listing budget on `ox --list`.** Currently uncapped. A default cap (with `--limit` opt-out) keeps `--list` snappy and matches how every reference CLI handles bulk listing. None of them dump every session unconditionally.
8. **Footer key-hint line.** All three picker variants use a one-row footer for discoverability. Stick to the established `Enter to confirm  ·  Esc to cancel` shape and add `/ to search` etc.

## Patterns to Reject

1. **Ctrl-modifier toggle clusters (Claude Code's `Ctrl+A` / `Ctrl+W` / `Ctrl+B` / `Ctrl+V`).** Cute but undiscoverable without footer hints, and the toggle state isn't visible mid-list. A single `Tab` to widen scope (Codex's pattern) or a typed `--all` flag (oxide-code's existing convention) covers the same ground with less keymap churn.
2. **Agentic AI search.** API call per query, latency, cost, opacity. Defer indefinitely.
3. **Worktree-tree expansion (`▼` / `▶`).** Adds a navigation axis (`Right` / `Left` to expand). Date-grouped flat list reads cleaner, and users who care about worktree separation can filter by typing the path.
4. **Cross-project copy-to-clipboard gate.** Claude Code refuses to resume a session from another cwd in-process. oxide-code's session-store already supports cross-project resume via path or `--all` flag, so the picker should mirror that and resume directly on pick.
5. **Custom-title rename in-picker (Claude Code's `Ctrl+R`).** Sessions already get an AI-generated title shortly after the first prompt, and user-overridable titles are a separate feature. Defer with the rest of "session metadata management."
6. **Single shared `/sessions`+`/resume`+`/continue` triple alias (opencode).** Three names for one command is noise. Pick one canonical (`/resume`, terminal-natural for the resume action) plus one alias (`/continue`, matches the existing `--continue` flag), and that's enough.
7. **Process exit / relaunch.** Even though `std::env::current_exe()` plus `Command::exec` would work on Unix, it kills the modal stack, queued prompts, and live theme preview. In-process replacement is the only honest answer.
8. **Preview pane.** Claude Code's preview is genuinely useful but doubles the picker's layout complexity (split-pane layout, focus model, scrollback). Land the picker first, then treat preview as a follow-up if users ask.
