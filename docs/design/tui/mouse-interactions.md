# Mouse Interactions

The TUI captures the mouse so the wheel scrolls the chat in a bare terminal and inside tmux. Capture takes drags away from the terminal, so `ox` selects and copies chat text itself, in any terminal the user runs it in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, and the integrated terminals of VS Code and Cursor). A plain click expands a collapsed block, on its overflow footer or its `Thought` header, and opens the pull request page a status-bar segment links. The links are also OSC 8 hyperlinks, which terminals open on Ctrl+click or Cmd+click.

## Setup and Event Routing

`init` enables raw mode, then `enter_tui_mode` enables the alternate screen, bracketed paste, and, outside Windows, Kitty keyboard disambiguation and crossterm's `EnableMouseCapture`. crossterm captures the Windows mouse through the console API, so `init` enables it there after `enter_tui_mode` and also requests SGR mouse reports (`?1006h`), which some Windows terminals need before ConPTY delivers mouse records. Pairs unwind on `leave_tui_mode`, or in `restore` for the Windows mouse.

`App::handle_crossterm_event` forwards wheel events to `ChatView::handle_event`, which scrolls three rows per notch without moving the input cursor. While a modal or the slash-command popup is open, each notch reaches it as one Up or Down key instead, so the wheel steps a list's selection or scrolls an overview. A plain left press on a status-bar link opens it, as [below](#osc-8-hyperlinks-on-the-pr-status-segments) describes. Other left-button presses, drags, and releases go to `ChatView::handle_mouse`, which selects text. Other mouse events return early so the motion reports that capture enables don't each force a frame.

## Mouse Capture

`EnableMouseCapture` writes `?1000h`, `?1002h`, `?1003h`, `?1015h`, and `?1006h`, and crossterm parses the SGR (`?1006`) reports into `MouseEvent`s. Selection needs the drag mode, and `?1000h` alone already makes the terminal hand clicks to the app, so a narrower set of modes would not keep native selection. On Windows, crossterm enables console mouse input, which has no wheel-only setting.

Terminal-side wheel translation can't cover tmux. DECSET 1007 (alternate scroll) makes a bare terminal send wheel events in the alternate screen as arrow keys, but under tmux with `mouse on`, tmux receives the wheel itself. Its default `WheelUpPane` binding passes the event to a pane only when the pane reports the mouse (`#{mouse_any_flag}`). Otherwise tmux drops the event or enters copy mode and scrolls tmux history over the TUI. With capture on, tmux forwards both wheel directions to `ox` under its default bindings.

## Text Selection

Dragging with the left button over the chat selects text, and releasing the button copies it. A double click selects a word, which whitespace and bracket, quote, and separator characters bound, and a triple click selects a row. Dragging after a double or triple click extends the selection by words or rows. A press on the second cell of a wide character takes the whole character. A plain click selects nothing and clears the previous selection, though one on a collapsed block [expands it](#expanding-a-block). Selection covers only the chat area, and a drag past the last row ends at that row's end.

The highlight stays until the next keystroke clears it. While it shows, Ctrl+C copies it again and clears it without interrupting a turn or arming exit.

- **Anchoring:** Each end of the selection is a content anchor from `SlotTops`, the scheme the scroll position already uses, plus a column. A slot starts at its block's first row, and a separator above a block belongs to the slot before it, so a tool result inserted above a selected call keeps the highlight on the call. Every frame resolves the anchors against the current slot tops, so the highlight stays on the same text while blocks stream in below it or are inserted above it. A release renders any block the selection covers that the last frame did not, such as a reply committed since. A width change rewraps every row and a view toggle reshapes the blocks, so either drops the selection.
- **Following the tail:** A drag pauses following the tail, so streamed output can't move text out from under the pointer, and scrolling to the bottom doesn't resume it while the button is held. The release resumes following when the drag paused it, as does anything else that drops a held selection: a keystroke, a view toggle, a width change, or a press after a lost release.
- **Edge auto-scroll:** While the pointer is held above or below the chat, the frame tick scrolls one row every 50 ms toward it until the view reaches the content's end, and the selection's head follows the row at that edge. After 200 rows without a mouse event, it stops, in case the terminal lost the release.
- **Highlight:** Selected cells take the `selection` theme slot's style, a background fill in the built-in themes.
- **Copied text:** Rows join with newlines, and each row drops its trailing whitespace. The text is what the rows show, including gutters such as the block bar and diff line numbers. Columns follow ratatui's paint, which skips control characters and zero-width graphemes and measures each grapheme with ratatui's cell width, so a row holding an escape sequence copies what it shows, and a halfwidth katakana letter with a sound mark, such as `ｶﾞ`, covers the two cells it paints. A selection of only whitespace copies nothing.

The terminal's bypass modifier still gives native selection: Shift in most terminals, Option in iTerm2, Fn in Terminal.app, and Option in VS Code and Cursor on macOS with `terminal.integrated.macOptionClickForcesSelection` enabled. Inside tmux with `mouse on`, tmux's default `MouseDrag1Pane` binding forwards drags to a pane that reports the mouse, so tmux's own copy-mode selection needs the same bypass and selects across pane borders.

## Expanding a Block

In the compact view, a plain click on a block's overflow footer, such as `... +3 lines · Ctrl+O to expand`, or on a collapsed `Thought` header renders that block in full. Each block marks these rows when it renders, so the click finds them in the cached render it landed on. The block expands on the release, which keeps a drag that starts on the footer a selection, and only for a press on a painted row, since a press below the last row still lands on that row for selection. The click pauses following the tail, so the rows on screen stay put while the block grows below them. Ctrl+O returns every block to the view it switches to.

## Copying

`ox_tui::clipboard::Clipboard` runs each copy in a tokio task, off the event loop, and reports the outcome back to `App` through a channel the loop selects on. A copy that takes over 2 s reports a timeout, and dropping the timed-out task kills any tmux child, which tokio reaps. The routes depend on where `ox` runs:

| Session                                         | Routes                                                                       |
| ----------------------------------------------- | ---------------------------------------------------------------------------- |
| Local (no `SSH_CONNECTION` or `SSH_TTY`)        | The system clipboard through `arboard`, and inside tmux also the tmux buffer |
| Remote inside tmux                              | `tmux load-buffer -w -`, which forwards the text to the outer terminal       |
| Remote outside tmux, or when every route failed | OSC 52 (`\x1b]52;c;<base64>\x07`), written after the next frame              |

The native write runs on a blocking thread and keeps its `arboard` handle for the session, since X11 and some Wayland compositors drop the contents once the process that wrote them lets go. On X11 the contents still vanish when `ox` exits, unless a clipboard manager took them over. A native write works under every terminal's default settings, including iTerm2's and Terminal.app's, which ignore OSC 52.

`tmux load-buffer -w` stores the text in a paste buffer and, on tmux 3.2 and later, forwards it to the outer terminal's clipboard unless `set-clipboard` is off. This works under tmux's default `set-clipboard external`, where tmux ignores OSC 52 written by the pane itself. Older tmux rejects `-w`, so `ox` retries without it, and the notice says that only the tmux buffer holds the text.

The OSC 52 write goes out inside the next frame's synchronized update. Under GNU screen (`STY` set, outside tmux), screen passes it to the outer terminal only inside its DCS passthrough (`ESC P` ... `ESC \`) and abandons a string over its 768-byte buffer, so `ox` sends the payload as a run of DCS strings of 76 base64 bytes each. Terminal.app and the Linux console ignore OSC 52, so when no other route worked, `ox` writes nothing and names the fix: Fn+drag or Cmd+R, which turns off mouse reporting, in Terminal.app, and Shift+drag in the console. iTerm2 applies OSC 52 only once "Applications in terminal may access clipboard" is on, so its notice names that setting. Selections over 100,000 bytes, xterm's default cap, go only to the system clipboard.

A pill at the chat's bottom-left corner reports the outcome for three seconds, truncated to the width the jump-to-bottom pill leaves. OSC 52 has no acknowledgment, so a terminal that blocks clipboard writes still shows the copy as done. VS Code's Remote SSH terminal drops OSC 52 this way, and since Cursor's remote terminal reports the same `TERM_PROGRAM` and applies it, `ox` cannot warn about it.

## OSC 8 Hyperlinks on the PR Status Segments

The `pull-request` status segment renders `#NN` as plain spans and links the PR, while `pull-request-checks` and `pull-request-review` link its checks and changed-files pages. After the frame buffer is painted, `StatusBar::render` records each hyperlink rect, URL, chars, and style. `App::render` drains that queue after `terminal.draw()` and replays OSC 8 directly to the crossterm backend inside the same synchronized update, so the linked cells present with the frame.

```text
\x1b[<row>;<col>H\x1b]8;;<URL>\x07<styled cells>\x1b]8;;\x07
```

The envelope must live **outside** the cell symbols. ratatui's `Buffer::diff` uses each cell's `CellWidth` to decide how many trailing cells the cell occupies. A URL like `https://github.com/o/r/pull/86` makes that width look like roughly 30 cells, which drops the rest of `#86` and shifts later text. Replaying after the draw keeps buffer cells plain, so the next diff still sees one-cell symbols.

Three mechanics worth surfacing:

- **Out-of-band emission via the crossterm backend.** `App::emit_status_hyperlinks` writes DECSC (`\x1b7`), positions each link with CUP (`\x1b[<row>;<col>H`, 1-based), writes the OSC 8 opener, replays captured cells with an SGR reset after each styled cell, closes OSC 8, then restores with DECRC (`\x1b8`). DECSC / DECRC avoids the stdin race from `terminal.get_cursor_position()`'s DSR query.
- **BEL (`\x07`) terminator.** Some xterm.js-based terminals (VS Code's and Cursor's integrated terminals) misparse self-contained per-cell ST (`\x1b\\`) closers, leaking visible bytes into the next cells of the line. BEL is one byte and every modern emulator parses it identically.
- **Plain OSC 8 inside tmux.** tmux 3.4 and later parse OSC 8, place the link in pane coordinates, and forward it to outer terminals that have the `hyperlinks` terminal feature. For an outer terminal tmux does not detect, add `set -as terminal-features ',<TERM>:hyperlinks'` to `tmux.conf`, where `<TERM>` matches the outer terminal's `$TERM`. Older tmux drops the OSC and keeps the visible text.

Mouse capture hands plain clicks to `ox`, and some terminals, such as kitty and WezTerm, also deliver clicks on links to an app that captures the mouse. `App::emit_status_hyperlinks` therefore keeps the frame's link rects, and a left press without modifiers inside one opens its URL through `App::open_browser`. A press with modifiers is left to the terminal, so a terminal that opens OSC 8 links on Ctrl+click or Cmd+click itself doesn't open the page twice. Terminals that ignore OSC control strings leave the visible text intact. Terminals that print raw OSC bytes may show escape text, which is the legacy fallback.

URLs are sanitized: every control char is filtered out before the envelope is written, so a malformed value can't break out of the OSC 8 sequence.

## Out of Scope

- OSC 8 hyperlinks inside markdown body text (would require threading URLs through the markdown renderer).
- Selecting text outside the chat, such as in the input or a modal.
- Copying source text without gutters or soft-wrap breaks, which would require each block to map rendered rows back to its source ([#300](https://github.com/hakula139/oxide-code/issues/300)).

## Sources

- `crates/ox-tui/src/app/dispatch.rs`: `App::handle_crossterm_event`, which routes wheel and button events, and `open_status_link`.
- `crates/ox-tui/src/app/render.rs`: `App::emit_status_hyperlinks`, which replays the link cells inside OSC 8 and keeps their rects for clicks.
- `crates/ox-tui/src/clipboard.rs`: `Clipboard`, its copy routes, and `osc52_sequence`.
- `crates/ox-tui/src/components/chat.rs`: `ChatView::expand_at`, the click expansion.
- `crates/ox-tui/src/components/chat/scroll.rs`: `ChatView::handle_event`, the wheel and key scroll.
- `crates/ox-tui/src/components/chat/selection.rs`: `ChatView::handle_mouse`, the selection units, anchors, edge auto-scroll, and copied text.
- `crates/ox-tui/src/components/status.rs`: `StatusBar::render`, which captures each hyperlink's cells after the paint.
- `crates/ox-tui/src/terminal.rs`: `init`, `enter_tui_mode`, and `leave_tui_mode`, which enable and release mouse capture, and `write_status_hyperlinks`.
