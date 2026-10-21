//! Root TUI application.
//!
//! [`App`] owns every component, holds the cross-task channels, and runs the `tokio::select!`
//! loop multiplexing crossterm events, engine events, git probe results, clipboard reports,
//! background slash-command results, and a render tick.
//! The tick runs at up to 60 FPS while a redraw is pending and otherwise sleeps until the next
//! timed change.
//! A dirty flag coalesces redraws so each state change renders once.
//! Input, engine events, drawing, and the prompt queue live in the `dispatch`, `events`, `render`,
//! and `queue` submodules.

mod dispatch;
mod events;
mod queue;
mod render;
#[cfg(test)]
mod testing;

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{Stream, StreamExt as _};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use ox_core::permission::Mode;
use ox_core::protocol::{Event, Interaction, Op};
use ox_core::session::display::archive_notice;
use ox_core::session::entry::ArchiveState;

use self::queue::PromptQueue;
use crate::clipboard::{Clipboard, Environment};
use crate::components::chat::ChatView;
use crate::components::input::InputArea;
use crate::components::status::{Status, StatusBar, StatusBarInit, StatusHyperlink};
use crate::components::tasks::TaskPanel;
use crate::git_probes::GitProbes;
use crate::modal::{Modal, ModalStack};
use crate::now;
use crate::session_info::LiveSessionInfo;
use crate::slash::ChatOutput;
use crate::terminal::{Tui, TuiBackend};
use crate::theme::{Theme, ThemeOverrides};

/// Shortest gap between ticks, which paces animation frames and coalesces renders (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Window in which a second Ctrl+C confirms exit.
const EXIT_WINDOW: Duration = Duration::from_secs(1);

pub(crate) struct App {
    theme: Theme,
    /// The configured `[tui.theme.overrides]`, which every `/theme` preview and swap reapplies.
    theme_overrides: ThemeOverrides,
    /// Theme saved when a `/theme` picker opens. Restored if the modal cancels.
    preview_theme_snapshot: Option<Theme>,
    /// What the app is doing, which gates input and sets the status bar's run state. Change it
    /// through [`Self::set_status`], which shows it in both.
    status: Status,
    status_bar: StatusBar,
    git_probes: GitProbes,
    chat: ChatView,
    tasks: TaskPanel,
    input: InputArea,
    modals: ModalStack,
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
    open_browser: fn(&str) -> io::Result<()>,
    /// Status-bar links of the last frame, which a plain left click opens.
    status_links: Vec<StatusHyperlink>,
    clipboard: Clipboard,
    /// OSC 52 sequence the next frame writes to the terminal.
    pending_clipboard: Option<String>,
    copy_notice: Option<CopyNotice>,
    /// Background slash commands still running for the live session.
    slash_tasks: JoinSet<Result<ChatOutput, String>>,
    queue: PromptQueue,
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
    /// The session's state before this resume, so an archived one opens with a note.
    pub(crate) prior: ArchiveState,
}

impl App {
    /// Builds the app with `git_branch` as the status bar's branch until the first git probe.
    pub(crate) fn new(
        theme: Theme,
        theme_overrides: ThemeOverrides,
        session_info: LiveSessionInfo,
        git_branch: Option<String>,
        events: mpsc::UnboundedReceiver<Event>,
        ops: mpsc::Sender<Op>,
        history: AppHistory,
    ) -> Self {
        let mut chat = ChatView::new(&theme, session_info.config.show_thinking);
        chat.load_history(history.interactions);
        if history.prior == ArchiveState::Archived {
            chat.push_system_message(archive_notice(
                &session_info.session_id,
                ArchiveState::Active,
            ));
        }
        let mut status_bar = StatusBar::new(StatusBarInit {
            segments: session_info.config.status_line.clone(),
            model: session_info.config.model.short_display_name().to_owned(),
            effort: session_info.config.effort,
            cwd: session_info.display_cwd.clone(),
            git_branch,
        });
        status_bar.set_title(history.title);
        let git_probes = GitProbes::new(
            session_info.cwd.clone(),
            &session_info.config.status_line,
            now(),
        );
        let requested_mode = session_info.config.permission_mode;
        let mode_cycle = match requested_mode {
            Mode::Yolo => &[Mode::Manual, Mode::Plan, Mode::Yolo][..],
            Mode::Manual | Mode::Plan => &[Mode::Manual, Mode::Plan][..],
        };
        Self {
            theme,
            theme_overrides,
            preview_theme_snapshot: None,
            status: Status::Idle,
            status_bar,
            git_probes,
            chat,
            tasks: TaskPanel::default(),
            input: InputArea::new(),
            modals: ModalStack::default(),
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
            slash_tasks: JoinSet::new(),
            queue: PromptQueue::default(),
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

    /// Moves to `status` and shows it in the status bar.
    fn set_status(&mut self, status: Status) {
        self.status_bar.set_status(status.clone());
        self.status = status;
    }

    /// Runs until the user quits or the engine's event channel closes.
    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        self.run_with_events(terminal, EventStream::new()).await
    }

    async fn run_with_events<B, S>(
        &mut self,
        terminal: &mut Terminal<B>,
        mut crossterm_events: S,
    ) -> Result<()>
    where
        B: TuiBackend,
        S: Stream<Item = io::Result<CrosstermEvent>> + Unpin,
    {
        self.render(terminal)?;
        let mut last_tick = now();

        loop {
            let next_tick = self.next_tick(last_tick);
            // Six sources can wake the loop. The tick arm is the only renderer, so handlers only
            // mark `dirty` and render work stays paced by the frame clock.
            tokio::select! {
                event = crossterm_events.next() => {
                    match event {
                        Some(Ok(event)) => self.handle_crossterm_event(&event),
                        Some(Err(e)) => return Err(e).context("failed to read terminal input"),
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
                Some(finished) = self.slash_tasks.join_next() => self.apply_slash_task(finished),
                () = sleep_until(next_tick) => {
                    let now = now();
                    last_tick = now;
                    self.git_probes.poll(now);
                    self.dirty |= self.status_bar.tick(now);
                    self.dirty |= self.expire_armed_exit(now);
                    self.dirty |= self.chat.tick_selection(now);
                    self.dirty |= self.expire_copy_notice(now);
                    self.dirty |= self.modals.tick(now);
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

    /// When the loop next ticks: one frame after `last_tick` while a redraw is pending, and
    /// otherwise at the earliest deadline a component reports, never sooner than that frame.
    /// `None` when only an event can change what the app shows.
    fn next_tick(&self, last_tick: Instant) -> Option<Instant> {
        let frame = last_tick + TICK_INTERVAL;
        if self.dirty {
            return Some(frame);
        }
        let now = now();
        let exit_window = match self.status {
            Status::ExitArmed { until } => Some(until),
            _ => None,
        };
        [
            self.git_probes.next_poll(),
            self.status_bar.next_tick(now),
            exit_window,
            self.chat.next_selection_tick(now),
            self.copy_notice.as_ref().map(|notice| notice.until),
            self.modals.next_tick(now),
        ]
        .into_iter()
        .flatten()
        .min()
        .map(|due| due.max(frame))
    }
}

/// Sleeps until `deadline`, or forever without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

    use ox_core::config::StatusLineSegment;

    use super::testing::{
        long_chat_block, render_app, rendered_text, test_app, unknown_model_session_info,
    };
    use super::*;
    use crate::components::status::SPINNER_FRAME_INTERVAL;
    use crate::components::status::testing::status_bar_init;
    use crate::git_probes::GitUpdate;
    use crate::glyphs::SPINNER_FRAMES;
    use crate::modal::ModalAction;
    use crate::modal::testing::ScriptedModal;
    use crate::terminal::testing::capture_terminal;
    use crate::testing::{buffer_rows, key_event, mouse_event, mouse_event_at};

    mod new {
        use super::*;

        #[test]
        fn plumbs_resumed_title_and_branch_into_status_bar() {
            let (app, _rx, _event_tx) = test_app(Some("Resumed title"));

            assert_eq!(app.status_bar.title(), Some("Resumed title"));
            assert_eq!(app.status_bar.git_branch(), Some("main"));
            assert_eq!(app.status, Status::Idle);
            assert_eq!(app.chat.entry_count(), 0);
            assert!(!app.should_quit);
            assert!(app.dirty, "first frame must render");
        }

        #[test]
        fn without_title_leaves_slot_unset() {
            let (app, _rx, _event_tx) = test_app(None);

            assert!(app.status_bar.title().is_none());
        }

        #[test]
        fn whitespace_title_is_filtered_by_status_bar() {
            // The status bar drops whitespace-only titles, so a blank resumed title leaves no slot.
            let (app, _rx, _event_tx) = test_app(Some("   \n "));

            assert!(app.status_bar.title().is_none());
        }

        #[test]
        fn after_resume_unarchived_the_session_opens_with_a_notice() {
            let (_event_tx, events) = mpsc::unbounded_channel::<Event>();
            let (ops, _op_rx) = mpsc::channel::<Op>(1);

            let mut app = App::new(
                Theme::default(),
                ThemeOverrides::default(),
                unknown_model_session_info(),
                None,
                events,
                ops,
                AppHistory {
                    interactions: vec![Interaction::UserText("earlier".to_owned())],
                    title: None,
                    prior: ArchiveState::Archived,
                },
            );

            assert_eq!(app.chat.entry_count(), 2);
            let text = rendered_text(&mut app, 80, 12);
            assert!(text.contains("Unarchived session test-ses"), "{text}");
        }
    }

    mod push_system_message {
        use super::*;

        #[test]
        fn replaces_welcome_with_notice() {
            let (mut app, _rx, _event_tx) = test_app(None);

            app.push_system_message("Model not supported.".to_owned());

            assert!(rendered_text(&mut app, 80, 24).contains("Model not supported."));
            assert!(!app.chat.is_empty());
        }
    }

    mod run_with_events {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn expires_armed_exit_on_tick_before_channel_close() {
            let (mut app, _rx, event_tx) = test_app(None);
            let until = now()
                .checked_sub(EXIT_WINDOW)
                .expect("monotonic clock has run long enough for a test deadline");
            app.set_status(Status::ExitArmed { until });
            app.dirty = false;
            let mut terminal = capture_terminal(60, 8);
            let closer = tokio::spawn(async move {
                tokio::time::sleep(TICK_INTERVAL * 2).await;
                drop(event_tx);
            });

            app.run_with_events(
                &mut terminal,
                futures::stream::pending::<io::Result<CrosstermEvent>>(),
            )
            .await
            .unwrap();
            closer.await.unwrap();

            assert_eq!(app.status, Status::Idle);
            assert!(app.should_quit);
        }

        #[tokio::test(start_paused = true)]
        async fn sleeps_while_idle() {
            let (mut app, _rx, event_tx) = test_app(None);
            app.git_probes = GitProbes::new(PathBuf::from("/repo"), &[], now());
            let modal = ScriptedModal::new(ModalAction::Cancelled);
            let ticks = Arc::clone(&modal.tick_count);
            app.modals.push(Box::new(modal));
            let mut terminal = capture_terminal(60, 8);
            let closer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                drop(event_tx);
            });

            app.run_with_events(
                &mut terminal,
                futures::stream::pending::<io::Result<CrosstermEvent>>(),
            )
            .await
            .unwrap();
            closer.await.unwrap();

            assert_eq!(
                ticks.load(Ordering::Relaxed),
                1,
                "only the startup redraw ticks"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn repaints_when_the_spinner_frame_advances() {
            let (mut app, _rx, event_tx) = test_app(None);
            app.set_status(Status::Streaming);
            app.dirty = false;
            let mut terminal = capture_terminal(60, 8);
            let driver = tokio::spawn(async move {
                tokio::time::sleep(SPINNER_FRAME_INTERVAL).await;
                drop(event_tx);
            });

            app.run_with_events(
                &mut terminal,
                futures::stream::pending::<io::Result<CrosstermEvent>>(),
            )
            .await
            .unwrap();
            driver.await.unwrap();

            let status = &buffer_rows(terminal.backend().buffer())[0];
            assert_eq!(
                status.trim_end(),
                format!(
                    "  test-model (high) │ {} Streaming · Esc to interrupt",
                    SPINNER_FRAMES[1],
                ),
            );
        }

        #[tokio::test(start_paused = true)]
        async fn pumps_crossterm_and_engine_events_through_select() {
            let (mut app, _rx, event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);
            let key = Ok(key_event(KeyCode::Char('x'), KeyModifiers::NONE));
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
            assert_eq!(app.status, Status::Streaming, "engine StreamToken arrived");
        }

        #[tokio::test(start_paused = true)]
        async fn applies_git_probe_results() {
            let (mut app, _rx, event_tx) = test_app(None);
            app.git_probes
                .inject(GitUpdate::Branch(Some("feat/x".to_owned())));
            let mut terminal = capture_terminal(60, 8);
            let closer = tokio::spawn(async move {
                tokio::time::sleep(TICK_INTERVAL * 2).await;
                drop(event_tx);
            });

            app.run_with_events(
                &mut terminal,
                futures::stream::pending::<io::Result<CrosstermEvent>>(),
            )
            .await
            .unwrap();
            closer.await.unwrap();

            assert_eq!(app.status_bar.git_branch(), Some("feat/x"));
        }

        #[tokio::test]
        async fn returns_terminal_read_errors() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);
            let stream = futures::stream::iter(vec![Err(io::Error::other("tty gone"))]);

            let err = app
                .run_with_events(&mut terminal, stream)
                .await
                .unwrap_err();

            assert_eq!(
                format!("{err:#}"),
                "failed to read terminal input: tty gone"
            );
        }

        #[tokio::test]
        async fn quits_when_the_terminal_stream_ends() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let mut terminal = capture_terminal(60, 8);

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

    mod next_tick {
        use super::*;

        /// An idle app with no probes, no clock segment, and nothing to redraw.
        fn settled_app() -> App {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.git_probes = GitProbes::new(PathBuf::from("/repo"), &[], now());
            app.status_bar = StatusBar::new(status_bar_init(vec![StatusLineSegment::RunState]));
            app.dirty = false;
            app
        }

        #[test]
        fn sleeps_until_an_event_when_nothing_is_timed() {
            let app = settled_app();

            assert_eq!(app.next_tick(now()), None);
        }

        #[test]
        fn ticks_one_frame_on_while_a_redraw_is_pending() {
            let last_tick = now();
            let mut app = settled_app();
            app.dirty = true;

            assert_eq!(app.next_tick(last_tick), Some(last_tick + TICK_INTERVAL));
        }

        #[test]
        fn wakes_for_the_next_git_probe() {
            let last_tick = now();
            let mut app = settled_app();
            app.git_probes = GitProbes::new(
                PathBuf::from("/repo"),
                &[StatusLineSegment::GitBranch],
                last_tick,
            );

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_secs(5))
            );
        }

        #[tokio::test(start_paused = true)]
        async fn wakes_for_the_next_spinner_frame_while_busy() {
            let last_tick = now();
            let mut app = settled_app();
            app.set_status(Status::Streaming);

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + SPINNER_FRAME_INTERVAL)
            );
        }

        #[test]
        fn wakes_when_the_exit_window_closes() {
            let last_tick = now();
            let mut app = settled_app();
            app.set_status(Status::ExitArmed {
                until: last_tick + Duration::from_secs(1),
            });

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_secs(1))
            );
        }

        #[tokio::test(start_paused = true)]
        async fn ticks_every_frame_while_a_drag_holds_above_the_chat() {
            let last_tick = now();
            let mut app = settled_app();
            app.chat.push_system_message(long_chat_block());
            _ = render_app(&mut app, 60, 10);
            app.handle_crossterm_event(&mouse_event(MouseEventKind::ScrollUp));
            _ = render_app(&mut app, 60, 10);
            app.handle_crossterm_event(&mouse_event_at(
                MouseEventKind::Down(MouseButton::Left),
                1,
                3,
            ));
            app.handle_crossterm_event(&mouse_event_at(
                MouseEventKind::Drag(MouseButton::Left),
                1,
                0,
            ));
            app.dirty = false;

            assert_eq!(app.next_tick(last_tick), Some(last_tick + TICK_INTERVAL));
        }

        #[test]
        fn wakes_when_the_copy_notice_expires() {
            let last_tick = now();
            let mut app = settled_app();
            app.copy_notice = Some(CopyNotice {
                text: "Copied".to_owned(),
                failed: false,
                until: last_tick + Duration::from_secs(3),
            });

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_secs(3))
            );
        }

        #[test]
        fn wakes_for_the_top_modal() {
            let last_tick = now();
            let mut app = settled_app();
            let mut modal = ScriptedModal::new(ModalAction::Cancelled);
            modal.next_tick = Some(last_tick + Duration::from_secs(2));
            app.modals.push(Box::new(modal));

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_secs(2))
            );
        }

        #[test]
        fn wakes_for_the_earliest_deadline_but_no_sooner_than_a_frame() {
            let last_tick = now();
            let mut app = settled_app();
            app.set_status(Status::ExitArmed {
                until: last_tick + Duration::from_secs(1),
            });
            app.copy_notice = Some(CopyNotice {
                text: "Copied".to_owned(),
                failed: false,
                until: last_tick + Duration::from_millis(500),
            });

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_millis(500))
            );

            app.set_status(Status::ExitArmed { until: last_tick });

            assert_eq!(app.next_tick(last_tick), Some(last_tick + TICK_INTERVAL));
        }
    }
}
