//! `/resume` (alias `/continue`) — open the session picker, or resume directly with
//! `/resume <id-prefix>`. `ox --resume` opens the same picker at startup. The picker uses
//! [`SearchableList`] so the same chrome powers any future searchable modal.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use time::UtcOffset;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::protocol::Op;
use crate::session::display::{display_title, format_metadata_line};
use crate::session::entry::SessionInfo;
use crate::session::resolver::resolve_prefix_to_info;
use crate::session::store::SessionStore;
use crate::session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListPage, ListScope};
use crate::tui::action::Action;
use crate::tui::modal::chrome::{FOOTER_HEIGHT, render_footer};
use crate::tui::modal::searchable_list::{SearchableItem, SearchableList};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::path::tildify;
use crate::util::text::truncate_to_width;

// ── Constants ──

const PICKER_TITLE: &str = "Resume session";
const PICKER_DESCRIPTION: &str = "Pick a session to resume in place.";
const ARCHIVED_PICKER_TITLE: &str = "Resume archived session";
const ARCHIVED_PICKER_DESCRIPTION: &str = "Pick a session to unarchive and resume in place.";
const VIEWPORT_HEIGHT: u16 = 6;
/// Each row paints title + metadata + a trailing blank for breathing room between sessions.
const ROW_HEIGHT: u16 = 3;
/// Floor on the title column so narrow terminals still show a truncated label.
const TITLE_FLOOR: usize = 8;

// ── SessionRow ──

/// Row payload for the resume picker — display strings + a search haystack.
struct SessionRow {
    session_id: String,
    path: PathBuf,
    last_active_at: time::OffsetDateTime,
    local_offset: UtcOffset,
    title: String,
    /// `0` when no `Summary` line was found (older sessions or sessions that never finalized).
    /// Rendered as "N msgs" so the user can see session weight at a glance.
    message_count: u32,
    git_branch: Option<String>,
    /// `Some` only when the picker scope is widened to all projects, so the metadata column can
    /// surface the project path for disambiguation. Scoped picks already share a project, so
    /// painting it would be noise.
    project: Option<String>,
    haystack: String,
}

impl SessionRow {
    fn from_info(info: SessionInfo, local_offset: UtcOffset, show_project: bool) -> Self {
        let title = display_title(&info);
        let project_path = tildify(Path::new(&info.cwd));
        // Project participates in search only when the user can see it. In scoped mode every row
        // shares the same project, so substring-matching against it just confuses the filter.
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

    /// Single-line metadata summary used by both the picker row and the delete-confirm modal.
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
}

impl SearchableItem for SessionRow {
    fn haystack(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.haystack)
    }

    fn render(&self, width: u16, is_cursor: bool, theme: &Theme) -> Vec<Line<'static>> {
        let budget = usize::from(width).max(TITLE_FLOOR);
        let title_style = if is_cursor {
            theme.accent().add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            theme.text()
        };
        let title_line = Line::from(Span::styled(
            truncate_to_width(&self.title, budget),
            title_style,
        ));
        let meta_line = Line::from(Span::styled(
            truncate_to_width(&self.metadata_line(), budget),
            theme.dim(),
        ));
        vec![title_line, meta_line, Line::default()]
    }

    fn row_height() -> u16 {
        ROW_HEIGHT
    }
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

pub(crate) struct ResumePicker {
    store: SessionStore,
    list: SearchableList<SessionRow>,
    /// `false` = current project, `true` = every project.
    all: bool,
    /// `false` = active sessions, `true` = archived ones.
    archived: bool,
    dismissal: Dismissal,
    local_offset: UtcOffset,
    /// Filtered out of every reload so the user can't self-resume onto the open append-writer.
    live_session_id: String,
    /// Rows loaded into the list, at most [`DEFAULT_SESSION_LIST_LIMIT`].
    loaded: usize,
    /// More sessions in scope follow the loaded rows.
    more: bool,
    /// Last reload's failure, surfaced inline so failures don't disguise themselves as "no
    /// sessions found". Cleared on the next successful reload.
    load_error: Option<String>,
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
        let local_offset = crate::util::time::local_offset();
        let mut picker = Self {
            store,
            list: SearchableList::new(PICKER_TITLE, Vec::new(), VIEWPORT_HEIGHT)
                .with_description(PICKER_DESCRIPTION),
            all,
            archived: false,
            dismissal,
            local_offset,
            live_session_id,
            loaded: 0,
            more: false,
            load_error: None,
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        let result = self.store.list_paged(
            Some(DEFAULT_SESSION_LIST_LIMIT),
            ListScope {
                all: self.all,
                archived: self.archived,
            },
        );
        let page = match result {
            Ok(p) => {
                self.load_error = None;
                p
            }
            Err(err) => {
                tracing::warn!("resume picker: list_paged failed: {err:#}");
                self.load_error = Some(format!("failed to load sessions: {err:#}"));
                ListPage::default()
            }
        };
        let local_offset = self.local_offset;
        let live_id = self.live_session_id.as_str();
        let show_project = self.all;
        self.more = page.has_more();
        let rows: Vec<SessionRow> = page
            .into_sessions()
            .into_iter()
            .filter(|info| info.session_id != live_id)
            .map(|info| SessionRow::from_info(info, local_offset, show_project))
            .collect();
        self.loaded = rows.len();
        self.list.replace_items(rows);
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

    fn submit(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume {
                path: row.path.clone(),
            }))),
            // Stay open so the user can Tab the scope or Esc out. Silent dismissal hides why
            // nothing happened.
            None => ModalKey::Consumed,
        }
    }

    /// Builds and pushes the [`ConfirmDeleteSessionModal`] for the cursor row. No-op (key
    /// consumed silently) when the list is empty, so an accidental Ctrl+D with no row selected
    /// doesn't surface anything misleading.
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

    fn footer_text(&self) -> String {
        let scope = if self.all {
            "all projects"
        } else {
            "current project"
        };
        let (kind, other_view) = if self.archived {
            ("archived ", "active")
        } else {
            ("", "archived")
        };
        let loaded = self.loaded;
        let matched = self.list.visible_len();
        let count = match (self.list.is_filtered(), self.more) {
            (true, true) => {
                format!("{matched} matching in the {loaded} most recent · more not loaded")
            }
            (true, false) => format!("{matched} / {loaded} matching"),
            (false, true) => format!("{loaded} most recent {kind}sessions · more not loaded"),
            (false, false) => {
                format!(
                    "{loaded} {kind}session{}",
                    if loaded == 1 { "" } else { "s" }
                )
            }
        };
        let dismiss = match self.dismissal {
            Dismissal::Cancel => "cancel",
            Dismissal::Quit => "quit",
        };
        format!(
            "{count} · scope: {scope} · Tab to toggle · Ctrl+A for {other_view} · Enter to resume · \
             Ctrl+D to delete · Esc to {dismiss}"
        )
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

        // Load error owns the footer when set, so failure doesn't look like "0 sessions".
        let footer = if let Some(err) = &self.load_error {
            Line::from(Span::styled(format!("! {err}"), theme.error()))
        } else {
            Line::from(Span::styled(self.footer_text(), theme.dim()))
        };
        render_footer(frame, area, footer, theme);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        match event.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Up => {
                self.list.select_prev();
                ModalKey::Consumed
            }
            KeyCode::Down => {
                self.list.select_next();
                ModalKey::Consumed
            }
            KeyCode::PageUp => {
                self.list.page_up();
                ModalKey::Consumed
            }
            KeyCode::PageDown => {
                self.list.page_down();
                ModalKey::Consumed
            }
            KeyCode::Tab => {
                self.all = !self.all;
                self.reload();
                ModalKey::Consumed
            }
            KeyCode::Char('a') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_archived();
                ModalKey::Consumed
            }
            KeyCode::Char('d') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_delete_confirm()
            }
            KeyCode::Char('u') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.list.set_query("");
                ModalKey::Consumed
            }
            _ => {
                self.list.edit_query(event);
                ModalKey::Consumed
            }
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.list.paste_query(text);
    }

    fn on_focus_regained(&mut self) {
        // Re-seek the cursor after reload so cancel-delete keeps the user on the same row.
        let prev_id = self.list.selected().map(|r| r.session_id.clone());
        self.reload();
        if let Some(id) = prev_id {
            self.list.cursor_to(|row| row.session_id == id);
        }
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        (self.dismissal == Dismissal::Quit)
            .then_some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
    }
}

// ── ResumeCmd ──

pub(super) struct ResumeCmd;

impl SlashCommand for ResumeCmd {
    fn name(&self) -> &'static str {
        "resume"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["continue"]
    }

    fn description(&self) -> &'static str {
        "Resume a previous session"
    }

    fn classify(&self, _args: &str) -> SlashKind {
        // Mutating in both forms: bare opens a picker that submits a Resume op, and typed-arg
        // forwards one directly. The engine drops mid-turn ops, so both forms wait for idle.
        SlashKind::Mutating
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn execute(&self, args: &str, ctx: &SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        let store =
            SessionStore::open().map_err(|e| format!("session store unavailable: {e:#}"))?;
        if arg.is_empty() {
            return Ok(SlashOutcome::Modal(Box::new(ResumePicker::new(
                store,
                ctx.info.session_id.clone(),
            ))));
        }
        let path = resolve_prefix(&store, arg, ctx.info.session_id.as_str())?;
        Ok(SlashOutcome::Forward(Action::Op(Op::Resume { path })))
    }
}

/// Project the shared resolver onto a session file. `/resume` carries no live-id-specific error
/// since the picker already filters the live session out of its rows.
fn resolve_prefix(store: &SessionStore, prefix: &str, live_id: &str) -> Result<PathBuf, String> {
    resolve_prefix_to_info(store, prefix, live_id)?
        .map(|info| info.path)
        .ok_or_else(|| format!("no session matching `{prefix}`"))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use time::OffsetDateTime;
    use time::macros::datetime;

    use super::*;
    use crate::session::display::UNTITLED_MARKER;
    use crate::session::entry::TitleInfo;
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{seed_test_session, test_store};
    use crate::slash::{stamped_id, test_session_info, with_isolated_xdg};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn seed_session(
        store: &SessionStore,
        id: &str,
        title: Option<&str>,
        msg_count: u32,
        created_at: OffsetDateTime,
    ) {
        seed_test_session(store, id, title, Some(msg_count), created_at);
    }

    fn isolated_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        (dir, store)
    }

    // ── SessionRow ──

    fn raw_session_info(
        session_id: String,
        cwd: &str,
        title: Option<&str>,
        last_active_at: time::OffsetDateTime,
    ) -> SessionInfo {
        SessionInfo {
            session_id,
            path: PathBuf::new(),
            cwd: cwd.to_owned(),
            last_active_at,
            title: title.map(|t| TitleInfo {
                title: t.to_owned(),
                updated_at: last_active_at,
            }),
            exit: None,
            git_branch: None,
            archived: false,
        }
    }

    fn raw_session_info_full(
        session_id: String,
        cwd: &str,
        title: Option<&str>,
        last_active_at: time::OffsetDateTime,
        message_count: u32,
        git_branch: Option<&str>,
    ) -> SessionInfo {
        SessionInfo {
            session_id,
            path: PathBuf::new(),
            cwd: cwd.to_owned(),
            last_active_at,
            title: title.map(|t| TitleInfo {
                title: t.to_owned(),
                updated_at: last_active_at,
            }),
            exit: Some(crate::session::entry::ExitInfo {
                message_count,
                updated_at: last_active_at,
            }),
            git_branch: git_branch.map(str::to_owned),
            archived: false,
        }
    }

    #[test]
    fn from_info_handles_title_present_and_absent() {
        let absent = raw_session_info(
            stamped_id(0xab),
            "/work/oxide",
            None,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(absent, UtcOffset::UTC, true);
        assert_eq!(row.title, UNTITLED_MARKER);
        assert!(row.haystack.contains(&row.session_id));
        assert!(row.haystack.contains("/work/oxide"));

        let present = raw_session_info(
            stamped_id(0xcd),
            "/work/oxide",
            Some("Fix auth bug"),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(present, UtcOffset::UTC, true);
        assert_eq!(row.title, "Fix auth bug");
        assert!(row.haystack.contains("Fix auth bug"));
    }

    #[test]
    fn from_info_in_scoped_mode_omits_project_from_haystack_and_metadata() {
        // scope=current means every visible row shares the project — surfacing it would be
        // visual noise and would confuse the substring filter.
        let info = raw_session_info(
            stamped_id(0xab),
            "/work/oxide",
            Some("Fix auth"),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(info, UtcOffset::UTC, false);
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

    // ── render ──

    #[test]
    fn render_paints_title_then_metadata_with_id_prefix_and_relative_time() {
        let info = raw_session_info(
            stamped_id(0xab),
            "/work/oxide",
            Some("Fix auth"),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(info, UtcOffset::UTC, true);
        let theme = Theme::default();
        let lines = row.render(60, false, &theme);
        assert_eq!(
            lines.len(),
            3,
            "row must paint title + meta + trailing blank"
        );
        let title_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let meta_text: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(title_text.contains("Fix auth"), "title row: {title_text}");
        assert!(
            meta_text.contains(&row.session_id[..8]),
            "meta row: {meta_text}"
        );
        assert!(
            meta_text.contains(" ago") || meta_text.contains('-'),
            "meta row must carry a relative time or ISO date: {meta_text}",
        );
        assert!(
            lines[2].spans.iter().all(|s| s.content.is_empty()),
            "trailing row must be blank for between-row breathing room",
        );
    }

    #[test]
    fn render_uses_two_distinct_foregrounds_with_bold_marking_the_cursor_row() {
        let info = raw_session_info(
            stamped_id(0xab),
            "/work/oxide",
            Some("Fix auth"),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(info, UtcOffset::UTC, true);
        let theme = Theme::default();

        let cursor = row.render(60, true, &theme);
        assert_eq!(
            cursor[0].spans[0].style.fg, theme.accent.fg,
            "cursor title uses accent fg",
        );
        assert!(
            cursor[0].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "cursor title is bold",
        );

        let unselected = row.render(60, false, &theme);
        assert_eq!(
            unselected[0].spans[0].style.fg, theme.text.fg,
            "non-cursor title uses text fg",
        );
        assert!(
            !unselected[0].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "non-cursor title is not bold",
        );
        assert_ne!(
            theme.accent.fg, theme.text.fg,
            "cursor vs non-cursor must use distinct fgs",
        );
        assert_eq!(
            unselected[1].spans[0].style.fg, theme.dim.fg,
            "metadata uses dim fg",
        );
        assert_ne!(
            theme.text.fg, theme.dim.fg,
            "title vs metadata must be distinct fgs",
        );
    }

    #[test]
    fn render_metadata_includes_msg_count_and_git_branch_when_present() {
        // The full picker row: id · time · N msgs · branch · project. Singular vs plural is
        // exercised here too — `1 msg` vs `5 msgs`.
        let info = raw_session_info_full(
            stamped_id(0xab),
            "/work/oxide",
            Some("Fix auth"),
            datetime!(2026-04-18 09:00:00 UTC),
            14,
            Some("feat/login"),
        );
        let row = SessionRow::from_info(info, UtcOffset::UTC, true);
        let lines = row.render(80, false, &Theme::default());
        let meta: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(meta.contains("14 msgs"), "plural msgs: {meta}");
        assert!(meta.contains("feat/login"), "branch: {meta}");

        let single = raw_session_info_full(
            stamped_id(0xcd),
            "/work/oxide",
            Some("First turn"),
            datetime!(2026-04-18 09:00:00 UTC),
            1,
            None,
        );
        let row = SessionRow::from_info(single, UtcOffset::UTC, false);
        let lines = row.render(80, false, &Theme::default());
        let meta: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(meta.contains("1 msg"), "singular: {meta}");
        assert!(!meta.contains("1 msgs"), "no `1 msgs` plural slip: {meta}");
    }

    #[test]
    fn render_metadata_omits_msg_count_when_session_never_finalized() {
        // `exit: None` means no Summary line was found — surface no count rather than `0 msgs`.
        let info = raw_session_info(
            stamped_id(0xab),
            "/work/oxide",
            Some("Fix auth"),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let row = SessionRow::from_info(info, UtcOffset::UTC, false);
        let lines = row.render(60, false, &Theme::default());
        let meta: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !meta.contains("msgs"),
            "no count when exit is missing: {meta}"
        );
        assert!(!meta.contains("msg"), "no singular either: {meta}");
    }

    // ── ResumePicker::new ──

    #[test]
    fn new_loads_current_project_rows_into_searchable_list() {
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
        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert_eq!(
            picker.loaded, 2,
            "both seeded sessions should populate the list",
        );
    }

    #[test]
    fn new_filters_out_live_session_to_block_self_resume() {
        // Critical invariant: the live session id never appears as a row, so the user can't
        // submit a Resume that would race the open append-writer.
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
        let picker = ResumePicker::new(store, live_id.clone());
        assert_eq!(picker.loaded, 1, "live session must be filtered out");
        let visible_id = picker
            .list
            .selected()
            .expect("one row remaining")
            .session_id
            .clone();
        assert_ne!(visible_id, live_id);
    }

    // ── ResumePicker::at_startup ──

    #[test]
    fn at_startup_with_all_lists_every_project_and_names_quit_as_the_dismissal() {
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

        let scoped = ResumePicker::at_startup(store.clone(), "live-session-id".to_owned(), false);
        assert_eq!(scoped.loaded, 0);
        let picker = ResumePicker::at_startup(store, "live-session-id".to_owned(), true);
        assert_eq!(
            picker.list.selected().map(|row| row.session_id.as_str()),
            Some(stamped_id(0x22).as_str()),
        );
        assert_eq!(
            picker.footer_text(),
            "1 session · scope: all projects · Tab to toggle · Ctrl+A for archived · \
             Enter to resume · Ctrl+D to delete · Esc to quit",
        );
    }

    // ── ResumePicker::reload ──

    #[test]
    fn reload_uses_shared_default_session_list_cap() {
        let (_dir, store) = isolated_store();
        for i in 0..=DEFAULT_SESSION_LIST_LIMIT {
            seed_session(
                &store,
                &stamped_id(u8::try_from(i + 1).unwrap()),
                Some("session"),
                1,
                datetime!(2026-04-18 09:00:00 UTC)
                    + time::Duration::seconds(i64::try_from(i).unwrap()),
            );
        }

        let picker = ResumePicker::new(store, "live-session-id".to_owned());

        assert_eq!(picker.list.visible_len(), DEFAULT_SESSION_LIST_LIMIT);
        assert!(picker.more);
    }

    #[test]
    fn reload_sets_load_error_and_clears_rows_when_list_paged_fails() {
        // `load_error` lets the footer distinguish "load failed" from "0 sessions"; without it
        // the user has no signal that the listing call errored.
        let (dir, store) = isolated_store();
        let project_dir = crate::session::store::testing::test_project_dir(dir.path());
        std::fs::remove_dir_all(&project_dir).unwrap();

        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(
            picker.load_error.is_some(),
            "reload should surface list_paged failure: {:?}",
            picker.load_error,
        );
        assert_eq!(picker.loaded, 0, "no rows materialise on the Err arm");
    }

    // ── ResumePicker::submit ──

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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let outcome = picker.handle_key(&key(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume { path }))) = outcome
        else {
            panic!("expected Submitted(Resume), got {outcome:?}");
        };
        assert_eq!(path, expected);
    }

    #[test]
    fn enter_with_no_rows_keeps_picker_open() {
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(matches!(
            picker.handle_key(&key(KeyCode::Enter)),
            ModalKey::Consumed,
        ));
    }

    // ── ResumePicker::footer_text ──

    #[test]
    fn footer_text_singular_plural_and_scope_label() {
        // Three permutations: 1 + current_project (singular + scoped), 0 + all_projects after
        // Tab (zero plural + widened), 2 + current_project (plural + scoped).
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("only"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert_eq!(
            picker.footer_text(),
            "1 session · scope: current project · Tab to toggle · Ctrl+A for archived · \
             Enter to resume · Ctrl+D to delete · Esc to cancel",
        );

        let (_dir2, empty_store) = isolated_store();
        let mut empty = ResumePicker::new(empty_store, "live-session-id".to_owned());
        empty.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            empty.footer_text(),
            "0 sessions · scope: all projects · Tab to toggle · Ctrl+A for archived · \
             Enter to resume · Ctrl+D to delete · Esc to cancel",
        );

        let (_dir3, two_store) = isolated_store();
        for byte in [0x11_u8, 0x22] {
            seed_session(
                &two_store,
                &stamped_id(byte),
                Some("t"),
                1,
                datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
            );
        }
        let picker_two = ResumePicker::new(two_store, "live-session-id".to_owned());
        assert!(
            picker_two
                .footer_text()
                .starts_with("2 sessions · scope: current project")
        );
    }

    #[test]
    fn footer_text_shows_matches_over_loaded_rows_when_query_active() {
        let (_dir, store) = isolated_store();
        for (byte, title) in [
            (0x11_u8, "auth fix"),
            (0x22, "ui tweak"),
            (0x33, "ai title"),
        ] {
            seed_session(
                &store,
                &stamped_id(byte),
                Some(title),
                1,
                datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(byte)),
            );
        }
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "fix".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        let footer = picker.footer_text();
        assert!(
            footer.starts_with("1 / 3 matching · scope:"),
            "filter `fix` should narrow to one title but keep the loaded count visible: {footer}",
        );
    }

    #[test]
    fn footer_text_names_the_load_cap_when_sessions_exceed_it() {
        let (_dir, store) = isolated_store();
        for i in 0..=DEFAULT_SESSION_LIST_LIMIT {
            let title = if i == 0 { "oldest" } else { "session" };
            seed_session(
                &store,
                &stamped_id(u8::try_from(i + 1).unwrap()),
                Some(title),
                1,
                datetime!(2026-04-18 09:00:00 UTC)
                    + time::Duration::seconds(i64::try_from(i).unwrap()),
            );
        }
        let live_id = stamped_id(u8::try_from(DEFAULT_SESSION_LIST_LIMIT + 1).unwrap());
        let mut picker = ResumePicker::new(store, live_id);
        let footer = picker.footer_text();
        assert!(
            footer.starts_with("29 most recent sessions · more not loaded · scope:"),
            "the live session is not a loaded row: {footer}",
        );

        for c in "oldest".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        let footer = picker.footer_text();
        assert!(
            footer.starts_with("0 matching in the 29 most recent · more not loaded · scope:"),
            "the filter only searches loaded rows: {footer}",
        );
    }

    // ── Modal::render ──

    #[test]
    fn render_runs_at_typical_widths_without_panicking() {
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("Auth"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        let theme = Theme::default();
        for width in [60_u16, 100, 140] {
            let h = picker.height(width, 40);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| picker.render(frame, Rect::new(0, 0, width, h), &theme))
                .expect("render must not panic");
        }
    }

    #[test]
    fn render_survives_an_area_too_short_for_the_footer() {
        let (_dir, store) = isolated_store();
        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 1)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, Rect::new(0, 0, 60, 1), &theme))
            .expect("render must not panic at height=1");
    }

    #[test]
    fn render_surfaces_load_error_in_footer() {
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        picker.load_error = Some("permission denied".to_owned());

        let theme = Theme::default();
        let h = picker.height(60, 40);
        let mut terminal = Terminal::new(TestBackend::new(60, h)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, Rect::new(0, 0, 60, h), &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        let dump: String = (0..h)
            .flat_map(|y| (0..60_u16).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_owned())
            .collect();
        assert!(
            dump.contains("permission denied"),
            "load error should appear inline: {dump}"
        );
    }

    // ── Modal::handle_key ──

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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "auth".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "ab".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        picker.handle_key(&key(KeyCode::Backspace));
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "zzz".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(picker.list.visible_len(), 0);
        let mut event = KeyEvent::from(KeyCode::Char('u'));
        event.modifiers = KeyModifiers::CONTROL;
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, Rect::new(0, 0, 60, 30), &Theme::default()))
            .unwrap();
        assert_eq!(picker.list.cursor_index(), 0);
        picker.handle_key(&key(KeyCode::Down));
        assert_eq!(picker.list.cursor_index(), 1);
        picker.handle_key(&key(KeyCode::Up));
        assert_eq!(picker.list.cursor_index(), 0);
        picker.handle_key(&key(KeyCode::PageDown));
        assert_eq!(
            picker.list.cursor_index(),
            2,
            "PageDown clamps at last visible row",
        );
        picker.handle_key(&key(KeyCode::PageUp));
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(!picker.all);
        picker.handle_key(&key(KeyCode::Tab));
        assert!(picker.all, "Tab flips scope to all-projects");
        assert!(picker.footer_text().contains("all projects"));
        picker.handle_key(&key(KeyCode::Tab));
        assert!(!picker.all, "second Tab flips back");
    }

    #[test]
    fn tab_widens_scope_to_other_project_sessions_and_preserves_query() {
        // Bare picker stays scoped; Tab widens, preserves the typed filter, AND keeps the
        // live-session out of the widened result so the user can never resume themselves.
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

        let mut picker = ResumePicker::new(store, live_id.clone());
        assert_eq!(picker.loaded, 1, "scoped: only the home project");
        for c in "11".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(picker.list.query(), "11");
        picker.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            picker.loaded, 2,
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
        let active_id = stamped_id(0x11);
        let archived_id = stamped_id(0x22);
        seed_session(
            &store,
            &active_id,
            Some("active"),
            1,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        seed_session(
            &store,
            &archived_id,
            Some("archived"),
            1,
            datetime!(2026-04-18 09:01:00 UTC),
        );
        let archived_path = store.find_session_path(&archived_id).unwrap();
        set_archived_at(&archived_path, true).unwrap();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let selected_id =
            |picker: &ResumePicker| picker.list.selected().unwrap().session_id.clone();
        let mut ctrl_a = KeyEvent::from(KeyCode::Char('a'));
        ctrl_a.modifiers = KeyModifiers::CONTROL;
        assert_eq!(picker.loaded, 1);
        assert_eq!(selected_id(&picker), active_id);

        picker.handle_key(&ctrl_a);
        assert_eq!(picker.loaded, 1);
        assert_eq!(selected_id(&picker), archived_id);
        assert_eq!(
            picker.footer_text(),
            "1 archived session · scope: current project · Tab to toggle · Ctrl+A for active · \
             Enter to resume · Ctrl+D to delete · Esc to cancel",
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, Rect::new(0, 0, 60, 20), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let title: String = (0..60_u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(title.trim_end(), ARCHIVED_PICKER_TITLE);
        let outcome = picker.handle_key(&key(KeyCode::Enter));
        assert!(
            matches!(
                &outcome,
                ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resume { path })))
                    if *path == archived_path
            ),
            "Enter must resume the archived session: {outcome:?}",
        );

        picker.handle_key(&ctrl_a);
        assert_eq!(selected_id(&picker), active_id);
    }

    #[test]
    fn unrecognized_keys_are_consumed_without_side_effects() {
        // Both bare unhandled keys (Insert) and unbound Ctrl chars are absorbed silently —
        // neither may push into the filter or move the cursor.
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("only"),
            1,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let outcome = picker.handle_key(&key(KeyCode::Insert));
        assert!(matches!(outcome, ModalKey::Consumed));
        assert_eq!(picker.list.query(), "");
        assert_eq!(picker.list.cursor_index(), 0);

        let mut event = KeyEvent::from(KeyCode::Char('g'));
        event.modifiers = KeyModifiers::CONTROL;
        picker.handle_key(&event);
        assert_eq!(picker.list.query(), "");
    }

    #[test]
    fn ctrl_d_pushes_confirm_modal_for_cursor_row_while_delete_edits_the_query() {
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("Fix auth"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "auths".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        picker.handle_key(&key(KeyCode::Left));

        let outcome = picker.handle_key(&key(KeyCode::Delete));
        assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
        assert_eq!(
            picker.list.query(),
            "auth",
            "Delete removes the char after the cursor"
        );

        let mut ctrl_d = KeyEvent::from(KeyCode::Char('d'));
        ctrl_d.modifiers = KeyModifiers::CONTROL;
        let outcome = picker.handle_key(&ctrl_d);
        assert!(
            matches!(outcome, ModalKey::Push(_)),
            "Ctrl+D must push the confirm modal; got {outcome:?}",
        );
    }

    #[test]
    fn ctrl_d_with_no_rows_consumes_silently_instead_of_pushing() {
        // No selection → no row identity to confirm against. Better to consume than to surface a
        // confused-looking "delete (nothing)?" modal.
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());

        let mut ctrl_d = KeyEvent::from(KeyCode::Char('d'));
        ctrl_d.modifiers = KeyModifiers::CONTROL;
        let outcome = picker.handle_key(&ctrl_d);
        assert!(matches!(outcome, ModalKey::Consumed));
    }

    // ── ResumePicker::on_focus_regained ──

    #[test]
    fn on_focus_regained_reloads_rows_so_an_externally_deleted_session_disappears() {
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
        let mut picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        assert_eq!(picker.loaded, 2, "both seeded rows present");

        std::fs::remove_file(store.find_session_path(&id_a).unwrap()).unwrap();
        picker.on_focus_regained();
        assert_eq!(picker.loaded, 1, "deleted row drops on reload");
    }

    #[test]
    fn on_focus_regained_preserves_cursor_when_row_is_still_present() {
        // Cancel-delete must leave the user on the row they were inspecting; without re-seeking
        // after reload, replace_items would yank the cursor back to row 0.
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        picker.handle_key(&key(KeyCode::Down));
        let pinned = picker.list.selected().unwrap().session_id.clone();

        picker.on_focus_regained();
        let after = picker.list.selected().unwrap().session_id.clone();
        assert_eq!(
            after, pinned,
            "cursor must remain on the previously selected row"
        );
    }

    #[test]
    fn on_focus_regained_falls_back_to_top_when_previously_selected_row_was_deleted() {
        // If the row is gone, cursor_to is a no-op and the cursor stays at the post-reload zero.
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
        let mut picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        picker.list.cursor_to(|row| row.session_id == target);
        assert_eq!(picker.list.selected().unwrap().session_id, target);

        std::fs::remove_file(store.find_session_path(&target).unwrap()).unwrap();
        picker.on_focus_regained();
        assert_eq!(picker.loaded, 2, "deleted row dropped");
        assert_ne!(
            picker.list.selected().unwrap().session_id,
            target,
            "cursor moved off the deleted row",
        );
        assert_eq!(
            picker.list.cursor_index(),
            0,
            "cursor lands at row 0, the post-reload default",
        );
    }

    #[test]
    fn ctrl_d_then_y_through_modal_stack_unlinks_the_session() {
        // End-to-end: picker on the stack, Ctrl+D pushes the confirm child, Y on the child runs
        // the unlink and pops back. Pins the wiring across ResumePicker, ModalKey::Push,
        // ModalStack::handle_key, ConfirmDeleteSessionModal, and delete_at.
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
        let picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        let mut stack = crate::tui::modal::ModalStack::new();
        stack.push(Box::new(picker));

        let mut ctrl_d = KeyEvent::from(KeyCode::Char('d'));
        ctrl_d.modifiers = KeyModifiers::CONTROL;
        assert!(
            stack.handle_key(&ctrl_d).is_none(),
            "Push outcome must not surface a ModalAction",
        );
        assert!(stack.is_active(), "child sits atop the picker");

        let outcome = stack.handle_key(&key(KeyCode::Char('y')));
        let Some(ModalAction::SystemMessage(msg)) = outcome else {
            panic!("Y must surface a SystemMessage; got {outcome:?}");
        };
        assert!(
            msg.starts_with("Deleted session "),
            "chat-stream confirmation: {msg}",
        );
        assert!(stack.is_active(), "picker remains after the child pops");
        assert_eq!(
            store.list().unwrap().len(),
            1,
            "one row was actually deleted from disk",
        );
    }

    // ── Modal::on_cancel ──

    #[test]
    fn on_cancel_quits_from_the_startup_picker_only() {
        let (_dir, store) = isolated_store();
        let mut stack = crate::tui::modal::ModalStack::new();
        stack.push(Box::new(ResumePicker::at_startup(
            store.clone(),
            "live-session-id".to_owned(),
            false,
        )));
        let outcome = stack.handle_key(&key(KeyCode::Esc));
        assert!(
            matches!(
                outcome,
                Some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
            ),
            "{outcome:?}",
        );
        assert!(!stack.is_active());

        let mut slash_picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(slash_picker.on_cancel().is_none());
    }

    // ── ResumeCmd ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ResumeCmd.name(), "resume");
        assert_eq!(ResumeCmd.aliases(), &["continue"]);
        assert_ne!(ResumeCmd.description(), "");
        assert_eq!(ResumeCmd.usage(), Some("[<id-prefix>]"));
    }

    #[test]
    fn classify_is_always_mutating() {
        // Both forms reach the engine (typed-arg → Forward, picker → modal-submitted Resume),
        // and mid-turn ops get dropped, so neither may run while busy.
        assert_eq!(ResumeCmd.classify(""), SlashKind::Mutating);
        assert_eq!(ResumeCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(ResumeCmd.classify("ab"), SlashKind::Mutating);
    }

    #[test]
    fn echoes_input_only_when_arg_present() {
        assert!(!ResumeCmd.echoes_input(""));
        assert!(ResumeCmd.echoes_input("ab"));
    }

    #[test]
    fn execute_bare_opens_the_picker() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let outcome = ResumeCmd.execute("", &SlashContext::new(&info, None));
            outcome.unwrap().into_modal();
        });
    }

    #[test]
    fn execute_with_unknown_prefix_returns_friendly_error() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let err = ResumeCmd
                .execute("zzz1234", &SlashContext::new(&info, None))
                .unwrap_err();
            assert!(err.contains("no session matching"), "{err}");
        });
    }

    #[test]
    fn execute_typed_arg_unique_match_forwards_the_matched_session_file() {
        with_isolated_xdg(|_dir| {
            let store = SessionStore::open().unwrap();
            let target_id = stamped_id(0xab);
            seed_session(
                &store,
                &target_id,
                Some("only"),
                1,
                datetime!(2026-04-18 09:00:00 UTC),
            );
            let info = test_session_info();
            // 4-char prefix is enough to pin a single session in the seeded store.
            let outcome = ResumeCmd
                .execute(&target_id[..4], &SlashContext::new(&info, None))
                .unwrap();
            let SlashOutcome::Forward(Action::Op(Op::Resume { path })) = outcome else {
                panic!("expected Forward(Resume), got {outcome:?}");
            };
            assert_eq!(path, store.find_session_path(&target_id).unwrap());
        });
    }

    // ── resolve_prefix ──

    #[test]
    fn resolve_prefix_excludes_live_session_id() {
        let (_dir, store) = isolated_store();
        let live = stamped_id(0x11);
        seed_session(
            &store,
            &live,
            Some("live"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let err = resolve_prefix(&store, &live[..2], &live).unwrap_err();
        assert!(err.contains("no session matching"), "{err}");
    }

    #[test]
    fn resolve_prefix_unique_match_returns_its_session_file() {
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("a"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        seed_session(
            &store,
            &stamped_id(0x22),
            Some("b"),
            3,
            datetime!(2026-04-18 09:05:00 UTC),
        );
        let path = resolve_prefix(&store, "1111", "other").unwrap();
        assert_eq!(path, store.find_session_path(&stamped_id(0x11)).unwrap());
    }

    #[test]
    fn resolve_prefix_widens_to_other_projects_when_current_project_misses() {
        // Pin the widening path inside resolve_prefix: scoped match returns None, the all-projects
        // retry finds the target. Without widening, typing a foreign session id would surface
        // as "no session matching" rather than resolving cleanly.
        let dir = tempfile::tempdir().unwrap();
        let other = SessionStore::open_at(dir.path().to_path_buf(), "other-project").unwrap();
        let target = stamped_id(0xab);
        seed_session(
            &other,
            &target,
            Some("foreign"),
            1,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let store = test_store(dir.path());
        let resolved = resolve_prefix(&store, &target[..4], "live").unwrap();
        assert_eq!(resolved, other.find_session_path(&target).unwrap());
    }

    #[test]
    fn resolve_prefix_full_id_match_is_unique() {
        // Pasting a full session id is the common power-user case — must round-trip cleanly
        // and not be confused with an ambiguous prefix.
        let (_dir, store) = isolated_store();
        let id = stamped_id(0x11);
        seed_session(
            &store,
            &id,
            Some("a"),
            1,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        seed_session(
            &store,
            &stamped_id(0x22),
            Some("b"),
            1,
            datetime!(2026-04-18 09:00:01 UTC),
        );
        assert_eq!(
            resolve_prefix(&store, &id, "other").unwrap(),
            store.find_session_path(&id).unwrap(),
        );
    }

    #[test]
    fn resolve_prefix_ambiguous_lists_short_ids_in_error() {
        let (_dir, store) = isolated_store();
        // Same first 2 hex digits → both ids start with `aaaa`.
        let id_a = format!("aaaa{}", &stamped_id(0xaa)[4..]);
        let id_b = format!("aaaa{}", &stamped_id(0xab)[4..]);
        seed_session(
            &store,
            &id_a,
            Some("a"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        seed_session(
            &store,
            &id_b,
            Some("b"),
            3,
            datetime!(2026-04-18 09:05:00 UTC),
        );
        let err = resolve_prefix(&store, "aaaa", "other").unwrap_err();
        assert!(err.contains("ambiguous prefix"), "{err}");
        assert!(err.contains(&id_a[..8]), "expected id_a in {err}");
        assert!(err.contains(&id_b[..8]), "expected id_b in {err}");
    }
}
