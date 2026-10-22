# Terminal UI

The terminal UI is a ratatui app that composes its components directly, paints each frame without flicker, renders only the chat blocks near the viewport, and renders streamed markdown from a cache of the blocks already complete.

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

Components are concrete structs (`ChatView`, `InputArea`, `StatusBar`, modals) that the App composes directly, since there's no formal `Component` trait. Each owns its state, handles its events, and renders into a given area, while the root `App` dispatches events top-down and collects actions bottom-up.

### Named Keys

The app's own keys live in `APP_KEYMAP` in `ox_tui::action`, a table of named `AppAction`s built from the `KeyAction`, `KeyChord`, and `Keymap` types in `ox_tui::keymap`. Each entry carries its key and hint, so the hint a footer shows comes from the entry the key dispatches through. A key in the table works while no modal holds the keyboard and reaches the app before the input box. The `/resume` picker builds its keys and footer from the same types.

### Async Event Loop

```text
tokio::select! {
    event = crossterm_events.next()               => { /* keyboard, mouse, resize */ }
    event = self.events.recv()                    => { /* Event: stream tokens, tool calls, session updates */ }
    Some(update) = self.git_probes.recv()         => { /* branch / PR probe results for the status line */ }
    Some(report) = self.clipboard.recv()          => { /* outcome of a copy, shown as a notice */ }
    Some(finished) = self.slash_tasks.join_next() => { /* output of a background slash command, such as /diff */ }
    () = sleep_until(self.next_tick(last_tick))   => { /* spinner frame, probe scheduling, exit-arm expiry, selection edge scroll, copy-notice expiry, top-modal tick, render when dirty */ }
}
```

## Flickering Prevention

1. **Double-buffer cell diffing** (ratatui built-in). Emits ANSI codes only for changed cells.
2. **Synchronized output** (DEC private mode 2026). Atomic frame paint.
3. **Render throttling** at ~60 FPS. Batch streaming tokens between frames.
4. **Overwrite, don't clear.** Never erase-then-redraw.
5. **Cached block renders near the viewport.** Each committed block renders once per width, theme, and view when it comes near the viewport, as [Chat Virtualization](#chat-virtualization) describes, and each frame paints only the rows inside the viewport.

## Chat Virtualization

A resize, theme swap, or view toggle invalidates every cached block, so the chat renders only the blocks it is about to paint:

- **Render near the viewport.** A frame renders the stale blocks within a viewport height of the rows it paints, nearest the anchored row first, and places the paint again after each one. It paints once every block in that range is current at the frame's width, theme, and view. Under auto-scroll, the range extends up from the tail.
- **Estimate the rest.** A farther block keeps its last render as its height estimate until it comes into range, and a block that has never rendered counts as one blank row. An estimate above the range shifts every row in it equally, so the painted rows and the bottom clamp stay exact.
- **Anchor on a block.** While scrolled up, a frame names the previous paint's top row by its block and its row within that block. When the block changes height, the row keeps its relative position, so the rows the user was reading stay in place as blocks above them render at a new width. Rows in a live reply are named by their distance from the last committed block, which keeps them in place when the reply commits. After the view toggles or a click expands a block, the row keeps its index in its block, since expanding or collapsing changes only what follows a block's head. Under auto-scroll, the paint stays on the tail, and a paused paint that the change leaves at the bottom follows the tail again.
- **Apply scrolls at render.** Scroll keys and the wheel add to a pending delta that the next frame applies to the anchored row. The render range spans the anchored row and the new top, so a burst of `PageUp` presses between frames lands on the same row as presses one frame apart. `Ctrl+Home` jumps to row 0, and scrolling down to the bottom re-arms auto-scroll.

## Compact and Expanded Views

The chat opens in the compact view, and **Ctrl+O** switches between it and the expanded view:

- **Compact.** A tool body caps its rows and cuts each at 512 bytes. Most tools show their first five rows. An edit's diff shows up to twenty, and a side too long for its share keeps its first rows and its last line on either side of its footer. A task list shows five tasks, starting just before the one in progress, and a list of edit locations shows eight. The hidden rows collapse into an overflow footer, such as `... +3 lines · Ctrl+O to expand`, in `dim`, which says `files` or `tasks` for those lists and adds `(limit reached)` when the tool's own cap also cut them. Committed thinking shows only its `Thought` header. A click on a footer or header expands that block alone, as [Mouse Interactions](mouse-interactions.md#expanding-a-block) describes.
- **Expanded.** Every tool body and thinking block renders in full. A toggle in either direction drops the expansions made by clicks.

Thinking that is still streaming shows its body in both views under the spinner and `Thinking`. A progress note the model wrote before a tool call shows as prose only while `show_thinking` is off, since otherwise the thinking block holds the same text.

## Streaming Markdown

Streaming markdown uses a block-based stable-prefix cache:

- **Commit complete blocks.** A top-level markdown block is complete once another top-level block starts after it. The cache renders complete blocks once into owned `Line<'static>` values, and each frame re-parses only the tail from the last block start.
- **Detect stable boundaries.** Block starts are found in newline-terminated lines only, because a partial line can still change which block it opens. Blank lines inside a fenced code block or a loose list stay in the tail.
- **Carry renderer state.** Each chunk starts from the renderer state the chunk before it ended with, so block separators and the icon row match a single render.
- **Resolve cross-block references on completion.** The one difference from the committed render is a reference-style link whose definition is in another block, which renders literally until the turn commits.

An open code block stays in the tail until the next block starts. Its highlighter resumes after the block's last complete line, so each frame highlights only the new lines, though it still wraps every row of the block.

## Markdown Tables

A table renders as a box in the `table_border` slot, with one cell of padding inside each border and the header row in the `table_header` slot, a bold accent by default. A rule with `├ ┼ ┤` junctions separates every pair of rows. Inside a list item or blockquote, every row carries the item indent or the `│` bar, and the table fits in the width that remains.

Columns keep their natural widths when the table fits. Otherwise each column gets the width of its longest word, up to 30 columns so a long path or URL still wraps, and the remaining width goes to the columns in proportion to how far each falls short of its natural width. Cells word-wrap within their columns. When even those minimum widths don't fit, each body row stacks into `header: value` lines, with a `table_border` rule between rows.

A `<br>` in a cell starts a new line in that cell, since a GFM cell has no other line break.

## Design Decisions

1. **ratatui + crossterm + tokio.** No custom rendering engine.
2. **Concrete component structs.** `App` composes chat, input, status, and modal surfaces directly, with no shared component trait.
3. **Synchronized output by default.** A terminal that supports mode 2026 presents each frame at once, so a large repaint never shows half drawn, and a terminal without it ignores the mode.
4. **Render throttling at ~60 FPS, and a sleeping idle loop.** Tokens can stream faster than a terminal can repaint, so the tick batches them into one frame, which keeps the event loop free for input. The tick runs every frame while a redraw or a modal's page load is pending, and otherwise only at the earliest timed change, such as the busy spinner's next frame, a git probe falling due, the clock segment's next minute, a usage-limit reset, or an expiring notice, so an idle session wakes about once every 5 s, for the branch probe. Each component that changes with time reports its next deadline beside its `tick`.
5. **Block-based markdown commit with stable-prefix cache** during streaming, full render on completion.
6. **Custom pulldown-cmark + syntect renderer.** Uses Codex's `pending_marker` pattern for correct list handling.
7. **Catppuccin Mocha dark theme by default** with named color slots covering text, surfaces, accents, status, code, diff, headings, body, and chrome. Transparent background.
8. **Two-tier tool display.** Inline summary with per-tool icons, plus an output body the compact view caps.
9. **Per-block render cache with viewport-scoped rendering.** Rendering markdown and highlighting code for a whole long session on each resize or theme swap would stall the frame, so only the blocks near the viewport render, and anchoring the scroll offset on a block keeps the rows the user is reading in place while farther blocks catch up.
10. **A rule between every table row.** Without one, the continuation lines of a wrapped row leave blank cells that read as spacing, so the single-line rows around it seem to run together. Claude Code and oh-my-pi rule off every row of their boxed tables, as Codex does in its borderless ones.
11. **Table columns no narrower than their longest word.** A narrower column splits words mid-way, so a flag such as `--all-targets` breaks across rows. When the words can't all fit side by side, the rows stack as `header: value` lines. Claude Code and oh-my-pi also start each column at its longest word, and Claude Code stacks rows the same way once wrapped rows grow too tall.
12. **One toggle for every collapsed block.** Claude Code's Ctrl+O transcript viewer expands lines that collapse by default, and its collapsed thinking names the key. Codex ends truncated output with the key that opens its transcript. A toggle in place keeps the reader's position in the chat, so one key serves tool bodies and thinking, and each footer names it.

## Sources

- `crates/ox-tui/src/action.rs`: `APP_KEYMAP`, the app's named keys and their hints.
- `crates/ox-tui/src/app.rs`: `App`, its `tokio::select!` loop, `TICK_INTERVAL`, the shortest gap between ticks, and `App::next_tick`, which picks when the loop wakes.
- `crates/ox-tui/src/components/chat.rs`: `ChatView` and the compact and expanded views.
- `crates/ox-tui/src/components/chat/blocks.rs`: `ChatBlock` and `RenderCtx`, the render contract of a chat block.
- `crates/ox-tui/src/components/chat/blocks/streaming.rs`: `StreamingAssistant`, the stable-prefix cache of a streaming reply.
- `crates/ox-tui/src/components/chat/scroll.rs`: `ChatView::place`, the anchored placement each frame paints, and the auto-scroll that follows the tail.
- `crates/ox-tui/src/components/chat/stack.rs`: `CachedBlock`, `SlotTops`, and `Stack`, the block cache and the row anchors.
- `crates/ox-tui/src/keymap.rs`: `KeyAction`, `KeyChord`, and `Keymap`.
- `crates/ox-tui/src/markdown.rs`: `render_markdown_chunk` and `complete_blocks_len`, the stable block boundaries.
- `crates/ox-tui/src/markdown/render.rs`: the pulldown-cmark renderer, its `pending_markers`, and the table layout.
- `crates/ox-tui/src/terminal.rs`: `synchronized_update`, which wraps each frame in mode 2026.
