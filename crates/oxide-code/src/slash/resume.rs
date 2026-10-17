//! `/resume` (alias `/continue`) opens a [`SearchableList`] session picker, and
//! `/resume <id-prefix>` resumes directly. `ox --resume` opens the same picker at startup.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use time::UtcOffset;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use super::sessions::open_store;
use crate::command;
use crate::protocol::{Op, ResumeTarget};
use crate::session::display::{display_title, format_metadata_line, id_prefix};
use crate::session::entry::SessionInfo;
use crate::session::saved;
use crate::session::store::SessionStore;
use crate::session::store::listing::{DEFAULT_SESSION_LIST_LIMIT, ListPage, ListScope};
use crate::tui::action::Action;
use crate::tui::glyphs::MODAL_ERROR_PREFIX;
use crate::tui::modal::chrome::{FOOTER_HEIGHT, render_footer, render_footer_text};
use crate::tui::modal::keymap::{KeyAction, KeyChord, Keymap};
use crate::tui::modal::searchable_list::{SearchableItem, SearchableList};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::path::tildify;
use crate::util::text::truncate_to_width;

// ── Constants ──

const PICKER_TITLE: &str = "Resume session";
const PICKER_DESCRIPTION: &str = "Pick a session to resume in place";
const ARCHIVED_PICKER_TITLE: &str = "Resume archived session";
const ARCHIVED_PICKER_DESCRIPTION: &str = "Pick a session to unarchive and resume in place";
const VIEWPORT_HEIGHT: u16 = 6;
/// Each row paints a title, a metadata line, and a blank line that separates sessions.
const ROW_HEIGHT: u16 = 3;
/// Floor on the title column so narrow terminals still show a truncated label.
const TITLE_FLOOR: usize = 8;

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
    last_active_at: time::OffsetDateTime,
    local_offset: UtcOffset,
    title: String,
    /// `0` when no `Summary` line was found, as in a session that never finalized.
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

// ── ResumePicker ──

/// What Esc does to the picker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dismissal {
    /// Closes the picker and keeps the live session.
    Cancel,
    /// Quits `ox`, since the picker stands in for the session at startup.
    Quit,
}

/// Outcome of the last row action, shown in place of the footer hints until the next key.
enum Notice {
    Info(String),
    Error(String),
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
            notice: None,
        };
        picker.reload();
        picker
    }

    /// Reloads rows with the cursor and scroll back at the top, for a new scope or view.
    fn reload(&mut self) {
        let rows = self.load_rows();
        self.list.replace_items(rows);
    }

    /// Reloads rows keeping the cursor's index and the scroll position, so the cursor lands on the
    /// row after one that left the view.
    fn refresh(&mut self) {
        let rows = self.load_rows();
        self.list.refresh_items(rows);
    }

    fn load_rows(&mut self) -> Vec<SessionRow> {
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
                self.load_error = Some(format!("Failed to load sessions: {err:#}"));
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
        rows
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
        self.notice = Some(match result {
            Ok(()) => {
                let verb = if archived { "Archived" } else { "Unarchived" };
                let note = format!(
                    "{verb} session {}: {}",
                    id_prefix(&row.session_id),
                    row.title
                );
                self.refresh();
                Notice::Info(note)
            }
            Err(err) => Notice::Error(err),
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

    fn footer_hints(&self) -> Vec<String> {
        let scope = if self.all {
            "all projects"
        } else {
            "current project"
        };
        let kind = if self.archived { "archived " } else { "" };
        let loaded = self.loaded;
        let matched = self.list.visible_len();
        let count = match (self.list.is_filtered(), self.more) {
            (true, true) => format!("{matched} matching in the {loaded} most recent"),
            (true, false) => format!("{matched} / {loaded} matching"),
            (false, true) => format!("{loaded} most recent {kind}sessions"),
            (false, false) => {
                format!(
                    "{loaded} {kind}session{}",
                    if loaded == 1 { "" } else { "s" }
                )
            }
        };

        let mut status = vec![count];
        if self.more {
            status.push("more not loaded".to_owned());
        }
        status.push(format!("scope: {scope}"));

        // Status follows the Esc hint, ahead of the other keys, so a medium-width footer keeps it.
        let mut hints = Vec::new();
        for (action, hint) in ACTIONS.hints(self) {
            hints.push(hint);
            if action == PickerAction::Dismiss {
                hints.append(&mut status);
            }
        }
        hints
    }

    fn has_selection(&self) -> bool {
        self.list.selected().is_some()
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
        match (&self.load_error, &self.notice) {
            (Some(err), _) | (None, Some(Notice::Error(err))) => render_footer_text(
                frame,
                area,
                &format!("{MODAL_ERROR_PREFIX}{err}"),
                theme.error(),
                theme,
            ),
            (None, Some(Notice::Info(note))) => {
                render_footer_text(frame, area, note, theme.success(), theme);
            }
            (None, None) => render_footer(frame, area, &self.footer_hints(), theme),
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        self.notice = None;
        if let Some(action) = ACTIONS.action_for(event) {
            return self.run(action);
        }
        match event.code {
            KeyCode::Up => self.list.select_prev(),
            KeyCode::Down => self.list.select_next(),
            KeyCode::PageUp => self.list.page_up(),
            KeyCode::PageDown => self.list.page_down(),
            _ => self.list.edit_query(event),
        }
        ModalKey::Consumed
    }

    fn handle_paste(&mut self, text: &str) {
        self.notice = None;
        self.list.paste_query(text);
    }

    fn on_focus_regained(&mut self) {
        // Re-seek the cursor after the refresh so cancel-delete keeps the user on the same row.
        let prev_id = self.list.selected().map(|r| r.session_id.clone());
        self.refresh();
        if let Some(id) = prev_id {
            self.list.cursor_to(|row| row.session_id == id);
        }
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        (self.dismissal == Dismissal::Quit)
            .then_some(ModalAction::Dispatch(Action::Op(Op::Shutdown)))
    }

    /// Esc dispatches through [`ACTIONS`], so its key and hint stay in one table.
    fn captures_esc(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use time::OffsetDateTime;
    use time::macros::datetime;

    use super::*;
    use crate::session::display::UNTITLED_MARKER;
    use crate::session::entry::TitleInfo;
    use crate::session::store::set_archived_at;
    use crate::session::store::testing::{listed_ids, seed_test_session, test_store};
    use crate::slash::{stamped_id, test_session_info, with_isolated_xdg};
    use crate::tui::modal::chrome::HINT_SEPARATOR;
    use crate::tui::modal::testing::render_rows;

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

    fn footer_row(picker: &ResumePicker, width: u16) -> String {
        let rows = render_rows(picker, width, picker.height(width, 40));
        rows.last().unwrap().trim_end().to_owned()
    }

    // ── ResumeCmd::name ──

    #[test]
    fn name_and_metadata_match_built_ins_contract() {
        assert_eq!(ResumeCmd.name(), "resume");
        assert_eq!(ResumeCmd.aliases(), &["continue"]);
        assert_ne!(ResumeCmd.description(), "");
        assert_eq!(ResumeCmd.usage(), Some("[<id-prefix>]"));
    }

    // ── ResumeCmd::classify ──

    #[test]
    fn classify_is_always_mutating() {
        // Both forms reach the engine (typed-arg → Forward, picker → modal-submitted Resume),
        // and mid-turn ops get dropped, so neither may run while busy.
        assert_eq!(ResumeCmd.classify(""), SlashKind::Mutating);
        assert_eq!(ResumeCmd.classify("   "), SlashKind::Mutating);
        assert_eq!(ResumeCmd.classify("ab"), SlashKind::Mutating);
    }

    // ── ResumeCmd::echoes_input ──

    #[test]
    fn echoes_input_only_when_arg_present() {
        assert!(!ResumeCmd.echoes_input(""));
        assert!(ResumeCmd.echoes_input("ab"));
    }

    // ── ResumeCmd::execute ──

    #[test]
    fn execute_bare_opens_the_picker() {
        with_isolated_xdg(|_| {
            let info = test_session_info();
            let outcome = ResumeCmd.execute("", &SlashContext::new(&info, None));
            outcome.unwrap().into_modal();
        });
    }

    #[test]
    fn execute_typed_arg_forwards_the_prefix_for_the_engine_to_resolve() {
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

    // ── SessionRow::from_info ──

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
        // Every row in the current scope shares the project, so showing it would add noise and
        // let the substring filter match on it.
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

    // ── SessionRow::render ──

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
        // The full picker row: id · time · N msgs · branch · project, in both `1 msg` and
        // `5 msgs` forms.
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
        // `exit: None` means the file has no Summary line, so the row shows no message count.
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

    // ── ACTIONS ──

    #[test]
    fn actions_dispatch_each_key_to_its_own_action() {
        for entry in ACTIONS.actions() {
            assert_eq!(
                ACTIONS.action_for(&entry.key.event()),
                Some(entry.action),
                "{} is bound to an earlier action",
                entry.key,
            );
        }
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
            picker.footer_hints(),
            [
                "Enter to resume",
                "Esc to quit",
                "1 session",
                "scope: all projects",
                "Ctrl+X to archive",
                "Ctrl+D to delete",
                "Tab for current project",
                "Ctrl+A for archived",
            ],
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
        // `load_error` lets the footer tell a failed load apart from "0 sessions".
        let (dir, store) = isolated_store();
        let project_dir = crate::session::store::testing::test_project_dir(dir.path());
        std::fs::remove_dir_all(&project_dir).unwrap();
        let cause = store
            .list_paged(
                Some(DEFAULT_SESSION_LIST_LIMIT),
                ListScope {
                    all: false,
                    archived: false,
                },
            )
            .unwrap_err();

        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert_eq!(
            picker.load_error,
            Some(format!("Failed to load sessions: {cause:#}")),
        );
        assert_eq!(picker.loaded, 0, "no rows materialize on the Err arm");
    }

    // ── ResumePicker::submit ──

    #[test]
    fn submit_enter_with_selection_emits_resume_action() {
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
    fn submit_enter_with_no_rows_keeps_picker_open() {
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(matches!(
            picker.handle_key(&key(KeyCode::Enter)),
            ModalKey::Consumed,
        ));
    }

    // ── ResumePicker::footer_hints ──

    #[test]
    fn footer_hints_follow_esc_with_the_status_and_hide_row_actions_without_rows() {
        // Three permutations: 1 + current project (singular + scoped), 0 + all projects after
        // Tab (zero plural + widened), 2 + current project (plural + scoped).
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
            picker.footer_hints(),
            [
                "Enter to resume",
                "Esc to cancel",
                "1 session",
                "scope: current project",
                "Ctrl+X to archive",
                "Ctrl+D to delete",
                "Tab for all projects",
                "Ctrl+A for archived",
            ],
        );

        let (_dir2, empty_store) = isolated_store();
        let mut empty = ResumePicker::new(empty_store, "live-session-id".to_owned());
        empty.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            empty.footer_hints(),
            [
                "Esc to cancel",
                "0 sessions",
                "scope: all projects",
                "Tab for current project",
                "Ctrl+A for archived",
            ],
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
        assert_eq!(
            picker_two.footer_hints()[2..4],
            ["2 sessions", "scope: current project"]
        );
    }

    #[test]
    fn footer_hints_show_matches_over_loaded_rows_when_query_active() {
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
        let hints = picker.footer_hints();
        assert_eq!(
            hints[2..4],
            ["1 / 3 matching", "scope: current project"],
            "filter `fix` should narrow to one title but keep the loaded count visible",
        );
        assert_eq!(hints.last().unwrap(), "Ctrl+U to clear filter");
    }

    #[test]
    fn footer_hints_name_the_load_cap_when_sessions_exceed_it() {
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
        assert_eq!(
            picker.footer_hints()[2..5],
            [
                "29 most recent sessions",
                "more not loaded",
                "scope: current project",
            ],
            "the live session is not a loaded row",
        );

        for c in "oldest".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(
            picker.footer_hints(),
            [
                "Esc to cancel",
                "0 matching in the 29 most recent",
                "more not loaded",
                "scope: current project",
                "Tab for all projects",
                "Ctrl+A for archived",
                "Ctrl+U to clear filter",
            ],
            "the filter only searches loaded rows",
        );
    }

    // ── ResumePicker::render ──

    #[test]
    fn render_paints_every_hint_on_a_wide_footer() {
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("Auth"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let picker = ResumePicker::new(store, "live-session-id".to_owned());

        let footer = footer_row(&picker, 180);

        assert_eq!(
            footer,
            "Enter to resume  ·  Esc to cancel  ·  1 session  ·  scope: current project  ·  \
             Ctrl+X to archive  ·  Ctrl+D to delete  ·  Tab for all projects  ·  \
             Ctrl+A for archived",
        );
    }

    #[test]
    fn render_keeps_the_status_beside_esc_on_medium_footers() {
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

        let medium = footer_row(&picker, 80);
        let wide = footer_row(&picker, 120);

        assert_eq!(
            medium,
            "Enter to resume  ·  Esc to cancel  ·  30 most recent sessions  ·  more not lo...",
        );
        assert_eq!(
            wide,
            "Enter to resume  ·  Esc to cancel  ·  30 most recent sessions  ·  more not loaded  ·  \
             scope: current project  ·  Ctrl...",
        );
    }

    #[test]
    fn render_skips_the_footer_in_an_area_too_short_for_it() {
        let (_dir, store) = isolated_store();
        let picker = ResumePicker::new(store, "live-session-id".to_owned());

        let rows = render_rows(&picker, 60, 1);

        assert_eq!(rows, [" ".repeat(60)]);
    }

    #[test]
    fn render_keeps_the_resume_and_cancel_hints_on_a_narrow_footer() {
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("Auth"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        let rows = render_rows(&picker, 40, picker.height(40, 40));
        assert_eq!(
            rows.last().unwrap(),
            "Enter to resume  ·  Esc to cancel  · ..."
        );
    }

    #[test]
    fn render_surfaces_load_error_in_footer() {
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        picker.load_error = Some("permission denied".to_owned());

        let rows = render_rows(&picker, 60, picker.height(60, 40));
        assert_eq!(rows.last().unwrap().trim_end(), "! permission denied");
    }

    // ── ResumePicker::handle_key ──

    #[test]
    fn handle_key_typing_or_pasting_filters_rows_via_searchable_list() {
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
    fn handle_key_backspace_pops_one_char_from_query() {
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
    fn handle_key_ctrl_u_clears_query() {
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
        let event = KeyChord::ctrl('u').event();
        picker.handle_key(&event);
        assert_eq!(picker.list.query(), "");
        assert_eq!(picker.list.visible_len(), 1);
    }

    #[test]
    fn handle_key_navigation_keys_move_cursor_within_visible_set() {
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
        render_rows(&picker, 60, 30);
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
    fn handle_key_tab_toggles_scope_and_reloads_rows() {
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
        assert_eq!(picker.footer_hints()[3], "scope: all projects");
        picker.handle_key(&key(KeyCode::Tab));
        assert!(!picker.all, "second Tab flips back");
    }

    #[test]
    fn handle_key_tab_widens_scope_to_other_project_sessions_and_preserves_query() {
        // Tab widens the scoped picker and keeps the typed filter, while the live session stays
        // out of the widened rows so the user can't resume it.
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
    fn handle_key_ctrl_a_toggles_between_active_and_archived_sessions() {
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
        let ctrl_a = KeyChord::ctrl('a').event();
        assert_eq!(picker.loaded, 1);
        assert_eq!(selected_id(&picker), active_id);

        picker.handle_key(&ctrl_a);
        assert_eq!(picker.loaded, 1);
        assert_eq!(selected_id(&picker), archived_id);
        assert_eq!(
            picker.footer_hints()[2..],
            [
                "1 archived session",
                "scope: current project",
                "Ctrl+X to unarchive",
                "Ctrl+D to delete",
                "Tab for all projects",
                "Ctrl+A for active",
            ],
        );
        let rows = render_rows(&picker, 60, 20);
        let title = &rows[0];
        assert_eq!(title.trim_end(), ARCHIVED_PICKER_TITLE);
        let outcome = picker.handle_key(&key(KeyCode::Enter));
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
        assert_eq!(selected_id(&picker), active_id);
    }

    #[test]
    fn handle_key_unrecognized_keys_are_consumed_without_side_effects() {
        // An unhandled bare key such as Insert and an unbound Ctrl chord are both absorbed, with
        // neither editing the filter or moving the cursor.
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

        let event = KeyChord::ctrl('g').event();
        picker.handle_key(&event);
        assert_eq!(picker.list.query(), "");
    }

    #[test]
    fn handle_key_ctrl_d_pushes_the_confirm_modal_for_the_cursor_row_while_delete_edits_the_query()
    {
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

        let ctrl_d = KeyChord::ctrl('d').event();
        let outcome = picker.handle_key(&ctrl_d);

        let ModalKey::Push(confirm) = outcome else {
            panic!("Ctrl+D must push the confirm modal, got {outcome:?}");
        };
        let rows = render_rows(confirm.as_ref(), 60, confirm.height(60, u16::MAX));
        assert_eq!(rows[2].trim_end(), "11111111 — Fix auth");
    }

    #[test]
    fn handle_key_ctrl_d_with_no_rows_consumes_silently_instead_of_pushing() {
        // No selection → no row identity to confirm against. Better to consume than to surface a
        // confused-looking "delete (nothing)?" modal.
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());

        let ctrl_d = KeyChord::ctrl('d').event();
        let outcome = picker.handle_key(&ctrl_d);
        assert!(matches!(outcome, ModalKey::Consumed));
    }

    #[test]
    fn handle_key_ctrl_d_then_y_deletes_the_cursor_session_through_the_modal_stack() {
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
        let ctrl_d = KeyChord::ctrl('d').event();

        let pushed = stack.handle_key(&ctrl_d);
        let deleted = stack.handle_key(&key(KeyCode::Char('y')));

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
    fn handle_key_ctrl_x_archives_the_cursor_row_and_keeps_the_filter_and_scope() {
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
        let mut picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        picker.handle_key(&key(KeyCode::Tab));
        for c in "fix".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        picker.handle_key(&key(KeyCode::Down));

        let outcome = picker.handle_key(&KeyChord::ctrl('x').event());

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

        picker.handle_key(&key(KeyCode::Up));
        assert_eq!(
            footer_row(&picker, 80),
            "Enter to resume  ·  Esc to cancel  ·  2 / 3 matching  ·  scope: all projects ...",
            "the next key brings the hints back",
        );
    }

    #[test]
    fn handle_key_ctrl_x_keeps_the_visible_rows_in_place() {
        let (_dir, store) = isolated_store();
        for i in 0..12_u8 {
            seed_session(
                &store,
                &stamped_id(i + 1),
                Some(&format!("row-{i}")),
                1,
                datetime!(2026-04-18 09:00:00 UTC) + time::Duration::seconds(i64::from(i)),
            );
        }
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let first_shown = |picker: &ResumePicker| -> String {
            let rows = render_rows(picker, 60, picker.height(60, 40));
            rows[5].trim_end().to_owned()
        };
        for _ in 0..9 {
            picker.handle_key(&key(KeyCode::Down));
        }
        first_shown(&picker);
        picker.handle_key(&key(KeyCode::Up));
        picker.handle_key(&key(KeyCode::Up));
        assert_eq!(first_shown(&picker), "  row-7");

        picker.handle_key(&KeyChord::ctrl('x').event());

        assert_eq!(
            first_shown(&picker),
            "  row-7",
            "the window keeps its first row"
        );
        assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(4));
    }

    #[test]
    fn handle_key_ctrl_x_in_the_archived_view_unarchives_the_last_row_and_lands_above_it() {
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
            set_archived_at(&store.find_session_path(&id).unwrap(), true).unwrap();
        }
        let mut picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        picker.handle_key(&KeyChord::ctrl('a').event());
        picker.handle_key(&key(KeyCode::Down));

        picker.handle_key(&KeyChord::ctrl('x').event());

        assert_eq!(listed_ids(&store, false), [stamped_id(0x11)]);
        assert_eq!(
            footer_row(&picker, 80),
            "Unarchived session 11111111: first"
        );
        assert_eq!(picker.list.selected().unwrap().session_id, stamped_id(0x22));
    }

    #[test]
    fn handle_key_ctrl_x_on_the_live_session_points_to_bare_archive_and_keeps_the_row() {
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
        let mut picker = ResumePicker::new(store.clone(), "live-session-id".to_owned());
        picker.live_session_id.clone_from(&id);

        picker.handle_key(&KeyChord::ctrl('x').event());

        assert_eq!(
            footer_row(&picker, 120),
            "! Session 11111111 is the live session. \
             Use bare `/archive` to archive it and start a new one.",
        );
        assert_eq!(listed_ids(&store, false), [id.as_str()]);
        assert_eq!(picker.loaded, 1);
    }

    #[cfg(unix)]
    #[test]
    fn handle_key_ctrl_x_shows_a_failed_write_and_keeps_the_row() {
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
        let cause = set_archived_at(&path, true).unwrap_err();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());

        picker.handle_key(&KeyChord::ctrl('x').event());

        assert_eq!(
            footer_row(&picker, 400),
            format!("! Failed to update session 11111111: {cause:#}"),
        );
        assert_eq!(picker.list.selected().unwrap().session_id, id);
    }

    #[test]
    fn handle_key_ctrl_x_never_cuts_from_the_filter() {
        // The filter's editor binds Ctrl+X to cutting the selection, which the picker shadows.
        let (_dir, store) = isolated_store();
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        for c in "ab".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        picker.handle_key(&KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));

        let outcome = picker.handle_key(&KeyChord::ctrl('x').event());

        assert!(matches!(outcome, ModalKey::Consumed), "{outcome:?}");
        assert_eq!(picker.list.query(), "ab");
        assert!(picker.notice.is_none(), "no row, so nothing to archive");
    }

    // ── ResumePicker::handle_paste ──

    #[test]
    fn handle_paste_clears_the_notice() {
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        picker.handle_key(&KeyChord::ctrl('x').event());
        assert_eq!(footer_row(&picker, 80), "Archived session 22222222: second");

        picker.handle_paste("fir");

        assert_eq!(
            footer_row(&picker, 200),
            picker.footer_hints().join(HINT_SEPARATOR),
            "the paste brings the hints back",
        );
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
        // Cancelling a delete leaves the user on the row they were inspecting, since the reload
        // re-seeks after `replace_items` resets the cursor to row 0.
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
    fn on_focus_regained_moves_to_the_next_row_when_the_selected_row_was_deleted() {
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
        assert_eq!(
            picker.list.selected().unwrap().session_id,
            stamped_id(0x11),
            "the row that followed takes the highlight",
        );
    }

    // ── ResumePicker::on_cancel ──

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
}
