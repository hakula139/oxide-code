//! Root TUI application.
//!
//! [`App`] owns every component, holds the cross-task channels, and runs the `tokio::select!`
//! loop multiplexing crossterm events, engine events, git probe results, clipboard reports,
//! background slash-command results, and a 60 FPS render tick. A dirty flag coalesces redraws so
//! each state change renders once. Terminal input, engine events, and drawing live in the
//! `dispatch`, `events`, and `render` submodules.

mod dispatch;
mod events;
mod render;
#[cfg(test)]
mod testing;

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{Stream, StreamExt};
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use ox_core::permission::Mode;
use ox_core::protocol::{Event, Interaction, Op};
use ox_core::session::display::archive_notice;

use crate::clipboard::{Clipboard, Environment};
use crate::components::chat::ChatView;
use crate::components::input::InputArea;
use crate::components::status::{StatusBar, StatusBarInit, StatusHyperlink};
use crate::components::tasks::TaskPanel;
use crate::git_probes::GitProbes;
use crate::modal::{Modal, ModalStack};
use crate::now;
use crate::slash::{ChatOutput, LiveSessionInfo};
use crate::terminal::{Tui, TuiBackend};
use crate::theme::{Theme, ThemeOverrides};

/// Tick interval for animation frames and render coalescing (~60 FPS).
const TICK_INTERVAL: Duration = Duration::from_millis(16);

/// Window in which a second Ctrl+C confirms exit.
const EXIT_WINDOW: Duration = Duration::from_secs(1);

pub(crate) struct App {
    theme: Theme,
    /// The configured `[tui.theme.overrides]`, which every `/theme` preview and swap reapplies.
    theme_overrides: ThemeOverrides,
    /// Theme saved when a `/theme` picker opens. Restored if the modal cancels.
    preview_theme_snapshot: Option<Theme>,
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
    /// Prompt already painted for the in-flight turn, kept as typed. Replayed if auto-compaction
    /// clears chat, and handed back if auto-compaction fails before the prompt is sent.
    active_prompt: Option<String>,
    /// What a failure of the active prompt hands back to the user.
    hand_back: HandBack,
    /// FIFO of prompts submitted mid-turn, kept as typed so Esc restores a `//` escape. Each stays
    /// with the TUI until the engine confirms it with `PromptDrained` or it starts its own turn.
    pending_prompts: VecDeque<String>,
    /// How many leading `pending_prompts` the in-flight turn already holds.
    forwarded_prompts: usize,
    /// Queued prompts the in-flight turn took in, kept as typed so a rolled-back turn hands them
    /// back.
    drained_prompts: Vec<String>,
    should_quit: bool,
    dirty: bool,
}

/// What a failure of the active prompt hands back to the user.
#[derive(Clone, Copy, Default)]
enum HandBack {
    /// Nothing, since the prompt ran and the queue goes on.
    #[default]
    Nothing,
    /// The prompt, since an automatic compaction runs ahead of it and a failure means the engine
    /// never sent it.
    UnsentPrompt,
    /// The prompt and the queued prompts its turn took in, since the engine rolled the turn back.
    RolledBackTurn,
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
    /// Whether resuming the session unarchived it, so the chat opens with a note saying so.
    pub(crate) unarchived: bool,
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
        if history.unarchived {
            chat.push_system_message(archive_notice(&session_info.session_id, false));
        }
        let mut status_bar = StatusBar::new(
            &theme,
            StatusBarInit {
                segments: session_info.config.status_line.clone(),
                model: session_info.config.model.short_display_name().to_owned(),
                effort: session_info.config.effort,
                cwd: session_info.cwd.clone(),
                git_branch,
            },
        );
        status_bar.set_title(history.title);
        let git_probes = GitProbes::new(
            session_info.working_dir.clone(),
            &session_info.config.status_line,
            now(),
        );
        let requested_mode = session_info.config.permission_mode;
        let mode_cycle = match requested_mode {
            Mode::Yolo => &[Mode::Manual, Mode::Plan, Mode::Yolo][..],
            Mode::Manual | Mode::Plan => &[Mode::Manual, Mode::Plan][..],
        };
        let input = InputArea::new(&theme);
        Self {
            theme,
            theme_overrides,
            preview_theme_snapshot: None,
            status_bar,
            git_probes,
            chat,
            tasks: TaskPanel::default(),
            input,
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
            active_prompt: None,
            hand_back: HandBack::Nothing,
            pending_prompts: VecDeque::new(),
            forwarded_prompts: 0,
            drained_prompts: Vec::new(),
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
        let mut tick = tokio::time::interval(TICK_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        self.render(terminal)?;

        loop {
            // Six sources can wake the loop. The tick arm is the only renderer, so handlers only
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
                Some(finished) = self.slash_tasks.join_next() => self.apply_slash_task(finished),
                _ = tick.tick() => {
                    let now = now();
                    self.git_probes.poll(now);
                    self.dirty |= self.status_bar.tick();
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
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::testing::{rendered_text, test_app, test_session_info};
    use super::*;
    use crate::components::status::{Status, TICKS_PER_FRAME};
    use crate::git_probes::GitUpdate;
    use crate::glyphs::SPINNER_FRAMES;
    use crate::terminal::testing::capture_terminal;
    use crate::testing::{buffer_rows, key_event};

    mod new {
        use super::*;

        #[test]
        fn plumbs_resumed_title_and_branch_into_status_bar() {
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
                test_session_info(),
                None,
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
            app.status_bar.set_status(Status::ExitArmed { until });
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

            assert_eq!(app.status_bar.status(), &Status::Idle);
            assert!(app.should_quit);
        }

        #[tokio::test(start_paused = true)]
        async fn repaints_when_the_spinner_frame_advances() {
            // Sleep past the spinner interval so `tick()` reports a visible frame change.
            let (mut app, _rx, event_tx) = test_app(None);
            app.status_bar.set_status(Status::Streaming);
            app.dirty = false;

            let mut terminal = capture_terminal(60, 8);
            let frame = TICK_INTERVAL * u32::try_from(TICKS_PER_FRAME).unwrap();
            let driver = tokio::spawn(async move {
                tokio::time::sleep(frame).await;
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
            assert!(
                !app.input.is_enabled(),
                "engine StreamToken disabled the input",
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

            assert_eq!(format!("{err:#}"), "cannot read terminal input: tty gone");
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
}
