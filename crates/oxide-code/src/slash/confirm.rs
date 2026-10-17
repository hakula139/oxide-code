//! Destructive-action confirm modal scoped to session deletion. Generalize when a second use
//! case lands.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::session::display::id_prefix;
use crate::session::saved;
use crate::tui::glyphs::MODAL_ERROR_PREFIX;
use crate::tui::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::tui::modal::{Modal, ModalAction, ModalKey, is_plain_char};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── Constants ──

const TITLE: &str = "Delete this session?";
const FOOTER_HINTS: [&str; 2] = ["Y to delete", "N to cancel"];

/// The identity and metadata rows.
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
    /// Pre-formatted metadata ("14 msgs · 2 hours ago"). Caller builds it so the modal stays
    /// decoupled from `time::OffsetDateTime` formatting.
    metadata: String,
    live_session_id: String,
    /// Sticky error from a failed delete attempt. Kept until a deliberate confirm or cancel key.
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

    /// Deletes the session file. On Ok, pop and emit a chat-stream confirmation. On Err, stay open
    /// with a sticky inline error.
    fn confirm(&mut self) -> ModalKey {
        match saved::delete(&self.path, &self.session_id, &self.live_session_id) {
            Ok(()) => ModalKey::Submitted(ModalAction::SystemMessage(format!(
                "Deleted session {}: {}",
                id_prefix(&self.session_id),
                self.display_title,
            ))),
            Err(err) => {
                self.error = Some(err);
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

        let identity = format!("{} — {}", id_prefix(&self.session_id), self.display_title);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&identity, budget),
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
        // Esc and Ctrl+C are intercepted at the stack level. Sticky error stays visible until the
        // user makes a deliberate Y / N / Enter choice, so a stray arrow key or modifier-only
        // event can't wipe the only diagnostic from a failed delete.
        match event.code {
            KeyCode::Char('y' | 'Y') if is_plain_char(event) => {
                self.error = None;
                self.confirm()
            }
            KeyCode::Enter if event.modifiers.is_empty() => {
                self.error = None;
                self.confirm()
            }
            KeyCode::Char('n' | 'N') if is_plain_char(event) => ModalKey::Cancelled,
            _ => ModalKey::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::session::store::SessionStore;
    use crate::session::store::testing::{seed_test_session, test_store};
    use crate::tui::modal::testing::render_rows;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn isolated_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        (dir, store)
    }

    fn seed_modal(store: &SessionStore, id: &str, title: &str) -> ConfirmDeleteSessionModal {
        let stamped_id = format!("{id:0<36}");
        seed_test_session(
            store,
            &stamped_id,
            Some(title),
            Some(3),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );
        ConfirmDeleteSessionModal::new(
            store.find_session_path(&stamped_id).unwrap(),
            stamped_id,
            title.to_owned(),
            "3 msgs · 2 hours ago".to_owned(),
            "live-session-id".to_owned(),
        )
    }

    fn rendered_rows(modal: &ConfirmDeleteSessionModal, width: u16) -> Vec<String> {
        render_rows(modal, width, modal.height(width, u16::MAX))
            .into_iter()
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    // ── ConfirmDeleteSessionModal::render ──

    #[test]
    fn render_paints_title_identity_metadata_and_footer() {
        let (_dir, store) = isolated_store();
        let modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        assert_eq!(
            rendered_rows(&modal, 60),
            [
                "Delete this session?",
                "",
                "abcd1234 — Fix auth flow",
                "3 msgs · 2 hours ago",
                "",
                "Y to delete  ·  N to cancel",
            ],
        );
    }

    #[test]
    fn render_places_the_error_row_above_the_footer() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        modal.error = Some("permission denied".to_owned());
        assert_eq!(
            rendered_rows(&modal, 60)[3..],
            [
                "3 msgs · 2 hours ago",
                "! permission denied",
                "",
                "Y to delete  ·  N to cancel",
            ],
        );
    }

    #[test]
    fn render_does_not_panic_at_minimum_widths() {
        let (_dir, store) = isolated_store();
        let modal = seed_modal(&store, "abcd1234", "T");
        for width in [4_u16, 8, 20] {
            rendered_rows(&modal, width);
        }
    }

    // ── ConfirmDeleteSessionModal::handle_key ──

    #[test]
    fn handle_key_y_press_runs_delete_and_submits_with_chat_confirmation() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let id_to_delete = modal.session_id.clone();

        let outcome = modal.handle_key(&key(KeyCode::Char('y')));
        let ModalKey::Submitted(ModalAction::SystemMessage(msg)) = outcome else {
            panic!("Y must Submit(SystemMessage), got {outcome:?}");
        };
        assert_eq!(msg, "Deleted session abcd1234: Fix auth flow");
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .all(|s| s.session_id != id_to_delete),
            "row gone from list",
        );
    }

    #[test]
    fn handle_key_enter_press_is_an_alias_for_y_and_runs_delete() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let outcome = modal.handle_key(&key(KeyCode::Enter));
        assert!(matches!(
            outcome,
            ModalKey::Submitted(ModalAction::SystemMessage(_)),
        ));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn handle_key_uppercase_y_also_confirms() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth");
        let outcome = modal.handle_key(&key(KeyCode::Char('Y')));
        assert!(matches!(
            outcome,
            ModalKey::Submitted(ModalAction::SystemMessage(_)),
        ));
    }

    #[test]
    fn handle_key_control_y_and_control_enter_do_not_confirm_delete() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth");
        let id_kept = modal.session_id.clone();

        for event in [
            modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            assert!(matches!(modal.handle_key(&event), ModalKey::Consumed));
        }
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .any(|s| s.session_id == id_kept),
            "modified confirmation keys must not delete the session",
        );
    }

    #[test]
    fn handle_key_n_press_cancels_without_running_delete() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let id_kept = modal.session_id.clone();

        let outcome = modal.handle_key(&key(KeyCode::Char('n')));
        assert!(
            matches!(outcome, ModalKey::Cancelled),
            "N must Cancel, got {outcome:?}"
        );
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .any(|s| s.session_id == id_kept),
            "session must still exist after cancel",
        );
    }

    #[test]
    fn handle_key_confirm_failure_latches_error_until_a_deliberate_y_or_n_choice() {
        // Same-id-as-live triggers delete_at's refusal, so the modal stays on screen with the
        // error visible. Stray keys (arrows, unrecognized chars, modifier events) must leave the
        // latch, which only Y / Enter (re-attempt) or N (cancel) clears. This pins the diagnostic
        // against a stray-keystroke race that would otherwise leave the user with no explanation
        // for why the row reappeared after delete.
        let (_dir, store) = isolated_store();
        let live_id = format!("{:0<36}", "abcd1234");
        seed_test_session(
            &store,
            &live_id,
            Some("Fix auth flow"),
            Some(3),
            time::macros::datetime!(2026-04-18 09:00:00 UTC),
        );
        let mut modal = ConfirmDeleteSessionModal::new(
            store.find_session_path(&live_id).unwrap(),
            live_id.clone(),
            "Fix auth flow".to_owned(),
            "3 msgs · 2 hours ago".to_owned(),
            live_id.clone(),
        );

        let outcome = modal.handle_key(&key(KeyCode::Char('y')));
        assert!(matches!(outcome, ModalKey::Consumed));
        assert_eq!(
            modal.error.as_deref(),
            Some(
                format!(
                    "Failed to delete session abcd1234: refusing to delete the live session: \
                     {live_id}"
                )
                .as_str()
            ),
        );

        // Stray keys must not wipe the latch.
        for stray in [KeyCode::Char('x'), KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
            let outcome = modal.handle_key(&key(stray));
            assert!(matches!(outcome, ModalKey::Consumed), "{stray:?}");
            assert!(modal.error.is_some(), "{stray:?} must not clear the latch");
        }

        // A deliberate Y re-attempt clears the latch before re-running confirm; the re-attempt
        // hits the same refusal and re-latches, but it's a fresh diagnostic, not the stale one.
        let outcome = modal.handle_key(&key(KeyCode::Char('y')));
        assert!(matches!(outcome, ModalKey::Consumed));
        assert!(modal.error.is_some(), "fresh failure re-latches");
    }

    #[test]
    fn handle_key_unrecognized_keys_consume_silently_without_running_delete() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let outcome = modal.handle_key(&key(KeyCode::Char('z')));
        assert!(matches!(outcome, ModalKey::Consumed));
        assert!(
            !store.list().unwrap().is_empty(),
            "session untouched on unrecognized key",
        );
    }
}
