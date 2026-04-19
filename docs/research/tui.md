# Terminal UI Research

Research findings for the oxide-code TUI, based on analysis of reference projects (claude-code, opencode, codex-rs), the Rust TUI ecosystem, and the terminal flickering problem.

## Reference Projects

### claude-code (TypeScript / Ink)

claude-code uses a **custom fork of Ink** — a React-based terminal rendering engine with a custom reconciler. The rendering pipeline is: React component tree → Yoga Flexbox layout (pure TypeScript, no C++ bindings) → screen buffer diff → minimal ANSI output.

#### Key Patterns

- Streaming-first components — every component handles partial / streaming data. Text tokens accumulate in React state; tool call JSON is parsed incrementally on each delta.
- Double-buffered frames — `frontFrame` / `backFrame` buffers, diffing to emit only changed cells. Reduces terminal I/O but doesn't eliminate flicker because React's reconciler still triggers full tree traversals on every state change.
- ANSI parser as React component — an `Ansi` component converts raw escape sequences from shell output into React-compatible `Text` spans.
- Collapsible tool groups — `CollapsedReadSearchContent` groups repeated tool calls (e.g., multiple file reads) into a single expandable row.
- Glimmer animation — `GlimmerMessage` renders a shimmering progress indicator with elapsed time for long-running operations.
- Theme system — CSS-like theming via `ThemedBox` / `ThemedText` with terminal color adaptation.

Full-screen redraw on every React state change causes severe flickering in long sessions (anthropics/claude-code#1913 — 315 reactions). This is Ink's fundamental limitation.

#### Streaming Markdown

`Markdown.tsx`, `utils/markdown.ts`

- Two-layer hybrid: `marked` lexer for tokenization, `chalk` for ANSI styling, `cli-highlight` (lazy-loaded via Suspense) for syntax highlighting in code blocks.
- `StreamingMarkdown` splits content at the last _top-level block boundary_ (not line). Maintains a monotonic `useRef` boundary — only the final growing block is re-parsed per delta, giving O(1) amortized cost regardless of total text length.
- Module-level LRU token cache (500 entries, keyed by content hash) avoids re-parsing on virtual-scroll remount (~3 ms per `marked.lexer` call saved).
- Fast-path regex check: scans first 500 chars for markdown syntax; if none found, skips the lexer entirely and returns a single paragraph token.
- Tables are extracted and rendered as React components with flexbox layout; all other content is concatenated into ANSI strings and rendered via `<Ansi>`.

#### Thinking Display

`AssistantThinkingMessage.tsx`

- Collapsed by default: shows `"∴ Thinking"` in dim italic as a single line, with a `Ctrl+O` expand hint. Only expanded in verbose / transcript mode.
- When expanded: `"∴ Thinking..."` header, then full thinking content rendered via `<Markdown dimColor>` with `paddingLeft={2}`.

#### Tool Display

`AssistantToolUseMessage.tsx`

- Highly polymorphic: each `Tool` object provides its own `renderToolUseMessage()`, `renderToolUseProgressMessage()`, and `renderToolUseQueuedMessage()`.
- Status dot: dim `●` (queued) → animated spinner (in progress) → error state.
- Tool name rendered bold, optional background color and tags (timeout, model, resume ID).
- Some tools are "transparent wrappers" — hide their name and show only progress.

#### Input

`PromptInput.tsx` (~2300 lines)

- Full vim emulation via `src/vim/` module (motions, operators, text objects, mode transitions).
- Command autocomplete with typeahead suggestions, slash commands.
- Arrow-key history navigation, history search.
- Image paste detection from clipboard.

#### Virtual Scrolling

`VirtualMessageList.tsx`, `ScrollBox.tsx`

- `ScrollBox` bypasses React for scroll — `scrollTo` / `scrollBy` mutate DOM directly and schedule a throttled render. No React state per wheel event.
- Height cache per message, invalidated on terminal width change. Viewport culling — only visible children rendered.
- `React.memo` on `LogoHeader` prevents dirty-flag cascade through all `MessageRow` siblings (critical for long sessions — without it, 150K+ writes per frame).
- `OffscreenFreeze` wraps static content to prevent re-renders. `useDeferredValue` for non-critical state updates.

### opencode (TypeScript / @opentui + Solid.js)

opencode uses **@opentui/core** with **Solid.js** for fine-grained reactive terminal rendering. (Note: despite early documentation suggesting Go / Bubble Tea, the current implementation is a TypeScript monorepo.)

#### Key Patterns

- Fine-grained reactivity — Solid.js signals trigger surgical updates; only the specific text node receiving a new token re-renders, not the entire component tree. This avoids the redraw problem that plagues Ink.
- SDK event-driven streaming — typed events (`message.part.updated`) applied to a Solid store via `produce()`. Dependent `createMemo` computations and UI nodes update automatically.
- 30+ themes with auto-detection — JSON-defined themes with dark / light detection via ANSI OSC 11 query. Adaptive foreground contrast calculation. Theme priority: defaults < plugins < custom files < system.
- Leader-key input — default `Ctrl+X` prefix for extended keybinds, reducing conflicts with terminal and shell bindings.
- Plugin system — full API with command registration, custom routes, theme injection, and slot-based extension points.
- Scroll acceleration — macOS-aware scroll speed with configurable acceleration curves.
- Responsive layout — width breakpoint at 120 columns, sidebar toggling, `contentWidth = width - (sidebarVisible ? 42 : 0) - 4`.

#### Markdown Rendering

`routes/session/index.tsx`

- Uses tree-sitter WASM parsers for syntax highlighting (~20 languages declared in `parsers-config.ts`).
- Two rendering modes: `<code filetype="markdown">` (standard) and `<markdown>` (experimental, behind a feature flag). Both accept `streaming={true}` for incremental parsing.
- Concealment: toggle to hide markdown syntax characters (e.g., `**` for bold) — saves horizontal space.
- Dedicated theme colors for each markdown element: `markdownHeading`, `markdownCode`, `markdownBlockQuote`, `markdownEmph`, etc.

#### Tool Display

`routes/session/index.tsx`

- Two-tier pattern:
  - `InlineTool`: compact one-liner with icon prefix (`→` read, `←` write, `$` bash, `✱` glob, `⌕` grep, `⚙` generic). Pending state shows `~ message` with spinner. Denied permissions render with strikethrough.
  - `BlockTool`: bordered panel (`┃` left border) with title, body content, and hover background. Used for tools with output.
- Bash output capped at 10 lines with expand / collapse. Generic tools capped at 3 lines.
- Entire tool detail layer is toggle-able via keybind — when hidden, completed tools vanish entirely.

#### Thinking Display

`routes/session/index.tsx`

- Left `┃` border in `backgroundElement` color (subtler than tool borders).
- Content rendered at 60% opacity via `subtleSyntax()` — same syntax rules but with alpha-reduced foreground colors.
- Prefixed with italic `_Thinking:_`. `[REDACTED]` tokens stripped.
- Toggle-able via keybind or `/thinking` command.

#### Input

`component/prompt/index.tsx` (~1280 lines)

- Extmarks — virtual inline text markers for file references (`[Image 1]`), agent mentions, pasted text (`[Pasted ~N lines]`). Expanded inline on submit.
- Prompt stash — push / pop prompt content for later use (switch context without losing draft).
- `$EDITOR` integration — opens external editor with current prompt, reconciles extmark positions on return.
- Shell mode entered by typing `!` at position 0.
- `Meta+Enter` for newline (vs Shift+Enter).

#### Footer

`routes/session/footer.tsx`

- Left: working directory. Right: LSP count (`• N LSP`), MCP count (`⊙ N MCP`) with error coloring, permission warnings, `/status` hint.
- Subagent footer shows agent label, sibling index (e.g., "3 of 5"), token usage, parent / prev / next navigation.

### codex-rs (Rust / ratatui)

[codex-rs](https://github.com/openai/codex) (`codex/codex-rs/`) is the Rust TUI for OpenAI's Codex terminal agent. Its markdown renderer (`tui/markdown_render.rs`) was the primary reference for oxide-code's custom pulldown-cmark renderer.

#### Markdown Rendering — `pending_marker` Pattern

`tui/markdown_render.rs`

The key insight is a **deferred list marker** approach for correct list item rendering. When a `Start(Item)` event arrives, the renderer does not emit the marker (`1.`, `-`) immediately. Instead, it stores the marker in a `pending_marker` field and waits for the next content event (`Text`, `Code`, etc.) to emit both the marker and content on the same line. This solves the "loose list" problem where pulldown-cmark wraps list item content in `<p>` tags, causing naïve renderers to place the marker and content on separate lines.

State management:

- `list_stack` — tracks nesting depth and item counters (ordered vs. unordered).
- `indent_stack` — accumulated indent string per nesting level (e.g., `"   "` for each level).
- `inline_styles` — stack of active `Style` modifiers, pushed on `Start(Emphasis)` / `Start(Strong)` / etc., popped on corresponding `End`.
- `pending_marker` — `Option<String>` holding the deferred list marker. Consumed and prepended when the next text-bearing event arrives.

Other patterns: fenced code blocks are buffered entirely and syntax-highlighted on `End(CodeBlock)` via syntect. Inline code uses a distinct foreground color. Headings are styled per level (H1–H6). Blockquotes use a `▎` left border with dimmed style.

## Reference Apps

Actively maintained, visually impressive ratatui apps to study for patterns.

| App                                                        | Relevance                                                                                  |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| [yazi](https://github.com/sxyazi/yazi)                     | Async file manager — image previews, Lua plugin system, theme system, responsive layout    |
| [atuin](https://github.com/atuinsh/atuin)                  | Shell history search — fuzzy search UI, SQLite integration, large dataset handling         |
| [gitui](https://github.com/gitui-org/gitui)                | Git TUI — complex state management, diff rendering, multi-pane layout, keybinding system   |
| [serie](https://github.com/lusingander/serie)              | Git commit graph — creative visual rendering, terminal image protocol                      |
| [television](https://github.com/alexpasmantier/television) | Fuzzy finder — extensible data source pattern, async fuzzy matching, provider architecture |

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

| Crate              | Purpose                                                                            |
| ------------------ | ---------------------------------------------------------------------------------- |
| `pulldown-cmark`   | CommonMark parser — event-driven iterator over markdown elements (custom renderer) |
| `syntect`          | Syntax highlighting for fenced code blocks (lazy-loaded `SyntaxSet` / `ThemeSet`)  |
| `ratatui-textarea` | Multi-line text input widget with cursor, selection, undo / redo                   |

### Architecture Pattern: Component Trait

Our simplified variant of the pattern from ratatui's official templates:

```text
trait Component {
    fn handle_event(&mut self, event: &Event) -> Option<Action>;
    fn render(&self, frame: &mut Frame, area: Rect);
}
```

Each component owns its state, handles its events, and renders into a given area. The root `App` dispatches events top-down and collects actions bottom-up. We omit `init()` and `update()` — state mutations happen directly in event handlers, keeping the interface minimal.

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

1. **Line-based commit with stable-prefix cache**: Buffer tokens, commit to the rendered view at `\n` boundaries. Track a monotonic byte boundary — only lines beyond the cached boundary are re-parsed. The stable prefix is rendered once and stored as owned `Line<'static>` values. This gives O(new lines) per token instead of O(total text). Adopted by oxide-code; inspired by claude-code's block-level variant.
2. **Block-level commit** (claude-code): Same idea but at pulldown-cmark block boundaries instead of line boundaries. Theoretically more precise (a code fence mid-line is still one block) but requires deeper parser integration.
3. **Code block handling**: Buffer entire code blocks before applying syntax highlighting (avoids partial-highlight flicker), or apply a simple monospace style during streaming and re-highlight on block completion.

Adopted: line-based commit with stable-prefix cache. Upgrade to block-level boundaries when viewport virtualization is added.

## Design Decisions for oxide-code

Based on the research, the following decisions guide the TUI implementation:

1. **ratatui + crossterm + tokio** as the core stack. No custom rendering engine — leverage ratatui's battle-tested double-buffer diffing.
2. **Component trait pattern** for UI architecture. Each view (chat, input, status, tool display) is a self-contained component.
3. **Synchronized output** enabled by default. Wrap every frame in DEC 2026 sequences.
4. **Render throttling at ~60 FPS**. Batch streaming tokens between frames.
5. **Line-based markdown commit with stable-prefix cache** during streaming, full re-render on message completion. Monotonic boundary avoids O(n) re-parsing.
6. **Custom pulldown-cmark + syntect renderer** instead of `tui-markdown`. The external crate had incorrect loose-list rendering (marker and content on separate lines) and limited control over styling. The custom renderer uses codex-rs's `pending_marker` pattern for correct list handling and gives full control over heading styles, blockquote borders, and inline formatting.
7. **Catppuccin Mocha dark theme by default** with 11 named color slots. Transparent background to respect user's terminal theme.
8. **Two-tier tool display** — inline summary with per-tool icons, plus truncated output body (inspired by opencode's InlineTool / BlockTool pattern). Truncation at 5 lines with overflow count.
9. **Viewport virtualization** for long conversations — only render visible messages (planned).
