use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Initializes the terminal for TUI mode.
///
/// - Enters raw mode (no line buffering, no echo).
/// - Switches to the alternate screen buffer (preserves the user's scrollback).
/// - Enables mouse capture for scroll and click events.
/// - Clears the screen.
///
/// Returns a [`Terminal`] ready for rendering. The caller must ensure
/// [`restore`] is called on exit (including panics — see [`install_panic_hook`]).
pub fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
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
pub fn restore() {
    _ = execute!(
        io::stdout(),
        DisableMouseCapture,
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
pub fn draw_sync(terminal: &mut Tui, f: impl FnOnce(&mut ratatui::Frame)) -> Result<()> {
    queue!(terminal.backend_mut(), terminal::BeginSynchronizedUpdate,)?;
    terminal.draw(f)?;
    queue!(terminal.backend_mut(), terminal::EndSynchronizedUpdate,)?;
    terminal.backend_mut().flush()?;
    Ok(())
}

/// Installs a panic hook that restores the terminal before printing the
/// panic message. Without this, a panic leaves the terminal in raw mode
/// with the alternate screen active, making the error unreadable.
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        restore();
        original_hook(panic_info);
    }));
}
