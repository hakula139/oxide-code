# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Three user-visible features:

1. **Scroll the chat with the mouse wheel**, in a bare terminal and inside tmux.
2. **Click `#NN` in the status bar to open the pull request in the browser**, with a plain click or the terminal's own Ctrl / Cmd-click.
3. **Select chat content with the mouse and paste it elsewhere**, in any terminal the user runs `ox` in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, VS Code's integrated terminal, Cursor's integrated terminal).

The TUI captures the mouse so wheel events reach it everywhere, and selects and copies chat text itself, since capture takes drags away from the terminal. The status-bar PR segments are wrapped in an OSC 8 hyperlink envelope, which terminals open on Ctrl / Cmd-click, and `ox` opens them itself on a plain click.

## Decision

`init` enables raw mode, then `enter_tui_mode` enables the alternate screen, bracketed paste, and, outside Windows, Kitty keyboard disambiguation and crossterm's `EnableMouseCapture`. crossterm captures the Windows mouse through the console API, so `init` enables it there after `enter_tui_mode` and also requests SGR mouse reports (`?1006h`), which some Windows terminals need before ConPTY delivers mouse records. Pairs unwind on `leave_tui_mode`, or in `restore` for the Windows mouse.

`App::handle_crossterm_event` forwards wheel events to `ChatView::handle_event`, which scrolls three rows per notch without moving the input cursor. While a modal or the slash-command popup is open, each notch reaches it as one Up or Down key instead, so the wheel steps a list's selection or scrolls an overview. A plain left press on a status-bar link opens it, as [below](#osc-8-hyperlinks-on-the-pr-status-segments) describes. Other left-button presses, drags, and releases go to `ChatView::handle_mouse`, which selects text. Other mouse events return early so the motion reports that capture enables don't each force a frame.

## Mouse capture

`EnableMouseCapture` writes `?1000h`, `?1002h`, `?1003h`, `?1015h`, and `?1006h`, and crossterm parses the SGR (`?1006`) reports into `MouseEvent`s. Selection needs the drag mode, and `?1000h` alone already makes the terminal hand clicks to the app, so a narrower set of modes would not keep native selection. On Windows, crossterm enables console mouse input, which has no wheel-only setting.

Terminal-side wheel translation can't cover tmux. DECSET 1007 (alternate scroll) makes a bare terminal send wheel events in the alternate screen as arrow keys, but under tmux with `mouse on`, tmux receives the wheel itself. Its default `WheelUpPane` binding passes the event to a pane only when the pane reports the mouse (`#{mouse_any_flag}`). Otherwise tmux drops the event or enters copy mode and scrolls tmux history over the TUI. With capture on, tmux forwards both wheel directions to `ox` under its default bindings.

## Text selection

Dragging with the left button over the chat selects text, and releasing the button copies it. A double click selects a word, which whitespace and bracket, quote, and separator characters bound, and a triple click selects a row. Dragging after a double or triple click extends the selection by words or rows. A press on the second cell of a wide character takes the whole character. A plain click selects nothing and clears the previous selection. Selection covers only the chat area, and a drag past the last row ends at that row's end.

The highlight stays until the next keystroke clears it. While it shows, Ctrl+C copies it again and clears it without interrupting a turn or arming exit.

- **Anchoring:** Each end of the selection is a content anchor from `SlotTops`, the scheme the scroll position already uses, plus a column. A slot starts at its block's first row, and a separator above a block belongs to the slot before it, so a tool result inserted above a selected call keeps the highlight on the call. Every frame resolves the anchors against the current slot tops, so the highlight stays on the same text while blocks stream in below it or are inserted above it. A release renders any block the selection covers that the last frame did not, such as a reply committed since. A width change rewraps every row, so it drops the selection.
- **Following the tail:** A drag pauses following the tail, so streamed output can't move text out from under the pointer, and scrolling to the bottom doesn't resume it while the button is held. The release resumes following when the drag paused it.
- **Edge auto-scroll:** While the pointer is held above or below the chat, the 60 FPS tick scrolls one row every 50 ms toward it until the view reaches the content's end, and the selection's head follows the row at that edge. After 200 rows without a mouse event, it stops, in case the terminal lost the release.
- **Highlight:** Selected cells take the `selection` theme slot's style, a background fill in the built-in themes.
- **Copied text:** Rows join with newlines, and each row drops its trailing whitespace. The text is what the rows show, including gutters such as the block bar and diff line numbers. Columns follow ratatui's paint, which skips control characters and zero-width graphemes, so a row holding an escape sequence copies what it shows. A selection of only whitespace copies nothing.

The terminal's bypass modifier still gives native selection: Shift in most terminals, Option in iTerm2, Fn in Terminal.app, and Option in VS Code and Cursor on macOS with `terminal.integrated.macOptionClickForcesSelection` enabled. Inside tmux with `mouse on`, tmux's default `MouseDrag1Pane` binding forwards drags to a pane that reports the mouse, so tmux's own copy-mode selection needs the same bypass and selects across pane borders.

## Copying

`tui::clipboard::Clipboard` runs each copy in a tokio task, off the event loop, and reports the outcome back to `App` through a channel the loop selects on. A copy that takes over 2 s reports a timeout, and dropping the timed-out task kills any tmux child, which tokio reaps. The routes depend on where `ox` runs:

| Session                                         | Routes                                                                       |
| ----------------------------------------------- | ---------------------------------------------------------------------------- |
| Local (no `SSH_CONNECTION` or `SSH_TTY`)        | The system clipboard through `arboard`, and inside tmux also the tmux buffer |
| Remote inside tmux                              | `tmux load-buffer -w -`, which forwards the text to the outer terminal       |
| Remote outside tmux, or when every route failed | OSC 52 (`\x1b]52;c;<base64>\x07`), written after the next frame              |

The native write runs on a blocking thread and keeps its `arboard` handle for the session, since X11 and some Wayland compositors drop the contents once the process that wrote them lets go. On X11 the contents still vanish when `ox` exits, unless a clipboard manager took them over. A native write works under every terminal's default settings, including iTerm2's and Terminal.app's, which ignore OSC 52.

`tmux load-buffer -w` stores the text in a paste buffer and, on tmux 3.2 and later, forwards it to the outer terminal's clipboard unless `set-clipboard` is off. This works under tmux's default `set-clipboard external`, where tmux ignores OSC 52 written by the pane itself. Older tmux rejects `-w`, so `ox` retries without it, and the notice says that only the tmux buffer holds the text.

The OSC 52 write goes out inside the next frame's synchronized update. Under GNU screen (`STY` set, outside tmux), screen passes it to the outer terminal only inside its DCS passthrough (`ESC P` ... `ESC \`) and abandons a string over its 768-byte buffer, so `ox` sends the payload as a run of DCS strings of 76 base64 bytes each. Terminal.app and the Linux console ignore OSC 52, so when no other route worked, `ox` writes nothing and names the fix: Fn-drag or Cmd-R, which turns off mouse reporting, in Terminal.app, and Shift-drag in the console. iTerm2 applies OSC 52 only once "Applications in terminal may access clipboard" is on, so its notice names that setting. Selections over 100,000 bytes, xterm's default cap, go only to the system clipboard.

A pill at the chat's bottom-left corner reports the outcome for three seconds, truncated to the width the jump-to-bottom pill leaves. OSC 52 has no acknowledgement, so a terminal that blocks clipboard writes still shows the copy as done. VS Code's Remote SSH terminal drops OSC 52 this way, and since Cursor's remote terminal reports the same `TERM_PROGRAM` and applies it, `ox` cannot warn about it.

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

Mouse capture hands plain clicks to `ox`, and some terminals, such as kitty and WezTerm, also deliver clicks on links to an app that captures the mouse. `App::emit_status_hyperlinks` therefore keeps the frame's link rects, and a left press without modifiers inside one opens its URL through `App::open_browser`. A press with modifiers is left to the terminal, so a terminal that opens OSC 8 links on Ctrl / Cmd-click itself doesn't open the page twice. Terminals that ignore OSC control strings leave the visible text intact. Terminals that print raw OSC bytes may show escape text, which is the legacy fallback.

URLs are sanitized: every control char is filtered out before the envelope is built, so a malformed value can't break out of the OSC 8 sequence.

## Out of scope

- Click-to-expand on tool-result blocks.
- OSC 8 hyperlinks inside markdown body text (would require threading URLs through the markdown renderer).
- Selecting text outside the chat, such as in the input or a modal.
- Copying source text without gutters or soft-wrap breaks, which would require each block to map rendered rows back to its source.

## Verification

Manual verification across terminals:

1. Start `ox` and generate enough chat content to scroll.
2. Page up. Confirm the jump-to-bottom pill appears.
3. Press Ctrl+End. Confirm chat snaps to bottom and re-arms auto-scroll.
4. Drag-select a chat region in a bare terminal, inside tmux, and over SSH. Confirm the highlight and the copy notice, then paste somewhere external and confirm the bytes round-trip. Double-click a word and triple-click a row. Hold a drag past the top edge and confirm the chat scrolls. Press Ctrl+C and confirm it copies without interrupting. Then hold the bypass modifier and confirm native selection still works.
5. With the `pull-request`, `pull-request-checks`, and `pull-request-review` status segments configured, click each, then Ctrl-click each (Cmd-click on iTerm2 / Terminal.app). Confirm the browser opens the PR, its checks page, and its changed-files page once per click.
6. Wheel scroll, both in a bare terminal and inside tmux with `mouse on`. Confirm chat scrolls and tmux stays out of copy mode.
7. Quit. Confirm alt-screen restored.

Automated coverage pins the mouse-capture enter / leave bytes, the Windows SGR request bytes, wheel scroll steps, wheel routing to modals and the slash popup, plain-click link opening, ignored mouse events, selection units, anchoring across inserted and streamed blocks, edge auto-scroll and its stops, the highlight, keystrokes that clear or copy the selection, copy routing, the tmux retry and timeout, the OSC 52 write, the notice truncation, OSC 8 replay bytes, URL sanitization, the unwrapped envelope inside tmux, and status-bar hyperlink capture.
