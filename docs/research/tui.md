# Terminal UI Research

Research findings for the oxide-code TUI, based on analysis of reference projects (claude-code, opencode), the Rust TUI ecosystem, and the terminal flickering problem.

## Reference Projects

### claude-code (TypeScript / Ink)

claude-code uses a **custom fork of Ink** — a React-based terminal rendering engine with a custom reconciler. The rendering pipeline is: React component tree → Yoga Flexbox layout (pure TypeScript, no C++ bindings) → screen buffer diff → minimal ANSI output.

**Key patterns**:

- **Streaming-first components**: Every component is designed to handle partial / streaming data. Text tokens accumulate in React state; tool call JSON is parsed incrementally on each delta.
- **Double-buffered frames**: The Ink instance maintains `frontFrame` and `backFrame` buffers, diffing them to emit only changed cells. This reduces terminal I/O but doesn't eliminate flicker because React's reconciler still triggers full tree traversals on every state change.
- **ANSI parser as React component**: An `Ansi` component converts raw escape sequences from shell output into React-compatible `Text` spans, bridging imperative terminal output into the declarative component model.
- **Collapsible tool groups**: `CollapsedReadSearchContent` groups repeated tool calls (e.g., multiple file reads) into a single expandable row, keeping the chat history scannable.
- **Glimmer animation**: `GlimmerMessage` renders a shimmering progress indicator with elapsed time for long-running operations.
- **Theme system**: CSS-like theming via `ThemedBox` / `ThemedText` components with terminal color adaptation.

**Weakness**: Full-screen redraw on every React state change causes severe flickering in long sessions (anthropics/claude-code#1913 — 315 reactions). This is Ink's fundamental limitation.

### opencode (TypeScript / @opentui + Solid.js)

opencode uses **@opentui/core** with **Solid.js** for fine-grained reactive terminal rendering.

**Key patterns**:

- **Fine-grained reactivity**: Solid.js signals trigger surgical updates — only the specific text node receiving a new token re-renders, not the entire component tree. This avoids the redraw problem that plagues Ink.
- **SDK event-driven streaming**: The SDK emits typed events (`message.part.updated`), which the Session component catches and applies to a Solid store via `produce()`. Dependent `createMemo` computations and UI nodes update automatically.
- **30+ themes with auto-detection**: JSON-defined themes with dark / light detection via ANSI OSC 11 query. Adaptive foreground contrast calculation. Theme priority: defaults < plugins < custom files < system.
- **Leader-key input**: Default `Ctrl+X` prefix for extended keybinds, reducing conflicts with terminal and shell bindings.
- **Plugin system**: Full plugin API with command registration, custom routes, theme injection, and slot-based extension points (home footer, sidebar panels, session routes).
- **Scroll acceleration**: macOS-aware scroll speed with configurable acceleration curves.
- **Responsive layout**: Width breakpoint at 120 columns, sidebar toggling, `contentWidth = width - (sidebarVisible ? 42 : 0) - 4`.

## Flickering Prevention

The terminal flickering problem (anthropics/claude-code#1913) affects most CLI-based AI assistants as conversations grow. Root cause: full-screen redraws during high-frequency streaming updates.

### Techniques (Ordered by Impact)

1. **Double-buffer cell diffing** (ratatui built-in) — Maintains previous and current frame buffers, emits ANSI codes only for changed cells. This is the single most effective technique and comes free with ratatui's `Terminal::draw()`.

2. **Synchronized output** (DEC private mode 2026) — Wraps each frame in `ESC[?2026h` ... `ESC[?2026l`, telling the terminal to queue updates and paint atomically. Supported by: Alacritty, Warp, Windows Terminal, tmux, kitty, iTerm2, Contour. crossterm exposes this as `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`.

3. **Render throttling** — Cap render frequency to ~60 FPS (16 ms intervals). During streaming, tokens arrive faster than the eye can follow; batching multiple token events into one frame reduces CPU and terminal I/O without perceptible delay.

4. **Overwrite, don't clear** — Write full content in a single pass without intermediate blank states. Never erase-then-redraw; always overwrite-in-place.

5. **Hidden cursor during render** — Hide cursor before frame output, restore after. Prevents visible cursor jumping.

6. **Viewport virtualization** — Only render lines visible in the current viewport. Long conversations with hundreds of messages should not re-layout off-screen content.

## Rust TUI Ecosystem

### Core Stack

| Crate                                     | Purpose                                                            |
| ----------------------------------------- | ------------------------------------------------------------------ |
| `ratatui`                                 | Terminal UI framework — layout, widgets, double-buffer rendering   |
| `crossterm` (with `event-stream` feature) | Backend — async terminal events, ANSI output, synchronized updates |
| `tokio`                                   | Async runtime for streaming, tool execution, event multiplexing    |

### Rendering & Content

| Crate                                          | Purpose                                                      |
| ---------------------------------------------- | ------------------------------------------------------------ |
| `tui-markdown` (with `highlight-code` feature) | Markdown → ratatui widget, uses pulldown-cmark + syntect     |
| `syntect`                                      | Syntax highlighting for code blocks (Sublime Text grammar)   |
| `ansi-to-tui`                                  | Convert raw ANSI output (from shell tools) to ratatui Styles |

### Input & Interaction

| Crate          | Purpose                                                          |
| -------------- | ---------------------------------------------------------------- |
| `tui-textarea` | Multi-line text input widget with cursor, selection, undo / redo |

### Visual Polish

| Crate                  | Purpose                                                 |
| ---------------------- | ------------------------------------------------------- |
| `throbber-widgets-tui` | Spinners and activity indicators (braille dot patterns) |

### Architecture Pattern: Component Trait

The recommended pattern from ratatui's official templates (and used by gitui, bottom, etc.):

```text
trait Component {
    fn init(&mut self) -> Result<()>;
    fn handle_event(&mut self, event: Event) -> Result<Option<Action>>;
    fn update(&mut self, action: Action) -> Result<Option<Action>>;
    fn render(&self, frame: &mut Frame, area: Rect);
}
```

Each component owns its state, handles its events, and renders into a given area. The root `App` dispatches events top-down and collects actions bottom-up.

### Async Integration Pattern

```text
tokio::select! {
    event = crossterm_events.next() => { /* keyboard, mouse, resize */ }
    token = stream_rx.recv()        => { /* LLM streaming token */ }
    result = tool_rx.recv()         => { /* tool execution result */ }
    _ = tick_interval.tick()        => { /* animation frame */ }
}
```

This multiplexes all event sources into a single loop. Render is triggered after any event that mutates state, throttled to the tick interval.

### Streaming Markdown Strategy

Tokens arrive mid-syntax (e.g., `**part` then `ial**`). Approaches:

1. **Line-based commit**: Buffer tokens, commit to the rendered view at `\n` boundaries. Prevents mid-tag visual glitches. Simple and effective.
2. **Incremental parse**: `pulldown-cmark` supports event-based parsing. Process events as they arrive, re-parse the trailing incomplete line on each frame.
3. **Code block handling**: Buffer entire code blocks before applying syntax highlighting (avoids partial-highlight flicker), or apply a simple monospace style during streaming and re-highlight on block completion.

Recommendation: line-based commit for prose, buffered re-highlight for code blocks.

## Reference Apps

Actively maintained, visually impressive ratatui apps to study for patterns.

| App                                                        | Relevance                                                                                        |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| [Codex](https://github.com/openai/codex)                   | Terminal AI coding agent — streaming token display, agent state machine, closest to our use case |
| [yazi](https://github.com/sxyazi/yazi)                     | Async file manager — image previews, Lua plugin system, theme system, responsive layout          |
| [atuin](https://github.com/atuinsh/atuin)                  | Shell history search — fuzzy search UI, SQLite integration, large dataset handling               |
| [gitui](https://github.com/gitui-org/gitui)                | Git TUI — complex state management, diff rendering, multi-pane layout, keybinding system         |
| [serie](https://github.com/lusingander/serie)              | Git commit graph — creative visual rendering, terminal image protocol                            |
| [television](https://github.com/alexpasmantier/television) | Fuzzy finder — extensible data source pattern, async fuzzy matching, provider architecture       |

## Design Decisions for oxide-code

Based on the research, the following decisions guide the TUI implementation:

1. **ratatui + crossterm + tokio** as the core stack. No custom rendering engine — leverage ratatui's battle-tested double-buffer diffing.
2. **Component trait pattern** for UI architecture. Each view (chat, input, status, tool display) is a self-contained component.
3. **Synchronized output** enabled by default. Wrap every frame in DEC 2026 sequences.
4. **Render throttling at ~60 FPS**. Batch streaming tokens between frames.
5. **Line-based markdown commit** during streaming, full re-render on message completion.
6. **Dark theme by default** with a curated palette (4–6 colors). Light theme as an option, detected via OSC 11 or config.
7. **Collapsible tool groups** for repeated operations (inspired by claude-code's `CollapsedReadSearchContent`).
8. **Viewport virtualization** for long conversations — only render visible messages.
