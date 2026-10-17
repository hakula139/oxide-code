# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Three user-visible features:

1. **Scroll the chat with the mouse wheel**, in a bare terminal and inside tmux.
2. **Click `#NN` in the status bar to open the pull request in the browser**, even though no app code routes the click.
3. **Select chat content with the mouse and paste it elsewhere**, in any terminal the user runs `ox` in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, VS Code's integrated terminal, Cursor's integrated terminal).

The TUI captures the mouse so wheel events reach it everywhere. The status-bar PR segments are wrapped in an OSC 8 hyperlink envelope that every modern terminal already knows how to make Ctrl-clickable. Selection goes through the terminal's bypass modifier until in-app selection lands ([#289](https://github.com/hakula139/oxide-code/issues/289)).

## Decision

`enter_tui_mode` enables raw mode, the alternate screen, bracketed paste, and, outside Windows, Kitty keyboard disambiguation and crossterm's `EnableMouseCapture`. crossterm captures the Windows mouse through the console API, so `init` enables it there after `enter_tui_mode`. Pairs unwind on `leave_tui_mode`, or in `restore` for the Windows mouse.

`App::handle_crossterm_event` forwards wheel events to `ChatView::handle_event`, which scrolls three rows per notch without moving the input cursor. While a modal is open, each notch reaches the modal as one Up or Down key instead, so the wheel steps a picker's selection or scrolls an overview. Other mouse events return early so the motion reports that capture enables don't each force a frame.

## Mouse capture

`EnableMouseCapture` writes `?1000h`, `?1002h`, `?1003h`, `?1015h`, and `?1006h`, and crossterm parses the SGR (`?1006`) reports into `MouseEvent`s. Only the wheel is handled today, but in-app selection needs the drag mode, and `?1000h` alone already makes the terminal hand clicks to the app, so a narrower set of modes would not keep native selection. On Windows, crossterm enables console mouse input, which has no wheel-only setting.

Terminal-side wheel translation can't cover tmux. DECSET 1007 (alternate scroll) makes a bare terminal send wheel events in the alternate screen as arrow keys, but under tmux with `mouse on`, tmux receives the wheel itself. Its default `WheelUpPane` binding passes the event to a pane only when the pane reports the mouse (`#{mouse_any_flag}`). Otherwise tmux drops the event or enters copy mode and scrolls tmux history over the TUI. With capture on, tmux forwards both wheel directions to `ox` under its default bindings.

## Text selection

With capture on, the terminal hands clicks and drags to `ox`, which ignores them. Holding the terminal's bypass modifier restores native selection: Shift in most terminals, Option in iTerm2, and Option in VS Code and Cursor on macOS with `terminal.integrated.macOptionClickForcesSelection` enabled. Inside tmux with `mouse on`, tmux's default `MouseDrag1Pane` binding also forwards drags to a pane that reports the mouse, so tmux's copy-mode selection needs the same bypass and selects across pane borders.

In-app selection with copy-on-select ([#289](https://github.com/hakula139/oxide-code/issues/289)) will make a plain drag select chat text again.

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

## Out of scope

- Click-to-expand on tool-result blocks.
- OSC 8 hyperlinks inside markdown body text (would require threading URLs through the markdown renderer).

## Verification

Manual verification across terminals:

1. Start `ox` and generate enough chat content to scroll.
2. Page up. Confirm the jump-to-bottom pill appears.
3. Press Ctrl+End. Confirm chat snaps to bottom and re-arms auto-scroll.
4. Hold the terminal's bypass modifier and drag-select a chat region. Confirm the highlight uses the terminal's native selection style. Mouse up. Paste somewhere external. Confirm bytes round-trip.
5. With the `pull-request`, `pull-request-checks`, and `pull-request-review` status segments configured, Ctrl-click (Cmd-click on iTerm2 / Terminal.app) on each. Confirm the browser opens the PR, its checks page, and its changed-files page.
6. Wheel scroll, both in a bare terminal and inside tmux with `mouse on`. Confirm chat scrolls and tmux stays out of copy mode.
7. Quit. Confirm alt-screen restored.

Automated coverage pins the mouse-capture enter / leave bytes, wheel scroll steps, ignored non-wheel mouse events, OSC 8 replay bytes, URL sanitization, the unwrapped envelope inside tmux, and status-bar hyperlink capture.
