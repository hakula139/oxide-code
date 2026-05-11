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
    event = crossterm_events.next() => { /* keyboard, mouse, resize */ }
    token = stream_rx.recv()        => { /* LLM streaming token */ }
    result = tool_rx.recv()         => { /* tool execution result */ }
    _ = tick_interval.tick()        => { /* animation frame */ }
}
```

## Flickering Prevention

1. **Double-buffer cell diffing** (ratatui built-in). Emits ANSI codes only for changed cells.
2. **Synchronized output** (DEC private mode 2026). Atomic frame paint.
3. **Render throttling** at ~60 FPS. Batch streaming tokens between frames.
4. **Overwrite, don't clear.** Never erase-then-redraw.
5. **Hidden cursor during render**.
6. **Viewport virtualization.** Only render visible lines (deferred).

## Streaming Markdown

Line-based commit with a stable-prefix cache: buffer tokens and commit at `\n` boundaries, tracking a monotonic byte boundary so only lines past the cached boundary get re-parsed. The stable prefix is stored as owned `Line<'static>` values, which keeps the hot path at O(new lines) per token.

Code blocks: buffer entire block, apply syntax highlighting on completion.

## Design Decisions

1. **ratatui + crossterm + tokio.** No custom rendering engine.
2. **Component trait.** Self-contained views (chat, input, status, tool display).
3. **Synchronized output** enabled by default.
4. **Render throttling at ~60 FPS**.
5. **Line-based markdown commit with stable-prefix cache** during streaming, full re-render on completion.
6. **Custom pulldown-cmark + syntect renderer.** Uses Codex's `pending_marker` pattern for correct list handling.
7. **Catppuccin Mocha dark theme by default** with named color slots covering text, surfaces, accents, status, code, diff, headings, body, and chrome. Transparent background.
8. **Two-tier tool display.** Inline summary with per-tool icons, plus truncated output body.
9. **Viewport virtualization** for long conversations (deferred).
