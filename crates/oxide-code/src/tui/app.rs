//! Root TUI application.
//!
//! [`App`] owns every component, holds the cross-task channels, and runs the `tokio::select!`
//! loop multiplexing crossterm events, engine events, git probe results, clipboard reports, and a
//! 60 FPS render tick. A dirty flag coalesces redraws so each state change renders once. Terminal
//! input, engine events, and drawing live in the `dispatch`, `events`, and `render` submodules.

mod dispatch;
mod events;
mod render;
#[cfg(test)]
mod testing;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;

use super::clipboard::{Clipboard, Environment};
use super::components::chat::ChatView;
use super::components::input::InputArea;
use super::components::status::{StatusBar, StatusHyperlink};
use super::components::tasks::TaskPanel;
use super::git_probes::GitProbes;
use super::modal::{Modal, ModalStack};
use super::terminal::Tui;
use super::theme::Theme;
use crate::permission::Mode;
use crate::protocol::{Event, Interaction, Op};
use crate::session::display::archive_notice;
use crate::slash::LiveSessionInfo;

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Window in which a second Ctrl+C confirms exit.
const EXIT_WINDOW: Duration = Duration::from_secs(1);

/// How long the outcome of a copy stays over the chat.
const COPY_NOTICE_DURATION: Duration = Duration::from_secs(3);

pub(crate) struct App {
    theme: Theme,
    status_bar: StatusBar,
    git_probes: GitProbes,
    chat: ChatView,
    tasks: TaskPanel,
    input: InputArea,
    session_info: LiveSessionInfo,
    /// The modes Shift+Tab steps through. `yolo` joins only when the session starts in it, so a
    /// stray key press cannot leave every call unprompted.
    mode_cycle: &'static [Mode],
    /// The mode of the last switch sent to the engine, which Shift+Tab steps on from so a quick
    /// second press does not repeat a switch still awaiting its confirmation.
    requested_mode: Mode,
    events: mpsc::UnboundedReceiver<Event>,
    ops: mpsc::Sender<Op>,
    /// Opens a URL in the system browser. Tests replace it so they never launch one.
    open_browser: fn(&str) -> std::io::Result<()>,
    /// Status-bar links of the last frame, which a plain left click opens.
    status_links: Vec<StatusHyperlink>,
    clipboard: Clipboard,
    /// OSC 52 sequence the next frame writes to the terminal.
    pending_clipboard: Option<String>,
    copy_notice: Option<CopyNotice>,
    /// Prompt already painted for the in-flight turn, kept as typed. Replayed if auto-compaction
    /// clears chat, and handed back if auto-compaction fails before the prompt is sent.
    active_prompt: Option<String>,
    /// Whether an automatic compaction runs ahead of the active prompt, so a failure means the
    /// engine never sent it.
    auto_compacting: bool,
    /// FIFO of prompts submitted mid-turn, kept as typed so Esc restores a `//` escape. Each stays
    /// with the TUI until the engine confirms it with `PromptDrained` or it starts its own turn.
    pending_prompts: VecDeque<String>,
    /// How many leading `pending_prompts` the in-flight turn already holds.
    forwarded_prompts: usize,
    modals: ModalStack,
    /// Theme saved when a `/theme` picker opens. Restored if the modal cancels.
    preview_theme_snapshot: Option<Theme>,
    should_quit: bool,
    dirty: bool,
}

/// Outcome of the last copy, shown over the chat until `until`.
struct CopyNotice {
    text: String,
    failed: bool,
    until: Instant,
}

pub(crate) struct AppHistory {
    pub(crate) interactions: Vec<Interaction>,
    pub(crate) title: Option<String>,
    /// Resuming the session unarchived it, so the chat opens with a note saying so.
    pub(crate) unarchived: bool,
}

impl App {
    /// Builds the app with `git_branch` as the status bar's branch until the first git probe.
    pub(crate) fn new(
        theme: &Theme,
        session_info: LiveSessionInfo,
        git_branch: Option<String>,
        show_thinking: bool,
        events: mpsc::UnboundedReceiver<Event>,
        ops: mpsc::Sender<Op>,
        history: AppHistory,
    ) -> Self {
        let mut chat = ChatView::new(theme, show_thinking);
        chat.load_history(history.interactions);
        if history.unarchived {
            chat.push_system_message(archive_notice(&session_info.session_id, false));
        }
        let mut status_bar = StatusBar::new(
            theme,
            session_info.config.status_line.clone(),
            session_info.config.model.short_display_name().to_owned(),
            session_info.config.effort,
            session_info.cwd.clone(),
            git_branch,
        );
        status_bar.set_title(history.title);
        let git_probes = GitProbes::new(
            session_info.working_dir.clone(),
            &session_info.config.status_line,
            Instant::now(),
        );
        let requested_mode = session_info.config.permission_mode;
        let mode_cycle = match requested_mode {
            Mode::Yolo => &[Mode::Manual, Mode::Plan, Mode::Yolo][..],
            Mode::Manual | Mode::Plan => &[Mode::Manual, Mode::Plan][..],
        };
        Self {
            theme: theme.clone(),
            status_bar,
            git_probes,
            chat,
            tasks: TaskPanel::default(),
            input: InputArea::new(theme),
            session_info,
            mode_cycle,
            requested_mode,
            events,
            ops,
            open_browser: |url| open::that_detached(url),
            status_links: Vec::new(),
            clipboard: Clipboard::new(Environment::detect()),
            pending_clipboard: None,
            copy_notice: None,
            active_prompt: None,
            auto_compacting: false,
            pending_prompts: VecDeque::new(),
            forwarded_prompts: 0,
            modals: ModalStack::new(),
            preview_theme_snapshot: None,
            should_quit: false,
            dirty: true,
        }
    }

    /// Appends a system message ahead of the first frame, for warnings raised at startup.
    pub(crate) fn push_system_message(&mut self, body: String) {
        self.chat.push_system_message(body);
    }

    /// Opens a modal ahead of the first frame, for overlays raised at startup.
    pub(crate) fn push_modal(&mut self, modal: Box<dyn Modal>) {
        self.modals.push(modal);
        self.dirty = true;
    }

    /// Runs until the user quits or the engine's event channel closes.
    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        self.run_with_events(terminal, EventStream::new()).await
    }

    async fn run_with_events<W, S>(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::prelude::CrosstermBackend<W>>,
        mut crossterm_events: S,
    ) -> Result<()>
    where
        W: std::io::Write,
        S: Stream<Item = std::io::Result<CrosstermEvent>> + Unpin,
    {
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        self.render(terminal)?;

        loop {
            // Five sources can wake the loop. The tick arm is the only renderer, so handlers only
            // mark `dirty` and render work stays paced by the 60 FPS clock.
            tokio::select! {
                event = crossterm_events.next() => {
                    match event {
                        Some(Ok(event)) => self.handle_crossterm_event(&event),
                        Some(Err(e)) => return Err(e).context("cannot read terminal input"),
                        None => self.should_quit = true,
                    }
                }
                event = self.events.recv() => {
                    match event {
                        Some(event) => self.handle_engine_event(event),
                        // The engine stopped, and `recv` would keep returning `None`, so quit.
                        None => self.should_quit = true,
                    }
                }
                Some(update) = self.git_probes.recv() => self.apply_git_update(update),
                Some(report) = self.clipboard.recv() => self.apply_copy_report(report),
                _ = tick.tick() => {
                    self.git_probes.poll(Instant::now());
                    if self.status_bar.tick() {
                        self.dirty = true;
                    }
                    if self.expire_armed_exit() {
                        self.dirty = true;
                    }
                    let now = Instant::now();
                    if self.chat.tick_selection(now) {
                        self.dirty = true;
                    }
                    if self.expire_copy_notice(now) {
                        self.dirty = true;
                    }
                    if self.dirty {
                        self.render(terminal)?;
                        self.dirty = false;
                    }
                }
            }

            if self.should_quit {
                break;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::testing::{crossterm_test_terminal, rendered_text, test_app, test_session_info};
    use super::*;
    use crate::tui::components::status::{Status, TICKS_PER_FRAME};
    use crate::tui::git_probes::GitUpdate;
    use crate::tui::glyphs::SPINNER_FRAMES;

    // ── App::new ──

    #[test]
    fn new_plumbs_resumed_title_and_branch_into_status_bar() {
        let (app, _rx, _event_tx) = test_app(Some("Resumed title"));
        assert_eq!(app.status_bar.title(), Some("Resumed title"));
        assert_eq!(app.status_bar.git_branch(), Some("main"));
        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert_eq!(app.chat.entry_count(), 0);
        assert!(app.input.is_enabled());
        assert!(!app.should_quit);
        assert!(app.dirty, "first frame must render");
    }

    #[test]
    fn new_without_title_leaves_slot_unset() {
        let (app, _rx, _event_tx) = test_app(None);
        assert!(app.status_bar.title().is_none());
    }

    #[test]
    fn new_whitespace_title_is_filtered_by_status_bar() {
        // Status bar filters whitespace-only titles so a resumed blank title doesn't leave a slot.
        let (app, _rx, _event_tx) = test_app(Some("   \n "));
        assert!(app.status_bar.title().is_none());
    }

    #[test]
    fn new_after_resume_unarchived_the_session_opens_with_a_notice() {
        let (_event_tx, events) = mpsc::unbounded_channel::<Event>();
        let (ops, _op_rx) = mpsc::channel::<Op>(1);
        let mut app = App::new(
            &Theme::default(),
            test_session_info(),
            None,
            false,
            events,
            ops,
            AppHistory {
                interactions: vec![Interaction::UserText("earlier".to_owned())],
                title: None,
                unarchived: true,
            },
        );

        assert_eq!(app.chat.entry_count(), 2);
        let text = rendered_text(&mut app, 80, 12);
        assert!(text.contains("Unarchived session test-ses"), "{text}");
    }

    // ── push_system_message ──

    #[test]
    fn push_system_message_replaces_welcome_with_notice() {
        let (mut app, _rx, _event_tx) = test_app(None);
        app.push_system_message("Model not supported.".to_owned());
        assert!(rendered_text(&mut app, 80, 24).contains("Model not supported."));
        assert!(!app.chat.is_empty());
    }

    // ── run_with_events ──

    #[tokio::test]
    async fn run_with_events_expires_armed_exit_on_tick_before_channel_close() {
        let (mut app, _rx, event_tx) = test_app(None);
        let until = Instant::now()
            .checked_sub(EXIT_WINDOW)
            .expect("monotonic clock has run long enough for a test deadline");
        app.status_bar.set_status(Status::ExitArmed { until });
        app.dirty = false;

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        closer.await.unwrap();

        assert_eq!(app.status_bar.status(), &Status::Idle);
        assert!(app.should_quit);
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_events_repaints_when_the_spinner_frame_advances() {
        // Sleep past the spinner interval so `tick()` reports a visible frame change.
        let (mut app, _rx, event_tx) = test_app(None);
        app.status_bar.set_status(Status::Streaming);
        app.dirty = false;

        let (mut terminal, buf) = crossterm_test_terminal(60, 8);
        let frame = TICK_INTERVAL * u32::try_from(TICKS_PER_FRAME).unwrap();
        let driver = tokio::spawn(async move {
            tokio::time::sleep(frame).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        driver.await.unwrap();

        let painted = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        assert!(painted.contains(SPINNER_FRAMES[1]), "{painted:?}");
    }

    #[tokio::test]
    async fn run_with_events_pumps_crossterm_and_engine_events_through_select() {
        // Pins both arms of `select!`: a key reaches input, a stream token disables it,
        // and closing the engine's event channel ends the loop.
        let (mut app, _rx, event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);

        let key = Ok(CrosstermEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        let stream = futures::stream::iter(vec![key]).chain(futures::stream::pending());

        let driver = tokio::spawn(async move {
            event_tx.send(Event::StreamToken("hi".to_owned())).unwrap();
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(&mut terminal, stream).await.unwrap();
        driver.await.unwrap();

        assert!(app.should_quit);
        assert_eq!(app.input.lines(), vec!["x"], "crossterm key reached input");
        assert!(
            !app.input.is_enabled(),
            "engine StreamToken disabled the input",
        );
    }

    #[tokio::test]
    async fn run_with_events_applies_git_probe_results() {
        let (mut app, _rx, event_tx) = test_app(None);
        app.git_probes
            .inject(GitUpdate::Branch(Some("feat/x".to_owned())));

        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let closer = tokio::spawn(async move {
            tokio::time::sleep(TICK_INTERVAL * 2).await;
            drop(event_tx);
        });

        app.run_with_events(
            &mut terminal,
            futures::stream::pending::<std::io::Result<CrosstermEvent>>(),
        )
        .await
        .unwrap();
        closer.await.unwrap();

        assert_eq!(app.status_bar.git_branch(), Some("feat/x"));
    }

    #[tokio::test]
    async fn run_with_events_returns_terminal_read_errors() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);
        let stream = futures::stream::iter(vec![Err(io::Error::other("tty gone"))]);

        let err = app
            .run_with_events(&mut terminal, stream)
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("tty gone"), "{err:#}");
    }

    #[tokio::test]
    async fn run_with_events_quits_when_the_terminal_stream_ends() {
        let (mut app, _rx, _event_tx) = test_app(None);
        let (mut terminal, _buf) = crossterm_test_terminal(60, 8);

        tokio::time::timeout(
            Duration::from_secs(5),
            app.run_with_events(&mut terminal, futures::stream::empty()),
        )
        .await
        .expect("an ended stream must stop the loop")
        .unwrap();

        assert!(app.should_quit);
    }
}
