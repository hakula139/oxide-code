# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Two user-visible features:

1. **Click `#NN` in the status bar to open the pull request in the browser**, even though no app code routes the click.
2. **Drag-select chat content with the mouse and paste it elsewhere**, in any terminal the user runs `ox` in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, VS Code's integrated terminal, Cursor's integrated terminal).

The cleanest way to deliver both is to let the terminal do the work. The TUI does not enable mouse capture, so the terminal's own selection layer is intact. The status-bar PR segment is wrapped in an OSC 8 hyperlink envelope that every modern terminal already knows how to make Ctrl-clickable.

## Decision

`enter_tui_mode` enables raw mode, the alternate screen, Kitty keyboard disambiguation, and DECSET 1007 (alternate-scroll). It does **not** enable `EnableMouseCapture`. Pairs unwind on `leave_tui_mode`.

`App::handle_crossterm_event` still forwards any mouse event it receives to `ChatView::handle_event`, but real sessions rely on DECSET 1007 for wheel scroll because the TUI does not claim mouse capture.

## DECSET 1007 (alternate-scroll)

`\x1b[?1007h` on enter, `\x1b[?1007l` on leave. While the alt-screen is active the terminal translates physical wheel events into `\x1b[A` / `\x1b[B` arrow-key sequences, so `ChatView::handle_event` sees `KeyCode::Up` / `KeyCode::Down` and scrolls. The user's terminal handles the wheel without `EnableMouseCapture`, so native drag-select stays available.

Modern emulators (iTerm2, WezTerm, kitty, Alacritty, foot, Ghostty, Windows Terminal, VS Code / Cursor's xterm.js, recent GNOME Terminal, Konsole, Terminal.app via vim-mode) implement 1007. Older emulators ignore it without falling back to anything; for those the user uses keyboard scroll (`PageUp` / `PageDown` / `Ctrl+End`).

## OSC 8 hyperlink on the PR status segment

The `pull-request` status segment renders `#NN` as plain spans. After the frame buffer is painted, `StatusBar::render` records each hyperlink rect, URL, chars, and style. `App::render` drains that queue after `terminal.draw()` flushes and replays OSC 8 directly to the crossterm backend.

```text
\x1b[<row>;<col>H\x1b]8;;<URL>\x07<styled cells>\x1b]8;;\x07\x1b[0m
```

The envelope must live **outside** the cell symbols. ratatui's `Buffer::diff` reads each cell symbol's `unicode-width` to decide how many trailing cells the cell occupies. A URL like `https://github.com/o/r/pull/86` makes that width look like roughly 30 cells, which drops the rest of `#86` and shifts later text. Post-flush replay keeps buffer cells plain, so the next diff still sees one-cell symbols.

Three mechanics worth surfacing:

- **Out-of-band emission via the crossterm backend.** `App::emit_status_hyperlinks` writes DECSC (`\x1b7`), positions each link with CUP (`\x1b[<row>;<col>H`, 1-based), writes the OSC 8 opener, replays captured cells, closes OSC 8, resets SGR, then restores with DECRC (`\x1b8`). DECSC / DECRC avoids the stdin race from `terminal.get_cursor_position()`'s DSR query.
- **BEL (`\x07`) terminator over ST (`\x1b\\`).** Some xterm.js-based terminals (VS Code's and Cursor's integrated terminals) misparse self-contained per-cell ST closers, leaking visible bytes into the next cells of the line. BEL is one byte and every modern emulator parses it identically.
- **DCS pass-through inside tmux.** tmux does not forward OSC 8 by default. When `$TMUX` is set and non-empty, `write_status_hyperlinks` wraps the envelope in `\x1bPtmux;...\x1b\\` with every inner ESC doubled. tmux 3.3+ also requires `set -g allow-passthrough on`.

Terminals with OSC 8 support make the segment Ctrl-clickable (Cmd-click on macOS in some terminals). Terminals that ignore OSC control strings leave the visible `#NN` intact. Terminals that print raw OSC bytes may show escape text, which is the legacy fallback.

URLs are sanitized: every control char is filtered out before the envelope is built, so a malformed value can't break out of the OSC 8 sequence.

## Native drag-select-and-copy

Without `EnableMouseCapture`, the terminal sees every mouse event itself. Drag-select uses the user's existing terminal selection model: which keys to hold, what the highlight looks like, what gets copied, and how it gets onto the clipboard are all the user's choice (or the user's terminal's defaults).

This means we don't need:

- A `Selection` state machine in the app.
- An app-side highlight overlay.
- An OSC 52 encoder.
- A `selection` theme slot.
- Per-terminal escape hatches (Option+drag, Shift+drag, etc.). The terminal's normal drag is the primary path.
- `set -g set-clipboard on` in tmux (the user's tmux selection model is whatever the user already configured).

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
5. With a `pull-request` status segment configured, Ctrl-click (Cmd-click on iTerm2 / Terminal.app) on `#NN`. Confirm the browser opens to the PR URL.
6. Wheel scroll. Confirm chat scrolls (DECSET 1007 in a supporting terminal).
7. Quit. Confirm alt-screen restored.

Automated coverage pins DECSET 1007 enter / leave bytes, no `EnableMouseCapture`, OSC 8 replay bytes, URL sanitization, tmux wrapping, and status-bar hyperlink capture.
