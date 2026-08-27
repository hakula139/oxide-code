//! Tool-call approval overlay. Surfaces a gated call's preview and resolves the blocked agent to
//! `Approve` or `Deny`. Built from the `ConfirmDeleteSessionModal` template. See
//! `docs/design/tools/permissions.md`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::agent::event::{ApprovalBody, ApprovalDecision, ApprovalPreview, UserAction};
use crate::tool::DiffChunk;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::util::text::truncate_to_width;

// ── Constants ──

const FOOTER_HINT: &str = "[Y] approve   [N] deny";
/// Width floor so narrow terminals still paint the body without panicking.
const MIN_BUDGET: usize = 8;
/// Cap on preview body rows. An overflowing diff or multi-line command collapses to a count.
const MAX_BODY_LINES: usize = 16;

// ── ApprovalModal ──

/// Approve-or-deny overlay for a gated tool call. Holds the tool-use `id` so the emitted decision
/// matches the blocked call, and resolves every dismissal path to a decision (cancel and session
/// swap deny) rather than stranding the agent.
pub(crate) struct ApprovalModal {
    id: String,
    title: String,
    body: Vec<BodyLine>,
}

impl ApprovalModal {
    pub(crate) fn new(id: String, preview: ApprovalPreview) -> Self {
        Self {
            id,
            title: preview.title,
            body: flatten_body(&preview.body),
        }
    }

    fn decision(&self, decision: ApprovalDecision) -> ModalAction {
        ModalAction::User(UserAction::ApprovalDecision {
            id: self.id.clone(),
            decision,
        })
    }
}

impl Modal for ApprovalModal {
    fn height(&self, _width: u16) -> u16 {
        // title + blank + body + blank + footer.
        let body = u16::try_from(self.body.len()).unwrap_or(u16::MAX);
        body.saturating_add(4)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let budget = usize::from(area.width).max(MIN_BUDGET);
        let mut lines: Vec<Line<'static>> =
            Vec::with_capacity(usize::from(self.height(area.width)));

        lines.push(Line::from(Span::styled(
            truncate_to_width(&self.title, budget),
            theme.accent().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        for body_line in &self.body {
            lines.push(Line::from(Span::styled(
                truncate_to_width(&body_line.text, budget),
                body_line.kind.style(theme),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            truncate_to_width(FOOTER_HINT, budget),
            theme.dim(),
        )));

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        // Esc and Ctrl+C are intercepted at the stack level and resolve through `on_cancel`. N
        // routes there too so the deny decision lives in exactly one place.
        match event.code {
            KeyCode::Char('y' | 'Y') if is_plain_char(event) => {
                ModalKey::Submitted(self.decision(ApprovalDecision::Approve))
            }
            KeyCode::Enter if event.modifiers.is_empty() => {
                ModalKey::Submitted(self.decision(ApprovalDecision::Approve))
            }
            KeyCode::Char('n' | 'N') if is_plain_char(event) => ModalKey::Cancelled,
            _ => ModalKey::Consumed,
        }
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        Some(self.decision(ApprovalDecision::Deny))
    }
}

fn is_plain_char(event: &KeyEvent) -> bool {
    event.modifiers.is_empty() || event.modifiers == KeyModifiers::SHIFT
}

// ── Body ──

/// A precomputed preview row. Text is split at construction so [`ApprovalModal::height`] stays
/// width-independent, and `render` truncates to the live width.
struct BodyLine {
    kind: BodyKind,
    text: String,
}

#[derive(Clone, Copy)]
enum BodyKind {
    Plain,
    Removed,
    Added,
    Dim,
}

impl BodyKind {
    fn style(self, theme: &Theme) -> ratatui::style::Style {
        match self {
            Self::Plain => theme.text(),
            Self::Removed => theme.error(),
            Self::Added => theme.success(),
            Self::Dim => theme.dim(),
        }
    }
}

/// Flattens an [`ApprovalBody`] into capped preview rows: a command's lines verbatim, or a diff's
/// hunks as `-` / `+` rows. Overflow past [`MAX_BODY_LINES`] collapses to a dim count.
fn flatten_body(body: &ApprovalBody) -> Vec<BodyLine> {
    let mut rows = Vec::new();
    match body {
        ApprovalBody::Command(command) => {
            for line in command.lines() {
                rows.push(BodyLine {
                    kind: BodyKind::Plain,
                    text: line.to_owned(),
                });
            }
        }
        ApprovalBody::Diff(chunks) => collect_diff_rows(chunks, &mut rows),
    }
    cap_rows(rows)
}

fn collect_diff_rows(chunks: &[DiffChunk], rows: &mut Vec<BodyLine>) {
    for chunk in chunks {
        for line in &chunk.old {
            rows.push(BodyLine {
                kind: BodyKind::Removed,
                text: format!("- {}", line.text),
            });
        }
        for line in &chunk.new {
            rows.push(BodyLine {
                kind: BodyKind::Added,
                text: format!("+ {}", line.text),
            });
        }
    }
}

fn cap_rows(mut rows: Vec<BodyLine>) -> Vec<BodyLine> {
    if rows.len() <= MAX_BODY_LINES {
        return rows;
    }
    let hidden = rows.len() - (MAX_BODY_LINES - 1);
    rows.truncate(MAX_BODY_LINES - 1);
    rows.push(BodyLine {
        kind: BodyKind::Dim,
        text: format!("... {hidden} more lines"),
    });
    rows
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tool::DiffLine;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn command_modal(command: &str) -> ApprovalModal {
        ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(command.to_owned()),
            },
        )
    }

    fn render_to_string(modal: &ApprovalModal, width: u16, height: u16) -> String {
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
    fn render_paints_title_command_body_and_footer() {
        let modal = command_modal("cargo test");
        let dump = render_to_string(&modal, 40, modal.height(40));
        assert!(dump.contains("Bash"), "title appears: {dump}");
        assert!(dump.contains("cargo test"), "command body appears: {dump}");
        assert!(dump.contains("[Y] approve"), "footer hint appears: {dump}");
    }

    #[test]
    fn render_paints_diff_body_with_signs() {
        let modal = ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Write(src/main.rs)".to_owned(),
                body: ApprovalBody::Diff(vec![DiffChunk {
                    old: vec![DiffLine {
                        number: 1,
                        text: "old line".to_owned(),
                    }],
                    new: vec![DiffLine {
                        number: 1,
                        text: "new line".to_owned(),
                    }],
                }]),
            },
        );
        let dump = render_to_string(&modal, 40, modal.height(40));
        assert!(dump.contains("- old line"), "removed row appears: {dump}");
        assert!(dump.contains("+ new line"), "added row appears: {dump}");
    }

    #[test]
    fn render_does_not_panic_at_minimum_widths() {
        let modal = command_modal("cargo test --all");
        for w in [4_u16, 8, 20] {
            render_to_string(&modal, w, modal.height(w));
        }
    }

    // ── height ──

    #[test]
    fn height_counts_body_rows_plus_chrome() {
        // Single command line → 1 body row, plus chrome of title + 2 blanks + footer = 4.
        let modal = command_modal("ls");
        assert_eq!(modal.height(40), 5);
    }

    // ── handle_key ──

    #[test]
    fn y_and_enter_submit_an_approve_decision() {
        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            let mut modal = command_modal("ls");
            let outcome = modal.handle_key(&key(code));
            let ModalKey::Submitted(ModalAction::User(UserAction::ApprovalDecision {
                id,
                decision,
            })) = outcome
            else {
                panic!("{code:?} must Submit an Approve decision; got {outcome:?}");
            };
            assert_eq!(id, "call-1");
            assert_eq!(decision, ApprovalDecision::Approve);
        }
    }

    #[test]
    fn n_cancels_so_the_stack_resolves_the_deny_via_on_cancel() {
        // N returns Cancelled, since the deny decision is produced by on_cancel, not handle_key, so
        // the cancel and Esc paths share one source of the verdict.
        let mut modal = command_modal("ls");
        assert!(matches!(
            modal.handle_key(&key(KeyCode::Char('n'))),
            ModalKey::Cancelled
        ));
    }

    #[test]
    fn modified_confirmation_keys_do_not_approve() {
        let mut modal = command_modal("ls");
        for event in [
            modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            assert!(matches!(modal.handle_key(&event), ModalKey::Consumed));
        }
    }

    #[test]
    fn unrecognized_keys_consume_silently() {
        let mut modal = command_modal("ls");
        assert!(matches!(
            modal.handle_key(&key(KeyCode::Char('z'))),
            ModalKey::Consumed
        ));
        assert!(matches!(
            modal.handle_key(&key(KeyCode::Up)),
            ModalKey::Consumed
        ));
    }

    // ── on_cancel ──

    #[test]
    fn on_cancel_yields_a_deny_decision_for_this_call() {
        let mut modal = command_modal("ls");
        let action = modal.on_cancel().expect("approval cancel must deny");
        let ModalAction::User(UserAction::ApprovalDecision { id, decision }) = action else {
            panic!("on_cancel must deny via a UserAction; got {action:?}");
        };
        assert_eq!(id, "call-1");
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    // ── cap_rows ──

    #[test]
    fn cap_rows_collapses_overflow_to_a_dim_count() {
        let rows = (0..MAX_BODY_LINES + 5)
            .map(|i| BodyLine {
                kind: BodyKind::Plain,
                text: format!("line {i}"),
            })
            .collect();
        let capped = cap_rows(rows);
        assert_eq!(capped.len(), MAX_BODY_LINES);
        assert!(
            matches!(capped.last().unwrap().kind, BodyKind::Dim),
            "overflow row must be dim",
        );
        assert_eq!(
            capped.last().unwrap().text,
            "... 6 more lines",
            "hidden count = total - (cap - 1)",
        );
    }

    #[test]
    fn cap_rows_keeps_rows_under_the_cap_untouched() {
        let rows = (0..3)
            .map(|i| BodyLine {
                kind: BodyKind::Plain,
                text: format!("line {i}"),
            })
            .collect();
        assert_eq!(cap_rows(rows).len(), 3);
    }
}
