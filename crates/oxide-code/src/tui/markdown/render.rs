//! Markdown → [`ratatui::text::Text`] renderer.
//!
//! Walks the pulldown-cmark event stream and produces styled lines
//! sized to a fixed terminal width. Supports inline formatting, code
//! blocks (syntect-highlighted via [`super::highlight`]), lists,
//! blockquotes, tables (box-drawing borders with column alignment),
//! and horizontal rules. Wrapping is block-aware so word breaks
//! respect the enclosing block's continuation indent.

use pulldown_cmark::{Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tracing::{debug, warn};
use unicode_width::UnicodeWidthStr;

use super::highlight::highlight_code;
use crate::tui::theme::Theme;
use crate::tui::wrap::wrap_line;

// ── Renderer ──

pub(super) struct MarkdownRenderer<I> {
    // Core
    iter: I,
    theme: Theme,
    /// Max line width for word-wrapping (0 = no wrapping).
    width: usize,
    pub(super) lines: Vec<Line<'static>>,

    // Block-level spacing
    /// Whether to insert a blank line before the next block element.
    needs_newline: bool,

    // Block nesting (lists + blockquotes)
    /// Indent prefix per nesting level (list continuation or blockquote)
    indent_stack: Vec<Vec<Span<'static>>>,
    /// Ordered (`Some(index)`) or unordered (`None`) per nesting level
    list_stack: Vec<Option<u64>>,
    /// Deferred list marker spans, emitted on the next `push_line`
    pending_marker: Option<Vec<Span<'static>>>,

    // Inline state
    /// Nested inline style stack (bold, italic, strikethrough)
    inline_styles: Vec<Style>,
    /// Stored link destination, appended at `End(Link)`
    link_url: Option<String>,

    // Buffered blocks
    code_block: CodeBlockState,
    table: TableState,
}

/// Buffered state for a fenced / indented code block
#[derive(Default)]
struct CodeBlockState {
    active: bool,
    lang: Option<String>,
    buf: String,
}

/// Buffered state for a table (header + body rows)
#[derive(Default)]
struct TableState {
    active: bool,
    in_head: bool,
    alignments: Vec<Alignment>,
    head_rows: usize,
    /// Completed rows (each row is a vec of cells; each cell is a vec of spans)
    rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Row being accumulated (cells pushed on `End(TableCell)`)
    current_row: Vec<Vec<Span<'static>>>,
    /// Spans accumulated for the current cell
    cell_buf: Vec<Span<'static>>,
}

impl<'a, I> MarkdownRenderer<I>
where
    I: Iterator<Item = Event<'a>>,
{
    pub(super) fn new(iter: I, theme: Theme, width: usize) -> Self {
        Self {
            iter,
            theme,
            width,
            lines: Vec::new(),
            needs_newline: false,
            indent_stack: Vec::new(),
            list_stack: Vec::new(),
            pending_marker: None,
            inline_styles: Vec::new(),
            link_url: None,
            code_block: CodeBlockState::default(),
            table: TableState::default(),
        }
    }

    pub(super) fn run(&mut self) {
        while let Some(event) = self.iter.next() {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(ref text) => self.text(text),
            Event::Code(code) => self.code(code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::Html(ref html) => self.html(html),
            Event::InlineHtml(ref html) => self.inline_html(html),
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {
                debug!(?event, "unsupported markdown event");
            }
        }
    }

    // ── Tag Dispatch ──

    fn start_tag(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.start_paragraph(),
            Tag::Heading { level, .. } => self.start_heading(level),
            Tag::BlockQuote(_) => self.start_blockquote(),
            Tag::CodeBlock(kind) => self.start_code_block(kind),
            Tag::List(start) => self.start_list(start),
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.push_inline_style(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.push_inline_style(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                self.push_inline_style(Style::new().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
            }
            Tag::Table(alignments) => self.start_table(alignments),
            Tag::TableHead => self.table.in_head = true,
            Tag::TableRow => {}
            Tag::TableCell => self.table.cell_buf.clear(),
            Tag::HtmlBlock
            | Tag::Image { .. }
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Subscript
            | Tag::Superscript => {
                warn!(?tag, "unsupported markdown tag");
            }
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_blockquote(),
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => {
                // Tight list items emit `Text` directly without paragraph
                // wrappers, so `end_paragraph` never runs — wrap here to
                // respect the width budget before the indent is popped.
                self.wrap_last_line();
                self.indent_stack.pop();
                self.pending_marker = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.pop_inline_style();
            }
            TagEnd::Link => self.pop_link(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => {
                // pulldown-cmark 0.13 puts header cells directly inside
                // TableHead without a wrapping TableRow, so flush here.
                if !self.table.current_row.is_empty() {
                    let row = std::mem::take(&mut self.table.current_row);
                    self.table.rows.push(row);
                }
                self.table.in_head = false;
                self.table.head_rows = self.table.rows.len();
            }
            TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table.current_row);
                self.table.rows.push(row);
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.table.cell_buf);
                self.table.current_row.push(cell);
            }
            _ => {}
        }
    }

    // ── Block Elements ──

    fn start_paragraph(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::default());
        self.needs_newline = false;
    }

    fn end_paragraph(&mut self) {
        self.wrap_last_line();
        self.needs_newline = true;
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        if self.needs_newline {
            self.push_blank_line();
        }
        let style = match level {
            HeadingLevel::H1 => self.theme.heading_h1(),
            HeadingLevel::H2 => self.theme.heading_h2(),
            HeadingLevel::H3 => self.theme.heading_h3(),
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => self.theme.heading_minor(),
        };
        let prefix = format!("{} ", "#".repeat(level as usize));
        self.push_line(Line::from(vec![Span::styled(prefix, style)]));
        self.push_inline_style(style);
        self.needs_newline = false;
    }

    fn end_heading(&mut self) {
        self.pop_inline_style();
        self.wrap_last_line();
        self.needs_newline = true;
    }

    fn start_blockquote(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
        self.indent_stack
            .push(vec![Span::styled("> ", self.theme.blockquote())]);
    }

    fn end_blockquote(&mut self) {
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    fn rule(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::from(vec![Span::styled(
            "───",
            self.theme.horizontal_rule(),
        )]));
        self.needs_newline = true;
    }

    // ── Lists ──

    fn start_list(&mut self, start_index: Option<u64>) {
        if self.list_stack.is_empty() && self.needs_newline {
            self.push_blank_line();
        }
        self.list_stack.push(start_index);
    }

    fn end_list(&mut self) {
        self.list_stack.pop();
        self.needs_newline = true;
    }

    fn start_item(&mut self) {
        let marker = match self.list_stack.last_mut() {
            Some(Some(index)) => {
                let m = format!("{}. ", *index);
                *index += 1;
                m
            }
            _ => "- ".to_owned(),
        };

        let continuation = vec![Span::raw(" ".repeat(marker.len()))];
        self.pending_marker = Some(vec![Span::styled(marker, self.theme.list_marker())]);
        self.indent_stack.push(continuation);
        self.needs_newline = false;
    }

    // ── Code Blocks ──

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        if self.needs_newline {
            self.push_blank_line();
        }
        let lang = match kind {
            CodeBlockKind::Fenced(lang) => {
                let l = lang.to_string();
                if l.is_empty() { None } else { Some(l) }
            }
            CodeBlockKind::Indented => None,
        };
        self.code_block = CodeBlockState {
            active: true,
            lang,
            ..Default::default()
        };
    }

    fn end_code_block(&mut self) {
        self.code_block.active = false;
        let code = std::mem::take(&mut self.code_block.buf);
        let lang = self.code_block.lang.take();

        let highlighted = highlight_code(
            lang.as_deref().unwrap_or(""),
            &code,
            self.theme.code_block_fallback(),
        );
        for line in highlighted {
            self.lines.push(line);
        }
        self.needs_newline = true;
    }

    // ── Tables ──

    fn start_table(&mut self, alignments: Vec<Alignment>) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.table = TableState {
            active: true,
            alignments,
            ..Default::default()
        };
    }

    fn end_table(&mut self) {
        let TableState {
            rows,
            alignments,
            head_rows,
            ..
        } = std::mem::take(&mut self.table);

        if rows.is_empty() {
            return;
        }

        let col_count = alignments
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        let natural_widths = compute_column_widths(&rows, col_count);
        let col_widths = if self.width == 0 {
            natural_widths
        } else {
            fit_column_widths(&natural_widths, self.width)
        };

        let border_style = self.theme.table_border();
        let header_style = self.theme.table_header();

        // Top border: ┌─┬─┐
        self.lines.push(build_horizontal_rule(
            &col_widths,
            border_style,
            '┌',
            '┬',
            '┐',
        ));

        for (row_idx, row) in rows.iter().enumerate() {
            let cell_style = if row_idx < head_rows {
                header_style
            } else {
                Style::default()
            };
            for line in build_data_rows(
                row,
                &col_widths,
                &alignments,
                col_count,
                border_style,
                cell_style,
            ) {
                self.lines.push(line);
            }

            // Separator after header: ├─┼─┤
            if row_idx + 1 == head_rows && head_rows < rows.len() {
                self.lines.push(build_horizontal_rule(
                    &col_widths,
                    border_style,
                    '├',
                    '┼',
                    '┤',
                ));
            }
        }

        // Bottom border: └─┴─┘
        self.lines.push(build_horizontal_rule(
            &col_widths,
            border_style,
            '└',
            '┴',
            '┘',
        ));

        self.needs_newline = true;
    }

    // ── Inline Content ──

    fn text(&mut self, text: &str) {
        if self.code_block.active {
            self.code_block.buf.push_str(text);
            return;
        }
        if self.table.active {
            let style = self.current_inline_style();
            self.table
                .cell_buf
                .push(Span::styled(text.to_owned(), style));
            return;
        }

        if self.pending_marker.is_some() {
            self.push_line(Line::default());
        }

        let style = self.current_inline_style();
        for (i, line) in text.lines().enumerate() {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if i > 0 {
                self.push_line(Line::default());
            }
            self.push_span(Span::styled(line.to_owned(), style));
        }
        self.needs_newline = false;
    }

    fn code(&mut self, code: CowStr<'a>) {
        // Propagate the surrounding inline style (bold, italic, heading
        // modifiers) onto the code span; `inline_code()` supplies the
        // distinctive fg + bg, enclosing modifiers apply on top.
        let style = self.current_inline_style().patch(self.theme.inline_code());
        if self.table.active {
            self.table
                .cell_buf
                .push(Span::styled(code.into_string(), style));
            return;
        }
        if self.pending_marker.is_some() {
            self.push_line(Line::default());
        }
        self.push_span(Span::styled(code.into_string(), style));
    }

    fn html(&mut self, html: &str) {
        let style = self.current_inline_style();
        for (i, line) in html.lines().enumerate() {
            if i > 0 || self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            self.push_span(Span::styled(line.to_owned(), style));
        }
    }

    fn inline_html(&mut self, html: &str) {
        self.push_span(Span::raw(html.to_owned()));
    }

    fn soft_break(&mut self) {
        self.push_span(Span::raw(" "));
    }

    fn hard_break(&mut self) {
        self.push_line(Line::default());
    }

    // ── Links ──

    fn pop_link(&mut self) {
        if let Some(url) = self.link_url.take() {
            self.push_span(Span::raw(" ("));
            self.push_span(Span::styled(url, self.theme.link()));
            self.push_span(Span::raw(")"));
        }
    }

    // ── Inline Style Stack ──

    fn push_inline_style(&mut self, style: Style) {
        let merged = self
            .inline_styles
            .last()
            .copied()
            .unwrap_or_default()
            .patch(style);
        self.inline_styles.push(merged);
    }

    fn pop_inline_style(&mut self) {
        self.inline_styles.pop();
    }

    fn current_inline_style(&self) -> Style {
        self.inline_styles.last().copied().unwrap_or_default()
    }

    // ── Line Building ──

    /// Starts a new output line with indent prefixes and any pending list marker.
    ///
    /// When `self.width` is set, lines that exceed the width budget are
    /// word-wrapped with continuation lines prefixed by the current
    /// `indent_stack` (using continuation forms, not markers — so
    /// blockquote `> ` repeats while list `- ` becomes spaces).
    fn push_line(&mut self, line: Line<'static>) {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // Emit indent prefixes. If a pending marker exists, use it for the
        // deepest list level instead of the continuation indent.
        let marker = self.pending_marker.take();
        let marker_depth = if marker.is_some() {
            self.indent_stack.len().saturating_sub(1)
        } else {
            usize::MAX
        };

        for (i, prefix) in self.indent_stack.iter().enumerate() {
            if i == marker_depth
                && let Some(ref m) = marker
            {
                spans.extend(m.iter().cloned());
                continue;
            }
            spans.extend(prefix.iter().cloned());
        }

        spans.extend(line.spans);

        self.wrap_and_push(Line::from(spans));
    }

    /// Wraps `line` against the current width budget (using the indent
    /// stack as the continuation prefix) and append the results.
    fn wrap_and_push(&mut self, line: Line<'static>) {
        if self.width == 0 {
            self.lines.push(line);
            return;
        }
        let cont_prefix = self.continuation_indent_spans();
        let indent = cont_prefix
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let prefix = if cont_prefix.is_empty() {
            None
        } else {
            Some(cont_prefix.as_slice())
        };
        for wrapped in wrap_line(line, self.width, indent, prefix) {
            self.lines.push(wrapped);
        }
    }

    /// Flattens the `indent_stack` into a single span vector, used as the
    /// continuation prefix when wrapping. Entries store the continuation
    /// form (spaces for lists, `> ` for blockquotes) so that wrapped
    /// lines repeat blockquote markers without duplicating list markers.
    fn continuation_indent_spans(&self) -> Vec<Span<'static>> {
        self.indent_stack
            .iter()
            .flat_map(|prefix| prefix.iter().cloned())
            .collect()
    }

    /// Appends a span to the last line, creating one if needed.
    fn push_span(&mut self, span: Span<'static>) {
        if let Some(line) = self.lines.last_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    /// Word-wrap the last accumulated line in place.
    ///
    /// Inline content (paragraphs, headings, tight list items) is built
    /// by appending spans to the last line via
    /// [`push_span`](Self::push_span), bypassing the wrapping in
    /// [`push_line`](Self::push_line). This method retroactively wraps
    /// the completed line so it respects the width budget.
    fn wrap_last_line(&mut self) {
        if self.width == 0 {
            return;
        }
        let Some(line) = self.lines.pop() else {
            return;
        };
        self.wrap_and_push(line);
    }

    fn push_blank_line(&mut self) {
        self.lines.push(Line::default());
    }
}

// ── Table Helpers ──

/// Computes the max display width for each column across all rows.
fn compute_column_widths(rows: &[Vec<Vec<Span<'_>>>], col_count: usize) -> Vec<usize> {
    let mut widths = vec![0_usize; col_count];
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            widths[col] = widths[col].max(cell_width(cell));
        }
    }
    widths
}

/// Shrink column widths so the rendered table fits within `width_budget`.
///
/// Table overhead per row is `1 + 3 * n` columns (left border + 2 padding
/// spaces and 1 separator per column). Non-empty columns are floored at 1
/// so cell wrapping stays viable on narrow terminals, even if that causes
/// marginal overflow — preferable to dropping content.
fn fit_column_widths(natural: &[usize], width_budget: usize) -> Vec<usize> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    let overhead = 1 + 3 * n;
    let available = width_budget.saturating_sub(overhead);
    let total_natural: usize = natural.iter().sum();
    if total_natural <= available {
        return natural.to_vec();
    }

    let max_natural = *natural.iter().max().unwrap_or(&0);
    let mut lo: usize = 0;
    let mut hi: usize = max_natural;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let sum: usize = natural.iter().map(|&w| w.min(mid)).sum();
        if sum <= available {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    natural
        .iter()
        .map(|&w| if w > 0 { w.min(lo).max(1) } else { 0 })
        .collect()
}

/// Builds a horizontal rule line: e.g. `┌───┬───┐` or `├───┼───┤`.
fn build_horizontal_rule(
    col_widths: &[usize],
    style: Style,
    left: char,
    mid: char,
    right: char,
) -> Line<'static> {
    let mut buf = String::with_capacity(col_widths.len() * 6);
    buf.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        // +2 for the padding spaces around cell content
        for _ in 0..w + 2 {
            buf.push('─');
        }
        buf.push(if i + 1 < col_widths.len() { mid } else { right });
    }
    Line::from(Span::styled(buf, style))
}

/// Word-wraps a cell's spans into sub-lines of at most `target_width` columns.
/// Always returns at least one sub-line so every row has a visual line.
fn wrap_cell(cell: &[Span<'static>], target_width: usize) -> Vec<Vec<Span<'static>>> {
    if cell.is_empty() || target_width == 0 {
        return vec![cell.to_vec()];
    }
    wrap_line(Line::from(cell.to_vec()), target_width, 0, None)
        .into_iter()
        .map(|line| line.spans)
        .collect()
}

/// Builds the visual lines for a data row, wrapping cells to column widths.
/// A wrapping cell produces multiple lines; other columns pad out on
/// trailing sub-lines so column separators stay aligned.
fn build_data_rows(
    row: &[Vec<Span<'static>>],
    col_widths: &[usize],
    alignments: &[Alignment],
    col_count: usize,
    border_style: Style,
    cell_style: Style,
) -> Vec<Line<'static>> {
    let empty: Vec<Span<'static>> = Vec::new();
    let cols: Vec<(Vec<Vec<Span<'static>>>, usize)> = (0..col_count)
        .map(|col| {
            let cell = row.get(col).unwrap_or(&empty).as_slice();
            let target_width = col_widths.get(col).copied().unwrap_or(0);
            (wrap_cell(cell, target_width), target_width)
        })
        .collect();

    let sub_row_count = cols.iter().map(|(c, _)| c.len()).max().unwrap_or(1).max(1);
    let pipe = Span::styled("│", border_style);

    (0..sub_row_count)
        .map(|sub_idx| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 4 + 1);
            spans.push(pipe.clone());
            for (col, (sub_lines, target_width)) in cols.iter().enumerate() {
                let sub_cell: &[Span<'static>] = sub_lines
                    .get(sub_idx)
                    .map_or(empty.as_slice(), Vec::as_slice);
                let pad = target_width.saturating_sub(cell_width(sub_cell));

                let alignment = alignments.get(col).copied().unwrap_or(Alignment::None);
                let (pad_left, pad_right) = match alignment {
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::Right => (pad, 0),
                    Alignment::Left | Alignment::None => (0, pad),
                };

                spans.push(Span::raw(" ".repeat(1 + pad_left)));

                for span in sub_cell {
                    let styled = if cell_style == Style::default() {
                        span.clone()
                    } else {
                        Span::styled(span.content.clone(), span.style.patch(cell_style))
                    };
                    spans.push(styled);
                }

                spans.push(Span::raw(" ".repeat(1 + pad_right)));
                spans.push(pipe.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// Measure the display width of a cell's spans.
fn cell_width(cell: &[Span<'_>]) -> usize {
    cell.iter().map(|s| s.content.width()).sum()
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::{Color, Modifier};
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;

    use super::super::render_markdown;
    use super::{fit_column_widths, wrap_cell};
    use crate::tui::theme::Theme;

    fn theme() -> Theme {
        Theme::default()
    }

    /// Render `input` at the given width budget and flatten each line
    /// into a concatenated string (spans joined in order). `width == 0`
    /// disables word-wrapping.
    fn rendered_text_at_width(input: &str, width: usize) -> Vec<String> {
        render_markdown(input, &theme(), width)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Convenience wrapper around [`rendered_text_at_width`] at width 0
    /// (no wrapping) — used by most tests that don't exercise wrapping.
    fn rendered_text(input: &str) -> Vec<String> {
        rendered_text_at_width(input, 0)
    }

    // ── render_markdown ──

    #[test]
    fn plain_text() {
        let lines = rendered_text("Hello, world!");
        assert_eq!(lines, vec!["Hello, world!"]);
    }

    // ── Paragraphs ──

    #[test]
    fn paragraph_separation() {
        let lines = rendered_text(indoc! {"
            First paragraph

            Second paragraph
        "});
        let first_pos = lines.iter().position(|l| l.contains("First")).unwrap();
        let blank_pos = lines.iter().position(String::is_empty).unwrap();
        let second_pos = lines.iter().position(|l| l.contains("Second")).unwrap();
        assert!(
            first_pos < blank_pos && blank_pos < second_pos,
            "expected First < blank < Second, got {first_pos} < {blank_pos} < {second_pos}"
        );
    }

    #[test]
    fn paragraph_soft_break_joins() {
        let lines = rendered_text(indoc! {"
            Hello
            World
        "});
        assert_eq!(lines, vec!["Hello World"]);
    }

    // ── Headings ──

    #[test]
    fn heading_levels_text_and_styles() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                # H1
                ## H2
                ### H3
                #### H4
            "},
            &t,
            0,
        );

        let find_heading = |prefix: &str| {
            text.lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.starts_with(prefix)))
                .unwrap_or_else(|| panic!("no line starting with {prefix}"))
                .clone()
        };

        let h1 = find_heading("# ");
        assert!(h1.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            h1.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(h1.spans[0].style.fg, Some(t.fg));

        let h2 = find_heading("## ");
        assert!(h2.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !h2.spans[0]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );

        let h3 = find_heading("### ");
        assert!(h3.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(h3.spans[0].style.add_modifier.contains(Modifier::ITALIC));

        let h4 = find_heading("#### ");
        assert!(h4.spans[0].style.add_modifier.contains(Modifier::ITALIC));
        assert!(!h4.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    // ── Blockquotes ──

    #[test]
    fn blockquote_text_and_style() {
        let t = theme();
        let text = render_markdown("> Quoted text", &t, 0);
        let bq_line = text
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Quoted")))
            .expect("blockquote line not found");
        let marker_span = bq_line
            .spans
            .iter()
            .find(|s| s.content.contains("> "))
            .expect("> marker not found");
        assert_eq!(marker_span.style.fg, Some(t.success));
    }

    #[test]
    fn nested_blockquote() {
        let lines = rendered_text(indoc! {"
            > Outer
            > > Inner
        "});
        assert!(lines.iter().any(|l| l.contains("Outer")));
        let inner = lines.iter().find(|l| l.contains("Inner")).unwrap();
        assert_eq!(
            inner.matches("> ").count(),
            2,
            "inner blockquote should have exactly 2 nested > markers: {inner:?}"
        );
    }

    // ── Horizontal Rule ──

    #[test]
    fn horizontal_rule_text_and_style() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                Above

                ---

                Below
            "},
            &t,
            0,
        );
        let rule_span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains('─')))
            .expect("rule span not found");
        assert_eq!(rule_span.style.fg, Some(t.fg_dim));
    }

    // ── Lists ──

    #[test]
    fn tight_ordered_list() {
        let lines = rendered_text(indoc! {"
            1. First
            2. Second
            3. Third
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("1.") && l.contains("First"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2.") && l.contains("Second"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("3.") && l.contains("Third"))
        );
    }

    #[test]
    fn loose_ordered_list() {
        let lines = rendered_text(indoc! {"
            1. First

            2. Second

            3. Third
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("1.") && l.contains("First")),
            "loose list marker and content should be on the same line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("2.") && l.contains("Second")),
            "second item: {lines:?}"
        );
    }

    #[test]
    fn ordered_list_double_digit_alignment() {
        let mut input = String::new();
        for i in 1..=12 {
            use std::fmt::Write;
            writeln!(input, "{i}. Item {i}").unwrap();
        }
        let lines = rendered_text(&input);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("10.") && l.contains("Item 10")),
            "marker and content on same line for item 10: {lines:?}"
        );
    }

    #[test]
    fn tight_unordered_list() {
        let lines = rendered_text(indoc! {"
            - Alpha
            - Beta
        "});
        assert!(
            lines
                .iter()
                .any(|l| l.contains("- ") && l.contains("Alpha"))
        );
        assert!(lines.iter().any(|l| l.contains("- ") && l.contains("Beta")));
    }

    #[test]
    fn loose_unordered_list() {
        let lines = rendered_text(indoc! {"
            - Alpha

            - Beta
        "});
        assert!(
            lines.iter().any(|l| l.contains('-') && l.contains("Alpha")),
            "loose unordered: {lines:?}"
        );
    }

    #[test]
    fn nested_list() {
        let lines = rendered_text(indoc! {"
            - Outer
              - Inner
        "});
        let inner = lines.iter().find(|l| l.contains("Inner")).unwrap();
        let outer = lines.iter().find(|l| l.contains("Outer")).unwrap();
        let inner_indent = inner.len() - inner.trim_start().len();
        let outer_indent = outer.len() - outer.trim_start().len();
        assert!(
            inner_indent > outer_indent,
            "inner indent ({inner_indent}) should exceed outer ({outer_indent})"
        );
    }

    #[test]
    fn list_marker_uses_accent_color() {
        let t = theme();
        let text = render_markdown("- Item", &t, 0);
        let marker_span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains("- ")));
        let span = marker_span.expect("list marker span not found");
        assert_eq!(span.style.fg, Some(t.accent));
    }

    #[test]
    fn inline_code_in_list_item() {
        let t = theme();
        let text = render_markdown("- Use `foo()` here", &t, 0);
        let code_span = text
            .lines
            .iter()
            .find_map(|line| line.spans.iter().find(|s| s.content.contains("foo()")))
            .expect("inline code span missing inside list item");
        assert_eq!(
            code_span.style.fg,
            Some(t.code),
            "inline code keeps teal fg inside list items"
        );
        assert_eq!(
            code_span.style.bg,
            Some(t.surface),
            "inline code keeps surface bg inside list items"
        );
    }

    #[test]
    fn link_in_list_item() {
        let lines = rendered_text("- [Click](https://example.com)");
        assert!(lines.iter().any(|l| l.contains("Click")));
        assert!(lines.iter().any(|l| l.contains("https://example.com")));
    }

    // ── Code Blocks ──

    #[test]
    fn fenced_code_block_plain() {
        let lines = rendered_text(indoc! {"
            ```
            fn main() {}
            ```
        "});
        assert!(lines.iter().any(|l| l.contains("fn main()")));
    }

    #[test]
    fn fenced_code_block_plain_has_no_background() {
        // Plain fenced blocks (no language) use the `code_block_fallback`
        // style, which omits the bg fill so wrapping / width variance
        // across lines doesn't leave ragged highlight edges. The inline
        // code bg (surface) must not leak into fenced blocks.
        //
        // The fallback path builds each line via `Line::styled(..)`, so
        // the style lands on the Line, not the inner spans. Walk both
        // layers to guard against either placement.
        let t = theme();
        let text = render_markdown(
            indoc! {"
                ```
                fn main() {}
                let x = 1;
                ```
            "},
            &t,
            0,
        );
        let code_lines: Vec<_> = text
            .lines
            .iter()
            .filter(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("fn") || s.content.contains("let"))
            })
            .collect();
        assert!(
            !code_lines.is_empty(),
            "fenced block content missing from render"
        );
        for line in code_lines {
            assert_eq!(
                line.style.bg, None,
                "fenced block line style must not have a bg fill: {line:?}"
            );
            let effective_fg = line
                .style
                .fg
                .or_else(|| line.spans.iter().find_map(|s| s.style.fg));
            assert_eq!(
                effective_fg,
                Some(t.code),
                "plain fenced block falls back to `code` fg: {line:?}"
            );
            for span in &line.spans {
                assert_eq!(
                    span.style.bg, None,
                    "fenced block span must not have a bg fill: {span:?}"
                );
            }
        }
    }

    #[test]
    fn fenced_code_block_with_lang_highlights() {
        let text = render_markdown(
            indoc! {"
                ```rust
                fn main() {}
                ```
            "},
            &theme(),
            0,
        );
        let has_rgb = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))))
        });
        assert!(has_rgb, "syntax highlighting should produce RGB colors");
    }

    // ── Tables ──

    #[test]
    fn table_basic() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
            | 1 | 2 |
            | 3 | 4 |
        "});
        // Top border
        assert!(
            lines
                .iter()
                .any(|l| l.contains('┌') && l.contains('┬') && l.contains('┐')),
            "top border missing: {lines:?}"
        );
        // Header cells
        assert!(
            lines
                .iter()
                .any(|l| l.contains('A') && l.contains('B') && l.contains('│')),
            "header row missing: {lines:?}"
        );
        // Header separator
        assert!(
            lines
                .iter()
                .any(|l| l.contains('├') && l.contains('┼') && l.contains('┤')),
            "header separator missing: {lines:?}"
        );
        // Body cells
        assert!(
            lines.iter().any(|l| l.contains('1') && l.contains('2')),
            "first body row missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('3') && l.contains('4')),
            "second body row missing: {lines:?}"
        );
        // Bottom border
        assert!(
            lines
                .iter()
                .any(|l| l.contains('└') && l.contains('┴') && l.contains('┘')),
            "bottom border missing: {lines:?}"
        );
    }

    #[test]
    fn table_alignment() {
        let lines = rendered_text(indoc! {"
            | Left | Center | Right |
            |:-----|:------:|------:|
            | a    |   b    |     c |
        "});
        let body_row = lines
            .iter()
            .find(|l| l.contains('a') && l.contains('b') && l.contains('c'))
            .expect("body row not found");

        // "a" should be left-aligned (near left pipe), "c" right-aligned (near right pipe).
        let a_pos = body_row.find('a').unwrap();
        let c_pos = body_row.find('c').unwrap();
        assert!(
            a_pos < c_pos,
            "left-aligned 'a' should appear before right-aligned 'c'"
        );

        // For center alignment, 'b' should have padding on both sides.
        let b_segment: String = body_row
            .split('│')
            .nth(2)
            .expect("center column not found")
            .to_owned();
        let trimmed = b_segment.trim();
        let left_pad = b_segment.len() - b_segment.trim_start().len();
        let right_pad = b_segment.len() - b_segment.trim_end().len();
        assert!(
            !trimmed.is_empty() && left_pad > 0 && right_pad > 0,
            "center-aligned cell should have padding on both sides: {body_row:?}"
        );
    }

    #[test]
    fn table_header_style() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                | Name |
                |------|
                | val  |
            "},
            &t,
            0,
        );
        let header_line = text
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Name")))
            .expect("header line not found");
        let name_span = header_line
            .spans
            .iter()
            .find(|s| s.content.contains("Name"))
            .unwrap();
        assert!(
            name_span.style.add_modifier.contains(Modifier::BOLD),
            "header cells should be bold: {name_span:?}"
        );
    }

    #[test]
    fn table_inline_styles() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                | Header |
                |--------|
                | `code` |
            "},
            &t,
            0,
        );
        let code_span = text
            .lines
            .iter()
            .find_map(|line| line.spans.iter().find(|s| s.content.contains("code")))
            .expect("inline code span missing inside table cell");
        assert_eq!(
            code_span.style.fg,
            Some(t.code),
            "inline code keeps teal fg inside table cells"
        );
        assert_eq!(
            code_span.style.bg,
            Some(t.surface),
            "inline code keeps surface bg inside table cells"
        );
    }

    #[test]
    fn table_empty_cells() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
            |   | x |
        "});
        let body_row = lines
            .iter()
            .find(|l| l.contains('x'))
            .expect("body row not found");
        // Row should still have 3 pipe characters (left, middle, right).
        let pipe_count = body_row.matches('│').count();
        assert_eq!(
            pipe_count, 3,
            "row with empty cell should have 3 borders: {body_row:?}"
        );
    }

    #[test]
    fn table_mismatched_column_counts() {
        let lines = rendered_text(indoc! {"
            | A | B | C |
            |---|---|---|
            | 1 | 2 |
        "});
        // Short row should be padded to 3 columns.
        let body_row = lines
            .iter()
            .find(|l| l.contains('1') && l.contains('2'))
            .expect("body row not found");
        let pipe_count = body_row.matches('│').count();
        assert_eq!(
            pipe_count, 4,
            "short row should still have 4 borders (3 columns): {body_row:?}"
        );
    }

    #[test]
    fn table_header_only() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
        "});
        // Should render top border, header row, bottom border (no separator
        // since there are no body rows).
        assert!(
            lines.iter().any(|l| l.contains('┌')),
            "top border missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('A') && l.contains('B')),
            "header row missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('└')),
            "bottom border missing: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains('├')),
            "separator should not appear with no body rows: {lines:?}"
        );
    }

    #[test]
    fn table_fits_width_budget_and_wraps_cells() {
        // Natural width of the second column (~35 cols of content) would
        // overflow a 40-col budget; the table should shrink that column and
        // wrap the long cell across multiple visual sub-lines. All rendered
        // lines — borders and data rows alike — must stay within the budget.
        let width = 40;
        let text = render_markdown(
            indoc! {"
                | # | Description                         |
                |---|-------------------------------------|
                | 1 | short                               |
                | 2 | a cell with enough words to wrap it |
            "},
            &theme(),
            width,
        );
        let rendered: Vec<String> = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        for line in &rendered {
            if line.contains('│') || line.contains('┌') || line.contains('└') {
                assert!(
                    UnicodeWidthStr::width(line.as_str()) <= width,
                    "table line exceeds width budget ({width}): {line:?}"
                );
            }
        }

        let border_chars: Vec<&str> = rendered
            .iter()
            .filter(|l| l.starts_with('┌') || l.starts_with('├') || l.starts_with('└'))
            .map(String::as_str)
            .collect();
        assert!(!border_chars.is_empty(), "borders missing: {rendered:?}");
        let border_width = UnicodeWidthStr::width(border_chars[0]);
        for b in &border_chars {
            assert_eq!(
                UnicodeWidthStr::width(*b),
                border_width,
                "borders should share the same width: {border_chars:?}"
            );
        }

        // All data rows should also share that width so the right border
        // stays aligned with the top/bottom borders.
        let data_rows: Vec<&str> = rendered
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(String::as_str)
            .collect();
        for row in &data_rows {
            let row_width = UnicodeWidthStr::width(*row);
            assert_eq!(
                row_width, border_width,
                "data row width ({row_width}) != border width ({border_width}): {row:?}",
            );
        }

        // The long cell from row "2" must produce more than one visual
        // sub-line starting with │ between the separator and the bottom border.
        let sep_idx = rendered.iter().position(|l| l.starts_with('├')).unwrap();
        let bot_idx = rendered.iter().position(|l| l.starts_with('└')).unwrap();
        let body = &rendered[sep_idx + 1..bot_idx];
        assert!(
            body.len() >= 3,
            "long cell should wrap into multiple sub-lines, got body: {body:?}"
        );
    }

    #[test]
    fn table_wrapped_cell_keeps_alignment_in_other_columns() {
        // A wrapping cell in one column should leave the other columns
        // padded out on trailing sub-lines so the column pipes stay aligned.
        let text = render_markdown(
            indoc! {"
                | ID | Note                               |
                |----|------------------------------------|
                | 42 | enough words here to force a wrap  |
            "},
            &theme(),
            30,
        );
        let rendered: Vec<String> = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        let sep_idx = rendered.iter().position(|l| l.starts_with('├')).unwrap();
        let bot_idx = rendered.iter().position(|l| l.starts_with('└')).unwrap();
        let body: Vec<&str> = rendered[sep_idx + 1..bot_idx]
            .iter()
            .map(String::as_str)
            .collect();
        assert!(
            body.len() >= 2,
            "expected wrapped sub-lines in body: {body:?}"
        );

        // Every sub-line should have the same number of `│` separators as
        // the header row (left + per-column = col_count + 1).
        let header_idx = rendered.iter().position(|l| l.contains("ID")).unwrap();
        let expected_pipes = rendered[header_idx].matches('│').count();
        for row in &body {
            assert_eq!(
                row.matches('│').count(),
                expected_pipes,
                "sub-row should keep all column pipes: {row:?}"
            );
        }
    }

    // ── fit_column_widths ──

    #[test]
    fn fit_column_widths_natural_fits_budget_unchanged() {
        // Natural total (5 + 5 = 10) + overhead (1 + 3*2 = 7) = 17 ≤ 80.
        let natural = vec![5, 5];
        assert_eq!(fit_column_widths(&natural, 80), natural);
    }

    #[test]
    fn fit_column_widths_shrinks_widest_column_first() {
        // Budget = 20, overhead for 2 cols = 7, available = 13.
        // Natural [3, 20] → sum 23 > 13. Cap should settle at 10 so
        // sums to 3 + 10 = 13, leaving the narrow column untouched.
        let out = fit_column_widths(&[3, 20], 20);
        assert_eq!(out, vec![3, 10]);
    }

    #[test]
    fn fit_column_widths_preserves_zero_width_columns() {
        // An empty column (natural=0) stays at 0 even when shrinking;
        // non-empty columns keep a minimum of 1 so wrapping stays viable.
        let out = fit_column_widths(&[0, 40], 10);
        assert_eq!(out[0], 0);
        assert!(out[1] >= 1);
    }

    #[test]
    fn fit_column_widths_empty_input_yields_empty() {
        assert_eq!(fit_column_widths(&[], 80), Vec::<usize>::new());
    }

    // ── wrap_cell ──

    #[test]
    fn wrap_cell_wraps_long_content_to_target_width() {
        let cell = vec![Span::raw("one two three four".to_owned())];
        let out = wrap_cell(&cell, 8);
        assert!(out.len() >= 2, "long cell should wrap: {out:?}");
        for sub_line in &out {
            let width: usize = sub_line
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(width <= 8, "sub-line exceeds target width: {sub_line:?}");
        }
    }

    #[test]
    fn wrap_cell_empty_returns_single_empty_sub_line() {
        let out = wrap_cell(&[], 10);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_empty());
    }

    #[test]
    fn wrap_cell_zero_target_width_returns_cell_as_is() {
        let cell = vec![Span::raw("hello".to_owned())];
        let out = wrap_cell(&cell, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], cell);
    }

    // ── Inline Content ──

    #[test]
    fn bold_and_italic() {
        let text = render_markdown("**bold** and *italic*", &theme(), 0);
        let bold_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold"))
            .unwrap();
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
        let italic_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("italic"))
            .unwrap();
        assert!(italic_span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn bold_italic_combined() {
        let text = render_markdown("***bold italic***", &theme(), 0);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("bold italic"))
            .unwrap();
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert!(span.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn strikethrough() {
        let text = render_markdown("~~struck~~", &theme(), 0);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("struck"))
            .unwrap();
        assert!(span.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn inline_code() {
        let t = theme();
        let text = render_markdown("Use `foo()` here", &t, 0);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("foo()"))
            .unwrap();
        assert_eq!(span.style.fg, Some(t.code));
        assert_eq!(
            span.style.bg,
            Some(t.surface),
            "inline code should have a surface background fill to stand out"
        );
    }

    #[test]
    fn inline_code_inside_bold_inherits_bold() {
        let t = theme();
        let text = render_markdown("**use `foo()` here**", &t, 0);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("foo()"))
            .expect("code span not found");
        assert_eq!(
            span.style.fg,
            Some(t.code),
            "code span keeps its distinctive fg"
        );
        assert_eq!(
            span.style.bg,
            Some(t.surface),
            "code span keeps its surface fill"
        );
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "code span inside **bold** should inherit BOLD modifier"
        );
    }

    #[test]
    fn inline_code_inside_heading_inherits_modifiers() {
        let t = theme();
        let text = render_markdown("# see `foo()`", &t, 0);
        let span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains("foo()")))
            .expect("code span not found in heading");
        assert_eq!(span.style.fg, Some(t.code));
        assert_eq!(span.style.bg, Some(t.surface));
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "code inside an H1 should inherit the heading's BOLD"
        );
        assert!(
            span.style.add_modifier.contains(Modifier::UNDERLINED),
            "code inside an H1 should inherit the heading's UNDERLINED"
        );
    }

    // ── HTML ──

    #[test]
    fn html_block_rendered_as_text() {
        let lines = rendered_text("<div>hello</div>");
        assert!(lines.iter().any(|l| l.contains("<div>hello</div>")));
    }

    #[test]
    fn inline_html_preserved() {
        let lines = rendered_text("text <br> more");
        let joined: String = lines.join("");
        assert!(joined.contains("<br>"));
    }

    // ── Links ──

    #[test]
    fn link_appends_url() {
        let lines = rendered_text("[Click](https://example.com)");
        assert_eq!(lines, vec!["Click (https://example.com)"]);
    }

    #[test]
    fn link_url_has_accent_underline_style() {
        let t = theme();
        let text = render_markdown("[text](https://example.com)", &t, 0);
        let url_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("https://example.com"))
            .unwrap();
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(url_span.style.fg, Some(t.accent));
    }

    // ── Word Wrapping ──

    #[test]
    fn paragraph_wraps_at_width() {
        let t = theme();
        let text = render_markdown("one two three four five six", &t, 15);
        assert!(
            text.lines.len() >= 2,
            "long paragraph should wrap into multiple lines, got {} line(s)",
            text.lines.len()
        );
    }

    #[test]
    fn heading_wraps_at_width() {
        let t = theme();
        let text = render_markdown("# one two three four five six", &t, 15);
        assert!(
            text.lines.len() >= 2,
            "long heading should wrap into multiple lines, got {} line(s)",
            text.lines.len()
        );
    }

    #[test]
    fn blockquote_wraps_with_marker_on_continuations() {
        let lines = rendered_text_at_width("> one two three four five six seven eight", 15);
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
        for line in &lines {
            assert!(
                line.starts_with("> "),
                "blockquote continuation must repeat `> ` marker: {line:?}"
            );
        }
    }

    #[test]
    fn nested_blockquote_wraps_with_nested_markers() {
        let lines = rendered_text_at_width("> > one two three four five six seven eight", 20);
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
        for line in &lines {
            assert!(
                line.starts_with("> > "),
                "nested blockquote continuation must repeat `> > ` markers: {line:?}"
            );
        }
    }

    #[test]
    fn tight_list_item_wraps_without_repeating_marker() {
        let lines = rendered_text_at_width("- one two three four five six seven eight", 15);
        assert!(lines.len() >= 2, "tight list item should wrap: {lines:?}");
        assert!(
            lines[0].starts_with("- "),
            "first line has marker: {lines:?}"
        );
        for line in &lines[1..] {
            assert!(
                line.starts_with("  ") && !line.starts_with("- "),
                "continuation should be space-indented, not a new marker: {line:?}"
            );
        }
    }

    #[test]
    fn blockquote_list_item_wraps_with_blockquote_marker_only() {
        let lines = rendered_text_at_width("> - one two three four five six seven eight", 20);
        assert!(lines.len() >= 2, "should wrap: {lines:?}");
        assert!(
            lines[0].starts_with("> - "),
            "first line has blockquote + list markers: {lines:?}"
        );
        for line in &lines[1..] {
            assert!(
                line.starts_with("> "),
                "continuation keeps blockquote marker: {line:?}"
            );
            assert!(
                !line.contains("- "),
                "continuation should not repeat list marker: {line:?}"
            );
        }
    }
}
