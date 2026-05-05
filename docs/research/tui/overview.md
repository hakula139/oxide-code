# Terminal UI (Reference)

Research on TUI patterns across reference projects. Based on [Claude Code](https://github.com/hakula139/claude-code) (v2.1.87), [OpenAI Codex](https://github.com/openai/codex), and [opencode](https://github.com/anomalyco/opencode).

## Claude Code (TypeScript / Ink)

Custom fork of Ink — React-based terminal rendering with Yoga Flexbox layout.

- **Streaming**: `StreamingMarkdown` splits at last block boundary. Monotonic `useRef` boundary — only the final block re-parsed per delta, O(1) amortized. LRU token cache (500 entries). Fast-path regex skip for non-markdown text.
- **Thinking**: Collapsed by default (`"... Thinking"` dim italic). Ctrl+O to expand.
- **Tools**: Polymorphic — each tool provides `renderToolUseMessage()` / `renderToolUseProgressMessage()`. Status dot animation.
- **Input**: Full vim emulation, command autocomplete, image paste.
- **Scroll**: `ScrollBox` bypasses React for scroll. Height cache per message. `React.memo` on static content. `OffscreenFreeze`.
- **Flickering**: Full-screen redraw on every state change causes severe flicker in long sessions (anthropics/claude-code#1913).

## OpenAI Codex (Rust / ratatui)

Markdown renderer (`tui/markdown_render.rs`) was the primary reference for oxide-code's pulldown-cmark renderer.

Key insight: **deferred list marker** (`pending_marker`) pattern. When `Start(Item)` arrives, store the marker and wait for the next content event to emit both on the same line. Solves the "loose list" problem where pulldown-cmark wraps content in `<p>` tags.

State: `list_stack` (nesting + counters), `indent_stack`, `inline_styles` (style modifiers stack), `pending_marker` (consumed on next text event).

## opencode (TypeScript / @opentui + Solid.js)

Fine-grained reactivity via Solid.js signals — only the specific text node receiving a new token re-renders.

- **Markdown**: tree-sitter WASM parsers for highlighting (~20 languages). Two modes: `<code filetype="markdown">` and `<markdown>` (experimental). Concealment toggle hides syntax characters.
- **Tools**: Two-tier — `InlineTool` (one-liner with icon prefix) and `BlockTool` (bordered panel with title/body). Bash capped at 10 lines, generic at 3.
- **Thinking**: Left `|` border, 60% opacity, toggle-able.
- **Input**: Extmarks, prompt stash, `$EDITOR` integration, shell mode (`!` at position 0).
- **Themes**: 30+ with auto dark/light detection via ANSI OSC 11 query.

## Reference Apps

| App                                                        | Relevance                                                     |
| ---------------------------------------------------------- | ------------------------------------------------------------- |
| [yazi](https://github.com/sxyazi/yazi)                     | Async file manager — image previews, Lua plugins, themes      |
| [atuin](https://github.com/atuinsh/atuin)                  | Shell history — fuzzy search UI, SQLite, large datasets       |
| [gitui](https://github.com/gitui-org/gitui)                | Git TUI — state management, diff rendering, keybinding system |
| [serie](https://github.com/lusingander/serie)              | Git commit graph — creative visual rendering                  |
| [television](https://github.com/alexpasmantier/television) | Fuzzy finder — extensible data sources, async matching        |

## Flickering Prevention Techniques

1. **Double-buffer cell diffing** (ratatui built-in) — only changed cells emit ANSI.
2. **Synchronized output** (DEC 2026) — atomic frame paint. Supported by: Alacritty, Warp, Windows Terminal, tmux, kitty, iTerm2.
3. **Render throttling** — cap to ~60 FPS (16 ms).
4. **Overwrite, don't clear** — no intermediate blank states.
5. **Hidden cursor during render**.
6. **Viewport virtualization** — only render visible lines.
