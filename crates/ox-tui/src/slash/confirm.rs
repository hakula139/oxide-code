//! The confirm modal for deleting a saved session.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::protocol::{Event, Op};
use ox_core::session::display::{DisplayTitle, id_prefix};
use ox_core::util::text::truncate_to_width;

use super::sessions::{CLEAR_THEN_DELETE_HINT, saved_error_message};
use crate::action::Action;
use crate::glyphs::MODAL_ERROR_PREFIX;
use crate::modal::acceptance::Acceptance;
use crate::modal::chrome::{FOOTER_HEIGHT, Header, render_footer};
use crate::modal::{EngineUpdate, Modal, ModalAction, ModalKey, UpdateReply};
use crate::theme::Theme;

// ── Constants ──

const TITLE: &str = "Delete this session?";
const FOOTER_HINTS: [&str; 2] = ["Y to delete", "N to cancel"];

/// The title and metadata rows.
const BODY_HEIGHT: u16 = 2;

// ── ConfirmDeleteSessionModal ──

/// Confirm-and-delete overlay. Y sends [`Op::DeleteSaved`] and keeps the modal open until the
/// engine reports the result, which closes it on success and shows the error on failure.
pub(super) struct ConfirmDeleteSessionModal {
    header: Header,
    session_id: String,
    display_title: DisplayTitle,
    /// The [`format_metadata_line`](crate::slash::sessions::format_metadata_line) row, such as
    /// `abcd1234 · 2 hours ago · 14 msgs`, which carries the ID prefix.
    metadata: String,
    /// Whether the engine accepted a delete that has not reported back, which ignores another Y.
    pending: bool,
    /// Error from the last failed delete, shown until a retry replaces it or the modal closes.
    error: Option<String>,
    acceptance: Acceptance,
}

impl ConfirmDeleteSessionModal {
    pub(super) fn new(session_id: String, display_title: DisplayTitle, metadata: String) -> Self {
        Self {
            header: Header::new(TITLE),
            session_id,
            display_title,
            metadata,
            pending: false,
            error: None,
            acceptance: Acceptance::default(),
        }
    }

    fn confirm(&self) -> ModalKey {
        ModalKey::Emit(ModalAction::Dispatch(Action::Op(Op::DeleteSaved {
            session_id: self.session_id.clone(),
        })))
    }
}

impl Modal for ConfirmDeleteSessionModal {
    fn height(&mut self, _width: u16, _max_height: u16) -> u16 {
        self.header.height() + BODY_HEIGHT + u16::from(self.error.is_some()) + FOOTER_HEIGHT
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let budget = usize::from(area.width);
        let fits = area.height >= self.height(area.width, u16::MAX);
        let metadata = truncate_to_width(&self.metadata, budget);
        let targeted = area.width > 0 && metadata.starts_with(id_prefix(&self.session_id));
        self.acceptance.update(targeted && fits);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(usize::from(area.height));
        self.header.push_lines(&mut lines, area.width, theme);

        lines.push(Line::from(Span::styled(
            self.display_title.fit(budget),
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
            KeyCode::Char('y' | 'Y') if !self.pending && self.acceptance.accepts(event) => {
                self.confirm()
            }
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

    fn on_engine(&mut self, update: EngineUpdate<'_>) -> UpdateReply {
        match update {
            EngineUpdate::Admitted(Op::DeleteSaved { session_id })
                if *session_id == self.session_id =>
            {
                self.pending = true;
                UpdateReply::Unclaimed
            }
            EngineUpdate::Event(Event::SavedDeleted { id, result }) if *id == self.session_id => {
                self.pending = false;
                match result {
                    Ok(()) => UpdateReply::Closed(ModalAction::SystemMessage(format!(
                        "Deleted session {}: {}",
                        id_prefix(id),
                        self.display_title,
                    ))),
                    Err(error) => {
                        self.error = Some(saved_error_message(error, CLEAR_THEN_DELETE_HINT));
                        UpdateReply::Shown
                    }
                }
            }
            _ => UpdateReply::Unclaimed,
        }
    }

    fn sends_idle_only_ops(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyModifiers};
    use time::OffsetDateTime;
    use time::macros::datetime;

    use ox_core::session::saved::SavedError;
    use ox_core::session::store::listing::SessionInfo;
    use ox_core::session::store::testing::{listed_session, stamped_id};

    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::testing::{render_modal, render_rows_with};
    use crate::slash::sessions::format_metadata_line;

    const LAST_ACTIVE: OffsetDateTime = datetime!(2026-04-18 09:00:00 UTC);
    /// Past the 30 days after which the metadata line shows a date.
    const NOW: OffsetDateTime = datetime!(2026-06-01 09:00:00 UTC);
    const MESSAGES: u32 = 3;

    /// The error a delete of the live session stamped `0xab` reports.
    const LIVE_REFUSAL: &str = "Session abababab is the live session. \
                                Run `/clear` to start a new one, then delete it.";

    /// The confirm modal for the session stamped with `byte`.
    fn modal(byte: u8, title: &str) -> ConfirmDeleteSessionModal {
        let id = stamped_id(byte);
        let metadata = format_metadata_line(&id, LAST_ACTIVE, NOW, MESSAGES, None, None);
        let title = DisplayTitle::of(&SessionInfo {
            title: Some(title.to_owned()),
            ..listed_session(&id)
        });
        ConfirmDeleteSessionModal::new(id, title, metadata)
    }

    /// Renders `modal` in full so its acceptance arms, then lets the type-ahead guard lapse.
    fn show_and_arm(modal: &mut ConfirmDeleteSessionModal) {
        render_modal(modal, 60, u16::MAX);
        modal.acceptance.expire();
    }

    /// Returns whether `action` asks the engine to delete the session stamped `0xab`.
    fn deletes(action: Option<&ModalAction>) -> bool {
        matches!(
            action,
            Some(ModalAction::Dispatch(Action::Op(Op::DeleteSaved { session_id })))
                if *session_id == stamped_id(0xab)
        )
    }

    /// Tells `modal` the App sent the delete of the session stamped `byte`.
    fn admit(modal: &mut ConfirmDeleteSessionModal, byte: u8) {
        modal.on_engine(EngineUpdate::Admitted(&Op::DeleteSaved {
            session_id: stamped_id(byte),
        }));
    }

    /// Reports `result` for the delete of the session stamped `byte` to `modal`.
    fn report(
        modal: &mut ConfirmDeleteSessionModal,
        byte: u8,
        result: Result<(), SavedError>,
    ) -> UpdateReply {
        modal.on_engine(EngineUpdate::Event(&Event::SavedDeleted {
            id: stamped_id(byte),
            result,
        }))
    }

    mod render {
        use super::*;

        #[test]
        fn names_the_session_by_title_above_the_metadata_that_carries_its_id() {
            let mut modal = modal(0xab, "Fix auth flow");

            let rows = render_modal(&mut modal, 60, u16::MAX);

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
            let mut modal = modal(0xab, "Fix auth flow");
            modal.error = Some("permission denied".to_owned());

            let rows = render_modal(&mut modal, 60, u16::MAX);

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
            let mut modal = modal(0xab, "T");
            for width in [4_u16, 8, 20] {
                render_modal(&mut modal, width, u16::MAX);
            }
        }
    }

    mod handle_key {
        use super::*;

        #[test]
        fn y_sends_the_delete_and_ignores_another_once_it_starts() {
            for event in [
                KeyEvent::from(KeyCode::Char('y')),
                KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT),
            ] {
                let mut modal = modal(0xab, "Fix auth flow");
                show_and_arm(&mut modal);

                let outcome = modal.handle_key(&event);
                admit(&mut modal, 0xab);
                let repeat = modal.handle_key(&event);

                assert!(
                    matches!(&outcome, ModalKey::Emit(action) if deletes(Some(action))),
                    "{event:?}: {outcome:?}",
                );
                assert!(
                    matches!(repeat, ModalKey::Consumed),
                    "{event:?}: {repeat:?}"
                );
            }
        }

        #[tokio::test(start_paused = true)]
        async fn a_visible_target_waits_for_quiet_and_rejects_enter_and_repeated_y() {
            let mut stack = ModalStack::default();
            stack.push(Box::new(modal(0xab, "Fix auth flow")));
            render_rows_with(60, 7, |frame, area, theme| stack.render(frame, area, theme));

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(outcome.is_none());

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
            }

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(outcome.is_none(), "a repeat restarts the quiet period");

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(deletes(outcome.as_ref()), "{outcome:?}");
            assert!(stack.is_active(), "the modal waits for the result");
        }

        #[tokio::test(start_paused = true)]
        async fn clipped_targets_rearm_when_they_reappear() {
            for (width, height) in [(60, 0), (60, 1), (60, 6), (0, 7), (7, 7), (8, 7), (10, 7)] {
                let mut modal = modal(0xab, "Fix auth flow");
                show_and_arm(&mut modal);
                let mut stack = ModalStack::default();
                stack.push(Box::new(modal));
                render_rows_with(width, height, |frame, area, theme| {
                    stack.render(frame, area, theme);
                });

                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(outcome.is_none(), "{width}x{height}: {outcome:?}");

                tokio::time::advance(TYPE_AHEAD_GUARD).await;
                render_rows_with(60, 7, |frame, area, theme| stack.render(frame, area, theme));
                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(outcome.is_none(), "restoring {width}x{height} must rearm");

                tokio::time::advance(TYPE_AHEAD_GUARD).await;
                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

                assert!(deletes(outcome.as_ref()), "{width}x{height}: {outcome:?}");
            }
        }

        #[test]
        fn modified_confirmation_keys_do_not_delete() {
            let mut modal = modal(0xab, "Fix auth");
            show_and_arm(&mut modal);
            let mut stack = ModalStack::default();
            stack.push(Box::new(modal));

            for event in [
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            ] {
                assert!(stack.handle_key(&event).is_none(), "{event:?}");
            }
        }

        #[test]
        fn cancel_keys_close_the_modal_before_rendering() {
            for event in [
                KeyEvent::from(KeyCode::Char('n')),
                KeyEvent::from(KeyCode::Esc),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ] {
                let mut stack = ModalStack::default();
                stack.push(Box::new(modal(0xab, "Fix auth flow")));

                let outcome = stack.handle_key(&event);

                assert!(outcome.is_none(), "{event:?}: {outcome:?}");
                assert!(!stack.is_active());
            }
        }

        #[test]
        fn unrecognized_keys_consume_silently() {
            let mut modal = modal(0xab, "Fix auth flow");
            show_and_arm(&mut modal);

            for stray in [KeyCode::Char('z'), KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
                let outcome = modal.handle_key(&KeyEvent::from(stray));

                assert!(matches!(outcome, ModalKey::Consumed), "{stray:?}");
            }
        }
    }

    mod on_engine {
        use super::*;

        #[test]
        fn another_sessions_delete_leaves_y_sending() {
            let mut modal = modal(0xab, "Fix auth flow");
            show_and_arm(&mut modal);
            modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            admit(&mut modal, 0xcd);

            let repeat = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            assert!(
                matches!(&repeat, ModalKey::Emit(action) if deletes(Some(action))),
                "{repeat:?}"
            );
        }

        /// The modal of the session stamped `0xab`, with its delete accepted by the engine.
        fn pending_modal() -> ConfirmDeleteSessionModal {
            let mut modal = modal(0xab, "Fix auth flow");
            show_and_arm(&mut modal);
            modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            admit(&mut modal, 0xab);
            modal
        }

        #[test]
        fn a_delete_closes_the_modal_with_a_chat_confirmation() {
            let mut modal = pending_modal();

            let action = report(&mut modal, 0xab, Ok(()));

            assert!(
                matches!(&action, UpdateReply::Closed(ModalAction::SystemMessage(msg))
                    if msg == "Deleted session abababab: Fix auth flow"),
                "{action:?}",
            );
        }

        #[test]
        fn a_failure_replaces_the_error_and_takes_y_again() {
            let mut modal = pending_modal();
            modal.error = Some("permission denied".to_owned());
            let live = SavedError::Live {
                id: "abababab".to_owned(),
            };

            let action = report(&mut modal, 0xab, Err(live));

            assert!(matches!(action, UpdateReply::Shown), "{action:?}");
            assert_eq!(modal.error.as_deref(), Some(LIVE_REFUSAL));
            let retry = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            assert!(
                matches!(&retry, ModalKey::Emit(action) if deletes(Some(action))),
                "{retry:?}"
            );
        }

        #[test]
        fn ignores_another_sessions_result() {
            let mut modal = pending_modal();

            let action = report(&mut modal, 0xcd, Ok(()));

            assert!(matches!(action, UpdateReply::Unclaimed), "{action:?}");
            let repeat = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));
            assert!(
                matches!(repeat, ModalKey::Consumed),
                "still pending: {repeat:?}"
            );
        }
    }
}
