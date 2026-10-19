//! Terminal initialization, restore, and panic hook.

use std::fmt;
use std::io::{self, Stdout, Write};
use std::sync::Mutex;

use anyhow::Result;
#[cfg(windows)]
use anyhow::ensure;
#[cfg(windows)]
use crossterm::ansi_support::supports_ansi;
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
#[cfg(not(windows))]
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, ContentStyle, Print, SetAttribute, SetAttributes, SetBackgroundColor,
    SetForegroundColor, SetUnderlineColor,
};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{Command, execute, queue};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::prelude::{CrosstermBackend, IntoCrossterm};
use tracing::error;

use super::components::status::{HyperlinkCell, StatusHyperlink};
use crate::util::sync::lock_or_recover;

pub(crate) type Tui = Terminal<CrosstermBackend<Stdout>>;

/// A backend the TUI draws frames to and then writes escape sequences to, such as the OSC 8 status
/// links and OSC 52 copies.
pub(super) trait TuiBackend: Backend<Error: Send + Sync + 'static> + Write {}

impl<B: Backend<Error: Send + Sync + 'static> + Write> TuiBackend for B {}

/// Enters raw mode, the alternate screen, mouse capture, bracketed paste, and, outside Windows,
/// Kitty keyboard disambiguation. A failed setup restores the terminal before returning the error.
/// Otherwise the caller must invoke [`restore`] on exit, including panics.
pub(crate) fn init() -> Result<Tui> {
    // Besides answering, the check turns on the console's escape-sequence processing, which every
    // sequence the TUI writes relies on.
    #[cfg(windows)]
    ensure!(
        supports_ansi(),
        "the terminal does not support ANSI escape sequences"
    );
    enable_raw_mode()?;
    open_tui().inspect_err(|_| restore())
}

fn open_tui() -> Result<Tui> {
    let mut stdout = io::stdout();
    enter_tui_mode(&mut stdout)?;
    #[cfg(windows)]
    {
        execute!(stdout, EnableMouseCapture)?;
        write_sgr_mouse(&mut stdout, true)?;
    }

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn enter_tui_mode(stdout: &mut impl Write) -> Result<()> {
    queue!(
        stdout,
        Ansi(EnterAlternateScreen),
        Ansi(EnableBracketedPaste)
    )?;
    // crossterm writes the Kitty keyboard commands only as escape codes and fails them on Windows.
    // It captures the Windows mouse through the console API instead, so `init` enables it there.
    #[cfg(not(windows))]
    {
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        queue!(
            stdout,
            PushKeyboardEnhancementFlags(flags),
            EnableMouseCapture
        )?;
    }
    execute!(stdout, Ansi(terminal::Clear(terminal::ClearType::All)))?;
    Ok(())
}

/// Restores the terminal, then prints deferred background panic reports. Later panics print
/// directly. Safe to call multiple times.
pub(crate) fn restore() {
    // Errors are swallowed because restore runs on the panic path, where surfacing one would mask
    // the original panic or abort cleanup with the terminal still in raw mode.
    let mut stdout = io::stdout();
    #[cfg(windows)]
    {
        _ = execute!(stdout, DisableMouseCapture);
        _ = write_sgr_mouse(&mut stdout, false);
    }
    _ = leave_tui_mode(&mut stdout);
    _ = disable_raw_mode();
    _ = write_deferred_panics(&DEFERRED_PANICS, &mut io::stderr());
}

fn leave_tui_mode(stdout: &mut impl Write) -> Result<()> {
    queue!(stdout, Ansi(DisableBracketedPaste))?;
    #[cfg(not(windows))]
    queue!(stdout, PopKeyboardEnhancementFlags, DisableMouseCapture)?;
    execute!(stdout, Ansi(LeaveAlternateScreen), Ansi(Show))?;
    Ok(())
}

/// Requests or releases SGR mouse reports (DEC mode 1006). Some Windows terminals otherwise send
/// mouse reports as key records, and `ConPTY` translates only SGR reports into mouse records.
#[cfg(any(windows, test))]
fn write_sgr_mouse(out: &mut impl Write, enable: bool) -> io::Result<()> {
    out.write_all(if enable {
        b"\x1b[?1006h"
    } else {
        b"\x1b[?1006l"
    })?;
    out.flush()
}

/// Marks the terminal restored and drains the deferred panic reports into `out`, one per line.
fn write_deferred_panics(deferred: &Mutex<DeferredPanics>, out: &mut impl Write) -> io::Result<()> {
    let reports = {
        let mut deferred = lock_or_recover(deferred, "deferred panics");
        deferred.restored = true;
        std::mem::take(&mut deferred.reports)
    };
    for report in reports {
        writeln!(out, "{report}")?;
    }
    Ok(())
}

/// Runs `f` inside a DEC synchronized update (mode 2026), so the terminal presents everything it
/// writes as one tear-free frame.
pub(super) fn synchronized_update<B: TuiBackend>(
    terminal: &mut Terminal<B>,
    f: impl FnOnce(&mut Terminal<B>) -> Result<()>,
) -> Result<()> {
    queue!(terminal.backend_mut(), terminal::BeginSynchronizedUpdate)?;
    let result = f(terminal);
    queue!(terminal.backend_mut(), terminal::EndSynchronizedUpdate)?;
    Write::flush(terminal.backend_mut())?;
    result
}

pub(crate) fn write_status_hyperlinks<W: Write>(
    out: &mut W,
    links: &[StatusHyperlink],
) -> io::Result<()> {
    // Plain OSC 8 suits tmux as well: 3.4+ maps it to pane cells and forwards it to outer
    // terminals with the `hyperlinks` feature, and older versions drop the OSC but keep the text.
    out.write_all(&build_status_hyperlink_envelope(links)?)
}

/// Builds the OSC 8 byte stream for the link batch. Empty when no link has a usable URL.
fn build_status_hyperlink_envelope(links: &[StatusHyperlink]) -> io::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut wrote_any = false;
    for link in links {
        let safe_url: String = link.url.chars().filter(|c| !c.is_control()).collect();
        if safe_url.is_empty() {
            continue;
        }
        if !wrote_any {
            queue!(buf, Ansi(SavePosition))?;
            wrote_any = true;
        }
        queue!(buf, Ansi(MoveTo(link.rect.x, link.rect.y)))?;
        write!(buf, "\x1b]8;;{safe_url}\x07")?;
        for cell in &link.cells {
            queue_cell(&mut buf, cell)?;
        }
        write!(buf, "\x1b]8;;\x07")?;
    }
    if wrote_any {
        queue!(buf, Ansi(RestorePosition))?;
    }
    Ok(buf)
}

/// Queues `cell`'s symbol in its style, resetting the style after a styled cell. crossterm's
/// `PrintStyledContent` and `SetStyle` send their colors through the Windows console API when it
/// finds no escape-sequence support, so each color goes out as its own [`Ansi`] command.
fn queue_cell(out: &mut impl Write, cell: &HyperlinkCell) -> io::Result<()> {
    let style = cell.style.into_crossterm();
    if style == ContentStyle::default() {
        return queue!(out, Print(&cell.symbol));
    }
    if let Some(color) = style.background_color {
        queue!(out, Ansi(SetBackgroundColor(color)))?;
    }
    if let Some(color) = style.foreground_color {
        queue!(out, Ansi(SetForegroundColor(color)))?;
    }
    if let Some(color) = style.underline_color {
        queue!(out, Ansi(SetUnderlineColor(color)))?;
    }
    queue!(
        out,
        Ansi(SetAttributes(style.attributes)),
        Print(&cell.symbol),
        Ansi(SetAttribute(Attribute::Reset)),
    )
}

/// Writes the wrapped command as its escape sequence even where crossterm would call the Windows
/// console API in its place. [`init`] requires a terminal that takes escape sequences, and output
/// built in memory has no console to call.
struct Ansi<C>(C);

impl<C: Command> Command for Ansi<C> {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        self.0.write_ansi(f)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        self.0.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Background-thread panic reports waiting for [`restore`] to print them.
#[derive(Default)]
struct DeferredPanics {
    /// Whether [`restore`] has run, after which panics print through the original hook.
    restored: bool,
    reports: Vec<String>,
}

static DEFERRED_PANICS: Mutex<DeferredPanics> = Mutex::new(DeferredPanics {
    restored: false,
    reports: Vec::new(),
});

/// What the panic hook does with one panic.
#[derive(Debug, PartialEq, Eq)]
enum PanicRoute {
    /// Restores the terminal, then prints through the original hook.
    RestoreThenPrint,
    /// Prints through the original hook, since the terminal is already restored.
    Print,
    /// Held for [`restore`] to print.
    Deferred,
}

/// Installs a panic hook that restores the terminal before delegating to the previous hook when
/// the calling (UI) thread panics.
///
/// tokio catches a spawned task's panic and the TUI keeps running, so that report goes to the log
/// and waits for [`restore`]. Printing it mid-session would scribble over the frame.
pub(crate) fn install_panic_hook() {
    let ui_thread = std::thread::current().id();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let thread = std::thread::current();
        let on_ui_thread = thread.id() == ui_thread;
        match route_panic(&DEFERRED_PANICS, on_ui_thread, thread.name(), panic_info) {
            PanicRoute::RestoreThenPrint => {
                restore();
                original_hook(panic_info);
            }
            PanicRoute::Print => original_hook(panic_info),
            PanicRoute::Deferred => {}
        }
    }));
}

/// Routes one panic, queueing a background thread's report while the TUI owns the terminal.
fn route_panic(
    deferred: &Mutex<DeferredPanics>,
    on_ui_thread: bool,
    thread_name: Option<&str>,
    panic_info: &dyn fmt::Display,
) -> PanicRoute {
    let mut deferred = lock_or_recover(deferred, "deferred panics");
    if deferred.restored {
        return PanicRoute::Print;
    }
    if on_ui_thread {
        return PanicRoute::RestoreThenPrint;
    }
    let report = format!(
        "thread '{}' {panic_info}",
        thread_name.unwrap_or("<unnamed>")
    );
    deferred.reports.push(report.clone());
    drop(deferred);
    error!("{report}");
    PanicRoute::Deferred
}

// ── Testing ──

#[cfg(test)]
pub(super) mod testing {
    //! A terminal stand-in for render tests that records the escape sequences written to it.

    use std::convert::Infallible;

    use ratatui::backend::{ClearType, TestBackend, WindowSize};
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::layout::{Position, Size};

    use super::*;

    pub(crate) const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
    pub(crate) const END_SYNC: &[u8] = b"\x1b[?2026l";
    pub(crate) const FLUSH: &[u8] = b"<flush>";
    /// Where [`CaptureBackend`]'s byte log shows a frame the terminal finished drawing.
    pub(crate) const FRAME: &[u8] = b"<frame>";

    /// A [`TestBackend`] screen that also logs the bytes written to it, with a [`FRAME`] at the end
    /// of each draw, and none of the Windows console detection a `CrosstermBackend` goes through.
    pub(crate) struct CaptureBackend {
        screen: TestBackend,
        bytes: Vec<u8>,
    }

    impl CaptureBackend {
        pub(crate) fn buffer(&self) -> &Buffer {
            self.screen.buffer()
        }

        pub(crate) fn bytes(&self) -> &[u8] {
            &self.bytes
        }
    }

    impl Backend for CaptureBackend {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.screen.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Infallible> {
            self.screen.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Infallible> {
            self.screen.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Infallible> {
            self.screen.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Infallible> {
            self.screen.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Infallible> {
            self.screen.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Infallible> {
            self.screen.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Infallible> {
            self.screen.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Infallible> {
            self.screen.window_size()
        }

        fn flush(&mut self) -> Result<(), Infallible> {
            self.bytes.extend_from_slice(FRAME);
            Backend::flush(&mut self.screen)
        }
    }

    impl Write for CaptureBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.bytes.extend_from_slice(FLUSH);
            Ok(())
        }
    }

    /// A `width` x `height` terminal drawing to a [`CaptureBackend`].
    pub(crate) fn capture_terminal(width: u16, height: u16) -> Terminal<CaptureBackend> {
        let backend = CaptureBackend {
            screen: TestBackend::new(width, height),
            bytes: Vec::new(),
        };
        Terminal::new(backend).expect("a test backend reports its size")
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use indoc::indoc;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};

    use super::testing::{BEGIN_SYNC, END_SYNC, FLUSH, capture_terminal};
    use super::*;

    fn plain_hyperlink(rect: Rect, url: &str, symbols: &[&str]) -> StatusHyperlink {
        StatusHyperlink {
            rect,
            url: url.to_owned(),
            cells: symbols
                .iter()
                .map(|s| HyperlinkCell {
                    symbol: (*s).to_owned(),
                    style: Style::default(),
                })
                .collect(),
        }
    }

    mod enter_tui_mode {
        use super::*;

        const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
        const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
        #[cfg(not(windows))]
        const PUSH_KITTY_DISAMBIGUATE: &[u8] = b"\x1b[>1u";
        #[cfg(not(windows))]
        const ENABLE_MOUSE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
        const CLEAR_SCREEN: &[u8] = b"\x1b[2J";

        #[test]
        fn writes_setup_sequences() {
            let mut buf = Vec::new();

            enter_tui_mode(&mut buf).unwrap();

            #[cfg(not(windows))]
            let expected = [
                ENTER_ALT_SCREEN,
                ENABLE_BRACKETED_PASTE,
                PUSH_KITTY_DISAMBIGUATE,
                ENABLE_MOUSE,
                CLEAR_SCREEN,
            ];
            #[cfg(windows)]
            let expected = [ENTER_ALT_SCREEN, ENABLE_BRACKETED_PASTE, CLEAR_SCREEN];
            assert_eq!(
                buf.escape_ascii().to_string(),
                expected.concat().escape_ascii().to_string()
            );
        }
    }

    mod leave_tui_mode {
        use super::*;

        const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
        #[cfg(not(windows))]
        const POP_KITTY_FLAGS: &[u8] = b"\x1b[<1u";
        #[cfg(not(windows))]
        const DISABLE_MOUSE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
        const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";
        const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

        #[test]
        fn writes_restore_sequences() {
            let mut buf = Vec::new();

            leave_tui_mode(&mut buf).unwrap();

            #[cfg(not(windows))]
            let expected = [
                DISABLE_BRACKETED_PASTE,
                POP_KITTY_FLAGS,
                DISABLE_MOUSE,
                LEAVE_ALT_SCREEN,
                SHOW_CURSOR,
            ];
            #[cfg(windows)]
            let expected = [DISABLE_BRACKETED_PASTE, LEAVE_ALT_SCREEN, SHOW_CURSOR];
            assert_eq!(
                buf.escape_ascii().to_string(),
                expected.concat().escape_ascii().to_string()
            );
        }

        #[test]
        fn write_failure_returns_the_io_error() {
            let mut out = &mut [][..];

            let error = leave_tui_mode(&mut out).unwrap_err();

            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::WriteZero,
            );
        }
    }

    mod write_sgr_mouse {
        use super::*;

        #[test]
        fn toggles_dec_mode_1006() {
            let (mut enable, mut disable) = (Vec::new(), Vec::new());

            write_sgr_mouse(&mut enable, true).unwrap();
            write_sgr_mouse(&mut disable, false).unwrap();

            assert_eq!(
                (enable.as_slice(), disable.as_slice()),
                (&b"\x1b[?1006h"[..], &b"\x1b[?1006l"[..])
            );
        }
    }

    mod write_deferred_panics {
        use super::*;

        #[test]
        fn prints_each_report_once() {
            let deferred = Mutex::new(DeferredPanics::default());
            route_panic(&deferred, false, Some("a"), &"panicked at x.rs:1:1:\nfirst");
            route_panic(
                &deferred,
                false,
                Some("b"),
                &"panicked at y.rs:2:2:\nsecond",
            );

            let mut first = Vec::new();
            write_deferred_panics(&deferred, &mut first).unwrap();
            let mut second = Vec::new();
            write_deferred_panics(&deferred, &mut second).unwrap();

            assert_eq!(
                String::from_utf8(first).unwrap(),
                indoc! {"
                    thread 'a' panicked at x.rs:1:1:
                    first
                    thread 'b' panicked at y.rs:2:2:
                    second
                "},
            );
            assert!(second.is_empty(), "a repeat restore must not reprint");
        }
    }

    mod synchronized_update {
        use super::*;

        #[test]
        fn brackets_everything_the_closure_writes() {
            let mut terminal = capture_terminal(4, 1);

            synchronized_update(&mut terminal, |terminal| {
                terminal.backend_mut().write_all(b"LINKS")?;
                Ok(())
            })
            .unwrap();

            assert_eq!(
                terminal.backend().bytes(),
                [BEGIN_SYNC, b"LINKS", END_SYNC, FLUSH].concat(),
            );
        }

        #[test]
        fn ends_the_update_when_the_closure_fails() {
            let mut terminal = capture_terminal(4, 1);

            let result = synchronized_update(&mut terminal, |_| Err(anyhow!("draw failed")));

            assert_eq!(result.unwrap_err().to_string(), "draw failed");
            assert_eq!(
                terminal.backend().bytes(),
                [BEGIN_SYNC, END_SYNC, FLUSH].concat()
            );
        }
    }

    mod write_status_hyperlinks {
        use super::*;

        #[test]
        fn writes_the_envelope() {
            let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "https://example.com", &["x"]);
            let mut buf = Vec::new();

            write_status_hyperlinks(&mut buf, &[link]).unwrap();

            assert_eq!(
                String::from_utf8(buf).unwrap(),
                "\x1b7\x1b[1;1H\x1b]8;;https://example.com\x07x\x1b]8;;\x07\x1b8",
            );
        }

        #[test]
        fn is_a_noop_when_no_links_pending() {
            let mut buf: Vec<u8> = Vec::new();

            write_status_hyperlinks(&mut buf, &[]).unwrap();

            assert_eq!(buf, b"");
        }
    }

    mod build_status_hyperlink_envelope {
        use super::*;

        #[test]
        fn wraps_each_link_in_cup_and_osc8() {
            let links = [
                plain_hyperlink(
                    Rect::new(2, 0, 3, 1),
                    "https://example.com/pull/86",
                    &["#", "8", "6"],
                ),
                plain_hyperlink(Rect::new(9, 1, 2, 1), "https://example.com/ci", &["C", "I"]),
            ];

            let bytes = build_status_hyperlink_envelope(&links).unwrap();

            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                "\x1b7\
                 \x1b[1;3H\x1b]8;;https://example.com/pull/86\x07#86\x1b]8;;\x07\
                 \x1b[2;10H\x1b]8;;https://example.com/ci\x07CI\x1b]8;;\x07\
                 \x1b8",
            );
        }

        #[test]
        fn replays_each_cell_style_and_resets_it() {
            const CHILD: &str = "OX_TEST_HYPERLINK_STYLES_CHILD";
            if std::env::var_os(CHILD).is_none() {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap();

                // crossterm caches NO_COLOR globally, so isolate this exact-color fixture.
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", test_name, "--nocapture"])
                    .env(CHILD, "1")
                    .env_remove("NO_COLOR")
                    .output()
                    .unwrap();

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    output.status.success() && stdout.contains(&format!("test {test_name} ... ok")),
                    "{stdout}{stderr}",
                );
                return;
            }

            let link = StatusHyperlink {
                rect: Rect::new(0, 0, 2, 1),
                url: "https://x".to_owned(),
                cells: vec![
                    HyperlinkCell {
                        symbol: "A".to_owned(),
                        style: Style::default()
                            .fg(Color::Red)
                            .bg(Color::Blue)
                            .underline_color(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    },
                    HyperlinkCell {
                        symbol: "B".to_owned(),
                        style: Style::default().add_modifier(Modifier::ITALIC),
                    },
                ],
            };

            let bytes = build_status_hyperlink_envelope(&[link]).unwrap();

            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                "\x1b7\x1b[1;1H\x1b]8;;https://x\x07\
                 \x1b[48;5;4m\x1b[38;5;1m\x1b[58;5;2m\x1b[1mA\x1b[0m\x1b[3mB\x1b[0m\
                 \x1b]8;;\x07\x1b8",
            );
        }

        #[test]
        fn strips_control_chars_from_url() {
            let link = plain_hyperlink(
                Rect::new(0, 0, 1, 1),
                "https://example.com/\x1b\x07\x00ok",
                &["x"],
            );

            let bytes = build_status_hyperlink_envelope(&[link]).unwrap();

            assert_eq!(
                String::from_utf8(bytes).unwrap(),
                "\x1b7\x1b[1;1H\x1b]8;;https://example.com/ok\x07x\x1b]8;;\x07\x1b8",
            );
        }

        #[test]
        fn is_empty_when_safe_url_is_empty() {
            let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "\x1b\x07", &["x"]);

            let bytes = build_status_hyperlink_envelope(&[link]).unwrap();

            assert_eq!(bytes, b"");
        }
    }

    mod route_panic {
        use super::*;

        #[test]
        fn restores_before_printing_a_ui_thread_panic() {
            let deferred = Mutex::new(DeferredPanics::default());
            let route = route_panic(
                &deferred,
                true,
                Some("main"),
                &"panicked at x.rs:1:1:\nboom",
            );
            assert_eq!(route, PanicRoute::RestoreThenPrint);
            assert_eq!(deferred.lock().unwrap().reports, [] as [String; 0]);
        }

        #[test]
        fn defers_a_background_panic_and_names_its_thread() {
            let deferred = Mutex::new(DeferredPanics::default());
            let route = route_panic(&deferred, false, None, &"panicked at x.rs:1:1:\nboom");
            assert_eq!(route, PanicRoute::Deferred);
            assert_eq!(
                deferred.lock().unwrap().reports,
                ["thread '<unnamed>' panicked at x.rs:1:1:\nboom"],
            );
        }

        #[test]
        fn prints_directly_once_the_terminal_is_restored() {
            let deferred = Mutex::new(DeferredPanics::default());
            write_deferred_panics(&deferred, &mut Vec::new()).unwrap();

            for on_ui_thread in [false, true] {
                let route = route_panic(&deferred, on_ui_thread, Some("worker"), &"late");
                assert_eq!(route, PanicRoute::Print, "on_ui_thread = {on_ui_thread}");
            }
            assert_eq!(
                deferred.lock().unwrap().reports,
                [] as [String; 0],
                "nothing is left for a restore that already ran",
            );
        }
    }
}
