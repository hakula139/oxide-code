//! Destructive-action confirm modal, currently scoped to session deletion. Generalize when a
//! second use case lands.
//!
//! Pushed as a nested overlay above the resume picker, or directly from `/delete <id-prefix>`.
//! Y or Enter runs the delete. Failures latch inline so the user sees the error without losing
//! the modal.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::session::store::SessionStore;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── Constants ──

const TITLE: &str = "Delete this session?";
const FOOTER_HINT: &str = "[Y] delete   [N] cancel   Esc to cancel";
const ID_PREFIX_WIDTH: usize = 8;
/// Width floor so narrow terminals still paint the full body without panicking.
const MIN_BUDGET: usize = 8;

// ── ConfirmDeleteSessionModal ──

/// Confirm-and-delete overlay. Owns the `SessionStore` clone so the unlink fires synchronously on
/// Y without a roundtrip through the agent loop. The `live_session_id` field threads the live id
/// down to `store.delete` for its FS-boundary refusal check, even though upstream callers
/// (resume picker filter, `/delete` resolver) already filter it.
pub(super) struct ConfirmDeleteSessionModal {
    store: SessionStore,
    session_id: String,
    display_title: String,
    /// Pre-formatted metadata ("14 msgs · 2 hours ago"). Caller builds it so the modal stays
    /// decoupled from `time::OffsetDateTime` formatting.
    metadata: String,
    live_session_id: String,
    /// Sticky error from a failed delete attempt. Cleared on next press.
    error: Option<String>,
}

impl ConfirmDeleteSessionModal {
    pub(super) fn new(
        store: SessionStore,
        session_id: String,
        display_title: String,
        metadata: String,
        live_session_id: String,
    ) -> Self {
        Self {
            store,
            session_id,
            display_title,
            metadata,
            live_session_id,
            error: None,
        }
    }

    fn id_prefix(&self) -> &str {
        self.session_id
            .get(..ID_PREFIX_WIDTH)
            .unwrap_or(&self.session_id)
    }

    /// Run `store.delete`. On Ok, pop and emit a chat-stream confirmation. On Err, stay open with
    /// a sticky inline error that clears on the next non-confirm keypress.
    fn confirm(&mut self) -> ModalKey {
        match self.store.delete(&self.session_id, &self.live_session_id) {
            Ok(()) => ModalKey::Submitted(ModalAction::SystemMessage(format!(
                "Deleted session {}: {}",
                self.id_prefix(),
                self.display_title,
            ))),
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                ModalKey::Consumed
            }
        }
    }
}

impl Modal for ConfirmDeleteSessionModal {
    fn height(&self, _width: u16) -> u16 {
        if self.error.is_some() { 7 } else { 6 }
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let budget = usize::from(area.width).max(MIN_BUDGET);
        let mut lines: Vec<Line<'static>> =
            Vec::with_capacity(usize::from(self.height(area.width)));

        lines.push(Line::from(Span::styled(
            TITLE,
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());

        let identity = format!("{} — {}", self.id_prefix(), self.display_title);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&identity, budget),
            theme.text(),
        )));
        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.metadata, budget),
            theme.dim(),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            truncate_to_width(FOOTER_HINT, budget),
            theme.dim(),
        )));
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                truncate_to_width(&format!("! {err}"), budget),
                theme.error(),
            )));
        }

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        // Esc and Ctrl+C are intercepted at the stack level. Any other key clears the sticky
        // error so the user's next confirm attempt isn't shadowed by the previous failure.
        match event.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.confirm(),
            KeyCode::Char('n' | 'N') => ModalKey::Cancelled,
            _ => {
                self.error = None;
                ModalKey::Consumed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::session::store::{seed_test_session, test_store};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
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
            store.clone(),
            stamped_id,
            title.to_owned(),
            "3 msgs · 2 hours ago".to_owned(),
            "live-session-id".to_owned(),
        )
    }

    fn render_to_string(modal: &ConfirmDeleteSessionModal, width: u16, height: u16) -> String {
        let theme = Theme::default();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| modal.render(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    // ── render ──

    #[test]
    fn render_paints_title_identity_metadata_and_footer() {
        let (_dir, store) = isolated_store();
        let modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let dump = render_to_string(&modal, 60, modal.height(60));
        assert!(dump.contains(TITLE), "title appears: {dump}");
        assert!(dump.contains("abcd1234"), "id prefix appears: {dump}");
        assert!(dump.contains("Fix auth flow"), "title appears: {dump}");
        assert!(
            dump.contains("3 msgs · 2 hours ago"),
            "metadata appears: {dump}"
        );
        assert!(dump.contains("[Y] delete"), "footer hint appears: {dump}");
    }

    #[test]
    fn render_appends_error_row_when_previous_attempt_failed() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        modal.error = Some("permission denied".to_owned());
        // Height grows by one to accommodate the error row.
        assert_eq!(modal.height(60), 7, "error row claims an extra line");
        let dump = render_to_string(&modal, 60, modal.height(60));
        assert!(dump.contains("permission denied"), "error visible: {dump}");
    }

    #[test]
    fn render_does_not_panic_at_minimum_widths() {
        let (_dir, store) = isolated_store();
        let modal = seed_modal(&store, "abcd1234", "T");
        for w in [4_u16, 8, 20] {
            render_to_string(&modal, w, modal.height(w));
        }
    }

    // ── handle_key ──

    #[test]
    fn y_press_runs_delete_and_submits_with_chat_confirmation() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let id_to_delete = modal.session_id.clone();

        let outcome = modal.handle_key(&key(KeyCode::Char('y')));
        let ModalKey::Submitted(ModalAction::SystemMessage(msg)) = outcome else {
            panic!("Y must Submit(SystemMessage); got {outcome:?}");
        };
        assert!(
            msg.starts_with("Deleted session abcd1234"),
            "confirmation must lead with the id prefix: {msg}",
        );
        assert!(
            msg.contains("Fix auth flow"),
            "confirmation includes title: {msg}"
        );
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
    fn enter_press_is_an_alias_for_y_and_runs_delete() {
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
    fn uppercase_y_also_confirms() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth");
        let outcome = modal.handle_key(&key(KeyCode::Char('Y')));
        assert!(matches!(
            outcome,
            ModalKey::Submitted(ModalAction::SystemMessage(_)),
        ));
    }

    #[test]
    fn n_press_cancels_without_running_delete() {
        let (_dir, store) = isolated_store();
        let mut modal = seed_modal(&store, "abcd1234", "Fix auth flow");
        let id_kept = modal.session_id.clone();

        let outcome = modal.handle_key(&key(KeyCode::Char('n')));
        assert!(
            matches!(outcome, ModalKey::Cancelled),
            "N must Cancel; got {outcome:?}"
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
    fn confirm_failure_stays_open_with_inline_error_then_clears_on_next_key() {
        // Same-id-as-live triggers store.delete's refusal; modal must stay on screen with the
        // error visible. A subsequent unrecognized key clears the error so the user can re-attempt.
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
            store.clone(),
            live_id.clone(),
            "Fix auth flow".to_owned(),
            "3 msgs · 2 hours ago".to_owned(),
            live_id.clone(),
        );

        let outcome = modal.handle_key(&key(KeyCode::Char('y')));
        assert!(matches!(outcome, ModalKey::Consumed));
        let err = modal.error.as_deref().expect("error must latch on failure");
        assert!(
            err.contains("refusing to delete the live session"),
            "got: {err}"
        );
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .any(|s| s.session_id == live_id),
            "row must still exist after refused delete",
        );

        // An unrelated key clears the latch so the next attempt isn't shadowed by stale text.
        let outcome = modal.handle_key(&key(KeyCode::Char('x')));
        assert!(matches!(outcome, ModalKey::Consumed));
        assert!(modal.error.is_none(), "error cleared on next press");
    }

    #[test]
    fn unrecognized_keys_consume_silently_without_running_delete() {
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
