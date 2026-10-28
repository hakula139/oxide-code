//! `/resume` (alias `/continue`) opens a [`SearchableList`] session picker, and
//! `/resume <id-prefix>` resumes directly. `ox -r` without a session opens the same picker at
//! startup.

use std::borrow::Cow;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use strum::VariantArray;
use time::OffsetDateTime;

use ox_core::protocol::{Op, ResumeTarget, SavedTarget};
use ox_core::session::display::{DisplayTitle, ID_PREFIX_WIDTH, id_prefix};
use ox_core::session::saved::{ArchiveUpdate, SavedError};
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::{ArchiveState, SessionInfo};
use ox_core::session::store::listing::{ListScope, ProjectScope, SessionWalk};
use ox_core::util::path::tildify;
use ox_core::util::text::{
    center_truncate_to_width, compact_count, escape_line, pad_start_to_width, pad_to_width, plural,
    truncate_to_width,
};
use ox_core::util::time::now_local;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashOutcome, typed_form};
use super::sessions::{
    BARE_ARCHIVE_HINT, SHORT_RELATIVE_TIME_WIDTH, format_metadata_line, format_short_relative_time,
    saved_error_message,
};
use crate::action::Action;
use crate::glyphs::MODAL_ERROR_PREFIX;
use crate::keymap::{KeyAction, KeyChord, Keymap, is_unmodified};
use crate::modal::chrome::{
    COLUMN_GAP, FOOTER_HEIGHT, above, height_above, render_footer, render_footer_text,
};
use crate::modal::line_editor::LineEditor;
use crate::modal::searchable_list::{SearchableItem, SearchableList};
use crate::modal::{DeleteReply, Modal, ModalAction, ModalKey};
use crate::theme::Theme;
use crate::{NOTICE_DURATION, now};

// ── Constants ──

const PICKER_TITLE: &str = "Resume session";
const PICKER_DESCRIPTION: &str = "Pick a session to resume in place";
const ARCHIVED_PICKER_TITLE: &str = "Resume archived session";
const ARCHIVED_PICKER_DESCRIPTION: &str = "Pick a session to unarchive and resume in place";
const EMPTY_TEXT: &str = "No sessions found";
const VIEWPORT_HEIGHT: u16 = 12;
/// Sessions a worker reads per page.
const PAGE_SIZE: usize = 30;
/// Rows left below the cursor at which the picker loads the next page of sessions.
const LOAD_AHEAD: usize = VIEWPORT_HEIGHT as usize;
const LOADER_STOPPED: &str = "Failed to load sessions: the page loader stopped";

/// Narrowest title a row keeps before it drops a column besides the time.
const TITLE_FLOOR: usize = 24;
/// Narrowest title a row keeps beside the time. Any narrower and the time drops too.
const TITLE_MIN: usize = 12;
const BRANCH_WIDTH: usize = 16;
const PROJECT_WIDTH: usize = 20;
/// Widest message count below a billion, as [`compact_count`] abbreviates it.
const MSGS_WIDTH: usize = "999k msgs".len();

// ── ResumeCmd ──

pub(super) struct ResumeCmd;

impl SlashCommand for ResumeCmd {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn description(&self) -> &'static str {
        "Resume a previous session"
    }

    fn idle_only(&self) -> bool {
        true
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.is_empty()
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if !args.is_empty() {
            return typed_form(self.name(), args, ctx);
        }
        Ok(SlashOutcome::Modal(Box::new(ResumePicker::new(
            ctx.info.store.clone(),
            ctx.info.session_id.clone(),
        ))))
    }
}

// ── SessionRow ──

/// Row payload for the resume picker: display strings and a search haystack.
struct SessionRow {
    session_id: String,
    last_active_at: OffsetDateTime,
    /// When the listing started, in local time. The row's relative times count from here.
    listed_at: OffsetDateTime,
    title: DisplayTitle,
    /// The messages on the session's branch since its last compaction, `0` for an empty branch.
    message_count: u32,
    git_branch: Option<String>,
    /// `Some` only when the picker spans every project, where the path tells rows apart.
    project: Option<String>,
    haystack: String,
}

/// One column of a picker row, declared in the order a row lays them out. The title takes whatever
/// width the other columns leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, VariantArray)]
enum Column {
    Id,
    Title,
    Branch,
    Project,
    Msgs,
    When,
}

impl Column {
    /// The columns a narrowing row drops to keep [`TITLE_FLOOR`], first to last.
    const DROP_ORDER: [Self; 4] = [Self::Id, Self::Branch, Self::Project, Self::Msgs];

    /// Fixed width, or `None` for the flexible title.
    fn width(self) -> Option<usize> {
        match self {
            Self::Id => Some(ID_PREFIX_WIDTH),
            Self::Title => None,
            Self::Branch => Some(BRANCH_WIDTH),
            Self::Project => Some(PROJECT_WIDTH),
            Self::Msgs => Some(MSGS_WIDTH),
            Self::When => Some(SHORT_RELATIVE_TIME_WIDTH),
        }
    }
}

impl SessionRow {
    fn from_info(info: SessionInfo, listed_at: OffsetDateTime, show_project: bool) -> Self {
        let title = DisplayTitle::of(&info);
        let project_path = escape_line(&tildify(Path::new(&info.project_root)));
        // The project is searchable only when shown, since every scoped row shares it.
        let haystack = if show_project {
            format!("{} {} {}", info.session_id, title, project_path)
        } else {
            format!("{} {}", info.session_id, title)
        };
        Self {
            session_id: info.session_id,
            last_active_at: info.last_active_at,
            listed_at,
            title,
            message_count: info.message_count,
            git_branch: info.git_branch,
            project: show_project.then_some(project_path),
            haystack,
        }
    }

    /// Single-line metadata summary for the delete-confirm modal, relative to `now`.
    fn metadata_line(&self, now: OffsetDateTime) -> String {
        format_metadata_line(
            &self.session_id,
            self.last_active_at,
            now,
            self.message_count,
            self.git_branch.as_deref(),
            self.project.as_deref(),
        )
    }

    /// The columns a `width`-column row shows, and the title's width. Columns drop in
    /// [`Column::DROP_ORDER`] until the title keeps [`TITLE_FLOOR`] columns, and the time drops too
    /// when the title would still fall below [`TITLE_MIN`]. The project shows only when the picker
    /// spans every project.
    fn layout(&self, width: usize) -> (Vec<Column>, usize) {
        let mut columns: Vec<Column> = Column::VARIANTS
            .iter()
            .copied()
            .filter(|&c| c != Column::Project || self.project.is_some())
            .collect();
        for drop in Column::DROP_ORDER {
            if width.saturating_sub(fixed_width(&columns)) >= TITLE_FLOOR {
                break;
            }
            columns.retain(|&c| c != drop);
        }
        if width.saturating_sub(fixed_width(&columns)) < TITLE_MIN {
            columns.retain(|&c| c != Column::When);
        }
        let title_width = width.saturating_sub(fixed_width(&columns));
        (columns, title_width)
    }

    /// The text of `column`, fit to `width` columns.
    fn cell(&self, column: Column, width: usize) -> String {
        match column {
            Column::Id => pad_to_width(id_prefix(&self.session_id), width),
            Column::Title => pad_to_width(&self.title.fit(width), width),
            Column::Branch => {
                let branch = self.git_branch.as_deref().unwrap_or_default();
                pad_to_width(&truncate_to_width(branch, width), width)
            }
            Column::Project => {
                let project = self.project.as_deref().unwrap_or_default();
                pad_to_width(&center_truncate_to_width(project, width), width)
            }
            Column::Msgs if self.message_count == 0 => " ".repeat(width),
            Column::Msgs => {
                let n = self.message_count;
                let msgs = format!("{} {}", compact_count(n), plural(n, "msg", "msgs"));
                pad_start_to_width(&truncate_to_width(&msgs, width), width)
            }
            Column::When => {
                let when = format_short_relative_time(self.last_active_at, self.listed_at);
                pad_start_to_width(&when, width)
            }
        }
    }
}

/// Columns besides the title and the gaps between all of `columns`.
fn fixed_width(columns: &[Column]) -> usize {
    let gaps = COLUMN_GAP * columns.len().saturating_sub(1);
    columns.iter().filter_map(|c| c.width()).sum::<usize>() + gaps
}

impl SearchableItem for SessionRow {
    const ROW_HEIGHT: u16 = 1;

    fn haystack(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.haystack)
    }

    /// `{id}  {title}  {branch}  {project}  {msgs}  {when}`, less the columns
    /// [`SessionRow::layout`] drops at `width`.
    fn render(&self, width: u16, is_cursor: bool, theme: &Theme) -> Vec<Line<'static>> {
        let (columns, title_width) = self.layout(usize::from(width));
        let title_style = if is_cursor {
            theme.accent().add_modifier(Modifier::BOLD)
        } else {
            theme.text()
        };
        let mut spans = Vec::with_capacity(columns.len() * 2);
        for (i, column) in columns.into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" ".repeat(COLUMN_GAP)));
            }
            let (width, style) = match column.width() {
                Some(width) => (width, theme.dim()),
                None => (title_width, title_style),
            };
            spans.push(Span::styled(self.cell(column, width), style));
        }
        vec![Line::from(spans)]
    }
}

// ── Actions ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerAction {
    Resume,
    Dismiss,
    /// Archives the highlighted session, or unarchives it in the archived view.
    Archive,
    Delete,
    ToggleScope,
    ToggleArchivedView,
    ClearFilter,
}

const ACTIONS: Keymap<PickerAction, ResumePicker> = Keymap::new(&[
    KeyAction {
        action: PickerAction::Resume,
        key: KeyChord::plain(KeyCode::Enter),
        effect: |picker| picker.has_selection().then_some("resume"),
    },
    KeyAction {
        action: PickerAction::Dismiss,
        key: KeyChord::plain(KeyCode::Esc),
        effect: |picker| {
            Some(match picker.dismissal {
                Dismissal::Cancel => "cancel",
                Dismissal::Quit => "quit",
            })
        },
    },
    KeyAction {
        action: PickerAction::Archive,
        key: KeyChord::ctrl('x'),
        effect: |picker| {
            let effect = match picker.scope.archive {
                ArchiveState::Active => "archive",
                ArchiveState::Archived => "unarchive",
            };
            picker.has_selection().then_some(effect)
        },
    },
    KeyAction {
        action: PickerAction::Delete,
        key: KeyChord::ctrl('d'),
        effect: |picker| picker.has_selection().then_some("delete"),
    },
    KeyAction {
        action: PickerAction::ToggleScope,
        key: KeyChord::plain(KeyCode::Tab),
        effect: |picker| {
            Some(match picker.scope.projects {
                ProjectScope::Current => "show all projects",
                ProjectScope::Every => "show the current project",
            })
        },
    },
    KeyAction {
        action: PickerAction::ToggleArchivedView,
        key: KeyChord::ctrl('a'),
        effect: |picker| {
            Some(match picker.scope.archive {
                ArchiveState::Active => "show archived",
                ArchiveState::Archived => "show active",
            })
        },
    },
    KeyAction {
        action: PickerAction::ClearFilter,
        key: KeyChord::ctrl('u'),
        effect: |picker| picker.list.is_filtered().then_some("clear filter"),
    },
]);

// ── Pages ──

/// The sessions still to load, read a page at a time on a worker thread so a slow store never
/// stalls the render loop.
enum Pages {
    /// Waiting for the next request.
    Idle(SessionWalk),
    /// A worker is reading the next page.
    Loading(Worker),
    /// Every session in scope is loaded.
    Done,
}

/// A worker reading one page, which hands the walk back with it. A reload or a closing picker
/// drops the receiver, and the worker's page goes nowhere.
struct Worker {
    rx: Receiver<(SessionWalk, anyhow::Result<Vec<SessionInfo>>)>,
}

/// What [`Pages::poll`] found.
enum Polled {
    /// No page is ready.
    Pending,
    Page(Vec<SessionInfo>),
    /// The store failed to read the page, which ends the listing.
    Failed(anyhow::Error),
    /// The worker died without a page, which only a panic does.
    Stopped,
}

impl Pages {
    fn has_more(&self) -> bool {
        !matches!(self, Self::Done)
    }

    /// `walk` back at rest, or [`Self::Done`] once it has nothing left.
    fn rest(walk: SessionWalk) -> Self {
        if walk.is_exhausted() {
            Self::Done
        } else {
            Self::Idle(walk)
        }
    }

    /// Starts reading the next page when idle. No-op while loading or done.
    fn request(&mut self) {
        match std::mem::replace(self, Self::Done) {
            Self::Idle(mut walk) => {
                let (tx, rx) = mpsc::channel();
                thread::spawn(move || {
                    let page = walk.next_page(PAGE_SIZE);
                    // A closed channel means the picker moved on to another view or closed.
                    _ = tx.send((walk, page));
                });
                *self = Self::Loading(Worker { rx });
            }
            pages => *self = pages,
        }
    }

    /// The page a worker finished, if any, putting the walk back at rest.
    fn poll(&mut self) -> Polled {
        let Self::Loading(worker) = self else {
            return Polled::Pending;
        };
        match worker.rx.try_recv() {
            Ok((walk, Ok(page))) => {
                *self = Self::rest(walk);
                Polled::Page(page)
            }
            Ok((_, Err(err))) => {
                *self = Self::Done;
                Polled::Failed(err)
            }
            Err(TryRecvError::Empty) => Polled::Pending,
            Err(TryRecvError::Disconnected) => {
                *self = Self::Done;
                Polled::Stopped
            }
        }
    }

    /// [`Self::poll`] that waits for the worker, for tests that need the page now.
    #[cfg(test)]
    fn wait(&mut self) -> Option<Vec<SessionInfo>> {
        let Self::Loading(worker) = self else {
            return None;
        };
        let (walk, page) = worker.rx.recv().expect("page worker panicked");
        *self = Self::rest(walk);
        Some(page.expect("page read failed"))
    }
}

// ── ResumePicker ──

/// What Esc does to the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dismissal {
    /// Closes the picker and keeps the live session.
    Cancel,
    /// Quits `ox`, since the picker stands in for the session at startup.
    Quit,
}

/// Outcome of the last row action, shown in place of the footer hints until the next key or
/// [`NOTICE_DURATION`] passes.
struct Notice {
    text: String,
    failed: bool,
    until: Instant,
}

struct PendingArchive {
    session_id: String,
    title: String,
    listed_at: OffsetDateTime,
}

pub struct ResumePicker {
    store: SessionStore,
    list: SearchableList<SessionRow>,
    scope: ListScope,
    dismissal: Dismissal,
    /// Filtered out of every page so the user can't self-resume onto the open append-writer.
    live_session_id: String,
    pages: Pages,
    /// When the last reload started, in local time, which every page's rows count from.
    listed_at: OffsetDateTime,
    /// Last reload's failure, surfaced inline so failures don't disguise themselves as "no sessions
    /// found". Cleared on the next successful reload.
    load_error: Option<String>,
    notice: Option<Notice>,
    archive: Option<PendingArchive>,
}

impl ResumePicker {
    pub(super) fn new(store: SessionStore, live_session_id: String) -> Self {
        Self::open(
            store,
            live_session_id,
            ProjectScope::Current,
            Dismissal::Cancel,
        )
    }

    /// The picker `ox -r` opens without a session, which starts in `projects`.
    pub fn at_startup(
        store: SessionStore,
        live_session_id: String,
        projects: ProjectScope,
    ) -> Self {
        Self::open(store, live_session_id, projects, Dismissal::Quit)
    }

    fn open(
        store: SessionStore,
        live_session_id: String,
        projects: ProjectScope,
        dismissal: Dismissal,
    ) -> Self {
        let mut picker = Self {
            store,
            list: SearchableList::new(PICKER_TITLE, Vec::new(), VIEWPORT_HEIGHT)
                .with_description(PICKER_DESCRIPTION)
                .with_empty_text(EMPTY_TEXT),
            scope: ListScope {
                projects,
                archive: ArchiveState::Active,
            },
            dismissal,
            live_session_id,
            pages: Pages::Done,
            listed_at: now_local(),
            load_error: None,
            notice: None,
            archive: None,
        };
        picker.reload();
        picker
    }

    /// Restarts the listing for the current scope and view, with the cursor and scroll back at the
    /// top. Rows arrive as pages load.
    fn reload(&mut self) {
        self.listed_at = now_local();
        self.pages = Pages::rest(self.store.walk(self.scope));
        self.load_error = None;
        self.list.replace_items(Vec::new());
        self.load_ahead();
    }

    /// Requests the next page while fewer than [`LOAD_AHEAD`] rows pass the filter below the
    /// cursor, so scrolling reaches the end and a search covers every session in scope.
    fn load_ahead(&mut self) {
        if self.list.rows_below_cursor() < LOAD_AHEAD {
            self.pages.request();
        }
        self.list.set_has_more(self.pages.has_more());
    }

    /// Appends a loaded page and asks for the next one when needed.
    fn append_page(&mut self, page: Vec<SessionInfo>) {
        let live_id = self.live_session_id.as_str();
        let rows = page
            .into_iter()
            .filter(|info| info.session_id != live_id)
            .map(|info| {
                SessionRow::from_info(
                    info,
                    self.listed_at,
                    self.scope.projects == ProjectScope::Every,
                )
            })
            .collect();
        self.list.append_items(rows);
        self.load_ahead();
    }

    fn run(&mut self, action: PickerAction) -> ModalKey {
        if self.archive.is_some()
            && matches!(
                action,
                PickerAction::Resume | PickerAction::Archive | PickerAction::Delete
            )
        {
            return ModalKey::Consumed;
        }
        match action {
            PickerAction::Resume => return self.submit(),
            PickerAction::Dismiss => return ModalKey::Cancelled,
            PickerAction::Delete => return self.start_delete_confirm(),
            PickerAction::Archive => return self.archive_selected(),
            PickerAction::ToggleScope => self.toggle_scope(),
            PickerAction::ToggleArchivedView => self.toggle_archived(),
            PickerAction::ClearFilter => self.list.set_query(""),
        }
        ModalKey::Consumed
    }

    fn toggle_scope(&mut self) {
        self.scope.projects = self.scope.projects.toggled();
        self.reload();
    }

    fn toggle_archived(&mut self) {
        self.scope.archive = self.scope.archive.toggled();
        match self.scope.archive {
            ArchiveState::Active => self.list.set_header(PICKER_TITLE, PICKER_DESCRIPTION),
            ArchiveState::Archived => self
                .list
                .set_header(ARCHIVED_PICKER_TITLE, ARCHIVED_PICKER_DESCRIPTION),
        }
        self.reload();
    }

    fn archive_selected(&self) -> ModalKey {
        let Some(row) = self.list.selected() else {
            return ModalKey::Consumed;
        };
        ModalKey::Emit(ModalAction::Dispatch(Action::Op(Op::SetArchive {
            target: SavedTarget::Id(row.session_id.clone()),
            state: self.scope.archive.toggled(),
        })))
    }

    fn submit(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                target: ResumeTarget::Id(row.session_id.clone()),
            }))),
            // Stay open so the user can Tab the scope or Esc out. Silent dismissal hides why
            // nothing happened.
            None => ModalKey::Consumed,
        }
    }

    /// Opens the [`ConfirmDeleteSessionModal`] for the cursor row, or consumes the key when the
    /// list is empty.
    fn start_delete_confirm(&self) -> ModalKey {
        let Some(row) = self.list.selected() else {
            return ModalKey::Consumed;
        };
        ModalKey::Push(Box::new(ConfirmDeleteSessionModal::new(
            row.session_id.clone(),
            row.title.clone(),
            row.metadata_line(now_local()),
        )))
    }

    fn footer_hints(&self) -> Vec<String> {
        let scope = match self.scope.projects {
            ProjectScope::Current => "current project",
            ProjectScope::Every => "all projects",
        };
        // The scope follows the Esc hint, ahead of the other keys, so a medium-width footer keeps
        // it.
        let mut hints = Vec::new();
        for (action, hint) in ACTIONS.hints(self) {
            hints.push(hint);
            if action == PickerAction::Dismiss {
                hints.push(format!("scope: {scope}"));
            }
        }
        hints
    }

    fn has_selection(&self) -> bool {
        self.archive.is_none() && self.list.selected().is_some()
    }

    /// Loads every page the picker asks for, waiting on each worker.
    #[cfg(test)]
    fn settle(&mut self) {
        while let Some(page) = self.pages.wait() {
            self.append_page(page);
        }
    }
}

impl Modal for ResumePicker {
    fn height(&mut self, width: u16, max_height: u16) -> u16 {
        height_above(max_height, FOOTER_HEIGHT, |room| {
            self.list.height(width, room)
        })
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.list.render(frame, above(area, FOOTER_HEIGHT), theme);

        if let Some(err) = &self.load_error {
            let text = format!("{MODAL_ERROR_PREFIX}{err}");
            render_footer_text(frame, area, &text, theme.error(), theme);
        } else if let Some(notice) = &self.notice {
            if notice.failed {
                let text = format!("{MODAL_ERROR_PREFIX}{}", notice.text);
                render_footer_text(frame, area, &text, theme.error(), theme);
            } else {
                render_footer_text(frame, area, &notice.text, theme.success(), theme);
            }
        } else {
            render_footer(frame, area, &self.footer_hints(), theme);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        self.notice = None;
        let key = if let Some(action) = ACTIONS.action_for(event) {
            self.run(action)
        } else {
            match event.code {
                KeyCode::Up if is_unmodified(event) => self.list.select_prev(),
                KeyCode::Down if is_unmodified(event) => self.list.select_next(),
                KeyCode::PageUp if is_unmodified(event) => self.list.page_up(),
                KeyCode::PageDown if is_unmodified(event) => self.list.page_down(),
                _ => self.list.edit_query(event),
            }
            ModalKey::Consumed
        };
        self.load_ahead();
        key
    }

    fn handle_paste(&mut self, text: &str) {
        self.notice = None;
        self.list.paste_query(text);
        self.load_ahead();
    }

    fn archive_started(&mut self, target: &SavedTarget) {
        let SavedTarget::Id(id) = target else {
            return;
        };
        if let Some(row) = self.list.selected().filter(|row| row.session_id == *id) {
            self.archive = Some(PendingArchive {
                session_id: id.clone(),
                title: row.title.to_string(),
                listed_at: self.listed_at,
            });
        }
    }

    fn archive_updated(
        &mut self,
        target: &SavedTarget,
        state: ArchiveState,
        result: &Result<ArchiveUpdate, SavedError>,
    ) {
        let Some(pending) = self
            .archive
            .take_if(|pending| matches!(target, SavedTarget::Id(id) if *id == pending.session_id))
        else {
            return;
        };
        let id = pending.session_id.as_str();
        let (text, failed) = match result {
            Ok(_) => {
                if self.listed_at == pending.listed_at {
                    self.list.retain_items(|row| row.session_id != id);
                } else {
                    self.reload();
                }
                let verb = match state {
                    ArchiveState::Active => "Unarchived",
                    ArchiveState::Archived => "Archived",
                };
                (
                    format!("{verb} session {}: {}", id_prefix(id), pending.title),
                    false,
                )
            }
            Err(error) => (saved_error_message(error, BARE_ARCHIVE_HINT), true),
        };
        self.notice = Some(Notice {
            text,
            failed,
            until: now() + NOTICE_DURATION,
        });
    }

    fn saved_deleted(&mut self, id: &str, result: &Result<(), SavedError>) -> DeleteReply {
        if result.is_ok() {
            self.list.retain_items(|row| row.session_id != id);
            self.load_ahead();
        }
        DeleteReply::Unclaimed
    }

    fn tick(&mut self, now: Instant) -> bool {
        let expired = self.notice.as_ref().is_some_and(|n| n.until <= now);
        if expired {
            self.notice = None;
        }
        match self.pages.poll() {
            Polled::Pending => expired,
            Polled::Page(page) => {
                self.append_page(page);
                true
            }
            Polled::Failed(err) => {
                tracing::warn!(
                    error = %format_args!("{err:#}"),
                    "resume picker failed to list sessions"
                );
                self.load_error = Some(format!("Failed to load sessions: {err:#}"));
                self.list.set_has_more(false);
                true
            }
            Polled::Stopped => {
                self.load_error = Some(LOADER_STOPPED.to_owned());
                self.list.set_has_more(false);
                true
            }
        }
    }

    fn next_tick(&self, now: Instant) -> Option<Instant> {
        if matches!(self.pages, Pages::Loading(_)) {
            return Some(now);
        }
        self.notice.as_ref().map(|notice| notice.until)
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        (self.dismissal == Dismissal::Quit)
            .then_some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
    }

    // Esc dispatches through `ACTIONS`, so its key and hint stay in one table.
    fn captures_esc(&self) -> bool {
        true
    }

    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        LineEditor::binds_modified_key(event)
    }

    fn sends_idle_only_ops(&self) -> bool {
        true
    }
}

// ── Testing ──

#[cfg(test)]
mod testing {
    use super::{ResumePicker, SessionStore};

    impl ResumePicker {
        pub(crate) fn loaded(store: SessionStore, live_session_id: String) -> Self {
            let mut picker = Self::new(store, live_session_id);
            picker.settle();
            picker
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use time::macros::datetime;

    use ox_core::session::display::UNTITLED_MARKER;
    use ox_core::session::saved::{self, SavedAction};
    use ox_core::session::store::testing::{
        WriterBarrier, archive_test_session, isolated_store, listed_ids, listed_session,
        seed_test_session, stamped_id, test_store,
    };
    use ox_core::util::log::testing::LogCapture;

    use super::Column::{Branch, Id, Msgs, Project, Title, When};
    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::chrome::HINT_SEPARATOR;
    use crate::modal::testing::{render_modal, render_rows, render_rows_with, rendered_text};
    use crate::slash::registry::testing::run_execute;

    fn archive(picker: &mut ResumePicker) -> ModalKey {
        let outcome = picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        if let ModalKey::Emit(ModalAction::Dispatch(Action::Op(Op::SetArchive { target, state }))) =
            &outcome
        {
            picker.archive_started(target);
            let result = saved::testing::set_archived(
                &picker.store,
                target,
                *state,
                &picker.live_session_id,
            );
            picker.archive_updated(target, *state, &result);
        }
        outcome
    }

    /// Seeds one titled session per `(byte, title)` under [`stamped_id`], each last active `byte`
    /// seconds after a fixed time, so a higher byte sorts newer.
    fn seed_titled(store: &SessionStore, sessions: &[(u8, &str)]) {
        for &(byte, title) in sessions {
            seed_test_session(
                store,
                &stamped_id(byte),
                Some(title),
                1,
                datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
            );
        }
    }

    /// Seeds active sessions `0x11` and `0x22`, then archived sessions `0x33` and `0x44`, each last
    /// active a day before the previous one. The fixed dates fall outside the relative-time window,
    /// so each row reads the same every run.
    fn seed_active_and_archived(store: &SessionStore) {
        let sessions = [
            (0x11, "Fix login redirect", 12, false),
            (
                0x22,
                "Tidy the config loader and move the provider settings into their own module",
                1,
                false,
            ),
            (0x33, "Prototype the plugin API", 7, true),
            (0x44, "Draft the release notes", 4, true),
        ];
        for ((byte, title, msg_count, archived), days_ago) in sessions.into_iter().zip(0..) {
            let id = stamped_id(byte);
            let last_active_at =
                datetime!(2026-04-18 09:00:00 UTC) - time::Duration::days(days_ago);
            seed_test_session(store, &id, Some(title), msg_count, last_active_at);
            if archived {
                archive_test_session(store, &id);
            }
        }
    }

    fn footer_row(picker: &mut ResumePicker, width: u16) -> String {
        let rows = render_modal(picker, width, 40);
        rows.last().unwrap().clone()
    }

    /// Seeds `count` sessions titled `session`, the newest first in the walk, and returns their IDs
    /// in list order.
    fn seed_many(store: &SessionStore, count: usize) -> Vec<String> {
        let mut ids: Vec<_> = (1..=count)
            .map(|i| {
                let id = stamped_id(u8::try_from(i).unwrap());
                seed_test_session(
                    store,
                    &id,
                    Some("session"),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC)
                        + time::Duration::seconds(i64::try_from(i).unwrap()),
                );
                id
            })
            .collect();
        ids.reverse();
        ids
    }

    /// `picker` once every page it asks for has loaded.
    fn opened(mut picker: ResumePicker) -> ResumePicker {
        picker.settle();
        picker
    }

    /// Session IDs of the rows passing the filter, in list order.
    fn row_ids(picker: &ResumePicker) -> Vec<String> {
        picker
            .list
            .visible_items()
            .map(|row| row.session_id.clone())
            .collect()
    }

    mod resume_cmd {
        use super::*;

        mod echoes_input {
            use super::*;

            #[test]
            fn only_when_arg_present() {
                assert!(!ResumeCmd.echoes_input(""));
                assert!(ResumeCmd.echoes_input("ab"));
            }
        }

        mod execute {
            use super::*;

            #[test]
            fn bare_opens_the_picker() {
                let outcome = run_execute(&ResumeCmd, "");

                outcome.unwrap().into_modal();
            }

            #[test]
            fn typed_arg_forwards_the_prefix_for_the_engine_to_resolve() {
                let outcome = run_execute(&ResumeCmd, " ab12 ").unwrap();

                assert_eq!(
                    outcome.into_forward(),
                    Action::Op(Op::Resume {
                        target: ResumeTarget::Prefix("ab12".to_owned()),
                    }),
                );
            }
        }
    }

    mod session_row {
        use super::*;

        /// When the test rows are listed, more than 30 days after they were last active.
        const LISTED_AT: OffsetDateTime = datetime!(2026-06-01 00:00:00 UTC);

        /// A session last active on 2026-04-18, more than 30 days ago, so rows show the date.
        fn raw_session_info(
            session_id: &str,
            project_root: &str,
            title: Option<&str>,
            message_count: u32,
            git_branch: Option<&str>,
        ) -> SessionInfo {
            SessionInfo {
                project_root: project_root.to_owned(),
                title: title.map(str::to_owned),
                message_count,
                git_branch: git_branch.map(str::to_owned),
                ..listed_session(session_id)
            }
        }

        /// The `Fix auth` row with 14 messages on `feat/login` in `/work/oxide`.
        fn fix_auth_row(show_project: bool) -> SessionRow {
            let info = raw_session_info(
                &stamped_id(0xab),
                "/work/oxide",
                Some("Fix auth"),
                14,
                Some("feat/login"),
            );
            SessionRow::from_info(info, LISTED_AT, show_project)
        }

        mod from_info {
            use super::*;

            #[test]
            fn shows_the_title_or_the_untitled_marker_and_the_fork_source() {
                let absent = raw_session_info(&stamped_id(0xab), "/work/oxide", None, 0, None);
                let row = SessionRow::from_info(absent, LISTED_AT, true);
                assert_eq!(row.title.to_string(), UNTITLED_MARKER);
                assert!(row.haystack.contains(&row.session_id));
                assert!(row.haystack.contains("/work/oxide"));

                let present = raw_session_info(
                    &stamped_id(0xcd),
                    "/work/oxide",
                    Some("Fix auth bug"),
                    0,
                    None,
                );
                let row = SessionRow::from_info(present, LISTED_AT, true);
                assert_eq!(row.title.to_string(), "Fix auth bug");
                assert!(row.haystack.contains("Fix auth bug"));

                let fork = SessionInfo {
                    forked_from: Some("12345678-0000-4000-8000-000000000000".to_owned()),
                    ..raw_session_info(
                        &stamped_id(0xef),
                        "/work/oxide",
                        Some("Fix auth bug"),
                        0,
                        None,
                    )
                };
                let row = SessionRow::from_info(fork, LISTED_AT, true);
                assert_eq!(row.title.to_string(), "Fix auth bug (fork of 12345678)");
            }

            #[test]
            fn in_scoped_mode_omits_project_from_haystack_and_metadata() {
                let row = fix_auth_row(false);
                assert!(
                    row.project.is_none(),
                    "scoped rows must not carry a project"
                );
                assert!(
                    !row.haystack.contains("/work/oxide"),
                    "scoped haystack must not contain the project path: {}",
                    row.haystack,
                );
            }
        }

        mod layout {
            use super::*;

            #[test]
            fn drops_the_id_then_the_branch_project_and_msgs_as_the_width_shrinks() {
                let row = fix_auth_row(true);
                let columns = |width| row.layout(width).0;

                assert_eq!(columns(97), [Id, Title, Branch, Project, Msgs, When]);
                assert_eq!(columns(96), [Title, Branch, Project, Msgs, When]);
                assert_eq!(columns(86), [Title, Project, Msgs, When]);
                assert_eq!(columns(68), [Title, Msgs, When]);
                assert_eq!(columns(46), [Title, When]);
            }

            #[test]
            fn drops_the_time_once_the_title_would_fall_below_its_minimum() {
                let row = fix_auth_row(true);

                assert_eq!(row.layout(24), (vec![Title, When], 12));
                assert_eq!(row.layout(23), (vec![Title], 23));
            }

            #[test]
            fn shows_no_project_column_in_a_scoped_picker() {
                let row = fix_auth_row(false);

                assert_eq!(row.layout(80).0, [Id, Title, Branch, Msgs, When]);
            }
        }

        mod render {
            use super::*;

            fn rendered(row: &SessionRow, width: u16) -> String {
                row.render(width, false, &Theme::default())[0].to_string()
            }

            #[test]
            fn aligns_each_cell_in_its_column() {
                let one = raw_session_info(
                    &stamped_id(0x22),
                    "/work/oxide",
                    Some("First turn"),
                    1,
                    None,
                );
                let untitled = raw_session_info(&stamped_id(0x33), "/work/oxide", None, 0, None);

                let recent = SessionRow {
                    listed_at: datetime!(2026-04-18 12:00:00 UTC),
                    ..SessionRow::from_info(untitled.clone(), LISTED_AT, true)
                };

                let rows = [
                    rendered(&fix_auth_row(true), 120),
                    rendered(&SessionRow::from_info(one, LISTED_AT, true), 120),
                    rendered(&SessionRow::from_info(untitled, LISTED_AT, true), 120),
                    rendered(&recent, 120),
                ];

                assert_eq!(
                    rows,
                    [
                        "abababab  Fix auth                                         feat/login        /work/oxide             14 msgs  2026-04-18",
                        "22222222  First turn                                                         /work/oxide               1 msg  2026-04-18",
                        "33333333  (untitled)                                                         /work/oxide                      2026-04-18",
                        "33333333  (untitled)                                                         /work/oxide                          3h ago",
                    ],
                );
            }

            #[test]
            fn abbreviates_counts_past_999_to_fit_the_column() {
                let row = |count| {
                    let info = raw_session_info(
                        &stamped_id(0xab),
                        "/work/oxide",
                        Some("Long run"),
                        count,
                        None,
                    );
                    rendered(&SessionRow::from_info(info, LISTED_AT, false), 60)
                };

                assert_eq!(
                    [row(1_500), row(12_345)],
                    [
                        "Long run                                 1k msgs  2026-04-18",
                        "Long run                                12k msgs  2026-04-18",
                    ],
                );
            }

            #[test]
            fn truncates_every_long_cell_to_fill_exactly_its_width() {
                let info = raw_session_info(
                    &stamped_id(0xab),
                    "/home/user/projects/a-rather-deep/checkout",
                    Some("修复登录重定向 and a long English tail that never fits"),
                    123_456,
                    Some("feature/an-unusually-long-branch-name"),
                );
                let row = SessionRow::from_info(info, LISTED_AT, true);

                assert_eq!(
                    [rendered(&row, 160), rendered(&row, 80), rendered(&row, 14)],
                    [
                        "abababab  修复登录重定向 and a long English tail that never fits                                   feature/an-un...  /home/us.../checkout  123k msgs  2026-04-18",
                        "修复登录重定向 and a long Englis...  /home/us.../checkout  123k msgs  2026-04-18",
                        "修复登录重... ",
                    ],
                );
                for width in 12..=160 {
                    let line = row.render(width, false, &Theme::default());
                    assert_eq!(line[0].width(), usize::from(width), "width {width}");
                }
            }

            #[test]
            fn marks_the_cursor_title_in_bold_accent_and_dims_the_other_cells() {
                let row = fix_auth_row(false);
                let theme = Theme::default();
                let title = |line: &Line<'static>| {
                    line.spans
                        .iter()
                        .find(|span| span.content.starts_with("Fix auth"))
                        .unwrap()
                        .style
                };

                let cursor = &row.render(80, true, &theme)[0];
                let other = &row.render(80, false, &theme)[0];

                assert_eq!(title(cursor), theme.accent().add_modifier(Modifier::BOLD));
                assert_eq!(title(other), theme.text());
                assert_eq!(cursor.spans[0].style, theme.dim(), "the ID cell");
                assert_eq!(
                    cursor.spans.last().unwrap().style,
                    theme.dim(),
                    "the time cell"
                );
            }
        }
    }

    mod actions {
        use super::*;

        #[test]
        fn dispatch_each_key_to_its_own_action() {
            let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
            for (event, action) in [
                (KeyEvent::from(KeyCode::Enter), PickerAction::Resume),
                (KeyEvent::from(KeyCode::Esc), PickerAction::Dismiss),
                (ctrl('x'), PickerAction::Archive),
                (ctrl('d'), PickerAction::Delete),
                (KeyEvent::from(KeyCode::Tab), PickerAction::ToggleScope),
                (ctrl('a'), PickerAction::ToggleArchivedView),
                (ctrl('u'), PickerAction::ClearFilter),
            ] {
                assert_eq!(ACTIONS.action_for(&event), Some(action), "{event:?}");
            }
        }
    }

    mod new {
        use super::*;

        #[test]
        fn loads_current_project_rows_into_searchable_list() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("First"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_test_session(
                &store,
                &stamped_id(0x22),
                Some("Second"),
                5,
                datetime!(2026-04-18 09:05:00 UTC),
            );
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert_eq!(row_ids(&picker), [stamped_id(0x22), stamped_id(0x11)]);
        }

        #[test]
        fn filters_out_live_session_to_block_self_resume() {
            let (_dir, store) = isolated_store();
            let live_id = stamped_id(0x11);
            seed_test_session(
                &store,
                &live_id,
                Some("live"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_test_session(
                &store,
                &stamped_id(0x22),
                Some("other"),
                1,
                datetime!(2026-04-18 09:01:00 UTC),
            );
            let picker = opened(ResumePicker::new(store, live_id.clone()));
            assert_eq!(row_ids(&picker), [stamped_id(0x22)]);
        }
    }

    mod at_startup {
        use super::*;

        #[test]
        fn with_all_lists_every_project_and_names_quit_as_the_dismissal() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            seed_test_session(
                &other,
                &stamped_id(0x22),
                Some("foreign"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );

            let scoped = opened(ResumePicker::at_startup(
                store.clone(),
                "live-session-id".to_owned(),
                ProjectScope::Current,
            ));
            assert_eq!(row_ids(&scoped), Vec::<String>::new());
            let picker = opened(ResumePicker::at_startup(
                store,
                "live-session-id".to_owned(),
                ProjectScope::Every,
            ));
            assert_eq!(
                picker.list.selected().map(|row| row.session_id.as_str()),
                Some(stamped_id(0x22).as_str()),
            );
            assert_eq!(
                picker.footer_hints(),
                [
                    "Enter to resume",
                    "Esc to quit",
                    "scope: all projects",
                    "Ctrl+X to archive",
                    "Ctrl+D to delete",
                    "Tab to show the current project",
                    "Ctrl+A to show archived",
                ],
            );
        }
    }

    mod submit {
        use super::*;

        #[test]
        fn enter_with_selection_emits_resume_action() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let ctrl_enter =
                picker.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
            let outcome = picker.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(matches!(ctrl_enter, ModalKey::Consumed), "{ctrl_enter:?}");
            let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                target: ResumeTarget::Id(id),
            }))) = outcome
            else {
                panic!("expected Submitted(Resume), got {outcome:?}");
            };
            assert_eq!(id, stamped_id(0x11));
        }

        #[test]
        fn enter_with_no_rows_keeps_picker_open() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert!(matches!(
                picker.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Consumed,
            ));
        }
    }

    mod footer_hints {
        use super::*;

        #[test]
        fn follow_esc_with_the_scope_and_hide_row_actions_without_rows() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert_eq!(
                picker.footer_hints(),
                [
                    "Enter to resume",
                    "Esc to cancel",
                    "scope: current project",
                    "Ctrl+X to archive",
                    "Ctrl+D to delete",
                    "Tab to show all projects",
                    "Ctrl+A to show archived",
                ],
            );

            let (_dir2, empty_store) = isolated_store();
            let mut empty = opened(ResumePicker::new(empty_store, "live-session-id".to_owned()));
            empty.handle_key(&KeyEvent::from(KeyCode::Tab));
            empty.settle();
            assert_eq!(
                empty.footer_hints(),
                [
                    "Esc to cancel",
                    "scope: all projects",
                    "Tab to show the current project",
                    "Ctrl+A to show archived",
                ],
            );
        }

        #[test]
        fn offer_to_clear_an_active_query() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "auth fix"), (0x22, "ui tweak")]);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            picker.handle_paste("fix");

            assert_eq!(row_ids(&picker), [stamped_id(0x11)]);
            assert_eq!(
                picker.footer_hints().last().unwrap(),
                "Ctrl+U to clear filter"
            );
        }
    }

    mod render {
        use super::*;

        /// Renders `picker` at `width` and its requested height, for a snapshot of the whole frame.
        fn render_to_backend(picker: &mut ResumePicker, width: u16) -> TestBackend {
            let height = picker.height(width, u16::MAX);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| picker.render(frame, frame.area(), &Theme::default()))
                .unwrap();
            terminal.backend().clone()
        }

        #[test]
        fn active_view_lists_only_active_sessions_with_every_hint() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            insta::assert_snapshot!(render_to_backend(&mut picker, 120));
        }

        #[test]
        fn fits_every_project_into_an_80_column_terminal() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::at_startup(
                store,
                "live-session-id".to_owned(),
                ProjectScope::Every,
            ));

            insta::assert_snapshot!(render_to_backend(&mut picker, 80));
        }

        #[test]
        fn keeps_the_title_and_the_time_in_a_narrow_terminal() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            insta::assert_snapshot!(render_to_backend(&mut picker, 48));
        }

        #[test]
        fn archived_view_after_ctrl_a_swaps_the_header_rows_and_hints() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            picker.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            picker.settle();

            insta::assert_snapshot!(render_to_backend(&mut picker, 120));
        }

        #[test]
        fn keeps_the_scope_beside_esc_on_medium_footers() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let medium = footer_row(&mut picker, 80);

            assert_eq!(
                medium,
                "Enter to resume  ·  Esc to cancel  ·  scope: current project  ·  Ctrl+X to ar...",
            );
        }

        #[test]
        fn skips_the_footer_in_an_area_too_short_for_it() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let rows = render_rows(&mut picker, 60, 1);

            assert_eq!(rows, [" ".repeat(60)]);
        }

        #[test]
        fn keeps_the_resume_and_cancel_hints_on_a_narrow_footer() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("Auth"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let rows = render_modal(&mut picker, 40, 40);
            assert_eq!(
                rows.last().unwrap(),
                "Enter to resume  ·  Esc to cancel  · ..."
            );
        }

        #[test]
        fn surfaces_load_error_in_footer() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.load_error = Some("permission denied".to_owned());

            let rows = render_modal(&mut picker, 60, 40);
            assert_eq!(rows.last().unwrap(), "! permission denied");
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn loads_the_next_page_as_the_cursor_nears_the_last_loaded_row() {
            let (_dir, store) = isolated_store();
            let ids = seed_many(&store, PAGE_SIZE + 5);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert_eq!(row_ids(&picker), ids[..PAGE_SIZE]);
            assert!(picker.pages.has_more());

            while picker.list.rows_below_cursor() >= LOAD_AHEAD {
                picker.handle_key(&KeyEvent::from(KeyCode::Down));
            }
            picker.settle();

            assert_eq!(row_ids(&picker), ids);
            assert!(!picker.pages.has_more());
        }

        #[test]
        fn a_new_scope_discards_the_page_still_loading_for_the_old_one() {
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            let local = seed_many(&store, 1);
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            seed_test_session(
                &other,
                &stamped_id(0x99),
                Some("foreign"),
                1,
                datetime!(2026-04-19 09:00:00 UTC),
            );
            let mut picker = ResumePicker::new(store, "live-session-id".to_owned());

            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();

            assert_eq!(row_ids(&picker), [stamped_id(0x99), local[0].clone()]);
        }

        #[test]
        fn typing_or_pasting_filters_rows_via_searchable_list() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("Auth fix"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_test_session(
                &store,
                &stamped_id(0x22),
                Some("Refactor pass"),
                5,
                datetime!(2026-04-18 09:05:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for c in "auth".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            assert_eq!(picker.list.visible_len(), 1, "only `Auth fix` should match");

            picker.list.set_query("");
            picker.handle_paste("refactor\n");
            assert_eq!(picker.list.query(), "refactor");
            assert_eq!(
                picker.list.visible_len(),
                1,
                "only `Refactor pass` should match"
            );
        }

        #[test]
        fn backspace_pops_one_char_from_query() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("Auth"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for c in "ab".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            picker.handle_key(&KeyEvent::from(KeyCode::Backspace));
            assert_eq!(picker.list.query(), "a");
        }

        #[test]
        fn ctrl_u_clears_query() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("Auth"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for c in "zzz".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            assert_eq!(picker.list.visible_len(), 0);
            let event = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
            picker.handle_key(&event);
            assert_eq!(picker.list.query(), "");
            assert_eq!(picker.list.visible_len(), 1);
        }

        #[test]
        fn navigation_keys_move_cursor_within_visible_set() {
            let (_dir, store) = isolated_store();
            for i in 0..3 {
                seed_test_session(
                    &store,
                    &stamped_id(0x10 + i),
                    Some(&format!("row-{i}")),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(i)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            render_rows(&mut picker, 60, 30);
            assert_eq!(picker.list.cursor_index(), 0);
            picker.handle_key(&KeyEvent::from(KeyCode::Down));
            assert_eq!(picker.list.cursor_index(), 1);
            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            assert_eq!(picker.list.cursor_index(), 0);
            picker.handle_key(&KeyEvent::from(KeyCode::PageDown));
            assert_eq!(
                picker.list.cursor_index(),
                2,
                "PageDown clamps at last visible row",
            );
            picker.handle_key(&KeyEvent::from(KeyCode::PageUp));
            assert_eq!(picker.list.cursor_index(), 0, "PageUp clamps at zero");
        }

        #[test]
        fn tab_toggles_scope_and_reloads_rows() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert_eq!(picker.scope.projects, ProjectScope::Current);
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            assert_eq!(
                picker.scope.projects,
                ProjectScope::Every,
                "Tab flips scope to all-projects"
            );
            assert_eq!(picker.footer_hints()[2], "scope: all projects");
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            assert_eq!(
                picker.scope.projects,
                ProjectScope::Current,
                "second Tab flips back"
            );
        }

        #[test]
        fn tab_widens_scope_to_other_project_sessions_and_preserves_query() {
            // The live session stays out of the widened rows.
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("local"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            seed_test_session(
                &other,
                &stamped_id(0x22),
                Some("foreign"),
                1,
                datetime!(2026-04-18 09:01:00 UTC),
            );
            let live_id = stamped_id(0x33);
            seed_test_session(
                &other,
                &live_id,
                Some("live-foreign"),
                1,
                datetime!(2026-04-18 09:02:00 UTC),
            );

            let mut picker = opened(ResumePicker::new(store, live_id.clone()));
            assert_eq!(
                row_ids(&picker),
                [stamped_id(0x11)],
                "scoped: only the home project"
            );
            for c in "11".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            assert_eq!(picker.list.query(), "11");
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            assert_eq!(
                row_ids(&picker),
                [stamped_id(0x11)],
                "Tab widens to all projects but excludes the live session",
            );
            assert_eq!(
                picker.list.query(),
                "11",
                "Tab must not reset the user's filter",
            );
        }

        #[test]
        fn ctrl_a_toggles_between_active_and_archived_sessions() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
            let active_view = rendered_text(&mut picker, 180);

            picker.handle_key(&ctrl_a);
            picker.settle();
            let outcome = picker.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(
                    &outcome,
                    ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                        target: ResumeTarget::Id(id),
                    }))) if *id == stamped_id(0x33)
                ),
                "Enter must resume the archived session: {outcome:?}",
            );

            picker.handle_key(&ctrl_a);
            picker.settle();
            assert_eq!(
                rendered_text(&mut picker, 180),
                active_view,
                "a second Ctrl+A restores the active view",
            );
        }

        #[test]
        fn unrecognized_keys_are_consumed_without_side_effects() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let outcome = picker.handle_key(&KeyEvent::from(KeyCode::Insert));
            assert!(matches!(outcome, ModalKey::Consumed));
            assert_eq!(picker.list.query(), "");
            assert_eq!(picker.list.cursor_index(), 0);

            let event = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
            picker.handle_key(&event);
            assert_eq!(picker.list.query(), "");
        }

        #[test]
        fn ctrl_d_pushes_the_confirm_modal_for_the_cursor_row_while_delete_edits_the_query() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("Fix auth"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for c in "auths".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            picker.handle_key(&KeyEvent::from(KeyCode::Left));

            let outcome = picker.handle_key(&KeyEvent::from(KeyCode::Delete));
            assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
            assert_eq!(
                picker.list.query(),
                "auth",
                "Delete removes the char after the cursor"
            );

            let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
            let outcome = picker.handle_key(&ctrl_d);

            let ModalKey::Push(mut confirm) = outcome else {
                panic!("Ctrl+D must push the confirm modal, got {outcome:?}");
            };
            let rows = render_modal(&mut *confirm, 60, u16::MAX);
            assert_eq!(rows[2], "Fix auth");
            assert_eq!(rows[3], "11111111 · 2026-04-18 · 3 msgs");
        }

        #[test]
        fn ctrl_d_with_no_rows_consumes_silently_instead_of_pushing() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
            let outcome = picker.handle_key(&ctrl_d);
            assert!(matches!(outcome, ModalKey::Consumed));
        }

        #[tokio::test(start_paused = true)]
        async fn ctrl_d_then_y_deletes_the_cursor_session_through_the_modal_stack() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "first"), (0x22, "second")]);
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let mut stack = ModalStack::default();
            stack.push(Box::new(picker));
            let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
            let shown = |stack: &mut ModalStack| {
                render_rows_with(60, 10, |frame, area, theme| {
                    stack.render(frame, area, theme);
                })
                .join("\n")
            };

            let pushed = stack.handle_key(&ctrl_d);
            assert!(pushed.is_none(), "{pushed:?}");
            shown(&mut stack);
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let confirmed = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            assert!(
                matches!(
                    &confirmed,
                    Some(ModalAction::Dispatch(Action::Op(Op::DeleteSaved { session_id })))
                        if *session_id == stamped_id(0x22)
                ),
                "{confirmed:?}",
            );

            let actions = stack.saved_deleted(&stamped_id(0x22), &Ok(()));

            assert!(
                matches!(
                    actions.as_deref(),
                    Some([ModalAction::SystemMessage(msg)]) if msg == "Deleted session 22222222: second"
                ),
                "{actions:?}",
            );
            assert!(
                stack.is_active(),
                "the picker stays after the confirm modal closes"
            );
            let rows = shown(&mut stack);
            assert!(rows.contains("first") && !rows.contains("second"), "{rows}");
        }

        #[test]
        fn ctrl_x_archives_the_cursor_row_and_keeps_the_filter_and_scope() {
            let (_dir, store) = isolated_store();
            seed_titled(
                &store,
                &[
                    (0x11, "fix one"),
                    (0x22, "fix two"),
                    (0x33, "fix three"),
                    (0x44, "docs"),
                ],
            );
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            for c in "fix".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            picker.handle_key(&KeyEvent::from(KeyCode::Down));

            let outcome = archive(&mut picker);

            assert!(matches!(outcome, ModalKey::Emit(_)), "{outcome:?}");
            assert_eq!(
                listed_ids(&store, ArchiveState::Archived),
                [stamped_id(0x22)]
            );
            assert_eq!(
                footer_row(&mut picker, 80),
                "Archived session 22222222: fix two"
            );
            assert_eq!(
                picker.list.selected().unwrap().session_id,
                stamped_id(0x11),
                "the highlight moves to the row that followed",
            );
            assert_eq!(picker.list.visible_len(), 2);
            assert_eq!(picker.list.query(), "fix");
            assert_eq!(
                picker.scope.projects,
                ProjectScope::Every,
                "the scope survives the reload"
            );

            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            assert_eq!(
                footer_row(&mut picker, 200),
                picker.footer_hints().join(HINT_SEPARATOR),
                "the next key brings the hints back",
            );
        }

        #[test]
        fn ctrl_x_keeps_the_visible_rows_in_place() {
            let (_dir, store) = isolated_store();
            for i in 0..20_u8 {
                seed_test_session(
                    &store,
                    &stamped_id(i + 1),
                    Some(&format!("row-{i}")),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(i)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let first_shown = |picker: &mut ResumePicker| -> String {
                let rows = render_modal(picker, 60, 40);
                rows[5].split_whitespace().next().unwrap().to_owned()
            };
            for _ in 0..15 {
                picker.handle_key(&KeyEvent::from(KeyCode::Down));
            }
            first_shown(&mut picker);
            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            assert_eq!(first_shown(&mut picker), "row-15");

            archive(&mut picker);

            assert_eq!(
                first_shown(&mut picker),
                "row-15",
                "the window keeps its first row"
            );
            assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(6));
        }

        #[test]
        fn ctrl_x_in_the_archived_view_unarchives_the_last_row_and_lands_above_it() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "first"), (0x22, "second")]);
            for byte in [0x11, 0x22] {
                archive_test_session(&store, &stamped_id(byte));
            }
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            picker.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            picker.settle();
            picker.handle_key(&KeyEvent::from(KeyCode::Down));

            archive(&mut picker);

            assert_eq!(listed_ids(&store, ArchiveState::Active), [stamped_id(0x11)]);
            assert_eq!(
                footer_row(&mut picker, 80),
                "Unarchived session 11111111: first"
            );
            assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(0x22));
        }

        #[test]
        fn ctrl_x_on_the_live_session_points_to_bare_archive_and_keeps_the_row() {
            // The picker never lists the live session, so make the cursor row live to reach the
            // session layer's refusal.
            let (_dir, store) = isolated_store();
            let id = stamped_id(0x11);
            seed_test_session(
                &store,
                &id,
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            picker.live_session_id.clone_from(&id);

            archive(&mut picker);

            assert_eq!(
                footer_row(&mut picker, 120),
                "! Session 11111111 is the live session. \
                 Use bare `/archive` to archive it and start a new one.",
            );
            assert_eq!(listed_ids(&store, ArchiveState::Active), [id.as_str()]);
            assert_eq!(row_ids(&picker), [id]);
        }

        #[test]
        fn ctrl_x_never_cuts_from_the_filter() {
            // The filter's editor binds Ctrl+X to cutting the selection, which the picker shadows.
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for c in "ab".chars() {
                picker.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
            picker.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));

            let outcome = archive(&mut picker);

            assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
            assert_eq!(picker.list.query(), "ab");
            assert!(picker.notice.is_none(), "no row, so nothing to archive");
        }

        #[test]
        fn ctrl_x_shows_a_failed_write_and_keeps_the_row() {
            let (_dir, store) = isolated_store();
            let id = stamped_id(0x11);
            seed_test_session(
                &store,
                &id,
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let target = SavedTarget::Id(id.clone());
            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            picker.archive_started(&target);

            picker.archive_updated(
                &target,
                ArchiveState::Archived,
                &Err(SavedError::Failed {
                    action: SavedAction::Archive,
                    id: id.clone(),
                    cause: "disk full".to_owned(),
                }),
            );

            assert_eq!(
                footer_row(&mut picker, 400),
                format!("! Failed to archive session {id}: disk full"),
            );
            assert_eq!(picker.list.selected().unwrap().session_id, id);
        }
    }

    mod handle_paste {
        use super::*;

        #[test]
        fn a_query_loads_pages_until_it_reaches_an_unloaded_match() {
            let (_dir, store) = isolated_store();
            let ids = seed_many(&store, 2 * PAGE_SIZE + 5);
            let oldest = ids.last().unwrap().clone();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            picker.handle_paste(&oldest);
            picker.settle();

            assert_eq!(row_ids(&picker), [oldest]);
            assert!(!picker.pages.has_more());
        }

        #[test]
        fn clears_the_notice() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "first"), (0x22, "second")]);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            archive(&mut picker);
            assert_eq!(
                footer_row(&mut picker, 80),
                "Archived session 22222222: second"
            );

            picker.handle_paste("fir");

            assert_eq!(
                footer_row(&mut picker, 200),
                picker.footer_hints().join(HINT_SEPARATOR),
                "the paste brings the hints back",
            );
        }
    }

    mod archive_updated {
        use super::*;

        #[test]
        fn keeps_input_available_until_completion_and_refreshes_a_changed_view() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "first"), (0x22, "second")]);
            let target = stamped_id(0x22);
            let mut picker = opened(ResumePicker::new(store.clone(), "live".to_owned()));
            let barrier = WriterBarrier::hold(&store);

            let action =
                picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert!(matches!(action, ModalKey::Emit(_)));
            assert!(
                picker.archive.is_none(),
                "only accepted operations become pending"
            );
            let accepted = SavedTarget::Id(target.clone());
            picker.archive_started(&accepted);
            picker.archive_updated(
                &SavedTarget::Id("unrelated".to_owned()),
                ArchiveState::Archived,
                &Ok(ArchiveUpdate {
                    id: "unrelated".to_owned(),
                    changed: true,
                }),
            );
            assert!(matches!(
                picker.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Consumed
            ));
            assert!(matches!(
                picker.handle_key(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                ModalKey::Consumed
            ));
            picker.handle_paste("first");

            assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(0x11));
            assert_eq!(
                listed_ids(&store, ArchiveState::Archived),
                [] as [String; 0]
            );
            picker.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            picker.handle_key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            barrier.release();
            let result =
                saved::testing::set_archived(&store, &accepted, ArchiveState::Archived, "live");

            picker.archive_updated(&accepted, ArchiveState::Archived, &result);
            picker.settle();

            assert_eq!(row_ids(&picker), [target]);
            assert_eq!(
                footer_row(&mut picker, 80),
                "Archived session 22222222: second"
            );
        }
    }

    mod saved_deleted {
        use super::*;

        #[test]
        fn drops_the_deleted_row_and_moves_the_highlight_to_the_next() {
            let (_dir, store) = isolated_store();
            seed_titled(
                &store,
                &[(0x11, "first"), (0x22, "doomed"), (0x33, "third")],
            );
            let target = stamped_id(0x22);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.handle_key(&KeyEvent::from(KeyCode::Down));
            assert_eq!(picker.list.selected().unwrap().session_id, target);

            let action = picker.saved_deleted(&target, &Ok(()));

            assert!(
                matches!(action, DeleteReply::Unclaimed),
                "the picker stays open: {action:?}"
            );
            assert_eq!(row_ids(&picker), [stamped_id(0x33), stamped_id(0x11)]);
            assert_eq!(
                picker.list.selected().unwrap().session_id,
                stamped_id(0x11),
                "the row that followed takes the highlight",
            );
        }

        #[test]
        fn loads_the_next_page_once_the_delete_leaves_too_few_rows_below_the_cursor() {
            let (_dir, store) = isolated_store();
            let ids = seed_many(&store, PAGE_SIZE + 5);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            while picker.list.rows_below_cursor() > LOAD_AHEAD {
                picker.handle_key(&KeyEvent::from(KeyCode::Down));
            }
            picker.settle();
            assert_eq!(row_ids(&picker), ids[..PAGE_SIZE]);

            let action = picker.saved_deleted(&ids[PAGE_SIZE - 1], &Ok(()));
            picker.settle();

            assert!(matches!(action, DeleteReply::Unclaimed), "{action:?}");
            let mut remaining = ids.clone();
            remaining.remove(PAGE_SIZE - 1);
            assert_eq!(row_ids(&picker), remaining);
        }

        #[test]
        fn keeps_every_row_and_the_highlight_after_a_failed_delete() {
            let (_dir, store) = isolated_store();
            seed_titled(&store, &[(0x11, "first"), (0x22, "second")]);
            let target = stamped_id(0x11);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.handle_key(&KeyEvent::from(KeyCode::Down));
            let failure = SavedError::Failed {
                action: SavedAction::Delete,
                id: "11111111".to_owned(),
                cause: "permission denied".to_owned(),
            };

            let action = picker.saved_deleted(&target, &Err(failure));

            assert!(matches!(action, DeleteReply::Unclaimed), "{action:?}");
            assert_eq!(row_ids(&picker), [stamped_id(0x22), target.clone()]);
            assert_eq!(picker.list.selected().unwrap().session_id, target);
        }
    }

    mod tick {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn restores_the_hints_once_the_archive_notice_expires() {
            let (_dir, store) = isolated_store();
            seed_test_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            archive(&mut picker);
            let notice = "Archived session 11111111: only";
            assert_eq!(footer_row(&mut picker, 120), notice);

            let just_before = NOTICE_DURATION
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            tokio::time::advance(just_before).await;
            assert!(!picker.tick(now()), "the notice is still due");
            assert_eq!(footer_row(&mut picker, 120), notice);

            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(picker.tick(now()), "the expiry asks for a repaint");
            assert_eq!(
                footer_row(&mut picker, 120),
                picker.footer_hints().join(HINT_SEPARATOR),
            );
        }

        #[test]
        fn appends_the_page_a_worker_finished_and_asks_for_a_repaint() {
            let (_dir, store) = isolated_store();
            let ids = seed_many(&store, 2);
            let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
            assert_eq!(
                row_ids(&picker),
                Vec::<String>::new(),
                "the page is still loading"
            );

            let deadline = Instant::now() + Duration::from_secs(5);
            while !picker.tick(now()) {
                assert!(Instant::now() < deadline, "the page never arrived");
                std::thread::yield_now();
            }

            assert_eq!(row_ids(&picker), ids);
            assert!(!picker.tick(now()), "nothing left to load or expire");
        }

        #[test]
        fn surfaces_and_logs_a_page_the_store_failed_to_read() {
            let (_dir, store) = isolated_store();
            let walk = store.walk(ListScope::default());
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let (tx, rx) = mpsc::channel();
            tx.send((walk, Err(anyhow::anyhow!("disk I/O error"))))
                .unwrap();
            picker.pages = Pages::Loading(Worker { rx });
            picker.list.set_has_more(true);
            let logs = LogCapture::default();

            let repaint =
                tracing::dispatcher::with_default(&logs.dispatch(), || picker.tick(now()));

            assert!(repaint);
            let error = "Failed to load sessions: disk I/O error";
            assert_eq!(picker.load_error.as_deref(), Some(error));
            assert_eq!(
                footer_row(&mut picker, 80),
                format!("{MODAL_ERROR_PREFIX}{error}"),
            );
            assert!(!picker.pages.has_more());
            assert_eq!(
                logs.events(),
                [BTreeMap::from([
                    ("error".to_owned(), "disk I/O error".to_owned()),
                    (
                        "message".to_owned(),
                        "resume picker failed to list sessions".to_owned(),
                    ),
                ])]
            );
        }

        #[test]
        fn surfaces_a_page_loader_that_stopped_without_a_page() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let (tx, rx) = mpsc::channel();
            drop(tx);
            picker.pages = Pages::Loading(Worker { rx });
            picker.list.set_has_more(true);

            assert!(picker.tick(now()));

            assert_eq!(picker.load_error.as_deref(), Some(LOADER_STOPPED));
            assert_eq!(
                footer_row(&mut picker, 80),
                format!("{MODAL_ERROR_PREFIX}{LOADER_STOPPED}"),
            );
            assert!(!picker.pages.has_more());
        }
    }

    mod next_tick {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn ticks_every_frame_while_a_page_loads_and_then_waits_for_the_notice() {
            let (_dir, store) = isolated_store();
            seed_many(&store, 1);
            let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
            let start = now();
            assert_eq!(picker.next_tick(start), Some(start), "the first page loads");

            picker.settle();
            assert_eq!(picker.next_tick(start), None);

            archive(&mut picker);

            assert_eq!(picker.next_tick(start), Some(start + NOTICE_DURATION));
        }
    }

    mod on_cancel {
        use super::*;

        #[test]
        fn quits_from_the_startup_picker_only() {
            let (_dir, store) = isolated_store();
            let mut stack = ModalStack::default();
            stack.push(Box::new(ResumePicker::at_startup(
                store.clone(),
                "live-session-id".to_owned(),
                ProjectScope::Current,
            )));
            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));
            assert!(
                matches!(
                    outcome,
                    Some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
                ),
                "{outcome:?}",
            );
            assert!(!stack.is_active());

            let mut slash_picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert!(slash_picker.on_cancel().is_none());
        }
    }
}
