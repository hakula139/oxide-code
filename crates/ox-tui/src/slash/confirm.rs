//! The confirm modal for deleting a saved session.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::session::display::id_prefix;
use ox_core::session::saved;
use ox_core::util::text::truncate_to_width;

use super::sessions::{CLEAR_THEN_DELETE_HINT, saved_error_message};
use crate::glyphs::MODAL_ERROR_PREFIX;
use crate::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;

// ── Constants ──

const TITLE: &str = "Delete this session?";
const FOOTER_HINTS: [&str; 2] = ["Y to delete", "N to cancel"];

/// The title and metadata rows.
const BODY_HEIGHT: u16 = 2;
/// Width floor so narrow terminals still paint the full body without panicking.
const MIN_BUDGET: usize = 8;

// ── ConfirmDeleteSessionModal ──

/// Confirm-and-delete overlay. Y unlinks the session file synchronously, bypassing the engine.
pub(super) struct ConfirmDeleteSessionModal {
    header: Header,
    path: PathBuf,
    session_id: String,
    display_title: String,
    /// The [`format_metadata_line`](ox_core::session::display::format_metadata_line) row, such as
    /// `abcd1234 · 2 hours ago · 14 msgs`, which carries the ID prefix.
    metadata: String,
    live_session_id: String,
    /// Error from the last failed delete, shown until a retry replaces it or the modal closes.
    error: Option<String>,
}

impl ConfirmDeleteSessionModal {
    pub(super) fn new(
        path: PathBuf,
        session_id: String,
        display_title: String,
        metadata: String,
        live_session_id: String,
    ) -> Self {
        Self {
            header: Header::new(TITLE),
            path,
            session_id,
            display_title,
            metadata,
            live_session_id,
            error: None,
        }
    }

    /// Deletes the session file, submitting a chat confirmation on success and keeping the modal
    /// open with the error on failure.
    fn confirm(&mut self) -> ModalKey {
        match saved::delete(&self.path, &self.session_id, &self.live_session_id) {
            Ok(()) => ModalKey::Submitted(ModalAction::SystemMessage(format!(
                "Deleted session {}: {}",
                id_prefix(&self.session_id),
                self.display_title,
            ))),
            Err(err) => {
                self.error = Some(saved_error_message(err, Some(CLEAR_THEN_DELETE_HINT)));
                ModalKey::Consumed
            }
        }
    }
}

impl Modal for ConfirmDeleteSessionModal {
    fn height(&self, _width: u16, _max_height: u16) -> u16 {
        self.header.height() + BODY_HEIGHT + u16::from(self.error.is_some()) + FOOTER_HEIGHT
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let budget = usize::from(area.width).max(MIN_BUDGET);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);

        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.display_title, budget),
            theme.text(),
        )));
        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.metadata, budget),
            theme.dim(),
        )));
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                truncate_to_width(&format!("{MODAL_ERROR_PREFIX}{err}"), budget),
                theme.error(),
            )));
        }
        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        render_footer(frame, area, &FOOTER_HINTS, theme);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        // The modal stack handles Esc and Ctrl+C.
        match event.code {
            KeyCode::Char('y' | 'Y') => self.confirm(),
            KeyCode::Enter if event.modifiers.is_empty() => self.confirm(),
            KeyCode::Char('n' | 'N') => ModalKey::Cancelled,
            _ => ModalKey::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use time::OffsetDateTime;
    use time::macros::datetime;

    use ox_core::session::store::SessionStore;
    use ox_core::session::store::testing::{isolated_store, seed_test_session, stamped_id};

    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::testing::render_rows;
    use crate::slash::sessions::format_metadata_line;

    const LAST_ACTIVE: OffsetDateTime = datetime!(2026-04-18 09:00:00 UTC);
    /// Past the 30 days after which the metadata line shows a date.
    const NOW: OffsetDateTime = datetime!(2026-06-01 09:00:00 UTC);
    const MESSAGES: u32 = 3;

    fn seed_modal(store: &SessionStore, byte: u8, title: &str) -> ConfirmDeleteSessionModal {
        seed_modal_with_live(store, byte, title, "live-session-id")
    }

    /// Seeds a session stamped with `byte` and opens its confirm modal, with `live_id` live.
    fn seed_modal_with_live(
        store: &SessionStore,
        byte: u8,
        title: &str,
        live_id: &str,
    ) -> ConfirmDeleteSessionModal {
        let id = stamped_id(byte);
        seed_test_session(store, &id, Some(title), Some(MESSAGES), LAST_ACTIVE);
        let path = store.find_session_path(&id).unwrap();
        let metadata = metadata(&id);
        ConfirmDeleteSessionModal::new(path, id, title.to_owned(), metadata, live_id.to_owned())
    }

    /// The [`format_metadata_line`] row of a seeded session, as the `/resume` picker builds it in
    /// the current project.
    fn metadata(session_id: &str) -> String {
        format_metadata_line(session_id, LAST_ACTIVE, NOW, MESSAGES, None, None)
    }

    mod render {
        use super::*;

        fn rendered_rows(modal: &ConfirmDeleteSessionModal, width: u16) -> Vec<String> {
            render_rows(modal, width, modal.height(width, u16::MAX))
                .into_iter()
                .map(|row| row.trim_end().to_owned())
                .collect()
        }

        #[test]
        fn names_the_session_by_title_above_the_metadata_that_carries_its_id() {
            let (_dir, store) = isolated_store();
            let modal = seed_modal(&store, 0xab, "Fix auth flow");

            let rows = rendered_rows(&modal, 60);

            assert_eq!(
                rows,
                [
                    "Delete this session?",
                    "",
                    "Fix auth flow",
                    "abababab · 2026-04-18 · 3 msgs",
                    "",
                    "Y to delete  ·  N to cancel",
                ],
            );
        }

        #[test]
        fn places_the_error_row_above_the_footer() {
            let (_dir, store) = isolated_store();
            let mut modal = seed_modal(&store, 0xab, "Fix auth flow");
            modal.error = Some("permission denied".to_owned());

            let rows = rendered_rows(&modal, 60);

            assert_eq!(
                rows[3..],
                [
                    "abababab · 2026-04-18 · 3 msgs",
                    "! permission denied",
                    "",
                    "Y to delete  ·  N to cancel",
                ],
            );
        }

        #[test]
        fn does_not_panic_at_minimum_widths() {
            let (_dir, store) = isolated_store();
            let modal = seed_modal(&store, 0xab, "T");
            for width in [4_u16, 8, 20] {
                rendered_rows(&modal, width);
            }
        }
    }

    mod handle_key {
        use super::*;

        fn is_listed(store: &SessionStore, session_id: &str) -> bool {
            store
                .list()
                .unwrap()
                .iter()
                .any(|s| s.session_id == session_id)
        }

        /// The error a delete of the live session stamped `0xab` reports.
        const LIVE_REFUSAL: &str = "Session abababab is the live session. \
                                    Run `/clear` to start a new one, then delete it.";

        #[test]
        fn y_deletes_and_submits_a_chat_confirmation() {
            let (_dir, store) = isolated_store();
            let mut modal = seed_modal(&store, 0xab, "Fix auth flow");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(
                matches!(&outcome, ModalKey::Submitted(ModalAction::SystemMessage(msg))
                    if msg == "Deleted session abababab: Fix auth flow"),
                "{outcome:?}",
            );
            assert!(!is_listed(&store, &stamped_id(0xab)), "row gone from list");
        }

        #[test]
        fn shifted_y_and_plain_enter_also_delete() {
            for event in [
                KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
                KeyEvent::from(KeyCode::Enter),
            ] {
                let (_dir, store) = isolated_store();
                let mut modal = seed_modal(&store, 0xab, "Fix auth flow");

                let outcome = modal.handle_key(&event);

                assert!(
                    matches!(outcome, ModalKey::Submitted(ModalAction::SystemMessage(_))),
                    "{event:?}: {outcome:?}",
                );
                assert!(!is_listed(&store, &stamped_id(0xab)), "{event:?}");
            }
        }

        #[test]
        fn modified_confirmation_keys_do_not_delete() {
            let (_dir, store) = isolated_store();
            let modal = seed_modal(&store, 0xab, "Fix auth");

            let mut stack = ModalStack::default();
            stack.push(Box::new(modal));

            for event in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            ] {
                assert!(stack.handle_key(&event).is_none(), "{event:?}");
            }

            assert!(
                is_listed(&store, &stamped_id(0xab)),
                "modified confirmation keys must not delete the session",
            );
        }

        #[test]
        fn n_cancels_without_deleting() {
            let (_dir, store) = isolated_store();
            let mut modal = seed_modal(&store, 0xab, "Fix auth flow");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('n')));

            assert!(
                matches!(outcome, ModalKey::Cancelled),
                "N must Cancel, got {outcome:?}"
            );
            assert!(
                is_listed(&store, &stamped_id(0xab)),
                "session must still exist after cancel",
            );
        }

        #[test]
        fn a_failed_delete_keeps_the_modal_open_through_stray_keys() {
            // Deleting the live session fails, so the modal stays open with the error.
            let (_dir, store) = isolated_store();
            let live_id = stamped_id(0xab);
            let mut modal = seed_modal_with_live(&store, 0xab, "Fix auth flow", &live_id);

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            assert!(matches!(outcome, ModalKey::Consumed));

            for stray in [KeyCode::Char('x'), KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
                let outcome = modal.handle_key(&KeyEvent::from(stray));
                assert!(matches!(outcome, ModalKey::Consumed), "{stray:?}");
            }

            assert_eq!(modal.error.as_deref(), Some(LIVE_REFUSAL));
            assert!(is_listed(&store, &live_id), "the live session survives");
        }

        #[test]
        fn a_retry_replaces_the_previous_error() {
            let (_dir, store) = isolated_store();
            let live_id = stamped_id(0xab);
            let mut modal = seed_modal_with_live(&store, 0xab, "Fix auth flow", &live_id);
            modal.error = Some("permission denied".to_owned());

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert_eq!(modal.error.as_deref(), Some(LIVE_REFUSAL));
        }

        #[test]
        fn unrecognized_keys_consume_silently_without_running_delete() {
            let (_dir, store) = isolated_store();
            let mut modal = seed_modal(&store, 0xab, "Fix auth flow");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('z')));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert!(
                is_listed(&store, &stamped_id(0xab)),
                "session untouched on unrecognized key",
            );
        }
    }
}
