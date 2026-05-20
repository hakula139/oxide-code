//! Terminal initialization, restore, and panic hook.

use std::fmt;
use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::Command;
use crossterm::cursor::{RestorePosition, SavePosition};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute as CtAttribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;

use super::components::status::{HyperlinkCell, StatusHyperlink};

pub(crate) type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Enters raw mode + alt screen + alternate-scroll + Kitty keyboard. Caller must invoke [`restore`]
/// on exit (including panics).
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
        EnableAlternateScroll,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        terminal::Clear(terminal::ClearType::All),
    )?;
    Ok(())
}

/// Restores the terminal to its original state. Safe to call multiple times.
///
/// Errors from each step are intentionally swallowed — restore runs on the panic path and during
/// normal shutdown, where surfacing an `io::Error` would either mask the original panic or
/// abort cleanup midway and leave the terminal in raw mode.
pub(crate) fn restore() {
    let mut stdout = io::stdout();
    _ = leave_tui_mode(&mut stdout);
    _ = disable_raw_mode();
}

fn leave_tui_mode(stdout: &mut impl Write) -> Result<()> {
    execute!(
        stdout,
        DisableAlternateScroll,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        crossterm::cursor::Show,
    )?;
    Ok(())
}

/// DECSET `?1007h`. Tells the terminal emulator to translate physical wheel-mouse events into
/// arrow-key sequences while the alternate screen is active. Wheel scroll keeps working without
/// claiming `EnableMouseCapture`, so native drag-select-and-copy stays available to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "EnableAlternateScroll requires ANSI sequences",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// DECSET `?1007l`. Pairs with [`EnableAlternateScroll`] on shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "DisableAlternateScroll requires ANSI sequences",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Brackets a render closure with DEC synchronized-update (mode 2026) sequences so the terminal
/// presents the new frame atomically; without this, fast successive renders can show partial
/// frames with mid-line tearing.
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

/// Writes status-link OSC 8 envelopes, using tmux DCS pass-through when needed.
pub(crate) fn write_status_hyperlinks<W: Write>(
    out: &mut W,
    links: &[StatusHyperlink],
) -> io::Result<()> {
    let envelope = build_status_hyperlink_envelope(links)?;
    if envelope.is_empty() {
        return Ok(());
    }
    if running_inside_tmux() {
        out.write_all(&tmux_passthrough(&envelope))?;
    } else {
        out.write_all(&envelope)?;
    }
    out.flush()?;
    Ok(())
}

/// Builds the OSC 8 byte stream for the link batch. Empty when no link has a usable URL.
fn build_status_hyperlink_envelope(links: &[StatusHyperlink]) -> io::Result<Vec<u8>> {
    use std::io::Write as _;

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
            write_styled_symbol(&mut buf, cell)?;
        }
        write!(buf, "\x1b]8;;\x07")?;
        write!(buf, "\x1b[0m")?;
    }
    if wrote_any {
        queue!(&mut buf, RestorePosition)?;
    }
    Ok(buf)
}

/// Empty `$TMUX` is treated as absent to avoid writing DCS bytes to a raw terminal.
fn running_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

/// Wraps an escape sequence in tmux's DCS pass-through so OSC 8 reaches the outer terminal.
fn tmux_passthrough(escape: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escape.len() + 8);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in escape {
        if b == 0x1b {
            out.push(0x1b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Replays one captured cell, resetting SGR first so modifiers cannot leak across cells.
fn write_styled_symbol<W: Write>(out: &mut W, cell: &HyperlinkCell) -> io::Result<()> {
    queue!(out, SetAttribute(CtAttribute::Reset))?;
    if let Some(fg) = cell.style.fg {
        queue!(out, SetForegroundColor(ratatui_color_to_crossterm(fg)))?;
    }
    if let Some(bg) = cell.style.bg {
        queue!(out, SetBackgroundColor(ratatui_color_to_crossterm(bg)))?;
    }
    let modifier = cell.style.add_modifier;
    if modifier.contains(ratatui::style::Modifier::BOLD) {
        queue!(out, SetAttribute(CtAttribute::Bold))?;
    }
    if modifier.contains(ratatui::style::Modifier::DIM) {
        queue!(out, SetAttribute(CtAttribute::Dim))?;
    }
    if modifier.contains(ratatui::style::Modifier::ITALIC) {
        queue!(out, SetAttribute(CtAttribute::Italic))?;
    }
    if modifier.contains(ratatui::style::Modifier::UNDERLINED) {
        queue!(out, SetAttribute(CtAttribute::Underlined))?;
    }
    if modifier.contains(ratatui::style::Modifier::REVERSED) {
        queue!(out, SetAttribute(CtAttribute::Reverse))?;
    }
    queue!(out, Print(&cell.symbol))?;
    Ok(())
}

fn ratatui_color_to_crossterm(c: ratatui::style::Color) -> CtColor {
    use ratatui::style::Color as RC;
    match c {
        RC::Reset => CtColor::Reset,
        RC::Black => CtColor::Black,
        RC::Red => CtColor::DarkRed,
        RC::Green => CtColor::DarkGreen,
        RC::Yellow => CtColor::DarkYellow,
        RC::Blue => CtColor::DarkBlue,
        RC::Magenta => CtColor::DarkMagenta,
        RC::Cyan => CtColor::DarkCyan,
        RC::Gray => CtColor::Grey,
        RC::DarkGray => CtColor::DarkGrey,
        RC::LightRed => CtColor::Red,
        RC::LightGreen => CtColor::Green,
        RC::LightYellow => CtColor::Yellow,
        RC::LightBlue => CtColor::Blue,
        RC::LightMagenta => CtColor::Magenta,
        RC::LightCyan => CtColor::Cyan,
        RC::White => CtColor::White,
        RC::Indexed(i) => CtColor::AnsiValue(i),
        RC::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

/// Installs a panic hook that restores the terminal before delegating to the previous hook.
///
/// Without this, a panic inside the TUI loop would leave the terminal in raw mode + alternate
/// screen, hiding the panic message and forcing the user to reset their shell. The original hook
/// is preserved so any custom panic handler (test harness, backtrace printer, etc.) still runs.
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

    use ratatui::style::{Modifier, Style};
    use ratatui::{TerminalOptions, Viewport, layout::Rect};

    use super::*;
    use crate::tui::components::status::{HyperlinkCell, StatusHyperlink};

    // ── enter_tui_mode ──

    const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
    const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
    const ENABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007h";
    const ENABLE_MOUSE_BASIC: &[u8] = b"\x1b[?1000h";

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

    // ── write_status_hyperlinks ──

    #[test]
    fn write_status_hyperlinks_passes_envelope_through_unchanged_outside_tmux() {
        let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "https://x", &["x"]);
        let mut buf: Vec<u8> = Vec::new();
        temp_env::with_var_unset("TMUX", || {
            write_status_hyperlinks(&mut buf, std::slice::from_ref(&link)).unwrap();
        });
        let envelope = build_status_hyperlink_envelope(std::slice::from_ref(&link)).unwrap();
        assert_eq!(
            buf, envelope,
            "outside tmux the wire bytes match the raw envelope",
        );
    }

    #[test]
    fn write_status_hyperlinks_wraps_envelope_in_dcs_passthrough_inside_tmux() {
        let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "https://x", &["x"]);
        let mut buf: Vec<u8> = Vec::new();
        temp_env::with_var("TMUX", Some("/tmp/tmux-1000/default,1234,0"), || {
            write_status_hyperlinks(&mut buf, &[link]).unwrap();
        });
        assert!(
            buf.starts_with(b"\x1bPtmux;"),
            "DCS opener brackets the envelope: {buf:?}",
        );
        assert!(
            buf.ends_with(b"\x1b\\"),
            "ST closer terminates the DCS: {buf:?}",
        );
        assert!(
            buf.windows(8).any(|w| w == b"\x1b\x1b]8;;ht"),
            "inner ESC before OSC 8 opener is doubled: {buf:?}",
        );
    }

    #[test]
    fn write_status_hyperlinks_treats_empty_tmux_env_as_outside_tmux() {
        let link = plain_hyperlink(Rect::new(0, 0, 1, 1), "https://x", &["x"]);
        let mut buf: Vec<u8> = Vec::new();
        temp_env::with_var("TMUX", Some(""), || {
            write_status_hyperlinks(&mut buf, std::slice::from_ref(&link)).unwrap();
        });
        let envelope = build_status_hyperlink_envelope(std::slice::from_ref(&link)).unwrap();
        assert_eq!(buf, envelope, "empty $TMUX is treated as absent");
    }

    #[test]
    fn write_status_hyperlinks_is_a_noop_when_no_links_pending() {
        let mut buf: Vec<u8> = Vec::new();
        write_status_hyperlinks(&mut buf, &[]).unwrap();
        assert!(buf.is_empty(), "no bytes for an empty link list");
    }

    // ── build_status_hyperlink_envelope ──

    #[test]
    fn build_status_hyperlink_envelope_emits_cup_then_osc8_per_link() {
        let link = plain_hyperlink(
            Rect::new(2, 0, 3, 1),
            "https://example.com/pull/86",
            &["#", "8", "6"],
        );
        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(
            output.starts_with("\x1b7"),
            "DECSC parks the cursor before our writes: {output:?}",
        );
        assert!(
            output.ends_with("\x1b8"),
            "DECRC restores the cursor after our writes: {output:?}",
        );
        assert!(
            output.contains("\x1b[1;3H"),
            "CUP to row 1 col 3 (1-based): {output:?}",
        );
        assert!(
            output.contains("\x1b]8;;https://example.com/pull/86\x07"),
            "OSC 8 opener with URL: {output:?}",
        );
        let osc8_open_at = output.find("\x1b]8;;https").unwrap();
        let osc8_close_at = output.rfind("\x1b]8;;\x07").unwrap();
        let between = &output[osc8_open_at..osc8_close_at];
        assert!(
            between.contains('#') && between.contains('8') && between.contains('6'),
            "visible bytes #, 8, 6 sit between opener and closer: {between:?}",
        );
        assert!(
            output.contains("\x1b]8;;\x07\x1b[0m"),
            "closer + SGR reset: {output:?}",
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
    fn build_status_hyperlink_envelope_replays_styled_cell_modifiers() {
        let modifiers = Modifier::BOLD
            | Modifier::DIM
            | Modifier::ITALIC
            | Modifier::UNDERLINED
            | Modifier::REVERSED;
        let link = StatusHyperlink {
            rect: Rect::new(0, 0, 1, 1),
            url: "https://x".to_owned(),
            cells: vec![HyperlinkCell {
                symbol: "X".to_owned(),
                style: Style::default().add_modifier(modifiers),
            }],
        };
        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        for (sgr, label) in [
            ("\x1b[1m", "bold"),
            ("\x1b[2m", "dim"),
            ("\x1b[3m", "italic"),
            ("\x1b[4m", "underlined"),
            ("\x1b[7m", "reverse"),
        ] {
            assert!(
                output.contains(sgr),
                "missing {label} SGR escape: {output:?}",
            );
        }
        assert!(
            output.contains('X'),
            "visible cell symbol replayed: {output:?}"
        );
    }

    #[test]
    fn build_status_hyperlink_envelope_resets_sgr_between_cells_with_different_modifiers() {
        let link = StatusHyperlink {
            rect: Rect::new(0, 0, 2, 1),
            url: "https://x".to_owned(),
            cells: vec![
                HyperlinkCell {
                    symbol: "A".to_owned(),
                    style: Style::default().add_modifier(Modifier::BOLD),
                },
                HyperlinkCell {
                    symbol: "B".to_owned(),
                    style: Style::default().add_modifier(Modifier::ITALIC),
                },
            ],
        };
        let bytes = build_status_hyperlink_envelope(&[link]).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        let after_first_cell = output.split('A').nth(1).unwrap_or_default();
        assert!(
            after_first_cell.starts_with("\x1b[0m"),
            "second cell starts with SGR reset to drop the prior cell's modifiers: {output:?}",
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

    // ── tmux_passthrough ──

    #[test]
    fn tmux_passthrough_doubles_inner_escape_and_wraps_in_dcs() {
        let inner = b"\x1b]8;;https://x\x07X\x1b]8;;\x07";
        let wrapped = tmux_passthrough(inner);
        let expected = b"\x1bPtmux;\x1b\x1b]8;;https://x\x07X\x1b\x1b]8;;\x07\x1b\\";
        assert_eq!(&wrapped, expected);
    }

    // ── ratatui_color_to_crossterm ──

    #[test]
    fn ratatui_color_to_crossterm_round_trips_every_palette_variant() {
        use ratatui::style::Color as RC;
        let cases = [
            (RC::Reset, CtColor::Reset),
            (RC::Black, CtColor::Black),
            (RC::Red, CtColor::DarkRed),
            (RC::Green, CtColor::DarkGreen),
            (RC::Yellow, CtColor::DarkYellow),
            (RC::Blue, CtColor::DarkBlue),
            (RC::Magenta, CtColor::DarkMagenta),
            (RC::Cyan, CtColor::DarkCyan),
            (RC::Gray, CtColor::Grey),
            (RC::DarkGray, CtColor::DarkGrey),
            (RC::LightRed, CtColor::Red),
            (RC::LightGreen, CtColor::Green),
            (RC::LightYellow, CtColor::Yellow),
            (RC::LightBlue, CtColor::Blue),
            (RC::LightMagenta, CtColor::Magenta),
            (RC::LightCyan, CtColor::Cyan),
            (RC::White, CtColor::White),
            (RC::Indexed(33), CtColor::AnsiValue(33)),
            (RC::Rgb(1, 2, 3), CtColor::Rgb { r: 1, g: 2, b: 3 }),
        ];
        for (input, expected) in cases {
            assert_eq!(
                ratatui_color_to_crossterm(input),
                expected,
                "color {input:?}"
            );
        }
    }
}
