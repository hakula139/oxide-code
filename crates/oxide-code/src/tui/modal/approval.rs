//! Tool-call approval overlay. Surfaces a gated call's preview and the rule that holds it, and
//! resolves the blocked agent to `Approve`, `AllowForSession` with a rule the user can edit first,
//! `Deny`, `DenyWithFeedback` with typed text, or `Abort`.
//!
//! Design: `docs/design/tools/permissions.md`.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::line_editor::LineEditor;
use super::{Modal, ModalAction, ModalKey, is_plain_char};
use crate::permission::SessionGrant;
use crate::protocol::{Decision, Op};
use crate::tool::{ApprovalBody, ApprovalPreview, DiffChunk};
use crate::tui::action::Action;
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs, wrap_line};
use crate::tui::{now, saturating_u16};
use crate::util::text::{is_hidden, truncate_to_width};

// ── Constants ──

const FOOTER_HINT: &str = "[Y] approve   [N] deny   [F] feedback   [Esc] abort";
const GRANT_FOOTER_HINT: &str =
    "[Y] approve   [A] allow for session   [N] deny   [F] feedback   [Esc] abort";
const RULE_FOOTER_HINT: &str = "[Enter] allow for session   [Esc] back";
const FEEDBACK_FOOTER_HINT: &str = "[Enter] deny with feedback   [Esc] back";
const SCROLL_HINT: &str = "   [↑↓] scroll";

const RULE_LABEL: &str = "Allow for the rest of this session:";
const FEEDBACK_LABEL: &str = "Deny, and tell the model why:";

/// Width floor so narrow terminals still paint the body without panicking.
const MIN_BUDGET: usize = 8;
/// Body rows painted at once. A longer preview scrolls.
const MAX_BODY_ROWS: usize = 16;
/// Blank row under the heading, position row under the body, and the footer.
const CHROME_ROWS: usize = 3;
/// The label and the editor. A rejected answer's error wraps below them.
const EDIT_ROWS: usize = 2;
const CONTINUATION_INDENT: usize = 2;

/// Stands in for each character of a whitespace run a plain space would misrepresent.
const WHITESPACE_MARKER: char = '·';

/// Quiet period before the modal answers keys. Keys inside it drop and restart it, since a burst
/// of them was typed or pasted for the input the modal covers.
const TYPE_AHEAD_GUARD: Duration = Duration::from_millis(300);

// ── ApprovalModal ──

/// Approve-or-deny overlay for a gated tool call. Holds the tool-use `id` so the emitted decision
/// matches the blocked call, and resolves every dismissal path to a decision (cancel and session
/// swap abort the turn) so the agent is never left waiting.
pub(crate) struct ApprovalModal {
    id: String,
    title: String,
    /// Why the call asks, when a rule holds it.
    reason: Option<String>,
    body: Vec<BodyLine>,
    /// The "allow for this session" offer, absent when no session rule could allow the call or
    /// while its rule is being edited.
    grant: Option<Box<SessionGrant>>,
    /// The editor `A` or `F` opened, until Enter submits it or Esc returns to the choice.
    edit: Option<Edit>,
    /// First painted body row. Key handling clamps it to the last render's [`View`].
    scroll: usize,
    view: Cell<View>,
    /// Reused across frames so a large preview is wrapped once per width and palette.
    wrapped: RefCell<Option<Wrapped>>,
    /// End of the current [`TYPE_AHEAD_GUARD`] window.
    guard_until: Instant,
}

/// Viewport geometry from the last render.
#[derive(Clone, Copy, Default)]
struct View {
    rows: usize,
    max_scroll: usize,
    /// Whether the first body row fit the area. Approval waits for it, so a cramped layout can't
    /// approve a call the user never saw.
    body_shown: bool,
    /// Whether the whole edit step, through the footer, fit the area. Enter waits for it, so a
    /// cramped layout can't submit an answer the user never saw.
    editor_shown: bool,
}

/// An open one-line editor and what its Enter submits.
struct Edit {
    purpose: Purpose,
    editor: LineEditor,
    /// Why the last Enter rejected the text.
    error: Option<String>,
}

enum Purpose {
    /// A session allow rule, which the call's grant must admit.
    Rule(Box<SessionGrant>),
    /// Feedback returned to the model with a denial.
    Feedback,
}

enum EditStep {
    Stay,
    Back,
    Submit(Decision),
}

impl Edit {
    /// Applies `event` to the editor. Enter submits once `ready`, ignores empty feedback, and shows
    /// why a rule the grant does not admit was rejected.
    fn handle_key(&mut self, event: &KeyEvent, ready: bool) -> EditStep {
        match event.code {
            KeyCode::Esc => EditStep::Back,
            KeyCode::Enter if ready => {
                let text = self.editor.text().trim();
                match &self.purpose {
                    Purpose::Rule(offer) => match offer.accept(text) {
                        Ok(rule) => EditStep::Submit(Decision::AllowForSession {
                            rule: Box::new(rule),
                        }),
                        Err(e) => {
                            self.error = Some(format!("Rule rejected: {e:#}"));
                            EditStep::Stay
                        }
                    },
                    Purpose::Feedback if text.is_empty() => EditStep::Stay,
                    Purpose::Feedback => EditStep::Submit(Decision::DenyWithFeedback {
                        feedback: text.to_owned(),
                    }),
                }
            }
            KeyCode::Enter => EditStep::Stay,
            _ => {
                if self.editor.handle_key(event) {
                    self.error = None;
                }
                EditStep::Stay
            }
        }
    }

    fn label(&self) -> &'static str {
        match self.purpose {
            Purpose::Rule(_) => RULE_LABEL,
            Purpose::Feedback => FEEDBACK_LABEL,
        }
    }

    fn footer_hint(&self) -> &'static str {
        match self.purpose {
            Purpose::Rule(_) => RULE_FOOTER_HINT,
            Purpose::Feedback => FEEDBACK_FOOTER_HINT,
        }
    }
}

/// The heading and body wrapped for one width and palette.
struct Wrapped {
    width: u16,
    palette: Palette,
    /// The title's rows, then the reason's.
    heading: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
}

/// The heading and the visible body rows, with the window's offset and size within the body.
struct BodyWindow {
    lines: Vec<Line<'static>>,
    scroll: usize,
    rows: usize,
    total: usize,
    body_shown: bool,
}

impl ApprovalModal {
    pub(crate) fn new(
        id: String,
        preview: ApprovalPreview,
        reason: Option<String>,
        grant: Option<Box<SessionGrant>>,
    ) -> Self {
        Self {
            id,
            title: preview.title,
            reason,
            body: flatten_body(&preview.body),
            grant,
            edit: None,
            scroll: 0,
            view: Cell::default(),
            wrapped: RefCell::default(),
            guard_until: now() + TYPE_AHEAD_GUARD,
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_guard(&mut self) {
        self.guard_until = now();
    }

    fn decision(&self, decision: Decision) -> ModalAction {
        ModalAction::Dispatch(Action::Op(Op::Resolve {
            id: self.id.clone(),
            decision,
        }))
    }

    /// Runs `f` on the rows wrapped for `width`. A `None` palette accepts any cached palette.
    fn with_wrapped<R>(
        &self,
        width: u16,
        palette: Option<Palette>,
        f: impl FnOnce(&Wrapped) -> R,
    ) -> R {
        let mut cache = self.wrapped.borrow_mut();
        if let Some(wrapped) = cache.as_ref()
            && wrapped.width == width
            && palette.is_none_or(|palette| palette == wrapped.palette)
        {
            return f(wrapped);
        }
        f(cache.insert(self.wrap(width, palette.unwrap_or_default())))
    }

    fn wrap(&self, width: u16, palette: Palette) -> Wrapped {
        let budget = usize::from(width).max(MIN_BUDGET);
        let title = Line::from(reveal(&self.title, palette.title, palette.marker, false));
        let reason = self
            .reason
            .iter()
            .map(|reason| Line::from(reveal(reason, palette.reason, palette.marker, false)));
        let heading = std::iter::once(title)
            .chain(reason)
            .flat_map(|line| wrap_line(line, budget, CONTINUATION_INDENT, None))
            .collect();
        let body = self
            .body
            .iter()
            .flat_map(|line| wrap_line(line.to_line(&palette), budget, CONTINUATION_INDENT, None))
            .collect();
        Wrapped {
            width,
            palette,
            heading,
            body,
        }
    }

    fn scroll_by(&mut self, delta: isize) -> ModalKey {
        let max_scroll = self.view.get().max_scroll;
        self.scroll = self
            .scroll
            .min(max_scroll)
            .saturating_add_signed(delta)
            .min(max_scroll);
        ModalKey::Consumed
    }

    /// Opens an editor for `purpose` prefilled with `text`, re-arming the type-ahead guard so an
    /// Enter typed right behind the key that opened it can't submit it unseen.
    fn start_edit(&mut self, purpose: Purpose, text: &str) -> ModalKey {
        self.edit = Some(Edit {
            purpose,
            editor: LineEditor::new(text),
            error: None,
        });
        self.guard_until = now() + TYPE_AHEAD_GUARD;
        ModalKey::Consumed
    }

    /// Routes `event` to the open editor, or returns `None` when none is open.
    fn handle_edit_key(&mut self, event: &KeyEvent) -> Option<ModalKey> {
        let scroll = self.line_scroll(event.code);
        let ready = now() >= self.guard_until && self.view.get().editor_shown;
        let edit = self.edit.as_mut()?;
        if let Some(delta) = scroll {
            return Some(self.scroll_by(delta));
        }
        Some(match edit.handle_key(event, ready) {
            EditStep::Stay => ModalKey::Consumed,
            EditStep::Back => {
                let purpose = self.edit.take().map(|edit| edit.purpose);
                if let Some(Purpose::Rule(offer)) = purpose {
                    self.grant = Some(offer);
                }
                ModalKey::Consumed
            }
            EditStep::Submit(decision) => ModalKey::Submitted(self.decision(decision)),
        })
    }

    /// The scroll a line or page key asks for. Home and End are not among them, since the
    /// editors move their cursor with them.
    fn line_scroll(&self, code: KeyCode) -> Option<isize> {
        match code {
            KeyCode::Up => Some(-1),
            KeyCode::Down => Some(1),
            KeyCode::PageUp => Some(-self.page()),
            KeyCode::PageDown => Some(self.page()),
            _ => None,
        }
    }

    /// Rows the edit step adds below the body at `width`, with a rejected answer's error wrapped.
    fn edit_rows(&self, width: u16) -> usize {
        self.edit.as_ref().map_or(0, |edit| {
            EDIT_ROWS
                + edit
                    .error
                    .as_ref()
                    .map_or(0, |error| error_lines(error, width, Style::default()).len())
        })
    }

    /// The heading and the body rows that fit `area`. The edit step claims its rows first, so the
    /// body yields to the answer being typed.
    fn body_window(&self, wrapped: &Wrapped, area: Rect) -> BodyWindow {
        let room = usize::from(area.height)
            .saturating_sub(wrapped.heading.len() + CHROME_ROWS + self.edit_rows(area.width))
            .max(usize::from(self.edit.is_none()));
        let total = wrapped.body.len();
        let rows = total.min(MAX_BODY_ROWS).min(room);
        let scroll = self.scroll.min(total - rows);

        let mut lines = wrapped.heading.clone();
        lines.push(Line::default());
        lines.extend_from_slice(&wrapped.body[scroll..scroll + rows]);
        BodyWindow {
            lines,
            scroll,
            rows,
            total,
            body_shown: wrapped.heading.len() + 1 < usize::from(area.height),
        }
    }

    fn footer_hint(&self) -> &'static str {
        match (&self.edit, &self.grant) {
            (Some(edit), _) => edit.footer_hint(),
            (None, Some(_)) => GRANT_FOOTER_HINT,
            (None, None) => FOOTER_HINT,
        }
    }

    fn body_visible(&self) -> bool {
        self.body.is_empty() || self.view.get().body_shown
    }

    fn page(&self) -> isize {
        let rows = self.view.get().rows.saturating_sub(1).max(1);
        isize::try_from(rows).unwrap_or(isize::MAX)
    }
}

impl Modal for ApprovalModal {
    fn height(&self, width: u16, max_height: u16) -> u16 {
        let rows = self.with_wrapped(width, None, |wrapped| {
            wrapped.heading.len()
                + CHROME_ROWS
                + self.edit_rows(width)
                + wrapped.body.len().min(MAX_BODY_ROWS)
        });
        saturating_u16(rows).min(max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let BodyWindow {
            mut lines,
            scroll,
            rows,
            total,
            body_shown,
        } = self.with_wrapped(area.width, Some(Palette::new(theme)), |wrapped| {
            self.body_window(wrapped, area)
        });
        let max_scroll = total - rows;

        if max_scroll > 0 && rows > 0 {
            let position = format!(
                "rows {}-{} of {total}{SCROLL_HINT}",
                scroll + 1,
                scroll + rows
            );
            lines.push(Line::from(Span::styled(position, theme.dim())));
        } else {
            lines.push(Line::default());
        }
        let edit = self.edit.as_ref();
        let mut editor_row = None;
        if let Some(edit) = edit {
            lines.push(Line::from(Span::styled(edit.label(), theme.text())));
            editor_row = Some(lines.len());
            lines.push(edit.editor.line(area.width, theme));
            if let Some(error) = &edit.error {
                lines.extend(error_lines(error, area.width, theme.error()));
            }
        }
        let budget = usize::from(area.width).max(MIN_BUDGET);
        lines.push(Line::from(Span::styled(
            truncate_to_width(self.footer_hint(), budget),
            theme.dim(),
        )));
        let editor_shown = edit.is_some() && lines.len() <= usize::from(area.height);
        self.view.set(View {
            rows,
            max_scroll,
            body_shown,
            editor_shown,
        });

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        if let (Some(edit), Some(row), true) = (edit, editor_row, editor_shown) {
            let y = area.y.saturating_add(saturating_u16(row));
            edit.editor.place_cursor(frame, area, y);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.kind != KeyEventKind::Press {
            return ModalKey::Consumed;
        }
        if let Some(outcome) = self.handle_edit_key(event) {
            return outcome;
        }
        let now = now();
        if now < self.guard_until {
            self.guard_until = now + TYPE_AHEAD_GUARD;
            return ModalKey::Consumed;
        }
        if let Some(delta) = self.line_scroll(event.code) {
            return self.scroll_by(delta);
        }
        // Esc and Ctrl+C are intercepted at the stack level and resolve through `on_cancel`.
        match event.code {
            KeyCode::Char('y' | 'Y') if is_plain_char(event) && self.body_visible() => {
                ModalKey::Submitted(self.decision(Decision::Approve))
            }
            KeyCode::Char('a' | 'A') if is_plain_char(event) && self.body_visible() => {
                match self.grant.take() {
                    Some(offer) => {
                        let pattern = offer.suggested().pattern().to_owned();
                        self.start_edit(Purpose::Rule(offer), &pattern)
                    }
                    None => ModalKey::Consumed,
                }
            }
            KeyCode::Char('n' | 'N') if is_plain_char(event) => {
                ModalKey::Submitted(self.decision(Decision::Deny))
            }
            KeyCode::Char('f' | 'F') if is_plain_char(event) => {
                self.start_edit(Purpose::Feedback, "")
            }
            KeyCode::Home => self.scroll_by(isize::MIN),
            KeyCode::End => self.scroll_by(isize::MAX),
            _ => ModalKey::Consumed,
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if let Some(edit) = &mut self.edit
            && edit.editor.paste(text)
        {
            edit.error = None;
        }
    }

    fn on_cancel(&mut self) -> Option<ModalAction> {
        Some(self.decision(Decision::Abort))
    }

    fn captures_esc(&self) -> bool {
        self.edit.is_some()
    }
}

// ── Body ──

/// A preview row before wrapping. Diff text has its tabs expanded.
struct BodyLine {
    kind: BodyKind,
    text: String,
}

#[derive(Clone, Copy)]
enum BodyKind {
    Command,
    Removed,
    Added,
}

impl BodyLine {
    fn to_line(&self, palette: &Palette) -> Line<'static> {
        let (sign, style) = match self.kind {
            BodyKind::Command => {
                return Line::from(reveal(&self.text, palette.text, palette.marker, true));
            }
            BodyKind::Removed => ("- ", palette.removed),
            BodyKind::Added => ("+ ", palette.added),
        };
        let mut spans = vec![Span::styled(sign, style)];
        spans.extend(reveal(&self.text, style, palette.marker, false));
        Line::from(spans)
    }
}

/// Theme styles a preview row needs. The default is unstyled and serves [`ApprovalModal::height`],
/// where only the row count matters.
#[derive(Clone, Copy, Default, PartialEq)]
struct Palette {
    title: Style,
    reason: Style,
    text: Style,
    removed: Style,
    added: Style,
    marker: Style,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        Self {
            title: theme.accent().add_modifier(Modifier::BOLD),
            reason: theme.muted(),
            text: theme.text(),
            removed: theme.error(),
            added: theme.success(),
            marker: theme.dim(),
        }
    }
}

/// Flattens an [`ApprovalBody`] into preview rows: a command's lines verbatim, or a diff's hunks as
/// `-` / `+` rows.
fn flatten_body(body: &ApprovalBody) -> Vec<BodyLine> {
    match body {
        ApprovalBody::Command(command) => command
            .lines()
            .map(|line| BodyLine {
                kind: BodyKind::Command,
                text: line.to_owned(),
            })
            .collect(),
        ApprovalBody::Diff(chunks) => diff_rows(chunks),
    }
}

fn diff_rows(chunks: &[DiffChunk]) -> Vec<BodyLine> {
    chunks
        .iter()
        .flat_map(|chunk| {
            let removed = chunk.old.iter().map(|line| (BodyKind::Removed, line));
            let added = chunk.new.iter().map(|line| (BodyKind::Added, line));
            removed.chain(added)
        })
        .map(|(kind, line)| BodyLine {
            kind,
            text: expand_tabs(&line.text),
        })
        .collect()
}

// ── Reveal ──

/// Spans for `text` with content a terminal would hide made visible. Control and zero-width
/// characters become `\u{..}`-style escapes. With `mark_whitespace`, each whitespace character past
/// the leading indentation becomes [`WHITESPACE_MARKER`] unless it is a lone ASCII space.
fn reveal(text: &str, style: Style, marker: Style, mark_whitespace: bool) -> Vec<Span<'static>> {
    let rest = text.trim_start_matches([' ', '\t']);
    let mut spans = Vec::new();
    let mut plain = expand_tabs(&text[..text.len() - rest.len()]);

    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if mark_whitespace && is_blank(c) {
            let mut run = 1;
            while chars.next_if(|&next| is_blank(next)).is_some() {
                run += 1;
            }
            if c == ' ' && run == 1 {
                plain.push(' ');
            } else {
                flush(&mut spans, &mut plain, style);
                let markers = WHITESPACE_MARKER.to_string().repeat(run);
                spans.push(Span::styled(markers, marker));
            }
        } else if is_hidden(c) {
            flush(&mut spans, &mut plain, style);
            spans.push(Span::styled(c.escape_default().to_string(), marker));
        } else {
            plain.push(c);
        }
    }
    flush(&mut spans, &mut plain, style);
    spans
}

/// A rejected rule's error, wrapped to the modal's width.
fn error_lines(error: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let budget = usize::from(width).max(MIN_BUDGET);
    wrap_line(
        Line::from(Span::styled(error.to_owned(), style)),
        budget,
        0,
        None,
    )
}

/// Returns whether `c` renders as blank cells. Other control whitespace (`\r`, `\n`) is escaped.
fn is_blank(c: char) -> bool {
    c == '\t' || (c.is_whitespace() && !c.is_control())
}

fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String, style: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), style));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crossterm::event::KeyModifiers;
    use indoc::{formatdoc, indoc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::permission::rule::Rule;
    use crate::permission::{Access, PermissionClass, Policy, Workspace};
    use crate::tool::DiffLine;
    use crate::tui::modal::ModalStack;
    use crate::tui::modal::testing::render_rows;

    fn fresh_command_modal(command: &str) -> ApprovalModal {
        ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(command.to_owned()),
            },
            None,
            None,
        )
    }

    /// A modal past its [`TYPE_AHEAD_GUARD`], so it answers keys.
    fn command_modal(command: &str) -> ApprovalModal {
        let mut modal = fresh_command_modal(command);
        modal.expire_guard();
        modal
    }

    fn numbered_command(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("echo line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_to_string(modal: &ApprovalModal, width: u16, height: u16) -> String {
        let mut out = String::new();
        for row in render_rows(modal, width, height) {
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    mod height {
        use super::*;

        #[test]
        fn counts_body_rows_plus_chrome() {
            // Single command line → 1 body row, plus title + blank + position row + footer = 4.
            let modal = command_modal("ls");
            assert_eq!(modal.height(40, 40), 5);
        }

        #[test]
        fn counts_wrapped_rows_and_caps_the_body_at_the_viewport() {
            // 100 unbroken columns at width 40 → 40 + 38 + 22 after the 2-column hanging indent.
            let modal = command_modal(&"x".repeat(100));
            assert_eq!(modal.height(40, 40), 7);
            assert_eq!(modal.height(100, 40), 5, "a new width rewraps");
            assert_eq!(command_modal(&numbered_command(40)).height(40, 40), 20);
            assert_eq!(
                command_modal(&numbered_command(40)).height(40, 9),
                9,
                "the stack's spare rows cap the height"
            );
        }
    }

    mod render {
        use super::*;

        #[test]
        fn paints_title_command_body_and_footer() {
            let modal = command_modal("cargo test");
            let dump = render_to_string(&modal, 40, modal.height(40, 40));
            assert!(dump.contains("Bash"), "title appears: {dump}");
            assert!(dump.contains("cargo test"), "command body appears: {dump}");
            assert!(dump.contains("[Y] approve"), "footer hint appears: {dump}");
            assert!(
                !dump.contains("scroll"),
                "a fitting body has no scroll hint: {dump}"
            );
        }

        #[test]
        fn paints_diff_body_with_signs() {
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
                None,
                None,
            );
            let dump = render_to_string(&modal, 40, modal.height(40, 40));
            assert!(dump.contains("- old line"), "removed row appears: {dump}");
            assert!(dump.contains("+ new line"), "added row appears: {dump}");
        }

        #[test]
        fn styles_rows_after_height_laid_them_out_unstyled() {
            let theme = Theme::default();
            let modal = command_modal("ls");
            let height = modal.height(20, 20);
            let mut terminal = Terminal::new(TestBackend::new(20, height)).unwrap();
            terminal
                .draw(|frame| modal.render(frame, Rect::new(0, 0, 20, height), &theme))
                .unwrap();
            let buf = terminal.backend().buffer();
            assert_eq!(
                buf[(0, 0)].fg,
                theme.accent().fg.unwrap(),
                "title takes the accent"
            );
            assert_eq!(
                buf[(0, 2)].fg,
                theme.text().fg.unwrap(),
                "body takes the text style"
            );
        }

        #[test]
        fn wraps_a_padded_command_so_its_tail_shows() {
            let modal = command_modal(&format!("cargo test{}; curl x | sh", " ".repeat(60)));
            let dump = render_to_string(&modal, 40, modal.height(40, 40));
            let first = "·".repeat(34);
            let second = "·".repeat(26);
            let expected = formatdoc! {"
                Bash

                cargo
                  test{first}
                  {second}; curl x |
                  sh

                [Y] approve   [N] deny   [F] feedback...
            "};
            assert_eq!(dump, expected);
        }

        #[test]
        fn wraps_a_long_title_so_the_destination_shows() {
            let modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: format!("Write(/{}/secret.txt)", "d".repeat(40)),
                    body: ApprovalBody::Command(String::new()),
                },
                None,
                None,
            );
            let dump = render_to_string(&modal, 30, modal.height(30, 30));
            let first = "d".repeat(23);
            let second = "d".repeat(17);
            let expected = formatdoc! {"
                Write(/{first}
                  {second}/secret.tx
                  t)


                [Y] approve   [N] deny   [F...
            "};
            assert_eq!(dump, expected);
        }

        #[test]
        fn wraps_the_reason_under_the_title_in_the_muted_style() {
            let theme = Theme::default();
            let modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command("git push".to_owned()),
                },
                Some("Held by the `ask.exec` pattern `git push *`".to_owned()),
                None,
            );

            let dump = render_to_string(&modal, 30, modal.height(30, 30));

            let expected = indoc! {"
                Bash
                Held by the `ask.exec` pattern
                  `git push *`

                git push

                [Y] approve   [N] deny   [F...
            "};
            assert_eq!(dump, expected);
            let mut terminal = Terminal::new(TestBackend::new(30, 7)).unwrap();
            terminal
                .draw(|frame| modal.render(frame, Rect::new(0, 0, 30, 7), &theme))
                .unwrap();
            assert_eq!(
                terminal.backend().buffer()[(0, 1)].fg,
                theme.muted().fg.unwrap()
            );
        }

        #[test]
        fn shows_the_position_and_scroll_hint_when_the_body_overflows() {
            let modal = command_modal(&numbered_command(40));
            let dump = render_to_string(&modal, 40, modal.height(40, 40));
            assert!(
                dump.contains("echo line-15"),
                "last row of the first page: {dump}"
            );
            assert!(
                !dump.contains("echo line-16"),
                "rows past the viewport stay hidden: {dump}"
            );
            assert!(
                dump.contains("rows 1-16 of 40"),
                "position row appears: {dump}"
            );
            assert!(dump.contains("[↑↓] scroll"), "scroll hint appears: {dump}");
        }

        #[test]
        fn shrinks_the_viewport_to_fit_a_short_area() {
            let modal = command_modal(&numbered_command(40));
            let dump = render_to_string(&modal, 40, 9);
            assert!(
                dump.contains("rows 1-5 of 40"),
                "5 body rows fit under 4 chrome rows: {dump}"
            );
            assert!(
                dump.contains("[Y] approve"),
                "the footer still paints: {dump}"
            );
        }

        #[test]
        fn does_not_panic_at_minimum_widths() {
            let modal = command_modal("cargo test --all");
            for w in [4_u16, 8, 20] {
                render_to_string(&modal, w, modal.height(w, 40));
            }
        }
    }

    mod handle_key {
        use super::*;

        /// A modal the user has seen, since approval waits for the body to render.
        fn shown_modal(command: &str) -> ApprovalModal {
            let modal = command_modal(command);
            render_to_string(&modal, 40, modal.height(40, 40));
            modal
        }

        /// A shown modal offering the rule [`Policy::session_grant`] suggests for `command`.
        fn granted_modal(command: &str) -> ApprovalModal {
            let target = Access::for_command(command, &Workspace::new(Path::new("/repo")));
            let grant = Policy::default().session_grant(PermissionClass::Exec, &target);
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command(command.to_owned()),
                },
                None,
                grant.map(Box::new),
            );
            modal.expire_guard();
            render_to_string(&modal, 60, modal.height(60, 40));
            modal
        }

        fn type_text(modal: &mut ApprovalModal, text: &str) {
            for c in text.chars() {
                modal.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
        }

        #[test]
        fn non_press_key_events_are_ignored() {
            // Windows reports a release for every key, and a held key repeats, so neither approves.
            let mut modal = shown_modal("ls");
            for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
                let mut event = KeyEvent::from(KeyCode::Char('y'));
                event.kind = kind;
                assert!(
                    matches!(modal.handle_key(&event), ModalKey::Consumed),
                    "{kind:?}"
                );
            }
        }

        #[test]
        fn enter_keeps_editing_and_says_why_when_the_rule_no_longer_covers_the_call() {
            let mut modal = granted_modal("cargo test");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            modal.expire_guard();
            render_to_string(&modal, 60, modal.height(60, 40));
            for _ in 0.."test".len() {
                modal.handle_key(&KeyEvent::from(KeyCode::Backspace));
            }
            modal.handle_paste("build");

            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Consumed
            ));
            let height = modal.height(60, 40);
            let rejected = render_to_string(&modal, 60, height);
            assert!(
                rejected.contains("the pattern no longer covers this call"),
                "{rejected}"
            );

            // Editing again clears the error and gives its row back.
            modal.handle_key(&KeyEvent::from(KeyCode::Backspace));
            assert_eq!(modal.height(60, 40), height - 1);
        }

        #[tokio::test(start_paused = true)]
        async fn enter_waits_for_the_guard_and_a_render_that_shows_the_whole_editor() {
            let mut modal = granted_modal("cargo test");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            render_to_string(&modal, 60, modal.height(60, 40));
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "an Enter typed right behind the `a` must not grant the rule"
            );

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let cramped = render_to_string(&modal, 60, 5);
            assert!(!cramped.contains("[Enter]"), "{cramped}");
            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Consumed
            ));

            let full = render_to_string(&modal, 60, 6);
            assert!(full.contains("cargo test"), "{full}");
            assert!(
                full.contains("[Enter]"),
                "the body yields its rows to the editor: {full}"
            );
            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Submitted(_)
            ));
        }

        #[test]
        fn rule_editor_wraps_a_long_error_and_scrolls_the_body_with_up_and_down() {
            let command = format!("echo {}", "x".repeat(1_200));
            let mut modal = granted_modal(&command);
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            modal.expire_guard();
            render_to_string(&modal, 60, modal.height(60, 40));
            modal.handle_key(&KeyEvent::from(KeyCode::End));
            type_text(&mut modal, " $X");
            modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            let rejected = render_to_string(&modal, 60, modal.height(60, 40));
            let error: Vec<&str> = rejected
                .split(RULE_LABEL)
                .nth(1)
                .unwrap_or_default()
                .lines()
                .map(str::trim_end)
                .collect();
            assert!(
                error.windows(2).any(|lines| lines
                    == [
                        r#"Rule rejected: the word "$X" expands at run time. Quote it"#,
                        "to match it as written.",
                    ]),
                "{rejected}"
            );
            assert!(rejected.contains("rows 1-"), "{rejected}");

            modal.handle_key(&KeyEvent::from(KeyCode::Down));
            let scrolled = render_to_string(&modal, 60, modal.height(60, 40));
            assert!(scrolled.contains("rows 2-"), "{scrolled}");
            assert!(scrolled.contains("[Enter] allow for session"), "{scrolled}");
        }

        #[test]
        fn esc_in_an_editor_returns_to_the_choice_with_the_grant_still_offered() {
            for opener in ['a', 'f'] {
                let mut modal = granted_modal("cargo test");
                modal.handle_key(&KeyEvent::from(KeyCode::Char(opener)));
                modal.handle_key(&KeyEvent::from(KeyCode::Esc));
                assert!(!modal.captures_esc());
                let choice = render_to_string(&modal, 80, modal.height(80, 40));
                assert!(choice.contains("[A] allow for session"), "{choice}");

                let mut stack = ModalStack::default();
                modal.expire_guard();
                modal.handle_key(&KeyEvent::from(KeyCode::Char(opener)));
                stack.push(Box::new(modal));
                assert!(stack.handle_key(&KeyEvent::from(KeyCode::Esc)).is_none());
                assert!(stack.is_active());
                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));
                assert!(
                    matches!(
                        outcome,
                        Some(ModalAction::Dispatch(Action::Op(Op::Resolve {
                            decision: Decision::Abort,
                            ..
                        })))
                    ),
                    "a second Esc aborts from the choice: {outcome:?}"
                );
            }
        }

        #[tokio::test(start_paused = true)]
        async fn enter_typed_right_behind_f_does_not_submit_the_feedback() {
            let mut modal = shown_modal("ls");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('f')));
            render_to_string(&modal, 60, modal.height(60, 40));
            type_text(&mut modal, "no");
            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Enter)),
                ModalKey::Consumed
            ));
            assert!(modal.captures_esc(), "the editor stays open");
        }

        #[tokio::test(start_paused = true)]
        async fn keys_typed_ahead_of_the_modal_are_dropped() {
            let mut modal = fresh_command_modal("ls");
            render_to_string(&modal, 40, modal.height(40, 40));
            for code in [KeyCode::Char('y'), KeyCode::Char('n')] {
                assert!(
                    matches!(modal.handle_key(&KeyEvent::from(code)), ModalKey::Consumed),
                    "{code:?}"
                );
            }
        }

        #[tokio::test(start_paused = true)]
        async fn the_first_key_is_accepted_at_the_initial_guard_deadline() {
            let mut modal = fresh_command_modal("ls");
            render_to_string(&modal, 40, modal.height(40, 40));

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(matches!(outcome, ModalKey::Submitted(_)), "{outcome:?}");
        }

        #[tokio::test(start_paused = true)]
        async fn a_key_inside_the_guard_restarts_it() {
            // A paste arrives as a key burst, so a `y` deep inside one must still drop.
            let y = KeyEvent::from(KeyCode::Char('y'));
            let mut modal = fresh_command_modal("ls");
            render_to_string(&modal, 40, modal.height(40, 40));
            let just_inside = TYPE_AHEAD_GUARD
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            for _ in 0..2 {
                tokio::time::advance(just_inside).await;
                let outcome = modal.handle_key(&y);
                assert!(matches!(outcome, ModalKey::Consumed));
            }

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = modal.handle_key(&y);
            assert!(matches!(outcome, ModalKey::Submitted(_)), "{outcome:?}");
        }

        #[test]
        fn scroll_keys_move_within_the_rendered_viewport() {
            let mut modal = command_modal(&numbered_command(40));
            let height = modal.height(40, 40);
            render_to_string(&modal, 40, height);

            modal.handle_key(&KeyEvent::from(KeyCode::End));
            let dump = render_to_string(&modal, 40, height);
            assert!(
                dump.contains("echo line-39"),
                "End reaches the last row: {dump}"
            );
            assert!(dump.contains("rows 25-40 of 40"), "{dump}");

            modal.handle_key(&KeyEvent::from(KeyCode::Down));
            modal.handle_key(&KeyEvent::from(KeyCode::Up));
            let dump = render_to_string(&modal, 40, height);
            assert!(
                dump.contains("rows 24-39 of 40"),
                "Down past the end stays clamped: {dump}"
            );

            modal.handle_key(&KeyEvent::from(KeyCode::PageUp));
            let dump = render_to_string(&modal, 40, height);
            assert!(
                dump.contains("rows 9-24 of 40"),
                "a page keeps one row of overlap: {dump}"
            );

            modal.handle_key(&KeyEvent::from(KeyCode::PageDown));
            modal.handle_key(&KeyEvent::from(KeyCode::Home));
            let dump = render_to_string(&modal, 40, height);
            assert!(
                dump.contains("rows 1-16 of 40"),
                "Home returns to the top: {dump}"
            );
        }

        #[test]
        fn scroll_keys_do_nothing_when_the_body_fits() {
            let mut modal = command_modal("ls");
            render_to_string(&modal, 40, modal.height(40, 40));
            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Down)),
                ModalKey::Consumed
            ));
            assert_eq!(modal.scroll, 0);
        }

        #[test]
        fn y_submits_an_approve_decision() {
            for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
                let mut modal = shown_modal("ls");
                let outcome = modal.handle_key(&KeyEvent::from(code));
                let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resolve {
                    id,
                    decision,
                }))) = outcome
                else {
                    panic!("{code:?} must Submit an Approve decision, got {outcome:?}");
                };
                assert_eq!(id, "call-1");
                assert_eq!(decision, Decision::Approve);
            }
        }

        #[test]
        fn enter_and_modified_y_do_not_approve() {
            // Enter is the likeliest key to arrive mid-typing, so approving takes an explicit `y`.
            let mut modal = shown_modal("ls");
            for event in [
                KeyEvent::from(KeyCode::Enter),
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            ] {
                assert!(matches!(modal.handle_key(&event), ModalKey::Consumed));
            }
        }

        #[test]
        fn y_waits_until_a_body_row_has_shown() {
            let y = KeyEvent::from(KeyCode::Char('y'));
            let mut modal = command_modal("one\ntwo");
            assert!(
                matches!(modal.handle_key(&y), ModalKey::Consumed),
                "no render yet"
            );

            render_to_string(&modal, 40, 2);
            assert!(
                matches!(modal.handle_key(&y), ModalKey::Consumed),
                "body clipped"
            );

            render_to_string(&modal, 40, 3);
            assert!(matches!(modal.handle_key(&y), ModalKey::Submitted(_)));

            let mut empty = command_modal("");
            assert!(
                matches!(empty.handle_key(&y), ModalKey::Submitted(_)),
                "nothing to hide"
            );
        }

        #[test]
        fn a_edits_the_suggested_rule_and_enter_allows_it_for_the_session() {
            let mut modal = granted_modal("cargo test");
            let choice = render_to_string(&modal, 60, modal.height(60, 40));
            assert!(choice.contains("[A] allow for session"), "{choice}");

            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Char('a'))),
                ModalKey::Consumed
            ));
            assert!(modal.captures_esc());
            modal.expire_guard();
            let editing = render_to_string(&modal, 60, modal.height(60, 40));
            assert!(editing.contains(RULE_LABEL), "{editing}");
            assert!(editing.contains("cargo test"), "{editing}");
            assert!(editing.contains("[Enter] allow for session"), "{editing}");

            // `y` and `n` type into the pattern while it is being edited.
            type_text(&mut modal, "yn");
            assert!(modal.captures_esc());
            for _ in 0.."yn".len() {
                modal.handle_key(&KeyEvent::from(KeyCode::Backspace));
            }
            type_text(&mut modal, " *");
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resolve {
                id,
                decision,
            }))) = outcome
            else {
                panic!("Enter must submit the session rule, got {outcome:?}");
            };
            assert_eq!(id, "call-1");
            let Decision::AllowForSession { rule } = decision else {
                panic!("expected AllowForSession, got {decision:?}");
            };
            assert_eq!(
                *rule,
                Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()
            );
        }

        #[test]
        fn a_does_nothing_without_a_grant() {
            let mut modal = shown_modal("cargo test");
            let dump = render_to_string(&modal, 60, modal.height(60, 40));
            assert!(!dump.contains("[A]"), "{dump}");

            assert!(matches!(
                modal.handle_key(&KeyEvent::from(KeyCode::Char('a'))),
                ModalKey::Consumed
            ));
            assert!(!modal.captures_esc());
        }

        #[test]
        fn n_submits_a_deny_decision() {
            let mut modal = command_modal("ls");
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('n')));
            let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resolve {
                id,
                decision,
            }))) = outcome
            else {
                panic!("N must submit a Deny decision, got {outcome:?}");
            };
            assert_eq!(id, "call-1");
            assert_eq!(decision, Decision::Deny);
        }

        #[test]
        fn f_opens_a_feedback_editor_whose_enter_denies_with_the_text() {
            let mut modal = granted_modal("cargo test");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('f')));
            assert!(modal.captures_esc());
            modal.expire_guard();
            let editing = render_to_string(&modal, 60, modal.height(60, 40));
            assert!(editing.contains(FEEDBACK_LABEL), "{editing}");
            assert!(editing.contains("[Enter] deny with feedback"), "{editing}");
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            assert!(
                matches!(outcome, ModalKey::Consumed),
                "an empty answer submits nothing"
            );

            // The feedback holds the `y` and `n` answer keys, which the editor takes as text.
            type_text(&mut modal, " try nextest ");
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));
            let ModalKey::Submitted(ModalAction::Dispatch(Action::Op(Op::Resolve {
                id,
                decision,
            }))) = outcome
            else {
                panic!("Enter must submit the feedback, got {outcome:?}");
            };
            assert_eq!(id, "call-1");
            assert_eq!(
                decision,
                Decision::DenyWithFeedback {
                    feedback: "try nextest".to_owned()
                }
            );
        }

        #[test]
        fn unrecognized_keys_consume_silently() {
            let mut modal = command_modal("ls");
            for code in [KeyCode::Char('z'), KeyCode::Tab] {
                let outcome = modal.handle_key(&KeyEvent::from(code));
                assert!(matches!(outcome, ModalKey::Consumed));
            }
        }
    }

    mod on_cancel {
        use super::*;

        #[test]
        fn aborts_the_turn_for_this_call() {
            let mut modal = command_modal("ls");
            let action = modal
                .on_cancel()
                .expect("approval cancel must resolve the call");
            let ModalAction::Dispatch(Action::Op(Op::Resolve { id, decision })) = action else {
                panic!("on_cancel must resolve via Op::Resolve, got {action:?}");
            };
            assert_eq!(id, "call-1");
            assert_eq!(decision, Decision::Abort);
        }
    }

    mod flatten_body {
        use super::*;

        #[test]
        fn expands_diff_tabs() {
            let rows = flatten_body(&ApprovalBody::Diff(vec![DiffChunk {
                old: Vec::new(),
                new: vec![DiffLine {
                    number: 1,
                    text: "\tfoo()".to_owned(),
                }],
            }]));
            assert_eq!(rows[0].text, "    foo()");
        }
    }

    mod reveal {
        use super::*;

        fn texts(spans: &[Span<'_>]) -> String {
            spans.iter().map(|span| span.content.as_ref()).collect()
        }

        #[test]
        fn marks_whitespace_runs_but_keeps_indentation_and_single_spaces() {
            let dim = Style::default().add_modifier(Modifier::DIM);
            let spans = reveal("\ta  b\tc d\u{a0}e", Style::default(), dim, true);
            assert_eq!(texts(&spans), "    a··b·c d·e");
            let runs: Vec<_> = spans
                .iter()
                .filter(|span| span.style == dim)
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(runs, ["··", "·", "·"], "each run is one dim span");
        }

        #[test]
        fn escapes_control_and_zero_width_characters() {
            let spans = reveal(
                "safe\rrm\u{1b}[2J\u{200b}\u{202e}x",
                Style::default(),
                Style::default(),
                true,
            );
            assert_eq!(texts(&spans), r"safe\rrm\u{1b}[2J\u{200b}\u{202e}x");
        }

        #[test]
        fn without_whitespace_marking_keeps_runs_and_escapes_tabs() {
            let spans = reveal("a  b\tc", Style::default(), Style::default(), false);
            assert_eq!(texts(&spans), r"a  b\tc");
        }
    }
}
