//! Markdown → [`ratatui::text::Text`] renderer. Walks the pulldown-cmark stream into styled
//! lines wrapped to terminal width. Wrapped lines keep the enclosing block's continuation indent.

use pulldown_cmark::{
    Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tracing::debug;
use unicode_width::UnicodeWidthStr;

use super::highlight::highlight_code;
use crate::tui::glyphs::{
    HORIZONTAL_RULE, TABLE_BOTTOM, TABLE_MIDDLE, TABLE_TOP, TableRule, VERTICAL_RULE,
};
use crate::tui::theme::Theme;
use crate::tui::wrap::{expand_tabs_in_spans, wrap_line, wrap_line_keeping_lead};

// ── Renderer ──

/// Stateful walker over a `pulldown-cmark` event stream that materializes each block as one or
/// more styled [`Line`]s.
///
/// The renderer is single-pass: every `Event::Start` pushes onto the relevant stack
/// (`indent_stack` for blockquote / list nesting, `inline_styles` for emphasis / strong / heading
/// modifiers), every `Event::End` pops, and inline events append spans to `lines.last_mut()`.
/// Block boundaries set `needs_newline = true` so the next block opener emits a separator
/// without producing trailing blanks.
pub(super) struct MarkdownRenderer<I> {
    // Core
    iter: I,
    theme: Theme,
    width: usize,
    pub(super) lines: Vec<Line<'static>>,

    /// Set after a block ends, so the next block opener emits a blank separator before its content.
    pub(super) needs_newline: bool,

    /// Block-prefix stack — each entry is the spans prepended to every line inside that block
    /// (blockquote `> `, list-item continuation indent, etc.). Wrapping uses the concatenation
    /// as the continuation prefix so deep nesting stays visually aligned.
    indent_stack: Vec<Vec<Span<'static>>>,
    /// One entry per nested list; `Some(n)` for ordered (next marker = `n. `), `None` for
    /// unordered (`- `).
    list_stack: Vec<Option<u64>>,
    /// List-item marker queued by `start_item`. The next `push_line` consumes it for the deepest
    /// indent slot so the marker prints in place of that level's continuation indent.
    pending_marker: Option<Vec<Span<'static>>>,

    inline_styles: Vec<Style>,
    link: Option<OpenLink>,

    code_block: CodeBlockState,
    table: TableState,
}

enum OpenLink {
    /// Autolink whose text is already the URL, so it is styled as a link with no suffix.
    Bare,
    /// Labeled link whose URL prints after the text.
    Labeled(String),
}

#[derive(Default)]
struct CodeBlockState {
    active: bool,
    lang: String,
    buf: String,
}

#[derive(Default)]
struct TableState {
    active: bool,
    in_head: bool,
    alignments: Vec<Alignment>,
    head_rows: usize,
    rows: Vec<Vec<Line<'static>>>,
    current_row: Vec<Line<'static>>,
    cell_buf: Vec<Span<'static>>,
}

impl<'a, I> MarkdownRenderer<I>
where
    I: Iterator<Item = Event<'a>>,
{
    pub(super) fn new(iter: I, theme: &Theme, width: usize, after_block: bool) -> Self {
        Self {
            iter,
            theme: theme.clone(),
            width,
            lines: Vec::new(),
            needs_newline: after_block,
            indent_stack: Vec::new(),
            list_stack: Vec::new(),
            pending_marker: None,
            inline_styles: Vec::new(),
            link: None,
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
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => self.start_link(link_type, dest_url),
            Tag::Table(alignments) => self.start_table(alignments),
            Tag::TableHead => self.table.in_head = true,
            Tag::TableRow => {}
            Tag::TableCell => self.table.cell_buf.clear(),
            Tag::HtmlBlock => self.start_block(),
            Tag::Image { .. }
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Subscript
            | Tag::Superscript => {
                debug!(?tag, "unsupported markdown tag");
            }
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_blockquote(),
            TagEnd::HtmlBlock => self.needs_newline = true,
            TagEnd::CodeBlock => self.end_code_block(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => {
                // Tight list items bypass `end_paragraph`, so wrap here.
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
                // pulldown-cmark 0.13: header cells sit inside TableHead without a TableRow.
                if !self.table.current_row.is_empty() {
                    self.flush_row();
                }
                self.table.in_head = false;
                self.table.head_rows = self.table.rows.len();
            }
            TagEnd::TableRow => self.flush_row(),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.table.cell_buf);
                self.table.current_row.push(Line::from(cell));
            }
            _ => {}
        }
    }

    // ── Block Elements ──

    fn start_paragraph(&mut self) {
        self.start_block();
        self.push_line(Line::default());
    }

    fn end_paragraph(&mut self) {
        self.wrap_last_line();
        self.needs_newline = true;
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        self.start_block();
        let style = match level {
            HeadingLevel::H1 => self.theme.heading_h1(),
            HeadingLevel::H2 => self.theme.heading_h2(),
            HeadingLevel::H3 => self.theme.heading_h3(),
            HeadingLevel::H4 | HeadingLevel::H5 | HeadingLevel::H6 => self.theme.heading_minor(),
        };
        let prefix = format!("{} ", "#".repeat(level as usize));
        self.push_line(Line::from(vec![Span::styled(prefix, style)]));
        self.push_inline_style(style);
    }

    fn end_heading(&mut self) {
        self.pop_inline_style();
        self.wrap_last_line();
        self.needs_newline = true;
    }

    fn start_blockquote(&mut self) {
        self.start_block();
        self.indent_stack
            .push(vec![Span::styled("> ", self.theme.blockquote())]);
    }

    fn end_blockquote(&mut self) {
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    fn rule(&mut self) {
        self.start_block();
        self.push_line(Line::from(vec![Span::styled(
            HORIZONTAL_RULE.repeat(3),
            self.theme.horizontal_rule(),
        )]));
        self.needs_newline = true;
    }

    fn html(&mut self, html: &str) {
        for line in html.lines() {
            self.push_line(Line::from(line.to_owned()));
        }
    }

    // ── Lists ──

    fn start_list(&mut self, start_index: Option<u64>) {
        if self.list_stack.is_empty() {
            self.start_block();
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
        self.start_block();
        let lang = match kind {
            CodeBlockKind::Fenced(lang) => lang.into_string(),
            CodeBlockKind::Indented => String::new(),
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
        let lang = std::mem::take(&mut self.code_block.lang);

        for line in highlight_code(&lang, &code, &self.theme) {
            self.push_code_line(line);
        }
        self.needs_newline = true;
    }

    /// Pushes one highlighted line through the block indent and wrapping, keeping its line style.
    /// Wrapped rows hang at the code's own indent.
    fn push_code_line(&mut self, line: Line<'static>) {
        let style = line.style;
        let code = Line::from(expand_tabs_in_spans(line.spans));
        let code_width = code.width();
        let lead = leading_whitespace_width(&code);
        let line = self.prefixed(code);
        let keep = line.width() - code_width + lead;

        let start = self.lines.len();
        self.wrap_and_push_hanging(line, lead, keep);
        for row in &mut self.lines[start..] {
            row.style = style;
        }
    }

    // ── Tables ──

    fn start_table(&mut self, alignments: Vec<Alignment>) {
        self.start_block();
        self.table = TableState {
            active: true,
            alignments,
            ..Default::default()
        };
    }

    fn flush_row(&mut self) {
        let row = std::mem::take(&mut self.table.current_row);
        self.table.rows.push(row);
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

        self.lines
            .push(build_horizontal_rule(&col_widths, border_style, TABLE_TOP));

        for (row_idx, row) in rows.iter().enumerate() {
            let cell_style = if row_idx < head_rows {
                header_style
            } else {
                Style::default()
            };
            self.lines.extend(build_data_rows(
                row,
                &col_widths,
                &alignments,
                col_count,
                border_style,
                cell_style,
            ));

            if row_idx + 1 == head_rows && head_rows < rows.len() {
                self.lines.push(build_horizontal_rule(
                    &col_widths,
                    border_style,
                    TABLE_MIDDLE,
                ));
            }
        }

        self.lines.push(build_horizontal_rule(
            &col_widths,
            border_style,
            TABLE_BOTTOM,
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
        // Surrounding inline modifiers (bold / italic / heading) compose with the inline-code
        // palette: theme supplies the distinctive fg, enclosing style adds modifiers on top.
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

    fn start_link(&mut self, link_type: LinkType, dest_url: CowStr<'a>) {
        if matches!(link_type, LinkType::Autolink | LinkType::Email) {
            self.push_inline_style(self.theme.link());
            self.link = Some(OpenLink::Bare);
        } else {
            self.link = Some(OpenLink::Labeled(dest_url.into_string()));
        }
    }

    fn pop_link(&mut self) {
        if let Some(link) = self.link.take() {
            match link {
                OpenLink::Bare => self.pop_inline_style(),
                OpenLink::Labeled(url) => {
                    let suffix = [
                        Span::raw(" ("),
                        Span::styled(url, self.theme.link()),
                        Span::raw(")"),
                    ];
                    if self.table.active {
                        self.table.cell_buf.extend(suffix);
                    } else {
                        for span in suffix {
                            self.push_span(span);
                        }
                    }
                }
            }
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
    fn push_line(&mut self, line: Line<'static>) {
        let line = self.prefixed(line);
        self.wrap_and_push(line);
    }

    /// Prepends the indent prefixes and any pending list marker.
    fn prefixed(&mut self, line: Line<'static>) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // The pending list marker (if any) replaces the continuation indent at the deepest
        // indent level so the bullet / number lands at the correct column for that nesting depth.
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
        Line::from(spans)
    }

    fn wrap_and_push(&mut self, line: Line<'static>) {
        self.wrap_and_push_hanging(line, 0, 0);
    }

    /// Wraps without breaking inside the first `keep` columns, indenting continuation rows by
    /// `hang` extra columns, capped at half the width left after the block indent.
    fn wrap_and_push_hanging(&mut self, line: Line<'static>, hang: usize, keep: usize) {
        if self.width == 0 {
            self.lines.push(line);
            return;
        }
        let mut cont_prefix = Line::from(self.continuation_indent_spans());
        let indent = cont_prefix.width();
        let hang = hang.min(self.width.saturating_sub(indent) / 2);
        if hang > 0 {
            cont_prefix.push_span(Span::raw(" ".repeat(hang)));
        }
        let prefix = if cont_prefix.spans.is_empty() {
            None
        } else {
            Some(cont_prefix.spans.as_slice())
        };
        self.lines.extend(wrap_line_keeping_lead(
            line,
            self.width,
            indent + hang,
            prefix,
            keep,
        ));
    }

    fn continuation_indent_spans(&self) -> Vec<Span<'static>> {
        self.indent_stack
            .iter()
            .flat_map(|prefix| prefix.iter().cloned())
            .collect()
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(line) = self.lines.last_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    /// Word-wrap the last accumulated line in place (retroactive wrap after span accumulation).
    fn wrap_last_line(&mut self) {
        if self.width == 0 {
            return;
        }
        let Some(line) = self.lines.pop() else {
            return;
        };
        self.wrap_and_push(line);
    }

    fn start_block(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
    }

    fn push_blank_line(&mut self) {
        self.lines.push(Line::default());
    }
}

// ── Code Helpers ──

fn leading_whitespace_width(line: &Line<'_>) -> usize {
    let mut width = 0;
    for span in &line.spans {
        let content = span.content.as_ref();
        let rest = content.trim_start();
        width += content[..content.len() - rest.len()].width();
        if !rest.is_empty() {
            break;
        }
    }
    width
}

// ── Table Helpers ──

const TABLE_CELL_PADDING: usize = 1;
const TABLE_COLUMN_OVERHEAD: usize = 2 * TABLE_CELL_PADDING + 1;

/// Computes the max display width for each column across all rows.
fn compute_column_widths(rows: &[Vec<Line<'_>>], col_count: usize) -> Vec<usize> {
    let mut widths = vec![0_usize; col_count];
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            widths[col] = widths[col].max(cell.width());
        }
    }
    widths
}

/// Shrink column widths so the rendered table fits within `width_budget`.
fn fit_column_widths(natural: &[usize], width_budget: usize) -> Vec<usize> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    let overhead = 1 + TABLE_COLUMN_OVERHEAD * n;
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
fn build_horizontal_rule(col_widths: &[usize], style: Style, rule: TableRule) -> Line<'static> {
    let mut buf = String::new();
    buf.push(rule.left);
    for (i, &w) in col_widths.iter().enumerate() {
        buf.push_str(&HORIZONTAL_RULE.repeat(w + 2 * TABLE_CELL_PADDING));
        buf.push(if i + 1 < col_widths.len() {
            rule.join
        } else {
            rule.right
        });
    }
    Line::from(Span::styled(buf, style))
}

/// Word-wraps a cell's spans, always returning at least one sub-line.
fn wrap_cell(cell: &Line<'static>, target_width: usize) -> Vec<Line<'static>> {
    if cell.spans.is_empty() || target_width == 0 {
        return vec![cell.clone()];
    }
    wrap_line(cell.clone(), target_width, 0, None)
}

/// Builds the visual lines for a data row, wrapping cells to column widths.
fn build_data_rows(
    row: &[Line<'static>],
    col_widths: &[usize],
    alignments: &[Alignment],
    col_count: usize,
    border_style: Style,
    cell_style: Style,
) -> Vec<Line<'static>> {
    let empty = Line::default();
    let cols: Vec<(Vec<Line<'static>>, usize)> = (0..col_count)
        .map(|col| {
            let cell = row.get(col).unwrap_or(&empty);
            let target_width = col_widths.get(col).copied().unwrap_or(0);
            (wrap_cell(cell, target_width), target_width)
        })
        .collect();

    let sub_row_count = cols.iter().map(|(c, _)| c.len()).max().unwrap_or(1).max(1);
    let pipe = Span::styled(VERTICAL_RULE, border_style);

    (0..sub_row_count)
        .map(|sub_idx| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 4 + 1);
            spans.push(pipe.clone());
            for (col, (sub_lines, target_width)) in cols.iter().enumerate() {
                let sub_cell = sub_lines.get(sub_idx).unwrap_or(&empty);
                let pad = target_width.saturating_sub(sub_cell.width());

                let alignment = alignments.get(col).copied().unwrap_or(Alignment::None);
                let (pad_left, pad_right) = match alignment {
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::Right => (pad, 0),
                    Alignment::Left | Alignment::None => (0, pad),
                };

                spans.push(Span::raw(" ".repeat(TABLE_CELL_PADDING + pad_left)));

                for span in &sub_cell.spans {
                    let styled = if cell_style == Style::default() {
                        span.clone()
                    } else {
                        Span::styled(span.content.clone(), span.style.patch(cell_style))
                    };
                    spans.push(styled);
                }

                spans.push(Span::raw(" ".repeat(TABLE_CELL_PADDING + pad_right)));
                spans.push(pipe.clone());
            }
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::{Color, Stylize};

    use super::super::render_markdown;
    use super::*;

    fn theme() -> Theme {
        Theme::default()
    }

    /// Renders `input` at `width` and joins each row's spans, with `width == 0` disabling wrapping.
    fn rendered_text_at_width(input: &str, width: usize) -> Vec<String> {
        render_markdown(input, &theme(), width)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// Renders without wrapping.
    fn rendered_text(input: &str) -> Vec<String> {
        rendered_text_at_width(input, 0)
    }

    // ── render_markdown ──

    #[test]
    fn render_markdown_plain_text_is_one_row() {
        let lines = rendered_text("Hello, world!");
        assert_eq!(lines, vec!["Hello, world!"]);
    }

    // ── render_markdown: paragraphs ──

    #[test]
    fn render_markdown_paragraphs_are_separated_by_one_blank_row() {
        let lines = rendered_text(indoc! {"
            First paragraph

            Second paragraph
        "});
        assert_eq!(lines, ["First paragraph", "", "Second paragraph"]);
    }

    #[test]
    fn render_markdown_paragraph_soft_break_joins() {
        let lines = rendered_text(indoc! {"
            Hello
            World
        "});
        assert_eq!(lines, vec!["Hello World"]);
    }

    // ── render_markdown: headings ──

    #[test]
    fn render_markdown_heading_levels_text_and_styles() {
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
        assert_eq!(h1.spans[0].style.fg, t.text.fg);

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

    // ── render_markdown: blockquotes ──

    #[test]
    fn render_markdown_blockquote_text_and_style() {
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
        assert_eq!(marker_span.style.fg, t.success.fg);
    }

    #[test]
    fn render_markdown_nested_blockquote_repeats_both_markers() {
        let lines = rendered_text(indoc! {"
            > Outer
            > > Inner
        "});
        assert_eq!(lines, ["> Outer", "", "> > Inner"]);
    }

    // ── render_markdown: horizontal rule ──

    #[test]
    fn render_markdown_horizontal_rule_text_and_style() {
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
        assert_eq!(rule_span.style.fg, t.dim.fg);
    }

    // ── render_markdown: lists ──

    #[test]
    fn render_markdown_ordered_list_puts_each_marker_on_its_item_row() {
        let tight = rendered_text(indoc! {"
            1. First
            2. Second
            3. Third
        "});
        let loose = rendered_text(indoc! {"
            1. First

            2. Second

            3. Third
        "});

        assert_eq!(tight, ["1. First", "2. Second", "3. Third"]);
        assert_eq!(loose, tight, "loose items render without blank rows");
    }

    #[test]
    fn render_markdown_double_digit_marker_stays_on_the_item_row() {
        let items: Vec<String> = (1..=12).map(|i| format!("{i}. Item {i}")).collect();

        assert_eq!(rendered_text(&items.join("\n")), items);
    }

    #[test]
    fn render_markdown_unordered_list_puts_each_marker_on_its_item_row() {
        let tight = rendered_text(indoc! {"
            - Alpha
            - Beta
        "});
        let loose = rendered_text(indoc! {"
            - Alpha

            - Beta
        "});

        assert_eq!(tight, ["- Alpha", "- Beta"]);
        assert_eq!(loose, tight, "loose items render without blank rows");
    }

    #[test]
    fn render_markdown_nested_list_indents_the_inner_item() {
        let lines = rendered_text(indoc! {"
            - Outer
              - Inner
        "});
        assert_eq!(lines, ["- Outer", "  - Inner"]);
    }

    #[test]
    fn render_markdown_list_marker_uses_accent_color() {
        let t = theme();
        let text = render_markdown("- Item", &t, 0);
        let marker_span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains("- ")));
        let span = marker_span.expect("list marker span not found");
        assert_eq!(span.style.fg, t.accent.fg);
    }

    #[test]
    fn render_markdown_inline_code_in_list_item() {
        let t = theme();
        let text = render_markdown("- Use `foo()` here", &t, 0);
        let code_span = text
            .lines
            .iter()
            .find_map(|line| line.spans.iter().find(|s| s.content.contains("foo()")))
            .expect("inline code span missing inside list item");
        assert_eq!(
            code_span.style.fg, t.user.fg,
            "inline code keeps peach fg inside list items"
        );
        assert_eq!(
            code_span.style.bg, None,
            "inline code is fg-only inside list items"
        );
    }

    #[test]
    fn render_markdown_link_in_list_item_appends_url_on_the_item_row() {
        assert_eq!(
            rendered_text("- [Click](https://example.com)"),
            ["- Click (https://example.com)"],
        );
    }

    // ── render_markdown: code blocks ──

    #[test]
    fn render_markdown_indented_code_preserves_content_without_language() {
        assert_eq!(rendered_text("    let x = 1;\n"), ["let x = 1;"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_without_language_keeps_content() {
        let lines = rendered_text(indoc! {"
            ```
            fn main() {}
            ```
        "});
        assert_eq!(lines, ["fn main() {}"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_plain_has_no_background() {
        // Plain fenced blocks (no language) use the `code`
        // style, which omits the bg fill so wrapping / width variance
        // across lines doesn't leave ragged highlight edges.
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
                effective_fg, t.code.fg,
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
    fn render_markdown_fenced_code_block_with_lang_highlights() {
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

    #[test]
    fn render_markdown_fenced_code_block_expands_tabs_to_stops() {
        let lines = rendered_text(indoc! {"
            ```go
            func f() {
            \treturn
            }
            ```
        "});
        assert_eq!(lines, ["func f() {", "    return", "}"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_wraps_long_lines_and_keeps_line_style() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                ```
                alpha beta gamma delta epsilon
                ```
            "},
            &t,
            12,
        );
        let rows: Vec<String> = text.lines.iter().map(ToString::to_string).collect();
        assert_eq!(rows, ["alpha beta ", "gamma delta ", "epsilon"]);
        assert!(text.lines.iter().all(|l| l.style == t.code()), "{text:?}");
    }

    #[test]
    fn render_markdown_fenced_code_block_wrapped_rows_hang_at_the_code_indent_up_to_half_width() {
        let lines = rendered_text_at_width(
            indoc! {"
                ```
                        format value;
                ```
            "},
            12,
        );
        assert_eq!(lines, ["        form", "      at ", "      value;"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_in_list_item_keeps_item_indent() {
        let lines = rendered_text(indoc! {"
            - item

              ```
              let x = 1;
              ```
        "});
        assert_eq!(lines, ["- item", "", "  let x = 1;"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_in_list_item_hangs_wrapped_rows_past_the_item_indent() {
        let lines = rendered_text_at_width(
            indoc! {"
                - item

                  ```
                      foo bar baz
                  ```
            "},
            12,
        );
        assert_eq!(
            lines,
            ["- item", "", "      foo ", "      bar ", "      baz"]
        );
    }

    #[test]
    fn render_markdown_fenced_code_block_as_first_item_child_takes_the_marker() {
        let lines = rendered_text(indoc! {"
            - ```
              let x = 1;
              ```
        "});
        assert_eq!(lines, ["- let x = 1;"]);
    }

    #[test]
    fn render_markdown_fenced_code_block_in_blockquote_keeps_marker_on_wrapped_rows() {
        let lines = rendered_text_at_width(
            indoc! {"
                > ```
                > alpha beta gamma
                > ```
            "},
            12,
        );
        assert_eq!(lines, ["> alpha beta", "> gamma"]);
    }

    // ── render_markdown: tables ──

    #[test]
    fn render_markdown_table_draws_borders_header_rule_and_rows() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
            | 1 | 2 |
            | 3 | 4 |
        "});
        assert_eq!(
            lines,
            [
                "┌───┬───┐",
                "│ A │ B │",
                "├───┼───┤",
                "│ 1 │ 2 │",
                "│ 3 │ 4 │",
                "└───┴───┘",
            ]
        );
    }

    #[test]
    fn render_markdown_table_aligns_cells_per_column() {
        let lines = rendered_text(indoc! {"
            | Left | Center | Right |
            |:-----|:------:|------:|
            | a    |   b    |     c |
        "});
        assert_eq!(
            lines,
            [
                "┌──────┬────────┬───────┐",
                "│ Left │ Center │ Right │",
                "├──────┼────────┼───────┤",
                "│ a    │   b    │     c │",
                "└──────┴────────┴───────┘",
            ]
        );
    }

    #[test]
    fn render_markdown_table_header_cells_are_bold() {
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
    fn render_markdown_table_cell_keeps_inline_code_style() {
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
            code_span.style.fg, t.user.fg,
            "inline code keeps peach fg inside table cells"
        );
        assert_eq!(
            code_span.style.bg, None,
            "inline code is fg-only inside table cells"
        );
    }

    #[test]
    fn render_markdown_table_empty_cell_keeps_its_borders() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
            |   | x |
        "});
        assert_eq!(
            lines,
            [
                "┌───┬───┐",
                "│ A │ B │",
                "├───┼───┤",
                "│   │ x │",
                "└───┴───┘"
            ]
        );
    }

    #[test]
    fn render_markdown_table_pads_short_rows_to_the_column_count() {
        let lines = rendered_text(indoc! {"
            | A | B | C |
            |---|---|---|
            | 1 | 2 |
        "});
        assert_eq!(
            lines,
            [
                "┌───┬───┬───┐",
                "│ A │ B │ C │",
                "├───┼───┼───┤",
                "│ 1 │ 2 │   │",
                "└───┴───┴───┘",
            ]
        );
    }

    #[test]
    fn render_markdown_table_without_body_omits_the_header_rule() {
        let lines = rendered_text(indoc! {"
            | A | B |
            |---|---|
        "});
        assert_eq!(lines, ["┌───┬───┐", "│ A │ B │", "└───┴───┘"]);
    }

    #[test]
    fn render_markdown_table_fits_width_budget_and_wraps_cells() {
        let lines = rendered_text_at_width(
            indoc! {"
                | # | Description                         |
                |---|-------------------------------------|
                | 1 | short                               |
                | 2 | a cell with enough words to wrap it |
            "},
            40,
        );
        assert_eq!(
            lines,
            [
                "┌───┬──────────────────────────────────┐",
                "│ # │ Description                      │",
                "├───┼──────────────────────────────────┤",
                "│ 1 │ short                            │",
                "│ 2 │ a cell with enough words to wrap │",
                "│   │ it                               │",
                "└───┴──────────────────────────────────┘",
            ]
        );
    }

    #[test]
    fn render_markdown_table_wrapped_cell_keeps_alignment_in_other_columns() {
        let lines = rendered_text_at_width(
            indoc! {"
                | ID | Note                               |
                |----|------------------------------------|
                | 42 | enough words here to force a wrap  |
            "},
            30,
        );
        assert_eq!(
            lines,
            [
                "┌────┬───────────────────────┐",
                "│ ID │ Note                  │",
                "├────┼───────────────────────┤",
                "│ 42 │ enough words here to  │",
                "│    │ force a wrap          │",
                "└────┴───────────────────────┘",
            ]
        );
    }

    // ── render_markdown: inline content ──

    #[test]
    fn render_markdown_strong_and_emphasis_set_bold_and_italic() {
        let text = render_markdown("**bold** and *italic*", &theme(), 0);
        assert_eq!(
            text.lines[0].spans,
            [
                Span::raw("bold").bold(),
                Span::raw(" and "),
                Span::raw("italic").italic(),
            ]
        );
    }

    #[test]
    fn render_markdown_strong_emphasis_sets_bold_and_italic_together() {
        let text = render_markdown("***bold italic***", &theme(), 0);
        assert_eq!(
            text.lines[0].spans,
            [Span::raw("bold italic").bold().italic()]
        );
    }

    #[test]
    fn render_markdown_strikethrough_crosses_out_text() {
        let text = render_markdown("~~struck~~", &theme(), 0);
        assert_eq!(text.lines[0].spans, [Span::raw("struck").crossed_out()]);
    }

    #[test]
    fn render_markdown_inline_code_uses_the_inline_code_style() {
        let t = theme();
        let text = render_markdown("Use `foo()` here", &t, 0);
        assert_eq!(
            text.lines[0].spans,
            [
                Span::raw("Use "),
                Span::styled("foo()", t.inline_code()),
                Span::raw(" here"),
            ]
        );
    }

    #[test]
    fn render_markdown_image_renders_its_alt_text_inline() {
        assert_eq!(
            rendered_text("before ![alt](https://example.com) after"),
            ["before alt after"],
        );
    }

    #[test]
    fn render_markdown_inline_code_inside_bold_inherits_bold() {
        let t = theme();
        let text = render_markdown("**use `foo()` here**", &t, 0);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("foo()"))
            .expect("code span not found");
        assert_eq!(
            span.style.fg, t.user.fg,
            "code span keeps its distinctive fg"
        );
        assert_eq!(span.style.bg, None, "code span stays fg-only");
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "code span inside **bold** should inherit BOLD modifier"
        );
    }

    #[test]
    fn render_markdown_inline_code_inside_heading_inherits_modifiers() {
        let t = theme();
        let text = render_markdown("# see `foo()`", &t, 0);
        let span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains("foo()")))
            .expect("code span not found in heading");
        assert_eq!(span.style.fg, t.user.fg);
        assert_eq!(span.style.bg, None);
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "code inside an H1 should inherit the heading's BOLD"
        );
        assert!(
            span.style.add_modifier.contains(Modifier::UNDERLINED),
            "code inside an H1 should inherit the heading's UNDERLINED"
        );
    }

    // ── render_markdown: html ──

    #[test]
    fn render_markdown_html_block_renders_one_row_per_line_between_blank_separators() {
        let lines = rendered_text(indoc! {"
            para

            <div>
            hello
            </div>

            after
        "});
        assert_eq!(lines, ["para", "", "<div>", "hello", "</div>", "", "after"]);
    }

    #[test]
    fn render_markdown_html_block_in_blockquote_keeps_the_marker_on_every_row() {
        let lines = rendered_text(indoc! {"
            > <div>
            > x
            > </div>
        "});
        assert_eq!(lines, ["> <div>", "> x", "> </div>"]);
    }

    #[test]
    fn render_markdown_inline_html_is_kept_as_text() {
        assert_eq!(rendered_text("text <br> more"), ["text <br> more"]);
    }

    // ── render_markdown: links ──

    #[test]
    fn render_markdown_link_appends_url() {
        let lines = rendered_text("[Click](https://example.com)");
        assert_eq!(lines, vec!["Click (https://example.com)"]);
    }

    #[test]
    fn render_markdown_link_url_has_accent_underline_style() {
        let t = theme();
        let text = render_markdown("[text](https://example.com)", &t, 0);
        let url_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("https://example.com"))
            .unwrap();
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(url_span.style.fg, t.accent.fg);
    }

    #[test]
    fn render_markdown_link_autolink_renders_url_once_in_link_style() {
        let t = theme();
        let text = render_markdown("<https://x.io>", &t, 0);
        assert_eq!(
            text.lines[0].spans,
            [Span::styled("https://x.io", t.link())]
        );
    }

    #[test]
    fn render_markdown_link_email_autolink_renders_address_once() {
        assert_eq!(rendered_text("<me@x.io>"), ["me@x.io"]);
    }

    #[test]
    fn render_markdown_link_in_table_cell_keeps_url_in_the_cell() {
        let lines = rendered_text(indoc! {"
            intro

            | site |
            | ---- |
            | [x](https://x.io) |
        "});
        assert_eq!(
            lines,
            [
                "intro",
                "",
                "┌──────────────────┐",
                "│ site             │",
                "├──────────────────┤",
                "│ x (https://x.io) │",
                "└──────────────────┘",
            ]
        );
    }

    // ── render_markdown: word wrapping ──

    #[test]
    fn render_markdown_paragraph_wraps_at_width() {
        assert_eq!(
            rendered_text_at_width("one two three four five six", 15),
            ["one two three ", "four five six"],
        );
    }

    #[test]
    fn render_markdown_heading_wraps_at_width() {
        assert_eq!(
            rendered_text_at_width("# one two three four five six", 15),
            ["# one two three", "four five six"],
        );
    }

    #[test]
    fn render_markdown_blockquote_wraps_with_marker_on_continuations() {
        assert_eq!(
            rendered_text_at_width("> one two three four five six seven eight", 15),
            ["> one two three", "> four five six", "> seven eight"],
        );
    }

    #[test]
    fn render_markdown_nested_blockquote_wraps_with_nested_markers() {
        assert_eq!(
            rendered_text_at_width("> > one two three four five six seven eight", 20),
            [
                "> > one two three ",
                "> > four five six ",
                "> > seven eight"
            ],
        );
    }

    #[test]
    fn render_markdown_tight_list_item_wraps_without_repeating_marker() {
        assert_eq!(
            rendered_text_at_width("- one two three four five six seven eight", 15),
            ["- one two three", "  four five six", "  seven eight"],
        );
    }

    #[test]
    fn render_markdown_blockquote_list_item_wraps_with_blockquote_marker_only() {
        assert_eq!(
            rendered_text_at_width("> - one two three four five six seven eight", 20),
            [
                "> - one two three ",
                ">   four five six ",
                ">   seven eight"
            ],
        );
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
        let cell = Line::from("one two three four");
        let out = wrap_cell(&cell, 8);
        assert!(out.len() >= 2, "long cell should wrap: {out:?}");
        for sub_line in &out {
            let width = sub_line.width();
            assert!(width <= 8, "sub-line exceeds target width: {sub_line:?}");
        }
    }

    #[test]
    fn wrap_cell_empty_produces_single_empty_sub_line() {
        let out = wrap_cell(&Line::default(), 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], Line::default());
    }

    #[test]
    fn wrap_cell_zero_target_width_preserves_cell() {
        let cell = Line::from("hello");
        let out = wrap_cell(&cell, 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], cell);
    }
}
