//! Root TUI application and event loop.
//!
//! Input and background events update state and request a redraw. The frame clock coalesces redraws
//! at up to 60 FPS and sleeps until the next timed change when idle.

mod dispatch;
mod events;
mod operation;
mod queue;
mod render;
#[cfg(test)]
mod testing;

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{Stream, StreamExt as _};
use ratatui::Terminal;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use ox_core::config::StatusLineSegment;
use ox_core::permission::Mode;
use ox_core::protocol::{Event, Interaction, OpSender};
use ox_core::session::display::archive_notice;
use ox_core::session::store::listing::ArchiveState;

use self::operation::InFlight;
use self::queue::PromptQueue;
use crate::app_event::{AppEvent, AppEventSender};
use crate::clipboard::{Clipboard, Environment};
use crate::components::chat::ChatView;
use crate::components::input::InputArea;
use crate::components::status::{StatusBar, StatusHyperlink};
use crate::components::tasks::TaskPanel;
use crate::git_probes::GitProbes;
use crate::modal::{Modal, ModalStack};
use crate::now;
use crate::session_info::LiveSessionInfo;
use crate::slash::SlashOutcome;
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
    /// The idle-only operation the engine runs, until the engine ends it. While it runs, the App
    /// keeps other idle-only input local.
    in_flight: Option<InFlight>,
    /// Whether the last operation left a modal open that may send the next one, which defers the
    /// queue head until the user closes it.
    head_awaits_modal: bool,
    /// When the window for a second exit key closes, while one is open.
    exit_armed: Option<Instant>,
    status_bar: StatusBar,
    git_probes: GitProbes,
    chat: ChatView,
    tasks: TaskPanel,
    input: InputArea,
    modals: ModalStack,
    session_info: LiveSessionInfo,
    /// The mode of the last switch sent to the engine, which cycling advances from so a quick
    /// second press does not repeat a switch still awaiting its confirmation.
    requested_mode: Mode,
    events: mpsc::UnboundedReceiver<Event>,
    ops: OpSender,
    background: mpsc::UnboundedReceiver<AppEvent>,
    /// Opens a URL in the system browser.
    open_browser: fn(&str) -> io::Result<()>,
    /// Status-bar links of the last frame, which a plain left click opens.
    status_links: Vec<StatusHyperlink>,
    clipboard: Clipboard,
    /// OSC 52 sequence the next frame writes to the terminal.
    pending_clipboard: Option<String>,
    copy_notice: Option<CopyNotice>,
    /// Counts session switches, so a background slash command's output reaches only the chat it
    /// was typed in.
    session_epoch: u64,
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

/// What a background slash command resolved to, with the typed line a resolved prompt runs under.
pub(crate) struct SlashEnd {
    /// The [`App::session_epoch`] the command was typed in.
    epoch: u64,
    line: String,
    outcome: Result<SlashOutcome, String>,
}

/// What the App reads from and acts on outside the terminal: the git work tree, the clipboard, and
/// the browser. `run` detects them, and tests pass doubles that leave the host alone.
pub(crate) struct Host {
    /// The branch checked out at startup, shown until the first probe reports.
    pub(crate) git_branch: Option<String>,
    pub(crate) git_probes: GitProbes,
    pub(crate) clipboard: Clipboard,
    /// Opens a URL in the system browser.
    pub(crate) open_browser: fn(&str) -> io::Result<()>,
}

impl Host {
    /// The host of a session in `cwd`, probing the git state that `status_line` reads. Probes and
    /// copies report to `app_events`.
    pub(crate) fn detect(
        cwd: &Path,
        status_line: &[StatusLineSegment],
        git_branch: Option<String>,
        app_events: &AppEventSender,
    ) -> Self {
        Self {
            git_branch,
            git_probes: GitProbes::new(cwd.to_owned(), status_line, app_events.clone(), now()),
            clipboard: Clipboard::new(Environment::detect(), app_events.clone()),
            open_browser: |url| open::that_detached(url),
        }
    }
}

/// The channels the App runs on besides the terminal.
pub(crate) struct Channels {
    /// What the engine reports.
    pub(crate) events: mpsc::UnboundedReceiver<Event>,
    /// The engine's op queue, which the App never waits on, so an op that finds it full is dropped
    /// with a notice.
    pub(crate) ops: OpSender,
    /// What background work reports through the senders that the session info and the host hold.
    pub(crate) background: mpsc::UnboundedReceiver<AppEvent>,
}

pub(crate) struct AppHistory {
    pub(crate) interactions: Vec<Interaction>,
    pub(crate) title: Option<String>,
    /// The session's state before this resume, so an archived one opens with a note.
    pub(crate) prior: ArchiveState,
}

impl App {
    pub(crate) fn new(
        theme: Theme,
        theme_overrides: ThemeOverrides,
        mut session_info: LiveSessionInfo,
        host: Host,
        channels: Channels,
        history: AppHistory,
    ) -> Self {
        let Host {
            git_branch,
            git_probes,
            clipboard,
            open_browser,
        } = host;
        let Channels {
            events,
            ops,
            background,
        } = channels;
        let mut chat = ChatView::new(&theme, session_info.config.show_thinking);
        chat.load_history(history.interactions);
        if history.prior == ArchiveState::Archived {
            chat.push_system_message(archive_notice(
                &session_info.session_id,
                ArchiveState::Active,
            ));
        }
        session_info.set_title(history.title);
        let status_bar = StatusBar::new(session_info.config.status_line.clone(), git_branch);
        let requested_mode = session_info.config.permission_mode;
        Self {
            theme,
            theme_overrides,
            preview_theme_snapshot: None,
            in_flight: None,
            head_awaits_modal: false,
            exit_armed: None,
            status_bar,
            git_probes,
            chat,
            tasks: TaskPanel::default(),
            input: InputArea::new(),
            modals: ModalStack::default(),
            session_info,
            requested_mode,
            events,
            ops,
            background,
            open_browser,
            status_links: Vec::new(),
            clipboard,
            pending_clipboard: None,
            copy_notice: None,
            session_epoch: 0,
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
            // The tick arm is the only renderer, so handlers only mark `dirty` and render work
            // stays paced by the frame clock.
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
                Some(event) = self.background.recv() => self.apply_app_event(event),
                () = sleep_until(next_tick) => {
                    let now = now();
                    last_tick = now;
                    self.dirty |= self.tick(now);
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

    /// Advances every timed component to `now`. Returns whether anything shown changed.
    fn tick(&mut self, now: Instant) -> bool {
        self.git_probes.poll(now);
        let mut dirty = self.status_bar.tick(now);
        dirty |= self
            .session_info
            .expire_rate_limits(OffsetDateTime::now_utc().unix_timestamp());
        dirty |= self.expire_armed_exit(now);
        dirty |= self.chat.tick_selection(now);
        dirty |= self.expire_copy_notice(now);
        dirty |= self.modals.tick(now);
        dirty
    }

    /// When the loop next ticks: one frame after `last_tick` while a redraw is pending, and
    /// otherwise at the earliest deadline a component reports, never sooner than that frame. `None`
    /// when only an event can change what the app shows.
    fn next_tick(&self, last_tick: Instant) -> Option<Instant> {
        let frame = last_tick + TICK_INTERVAL;
        if self.dirty {
            return Some(frame);
        }
        let now = now();
        [
            self.git_probes.next_poll(),
            self.status_bar.next_tick(now),
            self.session_info
                .next_rate_limit_reset(now, OffsetDateTime::now_utc().unix_timestamp()),
            self.exit_armed,
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

    use ox_core::client::{RateLimitWindow, RateLimits};
    use ox_core::protocol::Op;

    use super::testing::{
        long_chat_block, render_app, rendered_text, start_op, test_app, test_app_from,
    };
    use super::*;
    use crate::components::status::SPINNER_FRAME_INTERVAL;
    use crate::git_probes::GitUpdate;
    use crate::glyphs::SPINNER_FRAMES;
    use crate::modal::testing::ScriptedModal;
    use crate::session_info::testing::unknown_model_session_info;
    use crate::terminal::testing::capture_terminal;
    use crate::testing::{buffer_rows, key_event, mouse_event, mouse_event_at};

    mod host {
        use super::*;

        mod detect {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn schedules_the_git_probes_the_status_line_reads() {
                let (app_events, _background) = mpsc::unbounded_channel();

                let host = Host::detect(
                    Path::new("/repo"),
                    &[StatusLineSegment::PullRequest],
                    None,
                    &app_events,
                );

                assert_eq!(host.git_probes.next_poll(), Some(now()));
            }
        }
    }

    mod new {
        use super::*;

        #[test]
        fn plumbs_resumed_title_and_branch() {
            let (app, _rx, _event_tx) = test_app(Some("Resumed title"));

            assert_eq!(app.session_info.title.as_deref(), Some("Resumed title"));
            assert_eq!(app.status_bar.git_branch(), Some("main"));
            assert!(app.in_flight.is_none());
            assert_eq!(app.chat.entry_count(), 0);
            assert!(!app.should_quit);
            assert!(app.dirty, "first frame must render");
        }

        #[test]
        fn without_title_leaves_slot_unset() {
            let (app, _rx, _event_tx) = test_app(None);

            assert!(app.session_info.title.is_none());
        }

        #[test]
        fn drops_a_blank_resumed_title() {
            let (app, _rx, _event_tx) = test_app(Some("   \n "));

            assert!(app.session_info.title.is_none());
        }

        #[test]
        fn after_resume_unarchived_the_session_opens_with_a_notice() {
            let history = AppHistory {
                interactions: vec![Interaction::UserText("earlier".to_owned())],
                title: None,
                prior: ArchiveState::Archived,
            };

            let (mut app, _rx, _event_tx) = test_app_from(unknown_model_session_info(), history);

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
            app.exit_armed = Some(until);
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

            assert_eq!(app.exit_armed, None);
            assert!(app.should_quit);
        }

        #[tokio::test(start_paused = true)]
        async fn sleeps_while_idle() {
            let (mut app, _rx, event_tx) = test_app(None);
            let modal = ScriptedModal::new();
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
            start_op(&mut app, &Op::prompt("running"));
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
                    "  main │ test-model (high) │ {} Streaming · Esc to interrupt",
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
            assert!(
                rendered_text(&mut app, 60, 8).contains("hi"),
                "engine StreamToken arrived"
            );
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

    mod tick {
        use super::*;

        #[test]
        fn drops_a_usage_limit_window_once_it_resets() {
            let (mut app, _rx, _event_tx) = test_app(None);
            let unix_now = OffsetDateTime::now_utc().unix_timestamp();
            let window = |resets_at| {
                Some(RateLimitWindow {
                    utilization: 0.97,
                    resets_at,
                })
            };
            app.session_info.rate_limits = Some(RateLimits {
                five_hour: window(unix_now - 1),
                seven_day: window(unix_now + 3600),
            });

            let dirty = app.tick(now());

            assert!(dirty, "the bar loses the reset window");
            assert_eq!(
                app.session_info.rate_limits,
                Some(RateLimits {
                    five_hour: None,
                    seven_day: window(unix_now + 3600),
                }),
            );
        }
    }

    mod next_tick {
        use super::*;

        /// An idle app with no clock segment and nothing to redraw.
        fn settled_app() -> App {
            let (mut app, _rx, _event_tx) = test_app(None);
            app.status_bar = StatusBar::new(vec![StatusLineSegment::RunState], None);
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
                app.session_info.app_events.clone(),
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
            start_op(&mut app, &Op::prompt("running"));
            _ = render_app(&mut app, 60, 8);

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + SPINNER_FRAME_INTERVAL)
            );
        }

        #[tokio::test(start_paused = true)]
        async fn wakes_when_a_usage_limit_window_resets() {
            let last_tick = now();
            let mut app = settled_app();
            app.session_info.rate_limits = Some(RateLimits {
                five_hour: Some(RateLimitWindow {
                    utilization: 0.5,
                    resets_at: OffsetDateTime::now_utc().unix_timestamp() + 60,
                }),
                seven_day: None,
            });

            let due = app.next_tick(last_tick).unwrap();

            assert!(
                [59, 60]
                    .map(|secs| last_tick + Duration::from_secs(secs))
                    .contains(&due),
                "{:?} after the last tick",
                due - last_tick,
            );
        }

        #[test]
        fn wakes_when_the_exit_window_closes() {
            let last_tick = now();
            let mut app = settled_app();
            app.exit_armed = Some(last_tick + Duration::from_secs(1));

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
            let mut modal = ScriptedModal::new();
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
            app.exit_armed = Some(last_tick + Duration::from_secs(1));
            app.copy_notice = Some(CopyNotice {
                text: "Copied".to_owned(),
                failed: false,
                until: last_tick + Duration::from_millis(500),
            });

            assert_eq!(
                app.next_tick(last_tick),
                Some(last_tick + Duration::from_millis(500))
            );

            app.exit_armed = Some(last_tick);

            assert_eq!(app.next_tick(last_tick), Some(last_tick + TICK_INTERVAL));
        }
    }
}
