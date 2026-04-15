use pulldown_cmark::{Alignment, CodeBlockKind, CowStr, Event, HeadingLevel, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tracing::{debug, warn};
use unicode_width::UnicodeWidthStr;

use super::highlight::highlight_code;
use crate::tui::theme::Theme;

// ── Renderer ──

#[expect(
    clippy::struct_excessive_bools,
    reason = "table + code block state flags; will extract sub-structs in a follow-up refactor"
)]
pub(super) struct MarkdownRenderer<I> {
    iter: I,
    theme: Theme,
    pub(super) lines: Vec<Line<'static>>,

    /// Nested inline style stack (bold, italic, strikethrough).
    inline_styles: Vec<Style>,

    /// Ordered (`Some(index)`) or unordered (`None`) per nesting level.
    list_stack: Vec<Option<u64>>,

    /// Deferred list marker spans, emitted on the next `push_line`.
    pending_marker: Option<Vec<Span<'static>>>,

    /// Indent prefix per nesting level (list continuation or blockquote).
    indent_stack: Vec<Vec<Span<'static>>>,

    /// Insert a blank line before the next block element.
    needs_newline: bool,

    // Code block state
    in_code_block: bool,
    code_lang: Option<String>,
    code_buf: String,

    /// Stored link destination, appended at `End(Link)`.
    link_url: Option<String>,

    // Table buffering state
    in_table: bool,
    table_alignments: Vec<Alignment>,
    /// Rows of cells. Each cell is a vec of styled spans.
    table_rows: Vec<Vec<Vec<Span<'static>>>>,
    /// Current row being accumulated.
    table_current_row: Vec<Vec<Span<'static>>>,
    /// Spans accumulated for the current cell.
    table_cell_buf: Vec<Span<'static>>,
    /// Whether the current row belongs to the header.
    in_table_head: bool,
    /// Number of header rows (for styling).
    table_head_rows: usize,
}

impl<'a, I> MarkdownRenderer<I>
where
    I: Iterator<Item = Event<'a>>,
{
    pub(super) fn new(iter: I, theme: Theme) -> Self {
        Self {
            iter,
            theme,
            lines: Vec::new(),
            inline_styles: Vec::new(),
            list_stack: Vec::new(),
            pending_marker: None,
            indent_stack: Vec::new(),
            needs_newline: false,
            in_code_block: false,
            code_lang: None,
            code_buf: String::new(),
            link_url: None,
            in_table: false,
            table_alignments: Vec::new(),
            table_rows: Vec::new(),
            table_current_row: Vec::new(),
            table_cell_buf: Vec::new(),
            in_table_head: false,
            table_head_rows: 0,
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
            Tag::TableHead => self.in_table_head = true,
            Tag::TableRow => {}
            Tag::TableCell => self.table_cell_buf.clear(),
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
                if !self.table_current_row.is_empty() {
                    let row = std::mem::take(&mut self.table_current_row);
                    self.table_rows.push(row);
                }
                self.in_table_head = false;
                self.table_head_rows = self.table_rows.len();
            }
            TagEnd::TableRow => {
                let row = std::mem::take(&mut self.table_current_row);
                self.table_rows.push(row);
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.table_cell_buf);
                self.table_current_row.push(cell);
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
        let depth = self.list_stack.len();
        let indent_width = depth * 4 - 3;

        let marker = if let Some(last) = self.list_stack.last_mut() {
            match last {
                None => " ".repeat(indent_width.saturating_sub(1)) + "- ",
                Some(index) => {
                    let m = format!("{:indent_width$}. ", *index);
                    *index += 1;
                    m
                }
            }
        } else {
            "- ".to_owned()
        };

        let continuation = vec![Span::raw(" ".repeat(marker.len()))];
        self.pending_marker = Some(vec![Span::styled(marker, self.theme.list_marker())]);
        self.indent_stack.push(continuation);
        self.needs_newline = false;
    }

    // ── Code Blocks ──

    fn start_code_block(&mut self, kind: CodeBlockKind<'_>) {
        if !self.lines.is_empty() {
            self.push_blank_line();
        }
        self.in_code_block = true;
        self.code_lang = match kind {
            CodeBlockKind::Fenced(lang) => {
                let l = lang.to_string();
                if l.is_empty() { None } else { Some(l) }
            }
            CodeBlockKind::Indented => None,
        };
        self.code_buf.clear();
    }

    fn end_code_block(&mut self) {
        self.in_code_block = false;
        let code = std::mem::take(&mut self.code_buf);
        let lang = self.code_lang.take();

        let highlighted = highlight_code(
            lang.as_deref().unwrap_or(""),
            &code,
            self.theme.inline_code(),
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
        self.in_table = true;
        self.table_alignments = alignments;
        self.table_rows.clear();
        self.table_current_row.clear();
        self.table_cell_buf.clear();
        self.in_table_head = false;
        self.table_head_rows = 0;
    }

    fn end_table(&mut self) {
        self.in_table = false;

        let rows = std::mem::take(&mut self.table_rows);
        let alignments = std::mem::take(&mut self.table_alignments);
        let head_rows = self.table_head_rows;

        if rows.is_empty() {
            return;
        }

        let col_count = alignments
            .len()
            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
        let col_widths = compute_column_widths(&rows, col_count);

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
            // Data row: │ cell │ cell │
            let cell_style = if row_idx < head_rows {
                header_style
            } else {
                Style::default()
            };
            self.lines.push(build_data_row(
                row,
                &col_widths,
                &alignments,
                col_count,
                border_style,
                cell_style,
            ));

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
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }
        if self.in_table {
            let style = self.current_inline_style();
            self.table_cell_buf
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
        if self.in_table {
            self.table_cell_buf
                .push(Span::styled(code.into_string(), self.theme.inline_code()));
            return;
        }
        if self.pending_marker.is_some() {
            self.push_line(Line::default());
        }
        self.push_span(Span::styled(code.into_string(), self.theme.inline_code()));
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

    /// Start a new output line with indent prefixes and any pending list marker.
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
        self.lines.push(Line::from(spans));
    }

    /// Append a span to the last line, creating one if needed.
    fn push_span(&mut self, span: Span<'static>) {
        if let Some(line) = self.lines.last_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    fn push_blank_line(&mut self) {
        self.lines.push(Line::default());
    }
}

// ── Table Helpers ──

/// Measure the display width of a cell's spans.
fn cell_width(cell: &[Span<'_>]) -> usize {
    cell.iter().map(|s| s.content.width()).sum()
}

/// Compute the max display width for each column across all rows.
fn compute_column_widths(rows: &[Vec<Vec<Span<'_>>>], col_count: usize) -> Vec<usize> {
    let mut widths = vec![0_usize; col_count];
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            widths[col] = widths[col].max(cell_width(cell));
        }
    }
    widths
}

/// Build a horizontal rule line: e.g. `┌───┬───┐` or `├───┼───┤`.
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

/// Build a data row: `│ cell │ cell │`, applying alignment and cell style.
fn build_data_row(
    row: &[Vec<Span<'static>>],
    col_widths: &[usize],
    alignments: &[Alignment],
    col_count: usize,
    border_style: Style,
    cell_style: Style,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let pipe = Span::styled("│", border_style);

    spans.push(pipe.clone());
    for col in 0..col_count {
        let cell = row.get(col).map_or(&[][..], Vec::as_slice);
        let content_width = cell_width(cell);
        let target_width = col_widths.get(col).copied().unwrap_or(0);
        let pad = target_width.saturating_sub(content_width);

        let alignment = alignments.get(col).copied().unwrap_or(Alignment::None);
        let (pad_left, pad_right) = match alignment {
            Alignment::Center => (pad / 2, pad - pad / 2),
            Alignment::Right => (pad, 0),
            Alignment::Left | Alignment::None => (0, pad),
        };

        // Left padding (always at least 1 space)
        spans.push(Span::raw(" ".repeat(1 + pad_left)));

        // Cell content with optional style override for headers
        for span in cell {
            let styled = if cell_style == Style::default() {
                span.clone()
            } else {
                Span::styled(span.content.clone(), span.style.patch(cell_style))
            };
            spans.push(styled);
        }

        // Right padding (always at least 1 space)
        spans.push(Span::raw(" ".repeat(1 + pad_right)));
        spans.push(pipe.clone());
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ratatui::style::{Color, Modifier};

    use super::super::render_markdown;
    use crate::tui::theme::Theme;

    fn theme() -> Theme {
        Theme::default()
    }

    fn rendered_text(input: &str) -> Vec<String> {
        render_markdown(input, &theme())
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
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

    // ── Inline Styles ──

    #[test]
    fn bold_and_italic() {
        let text = render_markdown("**bold** and *italic*", &theme());
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
        let text = render_markdown("***bold italic***", &theme());
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
        let text = render_markdown("~~struck~~", &theme());
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
        let text = render_markdown("Use `foo()` here", &t);
        let span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("foo()"))
            .unwrap();
        assert_eq!(span.style.fg, Some(t.code));
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
        let text = render_markdown("[text](https://example.com)", &t);
        let url_span = text.lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("https://example.com"))
            .unwrap();
        assert!(url_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(url_span.style.fg, Some(t.accent));
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
    fn fenced_code_block_with_lang_highlights() {
        let text = render_markdown(
            indoc! {"
                ```rust
                fn main() {}
                ```
            "},
            &theme(),
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

        // "a" should be left-aligned (near left pipe), "c" right-aligned (near right pipe)
        let a_pos = body_row.find('a').unwrap();
        let c_pos = body_row.find('c').unwrap();
        assert!(
            a_pos < c_pos,
            "left-aligned 'a' should appear before right-aligned 'c'"
        );

        // For center alignment, 'b' should have padding on both sides
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
    fn table_inline_styles() {
        let t = theme();
        let text = render_markdown(
            indoc! {"
                | Header |
                |--------|
                | `code` |
            "},
            &t,
        );
        let has_code = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("code") && s.style.fg == Some(t.code))
        });
        assert!(has_code, "inline code should be styled inside table cells");
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
        // Row should still have 3 pipe characters (left, middle, right)
        let pipe_count = body_row.matches('│').count();
        assert_eq!(
            pipe_count, 3,
            "row with empty cell should have 3 borders: {body_row:?}"
        );
    }

    // ── Ordered Lists ──

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

    // ── Unordered Lists ──

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

    // ── Nested Lists ──

    #[test]
    fn nested_list() {
        let lines = rendered_text(indoc! {"
            - Outer
              - Inner
        "});
        assert!(lines.iter().any(|l| l.contains("Outer")));
        assert!(lines.iter().any(|l| l.contains("Inner")));
        let inner = lines.iter().find(|l| l.contains("Inner")).unwrap();
        let outer = lines.iter().find(|l| l.contains("Outer")).unwrap();
        assert!(
            inner.len() > outer.len(),
            "inner should have more indent than outer"
        );
    }

    // ── Inline Elements in Lists ──

    #[test]
    fn inline_code_in_list_item() {
        let t = theme();
        let text = render_markdown("- Use `foo()` here", &t);
        let has_code_span = text.lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("foo()") && s.style.fg == Some(t.code))
        });
        assert!(has_code_span, "inline code should be styled in list items");
    }

    #[test]
    fn link_in_list_item() {
        let lines = rendered_text("- [Click](https://example.com)");
        assert!(lines.iter().any(|l| l.contains("Click")));
        assert!(lines.iter().any(|l| l.contains("https://example.com")));
    }

    // ── Blockquotes ──

    #[test]
    fn blockquote_text_and_style() {
        let t = theme();
        let text = render_markdown("> Quoted text", &t);
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
        assert!(
            inner.matches("> ").count() >= 2,
            "inner blockquote should have nested > markers: {inner:?}"
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
        );
        let rule_span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains('─')))
            .expect("rule span not found");
        assert_eq!(rule_span.style.fg, Some(t.fg_dim));
    }

    // ── List Marker Style ──

    #[test]
    fn list_marker_uses_accent_color() {
        let t = theme();
        let text = render_markdown("- Item", &t);
        let marker_span = text
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.contains("- ")));
        let span = marker_span.expect("list marker span not found");
        assert_eq!(span.style.fg, Some(t.accent));
    }

    // ── HTML ──

    #[test]
    fn html_block_rendered_as_text() {
        let lines = rendered_text("<div>hello</div>\n");
        assert!(lines.iter().any(|l| l.contains("<div>hello</div>")));
    }

    #[test]
    fn inline_html_preserved() {
        let lines = rendered_text("text <br> more");
        let joined: String = lines.join("");
        assert!(joined.contains("<br>"));
    }
}
