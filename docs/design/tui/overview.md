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
    event = self.agent_rx.recv()          => { /* AgentEvent: stream tokens, tool calls, session updates */ }
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
6. **Cached block renders.** Each committed block renders once per width and theme, and each frame paints only the rows inside the viewport.

## Streaming Markdown

Block-based commit with a stable-prefix cache. A top-level markdown block is complete once another top-level block starts after it, so the cache renders complete blocks once into owned `Line<'static>` values and each frame re-parses only the tail from the last block start. Block starts are found in newline-terminated lines only, because a partial line can still change which block it opens. Blank lines inside a fenced code block or a loose list stay in the tail. Each chunk starts from the renderer state the chunk before it ended with, so block separators and the icon row match a single render. The one difference from the committed render is a reference-style link whose definition is in another block, which renders literally until the turn commits.

An open code block stays in the tail until the next block starts. Its highlighter resumes after the block's last complete line, so each frame highlights only the new lines, though it still wraps every row of the block.

## Design Decisions

1. **ratatui + crossterm + tokio.** No custom rendering engine.
2. **Concrete component structs.** `App` composes chat, input, status, and modal surfaces directly, with no shared component trait.
3. **Synchronized output** enabled by default.
4. **Render throttling at ~60 FPS**.
5. **Block-based markdown commit with stable-prefix cache** during streaming, full render on completion.
6. **Custom pulldown-cmark + syntect renderer.** Uses Codex's `pending_marker` pattern for correct list handling.
7. **Catppuccin Mocha dark theme by default** with named color slots covering text, surfaces, accents, status, code, diff, headings, body, and chrome. Transparent background.
8. **Two-tier tool display.** Inline summary with per-tool icons, plus truncated output body.
9. **Per-block render cache with visible-slice paint.** A resize or theme swap still re-renders every block.
