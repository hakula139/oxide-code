# Terminal UI

Core stack, rendering strategy, and streaming architecture.

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

### Async event loop

```text
tokio::select! {
    event = crossterm_events.next()       => { /* keyboard, mouse, resize */ }
    event = self.events.recv()            => { /* Event: stream tokens, tool calls, session updates */ }
    Some(update) = self.git_probes.recv() => { /* branch / PR probe results for the status line */ }
    _ = tick.tick()                       => { /* spinner frame, probe scheduling, exit-arm expiry, render when dirty */ }
}
```

## Flickering Prevention

1. **Double-buffer cell diffing** (ratatui built-in). Emits ANSI codes only for changed cells.
2. **Synchronized output** (DEC private mode 2026). Atomic frame paint.
3. **Render throttling** at ~60 FPS. Batch streaming tokens between frames.
4. **Overwrite, don't clear.** Never erase-then-redraw.
5. **Hidden cursor during render**.
6. **Cached block renders near the viewport.** Each committed block renders once per width and theme when it comes near the viewport, as [Chat Virtualization](#chat-virtualization) describes, and each frame paints only the rows inside the viewport.

## Chat Virtualization

A resize or theme swap invalidates every cached block, so the chat renders only the blocks it is about to paint:

- **Render near the viewport.** A frame renders the stale blocks within a viewport height of the rows it paints, nearest the anchored row first, and places the paint again after each one. It paints once every block in that range is current at the frame's width and theme. Under auto-scroll, the range extends up from the tail.
- **Estimate the rest.** A farther block keeps its last render as its height estimate until it comes into range, and a block that has never rendered counts as one blank row. An estimate above the range shifts every row in it equally, so the painted rows and the bottom clamp stay exact.
- **Anchor on a block.** While scrolled up, a frame names the previous paint's top row by its block and its row within that block. When the block changes height, the row keeps its relative position, so the rows the user was reading stay in place as blocks above them render at a new width. Rows in a live reply are named by their distance from the last committed block, which keeps them in place when the reply commits.
- **Apply scrolls at render.** Scroll keys and the wheel add to a pending delta that the next frame applies to the anchored row. The render range spans the anchored row and the new top, so a burst of `PageUp` presses between frames lands on the same row as presses one frame apart. `Ctrl+Home` jumps to row 0, and scrolling down to the bottom re-arms auto-scroll.

## Streaming Markdown

Streaming markdown uses a block-based stable-prefix cache:

- **Commit complete blocks.** A top-level markdown block is complete once another top-level block starts after it. The cache renders complete blocks once into owned `Line<'static>` values, and each frame re-parses only the tail from the last block start.
- **Detect stable boundaries.** Block starts are found in newline-terminated lines only, because a partial line can still change which block it opens. Blank lines inside a fenced code block or a loose list stay in the tail.
- **Carry renderer state.** Each chunk starts from the renderer state the chunk before it ended with, so block separators and the icon row match a single render.
- **Resolve cross-block references on completion.** The one difference from the committed render is a reference-style link whose definition is in another block, which renders literally until the turn commits.

An open code block stays in the tail until the next block starts. Its highlighter resumes after the block's last complete line, so each frame highlights only the new lines, though it still wraps every row of the block.

## Markdown Tables

A table renders as a box in the `table_border` slot, with one cell of padding inside each border and the header row in the `table_header` slot, a bold accent by default. A rule with `├ ┼ ┤` junctions separates every pair of rows. Inside a list item or blockquote, every row carries the item indent or the `>` marker, and the table fits in the width that remains.

Columns keep their natural widths when the table fits. Otherwise each column gets the width of its longest word, up to 30 columns so a long path or URL still wraps, and the remaining width goes to the columns in proportion to how far each falls short of its natural width. Cells word-wrap within their columns. When even those minimum widths don't fit, each body row stacks into `header: value` lines, with a `table_border` rule between rows.

A `<br>` in a cell starts a new line in that cell, since a GFM cell has no other line break.

## Design Decisions

1. **ratatui + crossterm + tokio.** No custom rendering engine.
2. **Concrete component structs.** `App` composes chat, input, status, and modal surfaces directly, with no shared component trait.
3. **Synchronized output** enabled by default.
4. **Render throttling at ~60 FPS**.
5. **Block-based markdown commit with stable-prefix cache** during streaming, full render on completion.
6. **Custom pulldown-cmark + syntect renderer.** Uses Codex's `pending_marker` pattern for correct list handling.
7. **Catppuccin Mocha dark theme by default** with named color slots covering text, surfaces, accents, status, code, diff, headings, body, and chrome. Transparent background.
8. **Two-tier tool display.** Inline summary with per-tool icons, plus truncated output body.
9. **Per-block render cache with viewport-scoped rendering.** Committed blocks render lazily near the viewport, and the scroll offset anchors on a block so blocks above it changing height leave the visible rows in place.
10. **A rule between every table row.** Without one, the continuation lines of a wrapped row leave blank cells that read as spacing, so the single-line rows around it seem to run together. Claude Code and oh-my-pi rule off every row of their boxed tables, as Codex does in its borderless ones.
11. **Table columns no narrower than their longest word.** A narrower column splits words mid-way, so a flag such as `--all-targets` breaks across rows. When the words can't all fit side by side, the rows stack as `header: value` lines. Claude Code and oh-my-pi also start each column at its longest word, and Claude Code stacks rows the same way once wrapped rows grow too tall.
