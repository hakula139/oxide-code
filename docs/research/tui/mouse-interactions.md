# Mouse Interactions (Reference)

Research on mouse handling in terminal AI CLIs: capture defaults, click affordances, wheel scroll, text selection, copy-on-select strategies, and URL openers.

## Claude Code

The most polished mouse layer of the three peers. Claude Code's `src/utils/fullscreen.ts` reads `CLAUDE_CODE_DISABLE_MOUSE` to gate the entire mouse pipeline, with a separate `CLAUDE_CODE_DISABLE_MOUSE_CLICKS` env var that lets wheel work while blocking click events.

The mode bundle enabled by `src/ink/termio/dec.ts` is `?1000h`, `?1002h`, `?1003h`, `?1006h` — same as crossterm's `EnableMouseCapture`. Disabled via the matching `…l` set on suspend / exit.

Hit-testing lives in `src/ink/hit-test.ts`. Each render builds a Yoga DOM with rect-per-node, then `dispatchClick` bubbles from the deepest hit up the parent chain until `stopImmediatePropagation()`. Clickable elements include the jump-to-bottom pill (`FullscreenLayout.tsx:491`), expand / collapse on message rows (`VirtualMessageList.tsx:225`), background-task agent pills (`BackgroundTaskStatus.tsx:155`), and OSC 8 hyperlinks via `<Link url={...}>`.

Selection uses an in-process model in `src/ink/selection.ts`. Drag-start / update / finish / clear plus double-click word and triple-click line are handled in app-side code. `useCopyOnSelect` writes the selection to the OS clipboard on mouse-up via OSC 52, `pbcopy`, or `tmux load-buffer` depending on environment. `<NoSelect>` marks gutter cells (line numbers, diff sigils) as non-selectable so drag-copy yields clean text.

URL opening: `src/utils/browser.ts` validates the URL to `http:` / `https:` first, then dispatches: `BROWSER` env override, else `rundll32 url,OpenURL` (Windows), `open` (macOS), `xdg-open` (Linux). Single-click on an OSC 8 hyperlink defers 500 ms so a second click within the window can start a word-selection drag instead.

## OpenAI Codex

Codex's Rust TUI does **not** enable `EnableMouseCapture`. `set_modes()` in `codex-rs/tui/src/tui.rs` enables `EnableBracketedPaste`, `enable_raw_mode`, `KeyboardEnhancement`, and `EnableFocusChange`, but skips mouse. The event mapper at `event_stream.rs` explicitly drops mouse events with a doc comment "skipping events we don't use (mouse events, etc.)".

Wheel scroll uses DECSET `?1007` "alternate scroll" enabled in `tui.rs:621`, which tells the terminal emulator to translate physical wheel events into `\x1b[A` / `\x1b[B` arrow-key sequences. Codex receives them as keyboard events and never sees raw mouse. Trade-off: it works without claiming click / drag, but it loses every other mouse affordance.

Click on URLs is handled via OSC 8: `set_status_line_hyperlink(url)` at `chatwidget.rs:1684` and `bottom_pane/mod.rs:1584` wrap the open-PR URL on the status line. `mark_url_hyperlink(buf, area, url)` is the helper that overlays OSC 8 cells across a ratatui buffer rect. The terminal's own Ctrl-click handler opens the URL — no app-side click routing.

URL opening fallback uses the `webbrowser = "1.0"` crate via `webbrowser::open(&url)` at `app/history_ui.rs`. Triggered by an internal `AppEvent::OpenUrlInBrowser { url }` event for plugin auth and app-link views, but not for the OSC 8 hyperlinks (those go through the terminal).

No selection support, no copy-on-select. Native terminal selection works because mouse capture isn't on.

## opencode

opencode is built on opentui (TypeScript / SolidJS), not bubbletea / Go. The `mouse` config field defaults to `true` and combines with `OPENCODE_DISABLE_MOUSE` (env var wins) to set `useMouse` on the opentui renderer config (`app.tsx:120-130`).

The renderer exposes `<box onMouseUp>` / `<box onMouseOver>` element-level events. Click affordances include tool-output expand/collapse (`session/index.tsx:1678`), subagent inline tool navigation (`index.tsx:2055`), revert-message banner, subagent footer nav, question/option dialog rows, permission-dialog options, error-screen copy-issue-URL button. The clickable `<Link href>` component fires `open(href)` from the npm `open` package on mouse-up — no allowlist or sanitization.

Copy-on-select is implemented at `app.tsx:945-953`: `onMouseUp` on the root `<box>` calls `Selection.copy(renderer, toast)` which calls `renderer.getSelection().getSelectedText()` then `Clipboard.copy()`. Default-on; can flip to right-click-to-copy + Ctrl+C-to-copy via `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT` (default-on for Win32).

Wheel is handled inside opentui's `<scrollbox>` primitive with `stickyScroll={true}` and configurable `scroll_speed` / `scroll_acceleration`. When `mouse = false`, the renderer receives no events and wheel scroll is lost — there is no fallback to `?1007` alternate-scroll.

No documented escape hatch for native terminal selection while mouse is captured.

## OSC 52 protocol

`\x1b]52;Pc;Pd\x07` where `Pc` is a clipboard selector (`c` = system clipboard, `p` = primary, `s` = selection, `q` = q-clipboard, `0`-`7` = numbered cut-buffers, with `c` being the most-supported choice) and `Pd` is base64-encoded text. The terminal decodes and writes to its OS-clipboard handler.

Payload caps:

- **xterm**: 8 KB pre-base64 (with `allowWindowOps` enabled). Default off. `~/.Xresources`: `XTerm*allowWindowOps: true`.
- **kitty**: 64 KB. Enabled by default since 0.21 via `clipboard_control write-clipboard`.
- **iTerm2**: ~74 KB. Enabled by default.
- **WezTerm**, **Alacritty**, **foot**, **Ghostty**: enabled by default, multi-MB caps.
- **Windows Terminal**: enabled by default.
- **tmux**: `set -g set-clipboard on` (default in 3.2+) passes the OSC through to the outer terminal. tmux 2.6+ can also handle the OSC itself with `set-clipboard external`.

Failure modes: rejected payloads are silently dropped. The app cannot detect support; the user gets no clipboard write and no error. Falling back to native clipboard requires a separate channel like the `arboard` crate.

## OSC 8 protocol

`\x1b]8;params;URI\x1b\\<text>\x1b]8;;\x1b\\` where `params` is `key=value:key=value` (often empty) and `URI` is the link target. ST is `\x1b\\` (or `\x07` on legacy terminals).

Modern support: iTerm2, WezTerm, kitty, Alacritty, foot, Konsole, Ghostty, recent Windows Terminal, GNOME Terminal, VTE-based terminals. Legacy terminals print the escape bytes literally — the `<text>` part is what users see, so the fallback is graceful as long as the visible text alone is meaningful (e.g., `#NN` works; an empty link doesn't).

`unicode-width` reports 0 for ESC and the printable bytes inside a `]8;;…\\` sequence are also non-printable, so layout math sees the whole sequence as zero-width when wrapped in `Span::raw`. Truncation logic that measures `Span::content` width via `unicode_width::UnicodeWidthStr::width` is unaffected.

## Mouse capture mode bundle

`crossterm::EnableMouseCapture` writes five DECSETs:

- `?1000h` — X10/normal tracking (button press / release).
- `?1002h` — button-event tracking (adds drag while button held).
- `?1003h` — any-event tracking (adds motion without button).
- `?1006h` — SGR encoding (`\x1b[<button;col;rowM`, supports cols > 223).
- `?1015h` — URXVT encoding (legacy fallback).

Some terminals skip `?1003` for performance. SGR (`?1006`) is the only encoding modern crossterm reads, but the others are needed for older terminals that don't speak SGR. There's no portable terminal primitive that delivers wheel only without click / drag, so claiming wheel implies claiming the rest.

## User-environment signal

The author's tmux config enables tmux mouse mode, vi copy-mode bindings, and `y` for yank. With `set -g set-clipboard on`, OSC 52 from the inner app passes through to the outer terminal. Wheel-up enters copy-mode when the pane isn't already receiving mouse events; when oxide-code captures mouse, that gesture goes to oxide-code and tmux doesn't see it. Users who want tmux's wheel-to-copy-mode can `Ctrl-b z` to zoom out, or escape via `Ctrl-b [`.

## Takeaway for oxide-code

Capture mouse and build a small set of well-scoped affordances: jump-to-bottom click, OSC 8 PR hyperlink, drag-select-and-copy via OSC 52. Document the per-terminal selection-modifier escape hatches for content outside the chat (status bar, input box). Defer block selection, drag auto-scroll, double / triple-click word / line, click-to-expand, and an opt-out env var until usage validates the demand.

Claude Code's hit-test framework is the right model long-term but overengineered for a first pass. opencode's per-element click handlers via opentui aren't reachable from ratatui without a similar framework. Codex's "no capture, alternate-scroll" approach trades affordances for native selection — workable but limits future click features.
