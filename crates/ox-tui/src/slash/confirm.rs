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
use crate::modal::acceptance::Acceptance;
use crate::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::modal::{Modal, ModalAction, ModalKey};
use crate::theme::Theme;

// ── Constants ──

const TITLE: &str = "Delete this session?";
const FOOTER_HINTS: [&str; 2] = ["Y to delete", "N to cancel"];

/// The title and metadata rows.
const BODY_HEIGHT: u16 = 2;

// ── ConfirmDeleteSessionModal ──

/// Confirm-and-delete overlay. Y unlinks the session file synchronously, bypassing the engine.
pub(super) struct ConfirmDeleteSessionModal {
    header: Header,
    path: PathBuf,
    session_id: String,
    display_title: String,
    /// The [`format_metadata_line`](crate::slash::sessions::format_metadata_line) row, such as
    /// `abcd1234 · 2 hours ago · 14 msgs`, which carries the ID prefix.
    metadata: String,
    live_session_id: String,
    /// Error from the last failed delete, shown until a retry replaces it or the modal closes.
    error: Option<String>,
    acceptance: Acceptance,
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
            acceptance: Acceptance::default(),
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
                self.error = Some(saved_error_message(&err, Some(CLEAR_THEN_DELETE_HINT)));
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
        let budget = usize::from(area.width);
        let metadata = truncate_to_width(&self.metadata, budget);
        self.acceptance.update(
            area.width > 0
                && metadata.starts_with(id_prefix(&self.session_id))
                && area.height >= self.height(area.width, u16::MAX),
        );
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);

        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.display_title, budget),
            theme.text(),
        )));
        lines.push(Line::from(Span::styled(metadata, theme.dim())));
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
            KeyCode::Char('y' | 'Y') if self.acceptance.accepts(event) => self.confirm(),
            KeyCode::Char('n' | 'N') => ModalKey::Cancelled,
            _ => {
                self.acceptance.guard();
                ModalKey::Consumed
            }
        }
    }

    fn binds_modified_key(&self, _event: &KeyEvent) -> bool {
        self.acceptance.is_guarded()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyModifiers};
    use time::OffsetDateTime;
    use time::macros::datetime;

    use ox_core::session::store::SessionStore;
    use ox_core::session::store::testing::{isolated_store, seed_test_session, stamped_id};

    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::testing::{render_rows, render_rows_with};
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
        let metadata = format_metadata_line(&id, LAST_ACTIVE, NOW, MESSAGES, None, None);
        ConfirmDeleteSessionModal::new(path, id, title.to_owned(), metadata, live_id.to_owned())
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

        fn show_and_arm(modal: &ConfirmDeleteSessionModal) {
            render_rows(modal, 60, modal.height(60, u16::MAX));
            modal.acceptance.expire();
        }

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
            for event in [
                KeyEvent::from(KeyCode::Char('y')),
                KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            ] {
                let (_dir, store) = isolated_store();
                let mut modal = seed_modal(&store, 0xab, "Fix auth flow");
                show_and_arm(&modal);

                let outcome = modal.handle_key(&event);

                assert!(
                    matches!(&outcome, ModalKey::Submitted(ModalAction::SystemMessage(msg))
                        if msg == "Deleted session abababab: Fix auth flow"),
                    "{event:?}: {outcome:?}",
                );
                assert!(!is_listed(&store, &stamped_id(0xab)), "{event:?}");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_visible_target_waits_for_quiet_and_rejects_enter_and_repeated_y() {
            let (_dir, store) = isolated_store();
            let modal = seed_modal(&store, 0xab, "Fix auth flow");
            let mut stack = ModalStack::default();
            stack.push(Box::new(modal));
            render_rows_with(60, 7, |frame, area, theme| stack.render(frame, area, theme));

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(outcome.is_none());
            assert!(is_listed(&store, &stamped_id(0xab)));

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            for event in [
                KeyEvent::from(KeyCode::Enter),
                KeyEvent::new_with_kind(
                    KeyCode::Char('y'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                ),
            ] {
                let outcome = stack.handle_key(&event);

                assert!(outcome.is_none(), "{event:?}");
                assert!(is_listed(&store, &stamped_id(0xab)), "{event:?}");
            }

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(outcome.is_none(), "a repeat restarts the quiet period");

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(
                matches!(outcome, Some(ModalAction::SystemMessage(ref text))
                if text == "Deleted session abababab: Fix auth flow"),
                "{outcome:?}"
            );
            assert!(!is_listed(&store, &stamped_id(0xab)));
        }

        #[tokio::test(start_paused = true)]
        async fn clipped_targets_rearm_when_they_reappear() {
            for (width, height) in [(60, 0), (60, 1), (60, 6), (0, 7), (7, 7), (8, 7), (10, 7)] {
                let (_dir, store) = isolated_store();
                let modal = seed_modal(&store, 0xab, "Fix auth flow");
                show_and_arm(&modal);
                let mut stack = ModalStack::default();
                stack.push(Box::new(modal));
                render_rows_with(width, height, |frame, area, theme| {
                    stack.render(frame, area, theme);
                });

                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(outcome.is_none(), "{width}x{height}: {outcome:?}");
                assert!(is_listed(&store, &stamped_id(0xab)));

                tokio::time::advance(TYPE_AHEAD_GUARD).await;
                render_rows_with(60, 7, |frame, area, theme| stack.render(frame, area, theme));
                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(outcome.is_none(), "restoring {width}x{height} must rearm");

                tokio::time::advance(TYPE_AHEAD_GUARD).await;
                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(
                    matches!(outcome, Some(ModalAction::SystemMessage(ref text))
                    if text == "Deleted session abababab: Fix auth flow"),
                    "{outcome:?}"
                );
                assert!(!is_listed(&store, &stamped_id(0xab)));
            }
        }

        #[test]
        fn modified_confirmation_keys_do_not_delete() {
            let (_dir, store) = isolated_store();
            let modal = seed_modal(&store, 0xab, "Fix auth");
            show_and_arm(&modal);

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
        fn cancel_keys_leave_the_session_untouched_before_rendering() {
            for event in [
                KeyEvent::from(KeyCode::Char('n')),
                KeyEvent::from(KeyCode::Esc),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ] {
                let (_dir, store) = isolated_store();
                let modal = seed_modal(&store, 0xab, "Fix auth flow");
                let mut stack = ModalStack::default();
                stack.push(Box::new(modal));

                let outcome = stack.handle_key(&event);

                assert!(
                    matches!(outcome, Some(ModalAction::Cancelled)),
                    "{event:?}: {outcome:?}"
                );
                assert!(!stack.is_active());
                assert!(is_listed(&store, &stamped_id(0xab)), "{event:?}");
            }
        }

        #[test]
        fn a_failed_delete_keeps_the_modal_open_through_stray_keys() {
            // Deleting the live session fails, so the modal stays open with the error.
            let (_dir, store) = isolated_store();
            let live_id = stamped_id(0xab);
            let mut modal = seed_modal_with_live(&store, 0xab, "Fix auth flow", &live_id);
            show_and_arm(&modal);

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
            show_and_arm(&modal);

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
