//! `/resume` (alias `/continue`) — open the session picker, or resume directly with
//! `/resume <id-prefix>`. The picker uses [`SearchableList`] so the same chrome powers any
//! future searchable modal.

use std::borrow::Cow;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use time::UtcOffset;

use super::confirm::ConfirmDeleteSessionModal;
use super::context::SlashContext;
use super::registry::{SlashCommand, SlashKind, SlashOutcome};
use crate::agent::event::UserAction;
use crate::session::entry::SessionInfo;
use crate::session::store::SessionStore;
use crate::tui::modal::searchable_list::{SearchableItem, SearchableList};
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::path::tildify;
use crate::util::text::truncate_to_width;

// ── Constants ──

const PICKER_TITLE: &str = "Resume session";
const PICKER_DESCRIPTION: &str = "Pick a session to resume in place.";
const VIEWPORT_HEIGHT: u16 = 6;
const UNTITLED_MARKER: &str = "(untitled)";
const ID_WIDTH: usize = 8;
/// Each row paints title + metadata + a trailing blank for breathing room between sessions.
const ROW_HEIGHT: u16 = 3;
/// Floor on the title column so narrow terminals still show a truncated label.
const TITLE_FLOOR: usize = 8;
/// Visual separator between metadata segments.
const META_SEPARATOR: &str = " · ";

// ── SessionRow ──

/// Row payload for the resume picker — display strings + a search haystack.
struct SessionRow {
    session_id: String,
    last_active_at: time::OffsetDateTime,
    local_offset: UtcOffset,
    title: String,
    /// `0` when no `Summary` line was found (older sessions or sessions that never finalized).
    /// Rendered as "N msgs" so the user can see session weight at a glance.
    message_count: u32,
    git_branch: Option<String>,
    /// `Some` only when the picker scope is widened to all projects — the metadata column then
    /// surfaces the project path so the user can disambiguate. Scoped picks already share a
    /// project, so painting it would be noise.
    project: Option<String>,
    haystack: String,
}

impl SessionRow {
    fn from_info(info: SessionInfo, local_offset: UtcOffset, show_project: bool) -> Self {
        let title = info
            .title
            .as_ref()
            .map_or_else(|| UNTITLED_MARKER.to_owned(), |t| t.title.clone());
        let project_path = tildify(Path::new(&info.cwd));
        // Project name participates in search only when the user can see it; in scoped mode every
        // row shares the same project, so substring-matching against it just confuses the filter.
        let haystack = if show_project {
            format!("{} {} {}", info.session_id, title, project_path)
        } else {
            format!("{} {}", info.session_id, title)
        };
        Self {
            session_id: info.session_id,
            last_active_at: info.last_active_at,
            local_offset,
            title,
            message_count: info.exit.as_ref().map_or(0, |e| e.message_count),
            git_branch: info.git_branch,
            project: show_project.then_some(project_path),
            haystack,
        }
    }

    fn id_prefix(&self) -> &str {
        self.session_id.get(..ID_WIDTH).unwrap_or(&self.session_id)
    }

    /// Single-line metadata summary used by both the picker row and the delete-confirm modal.
    /// Shape: `{id_prefix} · {when} · {N msgs} · {branch} · {project}`. Empty fields are omitted.
    fn metadata_line(&self) -> String {
        use std::fmt::Write as _;
        let now = time::OffsetDateTime::now_utc().to_offset(self.local_offset);
        let when = format_relative_time(self.last_active_at.to_offset(self.local_offset), now);
        let mut meta = format!("{} · {when}", self.id_prefix());
        if self.message_count > 0 {
            let unit = if self.message_count == 1 {
                "msg"
            } else {
                "msgs"
            };
            _ = write!(meta, "{META_SEPARATOR}{} {unit}", self.message_count);
        }
        if let Some(branch) = self.git_branch.as_deref() {
            meta.push_str(META_SEPARATOR);
            meta.push_str(branch);
        }
        if let Some(project) = self.project.as_deref() {
            meta.push_str(META_SEPARATOR);
            meta.push_str(project);
        }
        meta
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

/// Coarse-grain "N seconds/minutes/hours/days ago"; falls back to ISO date past 30 days so "327
/// days ago" doesn't displace a more recognizable absolute reference. Negative deltas (clock skew,
/// future stamps from another machine) collapse to 0 to keep the singular/plural axis sane.
fn format_relative_time(ts: time::OffsetDateTime, now: time::OffsetDateTime) -> String {
    let secs = (now - ts).whole_seconds().max(0);
    if secs < 60 {
        return format!("{secs} {} ago", pluralize(secs, "second"));
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} {} ago", pluralize(mins, "minute"));
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours} {} ago", pluralize(hours, "hour"));
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} {} ago", pluralize(days, "day"));
    }
    ts.format(time::macros::format_description!("[year]-[month]-[day]"))
        .expect("static `[year]-[month]-[day]` description never fails on a valid `OffsetDateTime`")
}

fn pluralize(n: i64, unit: &str) -> String {
    if n == 1 {
        unit.to_owned()
    } else {
        format!("{unit}s")
    }
}

// ── ResumePicker ──

pub(super) struct ResumePicker {
    store: SessionStore,
    list: SearchableList<SessionRow>,
    /// `false` = current project, `true` = every project.
    all: bool,
    local_offset: UtcOffset,
    /// Filtered out of every reload so the user can't self-resume onto the open append-writer.
    live_session_id: String,
    total: usize,
    /// Last reload's failure, surfaced inline so failures don't disguise themselves as "no
    /// sessions found". Cleared on the next successful reload.
    load_error: Option<String>,
}

impl ResumePicker {
    pub(super) fn new(store: SessionStore, live_session_id: String) -> Self {
        let local_offset = crate::util::time::local_offset();
        let mut picker = Self {
            store,
            list: SearchableList::new(PICKER_TITLE, Vec::new(), VIEWPORT_HEIGHT)
                .with_description(PICKER_DESCRIPTION),
            all: false,
            local_offset,
            live_session_id,
            total: 0,
            load_error: None,
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        let page = match self.store.list_paged(None, self.all) {
            Ok(p) => {
                self.load_error = None;
                p
            }
            Err(err) => {
                tracing::warn!("resume picker: list_paged failed: {err:#}");
                self.load_error = Some(format!("failed to load sessions: {err:#}"));
                crate::session::store::ListPage::default()
            }
        };
        let local_offset = self.local_offset;
        let live_id = self.live_session_id.as_str();
        let show_project = self.all;
        let rows: Vec<SessionRow> = page
            .into_sessions()
            .into_iter()
            .filter(|info| info.session_id != live_id)
            .map(|info| SessionRow::from_info(info, local_offset, show_project))
            .collect();
        self.total = rows.len();
        self.list.replace_items(rows);
    }

    fn submit(&self) -> ModalKey {
        match self.list.selected() {
            Some(row) => ModalKey::Submitted(ModalAction::User(UserAction::Resume {
                session_id: row.session_id.clone(),
            })),
            // Stay open so the user can Tab the scope or Esc out — silent dismissal hides why
            // nothing happened.
            None => ModalKey::Consumed,
        }
    }

    /// Builds and pushes the [`ConfirmDeleteSessionModal`] for the cursor row. No-op (key
    /// consumed silently) when the list is empty, so an accidental Ctrl+D / Delete with no
    /// row selected doesn't surface anything misleading.
    fn start_delete_confirm(&self) -> ModalKey {
        let Some(row) = self.list.selected() else {
            return ModalKey::Consumed;
        };
        ModalKey::Push(Box::new(ConfirmDeleteSessionModal::new(
            self.store.clone(),
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
        let count = if self.list.is_filtered() {
            format!(
                "{matched} / {total} matching",
                matched = self.list.visible_len(),
                total = self.total,
            )
        } else {
            format!(
                "{total} session{plural}",
                total = self.total,
                plural = if self.total == 1 { "" } else { "s" },
            )
        };
        format!(
            "{count} · scope: {scope} · Tab to toggle · Enter to resume · Ctrl+D to delete · \
             Esc to cancel"
        )
    }
}

impl Modal for ResumePicker {
    fn height(&self, width: u16) -> u16 {
        self.list.height(width).saturating_add(2)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let list_h = self.list.height(area.width);
        let list_area = Rect {
            height: list_h.min(area.height),
            ..area
        };
        self.list.render(frame, list_area, theme);

        let remaining = area.height.saturating_sub(list_h);
        if remaining < 2 {
            return;
        }
        let footer_area = Rect {
            x: area.x,
            y: area.y.saturating_add(list_h).saturating_add(1),
            width: area.width,
            height: 1,
        };
        // Load error owns the footer when set — failure must not look like "0 sessions".
        let footer = if let Some(err) = &self.load_error {
            Line::from(Span::styled(format!("! {err}"), theme.error()))
        } else {
            Line::from(Span::styled(self.footer_text(), theme.dim()))
        };
        frame.render_widget(Paragraph::new(footer).style(theme.surface()), footer_area);
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
            KeyCode::Backspace => {
                self.list.pop_char();
                ModalKey::Consumed
            }
            // Both gestures open the same confirm — the footer hint advertises Ctrl+D, but the
            // dedicated Delete key (when the keyboard has one) is convenient muscle memory.
            KeyCode::Delete => self.start_delete_confirm(),
            KeyCode::Char('d') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_delete_confirm()
            }
            KeyCode::Char(c) if event.modifiers.contains(KeyModifiers::CONTROL) => {
                if c == 'u' {
                    self.list.set_query(String::new());
                }
                ModalKey::Consumed
            }
            KeyCode::Char(c) => {
                self.list.push_char(c);
                ModalKey::Consumed
            }
            _ => ModalKey::Consumed,
        }
    }

    fn on_focus_regained(&mut self) {
        // Re-seek the cursor after reload so cancel-delete keeps the user on the same row.
        let prev_id = self.list.selected().map(|r| r.session_id.clone());
        self.reload();
        if let Some(id) = prev_id {
            self.list.cursor_to(|row| row.session_id == id);
        }
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
        // Mutating in both forms: bare opens a picker that submits a Resume action; typed-arg
        // forwards Resume directly. The agent loop drops mid-turn user actions, so even the
        // picker form must wait for idle.
        SlashKind::Mutating
    }

    fn echoes_input(&self, args: &str) -> bool {
        !args.trim().is_empty()
    }

    fn usage(&self) -> Option<&'static str> {
        Some("[<id-prefix>]")
    }

    fn execute(&self, args: &str, ctx: &mut SlashContext<'_>) -> Result<SlashOutcome, String> {
        let arg = args.trim();
        let store =
            SessionStore::open().map_err(|e| format!("session store unavailable: {e:#}"))?;
        if arg.is_empty() {
            ctx.open_modal(Box::new(ResumePicker::new(
                store,
                ctx.info.session_id.clone(),
            )));
            return Ok(SlashOutcome::Done);
        }
        let session_id = resolve_prefix(&store, arg, ctx.info.session_id.as_str())?;
        Ok(SlashOutcome::Forward(UserAction::Resume { session_id }))
    }
}

/// Match `prefix` against current-project sessions first; widen to all projects on no match.
/// Excludes the live session id — resuming yourself would race the open append-writer.
fn resolve_prefix(store: &SessionStore, prefix: &str, live_id: &str) -> Result<String, String> {
    let scoped = match_in_scope(store, prefix, live_id, false)?;
    if let Some(id) = scoped {
        return Ok(id);
    }
    let widened = match_in_scope(store, prefix, live_id, true)?;
    widened.ok_or_else(|| format!("no session matching `{prefix}`"))
}

fn match_in_scope(
    store: &SessionStore,
    prefix: &str,
    live_id: &str,
    all: bool,
) -> Result<Option<String>, String> {
    let page = store
        .list_paged(None, all)
        .map_err(|e| format!("list sessions: {e:#}"))?;
    let mut matches: Vec<String> = page
        .into_sessions()
        .into_iter()
        .map(|s| s.session_id)
        .filter(|id| id != live_id && id.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        n => {
            // Reuse the CLI's preview formatter so /resume and `ox -c` share the same message.
            let preview = crate::session::resolver::format_session_id_preview(matches);
            Err(format!(
                "ambiguous prefix `{prefix}` matches {n} sessions: {preview}",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use temp_env::with_var;
    use time::OffsetDateTime;
    use time::macros::datetime;

    use super::*;
    use crate::session::entry::TitleInfo;
    use crate::session::store::{seed_test_session, test_store};
    use crate::slash::test_session_info;
    use crate::tui::components::chat::ChatView;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn stamped_id(byte: u8) -> String {
        let s = format!("{byte:02x}");
        // 36-char UUID-ish: `aabb1111-2222-3333-4444-555566667777`. 32 hex digits + 4 dashes.
        format!(
            "{s}{s}1111-2222-3333-4444-{s}{s}{s}{s}{s}{s}",
            s = s.repeat(2),
        )
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

    fn with_isolated_xdg<R>(f: impl FnOnce(&Path) -> R) -> R {
        let dir = tempfile::tempdir().unwrap();
        with_var("XDG_DATA_HOME", Some(dir.path().as_os_str()), || {
            f(dir.path())
        })
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
            cwd: cwd.to_owned(),
            last_active_at,
            title: title.map(|t| TitleInfo {
                title: t.to_owned(),
                updated_at: last_active_at,
            }),
            exit: None,
            git_branch: None,
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
        assert_eq!(row.id_prefix().len(), ID_WIDTH);
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
        assert!(meta_text.contains(row.id_prefix()), "meta row: {meta_text}");
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

    // ── format_relative_time ──

    #[test]
    fn format_relative_time_pluralizes_units_and_falls_back_to_iso_date_past_30_days() {
        let now = datetime!(2026-05-08 12:00:00 UTC);
        // Singular at the 1-of-each boundary, plural everywhere else.
        assert_eq!(
            format_relative_time(now - time::Duration::seconds(1), now),
            "1 second ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::seconds(3), now),
            "3 seconds ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::minutes(1), now),
            "1 minute ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::minutes(2), now),
            "2 minutes ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::hours(1), now),
            "1 hour ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::hours(5), now),
            "5 hours ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::days(1), now),
            "1 day ago",
        );
        assert_eq!(
            format_relative_time(now - time::Duration::days(3), now),
            "3 days ago",
        );
        // 30+ days falls back to the absolute ISO date.
        assert_eq!(
            format_relative_time(now - time::Duration::days(60), now),
            "2026-03-09",
        );
        // Negative delta (future stamp / clock skew) collapses to "0 seconds ago" rather than
        // emitting "-30 seconds ago".
        assert_eq!(
            format_relative_time(now + time::Duration::seconds(30), now),
            "0 seconds ago",
        );
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
            picker.total, 2,
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
        assert_eq!(picker.total, 1, "live session must be filtered out");
        let visible_id = picker
            .list
            .selected()
            .expect("one row remaining")
            .session_id
            .clone();
        assert_ne!(visible_id, live_id);
    }

    // ── ResumePicker::reload ──

    #[test]
    fn reload_sets_load_error_and_clears_rows_when_list_paged_fails() {
        // Pin the Err arm of `reload`: removing the project dir makes `list_paged` fail with
        // ENOENT on `read_dir`. The picker must surface that as `load_error` (so the footer can
        // distinguish "0 sessions" from "load failed") and zero out `total`.
        let (dir, store) = isolated_store();
        let project_dir = crate::session::store::test_project_dir(dir.path());
        std::fs::remove_dir_all(&project_dir).unwrap();

        let picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert!(
            picker.load_error.is_some(),
            "reload should surface list_paged failure: {:?}",
            picker.load_error,
        );
        assert_eq!(picker.total, 0, "no rows materialise on the Err arm");
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
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        let outcome = picker.handle_key(&key(KeyCode::Enter));
        let ModalKey::Submitted(ModalAction::User(UserAction::Resume { session_id })) = outcome
        else {
            panic!("expected Submitted(Resume), got {outcome:?}");
        };
        assert_eq!(session_id, stamped_id(0x11));
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
            "1 session · scope: current project · Tab to toggle · Enter to resume · \
             Ctrl+D to delete · Esc to cancel",
        );

        let (_dir2, empty_store) = isolated_store();
        let mut empty = ResumePicker::new(empty_store, "live-session-id".to_owned());
        empty.handle_key(&key(KeyCode::Tab));
        assert_eq!(
            empty.footer_text(),
            "0 sessions · scope: all projects · Tab to toggle · Enter to resume · \
             Ctrl+D to delete · Esc to cancel",
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
    fn footer_text_shows_filtered_over_total_when_query_active() {
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
            "filter `fix` should narrow to one title but keep `total` visible: {footer}",
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
            let h = picker.height(width).min(40);
            let mut terminal = Terminal::new(TestBackend::new(width, h)).unwrap();
            terminal
                .draw(|frame| picker.render(frame, Rect::new(0, 0, width, h), &theme))
                .expect("render must not panic");
        }
    }

    #[test]
    fn render_skips_footer_when_area_too_short_for_two_rows_below_list() {
        // Defensive guard: if the parent allocates less than `list_h + 2` rows the picker drops
        // the footer rather than spilling into the list area or panicking.
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
        let h = picker.height(60);
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
    fn typing_filters_rows_via_searchable_list() {
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
        // Bare picker stays scoped; Tab widens AND preserves the typed filter.
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

        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());
        assert_eq!(picker.total, 1, "scoped: only the home project");
        for c in "11".chars() {
            picker.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(picker.list.query(), "11");
        picker.handle_key(&key(KeyCode::Tab));
        assert_eq!(picker.total, 2, "Tab widens to all projects");
        assert_eq!(
            picker.list.query(),
            "11",
            "Tab must not reset the user's filter",
        );
    }

    #[test]
    fn unrecognized_keys_are_consumed_without_side_effects() {
        // Both bare unhandled keys (Insert) and Ctrl + non-`u` chars are absorbed silently —
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

        let mut event = KeyEvent::from(KeyCode::Char('a'));
        event.modifiers = KeyModifiers::CONTROL;
        picker.handle_key(&event);
        assert_eq!(picker.list.query(), "");
    }

    #[test]
    fn ctrl_d_pushes_confirm_modal_for_cursor_row() {
        // Both gestures route to the same confirm push — verifies the dual-binding contract from
        // the picker footer hint.
        let (_dir, store) = isolated_store();
        seed_session(
            &store,
            &stamped_id(0x11),
            Some("Fix auth"),
            3,
            datetime!(2026-04-18 09:00:00 UTC),
        );
        let mut picker = ResumePicker::new(store, "live-session-id".to_owned());

        let mut ctrl_d = KeyEvent::from(KeyCode::Char('d'));
        ctrl_d.modifiers = KeyModifiers::CONTROL;
        let outcome = picker.handle_key(&ctrl_d);
        assert!(
            matches!(outcome, ModalKey::Push(_)),
            "Ctrl+D must push the confirm modal; got {outcome:?}",
        );

        let outcome = picker.handle_key(&key(KeyCode::Delete));
        assert!(
            matches!(outcome, ModalKey::Push(_)),
            "Delete key must push the confirm modal; got {outcome:?}",
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
        assert_eq!(picker.total, 2, "both seeded rows present");

        store.delete(&id_a, "live-session-id").unwrap();
        picker.on_focus_regained();
        assert_eq!(picker.total, 1, "deleted row drops on reload");
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

        store.delete(&target, "live-session-id").unwrap();
        picker.on_focus_regained();
        assert_eq!(picker.total, 2, "deleted row dropped");
        assert_ne!(
            picker.list.selected().unwrap().session_id,
            target,
            "cursor moved off the deleted row",
        );
    }

    #[test]
    fn ctrl_d_then_y_through_modal_stack_unlinks_the_session() {
        // End-to-end: picker on the stack, Ctrl+D pushes the confirm child, Y on the child runs
        // the unlink and pops back. Pins the wiring across ResumePicker, ModalKey::Push,
        // ModalStack::handle_key, ConfirmDeleteSessionModal, and SessionStore::delete.
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
        assert!(
            matches!(outcome, Some(ModalAction::None)),
            "Y submits silently"
        );
        assert!(stack.is_active(), "picker remains after the child pops");
        assert_eq!(
            store.list().unwrap().len(),
            1,
            "one row was actually deleted from disk",
        );
    }

    // ── ResumeCmd ──

    #[test]
    fn metadata_matches_built_ins_contract() {
        assert_eq!(ResumeCmd.name(), "resume");
        assert_eq!(ResumeCmd.aliases(), &["continue"]);
        assert!(!ResumeCmd.description().is_empty());
        assert_eq!(ResumeCmd.usage(), Some("[<id-prefix>]"));
    }

    #[test]
    fn classify_is_always_mutating() {
        // Both forms reach the agent loop (typed-arg → Forward; picker → modal-submitted Resume),
        // and mid-turn user actions get dropped, so neither may run while busy.
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
    fn execute_bare_opens_picker_with_no_chat_writes() {
        with_isolated_xdg(|_| {
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let (outcome, modal) = {
                let mut ctx = SlashContext::new(&mut chat, &info);
                let outcome = ResumeCmd.execute("", &mut ctx).unwrap();
                (outcome, ctx.take_modal())
            };
            assert_eq!(outcome, SlashOutcome::Done);
            assert!(modal.is_some(), "bare /resume must open a modal");
            assert_eq!(chat.entry_count(), 0);
        });
    }

    #[test]
    fn execute_with_unknown_prefix_returns_friendly_error() {
        with_isolated_xdg(|_| {
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);
            let err = ResumeCmd.execute("zzz1234", &mut ctx).unwrap_err();
            assert!(err.contains("no session matching"), "{err}");
        });
    }

    #[test]
    fn execute_typed_arg_unique_match_emits_forward_with_resolved_id() {
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
            let mut chat = ChatView::new(&Theme::default(), false);
            let info = test_session_info();
            let mut ctx = SlashContext::new(&mut chat, &info);
            // 4-char prefix is enough to pin a single session in the seeded store.
            let outcome = ResumeCmd.execute(&target_id[..4], &mut ctx).unwrap();
            let SlashOutcome::Forward(UserAction::Resume { session_id }) = outcome else {
                panic!("expected Forward(Resume), got {outcome:?}");
            };
            assert_eq!(session_id, target_id);
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
    fn resolve_prefix_unique_match_returns_session_id() {
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
        let id = resolve_prefix(&store, "1111", "other").unwrap();
        assert_eq!(id, stamped_id(0x11));
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
        assert_eq!(resolve_prefix(&store, &id, "other").unwrap(), id);
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
        assert!(err.contains(&id_a[..ID_WIDTH]), "expected id_a in {err}");
        assert!(err.contains(&id_b[..ID_WIDTH]), "expected id_b in {err}");
    }
}
