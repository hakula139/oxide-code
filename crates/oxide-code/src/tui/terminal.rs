//! Terminal initialization, restore, and panic hook.

use std::fmt;
use std::io::{self, Stdout, Write};
use std::sync::{Mutex, PoisonError};

use anyhow::Result;
use crossterm::Command;
use crossterm::cursor::{RestorePosition, SavePosition};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::PrintStyledContent;
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::prelude::{CrosstermBackend, IntoCrossterm};

use super::components::status::StatusHyperlink;

pub(crate) type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enters raw mode + alt screen + alternate-scroll + bracketed paste + Kitty keyboard (outside
/// Windows). Caller must invoke [`restore`] on exit (including panics).
pub(crate) fn init() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_tui_mode(&mut stdout)?;

    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn enter_tui_mode(stdout: &mut impl Write) -> Result<()> {
    queue!(
        stdout,
        EnterAlternateScreen,
        AlternateScroll { enable: true },
        EnableBracketedPaste
    )?;
    // crossterm writes the Kitty keyboard commands only as escape codes and fails them on Windows.
    #[cfg(not(windows))]
    {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
        queue!(stdout, PushKeyboardEnhancementFlags(flags))?;
    }
    execute!(stdout, terminal::Clear(terminal::ClearType::All))?;
    Ok(())
}

/// Restores the terminal, then prints deferred background panic reports. Later panics print
/// directly. Safe to call multiple times.
pub(crate) fn restore() {
    // Errors are swallowed because restore runs on the panic path, where surfacing one would mask
    // the original panic or abort cleanup with the terminal still in raw mode.
    let mut stdout = io::stdout();
    _ = leave_tui_mode(&mut stdout);
    _ = disable_raw_mode();
    _ = write_deferred_panics(&DEFERRED_PANICS, &mut io::stderr());
}

fn leave_tui_mode(stdout: &mut impl Write) -> Result<()> {
    queue!(
        stdout,
        AlternateScroll { enable: false },
        DisableBracketedPaste
    )?;
    #[cfg(not(windows))]
    queue!(stdout, crossterm::event::PopKeyboardEnhancementFlags)?;
    execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show)?;
    Ok(())
}

/// Marks the terminal restored and drains the deferred panic reports into `out`, one per line.
fn write_deferred_panics(deferred: &Mutex<DeferredPanics>, out: &mut impl Write) -> io::Result<()> {
    let reports = {
        let mut deferred = deferred.lock().unwrap_or_else(PoisonError::into_inner);
        deferred.restored = true;
        std::mem::take(&mut deferred.reports)
    };
    for report in reports {
        writeln!(out, "{report}")?;
    }
    Ok(())
}

/// DEC mode 1007, which makes the terminal send wheel scrolls in the alternate screen as arrow
/// keys. Wheel scrolling then works without mouse capture, so native drag selection stays usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AlternateScroll {
    enable: bool,
}

impl Command for AlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007{}", if self.enable { 'h' } else { 'l' })
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "AlternateScroll requires ANSI sequences",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Runs `f` inside a DEC synchronized update (mode 2026), so the terminal presents everything it
/// writes as one tear-free frame.
pub(crate) fn synchronized_update<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    f: impl FnOnce(&mut Terminal<CrosstermBackend<W>>) -> Result<()>,
) -> Result<()> {
    queue!(terminal.backend_mut(), terminal::BeginSynchronizedUpdate)?;
    let result = f(terminal);
    queue!(terminal.backend_mut(), terminal::EndSynchronizedUpdate)?;
    terminal.backend_mut().flush()?;
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
            queue!(&mut buf, SavePosition)?;
            wrote_any = true;
        }
        let row = link.rect.y.saturating_add(1);
        let col = link.rect.x.saturating_add(1);
        write!(buf, "\x1b[{row};{col}H")?;
        write!(buf, "\x1b]8;;{safe_url}\x07")?;
        for cell in &link.cells {
            let style = cell.style.into_crossterm();
            queue!(buf, PrintStyledContent(style.apply(&cell.symbol)))?;
        }
        write!(buf, "\x1b]8;;\x07")?;
        write!(buf, "\x1b[0m")?;
    }
    if wrote_any {
        queue!(&mut buf, RestorePosition)?;
    }
    Ok(buf)
}

/// Background-thread panic reports waiting for [`restore`] to print them.
#[derive(Default)]
struct DeferredPanics {
    /// Set by [`restore`]. From then on panics print through the original hook.
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
    /// Restore the terminal, then print through the original hook.
    RestoreThenPrint,
    /// Print through the original hook, since the terminal is already restored.
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
    let mut deferred = deferred.lock().unwrap_or_else(PoisonError::into_inner);
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
    tracing::error!("{report}");
    PanicRoute::Deferred
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ratatui::style::{Color, Modifier, Style};
    use ratatui::{TerminalOptions, Viewport, layout::Rect};

    use super::*;
    use crate::tui::components::status::{HyperlinkCell, StatusHyperlink};

    // ── enter_tui_mode ──

    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
    const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
    const ENABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007h";
    const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
    const ENABLE_MOUSE_BASIC: &[u8] = b"\x1b[?1000h";
    #[cfg(not(windows))]
    const PUSH_KITTY_DISAMBIGUATE: &[u8] = b"\x1b[>1u";

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
        assert!(
            index_of(&buf, ENABLE_ALT_SCROLL).is_some(),
            "alternate-scroll must be enabled so wheel still scrolls without mouse capture"
        );
        assert!(
            index_of(&buf, ENABLE_BRACKETED_PASTE).is_some(),
            "bracketed paste must be enabled so pasted newlines do not submit"
        );
        #[cfg(not(windows))]
        assert!(
            index_of(&buf, PUSH_KITTY_DISAMBIGUATE).is_some(),
            "Kitty keyboard disambiguation must be pushed outside Windows"
        );
    }

    #[test]
    fn enter_tui_mode_does_not_enable_mouse_capture() {
        // Native drag-select-and-copy depends on the terminal seeing the mouse, so the TUI must
        // never claim it. Pin the negative case so a future refactor can't silently re-enable it.
        let mut buf = Vec::new();

        enter_tui_mode(&mut buf).unwrap();

        assert!(
            index_of(&buf, ENABLE_MOUSE_BASIC).is_none(),
            "EnableMouseCapture would suppress native terminal drag-select"
        );
    }

    // ── leave_tui_mode ──

    const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";
    const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
    const DISABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007l";
    const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
    #[cfg(not(windows))]
    const POP_KITTY_FLAGS: &[u8] = b"\x1b[<1u";

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
        assert!(
            index_of(&buf, DISABLE_ALT_SCROLL).is_some(),
            "alternate-scroll must be disabled when leaving the TUI"
        );
        assert!(
            index_of(&buf, DISABLE_BRACKETED_PASTE).is_some(),
            "bracketed paste must be disabled when leaving the TUI"
        );
        #[cfg(not(windows))]
        assert!(
            index_of(&buf, POP_KITTY_FLAGS).is_some(),
            "Kitty keyboard flags must be popped outside Windows"
        );
    }

    #[test]
    fn leave_tui_mode_write_failure_returns_the_io_error() {
        let mut out = &mut [][..];

        let error = leave_tui_mode(&mut out).unwrap_err();

        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::WriteZero,
        );
    }

    // ── write_deferred_panics ──

    #[test]
    fn write_deferred_panics_prints_each_report_once() {
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
            indoc::indoc! {"
                thread 'a' panicked at x.rs:1:1:
                first
                thread 'b' panicked at y.rs:2:2:
                second
            "},
        );
        assert!(second.is_empty(), "a repeat restore must not reprint");
    }

    // ── synchronized_update ──

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

    fn fixed_terminal(buf: &Arc<Mutex<Vec<u8>>>) -> Terminal<CrosstermBackend<SharedWriter>> {
        // `Terminal::new` queries stdout size and fails on CI without a TTY. `Viewport::Fixed`
        // skips that query.
        let opts = TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        };
        Terminal::with_options(CrosstermBackend::new(SharedWriter(Arc::clone(buf))), opts).unwrap()
    }

    #[test]
    fn synchronized_update_brackets_everything_the_closure_writes() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = fixed_terminal(&buf);

        synchronized_update(&mut terminal, |terminal| {
            terminal.draw(|_frame| {})?;
            terminal.backend_mut().write_all(b"LINKS")?;
            Ok(())
        })
        .unwrap();

        let bytes = buf.lock().unwrap();
        let begin = index_of(&bytes, BEGIN_SYNC).expect("BeginSynchronizedUpdate emitted");
        let links = index_of(&bytes, b"LINKS").expect("closure output written");
        let end = index_of(&bytes, END_SYNC).expect("EndSynchronizedUpdate emitted");
        assert!(begin < links && links < end, "{bytes:?}");
    }

    #[test]
    fn synchronized_update_ends_the_update_when_the_closure_fails() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let mut terminal = fixed_terminal(&buf);

        let result = synchronized_update(&mut terminal, |_| Err(anyhow::anyhow!("draw failed")));

        assert_eq!(result.unwrap_err().to_string(), "draw failed");
        let bytes = buf.lock().unwrap();
        assert!(
            index_of(&bytes, END_SYNC).is_some(),
            "a failed draw must not leave the terminal holding frames",
        );
    }

    // ── write_status_hyperlinks ──

    #[test]
    fn write_status_hyperlinks_writes_the_plain_envelope_inside_and_outside_tmux() {
        let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "https://x", &["x"]);
        let envelope = build_status_hyperlink_envelope(std::slice::from_ref(&link)).unwrap();
        for tmux in [None, Some("/tmp/tmux-1000/default,1234,0")] {
            let mut buf: Vec<u8> = Vec::new();
            temp_env::with_var("TMUX", tmux, || {
                write_status_hyperlinks(&mut buf, std::slice::from_ref(&link)).unwrap();
            });
            assert_eq!(buf, envelope, "TMUX={tmux:?}");
        }
    }

    #[test]
    fn write_status_hyperlinks_is_a_noop_when_no_links_pending() {
        let mut buf: Vec<u8> = Vec::new();
        write_status_hyperlinks(&mut buf, &[]).unwrap();
        assert!(buf.is_empty(), "no bytes for an empty link list");
    }

    // ── build_status_hyperlink_envelope ──

    #[test]
    fn build_status_hyperlink_envelope_wraps_each_link_in_cup_and_osc8() {
        let link = plain_hyperlink(
            Rect::new(2, 0, 3, 1),
            "https://example.com/pull/86",
            &["#", "8", "6"],
        );

        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();

        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\x1b7\x1b[1;3H\x1b]8;;https://example.com/pull/86\x07#86\x1b]8;;\x07\x1b[0m\x1b8",
        );
    }

    #[test]
    fn build_status_hyperlink_envelope_replays_each_cell_style_and_resets_it() {
        let link = StatusHyperlink {
            rect: Rect::new(0, 0, 2, 1),
            url: "https://x".to_owned(),
            cells: vec![
                HyperlinkCell {
                    symbol: "A".to_owned(),
                    style: Style::default()
                        .fg(Color::Red)
                        .bg(Color::Blue)
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
            "\x1b7\x1b[1;1H\x1b]8;;https://x\x07\x1b[48;5;4m\x1b[38;5;1m\x1b[1mA\x1b[0m\x1b[3mB\x1b[0m\
             \x1b]8;;\x07\x1b[0m\x1b8",
        );
    }

    #[test]
    fn build_status_hyperlink_envelope_strips_control_chars_from_url() {
        let link = plain_hyperlink(
            Rect::new(0, 0, 1, 1),
            "https://x.com/\x1b\x07\x00ok",
            &["x"],
        );
        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(
            output.contains("\x1b]8;;https://x.com/ok\x07"),
            "ESC, BEL, NUL stripped from URL: {output:?}",
        );
    }

    #[test]
    fn build_status_hyperlink_envelope_is_empty_when_safe_url_is_empty() {
        let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "\x1b\x07", &["x"]);
        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();
        assert!(
            bytes.is_empty(),
            "no bytes (no DECSC either) when sanitized URL is empty",
        );
    }

    // ── route_panic ──

    #[test]
    fn route_panic_restores_before_printing_a_ui_thread_panic() {
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
    fn route_panic_defers_a_background_panic_and_names_its_thread() {
        let deferred = Mutex::new(DeferredPanics::default());
        let route = route_panic(&deferred, false, None, &"panicked at x.rs:1:1:\nboom");
        assert_eq!(route, PanicRoute::Deferred);
        assert_eq!(
            deferred.lock().unwrap().reports,
            ["thread '<unnamed>' panicked at x.rs:1:1:\nboom"],
        );
    }

    #[test]
    fn route_panic_prints_directly_once_the_terminal_is_restored() {
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
