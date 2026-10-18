//! Test-only [`App`] fixtures shared by the app, dispatch, event, and render tests.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::text::{Line, Span};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use super::{App, AppHistory};
use crate::client::{RateLimitWindow, RateLimits};
use crate::model::ModelSpec;
use crate::protocol::{Event, Op};
use crate::slash::{self, LiveSessionInfo};
use crate::tool::{ApprovalBody, ApprovalPreview};
use crate::tui::clipboard::{self, Clipboard, Environment};
use crate::tui::theme::Theme;

/// Idle `App` plus the `ops` consumer and an `event_tx` kept alive so `events` stays open.
pub(super) fn test_app(
    title: Option<&str>,
) -> (App, mpsc::Receiver<Op>, mpsc::UnboundedSender<Event>) {
    test_app_with(test_session_info(), title)
}

/// [`test_app`] started on `session_info`.
pub(super) fn test_app_with(
    session_info: LiveSessionInfo,
    title: Option<&str>,
) -> (App, mpsc::Receiver<Op>, mpsc::UnboundedSender<Event>) {
    let (event_tx, events) = mpsc::unbounded_channel::<Event>();
    let (ops, op_rx) = mpsc::channel::<Op>(8);
    let mut app = App::new(
        &Theme::default(),
        session_info,
        Some("main".to_owned()),
        false,
        events,
        ops,
        AppHistory {
            interactions: Vec::new(),
            title: title.map(ToOwned::to_owned),
            unarchived: false,
        },
    );
    app.open_browser = |_| Ok(());
    // A remote session outside tmux copies only through OSC 52, leaving the system clipboard and
    // tmux alone.
    app.clipboard = Clipboard::new(Environment {
        local: false,
        tmux: false,
        screen: false,
        terminal: clipboard::Terminal::Other,
    });
    (app, op_rx, event_tx)
}

/// The slash fixture on `test-model`, an unknown id whose display name falls back to the literal
/// id, keeping insta snapshots stable.
pub(super) fn test_session_info() -> LiveSessionInfo {
    let mut info = slash::test_session_info();
    info.config.model = ModelSpec::resolve("test-model".to_owned());
    info
}

/// A five-hour window 42% spent that resets in 2100.
pub(super) fn five_hour_limit() -> RateLimits {
    RateLimits {
        five_hour: Some(RateLimitWindow {
            utilization: 0.42,
            resets_at: 4_102_444_800,
        }),
        seven_day: None,
    }
}

pub(super) fn key_event(code: KeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
    CrosstermEvent::Key(KeyEvent::new(code, modifiers))
}

pub(super) fn mouse_event(kind: MouseEventKind) -> CrosstermEvent {
    mouse_event_at(kind, 0, 0)
}

pub(super) fn mouse_event_at(kind: MouseEventKind, column: u16, row: u16) -> CrosstermEvent {
    CrosstermEvent::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// Minimal modal for layout tests: paints `title` on its only row, ignores keys.
pub(super) struct FakeModal {
    title: String,
}

impl FakeModal {
    pub(super) fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
        }
    }
}

impl crate::tui::modal::Modal for FakeModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&self, frame: &mut ratatui::Frame<'_>, area: Rect, theme: &Theme) {
        use ratatui::widgets::Paragraph;
        let line = Line::from(Span::styled(self.title.clone(), theme.text()));
        frame.render_widget(Paragraph::new(line).style(theme.surface()), area);
    }

    fn handle_key(&mut self, _event: &KeyEvent) -> crate::tui::modal::ModalKey {
        crate::tui::modal::ModalKey::Consumed
    }
}

/// Modal that records the code of every key it receives.
pub(super) struct KeyLogModal(pub(super) Arc<Mutex<Vec<KeyCode>>>);

impl crate::tui::modal::Modal for KeyLogModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&self, _frame: &mut ratatui::Frame<'_>, _area: Rect, _theme: &Theme) {}

    fn handle_key(&mut self, event: &KeyEvent) -> crate::tui::modal::ModalKey {
        self.0.lock().unwrap().push(event.code);
        crate::tui::modal::ModalKey::Consumed
    }
}

#[derive(Clone)]
pub(super) struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn crossterm_test_terminal(
    width: u16,
    height: u16,
) -> (
    Terminal<CrosstermBackend<SharedWriter>>,
    Arc<Mutex<Vec<u8>>>,
) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let backend = CrosstermBackend::new(SharedWriter(buf.clone()));
    let opts = TerminalOptions {
        viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
    };
    (Terminal::with_options(backend, opts).unwrap(), buf)
}

pub(super) fn render_app(app: &mut App, width: u16, height: u16) -> TestBackend {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw_frame(frame)).unwrap();
    terminal.backend().clone()
}

/// Renders the app and returns the buffer as a newline-joined string. Use when substring-asserting
/// on the rendered UI is more readable than a full `insta::assert_snapshot!`.
pub(super) fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
    let backend = render_app(app, width, height);
    let buffer = backend.buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn long_chat_block() -> String {
    use std::fmt::Write as _;

    let mut body = String::new();
    for i in 0..30 {
        _ = writeln!(body, "line {i:02} of a long chat block");
    }
    body
}

/// Pushes a Bash approval past its type-ahead guard, so it answers keys at once.
pub(super) fn push_answerable_approval(app: &mut App, command: &str) {
    let mut modal = crate::tui::modal::approval::ApprovalModal::new(
        "call-1".to_owned(),
        ApprovalPreview {
            title: "Bash".to_owned(),
            body: ApprovalBody::Command(command.to_owned()),
        },
        None,
        None,
    );
    modal.expire_guard();
    app.push_modal(Box::new(modal));
}
