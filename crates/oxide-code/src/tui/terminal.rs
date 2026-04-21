use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;

pub(crate) type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Initializes the terminal for TUI mode.
///
/// - Enters raw mode (no line buffering, no echo).
/// - Switches to the alternate screen buffer (preserves the user's scrollback).
/// - Enables mouse capture for scroll and click events.
/// - Pushes `DISAMBIGUATE_ESCAPE_CODES` (Kitty keyboard protocol) so
///   Shift+Enter is distinguishable from Enter on supporting terminals.
/// - Clears the screen.
///
/// Returns a [`Terminal`] ready for rendering. The caller must ensure
/// [`restore`] is called on exit (including panics — see [`install_panic_hook`]).
pub(crate) fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        terminal::Clear(terminal::ClearType::All),
    )?;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restores the terminal to its original state.
///
/// - Disables mouse capture.
/// - Leaves the alternate screen buffer.
/// - Disables raw mode.
/// - Shows the cursor (in case it was hidden).
///
/// Safe to call multiple times — each operation is idempotent.
pub(crate) fn restore() {
    _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    );
    _ = disable_raw_mode();
}

/// Wraps a render closure with synchronized output sequences.
///
/// Sends `BeginSynchronizedUpdate` before rendering and
/// `EndSynchronizedUpdate` after, telling the terminal emulator to buffer
/// the entire frame and paint it atomically. This eliminates tearing on
/// terminals that support DEC private mode 2026 (Alacritty, kitty, iTerm2,
/// `WezTerm`, Windows Terminal, tmux).
///
/// Terminals that don't recognize the sequence silently ignore it.
///
/// Generic over the backend writer so tests can drive it with an
/// in-memory `Vec<u8>`; production callers pass the [`Tui`] alias.
pub(crate) fn draw_sync<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    f: impl FnOnce(&mut ratatui::Frame),
) -> Result<()> {
    queue!(terminal.backend_mut(), terminal::BeginSynchronizedUpdate)?;
    terminal.draw(f)?;
    queue!(terminal.backend_mut(), terminal::EndSynchronizedUpdate)?;
    terminal.backend_mut().flush()?;
    Ok(())
}

/// Installs a panic hook that restores the terminal before printing the
/// panic message. Without this, a panic leaves the terminal in raw mode
/// with the alternate screen active, making the error unreadable.
pub(crate) fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore();
        original_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ratatui::{TerminalOptions, Viewport, layout::Rect};

    use super::*;

    // ── draw_sync ──
    //
    // `init` / `restore` need a real TTY; `install_panic_hook` clobbers
    // process-global panic state and cannot run cleanly under parallel
    // tests. `draw_sync` is the one function whose behavior we can pin
    // down in-process by swapping `Stdout` for an in-memory writer.

    // DEC private mode 2026 on/off escape sequences emitted by
    // `BeginSynchronizedUpdate` / `EndSynchronizedUpdate`.
    const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    const END_SYNC: &[u8] = b"\x1b[?2026l";

    /// `Write` sink that mirrors every byte into a shared buffer the
    /// test can inspect after the terminal has borrowed the backend.
    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn draw_sync_brackets_the_render_with_sync_update_bytes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let backend = CrosstermBackend::new(SharedWriter(buf.clone()));
        // `Terminal::new` queries stdout for the window size which fails
        // on CI without a TTY. `Viewport::Fixed` skips that query so the
        // test runs the same whether stdout is a pty or a pipe.
        let opts = TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        };
        let mut terminal = Terminal::with_options(backend, opts).unwrap();

        let mut drew = false;
        draw_sync(&mut terminal, |_frame| drew = true).unwrap();

        assert!(drew, "render closure must be invoked");
        let bytes = buf.lock().unwrap();
        let begin = index_of(&bytes, BEGIN_SYNC).expect("BeginSynchronizedUpdate emitted");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(begin < end, "sync update must bracket the render");
    }
}
