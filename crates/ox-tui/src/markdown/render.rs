//! Markdown → [`ratatui::text::Text`] renderer. Walks the pulldown-cmark stream into styled
//! lines wrapped to terminal width. Wrapped lines and table rows keep the enclosing block's
//! continuation indent. Table columns shrink to their longest word, below which rows stack as
//! `header: value` lines.

use pulldown_cmark::{
    Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, Tag, TagEnd,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tracing::debug;
use unicode_width::UnicodeWidthStr;

use super::highlight::highlight_code;
use crate::glyphs::{
    BLOCKQUOTE_PREFIX, HORIZONTAL_RULE, TABLE_BOTTOM, TABLE_MIDDLE, TABLE_TOP, TableRule,
    VERTICAL_RULE,
};
use crate::theme::Theme;
use crate::wrap::{expand_tabs_in_spans, longest_word_width, wrap_line, wrap_line_keeping_lead};

// ── Renderer ──

/// Stateful walker over a `pulldown-cmark` event stream that materializes each block as one or
/// more styled [`Line`]s.
///
/// The renderer is single-pass: every `Event::Start` pushes onto the relevant stack
/// (`indent_stack` for blockquote / list nesting, `inline_styles` for emphasis / strong / heading
/// modifiers), every `Event::End` pops, and inline events append spans to `lines.last_mut()`.
/// Block boundaries set `needs_newline = true` so the next block opener emits a separator
/// without producing trailing blanks.
pub(super) struct MarkdownRenderer<'t, I> {
    iter: I,
    theme: &'t Theme,
    /// Style of the body, which fills the spans still without a color once the walk ends and
    /// decides the quote style.
    base: Style,
    width: usize,
    pub(super) lines: Vec<Line<'static>>,

    /// Whether the next block opener emits a blank separator first, set when a block ends.
    pub(super) needs_newline: bool,
    /// Whether the last line still collects inline spans, so `finish_line` wraps it exactly once.
    line_open: bool,

    /// Block-prefix stack. Each entry holds the spans prepended to every line inside that block
    /// (blockquote `│ `, list-item continuation indent, etc.). Wrapping uses the concatenation
    /// as the continuation prefix so deep nesting stays visually aligned.
    indent_stack: Vec<Vec<Span<'static>>>,
    /// One entry per nested list: `Some(n)` for ordered (next marker = `n. `) and `None` for
    /// unordered (`- `).
    list_stack: Vec<Option<u64>>,
    /// List-item markers queued by `start_item`, each with the `indent_stack` depth of its item.
    /// The next line built by `prefixed` prints every marker in place of the continuation indent
    /// at its depth, so an item whose first child adds a prefix keeps its marker in front.
    pending_markers: Vec<(usize, Vec<Span<'static>>)>,
    /// Index into `lines` where each open blockquote starts.
    blockquote_starts: Vec<usize>,

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

/// A table cell's lines, which `<br>` tags separate.
type Cell = Vec<Line<'static>>;

#[derive(Default)]
struct TableState {
    active: bool,
    in_head: bool,
    alignments: Vec<Alignment>,
    head_rows: usize,
    rows: Vec<Vec<Cell>>,
    current_row: Vec<Cell>,
    /// Lines of the open cell before the one `cell_buf` collects.
    cell_lines: Cell,
    cell_buf: Vec<Span<'static>>,
}

impl TableState {
    /// Ends the open cell's current line as a `<br>` does, trimming whitespace around the break.
    fn break_cell_line(&mut self) {
        let mut line = Line::from(std::mem::take(&mut self.cell_buf));
        if !self.cell_lines.is_empty() {
            trim_line_start(&mut line);
        }
        trim_line_end(&mut line);
        self.cell_lines.push(line);
    }

    /// Ends the open cell, dropping the empty lines that trailing `<br>` tags leave.
    fn end_cell(&mut self) {
        self.break_cell_line();
        let mut cell = std::mem::take(&mut self.cell_lines);
        while cell.len() > 1 && cell.last().is_some_and(|line| line.width() == 0) {
            cell.pop();
        }
        self.current_row.push(cell);
    }
}

impl<'a, 't, I> MarkdownRenderer<'t, I>
where
    I: Iterator<Item = Event<'a>>,
{
    pub(super) fn new(
        iter: I,
        theme: &'t Theme,
        base: Style,
        width: usize,
        after_block: bool,
    ) -> Self {
        Self {
            iter,
            theme,
            base,
            width,
            lines: Vec::new(),
            needs_newline: after_block,
            line_open: false,
            indent_stack: Vec::new(),
            list_stack: Vec::new(),
            pending_markers: Vec::new(),
            blockquote_starts: Vec::new(),
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
        for line in &mut self.lines {
            fill_uncolored_spans(line, self.base);
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
            TagEnd::Item => self.end_item(),
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
            TagEnd::TableCell => self.table.end_cell(),
            TagEnd::Image
            | TagEnd::FootnoteDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Subscript
            | TagEnd::Superscript => {}
        }
    }

    // ── Block Elements ──

    fn start_paragraph(&mut self) {
        self.start_block();
        self.push_line(Line::default());
    }

    fn end_paragraph(&mut self) {
        self.finish_line();
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
        self.finish_line();
        self.needs_newline = true;
    }

    fn start_blockquote(&mut self) {
        self.start_block();
        self.indent_stack.push(vec![Span::styled(
            BLOCKQUOTE_PREFIX,
            self.theme.blockquote(),
        )]);
        self.blockquote_starts.push(self.lines.len());
    }

    /// Fills the quote's uncolored spans with [`Self::quote_style`], so headings, links, code, and
    /// table headers keep their own colors.
    fn end_blockquote(&mut self) {
        let start = self
            .blockquote_starts
            .pop()
            .expect("every blockquote end follows its start");
        let style = self.quote_style();
        for line in &mut self.lines[start..] {
            fill_uncolored_spans(line, style);
        }
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    /// Style of quoted text: `muted` over a body with no color or in `text`, and otherwise the
    /// body's style, since a body in another color, such as thinking, already reads as secondary.
    fn quote_style(&self) -> Style {
        if self.base.fg.is_none() || self.base.fg == self.theme.text().fg {
            self.base.patch(self.theme.muted())
        } else {
            self.base
        }
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
        } else {
            self.finish_line();
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
                let m = format!("{index}. ");
                *index += 1;
                m
            }
            _ => "- ".to_owned(),
        };

        let continuation = vec![Span::raw(" ".repeat(marker.len()))];
        let marker = vec![Span::styled(marker, self.theme.list_marker())];
        self.pending_markers.push((self.indent_stack.len(), marker));
        self.indent_stack.push(continuation);
        self.needs_newline = false;
    }

    fn end_item(&mut self) {
        // Tight list items bypass `end_paragraph`, so wrap here.
        self.finish_line();
        self.indent_stack.pop();
        let depth = self.indent_stack.len();
        self.pending_markers
            .retain(|&(marker_depth, _)| marker_depth < depth);
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

        for line in highlight_code(&lang, &code, self.theme) {
            self.push_code_line(line);
        }
        self.needs_newline = true;
    }

    /// Pushes one highlighted line through the block indent and wrapping, keeping its line style.
    /// Wrapped rows hang at the code's own indent.
    fn push_code_line(&mut self, line: Line<'static>) {
        let code = Line {
            spans: expand_tabs_in_spans(line.spans),
            ..line
        };
        let code_width = code.width();
        let lead = leading_whitespace_width(&code);
        let line = self.prefixed(code);
        let keep = line.width() - code_width + lead;

        self.wrap_and_push_hanging(line, lead, keep);
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
            mut rows,
            alignments,
            head_rows,
            ..
        } = std::mem::take(&mut self.table);

        let header_style = self.theme.table_header();
        for line in rows.iter_mut().take(head_rows).flatten().flatten() {
            for span in &mut line.spans {
                span.style = header_style.patch(span.style);
            }
        }

        let col_count = alignments
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        let columns = measure_columns(&rows, col_count);
        let col_widths = if self.width == 0 {
            Some(columns.iter().map(|column| column.natural).collect())
        } else {
            fit_column_widths(&columns, self.width.saturating_sub(self.indent_width()))
        };
        match col_widths {
            Some(col_widths) => self.push_table_box(&rows, &alignments, &col_widths),
            None => self.push_stacked_table(&rows, head_rows),
        }

        self.needs_newline = true;
    }

    fn push_table_box(
        &mut self,
        rows: &[Vec<Cell>],
        alignments: &[Alignment],
        col_widths: &[usize],
    ) {
        let border_style = self.theme.table_border();
        let row_rule = build_horizontal_rule(col_widths, border_style, TABLE_MIDDLE);

        self.push_line(build_horizontal_rule(col_widths, border_style, TABLE_TOP));
        for (row_idx, row) in rows.iter().enumerate() {
            if row_idx > 0 {
                self.push_line(row_rule.clone());
            }
            for line in build_data_rows(row, col_widths, alignments, border_style) {
                self.push_line(line);
            }
        }
        self.push_line(build_horizontal_rule(
            col_widths,
            border_style,
            TABLE_BOTTOM,
        ));
    }

    /// Lays out each body row as `header: value` lines separated by rules. A table with no body
    /// lists its header cells.
    fn push_stacked_table(&mut self, rows: &[Vec<Cell>], head_rows: usize) {
        let (head, body) = rows.split_at(head_rows);
        let (labels, blocks) = if body.is_empty() {
            (None, head)
        } else {
            (head.first(), body)
        };
        let label_style = self.theme.table_header();
        let rule = Line::from(Span::styled(
            HORIZONTAL_RULE.repeat(self.width.saturating_sub(self.indent_width())),
            self.theme.table_border(),
        ));

        for (row_idx, row) in blocks.iter().enumerate() {
            if row_idx > 0 {
                self.push_line(rule.clone());
            }
            for (col, cell) in row.iter().enumerate() {
                let label = labels
                    .and_then(|labels| labels.get(col))
                    .map(|label| joined_cell_spans(label))
                    .filter(|label| !label.is_empty());
                let mut spans = Vec::new();
                if let Some(label) = label {
                    spans.extend(label);
                    spans.push(Span::styled(":", label_style));
                    spans.push(Span::raw(" "));
                }
                spans.extend(joined_cell_spans(cell));
                let line = self.prefixed(Line::from(spans));
                self.wrap_and_push_hanging(line, TABLE_STACKED_HANG, 0);
            }
        }
    }

    // ── Inline Content ──

    fn text(&mut self, text: &str) {
        if self.code_block.active {
            self.code_block.buf.push_str(text);
            return;
        }
        if self.table.active {
            self.emit_span(Span::styled(text.to_owned(), self.current_inline_style()));
            return;
        }

        if !self.pending_markers.is_empty() {
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
        self.emit_span(Span::styled(code.into_string(), style));
    }

    /// Writes inline HTML verbatim, except that a `<br>` in a table cell breaks the cell's line,
    /// since a GFM cell has no other way to hold several lines.
    fn inline_html(&mut self, html: &str) {
        if self.table.active && is_line_break_tag(html) {
            self.table.break_cell_line();
        } else {
            self.emit_span(Span::raw(html.to_owned()));
        }
    }

    /// Appends `span` to the open table cell, or else to the current line.
    fn emit_span(&mut self, span: Span<'static>) {
        if self.table.active {
            self.table.cell_buf.push(span);
        } else {
            self.push_span(span);
        }
    }

    fn soft_break(&mut self) {
        self.push_span(Span::raw(" "));
    }

    fn hard_break(&mut self) {
        self.finish_line();
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
                    for span in suffix {
                        self.emit_span(span);
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

    /// Starts a new output line with indent prefixes and any pending list markers.
    fn push_line(&mut self, line: Line<'static>) {
        let line = self.prefixed(line);
        self.wrap_and_push(line);
    }

    /// Prepends the indent prefixes, each pending marker in place of the indent at its depth.
    fn prefixed(&mut self, mut line: Line<'static>) -> Line<'static> {
        let markers = std::mem::take(&mut self.pending_markers);
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (depth, prefix) in self.indent_stack.iter().enumerate() {
            let marker = markers
                .iter()
                .find_map(|(marker_depth, marker)| (*marker_depth == depth).then_some(marker));
            spans.extend(marker.unwrap_or(prefix).iter().cloned());
        }
        line.spans.splice(0..0, spans);
        line
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

    fn indent_width(&self) -> usize {
        self.indent_stack.iter().flatten().map(Span::width).sum()
    }

    fn push_span(&mut self, span: Span<'static>) {
        match self.lines.last_mut() {
            Some(line) if self.pending_markers.is_empty() => line.push_span(span),
            _ => self.push_line(Line::from(vec![span])),
        }
        self.line_open = true;
    }

    /// Wraps only the open line, since rewrapping a finished one could split it under a new prefix.
    fn finish_line(&mut self) {
        if !std::mem::take(&mut self.line_open) || self.width == 0 {
            return;
        }
        let line = self
            .lines
            .pop()
            .expect("push_span leaves a line whenever it opens one");
        self.wrap_and_push(line);
    }

    /// Separates the next block from the one before it, first finishing an open line such as a
    /// tight list item's text.
    fn start_block(&mut self) {
        self.finish_line();
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
    }

    fn push_blank_line(&mut self) {
        self.lines.push(Line::default());
    }
}

// ── Style Helpers ──

/// Patches `base` onto spans without a foreground. A line colored as a whole, such as a plain code
/// fence, keeps its color, since any span color would paint over it.
fn fill_uncolored_spans(line: &mut Line<'_>, base: Style) {
    if line.style.fg.is_some() {
        return;
    }
    for span in &mut line.spans {
        if span.style.fg.is_none() {
            span.style = span.style.patch(base);
        }
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
const TABLE_STACKED_HANG: usize = 2;

/// Widest word a column keeps whole. A longer word, such as a path or URL, breaks mid-word so the
/// column stays narrow enough for the boxed layout.
const TABLE_MAX_WORD_WIDTH: usize = 30;

/// Returns whether `html` is a `<br>` tag in any of its spellings, such as `<br/>` or `<BR />`.
fn is_line_break_tag(html: &str) -> bool {
    let tag = html.trim().to_ascii_lowercase();
    tag.strip_prefix("<br")
        .and_then(|rest| rest.strip_suffix('>'))
        .is_some_and(|rest| matches!(rest.trim(), "" | "/"))
}

fn trim_line_start(line: &mut Line<'_>) {
    while let Some(span) = line.spans.first_mut() {
        let lead = span.content.len() - span.content.trim_start().len();
        if lead == span.content.len() {
            line.spans.remove(0);
            continue;
        }
        if lead > 0 {
            span.content.to_mut().drain(..lead);
        }
        return;
    }
}

fn trim_line_end(line: &mut Line<'_>) {
    while let Some(span) = line.spans.last_mut() {
        let kept = span.content.trim_end().len();
        if kept == 0 {
            line.spans.pop();
            continue;
        }
        if kept < span.content.len() {
            span.content.to_mut().truncate(kept);
        }
        return;
    }
}

#[derive(Clone, Copy, Default)]
struct ColumnWidth {
    natural: usize,
    /// Longest word, up to [`TABLE_MAX_WORD_WIDTH`].
    min: usize,
}

fn measure_columns(rows: &[Vec<Cell>], col_count: usize) -> Vec<ColumnWidth> {
    let mut columns = vec![ColumnWidth::default(); col_count];
    for row in rows {
        for (column, cell) in columns.iter_mut().zip(row) {
            for line in cell {
                let word = longest_word_width(line).min(TABLE_MAX_WORD_WIDTH);
                column.min = column.min.max(word);
                // Wrapping sums grapheme widths, which count some ligatures wider than
                // `Line::width` does.
                column.natural = column.natural.max(line.width().max(word));
            }
        }
    }
    columns
}

/// Fits the columns into a table `width` wide, borders included, or returns `None` when even
/// their minimums don't fit.
fn fit_column_widths(columns: &[ColumnWidth], width: usize) -> Option<Vec<usize>> {
    let budget = width.checked_sub(1 + TABLE_COLUMN_OVERHEAD * columns.len())?;
    let natural: usize = columns.iter().map(|column| column.natural).sum();
    if natural <= budget {
        return Some(columns.iter().map(|column| column.natural).collect());
    }

    let min: usize = columns.iter().map(|column| column.min).sum();
    let spare = budget.checked_sub(min)?;
    let shortfall = natural - min;
    let mut col_widths: Vec<usize> = columns
        .iter()
        .map(|column| column.min + (column.natural - column.min) * spare / shortfall)
        .collect();

    // Rounding down loses under one cell of width per short column, so one pass hands out the rest.
    let mut leftover = budget - col_widths.iter().sum::<usize>();
    for (col_width, column) in col_widths.iter_mut().zip(columns) {
        if leftover > 0 && *col_width < column.natural {
            *col_width += 1;
            leftover -= 1;
        }
    }
    Some(col_widths)
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

/// Word-wraps each of a cell's lines, dropping the whitespace left at each break and always
/// returning at least one sub-line.
fn wrap_cell(cell: &[Line<'static>], target_width: usize) -> Vec<Line<'static>> {
    if cell.is_empty() {
        return vec![Line::default()];
    }
    cell.iter()
        .flat_map(|line| wrap_line(line.clone(), target_width, 0, None))
        .map(|mut line| {
            trim_line_end(&mut line);
            line
        })
        .collect()
}

/// Builds the visual lines for a data row, wrapping cells to column widths.
fn build_data_rows(
    row: &[Cell],
    col_widths: &[usize],
    alignments: &[Alignment],
    border_style: Style,
) -> Vec<Line<'static>> {
    let empty = Line::default();
    let cols: Vec<Vec<Line<'static>>> = col_widths
        .iter()
        .enumerate()
        .map(|(col, &target_width)| {
            let cell = row.get(col).map_or(&[][..], Vec::as_slice);
            wrap_cell(cell, target_width)
        })
        .collect();

    let sub_row_count = cols.iter().map(Vec::len).max().unwrap_or(1);
    let pipe = Span::styled(VERTICAL_RULE, border_style);

    (0..sub_row_count)
        .map(|sub_idx| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_widths.len() * 4 + 1);
            spans.push(pipe.clone());
            for (col, (sub_lines, &target_width)) in cols.iter().zip(col_widths).enumerate() {
                let sub_cell = sub_lines.get(sub_idx).unwrap_or(&empty);
                let pad = target_width.saturating_sub(sub_cell.width());

                let alignment = alignments.get(col).copied().unwrap_or(Alignment::None);
                let (pad_left, pad_right) = match alignment {
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    Alignment::Right => (pad, 0),
                    Alignment::Left | Alignment::None => (0, pad),
                };

                spans.push(Span::raw(" ".repeat(TABLE_CELL_PADDING + pad_left)));
                spans.extend(sub_cell.spans.iter().cloned());
                spans.push(Span::raw(" ".repeat(TABLE_CELL_PADDING + pad_right)));
                spans.push(pipe.clone());
            }
            Line::from(spans)
        })
        .collect()
}

/// Joins a cell's lines with spaces, for the stacked layout.
fn joined_cell_spans(cell: &[Line<'static>]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for line in cell.iter().filter(|line| line.width() > 0) {
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.extend(line.spans.iter().cloned());
    }
    spans
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::{Color, Stylize};

    use super::*;
    use crate::markdown::{render_markdown, render_markdown_on};

    mod render_markdown {
        use super::*;

        fn theme() -> Theme {
            Theme::default()
        }

        /// Renders `input` at `width` and joins each row's raw span text, with `width == 0`
        /// disabling wrapping. `Line::to_string` would drop a line ending left in a span.
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

        #[test]
        fn plain_text_is_one_row() {
            let lines = rendered_text("Hello, world!");
            assert_eq!(lines, vec!["Hello, world!"]);
        }

        #[test]
        fn paragraphs_are_separated_by_one_blank_row() {
            let lines = rendered_text(indoc! {"
                First paragraph

                Second paragraph
            "});
            assert_eq!(lines, ["First paragraph", "", "Second paragraph"]);
        }

        #[test]
        fn paragraph_soft_break_joins() {
            let lines = rendered_text(indoc! {"
                Hello
                World
            "});
            assert_eq!(lines, vec!["Hello World"]);
        }

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

        #[test]
        fn blockquote_draws_a_bar_before_muted_text() {
            let t = theme();

            let text = render_markdown("> Quoted *text*", &t, 0);

            assert_eq!(
                text.lines,
                [Line::from(vec![
                    Span::styled("│ ", t.blockquote()),
                    Span::styled("Quoted ", t.muted()),
                    Span::styled("text", t.muted().add_modifier(Modifier::ITALIC)),
                ])]
            );
        }

        #[test]
        fn blockquote_text_returns_to_the_default_style_after_the_quote() {
            let t = theme();

            let text = render_markdown("> quoted\n\nafter", &t, 0);

            assert_eq!(text.lines[2], Line::from(Span::raw("after")));
        }

        #[test]
        fn blockquote_leaves_code_in_its_line_color() {
            let t = theme();

            let text = render_markdown("> ```\n> plain\n> ```", &t, 0);

            let quoted = Line::from(vec![Span::styled("│ ", t.blockquote()), Span::raw("plain")]);
            assert_eq!(text.lines, [quoted.style(t.code())]);
        }

        #[test]
        fn nested_blockquote_repeats_both_markers() {
            let lines = rendered_text(indoc! {"
                > Outer
                > > Inner
            "});
            assert_eq!(lines, ["│ Outer", "", "│ │ Inner"]);
        }

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
            assert_eq!(rule_span.style, t.horizontal_rule());
        }

        #[test]
        fn ordered_list_puts_each_marker_on_its_item_row() {
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
        fn double_digit_marker_stays_on_the_item_row() {
            let items: Vec<String> = (1..=12).map(|i| format!("{i}. Item {i}")).collect();

            assert_eq!(rendered_text(&items.join("\n")), items);
        }

        #[test]
        fn unordered_list_puts_each_marker_on_its_item_row() {
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
        fn nested_list_indents_the_inner_item() {
            let lines = rendered_text(indoc! {"
                - Outer
                  - Inner
            "});
            assert_eq!(lines, ["- Outer", "  - Inner"]);
        }

        #[test]
        fn list_item_keeps_its_marker_in_front_of_a_first_child_prefix() {
            let quote = rendered_text_at_width("- > one two three four", 12);
            let nested = rendered_text(indoc! {"
                - - inner
                  - second
            "});

            assert_eq!(quote, ["- │ one two ", "  │ three ", "  │ four"]);
            assert_eq!(nested, ["- - inner", "  - second"]);
        }

        #[test]
        fn empty_list_item_drops_only_its_own_marker() {
            let empty_first_inner = rendered_text(indoc! {"
                1. 1.
                   2. second
            "});
            let empty_then_quote = rendered_text(indoc! {"
                -

                > after
            "});

            assert_eq!(empty_first_inner, ["1. 2. second"]);
            assert_eq!(empty_then_quote, ["", "│ after"]);
        }

        #[test]
        fn list_item_starting_with_inline_html_or_an_empty_link_keeps_its_marker() {
            let kbd = rendered_text("- a\n- <kbd>Ctrl</kbd>+C");
            let sup = rendered_text("- a\n- <sup>1</sup> note");
            let empty_link = rendered_text("- a\n- [](https://x.io)");

            assert_eq!(kbd, ["- a", "- <kbd>Ctrl</kbd>+C"]);
            assert_eq!(sup, ["- a", "- <sup>1</sup> note"]);
            assert_eq!(empty_link, ["- a", "-  (https://x.io)"]);
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
            assert_eq!(span.style.fg, t.accent.fg);
        }

        #[test]
        fn inline_code_leading_a_list_item_keeps_its_style_after_the_marker() {
            let t = theme();

            let text = render_markdown("- a\n- `foo()` here", &t, 0);

            assert_eq!(
                text.lines[1].spans,
                [
                    Span::styled("- ", t.list_marker()),
                    Span::styled("foo()", t.inline_code()),
                    Span::raw(" here"),
                ]
            );
        }

        #[test]
        fn link_in_list_item_appends_url_on_the_item_row() {
            assert_eq!(
                rendered_text("- [Click](https://example.com)"),
                ["- Click (https://example.com)"],
            );
        }

        #[test]
        fn indented_code_preserves_content_without_language() {
            assert_eq!(rendered_text("    let x = 1;\n"), ["let x = 1;"]);
        }

        #[test]
        fn fenced_code_block_without_language_keeps_content() {
            let lines = rendered_text(indoc! {"
                ```
                fn main() {}
                ```
            "});
            assert_eq!(lines, ["fn main() {}"]);
        }

        #[test]
        fn fenced_code_block_plain_has_no_background() {
            // Plain fenced blocks (no language) use the `code` style, which omits the bg fill so
            // wrapping / width variance across lines doesn't leave ragged highlight edges.
            //
            // The fallback path builds each line via `Line::styled(..)`, so the style lands on the
            // Line and the spans stay unstyled. Walk both layers to guard against either placement.
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

        #[test]
        fn fenced_code_block_expands_tabs_to_stops() {
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
        fn fenced_code_block_wraps_long_lines_and_keeps_line_style() {
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
        fn fenced_code_block_wrapped_rows_hang_at_the_code_indent_up_to_half_width() {
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
        fn fenced_code_block_in_list_item_keeps_item_indent() {
            let lines = rendered_text(indoc! {"
                - item

                  ```
                  let x = 1;
                  ```
            "});
            assert_eq!(lines, ["- item", "", "  let x = 1;"]);
        }

        #[test]
        fn fenced_code_block_in_list_item_hangs_wrapped_rows_past_the_item_indent() {
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
        fn fenced_code_block_as_first_item_child_takes_the_marker() {
            let lines = rendered_text(indoc! {"
                - ```
                  let x = 1;
                  ```
            "});
            assert_eq!(lines, ["- let x = 1;"]);
        }

        #[test]
        fn fenced_code_block_in_blockquote_keeps_marker_on_wrapped_rows() {
            let lines = rendered_text_at_width(
                indoc! {"
                    > ```
                    > alpha beta gamma
                    > ```
                "},
                12,
            );
            assert_eq!(lines, ["│ alpha beta", "│ gamma"]);
        }

        #[test]
        fn table_draws_borders_and_a_rule_between_every_row() {
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
                    "├───┼───┤",
                    "│ 3 │ 4 │",
                    "└───┴───┘",
                ]
            );
        }

        #[test]
        fn table_aligns_cells_per_column() {
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
        fn table_styles_header_cells_and_borders_from_their_slots() {
            let t = theme();
            let input = indoc! {"
                | Name |
                |------|
                | a    |
                | b    |
            "};

            let boxed = render_markdown(input, &t, 0).lines;
            let stacked = render_markdown(input, &t, 7).lines;

            assert_eq!(boxed[1].spans[2], Span::styled("Name", t.table_header()));
            assert_eq!(boxed[3].spans[2], Span::raw("a"));
            assert_eq!(boxed[1].spans[0], Span::styled("│", t.table_border()));
            for rule in [&boxed[0], &boxed[2], &boxed[4], &boxed[6]] {
                assert_eq!(rule.spans[0].style, t.table_border(), "{rule}");
            }
            assert_eq!(
                stacked[0].spans,
                [
                    Span::styled("Name", t.table_header()),
                    Span::styled(":", t.table_header()),
                    Span::raw(" "),
                    Span::raw("a"),
                ]
            );
            assert_eq!(
                stacked[1].spans,
                [Span::styled("───────", t.table_border())]
            );
        }

        #[test]
        fn table_inline_code_keeps_its_color_in_header_and_body_cells() {
            let t = theme();

            let text = render_markdown(
                indoc! {"
                    | `head` |
                    |--------|
                    | `body` |
                "},
                &t,
                0,
            );

            assert_eq!(
                text.lines[1].spans[2],
                Span::styled("head", t.table_header().patch(t.inline_code()))
            );
            assert_eq!(
                text.lines[3].spans[2],
                Span::styled("body", t.inline_code())
            );
        }

        #[test]
        fn table_empty_cell_keeps_its_borders() {
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
        fn table_pads_short_rows_to_the_column_count() {
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
        fn table_without_body_omits_the_header_rule() {
            let lines = rendered_text(indoc! {"
                | A | B |
                |---|---|
            "});
            assert_eq!(lines, ["┌───┬───┐", "│ A │ B │", "└───┴───┘"]);
        }

        #[test]
        fn table_fits_width_budget_and_rules_off_a_wrapped_row() {
            let lines = rendered_text_at_width(
                indoc! {"
                    | Check  | Result |
                    |--------|--------|
                    | Build  | passed |
                    | Tests  | 2798 passed after the flaky one was retried |
                    | Lint   | clean  |
                    | 格式化 | 通过   |
                "},
                40,
            );

            assert_eq!(
                lines,
                [
                    "┌────────┬─────────────────────────────┐",
                    "│ Check  │ Result                      │",
                    "├────────┼─────────────────────────────┤",
                    "│ Build  │ passed                      │",
                    "├────────┼─────────────────────────────┤",
                    "│ Tests  │ 2798 passed after the flaky │",
                    "│        │ one was retried             │",
                    "├────────┼─────────────────────────────┤",
                    "│ Lint   │ clean                       │",
                    "├────────┼─────────────────────────────┤",
                    "│ 格式化 │ 通过                        │",
                    "└────────┴─────────────────────────────┘",
                ]
            );
        }

        #[test]
        fn table_narrow_width_shrinks_the_wide_columns_and_keeps_the_rules_aligned() {
            let lines = rendered_text_at_width(
                indoc! {"
                    | Step | Run         | Note             |
                    |------|-------------|------------------|
                    | 1    | cargo build | makes the binary |
                    | 2    | cargo test  | runs all tests   |
                "},
                26,
            );

            assert_eq!(
                lines,
                [
                    "┌──────┬────────┬────────┐",
                    "│ Step │ Run    │ Note   │",
                    "├──────┼────────┼────────┤",
                    "│ 1    │ cargo  │ makes  │",
                    "│      │ build  │ the    │",
                    "│      │        │ binary │",
                    "├──────┼────────┼────────┤",
                    "│ 2    │ cargo  │ runs   │",
                    "│      │ test   │ all    │",
                    "│      │        │ tests  │",
                    "└──────┴────────┴────────┘",
                ]
            );
        }

        #[test]
        fn table_narrow_width_keeps_each_column_as_wide_as_its_longest_word() {
            let lines = rendered_text_at_width(
                indoc! {"
                    | Command                    | Purpose            |
                    |----------------------------|--------------------|
                    | cargo clippy --all-targets | lints every target |
                "},
                30,
            );

            assert_eq!(
                lines,
                [
                    "┌─────────────────┬──────────┐",
                    "│ Command         │ Purpose  │",
                    "├─────────────────┼──────────┤",
                    "│ cargo clippy    │ lints    │",
                    "│ --all-targets   │ every    │",
                    "│                 │ target   │",
                    "└─────────────────┴──────────┘",
                ]
            );
        }

        #[test]
        fn table_wide_and_ligature_characters_keep_every_row_as_wide_as_the_rules() {
            let narrowest_tables = [
                indoc! {"
                    | Note            | 级 |
                    |-----------------|----|
                    | runs every test | 高 |
                    | lints the code  | ✅ |
                "},
                indoc! {"
                    | A  | B             |
                    |----|---------------|
                    | لا | one two three |
                "},
            ];

            for input in narrowest_tables {
                let lines = render_markdown(input, &theme(), 14).lines;

                assert_eq!(lines[0].width(), 14, "{lines:?}");
                for line in &lines {
                    assert_eq!(line.width(), 14, "{line}");
                }
            }
        }

        #[test]
        fn table_aligns_wrapped_lines_without_the_space_at_each_break() {
            let lines = rendered_text_at_width(
                indoc! {"
                    | C       | R       |
                    |:-------:|--------:|
                    | cc dddd | aa bbbb |
                "},
                18,
            );

            assert_eq!(
                lines,
                [
                    "┌────────┬───────┐",
                    "│   C    │     R │",
                    "├────────┼───────┤",
                    "│   cc   │    aa │",
                    "│  dddd  │  bbbb │",
                    "└────────┴───────┘",
                ]
            );
        }

        #[test]
        fn table_narrower_than_its_longest_words_stacks_each_row_under_its_headers() {
            let lines = rendered_text_at_width(
                indoc! {"
                    | Command                    | Purpose            |
                    |----------------------------|--------------------|
                    | cargo clippy --all-targets | lints every target |
                    | cargo test                 | runs the tests     |
                "},
                20,
            );

            assert_eq!(
                lines,
                [
                    "Command: cargo ",
                    "  clippy ",
                    "  --all-targets",
                    "Purpose: lints every",
                    "  target",
                    "────────────────────",
                    "Command: cargo test",
                    "Purpose: runs the ",
                    "  tests",
                ]
            );
        }

        #[test]
        fn table_narrower_than_its_borders_stacks_each_row() {
            let body = rendered_text_at_width(
                indoc! {"
                    |   | B        | C |
                    |---|----------|---|
                    | 1 | 2<br>two | 3 |
                    | 4 | 5        | 6 |
                "},
                8,
            );
            let head_only = rendered_text_at_width(
                indoc! {"
                    | A | B | C |
                    |---|---|---|
                "},
                8,
            );

            assert_eq!(
                body,
                ["1", "B: 2 two", "C: 3", "────────", "4", "B: 5", "C: 6"]
            );
            assert_eq!(head_only, ["A", "B", "C"]);
        }

        #[test]
        fn table_br_breaks_a_cell_line_and_other_inline_html_stays_in_the_cell() {
            let lines = rendered_text(indoc! {"
                Intro.

                | A | B |
                |---|---|
                | one <br> two three four | `x` <br> y |
                | five<br> | <br> |
                | <b>bold</b><br><br> | w <br> `z` |
            "});

            assert_eq!(
                lines,
                [
                    "Intro.",
                    "",
                    "┌────────────────┬───┐",
                    "│ A              │ B │",
                    "├────────────────┼───┤",
                    "│ one            │ x │",
                    "│ two three four │ y │",
                    "├────────────────┼───┤",
                    "│ five           │   │",
                    "├────────────────┼───┤",
                    "│ <b>bold</b>    │ w │",
                    "│                │ z │",
                    "└────────────────┴───┘",
                ]
            );
        }

        #[test]
        fn table_in_list_item_keeps_the_item_indent_and_fits_inside_it() {
            let lines = rendered_text_at_width(
                indoc! {"
                    - Settings:
                      | Key  | Value     |
                      |------|-----------|
                      | mode | very fast |
                    - | A |
                      |---|
                      | 1 |
                "},
                21,
            );

            assert_eq!(
                lines,
                [
                    "- Settings:",
                    "  ┌──────┬──────────┐",
                    "  │ Key  │ Value    │",
                    "  ├──────┼──────────┤",
                    "  │ mode │ very     │",
                    "  │      │ fast     │",
                    "  └──────┴──────────┘",
                    "- ┌───┐",
                    "  │ A │",
                    "  ├───┤",
                    "  │ 1 │",
                    "  └───┘",
                ]
            );
        }

        #[test]
        fn table_in_blockquote_keeps_its_header_style() {
            let t = theme();

            let text = render_markdown("> | Name |\n> |------|\n> | a    |", &t, 0);

            assert_eq!(
                text.lines[1].spans[3],
                Span::styled("Name", t.table_header())
            );
            assert_eq!(text.lines[3].spans[3], Span::styled("a", t.muted()));
        }

        #[test]
        fn table_in_blockquote_keeps_the_marker_on_every_row() {
            let input = indoc! {"
                > | Key  | Value      |
                > |------|------------|
                > | mode | fast       |
                > | size | very large |
            "};

            let boxed = rendered_text_at_width(input, 18);
            let stacked = rendered_text_at_width(input, 14);

            assert_eq!(
                boxed,
                [
                    "│ ┌──────┬───────┐",
                    "│ │ Key  │ Value │",
                    "│ ├──────┼───────┤",
                    "│ │ mode │ fast  │",
                    "│ ├──────┼───────┤",
                    "│ │ size │ very  │",
                    "│ │      │ large │",
                    "│ └──────┴───────┘",
                ]
            );
            assert_eq!(
                stacked,
                [
                    "│ Key: mode",
                    "│ Value: fast",
                    "│ ────────────",
                    "│ Key: size",
                    "│ Value: very ",
                    "│   large",
                ]
            );
        }

        #[test]
        fn strong_and_emphasis_set_bold_and_italic() {
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
        fn strong_emphasis_sets_bold_and_italic_together() {
            let text = render_markdown("***bold italic***", &theme(), 0);
            assert_eq!(
                text.lines[0].spans,
                [Span::raw("bold italic").bold().italic()]
            );
        }

        #[test]
        fn strikethrough_crosses_out_text() {
            let text = render_markdown("~~struck~~", &theme(), 0);
            assert_eq!(text.lines[0].spans, [Span::raw("struck").crossed_out()]);
        }

        #[test]
        fn inline_code_uses_the_inline_code_style() {
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
        fn image_renders_its_alt_text_inline() {
            assert_eq!(
                rendered_text("before ![alt](https://example.com) after"),
                ["before alt after"],
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
                span.style.fg, t.inline_code.fg,
                "code span keeps its distinctive fg"
            );
            assert_eq!(span.style.bg, None, "code span stays fg-only");
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
            assert_eq!(span.style.fg, t.inline_code.fg);
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

        #[test]
        fn html_block_renders_one_row_per_line_between_blank_separators() {
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
        fn html_block_in_blockquote_keeps_the_marker_on_every_row() {
            let lines = rendered_text(indoc! {"
                > <div>
                > x
                > </div>
            "});
            assert_eq!(lines, ["│ <div>", "│ x", "│ </div>"]);
        }

        #[test]
        fn inline_html_is_kept_as_text() {
            assert_eq!(rendered_text("text <br> more"), ["text <br> more"]);
        }

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
            assert_eq!(url_span.style.fg, t.accent.fg);
        }

        #[test]
        fn link_autolink_renders_url_once_in_link_style() {
            let t = theme();
            let text = render_markdown("<https://x.io>", &t, 0);
            assert_eq!(
                text.lines[0].spans,
                [Span::styled("https://x.io", t.link())]
            );
        }

        #[test]
        fn link_email_autolink_renders_address_once() {
            assert_eq!(rendered_text("<me@x.io>"), ["me@x.io"]);
        }

        #[test]
        fn link_in_table_cell_keeps_url_in_the_cell() {
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

        #[test]
        fn paragraph_wraps_at_width() {
            assert_eq!(
                rendered_text_at_width("one two three four five six", 15),
                ["one two three ", "four five six"],
            );
        }

        #[test]
        fn heading_wraps_at_width() {
            assert_eq!(
                rendered_text_at_width("# one two three four five six", 15),
                ["# one two three", "four five six"],
            );
        }

        #[test]
        fn blockquote_wraps_with_marker_on_continuations() {
            assert_eq!(
                rendered_text_at_width("> one two three four five six seven eight", 15),
                ["│ one two three", "│ four five six", "│ seven eight"],
            );
        }

        #[test]
        fn nested_blockquote_wraps_with_nested_markers() {
            assert_eq!(
                rendered_text_at_width("> > one two three four five six seven eight", 20),
                [
                    "│ │ one two three ",
                    "│ │ four five six ",
                    "│ │ seven eight"
                ],
            );
        }

        #[test]
        fn tight_list_item_wraps_without_repeating_marker() {
            assert_eq!(
                rendered_text_at_width("- one two three four five six seven eight", 15),
                ["- one two three", "  four five six", "  seven eight"],
            );
        }

        #[test]
        fn list_item_wraps_its_text_before_a_nested_block() {
            let nested_list = rendered_text_at_width(
                indoc! {"
                    - one two three four five
                      - inner
                "},
                15,
            );
            let loose_nested_list = rendered_text_at_width(
                indoc! {"
                    - one two three four five

                      - inner
                "},
                15,
            );
            let blockquote = rendered_text_at_width(
                indoc! {"
                    - one two three four five
                      > quoted
                "},
                15,
            );
            let code_block = rendered_text_at_width(
                indoc! {"
                    - one two three four five
                      ```
                      code
                      ```
                "},
                15,
            );
            let table = rendered_text_at_width(
                indoc! {"
                    - one two three four five
                      | A |
                      |---|
                      | 1 |
                "},
                15,
            );

            assert_eq!(nested_list, ["- one two three", "  four five", "  - inner"]);
            assert_eq!(loose_nested_list, nested_list);
            assert_eq!(blockquote, ["- one two three", "  four five", "  │ quoted"]);
            assert_eq!(code_block, ["- one two three", "  four five", "  code"]);
            assert_eq!(
                table,
                [
                    "- one two three",
                    "  four five",
                    "  ┌───┐",
                    "  │ A │",
                    "  ├───┤",
                    "  │ 1 │",
                    "  └───┘",
                ]
            );
        }

        #[test]
        fn hard_break_wraps_the_line_it_ends() {
            let paragraph =
                rendered_text_at_width("one two three four five six  \nseven eight", 15);
            let tight_item = rendered_text_at_width("- one two three four  \n  five six", 15);
            let blockquote = rendered_text_at_width("> one two three four  \n> five six", 15);

            assert_eq!(
                paragraph,
                ["one two three ", "four five six", "seven eight"]
            );
            assert_eq!(tight_item, ["- one two three", "  four", "  five six"]);
            assert_eq!(blockquote, ["│ one two three", "│ four", "│ five six"]);
        }

        #[test]
        fn narrow_blockquote_keeps_its_marker_on_every_row_when_a_block_follows() {
            let lines = rendered_text_at_width("> quote quote\n\npara text", 2);

            assert_eq!(
                lines,
                [
                    "│ ", "│ q", "│ u", "│ o", "│ t", "│ e", "│ q", "│ u", "│ o", "│ t", "│ e", "",
                    "pa", "ra", "te", "xt",
                ]
            );
        }

        #[test]
        fn blockquote_list_item_wraps_with_blockquote_marker_only() {
            assert_eq!(
                rendered_text_at_width("> - one two three four five six seven eight", 20),
                [
                    "│ - one two three ",
                    "│   four five six ",
                    "│   seven eight"
                ],
            );
        }
    }

    mod render_markdown_on {
        use super::*;

        #[test]
        fn fills_only_text_without_a_color_with_the_base() {
            let t = Theme::default();

            let text = render_markdown_on("plain `code`", &t, t.thinking(), 0);

            assert_eq!(
                text.lines,
                [Line::from(vec![
                    Span::styled("plain ", t.thinking()),
                    Span::styled("code", t.inline_code()),
                ])]
            );
        }

        #[test]
        fn quote_in_a_text_body_steps_down_to_muted_keeping_modifiers() {
            let t = Theme::default();
            let base = t.text().add_modifier(Modifier::BOLD);

            let text = render_markdown_on("> quoted", &t, base, 0);

            assert_eq!(
                text.lines,
                [Line::from(vec![
                    Span::styled("│ ", t.blockquote()),
                    Span::styled("quoted", t.muted().add_modifier(Modifier::BOLD)),
                ])]
            );
        }

        #[test]
        fn quote_in_a_thinking_body_keeps_the_thinking_style() {
            let t = Theme::default();

            let text = render_markdown_on("> Quoted **text**", &t, t.thinking(), 0);

            assert_eq!(
                text.lines,
                [Line::from(vec![
                    Span::styled("│ ", t.blockquote()),
                    Span::styled("Quoted ", t.thinking()),
                    Span::styled("text", t.thinking().add_modifier(Modifier::BOLD)),
                ])]
            );
        }
    }

    mod is_line_break_tag {
        use super::*;

        #[test]
        fn accepts_every_br_spelling_and_rejects_other_tags() {
            for html in ["<br>", "<br/>", "<br />", "<BR>", " <Br > "] {
                assert!(is_line_break_tag(html), "{html:?}");
            }
            for html in ["<b>", "<brand>", "</br>", "<br"] {
                assert!(!is_line_break_tag(html), "{html:?}");
            }
        }
    }

    mod fit_column_widths {
        use super::*;

        /// Builds columns from `(natural, min)` pairs.
        fn column_widths(widths: &[(usize, usize)]) -> Vec<ColumnWidth> {
            widths
                .iter()
                .map(|&(natural, min)| ColumnWidth { natural, min })
                .collect()
        }

        #[test]
        fn keeps_natural_widths_that_fit() {
            let columns = column_widths(&[(5, 3), (5, 2)]);

            assert_eq!(fit_column_widths(&columns, 17), Some(vec![5, 5]));
        }

        #[test]
        fn shares_the_spare_width_by_how_far_each_column_falls_short() {
            let columns = column_widths(&[(0, 0), (3, 3), (30, 6), (12, 6)]);

            let col_widths = fit_column_widths(&columns, 34);

            assert_eq!(col_widths, Some(vec![0, 3, 11, 7]));
        }

        #[test]
        fn minimums_or_borders_that_do_not_fit_stack_the_table() {
            let words = column_widths(&[(10, 8), (10, 8)]);
            let empty = column_widths(&[(0, 0)]);

            assert_eq!(fit_column_widths(&words, 23), Some(vec![8, 8]));
            assert_eq!(fit_column_widths(&words, 22), None);
            assert_eq!(fit_column_widths(&empty, 4), Some(vec![0]));
            assert_eq!(fit_column_widths(&empty, 3), None);
        }
    }

    mod wrap_cell {
        use super::*;

        #[test]
        fn wraps_each_line_in_order_without_the_space_at_each_break() {
            let cell = [Line::from("one two three"), Line::from("four")];

            let out = wrap_cell(&cell, 8);

            assert_eq!(
                out,
                [
                    Line::from("one two"),
                    Line::from("three"),
                    Line::from("four")
                ]
            );
        }

        #[test]
        fn missing_cell_produces_single_empty_sub_line() {
            assert_eq!(wrap_cell(&[], 10), [Line::default()]);
        }
    }
}
