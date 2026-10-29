//! Tool-call approval overlay. Surfaces a gated call's preview and the rule that holds it, and
//! resolves the blocked agent to `Approve`, `AllowForSession` with a rule the user can edit first,
//! `Deny`, `DenyWithFeedback` with typed text, or `Abort`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use ox_core::permission::SessionGrant;
use ox_core::protocol::{ApprovalDecision, Op};
use ox_core::tool::view::{ApprovalBody, ApprovalPreview, Diff, DiffLine, DiffLineKind, DiffRow};
use ox_core::util::text::is_hidden;

use super::acceptance::Acceptance;
use super::chrome::{HINT_SEPARATOR, KEY_UP_DOWN, wrap_hints};
use super::line_editor::LineEditor;
use super::{Modal, ModalAction, ModalKey};
use crate::action::Action;
use crate::glyphs::{DIFF_SIGN_WIDTH, HUNK_GAP, diff_cap_row, diff_sign};
use crate::keymap::{KeyChord, hint};
use crate::saturating_u16;
use crate::theme::Theme;
use crate::wrap::{expand_tabs, expand_tabs_in_spans, wrap_line};

// ── Constants ──

const RULE_LABEL: &str = "Allow for the rest of this session:";
const FEEDBACK_LABEL: &str = "Deny, and tell the model why:";

/// Width floor so narrow terminals still paint the body without panicking.
const MIN_BUDGET: usize = 8;
/// Body rows painted at once. A longer preview scrolls.
const MAX_BODY_ROWS: usize = 16;
/// Blank row under the heading and position row under the body. The footer adds its own rows.
const CHROME_ROWS: usize = 2;
/// The label and the editor. A rejected answer's error wraps below them.
const EDIT_ROWS: usize = 2;
const CONTINUATION_INDENT: usize = 2;

/// Stands in for each character of a whitespace run a plain space would misrepresent.
const WHITESPACE_MARKER: char = '·';

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
    view: View,
    /// Reused across frames so a large preview is wrapped once per width and palette.
    wrapped: Option<Wrapped>,
    acceptance: Acceptance,
}

/// Viewport geometry from the last render.
#[derive(Clone, Copy, Default)]
struct View {
    rows: usize,
    max_scroll: usize,
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
    Submit(ApprovalDecision),
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
                        Ok(rule) => EditStep::Submit(ApprovalDecision::AllowForSession {
                            rule: Box::new(rule),
                        }),
                        Err(e) => {
                            self.error = Some(format!("Rule rejected: {e:#}"));
                            EditStep::Stay
                        }
                    },
                    Purpose::Feedback if text.is_empty() => EditStep::Stay,
                    Purpose::Feedback => EditStep::Submit(ApprovalDecision::DenyWithFeedback {
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

    fn footer_hints(&self) -> Vec<String> {
        let effect = match self.purpose {
            Purpose::Rule(_) => "allow for the session",
            Purpose::Feedback => "deny with feedback",
        };
        vec![
            hint(KeyChord::plain(KeyCode::Enter), effect),
            hint(KeyChord::plain(KeyCode::Esc), "go back"),
        ]
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
            view: View::default(),
            wrapped: None,
            acceptance: Acceptance::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_guard(&mut self) {
        self.acceptance.expire();
    }

    fn decision(&self, decision: ApprovalDecision) -> ModalAction {
        ModalAction::Dispatch(Action::Op(Op::Resolve {
            id: self.id.clone(),
            decision,
        }))
    }

    /// Re-wraps the rows for `width` unless the cache already holds them. A `None` palette accepts
    /// any cached palette.
    fn refresh_wrapped(&mut self, width: u16, palette: Option<Palette>) {
        let cached = self.wrapped.as_ref().is_some_and(|wrapped| {
            wrapped.width == width && palette.is_none_or(|palette| palette == wrapped.palette)
        });
        if !cached {
            self.wrapped = Some(self.wrap(width, palette.unwrap_or_default()));
        }
    }

    /// The rows [`Self::refresh_wrapped`] resolved.
    fn wrapped(&self) -> &Wrapped {
        self.wrapped.as_ref().expect("refresh_wrapped ran first")
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
            .flat_map(|line| {
                wrap_line(
                    line.to_line(&palette),
                    budget,
                    line.continuation_indent(),
                    None,
                )
            })
            .collect();
        Wrapped {
            width,
            palette,
            heading,
            body,
        }
    }

    fn scroll_by(&mut self, delta: isize) -> ModalKey {
        let max_scroll = self.view.max_scroll;
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
        self.acceptance.reset();
        ModalKey::Consumed
    }

    /// Routes `event` to the open editor, or returns `None` when none is open.
    fn handle_edit_key(&mut self, event: &KeyEvent) -> Option<ModalKey> {
        let scroll = self.line_scroll(event.code);
        let edit = self.edit.as_mut()?;
        let ready = event.code == KeyCode::Enter && self.acceptance.accepts(event);
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
                self.acceptance.reset();
                ModalKey::Consumed
            }
            EditStep::Submit(decision) => ModalKey::Submitted(self.decision(decision)),
        })
    }

    /// The scroll a line or page key asks for. Home and End are not among them, since the editors
    /// move their cursor with them.
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
        let chrome = CHROME_ROWS + self.edit_rows(area.width) + self.footer_rows(area.width).len();
        let room = usize::from(area.height)
            .saturating_sub(wrapped.heading.len() + chrome)
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
            body_shown: area.width > 0 && wrapped.heading.len() + 1 < usize::from(area.height),
        }
    }

    /// The footer at `width`, wrapped between hints so the Esc hint is never cut.
    fn footer_rows(&self, width: u16) -> Vec<String> {
        wrap_hints(&self.footer_hints(), usize::from(width).max(MIN_BUDGET))
    }

    /// The single-letter decisions, plus the grant key while the call offers a session rule.
    fn footer_hints(&self) -> Vec<String> {
        if let Some(edit) = &self.edit {
            return edit.footer_hints();
        }
        let mut hints = vec![hint(KeyChord::plain(KeyCode::Char('y')), "approve")];
        if self.grant.is_some() {
            hints.push(hint(
                KeyChord::plain(KeyCode::Char('a')),
                "allow for the session",
            ));
        }
        hints.extend([
            hint(KeyChord::plain(KeyCode::Char('n')), "deny"),
            hint(KeyChord::plain(KeyCode::Char('f')), "give feedback"),
            hint(KeyChord::plain(KeyCode::Esc), "abort"),
        ]);
        hints
    }

    fn page(&self) -> isize {
        let rows = self.view.rows.saturating_sub(1).max(1);
        isize::try_from(rows).unwrap_or(isize::MAX)
    }
}

impl Modal for ApprovalModal {
    fn height(&mut self, width: u16, max_height: u16) -> u16 {
        self.refresh_wrapped(width, None);
        let rows = self.wrapped().heading.len()
            + CHROME_ROWS
            + self.edit_rows(width)
            + self.footer_rows(width).len()
            + self.wrapped().body.len().min(MAX_BODY_ROWS);
        saturating_u16(rows).min(max_height)
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        self.refresh_wrapped(area.width, Some(Palette::new(theme)));
        let BodyWindow {
            mut lines,
            scroll,
            rows,
            total,
            body_shown,
        } = self.body_window(self.wrapped(), area);
        let max_scroll = total - rows;

        if max_scroll > 0 && rows > 0 {
            let position = format!(
                "rows {}-{} of {total}{HINT_SEPARATOR}{}",
                scroll + 1,
                scroll + rows,
                hint(KEY_UP_DOWN, "scroll"),
            );
            lines.push(Line::from(Span::styled(position, theme.dim())));
        } else {
            lines.push(Line::default());
        }
        let has_edit = self.edit.is_some();
        let mut editor_row = None;
        if let Some(edit) = self.edit.as_mut() {
            lines.push(Line::from(Span::styled(edit.label(), theme.text())));
            editor_row = Some(lines.len());
            lines.push(edit.editor.line(area.width, theme));
            if let Some(error) = &edit.error {
                lines.extend(error_lines(error, area.width, theme.error()));
            }
        }
        lines.extend(
            self.footer_rows(area.width)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, theme.dim()))),
        );
        let editor_shown = area.width > 0 && has_edit && lines.len() <= usize::from(area.height);
        self.view = View { rows, max_scroll };
        self.acceptance.update(if has_edit {
            editor_shown
        } else if self.body.is_empty() {
            !area.is_empty()
        } else {
            body_shown
        });

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
        if let (Some(edit), Some(row), true) = (self.edit.as_mut(), editor_row, editor_shown) {
            let y = area.y.saturating_add(saturating_u16(row));
            edit.editor.place_cursor(frame, area, y);
        }
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if let Some(outcome) = self.handle_edit_key(event) {
            return outcome;
        }
        if self.acceptance.guard() {
            return ModalKey::Consumed;
        }
        if let Some(delta) = self.line_scroll(event.code) {
            return self.scroll_by(delta);
        }
        // Esc and Ctrl+C are intercepted at the stack level and resolve through `on_cancel`.
        match event.code {
            KeyCode::Char('y' | 'Y') if self.acceptance.accepts(event) => {
                ModalKey::Submitted(self.decision(ApprovalDecision::Approve))
            }
            KeyCode::Char('a' | 'A') if self.acceptance.accepts(event) => match self.grant.take() {
                Some(offer) => {
                    let pattern = offer.suggested().pattern().to_owned();
                    self.start_edit(Purpose::Rule(offer), &pattern)
                }
                None => ModalKey::Consumed,
            },
            KeyCode::Char('n' | 'N') => ModalKey::Submitted(self.decision(ApprovalDecision::Deny)),
            KeyCode::Char('f' | 'F') => self.start_edit(Purpose::Feedback, ""),
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
        Some(self.decision(ApprovalDecision::Abort))
    }

    fn captures_esc(&self) -> bool {
        self.edit.is_some()
    }

    /// Takes every key while the type-ahead guard is armed, so a modified key typed into the input
    /// restarts it too.
    fn binds_modified_key(&self, event: &KeyEvent) -> bool {
        self.acceptance.is_guarded() || self.edit.is_some() && LineEditor::binds_modified_key(event)
    }
}

// ── Body ──

/// A preview row before wrapping.
enum BodyLine {
    Command(String),
    /// A diff line behind its number, right-aligned to the widest number in the diff.
    Diff {
        number: String,
        line: DiffLine,
    },
    /// A marker row: the break between two hunks, as wide as the number column, or the count of
    /// lines the diff left out.
    Marker(String),
}

impl BodyLine {
    fn to_line(&self, palette: &Palette) -> Line<'static> {
        let (number, line) = match self {
            Self::Command(text) => {
                return Line::from(reveal(text, palette.text, palette.marker, true));
            }
            Self::Marker(text) => return Line::from(Span::styled(text.clone(), palette.marker)),
            Self::Diff { number, line } => (number, line),
        };
        let (style, word) = match line.kind {
            DiffLineKind::Context => (palette.context, palette.context),
            DiffLineKind::Removed => (palette.removed, palette.removed_word),
            DiffLineKind::Added => (palette.added, palette.added_word),
        };
        let mut spans = vec![
            Span::styled(number.clone(), palette.number),
            Span::styled(diff_sign(line.kind), style),
        ];
        let segments = line.segments().into_iter().map(|(text, changed)| {
            Span::styled(text.to_owned(), if changed { word } else { style })
        });
        for segment in expand_tabs_in_spans(segments.collect()) {
            spans.extend(reveal(
                &segment.content,
                segment.style,
                palette.marker,
                false,
            ));
        }
        Line::from(spans)
    }

    /// Where a wrapped row continues: under a diff line's text, or indented under a command.
    fn continuation_indent(&self) -> usize {
        match self {
            Self::Diff { number, .. } => number.len() + DIFF_SIGN_WIDTH,
            Self::Command(_) | Self::Marker(_) => CONTINUATION_INDENT,
        }
    }
}

/// Theme styles a preview row needs. The default is unstyled and serves [`ApprovalModal::height`],
/// where only the row count matters.
#[derive(Clone, Copy, Default, PartialEq)]
struct Palette {
    title: Style,
    reason: Style,
    text: Style,
    number: Style,
    context: Style,
    removed: Style,
    removed_word: Style,
    added: Style,
    added_word: Style,
    marker: Style,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        Self {
            title: theme.accent().add_modifier(Modifier::BOLD),
            reason: theme.muted(),
            text: theme.text(),
            number: theme.muted(),
            context: theme.dim(),
            removed: theme.error(),
            removed_word: theme.error().patch(theme.diff_del_word()),
            added: theme.success(),
            added_word: theme.success().patch(theme.diff_add_word()),
            marker: theme.dim(),
        }
    }
}

/// Flattens an [`ApprovalBody`] into preview rows: a command's lines verbatim, or a diff's hunks as
/// numbered rows with a gap row between hunks.
fn flatten_body(body: &ApprovalBody) -> Vec<BodyLine> {
    match body {
        // Not `lines`, which would drop a `\r` before each `\n` that the preview must reveal.
        ApprovalBody::Command(command) => command
            .split_terminator('\n')
            .map(|line| BodyLine::Command(line.to_owned()))
            .collect(),
        ApprovalBody::Diff(diff) => diff_rows(diff),
    }
}

fn diff_rows(diff: &Diff) -> Vec<BodyLine> {
    let width = diff.max_number().to_string().len();
    diff.rows()
        .map(|row| match row {
            DiffRow::Line(line) => BodyLine::Diff {
                number: format!("{:>width$}", line.number),
                line: line.clone(),
            },
            DiffRow::Gap => BodyLine::Marker(format!("{HUNK_GAP:>width$}")),
            DiffRow::Omitted(lines) => BodyLine::Marker(diff_cap_row(lines)),
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
    use std::ops::Range;
    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use indoc::{formatdoc, indoc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use ox_core::permission::PermissionClass;
    use ox_core::permission::rule::Rule;
    use ox_core::permission::testing::exec_grant;
    use ox_core::tool::testing::diff_line;
    use ox_core::tool::view::DiffHunk;

    use super::*;
    use crate::modal::ModalStack;
    use crate::modal::acceptance::TYPE_AHEAD_GUARD;
    use crate::modal::testing::render_rows;

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
        render_rows(&mut modal, 60, 40);
        modal.expire_guard();
        modal
    }

    fn numbered_command(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("echo line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_to_string(modal: &mut ApprovalModal, width: u16, height: u16) -> String {
        let mut out = String::new();
        for row in render_rows(modal, width, height) {
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    /// Renders `modal` at `width` and the height it asks for within `max_height`.
    fn rendered(modal: &mut ApprovalModal, width: u16, max_height: u16) -> String {
        let height = modal.height(width, max_height);
        render_to_string(modal, width, height)
    }

    mod height {
        use super::*;

        #[test]
        fn counts_body_rows_plus_chrome() {
            // Title, blank, one body row, position row, and a footer that wraps onto two rows.
            let mut modal = command_modal("ls");

            assert_eq!(modal.height(40, 40), 6);
        }

        #[test]
        fn counts_wrapped_rows_and_caps_the_body_at_the_viewport() {
            // 100 unbroken columns at width 40 → 40 + 38 + 22 after the 2-column hanging indent.
            let mut modal = command_modal(&"x".repeat(100));

            assert_eq!(modal.height(40, 40), 8);
            assert_eq!(modal.height(100, 40), 5, "a new width rewraps");
            assert_eq!(command_modal(&numbered_command(40)).height(40, 40), 21);
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
            let mut modal = command_modal("cargo test");

            let dump = rendered(&mut modal, 40, 40);

            assert!(dump.contains("Bash"), "title appears: {dump}");
            assert!(dump.contains("cargo test"), "command body appears: {dump}");
            assert!(dump.contains("Y to approve"), "footer hint appears: {dump}");
            assert!(
                !dump.contains("scroll"),
                "a fitting body has no scroll hint: {dump}"
            );
        }

        #[test]
        fn paints_numbered_hunks_with_a_gap_between_them() {
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Write(src/main.rs)".to_owned(),
                    body: ApprovalBody::Diff(Diff {
                        hunks: vec![
                            DiffHunk {
                                lines: vec![
                                    diff_line(DiffLineKind::Context, 8, "keep"),
                                    diff_line(DiffLineKind::Removed, 9, "old line"),
                                    diff_line(DiffLineKind::Added, 9, "new line"),
                                ],
                            },
                            DiffHunk {
                                lines: vec![diff_line(DiffLineKind::Added, 20, "tail")],
                            },
                        ],
                        omitted_lines: 0,
                    }),
                },
                None,
                None,
            );

            let dump = rendered(&mut modal, 40, 40);

            let body = indoc! {"
                 8 │ keep
                 9 - old line
                 9 + new line
                 ⋮
                20 + tail
            "};
            assert!(dump.contains(body), "numbered hunks appear: {dump}");
        }

        #[test]
        fn wraps_a_diff_line_under_its_text() {
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Edit(a.rs)".to_owned(),
                    body: ApprovalBody::Diff(Diff {
                        hunks: vec![DiffHunk {
                            lines: vec![diff_line(DiffLineKind::Added, 7, "alpha beta gamma")],
                        }],
                        omitted_lines: 0,
                    }),
                },
                None,
                None,
            );

            let dump = rendered(&mut modal, 14, 20);

            assert!(
                dump.contains("7 + alpha beta\n    gamma\n"),
                "continuations align under the text: {dump}"
            );
        }

        #[test]
        fn styles_rows_after_height_laid_them_out_unstyled() {
            let theme = Theme::default();
            let mut modal = command_modal("ls");
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
            let mut modal = command_modal(&format!("cargo test{}; curl x | sh", " ".repeat(60)));

            let dump = rendered(&mut modal, 40, 40);

            let first = "·".repeat(34);
            let second = "·".repeat(26);
            let expected = formatdoc! {"
                Bash

                cargo
                  test{first}
                  {second}; curl x |
                  sh

                Y to approve  ·  N to deny
                F to give feedback  ·  Esc to abort
            "};
            assert_eq!(dump, expected);
        }

        #[test]
        fn wraps_a_long_title_so_the_destination_shows() {
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: format!("Write(/{}/secret.txt)", "d".repeat(40)),
                    body: ApprovalBody::Command(String::new()),
                },
                None,
                None,
            );

            let dump = rendered(&mut modal, 30, 30);

            let first = "d".repeat(23);
            let second = "d".repeat(17);
            let expected = formatdoc! {"
                Write(/{first}
                  {second}/secret.tx
                  t)


                Y to approve  ·  N to deny
                F to give feedback
                Esc to abort
            "};
            assert_eq!(dump, expected);
        }

        #[test]
        fn wraps_the_reason_under_the_title_in_the_muted_style() {
            let theme = Theme::default();
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command("git push".to_owned()),
                },
                Some("Asks because of the `ask.exec` pattern `git push *`".to_owned()),
                None,
            );

            let dump = rendered(&mut modal, 30, 30);

            let expected = indoc! {"
                Bash
                Asks because of the `ask.exec`
                  pattern `git push *`

                git push

                Y to approve  ·  N to deny
                F to give feedback
                Esc to abort
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
            let mut modal = command_modal(&numbered_command(40));

            let dump = rendered(&mut modal, 40, 40);

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
            assert!(
                dump.contains("Up / Down to scroll"),
                "scroll hint appears: {dump}"
            );
        }

        #[test]
        fn shrinks_the_viewport_to_fit_a_short_area() {
            let mut modal = command_modal(&numbered_command(40));

            let dump = render_to_string(&mut modal, 40, 9);

            assert!(
                dump.contains("rows 1-4 of 40"),
                "4 body rows fit under 5 chrome rows: {dump}"
            );
            assert!(
                dump.contains("Y to approve"),
                "the footer still paints: {dump}"
            );
        }

        #[test]
        fn does_not_panic_at_minimum_widths() {
            let mut modal = command_modal("cargo test --all");

            for w in [4_u16, 8, 20] {
                rendered(&mut modal, w, 40);
            }
        }
    }

    mod handle_key {
        use super::*;

        /// A modal the user has seen, since approval waits for the body to render.
        fn shown_modal(command: &str) -> ApprovalModal {
            let mut modal = command_modal(command);
            rendered(&mut modal, 40, 40);
            modal
        }

        /// A shown modal offering the session grant the default policy suggests for `command`.
        fn granted_modal(command: &str) -> ApprovalModal {
            let mut modal = ApprovalModal::new(
                "call-1".to_owned(),
                ApprovalPreview {
                    title: "Bash".to_owned(),
                    body: ApprovalBody::Command(command.to_owned()),
                },
                None,
                Some(Box::new(exec_grant(command))),
            );
            rendered(&mut modal, 60, 40);
            modal.expire_guard();
            modal
        }

        fn type_text(modal: &mut ApprovalModal, text: &str) {
            for c in text.chars() {
                modal.handle_key(&KeyEvent::from(KeyCode::Char(c)));
            }
        }

        #[test]
        fn enter_keeps_editing_and_says_why_when_the_rule_no_longer_covers_the_call() {
            let mut modal = granted_modal("cargo test");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            rendered(&mut modal, 60, 40);
            modal.expire_guard();
            for _ in 0.."test".len() {
                modal.handle_key(&KeyEvent::from(KeyCode::Backspace));
            }
            modal.handle_paste("build");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(matches!(outcome, ModalKey::Consumed));
            let height = modal.height(60, 40);
            let rejected = render_to_string(&mut modal, 60, height);
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
            rendered(&mut modal, 60, 40);

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(
                matches!(outcome, ModalKey::Consumed),
                "an Enter typed right behind the `a` must not grant the rule"
            );

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let cramped = render_to_string(&mut modal, 60, 5);
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(
                !cramped.contains("Enter to allow for the session"),
                "{cramped}"
            );
            assert!(matches!(outcome, ModalKey::Consumed));

            let full = render_to_string(&mut modal, 60, 6);
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(full.contains("cargo test"), "{full}");
            assert!(
                full.contains("Enter to allow for the session"),
                "the body yields its rows to the editor: {full}"
            );
            assert!(matches!(outcome, ModalKey::Submitted(_)));
        }

        #[test]
        fn rule_editor_wraps_a_long_error_and_scrolls_the_body_with_up_and_down() {
            let command = format!("echo {}", "x".repeat(1_200));
            let mut modal = granted_modal(&command);
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            rendered(&mut modal, 60, 40);
            modal.expire_guard();
            modal.handle_key(&KeyEvent::from(KeyCode::End));
            type_text(&mut modal, " $X");

            modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            let rejected = rendered(&mut modal, 60, 40);
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

            let scrolled = rendered(&mut modal, 60, 40);
            assert!(scrolled.contains("rows 2-"), "{scrolled}");
            assert!(
                scrolled.contains("Enter to allow for the session"),
                "{scrolled}"
            );
        }

        #[test]
        fn esc_in_an_editor_returns_to_the_choice_with_the_grant_still_offered() {
            for opener in ['a', 'f'] {
                let mut modal = granted_modal("cargo test");
                modal.handle_key(&KeyEvent::from(KeyCode::Char(opener)));

                modal.handle_key(&KeyEvent::from(KeyCode::Esc));

                assert!(!modal.captures_esc());
                let choice = rendered(&mut modal, 80, 40);
                assert!(choice.contains("A to allow for the session"), "{choice}");

                let mut stack = ModalStack::default();
                modal.expire_guard();
                modal.handle_key(&KeyEvent::from(KeyCode::Char(opener)));
                stack.push(Box::new(modal));

                let first = stack.handle_key(&KeyEvent::from(KeyCode::Esc));

                assert!(first.is_none());
                assert!(stack.is_active());

                let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Esc));

                assert!(
                    matches!(
                        outcome,
                        Some(ModalAction::Dispatch(Action::Op(Op::Resolve {
                            decision: ApprovalDecision::Abort,
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
            rendered(&mut modal, 60, 40);
            type_text(&mut modal, "no");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Enter));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert!(modal.captures_esc(), "the editor stays open");
        }

        #[tokio::test(start_paused = true)]
        async fn keys_typed_ahead_of_the_modal_are_dropped() {
            let mut modal = fresh_command_modal("ls");
            rendered(&mut modal, 40, 40);

            for code in [KeyCode::Char('y'), KeyCode::Char('n')] {
                let outcome = modal.handle_key(&KeyEvent::from(code));

                assert!(matches!(outcome, ModalKey::Consumed), "{code:?}");
            }
        }

        #[tokio::test(start_paused = true)]
        async fn the_first_key_is_accepted_at_the_initial_guard_deadline() {
            let mut modal = fresh_command_modal("ls");
            rendered(&mut modal, 40, 40);

            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(matches!(outcome, ModalKey::Submitted(_)), "{outcome:?}");
        }

        #[tokio::test(start_paused = true)]
        async fn a_key_inside_the_guard_restarts_it() {
            // A paste arrives as a key burst, so a `y` deep inside one must still drop.
            let y = KeyEvent::from(KeyCode::Char('y'));
            let mut modal = fresh_command_modal("ls");
            rendered(&mut modal, 40, 40);
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
            render_to_string(&mut modal, 40, height);

            modal.handle_key(&KeyEvent::from(KeyCode::End));

            let dump = render_to_string(&mut modal, 40, height);
            assert!(
                dump.contains("echo line-39"),
                "End reaches the last row: {dump}"
            );
            assert!(dump.contains("rows 25-40 of 40"), "{dump}");

            modal.handle_key(&KeyEvent::from(KeyCode::Down));
            modal.handle_key(&KeyEvent::from(KeyCode::Up));

            let dump = render_to_string(&mut modal, 40, height);
            assert!(
                dump.contains("rows 24-39 of 40"),
                "Down past the end stays clamped: {dump}"
            );

            modal.handle_key(&KeyEvent::from(KeyCode::PageUp));

            let dump = render_to_string(&mut modal, 40, height);
            assert!(
                dump.contains("rows 9-24 of 40"),
                "a page keeps one row of overlap: {dump}"
            );

            modal.handle_key(&KeyEvent::from(KeyCode::PageDown));
            modal.handle_key(&KeyEvent::from(KeyCode::Home));

            let dump = render_to_string(&mut modal, 40, height);
            assert!(
                dump.contains("rows 1-16 of 40"),
                "Home returns to the top: {dump}"
            );
        }

        #[test]
        fn scroll_keys_do_nothing_when_the_body_fits() {
            let mut modal = command_modal("ls");
            rendered(&mut modal, 40, 40);

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Down));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert_eq!(modal.scroll, 0);
        }

        #[tokio::test(start_paused = true)]
        async fn a_modified_key_inside_the_guard_restarts_it() {
            let mut stack = ModalStack::default();
            let mut modal = fresh_command_modal("ls");
            rendered(&mut modal, 40, 40);
            stack.push(Box::new(modal));
            let half = TYPE_AHEAD_GUARD / 2 + Duration::from_millis(50);

            tokio::time::advance(half).await;
            stack.handle_key(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
            tokio::time::advance(half).await;
            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(outcome.is_none(), "{outcome:?}");
            assert!(stack.is_active());
        }

        #[test]
        fn shift_y_through_the_stack_approves() {
            let mut stack = ModalStack::default();
            stack.push(Box::new(shown_modal("ls")));

            let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT));

            assert!(
                matches!(
                    outcome,
                    Some(ModalAction::Dispatch(Action::Op(Op::Resolve {
                        decision: ApprovalDecision::Approve,
                        ..
                    })))
                ),
                "{outcome:?}"
            );
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
                assert_eq!(decision, ApprovalDecision::Approve);
            }
        }

        #[test]
        fn enter_and_modified_answer_keys_do_nothing() {
            // Enter is the likeliest key to arrive mid-typing, so approving takes an explicit `y`.
            let mut stack = ModalStack::default();
            stack.push(Box::new(granted_modal("cargo test")));
            let modified = ['y', 'n', 'a', 'f'].into_iter().flat_map(|c| {
                [KeyModifiers::CONTROL, KeyModifiers::ALT]
                    .map(|modifiers| KeyEvent::new(KeyCode::Char(c), modifiers))
            });
            let enters = [KeyModifiers::NONE, KeyModifiers::CONTROL]
                .map(|modifiers| KeyEvent::new(KeyCode::Enter, modifiers));

            for event in enters.into_iter().chain(modified) {
                let outcome = stack.handle_key(&event);

                assert!(outcome.is_none(), "{event:?}");
            }

            let approved = stack.handle_key(&KeyEvent::from(KeyCode::Char('y')));

            assert!(
                matches!(
                    approved,
                    Some(ModalAction::Dispatch(Action::Op(Op::Resolve {
                        decision: ApprovalDecision::Approve,
                        ..
                    })))
                ),
                "no modified key opened an editor or answered: {approved:?}",
            );
        }

        #[tokio::test(start_paused = true)]
        async fn y_waits_until_a_body_row_has_shown() {
            let y = KeyEvent::from(KeyCode::Char('y'));
            let mut modal = fresh_command_modal("one\ntwo");
            modal.expire_guard();

            let outcome = modal.handle_key(&y);

            assert!(matches!(outcome, ModalKey::Consumed), "no render yet");

            render_to_string(&mut modal, 40, 2);
            let outcome = modal.handle_key(&y);

            assert!(matches!(outcome, ModalKey::Consumed), "body clipped");

            let shown = render_to_string(&mut modal, 40, 3);
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = modal.handle_key(&y);

            assert!(shown.contains("one"), "{shown}");
            assert!(matches!(outcome, ModalKey::Submitted(_)));

            let mut empty = fresh_command_modal("");
            render_to_string(&mut empty, 40, 1);
            tokio::time::advance(TYPE_AHEAD_GUARD).await;
            let outcome = empty.handle_key(&y);

            assert!(matches!(outcome, ModalKey::Submitted(_)), "heading shown");
        }

        #[test]
        fn a_edits_the_suggested_rule_and_enter_allows_it_for_the_session() {
            let mut modal = granted_modal("cargo test");

            let choice = rendered(&mut modal, 60, 40);
            assert!(choice.contains("A to allow for the session"), "{choice}");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));

            assert!(matches!(outcome, ModalKey::Consumed));
            assert!(modal.captures_esc());

            let editing = rendered(&mut modal, 60, 40);
            modal.expire_guard();

            assert!(editing.contains(RULE_LABEL), "{editing}");
            assert!(editing.contains("cargo test"), "{editing}");
            assert!(
                editing.contains("Enter to allow for the session"),
                "{editing}"
            );

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
            let ApprovalDecision::AllowForSession { rule } = decision else {
                panic!("expected AllowForSession, got {decision:?}");
            };
            assert_eq!(
                *rule,
                Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()
            );
        }

        #[test]
        fn modified_editor_keys_edit_the_rule_without_submitting_it() {
            let mut modal = granted_modal("cargo test");
            modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));
            rendered(&mut modal, 60, 40);
            modal.expire_guard();
            let mut stack = ModalStack::default();
            stack.push(Box::new(modal));

            for code in [KeyCode::Char('a'), KeyCode::Char('k')] {
                let outcome = stack.handle_key(&KeyEvent::new(code, KeyModifiers::CONTROL));
                assert!(outcome.is_none(), "{code:?}: {outcome:?}");
            }
            stack.handle_paste("cargo test *");
            let outcome = stack.handle_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));

            assert!(
                outcome.is_none(),
                "Ctrl+Enter must keep editing: {outcome:?}"
            );
            assert!(stack.is_active());

            let outcome = stack.handle_key(&KeyEvent::from(KeyCode::Enter));

            let expected = ApprovalDecision::AllowForSession {
                rule: Box::new(Rule::parse(PermissionClass::Exec, "cargo test *").unwrap()),
            };
            assert!(
                matches!(outcome, Some(ModalAction::Dispatch(Action::Op(Op::Resolve {
                ref id, ref decision,
            }))) if id == "call-1" && decision == &expected),
                "{outcome:?}"
            );
            assert!(!stack.is_active());
        }

        #[test]
        fn a_does_nothing_without_a_grant() {
            let mut modal = shown_modal("cargo test");

            let dump = rendered(&mut modal, 60, 40);
            assert!(!dump.contains("[A]"), "{dump}");

            let outcome = modal.handle_key(&KeyEvent::from(KeyCode::Char('a')));

            assert!(matches!(outcome, ModalKey::Consumed));
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
            assert_eq!(decision, ApprovalDecision::Deny);
        }

        #[test]
        fn f_opens_a_feedback_editor_whose_enter_denies_with_the_text() {
            let mut modal = granted_modal("cargo test");

            modal.handle_key(&KeyEvent::from(KeyCode::Char('f')));

            assert!(modal.captures_esc());

            let editing = rendered(&mut modal, 60, 40);
            modal.expire_guard();

            assert!(editing.contains(FEEDBACK_LABEL), "{editing}");
            assert!(editing.contains("Enter to deny with feedback"), "{editing}");

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
                ApprovalDecision::DenyWithFeedback {
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
            assert_eq!(decision, ApprovalDecision::Abort);
        }
    }

    mod flatten_body {
        use super::*;

        #[test]
        fn keeps_a_carriage_return_before_a_line_break_in_its_row() {
            let rows = flatten_body(&ApprovalBody::Command("echo a\r\nls\n".to_owned()));

            let texts: Vec<&str> = rows
                .iter()
                .map(|row| match row {
                    BodyLine::Command(text) => text.as_str(),
                    _ => panic!("a command flattens to command rows"),
                })
                .collect();
            assert_eq!(texts, ["echo a\r", "ls"]);
        }

        #[test]
        fn expands_diff_tabs_and_marks_the_changed_words() {
            let theme = Theme::default();
            let palette = Palette::new(&theme);
            let line = DiffLine {
                changed: vec![Range { start: 1, end: 4 }],
                ..diff_line(DiffLineKind::Added, 9, "\tfoo()")
            };
            let rows = flatten_body(&ApprovalBody::Diff(Diff {
                hunks: vec![DiffHunk { lines: vec![line] }],
                omitted_lines: 0,
            }));

            let spans: Vec<_> = rows[0]
                .to_line(&palette)
                .spans
                .into_iter()
                .map(|span| (span.content.into_owned(), span.style))
                .collect();

            let (added, word) = (palette.added, palette.added_word);
            assert_eq!(
                spans,
                [
                    ("9".to_owned(), palette.number),
                    (" + ".to_owned(), added),
                    ("    ".to_owned(), added),
                    ("foo".to_owned(), word),
                    ("()".to_owned(), added),
                ],
            );
        }

        #[test]
        fn ends_a_capped_diff_with_the_count_it_left_out() {
            let theme = Theme::default();
            let palette = Palette::new(&theme);
            let rows = flatten_body(&ApprovalBody::Diff(Diff {
                hunks: vec![DiffHunk {
                    lines: vec![diff_line(DiffLineKind::Added, 9, "foo()")],
                }],
                omitted_lines: 1,
            }));

            let last = rows.last().unwrap().to_line(&palette);

            assert_eq!(rows.len(), 2);
            assert_eq!(last.to_string(), "... +1 line past the diff limit");
            assert_eq!(last.spans[0].style, palette.marker);
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
