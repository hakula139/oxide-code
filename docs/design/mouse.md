# Mouse Interactions

The TUI captures the mouse so the scroll wheel works in bare terminals and inside tmux. Because capture intercepts drags, `ox` handles chat text selection and copying itself. Plain clicks expand collapsed blocks (from their overflow footer or `Thought` header) and open pull-request links in the status bar, which are also OSC 8 hyperlinks for Ctrl+click / Cmd+click.

## Setup and Event Routing

`init` enables raw mode, and `enter_tui_mode` enables the alternate screen, bracketed paste, and (outside Windows) Kitty keyboard disambiguation and crossterm's `EnableMouseCapture`. On Windows, `init` enables console mouse input after `enter_tui_mode` and requests SGR mouse reports (`?1006h`) so ConPTY delivers mouse records. Modes unwind in `leave_tui_mode` (and `restore` for Windows mouse).

`App::handle_crossterm_event` routes mouse events:

- **Wheel.** `ChatView::handle_event` scrolls three rows per notch without moving the input cursor. When a modal or slash popup is open, each notch translates to Up or Down.
- **Left button.** A plain press on a status-bar link opens it ([below](#osc-8-hyperlinks-on-the-pr-status-segments)). Other presses, drags, and releases go to `ChatView::handle_mouse` for text selection.
- **Other events.** Return early so motion reports do not trigger frame redraws.

## Mouse Capture

`EnableMouseCapture` writes `?1000h`, `?1002h`, `?1003h`, `?1015h`, and `?1006h`, and crossterm parses SGR (`?1006`) events. Drag selection needs drag reporting, and `?1000h` already disables terminal selection (Windows console input also has no wheel-only mode).

Terminal wheel translation (DECSET 1007) fails under tmux with `mouse on`, because tmux's default `WheelUpPane` forwards wheel events only when the pane reports mouse capture (`#{mouse_any_flag}`), otherwise dropping them or entering copy mode. With capture on, tmux forwards both wheel directions by default.

## Text Selection

[Select and Copy](../guide/terminal-ui.md#select-and-copy) documents user gestures (drag, double-click, triple-click, bypass modifier). Word selection splits on whitespace, brackets, quotes, and separators, dragging after a double or triple click extends by words or rows, and clicking the trailing cell of a wide character selects the full character. Plain clicks clear selection without copying (unless clicking a collapsed block to [expand it](#expanding-a-block)). Selection stays within the chat area, clamping drags past the last row to that row's end.

The highlight lasts until the next keystroke. While active, Ctrl+C copies again and clears selection without interrupting a turn or arming exit.

- **Anchoring.** Endpoints store a `SlotTops` content anchor plus column (each slot starts at its block's first row, with preceding separators owned by the prior slot so inserted tool results keep highlights on the call). Every frame re-resolves anchors against current slot tops, releases render any unrendered covered blocks (such as a newly committed reply), and width changes or view toggles drop selection.
- **Following the tail.** Dragging pauses tail-following so streaming cannot shift text under the pointer, and scrolling to the bottom does not resume it while held. Any release, keystroke, view toggle, width change, or press after a lost release restores tail-following if the drag paused it.
- **Edge auto-scroll.** Holding the pointer above or below chat scrolls one row every 50 ms toward content bounds with the selection head tracking the edge, stopping after 200 rows without a mouse event in case release was lost.
- **Highlight.** Selected cells use the `selection` theme slot (background fill in built-in themes).
- **Copied text.** Rows join with newlines (trailing whitespace stripped) and copy rendered text including gutters (block bars, diff line numbers). Columns match ratatui's paint, skipping control chars and zero-width graphemes and using ratatui cell widths (so escape sequences copy as displayed and halfwidth katakana with a sound mark like `ｶﾞ` spans two cells). Whitespace-only selections copy nothing.

Under tmux with `mouse on`, default `MouseDrag1Pane` forwards drags to the pane, so tmux copy-mode selection requires the terminal's bypass modifier and spans pane borders.

## Expanding a Block

In compact view, a plain click on a block's marked overflow footer (such as `... +3 lines · Ctrl+O to expand`) or collapsed `Thought` header expands that block in full on button release (preserving drags starting on the footer) when pressed on a painted row (since presses below the last row clamp to it for selection only). Expanding pauses tail-following so visible rows stay put, and Ctrl+O resets all per-block expansions.

## Copying

`ox_tui::clipboard::Clipboard` runs copies on one background worker keeping only the latest pending selection, reporting outcomes via `AppEvent` (discarding superseded reports and clearing pending OSC 52 sequences on a new copy), timing out after 2 s (shared across tmux fallbacks), waiting for a started native write to finish or a timed-out tmux child to be killed and reaped before taking the next selection, and draining the latest selection on drop.

| Session                                         | Routes                                                                       |
| ----------------------------------------------- | ---------------------------------------------------------------------------- |
| Local (no `SSH_CONNECTION` or `SSH_TTY`)        | The system clipboard through `arboard`, and inside tmux also the tmux buffer |
| Remote inside tmux                              | `tmux load-buffer -w -`, which forwards the text to the outer terminal       |
| Remote outside tmux, or when every route failed | OSC 52 (`\x1b]52;c;<base64>\x07`), written after the next frame              |

The native route keeps one `arboard` handle on a blocking thread for the session because X11 and some Wayland compositors clear the clipboard when the writer drops (on X11, contents still vanish on exit without a clipboard manager). It works under every terminal's default settings, including iTerm2 and Terminal.app.

`tmux load-buffer -w` populates the paste buffer and (on tmux 3.2+) forwards to the outer clipboard unless `set-clipboard` is off, working under tmux's default `set-clipboard external` where pane-emitted OSC 52 is ignored. On older tmux rejecting `-w`, `ox` retries without `-w` and notes that only the tmux buffer holds the text.

OSC 52 emits inside the next frame's synchronized update:

- **GNU screen.** With `STY` set outside tmux, `ox` splits payload into 76-byte base64 chunks wrapped in DCS passthrough (`ESC P` ... `ESC \`) to stay under screen's 768-byte buffer.
- **Terminals that ignore it.** On Terminal.app and Linux console, when no other route succeeds, `ox` skips OSC 52 and shows the workaround (Fn+drag or Cmd+R in Terminal.app, Shift+drag in the console). On iTerm2, the notice names the clipboard-access setting.
- **Size cap.** Selections over 100,000 bytes (xterm's default cap) use only the system clipboard.

A bottom-left pill shows copy status for 3 s, truncated to leave room for the jump-to-bottom pill. Because OSC 52 has no acknowledgment, blocked writes still report success (VS Code Remote SSH drops OSC 52 while Cursor Remote SSH shares `TERM_PROGRAM` and applies it, so `ox` cannot warn).

## OSC 8 Hyperlinks on the PR Status Segments

`pull-request` links the PR on `#NN`, while `pull-request-checks` and `pull-request-review` link its checks and changed-files pages. After painting, `StatusBar::render` records each link's rect, URL, chars, and style, and `App::render` replays OSC 8 directly to crossterm after `terminal.draw()` inside the same synchronized update:

```text
\x1b[<row>;<col>H\x1b]8;;<URL>\x07<styled cells>\x1b]8;;\x07
```

Keeping the envelope outside buffer cells preserves ratatui's `Buffer::diff` cell widths (an embedded URL would count as ~30 cells and corrupt trailing cells).

- **Out-of-band emission.** `App::emit_status_hyperlinks` saves cursor with DECSC (`\x1b7`), moves with 1-based CUP (`\x1b[<row>;<col>H`), opens OSC 8, replays cells with SGR resets, closes OSC 8, and restores with DECRC (`\x1b8`), avoiding the stdin race of `terminal.get_cursor_position()`.
- **BEL (`\x07`) terminator.** Avoids xterm.js (VS Code, Cursor) misinterpreting ST (`\x1b\\`) closers and leaking visible bytes.
- **Plain OSC 8 inside tmux.** tmux 3.4+ forwards OSC 8 to outer terminals with the `hyperlinks` feature (`set -as terminal-features ',<TERM>:hyperlinks'` if undetected), while older tmux drops the OSC and keeps the text.

`App::emit_status_hyperlinks` retains each frame's link rects so unmodified left clicks open the URL via `App::open_browser` (needed when capture intercepts clicks on kitty, WezTerm, and non-OSC-8 terminals), while modified clicks pass to the terminal to avoid double-opening. Control characters are stripped from URLs before emission.

## Design Decisions

1. **Capture the mouse and select text in the app.** Wheel scrolling under tmux requires mouse capture, which takes drags from the terminal. In-app selection gives uniform behavior across terminals while the bypass modifier preserves native selection.
2. **Anchor both ends to content slots.** Prevents highlights from drifting when blocks stream below or insert above.
3. **Copy on release, once.** Avoids spamming the clipboard worker and copy notices on every drag motion event.
4. **Pause tail-follow while dragging.** Stops streaming output from moving text out from under the pointer until release.
5. **Offer both an OSC 8 link and a click target.** Covers terminals with differing modifier conventions or no OSC 8 support.

## Out of Scope

- **OSC 8 links in markdown body text.** Would require threading URLs through the markdown renderer.
- **Selecting text outside the chat.** Input and modals stay keyboard-driven.
- **Copying source text without gutters or soft-wrap breaks.** Requires mapping rendered rows back to block sources ([#300](https://github.com/hakulab/oxide-code/issues/300)).

## Sources

- `crates/ox-tui/src/app/dispatch.rs`: `App::handle_crossterm_event`, which routes wheel and button events, and `open_status_link`.
- `crates/ox-tui/src/app/render.rs`: `App::emit_status_hyperlinks`, which replays the link cells inside OSC 8 and keeps their rects for clicks.
- `crates/ox-tui/src/clipboard.rs`: `Clipboard`, its copy routes, and `osc52_sequence`.
- `crates/ox-tui/src/components/chat.rs`: `ChatView::expand_at`, the click expansion.
- `crates/ox-tui/src/components/chat/scroll.rs`: `ChatView::handle_event`, the wheel and key scroll.
- `crates/ox-tui/src/components/chat/selection.rs`: `ChatView::handle_mouse`, the selection units, anchors, edge auto-scroll, and copied text.
- `crates/ox-tui/src/components/status.rs`: `StatusBar::render`, which captures each hyperlink's cells after the paint.
- `crates/ox-tui/src/terminal.rs`: `init`, `enter_tui_mode`, and `leave_tui_mode`, which enable and release mouse capture, and `write_status_hyperlinks`.
