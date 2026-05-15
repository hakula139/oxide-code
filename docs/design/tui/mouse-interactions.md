# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Capture mouse events in service of useful app interactions. The TUI claims wheel scroll, app-owned click affordances (jump-to-bottom pill), and drag-to-select-and-copy. Native terminal selection is preserved through documented escape hatches and through copy-on-select via OSC 52, so users on any modern terminal can copy chat content with a normal drag.

## Decision

Keep `crossterm::EnableMouseCapture` enabled in `enter_tui_mode`. The full mode bundle (`?1000`, `?1002`, `?1003`, `?1006`, `?1015`) is required for crossterm to deliver wheel, click, drag, and motion events.

Route events from `App::handle_crossterm_event` in priority order:

1. Left-click on the cached jump-to-bottom pill rect → `ChatView::jump_to_bottom`.
2. Left-click inside the cached chat rect → arm the `Selection` state machine.
3. Left-button drag inside the chat rect → update the selection endpoint, clamped to chat bounds.
4. Left-button up → materialize the selection, emit OSC 52 set-clipboard, clear state.
5. Wheel and any other mouse event → `ChatView::handle_event` (wheel scroll up / down).

## Selection geometry

Terminal-convention line-rectangle: from `(start_row, start_col)` the selection extends to the end of that row, all of every intermediate row, and from column 0 to `(end_row, end_col)` on the final row.

Wide CJK chars and emoji (`UnicodeWidthChar::width == 2`) are taken whole when their leading half lands inside the selection range, so multi-byte sequences never split. The slicing walks `Text<'static>` spans tracking `unicode-width` columns rather than byte indices.

Block / column selection (Alt+drag in some terminals) is deferred. So is double-click word and triple-click line.

## Visual feedback

A `selection` theme slot was added. During render, after `ChatView::render` paints the chat into the frame buffer, `Selection::paint` walks the cached selection rect and applies `theme.selection.style()` to each cell. Per-built-in defaults pick a bg-only color one tier above `surface` so the highlight is legible against the chat background without colliding with diff or accent fills.

## OSC 52 emission

On left-button up, the app:

1. Reads `ChatView::rendered_text(width)` to materialize the same wrapped lines that were on screen.
2. Calls `Selection::materialize(text, area, scroll_offset)` to extract the substring (line-rect, unicode-width-aware).
3. Builds `\x1b]52;c;<base64>\x07` over the raw UTF-8 bytes via `osc52_set_clipboard`.
4. Writes the bytes to stdout (the terminal forwards them to the OS clipboard when configured).
5. Pushes a system-message warning when the payload was clamped.

Payload cap is 8 KB pre-base64 (xterm's conservative limit). kitty / iTerm2 / WezTerm tolerate more but the floor keeps the same selection working everywhere.

Truncation walks back to a UTF-8 char boundary so the encoded string is always valid UTF-8.

## OSC 8 hyperlinks

The `pull-request` status segment renders `#NN` as plain spans, then the parent `StatusBar` post-paints OSC 8 escape bytes onto each cell of the segment so modern terminals (iTerm2, WezTerm, kitty, Alacritty, foot, Konsole, Ghostty, recent Windows Terminal, GNOME Terminal) make the number Ctrl-clickable and open it via the user's browser. Older terminals print the bytes verbatim and just show `#NN` as plain text.

Direct embedding of escape bytes inside `Span::raw` does not survive ratatui's renderer: `Buffer::set_string` filters out any grapheme that contains a control char, so the leading and trailing `\x1b` are stripped while the printable bytes (`]8;;<URL>\\#NN]8;;\\`) leak into the output as visible text. The fix is to bypass that filter via `Cell::set_symbol`, which stores the symbol verbatim and the crossterm Backend prints it unchanged. `StatusLine::render` returns the cell-column ranges of every hyperlinked segment alongside the line, and `StatusBar::render` runs `mark_url_hyperlink` over each range after the line is painted. Control chars in the URL are stripped to keep a malformed value from breaking out of the OSC 8 envelope.

## Selection escape hatches

When the app captures mouse events, native terminal drag-select is suppressed by default. Users have several escape hatches:

- **iTerm2**: hold ⌥ (Option) and drag for native selection. Cmd-click on OSC 8 hyperlinks.
- **WezTerm**: hold Shift and drag. Ctrl-Shift-click for OSC 8 hyperlinks.
- **kitty**: hold Shift and drag. Ctrl-Shift-click for OSC 8.
- **Alacritty**: hold Shift and drag.
- **macOS Terminal.app**: hold ⌥ (Option) and drag.
- **GNOME Terminal / Konsole**: hold Shift and drag.
- **Windows Terminal**: hold Shift and drag.
- **tmux**: `Ctrl-b z` to zoom out, then enter copy-mode (default `Ctrl-b [`) and use copy-mode bindings.

Copy-on-select via OSC 52 makes this less necessary: a normal drag inside the chat now copies. The escape hatches are still useful for selecting status-bar / input / preview content (currently outside the selection scope).

## Required terminal config for OSC 52

OSC 52 needs explicit opt-in on some terminals:

- **xterm**: `XTerm*allowWindowOps: true` in `~/.Xresources`.
- **kitty**: `clipboard_control write-clipboard write-primary` in `kitty.conf` (default since 0.21).
- **tmux**: `set -g set-clipboard on` (already on by default since 3.2).
- **iTerm2**, **WezTerm**, **Alacritty**, **foot**, **Ghostty**: enabled by default.

When OSC 52 is rejected by the terminal, the user gets no clipboard write and no error. The escape hatches above remain available.

## Implementation files

- `crates/oxide-code/src/tui/terminal.rs` — `EnableMouseCapture` in `enter_tui_mode`.
- `crates/oxide-code/src/tui/selection.rs` — `Selection` state, materialization, OSC 52 encoder.
- `crates/oxide-code/src/tui/app.rs` — event routing (`handle_mouse_event`, `copy_selection_to_clipboard`).
- `crates/oxide-code/src/tui/components/chat.rs` — wheel scroll arms (`MouseEventKind::ScrollUp`/`ScrollDown`), `rendered_text` accessor.
- `crates/oxide-code/src/tui/components/status/line.rs` — OSC 8 hyperlink wrapper for the `pull-request` segment.
- `crates/oxide-code/src/tui/theme.rs` — `selection` slot.

## Out of scope (deferred follow-ups)

- Click-to-expand on tool-result blocks (requires per-block click rect tracking).
- OSC 8 hyperlinks inside markdown body text (requires threading URLs through the markdown renderer).
- `OX_DISABLE_MOUSE` opt-out (modeled after `CLAUDE_CODE_DISABLE_MOUSE`).
- Native clipboard fallback via `arboard` (when OSC 52 is rejected).
- Block / column selection (Alt+drag).
- Drag auto-scroll past the viewport edge.
- Double-click word and triple-click line.
- Modifier-aware mouse events (Shift+click extend, Alt+click block).
- Selection over status bar, input box, and preview pane.

## Verification

Manual verification across terminals:

1. Start `ox` and generate enough chat content to scroll.
2. Page up. Confirm the jump-to-bottom pill appears.
3. Click the pill. Confirm chat snaps to bottom and re-arms auto-scroll.
4. Drag-select a chat region containing ASCII + CJK + emoji. Mouse up. Paste somewhere external. Confirm bytes round-trip exactly.
5. With a `pull-request` status segment configured, Ctrl-click (or terminal-specific modifier) on `#NN`. Confirm the browser opens to the PR URL.
6. Try wheel scrolling. Confirm chat scrolls.
7. Try the terminal-specific selection escape hatch (e.g., Option+drag in iTerm2). Confirm native selection works for the status bar / input area.
8. Quit. Confirm the terminal is restored (alt-screen exited, mouse capture released).

Automated tests:

- `tui::selection::tests` — selection state, line-rect materialization, CJK round-trip, OSC 52 encoder, char-boundary clamping (14 tests).
- `tui::app::tests::left_click_on_jump_overlay_jumps_chat_to_bottom`, `drag_in_chat_area_arms_selection_state_machine`, `left_click_outside_chat_area_does_not_arm_selection`, `rect_contains_left_top_inclusive_right_bottom_exclusive`.
- `tui::components::status::line::tests::pull_request_segment_emits_osc8_open_and_close_around_visible_text`, `pull_request_segment_skips_osc8_when_absent`.
