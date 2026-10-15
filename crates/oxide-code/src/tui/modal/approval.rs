//! Tool-call approval overlay. Surfaces a gated call's preview and resolves the blocked agent to
//! `Approve` or `Deny`. Built from the `ConfirmDeleteSessionModal` template. See
//! `docs/design/tools/permissions.md`.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

use crate::agent::event::{ApprovalBody, ApprovalDecision, ApprovalPreview, UserAction};
use crate::tool::DiffChunk;
use crate::tui::modal::{Modal, ModalAction, ModalKey};
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs, wrap_line};
use crate::util::text::truncate_to_width;

// ── Constants ──

const FOOTER_HINT: &str = "[Y] approve   [N] deny";
const SCROLL_HINT: &str = "   [↑↓] scroll";
/// Width floor so narrow terminals still paint the body without panicking.
const MIN_BUDGET: usize = 8;
/// Body rows painted at once. A longer preview scrolls.
const MAX_BODY_ROWS: usize = 16;
/// Blank row under the title, position row under the body, and the footer.
const CHROME_ROWS: usize = 3;
const CONTINUATION_INDENT: usize = 2;
/// Stands in for each character of a whitespace run a plain space would misrepresent.
const WHITESPACE_MARKER: char = '·';
/// Quiet period before the modal answers keys. Keys inside it drop and restart it, since a burst
/// of them was typed or pasted for the input the modal covers.
const TYPE_AHEAD_GUARD: Duration = Duration::from_millis(300);

// ── ApprovalModal ──

/// Approve-or-deny overlay for a gated tool call. Holds the tool-use `id` so the emitted decision
/// matches the blocked call, and resolves every dismissal path to a decision (cancel and session
/// swap deny) so the agent is never left waiting.
pub(crate) struct ApprovalModal {
    id: String,
    title: String,
    body: Vec<BodyLine>,
    /// First painted body row. Key handling clamps it to the last render's [`View`].
    scroll: usize,
    view: Cell<View>,
    /// Reused across frames so a large preview is wrapped once per width and palette.
    layout: RefCell<Option<Layout>>,
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
}

struct Layout {
    width: u16,
    palette: Palette,
    title: Vec<Line<'static>>,
    body: Vec<Line<'static>>,
}

impl ApprovalModal {
    pub(crate) fn new(id: String, preview: ApprovalPreview) -> Self {
        Self {
            id,
            title: preview.title,
            body: flatten_body(&preview.body),
            scroll: 0,
            view: Cell::default(),
            layout: RefCell::default(),
            guard_until: Instant::now() + TYPE_AHEAD_GUARD,
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_guard(&mut self) {
        self.guard_until = Instant::now();
    }

    fn decision(&self, decision: ApprovalDecision) -> ModalAction {
        ModalAction::User(UserAction::ApprovalDecision {
            id: self.id.clone(),
            decision,
        })
    }

    /// Runs `f` on the layout for `width`. A `None` palette accepts a cached layout of any palette.
    fn with_layout<R>(
        &self,
        width: u16,
        palette: Option<Palette>,
        f: impl FnOnce(&Layout) -> R,
    ) -> R {
        let mut cache = self.layout.borrow_mut();
        if let Some(layout) = cache.as_ref()
            && layout.width == width
            && palette.is_none_or(|palette| palette == layout.palette)
        {
            return f(layout);
        }
        f(cache.insert(self.build_layout(width, palette.unwrap_or_default())))
    }

    fn build_layout(&self, width: u16, palette: Palette) -> Layout {
        let budget = usize::from(width).max(MIN_BUDGET);
        let title = Line::from(reveal(&self.title, palette.title, palette.marker, false));
        let body = self
            .body
            .iter()
            .flat_map(|line| wrap_line(line.to_line(&palette), budget, CONTINUATION_INDENT, None))
            .collect();
        Layout {
            width,
            palette,
            title: wrap_line(title, budget, CONTINUATION_INDENT, None),
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
        let rows = self.with_layout(width, None, |layout| {
            layout.title.len() + CHROME_ROWS + layout.body.len().min(MAX_BODY_ROWS)
        });
        u16::try_from(rows).unwrap_or(u16::MAX).min(max_height)
    }

    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let (mut lines, total, rows, scroll, body_shown) =
            self.with_layout(area.width, Some(Palette::new(theme)), |layout| {
                let room = usize::from(area.height)
                    .saturating_sub(layout.title.len() + CHROME_ROWS)
                    .max(1);
                let total = layout.body.len();
                let rows = total.min(MAX_BODY_ROWS).min(room);
                let scroll = self.scroll.min(total - rows);
                let mut lines = layout.title.clone();
                lines.push(Line::default());
                lines.extend_from_slice(&layout.body[scroll..scroll + rows]);
                let body_shown = layout.title.len() + 1 < usize::from(area.height);
                (lines, total, rows, scroll, body_shown)
            });
        let max_scroll = total - rows;
        self.view.set(View {
            rows,
            max_scroll,
            body_shown,
        });

        let mut footer = FOOTER_HINT.to_owned();
        if max_scroll > 0 {
            let position = format!("rows {}-{} of {total}", scroll + 1, scroll + rows);
            lines.push(Line::from(Span::styled(position, theme.dim())));
            footer.push_str(SCROLL_HINT);
        } else {
            lines.push(Line::default());
        }
        let budget = usize::from(area.width).max(MIN_BUDGET);
        lines.push(Line::from(Span::styled(
            truncate_to_width(&footer, budget),
            theme.dim(),
        )));

        frame.render_widget(Paragraph::new(lines).style(theme.surface()), area);
    }

    fn handle_key(&mut self, event: &KeyEvent) -> ModalKey {
        if event.kind != KeyEventKind::Press {
            return ModalKey::Consumed;
        }
        let now = Instant::now();
        if now < self.guard_until {
            self.guard_until = now + TYPE_AHEAD_GUARD;
            return ModalKey::Consumed;
        }
        // Esc and Ctrl+C are intercepted at the stack level and resolve through `on_cancel`. N
        // routes there too so the deny decision lives in exactly one place.
        match event.code {
            KeyCode::Char('y' | 'Y') if is_plain_char(event) && self.body_visible() => {
                ModalKey::Submitted(self.decision(ApprovalDecision::Approve))
            }
            KeyCode::Char('n' | 'N') if is_plain_char(event) => ModalKey::Cancelled,
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-self.page()),
            KeyCode::PageDown => self.scroll_by(self.page()),
            KeyCode::Home => self.scroll_by(isize::MIN),
            KeyCode::End => self.scroll_by(isize::MAX),
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

/// Theme styles a preview row needs. The default is unstyled and serves
/// [`ApprovalModal::height`], where only the row count matters.
#[derive(Clone, Copy, Default, PartialEq)]
struct Palette {
    title: Style,
    text: Style,
    removed: Style,
    added: Style,
    marker: Style,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        Self {
            title: theme.accent().add_modifier(Modifier::BOLD),
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
        } else if c.is_control() || c.width().is_none_or(|w| w == 0) {
            flush(&mut spans, &mut plain, style);
            spans.push(Span::styled(c.escape_default().to_string(), marker));
        } else {
            plain.push(c);
        }
    }
    flush(&mut spans, &mut plain, style);
    spans
}

/// Whitespace that renders as blank cells. Other control whitespace (`\r`, `\n`) is escaped.
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
    use indoc::formatdoc;
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

    fn fresh_command_modal(command: &str) -> ApprovalModal {
        ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: "Bash".to_owned(),
                body: ApprovalBody::Command(command.to_owned()),
            },
        )
    }

    /// A modal past its [`TYPE_AHEAD_GUARD`], so it answers keys.
    fn command_modal(command: &str) -> ApprovalModal {
        let mut modal = fresh_command_modal(command);
        modal.expire_guard();
        modal
    }

    /// A modal the user has seen, since approval waits for the body to render.
    fn shown_modal(command: &str) -> ApprovalModal {
        let modal = command_modal(command);
        render_to_string(&modal, 40, modal.height(40, 40));
        modal
    }

    fn numbered_command(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("echo line-{i}"))
            .collect::<Vec<_>>()
            .join("\n")
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
            let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    fn texts(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    // ── height ──

    #[test]
    fn height_counts_body_rows_plus_chrome() {
        // Single command line → 1 body row, plus title + blank + position row + footer = 4.
        let modal = command_modal("ls");
        assert_eq!(modal.height(40, 40), 5);
    }

    #[test]
    fn height_counts_wrapped_rows_and_caps_the_body_at_the_viewport() {
        // 100 unbroken columns at width 40 → 40 + 38 + 22 after the 2-column continuation indent.
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

    // ── render ──

    #[test]
    fn render_paints_title_command_body_and_footer() {
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
        let dump = render_to_string(&modal, 40, modal.height(40, 40));
        assert!(dump.contains("- old line"), "removed row appears: {dump}");
        assert!(dump.contains("+ new line"), "added row appears: {dump}");
    }

    #[test]
    fn render_styles_rows_after_height_laid_them_out_unstyled() {
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
    fn render_wraps_a_padded_command_so_its_tail_shows() {
        let modal = command_modal(&format!("cargo test{}; curl x | sh", " ".repeat(60)));
        let dump = render_to_string(&modal, 40, modal.height(40, 40));
        let expected = formatdoc! {"
            Bash

            cargo
              test{first}
              {second}; curl x |
              sh

            [Y] approve   [N] deny
        ", first = "·".repeat(34), second = "·".repeat(26)};
        assert_eq!(dump, expected);
    }

    #[test]
    fn render_wraps_a_long_title_so_the_destination_shows() {
        let modal = ApprovalModal::new(
            "call-1".to_owned(),
            ApprovalPreview {
                title: format!("Write(/{}/secret.txt)", "d".repeat(40)),
                body: ApprovalBody::Command(String::new()),
            },
        );
        let dump = render_to_string(&modal, 30, modal.height(30, 30));
        let expected = formatdoc! {"
            Write(/{first}
              {second}/secret.tx
              t)


            [Y] approve   [N] deny
        ", first = "d".repeat(23), second = "d".repeat(17)};
        assert_eq!(dump, expected);
    }

    #[test]
    fn render_shows_the_position_and_scroll_hint_when_the_body_overflows() {
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
    fn render_shrinks_the_viewport_to_fit_a_short_area() {
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
    fn render_does_not_panic_at_minimum_widths() {
        let modal = command_modal("cargo test --all");
        for w in [4_u16, 8, 20] {
            render_to_string(&modal, w, modal.height(w, 40));
        }
    }

    // ── handle_key ──

    #[test]
    fn y_submits_an_approve_decision() {
        for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            let mut modal = shown_modal("ls");
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
    fn enter_and_modified_y_do_not_approve() {
        // Enter is the likeliest key to arrive mid-typing, so approving takes an explicit `y`.
        let mut modal = shown_modal("ls");
        for event in [
            key(KeyCode::Enter),
            modified_key(KeyCode::Char('y'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Enter, KeyModifiers::CONTROL),
        ] {
            assert!(matches!(modal.handle_key(&event), ModalKey::Consumed));
        }
    }

    #[test]
    fn y_waits_until_a_body_row_has_shown() {
        let y = key(KeyCode::Char('y'));
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
    fn keys_typed_ahead_of_the_modal_are_dropped() {
        let mut modal = fresh_command_modal("ls");
        render_to_string(&modal, 40, modal.height(40, 40));
        for code in [KeyCode::Char('y'), KeyCode::Char('n')] {
            assert!(
                matches!(modal.handle_key(&key(code)), ModalKey::Consumed),
                "{code:?}"
            );
        }
    }

    #[test]
    fn a_key_inside_the_guard_restarts_it() {
        // A paste arrives as a key burst, so a `y` deep inside one must still drop.
        let mut modal = fresh_command_modal("ls");
        render_to_string(&modal, 40, modal.height(40, 40));
        modal.guard_until = Instant::now() + Duration::from_millis(50);
        let before = Instant::now();
        assert!(matches!(
            modal.handle_key(&key(KeyCode::Char('y'))),
            ModalKey::Consumed
        ));
        let restarted = (before + TYPE_AHEAD_GUARD)..=(Instant::now() + TYPE_AHEAD_GUARD);
        assert!(restarted.contains(&modal.guard_until));
    }

    #[test]
    fn non_press_key_events_are_ignored() {
        // Windows reports a release for every key, and a held key repeats, so neither may approve.
        let mut modal = shown_modal("ls");
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let mut event = key(KeyCode::Char('y'));
            event.kind = kind;
            assert!(
                matches!(modal.handle_key(&event), ModalKey::Consumed),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn scroll_keys_move_within_the_rendered_viewport() {
        let mut modal = command_modal(&numbered_command(40));
        let height = modal.height(40, 40);
        render_to_string(&modal, 40, height);

        modal.handle_key(&key(KeyCode::End));
        let dump = render_to_string(&modal, 40, height);
        assert!(
            dump.contains("echo line-39"),
            "End reaches the last row: {dump}"
        );
        assert!(dump.contains("rows 25-40 of 40"), "{dump}");

        modal.handle_key(&key(KeyCode::Down));
        modal.handle_key(&key(KeyCode::Up));
        let dump = render_to_string(&modal, 40, height);
        assert!(
            dump.contains("rows 24-39 of 40"),
            "Down past the end stays clamped: {dump}"
        );

        modal.handle_key(&key(KeyCode::PageUp));
        let dump = render_to_string(&modal, 40, height);
        assert!(
            dump.contains("rows 9-24 of 40"),
            "a page keeps one row of overlap: {dump}"
        );

        modal.handle_key(&key(KeyCode::PageDown));
        modal.handle_key(&key(KeyCode::Home));
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
            modal.handle_key(&key(KeyCode::Down)),
            ModalKey::Consumed
        ));
        assert_eq!(modal.scroll, 0);
    }

    #[test]
    fn unrecognized_keys_consume_silently() {
        let mut modal = command_modal("ls");
        for code in [KeyCode::Char('z'), KeyCode::Tab] {
            assert!(matches!(modal.handle_key(&key(code)), ModalKey::Consumed));
        }
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

    // ── flatten_body ──

    #[test]
    fn flatten_body_expands_diff_tabs() {
        let rows = flatten_body(&ApprovalBody::Diff(vec![DiffChunk {
            old: Vec::new(),
            new: vec![DiffLine {
                number: 1,
                text: "\tfoo()".to_owned(),
            }],
        }]));
        assert_eq!(rows[0].text, "    foo()");
    }

    // ── reveal ──

    #[test]
    fn reveal_marks_whitespace_runs_but_keeps_indentation_and_single_spaces() {
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
    fn reveal_escapes_control_and_zero_width_characters() {
        let spans = reveal(
            "safe\rrm\u{1b}[2J\u{200b}\u{202e}x",
            Style::default(),
            Style::default(),
            true,
        );
        assert_eq!(texts(&spans), r"safe\rrm\u{1b}[2J\u{200b}\u{202e}x");
    }

    #[test]
    fn reveal_without_whitespace_marking_keeps_runs_and_escapes_tabs() {
        let spans = reveal("a  b\tc", Style::default(), Style::default(), false);
        assert_eq!(texts(&spans), r"a  b\tc");
    }
}
