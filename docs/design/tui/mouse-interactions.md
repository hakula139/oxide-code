# Mouse Interactions

Design policy for mouse behavior in the TUI.

## Goal

Two user-visible features:

1. **Click `#NN` in the status bar to open the pull request in the browser**, even though no app code routes the click.
2. **Drag-select chat content with the mouse and paste it elsewhere**, in any terminal the user runs `ox` in (iTerm2, WezTerm, kitty, Alacritty, Terminal.app, GNOME Terminal, Konsole, Ghostty, Windows Terminal, VS Code's integrated terminal, Cursor's integrated terminal).

The cleanest way to deliver both is to let the terminal do the work. The TUI does not enable mouse capture, so the terminal's own selection layer is intact. The status-bar PR segment is wrapped in an OSC 8 hyperlink envelope that every modern terminal already knows how to make Ctrl-clickable.

## Decision

`enter_tui_mode` enables raw mode, the alternate screen, Kitty keyboard disambiguation, and DECSET 1007 (alternate-scroll). It does **not** enable `EnableMouseCapture`. Pairs unwind on `leave_tui_mode`.

`App::handle_mouse_event` only routes a left-click on the cached jump-to-bottom pill rect. Every other mouse event flows to `ChatView::handle_event` for wheel scroll. Wheel events arrive as keyboard arrow-key sequences via DECSET 1007 in real sessions, so the wheel-event path mostly exercises in tests. Both routes are kept for portability.

## DECSET 1007 (alternate-scroll)

`\x1b[?1007h` on enter, `\x1b[?1007l` on leave. While the alt-screen is active the terminal translates physical wheel events into `\x1b[A` / `\x1b[B` arrow-key sequences, so `ChatView::handle_event` sees `KeyCode::Up` / `KeyCode::Down` and scrolls. The user's terminal handles the wheel without `EnableMouseCapture`, so native drag-select stays available.

Modern emulators (iTerm2, WezTerm, kitty, Alacritty, foot, Ghostty, Windows Terminal, VS Code / Cursor's xterm.js, recent GNOME Terminal, Konsole, Terminal.app via vim-mode) implement 1007. Older emulators ignore it without falling back to anything; for those the user uses keyboard scroll (`PageUp` / `PageDown` / `Ctrl+End`).

## OSC 8 hyperlink on the PR status segment

The `pull-request` status segment renders `#NN` as plain spans. After `Paragraph::render` paints the line into the frame buffer, `StatusBar::render` records each `RenderedHyperlink` range — its absolute screen rect, URL, and a snapshot of the visible chars + styles in the link cells — into a `pending_hyperlinks` queue. `App::render` drains that queue after `terminal.draw()` flushes and replays each link as an OSC 8 envelope written directly to the crossterm backend.

```text
\x1b[<row>;<col>H\x1b]8;;<URL>\x07<styled cells>\x1b]8;;\x07\x1b[0m
```

The envelope must live **outside** the cell symbols. ratatui's `Buffer::diff` reads each cell symbol's `unicode-width` to decide how many trailing cells the cell occupies (multi-width handling), and stores that in `to_skip`. A URL like `https://github.com/o/r/pull/86` is plain ASCII, so `unicode-width` reports ~30 for the envelope. The diff would then skip ~30 trailing cells, dropping the rest of `#86` and shifting later text into the gap. Replaying the envelope after the flush bypasses the diff entirely. The just-painted cells stay plain, so the next frame's diff sees them as plain symbols of width 1.

Three mechanics worth surfacing:

- **Out-of-band emission via the crossterm backend.** Storing the envelope in the cell symbol breaks ratatui's diff math, since `unicode-width` over-counts the URL bytes. After `terminal.draw()` returns, `App::emit_status_hyperlinks` writes DECSC (`\x1b7`) to park the cursor terminal-side, then for each captured link rect it positions the cursor with CUP (`\x1b[<row>;<col>H`, 1-based), writes the OSC 8 opener, replays each cell's style + symbol via crossterm's `SetForegroundColor` / `SetBackgroundColor` / `SetAttribute` / `Print`, then writes the OSC 8 closer and an SGR reset. After the loop a single DECRC (`\x1b8`) restores the cursor `terminal.draw()` parked, so the user doesn't see a stray cursor next to `#NN`. DECSC / DECRC was chosen over `terminal.get_cursor_position()` because the latter issues a DSR query and reads from stdin, which races the TUI event loop and silently fails when the response gets consumed by the event reader.
- **BEL (`\x07`) terminator over ST (`\x1b\\`).** Some xterm.js-based terminals (VS Code's and Cursor's integrated terminals) misparse self-contained per-cell ST closers, leaking visible bytes into the next cells of the line. BEL is one byte and every modern emulator parses it identically.
- **DCS pass-through inside tmux.** tmux only forwards a small whitelist of OSC numbers (52, 4, 7, 9, 12) to the outer terminal; OSC 8 isn't on it and gets silently dropped. When the process runs inside tmux (`$TMUX` set and non-empty, so a stale rc-file export doesn't trigger the wrap), `write_status_hyperlinks` wraps the entire envelope (DECSC + per-link CUP / OSC 8 / replayed cells + DECRC) in `\x1bPtmux;...\x1b\\` with every inner ESC doubled. tmux strips the DCS and forwards the inner bytes verbatim to the outer terminal. Without this wrap, ctrl+click on `#NN` does nothing inside tmux even though OSC 8 works fine in the same terminal outside tmux. tmux 3.3+ requires `set -g allow-passthrough on`; without it the wrap is silently dropped at the tmux layer.

Modern terminals (iTerm2, WezTerm, kitty, Alacritty, foot, Konsole, Ghostty, recent Windows Terminal, GNOME Terminal, VS Code's terminal, Cursor's terminal) make the segment Ctrl-clickable (Cmd-click on macOS in some terminals) and open the URL via the user's browser. Older terminals print the raw bytes literally. The visible `#NN` still reads correctly because BEL is non-printable.

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

## Implementation files

- `crates/oxide-code/src/tui/terminal.rs` — `enter_tui_mode` / `leave_tui_mode` write the alt-screen + alternate-scroll + Kitty keyboard sequences.
- `crates/oxide-code/src/tui/app.rs` — `handle_mouse_event` routes the jump-pill click and forwards everything else to chat. `emit_status_hyperlinks` drains the bar's pending link list after `terminal.draw()` flushes, and `write_status_hyperlinks` emits the CUP + OSC 8 envelope + replayed cells + SGR reset to any `Write`.
- `crates/oxide-code/src/tui/components/status.rs` — `StatusBar::render` paints the line with plain cell symbols, then snapshots each link rect's visible chars + styles into `pending_hyperlinks` for `App::emit_status_hyperlinks` to drain post-flush.
- `crates/oxide-code/src/tui/components/status/line.rs` — `StatusLine::render` returns the cell-column ranges of every hyperlinked segment alongside the line.
- `crates/oxide-code/src/util/git.rs` — `current_pull_request` returns `Option<PullRequest { number, url }>` parsed from `gh pr view --json number,url`, so the status bar has the URL ready when the PR refresh fires.

## Out of scope

- Click-to-expand on tool-result blocks (would require capturing mouse).
- OSC 8 hyperlinks inside markdown body text (would require threading URLs through the markdown renderer).
- App-driven copy-on-select with OSC 52 / arboard fallback (rejected: native terminal selection covers it).

## Verification

Manual verification across terminals:

1. Start `ox` and generate enough chat content to scroll.
2. Page up. Confirm the jump-to-bottom pill appears.
3. Click the pill. Confirm chat snaps to bottom and re-arms auto-scroll.
4. Drag-select a chat region. Confirm the highlight uses the terminal's native selection style. Mouse up. Paste somewhere external. Confirm bytes round-trip.
5. With a `pull-request` status segment configured, Ctrl-click (Cmd-click on iTerm2 / Terminal.app) on `#NN`. Confirm the browser opens to the PR URL.
6. Wheel scroll. Confirm chat scrolls (DECSET 1007 in a supporting terminal).
7. Quit. Confirm alt-screen restored.

Automated tests:

- `tui::terminal::tests::enter_tui_mode_writes_setup_sequences`, `enter_tui_mode_does_not_enable_mouse_capture`, `leave_tui_mode_writes_restore_sequences` — pin the DECSET 1007 enable / disable and the absence of `EnableMouseCapture`.
- `tui::app::tests::left_click_on_jump_overlay_jumps_chat_to_bottom`, `left_click_outside_jump_overlay_does_not_jump_chat`, `wheel_scroll_event_routes_to_chat_view` — pin the mouse-routing surface.
- `tui::app::tests::write_status_hyperlinks_emits_cup_then_envelope_per_link`, `write_status_hyperlinks_strips_control_chars_from_url`, `write_status_hyperlinks_skips_link_with_empty_safe_url` — pin the on-the-wire byte sequence (CUP + OSC 8 opener + replayed cells + closer + SGR reset).
- `tui::components::status::tests::render_records_pull_request_hyperlink_rect_for_post_flush_emission`, `render_records_no_hyperlinks_when_pull_request_absent`, `render_drops_hyperlink_when_segment_clipped_to_zero_width`, `pull_request_segment_reports_hyperlink_range_for_post_render_marking`, `pull_request_segment_reports_no_hyperlink_when_absent` — pin what the bar surfaces to the App.
