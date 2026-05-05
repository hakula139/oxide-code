//! Terminal initialization, restore, and panic hook.

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

/// Enters raw mode + alt screen + mouse + Kitty keyboard. Caller must invoke [`restore`] on exit
/// (including panics).
pub(crate) fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_tui_mode(&mut stdout)?;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn enter_tui_mode(stdout: &mut impl Write) -> Result<()> {
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        terminal::Clear(terminal::ClearType::All),
    )?;
    Ok(())
}

/// Restores the terminal to its original state. Safe to call multiple times.
pub(crate) fn restore() {
    let mut stdout = io::stdout();
    _ = leave_tui_mode(&mut stdout);
    _ = disable_raw_mode();
}

fn leave_tui_mode(stdout: &mut impl Write) -> Result<()> {
    execute!(
        stdout,
        DisableMouseCapture,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    )?;
    Ok(())
}

/// Brackets a render closure with DEC synchronized-update sequences to eliminate tearing.
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

/// Installs a panic hook that restores the terminal before printing the panic message.
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

    // ── enter_tui_mode ──

    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
    const CLEAR_SCREEN: &[u8] = b"\x1b[2J";

    #[test]
    fn enter_tui_mode_writes_setup_sequences() {
        let mut buf = Vec::new();

        enter_tui_mode(&mut buf).unwrap();

        let enter = index_of(&buf, ENTER_ALT_SCREEN).expect("alternate screen entered");
        let clear = index_of(&buf, CLEAR_SCREEN).expect("screen cleared");
        assert!(
            enter < clear,
            "setup should enter alternate screen before clearing"
        );
    }

    // ── leave_tui_mode ──

    const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

    #[test]
    fn leave_tui_mode_writes_restore_sequences() {
        let mut buf = Vec::new();

        leave_tui_mode(&mut buf).unwrap();

        let leave = index_of(&buf, LEAVE_ALT_SCREEN).expect("alternate screen left");
        let show = index_of(&buf, SHOW_CURSOR).expect("cursor shown");
        assert!(
            leave < show,
            "restore should leave alternate screen before showing cursor"
        );
    }

    // ── draw_sync ──

    const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    const END_SYNC: &[u8] = b"\x1b[?2026l";

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
        // `Terminal::new` queries stdout size and fails on CI without a TTY; `Viewport::Fixed`
        // skips that query.
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
