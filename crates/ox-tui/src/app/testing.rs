//! Test-only [`App`] fixtures shared by the app, dispatch, event, and render tests.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use ox_core::client::{RateLimitWindow, RateLimits};
use ox_core::model::ModelSpec;
use ox_core::protocol::{Event, Op};
use ox_core::tool::view::{ApprovalBody, ApprovalPreview};

use super::{App, AppHistory};
use crate::clipboard::{self, Clipboard, Environment};
use crate::modal::approval::ApprovalModal;
use crate::modal::{Modal, ModalKey};
use crate::slash::{self, LiveSessionInfo};
use crate::testing::buffer_rows;
use crate::theme::{Theme, ThemeOverrides};

/// How many unread ops [`test_app`]'s channel holds before `try_send` reports it full.
pub(super) const OP_CHANNEL_CAPACITY: usize = 8;

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
    let (ops, op_rx) = mpsc::channel::<Op>(OP_CHANNEL_CAPACITY);
    let mut app = App::new(
        Theme::default(),
        ThemeOverrides::default(),
        session_info,
        Some("main".to_owned()),
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
    let mut info = slash::testing::test_session_info();
    info.config.model = ModelSpec::resolve("test-model");
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

impl Modal for FakeModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let line = Line::from(Span::styled(self.title.clone(), theme.text()));
        frame.render_widget(Paragraph::new(line).style(theme.surface()), area);
    }

    fn handle_key(&mut self, _event: &KeyEvent) -> ModalKey {
        ModalKey::Consumed
    }
}

/// Modal that records the code of every key it receives.
pub(super) struct KeyLogModal(pub(super) Arc<Mutex<Vec<KeyCode>>>);

impl Modal for KeyLogModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {}

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        self.0.lock().unwrap().push(event.code);
        ModalKey::Consumed
    }
}

pub(super) fn render_app(app: &mut App, width: u16, height: u16) -> TestBackend {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw_frame(frame)).unwrap();
    terminal.backend().clone()
}

/// Renders the app and returns its rows joined by newlines, for asserting on part of the frame.
pub(super) fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
    buffer_rows(render_app(app, width, height).buffer()).join("\n")
}

pub(super) fn long_chat_block() -> String {
    let mut body = String::new();
    for i in 0..30 {
        _ = writeln!(body, "line {i:02} of a long chat block");
    }
    body
}

/// Pushes a Bash approval past its type-ahead guard, so it answers keys at once.
pub(super) fn push_answerable_approval(app: &mut App, command: &str) {
    let mut modal = ApprovalModal::new(
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
