# Terminal UI

The terminal UI is a ratatui app composing concrete components directly, painting flicker-free frames with viewport-scoped block virtualization, cached streaming Markdown, and an empty-chat welcome screen.

## Stack

| Crate              | Purpose                                                       |
| ------------------ | ------------------------------------------------------------- |
| `ratatui`          | Terminal UI framework: layout, widgets, double-buffer diffing |
| `crossterm`        | Backend: async terminal events, ANSI output, sync updates     |
| `tokio`            | Async runtime for streaming, tool execution, event mux        |
| `pulldown-cmark`   | CommonMark parser: event-driven iterator (custom renderer)    |
| `syntect`          | Syntax highlighting for fenced code blocks                    |
| `ratatui-textarea` | Multi-line text input widget                                  |

## Architecture

### Components

`App` composes concrete structs (`ChatView`, `InputArea`, `StatusBar`, `ModalStack`) directly, dispatching events top-down and collecting actions bottom-up while each component owns its state and rendering.

### Named Keys

Global keys live in `APP_KEYMAP` (`ox_tui::action`), mapping `AppAction` entries built with `ox_tui::keymap` so keys and footer hints (`Key to effect` via `keymap::hint`) share one source. Global keys fire before the input box when no modal holds focus, and `/resume` builds its keys and footer from the same types. Context-local keys stay beside their state: prompt editing in `ox_tui::grapheme`, exit and interrupt in `app::dispatch`, and modal keys in each modal module.

### Async Event Loop

```text
tokio::select! {
    event = crossterm_events.next()             => { /* keyboard, mouse, resize */ }
    event = self.events.recv()                  => { /* Event: stream tokens, tool calls, session updates */ }
    Some(event) = self.background.recv()        => { /* AppEvent: git probe results, copy outcomes, background slash command output, /resume picker pages */ }
    () = sleep_until(self.next_tick(last_tick)) => { /* spinner frame, probe scheduling, exit-arm expiry, selection edge scroll, copy-notice expiry, top-modal tick, render when dirty */ }
}
```

## Flickering Prevention

1. **Double-buffer cell diffing** (ratatui built-in) emits ANSI only for changed cells.
2. **Synchronized output** (DEC private mode 2026) presents each frame atomically.
3. **Render throttling** (~60 FPS) batches streaming tokens between frames.
4. **Overwrite, don't clear** avoids erase-then-redraw flicker.
5. **Cached block renders near the viewport** render each committed block once per width, theme, and view ([Chat Virtualization](#chat-virtualization)) and paint only visible rows.

## Chat Virtualization

Resize, theme swap, or view toggle invalidates cached blocks, leaving only blocks near the viewport to re-render:

- **Render near the viewport.** Each frame renders stale blocks within one viewport height of the painted range, nearest the anchored row first, re-placing after each until the range is current (extending up from the tail under auto-scroll).
- **Estimate the rest.** Farther blocks reuse their previous height (or one blank row if unrendered). Estimates above the range shift every row inside it equally, keeping painted rows and bottom clamping exact.
- **Anchor on a block.** When scrolled up, the top row is anchored by block and relative row offset so reading position stays steady as blocks above rewrap. Live-reply rows anchor by distance from the last committed block so committing keeps them in place. After a view toggle or click expansion, rows keep their in-block index. Under auto-scroll (or when a change leaves a paused view at the bottom), the paint follows the tail.
- **Apply scrolls at render.** Scroll keys and wheel add to a pending delta applied to the anchored row on the next frame, spanning the render range from anchor to new top so rapid PageUp bursts land identically to per-frame presses. Ctrl+Home jumps to row 0, and scrolling to the bottom re-arms auto-scroll.

## Compact and Expanded Views

Chat opens in compact view, toggled with Ctrl+O:

- **Compact.** Tool bodies cap visible rows and per-row byte width, edit diffs collapse long or overflow hunks with `⋮`, task lists window around the in-progress task, and edit-location lists show a short prefix (see [Sources](#sources) for caps). Hidden rows collapse to a `dim` footer (`... +3 lines · Ctrl+O to expand`, or `files` / `tasks` / `hunks`), adding `(limit reached)` when Grep or Glob also hit its tool cap. Diffs hitting the line cap always end in `... +N lines past the diff limit` in both views. Committed thinking shows only its `Thought` header. Clicking a footer or header expands that block alone ([Mouse Interactions](mouse.md#expanding-a-block)).
- **Expanded.** Renders every tool body and thinking block in full, clearing any per-block click expansions on toggle in either direction.

Live streaming thinking shows its body in both views under the spinner and `Thinking`. Model progress notes before tool calls render as prose only while `show_thinking` is off, since the thinking block otherwise carries the same text.

## Streaming Markdown

Streaming replies, live thinking, and in-flight compaction summaries each wrap text in `LiveMarkdown` with a block-based stable-prefix cache that advances on render and resets on width change:

- **Commit complete blocks.** Once a new top-level block starts, prior blocks render once into `Line<'static>` and frames re-parse only the tail.
- **Detect stable boundaries.** Only newline-terminated lines start blocks (blank lines inside fenced code or loose lists remain in the tail).
- **Carry renderer state.** Each chunk starts from the previous chunk's renderer state so separators and icon rows match a full render.
- **Resolve cross-block references on completion.** Cross-block reference-style links render literally until the turn commits.

An open code block stays in the tail with its syntax highlighter resuming from the last complete line (highlighting only new lines while still re-wrapping the block).

## Markdown Tables

Tables render boxed in `table_border` with one-cell inner padding, `table_header` styling (bold accent by default), and `├ ┼ ┤` rules between every pair of rows, inheriting surrounding list indents or blockquote `│` bars.

Columns take natural widths when they fit. Otherwise each gets its longest word up to a per-column cap (see [Sources](#sources)) so long paths or URLs wrap, distributing remaining width proportionally to natural-width shortfall with word wrapping. When minimum widths still overflow, rows stack as `header: value` lines separated by `table_border` rules. `<br>` starts a new line within a GFM cell.

## Welcome Screen

When the chat is empty (including streaming) and `show_welcome` is on, `App::draw_frame` paints the welcome screen: identity ribbon with version, model / effort / auth source, working directory, three starter commands, and a tip. `/clear` restores it, while resumed sessions never show it.

Starters sample bare-runnable built-in commands with registry descriptions, and tips sample `TIP_POOL` (including the PR tip only when the status line contains `pull-request`), both seeded via `fastrand` from the xxh64 hash of the session ID so picks stay stable per session. Body lines pad to a shared width so centered text aligns on a common left column. Coarse width tiers (see [Sources](#sources)) show the full layout at ≥ 60 columns, drop to a wordmark and trim the environment line on narrower panes (hiding starters and the tip below 40 columns), and suppress the welcome below 25 columns. A short pane (height < 13 rows) hides only starters and the tip while keeping the ribbon and environment line for its width tier.

## Design Decisions

1. **ratatui + crossterm + tokio.** Matches Codex's stack for double-buffered diffing and async terminal event streaming without writing a custom renderer or input reader.
2. **Concrete component structs.** Components share nothing beyond painting an area, so concrete composition avoids pointless trait indirection.
3. **Synchronized output by default.** Terminals supporting mode 2026 paint frames atomically, and others ignore it.
4. **Render throttling at ~60 FPS, and a sleeping idle loop.** Throttling batches fast token streams off the input loop, background work wakes the loop via one `AppEvent` channel without polling, and ticks fire only on pending redraws or the earliest component deadline (such as spinner frames, expiring notices, or the 5 s branch probe when configured).
5. **Block-based markdown commit with a stable-prefix cache.** Avoids O(n) full-reply re-parsing on every streamed frame.
6. **Custom pulldown-cmark + syntect renderer.** Uses Codex's `pending_markers` pattern for list rendering.
7. **Two-tier tool display.** Inline icons and summaries keep long tool loops scannable while capped bodies expose enough detail to judge each call.
8. **Per-block render cache with viewport-scoped rendering.** Prevents long-session resize or theme-swap stalls while block-anchored scroll keeps reading position steady.
9. **A rule between every table row.** Prevents wrapped rows with blank cells from visually merging with adjacent single-line rows, matching Claude Code, oh-my-pi, and Codex.
10. **Table columns no narrower than their longest word.** Prevents breaking tokens like `--all-targets` across rows (matching Claude Code and oh-my-pi), and falls back to stacked `header: value` rows when minimum widths overflow (matching Claude Code).
11. **One toggle for every collapsed block.** An in-place Ctrl+O toggle preserves scroll position across both tool bodies and thinking blocks, matching Claude Code's Ctrl+O transcript shortcut and Codex's footer hint.
12. **Tildes stay literal.** Disables pulldown-cmark 0.13 strikethrough so prose like `cost ~$1-~$2` and `~` paths never render struck out, matching Claude Code (while Codex keeps strikethrough).
13. **The welcome paints in place of an empty chat.** Keeps `ChatView` free of ephemeral onboarding state that would break `is_empty` or leak into transcripts.
14. **The welcome is a stateless projection.** Reading `LiveSessionInfo` on each frame reflects model, effort, and auth changes immediately with no cache to invalidate.
15. **Three sampled starters and a tip.** Gives actionable next steps with variety without turning the welcome into a full command listing or dashboard.
16. **Starters come from the slash registry.** Reuses registry descriptions without drift, using `NOT_STARTERS` so welcome curation stays out of `SlashCommand`.
17. **An editorial ribbon, with no box, mascot, or animation.** Avoids generic CLI chrome and outsized mascots (Claude Code) or animations (Codex), using the project name `oxide-code` for discoverability while `ox` stays the binary name.
18. **A coarse width ladder.** Three fixed tiers are simpler than per-element truncation and reflow (Codex, Claude Code) or flex shrinking (opencode) for fixed welcome content.

## Out of Scope

- **Live feeds on the welcome.** Recent sessions, release notes, and upsells (as in Claude Code) would create dashboard maintenance overhead.
- **Interactive welcome.** The welcome stays read-only while users type in the input below.
- **Git status on the welcome.** Already shown in the status line.

## Sources

- `crates/ox-tui/src/action.rs`: `APP_KEYMAP`, the app's global key and its hint.
- `crates/ox-tui/src/app.rs`: `App`, its `tokio::select!` loop, `TICK_INTERVAL`, the shortest gap between ticks, and `App::next_tick`, which picks when the loop wakes.
- `crates/ox-tui/src/app/render.rs`: `App::draw_frame`, which paints the welcome while the chat is empty.
- `crates/ox-tui/src/app_event.rs`: `AppEvent`, the result of background work, and `AppEventSender`, which the work reports through.
- `crates/ox-tui/src/components/chat.rs`: `ChatView` and the compact and expanded views.
- `crates/ox-tui/src/components/chat/blocks.rs`: `ChatBlock` and `RenderCtx`, the render contract of a chat block.
- `crates/ox-tui/src/components/chat/blocks/live_markdown.rs`: `LiveMarkdown`, the text of a section that grows while it streams, with its stable-prefix cache.
- `crates/ox-tui/src/components/chat/blocks/rows.rs`: `MAX_ROW_BYTES`, the compact-view per-row byte cap.
- `crates/ox-tui/src/components/chat/blocks/streaming.rs`: `StreamingAssistant`, the streaming reply over its cache.
- `crates/ox-tui/src/components/chat/blocks/tool.rs`: `ToolCallBlock`, `ToolResultBlock`, and `MAX_TOOL_OUTPUT_LINES`, the compact-view tool-output row cap.
- `crates/ox-tui/src/components/chat/blocks/tool/diff.rs`: `MAX_DIFF_BODY_LINES` and `MAX_LOCATIONS_DISPLAYED`, the compact-view diff body and edit-location caps.
- `crates/ox-tui/src/components/chat/blocks/tool/tasks.rs`: `render`, which windows task lists around the in-progress task under `MAX_TOOL_OUTPUT_LINES`.
- `crates/ox-tui/src/components/chat/scroll.rs`: `ChatView::place`, the anchored placement each frame paints, and the auto-scroll that follows the tail.
- `crates/ox-tui/src/components/chat/stack.rs`: `CachedBlock`, `SlotTops`, and `Stack`, the block cache and the row anchors.
- `crates/ox-tui/src/components/welcome.rs`: `paint`, `Picks`, `NOT_STARTERS`, `TIP_POOL`, and the width ladder.
- `crates/ox-tui/src/grapheme.rs`: the grapheme-aware editing table the prompt and the modal text fields share, with `GraphemeStops`.
- `crates/ox-tui/src/keymap.rs`: `KeyAction`, `KeyChord`, `Keymap`, and `hint`, the shape every key hint takes.
- `crates/ox-tui/src/markdown.rs`: `OPTIONS`, the parser extensions, and `render_markdown_chunk` and `complete_blocks_len`, the stable block boundaries.
- `crates/ox-tui/src/markdown/render.rs`: the pulldown-cmark renderer, its `pending_markers`, and the table layout.
- `crates/ox-tui/src/terminal.rs`: `synchronized_update`, which wraps each frame in mode 2026.
