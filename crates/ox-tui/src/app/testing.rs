//! Test-only [`App`] fixtures shared by the app, dispatch, event, and render tests.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use ox_core::client::{RateLimitWindow, RateLimits};
use ox_core::protocol::testing::{OpReceiver, op_channel, op_id};
use ox_core::protocol::{Event, Op, OpEnd};
use ox_core::session::store::listing::ArchiveState;
use ox_core::tool::view::{ApprovalBody, ApprovalPreview};

use super::operation::{Activity, InFlight};
use super::{App, AppHistory, Channels, Host};
use crate::app_event::AppEventSender;
use crate::clipboard::testing::{REMOTE, next_report};
use crate::clipboard::{Clipboard, CopyReport};
use crate::git_probes::GitProbes;
use crate::modal::approval::ApprovalModal;
use crate::modal::{Modal, ModalKey};
use crate::now;
use crate::session_info::LiveSessionInfo;
use crate::session_info::testing::unknown_model_session_info;
use crate::testing::buffer_rows;
use crate::theme::{Theme, ThemeOverrides};

/// How many unread ops [`test_app`]'s channel holds before `try_submit` reports it full.
pub(super) const OP_CHANNEL_CAPACITY: usize = 8;

/// Idle `App` plus the `ops` consumer and an `event_tx` kept alive so `events` stays open.
pub(super) fn test_app(title: Option<&str>) -> (App, OpReceiver, mpsc::UnboundedSender<Event>) {
    test_app_with(unknown_model_session_info(), title)
}

/// [`test_app`] started on `session_info`.
pub(super) fn test_app_with(
    session_info: LiveSessionInfo,
    title: Option<&str>,
) -> (App, OpReceiver, mpsc::UnboundedSender<Event>) {
    test_app_from(
        session_info,
        AppHistory {
            interactions: Vec::new(),
            title: title.map(ToOwned::to_owned),
            prior: ArchiveState::Active,
        },
    )
}

/// [`test_app_with`] opening on `history`.
pub(super) fn test_app_from(
    mut session_info: LiveSessionInfo,
    history: AppHistory,
) -> (App, OpReceiver, mpsc::UnboundedSender<Event>) {
    let (event_tx, events) = mpsc::unbounded_channel::<Event>();
    let (ops, op_rx) = op_channel(OP_CHANNEL_CAPACITY);
    let (app_events, background) = mpsc::unbounded_channel();
    let host = test_host(&app_events);
    session_info.app_events = app_events;
    let app = App::new(
        Theme::default(),
        ThemeOverrides::default(),
        session_info,
        host,
        Channels {
            events,
            ops,
            background,
        },
        history,
    );
    (app, op_rx, event_tx)
}

/// A [`Host`] on `main` that never spawns a process or touches the system clipboard, reporting its
/// copies to `app_events`.
fn test_host(app_events: &AppEventSender) -> Host {
    Host {
        git_branch: Some("main".to_owned()),
        // With no segments to serve, the probes never spawn `git` or `gh`.
        git_probes: GitProbes::new(PathBuf::from("/repo"), &[], app_events.clone(), now()),
        // A remote session outside tmux copies only through OSC 52, leaving the system clipboard
        // and tmux alone.
        clipboard: Clipboard::new(REMOTE, app_events.clone()),
        open_browser: |_| Ok(()),
    }
}

/// Applies the next result that background work reports, as the event loop does.
pub(super) async fn apply_next_app_event(app: &mut App) {
    let event = app.background.recv().await.expect("the App holds a sender");
    app.apply_app_event(event);
}

/// The report of the latest copy, as the event loop takes it.
pub(super) async fn copy_report(app: &mut App) -> CopyReport {
    next_report(&app.clipboard, &mut app.background).await
}

/// Puts `op` in flight, under an ID the app's channel never issues, with no other local effect of
/// [`App::dispatch_op`], such as the queue marks a compaction or session switch sets.
pub(super) fn start_op(app: &mut App, op: &Op) {
    app.in_flight = InFlight::new(op, op_id(u64::MAX));
}

/// The [`Event::OpEnded`] that ends the in-flight operation as `end`.
pub(super) fn op_ended(app: &App, end: OpEnd) -> Event {
    let id = app.in_flight.as_ref().expect("an op is in flight").id();
    Event::OpEnded { id, end }
}

/// Shows `result`, then ends the in-flight operation as `end`, the order the engine reports them.
pub(super) fn end_op_with(app: &mut App, result: Event, end: OpEnd) {
    let ended = op_ended(app, end);
    app.handle_engine_event(result);
    app.handle_engine_event(ended);
}

/// What the in-flight operation does, or `None` while idle.
pub(super) fn activity(app: &App) -> Option<&Activity> {
    app.in_flight.as_ref().map(InFlight::activity)
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
    fn height(&mut self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
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
    fn height(&mut self, _width: u16, _max_height: u16) -> u16 {
        1
    }

    fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect, _theme: &Theme) {}

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

/// Screen cell where `needle` first shows in `buffer`.
pub(super) fn cell_of(buffer: &Buffer, needle: &str) -> (u16, u16) {
    let (row, x) = (0..)
        .zip(buffer_rows(buffer))
        .find_map(|(row, line)| Some((row, line[..line.find(needle)?].cell_width())))
        .expect("needle shown");
    (x, row)
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
    crate::modal::testing::render_modal(&mut modal, 60, u16::MAX);
    modal.expire_guard();
    app.push_modal(Box::new(modal));
}
