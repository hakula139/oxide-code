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
use crate::modal::{Modal, ModalAction, ModalKey, is_plain_char};
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
            KeyCode::Char('y' | 'Y') | KeyCode::Enter if is_plain_char(event) => self.confirm(),
            KeyCode::Char('n' | 'N') if is_plain_char(event) => ModalKey::Cancelled,
            _ => ModalKey::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use time::UtcOffset;
    use time::macros::datetime;

    use ox_core::session::display::format_metadata_line;
    use ox_core::session::store::SessionStore;
    use ox_core::session::store::testing::{isolated_store, seed_test_session};

    use super::*;
    use crate::modal::testing::render_rows;
    use crate::slash::testing::stamped_id;

    /// The metadata row `/delete` and the `/resume` picker build for a 3-message session.
    fn metadata(session_id: &str) -> String {
        let last_active = datetime!(2026-04-18 09:00:00 UTC);
        format_metadata_line(session_id, last_active, UtcOffset::UTC, 3, None, None)
    }

    /// Seeds a session stamped with `byte` and opens its confirm modal, with `live_id` live.
    fn seed_modal_with_live(
        store: &SessionStore,
        byte: u8,
        title: &str,
        live_id: &str,
    ) -> ConfirmDeleteSessionModal {
        let id = stamped_id(byte);
        seed_test_session(
            store,
            &id,
            Some(title),
            Some(3),
            datetime!(2026-04-18 09:00:00 UTC),
        );
        ConfirmDeleteSessionModal::new(
            store.find_session_path(&id).unwrap(),
            id.clone(),
            title.to_owned(),
            metadata(&id),
            live_id.to_owned(),
        )
    }

    fn seed_modal(store: &SessionStore, byte: u8, title: &str) -> ConfirmDeleteSessionModal {
        seed_modal_with_live(store, byte, title, "live-session-id")
    }

    fn is_listed(store: &SessionStore, session_id: &str) -> bool {
        store
            .list()
            .unwrap()
            .iter()
            .any(|s| s.session_id == session_id)
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
            let metadata = metadata(&stamped_id(0xab));
            assert!(metadata.starts_with("abababab · "), "{metadata}");

            assert_eq!(
                rendered_rows(&modal, 60),
                [
                    "Delete this session?",
                    "",
                    "Fix auth flow",
                    metadata.as_str(),
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
            assert_eq!(
                rendered_rows(&modal, 60)[3..],
                [
                    metadata(&stamped_id(0xab)).as_str(),
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
        fn uppercase_y_and_enter_also_delete() {
            for key in [KeyCode::Char('Y'), KeyCode::Enter] {
                let (_dir, store) = isolated_store();
                let mut modal = seed_modal(&store, 0xab, "Fix auth flow");

                let outcome = modal.handle_key(&KeyEvent::from(key));

                assert!(
                    matches!(outcome, ModalKey::Submitted(ModalAction::SystemMessage(_))),
                    "{key:?}: {outcome:?}",
                );
                assert!(!is_listed(&store, &stamped_id(0xab)), "{key:?}");
            }
        }

        #[test]
        fn control_y_and_control_enter_do_not_confirm_delete() {
            let (_dir, store) = isolated_store();
            let mut modal = seed_modal(&store, 0xab, "Fix auth");

            for event in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            ] {
                assert!(matches!(modal.handle_key(&event), ModalKey::Consumed));
            }
            assert!(
                is_listed(&store, &stamped_id(0xab)),
                "modified confirmation keys must not delete the session",
            );
        }

        #[test]
        fn n_press_cancels_without_running_delete() {
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
        fn a_failed_delete_keeps_the_modal_open_with_its_error() {
            // Deleting the live session fails, so the modal stays open with the error.
            let (_dir, store) = isolated_store();
            let live_id = stamped_id(0xab);
            let mut modal = seed_modal_with_live(&store, 0xab, "Fix auth flow", &live_id);

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert_eq!(
                modal.error.as_deref(),
                Some(
                    "Session abcd1234 is the live session. \
                     Run `/clear` to start a new one, then delete it."
                ),
            );
            for stray in [KeyCode::Char('x'), KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
                let outcome = modal.handle_key(&KeyEvent::from(stray));
                assert!(matches!(outcome, ModalKey::Consumed), "{stray:?}");
                assert!(modal.error.is_some(), "{stray:?} must not clear the error");
            }
            assert!(is_listed(&store, &live_id));
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
