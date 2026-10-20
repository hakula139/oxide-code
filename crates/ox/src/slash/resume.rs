//! `/resume` (alias `/continue`) opens a [`SearchableList`] session picker, and
//! `/resume <id-prefix>` resumes directly. `ox --resume` opens the same picker at startup.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use time::{OffsetDateTime, UtcOffset};
use unicode_width::UnicodeWidthStr;

use ox_core::command;
use ox_core::protocol::{Op, ResumeTarget};
use ox_core::session::display::{
    ID_PREFIX_WIDTH, SHORT_RELATIVE_TIME_WIDTH, display_title, format_metadata_line,
    format_short_relative_time, id_prefix,
};
use ox_core::session::entry::SessionInfo;
use ox_core::session::saved;
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::{ListScope, SessionWalk};
use ox_core::util::path::tildify;
use ox_core::util::text::{center_truncate_to_width, compact_count, plural, truncate_to_width};
use ox_core::util::time::local_offset;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::open_store;
use crate::tui::action::Action;
use crate::tui::glyphs::MODAL_ERROR_PREFIX;
use crate::tui::keymap::{KeyAction, KeyChord, Keymap};
use crate::tui::modal::chrome::{COLUMN_GAP, FOOTER_HEIGHT, render_footer, render_footer_text};
use crate::tui::modal::searchable_list::{SearchableItem, SearchableList};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::tui::{NOTICE_DURATION, now};

// ── Constants ──

const PICKER_TITLE: &str = "Resume session";
const PICKER_DESCRIPTION: &str = "Pick a session to resume in place";
const ARCHIVED_PICKER_TITLE: &str = "Resume archived session";
const ARCHIVED_PICKER_DESCRIPTION: &str = "Pick a session to unarchive and resume in place";
const EMPTY_TEXT: &str = "no sessions found";
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

    fn classify(&self, _args: &str) -> SlashKind {
        // Both forms send a `Resume` op, which the engine drops mid-turn, so both wait for idle.
        SlashKind::Mutating
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        if !args.trim().is_empty() {
            return Ok(SlashOutcome::Forward(Action::Op(command::resume(args)?)));
        }
        Ok(SlashOutcome::Modal(Box::new(ResumePicker::new(
            open_store(ctx.info)?,
            ctx.info.session_id.clone(),
        ))))
    }
}

// ── SessionRow ──

/// Row payload for the resume picker: display strings and a search haystack.
struct SessionRow {
    session_id: String,
    path: PathBuf,
    last_active_at: OffsetDateTime,
    local_offset: UtcOffset,
    title: String,
    /// `0` when no `Summary` line was found, as in a session that never finalized.
    message_count: u32,
    git_branch: Option<String>,
    /// `Some` only when the picker spans every project, where the path tells rows apart.
    project: Option<String>,
    haystack: String,
}

/// One column of a picker row. The title takes whatever width the other columns leave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Column {
    Id,
    Title,
    Branch,
    Project,
    Msgs,
    When,
}

impl Column {
    /// Every column, in the order a row lays them out.
    const LAYOUT: [Self; 6] = [
        Self::Id,
        Self::Title,
        Self::Branch,
        Self::Project,
        Self::Msgs,
        Self::When,
    ];

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

/// Columns besides the title and the gaps between all of `columns`.
fn fixed_width(columns: &[Column]) -> usize {
    let gaps = COLUMN_GAP * columns.len().saturating_sub(1);
    columns.iter().filter_map(|c| c.width()).sum::<usize>() + gaps
}

impl SessionRow {
    fn from_info(info: SessionInfo, local_offset: UtcOffset, show_project: bool) -> Self {
        let title = display_title(&info);
        let project_path = tildify(Path::new(&info.cwd));
        // The project is searchable only when shown, since every scoped row shares it.
        let haystack = if show_project {
            format!("{} {} {}", info.session_id, title, project_path)
        } else {
            format!("{} {}", info.session_id, title)
        };
        Self {
            session_id: info.session_id,
            path: info.path,
            last_active_at: info.last_active_at,
            local_offset,
            title,
            message_count: info.exit.as_ref().map_or(0, |e| e.message_count),
            git_branch: info.git_branch,
            project: show_project.then_some(project_path),
            haystack,
        }
    }

    /// Single-line metadata summary for the delete-confirm modal.
    fn metadata_line(&self) -> String {
        format_metadata_line(
            &self.session_id,
            self.last_active_at,
            self.local_offset,
            self.message_count,
            self.git_branch.as_deref(),
            self.project.as_deref(),
        )
    }

    /// The columns a `width`-column row shows, and the title's width. Columns drop in
    /// [`Column::DROP_ORDER`] until the title keeps [`TITLE_FLOOR`] columns, and the time drops
    /// too when the title would still fall below [`TITLE_MIN`]. The project shows only when the
    /// picker spans every project.
    fn layout(&self, width: usize) -> (Vec<Column>, usize) {
        let mut columns: Vec<Column> = Column::LAYOUT
            .into_iter()
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
            Column::Id => pad(id_prefix(&self.session_id), width, false),
            Column::Title => pad(&truncate_to_width(&self.title, width), width, false),
            Column::Branch => {
                let branch = self.git_branch.as_deref().unwrap_or_default();
                pad(&truncate_to_width(branch, width), width, false)
            }
            Column::Project => {
                let project = self.project.as_deref().unwrap_or_default();
                pad(&center_truncate_to_width(project, width), width, false)
            }
            Column::Msgs if self.message_count == 0 => " ".repeat(width),
            Column::Msgs => {
                let n = self.message_count;
                let msgs = format!("{} {}", compact_count(n), plural(n, "msg", "msgs"));
                pad(&truncate_to_width(&msgs, width), width, true)
            }
            Column::When => {
                let when = format_short_relative_time(self.last_active_at, self.local_offset);
                pad(&when, width, true)
            }
        }
    }
}

/// `text` padded with spaces to `width` columns, on the left when `right` is set.
fn pad(text: &str, width: usize, right: bool) -> String {
    let fill = " ".repeat(width.saturating_sub(text.width()));
    if right {
        format!("{fill}{text}")
    } else {
        format!("{text}{fill}")
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        label: |picker| picker.has_selection().then_some("to resume"),
    },
    KeyAction {
        action: PickerAction::Dismiss,
        key: KeyChord::plain(KeyCode::Esc),
        label: |picker| {
            Some(match picker.dismissal {
                Dismissal::Cancel => "to cancel",
                Dismissal::Quit => "to quit",
            })
        },
    },
    KeyAction {
        action: PickerAction::Archive,
        key: KeyChord::ctrl('x'),
        label: |picker| {
            let label = if picker.archived {
                "to unarchive"
            } else {
                "to archive"
            };
            picker.has_selection().then_some(label)
        },
    },
    KeyAction {
        action: PickerAction::Delete,
        key: KeyChord::ctrl('d'),
        label: |picker| picker.has_selection().then_some("to delete"),
    },
    KeyAction {
        action: PickerAction::ToggleScope,
        key: KeyChord::plain(KeyCode::Tab),
        label: |picker| {
            Some(if picker.all {
                "for current project"
            } else {
                "for all projects"
            })
        },
    },
    KeyAction {
        action: PickerAction::ToggleArchivedView,
        key: KeyChord::ctrl('a'),
        label: |picker| {
            Some(if picker.archived {
                "for active"
            } else {
                "for archived"
            })
        },
    },
    KeyAction {
        action: PickerAction::ClearFilter,
        key: KeyChord::ctrl('u'),
        label: |picker| picker.list.is_filtered().then_some("to clear filter"),
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

/// A worker reading one page, which hands the walk back with it. Dropping the handle, as a reload
/// or a closing picker does, stops the worker at the next file.
struct Worker {
    rx: Receiver<(SessionWalk, Vec<SessionInfo>)>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// What [`Pages::poll`] found.
enum Polled {
    /// No page is ready.
    Pending,
    Page(Vec<SessionInfo>),
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
                let cancel = Arc::new(AtomicBool::new(false));
                let cancelled = Arc::clone(&cancel);
                thread::spawn(move || {
                    let page = read_page(&mut walk, &cancelled);
                    // A closed channel means the picker moved on to another view or closed.
                    _ = tx.send((walk, page));
                });
                *self = Self::Loading(Worker { rx, cancel });
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
            Ok((walk, page)) => {
                *self = Self::rest(walk);
                Polled::Page(page)
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
        Some(page)
    }
}

/// Up to [`PAGE_SIZE`] sessions from `walk`, or fewer once `cancel` is set.
fn read_page(walk: &mut SessionWalk, cancel: &AtomicBool) -> Vec<SessionInfo> {
    let mut page = Vec::new();
    while page.len() < PAGE_SIZE
        && let Some(info) = walk.next_unless(|| cancel.load(Ordering::Relaxed))
    {
        page.push(info);
    }
    page
}

// ── ResumePicker ──

/// What Esc does to the picker.
#[derive(Clone, Copy, PartialEq, Eq)]
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

pub(crate) struct ResumePicker {
    store: SessionStore,
    list: SearchableList<SessionRow>,
    /// Whether the list covers every project.
    all: bool,
    /// Whether the list shows only archived sessions.
    archived: bool,
    dismissal: Dismissal,
    local_offset: UtcOffset,
    /// Filtered out of every page so the user can't self-resume onto the open append-writer.
    live_session_id: String,
    pages: Pages,
    /// Last reload's failure, surfaced inline so failures don't disguise themselves as "no
    /// sessions found". Cleared on the next successful reload.
    load_error: Option<String>,
    notice: Option<Notice>,
}

impl ResumePicker {
    pub(super) fn new(store: SessionStore, live_session_id: String) -> Self {
        Self::open(store, live_session_id, false, Dismissal::Cancel)
    }

    /// The `ox --resume` picker, scoped to every project when `all` is set.
    pub(crate) fn at_startup(store: SessionStore, live_session_id: String, all: bool) -> Self {
        Self::open(store, live_session_id, all, Dismissal::Quit)
    }

    fn open(store: SessionStore, live_session_id: String, all: bool, dismissal: Dismissal) -> Self {
        let mut picker = Self {
            store,
            list: SearchableList::new(PICKER_TITLE, Vec::new(), VIEWPORT_HEIGHT)
                .with_description(PICKER_DESCRIPTION)
                .with_empty_text(EMPTY_TEXT),
            all,
            archived: false,
            dismissal,
            local_offset: local_offset(),
            live_session_id,
            pages: Pages::Done,
            load_error: None,
            notice: None,
        };
        picker.reload();
        picker
    }

    /// Restarts the listing for the current scope and view, with the cursor and scroll back at
    /// the top. Rows arrive as pages load.
    fn reload(&mut self) {
        let scope = ListScope {
            all: self.all,
            archived: self.archived,
        };
        self.pages = match self.store.walk(scope) {
            Ok(walk) => {
                self.load_error = None;
                Pages::rest(walk)
            }
            Err(err) => {
                tracing::warn!("resume picker: listing sessions failed: {err:#}");
                self.load_error = Some(format!("Failed to load sessions: {err:#}"));
                Pages::Done
            }
        };
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
            .map(|info| SessionRow::from_info(info, self.local_offset, self.all))
            .collect();
        self.list.append_items(rows);
        self.load_ahead();
    }

    fn run(&mut self, action: PickerAction) -> ModalKey {
        match action {
            PickerAction::Resume => return self.submit(),
            PickerAction::Dismiss => return ModalKey::Cancelled,
            PickerAction::Delete => return self.start_delete_confirm(),
            PickerAction::Archive => self.archive_selected(),
            PickerAction::ToggleScope => self.toggle_scope(),
            PickerAction::ToggleArchivedView => self.toggle_archived(),
            PickerAction::ClearFilter => self.list.set_query(""),
        }
        ModalKey::Consumed
    }

    fn toggle_scope(&mut self) {
        self.all = !self.all;
        self.reload();
    }

    fn toggle_archived(&mut self) {
        self.archived = !self.archived;
        if self.archived {
            self.list
                .set_header(ARCHIVED_PICKER_TITLE, ARCHIVED_PICKER_DESCRIPTION);
        } else {
            self.list.set_header(PICKER_TITLE, PICKER_DESCRIPTION);
        }
        self.reload();
    }

    /// Moves the cursor row to the other archive view, landing the cursor on the row that followed.
    fn archive_selected(&mut self) {
        let Some(row) = self.list.selected() else {
            return;
        };
        let archived = !self.archived;
        let result =
            saved::set_archived(&row.path, &row.session_id, archived, &self.live_session_id);
        let (text, failed) = match result {
            Ok(()) => {
                let verb = if archived { "Archived" } else { "Unarchived" };
                let text = format!(
                    "{verb} session {}: {}",
                    id_prefix(&row.session_id),
                    row.title
                );
                let id = row.session_id.clone();
                self.list.retain_items(|row| row.session_id != id);
                (text, false)
            }
            Err(err) => (err, true),
        };
        self.notice = Some(Notice {
            text,
            failed,
            until: now() + NOTICE_DURATION,
        });
    }

    fn submit(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                target: ResumeTarget::Path(row.path.clone()),
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
            row.path.clone(),
            row.session_id.clone(),
            row.title.clone(),
            row.metadata_line(),
            self.live_session_id.clone(),
        )))
    }

    fn footer_hints(&self) -> Vec<String> {
        let scope = if self.all {
            "all projects"
        } else {
            "current project"
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
        self.list.selected().is_some()
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
    fn height(&self, width: u16, max_height: u16) -> u16 {
        let list_max = max_height.saturating_sub(FOOTER_HEIGHT);
        self.list.height(width, list_max) + FOOTER_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_area = Rect {
            height: area.height.saturating_sub(FOOTER_HEIGHT),
            ..area
        };
        self.list.render(frame, list_area, theme);

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
                KeyCode::Up => self.list.select_prev(),
                KeyCode::Down => self.list.select_next(),
                KeyCode::PageUp => self.list.page_up(),
                KeyCode::PageDown => self.list.page_down(),
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

    fn on_focus_regained(&mut self) {
        // A session deleted from the nested modal leaves its row behind. Re-seek the cursor after
        // the prune so a row dropped above it doesn't shift the cursor onto another session.
        let prev_id = self.list.selected().map(|r| r.session_id.clone());
        self.list.retain_items(|row| row.path.exists());
        if let Some(id) = prev_id {
            self.list.cursor_to(|row| row.session_id == id);
        }
        self.load_ahead();
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
            Polled::Stopped => {
                self.load_error = Some(LOADER_STOPPED.to_owned());
                self.list.set_has_more(false);
                true
            }
        }
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        (self.dismissal == Dismissal::Quit)
            .then_some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
    }

    // Esc dispatches through `ACTIONS`, so its key and hint stay in one table.
    fn captures_esc(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use time::OffsetDateTime;
    use time::macros::datetime;

    use ox_core::session::display::UNTITLED_MARKER;
    use ox_core::session::entry::{ExitInfo, TitleInfo};
    use ox_core::session::store::testing::{
        archive_test_session, listed_ids, seed_test_session, test_project_dir, test_store,
    };

    use super::Column::{Branch, Id, Msgs, Project, Title, When};
    use super::*;
    use crate::slash::testing::{stamped_id, test_session_info, with_isolated_xdg};
    use crate::tui::modal::chrome::HINT_SEPARATOR;
    use crate::tui::modal::testing::{render_rows, rendered_text};

    fn seed_session(
        store: &SessionStore,
        id: &str,
        title: Option<&str>,
        msg_count: u32,
        created_at: OffsetDateTime,
    ) {
        seed_test_session(store, id, title, Some(msg_count), created_at);
    }

    /// Seeds active sessions `0x11` and `0x22`, then archived sessions `0x33` and `0x44`, each last
    /// active a day before the previous one. The fixed dates fall outside the relative-time window,
    /// so each row reads the same on every run.
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
            seed_session(store, &id, Some(title), msg_count, last_active_at);
            if archived {
                archive_test_session(store, &id);
            }
        }
    }

    fn isolated_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        (dir, store)
    }

    fn footer_row(picker: &ResumePicker, width: u16) -> String {
        let rows = render_rows(picker, width, picker.height(width, 40));
        rows.last().unwrap().trim_end().to_owned()
    }

    /// Seeds `count` sessions titled `session`, the newest first in the walk, and returns
    /// their IDs in list order.
    fn seed_many(store: &SessionStore, count: usize) -> Vec<String> {
        let mut ids: Vec<_> = (1..=count)
            .map(|i| {
                let id = stamped_id(u8::try_from(i).unwrap());
                seed_session(
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

        #[test]
        fn name_and_metadata_match_built_ins_contract() {
            assert_eq!(ResumeCmd.name(), "resume");
            assert_ne!(ResumeCmd.description(), "");
            assert_eq!(ResumeCmd.usage(), Some("[<id-prefix>]"));
        }

        mod classify {
            use super::*;

            #[test]
            fn is_always_mutating() {
                assert_eq!(ResumeCmd.classify(""), SlashKind::Mutating);
                assert_eq!(ResumeCmd.classify("   "), SlashKind::Mutating);
                assert_eq!(ResumeCmd.classify("ab"), SlashKind::Mutating);
            }
        }

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
                with_isolated_xdg(|_| {
                    let info = test_session_info();
                    let outcome = ResumeCmd.execute("", &SlashContext::new(&info, None));
                    outcome.unwrap().into_modal();
                });
            }

            #[test]
            fn typed_arg_forwards_the_prefix_for_the_engine_to_resolve() {
                let info = test_session_info();

                let outcome = ResumeCmd
                    .execute(" ab12 ", &SlashContext::new(&info, None))
                    .unwrap();

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

        /// A session last active on 2026-04-18, more than 30 days ago, so rows show the date.
        /// `message_count` is `None` for a session that never finalized.
        fn raw_session_info(
            session_id: String,
            cwd: &str,
            title: Option<&str>,
            message_count: Option<u32>,
            git_branch: Option<&str>,
        ) -> SessionInfo {
            let last_active_at = datetime!(2026-04-18 09:00:00 UTC);
            SessionInfo {
                session_id,
                path: PathBuf::new(),
                cwd: cwd.to_owned(),
                last_active_at,
                title: title.map(|t| TitleInfo {
                    title: t.to_owned(),
                    updated_at: last_active_at,
                }),
                exit: message_count.map(|message_count| ExitInfo {
                    message_count,
                    updated_at: last_active_at,
                }),
                git_branch: git_branch.map(str::to_owned),
                archived: false,
            }
        }

        /// The `Fix auth` row with 14 messages on `feat/login` in `/work/oxide`.
        fn fix_auth_row(show_project: bool) -> SessionRow {
            let info = raw_session_info(
                stamped_id(0xab),
                "/work/oxide",
                Some("Fix auth"),
                Some(14),
                Some("feat/login"),
            );
            SessionRow::from_info(info, UtcOffset::UTC, show_project)
        }

        mod from_info {
            use super::*;

            #[test]
            fn handles_titled_and_untitled_sessions() {
                let absent = raw_session_info(stamped_id(0xab), "/work/oxide", None, None, None);
                let row = SessionRow::from_info(absent, UtcOffset::UTC, true);
                assert_eq!(row.title, UNTITLED_MARKER);
                assert!(row.haystack.contains(&row.session_id));
                assert!(row.haystack.contains("/work/oxide"));

                let present = raw_session_info(
                    stamped_id(0xcd),
                    "/work/oxide",
                    Some("Fix auth bug"),
                    None,
                    None,
                );
                let row = SessionRow::from_info(present, UtcOffset::UTC, true);
                assert_eq!(row.title, "Fix auth bug");
                assert!(row.haystack.contains("Fix auth bug"));
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
                    stamped_id(0x22),
                    "/work/oxide",
                    Some("First turn"),
                    Some(1),
                    None,
                );
                let unfinished =
                    raw_session_info(stamped_id(0x33), "/work/oxide", None, None, None);

                let rows = [
                    rendered(&fix_auth_row(true), 120),
                    rendered(&SessionRow::from_info(one, UtcOffset::UTC, true), 120),
                    rendered(
                        &SessionRow::from_info(unfinished, UtcOffset::UTC, true),
                        120,
                    ),
                ];

                assert_eq!(
                    rows,
                    [
                        "abababab  Fix auth                                         feat/login        /work/oxide             14 msgs  2026-04-18",
                        "22222222  First turn                                                         /work/oxide               1 msg  2026-04-18",
                        "33333333  (untitled)                                                         /work/oxide                      2026-04-18",
                    ],
                );
            }

            #[test]
            fn abbreviates_counts_past_999_to_fit_the_column() {
                let row = |count| {
                    let info = raw_session_info(
                        stamped_id(0xab),
                        "/work/oxide",
                        Some("Long run"),
                        Some(count),
                        None,
                    );
                    rendered(&SessionRow::from_info(info, UtcOffset::UTC, false), 60)
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
                    stamped_id(0xab),
                    "/home/user/projects/a-rather-deep/checkout",
                    Some("修复登录重定向 and a long English tail that never fits"),
                    Some(123_456),
                    Some("feature/an-unusually-long-branch-name"),
                );
                let row = SessionRow::from_info(info, UtcOffset::UTC, true);

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

    mod worker {
        use super::*;

        mod drop {
            use super::*;

            #[test]
            fn tells_the_worker_to_stop() {
                let cancel = Arc::new(AtomicBool::new(false));
                let (_tx, rx) = mpsc::channel();
                let worker = Worker {
                    rx,
                    cancel: Arc::clone(&cancel),
                };

                drop(worker);

                assert!(cancel.load(Ordering::Relaxed));
            }
        }
    }

    mod read_page {
        use super::*;

        #[test]
        fn stops_at_the_page_size() {
            let (_dir, store) = isolated_store();
            let ids = seed_many(&store, PAGE_SIZE + 1);
            let mut walk = store.walk(ListScope::default()).unwrap();

            let page = read_page(&mut walk, &AtomicBool::new(false));

            let page_ids: Vec<_> = page.into_iter().map(|info| info.session_id).collect();
            assert_eq!(page_ids, ids[..PAGE_SIZE]);
            assert!(!walk.is_exhausted());
        }

        #[test]
        fn reads_nothing_once_cancelled() {
            let (_dir, store) = isolated_store();
            seed_many(&store, 2);
            let mut walk = store.walk(ListScope::default()).unwrap();

            let page = read_page(&mut walk, &AtomicBool::new(true));

            assert!(page.is_empty());
            assert!(!walk.is_exhausted(), "no file was drawn");
        }
    }

    mod new {
        use super::*;

        #[test]
        fn loads_current_project_rows_into_searchable_list() {
            let (_dir, store) = isolated_store();
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("First"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_session(
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
            seed_session(
                &store,
                &live_id,
                Some("live"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_session(
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
            seed_session(
                &other,
                &stamped_id(0x22),
                Some("foreign"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );

            let scoped = opened(ResumePicker::at_startup(
                store.clone(),
                "live-session-id".to_owned(),
                false,
            ));
            assert_eq!(row_ids(&scoped), Vec::<String>::new());
            let picker = opened(ResumePicker::at_startup(
                store,
                "live-session-id".to_owned(),
                true,
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
                    "Tab for current project",
                    "Ctrl+A for archived",
                ],
            );
        }
    }

    mod reload {
        use super::*;

        #[test]
        fn sets_load_error_and_clears_rows_when_the_walk_fails() {
            let (dir, store) = isolated_store();
            let project_dir = test_project_dir(dir.path());
            std::fs::remove_dir_all(&project_dir).unwrap();
            let Err(cause) = store.walk(ListScope::default()) else {
                panic!("the walk must fail without the project directory");
            };

            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert_eq!(
                picker.load_error,
                Some(format!("Failed to load sessions: {cause:#}")),
            );
            assert_eq!(row_ids(&picker), Vec::<String>::new());
        }
    }

    mod submit {
        use super::*;

        #[test]
        fn enter_with_selection_emits_resume_action() {
            let (_dir, store) = isolated_store();
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let expected = store.find_session_path(&stamped_id(0x11)).unwrap();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
                let outcome = picker.handle_key(&KeyEvent::new(KeyCode::Enter, modifiers));
                let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                    target: ResumeTarget::Path(path),
                }))) = outcome
                else {
                    panic!("expected Submitted(Resume) for {modifiers:?}, got {outcome:?}");
                };
                assert_eq!(path, expected);
            }
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
            seed_session(
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
                    "Tab for all projects",
                    "Ctrl+A for archived",
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
                    "Tab for current project",
                    "Ctrl+A for archived",
                ],
            );
        }

        #[test]
        fn offer_to_clear_an_active_query() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [(0x11_u8, "auth fix"), (0x22, "ui tweak")] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
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
        fn render_to_backend(picker: &ResumePicker, width: u16) -> TestBackend {
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
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            insta::assert_snapshot!(render_to_backend(&picker, 120));
        }

        #[test]
        fn fits_every_project_into_an_80_column_terminal() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let picker = opened(ResumePicker::at_startup(
                store,
                "live-session-id".to_owned(),
                true,
            ));

            insta::assert_snapshot!(render_to_backend(&picker, 80));
        }

        #[test]
        fn keeps_the_title_and_the_time_in_a_narrow_terminal() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            insta::assert_snapshot!(render_to_backend(&picker, 48));
        }

        #[test]
        fn archived_view_after_ctrl_a_swaps_the_header_rows_and_hints() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            picker.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            picker.settle();

            insta::assert_snapshot!(render_to_backend(&picker, 120));
        }

        #[test]
        fn keeps_the_scope_beside_esc_on_medium_footers() {
            let (_dir, store) = isolated_store();
            seed_active_and_archived(&store);
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let medium = footer_row(&picker, 80);

            assert_eq!(
                medium,
                "Enter to resume  ·  Esc to cancel  ·  scope: current project  ·  Ctrl+X to ar...",
            );
        }

        #[test]
        fn skips_the_footer_in_an_area_too_short_for_it() {
            let (_dir, store) = isolated_store();
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let rows = render_rows(&picker, 60, 1);

            assert_eq!(rows, [" ".repeat(60)]);
        }

        #[test]
        fn keeps_the_resume_and_cancel_hints_on_a_narrow_footer() {
            let (_dir, store) = isolated_store();
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("Auth"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let rows = render_rows(&picker, 40, picker.height(40, 40));
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

            let rows = render_rows(&picker, 60, picker.height(60, 40));
            assert_eq!(rows.last().unwrap().trim_end(), "! permission denied");
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
            seed_session(
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
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("Auth fix"),
                3,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            seed_session(
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
            seed_session(
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
            seed_session(
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
                seed_session(
                    &store,
                    &stamped_id(0x10 + i),
                    Some(&format!("row-{i}")),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(i)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            render_rows(&picker, 60, 30);
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
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            assert!(!picker.all);
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            assert!(picker.all, "Tab flips scope to all-projects");
            assert_eq!(picker.footer_hints()[2], "scope: all projects");
            picker.handle_key(&KeyEvent::from(KeyCode::Tab));
            picker.settle();
            assert!(!picker.all, "second Tab flips back");
        }

        #[test]
        fn tab_widens_scope_to_other_project_sessions_and_preserves_query() {
            // The live session stays out of the widened rows.
            let dir = tempfile::tempdir().unwrap();
            let store = test_store(dir.path());
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("local"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
            seed_session(
                &other,
                &stamped_id(0x22),
                Some("foreign"),
                1,
                datetime!(2026-04-18 09:01:00 UTC),
            );
            let live_id = stamped_id(0x33);
            seed_session(
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
            let archived_path = store.find_session_path(&stamped_id(0x33)).unwrap();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
            let active_view = rendered_text(&picker, 180);

            picker.handle_key(&ctrl_a);
            picker.settle();
            let outcome = picker.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(
                    &outcome,
                    ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                        target: ResumeTarget::Path(path),
                    }))) if *path == archived_path
                ),
                "Enter must resume the archived session: {outcome:?}",
            );

            picker.handle_key(&ctrl_a);
            picker.settle();
            assert_eq!(
                rendered_text(&picker, 180),
                active_view,
                "a second Ctrl+A restores the active view",
            );
        }

        #[test]
        fn unrecognized_keys_are_consumed_without_side_effects() {
            let (_dir, store) = isolated_store();
            seed_session(
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
            seed_session(
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

            let ModalKey::Push(confirm) = outcome else {
                panic!("Ctrl+D must push the confirm modal, got {outcome:?}");
            };
            let rows = render_rows(confirm.as_ref(), 60, confirm.height(60, u16::MAX));
            assert_eq!(rows[2].trim_end(), "11111111 — Fix auth");
        }

        #[test]
        fn ctrl_d_with_no_rows_consumes_silently_instead_of_pushing() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
            let outcome = picker.handle_key(&ctrl_d);
            assert!(matches!(outcome, ModalKey::Consumed));
        }

        #[test]
        fn ctrl_d_then_y_deletes_the_cursor_session_through_the_modal_stack() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [(0x11_u8, "first"), (0x22, "second")] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
            let picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            let mut stack = crate::tui::modal::ModalStack::default();
            stack.push(Box::new(picker));
            let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

            let pushed = stack.handle_key(&ctrl_d);
            let deleted = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(pushed.is_none(), "{pushed:?}");
            assert!(
                matches!(
                    &deleted,
                    Some(ModalAction::SystemMessage(msg)) if msg == "Deleted session 22222222: second"
                ),
                "{deleted:?}",
            );
            assert!(
                stack.is_active(),
                "the picker stays after the confirm modal pops"
            );
            let remaining: Vec<String> = store
                .list()
                .unwrap()
                .into_iter()
                .map(|info| info.session_id)
                .collect();
            assert_eq!(remaining, [stamped_id(0x11)]);
        }

        #[test]
        fn ctrl_x_archives_the_cursor_row_and_keeps_the_filter_and_scope() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [
                (0x11_u8, "fix one"),
                (0x22, "fix two"),
                (0x33, "fix three"),
                (0x44, "docs"),
            ] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
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

            let outcome =
                picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
            assert_eq!(listed_ids(&store, true), [stamped_id(0x22)]);
            assert_eq!(
                footer_row(&picker, 80),
                "Archived session 22222222: fix two"
            );
            assert_eq!(
                picker.list.selected().unwrap().session_id,
                stamped_id(0x11),
                "the highlight moves to the row that followed",
            );
            assert_eq!(picker.list.visible_len(), 2);
            assert_eq!(picker.list.query(), "fix");
            assert!(picker.all, "the scope survives the reload");

            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            assert_eq!(
                footer_row(&picker, 200),
                picker.footer_hints().join(HINT_SEPARATOR),
                "the next key brings the hints back",
            );
        }

        #[test]
        fn ctrl_x_keeps_the_visible_rows_in_place() {
            let (_dir, store) = isolated_store();
            for i in 0..20_u8 {
                seed_session(
                    &store,
                    &stamped_id(i + 1),
                    Some(&format!("row-{i}")),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(i)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let first_shown = |picker: &ResumePicker| -> String {
                let rows = render_rows(picker, 60, picker.height(60, 40));
                rows[5].split_whitespace().next().unwrap().to_owned()
            };
            for _ in 0..15 {
                picker.handle_key(&KeyEvent::from(KeyCode::Down));
            }
            first_shown(&picker);
            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            picker.handle_key(&KeyEvent::from(KeyCode::Up));
            assert_eq!(first_shown(&picker), "row-15");

            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert_eq!(
                first_shown(&picker),
                "row-15",
                "the window keeps its first row"
            );
            assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(6));
        }

        #[test]
        fn ctrl_x_in_the_archived_view_unarchives_the_last_row_and_lands_above_it() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [(0x11_u8, "first"), (0x22, "second")] {
                let id = stamped_id(byte);
                seed_session(
                    &store,
                    &id,
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
                archive_test_session(&store, &id);
            }
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            picker.handle_key(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            picker.settle();
            picker.handle_key(&KeyEvent::from(KeyCode::Down));

            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert_eq!(listed_ids(&store, false), [stamped_id(0x11)]);
            assert_eq!(
                footer_row(&picker, 80),
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
            seed_session(
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

            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert_eq!(
                footer_row(&picker, 120),
                "! Session 11111111 is the live session. \
                 Use bare `/archive` to archive it and start a new one.",
            );
            assert_eq!(listed_ids(&store, false), [id.as_str()]);
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

            let outcome =
                picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
            assert_eq!(picker.list.query(), "ab");
            assert!(picker.notice.is_none(), "no row, so nothing to archive");
        }

        #[cfg(unix)]
        #[test]
        fn ctrl_x_shows_a_failed_write_and_keeps_the_row() {
            use std::os::unix::fs::PermissionsExt;

            let (_dir, store) = isolated_store();
            let id = stamped_id(0x11);
            seed_session(
                &store,
                &id,
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let path = store.find_session_path(&id).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
            let failure = saved::set_archived(&path, &id, true, "live-session-id").unwrap_err();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));

            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

            assert_eq!(footer_row(&picker, 400), format!("! {failure}"));
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
            for (byte, title) in [(0x11_u8, "first"), (0x22, "second")] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            assert_eq!(footer_row(&picker, 80), "Archived session 22222222: second");

            picker.handle_paste("fir");

            assert_eq!(
                footer_row(&picker, 200),
                picker.footer_hints().join(HINT_SEPARATOR),
                "the paste brings the hints back",
            );
        }
    }

    mod on_focus_regained {
        use super::*;

        #[test]
        fn drops_the_row_of_an_externally_deleted_session() {
            let (_dir, store) = isolated_store();
            let id_a = stamped_id(0x11);
            let id_b = stamped_id(0x22);
            for (id, title) in [(&id_a, "first"), (&id_b, "second")] {
                seed_session(
                    &store,
                    id,
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC),
                );
            }
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            assert_eq!(row_ids(&picker).len(), 2, "both seeded rows present");

            std::fs::remove_file(store.find_session_path(&id_a).unwrap()).unwrap();
            picker.on_focus_regained();
            assert_eq!(row_ids(&picker), [id_b], "the deleted row drops");
        }

        #[test]
        fn preserves_cursor_when_row_is_still_present() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [(0x11_u8, "first"), (0x22, "second"), (0x33, "third")] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.handle_key(&KeyEvent::from(KeyCode::Down));
            let pinned = picker.list.selected().unwrap().session_id.clone();

            picker.on_focus_regained();
            let after = picker.list.selected().unwrap().session_id.clone();
            assert_eq!(
                after, pinned,
                "cursor must remain on the previously selected row"
            );
        }

        #[test]
        fn moves_to_the_next_row_when_the_selected_row_was_deleted() {
            let (_dir, store) = isolated_store();
            for (byte, title) in [(0x11_u8, "first"), (0x22, "doomed"), (0x33, "third")] {
                seed_session(
                    &store,
                    &stamped_id(byte),
                    Some(title),
                    1,
                    datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
                );
            }
            let target = stamped_id(0x22);
            let mut picker = opened(ResumePicker::new(
                store.clone(),
                "live-session-id".to_owned(),
            ));
            picker.list.cursor_to(|row| row.session_id == target);
            assert_eq!(picker.list.selected().unwrap().session_id, target);

            std::fs::remove_file(store.find_session_path(&target).unwrap()).unwrap();

            picker.on_focus_regained();

            assert_eq!(row_ids(&picker), [stamped_id(0x33), stamped_id(0x11)]);
            assert_eq!(
                picker.list.selected().unwrap().session_id,
                stamped_id(0x11),
                "the row that followed takes the highlight",
            );
        }
    }

    mod tick {
        use super::*;

        #[tokio::test(start_paused = true)]
        async fn restores_the_hints_once_the_archive_notice_expires() {
            let (_dir, store) = isolated_store();
            seed_session(
                &store,
                &stamped_id(0x11),
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            picker.handle_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
            let notice = "Archived session 11111111: only";
            assert_eq!(footer_row(&picker, 120), notice);

            let just_before = NOTICE_DURATION
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            tokio::time::advance(just_before).await;
            assert!(!picker.tick(now()), "the notice is still due");
            assert_eq!(footer_row(&picker, 120), notice);

            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(picker.tick(now()), "the expiry asks for a repaint");
            assert_eq!(
                footer_row(&picker, 120),
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
        fn surfaces_a_page_loader_that_stopped_without_a_page() {
            let (_dir, store) = isolated_store();
            let mut picker = opened(ResumePicker::new(store, "live-session-id".to_owned()));
            let (tx, rx) = mpsc::channel();
            drop(tx);
            picker.pages = Pages::Loading(Worker {
                rx,
                cancel: Arc::new(AtomicBool::new(false)),
            });
            picker.list.set_has_more(true);

            assert!(picker.tick(now()));

            assert_eq!(picker.load_error.as_deref(), Some(LOADER_STOPPED));
            assert_eq!(
                footer_row(&picker, 80),
                format!("{MODAL_ERROR_PREFIX}{LOADER_STOPPED}"),
            );
            assert!(!picker.pages.has_more());
        }
    }

    mod on_cancel {
        use super::*;

        #[test]
        fn quits_from_the_startup_picker_only() {
            let (_dir, store) = isolated_store();
            let mut stack = crate::tui::modal::ModalStack::default();
            stack.push(Box::new(ResumePicker::at_startup(
                store.clone(),
                "live-session-id".to_owned(),
                false,
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
