//! Search and preview of retained user prompts, recalled into the composer without submitting.

use std::borrow::Cow;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr as _;

use ox_core::session::display::format_short_relative_time;
use ox_core::session::store::SessionStore;
use ox_core::session::store::listing::ProjectScope;
use ox_core::session::store::prompt_history::{PromptEntry, PromptWalk};
use ox_core::util::path::tildify;
use ox_core::util::text::{center_truncate_to_width, escape_line, pad_to_width, truncate_to_width};
use ox_core::util::time::now_local;

use crate::action::Action;
use crate::app_event::AppEventSender;
use crate::keymap::{KeyAction, KeyChord, Keymap, is_unmodified};
use crate::modal::chrome::{FOOTER_HEIGHT, render_footer};
use crate::modal::line_editor::LineEditor;
use crate::modal::pages::{Loaded, ModalPage, Pages};
use crate::modal::searchable_list::{LoadState, SearchableItem, SearchableList};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::session_info::LiveSessionInfo;
use crate::theme::Theme;
use crate::wrap::wrap_line;

const TITLE: &str = "Search prompts";
const EMPTY: &str = "No prompt history";
const VISIBLE_ROWS: u16 = 6;
const PREVIEW_HEIGHT: u16 = 9;
const PAGE_SIZE: usize = 30;

struct PromptRow {
    entry: PromptEntry,
    listed_at: OffsetDateTime,
    show_project: bool,
}

impl SearchableItem for PromptRow {
    const ROW_HEIGHT: u16 = 1;

    fn haystack(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.entry.text)
    }

    fn render(&self, width: u16, is_cursor: bool, theme: &Theme) -> Vec<Line<'static>> {
        let age = format_short_relative_time(self.entry.created_at, self.listed_at);
        let project = if self.show_project {
            format!(
                "  {}",
                center_truncate_to_width(
                    &escape_line(&tildify(Path::new(&self.entry.project_root))),
                    usize::from(width) / 3
                )
            )
        } else {
            String::new()
        };
        let first = escape_line(self.entry.text.split('\n').next().unwrap_or_default());
        let room = usize::from(width).saturating_sub(age.width() + 2 + project.width());
        let first = truncate_to_width(&first, room);
        let first = if self.show_project {
            pad_to_width(&first, room)
        } else {
            first
        };
        let text = truncate_to_width(&format!("{age}  {first}{project}"), usize::from(width));
        vec![Line::from(Span::styled(
            text,
            if is_cursor {
                theme.accent()
            } else {
                theme.text()
            },
        ))]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryAction {
    Recall,
    Dismiss,
    ToggleScope,
}

const ACTIONS: Keymap<HistoryAction, PromptHistoryPicker> = Keymap::new(&[
    KeyAction {
        action: HistoryAction::Recall,
        key: KeyChord::plain(KeyCode::Enter),
        effect: |picker| picker.list.selected().map(|_| "edit"),
    },
    KeyAction {
        action: HistoryAction::Dismiss,
        key: KeyChord::plain(KeyCode::Esc),
        effect: |_| Some("cancel"),
    },
    KeyAction {
        action: HistoryAction::ToggleScope,
        key: KeyChord::ctrl('s'),
        effect: |picker| {
            Some(match picker.scope {
                ProjectScope::Current => "search everywhere",
                ProjectScope::Every => "search this project",
            })
        },
    },
]);

pub(crate) struct PromptHistoryPicker {
    store: SessionStore,
    list: SearchableList<PromptRow>,
    scope: ProjectScope,
    pages: Pages<PromptWalk>,
    events: AppEventSender,
    listed_at: OffsetDateTime,
    preview_offset: usize,
    preview_page: usize,
    preview_max: usize,
}

impl PromptHistoryPicker {
    pub(crate) fn new(info: &LiveSessionInfo) -> Self {
        let mut picker = Self {
            store: info.store.clone(),
            list: SearchableList::new(TITLE, Vec::new(), VISIBLE_ROWS).with_empty_text(EMPTY),
            scope: ProjectScope::Current,
            pages: Pages::Done,
            events: info.app_events.clone(),
            listed_at: now_local(),
            preview_offset: 0,
            preview_page: 1,
            preview_max: 0,
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        self.pages
            .replace(self.store.prompt_history(self.scope, self.list.query()));
        self.listed_at = now_local();
        let scope = match self.scope {
            ProjectScope::Current => "This project",
            ProjectScope::Every => "Every project",
        };
        self.list.set_header(TITLE, scope);
        self.list.replace_items(Vec::new());
        self.list.set_load_state(LoadState::Loading);
        self.preview_offset = 0;
        self.load_ahead();
    }

    fn load_ahead(&mut self) {
        if self.list.rows_below_cursor() < usize::from(VISIBLE_ROWS) {
            self.pages.request(&self.events, PAGE_SIZE);
        }
    }

    fn page_loaded(&mut self, loaded: Loaded<PromptEntry>) {
        match loaded {
            Loaded::Page(entries) => {
                let rows = entries
                    .into_iter()
                    .map(|entry| PromptRow {
                        entry,
                        listed_at: self.listed_at,
                        show_project: self.scope == ProjectScope::Every,
                    })
                    .collect();
                self.list.append_items(rows);
                self.load_ahead();
                self.list.set_load_state(if self.pages.has_more() {
                    LoadState::Loading
                } else {
                    LoadState::Complete
                });
            }
            Loaded::Failed(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "prompt history failed to load");
                self.failed(format!("Failed to load prompts: {error:#}"));
            }
            Loaded::Stopped => {
                self.failed("Failed to load prompts: the page loader stopped".to_owned());
            }
        }
    }

    fn failed(&mut self, error: String) {
        self.list.set_header(TITLE, error);
        self.list.set_load_state(LoadState::Failed);
    }

    fn layout(&self, width: u16, max_height: u16) -> (u16, u16, u16) {
        let footer = if max_height >= 12 { FOOTER_HEIGHT } else { 0 };
        let room = max_height.saturating_sub(footer);
        let preview = room.saturating_sub(6).min(PREVIEW_HEIGHT);
        let list = self.list.height(width, room - preview);
        (list, preview, footer)
    }

    fn render_preview(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if area.height < 2 || area.width == 0 {
            return;
        }
        let Some(row) = self.list.selected() else {
            return;
        };
        let lines: Vec<_> = row
            .entry
            .text
            .split('\n')
            .flat_map(|line| {
                wrap_line(
                    Line::raw(escape_line(line)),
                    usize::from(area.width),
                    0,
                    None,
                )
            })
            .collect();
        self.preview_page = usize::from(area.height - 1);
        self.preview_max = lines.len().saturating_sub(self.preview_page);
        self.preview_offset = self.preview_offset.min(self.preview_max);
        let end = (self.preview_offset + self.preview_page).min(lines.len());
        let label = format!(
            "Preview {}-{end} / {} (PageUp / PageDown to scroll)",
            self.preview_offset + 1,
            lines.len()
        );
        frame.render_widget(Paragraph::new(label).style(theme.dim()), area);
        let body = Rect {
            y: area.y + 1,
            height: area.height - 1,
            ..area
        };
        let visible: Vec<_> = lines
            .into_iter()
            .skip(self.preview_offset)
            .take(self.preview_page)
            .collect();
        frame.render_widget(Paragraph::new(visible).style(theme.text()), body);
    }
}

impl Modal for PromptHistoryPicker {
    fn height(&mut self, width: u16, max_height: u16) -> u16 {
        let (list, preview, footer) = self.layout(width, max_height);
        list + preview + footer
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (list, preview, footer) = self.layout(area.width, area.height);
        self.list.render(
            frame,
            Rect {
                height: list,
                ..area
            },
            theme,
        );
        self.render_preview(
            frame,
            Rect {
                y: area.y + list,
                height: preview,
                ..area
            },
            theme,
        );
        if footer > 0 {
            let hints: Vec<_> = ACTIONS.hints(self).map(|(_, hint)| hint).collect();
            render_footer(frame, area, &hints, theme);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.code == KeyCode::Enter && !event.modifiers.is_empty() {
            return ModalKey::Consumed;
        }
        if let Some(action) = ACTIONS.action_for(event) {
            match action {
                HistoryAction::Recall => {
                    if let Some(row) = self.list.selected() {
                        return ModalKey::Submitted(ModalAction::Dispatch(Action::ReplaceInput(
                            row.entry.text.clone(),
                        )));
                    }
                }
                HistoryAction::Dismiss => return ModalKey::Cancelled,
                HistoryAction::ToggleScope => {
                    self.scope = self.scope.toggled();
                    self.reload();
                }
            }
            return ModalKey::Consumed;
        }
        let before = self.list.query().to_owned();
        let cursor = self.list.cursor_index();
        match event.code {
            KeyCode::Up if is_unmodified(event) => self.list.select_prev(),
            KeyCode::Down if is_unmodified(event) => self.list.select_next(),
            KeyCode::PageUp if is_unmodified(event) => {
                self.preview_offset = self.preview_offset.saturating_sub(self.preview_page);
            }
            KeyCode::PageDown if is_unmodified(event) => {
                self.preview_offset = self
                    .preview_offset
                    .saturating_add(self.preview_page)
                    .min(self.preview_max);
            }
            _ => self.list.edit_query(event),
        }
        if self.list.query() == before {
            if self.list.cursor_index() != cursor {
                self.preview_offset = 0;
            }
            self.load_ahead();
        } else {
            self.reload();
        }
        ModalKey::Consumed
    }

    fn handle_paste(&mut self, text: &str) {
        let before = self.list.query().to_owned();
        self.list.paste_query(text);
        if self.list.query() != before {
            self.reload();
        }
    }

    fn on_page(&mut self, page: ModalPage) -> Option<ModalPage> {
        let ModalPage::Prompts(page) = page else {
            return Some(page);
        };
        match self.pages.take(page) {
            Ok(loaded) => {
                self.page_loaded(loaded);
                None
            }
            Err(page) => {
                self.load_ahead();
                Some(ModalPage::Prompts(page))
            }
        }
    }

    fn captures_esc(&self) -> bool {
        true
    }

    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        LineEditor::binds_modified_key(event)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Style};
    use time::macros::datetime;
    use tokio::sync::mpsc;

    use ox_core::session::store::testing::{SEEDED_PROMPT, isolated_store, seed_test_session};

    use super::*;
    use crate::app_event::AppEvent;
    use crate::modal::pages::PageEnd;
    use crate::modal::testing::render_modal;
    use crate::session_info::testing::test_session_info;

    struct Harness {
        picker: PromptHistoryPicker,
        inbox: mpsc::UnboundedReceiver<AppEvent>,
    }

    impl Harness {
        fn new(store: SessionStore) -> Self {
            let (app_events, inbox) = mpsc::unbounded_channel();
            let info = LiveSessionInfo {
                store,
                app_events,
                ..test_session_info()
            };
            Self {
                picker: PromptHistoryPicker::new(&info),
                inbox,
            }
        }

        fn settle(&mut self) {
            while matches!(
                self.picker.pages,
                Pages::Loading(_) | Pages::Reloading { .. }
            ) {
                let page = self.next_page();
                _ = self.picker.on_page(page);
            }
        }

        fn next_page(&mut self) -> ModalPage {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(event) = self.inbox.try_recv() {
                    let AppEvent::Page(page) = event else {
                        panic!("expected page");
                    };
                    return page;
                }
                assert!(
                    Instant::now() < deadline,
                    "the requested page never arrived"
                );
                std::thread::yield_now();
            }
        }

        fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> ModalKey {
            self.picker.handle_key(&KeyEvent::new(code, modifiers))
        }
    }

    #[test]
    fn keeps_latest_filter_and_scope_and_recalls_exact_text_only_on_plain_enter() {
        let (_dir, store) = isolated_store();
        seed_test_session(&store, "session", None, 1, datetime!(2026-01-01 00:00 UTC));
        let mut history = Harness::new(store);

        history.picker.handle_paste("unmatched");
        history.key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        history.key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        history.picker.handle_paste("SEEDED");
        history.settle();

        assert_eq!(history.picker.scope, ProjectScope::Every);
        assert_eq!(history.picker.list.query(), "SEEDED");
        insta::assert_snapshot!(
            "prompt_every_project",
            render_modal(&mut history.picker, 65, 25).join("\n")
        );
        assert!(matches!(
            history.key(KeyCode::Enter, KeyModifiers::SHIFT),
            ModalKey::Consumed
        ));
        assert!(matches!(
            history.key(KeyCode::Enter, KeyModifiers::NONE),
            ModalKey::Submitted(ModalAction::Dispatch(Action::ReplaceInput(text))) if text == SEEDED_PROMPT
        ));
        assert!(matches!(
            history.key(KeyCode::Esc, KeyModifiers::NONE),
            ModalKey::Cancelled
        ));
    }

    #[test]
    fn renders_full_escaped_multiline_preview_and_reaches_its_final_lines() {
        let (_dir, store) = isolated_store();
        let mut history = Harness::new(store);
        history.settle();
        let text = format!(
            "  first\x1b[31m\n{}\nlast  ",
            (1..15)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        history.picker.page_loaded(Loaded::Page(vec![PromptEntry {
            text: text.clone(),
            created_at: datetime!(2026-01-01 00:00 UTC),
            project_root: "/home/u/src/app".to_owned(),
        }]));
        let before = render_modal(&mut history.picker, 65, 25).join("\n");
        assert!(before.contains("first\\u{1b}[31m"), "{before}");
        assert!(before.contains("Preview 1-"), "{before}");
        insta::assert_snapshot!("prompt_preview", before);

        for _ in 0..5 {
            history.key(KeyCode::PageDown, KeyModifiers::NONE);
        }
        let after = render_modal(&mut history.picker, 65, 25).join("\n");
        assert!(after.contains("last"), "{after}");
        insta::assert_snapshot!("prompt_preview_end", after);
        for _ in 0..5 {
            history.key(KeyCode::PageUp, KeyModifiers::NONE);
        }
        assert_eq!(history.picker.preview_offset, 0);
        for _ in 0..5 {
            history.key(KeyCode::PageDown, KeyModifiers::NONE);
        }
        assert!(matches!(
            history.key(KeyCode::Enter, KeyModifiers::NONE),
            ModalKey::Submitted(ModalAction::Dispatch(Action::ReplaceInput(recalled))) if recalled == text
        ));
        for (width, height) in [(1, 1), (2, 3), (10, 7), (25, 12)] {
            let rows = render_modal(&mut history.picker, width, height);
            assert!(rows.len() <= usize::from(height));
        }
        insta::assert_snapshot!(
            "prompt_narrow",
            render_modal(&mut history.picker, 25, 12).join("\n")
        );
        insta::assert_snapshot!(
            "prompt_tiny",
            render_modal(&mut history.picker, 10, 7).join("\n")
        );
    }

    #[test]
    fn previews_lines_beyond_u16_and_shows_project_without_losing_recall_text() {
        let (_dir, store) = isolated_store();
        let mut history = Harness::new(store);
        history.settle();
        history.picker.scope = ProjectScope::Every;
        let text = (0..65_540)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        history.picker.page_loaded(Loaded::Page(vec![PromptEntry {
            text: text.clone(),
            created_at: datetime!(2026-01-01 00:00 UTC),
            project_root: "/home/u/src/app".to_owned(),
        }]));
        history.picker.preview_offset = 65_536;

        let rendered = render_modal(&mut history.picker, 65, 25).join("\n");

        assert!(rendered.contains("row 65539"), "{rendered}");
        assert!(rendered.contains("/home/u/src/app"), "{rendered}");
        assert_eq!(history.picker.list.selected().unwrap().entry.text, text);
    }

    #[test]
    fn renders_empty_no_match_and_failure_states() {
        let (_dir, store) = isolated_store();
        let mut history = Harness::new(store);
        history.settle();
        insta::assert_snapshot!(
            "prompt_empty",
            render_modal(&mut history.picker, 60, 20).join("\n")
        );
        assert!(matches!(
            history.key(KeyCode::Enter, KeyModifiers::NONE),
            ModalKey::Consumed
        ));

        history.picker.handle_paste("absent");
        insta::assert_snapshot!(
            "prompt_searching",
            render_modal(&mut history.picker, 60, 20).join("\n")
        );
        history.settle();
        insta::assert_snapshot!(
            "prompt_no_matches",
            render_modal(&mut history.picker, 60, 20).join("\n")
        );
        history.picker.page_loaded(Loaded::Stopped);
        let failed = render_modal(&mut history.picker, 60, 20).join("\n");
        assert!(failed.contains("page loader stopped"));
        assert!(!failed.contains("No matches"), "{failed}");
        assert!(!failed.contains("Searching for"), "{failed}");
        insta::assert_snapshot!("prompt_failed", failed);

        history.key(KeyCode::Up, KeyModifiers::NONE);
        let failed = render_modal(&mut history.picker, 60, 20).join("\n");
        assert!(failed.contains("page loader stopped"));
        assert!(!failed.contains("No matches"), "{failed}");
        assert_eq!(history.picker.list.query(), "absent");

        history.key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let retrying = render_modal(&mut history.picker, 60, 20).join("\n");
        assert!(retrying.contains("Searching for `absent`"), "{retrying}");
        assert!(!retrying.contains("page loader stopped"), "{retrying}");
        history.settle();
        let complete = render_modal(&mut history.picker, 60, 20).join("\n");
        assert!(complete.contains("No matches for `absent`"), "{complete}");
        assert_eq!(history.picker.list.query(), "absent");

        let mut unavailable = Harness::new(SessionStore::unopened("project"));
        unavailable.settle();
        let failed = render_modal(&mut unavailable.picker, 60, 20).join("\n");
        assert!(failed.contains("Failed to load prompts:"), "{failed}");
    }

    #[test]
    fn leaves_session_pages_unclaimed_and_resets_preview_on_row_changes() {
        let (_dir, store) = isolated_store();
        let mut history = Harness::new(store.clone());
        history.settle();
        let entry = |text: &str| PromptEntry {
            text: text.to_owned(),
            created_at: datetime!(2026-01-01 00:00 UTC),
            project_root: "/home/u/src/app".to_owned(),
        };
        history
            .picker
            .page_loaded(Loaded::Page(vec![entry("first"), entry("second")]));
        history.picker.preview_offset = 5;
        let page = ModalPage::Sessions(PageEnd::for_request(
            999,
            store.walk(ox_core::session::store::listing::ListScope::default()),
            Some(Ok(Vec::new())),
        ));

        assert!(matches!(
            history.picker.on_page(page),
            Some(ModalPage::Sessions(_))
        ));
        for (key, expected) in [(KeyCode::Down, "second"), (KeyCode::Up, "first")] {
            history.picker.preview_offset = 5;

            history.key(key, KeyModifiers::NONE);

            assert_eq!(history.picker.preview_offset, 0, "{key:?}");
            assert_eq!(
                history.picker.list.selected().unwrap().entry.text,
                expected,
                "{key:?}"
            );
        }
        assert!(
            render_modal(&mut history.picker, 60, 20)
                .join("\n")
                .contains("first")
        );
    }

    #[test]
    fn continues_an_empty_nonfinal_batch_to_the_matching_project() {
        let (dir, store) = isolated_store();
        let other = SessionStore::open_at(dir.path().to_path_buf(), "other").unwrap();
        let at = datetime!(2026-01-01 00:00 UTC);
        seed_test_session(&store, "local", None, 1, at);
        for n in 0..31 {
            seed_test_session(
                &other,
                &format!("other-{n}"),
                None,
                1,
                at + time::Duration::hours(1),
            );
        }
        let mut history = Harness::new(store);

        let page = history.next_page();
        assert!(history.picker.on_page(page).is_none());
        assert!(history.picker.list.selected().is_none());
        assert!(history.picker.pages.has_more());
        history.settle();

        assert_eq!(
            history.picker.list.selected().unwrap().entry.text,
            SEEDED_PROMPT
        );
        assert!(!history.picker.pages.has_more());
    }

    #[test]
    fn keeps_the_selected_row_visible_in_a_short_picker_and_renders_within_its_grant() {
        let (_dir, store) = isolated_store();
        let mut history = Harness::new(store);
        history.settle();
        let entry = |text: &str| PromptEntry {
            text: text.to_owned(),
            created_at: datetime!(2026-01-01 00:00 UTC),
            project_root: "/home/u/src/app".to_owned(),
        };
        history
            .picker
            .page_loaded(Loaded::Page(vec![entry("first"), entry("second")]));
        history.key(KeyCode::Down, KeyModifiers::NONE);

        let rendered = render_modal(&mut history.picker, 40, 8);

        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("> ") && line.ends_with("second")),
            "{rendered:?}"
        );
        let mut terminal = Terminal::new(TestBackend::new(42, 14)).unwrap();
        for height in 8..12 {
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new("sentinel\n".repeat(14))
                            .style(Style::default().fg(Color::Red)),
                        frame.area(),
                    );
                    let before = frame.buffer_mut().clone();
                    let grant = Rect::new(1, 1, 40, height);

                    history.picker.render(frame, grant, &Theme::default());

                    for y in 0..14 {
                        for x in 0..42 {
                            if !grant.contains((x, y).into()) {
                                assert_eq!(
                                    frame.buffer_mut()[(x, y)],
                                    before[(x, y)],
                                    "outside {grant:?} at ({x}, {y})"
                                );
                            }
                        }
                    }
                })
                .unwrap();
        }
    }
}
