# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Two user-visible features:

1. **Click `#NN` in the status bar to open the pull request in the browser**, even though no app code routes the click.
2. **Drag-select chat content with the mouse and paste it elsewhere**, in any terminal the user runs `ox` in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, VS Code's integrated terminal, Cursor's integrated terminal).

The cleanest way to deliver both is to let the terminal do the work. The TUI does not enable mouse capture, so the terminal's own selection layer is intact. The status-bar PR segments are wrapped in an OSC 8 hyperlink envelope that every modern terminal already knows how to make Ctrl-clickable.

## Decision

`enter_tui_mode` enables raw mode, the alternate screen, bracketed paste, Kitty keyboard disambiguation (outside Windows, where crossterm cannot send it), and DECSET 1007 (alternate-scroll). It does **not** enable `EnableMouseCapture`. Pairs unwind on `leave_tui_mode`.

`App::handle_crossterm_event` still forwards any mouse event it receives to `ChatView::handle_event`, but real sessions rely on DECSET 1007 for wheel scroll because the TUI does not claim mouse capture.

## DECSET 1007 (alternate-scroll)

`\x1b[?1007h` on enter, `\x1b[?1007l` on leave. While the alt-screen is active the terminal translates physical wheel events into `\x1b[A` / `\x1b[B` arrow-key sequences. The input moves its cursor while a multi-line draft allows it, and otherwise returns the key as ignored so `ChatView::handle_event` scrolls. The user's terminal handles the wheel without `EnableMouseCapture`, so native drag-select stays available.

Modern emulators (iTerm2, WezTerm, kitty, Alacritty, foot, Ghostty, Windows Terminal, VS Code / Cursor's xterm.js, recent GNOME Terminal, Konsole, Terminal.app via vim-mode) implement 1007. Older emulators ignore it with no fallback, so users there scroll with the keyboard (`PageUp` / `PageDown` / `Ctrl+End`).

## OSC 8 hyperlinks on the PR status segments

The `pull-request` status segment renders `#NN` as plain spans and links the PR, while `pull-request-checks` and `pull-request-review` link its checks and changed-files pages. After the frame buffer is painted, `StatusBar::render` records each hyperlink rect, URL, chars, and style. `App::render` drains that queue after `terminal.draw()` and replays OSC 8 directly to the crossterm backend inside the same synchronized update, so the linked cells present with the frame.

```text
\x1b[<row>;<col>H\x1b]8;;<URL>\x07<styled cells>\x1b]8;;\x07\x1b[0m
```

The envelope must live **outside** the cell symbols. ratatui's `Buffer::diff` reads each cell symbol's `unicode-width` to decide how many trailing cells the cell occupies. A URL like `https://github.com/o/r/pull/86` makes that width look like roughly 30 cells, which drops the rest of `#86` and shifts later text. Replaying after the draw keeps buffer cells plain, so the next diff still sees one-cell symbols.

Three mechanics worth surfacing:

- **Out-of-band emission via the crossterm backend.** `App::emit_status_hyperlinks` writes DECSC (`\x1b7`), positions each link with CUP (`\x1b[<row>;<col>H`, 1-based), writes the OSC 8 opener, replays captured cells, closes OSC 8, resets SGR, then restores with DECRC (`\x1b8`). DECSC / DECRC avoids the stdin race from `terminal.get_cursor_position()`'s DSR query.
- **BEL (`\x07`) terminator.** Some xterm.js-based terminals (VS Code's and Cursor's integrated terminals) misparse self-contained per-cell ST (`\x1b\\`) closers, leaking visible bytes into the next cells of the line. BEL is one byte and every modern emulator parses it identically.
- **Plain OSC 8 inside tmux.** tmux 3.4 and later parse OSC 8, place the link in pane coordinates, and forward it to outer terminals that have the `hyperlinks` terminal feature. For an outer terminal tmux does not detect, add `set -as terminal-features ',<TERM>:hyperlinks'` to `tmux.conf`, where `<TERM>` matches the outer terminal's `$TERM`. Older tmux drops the OSC and keeps the visible text.

Terminals with OSC 8 support make these segments Ctrl-clickable (Cmd-click on macOS in some terminals). Terminals that ignore OSC control strings leave the visible text intact. Terminals that print raw OSC bytes may show escape text, which is the legacy fallback.

URLs are sanitized: every control char is filtered out before the envelope is built, so a malformed value can't break out of the OSC 8 sequence.

## Native drag-select-and-copy

Without `EnableMouseCapture`, the terminal sees every mouse event itself. Drag-select uses the user's existing terminal selection model: which keys to hold, what the highlight looks like, what gets copied, and how it gets onto the clipboard are all the user's choice (or the user's terminal's defaults).

The app therefore carries no selection state machine, highlight overlay, OSC 52 encoder, `selection` theme slot, or per-terminal drag modifier, and tmux users need no `set-clipboard` setting.

## Out of scope

- Click-to-expand on tool-result blocks.
- OSC 8 hyperlinks inside markdown body text (would require threading URLs through the markdown renderer).
- App-driven copy-on-select with OSC 52 / arboard fallback. Native terminal selection covers the current need.

## Verification

Manual verification across terminals:

1. Start `ox` and generate enough chat content to scroll.
2. Page up. Confirm the jump-to-bottom pill appears.
3. Press Ctrl+End. Confirm chat snaps to bottom and re-arms auto-scroll.
4. Drag-select a chat region. Confirm the highlight uses the terminal's native selection style. Mouse up. Paste somewhere external. Confirm bytes round-trip.
5. With the `pull-request`, `pull-request-checks`, and `pull-request-review` status segments configured, Ctrl-click (Cmd-click on iTerm2 / Terminal.app) on each. Confirm the browser opens the PR, its checks page, and its changed-files page.
6. Wheel scroll. Confirm chat scrolls (DECSET 1007 in a supporting terminal).
7. Quit. Confirm alt-screen restored.

Automated coverage pins DECSET 1007 enter / leave bytes, no `EnableMouseCapture`, OSC 8 replay bytes, URL sanitization, the unwrapped envelope inside tmux, and status-bar hyperlink capture.
