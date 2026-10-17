# Mouse Interactions (Reference)

Research on mouse handling in terminal AI CLIs: capture defaults, click affordances, wheel scroll, text selection, copy-on-select strategies, and URL openers.

## Claude Code

The most polished mouse layer of the three peers. Claude Code's `src/utils/fullscreen.ts` reads `CLAUDE_CODE_DISABLE_MOUSE` to gate the entire mouse pipeline, with a separate `CLAUDE_CODE_DISABLE_MOUSE_CLICKS` env var that lets wheel work while blocking click events.

The mode bundle enabled by `src/ink/termio/dec.ts` is `?1000h`, `?1002h`, `?1003h`, `?1006h`, the same shape as crossterm's `EnableMouseCapture`. Disabled via the matching `...l` set on suspend or exit.

Hit-testing lives in `src/ink/hit-test.ts`. Each render builds a Yoga DOM with rect-per-node, then `dispatchClick` bubbles from the deepest hit up the parent chain until `stopImmediatePropagation()`. Clickable elements include the jump-to-bottom pill (`FullscreenLayout.tsx:491`), expand / collapse on message rows (`VirtualMessageList.tsx:225`), background-task agent pills (`BackgroundTaskStatus.tsx:155`), and OSC 8 hyperlinks via `<Link url={...}>`.

Selection uses an in-process model in `src/ink/selection.ts`. Drag-start / update / finish / clear plus double-click word and triple-click line are handled in app-side code. `useCopyOnSelect` writes the selection to the OS clipboard on mouse-up via OSC 52, `pbcopy`, or `tmux load-buffer` depending on environment. `<NoSelect>` marks gutter cells (line numbers, diff sigils) as non-selectable so drag-copy yields clean text.

URL opening: `src/utils/browser.ts` validates the URL to `http:` / `https:` first, then dispatches: `BROWSER` env override, else `rundll32 url,OpenURL` (Windows), `open` (macOS), `xdg-open` (Linux). Single-click on an OSC 8 hyperlink defers 500 ms so a second click within the window can start a word-selection drag instead.

## OpenAI Codex

Codex's Rust TUI captures the mouse while it owns the alternate screen. `AlternateScreen::configure_input` in `codex-rs/tui/src/tui/alternate_screen.rs` disables DECSET 1007, enables pointer capture, and requests SGR reports, which ConPTY needs to translate Windows mouse reports into mouse records. When it doesn't capture, it turns 1007 back on so the terminal sends wheel events as arrow keys.

`codex-rs/tui/src/tui/tmux.rs` probes `#{mouse}` with one `tmux display-message` call and skips capture only when the session reports mouse off. Inside tmux with `mouse on`, tmux owns the wheel, so 1007 never takes effect there.

Transcript links open in the app under capture. `transcript_view/input.rs` resolves the link under a press, and a single plain click on it opens the URL on release, while a click with modifiers or a drag stays a selection. The status line still goes through OSC 8: `set_status_line_hyperlink(url)` at `chatwidget.rs:1684` and `bottom_pane/mod.rs:1584` wrap the open-PR URL, and `mark_url_hyperlink(buf, area, url)` overlays OSC 8 cells across a ratatui buffer rect, so the terminal's own Ctrl-click handler opens it.

URL opening fallback uses the `webbrowser = "1.0"` crate via `webbrowser::open(&url)` at `app/history_ui.rs`. `open_url_in_browser` serves the internal `AppEvent::OpenUrlInBrowser { url }` event for plugin auth and app-link views, and transcript link clicks call it directly. The status-line OSC 8 hyperlinks still go through the terminal.

Selection is app-side. `rendered_selection.rs` handles left-button press, drag, and release over regions each widget registers during render, and `transcript_view` adds keyboard selection. `clipboard_copy.rs` copies through the native clipboard (`arboard`) locally, also forwards to the attached terminal in tmux, and sends OSC 52 directly over SSH without a multiplexer.

## opencode

opencode is built on opentui (TypeScript / SolidJS). The `mouse` config field defaults to `true` and combines with `OPENCODE_DISABLE_MOUSE` (env var wins) to set `useMouse` on the opentui renderer config (`app.tsx:120-130`).

The renderer exposes `<box onMouseUp>` / `<box onMouseOver>` element-level events. Click affordances include tool-output expand / collapse (`session/index.tsx:1678`), subagent inline tool navigation (`index.tsx:2055`), revert-message banner, subagent footer nav, question / option dialog rows, permission-dialog options, error-screen copy-issue-URL button. The clickable `<Link href>` component fires `open(href)` from the npm `open` package on mouse-up, with no allowlist or sanitization.

Copy-on-select is implemented at `app.tsx:945-953`: `onMouseUp` on the root `<box>` calls `Selection.copy(renderer, toast)` which calls `renderer.getSelection().getSelectedText()` then `Clipboard.copy()`. Default-on, but flippable to right-click-to-copy + Ctrl+C-to-copy via `OPENCODE_EXPERIMENTAL_DISABLE_COPY_ON_SELECT` (default-on for Win32).

Wheel is handled inside opentui's `<scrollbox>` primitive with `stickyScroll={true}` and configurable `scroll_speed` / `scroll_acceleration`. When `mouse = false`, the renderer receives no events and wheel scroll is lost, since there is no fallback to `?1007` alternate-scroll.

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

Failure modes: rejected payloads are silently dropped. The app cannot detect support, so the user gets no clipboard write and no error. Falling back to native clipboard requires a separate channel like the `arboard` crate.

## OSC 8 protocol

`\x1b]8;params;URI<terminator><text>\x1b]8;;<terminator>` where `params` is `key=value:key=value` (often empty), `URI` is the link target, and `<terminator>` is either `\x1b\\` (ST, two bytes) or `\x07` (BEL, one byte). xterm.js-based terminals (VS Code's and Cursor's integrated terminals) misparse self-contained per-cell ST closers and leak visible bytes into adjacent cells, so the BEL form is the universally-safe choice.

Modern support: iTerm2, WezTerm, kitty, Alacritty, foot, Konsole, Ghostty, recent Windows Terminal, GNOME Terminal, VTE-based terminals, VS Code's terminal, Cursor's terminal. Legacy terminals print the escape bytes literally. The `<text>` part is what users see, so the fallback is graceful as long as the visible text alone is meaningful (e.g., `#NN` works, while an empty link doesn't).

`unicode-width` reports 0 for ESC and the printable bytes inside a `]8;;...\x07` sequence are also non-printable, so layout math sees the whole sequence as zero-width when wrapped in `Span::raw`. Truncation logic that measures `Span::content` width via `unicode_width::UnicodeWidthStr::width` is unaffected.

Two ratatui-specific traps appear when embedding the envelope inside a frame buffer cell:

1. **`Buffer::set_string` strips control chars.** Embedding ESC in a `Span::raw` strips the leading and trailing escape and leaks the printable middle. `Cell::set_symbol` bypasses that filter and stores the symbol verbatim, and crossterm's `Print(cell.symbol())` writes the bytes unchanged.
2. **`Buffer::diff` over-counts the cell's width.** The diff reads `cell.symbol().width()` to compute `to_skip` for trailing cells (multi-width handling). A symbol like `\x1b]8;;https://github.com/o/r/pull/86\x07#\x1b]8;;\x07` is mostly plain ASCII (the URL), so `unicode-width` reports ~30. The diff then skips ~30 trailing cells, dropping the rest of `#86` and shifting later text into the gap. Codex's `mark_url_hyperlink` writes the envelope into cell symbols too, but no Codex test exercises `Terminal::flush`, so the same bug lurks there.

The clean workaround: emit the OSC 8 envelope **out-of-band**, after `terminal.draw()` has flushed. Capture the link rect + visible cells at render time, then write CUP + `\x1b]8;;<URL>\x07` + replayed styled cells + `\x1b]8;;\x07` + SGR reset directly to the crossterm backend. Buffer cells stay plain (width 1) and the diff math behaves.

## Mouse capture mode bundle

`crossterm::EnableMouseCapture` writes five DECSETs:

- `?1000h`: X10 / normal tracking (button press / release).
- `?1002h`: button-event tracking (adds drag while button held).
- `?1003h`: any-event tracking (adds motion without button).
- `?1006h`: SGR encoding (`\x1b[<button;col;rowM`, supports cols > 223).
- `?1015h`: URXVT encoding (legacy fallback).

Some terminals skip `?1003` for performance. SGR (`?1006`) is the only encoding modern crossterm reads, but the others are needed for older terminals that don't speak SGR. There's no portable terminal primitive that delivers wheel only without click / drag, so claiming wheel implies claiming the rest.

Terminal-side wheel translation through DECSET 1007 doesn't survive tmux. With `mouse on`, tmux receives the wheel itself and forwards it to a pane only when the pane reports the mouse (`#{mouse_any_flag}`). Otherwise tmux 3.6's default binding passes the event to a pane that drops it, and older defaults or custom bindings enter copy mode.

## User-environment signal

The author's tmux config enables tmux mouse mode, vi copy-mode bindings, and `y` for yank. With `set -g set-clipboard on`, OSC 52 from an inner app passes through to the outer terminal. Wheel-up enters copy-mode when the pane isn't already receiving mouse events. An app that captures mouse intercepts those gestures. The author also runs `ox` inside VS Code's and Cursor's integrated terminals, which are xterm.js-based. Those have looser OSC parsing than xterm proper, so any per-cell escape sequence has to use the safest forms (BEL terminator, no relying on OSC 52 reaching the OS clipboard).

## Takeaway for oxide-code

Wheel scrolling has to work inside tmux, which the author runs with `mouse on`, and DECSET 1007 can't deliver that. Claude Code and Codex both capture the mouse in fullscreen, so oxide-code does the same:

- **Mouse capture.** `enter_tui_mode` enables `EnableMouseCapture`, and the chat scrolls on wheel events. Clicks and drags reach the app, so native selection needs the terminal's bypass modifier until in-app selection lands ([#289](https://github.com/hakula139/oxide-code/issues/289)).
- **App-side selection next.** Both peers keep selection usable under capture by implementing it in the app and copying through the native clipboard with OSC 52 for tmux and SSH. The xterm.js terminals the author uses parse OSC loosely, so the copy path should prefer the native clipboard over relying on OSC 52 alone.
- **OSC 8 on the PR segment, emitted out-of-band after `terminal.draw()` flushes.** Capturing the link rect + visible cells at render time and writing the envelope directly to the crossterm backend avoids both ratatui traps: `Buffer::set_string` no longer filters our control chars, and `Buffer::diff` no longer sees a 30-byte URL as one cell symbol. BEL (`\x07`) avoids xterm.js ST parsing leaks in self-contained per-cell closers.
